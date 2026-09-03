use std::sync::Arc;

use futures::{Stream, StreamExt, pin_mut};
use ruma::{Mxc, OwnedMxcUri, OwnedUserId, UserId, http_headers::ContentDisposition};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Error, Result, at, debug, debug_info, err,
	utils::{
		ReadyExt, str_from_bytes,
		stream::{TryExpect, TryIgnore},
		string_from_bytes,
	},
};
use tuwunel_database::{
	Cbor, CounterOperand, Database, Deserialized, Ignore, Interfix, Map, Txn, serialize_key,
};

use super::{Media, preview::CachedPreview, thumbnail::Dim};

pub(crate) struct Data {
	db: Arc<Database>,
	mediaid_file: Arc<Map>,
	mediaid_lazy: Arc<Map>,
	mediaid_lazycontent: Arc<Map>,
	mediaid_pending: Arc<Map>,
	mediaid_user: Arc<Map>,
	mxc_refcount: Arc<Map>,
	mxc_tombstone: Arc<Map>,
	mediaid_upload: Arc<Map>,
	mxc_chunk: Arc<Map>,
	mxc_chunked: Arc<Map>,
	url_preview: Arc<Map>,
}

/// One chunked upload in progress, keyed by its upload id.
///
/// Chunks arrive in order and are appended, so what has arrived is a count
/// and a byte total: chunk `received_count` is the next one expected and
/// `total_len` is where it will land. The client says which chunk is the
/// last; until then the upload has no known size.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Upload {
	pub mxc: OwnedMxcUri,
	pub owner: OwnedUserId,
	/// Plaintext chunk size the client declared; fixed for the upload. What a
	/// chunk is on the wire is the client's business: the server records each
	/// one's length as it arrived and caps it at `chunk_size` plus
	/// `media_chunk_overhead_max`.
	pub chunk_size: u32,
	/// How many chunks the client declared it will send.
	pub chunk_count: u32,
	/// Plaintext size of the whole file, as declared.
	pub file_size: u64,
	/// The client's encrypted description of the file, stored as it came.
	#[serde(with = "serde_bytes")]
	pub meta: Vec<u8>,
	/// Bytes received so far: the staging file's length and the next chunk's
	/// offset.
	pub total_len: u64,
	/// Chunks received so far: the next chunk's index.
	pub received_count: u32,
	/// Whether the last chunk has arrived. Only a finished upload seals.
	pub finished: bool,
	/// Whether the server ended the upload itself because the next chunk
	/// would have crossed `media_upload_max_len`: finished, but not whole.
	pub truncated: bool,
	pub created_at_secs: u64,
	pub last_chunk_at_secs: u64,
}


/// Where one chunk of chunked media sits in the stored object, as it
/// arrived. The server cannot re-chunk ciphertext and does not know how
/// long a chunk is until it sees it, so a download needs this to hand chunk
/// `i` back exactly as uploaded.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ChunkSpan {
	pub offset: u64,
	pub len: u32,
}

/// The shape of sealed chunked media: what the server knows, which is the
/// plaintext sizes the uploader declared, how many chunks came, how many
/// wire bytes they add up to, and the uploader's encrypted description of
/// the file. Plaintext position `p` lives in chunk `p / chunk_size`; the
/// last chunk's plaintext may be shorter.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChunkedMedia {
	pub chunk_size: u32,
	pub chunk_count: u32,
	pub file_size: u64,
	pub total_len: u64,
	/// Encrypted by the uploader; handed back by `Info`, never read here.
	#[serde(with = "serde_bytes")]
	pub meta: Vec<u8>,
	/// The upload was cut off at the size limit; what is stored is the
	/// beginning of the file, not all of it.
	pub truncated: bool,
}

/// Why media was removed. Stored in the tombstone so an operator reading it
/// back can tell a collection from a rebuild from a manual delete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TombstoneReason {
	/// The collector saw its reference count reach zero.
	GarbageCollected,
	/// `migrate-references` found it referenced by nothing.
	Migrated,
	/// An administrator deleted it by MXC.
	AdminDeleted,
}

/// The record left behind when media is removed, so a later fetch answers
/// "gone" rather than "never existed".
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Tombstone {
	pub deleted_at_secs: u64,
	pub reason: TombstoneReason,
}

/// The error a fetch of removed media gets: 410 with the standard not-found
/// code, since clients only know the standard codes. Built directly because
/// the `Err!` macro's status hint is always 400.
pub(super) fn gone(mxc: &Mxc<'_>) -> Error {
	Error::Request(
		ruma::api::error::ErrorKind::NotFound,
		format!("Media {mxc} has been deleted.").into(),
		StatusCode::GONE,
	)
}

