use ruma::OwnedMxcUri;
use tuwunel_core::Result;
use tuwunel_database::COUNTER_SENTINEL;

use crate::admin_command;

#[admin_command]
pub(super) async fn refcount(&self, mxc: OwnedMxcUri) -> Result {
	let mxc = mxc.as_str();

	let report = match self.services.media_refs.refcount(mxc).await? {
		| None => format!(
			"{mxc} has no reference count.\n\nIt was created before counting existed and nothing \
			 has touched it since. It will not be collected until references are rebuilt."
		),
		| Some(COUNTER_SENTINEL) => format!(
			"{mxc} predates reference counting (sentinel).\n\nSomething referenced or released \
			 it after counting began, but its true count is unknown. It will not be collected \
			 until references are rebuilt."
		),
		| Some(count) => format!("{mxc} has {count} reference(s)."),
	};

	self.write_str(&report).await
}
