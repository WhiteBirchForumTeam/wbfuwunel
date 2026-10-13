use std::{any::TypeId, str, time::Duration};

use axum::{RequestExt, RequestPartsExt, extract::Path};
use axum_extra::extract::cookie::CookieJar;
use bytes::Bytes;
use http::{header::CONTENT_LENGTH, request::Parts};
use ruma::api::{
	IncomingRequest,
	client::media::{create_content, create_content_async},
	error::{ErrorKind, LimitExceededErrorData, RetryAfter},
};
use serde::Deserialize;
use tuwunel_core::{
	Error, Result, err, smallstr::SmallString, smallvec::SmallVec, trace,
};
use tuwunel_service::{
	Services,
	media::upload_budget::{BudgetError, Reservation},
};

/// How long a refused upload is told to wait.
///
/// ⚠️ A floor, not a promise: the budget frees when uploads already running
/// finish, and nothing here knows when that will be. It exists so a client
/// backs off instead of retrying immediately.
const UPLOAD_RETRY_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize)]
pub(super) struct QueryParams {
	pub(super) access_token: Option<String>,

	pub(super) user_id: Option<UserId>,

	pub(super) device_id: Option<DeviceId>,

	#[serde(rename = "org.matrix.msc3202.device_id")]
	pub(super) msc3202_device_id: Option<DeviceId>,
}

impl QueryParams {
	pub(super) fn device_id(&self) -> Option<&str> {
		self.device_id
			.as_deref()
			.or(self.msc3202_device_id.as_deref())
	}
}

pub(super) type UserId = SmallString<[u8; 48]>;
pub(super) type DeviceId = SmallString<[u8; 24]>;

#[derive(Debug)]
pub(super) struct Request {
	pub(super) cookie: CookieJar,
	pub(super) path: Path<PathParams>,
	pub(super) query: QueryParams,
	pub(super) body: Bytes,
	pub(super) parts: Parts,

	/// Bytes booked out of the upload budget to hold this body.
	///
	/// `None` for everything but the traditional upload endpoints.
	///
	/// ⚠️ 🚫 Never given back by hand: it travels with the request so that
	/// every way a request can end — disconnect, timeout, a parse error, a
	/// handler returning early — gives the memory back
	/// (`/docs/design/media/legacy-upload-concurrency.md` §2).
	pub(super) upload: Option<Reservation>,
}

pub(super) type PathParams = SmallVec<[PathParam; 8]>;
pub(super) type PathParam = SmallString<[u8; 32]>;

#[tracing::instrument(
	name = "parse",
	level = "trace",
	skip(services),
	err(level = "debug")
	ret(level = "trace"),
)]
pub(super) async fn from(
	services: &Services,
	request: http::Request<axum::body::Body>,
	holds_whole_file: bool,
) -> Result<Request> {
	let limited = request.with_limited_body();
	let (mut parts, body) = limited.into_parts();
	trace!(?parts, ?body);

	let cookie: CookieJar = parts.extract().await?;
	trace!(?cookie);

	let path: Path<PathParams> = parts.extract().await?;
	trace!(?path);

	let query = parts.uri.query().unwrap_or_default();
	let query = serde_html_form::from_str(query)
		.map_err(|e| err!(Request(Unknown("Failed to read query parameters: {e}"))))?;

	let configured_max = services.server.config.max_request_size;

	// 🚨 Booked before the body is read, which is the one moment the memory
	// has not been spent yet.
	let upload = holds_whole_file
		.then(|| book_upload_memory(services, &parts))
		.transpose()?;

	// ⭐ The limit is what was booked, not what the configuration allows: a
	// client that declares less than it sends is cut off at its own number, so
	// "bytes held at once <= the budget" holds without believing the
	// declaration (`/docs/design/media/legacy-upload-concurrency.md` §2.1).
	let max_body_size = upload
		.as_ref()
		.map_or(configured_max, Reservation::bytes);

	let body = axum::body::to_bytes(body, max_body_size)
		.await
		.map_err(|e| err!(Request(TooLarge("Request body too large: {e}"))))?;

	Ok(Request { cookie, path, query, body, parts, upload })
}

