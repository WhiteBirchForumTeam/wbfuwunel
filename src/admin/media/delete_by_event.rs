use ruma::{CanonicalJsonValue, Mxc, OwnedEventId};
use tuwunel_core::{Err, Result, err, info, matrix::list_content_mxc_uris, warn};

use crate::admin_command;

#[admin_command]
pub(super) async fn delete_by_event(&self, event_id: OwnedEventId) -> Result {
	let event_json = self
		.services
		.timeline
		.get_pdu_json(&event_id)
		.await
		.map_err(|_| err!("Event ID does not exist or is not known to us."))?;

	let content = event_json
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.ok_or_else(|| {
			err!(
				"Event ID does not have a \"content\" key, this is not a message or an event \
				 type that contains media.",
			)
		})?;

	let mxc_urls = list_content_mxc_uris(content);

	if mxc_urls.is_empty() {
		return Err!("Parsed event ID but found no MXC URLs.",);
	}

	let mut mxc_deletion_count: usize = 0;

	for mxc_url in mxc_urls {
		let mxc: Mxc<'_> = mxc_url.as_str().try_into()?;

		match self.services.media.delete(&mxc).await {
			| Ok(()) => {
				info!("Successfully deleted {mxc_url} from filesystem and database");
				mxc_deletion_count = mxc_deletion_count.saturating_add(1);
			},
			| Err(e) => {
				warn!("Failed to delete {mxc_url}, ignoring error and skipping: {e}");
			},
		}
	}

	write!(
		self,
		"Deleted {mxc_deletion_count} total MXCs from our database and the filesystem from \
		 event ID {event_id}."
	)
	.await
}