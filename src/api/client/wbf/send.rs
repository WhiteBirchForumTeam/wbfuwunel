//! `Event/Send`: send one message event with its attachments declared, over
//! the wbf channel.
//!
//! Same rules as the legacy `PUT /rooms/{room}/send/{type}/{txn}` (one
//! function serves both); the difference is where the declaration travels:
//! here it is the `attachments` field of the meta, next to the room, type
//! and transaction id, and the event content is the pack's data. See
//! `docs/design/media-attachments.md` §3.

use ruma::{OwnedRoomId, OwnedTransactionId, UserId, events::MessageLikeEventType, serde::Raw};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tuwunel_core::wbf::{PackView, RejectCode};
use tuwunel_service::Services;

use super::{Reject, ack};
use crate::client::send::{SendMessageEvent, send_message_event};

/// The meta of an `Event/Send` request.
#[derive(Deserialize)]
struct SendMeta {
	room_id: OwnedRoomId,
	#[serde(rename = "type")]
	event_type: MessageLikeEventType,
	txn_id: OwnedTransactionId,
	#[serde(default)]
	attachments: Vec<String>,
}

/// Args:
///     user: the authenticated sender
///     view: meta example: `{"room_id":"!r:localhost","type":"m.room.encrypted",
///       "txn_id":"t1","attachments":["mxc://localhost/abc"]}`; data = the
///       event content as JSON
/// Return:
///     Result<Vec<u8>, Reject>  an `Ack` with meta `{"event_id": …}`;
///     `Conflict` for a malformed meta or content, and the send's own
///     refusals mapped like every other pack error.
pub(super) async fn handle_event_send(
	services: &Services,
	user: &UserId,
	view: &PackView<'_>,
) -> Result<Vec<u8>, Reject> {
	let meta: SendMeta = serde_json::from_slice(view.meta)
		.map_err(|error| Reject::code(RejectCode::InvalidRequest, format!("Event/Send meta: {error}")))?;

	let content: Box<RawValue> = serde_json::from_slice(view.data)
		.map_err(|error| Reject::code(RejectCode::InvalidRequest, format!("Event/Send data is not JSON: {error}")))?;
	let content = Raw::from_json(content);

	let event_id = send_message_event(services, SendMessageEvent {
		sender_user: user,
		sender_device: None,
		appservice_info: None,
		room_id: &meta.room_id,
		event_type: &meta.event_type,
		txn_id: &meta.txn_id,
		content: &content,
		timestamp: None,
		declared_attachments: meta.attachments,
		via_legacy_http: false,
	})
	.await?;

	Ok(ack(view.header.id, view.header.seq, json!({ "event_id": event_id }), Vec::new()))
}