#[derive(Debug)]
pub struct Metadata {
	pub content_disposition: Option<ContentDisposition>,
	pub content_type: Option<String>,
	pub(super) key: Vec<u8>,
}

/// Borrowed staging-cache value: written zero-copy from the measured bytes.
#[cfg(feature = "url_preview")]
#[derive(Serialize)]
struct LazyContentRef<'a> {
	content_type: Option<&'a str>,
	content_disposition: Option<&'a str>,
	#[serde(with = "serde_bytes")]
	content: &'a [u8],
}

/// Owned staging-cache value read back at promotion. `ContentDisposition` is
/// Serialize-only, so the disposition rides as its header string.
#[derive(Deserialize)]
struct LazyContent {
	content_type: Option<String>,
	content_disposition: Option<String>,
	#[serde(with = "serde_bytes")]
	content: Vec<u8>,
}

impl From<LazyContent> for Media {
	fn from(lazy: LazyContent) -> Self {
		let content_disposition = lazy
			.content_disposition
			.and_then(|disposition| disposition.parse().ok());

		Self {
			content: lazy.content,
			content_type: lazy.content_type,
			content_disposition,
		}
	}
}

impl Data {
	pub(super) fn new(db: &Arc<Database>) -> Self {
		Self {
			db: db.clone(),
			mediaid_file: db["mediaid_file"].clone(),
			mediaid_lazy: db["mediaid_lazy"].clone(),
			mediaid_lazycontent: db["mediaid_lazycontent"].clone(),
			mediaid_pending: db["mediaid_pending"].clone(),
			mediaid_user: db["mediaid_user"].clone(),
			mxc_refcount: db["mxc_refcount"].clone(),
			mxc_tombstone: db["mxc_tombstone"].clone(),
			mediaid_upload: db["mediaid_upload"].clone(),
			mxc_chunk: db["mxc_chunk"].clone(),
			mxc_chunked: db["mxc_chunked"].clone(),
			url_preview: db["url_preview"].clone(),
		}
	}

	/// Reads the upload with `upload_id`, if any.
	pub(super) async fn find_upload(&self, upload_id: u64) -> Option<Upload> {
		self.mediaid_upload
			.qry(&upload_id)
			.await
			.deserialized::<Cbor<Upload>>()
			.map(|Cbor(upload)| upload)
			.ok()
	}

	/// Writes the upload with `upload_id`, replacing what was there.
	pub(super) fn put_upload(&self, upload_id: u64, upload: &Upload) {
		self.mediaid_upload.put(upload_id, Cbor(upload));
	}

	/// Removes the upload row with `upload_id`.
	pub(super) fn del_upload(&self, upload_id: u64) { self.mediaid_upload.del(upload_id); }

