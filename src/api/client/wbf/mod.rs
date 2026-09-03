//! The wbf pack endpoints: one pack in, one pack out.
//!
//! `POST /_wbf/v1/pack` is the HTTP transport, one pack per request.
//! The WebSocket channel calls the same `handle_pack`, so there is one set of
//! semantics. Neither transport looks inside `data`; the only meta the server
//! reads is the plaintext meta of kinds it has to act on.

use axum::{
	body::Bytes,
	extract::State,
	http::{HeaderMap, StatusCode, header},
	response::{IntoResponse, Response},
};
use ruma::{
	Mxc, UserId,
	api::error::{ErrorKind, UnknownTokenErrorData},
};
use serde_json::{Value, json};
use tuwunel_core::{
	Error, Result, debug, err,
	wbf::{EncryptedFileInfo, Flags, Kind, PackBuilder, PackError, PackView, decode},
};
use tuwunel_service::{
	Services,
	media::{UploadError, UploadRequest},
};

/// `Control` subtypes.
mod control {
	pub(super) const ACK: u8 = 0x02;
	pub(super) const ERROR: u8 = 0x03;
	pub(super) const PING: u8 = 0x04;
	pub(super) const PONG: u8 = 0x05;
}

/// `Upload` subtypes.
mod upload {
	pub(super) const CREATE: u8 = 0x01;
	pub(super) const CHUNK: u8 = 0x02;
	pub(super) const STATUS: u8 = 0x03;
	pub(super) const SEAL: u8 = 0x04;
	pub(super) const ABORT: u8 = 0x05;
}

/// `Download` subtypes.
mod download {
	pub(super) const INFO: u8 = 0x01;
	pub(super) const READ: u8 = 0x02;
}

/// # `POST /_wbf/v1/pack`
///
/// Body is one pack; response body is one pack. The access token comes as a
/// bearer header like every other client endpoint. An unauthenticated or
/// undecodable request still answers with a pack, so a client has one parser.
pub(crate) async fn pack_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Response> {
	let user = match authenticate(&services, &headers).await {
		| Ok(user) => user,
		| Err(error) => {
			let reply = error_pack(0, 0, "Unauthorized", &error.to_string());
			return Ok(pack_response(StatusCode::UNAUTHORIZED, reply));
		},
	};

	let mut body = body.to_vec();
	let reply = match decode(&mut body) {
		| Ok(view) => handle_pack(&services, &user, view).await,
		| Err(error) => {
			debug!(?error, "Rejected pack");
			// Only a data CRC failure leaves the header trustworthy (the meta
			// CRC covers it), so only then can the error answer the request.
			let (id, seq) = match error {
				| PackError::DataCrc { .. } => header_id_seq(&body),
				| _ => (0, 0),
			};
			error_pack(id, seq, pack_error_code(error), &error.to_string())
		},
	};

	Ok(pack_response(StatusCode::OK, reply))
}

/// Resolves the bearer token to a user, failing closed on anything else.
async fn authenticate(services: &Services, headers: &HeaderMap) -> Result<ruma::OwnedUserId> {
	let token = headers
		.get(header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

	let unknown_token = |soft_logout: bool, message: &'static str| {
		Error::Request(
			ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout }),
			message.into(),
			StatusCode::UNAUTHORIZED,
		)
	};

	let (user, _device, expires_at) = services
		.users
		.find_from_token(token)
		.await
		.map_err(|_| unknown_token(false, "Unknown access token."))?;

	if expires_at.is_some_and(|at| at <= std::time::SystemTime::now()) {
		return Err(unknown_token(true, "Access token expired."));
	}

	Ok(user)
}

