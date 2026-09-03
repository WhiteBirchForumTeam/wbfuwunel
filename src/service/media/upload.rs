//! Chunked upload: chunks are the transfer unit, not the storage unit.
//!
//! Each chunk lands at its offset in one sparse staging file; which chunks
//! have arrived is a bitmap in `mediaid_upload`. The seal streams the staging
//! file into the storage provider as a single object and writes the same
//! rows `create()` writes, so a sealed upload is an ordinary media item to
//! everything else: reads, thumbnails, deletion, reference counting.
//!
//! The server never sees plaintext. A chunk on the wire is the client's
//! ciphertext, tag included, and is written where it arrived from without a
//! copy. Chunks must arrive in order within an upload; the WebSocket keeps
//! order, so an out-of-order chunk is a client bug and is told so.

use std::{
	io::{self, ErrorKind},
	path::PathBuf,
};

use bytes::Bytes;
use futures::{StreamExt, stream};
use ruma::{
	OwnedMxcUri, UserId,
	http_headers::{ContentDisposition, ContentDispositionType},
};
use tokio::{fs, io::AsyncReadExt};
use tuwunel_core::{
	Err, Result, debug, err, implement, info,
	utils::{self, time::now},
	warn,
};

use super::{Dim, MXC_LENGTH, Upload};

/// The authentication tag an AEAD adds to each chunk on the wire.
pub(super) const CHUNK_TAG_LEN: u32 = 16;

/// What a client asks for when it starts an upload.
#[derive(Debug)]
pub struct UploadRequest {
	/// Declared ciphertext total; a ceiling until the seal.
	pub total_len: u64,
	/// Plaintext chunk size; `None` takes the server default.
	pub chunk_size: Option<u32>,
	pub content_type: Option<String>,
	pub filename: Option<String>,
}

/// What the client gets back from `Create`.
#[derive(Debug)]
pub struct UploadCreated {
	pub upload_id: u64,
	pub mxc: OwnedMxcUri,
	pub chunk_size: u32,
	pub chunk_count: u32,
	pub expires_at_secs: u64,
}

/// Which chunks are in and which are missing, as inclusive runs.
#[derive(Debug)]
pub struct UploadStatus {
	pub received: Vec<[u32; 2]>,
	pub missing: Vec<[u32; 2]>,
	pub received_count: u32,
	pub chunk_count: u32,
}

/// Why an upload operation was refused. Maps onto pack error codes.
#[derive(Debug)]
pub enum UploadError {
	/// No such upload, or not this user's; not distinguished, on purpose.
	NotFound,
	/// The request contradicts the upload: a bad size, a chunk past the end.
	Conflict(String),
	/// A chunk arrived out of order; the client should resume from here.
	OutOfOrder { expected: u32 },
	/// A limit was exceeded.
	TooLarge(String),
	/// Something on the server failed.
	Internal(tuwunel_core::Error),
}

impl From<tuwunel_core::Error> for UploadError {
	fn from(error: tuwunel_core::Error) -> Self { Self::Internal(error) }
}

impl From<io::Error> for UploadError {
	fn from(error: io::Error) -> Self { Self::Internal(error.into()) }
}

type UploadResult<T> = std::result::Result<T, UploadError>;

