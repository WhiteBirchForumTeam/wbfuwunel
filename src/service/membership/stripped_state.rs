use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedRoomId, RoomId, RoomVersionId, UserId,
	api::federation::membership::RawStrippedState,
	events::AnyStrippedStateEvent,
	room_version_rules::RoomIdFormatVersion,
	serde::{JsonObject, Raw},
};
use serde_json::Value as JsonValue;
use tuwunel_core::{
	Event, PduEvent, Result, implement,
	matrix::event::gen_event_id,
	wbf::invites::{STRIPPED_EVENT_COUNT_MAX, is_stripped_event_kept},
};

use super::Service;

/// MSC4311 verdict for the create event carried in federated stripped state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrippedCreateVerdict {
	/// A full create PDU bound to the room with valid signatures.
	Valid,

	/// No `m.room.create` event was present.
	Missing,

	/// A create event was present only in the legacy stripped form.
	NotPdu,

	/// A create PDU was present but does not bind to the room.
	WrongRoom,

	/// A create PDU was present but failed signature or hash checks.
	BadSignature,
}

/// Whether a non-`Valid` verdict warrants rejecting an invite or dropping the
/// event from knock state, given the room version and operator policy.
#[must_use]
pub fn enforce_stripped_create(
	verdict: StrippedCreateVerdict,
	v12_room_ids: bool,
	enforce: bool,
) -> bool {
	use StrippedCreateVerdict::*;

	match verdict {
		| Valid => false,
		// A complete create PDU bound to a different room must fail for v12+
		// rooms even during the migration window (MSC4311 Migration).
		| WrongRoom => v12_room_ids || enforce,
		| Missing | NotPdu | BadSignature => enforce,
	}
}

/// Cuts a stripped state that came from another server down to what is worth
/// storing: the recommended types, nothing larger than a legal event, and at
/// most `STRIPPED_EVENT_COUNT_MAX` of them.
///
/// Args:
///     state: the down-converted entries, in the order the sender gave them
/// Return:
///     Vec<Raw<AnyStrippedStateEvent>>  the kept ones, order preserved.
///
/// 🚨 **Nothing else bounds this.** What a remote server puts in
/// `invite_room_state` (or a knock's state) is its own choice, and the only
/// limit above this is the HTTP body — 24 MiB by default — so one invite could
/// otherwise store 24 MiB per invited user, reach `/sync` as a single blob, and
/// make every wbf pack for that room degrade
/// (/docs/design/events/invites-on-the-wire.md §6).
///
/// ⚠️ Trimmed rather than refused: a bloated stripped state is the sender's
/// doing, and refusing the invite would punish the person being invited.
///
/// 🔴 Callers run the MSC4311 create-event validation on the **untrimmed**
/// input first. Trimming before it would quietly weaken that check.
#[must_use]
pub fn list_stripped_state_kept(
	state: Vec<Raw<AnyStrippedStateEvent>>,
) -> Vec<Raw<AnyStrippedStateEvent>> {
	state
		.into_iter()
		.filter(|event| is_stripped_event_kept(event.json().get().as_bytes()))
		.take(STRIPPED_EVENT_COUNT_MAX)
		.collect()
}

/// What an **invite** keeps of the stripped state its sender supplied: the
/// trim every path does, and then the sender's copy of the invited user's own
/// member event dropped.
///
/// Args:
///     state: the down-converted entries, in the order the sender gave them
///     invited_user: whose member event the caller appends itself,
///         example: "@bob:localhost"
///
/// Return:
///     Vec<Raw<AnyStrippedStateEvent>>  the kept ones, order preserved; the
///     caller appends its own member PDU after.
///
/// ⭐ **The two steps are one function because the second is the one that gets
/// forgotten.** A caller able to trim without dropping would compile, pass
/// every test, and reopen the hole described below — and the forgery only
/// exists on the federated path, which neither e2e nor a unit test of that
/// handler can reach, so nothing else would notice.
/// 📎 Knock state calls `list_stripped_state_kept` on its own: that path does
/// **not** append its own copy, so dropping the knocker's member event there
/// would simply lose it.
#[must_use]
pub fn list_invite_stripped_state(
	state: Vec<Raw<AnyStrippedStateEvent>>,
	invited_user: &UserId,
) -> Vec<Raw<AnyStrippedStateEvent>> {
	list_stripped_state_without_member_of(list_stripped_state_kept(state), invited_user)
}

