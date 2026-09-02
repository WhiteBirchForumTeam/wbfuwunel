//! The rebuild: recount every reference from what is stored, then remove
//! local media the recount found nothing pointing at.
//!
//! Offline work by design. Counts are recomputed in memory from every stored
//! event, every retained unredacted original and every local avatar, then
//! written over whatever the rows held, so the sentinel disappears and every
//! row becomes a real number. References added while the rebuild runs are
//! overwritten; run it in a maintenance window.

use std::{collections::HashMap, sync::atomic::Ordering, time::Duration};

use futures::StreamExt;
use ruma::{CanonicalJsonObject, OwnedMxcUri};
use tuwunel_core::{
	Result, debug, implement, info,
	utils::{
		stream::{BroadbandExt, ReadyExt, TryIgnore},
		time::now,
	},
	warn,
};
use tuwunel_database::CounterOperand;

use super::{Service, list_event_mxc_uris_in};
use crate::media::TombstoneReason;

/// What a rebuild scanned and decided.
#[derive(Debug, Default)]
pub struct RebuildReport {
	pub dry_run: bool,
	pub events: usize,
	pub retained_originals: usize,
	pub avatars: usize,
	pub media: usize,
	/// Local media the recount found unreferenced. Removed unless `dry_run`.
	pub orphans: Vec<OwnedMxcUri>,
	/// Unreferenced local media uploaded too recently to judge; left alone.
	pub skipped_recent: Vec<OwnedMxcUri>,
}

/// Merges one more reference into an in-memory recount.
fn count_reference(counts: &mut HashMap<String, i64>, mxc: String) {
	*counts.entry(mxc).or_default() += 1;
}

/// Counts the references in one stored event, if it parses as one.
///
/// Returns whether it parsed; unparseable rows are counted as scanned but
/// contribute nothing.
fn count_event(counts: &mut HashMap<String, i64>, event_bytes: &[u8]) -> bool {
	let Ok(event) = serde_json::from_slice::<CanonicalJsonObject>(event_bytes) else {
		return false;
	};

	for mxc in list_event_mxc_uris_in(&event) {
		count_reference(counts, mxc);
	}

	true
}

/// Recounts every reference and removes the orphans, with the collector
/// standing aside for the duration.
#[implement(Service)]
pub async fn rebuild(&self, dry_run: bool) -> Result<RebuildReport> {
	self.collector_paused.store(true, Ordering::Release);
	let report = self.rebuild_with_collector_paused(dry_run).await;
	self.collector_paused.store(false, Ordering::Release);

	report
}