/// Starts an upload: picks and checks the chunk size, mints the mxc and the
/// upload id, writes the row. No file is touched until the first chunk.
#[implement(super::Service)]
pub async fn upload_create(&self, user: &UserId, request: UploadRequest) -> UploadResult<UploadCreated> {
	let config = &self.services.config;

	let chunk_size = request
		.chunk_size
		.map_or(config.media_chunk_size_default, |size| size as usize);
	if chunk_size < config.media_chunk_size_min || chunk_size > config.media_chunk_size_max {
		return Err(UploadError::Conflict(format!(
			"chunk_size must be between {} and {}",
			config.media_chunk_size_min, config.media_chunk_size_max
		)));
	}
	if !chunk_size.is_power_of_two() {
		return Err(UploadError::Conflict("chunk_size must be a power of two".into()));
	}
	let chunk_size = u32::try_from(chunk_size).map_err(|_| UploadError::Conflict("chunk_size too large".into()))?;
	let wire_chunk_size = chunk_size.saturating_add(CHUNK_TAG_LEN);
	if wire_chunk_size as usize > config.wbf_data_max_bytes {
		return Err(UploadError::Conflict("chunk_size plus tag exceeds wbf_data_max_bytes".into()));
	}

	if request.total_len == 0 {
		return Err(UploadError::Conflict("total_len must be positive".into()));
	}
	if config.media_upload_max_len > 0 && request.total_len > config.media_upload_max_len {
		return Err(UploadError::TooLarge("total_len exceeds media_upload_max_len".into()));
	}
	let chunk_count = request.total_len.div_ceil(u64::from(wire_chunk_size));
	let chunk_count = u32::try_from(chunk_count).map_err(|_| UploadError::TooLarge("too many chunks".into()))?;

	// One quota with pending uploads: both are media promised but not yet
	// delivered.
	let (pending, _) = self.db.count_pending_mxc_for_user(user).await;
	let in_progress = self.count_uploads_for_user(user).await;
	if pending.saturating_add(in_progress) >= config.max_pending_media_uploads {
		return Err(UploadError::TooLarge("maximum number of pending media uploads reached".into()));
	}

	let media_id = utils::random_string(MXC_LENGTH);
	let mxc: OwnedMxcUri = format!("mxc://{}/{media_id}", self.services.globals.server_name()).into();
	let upload_id = mint_upload_id();
	let now_secs = now().as_secs();

	let upload = Upload {
		mxc: mxc.clone(),
		owner: user.to_owned(),
		chunk_size,
		wire_chunk_size,
		total_len: request.total_len,
		chunk_count,
		received: vec![0; (chunk_count as usize).div_ceil(8)],
		received_count: 0,
		content_type: request.content_type,
		filename: request.filename,
		created_at_secs: now_secs,
		last_chunk_at_secs: now_secs,
	};
	self.db.put_upload(upload_id, &upload);

	Ok(UploadCreated {
		upload_id,
		mxc,
		chunk_size,
		chunk_count,
		expires_at_secs: now_secs.saturating_add(config.media_upload_ttl),
	})
}

/// Stores chunk `index` of upload `upload_id`. Returns how many chunks are
/// in after it.
///
/// Chunks are ordered: `index` must be the first missing one. An index
/// already received is accepted again without rewriting (a lost ack); an
/// index beyond the first missing one is out of order.
#[implement(super::Service)]
pub async fn upload_chunk(&self, user: &UserId, upload_id: u64, index: u32, chunk: &[u8]) -> UploadResult<u32> {
	let mut upload = self.owned_upload(user, upload_id).await?;

	if index >= upload.chunk_count {
		return Err(UploadError::Conflict(format!("chunk index {index} is past the last chunk {}", upload.chunk_count - 1)));
	}

	let expected_len = upload.chunk_len(index);
	if chunk.len() as u64 != expected_len {
		return Err(UploadError::Conflict(format!(
			"chunk {index} must be {expected_len} bytes, got {}",
			chunk.len()
		)));
	}

	if upload.has_chunk(index) {
		debug!(upload_id, index, "Chunk already received; acknowledging again.");
		return Ok(upload.received_count);
	}

	let expected = upload.next_missing().unwrap_or(upload.chunk_count);
	if index != expected {
		return Err(UploadError::OutOfOrder { expected });
	}

	let path = self.staging_path(upload_id);
	write_at(&path, upload.chunk_offset(index), chunk).await?;

	upload.mark_chunk(index);
	upload.last_chunk_at_secs = now().as_secs();
	self.db.put_upload(upload_id, &upload);

	Ok(upload.received_count)
}

/// Reports which chunks are in.
#[implement(super::Service)]
pub async fn upload_status(&self, user: &UserId, upload_id: u64) -> UploadResult<UploadStatus> {
	let upload = self.owned_upload(user, upload_id).await?;
	let (received, missing) = upload.runs();

	Ok(UploadStatus {
		received,
		missing,
		received_count: upload.received_count,
		chunk_count: upload.chunk_count,
	})
}