	/// Every upload in progress, with its id.
	pub(super) fn list_uploads(&self) -> impl Stream<Item = (u64, Upload)> + Send + '_ {
		self.mediaid_upload
			.stream::<u64, Cbor<Upload>>()
			.ignore_err()
			.map(|(id, Cbor(upload))| (id, upload))
	}

	/// Records where chunk `index` of `mxc` landed.
	pub(super) fn put_chunk_span(&self, mxc: &Mxc<'_>, index: u32, span: ChunkSpan) {
		self.mxc_chunk.put((mxc, index), Cbor(span));
	}

	/// Where chunk `index` of `mxc` sits, if it arrived.
	pub(super) async fn find_chunk_span(&self, mxc: &Mxc<'_>, index: u32) -> Option<ChunkSpan> {
		self.mxc_chunk
			.qry(&(mxc, index))
			.await
			.deserialized::<Cbor<ChunkSpan>>()
			.map(|Cbor(span)| span)
			.ok()
	}

	/// Records that `mxc` is chunked media with this shape.
	pub(super) fn put_chunked_media(&self, mxc: &Mxc<'_>, chunked: &ChunkedMedia) {
		self.mxc_chunked.put(mxc.to_string(), Cbor(chunked));
	}

	/// The shape of `mxc` if it is chunked media; `None` for a whole-file
	/// upload.
	pub(super) async fn find_chunked_media(&self, mxc: &Mxc<'_>) -> Option<ChunkedMedia> {
		self.mxc_chunked
			.get(&mxc.to_string())
			.await
			.deserialized::<Cbor<ChunkedMedia>>()
			.map(|Cbor(chunked)| chunked)
			.ok()
	}

	/// Forgets every chunk span of `mxc` and its chunk shape: for an upload
	/// that was abandoned, or media that was deleted.
	pub(super) async fn delete_chunk_rows(&self, mxc: &Mxc<'_>) {
		let prefix = (mxc, Interfix);
		let mut txn = self
			.mxc_chunk
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_fold(self.db.txn(), |mut txn, key| {
				txn.del_raw(&self.mxc_chunk, key);

				txn
			})
			.await;

		txn.del(&self.mxc_chunked, mxc.to_string());
		txn.execute();
	}

	/// Reads the tombstone left when `mxc` was removed, if any.
	pub(super) async fn find_tombstone(&self, mxc: &Mxc<'_>) -> Option<Tombstone> {
		self.mxc_tombstone
			.get(&mxc.to_string())
			.await
			.deserialized::<Cbor<Tombstone>>()
			.map(|Cbor(tombstone)| tombstone)
			.ok()
	}

	/// Queues, in `txn`, the tombstone for `mxc` and the removal of its
	/// reference count row. Both land with the deletion they describe.
	pub(super) fn write_tombstone(
		&self,
		txn: &mut Txn,
		mxc: &Mxc<'_>,
		tombstone: &Tombstone,
	) {
		let key = mxc.to_string();
		txn.put(&self.mxc_tombstone, &key, Cbor(tombstone));
		txn.del(&self.mxc_refcount, &key);
	}

	pub(super) fn create_file_metadata(
		&self,
		mxc: &Mxc<'_>,
		user: Option<&UserId>,
		dim: &Dim,
		content_disposition: Option<&ContentDisposition>,
		content_type: Option<&str>,
	) -> Result<Vec<u8>> {
		let dim: &[u32] = &[dim.width, dim.height];
		let key = (mxc, dim, content_disposition, content_type);
		let key = serialize_key(key)?;
		let mut txn = self.db.txn();

		txn.insert_raw(&self.mediaid_file, &key, []);

		// Opens the reference count at zero in the same batch that creates the
		// media, so media without a count row is exactly media that predates
		// the counter. Init leaves an existing row alone, so a thumbnail made
		// later for already counted media cannot reset it.
		txn.merge(&self.mxc_refcount, mxc.to_string(), CounterOperand::Init.to_bytes());

		if let Some(user) = user {
			let key = (mxc, user);

			txn.put_raw(&self.mediaid_user, key, user);
		}

		txn.execute();

		Ok(key.to_vec())
	}

	/// Insert a pending MXC URI into the database
	pub(super) fn insert_pending_mxc(
		&self,
		mxc: &Mxc<'_>,
		user: &UserId,
		unused_expires_at: u64,
	) {
		let value = (unused_expires_at, user);
		debug!(?mxc, ?user, ?unused_expires_at, "Inserting pending");

		self.mediaid_pending
			.raw_put(mxc.to_string(), value);
	}

	/// Remove a pending MXC URI from the database
	pub(super) fn remove_pending_mxc(&self, mxc: &Mxc<'_>) {
		self.mediaid_pending.remove(&mxc.to_string());
	}

	/// Count the number of pending MXC URIs for a specific user
	pub(super) async fn count_pending_mxc_for_user(&self, user_id: &UserId) -> (usize, u64) {
		type KeyVal<'a> = (Ignore, (u64, &'a UserId));

		self.mediaid_pending
			.stream()
			.expect_ok()
			.ready_filter(|(_, (_, pending_user_id)): &KeyVal<'_>| user_id == *pending_user_id)
			.ready_fold(
				(0_usize, u64::MAX),
				|(count, earliest_expiration), (_, (expires_at, _))| {
					(count.saturating_add(1), earliest_expiration.min(expires_at))
				},
			)
			.await
	}

	/// Search for a pending MXC URI in the database
	pub(super) async fn search_pending_mxc(&self, mxc: &Mxc<'_>) -> Result<(OwnedUserId, u64)> {
		type Value<'a> = (u64, OwnedUserId);

		self.mediaid_pending
			.get(&mxc.to_string())
			.await
			.deserialized()
			.map(|(expires_at, user_id): Value<'_>| (user_id, expires_at))
			.inspect(|(user_id, expires_at)| debug!(?mxc, ?user_id, ?expires_at, "Found pending"))
			.map_err(|e| err!(Request(NotFound("Pending not found or error: {e}"))))
	}

	/// Map a minted mxc:// URI to the external URL it resolves to on first
	/// download (see `Service::fetch_lazy_media`).
	#[cfg(feature = "url_preview")]
	pub(super) fn insert_lazy_media(&self, mxc: &str, url: &str) {
		debug!(?mxc, ?url, "Registering lazy media");

		self.mediaid_lazy.insert(mxc, url.as_bytes());
	}

	#[cfg(feature = "url_preview")]
	pub(super) fn queue_lazy_media(&self, txn: &mut Txn, mxc: &str, url: &str) {
		debug!(?mxc, ?url, "Registering lazy media");

		txn.insert_raw(&self.mediaid_lazy, mxc, url.as_bytes());
	}

	/// Remove a lazy media reference by its mxc:// URI string, unregistering
	/// the mxc.
	pub(super) fn remove_lazy_media(&self, txn: &mut Txn, mxc: &str) {
		txn.del_raw(&self.mediaid_lazy, mxc);
	}

	/// Look up the external URL a lazy media MXC URI refers to.
	pub(super) async fn search_lazy_media(&self, mxc: &Mxc<'_>) -> Result<String> {
		let handle = self.mediaid_lazy.get(&mxc.to_string()).await?;

		string_from_bytes(&handle)
			.map_err(|e| err!(Database(error!(?mxc, "Lazy media URL is invalid: {e}"))))
	}

	/// Stage the measured preview media bytes under its minted mxc so the
	/// first client download can promote without touching the origin.
	#[cfg(feature = "url_preview")]
	pub(super) fn set_lazy_content(
		&self,
		txn: &mut Txn,
		mxc: &str,
		content_type: Option<&str>,
		content_disposition: Option<&str>,
		content: &[u8],
	) {
		let value = LazyContentRef {
			content_type,
			content_disposition,
			content,
		};

		txn.raw_put(&self.mediaid_lazycontent, mxc, Cbor(&value));
	}

	/// Take the staged bytes a preview seeded for a lazy media mxc, if any.
	pub(super) async fn get_lazy_content(&self, mxc: &str) -> Result<Media> {
		self.mediaid_lazycontent
			.get(mxc)
			.await
			.deserialized::<Cbor<LazyContent>>()
			.map(at!(0))
			.map(Into::into)
	}

	pub(super) fn remove_lazy_content(&self, txn: &mut Txn, mxc: &str) {
		txn.del_raw(&self.mediaid_lazycontent, mxc);
	}

	pub(super) async fn delete_file_mxc(&self, mxc: &Mxc<'_>) {
		debug!("MXC URI: {mxc}");

		let prefix = (mxc, Interfix);
		let txn = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_fold(self.db.txn(), |mut txn, key| {
				txn.del_raw(&self.mediaid_file, key);

				txn
			})
			.await;

		let txn = self
			.mediaid_user
			.stream_prefix_raw(&prefix)
			.ignore_err()
			.ready_fold(txn, |mut txn, (key, val)| {
				debug_assert!(
					key.starts_with(mxc.to_string().as_bytes()),
					"key should start with the mxc"
				);

				let user = str_from_bytes(val).unwrap_or_default();
				debug_info!("Deleting key {key:?} which was uploaded by user {user}");

				txn.del_raw(&self.mediaid_user, key);

				txn
			})
			.await;

		txn.execute();

		self.delete_chunk_rows(mxc).await;
	}

	/// Searches for all files with the given MXC
	pub(super) async fn search_mxc_metadata_prefix(&self, mxc: &Mxc<'_>) -> Result<Vec<Vec<u8>>> {
		debug!("MXC URI: {mxc}");

		let prefix = (mxc, Interfix);
		let keys: Vec<Vec<u8>> = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.map(<[u8]>::to_vec)
			.collect()
			.await;

		if keys.is_empty() {
			return Err!(Database("Failed to find any keys in database for `{mxc}`",));
		}

		debug!("Got the following keys: {keys:?}");

		Ok(keys)
	}

	pub(super) async fn file_metadata_exists(&self, mxc: &Mxc<'_>, dim: &Dim) -> bool {
		let dim: &[u32] = &[dim.width, dim.height];
		let prefix = (mxc, dim, Interfix);
		let keys = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err();

		pin_mut!(keys);
		keys.next().await.is_some()
	}

	pub(super) async fn search_file_metadata(
		&self,
		mxc: &Mxc<'_>,
		dim: &Dim,
	) -> Result<Metadata> {
		// Removed media answers "gone" here, at the one lookup every fetch of
		// content or a thumbnail goes through, rather than "not found".
		if self.find_tombstone(mxc).await.is_some() {
			return Err(gone(mxc));
		}

		let dim: &[u32] = &[dim.width, dim.height];
		let prefix = (mxc, dim, Interfix);

		let keys = self
			.mediaid_file
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.map(ToOwned::to_owned);

		pin_mut!(keys);
		let key = keys
			.next()
			.await
			.ok_or_else(|| err!(Request(NotFound("Media not found"))))?;

		let mut parts = key.rsplit(|&b| b == 0xFF);

		let content_type = parts
			.next()
			.map(string_from_bytes)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-type is invalid: {e}"))))?;

		let content_disposition = parts
			.next()
			.map(Some)
			.ok_or_else(|| err!(Database(error!(?mxc, "Media ID in db is invalid."))))?
			.filter(|bytes| !bytes.is_empty())
			.map(string_from_bytes)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-disposition is invalid: {e}"))))?
			.as_deref()
			.map(str::parse)
			.transpose()
			.map_err(|e| err!(Database(error!(?mxc, "Content-disposition is invalid: {e}"))))?;

		Ok(Metadata { content_disposition, content_type, key })
	}

	/// Uploading local user of the media at the given MXC, from the uploader
	/// index.
	pub(super) async fn mxc_user(&self, mxc: &Mxc<'_>) -> Option<OwnedUserId> {
		let prefix = (mxc, Interfix);
		let users = self
			.mediaid_user
			.stream_prefix(&prefix)
			.ignore_err()
			.map(|(_, user): (Ignore, &UserId)| user.to_owned());

		pin_mut!(users);
		users.next().await
	}

	/// Gets all the MXCs associated with a user
	pub(super) async fn get_all_user_mxcs(&self, user_id: &UserId) -> Vec<OwnedMxcUri> {
		self.mediaid_user
			.stream()
			.ignore_err()
			.ready_filter_map(|((key, _), user): ((&str, Ignore), &UserId)| {
				(user == user_id).then(|| key.into())
			})
			.collect()
			.await
	}

	/// Gets all the media keys in our database (this includes all the metadata
	/// associated with it such as width, height, content-type, etc)
	pub(crate) async fn get_all_media_keys(&self) -> Vec<Vec<u8>> {
		self.mediaid_file
			.raw_keys()
			.ignore_err()
			.map(<[u8]>::to_vec)
			.collect()
			.await
	}

	pub(super) fn set_url_preview(&self, url: &str, cached: &CachedPreview) -> Result {
		self.url_preview.raw_put(url, Cbor(cached));

		Ok(())
	}

	pub(super) async fn get_url_preview(&self, url: &str) -> Result<CachedPreview> {
		self.url_preview
			.get(url)
			.await
			.deserialized::<Cbor<_>>()
			.map(at!(0))
			.ok()
			.filter(CachedPreview::valid)
			.ok_or(err!(Request(NotFound("Expired from cache"))))
	}

	/// Streams every (mxc, uploader) pair in the user-media index.
	pub(super) fn all_uploads(
		&self,
	) -> impl Stream<Item = (OwnedMxcUri, OwnedUserId)> + Send + '_ {
		self.mediaid_user
			.keys()
			.ignore_err()
			.map(|(mxc, user): (&str, &UserId)| (mxc.into(), user.to_owned()))
	}
}

