//! Streams: what a wbf WebSocket connection is subscribed to, and the push
//! of new things to it (`docs/design/wbf-event-push.md`,
//! `docs/design/wbf-to-device.md`).
//!
//! A connection carries **several subscriptions at once** — the room channels
//! today, its to-device queue next, more later — and the client is not
//! expected to open one connection per kind (to-device §3.3, maintainer's
//! call): sharing one connection is the normal shape. So the push bookkeeping
//! is per (connection, stream), and every kind runs on one shared core
//! (`subscribers.rs`) rather than its own copy of the same plumbing.
//!
//! What each kind keeps for itself is policy: who may enter a topic, when
//! they join or leave one, and what its packs look like. The room channels'
//! policy is here in `rooms.rs`.
//!
//! A subscriber is a connection (`ConnectionId`, issued at upgrade), never a
//! user and never a device: a user's every device must receive, and which of
//! a device's connections handles events is the client's business. The
//! connection's `Session` says who it is.
//!
//! Pushing never blocks the thing it is pushing about: a full send queue
//! drops that pack and marks the subscriber's next push with `gap`, so the
//! client knows to fill in (`Recent` for rooms). Nothing here is retried or
//! stored; a restart empties the registry and the safe direction is fewer
//! pushes.

mod devices;
mod rooms;
mod subscribers;

use std::sync::{
	Arc,
	atomic::{AtomicU64, Ordering},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};

pub use self::{
	devices::{DEVICE_PUSH_SUBTYPE, PushedItem},
	rooms::{EVENT_PUSH_SUBTYPE, PushedEvent, Subscribed},
};
use self::{
	devices::DeviceTopic,
	rooms::RoomTopic,
	subscribers::{Occupancy, Subscribers},
};

/// One WebSocket connection, numbered at upgrade; unique for the life of the
/// process, meaningless outside it.
pub type ConnectionId = u64;

/// Something queued for a connection's send task. Defined here rather than in
/// the API crate because publishing happens in the service layer.
pub enum Outgoing {
	Pack(Vec<u8>),
	/// The close frame that ends the connection after everything queued
	/// before it.
	Close { code: u16, reason: &'static str },
}

impl Outgoing {
	/// What holding this in the queue costs.
	///
	/// Return:
	///     usize  the pack's length; 0 for a close frame, which is a few
	///     bytes the connection must always be able to queue — refusing it
	///     for want of room is refusing to hang up.
	#[must_use]
	pub fn queued_bytes(&self) -> usize {
		match self {
			| Self::Pack(pack) => pack.len(),
			| Self::Close { .. } => 0,
		}
	}
}

/// One connection's send queue: a bounded channel, and the bytes the
/// connection may hold in it at once.
///
/// 🚨 **The count alone was the wrong bound.** A queue of 32 packs sounds
/// small until a pack is `wbf_data_max_bytes`, which **was** 16 MiB of a
/// media chunk — and then one connection held 514 MiB, and four devices of
/// four connections 9 GiB, without anything being wrong from the client's
/// side (ask for 32 large chunks, read the socket slowly). ⚠️ That default is
/// 2 MiB now, but the byte budget is what holds the line: the limit is
/// configurable and the count would be the wrong bound again the day
/// somebody raises it. The count stays
/// because a `Device/Fetch` window is counted in packs (`check_wbf_device_window`);
/// the byte budget is what decides the memory.
///
/// ⚠️ The room is booked before the pack joins the queue and given back when
/// the send task drops it, which is **after** it has been written — so the
/// number really is "how much this connection can be holding", not "how much
/// it may enqueue".
#[derive(Clone)]
pub struct PackQueue {
	packs: mpsc::Sender<Queued>,
	budget: Arc<Semaphore>,
	/// What the budget started at, so an oversized pack can be refused
	/// rather than awaited forever.
	capacity_bytes: usize,
}

/// A pack waiting in a send queue, holding the room it booked.
///
/// ⚠️ The permit travels with the pack instead of being released where it was
/// taken: the memory is occupied until the pack has been written, and the
/// send task is what knows when that is.
pub struct Queued {
	pub outgoing: Outgoing,
	_room: Option<OwnedSemaphorePermit>,
}

/// Why a pack did not join a send queue.
#[derive(Debug, Eq, PartialEq)]
pub enum QueueError {
	/// The connection's send task is gone.
	Gone,
	/// The queue is full — of packs, or of bytes. For a push this means the
	/// receiver is not keeping up and the pack is dropped (and a `gap`
	/// recorded); for a reply it is backpressure and the caller waits.
	Full,
	/// The pack alone is larger than the whole budget, so no amount of
	/// waiting would ever make room. 🚨 Refused rather than awaited: this is
	/// a configuration that cannot work, and blocking forever would look
	/// like a hung client.
	TooLargeForBudget,
}

impl PackQueue {
	/// Args:
	///     packs: how many packs may wait, example: 32
	///     bytes: how many bytes they may add up to, example: 33554432
	/// Return:
	///     (PackQueue, mpsc::Receiver<Queued>)  the sender half, cloned by
	///     everything that can queue for this connection, and the receiver
	///     the send task owns.
	#[must_use]
	pub fn new(packs: usize, bytes: usize) -> (Self, mpsc::Receiver<Queued>) {
		let (sender, receiver) = mpsc::channel(packs.max(1));
		let queue = Self {
			packs: sender,
			budget: Arc::new(Semaphore::new(bytes.max(1))),
			capacity_bytes: bytes.max(1),
		};

		(queue, receiver)
	}