/// Turns a complete upload into media: the staging file becomes one object,
/// the same rows `create()` writes are written, the upload row goes.
///
/// `total_len` is the true size when the client declared a ceiling; it must
/// not exceed the ceiling and must leave every chunk before the last one
/// full.
#[implement(super::Service)]
pub async fn upload_seal(&self, user: &UserId, upload_id: u64, total_len: Option<u64>) -> UploadResult<OwnedMxcUri> {
	let mut upload = self.owned_upload(user, upload_id).await?;

	if let Some(true_len) = total_len {
		if true_len > upload.total_len {
			return Err(UploadError::Conflict("total_len exceeds the declared ceiling".into()));
		}
		let true_count = true_len.div_ceil(u64::from(upload.wire_chunk_size));
		if true_count != u64::from(upload.chunk_count) {
			// A shorter true length that drops whole chunks is a different upload.
			if true_count > u64::from(upload.received_count) {
				return Err(UploadError::Conflict("total_len does not match the chunks received".into()));
			}
			upload.chunk_count = u32::try_from(true_count).map_err(|_| UploadError::Conflict("too many chunks".into()))?;
		}
		upload.total_len = true_len;
	}

	if let Some(first_missing) = upload.next_missing() {
		let (_, missing) = upload.runs();
		return Err(UploadError::Conflict(format!("chunks missing from {first_missing}: {missing:?}")));
	}

	let path = self.staging_path(upload_id);
	let mxc_parts = upload.mxc.parts().map_err(|e| UploadError::Conflict(format!("stored mxc is invalid: {e}")))?;
	let content_disposition = upload.filename.as_deref().map(|filename| {
		ContentDisposition::new(ContentDispositionType::Attachment).with_filename(Some(filename.to_owned()))
	});

	// Same rows as a whole-file upload: metadata, uploader, and the count
	// opened at zero, so everything downstream sees ordinary media.
	let key = self.db.create_file_metadata(
		&mxc_parts,
		Some(user),
		&Dim::default(),
		content_disposition.as_ref(),
		upload.content_type.as_deref(),
	)?;

	self.store_staging_file(&key, &path, upload.total_len).await?;

	self.db.del_upload(upload_id);
	if let Err(e) = fs::remove_file(&path).await {
		warn!(?path, ?e, "Sealed upload's staging file could not be removed.");
	}

	info!(%upload.mxc, upload_id, total_len = upload.total_len, chunks = upload.chunk_count, "Sealed chunked upload.");

	Ok(upload.mxc)
}

/// Drops an upload in progress: its staging file and its row.
#[implement(super::Service)]
pub async fn upload_abort(&self, user: &UserId, upload_id: u64) -> UploadResult<()> {
	self.owned_upload(user, upload_id).await?;
	self.discard_upload(upload_id).await;

	Ok(())
}

/// Removes uploads that have gone `media_upload_ttl` without a chunk, and
/// staging files that belong to no upload.
#[implement(super::Service)]
pub async fn sweep_uploads(&self) {
	let ttl = self.services.config.media_upload_ttl;
	let now_secs = now().as_secs();

	let expired: Vec<u64> = self
		.db
		.list_uploads()
		.filter_map(async |(id, upload)| {
			(upload.last_chunk_at_secs.saturating_add(ttl) < now_secs).then_some(id)
		})
		.collect()
		.await;

	for upload_id in &expired {
		info!(upload_id, "Sweeping abandoned chunked upload.");
		self.discard_upload(*upload_id).await;
	}

	// Files without a row: the row is the truth, the file is not.
	let dir = self.staging_dir();
	let Ok(mut entries) = fs::read_dir(&dir).await else {
		return;
	};
	while let Ok(Some(entry)) = entries.next_entry().await {
		let name = entry.file_name();
		let Some(upload_id) = name.to_str().and_then(|name| u64::from_str_radix(name, 16).ok()) else {
			continue;
		};
		if self.db.find_upload(upload_id).await.is_none() {
			warn!(upload_id, "Staging file without an upload row; removing.");
			_ = fs::remove_file(entry.path()).await;
		}
	}
}

/// The upload, if it exists and `user` owns it. Both failures answer the
/// same, so an upload id cannot be probed.
#[implement(super::Service)]
async fn owned_upload(&self, user: &UserId, upload_id: u64) -> UploadResult<Upload> {
	match self.db.find_upload(upload_id).await {
		| Some(upload) if upload.owner == user => Ok(upload),
		| _ => Err(UploadError::NotFound),
	}
}

#[implement(super::Service)]
async fn count_uploads_for_user(&self, user: &UserId) -> usize {
	self.db
		.list_uploads()
		.filter(|(_, upload)| futures::future::ready(upload.owner == user))
		.count()
		.await
}

#[implement(super::Service)]
async fn discard_upload(&self, upload_id: u64) {
	self.db.del_upload(upload_id);
	match fs::remove_file(self.staging_path(upload_id)).await {
		| Ok(()) => {},
		| Err(e) if e.kind() == ErrorKind::NotFound => {},
		| Err(e) => warn!(upload_id, ?e, "Could not remove staging file."),
	}
}

#[implement(super::Service)]
fn staging_dir(&self) -> PathBuf { self.get_media_dir().join("staging") }

#[implement(super::Service)]
fn staging_path(&self, upload_id: u64) -> PathBuf { self.staging_dir().join(format!("{upload_id:016x}")) }

