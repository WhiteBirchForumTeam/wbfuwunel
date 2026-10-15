use std::{any::TypeId, fmt::Debug, mem, ops::Deref};

use axum::{body::Body, extract::FromRequest};
use axum_extra::extract::cookie::CookieJar;
use bytes::{BufMut, Bytes, BytesMut};
use http::Method;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, DeviceId, OwnedDeviceId, OwnedServerName,
	OwnedUserId, ServerName, UserId, api::IncomingRequest,
};
use tuwunel_core::{Err, Error, Result, err, trace, utils::string::EMPTY};
use tuwunel_service::{Services, appservice::RegistrationInfo, media::upload_budget::Reservation};

use super::{
	auth,
	auth::{Auth, AuthDispatch},
	request,
	request::Request,
};
use crate::State;

/// Extractor for Ruma request structs
#[derive(Debug)]
pub(crate) struct Args<T> {
	/// Request struct body
	pub(crate) body: T,

	/// Cookies received from the useragent.
	pub(crate) cookie: CookieJar,

	/// Federation server authentication: X-Matrix origin
	/// None when not a federation server.
	pub(crate) origin: Option<OwnedServerName>,

	/// Local user authentication: user_id.
	/// None when not an authenticated local user.
	pub(crate) sender_user: Option<OwnedUserId>,

	/// Local user authentication: device_id.
	/// None when not an authenticated local user or no device.
	pub(crate) sender_device: Option<OwnedDeviceId>,

	/// Appservice authentication; registration info.
	/// None when not an appservice.
	pub(crate) appservice_info: Option<RegistrationInfo>,

	/// Parsed JSON content.
	/// None when body is not a valid string
	pub(crate) json_body: Option<CanonicalJsonValue>,

	/// The request's HTTP headers, for the few endpoints that read one Ruma
	/// does not model (e.g. `X-Wbf-Attachments` on send).
	pub(crate) headers: http::HeaderMap,

	/// Bytes booked out of the upload budget to hold this request's body.
	///
	/// `None` for everything but the traditional upload endpoints.
	///
	/// ⚠️ 🚫 Nothing reads this and nothing releases it: it is here so that
	/// the memory stays booked for exactly as long as the handler holds the
	/// body, and is given back however the handler ends
	/// (`/docs/design/media/legacy-upload-concurrency.md` §2).
	pub(crate) _upload: Option<Reservation>,
}

impl<T> Args<T> {
	#[inline]
	pub(crate) fn sender_user(&self) -> &UserId {
		self.sender_user
			.as_deref()
			.expect("user must be authenticated for this handler")
	}

	#[inline]
	pub(crate) fn origin(&self) -> &ServerName {
		self.origin
			.as_deref()
			.expect("server must be authenticated for this handler")
	}

	#[inline]
	pub(crate) fn sender_device(&self) -> Result<&DeviceId> {
		self.sender_device
			.as_deref()
			.ok_or(err!(Request(Forbidden("user must be authenticated and device identified"))))
	}
}

impl<T> Deref for Args<T>
where
	T: Sync,
{
	type Target = T;

	fn deref(&self) -> &Self::Target { &self.body }
}

