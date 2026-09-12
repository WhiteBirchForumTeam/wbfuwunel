//! `Event/Subscribe` and `Event/Unsubscribe`: what a WebSocket connection
//! listens to (`docs/design/wbf-event-push.md`).
//!
//! A subscription is the connection's: `Subscribe` puts this connection into
//! the channels of the rooms it names (each checked for membership), or of
//! every room the user has joined when it names none, in which case rooms
//! joined later are followed too. With `cg_seq` the events newer than the
//! client's watermark are pushed first, the subscriber being registered
//! before that window is read so nothing appended in between is missed (a
//! duplicate is possible instead; the client drops it by `event_id`).
//! `Unsubscribe` leaves the named channels, or all of them.

use ruma::{OwnedRoomId, UserId};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	matrix::pdu::PduCount,
	wbf::{PackView, RejectCode},
};
use tuwunel_service::{Services, streams::PushedEvent};

use super::{Failure, PackContext, Reject, Reply, ack, parse_meta, recent};

#[derive(Default, Deserialize)]
struct SubscribeMeta {
	#[serde(default)]
	rooms: Option<Vec<OwnedRoomId>>,
	#[serde(default)]
	cg_seq: Option<i64>,
}

#[derive(Default, Deserialize)]
struct UnsubscribeMeta {
	#[serde(default)]
	rooms: Option<Vec<OwnedRoomId>>,
}

/// Args:
///     ctx: the connection (its number is the subscriber) and its session
///     view: meta example: `{"rooms":["!r:localhost"],"cg_seq":4711}` or `{}`
///     reply: the Ack goes here; caught-up events go straight to the
///         connection's queue as `Push` packs
/// Return:
///     Result<(), Failure>  Ack meta `{latest_g_seq, joined, skipped}`;
///     `skipped` lists rooms the user is not in — the named ones checked
///     before subscribing, plus any left between that check and the
///     registration.
pub(super) async fn handle_subscribe(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let user = ctx.user()?;
	let queue = reply
		.websocket_queue()
		.ok_or_else(|| Reject::code(RejectCode::Unsupported, "subscriptions need the WebSocket channel"))?;
	let meta: SubscribeMeta = parse_meta(view, "Subscribe")?;

	let (rooms, mut skipped, account_wide) = match meta.rooms {
		| Some(named) => {
			let mut rooms = Vec::with_capacity(named.len());
			let mut skipped = Vec::new();
			for room in named {
				if services.state_cache.is_joined(user, &room).await {
					rooms.push(room);
				} else {
					skipped.push(room);
				}
			}
			(rooms, skipped, false)
		},
		| None => (joined_rooms(services, user).await, Vec::new(), true),
	};

	// Registered before the catch-up window is read (event-push 3): a join
	// or an append during the read reaches the channel, at worst twice.
	let subscribed = services.streams.subscribe(
		ctx.connection,
		user,
		queue,
		view.header.id,
		&rooms,
		account_wide,
	);

	// Registered, so a leave from here on finds a subscriber to evict — but
	// one that landed between the membership check above and that
	// registration found none, and nothing would come back for it. Ask the
	// database again and drop whatever the user is no longer in.
	let left_since_the_check = list_rooms_no_longer_joined(services, user, &rooms).await;
	if !left_since_the_check.is_empty() {
		services
			.streams
			.unsubscribe(ctx.connection, &left_since_the_check);
		skipped.extend(left_since_the_check.iter().cloned());
	}
	let joined = subscribed
		.joined
		.saturating_sub(left_since_the_check.len());

	// Read before the window, like `Recent`: a client that stores it never
	// misses an event appended while the window was being read.
	let latest_g_seq = PduCount::Normal(services.globals.current_count()).into_signed();

	reply
		.send(ack(
			view.header.id,
			view.header.seq,
			json!({
				"latest_g_seq": latest_g_seq,
				"joined": joined,
				"skipped": skipped,
			}),
			Vec::new(),
		))
		.await?;

	if let Some(cg_seq) = meta.cg_seq.filter(|cg_seq| *cg_seq != 0) {
		let window = recent::window_after(services, user, Some(PduCount::from_signed(cg_seq)), services.config.wbf_recent_max_limit).await;
		let events: Vec<PushedEvent<'_>> = window
			.iter()
			.map(|event| PushedEvent { g_seq: event.g_seq, json: &event.json })
			.collect();
		services
			.streams
			.push_window(
				ctx.connection,
				&events,
				services.config.wbf_push_max_events_per_pack,
				services.config.wbf_data_max_bytes,
			);
	}

	Ok(())
}

/// Args:
///     view: meta example: `{"rooms":["!r:localhost"]}` or `{}` (everything)
/// Return:
///     Result<(), Failure>  Ack `{}`; leaving a channel the connection is not
///     in is a no-op.
pub(super) async fn handle_unsubscribe(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let meta: UnsubscribeMeta = parse_meta(view, "Unsubscribe")?;
	match meta.rooms {
		| Some(rooms) => services.streams.unsubscribe(ctx.connection, &rooms),
		| None => services.streams.unsubscribe_all_rooms(ctx.connection),
	}

	reply
		.send(ack(view.header.id, view.header.seq, json!({}), Vec::new()))
		.await?;

	Ok(())
}

/// The rooms among `rooms` the user is not in any more, read after the
/// connection was registered as a subscriber.
///
/// A leave (left, kicked, banned) that lands between the membership check and
/// the registration evicts nobody — the registry has no such subscriber yet —
/// and nothing brings the hook back, so the channel would push that room to a
/// non-member until the connection closed. The channels are a projection of
/// the database's membership, and this is the one place the projection can be
/// written from stale truth, so the truth is read once more.
///
/// Args:
///     user: who the connection is, example: `@alice:localhost`
///     rooms: what was just subscribed, example: every joined room
/// Return:
///     Vec<OwnedRoomId>  empty in the ordinary case; the rooms to leave again
///     otherwise.
async fn list_rooms_no_longer_joined(services: &Services, user: &UserId, rooms: &[OwnedRoomId]) -> Vec<OwnedRoomId> {
	let mut left = Vec::new();
	for room in rooms {
		if !services.state_cache.is_joined(user, room).await {
			left.push(room.clone());
		}
	}

	left
}

async fn joined_rooms(services: &Services, user: &UserId) -> Vec<OwnedRoomId> {
	use futures::StreamExt;

	services
		.state_cache
		.rooms_joined(user)
		.map(ToOwned::to_owned)
		.collect()
		.await
}
