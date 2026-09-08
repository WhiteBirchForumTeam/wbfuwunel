//! Channels: who is listening to which room over a wbf WebSocket, and the
//! push of new events to them (`docs/design/wbf-event-push.md`).
//!
//! A channel is one room's set of subscribed connections. It exists only in
//! memory and only while someone is subscribed: the first subscriber creates
//! it, the last one to leave removes it. Who may be in it is decided by the
//! room's membership in the database; this registry is a projection of that
//! truth kept current by two hooks, `follow` at join and `evict` at leave,
//! both called from `state_cache`'s membership writes. A restart empties it,
//! and the safe direction is fewer pushes: a client recovers with `Recent`.
//!
//! A subscriber is a connection (`ConnectionId`, issued at upgrade), never a
//! user and never a device: a user's every device must receive, and which of
//! a device's connections handles events is the client's business. The
//! connection's `Session` says who it is.
//!
//! Pushing never blocks the event path: a full send queue drops the push and
//! marks the subscriber's next push with `gap`, so the client knows to fill
//! in with `Recent`. Nothing here is retried or stored.

use std::{
	collections::{HashMap, HashSet},
	sync::{
		Arc, RwLock,
		atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
	},
};

use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::json;
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tuwunel_core::{
	debug,
	wbf::{Flags, Kind, PackBuilder, PackError, events::length_prefixed},
};

/// One WebSocket connection, numbered at upgrade; unique for the life of the
/// process, meaningless outside it.
pub type ConnectionId = u64;

/// `Event/Push`, server to client only.
pub const EVENT_PUSH_SUBTYPE: u8 = 0x06;

/// Something queued for a connection's send task. Defined here rather than
/// in the API crate because publishing happens in the service layer.
pub enum Outgoing {
	Pack(Vec<u8>),
	/// The close frame that ends the connection after everything queued
	/// before it.
	Close { code: u16, reason: &'static str },
}

/// One event as it goes on the wire: its global position and its JSON as
/// served to clients.
pub struct PushedEvent<'a> {
	pub g_seq: i64,
	pub json: &'a [u8],
}

/// A subscribed connection. Which device it is lives in the connection's
/// `Session`; the registry only needs the user, for the join and leave hooks.
struct Subscriber {
	user: OwnedUserId,
	queue: Sender<Outgoing>,
	/// The client's `Subscribe` id, copied into every `Push`.
	id: u64,
	/// The next `Push`'s `seq`.
	seq: AtomicU32,
	/// A push was dropped since the last one that went out.
	gap: AtomicBool,
	/// Follows rooms the user joins later (a `Subscribe` without `rooms`).
	account_wide: bool,
}

/// What the read lock hands out for one push: enough to build and send the
/// pack without holding the lock.
struct Target {
	queue: Sender<Outgoing>,
	id: u64,
	seq: u32,
	gap: bool,
	/// To record a drop after the lock is gone.
	connection: ConnectionId,
}

#[derive(Default)]
struct Registry {
	/// The channels: room to the connections listening to it.
	channels: HashMap<OwnedRoomId, HashSet<ConnectionId>>,
	subscribers: HashMap<ConnectionId, Subscriber>,
	/// A user's subscribed connections, for the join and leave hooks.
	by_user: HashMap<OwnedUserId, HashSet<ConnectionId>>,
}

type Shared = Arc<RwLock<Registry>>;

pub struct Channels {
	registry: Shared,
	next_connection: AtomicU64,
}

/// Held by a connection's serving loop for its whole life; dropping it
/// unsubscribes the connection from everything, whichever way the loop
/// ended.
pub struct ConnectionGuard {
	registry: Shared,
	connection: ConnectionId,
}

impl Drop for ConnectionGuard {
	fn drop(&mut self) {
		let mut registry = self.registry.write().expect("channels lock poisoned");
		registry.remove_subscriber(self.connection);
	}
}

/// What `subscribe` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Subscribed {
	/// Channels the connection is now in because of this call (rooms it was
	/// already in are not counted: subscribing twice is a no-op).
	pub joined: usize,
}

impl Default for Channels {
	fn default() -> Self { Self::new() }
}

