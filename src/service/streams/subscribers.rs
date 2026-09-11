//! The part of a WebSocket subscription that every kind shares: who is
//! listening to what, each subscription's own `id`/`seq`/`gap`, and a push
//! that never blocks the thing it is pushing about.
//!
//! One connection can hold several subscriptions at once — the room channels
//! (`0x14 Event`) and its own to-device queue (`0x16 Device`) today, more
//! later — so the push bookkeeping is per **(connection, stream)**, not per
//! connection: each subscription copies its own client-chosen `id` into its
//! packs and counts its own `seq`. Sharing one counter between two streams
//! would make each one's packs look like gaps in the other's.
//!
//! What is *not* here is policy: which topics a subscriber may enter, when it
//! follows or leaves one, and what a pack's meta looks like. Those differ per
//! kind and live with that kind (`rooms.rs`, and the device queue next to
//! it); this module only knows connections, topics, and queues.

use std::{
	collections::{HashMap, HashSet},
	hash::Hash,
	sync::{
		RwLock,
		atomic::{AtomicBool, AtomicU32, Ordering},
	},
};

use ruma::{OwnedUserId, UserId};
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tuwunel_core::{debug, wbf::PackError};

use super::{ConnectionId, Outgoing};

/// One connection's subscription to one stream.
struct Subscriber {
	user: OwnedUserId,
	queue: Sender<Outgoing>,
	/// The client's `Subscribe` id, copied into every push.
	id: u64,
	/// The next push's `seq`.
	seq: AtomicU32,
	/// A push was dropped since the last one that went out.
	gap: AtomicBool,
}

/// What one push to one subscriber needs, taken under the read lock.
pub(super) struct Target {
	pub(super) connection: ConnectionId,
	pub(super) queue: Sender<Outgoing>,
	pub(super) id: u64,
	pub(super) seq: u32,
	pub(super) gap: bool,
}

/// The subscribers of one stream, indexed by topic.
///
/// `Topic` is whatever that stream subscribes by — a room for the event
/// channels, a device for the to-device queue.
pub(super) struct Subscribers<Topic> {
	registry: RwLock<Registry<Topic>>,
}

struct Registry<Topic> {
	/// Topic to the connections listening to it.
	topics: HashMap<Topic, HashSet<ConnectionId>>,
	subscribers: HashMap<ConnectionId, Subscriber>,
	/// A user's subscribed connections, for hooks that work by user.
	by_user: HashMap<OwnedUserId, HashSet<ConnectionId>>,
}

impl<Topic> Default for Subscribers<Topic>
where
	Topic: Eq + Hash,
{
	fn default() -> Self { Self::new() }
}

