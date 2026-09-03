//! Chunked upload: chunks are the transfer unit, not the storage unit.
//!
//! Chunks arrive in order and are appended to one staging file; the row in
//! `mediaid_upload` says how many have arrived and how many bytes that is.
//! The client marks the last chunk (`IS_LAST`), which is the only way the
//! server learns the size. The seal streams the staging file into the storage
//! provider as a single object and writes the same rows `create()` writes, so
//! a sealed upload is an ordinary media item to everything else: reads,
//! thumbnails, deletion, reference counting.
//!
//! The server never sees plaintext. A chunk on the wire is the client's
//! ciphertext, and how long that is belongs to the client: the server caps it
//! at the declared chunk size plus `media_chunk_overhead_max`, and trusts the
//! pack's CRC for integrity. The decrypting client checks the exact size when
//! it unwraps the chunk.
//!
//! The server cannot re-chunk ciphertext, so a download must hand back each
//! chunk exactly as it arrived. Nothing is recorded per chunk for that: chunk
//! 0 sets the wire chunk length, every chunk but the last must match it, and
//! chunk `i` is then simply the bytes at `i * wire_chunk_size`.

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

use super::{ChunkedMedia, Dim, MXC_LENGTH, Upload};

/// What a client asks for when it starts an upload.
#[derive(Debug)]
pub struct UploadRequest {
	/// Plaintext chunk size; `None` takes the server default. Fixed once set.
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
	/// Largest chunk the server will accept on the wire for this upload.
	pub chunk_max_bytes: u32,
	pub expires_at_secs: u64,
}

/// Where an upload stands: the next chunk expected is `received_count`.
#[derive(Clone, Copy, Debug)]
pub struct UploadStatus {
	pub received_count: u32,
	pub total_len: u64,
	pub finished: bool,
	pub chunk_size: u32,
}

/// What a stored chunk changed: how many are in and how many bytes.
#[derive(Clone, Copy, Debug)]
pub struct ChunkStored {
	pub received_count: u32,
	pub total_len: u64,
	pub finished: bool,
}

/// Why an upload operation was refused. Maps onto pack error codes.
#[derive(Debug)]
pub enum UploadError {
	/// No such upload, or not this user's; not distinguished, on purpose.
	NotFound,
	/// The request contradicts the upload: a chunk after the last, a seal
	/// before it.
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

/// Starts an upload: checks the chunk size, mints the mxc and the upload id,
/// writes the row. No file is touched until the first chunk.
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
	let chunk_max_bytes = chunk_size.saturating_add(config.media_chunk_overhead_max);
	if chunk_max_bytes > config.wbf_data_max_bytes {
		return Err(UploadError::Conflict("chunk_size plus overhead exceeds wbf_data_max_bytes".into()));
	}
	let chunk_size = u32::try_from(chunk_size).map_err(|_| UploadError::Conflict("chunk_size too large".into()))?;
	let chunk_max_bytes =
		u32::try_from(chunk_max_bytes).map_err(|_| UploadError::Conflict("chunk_size too large".into()))?;

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
		wire_chunk_size: 0,
		total_len: 0,
		received_count: 0,
		finished: false,
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
		chunk_max_bytes,
		expires_at_secs: now_secs.saturating_add(config.media_upload_ttl),
	})
}