/// Dispatches one decoded pack to its handler and returns the reply pack.
///
/// Both transports call this. `Stream` has no meaning on HTTP and no
/// implementation yet on either; it answers `Conflict`.
pub(crate) async fn handle_pack(services: &Services, user: &UserId, view: PackView<'_>) -> Vec<u8> {
	let header = view.header;
	let limits_ok = view.meta.len() <= services.config.wbf_meta_max_bytes
		&& view.data.len() <= services.config.wbf_data_max_bytes;
	if !limits_ok {
		return error_pack(header.id, header.seq, "TooLarge", "meta or data exceeds the configured limit");
	}

	let result = match (header.kind, header.subtype) {
		| (Kind::Control, control::PING) => Ok(pong(&view)),
		| (Kind::Upload, upload::CREATE) => handle_upload_create(services, user, &view).await,
		| (Kind::Upload, upload::CHUNK) => handle_upload_chunk(services, user, &view).await,
		| (Kind::Upload, upload::STATUS) => handle_upload_status(services, user, &view).await,
		| (Kind::Upload, upload::SEAL) => handle_upload_seal(services, user, &view).await,
		| (Kind::Upload, upload::ABORT) => handle_upload_abort(services, user, &view).await,
		| (Kind::Download, download::INFO) => handle_download_info(services, &view).await,
		| (Kind::Download, download::READ) => handle_download_read(services, &view).await,
		| (Kind::Stream, _) => Err(Reject::code("Conflict", "streams need the WebSocket channel")),
		| _ => Err(Reject::code("UnknownKind", "no handler for this kind and subtype")),
	};

	match result {
		| Ok(reply) => reply,
		| Err(reject) => reject.into_pack(header.id, header.seq),
	}
}

/// A refused request, in the vocabulary of the wire format's error codes.
struct Reject {
	code: &'static str,
	message: String,
	extra: Value,
}

impl Reject {
	fn code(code: &'static str, message: impl Into<String>) -> Self {
		Self { code, message: message.into(), extra: Value::Null }
	}

	fn into_pack(self, id: u64, seq: u32) -> Vec<u8> {
		let mut meta = json!({ "code": self.code, "message": self.message });
		if let (Value::Object(target), Value::Object(extra)) = (&mut meta, self.extra) {
			target.extend(extra);
		}

		PackBuilder::new(Kind::Control, control::ERROR, Flags::IS_RESPONSE, id, seq)
			.json_meta(&meta)
			.map(PackBuilder::finish)
			.unwrap_or_else(|_| error_pack(id, seq, "Internal", "could not encode the error"))
	}
}

impl From<UploadError> for Reject {
	fn from(error: UploadError) -> Self {
		match error {
			| UploadError::NotFound => Self::code("NotFound", "no such upload"),
			| UploadError::Conflict(message) => Self::code("Conflict", message),
			| UploadError::TooLarge(message) => Self::code("TooLarge", message),
			| UploadError::Truncated(stored) => Self {
				code: "Truncated",
				message: format!(
					"upload hit the size limit after {} chunks, {} bytes; it is finished as incomplete and may be sealed",
					stored.received_count, stored.total_len
				),
				extra: json!({
					"received": stored.received_count,
					"total_len": stored.total_len,
					"finished": stored.finished,
					"truncated": stored.truncated,
				}),
			},
			| UploadError::OutOfOrder { expected } => Self {
				code: "OutOfOrder",
				message: format!("expected chunk {expected}"),
				extra: json!({ "expected_seq": expected }),
			},
			| UploadError::Internal(error) => Self::code("Internal", error.to_string()),
		}
	}
}

impl From<Error> for Reject {
	fn from(error: Error) -> Self {
		let code = match error.status_code() {
			| StatusCode::NOT_FOUND | StatusCode::GONE => "NotFound",
			| StatusCode::BAD_REQUEST => "Conflict",
			| StatusCode::PAYLOAD_TOO_LARGE => "TooLarge",
			| _ => "Internal",
		};

		Self::code(code, error.to_string())
	}
}

impl From<PackError> for Reject {
	fn from(error: PackError) -> Self { Self::code(pack_error_code(error), error.to_string()) }
}

async fn handle_upload_create(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	// meta is the 16-byte EncryptedFileInfo, not JSON: plaintext facts only.
	let info = EncryptedFileInfo::decode(view.meta).map_err(|e| Reject::code("Conflict", e.to_string()))?;
	let request = UploadRequest {
		file_size: info.file_size,
		chunk_size: (info.chunk_size != 0).then_some(info.chunk_size),
		chunk_count: info.chunk_count,
		// The client's encrypted description of the file: stored and returned
		// as it is, never read.
		meta: view.data.to_vec(),
	};

	let created = services.media.upload_create(user, request).await?;

	Ok(ack(
		created.upload_id,
		view.header.seq,
		json!({
			"id": created.upload_id,
			"mxc": created.mxc,
			"chunk_size": created.chunk_size,
			"chunk_max_bytes": created.chunk_max_bytes,
			"expires_at": created.expires_at_secs,
		}),
		Vec::new(),
	))
}

