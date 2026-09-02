use ruma::OwnedMxcUri;
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn list_references(&self, mxc: OwnedMxcUri) -> Result {
	let mxc = mxc.as_str();

	let holders = self
		.services
		.media_refs
		.list_mxc_holders(mxc)
		.await;

	if holders.is_empty() {
		return self
			.write_str(&format!(
				"Nothing references {mxc}.\n\nThat is not on its own a reason to delete it: \
				 anything stored before the index existed was never recorded.",
			))
			.await;
	}

	let listed = holders
		.iter()
		.map(|holder| format!("- {holder}"))
		.collect::<Vec<_>>()
		.join("\n");

	self.write_str(&format!("{} reference(s) to {mxc}:\n\n{listed}", holders.len()))
		.await
}
