//! `Event/Seek`: one window of one room's events from one anchor, either
//! towards the newest event or away from it, answered as a stream of
//! `Event/Batch` packs — the same packs `Event/Recent` answers with.
//!
//! `Recent` can already filter a window ("only what is newer than my
//! watermark"), but it always fills from the newest end, so there was no way
//! to ask for "the twenty events after this one": a `g_seq` is global, so a
//! client cannot work out the number of the twentieth event of one room
//! after a position. This subtype is that missing direction, and nothing
//! else — the scan, the filters and the batches are `Recent`'s
//! (`collect_window`, one function with a direction).
//!
//! 🔴 The window **includes the anchor itself**, both ways: jumping to an
//! event and not being handed that event means another round trip for it,
//! which is the round trip this subtype exists to save. The cost is written
//! down rather than hidden: continuing with `g_seq = ls` repeats one event,
//! and the client already converges by `g_seq`.
//!
//! See `/docs/design/events/seek-window.md` §3 and §4.

use std::slice;

use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, UserId};
use serde_json::{Value, json};
use tuwunel_core::{
	matrix::pdu::{PduCount, PduId},
	wbf::{PackView, RejectCode},
};
use tuwunel_service::Services;

use super::{
	Failure, Reject, Reply,
	recent::{Fill, WindowSpec, build_batches, collect_window, count_field, scan_start_including},
};

/// Which event the window starts at, as the client named it.
///
/// 📌 Both kinds are accepted because the client's own store is keyed by
/// `event_id` while this server already has the position; neither side has
/// to translate before asking (/docs/design/events/seek-window.md §3.1).
enum Anchor {
	Event(OwnedEventId),
	Position(PduCount),
}

/// What the client asks for.
struct SeekRequest {
	/// The one room the window is about. 🚫 Not a list: a seek answers a
	/// question about a position *inside a room*.
	room_id: OwnedRoomId,
	anchor: Anchor,
	fill: Fill,
	/// Most events in this window; clamped to `wbf_recent_max_limit`.
	limit: usize,
	/// Most events per `Batch`; clamped to `wbf_recent_max_batch`.
	batch: usize,
}

/// The `Seek` settings from the config, in one place for `parse`.
struct SeekLimits {
	default_limit: usize,
	max_limit: usize,
	default_batch: usize,
	max_batch: usize,
}

impl SeekRequest {
	/// Args:
	///     view: the request pack, meta example:
	///         `{"room_id":"!r:localhost","event_id":"$x","direction":"newer"}`
	///     limits: the config's defaults and caps
	/// Return:
	///     Result<SeekRequest, Reject>  `InvalidRequest` when `room_id`,
	///     `direction` or the anchor is missing, is the wrong type, or when
	///     both anchors are given.
	fn parse(view: &PackView<'_>, limits: &SeekLimits) -> Result<Self, Reject> {
		let meta = if view.meta.is_empty() { json!({}) } else { view.meta_json()? };

		Ok(Self {
			room_id: room_id_field(&meta)?,
			anchor: anchor_field(&meta)?,
			fill: fill_field(&meta)?,
			limit: count_field(&meta, "limit", limits.default_limit, limits.max_limit),
			batch: count_field(&meta, "batch", limits.default_batch, limits.max_batch).max(1),
		})
	}
}

/// Args:
///     meta: the request meta, example: `{"room_id":"!r:localhost"}`
/// Return:
///     Result<OwnedRoomId, Reject>  `InvalidRequest` when absent or not a
///     room id.
fn room_id_field(meta: &Value) -> Result<OwnedRoomId, Reject> {
	meta["room_id"]
		.as_str()
		.and_then(|name| RoomId::parse(name).ok())
		.ok_or_else(|| Reject::code(RejectCode::InvalidRequest, "`room_id` must be one room id"))
}

