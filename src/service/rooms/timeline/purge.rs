use futures::TryStreamExt;
use ruma::{RoomId, api::Direction, events::TimelineEventType};
use tuwunel_core::{
	Result, implement,
	matrix::{
		Event,
		pdu::{PduCount, PduEvent},
	},
	trace,
	utils::stream::TryReadyExt,
};

use super::{ExtractBody, RawPduId, bias_count};
use crate::media_refs::Holder;

/// Selectively purges room history strictly before `until` in stream order,
/// returning the number of events removed. State events are always preserved,
/// and locally-sent events are kept unless `delete_local_events`. Forward
/// extremities are never touched, so the room stays live. tuwunel orders by
/// per-room stream position rather than topological depth, so "same-depth
/// events retained" becomes "strictly earlier in stream order".
#[implement(super::Service)]
pub async fn purge_history(
	&self,
	room_id: &RoomId,
	until: PduCount,
	delete_local_events: bool,
) -> Result<usize> {
	let shortroomid = self
		.services
		.short
		.get_shortroomid(room_id)
		.await?;

	let start = self
		.count_to_id(room_id, PduCount::min(), Direction::Forward)
		.await?;

	let prefix = start.shortroomid();

	// 🚨 **No batch release of the range.** It used to release every Event and
	// Backup holder below `until` in one go, "without reading a single event" —
	// and that was exactly why it could not be right: the loop below *keeps*
	// state events, and keeps locally-sent ones unless `delete_local_events`, so
	// the released set was a superset of the deleted set and a kept, live event
	// lost its media. The room avatar was the plainest case: `m.room.avatar` is
	// a state event, `url` is a media reference, and purging history deleted the
	// picture the room still points at
	// (/docs/design/media/purge-release-set.md, external review 2026-09-29 #3).
	//
	// ⭐ So a holder is released by the one place that knows the event is going:
	// the deleting transaction itself, below. The two sets are then equal by
	// construction rather than by two separate pieces of code agreeing.
	// 📎 It also fixes a second case nobody had noticed: backfilled holders have
	// a negative `g_seq`, which is below *any* positive `until`, so every purge
	// released the whole backfilled history's holders regardless of the
	// boundary.

	self.db
		.pduid_pdu
		.raw_stream_from(&start)
		.ready_try_take_while(move |kv| {
			let (key, _) = *kv;
			Ok(key.starts_with(&prefix) && RawPduId::from(key).pdu_count() < until)
		})
		.try_fold(0_usize, async |purged, (key, value)| {
			let pdu = serde_json::from_slice::<PduEvent>(value)?;

			if pdu.state_key.is_some()
				|| (!delete_local_events && self.services.globals.user_is_local(&pdu.sender))
			{
				return Ok(purged);
			}

			let mut txn = self.db.db.txn();

			let raw_id = RawPduId::from(key);
			let count = raw_id.pdu_count();
			let event_id = pdu.event_id.clone();
			let ts: u64 = pdu.origin_server_ts.into();

			txn.del_raw(&self.db.pduid_pdu, key);
			txn.del_raw(&self.db.eventid_pduid, &event_id);
			txn.del_raw(&self.db.eventid_outlierpdu, &event_id);

			let room_id_ts_id = (room_id, ts, bias_count(raw_id.count()));
			txn.del(&self.db.roomid_tscount_pducount, room_id_ts_id);

			// This event is going, so its media holder goes with it — in the same
			// transaction, so there is no moment where the row is gone and the
			// holder is not (or the other way round).
			let released = self
				.services
				.media_refs
				.release_all_of(&mut txn, &Holder::event(room_id, count.into_signed()))
				.await;

			txn.execute();

			if !released.is_empty() {
				trace!(?event_id, ?room_id, ?released, "Released the media of a purged event");
			}

			if pdu.kind == TimelineEventType::RoomMessage
				&& let Ok(ExtractBody { body: Some(body) }) = pdu.get_content()
			{
				self.services
					.search
					.deindex_pdu(shortroomid, &raw_id, &body);
			}

			self.services
				.pdu_metadata
				.purge_event_relations(shortroomid, count, room_id, &event_id)
				.await;

			// Dropping the retained original removes its `Backup` holder too, and
			// ⚠️ since the range is no longer released in one batch this is where
			// that actually happens — it used to be a no-op for media.
			self.services
				.retention
				.purge_original(&event_id)
				.await;

			trace!(?event_id, ?room_id, "Purged");

			Ok(purged.saturating_add(1))
		})
		.await
}
