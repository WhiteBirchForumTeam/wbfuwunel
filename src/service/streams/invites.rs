//! The pending-invite stream (`0x05 Invite`): one topic per user, and the two
//! packs the server pushes on it
//! (/docs/design/events/invites-on-the-wire.md).
//!
//! ⭐ Why this is its own stream rather than a subtype of the room channels:
//! an invite has to be **caught up completely**, while a room's timeline is
//! caught up a window at a time and may legitimately never finish. One
//! subscription cannot promise both, and sharing the room channels' `gap`
//! would make "an invite was dropped" indistinguishable from "a message was
//! dropped" (same doc, §4).

use ruma::{OwnedUserId, RoomId, UserId};
use serde_json::{Map, Value, json};

use tuwunel_core::{
	debug, debug_warn,
	wbf::{Flags, Kind, PackBuilder, PackError, events::length_prefixed},
};

use super::{ConnectionId, PackQueue, SessionEnded, Streams};

/// `Invite/Push`, server to client only.
pub const INVITE_PUSH_SUBTYPE: u8 = 0x06;

/// `Invite/Gone`, server to client only.
pub const INVITE_GONE_SUBTYPE: u8 = 0x07;

/// Whose invites. One value, but a type of its own so the registry cannot be
/// keyed by a user from another stream by accident.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct InviteTopic(OwnedUserId);

/// One pending invite as it goes out, already read back from storage.
pub struct PushedInvite<'a> {
	/// The `g_seq` of the member event that created it.
	pub count: u64,
	/// The stripped state as `invite_state` stored it, each item serialized.
	pub state: &'a [Vec<u8>],
}

impl Streams {
	/// Subscribes `connection` to its own user's pending invites.
	///
	/// ⚠️ Unlike the to-device queue this is `Occupancy::Many`: every
	/// connection of a user may hold it at once, because nothing here is
	/// destroyed by reading it — two connections both learning about the same
	/// invite is harmless, while two connections both destroying to-device
	/// items is not (/docs/design/keys/to-device.md).
	///
	/// Args:
	///     connection: example: 7
	///     user: whose invites, from the session
	///     queue: the connection's send queue
	///     id: the client's `Subscribe` id, example: 42
	/// Return:
	///     Result<(), SessionEnded>  SessionEnded when this connection's
	///     session ended while the request was in flight.
	pub fn subscribe_invites(
		&self,
		connection: ConnectionId,
		user: &UserId,
		queue: PackQueue,
		id: u64,
	) -> Result<(), SessionEnded> {
		let topic = InviteTopic(user.to_owned());
		self.invites
			.subscribe(connection, user, queue, id, &[topic])
			.map(|_| ())
			.ok_or(SessionEnded)
	}

	/// Releases this connection's invite subscription. Both ways out must do
	/// this — `Unsubscribe` and the connection ending.
	pub fn unsubscribe_invites(&self, connection: ConnectionId) {
		self.invites.remove_connection(connection);
	}

	/// Return:
	///     bool  whether any connection is subscribed to this user's invites.
	///
	/// ⭐ The caller asks this **before** reading the invite's stripped state
	/// back out of the database: most invites go to nobody who is listening —
	/// remote users, and anyone offline — and that read copies the whole state
	/// (external review 2026-10-08, oliver).
	///
	/// ⚠️ Racy by nature: a connection subscribing right after this returns
	/// false misses the push and catches up with `Invite/Fetch`, which it owes
	/// itself on every connect anyway.
	#[must_use]
	pub fn is_listened_for_invites(&self, user: &UserId) -> bool {
		!self
			.invites
			.listeners(&InviteTopic(user.to_owned()))
			.is_empty()
	}

