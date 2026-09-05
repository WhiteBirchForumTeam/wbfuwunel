//! `Event/Recent`: the newest events across every room the user is joined
//! to, in the order this server received them, newest first.
//!
//! There is no global index of events; there does not need to be one. The
//! global count every event carries is comparable across rooms, so one
//! reverse stream per joined room merged by count is the global order. The
//! cursor is the count of the last event returned, in the same string form as
//! a `/messages` token; the client passes it back as `before` and never reads
//! it. See `docs/design/room-seq-and-recent.md` §2.

use std::{cmp::Ordering, collections::BinaryHeap, pin::Pin, str::FromStr};

use futures::{Stream, StreamExt};
use ruma::{OwnedRoomId, UserId, events::AnyTimelineEvent, serde::Raw};
use serde_json::json;
use tuwunel_core::{
	Result, debug_warn,
	matrix::{event::Event, pdu::PduCount},
	wbf::PackView,
};
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

use super::{Reject, ack};
use crate::client::message::{ignored_filter, visibility_filter};

/// How the request names its page.
struct RecentRequest {
	limit: usize,
	before: Option<PduCount>,
}

impl RecentRequest {
	/// Args:
	///     view: the request pack, meta example: `{"limit":100,"before":"4711"}`
	///     max_limit: `wbf_recent_max_limit`, example: 10000
	/// Return:
	///     Result<RecentRequest, Reject>  Conflict when `before` is not a count.
	fn parse(view: &PackView<'_>, max_limit: usize) -> std::result::Result<Self, Reject> {
		let meta = if view.meta.is_empty() { json!({}) } else { view.meta_json()? };

		let limit = meta["limit"]
			.as_u64()
			.and_then(|limit| usize::try_from(limit).ok())
			.map_or(max_limit, |limit| limit.min(max_limit));

		let before = match &meta["before"] {
			| serde_json::Value::Null => None,
			| serde_json::Value::String(token) => Some(
				PduCount::from_str(token).map_err(|_| Reject::code("Conflict", "`before` is not a cursor this server issued"))?,
			),
			| _ => return Err(Reject::code("Conflict", "`before` must be a string cursor")),
		};

		Ok(Self { limit, before })
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

pub(super) async fn handle_event_recent(
	services: &Services,
	user: &UserId,
	view: &PackView<'_>,
) -> std::result::Result<Vec<u8>, Reject> {
	let request = RecentRequest::parse(view, services.config.wbf_recent_max_limit)?;
	let data_max = services.config.wbf_data_max_bytes;

	let rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_joined(user)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	// One reverse stream per room, each already past the cursor. The heads
	// hold the event the heap is ranking; the streams wait behind them.
	let mut streams: Vec<RoomStream<'_>> = rooms
		.iter()
		.map(|room_id| {
			let stream = services
				.timeline
				.pdus_rev(Some(user), room_id, request.before);
			Box::pin(stream) as RoomStream<'_>
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

	while returned < request.limit {
		let Some(Head { room, .. }) = heap.pop() else {
			break;
		};
		let Some(item) = heads[room].take() else {
			break;
		};

		// Refill this room before anything else, so the heap always ranks
		// every room's newest unseen event.
		if let Some(next) = next_item(&mut streams[room]).await {
			heap.push(Head { count: next.0, room });
			heads[room] = Some(next);
		}

		let count = item.0;
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
			// Put the event back conceptually: the cursor stays at the last
			// event that was returned, so this one leads the next page.
			heap.clear();
			heap.push(Head { count, room: usize::MAX });
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

	// More remains when any room still has a candidate; then the cursor is
	// the last event this page covered. Exhausted rooms end the pagination.
	let next = (!heap.is_empty())
		.then_some(last_count)
		.flatten()
		.map(|count| count.to_string());

	Ok(ack(view.header.id, view.header.seq, json!({ "count": returned, "next": next }), data))
}

/// The room's next event, skipping rows that fail to decode.
async fn next_item(stream: &mut RoomStream<'_>) -> Option<PdusIterItem> {
	loop {
		match stream.next().await? {
			| Ok(item) => return Some(item),
			| Err(error) => debug_warn!(?error, "Skipping an undecodable timeline row"),
		}
	}
}
