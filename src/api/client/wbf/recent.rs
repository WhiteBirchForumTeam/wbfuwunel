//! `Event/Recent`: one window of the events newer than the client's watermark
//! across every room the user is joined to, newest first, in the order this
//! server received them, answered as a stream of `Event/Batch` packs.
//!
//! There is no global index of events; there does not need to be one. The
//! global count every event carries is comparable across rooms, so one
//! reverse stream per joined room merged by count is the global order. That
//! count is the `g_seq` each served event already carries in its
//! `unsigned`; the request's `cg_seq` and `before` and the batches' `fs` and
//! `ls` are the same number. See `docs/design/room-seq-and-recent.md` §2 and
//! `docs/design/wbf-pack-pipeline.md` §6.
//!
//! A window is small (`wbf_recent_max_limit` events and `wbf_window_max_bytes`
//! bytes, whichever is reached first), so it is gathered whole before the
//! first `Batch` goes out: that is how `tc`, the window's size, is known in
//! every batch. The client pulls the next window with `before` while `more`
//! says there may be one; the server keeps nothing between windows.

use std::{
	cmp::Ordering,
	collections::{BinaryHeap, HashSet},
	pin::Pin,
};

use futures::{Stream, StreamExt};
use ruma::{OwnedRoomId, RoomId, UserId, events::AnyTimelineEvent, serde::Raw};
use serde_json::{Value, json};
use tuwunel_core::{
	Result, debug_warn,
	matrix::{event::Event, pdu::PduCount},
	wbf::{
		Flags, Kind, PackBuilder, PackError, PackView, RejectCode,
		events::{EVENT_LEN_PREFIX, WindowBudget, framed_len, length_prefixed, list_pack_ranges},
	},
};
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

use super::{Failure, Reject, Reply, event};
use crate::client::message::{ignored_filter, visibility_filter};

/// What the client asks for.
struct RecentRequest {
	/// The rooms to read, or `None` for every room the user has joined.
	///
	/// ⭐ This is what makes a window a room's history: one room plus
	/// `before` is "the events of this room older than that", which is the
	/// question `/messages` answers over HTTP — without the opaque tokens,
	/// because the events carry `r_seq` and the cursor is a `g_seq`.
	rooms: Option<Vec<OwnedRoomId>>,
	/// Most events in this window; clamped to `wbf_recent_max_limit`.
	limit: usize,
	/// Most events per `Batch`; clamped to `wbf_recent_max_batch`.
	batch: usize,
	/// The newest g_seq the client has cached; the window stops there. Absent
	/// or 0 means no cache: the newest `limit` events.
	cg_seq: Option<PduCount>,
	/// Only events older than this g_seq (the `ls` of the previous window's
	/// last batch).
	before: Option<PduCount>,
}

/// The `Recent` settings from the config, in one place for `parse`.
struct RecentLimits {
	default_limit: usize,
	max_limit: usize,
	default_batch: usize,
	max_batch: usize,
}

impl RecentRequest {
	/// Args:
	///     view: the request pack, meta example: `{"limit":320,"cg_seq":4711,"batch":10}`
	///     limits: the config's defaults and caps
	/// Return:
	///     Result<RecentRequest, Reject>  Conflict when a position is not an
	///     integer.
	fn parse(view: &PackView<'_>, limits: &RecentLimits) -> std::result::Result<Self, Reject> {
		let meta = if view.meta.is_empty() { json!({}) } else { view.meta_json()? };

		Ok(Self {
			rooms: rooms_field(&meta)?,
			limit: count_field(&meta, "limit", limits.default_limit, limits.max_limit),
			// A client that asks for batches of 0 gets batches of 1 (a negative
			// or non-numeric `batch` falls to the default instead): a window
			// is never cut into nothing.
			batch: count_field(&meta, "batch", limits.default_batch, limits.max_batch).max(1),
			cg_seq: g_seq_field(&meta, "cg_seq")?.filter(|cached| *cached != PduCount::from_signed(0)),
			before: g_seq_field(&meta, "before")?,
		})
	}
}

