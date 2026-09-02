use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{Stream, TryStreamExt};
use ruma::{CanonicalJsonObject, EventId, OwnedEventId};
use tuwunel_core::{
	Result, debug_info, expected, implement,
	matrix::pdu::PduEvent,
	utils::{TryReadyExt, time::now},
	warn,
};
use tuwunel_database::{Database, Deserialized, Json, Map};

use crate::rooms::timeline::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Arc<Database>,
	eventid_originalpdu: Arc<Map>,
	timeredacted_eventid: Arc<Map>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: args.db.clone(),
			eventid_originalpdu: args.db["eventid_originalpdu"].clone(),
			timeredacted_eventid: args.db["timeredacted_eventid"].clone(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		loop {
			let retention_seconds = self.services.config.redaction_retention_seconds;

			if retention_seconds != 0 {
				debug_info!("Cleaning up retained events");

				let now = now().as_secs();
				let expired: Vec<(u64, OwnedEventId)> = self
					.timeredacted_eventid
					.keys::<(u64, &EventId)>()
					.ready_try_take_while(|(time_redacted, _)| {
						let time_redacted = *time_redacted;
						Ok(expected!(time_redacted + retention_seconds) < now)
					})
					.map_ok(|(time_redacted, event_id)| (time_redacted, event_id.to_owned()))
					.try_collect()
					.await?;

				let count = expired.len();
				for (time_redacted, event_id) in expired {
					self.drop_original(&event_id, Some(time_redacted))
						.await;
				}

				debug_info!(?count, "Finished cleaning up retained events");
			}

			tokio::select! {
				() = tokio::time::sleep(Duration::from_hours(1)) => {},
				() = self.services.server.until_shutdown() => return Ok(())
			};
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn get_original_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
	self.eventid_originalpdu
		.get(event_id)
		.await?
		.deserialized()
}

#[implement(Service)]
pub async fn get_original_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
	self.eventid_originalpdu
		.get(event_id)
		.await?
		.deserialized()
}

/// Retains the unredacted original of `event_id` for the retention period.
///
/// Returns whether an original is retained afterwards, either by this call or
/// from before. `false` means nothing holds the original, and the caller must
/// treat the event's media references as released now rather than when the
/// original is dropped.
#[implement(Service)]
pub async fn save_original_pdu(
	&self,
	event_id: &EventId,
	pdu: &CanonicalJsonObject,
	_state_lock: &RoomMutexGuard,
) -> bool {
	if !self.services.config.save_unredacted_events {
		return false;
	}

	if self
		.eventid_originalpdu
		.exists(event_id)
		.await
		.is_ok()
	{
		return true;
	}

	let now = now().as_secs();

	self.eventid_originalpdu
		.raw_put(event_id, Json(pdu));

	self.timeredacted_eventid
		.put_raw((now, event_id), []);

	true
}

#[implement(Service)]
pub fn retained_pdus_raw(&self) -> impl Stream<Item = Result<&[u8]>> + Send {
	self.eventid_originalpdu
		.raw_stream()
		.map_ok(|x| x.1)
}

/// Drops the retained unredacted original of a purged event. The paired
/// `timeredacted_eventid` index entry is left for the retention worker to reap
/// at its scheduled time.
#[implement(Service)]
pub async fn purge_original(&self, event_id: &EventId) { self.drop_original(event_id, None).await; }

/// Drops a retained original and releases the media references it held, in
/// one batch. `time_redacted` also drops the retention index entry.
///
/// The original is the only remaining copy of the content that named the
/// media, so it is read for its references before it goes. An unreadable
/// original releases nothing: media held one count too long is recoverable,
/// media released one count too early is not.
#[implement(Service)]
async fn drop_original(&self, event_id: &EventId, time_redacted: Option<u64>) {
	let media_refs = match self.get_original_pdu_json(event_id).await {
		| Ok(original) => self.services.media_refs.list_event_mxc_uris(&original),
		| Err(e) if e.is_not_found() => Vec::new(),
		| Err(e) => {
			warn!(?event_id, ?e, "Retained original unreadable; its media references stay held.");
			Vec::new()
		},
	};

	let mut txn = self.db.txn();
	txn.del(&self.eventid_originalpdu, event_id);
	if let Some(time_redacted) = time_redacted {
		txn.del(&self.timeredacted_eventid, (time_redacted, event_id));
	}
	self.services
		.media_refs
		.del_event_refs(&mut txn, event_id, &media_refs);
	txn.execute();
}
