//! The room channels (`0x14 Event`): one topic per room, the two membership
//! hooks, and the `Push` pack's shape. Everything about *who is subscribed*
//! and *how a push is queued* is the shared core (`subscribers.rs`); this is
//! only what the rooms decide for themselves.
//!
//! A channel is one room's set of subscribed connections. It exists only in
//! memory and only while someone is subscribed. Who may be in it is decided
//! by the room's membership in the database; this registry is a projection of
//! that truth, kept current by two hooks — `follow` at join and `evict` at
//! leave — both called from `state_cache`'s membership writes.

use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::json;
use tokio::sync::mpsc::Sender;
use tuwunel_core::{
	debug,
	wbf::{
		Flags, Kind, PackBuilder, PackError,
		events::{length_prefixed, list_pack_ranges},
	},
};

use super::{ConnectionId, Outgoing, Streams};

/// `Event/Push`, server to client only.
pub const EVENT_PUSH_SUBTYPE: u8 = 0x06;

/// One event as it goes on the wire: its global position and its JSON as
/// served to clients.
pub struct PushedEvent<'a> {
	pub g_seq: i64,
	pub json: &'a [u8],
}

/// What a `Subscribe` entered.
#[derive(Clone, Copy)]
pub struct Subscribed {
	/// How many channels this connection was not already in.
	pub joined: usize,
}

/// What a room subscriber is indexed by.
///
/// A subscription that named no rooms follows the rooms its user joins later,
/// and that is a topic of its own rather than a flag on the subscriber: the
/// join hook then asks the same index everything else asks, instead of a
/// second table that can disagree with it.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) enum RoomTopic {
	Room(OwnedRoomId),
	/// The user's subscriptions that follow every room they join.
	FollowsJoins(OwnedUserId),
}

impl Streams {
	/// Puts `connection` into the channels of `rooms` (which the caller has
	/// already checked the user is a member of), registering it as a
	/// subscriber first so a join happening during the call is not missed.
	///
	/// Args:
	///     connection: example: 7
	///     user: who the connection is, from its Session
	///     queue: the connection's send queue
	///     id: the client's `Subscribe` id, example: 42
	///     rooms: the channels to enter, example: every room the user joined
	///     account_wide: true when the client named no rooms, so rooms joined
	///         later are entered by the join hook
	/// Return:
	///     Subscribed  how many channels were newly entered.
	pub fn subscribe(
		&self,
		connection: ConnectionId,
		user: &UserId,
		queue: Sender<Outgoing>,
		id: u64,
		rooms: &[OwnedRoomId],
		account_wide: bool,
	) -> Subscribed {
		let mut topics: Vec<RoomTopic> = rooms
			.iter()
			.map(|room| RoomTopic::Room(room.clone()))
			.collect();
		if account_wide {
			topics.push(RoomTopic::FollowsJoins(user.to_owned()));
		}

		let entered = self
			.rooms
			.subscribe(connection, user, queue, id, &topics);

		// The follow topic is not a channel; it must not count as one joined.
		let joined = if account_wide {
			entered.saturating_sub(1)
		} else {
			entered
		};

		Subscribed { joined }
	}

	/// Takes `connection` out of the channels of `rooms`; rooms it is not in
	/// are no-ops. The subscriber stays registered either way — in no channel
	/// it simply receives nothing — and keeps following joins; only
	/// `unsubscribe_all` and dropping the connection forget it.
	pub fn unsubscribe(&self, connection: ConnectionId, rooms: &[OwnedRoomId]) {
		let topics: Vec<RoomTopic> = rooms
			.iter()
			.map(|room| RoomTopic::Room(room.clone()))
			.collect();
		self.rooms.unsubscribe(connection, &topics);
	}

	/// Takes `connection` out of every channel and forgets it as a room
	/// subscriber. Idempotent.
	pub fn unsubscribe_all(&self, connection: ConnectionId) { self.rooms.remove_connection(connection); }