/// Args:
///     meta: the request meta, example: `{"limit":320}`
///     name: example: "limit"
///     default: used when absent or not a number, example: 320
///     max: example: 500
/// Return:
///     usize  the field clamped to `max`; `default` when absent or not a
///     non-negative integer.
fn count_field(meta: &Value, name: &str, default: usize, max: usize) -> usize {
	meta[name]
		.as_u64()
		.and_then(|value| usize::try_from(value).ok())
		.map_or(default, |value| value.min(max))
}

/// The rooms a window is about.
///
/// Args:
///     meta: the request meta, example: `{"rooms":["!r:localhost"]}`
/// Return:
///     Result<Option<Vec<OwnedRoomId>>, Reject>  None when the field is
///     absent or null, meaning every joined room; `InvalidRequest` when it is
///     there but is not a list of room ids. ⚠️ An empty list is a list: it
///     asks about no rooms and gets an empty window, the same way
///     `Subscribe` with an empty list subscribes to nothing.
fn rooms_field(meta: &Value) -> std::result::Result<Option<Vec<OwnedRoomId>>, Reject> {
	match &meta["rooms"] {
		| Value::Null => Ok(None),
		| Value::Array(names) => names
			.iter()
			.map(|name| {
				name.as_str()
					.and_then(|name| RoomId::parse(name).ok())
					.ok_or_else(|| {
						Reject::code(RejectCode::InvalidRequest, "`rooms` holds something that is not a room id")
					})
			})
			.collect::<std::result::Result<Vec<_>, _>>()
			.map(Some),
		| _ => Err(Reject::code(RejectCode::InvalidRequest, "`rooms` must be a list of room ids")),
	}
}

/// Args:
///     meta: the request meta, example: `{"cg_seq":4711}`
///     name: example: "cg_seq"
/// Return:
///     Result<Option<PduCount>, Reject>  None when absent or null; Conflict
///     when present but not an integer.
fn g_seq_field(meta: &Value, name: &str) -> std::result::Result<Option<PduCount>, Reject> {
	match &meta[name] {
		| Value::Null => Ok(None),
		| Value::Number(number) => number
			.as_i64()
			.map(PduCount::from_signed)
			.map(Some)
			.ok_or_else(|| Reject::code(RejectCode::InvalidRequest, format!("`{name}` is not a g_seq this server issued"))),
		| _ => Err(Reject::code(RejectCode::InvalidRequest, format!("`{name}` must be an integer g_seq"))),
	}
}

type RoomStream<'a> = Pin<Box<dyn Stream<Item = Result<PdusIterItem>> + Send + 'a>>;

/// One room's next candidate, keyed for the heap by its global count.
struct Head {
	count: PduCount,
	room: usize,
}