	/// Tells this user's invite subscriptions that `room` has invited them.
	///
	/// Args:
	///     user: who was invited, example: "@bob:localhost"
	///     room: example: "!r:localhost"
	///     invite: its count and the stripped state as stored
	///     data_max: `wbf_data_max_bytes`
	///     meta_max: `wbf_meta_max_bytes`
	///
	/// 🚨 A state that does not fit is sent as **no state** rather than not
	/// sent at all: "somebody invited you" matters more than the room's name,
	/// and `Invite/Fetch` can still answer with it
	/// (/docs/design/events/invites-on-the-wire.md §5.3).
	pub fn push_invited(
		&self,
		user: &UserId,
		room: &RoomId,
		invite: &PushedInvite<'_>,
		data_max: usize,
		meta_max: usize,
	) {
		let topic = InviteTopic(user.to_owned());
		let connections: Vec<ConnectionId> = self
			.invites
			.listeners(&topic)
			.into_iter()
			.map(|(connection, _)| connection)
			.collect();

		if connections.is_empty() {
			return;
		}

		let fields = tuwunel_core::wbf::get_invite_fields(
			invite.state.iter().map(Vec::as_slice),
			user.as_str(),
		);
		let data = framed_state(user, room, invite.state, data_max);
		let count = if data.is_empty() { 0 } else { invite.state.len() };

		self.invites
			.push_with(Some(&topic), &connections, |id, seq, gap| {
				let meta = invited_meta(room, &fields, invite.count, count, gap, meta_max);
				invited_pack(id, seq, &meta, &data)
			});
	}

	/// Tells this user's invite subscriptions that an invite is no longer
	/// pending, and what replaced it.
	///
	/// Args:
	///     user: who was invited, example: "@bob:localhost"
	///     room: example: "!r:localhost"
	///     membership: the state that ended it, example: "leave" (withdrawn or
	///         declined), "ban", "join" (accepted), "knock"
	///     count: the `g_seq` of the member event that ended it
	pub fn push_invite_gone(&self, user: &UserId, room: &RoomId, membership: &str, count: u64) {
		let topic = InviteTopic(user.to_owned());
		let connections: Vec<ConnectionId> = self
			.invites
			.listeners(&topic)
			.into_iter()
			.map(|(connection, _)| connection)
			.collect();

		if connections.is_empty() {
			return;
		}

		self.invites
			.push_with(Some(&topic), &connections, |id, seq, gap| {
				invite_gone_pack(
					id,
					seq,
					&json!({ "room_id": room, "membership": membership, "is": count, "gap": gap }),
				)
			});
	}
}

/// The data section of an `Invite/Push`: the stripped state, length-prefixed.
///
/// Args:
///     data_max: `wbf_data_max_bytes`
/// Return:
///     Vec<u8>  empty when the state does not fit one pack or cannot be
///     framed, which the caller reports as `sc: 0`.
///
/// ⚠️ The size is checked here rather than trusted to the framing: framing
/// only fails past `u32::MAX`, which is three orders of magnitude above the
/// limit this server announces in its `Hello` (external review 2026-10-08,
/// oliver).
fn framed_state(user: &UserId, room: &RoomId, state: &[Vec<u8>], data_max: usize) -> Vec<u8> {
	let framed = match length_prefixed(state.iter().map(Vec::as_slice)) {
		| Ok(framed) => framed,
		| Err(error) => {
			debug_warn!(%user, %room, "wbf invite push: the stripped state does not frame ({error}); sending none");
			return Vec::new();
		},
	};

	if framed.len() > data_max {
		debug_warn!(
			%user, %room,
			len = framed.len(),
			data_max,
			"wbf invite push: the stripped state exceeds wbf_data_max_bytes; sending none"
		);
		return Vec::new();
	}

	framed
}

/// The meta of an `Invite/Push`. `inviter` and `reason` are left out when the
/// invite carried none, rather than sent as null.
///
/// Args:
///     state_count: how many stripped events the data really holds
///     meta_max: `wbf_meta_max_bytes`
///
/// 🚨 `reason` is free text the inviter wrote, and it is bounded only by the
/// size of one event — which is the same order as `wbf_meta_max_bytes`. A meta
/// that would not fit drops it **whole**: a reason cut in half and presented
/// as the reason is a lie, and the full text is still in the stripped state.
fn invited_meta(
	room: &RoomId,
	fields: &tuwunel_core::wbf::InviteFields,
	count: u64,
	state_count: usize,
	gap: bool,
	meta_max: usize,
) -> Value {
	let mut meta = Map::new();
	meta.insert("room_id".into(), json!(room));
	if let Some(inviter) = fields.inviter.as_deref() {
		meta.insert("inviter".into(), json!(inviter));
	}
	meta.insert("is_direct".into(), json!(fields.is_direct));
	if let Some(reason) = fields.reason.as_deref() {
		meta.insert("reason".into(), json!(reason));
	}
	meta.insert("is".into(), json!(count));
	meta.insert("sc".into(), json!(state_count));
	meta.insert("gap".into(), json!(gap));

	let mut meta = Value::Object(meta);
	if is_over_meta_max(&meta, meta_max) {
		if let Some(object) = meta.as_object_mut() {
			object.remove("reason");
		}
		debug!(%room, meta_max, "wbf invite push: the reason does not fit the meta; sending it without one");
	}

	meta
}