async fn handle_upload_chunk(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let stored = services
		.media
		.upload_chunk(user, view.header.id, view.header.seq, view.data, view.header.flags.is_last())
		.await?;

	Ok(ack(
		view.header.id,
		view.header.seq,
		json!({
			"received": stored.received_count,
			"chunk_count": known(stored.chunk_count),
			"total_len": stored.total_len,
			"finished": stored.finished,
			"truncated": stored.truncated,
		}),
		Vec::new(),
	))
}

async fn handle_upload_status(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let status = services.media.upload_status(user, view.header.id).await?;

	Ok(ack(
		view.header.id,
		view.header.seq,
		json!({
			"received": status.received_count,
			"chunk_count": known(status.chunk_count),
			"total_len": status.total_len,
			"finished": status.finished,
			"truncated": status.truncated,
			"chunk_size": status.chunk_size,
			"file_size": known(status.file_size),
		}),
		Vec::new(),
	))
}

async fn handle_upload_seal(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	// A new encrypted description may ride along: a stream learns its size
	// only at the end.
	let new_meta = (!view.data.is_empty()).then(|| view.data.to_vec());
	let mxc = services
		.media
		.upload_seal(user, view.header.id, new_meta)
		.await?;

	Ok(ack(view.header.id, view.header.seq, json!({ "mxc": mxc }), Vec::new()))
}

async fn handle_upload_abort(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	services.media.upload_abort(user, view.header.id).await?;

	Ok(ack(view.header.id, view.header.seq, json!({ "ok": true }), Vec::new()))
}

async fn handle_download_info(services: &Services, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let meta = view.meta_json()?;
	let mxc = mxc_from_meta(&meta)?;
	let info = services.media.media_info(&mxc.as_str().try_into().map_err(|_| Reject::code("Conflict", "invalid mxc"))?).await?;

	Ok(ack(
		0,
		view.header.seq,
		json!({
			"total_len": info.total_len,
			"content_type": info.content_type,
			"file_size": info.chunked.as_ref().and_then(|chunked| known(chunked.file_size)),
			"chunk_size": info.chunked.as_ref().map(|chunked| chunked.chunk_size),
			"chunk_count": info.chunked.as_ref().map(|chunked| chunked.chunk_count),
			"truncated": info.chunked.as_ref().map(|chunked| chunked.truncated),
			"read_len": services.config.media_download_default_len,
			"chunk_size_large": services.config.media_chunk_size_large,
		}),
		// The uploader's encrypted description, exactly as it was declared.
		info.chunked.map(|chunked| chunked.meta).unwrap_or_default(),
	))
}

/// Chunked media comes back one whole chunk at a time, exactly as uploaded:
/// by `chunk` index, or by `pos`, a plaintext position, which picks the
/// chunk holding it (`pos / chunk_size`). Whole-file media is read by `pos`
/// and `len` in the object's own bytes.
async fn handle_download_read(services: &Services, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let meta = view.meta_json()?;
	let mxc = mxc_from_meta(&meta)?;
	let mxc: Mxc<'_> = mxc.as_str().try_into().map_err(|_| Reject::code("Conflict", "invalid mxc"))?;
	let pos = meta["pos"].as_u64().unwrap_or(0);

	if let Some(chunked) = services.media.chunked_shape(&mxc).await {
		let index = match meta["chunk"].as_u64() {
			| Some(index) => u32::try_from(index).map_err(|_| Reject::code("Conflict", "chunk index too large"))?,
			| None => u32::try_from(pos / u64::from(chunked.chunk_size.max(1)))
				.map_err(|_| Reject::code("Conflict", "position too large"))?,
		};
		let read = services.media.read_chunk(&mxc, index).await?;

		return Ok(ack(
			0,
			view.header.seq,
			json!({
				"chunk": read.index,
				"pos": read.plain_pos,
				"len": read.bytes.len(),
				"chunk_size": read.chunk_size,
				"chunk_count": read.chunk_count,
				"total_len": read.total_len,
			}),
			read.bytes.to_vec(),
		));
	}

	let len = meta["len"]
		.as_u64()
		.unwrap_or(services.config.media_download_default_len as u64)
		.min(services.config.wbf_data_max_bytes as u64);

	let read = services.media.read_range(&mxc, pos, len).await?;

	Ok(ack(
		0,
		view.header.seq,
		json!({ "pos": read.pos, "len": read.bytes.len(), "total_len": read.total_len }),
		read.bytes.to_vec(),
	))
}

