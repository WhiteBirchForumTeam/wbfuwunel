//! The sweep: the one thing that removes media, and the one decision it asks.
//!
//! Releasing a holder does not remove anything (`mod.rs`), so everything that
//! ever loses its last holder is removed here or not at all. That is sound
//! because the sweep walks every managed media rather than only the ones a
//! release pointed it at: how a media came to be unheld cannot hide it.
//! See `/docs/design/media/premature-deletion.md` §2.4.

use std::time::Duration;

use futures::StreamExt;
use ruma::Mxc;
use tokio::time::{Interval, MissedTickBehavior, interval};
use tuwunel_core::{debug, error, implement, info, utils::{stream::TryIgnore, time::now_millis}};

use super::Service;
use crate::media::TombstoneReason;

/// The moment before which a media is old enough to be removed: now minus the
/// protection period (`media_unreferenced_grace_seconds`, floored at 7 days by
/// the config).
///
/// 🚨 The only place this is computed, so the decision and the sweep's candidate
/// filter cannot drift apart: they are two gates on one number, and **either one
/// alone refuses a media that is too young** (verified by mutation 2026-11-07:
/// disabling the condition in `is_removable` changed no observable behaviour,
/// because the filter never offered it a young candidate). That is deliberate
/// depth on a path that deletes files, and it is why the condition in
/// `is_removable` is asserted directly rather than through a sweep.
///
/// Return:
///     u64  milliseconds since the epoch; 0 when the period is longer than the
///     server has existed, which removes nothing.
#[implement(Service)]
pub(super) fn get_grace_cutoff_millis(&self) -> u64 {
	let grace_millis = self
		.services
		.config
		.media_unreferenced_grace_seconds_effective()
		.saturating_mul(1000);

	now_millis().saturating_sub(grace_millis)
}

/// The one decision: local, managed by this model, past the protection period,
/// and held by nothing. Unknown (not managed) is "no", never "yes".
///
/// 🚨 The protection period is part of this decision, not of the caller that
/// happens to ask. A media is protected until it is old enough **whoever**
/// released it and **however** it came to be unheld: a third party could
/// otherwise add a holder to someone else's fresh upload and take it away
/// again, which deleted it inside the window the period exists to guard
/// (/docs/design/media/premature-deletion.md §2).
///
/// 📎 `pub` so it can be asked directly: the sweep's candidate filter hides
/// young media from it, so a test driving a sweep cannot tell whether this
/// function knows about the protection period at all
/// (`src/main/tests/media_release_does_not_delete.rs`).
///
/// Args:
///     mxc: example: "mxc://localhost/abcdef"
/// Return:
///     bool  true only when all four hold; false for anything unparseable,
///     remote, unmanaged, young, or still held.
#[implement(Service)]
pub async fn is_removable(&self, mxc: &str) -> bool {
	let Ok(parsed) = Mxc::try_from(mxc) else {
		return false;
	};
	if !self.services.media.is_local(&parsed) {
		return false;
	}
	let Some(created) = self.managed_since(mxc).await else {
		return false;
	};
	if created >= self.get_grace_cutoff_millis() {
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

/// Removes every managed media nothing holds any more, once it is past the
/// protection period. Walks `mxc_managed` only, so media from before the model
/// is never looked at.
///
/// 🚨 This is the only path that removes media automatically, so it also has to
/// be the only path that is *noticed* when it stops running: it logs every
/// sweep, including the ones that found nothing. A sweep that silently died
/// looks exactly like a server with nothing to collect
/// (/docs/design/media/premature-deletion.md §2.4).
///
/// 📎 `pub` for one reason: the integration test
/// `src/main/tests/media_release_does_not_delete.rs` drives a sweep rather than
/// waiting `media_gc_sweep_interval` for one.
#[implement(Service)]
pub async fn sweep_unreferenced(&self) {
	let cutoff = self.get_grace_cutoff_millis();

	// The first of the two age gates: it keeps the protected majority out of
	// the candidate list, so this does not build a Vec of every managed media,
	// and `is_removable` asks the same question again on what survives. A row
	// whose value is not a timestamp is kept out of the list (`u64::MAX`),
	// never swept in by accident.
	let candidates: Vec<String> = self
		.db
		.mxc_managed
		.raw_stream()
		.ignore_err()
		.filter_map(|(key, value)| {
			let created = <[u8; 8]>::try_from(value).map(u64::from_be_bytes).unwrap_or(u64::MAX);
			let mxc = (created < cutoff).then(|| String::from_utf8_lossy(key).into_owned());
			async move { mxc }
		})
		.collect()
		.await;

	let mut removed: usize = 0;
	for mxc in &candidates {
		// Held from the decision through the removal, so a holder being added
		// right now either lands before the look or waits until after the
		// removal.
		let _held = self.hold_media(mxc).await;
		if !self.is_removable(mxc).await {
			continue;
		}
		if self.remove(mxc, TombstoneReason::Unreferenced).await {
			removed = removed.saturating_add(1);
		}
	}

	info!(candidates = candidates.len(), removed, "Unreferenced media sweep finished.");
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
