//! The collector: acts on one released media at a time.

use ruma::Mxc;
use tuwunel_core::{debug, error, implement, info};
use tuwunel_database::COUNTER_SENTINEL;

use super::Service;
use crate::media::TombstoneReason;

/// Removes `mxc` if nothing references it any more.
///
/// The rule is `MIN < count <= 0`: an unknown count (no row, or the sentinel)
/// holds the media, a positive count holds it, anything else releases it.
/// Remote media is left to its cache expiry. With collection disabled the
/// decision is logged and nothing is removed.
#[implement(Service)]
pub(super) async fn collect(&self, mxc: &str) {
	if self.is_collector_paused() {
		debug!(?mxc, "Collector paused for a rebuild; the rebuild decides.");
		return;
	}

	let count = match self.refcount(mxc).await {
		| Ok(Some(count)) => count,
		| Ok(None) => return,
		| Err(e) => {
			error!(?mxc, ?e, "Media reference count unreadable; media stays.");
			return;
		},
	};

	if count == COUNTER_SENTINEL || count > 0 {
		return;
	}

	if count < 0 {
		error!(
			?mxc,
			count,
			"Media reference count is negative: some caller released more than it counted. \
			 Deleting under the rule regardless; find that caller."
		);
	}

	let Ok(parsed) = Mxc::try_from(mxc) else {
		error!(?mxc, "Released media has an unparseable MXC; media stays.");
		return;
	};

	if !self.services.media.is_local(&parsed) {
		return;
	}

	if !self.services.config.media_gc_enabled {
		info!(?mxc, count, "Media garbage collection is disabled; would have deleted.");
		return;
	}

	match self
		.services
		.media
		.collect(&parsed, TombstoneReason::GarbageCollected)
		.await
	{
		| Ok(()) => info!(?mxc, "Collected media nothing references any more."),
		| Err(e) => error!(?mxc, ?e, "Failed to collect media; it stays until a rebuild."),
	}
}