/// Args:
///     meta: the request meta, example: `{"event_id":"$x"}`
/// Return:
///     Result<Anchor, Reject>  `InvalidRequest` when neither `event_id` nor
///     `g_seq` is given, when both are, or when the one given is not an
///     event id / integer.
///
/// 🚨 Both given is refused rather than resolved by precedence: picking one
/// would make the answer depend on which field this server happens to read
/// first, and a client sending a mismatched pair would be told nothing.
fn anchor_field(meta: &Value) -> Result<Anchor, Reject> {
	let named = &meta["event_id"];
	let position = &meta["g_seq"];

	match (named.is_null(), position.is_null()) {
		| (false, false) => Err(Reject::code(
			RejectCode::InvalidRequest,
			"give `event_id` or `g_seq`, not both",
		)),
		| (true, true) => Err(Reject::code(
			RejectCode::InvalidRequest,
			"an anchor is required: `event_id` or `g_seq`",
		)),
		| (false, true) => named
			.as_str()
			.and_then(|name| EventId::parse(name).ok())
			.map(Anchor::Event)
			.ok_or_else(|| Reject::code(RejectCode::InvalidRequest, "`event_id` must be an event id")),
		| (true, false) => position
			.as_i64()
			.map(PduCount::from_signed)
			.map(Anchor::Position)
			.ok_or_else(|| {
				Reject::code(RejectCode::InvalidRequest, "`g_seq` is not a g_seq this server issued")
			}),
	}
}

/// Args:
///     meta: the request meta, example: `{"direction":"newer"}`
/// Return:
///     Result<Fill, Reject>  `InvalidRequest` for anything but the two
///     strings, the missing field included. ⚠️ **No default on purpose**: a
///     default would make the order of the answer depend on what the client
///     left out, which is the whole reason this is a subtype of its own
///     rather than a field on `Recent`.
fn fill_field(meta: &Value) -> Result<Fill, Reject> {
	match meta["direction"].as_str() {
		| Some("newer") => Ok(Fill::Newer),
		| Some("older") => Ok(Fill::Older),
		| _ => Err(Reject::code(
			RejectCode::InvalidRequest,
			"`direction` must be \"newer\" or \"older\"",
		)),
	}
}

/// Args:
///     user: the requester
///     view: the `Seek` request
///     reply: where the `Batch` packs go
/// Return:
///     Result<(), Failure>  Ok once the window's last batch is queued;
///     `Forbidden` when the user is not in the room, `NotFound` when the
///     anchor does not resolve for them.
pub(super) async fn handle_event_seek(
	services: &Services,
	user: &UserId,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let limits = SeekLimits {
		default_limit: services.config.wbf_seek_default_limit,
		max_limit: services.config.wbf_recent_max_limit,
		default_batch: services.config.wbf_recent_default_batch,
		max_batch: services.config.wbf_recent_max_batch,
	};
	let request = SeekRequest::parse(view, &limits)?;
	let data_max = services.config.wbf_data_max_bytes;

	// 🚨 Room first, anchor second. The other order answers "that event is
	// not here" differently from "that event is not yours to see" for
	// somebody who is not even in the room, which tells them the room holds
	// it.
	if !services
		.state_cache
		.is_joined(user, &request.room_id)
		.await
	{
		return Err(Reject::code(
			RejectCode::Forbidden,
			format!("you are not in {}, so it has no window for you", request.room_id),
		)
		.into());
	}

	let anchor = find_anchor_count(services, user, &request.room_id, &request.anchor).await?;
	let spec = WindowSpec {
		fill: request.fill,
		from: Some(scan_start_including(anchor, request.fill)),
		// A seek has no watermark: it reads until `limit`, the byte budget,
		// or the end of what this server holds.
		stop_at: None,
		limit: request.limit,
	};

	// One room, borrowed as the slice `collect_window` takes: a seek never
	// merges rooms, so there is nothing to build a list out of.
	let rooms = slice::from_ref(&request.room_id);
	let window = collect_window(services, user, rooms, &spec, data_max).await;

	for pack in build_batches(view.header.id, &window, request.batch, data_max) {
		reply.send(pack?).await?;
	}

	Ok(())
}