impl Channels {
	#[must_use]
	pub fn new() -> Self {
		Self {
			registry: Arc::new(RwLock::new(Registry::default())),
			next_connection: AtomicU64::new(1),
		}
	}

	/// A fresh connection number. Starts at 1; 0 is never issued, so it can
	/// mean "no connection" where one is optional.
	pub fn next_connection_id(&self) -> ConnectionId { self.next_connection.fetch_add(1, Ordering::Relaxed) }

	/// Return:
	///     ConnectionGuard  to keep for the connection's life.
	#[must_use = "dropping the guard at once unsubscribes the connection at once"]
	pub fn connection_guard(&self, connection: ConnectionId) -> ConnectionGuard {
		ConnectionGuard { registry: Arc::clone(&self.registry), connection }
	}

	/// Puts `connection` into the channels of `rooms` (which the caller has
	/// already checked the user is a member of), registering it as a
	/// subscriber first so a join happening during the call is not missed.
	///
	/// Args:
	///     connection: example: 7
	///     user: who the connection is, from its Session
	///     queue: the connection's send queue
	///     id: the client's `Subscribe` id, example: 42
	///     rooms: the channels to enter, example: every room the user has joined
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
		let mut registry = self.registry.write().expect("channels lock poisoned");

		// Register (or refresh) the subscriber before touching any channel.
		match registry.subscribers.get_mut(&connection) {
			| Some(existing) => {
				if existing.id != id {
					// A new subscription id starts its own push sequence.
					existing.id = id;
					existing.seq.store(0, Ordering::Relaxed);
					existing.gap.store(false, Ordering::Relaxed);
				}
				existing.queue = queue;
				existing.account_wide |= account_wide;
			},
			| None => {
				registry.subscribers.insert(connection, Subscriber {
					user: user.to_owned(),
					queue,
					id,
					seq: AtomicU32::new(0),
					gap: AtomicBool::new(false),
					account_wide,
				});
			},
		}
		registry
			.by_user
			.entry(user.to_owned())
			.or_default()
			.insert(connection);

		let mut joined = 0;
		for room in rooms {
			if registry
				.channels
				.entry(room.clone())
				.or_default()
				.insert(connection)
			{
				joined += 1;
			}
		}