/// A declared size or count, or `None` when the client declared none (a
/// stream): `0` is the sentinel on the wire, `null` in the answer.
fn known<T: Into<u64> + Copy>(value: T) -> Option<T> { (value.into() != 0).then_some(value) }

fn mxc_from_meta(meta: &Value) -> std::result::Result<String, Reject> {
	meta["mxc"]
		.as_str()
		.map(ToOwned::to_owned)
		.ok_or_else(|| Reject::code("Conflict", "mxc is required"))
}

/// A `Pong` echoing the ping's meta (its nonce) back.
fn pong(view: &PackView<'_>) -> Vec<u8> {
	PackBuilder::new(Kind::Control, control::PONG, Flags::IS_RESPONSE, view.header.id, view.header.seq)
		.meta(view.meta)
		.map(PackBuilder::finish)
		.unwrap_or_else(|_| error_pack(view.header.id, view.header.seq, "Internal", "could not encode pong"))
}

/// An `Ack` answering `(id, seq)` with `meta` and, for reads, `data`.
fn ack(id: u64, seq: u32, meta: Value, data: Vec<u8>) -> Vec<u8> {
	PackBuilder::new(Kind::Control, control::ACK, Flags::IS_RESPONSE, id, seq)
		.json_meta(&meta)
		.and_then(|builder| builder.data(&data))
		.map(PackBuilder::finish)
		.unwrap_or_else(|_| error_pack(id, seq, "Internal", "could not encode the reply"))
}

fn error_pack(id: u64, seq: u32, code: &str, message: &str) -> Vec<u8> {
	PackBuilder::new(Kind::Control, control::ERROR, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({ "code": code, "message": message }))
		.map(PackBuilder::finish)
		.unwrap_or_else(|_| PackBuilder::new(Kind::Control, control::ERROR, Flags::IS_RESPONSE, id, seq).finish())
}

/// The `id` and `seq` of a pack whose header has been checksummed but whose
/// data has not: fixed offsets, nothing else read.
fn header_id_seq(bytes: &[u8]) -> (u64, u32) {
	if bytes.len() < 16 {
		return (0, 0);
	}
	let id = u64::from_be_bytes(bytes[4..12].try_into().expect("8 bytes"));
	let seq = u32::from_be_bytes(bytes[12..16].try_into().expect("4 bytes"));

	(id, seq)
}

fn pack_error_code(error: PackError) -> &'static str {
	match error {
		| PackError::UnsupportedVersion(_) => "UnsupportedVersion",
		| PackError::UnknownKind(_) => "UnknownKind",
		| PackError::SectionTooLarge { .. } => "TooLarge",
		| _ => "Corrupt",
	}
}

fn pack_response(status: StatusCode, pack: Vec<u8>) -> Response {
	(status, [(header::CONTENT_TYPE, "application/octet-stream")], pack).into_response()
}

#[cfg(test)]
mod tests {
	use tuwunel_core::wbf::{Kind, decode};

	use super::{Reject, control};

	#[test]
	fn a_reject_becomes_an_error_pack_answering_the_request() {
		let mut pack = Reject::code("NotFound", "no such upload").into_pack(42, 7);
		let view = decode(&mut pack).expect("error pack decodes");

		assert_eq!(view.header.kind, Kind::Control);
		assert_eq!(view.header.subtype, control::ERROR);
		assert!(view.header.flags.is_response());
		assert_eq!(view.header.id, 42);
		assert_eq!(view.header.seq, 7);
		assert_eq!(view.meta_json().expect("json")["code"], "NotFound");
	}

	#[test]
	fn out_of_order_carries_the_expected_seq() {
		let reject: Reject = tuwunel_service::media::UploadError::OutOfOrder { expected: 12 }.into();
		let mut pack = reject.into_pack(1, 30);
		let view = decode(&mut pack).expect("decodes");

		assert_eq!(view.meta_json().expect("json")["expected_seq"], 12);
	}
}
