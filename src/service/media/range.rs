//! Ranged reads of stored media: what a seek costs is the bytes sought.

use bytes::Bytes;
use futures::{StreamExt, pin_mut};
use ruma::Mxc;
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{result::LogDebugErr, stream::IterStream},
};

use super::{ChunkedMedia, Dim, Metadata};

/// What a client needs to plan reads of one media item.
#[derive(Debug)]
pub struct MediaInfo {
	pub total_len: u64,
	pub content_type: Option<String>,
	/// The chunk shape when the media was uploaded in chunks; `None` for a
	/// whole-file upload, which is read by position instead.
	pub chunked: Option<ChunkedMedia>,
}

/// One chunk of chunked media, exactly as it was uploaded.
#[derive(Debug)]
pub struct ChunkRead {
	pub index: u32,
	pub pos: u64,
	pub bytes: Bytes,
	/// Plaintext chunk size; plaintext position `p` is in chunk
	/// `p / chunk_size`.
	pub chunk_size: u32,
	pub chunk_count: u32,
	pub total_len: u64,
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

	// The stored type is an empty string when the uploader gave none; say
	// "none" rather than hand the client a placeholder.
	let content_type = content_type.filter(|content_type| !content_type.is_empty());
	let chunked = self.db.find_chunked_media(mxc).await;

	Ok(MediaInfo { total_len: object.size, content_type, chunked })
}

/// The chunk shape of `mxc` if it was uploaded in chunks; `None` for a
/// whole-file upload. One point read; no object access.
#[implement(super::Service)]
pub async fn chunked_shape(&self, mxc: &Mxc<'_>) -> Option<ChunkedMedia> {
	self.db.find_chunked_media(mxc).await
}

/// Reads chunk `index` of chunked `mxc`: the bytes that chunk arrived as,
/// no more and no less, because only the uploader's key can make sense of
/// them and it did so per chunk. Where it sits was recorded when it arrived.
#[implement(super::Service)]
pub async fn read_chunk(&self, mxc: &Mxc<'_>, index: u32) -> Result<ChunkRead> {
	let Metadata { key, .. } = self
		.db
		.search_file_metadata(mxc, &Dim::default())
		.await?;

	let Some(chunked) = self.db.find_chunked_media(mxc).await else {
		return Err!(Request(InvalidParam("Not chunked media; read it by position.")));
	};
	if index >= chunked.chunk_count {
		return Err!(Request(InvalidParam("Chunk index {index} is past the last chunk {}.", chunked.chunk_count.saturating_sub(1))));
	}
	let Some(span) = self.db.find_chunk_span(mxc, index).await else {
		return Err!(Database("Chunk {index} of {mxc} has no span row."));
	};

	let path = self.get_media_name_sha256(&key);
	let pos = span.offset;
	let end = pos.saturating_add(u64::from(span.len));
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
		return Err!(Request(NotFound("Media chunk not readable from any provider.")));
	};

	Ok(ChunkRead {
		index,
		pos,
		bytes,
		chunk_size: chunked.chunk_size,
		chunk_count: chunked.chunk_count,
		total_len: chunked.total_len,
	})
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
