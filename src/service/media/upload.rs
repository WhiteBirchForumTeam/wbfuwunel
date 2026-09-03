//! Chunked upload: chunks are the transfer unit, not the storage unit.
//!
//! `Create` declares the file once: plaintext size, plaintext chunk size,
//! chunk count, and an encrypted description the server stores opaquely and
//! hands back to downloaders. Chunks then arrive in order and are appended to
//! one staging file; a small progress row says how many have arrived and how
//! many bytes that is. The seal streams the staging file into the storage
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
//! chunk exactly as it arrived. The server does not know how long a chunk
//! will be until it sees it, and every chunk may differ, so it records where
//! each one landed (`mxc_chunk`) and judges nothing about it.
//!
//! Uploads in progress are held in memory (`uploads_hot`: the progress plus
//! the few numbers a chunk is checked against, never the encrypted
//! description) and serialized per id (`upload_locks`): a chunk is validated
//! against the in-memory copy, the bytes go to the staging file, then one
//! transaction writes the chunk's position and the progress row, and only
//! after it has executed is the in-memory copy advanced. The rows are the
//! truth after a restart; the memory is the truth while running, and never
//! runs ahead of the rows.

use std::{
	io::{self, ErrorKind},
	path::PathBuf,
};

use bytes::Bytes;
use futures::{StreamExt, stream};
use ruma::{OwnedMxcUri, OwnedUserId, UserId};
use tokio::{fs, io::AsyncReadExt};
use tuwunel_core::{
	Err, Result, debug, err, implement, info,
	utils::{self, time::now},
	warn,
};

use super::{ChunkSpan, ChunkedMedia, Dim, Upload, UploadProgress};

/// What a client declares when it starts an upload. The numbers are
/// plaintext facts the server runs the upload by; `meta` is the client's
/// encrypted description of the file (name, type, keys, whatever it likes),
/// which the server stores as it is and returns to downloaders unread.
#[derive(Debug)]
pub struct UploadRequest {
	/// Plaintext size of the whole file; 0 with `chunk_count` 0 is a stream.
	pub file_size: u64,
	/// Plaintext chunk size; `None` takes the server default. Fixed once set.
	pub chunk_size: Option<u32>,
	/// How many chunks the client will send: `ceil(file_size / chunk_size)`,
	/// or 0 for a stream.
	pub chunk_count: u32,
	/// Encrypted file description, opaque to the server.
	pub meta: Vec<u8>,
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
	pub chunk_count: u32,
	pub total_len: u64,
	pub finished: bool,
	pub truncated: bool,
	pub chunk_size: u32,
	pub file_size: u64,
}

/// What a stored chunk changed: how many are in and how many bytes.
#[derive(Clone, Copy, Debug)]
pub struct ChunkStored {
	pub received_count: u32,
	pub chunk_count: u32,
	pub total_len: u64,
	pub finished: bool,
	pub truncated: bool,
}

