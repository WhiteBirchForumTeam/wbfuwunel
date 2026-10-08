//! What a pending invite looks like on the wire, and the two pure functions
//! both directions share: reading the invite's own fields out of the stripped
//! state, and trimming a stripped state that arrived from another server.
//!
//! ⭐ Why both directions read the **stored** stripped state rather than the
//! values a caller is holding: the federated path hands `update_membership` a
//! blank `RoomMemberEventContent`, so a caller's `is_direct` and `reason` are
//! lost there — while the real member event is in the state it stored
//! (/docs/design/events/invites-on-the-wire.md §5.2).

use serde_json::{Map, Value};

/// Bytes a single stripped state event may take before we refuse to store it:
/// the Matrix limit on one event. Anything larger was never a legal state
/// event, so it cannot be a legal stripped copy of one either
/// (/docs/design/events/invites-on-the-wire.md §6).
pub const STRIPPED_EVENT_LEN_MAX: usize = 65536;

/// Stripped state events one invite may carry after trimming. The recommended
/// cells plus both member events leave room to spare; with
/// `STRIPPED_EVENT_LEN_MAX` this is what bounds an invite's bytes, so no
/// separate byte setting is needed (same doc, §6 rule ③).
pub const STRIPPED_EVENT_COUNT_MAX: usize = 16;

/// The `type`s a stripped state event may keep. Matrix calls these the
/// recommended ones, and they are exactly what this server sends out itself
/// (`rooms::state::summary_stripped`).
pub const STRIPPED_TYPES_KEPT: [&str; 8] = [
	"m.room.create",
	"m.room.join_rules",
	"m.room.canonical_alias",
	"m.room.name",
	"m.room.avatar",
	"m.room.encryption",
	"m.room.topic",
	"m.room.member",
];

/// The three fields a client would otherwise have to dig out of the stripped
/// state itself.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InviteFields {
	/// Who sent the invite: the member event's `sender`.
	pub inviter: Option<String>,
	/// The invite's `is_direct`; false when it said nothing.
	pub is_direct: bool,
	/// The invite's `reason`, when it carried one.
	pub reason: Option<String>,
}

/// Args:
///     state: each stripped state event's JSON, in stored order
///     user: the invited user, example: "@bob:localhost"
/// Return:
///     InviteFields  all three derived from the invited user's **own**
///     `m.room.member` event; every field empty/false when the state does not
///     carry it.
///
/// 🚨 The state holds **two** member events — the inviter's and the invited
/// user's — so this matches on `state_key`. Taking the first `m.room.member`
/// reads the wrong person's event
/// (/docs/design/events/invites-on-the-wire.md §5.2).
#[must_use]
pub fn get_invite_fields<'a, I>(state: I, user: &str) -> InviteFields
where
	I: IntoIterator<Item = &'a [u8]>,
{
	let Some(member) = find_own_member_event(state, user) else {
		return InviteFields::default();
	};

	let content = member
		.get("content")
		.and_then(Value::as_object);

	InviteFields {
		inviter: member
			.get("sender")
			.and_then(Value::as_str)
			.map(ToOwned::to_owned),
		is_direct: content
			.and_then(|content| content.get("is_direct"))
			.and_then(Value::as_bool)
			.unwrap_or(false),
		reason: content
			.and_then(|content| content.get("reason"))
			.and_then(Value::as_str)
			.map(ToOwned::to_owned),
	}
}

/// The invited user's own `m.room.member` event out of a stripped state.
///
/// Args:
///     state: each stripped state event's JSON, in stored order
///     user: the invited user, whose `state_key` the event carries
/// Return:
///     Option<Map<String, Value>>  None when the state does not carry it, or
///     when what it carries is not a JSON object.
fn find_own_member_event<'a, I>(state: I, user: &str) -> Option<Map<String, Value>>
where
	I: IntoIterator<Item = &'a [u8]>,
{
	state
		.into_iter()
		.filter_map(|event| serde_json::from_slice::<Value>(event).ok())
		.filter_map(|event| match event {
			| Value::Object(event) => Some(event),
			| _ => None,
		})
		.find(|event| {
			event.get("type").and_then(Value::as_str) == Some("m.room.member")
				&& event.get("state_key").and_then(Value::as_str) == Some(user)
		})
}

