//! The collector, acting on one released media at a time, and the sweep,
//! acting on media whose count never left zero.

use std::time::Duration;

use futures::StreamExt;
use ruma::{Mxc, OwnedMxcUri};
use tokio::time::{Interval, MissedTickBehavior, interval};
use tuwunel_core::{debug, error, implement, info, utils::time::now_millis, warn};
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
	// Held from the read through the removal, so a reference being added
	// right now either lands before the read or waits until after the
	// removal.
	let _held = self.hold_media(mxc).await;

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

	self.remove_if_local(mxc, count, TombstoneReason::GarbageCollected)
		.await;
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

/// Removes local media whose count is still zero after the protection period.
///
/// Zero for that long means uploaded and never attached to anything the
/// server was told about: a released reference is collected on the spot, so
/// a count that has sat at zero longer than the grace never had one. Media
/// with no row or the sentinel is unknown and never touched. The decision is
/// made under the media's lock, on the count as it stands then, the same way
/// the collector decides.
#[implement(Service)]
pub(super) async fn sweep_unreferenced(&self) {
	let grace_millis = self
		.services
		.config
		.media_unreferenced_grace_seconds_effective()
		.saturating_mul(1000);
	let protected_since = now_millis().saturating_sub(grace_millis);

	let all_media: Vec<OwnedMxcUri> = match self.services.media.get_all_mxcs().await {
		| Ok(all) => all,
		| Err(e) => {
			error!(?e, "Could not list media for the unreferenced sweep; skipped this round.");
			return;
		},
	};

	let mut looked_at: usize = 0;
	let mut removed: usize = 0;
	let mut candidates = futures::stream::iter(all_media);
	while let Some(mxc) = candidates.next().await {
		let Ok(parsed) = mxc.parts() else {
			continue;
		};
		if !self.services.media.is_local(&parsed) {
			continue;
		}

		// Cheap reads first, so the lock is only taken for real candidates.
		if !matches!(self.refcount(mxc.as_str()).await, Ok(Some(0))) {
			continue;
		}
		let created_millis = self
			.services
			.media
			.find_mtime_millis(&parsed)
			.await;
		if created_millis.is_none_or(|created| created >= protected_since) {
			continue;
		}
		looked_at = looked_at.saturating_add(1);

		let _held = self.hold_media(mxc.as_str()).await;
		match self.refcount(mxc.as_str()).await {
			| Ok(Some(0)) => {},
			| Ok(_) => {
				debug!(?mxc, "Referenced since the first read; kept.");
				continue;
			},
			| Err(e) => {
				warn!(?mxc, ?e, "Reference count unreadable under the lock; kept.");
				continue;
			},
		}

		if self
			.remove_if_local(mxc.as_str(), 0, TombstoneReason::Unreferenced)
			.await
		{
			removed = removed.saturating_add(1);
		}
	}

	if looked_at > 0 {
		info!(looked_at, removed, "Unreferenced media sweep finished.");
	}
}

/// Removes local `mxc` with `reason`, honouring `media_gc_enabled`. Returns
/// whether bytes were removed.
#[implement(Service)]
async fn remove_if_local(&self, mxc: &str, count: i64, reason: TombstoneReason) -> bool {
	let Ok(parsed) = Mxc::try_from(mxc) else {
		error!(?mxc, "Media has an unparseable MXC; media stays.");
		return false;
	};

	if !self.services.media.is_local(&parsed) {
		return false;
	}

	if !self.services.config.media_gc_enabled {
		info!(?mxc, count, ?reason, "Media garbage collection is disabled; would have deleted.");
		return false;
	}

	match self.services.media.collect(&parsed, reason).await {
		| Ok(()) => {
			info!(?mxc, ?reason, "Removed media nothing references.");
			true
		},
		| Err(e) => {
			error!(?mxc, ?e, "Failed to remove media; it stays until the next sweep.");
			false
		},
	}
}