impl<Topic> Subscribers<Topic>
where
	Topic: Clone + Eq + Hash,
{
	pub(super) fn new() -> Self {
		Self {
			registry: RwLock::new(Registry {
				topics: HashMap::new(),
				subscribers: HashMap::new(),
				by_user: HashMap::new(),
			}),
		}
	}

	/// Registers `connection` (or refreshes it) and puts it into `topics`.
	///
	/// The caller has already decided this connection may enter them.
	///
	/// Args:
	///     connection: example: 7
	///     user: who the connection is, from its `Session`
	///     queue: the connection's send queue
	///     id: the client's `Subscribe` id, example: 42
	///     topics: what to enter, example: every joined room
	/// Return:
	///     usize  how many topics were newly entered (ones it was already in
	///     do not count).
	pub(super) fn subscribe(
		&self,
		connection: ConnectionId,
		user: &UserId,
		queue: Sender<Outgoing>,
		id: u64,
		topics: &[Topic],
	) -> usize {
		let mut registry = self.registry.write().expect("stream lock poisoned");

		// A connection subscribing as somebody else (a `Login` that kept the
		// connection) starts over: the hooks find subscribers through
		// `by_user`, so an identity left behind there would push the old
		// user's topics to the new one.
		let is_another_identity = registry
			.subscribers
			.get(&connection)
			.is_some_and(|existing| existing.user != *user);
		if is_another_identity {
			registry.remove_connection(connection);
		}

		match registry.subscribers.get_mut(&connection) {
			| Some(existing) => {
				if existing.id != id {
					// A new subscription id starts its own push sequence.
					existing.id = id;
					existing.seq.store(0, Ordering::Relaxed);
					existing.gap.store(false, Ordering::Relaxed);
				}
				existing.queue = queue;
			},
			| None => {
				registry.subscribers.insert(connection, Subscriber {
					user: user.to_owned(),
					queue,
					id,
					seq: AtomicU32::new(0),
					gap: AtomicBool::new(false),
				});
			},
		}
		registry
			.by_user
			.entry(user.to_owned())
			.or_default()
			.insert(connection);

		let mut entered = 0;
		for topic in topics {
			if registry
				.topics
				.entry(topic.clone())
				.or_default()
				.insert(connection)
			{
				entered = entered.saturating_add(1);
			}
		}

		entered
	}

	/// Takes `connection` out of `topics`; ones it is not in are no-ops. The
	/// subscriber stays registered — in no topic it simply receives nothing;
	/// only `remove_connection` forgets it.
	pub(super) fn unsubscribe(&self, connection: ConnectionId, topics: &[Topic]) {
		let mut registry = self.registry.write().expect("stream lock poisoned");
		for topic in topics {
			registry.leave_topic(connection, topic);
		}
	}

	/// Takes `connection` out of every topic and forgets it. Idempotent, and
	/// what a connection's guard calls for each stream when it ends.
	pub(super) fn remove_connection(&self, connection: ConnectionId) {
		let mut registry = self.registry.write().expect("stream lock poisoned");
		registry.remove_connection(connection);
	}

	/// Whether anyone listens to `topic`: the cheap check before anything is
	/// serialized for pushing.
	pub(super) fn is_listened(&self, topic: &Topic) -> bool {
		self.registry
			.read()
			.expect("stream lock poisoned")
			.topics
			.contains_key(topic)
	}

	/// Who is listening to `topic`, for the caller to filter before pushing.
	///
	/// Return:
	///     Vec<(ConnectionId, OwnedUserId)>  empty when nobody is.
	pub(super) fn listeners(&self, topic: &Topic) -> Vec<(ConnectionId, OwnedUserId)> {
		let registry = self.registry.read().expect("stream lock poisoned");
		let Some(listeners) = registry.topics.get(topic) else {
			return Vec::new();
		};
		listeners
			.iter()
			.filter_map(|connection| {
				registry
					.subscribers
					.get(connection)
					.map(|subscriber| (*connection, subscriber.user.clone()))
			})
			.collect()
	}

	pub(super) fn listener_count(&self, topic: &Topic) -> usize {
		self.registry
			.read()
			.expect("stream lock poisoned")
			.topics
			.get(topic)
			.map_or(0, HashSet::len)
	}

	/// Whether `connection` is registered at all; for tests and for the
	/// device queue's one-connection rule.
	pub(super) fn is_subscribed(&self, connection: ConnectionId) -> bool {
		self.registry
			.read()
			.expect("stream lock poisoned")
			.subscribers
			.contains_key(&connection)
	}

	/// The user's subscribed connections, for hooks that work by user (a
	/// room's join and leave).
	pub(super) fn connections_of(&self, user: &UserId) -> Vec<ConnectionId> {
		self.registry
			.read()
			.expect("stream lock poisoned")
			.by_user
			.get(user)
			.map(|connections| connections.iter().copied().collect())
			.unwrap_or_default()
	}

	/// Puts `connections` into `topic` without asking anything: the caller
	/// (a join hook) has decided they belong there.
	pub(super) fn enter_topic(&self, topic: &Topic, connections: &[ConnectionId]) {
		if connections.is_empty() {
			return;
		}
		let mut registry = self.registry.write().expect("stream lock poisoned");
		let listeners = registry.topics.entry(topic.clone()).or_default();
		for connection in connections {
			listeners.insert(*connection);
		}
	}

	/// Takes `connections` out of `topic` (a leave hook).
	pub(super) fn leave_topic(&self, topic: &Topic, connections: &[ConnectionId]) {
		let mut registry = self.registry.write().expect("stream lock poisoned");
		for connection in connections {
			registry.leave_topic(*connection, topic);
		}
	}

	/// Pushes to each of `connections`, never waiting: a full queue drops
	/// that pack and marks the subscriber's next push with `gap`.
	///
	/// The pack is the caller's: this module owns who gets pushed, which
	/// `id` and `seq` the pack carries and whether a gap is outstanding, and
	/// `make_pack` turns those three into the bytes for that kind (its meta
	/// shape is that kind's business).
	///
	/// Args:
	///     topic: `Some` to require current membership of it, read again
	///         under the lock — a leave that landed while the caller was
	///         awaiting counts. `None` for a push that is not one topic's.
	///     connections: the candidates
	///     make_pack: (id, seq, gap) -> the pack for that subscriber
	pub(super) fn push_with<F>(&self, topic: Option<&Topic>, connections: &[ConnectionId], make_pack: F)
	where
		F: Fn(u64, u32, bool) -> Result<Vec<u8>, PackError>,
	{
		let targets = self.take_targets(topic, connections);
		let mut dropped = Vec::new();
		for target in targets {
			let pack = match make_pack(target.id, target.seq, target.gap) {
				| Ok(pack) => pack,
				| Err(error) => {
					debug!(?error, "wbf push skipped: could not encode the pack");
					dropped.push(target.connection);
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

	/// Queues a pack the caller built for one connection, outside any
	/// subscription bookkeeping: what a stream sends on its own initiative
	/// (a relayed draft piece, a result the client is waiting for). Same
	/// never-block rule; no `gap` is recorded, because nothing here is part
	/// of a numbered sequence.
	///
	/// Return:
	///     bool  true when it was queued.
	pub(super) fn send_pack(&self, connection: ConnectionId, pack: Vec<u8>) -> bool {
		let queue = {
			let registry = self.registry.read().expect("stream lock poisoned");
			registry
				.subscribers
				.get(&connection)
				.map(|subscriber| subscriber.queue.clone())
		};

		queue.is_some_and(|queue| queue.try_send(Outgoing::Pack(pack)).is_ok())
	}

	/// Marks each connection's next push with `gap`: something it should
	/// have received did not go out.
	pub(super) fn mark_gap(&self, connections: &[ConnectionId]) {
		if connections.is_empty() {
			return;
		}
		let registry = self.registry.read().expect("stream lock poisoned");
		for connection in connections {
			if let Some(subscriber) = registry.subscribers.get(connection) {
				subscriber.gap.store(true, Ordering::Relaxed);
			}
		}
	}

	/// Snapshots what each connection's next push needs, advancing its `seq`
	/// and clearing its `gap`, under the read lock (the counters are atomic).
	///
	/// Args:
	///     topic: `Some` to also require current membership of it
	///     connections: the candidates
	/// Return:
	///     Vec<Target>  one per connection still subscribed (and still in
	///     `topic`, when given); anyone else is skipped, not queued.
	fn take_targets(&self, topic: Option<&Topic>, connections: &[ConnectionId]) -> Vec<Target> {
		let registry = self.registry.read().expect("stream lock poisoned");
		let is_still_in_topic = |connection: &ConnectionId| {
			topic.is_none_or(|topic| {
				registry
					.topics
					.get(topic)
					.is_some_and(|listeners| listeners.contains(connection))
			})
		};
		connections
			.iter()
			.filter(|connection| is_still_in_topic(connection))
			.filter_map(|connection| {
				let subscriber = registry.subscribers.get(connection)?;
				Some(Target {
					connection: *connection,
					queue: subscriber.queue.clone(),
					id: subscriber.id,
					seq: subscriber.seq.fetch_add(1, Ordering::Relaxed),
					gap: subscriber.gap.swap(false, Ordering::Relaxed),
				})
			})
			.collect()
	}
}

impl<Topic> Registry<Topic>
where
	Topic: Clone + Eq + Hash,
{
	/// Removes one connection from one topic, dropping the topic when it
	/// holds nobody.
	fn leave_topic(&mut self, connection: ConnectionId, topic: &Topic) {
		let Some(listeners) = self.topics.get_mut(topic) else {
			return;
		};
		listeners.remove(&connection);
		if listeners.is_empty() {
			self.topics.remove(topic);
		}
	}

	fn remove_connection(&mut self, connection: ConnectionId) {
		let Some(subscriber) = self.subscribers.remove(&connection) else {
			return;
		};
		if let Some(connections) = self.by_user.get_mut(&subscriber.user) {
			connections.remove(&connection);
			if connections.is_empty() {
				self.by_user.remove(&subscriber.user);
			}
		}
		// Topics are not indexed by connection; walk them. A connection is in
		// as many topics as its user has rooms, and this runs once per
		// disconnect.
		self.topics.retain(|_, listeners| {
			listeners.remove(&connection);
			!listeners.is_empty()
		});
	}
}