/// Whether one stripped state event is worth storing: a recommended type,
/// and no larger than a legal event.
///
/// Args:
///     event: the stripped state event's JSON
/// Return:
///     bool  false for anything oversized, unparseable, not an object, of a
///     type outside `STRIPPED_TYPES_KEPT`, or without a `state_key` (a
///     stripped **state** event always has one).
///
/// ⚠️ Fails closed: whatever cannot be read is not kept. What arrives here is
/// another server's choice of contents and nothing bounds it
/// (/docs/design/events/invites-on-the-wire.md §6).
#[must_use]
pub fn is_stripped_event_kept(event: &[u8]) -> bool {
	if event.len() > STRIPPED_EVENT_LEN_MAX {
		return false;
	}

	let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(event) else {
		return false;
	};

	if event.get("state_key").and_then(Value::as_str).is_none() {
		return false;
	}

	event
		.get("type")
		.and_then(Value::as_str)
		.is_some_and(|event_type| STRIPPED_TYPES_KEPT.contains(&event_type))
}

#[cfg(test)]
mod tests {
	use super::{
		InviteFields, STRIPPED_EVENT_LEN_MAX, get_invite_fields, is_stripped_event_kept,
	};

	fn member(state_key: &str, sender: &str, content: &str) -> Vec<u8> {
		format!(
			r#"{{"type":"m.room.member","state_key":"{state_key}","sender":"{sender}","content":{content}}}"#
		)
		.into_bytes()
	}

	/// 🚨 The state carries the inviter's member event as well as the invited
	/// user's, and the inviter's comes first (`summary_stripped` fetches the
	/// recommended cells before appending the invite itself). Matching on
	/// `type` alone reads the inviter's membership as if it were the invite.
	#[test]
	fn the_fields_come_from_the_invited_users_own_member_event() {
		let state = [
			br#"{"type":"m.room.name","state_key":"","content":{"name":"r"}}"#.to_vec(),
			member("@alice:localhost", "@alice:localhost", r#"{"membership":"join"}"#),
			member(
				"@bob:localhost",
				"@alice:localhost",
				r#"{"membership":"invite","is_direct":true,"reason":"come in"}"#,
			),
		];

		let fields = get_invite_fields(state.iter().map(Vec::as_slice), "@bob:localhost");

		assert_eq!(fields, InviteFields {
			inviter: Some("@alice:localhost".to_owned()),
			is_direct: true,
			reason: Some("come in".to_owned()),
		});
	}

	#[test]
	fn a_state_without_the_member_event_derives_nothing_rather_than_guessing() {
		let state = [br#"{"type":"m.room.name","state_key":"","content":{"name":"r"}}"#.to_vec()];

		let fields = get_invite_fields(state.iter().map(Vec::as_slice), "@bob:localhost");

		assert_eq!(fields, InviteFields::default());
		assert!(!fields.is_direct, "a missing invite is not a direct one");
	}

	#[test]
	fn an_invite_that_said_nothing_is_not_direct_and_has_no_reason() {
		let state = [member("@bob:localhost", "@alice:localhost", r#"{"membership":"invite"}"#)];

		let fields = get_invite_fields(state.iter().map(Vec::as_slice), "@bob:localhost");

		assert_eq!(fields.inviter.as_deref(), Some("@alice:localhost"));
		assert!(!fields.is_direct);
		assert_eq!(fields.reason, None);
	}

	#[test]
	fn the_recommended_types_are_kept_and_the_rest_are_not() {
		assert!(is_stripped_event_kept(
			br#"{"type":"m.room.name","state_key":"","content":{}}"#
		));
		assert!(is_stripped_event_kept(
			br#"{"type":"m.room.member","state_key":"@bob:localhost","content":{}}"#
		));
		assert!(
			!is_stripped_event_kept(br#"{"type":"m.room.power_levels","state_key":"","content":{}}"#),
			"a type outside the recommended set is not worth storing"
		);
	}

	#[test]
	fn anything_unreadable_or_oversized_is_dropped() {
		assert!(!is_stripped_event_kept(b"not json"));
		assert!(!is_stripped_event_kept(br#"["an array is not a state event"]"#));
		assert!(
			!is_stripped_event_kept(br#"{"type":"m.room.name","content":{}}"#),
			"a stripped state event always has a state_key"
		);

		let padding = "x".repeat(STRIPPED_EVENT_LEN_MAX);
		let oversized = format!(r#"{{"type":"m.room.topic","state_key":"","content":{{"topic":"{padding}"}}}}"#);
		assert!(
			!is_stripped_event_kept(oversized.as_bytes()),
			"larger than a legal event, so it was never a legal stripped copy of one"
		);
	}
}
