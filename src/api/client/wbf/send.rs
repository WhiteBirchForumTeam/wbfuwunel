//! `Event/Send`: send one message event with its attachments declared, over
//! the wbf channel.
//!
//! Same rules as the legacy `PUT /rooms/{room}/send/{type}/{txn}` (one
//! function serves both); the difference is where the declaration travels:
//! here it is the `attachments` field of the meta, next to the room, type
//! and transaction id, and the event content is the pack's data. See
//! `docs/design/media-attachments.md` §3.
//!
//! An encrypted send may also carry the room device version the sender
//! handed its room key out by, and is refused with `RoomDevicesChanged` when
//! the room's has moved since (`docs/design/wbf-room-device-version.md` §7).

use ruma::{OwnedRoomId, OwnedTransactionId, events::MessageLikeEventType, serde::Raw};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tuwunel_core::wbf::{PackView, RejectCode};
use tuwunel_service::Services;

use super::{PackContext, Reject, ack};
use crate::client::send::{SendMessageEvent, SendOutcome, send_message_event};

/// The meta of an `Event/Send` request.
#[derive(Deserialize)]
struct SendMeta {
	room_id: OwnedRoomId,
	#[serde(rename = "type")]
	event_type: MessageLikeEventType,
	txn_id: OwnedTransactionId,
	#[serde(default)]
	attachments: Vec<String>,
	#[serde(default)]
	room_version: Option<u64>,
}

/// Args:
///     ctx: the sender, and whether its connection declared device versions
///     view: meta example: `{"room_id":"!r:localhost","type":"m.room.encrypted",
///       "txn_id":"t1","attachments":["mxc://localhost/abc"],"room_version":81234}`;
///       data = the event content as JSON
/// Return:
///     Result<Vec<u8>, Reject>  an `Ack` with meta `{"event_id": …}`;
///     `RoomDevicesChanged` with the room's `room_version` when the one sent
///     is not it; `InvalidRequest` for a malformed meta or content, or an
///     encrypted send without `room_version` from a connection that declared
///     device versions; the send's own refusals mapped like every other pack
///     error.
pub(super) async fn handle_event_send(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
) -> Result<Vec<u8>, Reject> {
	let session = ctx.get_session()?;
	let meta: SendMeta = serde_json::from_slice(view.meta)
		.map_err(|error| Reject::code(RejectCode::InvalidRequest, format!("Event/Send meta: {error}")))?;

	let content: Box<RawValue> = serde_json::from_slice(view.data)
		.map_err(|error| Reject::code(RejectCode::InvalidRequest, format!("Event/Send data is not JSON: {error}")))?;
	let content = Raw::from_json(content);

	let expected_room_device_version = get_expected_room_device_version(
		meta.event_type == MessageLikeEventType::RoomEncrypted,
		meta.room_version,
		services
			.streams
			.is_device_versions_declared(ctx.connection),
	)?;

	let outcome = send_message_event(services, SendMessageEvent {
		sender_user: &session.user,
		// 🚨 The device, not `None`: the txn dedupe is keyed (user, device, txn),
		// so `None` keys it on the account and a second device reusing a
		// `txn_id` is handed the first one's `event_id` while its own event is
		// never written — and HTTP has always passed the device (issue #78).
		sender_device: Some(&session.device),
		appservice_info: None,
		room_id: &meta.room_id,
		event_type: &meta.event_type,
		txn_id: &meta.txn_id,
		content: &content,
		timestamp: None,
		declared_attachments: meta.attachments,
		via_legacy_http: false,
		may_write_reserved_type: false,
		expected_room_device_version,
	})
	.await?;

	match outcome {
		| SendOutcome::Sent(event_id) => Ok(ack(view.header.id, view.header.seq, json!({ "event_id": event_id }), Vec::new())),
		| SendOutcome::RoomDevicesChanged { room_device_version } => Err(room_devices_changed(room_device_version)),
	}
}

/// Args:
///     room_device_version: the room's version now, example: 81240
fn room_devices_changed(room_device_version: u64) -> Reject {
	Reject::with_extra(
		RejectCode::RoomDevicesChanged,
		"the room's members or their devices changed since this room_version: fetch the members again",
		json!({ "room_version": room_device_version }),
	)
}

/// §7.1: only an encrypted send is checked, and it is checked whenever it
/// carries a version; a connection that declared device versions must carry
/// one, so leaving it out cannot skip the check.
///
/// Args:
///     is_encrypted: the event type is `m.room.encrypted`
///     room_version: what the meta carried, example: Some(81234)
///     is_declared: the connection's `Hello` declared device versions
/// Return:
///     Result<Option<u64>, Reject>  the version to check against, None for no
///     check; `InvalidRequest` for a declared connection's encrypted send
///     without one.
fn get_expected_room_device_version(
	is_encrypted: bool,
	room_version: Option<u64>,
	is_declared: bool,
) -> Result<Option<u64>, Reject> {
	match (is_encrypted, room_version) {
		| (false, _) => Ok(None),
		| (true, Some(version)) => Ok(Some(version)),
		| (true, None) if is_declared => Err(Reject::code(
			RejectCode::InvalidRequest,
			"this connection declared org.wbftw.device_versions: an encrypted Event/Send must carry room_version",
		)),
		| (true, None) => Ok(None),
	}
}

#[cfg(test)]
mod tests {
	use super::{get_expected_room_device_version, room_devices_changed};

	/// Clients test their decoders against the golden vectors, so the 1506
	/// vector must be the bytes this server sends.
	#[test]
	fn the_room_devices_changed_vector_is_what_the_server_builds() {
		const VECTORS: &str = include_str!("../../../../docs/design/wbf-vectors.json");
		let vectors: serde_json::Value = serde_json::from_str(VECTORS).expect("the vectors file is JSON");
		let hex = vectors["packs"]
			.as_array()
			.expect("a packs list")
			.iter()
			.find(|vector| vector["name"] == "error_room_devices_changed")
			.expect("the 1506 vector")["bytes_hex"]
			.as_str()
			.expect("hex")
			.to_owned();
		let documented: Vec<u8> = (0..hex.len())
			.step_by(2)
			.map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex digit"))
			.collect();

		assert_eq!(room_devices_changed(81240).into_pack(0, 19), documented);
	}

	#[test]
	fn an_undeclared_connection_without_a_version_is_not_checked() {
		assert_eq!(get_expected_room_device_version(true, None, false).ok(), Some(None));
	}

	#[test]
	fn a_version_is_checked_whether_or_not_the_connection_declared() {
		assert_eq!(get_expected_room_device_version(true, Some(7), false).ok(), Some(Some(7)));
		assert_eq!(get_expected_room_device_version(true, Some(7), true).ok(), Some(Some(7)));
	}

	#[test]
	fn a_declared_connection_cannot_skip_the_check_by_leaving_the_version_out() {
		assert!(get_expected_room_device_version(true, None, true).is_err());
	}

	#[test]
	fn a_plaintext_send_is_never_checked() {
		assert_eq!(get_expected_room_device_version(false, Some(7), true).ok(), Some(None));
		assert_eq!(get_expected_room_device_version(false, None, true).ok(), Some(None));
	}
}