/// An upload in progress as the service keeps it in memory: what a chunk is
/// checked against and where the upload stands. The declaration's encrypted
/// description is not here; it is read from its row once, at the seal.
#[derive(Clone, Debug)]
pub(super) struct UploadHot {
	owner: OwnedUserId,
	mxc: OwnedMxcUri,
	chunk_size: u32,
	chunk_count: u32,
	file_size: u64,
	progress: UploadProgress,
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
	/// The chunk would have crossed `media_upload_max_len`, so the upload was
	/// ended without it: what is in stays, marked truncated, and may be
	/// sealed as an incomplete file.
	Truncated(ChunkStored),
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

/// Starts an upload: checks the declaration, mints the mxc and the upload
/// id, writes the rows. No file is touched until the first chunk.
#[implement(super::Service)]
pub async fn upload_create(&self, user: &UserId, request: UploadRequest) -> UploadResult<UploadCreated> {
	let config = &self.services.config;

	// file_size 0 and chunk_count 0 together mean a stream: the client does
	// not know the size yet and will mark the last chunk with IS_LAST.
	let is_stream = request.file_size == 0 && request.chunk_count == 0;
	if !is_stream && request.file_size == 0 {
		return Err(UploadError::Conflict("file_size must be positive (0 only with chunk_count 0, a stream)".into()));
	}
	if config.media_upload_max_len > 0 && request.file_size > config.media_upload_max_len {
		return Err(UploadError::TooLarge("file_size exceeds media_upload_max_len".into()));
	}
	if request.meta.len() > config.wbf_meta_max_bytes {
		return Err(UploadError::TooLarge(format!(
			"the encrypted description is {} bytes; at most {} are kept",
			request.meta.len(),
			config.wbf_meta_max_bytes
		)));
	}

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

	// Plaintext arithmetic the server is entitled to: the declared count must
	// be what the declared sizes give, or the client's picture of its own file
	// is wrong before a byte is sent.
	let expected_count = request.file_size.div_ceil(u64::from(chunk_size));
	if !is_stream && u64::from(request.chunk_count) != expected_count {
		return Err(UploadError::Conflict(format!(
			"chunk_count {} does not match file_size {} in chunks of {chunk_size}: expected {expected_count}",
			request.chunk_count, request.file_size
		)));
	}

	// One quota with pending uploads: both are media promised but not yet
	// delivered.
	let (pending, _) = self.db.count_pending_mxc_for_user(user).await;
	let in_progress = self.count_uploads_for_user(user).await;
	if pending.saturating_add(in_progress) >= config.max_pending_media_uploads {
		return Err(UploadError::TooLarge("maximum number of pending media uploads reached".into()));
	}

	// One unique value, two spellings: the 64-bit id goes in pack headers,
	// its hex is the mxc's media id. No table maps one to the other.
	//
	// 64 random bits make a collision negligible, but a collision would
	// overwrite someone's media, so the id is checked against uploads in
	// progress, existing media and tombstones before it is handed out.
	let (upload_id, mxc) = loop {
		let upload_id = mint_upload_id();
		let mxc: OwnedMxcUri = format!("mxc://{}/{upload_id:016x}", self.services.globals.server_name()).into();
		if self.is_upload_id_free(upload_id, &mxc).await {
			break (upload_id, mxc);
		}
		warn!(upload_id, "Minted an upload id already in use; minting another.");
	};
	let now_secs = now().as_secs();

	let upload = Upload {
		mxc: mxc.clone(),
		owner: user.to_owned(),
		chunk_size,
		chunk_count: request.chunk_count,
		file_size: request.file_size,
		meta: request.meta,
		created_at_secs: now_secs,
	};
	let progress = UploadProgress { last_chunk_at_secs: now_secs, ..UploadProgress::default() };
	self.db.create_upload(upload_id, &upload, &progress);
	self.remember_upload(upload_id, UploadHot {
		owner: upload.owner,
		mxc: upload.mxc,
		chunk_size,
		chunk_count: upload.chunk_count,
		file_size: upload.file_size,
		progress,
	});

	Ok(UploadCreated {
		upload_id,
		mxc,
		chunk_size,
		chunk_max_bytes,
		expires_at_secs: now_secs.saturating_add(config.media_upload_ttl),
	})
}

/// Appends chunk `index` of upload `upload_id`. The upload is finished when
/// chunk `chunk_count - 1` is in; `is_last` (the `IS_LAST` flag) may mark
/// that chunk and must not mark any other. A stream (`chunk_count` 0) has
/// no declared end: `is_last` is what finishes it.
///
/// Chunks are ordered: `index` must be `received_count`. A lower index was
/// already received and is acknowledged again without rewriting (a lost
/// ack); a higher one is out of order and told what was expected.
///
/// How long a chunk is on the wire is the client's business; the server
/// records where it landed and caps it, nothing more.
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
	// Chunks of one upload run one at a time: two arriving together would
	// both pass the checks against the same state and write the same offset.
	let _one_at_a_time = self.upload_locks.lock(&upload_id).await;
	let mut hot = self.owned_upload(user, upload_id).await?;

	if index < hot.progress.received_count {
		debug!(upload_id, index, "Chunk already received; acknowledging again.");
		return Ok(stored(&hot));
	}
	if index > hot.progress.received_count {
		return Err(UploadError::OutOfOrder { expected: hot.progress.received_count });
	}
	if hot.progress.finished {
		return Err(UploadError::Conflict(format!(
			"upload finished at chunk {}; nothing may follow",
			hot.progress.received_count.saturating_sub(1)
		)));
	}
	let is_stream = hot.chunk_count == 0;
	let finishes = if is_stream {
		is_last
	} else {
		if index >= hot.chunk_count {
			return Err(UploadError::Conflict(format!(
				"chunk {index} is past the declared last chunk {}",
				hot.chunk_count.saturating_sub(1)
			)));
		}
		let is_final_index = index.saturating_add(1) == hot.chunk_count;
		if is_last && !is_final_index {
			return Err(UploadError::Conflict(format!(
				"chunk {index} carries IS_LAST but the declared last chunk is {}",
				hot.chunk_count.saturating_sub(1)
			)));
		}
		is_final_index
	};