		Subscribed { joined }
	}

	/// Takes `connection` out of the channels of `rooms`; rooms it is not in
	/// are no-ops. The subscriber stays registered (and account-wide, if it
	/// was) unless it is in no channel afterwards and was not account-wide.
	pub fn unsubscribe(&self, connection: ConnectionId, rooms: &[OwnedRoomId]) {
		let mut registry = self.registry.write().expect("channels lock poisoned");
		for room in rooms {
			registry.leave_channel(connection, room);
		}
	}

	/// Takes `connection` out of every channel and forgets it. Idempotent.
	pub fn unsubscribe_all(&self, connection: ConnectionId) {
		let mut registry = self.registry.write().expect("channels lock poisoned");
		registry.remove_subscriber(connection);
	}

	/// The join hook: `user` has joined `room`, so the user's account-wide
	/// subscribers enter the room's channel.
	pub fn follow(&self, user: &UserId, room: &RoomId) {
		let mut registry = self.registry.write().expect("channels lock poisoned");
		let Some(connections) = registry.by_user.get(user).cloned() else {
			return;
		};
		let followers: Vec<ConnectionId> = connections
			.into_iter()
			.filter(|connection| {
				registry
					.subscribers
					.get(connection)
					.is_some_and(|subscriber| subscriber.account_wide)
			})
			.collect();
		if followers.is_empty() {
			return;
		}
		let channel = registry.channels.entry(room.to_owned()).or_default();
		for connection in followers {
			channel.insert(connection);
		}
	}

	/// The leave hook: `user` is no longer in `room` (left, kicked, banned),
	/// so every one of the user's connections leaves the room's channel.
	pub fn evict(&self, user: &UserId, room: &RoomId) {
		let mut registry = self.registry.write().expect("channels lock poisoned");
		let Some(connections) = registry.by_user.get(user).cloned() else {
			return;
		};
		for connection in connections {
			registry.leave_channel(connection, room);
		}
	}

	/// Whether anyone listens to `room`: the cheap check before an event is
	/// serialized for pushing.
	#[must_use]
	pub fn is_listened(&self, room: &RoomId) -> bool {
		self.registry
			.read()
			.expect("channels lock poisoned")
			.channels
			.contains_key(room)
	}

	/// Who is listening to `room`, for the caller to filter (ignored senders)
	/// before `push`.
	///
	/// Return:
	///     Vec<(ConnectionId, OwnedUserId)>  empty when there is no channel.
	#[must_use]
	pub fn listeners(&self, room: &RoomId) -> Vec<(ConnectionId, OwnedUserId)> {
		let registry = self.registry.read().expect("channels lock poisoned");
		let Some(channel) = registry.channels.get(room) else {
			return Vec::new();
		};
		channel
			.iter()
			.filter_map(|connection| {
				registry
					.subscribers
					.get(connection)
					.map(|subscriber| (*connection, subscriber.user.clone()))
			})
			.collect()
	}

	/// Pushes `events` to each of `connections` as one `Push` pack per
	/// connection, never waiting: a full queue drops the pack and marks the
	/// subscriber's next push with `gap`.
	///
	/// Args:
	///     connections: from `listeners`, minus whoever the caller filtered out
	///     events: newest first, example: the one event just appended
	pub fn push(&self, connections: &[ConnectionId], events: &[PushedEvent<'_>]) {
		if events.is_empty() {
			return;
		}
		let Ok(data) = length_prefixed(events.iter().map(|event| event.json)) else {
			debug!("wbf push skipped: an event does not fit a length prefix");
			return;
		};
		let fs = events.first().map_or(0, |event| event.g_seq);
		let ls = events.last().map_or(0, |event| event.g_seq);
		let bc = events.len();

		let targets = self.take_targets(connections);
		let mut dropped = Vec::new();
		for target in targets {
			let pack = match push_pack(target.id, target.seq, bc, fs, ls, target.gap, &data) {
				| Ok(pack) => pack,
				| Err(error) => {
					debug!(?error, "wbf push skipped: could not encode the pack");
					continue;
				},
			};
			match target.queue.try_send(Outgoing::Pack(pack)) {
				| Ok(()) => {},
				| Err(TrySendError::Full(_)) => dropped.push(target.connection),
				// The connection is gone; its guard cleans the registry.
				| Err(TrySendError::Closed(_)) => {},
			}
		}
		self.mark_gap(&dropped);
	}

	/// Pushes a window of events to one connection, `per_pack` events per
	/// `Push`, for a `Subscribe` that asked to be caught up from `cg_seq`.
	/// Same drop rule as `push`.
	pub fn push_window(&self, connection: ConnectionId, events: &[PushedEvent<'_>], per_pack: usize) {
		for chunk in events.chunks(per_pack.max(1)) {
			self.push(&[connection], chunk);
		}
	}

	/// Forwards a pack as it is to everyone listening to `room`, except
	/// `except` (the connection that sent it, when the sender must not get
	/// its own copy). For `Stream` drafts. Same drop rule as `push`, but no
	/// gap is recorded: a dropped draft piece is recovered by the client's
	/// own `Demand`, not by `Recent`.
	pub fn relay(&self, room: &RoomId, except: Option<ConnectionId>, pack: &[u8]) {
		let queues: Vec<Sender<Outgoing>> = {
			let registry = self.registry.read().expect("channels lock poisoned");
			let Some(channel) = registry.channels.get(room) else {
				return;
			};
			channel
				.iter()
				.filter(|connection| Some(**connection) != except)
				.filter_map(|connection| registry.subscribers.get(connection))
				.map(|subscriber| subscriber.queue.clone())
				.collect()
		};
		for queue in queues {
			let _dropped_or_gone = queue.try_send(Outgoing::Pack(pack.to_vec()));
		}
	}

	/// How many connections listen to `room`; for tests and the admin room.
	#[must_use]
	pub fn listener_count(&self, room: &RoomId) -> usize {
		self.registry
			.read()
			.expect("channels lock poisoned")
			.channels
			.get(room)
			.map_or(0, HashSet::len)
	}

	/// Whether `connection` is registered at all; for tests.
	#[must_use]
	pub fn is_subscribed(&self, connection: ConnectionId) -> bool {
		self.registry
			.read()
			.expect("channels lock poisoned")
			.subscribers
			.contains_key(&connection)
	}

	/// Snapshots what each connection's next push needs, advancing its `seq`
	/// and clearing its `gap`, under the read lock (the counters are atomic).
	fn take_targets(&self, connections: &[ConnectionId]) -> Vec<Target> {
		let registry = self.registry.read().expect("channels lock poisoned");
		connections
			.iter()
			.filter_map(|connection| {
				let subscriber = registry.subscribers.get(connection)?;
				Some(Target {
					queue: subscriber.queue.clone(),
					id: subscriber.id,
					seq: subscriber.seq.fetch_add(1, Ordering::Relaxed),
					gap: subscriber.gap.swap(false, Ordering::Relaxed),
					connection: *connection,
				})
			})
			.collect()
	}

	fn mark_gap(&self, connections: &[ConnectionId]) {
		if connections.is_empty() {
			return;
		}
		let registry = self.registry.read().expect("channels lock poisoned");
		for connection in connections {
			if let Some(subscriber) = registry.subscribers.get(connection) {
				subscriber.gap.store(true, Ordering::Relaxed);
			}
		}
	}
}

impl Registry {
	fn leave_channel(&mut self, connection: ConnectionId, room: &RoomId) {
		if let Some(channel) = self.channels.get_mut(room) {
			channel.remove(&connection);
			if channel.is_empty() {
				self.channels.remove(room);
			}
		}
	}

	fn remove_subscriber(&mut self, connection: ConnectionId) {
		let Some(subscriber) = self.subscribers.remove(&connection) else {
			return;
		};
		if let Some(connections) = self.by_user.get_mut(&subscriber.user) {
			connections.remove(&connection);
			if connections.is_empty() {
				self.by_user.remove(&subscriber.user);
			}
		}
		// Channels are not indexed by connection; walk them. A connection is
		// in as many channels as its user has rooms, and this runs once per
		// disconnect.
		self.channels
			.retain(|_, channel| {
				channel.remove(&connection);
				!channel.is_empty()
			});
	}
}

/// One `Event/Push` pack.
///
/// Args:
///     id: the subscription's id, example: 42
///     seq: this push's number, example: 0
///     bc, fs, ls: how many events, and the newest and oldest g_seq
///     gap: a push was dropped before this one
///     data: `length_prefixed` events
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
	use tuwunel_core::wbf::{Kind, decode, events::split_length_prefixed};

	use super::{Channels, EVENT_PUSH_SUBTYPE, Outgoing, PushedEvent};

	fn queue(capacity: usize) -> (mpsc::Sender<Outgoing>, mpsc::Receiver<Outgoing>) { mpsc::channel(capacity) }

	fn take_pack(rx: &mut mpsc::Receiver<Outgoing>) -> Vec<u8> {
		match rx.try_recv().expect("a pack was queued") {
			| Outgoing::Pack(pack) => pack,
			| Outgoing::Close { .. } => panic!("expected a pack, got a close"),
		}
	}

	#[test]
	fn subscribe_is_idempotent_and_unsubscribe_of_a_stranger_is_a_no_op() {
		let channels = Channels::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, _rx) = queue(4);

		let first = channels.subscribe(1, alice, tx.clone(), 9, &[room.clone()], false);
		let again = channels.subscribe(1, alice, tx, 9, &[room.clone()], false);
		assert_eq!(first.joined, 1);
		assert_eq!(again.joined, 0, "the second subscribe entered nothing new");
		assert_eq!(channels.listener_count(&room), 1);

		channels.unsubscribe(1, &[room_id!("!other:localhost").to_owned()]);
		channels.unsubscribe(99, &[room.clone()]);
		assert_eq!(channels.listener_count(&room), 1);

		channels.unsubscribe(1, &[room.clone()]);
		assert_eq!(channels.listener_count(&room), 0);
		assert!(!channels.is_listened(&room), "an empty channel is removed");
	}

	#[test]
	fn a_dropped_guard_leaves_every_channel() {
		let channels = Channels::new();
		let alice = user_id!("@alice:localhost");
		let a = room_id!("!a:localhost").to_owned();
		let b = room_id!("!b:localhost").to_owned();
		let (tx, _rx) = queue(4);
		let connection = channels.next_connection_id();
		let guard = channels.connection_guard(connection);
		channels.subscribe(connection, alice, tx, 1, &[a.clone(), b.clone()], true);
		assert!(channels.is_listened(&a) && channels.is_listened(&b));

		drop(guard);
		assert!(!channels.is_listened(&a) && !channels.is_listened(&b));
		assert!(!channels.is_subscribed(connection));
		assert!(channels.listeners(&a).is_empty());
	}

	#[test]
	fn follow_adds_only_account_wide_subscribers_and_evict_removes_all() {
		let channels = Channels::new();
		let alice = user_id!("@alice:localhost");
		let old = room_id!("!old:localhost").to_owned();
		let new = room_id!("!new:localhost").to_owned();
		let (tx, _rx) = queue(4);
		channels.subscribe(1, alice, tx.clone(), 1, &[old.clone()], true);
		channels.subscribe(2, alice, tx, 1, &[old.clone()], false);

		channels.follow(alice, &new);
		assert_eq!(channels.listener_count(&new), 1, "only the account-wide connection followed");
		assert_eq!(channels.listeners(&new)[0].0, 1);

		channels.evict(alice, &old);
		assert_eq!(channels.listener_count(&old), 0, "both connections left");
		assert!(channels.is_subscribed(2), "evicting from a room does not unsubscribe the connection");
	}

	#[test]
	fn push_numbers_packs_per_subscriber_and_a_full_queue_sets_gap() {
		let channels = Channels::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, mut rx) = queue(1);
		channels.subscribe(1, alice, tx, 42, &[room.clone()], false);
		let listeners: Vec<u64> = channels.listeners(&room).into_iter().map(|(c, _)| c).collect();

		channels.push(&listeners, &[PushedEvent { g_seq: 10, json: b"{\"e\":1}" }]);
		// The queue holds one; this one is dropped and marks the gap.
		channels.push(&listeners, &[PushedEvent { g_seq: 11, json: b"{\"e\":2}" }]);

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
		channels.push(&listeners, &[PushedEvent { g_seq: 12, json: b"{\"e\":3}" }]);
		let mut third = take_pack(&mut rx);
		let view = decode(&mut third).expect("decodes");
		assert_eq!(view.header.seq, 2);
		assert_eq!(view.meta_json().expect("meta")["gap"], true);

		// And the gap is cleared once reported.
		channels.push(&listeners, &[PushedEvent { g_seq: 13, json: b"{\"e\":4}" }]);
		let mut fourth = take_pack(&mut rx);
		assert_eq!(decode(&mut fourth).expect("decodes").meta_json().expect("meta")["gap"], false);
	}

	#[test]
	fn push_window_cuts_by_per_pack() {
		let channels = Channels::new();
		let alice = user_id!("@alice:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx, mut rx) = queue(8);
		channels.subscribe(1, alice, tx, 1, &[room], false);
		let events: Vec<PushedEvent<'_>> = (0..5).map(|n| PushedEvent { g_seq: 100 - n, json: b"{}" }).collect();

		channels.push_window(1, &events, 2);
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
	fn relay_skips_the_excepted_connection() {
		let channels = Channels::new();
		let alice = user_id!("@alice:localhost");
		let bob = user_id!("@bob:localhost");
		let room = room_id!("!r:localhost").to_owned();
		let (tx_a, mut rx_a) = queue(4);
		let (tx_b, mut rx_b) = queue(4);
		channels.subscribe(1, alice, tx_a, 1, &[room.clone()], false);
		channels.subscribe(2, bob, tx_b, 1, &[room.clone()], false);

		channels.relay(&room, Some(1), b"pack");
		assert!(rx_a.try_recv().is_err(), "the sender's connection got nothing");
		assert_eq!(take_pack(&mut rx_b), b"pack");

		channels.relay(&room, None, b"all");
		assert_eq!(take_pack(&mut rx_a), b"all");
		assert_eq!(take_pack(&mut rx_b), b"all");
	}
}