/// Drops the `m.room.member` events a **sender** supplied about `subject`,
/// for a caller that appends its own authoritative copy afterwards.
///
/// Args:
///     state: already through `list_stripped_state_kept`
///     subject: whose member event the caller is about to append,
///         example: "@bob:localhost"
///
/// Return:
///     Vec<Raw<AnyStrippedStateEvent>>  the rest, order preserved.
///
/// 🚨 **Without this, a remote server decides who invited you.** The invite's
/// `inviter`, `is_direct` and `reason` are read back out of the stored stripped
/// state, and the sender's copy sits **before** ours — so a forged
/// `m.room.member` for the invited user is the one a reader finds
/// (external review 2026-10-09, oliver and salvia).
///
/// ⭐ The invariant this establishes is what the readers rely on: after it,
/// the only member event in stored state naming `subject` is the one we built
/// from the PDU we verified (/docs/design/events/invites-on-the-wire.md §5.2).
/// 📎 A positional rule ("take the last one") cannot replace it: `chain`'s
/// order is an implementation detail of each call site, and the next call site
/// would have to remember it.
#[must_use]
fn list_stripped_state_without_member_of(
	state: Vec<Raw<AnyStrippedStateEvent>>,
	subject: &UserId,
) -> Vec<Raw<AnyStrippedStateEvent>> {
	state
		.into_iter()
		.filter(|event| !is_member_event_of(event.json().get().as_bytes(), subject.as_str()))
		.collect()
}

/// Return:
///     bool  whether this stripped state event is an `m.room.member` whose
///     `state_key` is `subject`; false for anything that does not parse, which
///     `list_stripped_state_kept` has already dropped.
fn is_member_event_of(event: &[u8], subject: &str) -> bool {
	serde_json::from_slice::<JsonValue>(event).is_ok_and(|event| {
		let field = |key| {
			event
				.get(key)
				.and_then(JsonValue::as_str)
				.map(ToOwned::to_owned)
		};

		field("type").as_deref() == Some("m.room.member")
			&& field("state_key").as_deref() == Some(subject)
	})
}

/// Whether the room version derives room ids from the create event hash
/// (MSC4291, room version 12 and above), which changes how a create event
/// binds to its room.
#[must_use]
pub fn v12_room_ids(room_version: &RoomVersionId) -> bool {
	room_version
		.rules()
		.is_some_and(|rules| matches!(rules.room_id_format, RoomIdFormatVersion::V2))
}

/// Down-convert a federation stripped-state entry to the 4-field client shape,
/// reducing a full PDU to content, sender, optional state_key, and type.
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
#[must_use]
pub fn into_client_stripped(
	room_id: &RoomId,
	state: RawStrippedState,
) -> Option<Raw<AnyStrippedStateEvent>> {
	match state {
		| RawStrippedState::Stripped(raw) => Some(raw),
		| RawStrippedState::Pdu(raw) => {
			let mut event: JsonObject = serde_json::from_str(raw.get()).ok()?;

			// PduEvent requires event_id and room_id; a v12 create PDU federates
			// with neither, and to_format() drops both from the stripped shape.
			event.insert("event_id".into(), "$placeholder".into());
			event
				.entry("room_id")
				.or_insert_with(|| room_id.as_str().into());

			let pdu: PduEvent = serde_json::from_value(event.into()).ok()?;

			Some(pdu.to_format())
		},
	}
}

/// Validate the `m.room.create` event in a federated invite's or knock's
/// stripped state against the stated room (MSC4311). Decision-free: callers map
/// the verdict to their own reject-or-warn policy.
#[implement(Service)]
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
#[tracing::instrument(level = "debug", skip_all, fields(%room_id))]
pub async fn validate_stripped_create(
	&self,
	state: &[RawStrippedState],
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
) -> Result<StrippedCreateVerdict> {
	let create = state.iter().find_map(|event| match event {
		| RawStrippedState::Pdu(raw) => serde_json::from_str::<CanonicalJsonObject>(raw.get())
			.ok()
			.filter(is_create),
		| RawStrippedState::Stripped(_) => None,
	});

	let Some(mut create) = create else {
		let stripped = state.iter().any(|event| match event {
			| RawStrippedState::Stripped(raw) =>
				serde_json::from_str::<CanonicalJsonObject>(raw.json().get())
					.is_ok_and(|json| is_create(&json)),
			| RawStrippedState::Pdu(_) => false,
		});

		return Ok(match stripped {
			| true => StrippedCreateVerdict::NotPdu,
			| false => StrippedCreateVerdict::Missing,
		});
	};

	create.remove("unsigned");

	// Room-id binding: v12+ rooms hash the create event (MSC4291); earlier
	// versions compare the create event's room_id field.
	let bound = if v12_room_ids(room_version_id) {
		gen_event_id(&create, room_version_id)
			.ok()
			.and_then(|event_id| OwnedRoomId::from_parts('!', event_id.localpart(), None).ok())
			.is_some_and(|expected| expected == room_id)
	} else {
		create
			.get("room_id")
			.and_then(CanonicalJsonValue::as_str)
			.is_some_and(|id| id == room_id.as_str())
	};

	if !bound {
		return Ok(StrippedCreateVerdict::WrongRoom);
	}

	if self
		.services
		.server_keys
		.verify_event(&create, Some(room_version_id))
		.await
		.is_err()
	{
		return Ok(StrippedCreateVerdict::BadSignature);
	}

	Ok(StrippedCreateVerdict::Valid)
}