	if chunk.is_empty() {
		return Err(UploadError::Conflict("a chunk carries at least one byte".into()));
	}
	let chunk_max_bytes = (hot.chunk_size as usize).saturating_add(config.media_chunk_overhead_max);
	if chunk.len() > chunk_max_bytes {
		return Err(UploadError::TooLarge(format!(
			"chunk {index} is {} bytes; this upload's chunks are at most {chunk_max_bytes}",
			chunk.len()
		)));
	}
	let len = u32::try_from(chunk.len()).map_err(|_| UploadError::TooLarge("chunk too large".into()))?;
	let total_len = hot.progress.total_len.saturating_add(u64::from(len));
	let received_count = index.saturating_add(1);
	if crosses_upload_limit(config.media_upload_max_len, hot.chunk_size, total_len, received_count) {
		// The file ends here, short. The row says so, and stays sealable: an
		// incomplete file marked incomplete beats nothing at all.
		hot.progress.finished = true;
		hot.progress.truncated = true;
		hot.progress.last_chunk_at_secs = now().as_secs();
		self.db.put_progress(upload_id, &hot.progress);
		warn!(
			upload_id,
			index,
			total_len = hot.progress.total_len,
			"Chunked upload hit media_upload_max_len; ended truncated."
		);
		let answer = stored(&hot);
		self.remember_upload(upload_id, hot);

		return Err(UploadError::Truncated(answer));
	}

	// The file is written first: if the server dies before the transaction
	// below, the progress row still says this chunk is missing and the resend
	// lands on the same offset. The chunk's position and the advanced
	// progress then go in one transaction, and the in-memory copy follows
	// only once it executed.
	let path = self.staging_path(upload_id);
	write_at(&path, hot.progress.total_len, chunk).await?;

	let mxc_parts = hot.mxc.parts().map_err(|e| UploadError::Conflict(format!("stored mxc is invalid: {e}")))?;
	let span = ChunkSpan { offset: hot.progress.total_len, len };
	hot.progress.total_len = total_len;
	hot.progress.received_count = received_count;
	hot.progress.finished = finishes;
	hot.progress.last_chunk_at_secs = now().as_secs();
	self.db
		.put_chunk_and_progress(&mxc_parts, index, span, upload_id, &hot.progress);

	let answer = stored(&hot);
	self.remember_upload(upload_id, hot);

	Ok(answer)
}

/// Reports where an upload stands, so a client can resume from
/// `received_count`.
#[implement(super::Service)]
pub async fn upload_status(&self, user: &UserId, upload_id: u64) -> UploadResult<UploadStatus> {
	let hot = self.owned_upload(user, upload_id).await?;

	Ok(UploadStatus {
		received_count: hot.progress.received_count,
		chunk_count: hot.chunk_count,
		total_len: hot.progress.total_len,
		finished: hot.progress.finished,
		truncated: hot.progress.truncated,
		chunk_size: hot.chunk_size,
		file_size: hot.file_size,
	})
}