	/// The join hook: `user` has joined `room`, so the user's subscriptions
	/// that follow joins enter the room's channel.
	///
	/// ⚠️ One transaction (`copy_topic`), not "look them up, then enter
	/// them": a connection that closed in between would otherwise be entered
	/// with no subscriber behind it, and nothing cleans that up.
	pub fn follow(&self, user: &UserId, room: &RoomId) {
		self.rooms.copy_topic(
			&RoomTopic::FollowsJoins(user.to_owned()),
			&RoomTopic::Room(room.to_owned()),
		);
	}

	/// The leave hook: `user` is no longer in `room` (left, kicked, banned),
	/// so every one of the user's connections leaves the room's channel.
	///
	/// ⚠️ One transaction for the same reason: between two locks the
	/// connection could have become somebody else's (a `Login` on it), and
	/// this would evict the new identity's subscription.
	pub fn evict(&self, user: &UserId, room: &RoomId) {
		self.rooms
			.leave_topic_of_user(user, &RoomTopic::Room(room.to_owned()));
	}

	/// Whether anyone listens to `room`: the cheap check before an event is
	/// serialized for pushing.
	#[must_use]
	pub fn is_listened(&self, room: &RoomId) -> bool { self.rooms.is_listened(&RoomTopic::Room(room.to_owned())) }

	/// Who is listening to `room`, for the caller to filter (ignored senders)
	/// before `push_to_room`.
	///
	/// Return:
	///     Vec<(ConnectionId, OwnedUserId)>  empty when there is no channel.
	#[must_use]
	pub fn listeners(&self, room: &RoomId) -> Vec<(ConnectionId, OwnedUserId)> {
		self.rooms.listeners(&RoomTopic::Room(room.to_owned()))
	}

	/// How many connections listen to `room`; for tests and the admin room.
	#[must_use]
	pub fn listener_count(&self, room: &RoomId) -> usize {
		self.rooms
			.listener_count(&RoomTopic::Room(room.to_owned()))
	}

