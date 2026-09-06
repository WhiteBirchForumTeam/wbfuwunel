use std::fmt::Write;

use ruma::OwnedMxcUri;
use tuwunel_core::Result;

use crate::admin_command;

/// Lists who holds a media item (see docs/design/media-holders.md), and
/// whether the holder model manages it at all.
#[admin_command]
pub(super) async fn refcount(&self, mxc: OwnedMxcUri) -> Result {
	let media_refs = &self.services.media_refs;
	let mxc = mxc.as_str();

	if let Some(tombstone) = self
		.services
		.media
		.find_tombstone(&mxc.try_into()?)
		.await
	{
		let report = format!(
			"{mxc} was removed at {} ({:?}); a fetch answers 410 Gone.",
			tombstone.deleted_at_secs, tombstone.reason
		);

		return self.write_str(&report).await;
	}

	let mut out = String::new();
	match media_refs.managed_since(mxc).await {
		| Some(created) => writeln!(out, "{mxc} is managed (stored at {created} ms).")?,
		| None => writeln!(
			out,
			"{mxc} predates the holder model: it is never removed automatically, and holders are not \
			 tracked for it."
		)?,
	}

	let holders = media_refs.list_holders(mxc).await;
	if holders.is_empty() {
		writeln!(out, "Nothing holds it.")?;
	} else {
		writeln!(out, "Held by {}:", holders.len())?;
		for holder in holders {
			writeln!(out, "- {holder}")?;
		}
	}

	self.write_str(&out).await
}
