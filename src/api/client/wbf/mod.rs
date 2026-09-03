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
	wbf::{Flags, Kind, PackBuilder, PackError, PackView, decode},
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
			let reply = error_pack(Kind::Control, 0, 0, "Unauthorized", &error.to_string());
			return Ok(pack_response(StatusCode::UNAUTHORIZED, reply));
		},
	};

	let mut body = body.to_vec();
	let reply = match decode(&mut body) {
		| Ok(view) => handle_pack(&services, &user, view).await,
		| Err(error) => {
			debug!(?error, "Rejected pack");
			error_pack(Kind::Control, 0, 0, pack_error_code(error), &error.to_string())
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
		return error_pack(header.kind, header.id, header.seq, "TooLarge", "meta or data exceeds the configured limit");
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
		| Err(reject) => reject.into_pack(header.kind, header.id, header.seq),
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

	fn into_pack(self, kind: Kind, id: u64, seq: u32) -> Vec<u8> {
		let mut meta = json!({ "code": self.code, "message": self.message });
		if let (Value::Object(target), Value::Object(extra)) = (&mut meta, self.extra) {
			target.extend(extra);
		}

		PackBuilder::new(Kind::Control, control::ERROR, Flags::IS_RESPONSE, id, seq)
			.json_meta(&meta)
			.map(PackBuilder::finish)
			.unwrap_or_else(|_| error_pack(kind, id, seq, "Internal", "could not encode the error"))
	}
}

impl From<UploadError> for Reject {
	fn from(error: UploadError) -> Self {
		match error {
			| UploadError::NotFound => Self::code("NotFound", "no such upload"),
			| UploadError::Conflict(message) => Self::code("Conflict", message),
			| UploadError::TooLarge(message) => Self::code("TooLarge", message),
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
	let meta = view.meta_json()?;
	let request = UploadRequest {
		total_len: meta["total_len"]
			.as_u64()
			.ok_or_else(|| Reject::code("Conflict", "total_len is required"))?,
		chunk_size: meta["chunk_size"]
			.as_u64()
			.map(|size| u32::try_from(size).map_err(|_| Reject::code("Conflict", "chunk_size too large")))
			.transpose()?,
		content_type: meta["content_type"].as_str().map(ToOwned::to_owned),
		filename: meta["filename"].as_str().map(ToOwned::to_owned),
	};

	let created = services.media.upload_create(user, request).await?;

	Ok(ack(
		Kind::Upload,
		created.upload_id,
		view.header.seq,
		json!({
			"id": created.upload_id,
			"mxc": created.mxc,
			"chunk_size": created.chunk_size,
			"chunk_count": created.chunk_count,
			"expires_at": created.expires_at_secs,
		}),
		Vec::new(),
	))
}

async fn handle_upload_chunk(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let received = services
		.media
		.upload_chunk(user, view.header.id, view.header.seq, view.data)
		.await?;

	Ok(ack(Kind::Upload, view.header.id, view.header.seq, json!({ "received": received }), Vec::new()))
}

async fn handle_upload_status(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let status = services.media.upload_status(user, view.header.id).await?;

	Ok(ack(
		Kind::Upload,
		view.header.id,
		view.header.seq,
		json!({
			"received": status.received,
			"missing": status.missing,
			"received_count": status.received_count,
			"chunk_count": status.chunk_count,
		}),
		Vec::new(),
	))
}

async fn handle_upload_seal(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let total_len = if view.meta.is_empty() { None } else { view.meta_json()?["total_len"].as_u64() };
	let mxc = services
		.media
		.upload_seal(user, view.header.id, total_len)
		.await?;

	Ok(ack(Kind::Upload, view.header.id, view.header.seq, json!({ "mxc": mxc }), Vec::new()))
}

async fn handle_upload_abort(services: &Services, user: &UserId, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	services.media.upload_abort(user, view.header.id).await?;

	Ok(ack(Kind::Upload, view.header.id, view.header.seq, json!({ "ok": true }), Vec::new()))
}

async fn handle_download_info(services: &Services, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let meta = view.meta_json()?;
	let mxc = mxc_from_meta(&meta)?;
	let info = services.media.media_info(&mxc.as_str().try_into().map_err(|_| Reject::code("Conflict", "invalid mxc"))?).await?;

	Ok(ack(
		Kind::Download,
		0,
		view.header.seq,
		json!({
			"total_len": info.total_len,
			"content_type": info.content_type,
			"chunk_size_large": services.config.media_chunk_size_large,
		}),
		Vec::new(),
	))
}

async fn handle_download_read(services: &Services, view: &PackView<'_>) -> std::result::Result<Vec<u8>, Reject> {
	let meta = view.meta_json()?;
	let mxc = mxc_from_meta(&meta)?;
	let pos = meta["pos"].as_u64().unwrap_or(0);
	let len = meta["len"]
		.as_u64()
		.unwrap_or(services.config.media_download_default_len as u64)
		.min(services.config.wbf_data_max_bytes as u64);

	let mxc: Mxc<'_> = mxc.as_str().try_into().map_err(|_| Reject::code("Conflict", "invalid mxc"))?;
	let read = services.media.read_range(&mxc, pos, len).await?;

	Ok(ack(
		Kind::Download,
		0,
		view.header.seq,
		json!({ "pos": read.pos, "len": read.bytes.len(), "total_len": read.total_len }),
		read.bytes.to_vec(),
	))
}

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
		.unwrap_or_else(|_| error_pack(Kind::Control, view.header.id, view.header.seq, "Internal", "could not encode pong"))
}

/// An `Ack` answering `(id, seq)` with `meta` and, for reads, `data`.
fn ack(_for_kind: Kind, id: u64, seq: u32, meta: Value, data: Vec<u8>) -> Vec<u8> {
	PackBuilder::new(Kind::Control, control::ACK, Flags::IS_RESPONSE, id, seq)
		.json_meta(&meta)
		.and_then(|builder| builder.data(&data))
		.map(PackBuilder::finish)
		.unwrap_or_else(|_| error_pack(Kind::Control, id, seq, "Internal", "could not encode the reply"))
}

fn error_pack(_for_kind: Kind, id: u64, seq: u32, code: &str, message: &str) -> Vec<u8> {
	PackBuilder::new(Kind::Control, control::ERROR, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({ "code": code, "message": message }))
		.map(PackBuilder::finish)
		.unwrap_or_else(|_| PackBuilder::new(Kind::Control, control::ERROR, Flags::IS_RESPONSE, id, seq).finish())
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
		let mut pack = Reject::code("NotFound", "no such upload").into_pack(Kind::Upload, 42, 7);
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
		let mut pack = reject.into_pack(Kind::Upload, 1, 30);
		let view = decode(&mut pack).expect("decodes");

		assert_eq!(view.meta_json().expect("json")["expected_seq"], 12);
	}
}
