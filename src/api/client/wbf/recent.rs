//! `Event/Recent`: the events newer than the client's watermark across every
//! room the user is joined to, newest first, in the order this server
//! received them.
//!
//! There is no global index of events; there does not need to be one. The
//! global count every event carries is comparable across rooms, so one
//! reverse stream per joined room merged by count is the global order. That
//! count is the `g_seq` each served event already carries in its
//! `unsigned`; the request's `after` and `before` and the reply's `next` and
//! `latest_g_seq` are the same number. See
//! `docs/design/room-seq-and-recent.md` §2.

use std::{cmp::Ordering, collections::BinaryHeap, pin::Pin};

use futures::{Stream, StreamExt};
use ruma::{OwnedRoomId, UserId, events::AnyTimelineEvent, serde::Raw};
use serde_json::{Value, json};
use tuwunel_core::{
	Result, debug_warn,
	matrix::{event::Event, pdu::PduCount},
	wbf::PackView,
};
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

use super::{Reject, ack};
use crate::client::message::{ignored_filter, visibility_filter};

/// What the client asks for.
struct RecentRequest {
	/// Most events in the reply; clamped to `wbf_recent_max_limit`.
	limit: usize,
	/// Only events after this g_seq: the client's watermark.
	after: Option<PduCount>,
	/// Only events older than this g_seq (a `next` from a reply).
	before: Option<PduCount>,
}

impl RecentRequest {
	/// Args:
	///     view: the request pack, meta example: `{"limit":100,"after":4711}`
	///     max_limit: `wbf_recent_max_limit`, example: 10000
	/// Return:
	///     Result<RecentRequest, Reject>  Conflict when a position is not an
	///     integer.
	fn parse(view: &PackView<'_>, max_limit: usize) -> std::result::Result<Self, Reject> {
		let meta = if view.meta.is_empty() { json!({}) } else { view.meta_json()? };

		let limit = meta["limit"]
			.as_u64()
			.and_then(|limit| usize::try_from(limit).ok())
			.map_or(max_limit, |limit| limit.min(max_limit));

		Ok(Self {
			limit,
			after: g_seq_field(&meta, "after")?,
			before: g_seq_field(&meta, "before")?,
		})
	}
}

/// Args:
///     meta: the request meta, example: `{"after":4711}`
///     name: example: "after"
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
			.ok_or_else(|| Reject::code("Conflict", format!("`{name}` is not a g_seq this server issued"))),
		| _ => Err(Reject::code("Conflict", format!("`{name}` must be an integer g_seq"))),
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

/// Why the page ended.
#[derive(PartialEq, Eq)]
enum Stop {
	/// Every event newer than `after` (or every event at all) was considered.
	Complete,
	/// `limit` or the pack's byte budget ended the page with newer-than-`after`
	/// events still unread; `next` points at them.
	More,
}

pub(super) async fn handle_event_recent(
	services: &Services,
	user: &UserId,
	view: &PackView<'_>,
) -> std::result::Result<Vec<u8>, Reject> {
	let request = RecentRequest::parse(view, services.config.wbf_recent_max_limit)?;
	let data_max = services.config.wbf_data_max_bytes;

	// Read before the merge, so a client that stores it never misses an event
	// appended while this reply was being built: it will be newer than this.
	let latest_g_seq = PduCount::Normal(services.globals.current_count()).into_signed();

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

	let mut data = Vec::with_capacity(data_max.min(1 << 20));
	data.push(b'[');
	let mut returned: usize = 0;
	let mut last_count: Option<PduCount> = None;
	let mut stop = Stop::Complete;

	loop {
		if returned >= request.limit {
			// The page is full; anything still on the heap that is newer than
			// `after` is unread.
			stop = match heap.peek() {
				| Some(head) if is_newer_than_after(head.count, request.after) => Stop::More,
				| _ => Stop::Complete,
			};
			break;
		}
		let Some(Head { count, room }) = heap.pop() else {
			break;
		};
		if !is_newer_than_after(count, request.after) {
			// The heap is descending: everything left is at or below the
			// watermark, which the client already has.
			break;
		}
		let Some(item) = heads[room].take() else {
			break;
		};

		// Refill this room before anything else, so the heap always ranks
		// every room's newest unseen event.
		if let Some(next) = next_item(&mut streams[room]).await {
			heap.push(Head { count: next.0, room });
			heads[room] = Some(next);
		}

		let Some(item) = ignored_filter(services, item, user).await else {
			last_count = Some(count);
			continue;
		};
		let Some((_, pdu)) = visibility_filter(services, item, user).await else {
			last_count = Some(count);
			continue;
		};

		let event: Raw<AnyTimelineEvent> = pdu.to_format();
		let event = event.json().get().as_bytes();
		let separator = usize::from(returned > 0);
		if data.len() + separator + event.len() + 1 > data_max {
			if returned == 0 {
				// A single event wider than a pack: it can never be served
				// here, so the cursor steps past it instead of stalling.
				debug_warn!(event_id = %pdu.event_id(), "Event exceeds wbf_data_max_bytes; skipped by Event/Recent");
				last_count = Some(count);
				continue;
			}
			// This event leads the next page: the cursor stays at the last
			// event returned, which is older than nothing on this page.
			stop = Stop::More;
			break;
		}

		if separator == 1 {
			data.push(b',');
		}
		data.extend_from_slice(event);
		returned = returned.saturating_add(1);
		last_count = Some(count);
	}
	data.push(b']');

	let next = (stop == Stop::More)
		.then_some(last_count)
		.flatten()
		.map(PduCount::into_signed);

	Ok(ack(
		view.header.id,
		view.header.seq,
		json!({
			"returned": returned,
			"latest_g_seq": latest_g_seq,
			"complete": stop == Stop::Complete,
			"next": next,
		}),
		data,
	))
}

/// Whether an event at `count` is one the client does not have yet.
fn is_newer_than_after(count: PduCount, after: Option<PduCount>) -> bool { after.is_none_or(|after| count > after) }

/// The room's next event, skipping rows that fail to decode.
async fn next_item(stream: &mut RoomStream<'_>) -> Option<PdusIterItem> {
	loop {
		match stream.next().await? {
			| Ok(item) => return Some(item),
			| Err(error) => debug_warn!(?error, "Skipping an undecodable timeline row"),
		}
	}
}
