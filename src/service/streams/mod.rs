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

mod rooms;
mod subscribers;

use std::sync::{
	Arc,
	atomic::{AtomicU64, Ordering},
};

pub use self::rooms::{EVENT_PUSH_SUBTYPE, PushedEvent, Subscribed};
use self::{rooms::RoomTopic, subscribers::Subscribers};

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
	next_connection: AtomicU64,
	/// The room channels (`0x14 Event`): a room may be listened to by as many
	/// of a user's connections as the user has open.
	rooms: Subscribers<RoomTopic>,
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
			next_connection: AtomicU64::new(1),
			rooms: Subscribers::new(),
		}
	}

	/// The next connection's number. Handed out at upgrade, before anything
	/// is subscribed.
	pub fn next_connection_id(&self) -> ConnectionId { self.next_connection.fetch_add(1, Ordering::Relaxed) }

	/// Args:
	///     connection: the number `next_connection_id` handed out
	/// Return:
	///     ConnectionGuard  drop it (the task ending, panicking, or being
	///     aborted) and the connection leaves every stream.
	#[must_use]
	pub fn connection_guard(self: &Arc<Self>, connection: ConnectionId) -> ConnectionGuard {
		ConnectionGuard { streams: self.clone(), connection }
	}

	/// Takes `connection` out of every stream and forgets it.
	pub fn remove_connection(&self, connection: ConnectionId) { self.rooms.remove_connection(connection); }

	/// Whether `connection` holds any subscription at all; for tests.
	#[must_use]
	pub fn is_subscribed(&self, connection: ConnectionId) -> bool { self.rooms.is_subscribed(connection) }
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use ruma::{room_id, user_id};
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
	fn connection_numbers_are_handed_out_once_each() {
		let streams = Streams::new();

		let first = streams.next_connection_id();
		let second = streams.next_connection_id();

		assert_ne!(first, second);
	}
}