#[implement(Service)]
async fn rebuild_with_collector_paused(&self, dry_run: bool) -> Result<RebuildReport> {
	let mut report = RebuildReport { dry_run, ..RebuildReport::default() };
	let mut counts: HashMap<String, i64> = HashMap::new();

	// Every stored event. Redacted content is empty and counts nothing; its
	// retained original, scanned next, is what holds the reference.
	let pdus = self.services.db["pduid_pdu"].clone();
	pdus.raw_stream()
		.ignore_err()
		.ready_for_each(|(_, value)| {
			report.events = report.events.saturating_add(1);
			if !count_event(&mut counts, value) {
				debug!("Stored event did not parse; counted as scanned only.");
			}
		})
		.await;

	self.services
		.retention
		.retained_pdus_raw()
		.ignore_err()
		.ready_for_each(|value| {
			report.retained_originals = report.retained_originals.saturating_add(1);
			if !count_event(&mut counts, value) {
				debug!("Retained original did not parse; counted as scanned only.");
			}
		})
		.await;

	// Every local user's avatar. Room avatars are state events, already
	// counted above.
	let avatars: Vec<OwnedMxcUri> = self
		.services
		.users
		.list_local_users()
		.map(ToOwned::to_owned)
		.broad_filter_map(async |user| self.services.profile.avatar_url(&user).await.ok())
		.collect()
		.await;

	report.avatars = avatars.len();
	for avatar in avatars {
		count_reference(&mut counts, avatar.to_string());
	}

	let all_media = self.services.media.get_all_mxcs().await?;
	report.media = all_media.len();

	if !dry_run {
		self.overwrite_counts(&counts, &all_media).await;
	}

	// Orphans: local media the recount found nothing pointing at, unless it
	// is new enough that the message naming it may still be on its way.
	let skip_recent = Duration::from_secs(
		self.services
			.config
			.media_gc_migrate_skip_recent_seconds,
	);
	let now_millis = u64::try_from(now().as_millis()).unwrap_or(u64::MAX);
	let recent_since_millis = now_millis.saturating_sub(
		u64::try_from(skip_recent.as_millis()).unwrap_or(u64::MAX),
	);

	for mxc in all_media {
		let Ok(parsed) = mxc.parts() else {
			continue;
		};

		if !self.services.media.is_local(&parsed) {
			continue;
		}

		if counts.get(mxc.as_str()).copied().unwrap_or(0) > 0 {
			continue;
		}

		let created_millis = self
			.services
			.media
			.find_mtime_millis(&parsed)
			.await;

		if created_millis.is_none_or(|created| created >= recent_since_millis) {
			report.skipped_recent.push(mxc);
			continue;
		}

		if !dry_run {
			match self
				.services
				.media
				.collect(&parsed, TombstoneReason::Migrated)
				.await
			{
				| Ok(()) => info!(?mxc, "Rebuild removed media nothing references."),
				| Err(e) => warn!(?mxc, ?e, "Rebuild failed to remove orphaned media."),
			}
		}

		report.orphans.push(mxc);
	}

	Ok(report)
}

/// Replaces every reference count row with the recount.
///
/// Media the recount never saw is set to zero rather than left absent, so a
/// later reference cannot turn it into the sentinel.
#[implement(Service)]
async fn overwrite_counts(&self, counts: &HashMap<String, i64>, all_media: &[OwnedMxcUri]) {
	const ROWS_PER_BATCH: usize = 1000;

	self.db.mxc_refcount.clear().await;

	let mut rows: Vec<(&str, i64)> = all_media
		.iter()
		.map(|mxc| (mxc.as_str(), counts.get(mxc.as_str()).copied().unwrap_or(0)))
		.collect();

	rows.extend(
		counts
			.iter()
			.filter(|(mxc, _)| !all_media.iter().any(|known| known.as_str() == mxc.as_str()))
			.map(|(mxc, count)| (mxc.as_str(), *count)),
	);

	for chunk in rows.chunks(ROWS_PER_BATCH) {
		let mut txn = self.services.db.txn();
		for (mxc, count) in chunk {
			txn.merge(&self.db.mxc_refcount, *mxc, CounterOperand::Set(*count).to_bytes());
		}
		txn.execute();
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use super::count_event;

	#[test]
	fn a_live_event_counts_each_media_it_names() {
		let mut counts = HashMap::new();
		let event = br#"{"type":"m.room.message","content":{"msgtype":"m.image","url":"mxc://a/one","info":{"thumbnail_url":"mxc://a/two"}}}"#;

		assert!(count_event(&mut counts, event));
		assert_eq!(counts.get("mxc://a/one"), Some(&1));
		assert_eq!(counts.get("mxc://a/two"), Some(&1));
	}

	#[test]
	fn two_events_naming_one_media_count_two() {
		let mut counts = HashMap::new();
		let event = br#"{"type":"m.room.message","content":{"msgtype":"m.image","url":"mxc://a/one"}}"#;

		assert!(count_event(&mut counts, event));
		assert!(count_event(&mut counts, event));
		assert_eq!(counts.get("mxc://a/one"), Some(&2));
	}

	#[test]
	fn a_redacted_event_counts_nothing() {
		let mut counts = HashMap::new();

		assert!(count_event(&mut counts, br#"{"type":"m.room.message","content":{}}"#));
		assert!(counts.is_empty());
	}

	#[test]
	fn an_unparseable_row_counts_nothing_and_says_so() {
		let mut counts = HashMap::new();

		assert!(!count_event(&mut counts, b"not json"));
		assert!(counts.is_empty());
	}
}