#[cfg(feature = "url_preview")]
#[cfg(test)]
mod tests {
	use minicbor_serde::{from_slice, to_vec};

	use super::{LazyContent, LazyContentRef, Media};

	#[test]
	fn lazy_content_roundtrip() {
		let content: &[u8] = b"\x00\x01\xFF\xFE arbitrary staged bytes";
		let value = LazyContentRef {
			content_type: Some("image/png"),
			content_disposition: Some("inline; filename=\"cat.png\""),
			content,
		};

		let bytes = to_vec(&value).expect("encodes");
		let decoded: LazyContent = from_slice(&bytes).expect("decodes");

		assert_eq!(decoded.content_type.as_deref(), Some("image/png"));
		assert_eq!(decoded.content.as_slice(), content);

		let media = Media::from(decoded);
		assert_eq!(media.content.as_slice(), content);
		assert!(media.content_disposition.is_some(), "disposition re-parses to the ruma type");
	}

	#[test]
	fn lazy_content_bytes_compact() {
		let content = vec![0xAB_u8; 4096];
		let value = LazyContentRef {
			content_type: None,
			content_disposition: None,
			content: content.as_slice(),
		};

		let bytes = to_vec(&value).expect("encodes");

		// serde_bytes must encode a CBOR byte string, not an array-of-uints
		// (~1.9x); only a small fixed header of overhead is permitted
		assert!(bytes.len() <= content.len() + 64);
	}
}
