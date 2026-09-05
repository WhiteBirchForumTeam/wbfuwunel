//! One-time numbering of the events a database already holds.
//!
//! Every stored PDU gets its per-room seq written into `unsigned` and every
//! room gets its counters, so a database from before this fork numbered its
//! rooms looks the same as one that always did. Forward events are numbered
//! 1, 2, 3… in count order; backfilled events 0, −1, −2… from the newest
//! backfilled downward, which is the order backfill itself hands them out.
//! See `docs/design/room-seq-and-recent.md` §1.3.

use std::collections::BTreeMap;

use futures::StreamExt;
use ruma::{CanonicalJsonObject, OwnedRoomId};
use tuwunel_core::{
	Result, info,
	matrix::pdu::{
		PduCount, RawPduId,
		seq::{SeqBounds, set_json_seq},
	},
	utils::stream::TryIgnore,
	warn,
};
use tuwunel_database::Json;

use crate::Services;

pub(super) async fn backfill_room_seq(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let pduid_pdu = db["pduid_pdu"].clone();

	warn!("Numbering every stored event with its per-room seq (one-time)");

	// Keys only: the whole timeline's ids fit in memory long before its
	// bodies would. Grouped by room, then ordered by count within the room.
	let mut rooms: BTreeMap<[u8; 8], Vec<RawPduId>> = BTreeMap::new();
	let keys = pduid_pdu.raw_keys().ignore_err();
	futures::pin_mut!(keys);
	while let Some(key) = keys.next().await {
		let pdu_id = RawPduId::from(&*key);
		rooms
			.entry(pdu_id.shortroomid())
			.or_default()
			.push(pdu_id);
	}

	let mut numbered: usize = 0;
	for (_, mut pdu_ids) in rooms {
		pdu_ids.sort_by_key(|pdu_id| pdu_id.pdu_count());

		let mut bounds = SeqBounds::default();
		let mut room_id: Option<OwnedRoomId> = None;

		// Backfilled counts sort below Normal ones; the newest backfilled (the
		// largest nonpositive count) must get 0, so walk them from the top.
		let first_normal = pdu_ids.partition_point(|pdu_id| matches!(pdu_id.pdu_count(), PduCount::Backfilled(_)));
		let (backfilled, forward) = pdu_ids.split_at(first_normal);
		let ordered = backfilled.iter().rev().chain(forward.iter());

		for pdu_id in ordered {
			let Ok(stored) = pduid_pdu.get(pdu_id).await else {
				continue;
			};
			let Ok(mut json) = serde_json::from_slice::<CanonicalJsonObject>(&stored) else {
				warn!(?pdu_id, "Skipping an event that is not a JSON object");
				continue;
			};

			let seq = match pdu_id.pdu_count() {
				| PduCount::Backfilled(_) => bounds.take_backfilled(),
				| PduCount::Normal(_) => bounds.take_forward(),
			};
			set_json_seq(&mut json, seq);
			pduid_pdu.raw_put(pdu_id, Json(&json));
			numbered = numbered.saturating_add(1);

			if room_id.is_none() {
				room_id = json
					.get("room_id")
					.and_then(|value| value.as_str())
					.and_then(|value| OwnedRoomId::try_from(value).ok());
			}
		}

		match room_id {
			| Some(room_id) => services
				.timeline
				.set_seq_bounds(&room_id, bounds)?,
			| None => warn!("A room's events carried no room_id; its counters start at zero"),
		}
	}

	drop(cork);
	info!(%numbered, "Numbered stored events with their per-room seq");

	db["global"].insert(b"backfill_room_seq", []);
	pduid_pdu.sort()
}