impl<T> FromRequest<State, Body> for Args<T>
where
	T: IncomingRequest + Debug + Send + Sync + 'static,
	T::Authentication: AuthDispatch,
{
	type Rejection = Error;

	#[tracing::instrument(
		name = "ar",
		level = "debug",
		skip(services),
		err(level = "debug")
		ret(level = "trace"),
	)]
	async fn from_request(
		request: http::Request<Body>,
		services: &State,
	) -> Result<Self, Self::Rejection> {
		let mut request =
			request::from(services, request, request::is_whole_file_upload::<T>()).await?;
		// 🚨 Parsing a body grows a tree out of it that nothing else bounds, and it
		// happens before this request is authenticated — so what may be parsed, and how
		// much of it, is decided per endpoint before the parse runs
		// (/docs/design/media/legacy-upload-concurrency.md §2.4 and §2.5). An upload's
		// body is a file and is never parsed at all.
		let mut json_body = request::parse_json_body::<T>(services, &request)?;
		trace!(?request);

		// An empty body defaults to `{}` like Synapse so a UIA flow can start;
		// a present body that is not valid JSON is rejected as M_NOT_JSON.
		let json_endpoint = matches!(
			request.parts.method,
			Method::POST | Method::PUT | Method::DELETE | Method::PATCH
		) && !request.parts.uri.path().contains("/media/");

		if json_body.is_none() && json_endpoint {
			let empty = request.body.iter().all(u8::is_ascii_whitespace);

			if !empty && serde_json::from_slice::<serde_json::Value>(&request.body).is_err() {
				return Err!(Request(NotJson("Request body is not valid JSON.")));
			}

			if empty && matches!(request.parts.method, Method::POST | Method::DELETE) {
				json_body = Some(CanonicalJsonValue::Object(CanonicalJsonObject::new()));
			}
		}

		let auth = auth::auth::<T::Authentication>(
			services,
			&mut request,
			json_body.as_ref(),
			TypeId::of::<T>(),
		)
		.await?;

		let headers = request.parts.headers.clone();
		// ⚠️ Moved into `Args` rather than dropped here: the body lives for as
		// long as the handler holds this, and the bytes are booked until then
		// (`/docs/design/media/legacy-upload-concurrency.md` §2).
		let upload = request.upload.take();
		Ok(Self {
			body: make_body::<T>(services, &mut request, json_body.as_mut(), &auth)?,
			headers,
			cookie: request.cookie,
			origin: auth.origin,
			sender_user: auth.sender_user,
			sender_device: auth.sender_device,
			appservice_info: auth.appservice_info,
			json_body,
			_upload: upload,
		})
	}
}

fn make_body<T>(
	services: &Services,
	request: &mut Request,
	json_body: Option<&mut CanonicalJsonValue>,
	auth: &Auth,
) -> Result<T>
where
	T: IncomingRequest,
{
	let body = take_body(services, request, json_body, auth);
	let http_request = into_http_request(request, body);
	T::try_from_http_request(http_request, &request.path)
		.map_err(|e| err!(Request(BadJson(debug_warn!("{e}")))))
}

fn into_http_request(request: &Request, body: Bytes) -> http::Request<Bytes> {
	let mut http_request = http::Request::builder()
		.uri(request.parts.uri.clone())
		.method(request.parts.method.clone());

	*http_request
		.headers_mut()
		.expect("mutable http headers") = request.parts.headers.clone();

	http_request
		.body(body)
		.expect("http request body")
}

#[expect(clippy::needless_pass_by_value)]
fn take_body(
	services: &Services,
	request: &mut Request,
	json_body: Option<&mut CanonicalJsonValue>,
	auth: &Auth,
) -> Bytes {
	let Some(CanonicalJsonValue::Object(json_body)) = json_body else {
		return mem::take(&mut request.body);
	};

	let user_id = auth.sender_user.clone().unwrap_or_else(|| {
		let server_name = services.globals.server_name();
		UserId::parse_with_server_name(EMPTY, server_name).expect("valid user_id")
	});

	let uiaa_request = json_body
		.get("auth")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|auth| auth.get("session"))
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|session| {
			services
				.uiaa
				.get_uiaa_request(&user_id, auth.sender_device.as_deref(), session)
		});

	if let Some(CanonicalJsonValue::Object(initial_request)) = uiaa_request {
		for (key, value) in initial_request {
			json_body.entry(key).or_insert(value);
		}
	}

	let mut buf = BytesMut::new().writer();
	serde_json::to_writer(&mut buf, &json_body).expect("value serialization can't fail");
	buf.into_inner().freeze()
}
