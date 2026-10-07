//! `Room/InvitedRooms` (`0x13`/`0x01`): the invites that are pending right now.
//!
//! ⭐ Why this is native rather than bridged: the data it answers with lives
//! only in `/sync`'s `rooms.invite`, so there is no Matrix endpoint to bridge to
//! (/docs/design/events/invites-on-the-wire.md §2).
//!
//! 🚨 **Why it has to exist at all**: a push is a hint, not a guarantee
//! (/docs/design/events/event-push.md §2.1), so an invite that arrived while the
//! client was away has to be catchable afterwards. Without it `Event/Invited`
//! would be the only source, and a missed one would be missed for good.

use futures::StreamExt;
use ruma::{OwnedRoomId, RoomId, UserId, events::AnyStrippedStateEvent, serde::Raw};
use serde_json::{Map, Value, json};
use tuwunel_core::{Result, wbf::events::length_prefixed};
use tuwunel_service::Services;

use super::{Reject, RejectCode};

/// Args:
///     services: for `state_cache`
///     user: whose pending invites, example: "@bob:localhost"
/// Return:
///     Result<(Value, Vec<u8>), Reject>  the meta (`{ "rc": n }`) and the data
///     (`n` length-prefixed JSON objects, one per room); `Internal` only when
///     the framing of a room overflows a length prefix.
pub(super) async fn list_pending_invites(
	services: &Services,
	user: &UserId,
) -> Result<(Value, Vec<u8>), Reject> {
	let invites: Vec<(OwnedRoomId, Vec<Raw<AnyStrippedStateEvent>>)> = services
		.state_cache
		.rooms_invited_state(user)
		.collect()
		.await;

	// One room is one length-prefixed item, state included, so a client never
	// has to work out which stripped events belong to which room
	// (/docs/design/events/invites-on-the-wire.md §3.3).
	let entries: Vec<Vec<u8>> = invites
		.iter()
		.map(|(room, state)| to_entry(room, state, user).to_string().into_bytes())
		.collect();

	let data = length_prefixed(entries.iter().map(Vec::as_slice))
		.map_err(|_| Reject::code(RejectCode::Internal, "a pending invite does not fit a length prefix"))?;

	Ok((json!({ "rc": invites.len() }), data))
}

/// One room's entry: the stripped state as stored, plus the three fields a
/// client would otherwise have to dig out of it.
///
/// ⭐ Those three are **derived from the stored state**, not from a second
/// source: the invite's own `m.room.member` event is part of `invite_state`, so
/// reading them from there is what makes this answer and `Event/Invited` say the
/// same thing.
///
/// Args:
///     room: example: "!r:localhost"
///     state: the stripped state as `invite_state` stored it
///     user: the invited user, whose member event carries the three fields
/// Return:
///     Value  `{ "room_id", "inviter"?, "is_direct"?, "reason"?, "state": [...] }`
fn to_entry(room: &RoomId, state: &[Raw<AnyStrippedStateEvent>], user: &UserId) -> Value {
	let mut entry = Map::new();
	entry.insert("room_id".into(), json!(room));

	if let Some(member) = find_own_member_event(state, user) {
		for (from, to) in [("sender", "inviter")] {
			if let Some(value) = member.get(from) {
				entry.insert(to.into(), value.clone());
			}
		}
		if let Some(content) = member.get("content").and_then(Value::as_object) {
			for field in ["is_direct", "reason"] {
				if let Some(value) = content.get(field) {
					entry.insert(field.into(), value.clone());
				}
			}
		}
	}

	entry.insert(
		"state".into(),
		Value::Array(
			state
				.iter()
				.filter_map(|event| serde_json::from_str::<Value>(event.json().get()).ok())
				.collect(),
		),
	);

	Value::Object(entry)
}

/// The invited user's own `m.room.member` event out of the stripped state, which
/// is where the inviter and the invite's own fields are.
///
/// Return:
///     Option<Map<String, Value>>  None when the stripped state does not carry
///     it — ⚠️ possible, because what goes into `invite_state` is decided
///     elsewhere, so the three derived fields are left out rather than guessed.
fn find_own_member_event(state: &[Raw<AnyStrippedStateEvent>], user: &UserId) -> Option<Map<String, Value>> {
	state
		.iter()
		.filter_map(|event| serde_json::from_str::<Value>(event.json().get()).ok())
		.filter_map(|event| match event {
			| Value::Object(event) => Some(event),
			| _ => None,
		})
		.find(|event| {
			event.get("type").and_then(Value::as_str) == Some("m.room.member")
				&& event.get("state_key").and_then(Value::as_str) == Some(user.as_str())
		})
}
