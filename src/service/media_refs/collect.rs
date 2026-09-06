//! The collector, acting on one released media at a time, and the sweep,
//! acting on managed media nothing ever held. One decision for both.

use std::time::Duration;

use futures::StreamExt;
use ruma::Mxc;
use tokio::time::{Interval, MissedTickBehavior, interval};
use tuwunel_core::{debug, error, implement, info, utils::{stream::TryIgnore, time::now_millis}};

use super::Service;
use crate::media::TombstoneReason;

/// Removes `mxc` if nothing holds it any more. Called for every media a
/// holder was removed from, once that removal committed.
#[implement(Service)]
pub(super) async fn collect(&self, mxc: &str) {
	// Held from the decision through the removal, so a holder being added
	// right now either lands before the look or waits until after the
	// removal.
	let _held = self.hold_media(mxc).await;

	if !self.is_removable(mxc).await {
		return;
	}

	self.remove(mxc, TombstoneReason::GarbageCollected).await;
}

/// The one decision: local, managed by this model, and held by nothing.
/// Unknown (not managed) is "no", never "yes".
#[implement(Service)]
pub(super) async fn is_removable(&self, mxc: &str) -> bool {
	let Ok(parsed) = Mxc::try_from(mxc) else {
		return false;
	};
	if !self.services.media.is_local(&parsed) {
		return false;
	}
	if self.managed_since(mxc).await.is_none() {
		return false;
	}

	!self.has_holders(mxc).await
}

/// The sweep's clock: `media_gc_sweep_interval`, first tick after one whole
/// interval (not at startup), late ticks not made up.
#[implement(Service)]
pub(super) fn sweep_interval(&self) -> Interval {
	let period = Duration::from_secs(self.services.config.media_gc_sweep_interval.max(1));
	let mut clock = interval(period);
	clock.set_missed_tick_behavior(MissedTickBehavior::Delay);
	clock.reset();
	clock
}

/// Removes managed media nothing holds once it is older than the protection
/// period: uploaded and never attached to anything the server was told
/// about. Walks `mxc_managed` only, so media from before the model is never
/// looked at.
#[implement(Service)]
pub(super) async fn sweep_unreferenced(&self) {
	let grace_millis = self
		.services
		.config
		.media_unreferenced_grace_seconds_effective()
		.saturating_mul(1000);
	let protected_since = now_millis().saturating_sub(grace_millis);

	let candidates: Vec<String> = self
		.db
		.mxc_managed
		.raw_stream()
		.ignore_err()
		.filter_map(|(key, value)| {
			let created = <[u8; 8]>::try_from(value).map(u64::from_be_bytes).unwrap_or(u64::MAX);
			let mxc = (created < protected_since).then(|| String::from_utf8_lossy(key).into_owned());
			async move { mxc }
		})
		.collect()
		.await;

	let mut removed: usize = 0;
	for mxc in &candidates {
		let _held = self.hold_media(mxc).await;
		if !self.is_removable(mxc).await {
			continue;
		}
		if self.remove(mxc, TombstoneReason::Unreferenced).await {
			removed = removed.saturating_add(1);
		}
	}

	if !candidates.is_empty() {
		info!(candidates = candidates.len(), removed, "Unreferenced media sweep finished.");
	}
}

/// Removes `mxc` with `reason`, honouring `media_gc_enabled`. Returns
/// whether bytes were removed.
#[implement(Service)]
async fn remove(&self, mxc: &str, reason: TombstoneReason) -> bool {
	let Ok(parsed) = Mxc::try_from(mxc) else {
		error!(?mxc, "Media has an unparseable MXC; media stays.");
		return false;
	};

	if !self.services.config.media_gc_enabled {
		info!(?mxc, ?reason, "Media garbage collection is disabled; would have deleted.");
		return false;
	}

	match self.services.media.collect(&parsed, reason).await {
		| Ok(()) => {
			// `media.collect` also dropped the rows that pointed at it
			// (`forget_media`), as it does for an admin removal.
			debug!(?mxc, ?reason, "Removed media nothing holds.");
			true
		},
		| Err(e) => {
			error!(?mxc, ?e, "Failed to remove media; it stays until the next sweep.");
			false
		},
	}
}