	/// Queues a pack, waiting for room: the caller is a handler answering a
	/// request, and waiting here is the backpressure that stops the whole
	/// connection from running ahead of the socket.
	pub async fn send(&self, outgoing: Outgoing) -> Result<(), QueueError> {
		let room = self.book(outgoing.queued_bytes()).await?;
		self.packs
			.send(Queued { outgoing, _room: room })
			.await
			.map_err(|_| QueueError::Gone)
	}

	/// Queues a pack only if there is room right now: the caller is a push,
	/// and a push never blocks the thing that produced the event.
	pub fn try_send(&self, outgoing: Outgoing) -> Result<(), QueueError> {
		let room = self.book_now(outgoing.queued_bytes())?;
		self.packs
			.try_send(Queued { outgoing, _room: room })
			.map_err(|error| match error {
				| mpsc::error::TrySendError::Full(_) => QueueError::Full,
				| mpsc::error::TrySendError::Closed(_) => QueueError::Gone,
			})
	}

	/// Return:
	///     Result<Option<OwnedSemaphorePermit>, QueueError>  None when the
	///     item costs nothing (a close frame).
	async fn book(&self, bytes: usize) -> Result<Option<OwnedSemaphorePermit>, QueueError> {
		let Some(bytes) = self.bookable(bytes)? else {
			return Ok(None);
		};

		Arc::clone(&self.budget)
			.acquire_many_owned(bytes)
			.await
			.map(Some)
			.map_err(|_| QueueError::Gone)
	}

	fn book_now(&self, bytes: usize) -> Result<Option<OwnedSemaphorePermit>, QueueError> {
		let Some(bytes) = self.bookable(bytes)? else {
			return Ok(None);
		};

		match Arc::clone(&self.budget).try_acquire_many_owned(bytes) {
			| Ok(permit) => Ok(Some(permit)),
			| Err(TryAcquireError::NoPermits) => Err(QueueError::Full),
			| Err(TryAcquireError::Closed) => Err(QueueError::Gone),
		}
	}

