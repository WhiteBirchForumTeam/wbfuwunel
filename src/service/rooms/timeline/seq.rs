//! The per-room seq counters: `roomid_seqbounds`.
//!
//! The counters advance in the same transaction that stores the numbered
//! event, so a seq is never handed out without its event or the other way
//! round. Both callers (append and backfill) hold `mutex_insert` for the room
//! while they read the bounds and write the event, which is what makes the
//! read-then-write here safe without a merge operator.

use ruma::RoomId;
use tuwunel_core::{Result, implement, matrix::pdu::seq::SeqBounds, utils::result::LogErr};
use tuwunel_database::Txn;

/// Args:
///     room_id: example: !abc:localhost
/// Return:
///     SeqBounds  zeros for a room that has no counters yet (new room, or a
///     database from before the migration numbered it).
#[implement(super::Service)]
pub async fn get_seq_bounds(&self, room_id: &RoomId) -> SeqBounds {
	match self.db.roomid_seqbounds.get(room_id).await {
		| Ok(bytes) => SeqBounds::decode(&bytes)
			.log_err()
			.unwrap_or_default(),
		| Err(_) => SeqBounds::default(),
	}
}

/// Queues the counters into `txn`, alongside the event they number.
#[implement(super::Service)]
pub fn put_seq_bounds(&self, txn: &mut Txn, room_id: &RoomId, bounds: SeqBounds) {
	txn.put_raw(&self.db.roomid_seqbounds, room_id, bounds.encode());
}

/// Writes the counters outside any transaction; for the migration only, which
/// numbers a room's stored events one by one and records the total at the end.
#[implement(super::Service)]
pub fn set_seq_bounds(&self, room_id: &RoomId, bounds: SeqBounds) -> Result {
	self.db
		.roomid_seqbounds
		.insert(room_id, bounds.encode());

	Ok(())
}
