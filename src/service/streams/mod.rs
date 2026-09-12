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
	use tokio::sync::mpsc;

	use super::{Outgoing, Streams};

	#[test]
	fn a_dropped_guard_leaves_every_stream() {
		let streams = Arc::new(Streams::new());
		let alice = user_id!("@alice:localhost");
		let a = room_id!("!a:localhost").to_owned();
		let b = room_id!("!b:localhost").to_owned();
		let (tx, _rx) = mpsc::channel::<Outgoing>(4);
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
		let (tx, _rx) = mpsc::channel::<Outgoing>(4);
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
		let (tx, _rx) = mpsc::channel::<Outgoing>(4);
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