/// Where in `room_id` the window starts.
///
/// 🚨 **Every way of failing answers `NotFound`**: no such event, an outlier
/// (stored but never in the timeline, so it has no position at all), an
/// event of another room, and an event this user cannot see. Telling those
/// apart would let anyone hand over an event id and learn from the
/// difference that it exists — which is why `Context` and `GetEvent` answer
/// the same way (/docs/design/events/seek-window.md §3.1).
///
/// Args:
///     user: the requester, already known to be joined to `room_id`
///     room_id: the window's room, example: "!r:localhost"
///     anchor: as the client named it
/// Return:
///     Result<PduCount, Reject>  the anchor's position, or `NotFound`.
async fn find_anchor_count(
	services: &Services,
	user: &UserId,
	room_id: &RoomId,
	anchor: &Anchor,
) -> Result<PduCount, Reject> {
	let shortroomid = services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|_| not_found())?;

	// Both kinds end up as the same pair: a position in this room, and the
	// event at it to check visibility against. ⚠️ A `g_seq` is checked too
	// rather than trusted: a position this room has no event at would
	// otherwise seek to wherever that number falls between events, and a
	// position in another room would read that room's neighbours into a
	// window the client asked about this one.
	let (count, event_id): (PduCount, OwnedEventId) = match anchor {
		| Anchor::Event(event_id) => {
			let pdu_id: PduId = services
				.timeline
				.get_pdu_id(event_id)
				.await
				.map_err(|_| not_found())?
				.into();

			if pdu_id.shortroomid != shortroomid {
				return Err(not_found());
			}

			(pdu_id.count, event_id.clone())
		},
		| Anchor::Position(count) => {
			let pdu_id = PduId { shortroomid, count: *count };
			let event_id = services
				.timeline
				.get_event_id_from_pdu_id(&pdu_id)
				.await
				.map_err(|_| not_found())?;

			(*count, event_id)
		},
	};

	// The same check `collect_window` runs on every event it serves, run on
	// the anchor before the scan: an anchor the user cannot see must not be
	// usable as a position either, or the window around it answers a
	// question about an event they were never shown.
	if !services
		.state_accessor
		.user_can_see_event(user, room_id, &event_id)
		.await
	{
		return Err(not_found());
	}

	Ok(count)
}

/// The one answer every unresolvable anchor gets.
fn not_found() -> Reject {
	Reject::code(RejectCode::NotFound, "no such event in this room, or not one you can see")
}

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tuwunel_core::matrix::pdu::PduCount;

	use super::{Anchor, Fill, anchor_field, fill_field, room_id_field, scan_start_including};

	#[test]
	fn an_anchor_is_one_of_the_two_kinds_and_never_both() {
		assert!(matches!(
			anchor_field(&json!({ "event_id": "$x:localhost" })),
			Ok(Anchor::Event(_))
		));
		assert!(matches!(anchor_field(&json!({ "g_seq": 50 })), Ok(Anchor::Position(_))));
		// 🚨 The two rejections this subtype's shape rests on.
		assert!(anchor_field(&json!({ "event_id": "$x:localhost", "g_seq": 50 })).is_err());
		assert!(anchor_field(&json!({})).is_err());
		// A g_seq of the wrong type is not silently ignored into "no anchor".
		assert!(anchor_field(&json!({ "g_seq": "50" })).is_err());
		assert!(anchor_field(&json!({ "event_id": "not an event id" })).is_err());
	}

	#[test]
	fn a_direction_is_required_and_has_no_default() {
		assert_eq!(fill_field(&json!({ "direction": "newer" })).unwrap(), Fill::Newer);
		assert_eq!(fill_field(&json!({ "direction": "older" })).unwrap(), Fill::Older);
		assert!(fill_field(&json!({})).is_err(), "a missing direction has no default to fall back to");
		assert!(fill_field(&json!({ "direction": "forward" })).is_err(), "only the two strings");
		assert!(fill_field(&json!({ "direction": 1 })).is_err(), "and only as a string");
	}

	#[test]
	fn a_room_is_required_and_is_one_room() {
		assert!(room_id_field(&json!({ "room_id": "!r:localhost" })).is_ok(), "one room id is the shape");
		assert!(room_id_field(&json!({})).is_err(), "a seek is about a position inside a room");
		assert!(
			room_id_field(&json!({ "room_id": ["!r:localhost"] })).is_err(),
			"a list is `Recent`'s shape, not this one's"
		);
	}

	#[test]
	fn the_scan_starts_one_step_back_so_the_anchor_is_the_first_event() {
		// 🔴 This is what makes the window include its anchor: the iterators
		// skip the event at their cursor, so the cursor goes one step the
		// other way. If `pdus`/`pdus_rev` ever became inclusive, this test
		// is what says so.
		let anchor = PduCount::from_signed(50);
		assert_eq!(
			scan_start_including(anchor, Fill::Newer),
			PduCount::from_signed(49),
			"filling towards the newest starts below the anchor"
		);
		assert_eq!(
			scan_start_including(anchor, Fill::Older),
			PduCount::from_signed(51),
			"filling away from the newest starts above the anchor"
		);
	}
}