	/// Pushes `events` to each of `connections` as one `Push` pack per
	/// connection, never waiting: a full queue drops the pack and marks the
	/// subscriber's next push with `gap`.
	///
	/// Args:
	///     connections: from `listeners`, minus whoever the caller filtered
	///     events: newest first, example: the one event just appended
	pub fn push(&self, connections: &[ConnectionId], events: &[PushedEvent<'_>]) {
		self.push_inner(None, connections, events);
	}

	/// `push`, but a connection that has left `room` between the caller's
	/// `listeners` snapshot and here is dropped: membership is read again
	/// under the lock, so a kick that lands during the caller's `await` does
	/// not get one more event through.
	pub fn push_to_room(&self, room: &RoomId, connections: &[ConnectionId], events: &[PushedEvent<'_>]) {
		self.push_inner(Some(RoomTopic::Room(room.to_owned())), connections, events);
	}

	fn push_inner(&self, topic: Option<RoomTopic>, connections: &[ConnectionId], events: &[PushedEvent<'_>]) {
		if events.is_empty() {
			return;
		}
		let Ok(data) = length_prefixed(events.iter().map(|event| event.json)) else {
			// Nobody can be sent this; say so on their next push rather than
			// dropping it silently, so the client fills in with `Recent`.
			debug!("wbf push skipped: an event does not fit a length prefix");
			self.rooms.mark_gap(connections);
			return;
		};
		let fs = events.first().map_or(0, |event| event.g_seq);
		let ls = events.last().map_or(0, |event| event.g_seq);
		let bc = events.len();

		self.rooms
			.push_with(topic.as_ref(), connections, |id, seq, gap| {
				push_pack(id, seq, bc, fs, ls, gap, &data)
			});
	}

	/// Pushes a window of events to one connection for a `Subscribe` that
	/// asked to be caught up from `cg_seq`. Same drop rule as `push`.
	///
	/// Args:
	///     connection: the subscriber being caught up
	///     events: newest first, example: the 25 events after `cg_seq`
	///     per_pack: `wbf_push_max_events_per_pack`, example: 10
	///     data_max: `wbf_data_max_bytes`; a pack is cut here too, so a
	///         window of large events cannot exceed the connection's
	///         message size
	pub fn push_window(&self, connection: ConnectionId, events: &[PushedEvent<'_>], per_pack: usize, data_max: usize) {
		for range in list_pack_ranges(events.iter().map(|event| event.json.len()), per_pack, data_max) {
			self.push(&[connection], &events[range]);
		}
	}

	/// Forwards a pack as it is to everyone listening to `room`, except
	/// `except` (the connection that sent it, when the sender must not get
	/// its own copy). For `Stream` drafts. Same drop rule as `push`, but no
	/// gap is recorded: a dropped draft piece is recovered by the client's
	/// own `Demand`, not by `Recent`.
	pub fn relay(&self, room: &RoomId, except: Option<ConnectionId>, pack: &[u8]) {
		let listeners: Vec<ConnectionId> = self
			.rooms
			.listeners(&RoomTopic::Room(room.to_owned()))
			.into_iter()
			.map(|(connection, _)| connection)
			.filter(|connection| Some(*connection) != except)
			.collect();

		for connection in listeners {
			let _dropped_or_gone = self.rooms.send_pack(connection, pack.to_vec());
		}
	}
}

fn push_pack(id: u64, seq: u32, bc: usize, fs: i64, ls: i64, gap: bool, data: &[u8]) -> Result<Vec<u8>, PackError> {
	Ok(PackBuilder::new(Kind::Event, EVENT_PUSH_SUBTYPE, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({ "bc": bc, "fs": fs, "ls": ls, "gap": gap }))?
		.data(data)?
		.finish())
}

#[cfg(test)]
mod tests {
	use ruma::{room_id, user_id};
	use tokio::sync::mpsc;
	use tuwunel_core::wbf::{
		Kind, decode,
		events::{framed_len, split_length_prefixed},
	};

	use super::{EVENT_PUSH_SUBTYPE, PushedEvent};
	use crate::streams::{Outgoing, Streams};

	fn queue(capacity: usize) -> (mpsc::Sender<Outgoing>, mpsc::Receiver<Outgoing>) { mpsc::channel(capacity) }

	fn take_pack(rx: &mut mpsc::Receiver<Outgoing>) -> Vec<u8> {
		match rx.try_recv().expect("a pack was queued") {
			| Outgoing::Pack(pack) => pack,
			| Outgoing::Close { .. } => panic!("expected a pack, got a close"),
		}
	}

	#[test]
	fn subscribe_is_idempotent_and_unsubscribe_of_a_stranger_is_a_no_op() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, _rx) = queue(4);

		let first = streams.subscribe(1, alice, tx.clone(), 9, &[room.clone()], false);
		let again = streams.subscribe(1, alice, tx, 9, &[room.clone()], false);
		assert_eq!(first.joined, 1);
		assert_eq!(again.joined, 0, "the second subscribe entered nothing new");
		assert_eq!(streams.listener_count(&room), 1);

		streams.unsubscribe(1, &[room_id!("!other:localhost").to_owned()]);
		streams.unsubscribe(99, &[room.clone()]);
		assert_eq!(streams.listener_count(&room), 1);

		streams.unsubscribe(1, &[room.clone()]);
		assert_eq!(streams.listener_count(&room), 0);
		assert!(!streams.is_listened(&room), "an empty channel is removed");
	}

	#[test]
	fn follow_adds_only_account_wide_subscribers_and_evict_removes_all() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let old = room_id!("!old:localhost").to_owned();
		let new = room_id!("!new:localhost").to_owned();
		let (tx, _rx) = queue(4);
		streams.subscribe(1, alice, tx.clone(), 1, &[old.clone()], true);
		streams.subscribe(2, alice, tx, 1, &[old.clone()], false);

		streams.follow(alice, &new);
		assert_eq!(streams.listener_count(&new), 1, "only the account-wide connection followed");
		assert_eq!(streams.listeners(&new)[0].0, 1);

		streams.evict(alice, &old);
		assert_eq!(streams.listener_count(&old), 0, "both connections left");
		assert!(streams.is_subscribed(2), "evicting from a room does not unsubscribe the connection");
	}

	#[test]
	fn push_numbers_packs_per_subscriber_and_a_full_queue_sets_gap() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, mut rx) = queue(1);
		streams.subscribe(1, alice, tx, 42, &[room.clone()], false);
		let listeners: Vec<u64> = streams.listeners(&room).into_iter().map(|(c, _)| c).collect();

