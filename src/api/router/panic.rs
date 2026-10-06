//! The one reply a caught panic gets, shared by both routers.
//!
//! ⭐ It lives here rather than beside the served router's layers because the
//! **bridge's** router needs it too, and `tuwunel_api` is the crate both can
//! reach (`/docs/design/wire/bridge-catch-panic.md` §3). One copy, so the two
//! routers cannot drift apart on what a panic looks like.

#[cfg(test)]
mod tests;

use std::{any::Any, sync::Arc};

use bytes::Bytes;
use http::{Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::Full;
use tuwunel_core::error;
use tuwunel_service::Services;

/// Args:
///     err: what the panicking handler unwound with, example: the `&str` of a
///         `panic!("boom")`
///     services: for the `requests_panic` metric
/// Return:
///     Response<Full<Bytes>>  always a 500 with an `M_UNKNOWN` JSON body; the
///     panic's own text rides in `details`.
#[tracing::instrument(name = "panic", level = "error", skip_all)]
#[expect(clippy::needless_pass_by_value)]
pub fn catch_panic(err: Box<dyn Any + Send + 'static>, services: Arc<Services>) -> Response<Full<Bytes>> {
	services
		.server
		.metrics
		.requests_panic
		.fetch_add(1, std::sync::atomic::Ordering::Release);

	let details = get_panic_details(&*err);
	error!("{details:#}");

	to_panic_response(&details)
}

/// Args:
///     err: the unwind payload, example: `&"boom"` from `panic!("boom")`
/// Return:
///     String  the panic's own message when it was a `String` or a `&str`,
///     else a fixed sentence — 🚫 never empty, because it is the only thing
///     that says which panic this was.
pub(super) fn get_panic_details(err: &(dyn Any + Send)) -> String {
	if let Some(message) = err.downcast_ref::<String>() {
		return message.clone();
	}
	if let Some(message) = err.downcast_ref::<&str>() {
		return (*message).to_owned();
	}

	"Unknown internal server error occurred.".to_owned()
}

/// Args:
///     details: example: "boom"
/// Return:
///     Response<Full<Bytes>>  500 with `application/json` and an `M_UNKNOWN`
///     body. 🚨 Infallible on purpose: this runs while a handler is already
///     unwinding, so a panic here would be the one nothing catches
///     (CLAUDE.md P). A builder failure can only come from the two constants
///     below, and then the fallback is the same status with no body rather than
///     a guess about what went wrong.
pub(super) fn to_panic_response(details: &str) -> Response<Full<Bytes>> {
	let body = serde_json::json!({
		"errcode": "M_UNKNOWN",
		"error": "M_UNKNOWN: Internal server error occurred",
		"details": details,
	});

	Response::builder()
		.status(StatusCode::INTERNAL_SERVER_ERROR)
		.header(CONTENT_TYPE, "application/json")
		.body(Full::from(body.to_string()))
		.unwrap_or_else(|e| {
			error!("Could not build the response for a caught panic: {e}");
			let mut fallback = Response::new(Full::default());
			*fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
			fallback
		})
}