/// Return:
///     bool  whether this meta would be larger than `wbf_meta_max_bytes` on
///     the wire; false when it cannot be serialized at all, because then the
///     pack builder is what refuses it and dropping a field would not help.
fn is_over_meta_max(meta: &Value, meta_max: usize) -> bool {
	serde_json::to_vec(meta).is_ok_and(|bytes| bytes.len() > meta_max)
}

fn invited_pack(id: u64, seq: u32, meta: &Value, data: &[u8]) -> Result<Vec<u8>, PackError> {
	Ok(PackBuilder::new(Kind::Invite, INVITE_PUSH_SUBTYPE, Flags::IS_RESPONSE, id, seq)
		.json_meta(meta)?
		.data(data)?
		.finish())
}

fn invite_gone_pack(id: u64, seq: u32, meta: &Value) -> Result<Vec<u8>, PackError> {
	Ok(PackBuilder::new(Kind::Invite, INVITE_GONE_SUBTYPE, Flags::IS_RESPONSE, id, seq)
		.json_meta(meta)?
		.finish())
}

#[cfg(test)]
mod tests {
	use ruma::{room_id, user_id};
	use serde_json::Value;
	use tuwunel_core::wbf::InviteFields;

	use super::{framed_state, invited_meta, is_over_meta_max};

	fn fields() -> InviteFields {
		InviteFields {
			inviter: Some("@alice:localhost".to_owned()),
			is_direct: true,
			reason: Some("come in".to_owned()),
		}
	}

	#[test]
	fn a_state_over_the_data_limit_is_sent_as_none() {
		let state = vec![vec![b'x'; 4096], vec![b'y'; 4096]];

		assert!(
			framed_state(user_id!("@bob:localhost"), room_id!("!r:localhost"), &state, 64).is_empty(),
			"over wbf_data_max_bytes, so the push carries sc: 0 rather than an oversized pack"
		);
		assert!(
			!framed_state(user_id!("@bob:localhost"), room_id!("!r:localhost"), &state, 1 << 20)
				.is_empty(),
			"under the limit it frames as usual"
		);
	}

	/// 🚨 The reason is the inviter's own text, so this is the one meta field
	/// an outsider sizes. Dropping it keeps the invite itself deliverable.
	#[test]
	fn a_reason_that_does_not_fit_the_meta_is_dropped_whole() {
		let long = InviteFields { reason: Some("r".repeat(4096)), ..fields() };

		let meta = invited_meta(room_id!("!r:localhost"), &long, 7, 3, false, 512);

		assert_eq!(meta.get("reason"), None, "dropped rather than truncated");
		assert_eq!(meta.get("inviter").and_then(Value::as_str), Some("@alice:localhost"));
		assert_eq!(meta.get("sc").and_then(Value::as_u64), Some(3));
		assert_eq!(meta.get("is").and_then(Value::as_u64), Some(7));
		assert!(!is_over_meta_max(&meta, 512), "and what is left really does fit");
	}

	#[test]
	fn an_ordinary_invite_keeps_every_field() {
		let meta = invited_meta(room_id!("!r:localhost"), &fields(), 7, 3, true, 65536);

		assert_eq!(meta.get("reason").and_then(Value::as_str), Some("come in"));
		assert_eq!(meta.get("is_direct").and_then(Value::as_bool), Some(true));
		assert_eq!(meta.get("gap").and_then(Value::as_bool), Some(true));
	}

	/// An invite that carried neither leaves both out rather than sending
	/// nulls, but `is_direct` is always present: false is an answer.
	#[test]
	fn the_optional_fields_are_absent_rather_than_null() {
		let bare = InviteFields::default();

		let meta = invited_meta(room_id!("!r:localhost"), &bare, 1, 0, false, 65536);

		assert_eq!(meta.get("inviter"), None);
		assert_eq!(meta.get("reason"), None);
		assert_eq!(meta.get("is_direct").and_then(Value::as_bool), Some(false));
		assert_eq!(meta.get("sc").and_then(Value::as_u64), Some(0));
	}
}
