//! Ranged reads of stored media: what a seek costs is the bytes sought.

use bytes::Bytes;
use futures::{StreamExt, pin_mut};
use ruma::Mxc;
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{result::LogDebugErr, stream::IterStream},
};

use super::{Dim, Metadata};

/// What a client needs to plan reads of one media item.
#[derive(Debug)]
pub struct MediaInfo {
	pub total_len: u64,
	pub content_type: Option<String>,
}

/// One ranged read: where it started, what came back, and how big the whole
/// object is so the client knows when it has reached the end.
#[derive(Debug)]
pub struct RangeRead {
	pub pos: u64,
	pub bytes: Bytes,
	pub total_len: u64,
}

/// Size and type of `mxc`, without reading its bytes.
///
/// Removed media answers with the tombstone's 410 through the same metadata
/// lookup every fetch uses.
#[implement(super::Service)]
pub async fn media_info(&self, mxc: &Mxc<'_>) -> Result<MediaInfo> {
	let Metadata { content_type, key, .. } = self
		.db
		.search_file_metadata(mxc, &Dim::default())
		.await?;

	let object = self
		.head_meta(&key)
		.await
		.ok_or_else(|| err!(Request(NotFound("Media object not found on any provider."))))?;

	Ok(MediaInfo { total_len: object.size, content_type })
}

/// Reads `len` bytes of `mxc` from `pos`, clamped to the object's end.
///
/// A `pos` at or past the end is a client error, not an empty success: it
/// means the client's picture of the object is wrong.
#[implement(super::Service)]
pub async fn read_range(&self, mxc: &Mxc<'_>, pos: u64, len: u64) -> Result<RangeRead> {
	let Metadata { key, .. } = self
		.db
		.search_file_metadata(mxc, &Dim::default())
		.await?;

	let path = self.get_media_name_sha256(&key);
	let object = self
		.head_meta(&key)
		.await
		.ok_or_else(|| err!(Request(NotFound("Media object not found on any provider."))))?;

	let total_len = object.size;
	if pos >= total_len {
		return Err!(Request(InvalidParam("Read position is past the end of the media.")));
	}
	if len == 0 {
		return Err!(Request(InvalidParam("Read length must be positive.")));
	}
	let end = pos.saturating_add(len).min(total_len);

	let reads = self
		.storage_providers()
		.stream()
		.filter_map(async |provider| {
			provider
				.get_range(path.as_str(), pos..end)
				.await
				.log_debug_err()
				.ok()
		});

	pin_mut!(reads);
	let Some(bytes) = reads.next().await else {
		return Err!(Request(NotFound("Media range not readable from any provider.")));
	};

	Ok(RangeRead { pos, bytes, total_len })
}
