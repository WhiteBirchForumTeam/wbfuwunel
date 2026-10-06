#![cfg(test)]

use std::any::Any;

use axum::{Router, routing::get};
use bytes::Bytes;
use http::{Request, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::{BodyExt, Full};
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;

use super::{get_panic_details, to_panic_response};

#[test]
fn a_panic_message_is_carried_whichever_type_it_unwound_as() {
	assert_eq!(get_panic_details(&"boom"), "boom", "panic!(\"boom\") unwinds as a &str");
	assert_eq!(get_panic_details(&"boom".to_owned()), "boom", "a formatted panic! unwinds as a String");
	assert_eq!(
		get_panic_details(&7_u32),
		"Unknown internal server error occurred.",
		"🚫 never empty: it is the only thing saying which panic this was"
	);
}

#[tokio::test]
async fn the_panic_response_is_a_500_the_bridge_can_turn_into_an_error_pack() {
	let response = to_panic_response("boom");

	assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
	assert_eq!(
		response
			.headers()
			.get(CONTENT_TYPE)
			.and_then(|value| value.to_str().ok()),
		Some("application/json"),
		"the bridge reads the body as JSON to find the message"
	);

	let body = response
		.into_body()
		.collect()
		.await
		.expect("a Full body never fails")
		.to_bytes();
	let body: serde_json::Value = serde_json::from_slice(&body).expect("the body is JSON");

	// 🚨 `error` is the field `bridge::build_reply_pack` reads for an `Error`
	// pack's message, and 500 is what `reject_code_for_status` maps to
	// `RejectCode::Internal` — so these two are the contract with the bridge,
	// not decoration (/docs/design/wire/bridge-catch-panic.md §2).
	assert_eq!(body["errcode"], "M_UNKNOWN");
	assert!(
		body["error"]
			.as_str()
			.is_some_and(|error| error.contains("M_UNKNOWN")),
		"{body}"
	);
	assert_eq!(body["details"], "boom", "the panic's own text has to reach the log and the reply");
}

/// 🚨 The property the bridge depends on: a handler that unwinds under
/// `oneshot` must come back as a **response**, not as a panic travelling up the
/// WebSocket task that called it.
///
/// 📎 The closure here is `to_panic_response` rather than `catch_panic` itself —
/// the only difference is the `requests_panic` metric, which needs a whole
/// `Services`. So this pins the mechanism and the reply; that
/// `build_bridge_router` installs the layer at all is read from the code
/// (/docs/design/wire/bridge-catch-panic.md §4).
#[tokio::test]
async fn a_panicking_handler_under_this_layer_answers_instead_of_unwinding() {
	async fn boom() -> StatusCode { panic!("boom") }

	fn respond(panic: Box<dyn Any + Send + 'static>) -> Response<Full<Bytes>> {
		to_panic_response(&get_panic_details(&*panic))
	}

	let router: Router = Router::new()
		.route("/boom", get(boom))
		.layer(CatchPanicLayer::custom(respond));

	let response = router
		.oneshot(
			Request::builder()
				.uri("/boom")
				.body(axum::body::Body::empty())
				.expect("a well-formed request in a test"),
		)
		.await
		.expect("the layer answers, so the call cannot fail");

	assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
