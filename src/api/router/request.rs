use std::{any::TypeId, str, time::Duration};

use axum::{RequestExt, RequestPartsExt, extract::Path};
use axum_extra::extract::cookie::CookieJar;
use bytes::Bytes;
use http::{header::CONTENT_LENGTH, request::Parts};
use ruma::{
	CanonicalJsonValue,
	api::{
		IncomingRequest,
		client::media::{create_content, create_content_async},
		error::{ErrorKind, LimitExceededErrorData, RetryAfter},
	},
};
use serde::Deserialize;
use tuwunel_core::{
	Err, Error, Result, err, matrix::pdu::MAX_PDU_BYTES, smallstr::SmallString,
	smallvec::SmallVec, trace,
};
use tuwunel_service::{
	Services,
	media::upload_budget::{BudgetError, Reservation},
	sending::PDU_LIMIT,
};

use super::auth::{AuthDispatch, is_federation_scheme};

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

/// What one federation transaction may legitimately be.
///
/// ⭐ Derived, 🚫 not configured: the protocol already says how large it can
/// get — at most `PDU_LIMIT` events of at most `MAX_PDU_BYTES` each — so there
/// is no number for an operator to pick. The doubling is the allowance for the
/// EDUs riding in the same transaction (up to `EDU_LIMIT` of them, and 🚫
/// nothing caps an EDU's size) plus the JSON frame around them.
///
/// ⚠️ If this ever refuses a real transaction it will be the first suspect:
/// `config/check.rs` refuses `max_request_size` under 10 MB on the grounds
/// that federation needs it, which is more than this. The spec's own limits
/// say otherwise, and the refusal names itself, so it is diagnosable.
const FEDERATION_JSON_MAX: usize = PDU_LIMIT
	.saturating_mul(MAX_PDU_BYTES)
	.saturating_mul(2);

/// Args:
///     configured: `max_json_body_size`, example: 2097152
/// Return:
///     Option<(usize, &'static str)>  how many bytes of this endpoint's body
///     may be parsed as JSON and what says so, or None when its body is a
///     file that is never parsed.
pub(super) fn find_json_body_limit<T>(configured: usize) -> Option<(usize, &'static str)>
where
	T: IncomingRequest + 'static,
	T::Authentication: AuthDispatch,
{
	if is_whole_file_upload::<T>() {
		return None;
	}

	if is_federation_scheme::<T::Authentication>() {
		return Some((FEDERATION_JSON_MAX, "the federation transaction limit"));
	}

	Some((configured, "max_json_body_size"))
}

/// Args:
///     services: for the configured limit
///     request: the request whose body has already been read
/// Return:
///     Result<Option<CanonicalJsonValue>>  the parsed body; None when this
///     endpoint's body is never read as JSON, or when it is not JSON at all.
///     Err 413 when it is larger than this endpoint may have parsed.
///
/// 🚨 The size is checked **before** the parse, because the parse is the
/// expensive part: a body of nothing but a long array becomes some sixteen
/// times its own size in memory, and it happens before the request is
/// authenticated (`/docs/design/media/legacy-upload-concurrency.md` §2.5).
pub(super) fn parse_json_body<T>(
	services: &Services,
	request: &Request,
) -> Result<Option<CanonicalJsonValue>>
where
	T: IncomingRequest + 'static,
	T::Authentication: AuthDispatch,
{
	let configured = services.server.config.max_json_body_size;
	let Some((limit, named_by)) = find_json_body_limit::<T>(configured) else {
		return Ok(None);
	};

	if request.body.len() > limit {
		return Err!(Request(TooLarge(
			"Request body is {} bytes, more than the {limit} this endpoint may have read as \
			 JSON ({named_by}).",
			request.body.len(),
		)));
	}

	Ok(serde_json::from_slice(&request.body).ok())
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

#[cfg(test)]
mod json_limit_tests {
	use ruma::api::{
		client::{
			account::whoami,
			media::{create_content, create_content_async},
		},
		federation::transactions::send_transaction_message,
	};
	use tuwunel_core::matrix::pdu::MAX_PDU_BYTES;
	use tuwunel_service::sending::PDU_LIMIT;

	use super::{FEDERATION_JSON_MAX, find_json_body_limit};

	const CONFIGURED: usize = 2 * 1024 * 1024;

	// 🚫 Never parsed, so there is no allowance to compare a body against.
	#[test]
	fn an_upload_body_has_no_json_allowance_because_it_is_never_parsed() {
		assert_eq!(find_json_body_limit::<create_content::v3::Request>(CONFIGURED), None);
		assert_eq!(find_json_body_limit::<create_content_async::v3::Request>(CONFIGURED), None);
	}

	// ⭐ Told apart by the authentication scheme, so a federation endpoint
	// added later is covered without anyone editing a list.
	#[test]
	fn a_federation_transaction_is_allowed_what_the_protocol_allows() {
		let found = find_json_body_limit::<send_transaction_message::v1::Request>(CONFIGURED);

		assert_eq!(found.map(|(limit, _)| limit), Some(FEDERATION_JSON_MAX));
	}

	#[test]
	fn every_other_endpoint_is_allowed_what_the_configuration_says() {
		let found = find_json_body_limit::<whoami::v3::Request>(CONFIGURED);

		assert_eq!(found, Some((CONFIGURED, "max_json_body_size")));
	}

	// 🚨 The number itself: a transaction of the largest legal size has to fit,
	// or federation breaks on a busy peer and nothing here would say why.
	#[test]
	fn the_federation_allowance_holds_a_full_transaction_of_largest_events() {
		let largest_transaction = PDU_LIMIT * MAX_PDU_BYTES;

		assert!(
			FEDERATION_JSON_MAX >= largest_transaction,
			"{FEDERATION_JSON_MAX} must hold {PDU_LIMIT} events of {MAX_PDU_BYTES} bytes"
		);
		assert!(
			FEDERATION_JSON_MAX >= largest_transaction * 2,
			"and leave as much again for the EDUs riding with them"
		);
	}

	// ⚠️ The client allowance must not be the one federation gets by accident:
	// if these ever became the same number, the split would be doing nothing.
	#[test]
	fn the_two_allowances_are_not_the_same_number() {
		assert_ne!(CONFIGURED, FEDERATION_JSON_MAX);
	}
}