fn is_create(json: &CanonicalJsonObject) -> bool {
	let field = |key| json.get(key).and_then(CanonicalJsonValue::as_str);

	field("type") == Some("m.room.create") && field("state_key") == Some("")
}

#[cfg(test)]
mod tests {
	use ruma::{events::AnyStrippedStateEvent, serde::Raw, user_id};
	use serde_json::value::RawValue;
	use tuwunel_core::wbf::invites::{STRIPPED_EVENT_COUNT_MAX, STRIPPED_EVENT_LEN_MAX};

	use super::{list_invite_stripped_state, list_stripped_state_kept};

	fn stripped(json: &str) -> Raw<AnyStrippedStateEvent> {
		Raw::from_json(RawValue::from_string(json.to_owned()).expect("valid JSON"))
	}

	fn types_of(state: &[Raw<AnyStrippedStateEvent>]) -> Vec<String> {
		state
			.iter()
			.filter_map(|event| serde_json::from_str::<serde_json::Value>(event.json().get()).ok())
			.filter_map(|event| {
				event
					.get("type")
					.and_then(serde_json::Value::as_str)
					.map(ToOwned::to_owned)
			})
			.collect()
	}

	#[test]
	fn the_recommended_types_survive_in_their_original_order() {
		let state = vec![
			stripped(r#"{"type":"m.room.create","state_key":"","content":{}}"#),
			stripped(r#"{"type":"m.room.power_levels","state_key":"","content":{}}"#),
			stripped(r#"{"type":"m.room.name","state_key":"","content":{"name":"r"}}"#),
		];

		let kept = list_stripped_state_kept(state);

		assert_eq!(types_of(&kept), ["m.room.create", "m.room.name"]);
	}

	/// 🚨 The whole point of the trim: what arrives here is the sending
	/// server's choice and nothing above bounds it.
	#[test]
	fn an_event_larger_than_a_legal_event_is_dropped() {
		let padding = "x".repeat(STRIPPED_EVENT_LEN_MAX);
		let state = vec![
			stripped(&format!(
				r#"{{"type":"m.room.topic","state_key":"","content":{{"topic":"{padding}"}}}}"#
			)),
			stripped(r#"{"type":"m.room.name","state_key":"","content":{"name":"r"}}"#),
		];

		let kept = list_stripped_state_kept(state);

		assert_eq!(types_of(&kept), ["m.room.name"]);
	}

	#[test]
	fn no_more_than_the_cap_is_kept_however_many_arrive() {
		let state: Vec<_> = (0..64)
			.map(|position| {
				stripped(&format!(
					r#"{{"type":"m.room.member","state_key":"@u{position}:remote","content":{{}}}}"#
				))
			})
			.collect();

		let kept = list_stripped_state_kept(state);

		assert_eq!(kept.len(), STRIPPED_EVENT_COUNT_MAX);
	}

	#[test]
	fn an_empty_stripped_state_stays_empty_rather_than_failing() {
		assert!(list_stripped_state_kept(Vec::new()).is_empty());
		assert!(list_invite_stripped_state(Vec::new(), user_id!("@bob:localhost")).is_empty());
	}

	/// 🚨 This is the invariant the invite's three derived fields rest on:
	/// after it, the only member event naming the invited user is the one the
	/// caller appends from the PDU it verified (external review 2026-10-09).
	#[test]
	fn the_senders_copy_of_the_invited_users_member_event_is_dropped() {
		let state = vec![
			stripped(r#"{"type":"m.room.name","state_key":"","content":{"name":"r"}}"#),
			stripped(
				r#"{"type":"m.room.member","state_key":"@bob:localhost","sender":"@admin:localhost","content":{"membership":"invite","is_direct":true}}"#,
			),
			stripped(
				r#"{"type":"m.room.member","state_key":"@alice:remote","sender":"@alice:remote","content":{"membership":"join"}}"#,
			),
		];

		let kept = list_invite_stripped_state(state, user_id!("@bob:localhost"));

		assert_eq!(
			types_of(&kept),
			["m.room.name", "m.room.member"],
			"only the invited user's own is dropped; other members stay"
		);
		assert!(
			!kept
				.iter()
				.any(|event| event.json().get().contains("@bob:localhost")),
			"nothing the sender said about the invited user survives"
		);
	}
}
