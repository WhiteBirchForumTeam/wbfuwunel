//! The per-room seq counters: `roomid_seqbounds`.
//!
//! The counters advance in the same transaction that stores the numbered
//! event, so a seq is never handed out without its event or the other way
//! round. Both callers (append and backfill) hold `mutex_insert` for the room
//! while they read the bounds and write the event, which is what makes the
//! read-then-write here safe without a merge operator.

use ruma::RoomId;
use tuwunel_core::{Result, err, implement, matrix::pdu::seq::SeqBounds, utils::result::NotFound};
use tuwunel_database::Txn;

/// Args:
///     room_id: example: !abc:localhost
/// Return:
///     Result<SeqBounds>  zeros for a room that has no counters yet (a new
///     room, or a database from before the migration numbered it); Err when
///     the row exists but cannot be read or decoded. That case must not fall
///     back to zeros: it would hand out r_seq 1 again in a room that already
///     has one, so the append fails instead.
#[implement(super::Service)]
pub async fn get_seq_bounds(&self, room_id: &RoomId) -> Result<SeqBounds> {
	let read = self.db.roomid_seqbounds.get(room_id).await;
	if read.is_not_found() {
		return Ok(SeqBounds::default());
	}

	let bytes = read.map_err(|error| err!(Database("reading seq bounds of {room_id}: {error}")))?;
	SeqBounds::decode(&bytes).map_err(|error| err!(Database("seq bounds of {room_id} are corrupt: {error}")))
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