	/// The permit count for `bytes`, or `None` when it costs nothing.
	fn bookable(&self, bytes: usize) -> Result<Option<u32>, QueueError> {
		if bytes == 0 {
			return Ok(None);
		}
		if bytes > self.capacity_bytes {
			return Err(QueueError::TooLargeForBudget);
		}

		u32::try_from(bytes)
			.map(Some)
			.map_err(|_| QueueError::TooLargeForBudget)
	}
}

/// Every stream's subscribers, and the connection numbers they are keyed by.
pub struct Streams {
	/// Tells this run of the server apart from the last one.
	///
	/// ⚠️ Connection numbers restart at 1 every time the process does, so a
	/// connection number alone names a different connection after a restart
	/// — and anything that outlives the process must not be keyed by it. The
	/// draft anchors were: their transaction ids were built from the
	/// connection number, transaction ids are stored in the database, and
	/// after a restart the first connection's first draft answered with the
	/// anchor from before the restart (external review 2026-09-12, R1).
	epoch: u64,
	next_connection: AtomicU64,
	/// The room channels (`0x14 Event`): a room may be listened to by as many
	/// of a user's connections as the user has open.
	rooms: Subscribers<RoomTopic>,
	/// The to-device queues (`0x16 Device`), at most one connection each.
	devices: Subscribers<DeviceTopic>,
}

/// Leaves every stream when the connection's task ends, whichever way it
/// ends. The receive loop holds one; nothing else has to remember.
pub struct ConnectionGuard {
	streams: Arc<Streams>,
	connection: ConnectionId,
}

impl Drop for ConnectionGuard {
	fn drop(&mut self) { self.streams.remove_connection(self.connection); }
}

impl Default for Streams {
	fn default() -> Self { Self::new() }
}

impl Streams {
	#[must_use]
	pub fn new() -> Self {
		Self {
			epoch: rand::random(),
			next_connection: AtomicU64::new(1),
			rooms: Subscribers::new(Occupancy::Many),
			// ⚠️ The one-connection rule of the to-device queue is declared
			// here, once, rather than checked wherever a `Subscribe`
			// arrives: the registry can enforce it without a gap between
			// looking and entering, and a call site cannot.
			devices: Subscribers::new(Occupancy::OneTheLatest),
		}
	}

	/// The next connection's number. Handed out at upgrade, before anything
	/// is subscribed.
	pub fn next_connection_id(&self) -> ConnectionId { self.next_connection.fetch_add(1, Ordering::Relaxed) }

	/// A name for one connection that no other connection of any run of this
	/// server shares, for the things that are stored and outlive the process.
	///
	/// Args:
	///     connection: the number `next_connection_id` handed out, example: 7
	/// Return:
	///     String  example: "9f3c1ab0d4e27615-7"
	#[must_use]
	pub fn connection_tag(&self, connection: ConnectionId) -> String {
		format!("{:016x}-{connection}", self.epoch)
	}

	/// Args:
	///     connection: the number `next_connection_id` handed out
	/// Return:
	///     ConnectionGuard  drop it (the task ending, panicking, or being
	///     aborted) and the connection leaves every stream.
	#[must_use]
	pub fn connection_guard(self: &Arc<Self>, connection: ConnectionId) -> ConnectionGuard {
		ConnectionGuard { streams: self.clone(), connection }
	}

	/// Takes `connection` out of **every** stream and forgets it. One place,
	/// so a new stream cannot be added without its cleanup: the guard calls
	/// only this.
	pub fn remove_connection(&self, connection: ConnectionId) {
		self.rooms.remove_connection(connection);
		self.devices.remove_connection(connection);
	}

	/// Whether `connection` holds any subscription at all; for tests.
	#[must_use]
	pub fn is_subscribed(&self, connection: ConnectionId) -> bool { self.rooms.is_subscribed(connection) }
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use ruma::{device_id, room_id, user_id};

	use super::{Outgoing, PackQueue, QueueError, Streams};

	/// 🚨 The bound the count never was. Four packs of a megabyte each fit
	/// the count (four) and not the budget (2 MiB), and before this the
	/// queue would have held all four — which is how 32 packs of 16 MiB
	/// became half a gigabyte of one connection's memory.
	#[test]
	fn the_queue_runs_out_of_bytes_before_it_runs_out_of_packs() {
		let (queue, mut packs) = PackQueue::new(4, 2 * 1024 * 1024);
		let megabyte = || Outgoing::Pack(vec![0_u8; 1024 * 1024]);

		assert_eq!(queue.try_send(megabyte()), Ok(()));
		assert_eq!(queue.try_send(megabyte()), Ok(()));
		assert_eq!(
			queue.try_send(megabyte()),
			Err(QueueError::Full),
			"two megabytes is the whole budget, and the count still had room for two more"
		);

		// Room comes back when a pack is taken **and dropped**, not when it
		// is taken: what holds the memory is the pack itself.
		let taken = packs.try_recv().expect("a pack was queued");
		assert_eq!(queue.try_send(megabyte()), Err(QueueError::Full));
		drop(taken);
		assert_eq!(queue.try_send(megabyte()), Ok(()));
	}