/// Streams the staging file into every configured provider as one object
/// named for `key`, the way `create_media_file` stores a whole upload.
#[implement(super::Service)]
async fn store_staging_file(&self, key: &[u8], path: &PathBuf, len: u64) -> Result {
	let name = self.get_media_name_sha256(key);
	let store_on = &self.services.config.store_media_on_providers;
	let mut stored = 0_usize;

	for provider in self.storage_providers() {
		if !store_on.is_empty() && !store_on.contains(&provider.name) {
			continue;
		}

		let file = fs::File::open(path).await?;
		let chunks = stream::try_unfold(file, async |mut file| {
			let mut buf = vec![0_u8; 1024 * 1024];
			let read = file.read(&mut buf).await?;
			if read == 0 {
				return Ok::<_, tuwunel_core::Error>(None);
			}
			buf.truncate(read);
			Ok(Some((Bytes::from(buf), file)))
		});

		let size = usize::try_from(len).ok();
		provider
			.put(name.as_str(), size, chunks)
			.await
			.map_err(|e| err!(Database(error!(?name, provider = ?provider.name, "Failed to store sealed upload: {e:?}"))))?;
		stored = stored.saturating_add(1);
	}

	if stored == 0 {
		return Err!(Database("Sealed upload stored on no provider."));
	}

	Ok(())
}

/// A fresh upload id: random, non-zero, and not in use. Zero means "no id"
/// on the wire.
fn mint_upload_id() -> u64 {
	loop {
		let hex = utils::rand::string_from(b"0123456789abcdef", 16);
		let id = u64::from_str_radix(&hex, 16).expect("16 hex digits fit a u64");
		if id != 0 {
			return id;
		}
	}
}

/// Writes `bytes` at `offset`, creating the file and its directory if
/// needed. The file is sparse where nothing has been written yet.
async fn write_at(path: &PathBuf, offset: u64, bytes: &[u8]) -> io::Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).await?;
	}

	let path = path.clone();
	let bytes = bytes.to_vec();
	tokio::task::spawn_blocking(move || {
		let file = std::fs::OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(false)
			.open(&path)?;
		positioned_write(&file, offset, &bytes)
	})
	.await
	.map_err(|e| io::Error::other(e))?
}

#[cfg(unix)]
fn positioned_write(file: &std::fs::File, offset: u64, bytes: &[u8]) -> io::Result<()> {
	use std::os::unix::fs::FileExt;

	file.write_all_at(bytes, offset)
}

#[cfg(windows)]
fn positioned_write(file: &std::fs::File, offset: u64, bytes: &[u8]) -> io::Result<()> {
	use std::os::windows::fs::FileExt;

	let mut written = 0_usize;
	while written < bytes.len() {
		let n = file.seek_write(&bytes[written..], offset + written as u64)?;
		if n == 0 {
			return Err(io::Error::new(ErrorKind::WriteZero, "seek_write wrote nothing"));
		}
		written += n;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use ruma::{OwnedMxcUri, user_id};

	use super::super::Upload;

	fn upload(chunk_count: u32, wire_chunk_size: u32, total_len: u64) -> Upload {
		Upload {
			mxc: OwnedMxcUri::from("mxc://localhost/abc"),
			owner: user_id!("@a:localhost").to_owned(),
			chunk_size: wire_chunk_size - 16,
			wire_chunk_size,
			total_len,
			chunk_count,
			received: vec![0; (chunk_count as usize).div_ceil(8)],
			received_count: 0,
			content_type: None,
			filename: None,
			created_at_secs: 0,
			last_chunk_at_secs: 0,
		}
	}

	#[test]
	fn bitmap_tracks_chunks_and_reports_runs() {
		let mut upload = upload(100, 80, 100 * 80);
		assert_eq!(upload.next_missing(), Some(0));

		for index in 0..42 {
			assert!(upload.mark_chunk(index));
		}
		assert!(upload.mark_chunk(43));
		assert!(!upload.mark_chunk(43), "second mark is not new");

		assert_eq!(upload.received_count, 43);
		assert_eq!(upload.next_missing(), Some(42));
		assert!(!upload.is_complete());

		let (received, missing) = upload.runs();
		assert_eq!(received, vec![[0, 41], [43, 43]]);
		assert_eq!(missing, vec![[42, 42], [44, 99]]);
	}

	#[test]
	fn the_last_chunk_is_shorter_when_the_total_says_so() {
		// 120 KB of ciphertext in wire chunks of 64 KiB + 16: two chunks.
		let wire = 64 * 1024 + 16;
		let total = 120 * 1000;
		let upload = upload(2, wire, total);

		assert_eq!(upload.chunk_len(0), u64::from(wire));
		assert_eq!(upload.chunk_len(1), total - u64::from(wire));
		assert_eq!(upload.chunk_offset(1), u64::from(wire));
	}

	#[test]
	fn a_complete_bitmap_has_no_missing_run() {
		let mut upload = upload(3, 32, 96);
		for index in 0..3 {
			upload.mark_chunk(index);
		}

		assert!(upload.is_complete());
		assert_eq!(upload.next_missing(), None);
		assert_eq!(upload.runs(), (vec![[0, 2]], vec![]));
	}
}
