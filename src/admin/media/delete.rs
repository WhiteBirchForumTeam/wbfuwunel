use ruma::OwnedMxcUri;
use tuwunel_core::Result;
use tuwunel_service::media::TombstoneReason;

use crate::admin_command;

#[admin_command]
pub(super) async fn delete(&self, mxc: OwnedMxcUri) -> Result {
	self.services
		.media
		.collect(&mxc.as_str().try_into()?, TombstoneReason::AdminDeleted)
		.await?;

	self.write_str(
		"Deleted the MXC from our database and on our filesystem, and left a tombstone: \
		 fetches now answer 410 Gone.",
	)
	.await
}