/// Whether this endpoint's body is one whole file, buffered in memory.
///
/// ⭐ Told apart by the endpoint's Ruma type rather than by its path. One type
/// answers every generation of its path (`create_content` serves both
/// `/_matrix/media/r0/upload` and `/_matrix/media/v3/upload`) and some routes
/// are registered by hand, so a list of path spellings would have to be kept
/// in step with both — and the cost of missing one is a budget that quietly
/// does not apply. There is nothing to keep in step here.
///
/// Args:
///     T: the endpoint's Ruma request type
/// Return:
///     bool  true for the two traditional upload endpoints; false for
///     everything else, `create_mxc_uri` included — its request has no body,
///     it only mints the URI that `create_content_async` then fills.
pub(super) fn is_whole_file_upload<T: IncomingRequest + 'static>() -> bool {
	let endpoint = TypeId::of::<T>();

	endpoint == TypeId::of::<create_content::v3::Request>()
		|| endpoint == TypeId::of::<create_content_async::v3::Request>()
}

/// Args:
///     services: for the budget and the configured maximum
///     parts: the request head, read for its declared length
/// Return:
///     Result<Reservation>  Err 429 M_LIMIT_EXCEEDED when the budget is
///     spent, Err 413 when this one request exceeds the whole budget.
fn book_upload_memory(services: &Services, parts: &Parts) -> Result<Reservation> {
	let budget = services.media.upload_budget();

	// 🚨 What the client declares, never more than we would read anyway — and
	// the whole maximum when it declares nothing, which is the fail-closed
	// side: a chunked body has no length to believe.
	let declared = parts
		.headers
		.get(CONTENT_LENGTH)
		.and_then(|length| length.to_str().ok())
		.and_then(|length| length.parse::<usize>().ok())
		.map_or_else(
			|| services.server.config.max_request_size,
			|length| length.min(services.server.config.max_request_size),
		);

	budget.reserve(declared).map_err(|error| match error {
		| BudgetError::Full => Error::BadRequest(
			ErrorKind::LimitExceeded(LimitExceededErrorData {
				retry_after: Some(RetryAfter::Delay(UPLOAD_RETRY_AFTER)),
			}),
			"The server is already holding as much upload as it may; try again shortly.",
		),
		// 🚫 Not 429: no amount of retrying makes a request smaller than it
		// is, so inviting a retry loop would be worse than saying no.
		| BudgetError::LargerThanBudget => err!(Request(TooLarge(
			"Upload declares more than the server may hold at once ({} bytes).",
			budget.capacity_bytes(),
		))),
	})
}

#[cfg(test)]
mod tests {
	use ruma::api::client::{
		account::whoami,
		media::{create_content, create_content_async, create_mxc_uri},
	};

	use super::is_whole_file_upload;

	// 🚨 A traditional upload endpoint that is not named here is not bounded
	// by anything: the per-request size limit still applies, but nothing caps
	// what all of them hold together. So this test is where a new one has to
	// be noticed — it fails for the endpoint that was added without a line in
	// `is_whole_file_upload`, rather than the budget silently not applying.
	#[test]
	fn both_endpoints_that_buffer_a_whole_file_book_memory() {
		assert!(
			is_whole_file_upload::<create_content::v3::Request>(),
			"POST /_matrix/media/*/upload sends the file in the request body"
		);
		assert!(
			is_whole_file_upload::<create_content_async::v3::Request>(),
			"PUT /_matrix/media/*/upload/{{server}}/{{id}} does too"
		);
	}

	// ⭐ The budget is about bytes held, not about being an upload endpoint:
	// this one is named `create` and is part of uploading, but its request is
	// `Request {}` — there is nothing to book.
	#[test]
	fn minting_an_mxc_uri_books_nothing_because_its_request_has_no_body() {
		assert!(!is_whole_file_upload::<create_mxc_uri::v1::Request>());
	}

	// ⚠️ The negative side of §2.3: the gate must not reach every request, or
	// a server busy with uploads would start refusing logins.
	#[test]
	fn an_endpoint_that_is_not_an_upload_books_nothing() {
		assert!(!is_whole_file_upload::<whoami::v3::Request>());
	}
}
