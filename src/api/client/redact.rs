use axum::extract::State;
use ruma::api::client::redact::redact_event;
use tuwunel_core::Result;

use crate::{Ruma, client::utils::redact_event_as};

/// # `PUT /_matrix/client/r0/rooms/{roomId}/redact/{eventId}/{txnId}`
///
/// Tries to send a redaction event into the room.
///
/// - TODO: Handle txn id
pub(crate) async fn redact_event_route(
	State(services): State<crate::State>,
	body: Ruma<redact_event::v3::Request>,
) -> Result<redact_event::v3::Response> {
	// The policy and the append live in `redact_event_as`, which the wbf
	// channel's `Stream/Abandon` calls too: one act, one implementation.
	let event_id = redact_event_as(
		&services,
		body.sender_user(),
		&body.room_id,
		&body.event_id,
		body.reason.clone(),
	)
	.await?;

	Ok(redact_event::v3::Response { event_id })
}