/// Turns a finished upload into media: the staging file becomes one object,
/// the same rows `create()` writes are written, the upload rows go.
///
/// `new_meta` replaces the encrypted description declared at `Create`: a
/// stream only knows its size at the end.
#[implement(super::Service)]
pub async fn upload_seal(
	&self,
	user: &UserId,
	upload_id: u64,
	new_meta: Option<Vec<u8>>,
) -> UploadResult<OwnedMxcUri> {
	let _one_at_a_time = self.upload_locks.lock(&upload_id).await;
	let hot = self.owned_upload(user, upload_id).await?;

	if !hot.progress.finished {
		let message = if hot.chunk_count == 0 {
			format!(
				"stream not ended: {} chunks, {} bytes, no chunk carried IS_LAST yet",
				hot.progress.received_count, hot.progress.total_len
			)
		} else {
			format!(
				"upload not finished: {} of {} chunks, {} bytes",
				hot.progress.received_count, hot.chunk_count, hot.progress.total_len
			)
		};
		return Err(UploadError::Conflict(message));
	}
	if hot.progress.received_count == 0 {
		// Reachable only when the very first chunk crossed the size limit:
		// an empty object is not a file, truncated or not.
		return Err(UploadError::Conflict("nothing was received; abort this upload instead".into()));
	}

	// The declaration is read once, here: the description in it is the only
	// part of the upload the memory does not carry.
	let declared = self
		.db
		.find_upload(upload_id)
		.await
		.ok_or(UploadError::NotFound)?;
	let meta = match new_meta {
		| Some(meta) if meta.len() > self.services.config.wbf_meta_max_bytes =>
			return Err(UploadError::TooLarge("the encrypted description is too large".into())),
		| Some(meta) => meta,
		| None => declared.meta,
	};

	let path = self.staging_path(upload_id);
	let mxc_parts = hot.mxc.parts().map_err(|e| UploadError::Conflict(format!("stored mxc is invalid: {e}")))?;

	// Same rows as a whole-file upload: metadata, uploader, and the count
	// opened at zero, so everything downstream sees ordinary media. No name
	// and no type: the server was never told what the bytes are.
	let key = self
		.db
		.create_file_metadata(&mxc_parts, Some(user), &Dim::default(), None, None)?;

	self.store_staging_file(&key, &path, hot.progress.total_len).await?;

	// The shape a downloader needs, and the client's encrypted description,
	// exactly as declared; where each chunk sits was recorded as it arrived.
	self.db.put_chunked_media(&mxc_parts, &ChunkedMedia {
		chunk_size: hot.chunk_size,
		chunk_count: hot.progress.received_count,
		file_size: hot.file_size,
		total_len: hot.progress.total_len,
		truncated: hot.progress.truncated,
		meta,
	});

	self.db.del_upload(upload_id);
	self.forget_upload(upload_id);
	if let Err(e) = fs::remove_file(&path).await {
		warn!(?path, ?e, "Sealed upload's staging file could not be removed.");
	}

	info!(
		%hot.mxc,
		upload_id,
		total_len = hot.progress.total_len,
		chunks = hot.progress.received_count,
		truncated = hot.progress.truncated,
		"Sealed chunked upload."
	);

	Ok(hot.mxc)
}