impl PartialEq for Head {
	fn eq(&self, other: &Self) -> bool { self.count == other.count }
}
impl Eq for Head {}
impl PartialOrd for Head {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for Head {
	fn cmp(&self, other: &Self) -> Ordering { self.count.cmp(&other.count) }
}

/// One event of a window, as it will be sent: its `g_seq` and its JSON.
pub(super) struct WindowEvent {
	pub(super) g_seq: i64,
	pub(super) json: Vec<u8>,
}

/// A window's events, newest first, and whether it stopped at a cap.
pub(super) struct Window {
	pub(super) events: Vec<WindowEvent>,
	/// True when the window stopped at `limit` or at `wbf_window_max_bytes`,
	/// so older events may follow; the wire's `more`.
	pub(super) is_cut_short: bool,
}

/// The events newer than `cg_seq`, newest first, at most `limit`: what a
/// `Subscribe` that asks to be caught up pushes before the live events.
///
/// 🚨 `rooms` is the subscription's own list, not everything the user has
/// joined. Catching up used to be **global** whatever the subscription said,
/// so a connection subscribed to one room was pushed events of rooms it never
/// asked about; the design document papered over it by asking clients not to
/// advance their watermark on those (wbf-event-push §2.1, 審查者 rumia R4).
/// Asking a client not to believe what the server just sent it is not a rule
/// anybody can keep — this is the same filter `Recent` uses, applied where
/// the promise was made.
///
/// Args:
///     rooms: the rooms this subscription covers
///     cg_seq: the client's watermark, example: Some(4711); None = the newest `limit`
///     limit: example: `wbf_recent_max_limit`
/// Return:
///     Window  `is_cut_short` when there may be events after `cg_seq` that
///     this window did not reach.
pub(super) async fn window_after(
	services: &Services,
	user: &UserId,
	rooms: &[OwnedRoomId],
	cg_seq: Option<PduCount>,
	limit: usize,
) -> Window {
	let request = RecentRequest { rooms: None, limit, batch: 1, cg_seq, before: None };
	collect_window(services, user, rooms, &request, services.config.wbf_data_max_bytes).await
}

/// Args:
///     user: the requester
///     view: the `Recent` request
///     reply: where the `Batch` packs go
/// Return:
///     Result<(), Failure>  Ok once the window's last batch is queued (at
///     least one batch is always sent, empty for an empty window).
pub(super) async fn handle_event_recent(
	services: &Services,
	user: &UserId,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> std::result::Result<(), Failure> {
	let limits = RecentLimits {
		default_limit: services.config.wbf_recent_default_limit,
		max_limit: services.config.wbf_recent_max_limit,
		default_batch: services.config.wbf_recent_default_batch,
		max_batch: services.config.wbf_recent_max_batch,
	};
	let request = RecentRequest::parse(view, &limits)?;
	let data_max = services.config.wbf_data_max_bytes;
	let rooms = resolve_rooms(services, user, request.rooms.as_deref()).await?;

	let window = collect_window(services, user, &rooms, &request, data_max).await;

	// One pack at a time: building them all first held the window twice.
	for pack in build_batches(view.header.id, &window, request.batch, data_max) {
		reply.send(pack?).await?;
	}

	Ok(())
}

/// Which rooms a window covers.
///
/// 🚨 A named room the user is not in **refuses the whole request** rather
/// than being left out of the answer. `Subscribe` lists such rooms in
/// `skipped` and carries on, and that is right for a registration — but a
/// window is an answer to a question, and an answer that quietly omits one of
/// the rooms asked about is wrong in a way the client cannot see. It would
/// show up much later as "where did that room's history go".
///
/// Args:
///     named: the request's `rooms`, or None for every joined room
/// Return:
///     Result<Vec<OwnedRoomId>, Reject>  `Forbidden` naming the first room
///     the user is not in.
async fn resolve_rooms(
	services: &Services,
	user: &UserId,
	named: Option<&[OwnedRoomId]>,
) -> std::result::Result<Vec<OwnedRoomId>, Reject> {
	let Some(named) = named else {
		return Ok(services
			.state_cache
			.rooms_joined(user)
			.map(ToOwned::to_owned)
			.collect()
			.await);
	};

	for room_id in named {
		if !services.state_cache.is_joined(user, room_id).await {
			return Err(Reject::code(
				RejectCode::Forbidden,
				format!("you are not in {room_id}, so it has no window for you"),
			));
		}
	}

	Ok(named.to_vec())
}

/// The window's events, newest first: at most `limit` and at most
/// `wbf_window_max_bytes` of them (bytes asked first), all newer than
/// `cg_seq` and older than `before`, visible to `user`, and each small enough
/// for a pack of its own.
///
/// 📎 One room is the cheap case, not a special case: the heap ranks one
/// stream, so this becomes a single reverse scan of that room's prefix —
/// which is the same iterator `/messages` uses.
///
/// Args:
///     rooms: what this window covers, already checked (`resolve_rooms`)
async fn collect_window(
	services: &Services,
	user: &UserId,
	rooms: &[OwnedRoomId],
	request: &RecentRequest,
	data_max: usize,
) -> Window {
	// 🚨 One stream per room, and each room **once**. A name repeated in the
	// request would otherwise open two reverse streams over the same prefix,
	// and the heap would rank the same event from both: every event of that
	// room would reach the client twice, in a window whose whole contract is
	// that it holds each event once (PR #51 review, rumia and salvia).
	//
	// ⚠️ Deduplicated here, not at each caller's parsing, because this is the
	// place that turns a name into a scan — and there are two callers now
	// (a `Recent` and a `Subscribe` catching up), which is exactly the shape
	// where "every producer remembers to" fails.
	let mut seen: HashSet<&str> = HashSet::with_capacity(rooms.len());
	let mut once: Vec<&OwnedRoomId> = Vec::with_capacity(rooms.len());
	for room_id in rooms {
		if seen.insert(room_id.as_str()) {
			once.push(room_id);
		}
	}
	let rooms = once;

	// One reverse stream per room, each already past `before`. The heads
	// hold the event the heap is ranking; the streams wait behind them.
	let mut streams: Vec<RoomStream<'_>> = rooms
		.iter()
		.map(|room_id| {
			let stream = services
				.timeline
				.pdus_rev(Some(user), room_id, request.before);
			let boxed: RoomStream<'_> = Box::pin(stream);
			boxed
		})
		.collect();
	let mut heads: Vec<Option<PdusIterItem>> = Vec::with_capacity(streams.len());
	let mut heap = BinaryHeap::with_capacity(streams.len());
	for (room, stream) in streams.iter_mut().enumerate() {
		let head = next_item(stream).await;
		if let Some((count, _)) = &head {
			heap.push(Head { count: *count, room });
		}
		heads.push(head);
	}

	let mut window: Vec<WindowEvent> = Vec::with_capacity(request.limit.min(1024));
	let mut budget = WindowBudget::new(request.limit, services.config.wbf_window_max_bytes);

	while !budget.is_count_full() {
		let Some(Head { count, room }) = heap.pop() else {
			break;
		};
		if !is_newer_than_after(count, request.cg_seq) {
			// The heap is descending: everything left is at or below the
			// watermark, which the client already has.
			break;
		}
		let Some(item) = heads[room].take() else {
			break;
		};

		// Refill this room before anything else, so the heap always ranks
		// every room's newest unseen event. This happens before the filters on
		// purpose: an event the filters drop still advances the cursor, so its
		// room must already be represented by its next candidate.
		if let Some(next) = next_item(&mut streams[room]).await {
			heap.push(Head { count: next.0, room });
			heads[room] = Some(next);
		}

		let Some(item) = ignored_filter(services, item, user).await else {
			continue;
		};
		let Some((_, pdu)) = visibility_filter(services, item, user).await else {
			continue;
		};

		let event: Raw<AnyTimelineEvent> = pdu.to_format();
		let json = event.json().get().as_bytes();
		if framed_len(json.len()) > data_max {
			// A single event wider than a pack can never be served here, so
			// the window steps past it instead of stalling on it.
			debug_warn!(event_id = %pdu.event_id(), "Event exceeds wbf_data_max_bytes; skipped by Event/Recent");
			continue;
		}
		if !budget.try_admit(json.len()) {
			// Full by bytes. This event is the next window's first: the
			// client's `before` is the `ls` of the last one that joined.
			break;
		}

		window.push(WindowEvent { g_seq: count.into_signed(), json: json.to_vec() });
	}

	Window { events: window, is_cut_short: budget.is_cut_short() }
}

/// Cuts a window into `Batch` packs answering request `id`, building each
/// one only when it is asked for.
///
/// Args:
///     id: the `Recent` request's id, copied into every batch
///     window: newest first, example: 25 events
///     batch: most events per pack, example: 10 (then 10, 10, 5)
///     data_max: `wbf_data_max_bytes`; a batch is also cut when the next
///         event would not fit
/// Return:
///     impl Iterator<Item = Result<Vec<u8>, PackError>>  the packs in order,
///     `seq` 0, 1, 2…; exactly one (empty) pack for an empty window. Every
///     pack's meta is `{tc, bc, fs, ls, r, more}`, the last has `r = 0`, and
///     `more` is the same in all of them.
fn build_batches(
	id: u64,
	window: &Window,
	batch: usize,
	data_max: usize,
) -> impl Iterator<Item = std::result::Result<Vec<u8>, PackError>> + Send + '_ {
	debug_assert!(EVENT_LEN_PREFIX == 4, "the wire prefix is four bytes");
	let events = &window.events;
	let total = events.len();

	let mut ranges = list_pack_ranges(events.iter().map(|event| event.json.len()), batch, data_max);
	if ranges.is_empty() {
		// An empty window is still answered, by one empty Batch that ends it.
		ranges.push(0..0);
	}

	// The last batch's `r` is 0 by construction: the ranges cover the window.
	let mut sent: usize = 0;
	ranges
		.into_iter()
		.enumerate()
		.map(move |(position, range)| {
			let in_batch = &events[range];
			sent = sent.saturating_add(in_batch.len());
			let seq = u32::try_from(position).unwrap_or(u32::MAX);
			batch_pack(id, seq, total, in_batch, total.saturating_sub(sent), window.is_cut_short)
		})
}

/// One `Batch`: its meta is the six fields of pipeline §6.2.
fn batch_pack(
	id: u64,
	seq: u32,
	tc: usize,
	in_batch: &[WindowEvent],
	remaining: usize,
	more: bool,
) -> std::result::Result<Vec<u8>, PackError> {
	let fs = in_batch.first().map_or(0, |event| event.g_seq);
	let ls = in_batch.last().map_or(0, |event| event.g_seq);
	let data = length_prefixed(in_batch.iter().map(|event| event.json.as_slice()))?;

	Ok(PackBuilder::new(Kind::Event, event::BATCH, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({ "tc": tc, "bc": in_batch.len(), "fs": fs, "ls": ls, "r": remaining, "more": more }))?
		.data(&data)?
		.finish())
}

/// Whether an event at `count` is one the client does not have yet.
fn is_newer_than_after(count: PduCount, cg_seq: Option<PduCount>) -> bool { cg_seq.is_none_or(|cg_seq| count > cg_seq) }

/// The room's next event, skipping rows that fail to decode.
async fn next_item(stream: &mut RoomStream<'_>) -> Option<PdusIterItem> {
	loop {
		match stream.next().await? {
			| Ok(item) => return Some(item),
			| Err(error) => debug_warn!(?error, "Skipping an undecodable timeline row"),
		}
	}
}

#[cfg(test)]
mod tests {
	use serde_json::Value;
	use tuwunel_core::wbf::{Kind, decode};

