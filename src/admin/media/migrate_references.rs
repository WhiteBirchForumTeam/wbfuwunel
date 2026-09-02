use std::fmt::Write;

use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn migrate_references(&self, dry_run: bool) -> Result {
	let report = self.services.media_refs.rebuild(dry_run).await?;

	let mut out = String::new();

	writeln!(
		out,
		"Scanned {} events, {} retained originals, {} avatars and {} media.",
		report.events, report.retained_originals, report.avatars, report.media
	)?;

	if report.dry_run {
		writeln!(out, "Dry run: counts were not written and nothing was removed.")?;
		writeln!(out, "{} media would be removed as unreferenced:", report.orphans.len())?;
	} else {
		writeln!(out, "Counts rebuilt; the sentinel is gone from every row.")?;
		writeln!(out, "{} media removed as unreferenced:", report.orphans.len())?;
	}

	for mxc in &report.orphans {
		writeln!(out, "- {mxc}")?;
	}

	if !report.skipped_recent.is_empty() {
		writeln!(
			out,
			"{} unreferenced media skipped as too recent (see \
			 `media_gc_migrate_skip_recent_seconds`):",
			report.skipped_recent.len()
		)?;
		for mxc in &report.skipped_recent {
			writeln!(out, "- {mxc}")?;
		}
	}

	self.write_str(&out).await
}