/// Appends chunk `index` of upload `upload_id`; `is_last` marks the end of
/// the upload.
///
/// Chunks are ordered: `index` must be `received_count`. A lower index was
/// already received and is acknowledged again without rewriting (a lost
/// ack); a higher one is out of order and told what was expected.
///
/// Chunk 0 sets the wire chunk length; every later chunk must match it,
/// except the last, which may be shorter. That keeps chunk `i` at
/// `i * wire_chunk_size` so a download can find it without a table.
#[implement(super::Service)]
pub async fn upload_chunk(
	&self,
	user: &UserId,
	upload_id: u64,
	index: u32,
	chunk: &[u8],
	is_last: bool,
) -> UploadResult<ChunkStored> {
	let config = &self.services.config;
	let mut upload = self.owned_upload(user, upload_id).await?;

	if index < upload.received_count {
		debug!(upload_id, index, "Chunk already received; acknowledging again.");
		return Ok(stored(&upload));
	}
	if index > upload.received_count {
		return Err(UploadError::OutOfOrder { expected: upload.received_count });
	}
	if upload.finished {
		return Err(UploadError::Conflict(format!(
			"upload finished at chunk {}; nothing may follow",
			upload.received_count.saturating_sub(1)
		)));
	}

	if chunk.is_empty() {
		return Err(UploadError::Conflict("a chunk carries at least one byte".into()));
	}
	let chunk_max_bytes = (upload.chunk_size as usize).saturating_add(config.media_chunk_overhead_max);
	if chunk.len() > chunk_max_bytes {
		return Err(UploadError::TooLarge(format!(
			"chunk {index} is {} bytes; this upload's chunks are at most {chunk_max_bytes}",
			chunk.len()
		)));
	}
	let len = u32::try_from(chunk.len()).map_err(|_| UploadError::TooLarge("chunk too large".into()))?;
	let wire_chunk_size = if index == 0 { len } else { upload.wire_chunk_size };
	if !is_last && len != wire_chunk_size {
		return Err(UploadError::Conflict(format!(
			"chunk {index} is {len} bytes but chunk 0 was {wire_chunk_size}; only the last chunk may differ"
		)));
	}
	if is_last && len > wire_chunk_size {
		return Err(UploadError::Conflict(format!(
			"last chunk {index} is {len} bytes, longer than the {wire_chunk_size} of the chunks before it"
		)));
	}
	let total_len = upload.total_len.saturating_add(u64::from(len));
	if config.media_upload_max_len > 0 && total_len > config.media_upload_max_len {
		return Err(UploadError::TooLarge("upload exceeds media_upload_max_len".into()));
	}

	// The file is written before the row: if the server dies between them,
	// the row still says this chunk is missing and the resend lands on the
	// same offset.
	let path = self.staging_path(upload_id);
	write_at(&path, upload.total_len, chunk).await?;

	upload.wire_chunk_size = wire_chunk_size;
	upload.total_len = total_len;
	upload.received_count = index.saturating_add(1);
	upload.finished = is_last;
	upload.last_chunk_at_secs = now().as_secs();
	self.db.put_upload(upload_id, &upload);

	Ok(stored(&upload))
}

/// Reports where an upload stands, so a client can resume from
/// `received_count`.
#[implement(super::Service)]
pub async fn upload_status(&self, user: &UserId, upload_id: u64) -> UploadResult<UploadStatus> {
	let upload = self.owned_upload(user, upload_id).await?;

	Ok(UploadStatus {
		received_count: upload.received_count,
		total_len: upload.total_len,
		finished: upload.finished,
		chunk_size: upload.chunk_size,
	})
}

/// Turns a finished upload into media: the staging file becomes one object,
/// the same rows `create()` writes are written, the upload row goes.
#[implement(super::Service)]
pub async fn upload_seal(&self, user: &UserId, upload_id: u64) -> UploadResult<OwnedMxcUri> {
	let upload = self.owned_upload(user, upload_id).await?;

	if !upload.finished {
		return Err(UploadError::Conflict(format!(
			"upload not finished: {} chunks, {} bytes, last chunk not yet sent",
			upload.received_count, upload.total_len
		)));
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

	// The shape a downloader needs to find chunk `i` at `i * wire_chunk_size`.
	self.db.put_chunked_media(&mxc_parts, ChunkedMedia {
		chunk_size: upload.chunk_size,
		wire_chunk_size: upload.wire_chunk_size,
		chunk_count: upload.received_count,
		total_len: upload.total_len,
	});

	self.db.del_upload(upload_id);
	if let Err(e) = fs::remove_file(&path).await {
		warn!(?path, ?e, "Sealed upload's staging file could not be removed.");
	}

	info!(%upload.mxc, upload_id, total_len = upload.total_len, chunks = upload.received_count, "Sealed chunked upload.");

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

/// Streams the first `len` bytes of the staging file into every configured
/// provider as one object named for `key`, the way `create_media_file`
/// stores a whole upload.
#[implement(super::Service)]
async fn store_staging_file(&self, key: &[u8], path: &PathBuf, len: u64) -> Result {
	let name = self.get_media_name_sha256(key);
	let store_on = &self.services.config.store_media_on_providers;
	let mut stored = 0_usize;

	for provider in self.storage_providers() {
		if !store_on.is_empty() && !store_on.contains(&provider.name) {
			continue;
		}

		// `len` is the row's truth; the file is capped to it rather than trusted.
		let file = fs::File::open(path).await?.take(len);
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

fn stored(upload: &Upload) -> ChunkStored {
	ChunkStored {
		received_count: upload.received_count,
		total_len: upload.total_len,
		finished: upload.finished,
	}
}

/// A fresh upload id: random and non-zero. Zero means "no id" on the wire.
/// Collisions are not checked: 64 random bits make one negligible, and its
/// cost would be one owner's in-progress row.
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
/// needed. Positioned rather than appended so a resend after a crash lands
/// where the row says, not after whatever the crash left behind.
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