		streams.push(&listeners, &[PushedEvent { g_seq: 10, json: b"{\"e\":1}" }]);
		// The queue holds one; this one is dropped and marks the gap.
		streams.push(&listeners, &[PushedEvent { g_seq: 11, json: b"{\"e\":2}" }]);

		let mut first = take_pack(&mut rx);
		let view = decode(&mut first).expect("decodes");
		assert_eq!(view.header.kind, Kind::Event);
		assert_eq!(view.header.subtype, EVENT_PUSH_SUBTYPE);
		assert_eq!(view.header.id, 42);
		assert_eq!(view.header.seq, 0);
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["bc"], 1);
		assert_eq!(meta["fs"], 10);
		assert_eq!(meta["gap"], false);
		assert_eq!(split_length_prefixed(view.data).expect("events"), vec![b"{\"e\":1}".as_slice()]);

		// Room again: the next push carries gap=true and seq 2 (1 was the drop).
		streams.push(&listeners, &[PushedEvent { g_seq: 12, json: b"{\"e\":3}" }]);
		let mut third = take_pack(&mut rx);
		let view = decode(&mut third).expect("decodes");
		assert_eq!(view.header.seq, 2);
		assert_eq!(view.meta_json().expect("meta")["gap"], true);

		// And the gap is cleared once reported.
		streams.push(&listeners, &[PushedEvent { g_seq: 13, json: b"{\"e\":4}" }]);
		let mut fourth = take_pack(&mut rx);
		assert_eq!(decode(&mut fourth).expect("decodes").meta_json().expect("meta")["gap"], false);
	}

	#[test]
	fn push_window_cuts_by_per_pack() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, mut rx) = queue(8);
		streams.subscribe(1, alice, tx, 1, &[room], false);
		let events: Vec<PushedEvent<'_>> = (0..5).map(|n| PushedEvent { g_seq: 100 - n, json: b"{}" }).collect();

		streams.push_window(1, &events, 2, 1024);
		let sizes: Vec<usize> = (0..3)
			.map(|_| {
				let mut pack = take_pack(&mut rx);
				let view = decode(&mut pack).expect("decodes");
				split_length_prefixed(view.data).expect("events").len()
			})
			.collect();
		assert_eq!(sizes, vec![2, 2, 1]);
		assert!(rx.try_recv().is_err(), "nothing more");
	}

	#[test]
	fn a_join_hook_enters_nobody_when_the_connection_is_already_gone() {
		// ⚠️ This asserts the end state, not the race that produced it: the
		// window PR #42's review found (cirno) needs the lookup and the
		// insert to be two lock-takings, with the connection closing in
		// between, and that is not reproducible here without threads. What
		// closes it is that `copy_topic` is one transaction and that there
		// is no longer an API to do it in two — `enter_topic` is gone. The
		// invariant this guards is the visible half: a room that no
		// subscriber is behind must not read as listened-to, because nothing
		// would ever clean that up.
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let later = room_id!("!later:localhost");
		let (tx, _rx) = queue(4);
		streams.subscribe(1, alice, tx, 1, &[], true);
		streams.unsubscribe_all(1);

		streams.follow(alice, later);

		assert_eq!(streams.listener_count(later), 0, "a room with no subscriber behind it is not listened to");
		assert!(!streams.is_listened(later));
	}

	#[test]
	fn subscribing_as_another_user_leaves_the_old_identity_behind() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let bob = user_id!("@bob:localhost");
		let hers = room_id!("!hers:localhost").to_owned();
		let his = room_id!("!his:localhost").to_owned();
		let (tx, _rx) = queue(4);
		streams.subscribe(1, alice, tx.clone(), 1, &[hers.clone()], true);

		// The same connection, now logged in as bob.
		streams.subscribe(1, bob, tx, 2, &[his.clone()], false);

		assert_eq!(streams.listener_count(&hers), 0, "alice's channel let the connection go");
		assert_eq!(streams.listener_count(&his), 1);
		streams.follow(alice, &room_id!("!later:localhost").to_owned());
		assert_eq!(streams.listener_count(room_id!("!later:localhost")), 0, "alice's join hook no longer finds it");
		streams.evict(bob, &his);
		assert_eq!(streams.listener_count(&his), 0, "bob's leave hook does find it");
	}

	#[test]
	fn a_push_to_a_room_skips_whoever_left_it_since_the_snapshot() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, mut rx) = queue(4);
		streams.subscribe(1, alice, tx, 7, &[room.clone()], false);
		let listeners: Vec<u64> = streams.listeners(&room).into_iter().map(|(c, _)| c).collect();

		// What a kick during the caller's ignore lookup does to the snapshot.
		streams.evict(alice, &room);
		streams.push_to_room(&room, &listeners, &[PushedEvent { g_seq: 10, json: b"{}" }]);
		assert!(rx.try_recv().is_err(), "the event stopped at the channel it had left");

		// The connection is still subscribed, so a push of its own arrives.
		streams.push(&listeners, &[PushedEvent { g_seq: 11, json: b"{}" }]);
		assert!(matches!(rx.try_recv(), Ok(Outgoing::Pack(_))));
	}

	#[test]
	fn push_window_cuts_by_bytes_before_the_count_cap_is_reached() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, mut rx) = queue(8);
		streams.subscribe(1, alice, tx, 1, &[room], false);
		let body = vec![b'x'; 200];
		let events: Vec<PushedEvent<'_>> = (0..4).map(|n| PushedEvent { g_seq: 100 - n, json: &body }).collect();

		// Room for two events per pack by bytes, while the count cap (10) is
		// nowhere near: four events must still leave as two packs.
		let data_max = 2 * framed_len(body.len());
		streams.push_window(1, &events, 10, data_max);

		for expected_seq in 0..2 {
			let mut pack = take_pack(&mut rx);
			let view = decode(&mut pack).expect("decodes");
			assert_eq!(view.header.seq, expected_seq);
			assert_eq!(split_length_prefixed(view.data).expect("events").len(), 2);
			assert!(view.data.len() <= data_max, "a pack stays inside wbf_data_max_bytes");
		}
		assert!(rx.try_recv().is_err(), "two packs, not one oversized one");
	}

	#[test]
	fn relay_skips_the_excepted_connection() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let bob = user_id!("@bob:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx_a, mut rx_a) = queue(4);
		let (tx_b, mut rx_b) = queue(4);
		streams.subscribe(1, alice, tx_a, 1, &[room.clone()], false);
		streams.subscribe(2, bob, tx_b, 1, &[room.clone()], false);

		streams.relay(&room, Some(1), b"pack");
		assert!(rx_a.try_recv().is_err(), "the sender's connection got nothing");
		assert_eq!(take_pack(&mut rx_b), b"pack");

		streams.relay(&room, None, b"all");
		assert_eq!(take_pack(&mut rx_a), b"all");
		assert_eq!(take_pack(&mut rx_b), b"all");
	}
}