/// Drops an upload in progress: its staging file and its rows.
#[implement(super::Service)]
pub async fn upload_abort(&self, user: &UserId, upload_id: u64) -> UploadResult<()> {
	let _one_at_a_time = self.upload_locks.lock(&upload_id).await;
	let hot = self.owned_upload(user, upload_id).await?;
	self.discard_upload(upload_id, &hot.mxc).await;

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
		.list_progress()
		.filter_map(async |(id, progress)| {
			(progress.last_chunk_at_secs.saturating_add(ttl) < now_secs).then_some(id)
		})
		.collect()
		.await;

	for upload_id in expired {
		// Under the upload's lock, so a chunk arriving this instant cannot
		// re-create the in-memory copy of rows being deleted.
		let _one_at_a_time = self.upload_locks.lock(&upload_id).await;
		let Some(declared) = self.db.find_upload(upload_id).await else {
			// Progress without a declaration: half a row set, sweep it too.
			self.db.del_upload(upload_id);
			self.forget_upload(upload_id);
			continue;
		};
		info!(upload_id, "Sweeping abandoned chunked upload.");
		self.discard_upload(upload_id, &declared.mxc).await;
	}

	// Files without rows: the rows are the truth, the file is not.
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

/// Whether nothing is known under `upload_id` or its `mxc`: no upload in
/// progress, no media, no tombstone.
///
/// The media lookup is fail closed: only a definite "not found" counts as
/// free, any other error counts as taken. The upload and tombstone lookups
/// return `Option` and cannot tell an absent row from an unreadable one;
/// with 64 random bits behind the id, that gap needs a collision and a
/// database error at the same moment to matter.
#[implement(super::Service)]
async fn is_upload_id_free(&self, upload_id: u64, mxc: &OwnedMxcUri) -> bool {
	if self.db.find_upload(upload_id).await.is_some() {
		return false;
	}
	let Ok(mxc) = mxc.parts() else {
		return false;
	};
	if self.db.find_tombstone(&mxc).await.is_some() {
		return false;
	}

	match self
		.db
		.search_file_metadata(&mxc, &Dim::default())
		.await
	{
		| Err(error) => error.status_code() == http::StatusCode::NOT_FOUND,
		| Ok(_) => false,
	}
}

/// The upload, if it exists and `user` owns it. Both failures answer the
/// same, so an upload id cannot be probed.
#[implement(super::Service)]
async fn owned_upload(&self, user: &UserId, upload_id: u64) -> UploadResult<UploadHot> {
	match self.hot_upload(upload_id).await {
		| Some(hot) if hot.owner == user => Ok(hot),
		| _ => Err(UploadError::NotFound),
	}
}

/// The in-memory copy of an upload, loaded from its rows on first use.
#[implement(super::Service)]
async fn hot_upload(&self, upload_id: u64) -> Option<UploadHot> {
	if let Some(hot) = self
		.uploads_hot
		.lock()
		.expect("uploads_hot lock poisoned")
		.get(&upload_id)
	{
		return Some(hot.clone());
	}

	let declared = self.db.find_upload(upload_id).await?;
	let progress = self.db.find_progress(upload_id).await?;
	let hot = UploadHot {
		owner: declared.owner,
		mxc: declared.mxc,
		chunk_size: declared.chunk_size,
		chunk_count: declared.chunk_count,
		file_size: declared.file_size,
		progress,
	};
	self.remember_upload(upload_id, hot.clone());

	Some(hot)
}

/// Makes the in-memory copy match rows that have just been written.
#[implement(super::Service)]
fn remember_upload(&self, upload_id: u64, hot: UploadHot) {
	self.uploads_hot
		.lock()
		.expect("uploads_hot lock poisoned")
		.insert(upload_id, hot);
}

/// Drops the in-memory copy of an upload whose rows are gone.
#[implement(super::Service)]
fn forget_upload(&self, upload_id: u64) {
	self.uploads_hot
		.lock()
		.expect("uploads_hot lock poisoned")
		.remove(&upload_id);
}

#[implement(super::Service)]
async fn count_uploads_for_user(&self, user: &UserId) -> usize {
	self.db
		.list_uploads()
		.filter(|(_, upload)| futures::future::ready(upload.owner == user))
		.count()
		.await
}

/// Drops the rows, the in-memory copy, the chunk spans written so far, and
/// the staging file.
#[implement(super::Service)]
async fn discard_upload(&self, upload_id: u64, mxc: &OwnedMxcUri) {
	self.db.del_upload(upload_id);
	self.forget_upload(upload_id);
	if let Ok(mxc) = mxc.parts() {
		self.db.delete_chunk_rows(&mxc).await;
	}
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

fn stored(hot: &UploadHot) -> ChunkStored {
	ChunkStored {
		received_count: hot.progress.received_count,
		chunk_count: hot.chunk_count,
		total_len: hot.progress.total_len,
		finished: hot.progress.finished,
		truncated: hot.progress.truncated,
	}
}

/// Whether an upload that would then hold `total_len` bytes in
/// `received_count` chunks is over `max_len`. The chunk budget follows from
/// the byte budget and the declared chunk size: a 10 GiB limit with 1 MiB
/// chunks is 10240 chunks, however short the chunks actually are.
fn crosses_upload_limit(max_len: u64, chunk_size: u32, total_len: u64, received_count: u32) -> bool {
	if max_len == 0 {
		return false;
	}
	let max_chunks = max_len.div_ceil(u64::from(chunk_size.max(1)));

	total_len > max_len || u64::from(received_count) > max_chunks
}

/// A random, non-zero upload id. Zero means "no id" on the wire. Whether it
/// is free is `is_upload_id_free`'s business.
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

#[cfg(test)]
mod tests {
	use super::crosses_upload_limit;

	#[test]
	fn the_byte_limit_ends_an_upload() {
		assert!(!crosses_upload_limit(100_000, 65536, 100_000, 2), "exactly at the limit is in");
		assert!(crosses_upload_limit(100_000, 65536, 100_001, 2));
	}

	#[test]
	fn the_chunk_budget_follows_from_the_byte_limit() {
		// 40 000 bytes at 16 KiB chunks: three chunks at most, however short.
		assert!(!crosses_upload_limit(40_000, 16384, 3, 3));
		assert!(crosses_upload_limit(40_000, 16384, 4, 4), "a fourth 1-byte chunk is over budget");
		// 10 GiB at 1 MiB: 10240 chunks.
		let ten_gib = 10 * 1024 * 1024 * 1024;
		assert!(!crosses_upload_limit(ten_gib, 1024 * 1024, ten_gib, 10240));
		assert!(crosses_upload_limit(ten_gib, 1024 * 1024, ten_gib, 10241));
	}

	#[test]
	fn zero_means_no_limit() {
		assert!(!crosses_upload_limit(0, 4096, u64::MAX, u32::MAX));
	}
}
