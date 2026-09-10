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
//! A window is small (`wbf_recent_max_limit`, hundreds), so it is gathered
//! whole before the first `Batch` goes out: that is how `tc`, the window's
//! size, is known in every batch, and the memory it costs is a few hundred
//! events. The client pulls the next window with `before`; the server keeps
//! nothing between windows.

use std::{cmp::Ordering, collections::BinaryHeap, pin::Pin};

use futures::{Stream, StreamExt};
use ruma::{OwnedRoomId, UserId, events::AnyTimelineEvent, serde::Raw};
use serde_json::{Value, json};
use tuwunel_core::{
	Result, debug_warn,
	matrix::{event::Event, pdu::PduCount},
	wbf::{
		Flags, Kind, PackBuilder, PackError, PackView, RejectCode,
		events::{EVENT_LEN_PREFIX, framed_len, length_prefixed, list_pack_ranges},
	},
};
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

use super::{Failure, Reject, Reply, event};
use crate::client::message::{ignored_filter, visibility_filter};

/// What the client asks for.
struct RecentRequest {
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

/// The events newer than `cg_seq`, newest first, at most `limit`: what a
/// `Subscribe` that asks to be caught up pushes before the live events.
///
/// Args:
///     cg_seq: the client's watermark, example: Some(4711); None = the newest `limit`
///     limit: example: `wbf_recent_max_limit`
pub(super) async fn window_after(services: &Services, user: &UserId, cg_seq: Option<PduCount>, limit: usize) -> Vec<WindowEvent> {
	let request = RecentRequest { limit, batch: 1, cg_seq, before: None };
	collect_window(services, user, &request, services.config.wbf_data_max_bytes).await
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

	let window = collect_window(services, user, &request, data_max).await;

	for pack in build_batches(view.header.id, &window, request.batch, data_max)? {
		reply.send(pack).await?;
	}

	Ok(())
}

/// The window's events, newest first: at most `limit`, all newer than
/// `cg_seq` and older than `before`, visible to `user`, and each small enough
/// for a pack of its own.
async fn collect_window(services: &Services, user: &UserId, request: &RecentRequest, data_max: usize) -> Vec<WindowEvent> {
	let rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined(user)
		.map(ToOwned::to_owned)
		.collect()
		.await;

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

	while window.len() < request.limit {
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

		window.push(WindowEvent { g_seq: count.into_signed(), json: json.to_vec() });
	}

	window
}

/// Cuts a window into `Batch` packs answering request `id`.
///
/// Args:
///     id: the `Recent` request's id, copied into every batch
///     window: newest first, example: 25 events
///     batch: most events per pack, example: 10 (then 10, 10, 5)
///     data_max: `wbf_data_max_bytes`; a batch is also cut when the next
///         event would not fit
/// Return:
///     Result<Vec<Vec<u8>>, PackError>  the packs in order, `seq` 0, 1, 2…;
///     exactly one (empty) pack for an empty window. Every pack's meta is
///     `{tc, bc, fs, ls, r}` and the last has `r = 0`.
fn build_batches(id: u64, window: &[WindowEvent], batch: usize, data_max: usize) -> std::result::Result<Vec<Vec<u8>>, PackError> {
	let total = window.len();
	let mut packs = Vec::with_capacity(total.div_ceil(batch.max(1)).max(1));

	if window.is_empty() {
		packs.push(batch_pack(id, 0, BatchMeta { tc: 0, bc: 0, fs: 0, ls: 0, r: 0 }, &[])?);
		return Ok(packs);
	}
	debug_assert!(EVENT_LEN_PREFIX == 4, "the wire prefix is four bytes");

	let mut seq: u32 = 0;
	let mut sent: usize = 0;

	// The last batch's `r` is 0 by construction: the ranges cover the window.
	for range in list_pack_ranges(window.iter().map(|event| event.json.len()), batch, data_max) {
		let in_batch: Vec<&WindowEvent> = window[range].iter().collect();
		sent += in_batch.len();
		packs.push(batch_pack(id, seq, meta_for(total, &in_batch, total - sent), &in_batch)?);
		seq = seq.saturating_add(1);
	}

	Ok(packs)
}

/// The five numbers every `Batch` carries (pipeline §6.2).
struct BatchMeta {
	tc: usize,
	bc: usize,
	fs: i64,
	ls: i64,
	r: usize,
}

fn meta_for(total: usize, in_batch: &[&WindowEvent], remaining: usize) -> BatchMeta {
	BatchMeta {
		tc: total,
		bc: in_batch.len(),
		fs: in_batch.first().map_or(0, |event| event.g_seq),
		ls: in_batch.last().map_or(0, |event| event.g_seq),
		r: remaining,
	}
}

fn batch_pack(id: u64, seq: u32, meta: BatchMeta, events: &[&WindowEvent]) -> std::result::Result<Vec<u8>, PackError> {
	let data = length_prefixed(events.iter().map(|event| event.json.as_slice()))?;
	Ok(PackBuilder::new(Kind::Event, event::BATCH, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({ "tc": meta.tc, "bc": meta.bc, "fs": meta.fs, "ls": meta.ls, "r": meta.r }))?
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

	use super::{EVENT_LEN_PREFIX, WindowEvent, build_batches, event};

	fn window(count: usize) -> Vec<WindowEvent> {
		// Newest first: g_seq 100, 99, 98…
		(0..count)
			.map(|position| {
				let g_seq = 100 - i64::try_from(position).expect("small");
				WindowEvent { g_seq, json: format!(r#"{{"event_id":"$e{g_seq}","g":{g_seq}}}"#).into_bytes() }
			})
			.collect()
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
		let packs = build_batches(7, &window(25), 10, 1 << 20).expect("packs");
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
		let packs = build_batches(0xABCD, &window(3), 1, 1 << 20).expect("packs");
		for (position, mut pack) in packs.into_iter().enumerate() {
			let view = decode(&mut pack).expect("decodes");
			assert_eq!(view.header.id, 0xABCD);
			assert_eq!(view.header.seq, position as u32);
		}
	}

	#[test]
	fn an_empty_window_is_one_empty_batch() {
		let packs = build_batches(1, &[], 10, 1 << 20).expect("packs");
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
		let one = EVENT_LEN_PREFIX + events[0].json.len();
		let packs = build_batches(1, &events, 10, one * 2 + 1).expect("packs");
		assert_eq!(packs.len(), 3);
		let sizes: Vec<usize> = packs.into_iter().map(|pack| open(pack).1.len()).collect();
		assert_eq!(sizes, vec![2, 2, 1]);
	}

	#[test]
	fn the_last_batch_always_has_r_zero_even_when_the_window_divides_evenly() {
		let packs = build_batches(1, &window(20), 10, 1 << 20).expect("packs");
		assert_eq!(packs.len(), 2);
		let (meta, _) = open(packs.into_iter().last().expect("last"));
		assert_eq!(meta["r"], 0);
		assert_eq!(meta["bc"], 10);
	}
}