	use super::{EVENT_LEN_PREFIX, Window, WindowEvent, build_batches, event};

	fn window(count: usize) -> Window {
		// Newest first: g_seq 100, 99, 98…
		let events = (0..count)
			.map(|position| {
				let g_seq = 100 - i64::try_from(position).expect("small");
				WindowEvent { g_seq, json: format!(r#"{{"event_id":"$e{g_seq}","g":{g_seq}}}"#).into_bytes() }
			})
			.collect();
		Window { events, is_cut_short: false }
	}

	fn packs(id: u64, window: &Window, batch: usize, data_max: usize) -> Vec<Vec<u8>> {
		build_batches(id, window, batch, data_max)
			.collect::<Result<_, _>>()
			.expect("packs")
	}

	/// Decodes one pack into (meta, events as JSON strings).
	fn open(mut pack: Vec<u8>) -> (Value, Vec<String>) {
		let view = decode(&mut pack).expect("batch decodes");
		assert_eq!(view.header.kind, Kind::Event);
		assert_eq!(view.header.subtype, event::BATCH);
		assert!(view.header.flags.is_response());
		let meta = view.meta_json().expect("json meta");

		let mut events = Vec::new();
		let mut at = 0;
		while at < view.data.len() {
			let len = u32::from_be_bytes(view.data[at..at + EVENT_LEN_PREFIX].try_into().expect("4 bytes")) as usize;
			at += EVENT_LEN_PREFIX;
			events.push(String::from_utf8(view.data[at..at + len].to_vec()).expect("utf8"));
			at += len;
		}
		assert_eq!(at, view.data.len(), "data is exactly the prefixed events");

		(meta, events)
	}

	#[test]
	fn a_window_is_cut_into_batches_whose_numbers_add_up() {
		let packs = packs(7, &window(25), 10, 1 << 20);
		assert_eq!(packs.len(), 3);

		let mut sent = 0;
		for (position, pack) in packs.into_iter().enumerate() {
			let (meta, events) = open(pack);
			assert_eq!(meta["tc"], 25);
			let bc = meta["bc"].as_u64().expect("bc") as usize;
			assert_eq!(bc, events.len());
			assert_eq!(bc, if position < 2 { 10 } else { 5 });
			sent += bc;
			assert_eq!(meta["r"].as_u64().expect("r") as usize, 25 - sent, "r is what follows this batch");
			// fs is the newest of the batch, ls the oldest, both present in the data.
			assert_eq!(meta["fs"], 100 - (position as i64) * 10);
			assert_eq!(meta["ls"], 100 - (position as i64) * 10 - (bc as i64 - 1));
			assert!(events[0].contains(&format!("\"g\":{}", meta["fs"])));
			assert!(events[bc - 1].contains(&format!("\"g\":{}", meta["ls"])));
		}
		assert_eq!(sent, 25);
	}

	#[test]
	fn batch_seq_counts_from_zero_and_copies_the_request_id() {
		let packs = packs(0xABCD, &window(3), 1, 1 << 20);
		for (position, mut pack) in packs.into_iter().enumerate() {
			let view = decode(&mut pack).expect("decodes");
			assert_eq!(view.header.id, 0xABCD);
			assert_eq!(view.header.seq, position as u32);
		}
	}

	#[test]
	fn an_empty_window_is_one_empty_batch() {
		let packs = packs(1, &window(0), 10, 1 << 20);
		assert_eq!(packs.len(), 1);
		let (meta, events) = open(packs.into_iter().next().expect("one"));
		assert!(events.is_empty());
		assert_eq!(meta["tc"], 0);
		assert_eq!(meta["bc"], 0);
		assert_eq!(meta["r"], 0);
		assert_eq!(meta["fs"], 0);
		assert_eq!(meta["ls"], 0);
	}

	#[test]
	fn the_byte_budget_cuts_a_batch_before_it_would_overflow() {
		// Each event is about 30 bytes plus the 4-byte prefix; a budget of 80
		// holds two, so batches of "10" come out as pairs.
		let events = window(5);
		let one = EVENT_LEN_PREFIX + events.events[0].json.len();
		let packs = packs(1, &events, 10, one * 2 + 1);
		assert_eq!(packs.len(), 3);
		let sizes: Vec<usize> = packs.into_iter().map(|pack| open(pack).1.len()).collect();
		assert_eq!(sizes, vec![2, 2, 1]);
	}

	#[test]
	fn every_batch_says_whether_the_window_was_cut_short() {
		// 🚨 A window full by bytes holds fewer events than `limit`, which is
		// what "no more history" used to look like; `more` is the only thing
		// that tells the two apart, so it cannot be missing from any batch.
		let mut cut = window(25);
		cut.is_cut_short = true;
		for pack in packs(1, &cut, 10, 1 << 20) {
			assert_eq!(open(pack).0["more"], true);
		}

		for pack in packs(1, &window(25), 10, 1 << 20) {
			assert_eq!(open(pack).0["more"], false);
		}
		let (meta, _) = open(packs(1, &window(0), 10, 1 << 20).remove(0));
		assert_eq!(meta["more"], false, "an empty window that ran out of events");
	}

	#[test]
	fn the_last_batch_always_has_r_zero_even_when_the_window_divides_evenly() {
		let packs = packs(1, &window(20), 10, 1 << 20);
		assert_eq!(packs.len(), 2);
		let (meta, _) = open(packs.into_iter().last().expect("last"));
		assert_eq!(meta["r"], 0);
		assert_eq!(meta["bc"], 10);
	}
}
