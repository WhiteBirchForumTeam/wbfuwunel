use ruma::{
	CanonicalJsonValue, EventId, OwnedRoomId, RoomId,
	canonical_json::{RedactedBecause, redact_in_place},
};
use tuwunel_core::{
	Result, err, implement,
	matrix::{
		event::Event,
		pdu::seq::{get_json_positions, set_json_positions},
	},
};

use crate::{
	media_refs::Holder,
	rooms::{short::ShortRoomId, timeline::RoomMutexGuard},
};

/// Replace a PDU with the redacted form.
#[implement(super::Service)]
#[tracing::instrument(name = "redact", level = "debug", skip(self))]
pub async fn redact_pdu<Pdu: Event + Send + Sync>(
	&self,
	event_id: &EventId,
	reason: &Pdu,
	shortroomid: ShortRoomId,
	state_lock: &RoomMutexGuard,
) -> Result {
	let Ok(pdu_id) = self.get_pdu_id(event_id).await else {
		// If event does not exist, just noop
		// TODO this is actually wrong!
		return Ok(());
	};

	let mut pdu = self
		.get_pdu_json_from_id(&pdu_id)
		.await
		.map_err(|e| {
			err!(Database(error!(?pdu_id, ?event_id, ?e, "PDU ID points to invalid PDU.")))
		})?;

	// While the original is retained, it is the reference holder: the media
	// stays exactly as long as an administrator can still see the message.
	let original_retained = self
		.services
		.retention
		.save_original_pdu(event_id, &pdu, state_lock)
		.await;

	let body = pdu["content"]
		.as_object()
		.and_then(|obj| obj.get("body"))
		.and_then(|body| body.as_str());

	if let Some(body) = body {
		self.services
			.search
			.deindex_pdu(shortroomid, &pdu_id, body);
	}

	let room_id: OwnedRoomId = pdu
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.and_then(|room| RoomId::parse(room).ok())
		.ok_or_else(|| err!(Database("stored PDU {event_id} has no room_id")))?;
	let room_id: &RoomId = &room_id;

	let room_version_id = self
		.services
		.state
		.get_room_version(room_id)
		.await?;

	let room_version_rules = room_version_id.rules().ok_or_else(|| {
		err!(Request(UnsupportedRoomVersion(
			"Cannot redact event for unknown room version {room_version_id:?}."
		)))
	})?;

	self.services
		.pdu_metadata
		.delete_typed_relation(&pdu_id, &pdu)
		.await;

	// Redaction strips `unsigned`; the event keeps its place, so its positions
	// go back afterwards. They also name the event's holder (its g_seq).
	let positions = get_json_positions(&pdu);

	redact_in_place(
		&mut pdu,
		&room_version_rules.redaction,
		Some(RedactedBecause::from_json(reason.to_canonical_object())),
	)
	.map_err(|err| err!("invalid event: {err}"))?;

	if let Some(positions) = positions {
		set_json_positions(&mut pdu, positions);
	}

	self.replace_pdu(&pdu_id, &pdu).await?;

	// Only once the stripped event is stored. With the original retained it
	// takes the media over (Event becomes Backup, never unheld in between);
	// without one, the event's holder goes and the collector decides. An
	// event from before positions existed holds nothing to move.
	if let Some(positions) = positions {
		let media_refs = &self.services.media_refs;
		let event_holder = Holder::event(room_id, positions.g_seq);
		let mut txn = self.db.db.txn();
		if original_retained {
			media_refs
				.swap_all_of(&mut txn, &event_holder, &Holder::backup(room_id, positions.g_seq))
				.await;
		} else {
			media_refs
				.release_all_of(&mut txn, &event_holder)
				.await;
		}
		txn.execute();
	}

	Ok(())
}