	/// A close frame costs nothing, because refusing to queue one is
	/// refusing to hang up on a connection that is already misbehaving.
	#[test]
	fn a_close_frame_fits_in_a_queue_with_no_room_left() {
		let (queue, _packs) = PackQueue::new(4, 1024);
		assert_eq!(queue.try_send(Outgoing::Pack(vec![0_u8; 1024])), Ok(()));

		assert_eq!(queue.try_send(Outgoing::Pack(vec![0_u8; 1])), Err(QueueError::Full));
		assert_eq!(
			queue.try_send(Outgoing::Close { code: 1000, reason: "bye" }),
			Ok(())
		);
	}

	/// 🚨 Refused, not awaited: waiting for room that can never exist is a
	/// connection that hangs on an ordinary-looking request. The startup
	/// check (`check_wbf_send_queue_bytes`) is what keeps a server from
	/// being configured this way at all.
	#[test]
	fn a_pack_larger_than_the_whole_budget_is_refused_rather_than_queued() {
		let (queue, _packs) = PackQueue::new(4, 1024);

		assert_eq!(
			queue.try_send(Outgoing::Pack(vec![0_u8; 1025])),
			Err(QueueError::TooLargeForBudget)
		);
	}

	#[test]
	fn a_dropped_guard_leaves_every_stream() {
		let streams = Arc::new(Streams::new());
		let alice = user_id!("@alice:localhost");
		let a = room_id!("!a:localhost").to_owned();
		let b = room_id!("!b:localhost").to_owned();
		let (tx, _rx) = PackQueue::new(4, 1024 * 1024);
		let connection = streams.next_connection_id();
		let guard = streams.connection_guard(connection);
		streams.subscribe(connection, alice, tx, 1, &[a.clone(), b.clone()], true);
		assert!(streams.is_listened(&a) && streams.is_listened(&b));

		drop(guard);

		assert!(!streams.is_listened(&a) && !streams.is_listened(&b));
		assert!(!streams.is_subscribed(connection));
		assert!(streams.listeners(&a).is_empty());
	}

	#[test]
	fn a_connection_that_becomes_somebody_else_keeps_no_stream_of_the_old_identity() {
		// A `Login` on a live connection: what the old identity subscribed to
		// must be gone from **every** stream before the new one is served.
		// Leaving the device queue behind pushed one user's to-device items
		// into a queue that now belongs to another (PR #43 review), and the
		// rooms-only unsubscribe is not enough here — which is why the call
		// site uses this one entry point.
		let streams = Arc::new(Streams::new());
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let room = room_id!("!a:localhost").to_owned();
		let (tx, _rx) = PackQueue::new(4, 1024 * 1024);
		let connection = streams.next_connection_id();
		streams.subscribe(connection, alice, tx.clone(), 1, &[room.clone()], true);
		streams.subscribe_device(connection, alice, phone, tx, 2);
		assert_eq!(streams.device_holder(alice, phone), Some(connection));

		streams.remove_connection(connection);

		assert_eq!(streams.device_holder(alice, phone), None, "the old device queue is let go");
		assert!(!streams.is_listened(&room), "and so are the old rooms");
	}

	#[test]
	fn leaving_the_room_channels_is_not_leaving_the_device_queue() {
		// ⚠️ The two are different requests, which is why they are different
		// methods: `Event/Unsubscribe` with no rooms named means "stop
		// listening to rooms", and a client that sends it still wants its
		// keys. The identity swap wants the other one — and the leak it
		// caused was a call site reaching for a name that said "all" and
		// meant "rooms".
		let streams = Arc::new(Streams::new());
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let room = room_id!("!a:localhost").to_owned();
		let (tx, _rx) = PackQueue::new(4, 1024 * 1024);
		let connection = streams.next_connection_id();
		streams.subscribe(connection, alice, tx.clone(), 1, &[room.clone()], true);
		streams.subscribe_device(connection, alice, phone, tx, 2);

		streams.unsubscribe_all_rooms(connection);

		assert!(!streams.is_listened(&room), "the rooms are left");
		assert_eq!(
			streams.device_holder(alice, phone),
			Some(connection),
			"and the device queue is not"
		);
	}

	#[test]
	fn connection_numbers_are_handed_out_once_each() {
		let streams = Streams::new();

		let first = streams.next_connection_id();
		let second = streams.next_connection_id();

		assert_ne!(first, second);
	}
}
