//! The wbf pack endpoints and the line a pack travels from a transport to
//! its handler and back (`docs/design/wbf-pack-pipeline.md`).
//!
//! `POST /_wbf/v1/pack` is the HTTP transport, one pack per request, meant for
//! debugging and scripts. `GET /_wbf/v1/ws` (`ws.rs`) is the WebSocket channel
//! and the main road. Both call the same `handle_pack`, so there is one set of
//! semantics; the differences between them are held in exactly two places:
//! the admission table (`admission`: which kinds a transport, or a connection
//! that has not logged in, may use at all) and `Reply` (where a handler's
//! packs go). Handlers see neither.
//!
//! Neither transport looks inside `data`; the only meta the server reads is
//! the plaintext meta of kinds it has to act on.

use std::net::IpAddr;

use axum::{
	body::Bytes,
	extract::State,
	http::{HeaderMap, StatusCode, header},
	response::{IntoResponse, Response},
};
use ruma::{
	DeviceId, Mxc, OwnedDeviceId, OwnedUserId, UserId,
	api::error::{ErrorKind, UnknownTokenErrorData},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tuwunel_core::{
	Error, Result, debug, err, error,
	wbf::{EncryptedFileInfo, Flags, Kind, PackBuilder, PackError, PackView, decode},
};
use tuwunel_service::{
	Services,
	channels::{ConnectionId, Outgoing},
	connections::ConnectionSlot,
	media::{UploadError, UploadRequest},
};

mod recent;
mod send;
mod session;
mod subscribe;
mod ws;

pub(crate) use self::ws::ws_route;
use crate::ClientIp;

/// `Control` subtypes.
mod control {
	pub(super) const HELLO: u8 = 0x01;
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

/// `Event` subtypes.
mod event {
	pub(super) const RECENT: u8 = 0x01;
	pub(super) const SEND: u8 = 0x02;
	/// Server to client only: one slice of a `Recent` window.
	pub(super) const BATCH: u8 = 0x03;
	pub(super) const SUBSCRIBE: u8 = 0x04;
	pub(super) const UNSUBSCRIBE: u8 = 0x05;
	// 0x06 Push is server to client only: `channels::EVENT_PUSH_SUBTYPE`.
}

/// # `POST /_wbf/v1/pack`
///
/// Body is one pack; response body is one pack. The access token comes as a
/// bearer header like every other client endpoint. An unauthenticated or
/// undecodable request still answers with a pack, so a client has one parser.
/// Kinds whose reply is a stream, or that change a connection's session, are
/// not admitted here and answer `Error(Unsupported)`.
pub(crate) async fn pack_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	headers: HeaderMap,
	body: Bytes,
) -> Result<Response> {
	let session = match authenticate(&services, &headers).await {
		| Ok(session) => session,
		| Err(error) => {
			let reply = error_pack(0, 0, "Unauthorized", &error.to_string());
			return Ok(pack_response(StatusCode::UNAUTHORIZED, reply));
		},
	};

	let mut body = body.to_vec();
	let reply = match decode(&mut body) {
		| Ok(view) => {
			let (id, seq) = (view.header.id, view.header.seq);
			let ctx = PackContext { session: Some(&session), client, transport: Transport::Http, connection: 0 };
			let mut reply = Reply::for_http();
			// The session change is dropped: no kind admitted on HTTP makes one.
			match handle_pack(&services, &ctx, view, &mut reply).await {
				| Ok(_change) => reply
					.into_http_pack()
					.unwrap_or_else(|| error_pack(id, seq, "Internal", "the handler produced no reply")),
				| Err(failure) => {
					error!(?failure, "wbf handler misbehaved on the HTTP transport");
					error_pack(id, seq, "Internal", "the handler could not reply on this transport")
				},
			}
		},
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

/// What a bearer token resolved to, kept so a long-lived connection can ask
/// again later whether it is still good (`revalidate`), and the connection's
/// place in its device's connection count while it is this session.
pub(crate) struct Session {
	pub(crate) user: OwnedUserId,
	pub(crate) device: OwnedDeviceId,
	token: String,
	/// `Some` on a counted WebSocket connection; `None` on HTTP, when the
	/// limit is off, or for a session made only to compare (`revalidate`).
	slot: Option<ConnectionSlot>,
}

impl Session {
	/// A session with no place in any connection count: HTTP, or a session made
	/// only to compare against.
	fn new(user: OwnedUserId, device: OwnedDeviceId, token: String) -> Self { Self { user, device, token, slot: None } }

	/// A session on a counted WebSocket connection; `slot` is what
	/// `reserve_connection_slot` gave for it.
	fn with_slot(user: OwnedUserId, device: OwnedDeviceId, token: String, slot: Option<ConnectionSlot>) -> Self {
		Self { user, device, token, slot }
	}

	/// Whether `other` is the same user on the same device: the unit the
	/// connection limit counts.
	fn is_same_device(&self, other: &Self) -> bool { self.user == other.user && self.device == other.device }

	/// Whether this session is `user` on `device`.
	fn is_device(&self, user: &UserId, device: &DeviceId) -> bool { self.user == user && self.device == device }

	/// Carries the connection's place in the count over from the session it
	/// replaces, when that session was the same device and this one took no
	/// place of its own (`take_connection_slot` left it to be inherited).
	pub(crate) fn inherit_slot(&mut self, previous: &mut Self) {
		if self.slot.is_none()
			&& previous
				.slot
				.as_ref()
				.is_some_and(|slot| slot.is_for(&self.user, &self.device))
		{
			self.slot = previous.slot.take();
		}
	}
}

/// Reserves a place in `device`'s connection count for a connection that is
/// about to be `user` on `device`, where the transport counts connections
/// (only WebSocket does). Called before any token is minted, so a refusal
/// leaves the device's existing sessions exactly as they were.
///
/// Args:
///     current: the connection's session so far, example: None on a fresh
///         upgrade; Some(bob's session) when bob logs in again on the same
///         connection (then no second place is taken: the new session
///         inherits the old one's, see `Session::inherit_slot`)
///     user, device: who the connection is about to be
/// Return:
///     Result<Option<ConnectionSlot>, Reject>  Ok(None) when nothing is
///     counted (HTTP, limit off, or inherited); Ok(Some) the place to keep
///     for the connection's life; Err(TooManyConnections) when the device is
///     at `wbf_ws_max_connections_per_device`, and the caller closes the
///     connection.
fn reserve_connection_slot(
	services: &Services,
	transport: Transport,
	current: Option<&Session>,
	user: &UserId,
	device: &DeviceId,
) -> Result<Option<ConnectionSlot>, Reject> {
	if transport != Transport::WebSocket {
		return Ok(None);
	}
	if current.is_some_and(|current| current.is_device(user, device) && current.slot.is_some()) {
		return Ok(None);
	}

	let max = services.config.wbf_ws_max_connections_per_device;
	services
		.connections
		.take_slot(user, device, max)
		.ok_or_else(|| Reject::too_many_connections(max))
}

/// Resolves the bearer header to a session, failing closed on anything else:
/// no header, unknown or expired token, or a locked account (MSC3939, the
/// same check every standard client route runs). The session holds no
/// connection slot yet; a WebSocket reserves one with
/// `reserve_connection_slot` before upgrading.
async fn authenticate(services: &Services, headers: &HeaderMap) -> Result<Session> {
	let token = headers
		.get(header::AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.ok_or_else(|| err!(Request(MissingToken("Missing access token."))))?;

	check_token(services, token).await
}

/// Checks that `session` is still what its token resolves to: same user and
/// device, not expired, account not locked. A WebSocket calls this before
/// every pack, so a logout, a revoked device, an expiry or a lock ends the
/// connection's authority at the next message instead of never.
pub(super) async fn revalidate(services: &Services, session: &Session) -> Result {
	let current = check_token(services, &session.token).await?;
	if !current.is_same_device(session) {
		return Err(unknown_token(false, "Access token now belongs to another session."));
	}

	Ok(())
}

async fn check_token(services: &Services, token: &str) -> Result<Session> {
	let (user, device, expires_at) = services
		.users
		.find_from_token(token)
		.await
		.map_err(|_| unknown_token(false, "Unknown access token."))?;

	if expires_at.is_some_and(|at| at <= std::time::SystemTime::now()) {
		return Err(unknown_token(true, "Access token expired."));
	}

	services.users.locked_check(&user).await?;

	Ok(Session::new(user, device, token.to_owned()))
}

fn unknown_token(soft_logout: bool, message: &'static str) -> Error {
	Error::Request(
		ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout }),
		message.into(),
		StatusCode::UNAUTHORIZED,
	)
}

/// Which transport a pack arrived on. Read by the admission table and by
/// `Session::take_connection_slot`; no handler reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
	Http,
	WebSocket,
}

/// Who a pack is from and how it arrived: everything a handler may know about
/// the connection.
pub(crate) struct PackContext<'a> {
	/// `None` only for a WebSocket that has not logged in; the admission table
	/// lets such a connection reach only the handlers that expect it.
	pub(crate) session: Option<&'a Session>,
	/// The peer address, for throttles and the device's last-seen ip.
	pub(crate) client: IpAddr,
	pub(crate) transport: Transport,
	/// The WebSocket connection's number (`Channels::next_connection_id`);
	/// 0 on HTTP, which has no connection to subscribe.
	pub(crate) connection: ConnectionId,
}

impl PackContext<'_> {
	/// Return:
	///     Result<&UserId, Reject>  Unauthorized when the connection has no
	///     session. Handlers that need a user call this instead of unwrapping:
	///     the admission table already refused anonymous packs to them, and
	///     this is the check that does not trust the table alone.
	fn user(&self) -> Result<&UserId, Reject> {
		self.session
			.map(|session| session.user.as_ref())
			.ok_or_else(|| Reject::code("Unauthorized", "log in first: this connection has no session"))
	}
}

/// Why a connection is closed after a pack was handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseReason {
	/// `Logout` succeeded: 1000, the client asked for it.
	LoggedOut,
	/// The pack was refused in a way that ends the connection (`Login` on a
	/// device at its connection limit): 1008.
	Refused,
}

/// What a handled pack does to the connection's session, besides replying.
pub(crate) enum SessionChange {
	/// Nothing: the usual case.
	Keep,
	/// `Login` or `Refresh` succeeded: the connection is this session from now on.
	Replace(Session),
	/// The caller closes the connection after the replies already queued.
	Close(CloseReason),
}

/// Where a handler's packs go: the only way a handler sends anything.
///
/// On a WebSocket every pack joins the connection's bounded send queue, and
/// `send` waits when the queue is full (that wait is the backpressure a slow
/// reader exerts on the handler). On HTTP the reply is the one response body,
/// so a second pack is refused: a handler that streams must not be admitted
/// on HTTP, and this is what catches it if the admission table is wrong.
pub(crate) struct Reply {
	sink: ReplySink,
}

enum ReplySink {
	WebSocket(mpsc::Sender<Outgoing>),
	/// `Some` once the one pack has been sent.
	Http(Option<Vec<u8>>),
}

/// Why `Reply::send` could not take a pack. Neither is the client's fault,
/// so neither becomes an `Error` pack to it.
#[derive(Debug)]
pub(crate) enum ReplyError {
	/// The connection's send task is gone (the peer closed); the handler stops.
	ConnectionGone,
	/// A second pack on the HTTP transport: a handler that streams was
	/// admitted where it must not be.
	HttpSinglePack,
}

impl Reply {
	pub(crate) fn for_websocket(queue: mpsc::Sender<Outgoing>) -> Self { Self { sink: ReplySink::WebSocket(queue) } }

	fn for_http() -> Self { Self { sink: ReplySink::Http(None) } }

	/// Args:
	///     pack: one finished pack, example: the Ack for a Chunk
	/// Return:
	///     Result<(), ReplyError>  Ok once queued (WebSocket) or held (HTTP).
	pub(crate) async fn send(&mut self, pack: Vec<u8>) -> Result<(), ReplyError> {
		match &mut self.sink {
			| ReplySink::WebSocket(queue) => queue
				.send(Outgoing::Pack(pack))
				.await
				.map_err(|_| ReplyError::ConnectionGone),
			| ReplySink::Http(held) => {
				if held.is_some() {
					return Err(ReplyError::HttpSinglePack);
				}
				*held = Some(pack);
				Ok(())
			},
		}
	}

	/// The connection's send queue, for a handler that registers it with the
	/// channels; None on HTTP, where there is no connection to push to.
	fn websocket_queue(&self) -> Option<mpsc::Sender<Outgoing>> {
		match &self.sink {
			| ReplySink::WebSocket(queue) => Some(queue.clone()),
			| ReplySink::Http(_) => None,
		}
	}

	/// Return:
	///     Option<Vec<u8>>  the one pack an HTTP handler sent; None when it
	///     sent nothing (a handler bug) or this is a WebSocket reply.
	fn into_http_pack(self) -> Option<Vec<u8>> {
		match self.sink {
			| ReplySink::Http(held) => held,
			| ReplySink::WebSocket(_) => None,
		}
	}
}

/// Where a `(kind, subtype)` may come from. Anything not in the table is not
/// handled at all (`UnknownKind`): the table is the whole list of what this
/// server speaks, and a new kind is admitted by adding its row here first.
struct Admission {
	/// A WebSocket that has not logged in may send it.
	anonymous_ok: bool,
	/// It may come over `POST /_wbf/v1/pack`. Off for kinds whose reply is a
	/// stream and for kinds that change a connection's session.
	http_ok: bool,
}

/// Args:
///     kind: example: Kind::Event
///     subtype: example: event::RECENT
/// Return:
///     Option<Admission>  None for a pair this server does not handle.
const fn admission(kind: Kind, subtype: u8) -> Option<Admission> {
	let logged_in_any_transport = Admission { anonymous_ok: false, http_ok: true };
	let logged_in_websocket_only = Admission { anonymous_ok: false, http_ok: false };
	let anyone_websocket_only = Admission { anonymous_ok: true, http_ok: false };
	let anyone_any_transport = Admission { anonymous_ok: true, http_ok: true };

	match (kind, subtype) {
		| (Kind::Control, control::HELLO | control::PING) => Some(anyone_any_transport),
		| (Kind::Session, session::LOGIN | session::REFRESH) => Some(anyone_websocket_only),
		| (Kind::Session, session::LOGOUT) => Some(logged_in_websocket_only),
		| (Kind::Upload, upload::CREATE | upload::CHUNK | upload::STATUS | upload::SEAL | upload::ABORT) =>
			Some(logged_in_any_transport),
		| (Kind::Download, download::INFO | download::READ) => Some(logged_in_any_transport),
		| (Kind::Event, event::SEND) => Some(logged_in_any_transport),
		// Its reply is a stream of `Batch` packs: WebSocket only.
		| (Kind::Event, event::RECENT) => Some(logged_in_websocket_only),
		// They change what a connection listens to: WebSocket only.
		| (Kind::Event, event::SUBSCRIBE | event::UNSUBSCRIBE) => Some(logged_in_websocket_only),
		| _ => None,
	}
}

/// Why a pack could not be handled to completion: the client's request was
/// refused (an `Error` pack goes back), or the reply could not be sent.
#[derive(Debug)]
pub(crate) enum Failure {
	Reject(Reject),
	Reply(ReplyError),
}

impl From<Reject> for Failure {
	fn from(reject: Reject) -> Self { Self::Reject(reject) }
}

impl From<ReplyError> for Failure {
	fn from(error: ReplyError) -> Self { Self::Reply(error) }
}

impl From<UploadError> for Failure {
	fn from(error: UploadError) -> Self { Self::Reject(error.into()) }
}

impl From<Error> for Failure {
	fn from(error: Error) -> Self { Self::Reject(error.into()) }
}

impl From<PackError> for Failure {
	fn from(error: PackError) -> Self { Self::Reject(error.into()) }
}

/// Runs one decoded pack through the last gates (size, admission) and its
/// handler, sending every reply through `reply`.
///
/// Args:
///     ctx: who sent it and how
///     view: the decoded pack
///     reply: where its replies go
/// Return:
///     Result<SessionChange, ReplyError>  what the connection does next;
///     Err only when a reply could not be sent, which ends the connection.
pub(crate) async fn handle_pack(
	services: &Services,
	ctx: &PackContext<'_>,
	view: PackView<'_>,
	reply: &mut Reply,
) -> Result<SessionChange, ReplyError> {
	let (id, seq) = (view.header.id, view.header.seq);
	match dispatch(services, ctx, &view, reply).await {
		| Ok(change) => Ok(change),
		| Err(Failure::Reply(error)) => Err(error),
		| Err(Failure::Reject(reject)) => {
			let closes_connection = reject.closes_connection;
			reply.send(reject.into_pack(id, seq)).await?;
			Ok(if closes_connection { SessionChange::Close(CloseReason::Refused) } else { SessionChange::Keep })
		},
	}
}

async fn dispatch(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<SessionChange, Failure> {
	let header = view.header;
	let limits_ok = view.meta.len() <= services.config.wbf_meta_max_bytes
		&& view.data.len() <= services.config.wbf_data_max_bytes;
	if !limits_ok {
		return Err(Reject::code("TooLarge", "meta or data exceeds the configured limit").into());
	}

	let Some(admission) = admission(header.kind, header.subtype) else {
		return Err(Reject::code("UnknownKind", "no handler for this kind and subtype").into());
	};
	if ctx.transport == Transport::Http && !admission.http_ok {
		return Err(Reject::code(
			"Unsupported",
			"this kind is only served over the WebSocket channel; POST /_wbf/v1/pack is for one-pack requests",
		)
		.into());
	}
	if ctx.session.is_none() && !admission.anonymous_ok {
		return Err(Reject::code("Unauthorized", "log in first: this connection has no session").into());
	}

	match (header.kind, header.subtype) {
		| (Kind::Control, control::HELLO) => {
			reply.send(hello(services, ctx, view)).await?;
			Ok(SessionChange::Keep)
		},
		| (Kind::Control, control::PING) => {
			reply.send(pong(view)).await?;
			Ok(SessionChange::Keep)
		},
		| (Kind::Session, subtype) => session::handle(services, ctx, subtype, view, reply).await,
		| (Kind::Event, event::RECENT) => {
			recent::handle_event_recent(services, ctx.user()?, view, reply).await?;
			Ok(SessionChange::Keep)
		},
		| (Kind::Event, event::SUBSCRIBE) => {
			subscribe::handle_subscribe(services, ctx, view, reply).await?;
			Ok(SessionChange::Keep)
		},
		| (Kind::Event, event::UNSUBSCRIBE) => {
			subscribe::handle_unsubscribe(services, ctx, view, reply).await?;
			Ok(SessionChange::Keep)
		},
		| _ => {
			let pack = handle_one_reply(services, ctx.user()?, view).await?;
			reply.send(pack).await?;
			Ok(SessionChange::Keep)
		},
	}
}

/// The kinds whose reply is exactly one pack; the dispatcher sends it.
async fn handle_one_reply(services: &Services, user: &UserId, view: &PackView<'_>) -> Result<Vec<u8>, Reject> {
	match (view.header.kind, view.header.subtype) {
		| (Kind::Upload, upload::CREATE) => handle_upload_create(services, user, view).await,
		| (Kind::Upload, upload::CHUNK) => handle_upload_chunk(services, user, view).await,
		| (Kind::Upload, upload::STATUS) => handle_upload_status(services, user, view).await,
		| (Kind::Upload, upload::SEAL) => handle_upload_seal(services, user, view).await,
		| (Kind::Upload, upload::ABORT) => handle_upload_abort(services, user, view).await,
		| (Kind::Download, download::INFO) => handle_download_info(services, view).await,
		| (Kind::Download, download::READ) => handle_download_read(services, view).await,
		| (Kind::Event, event::SEND) => send::handle_event_send(services, user, view).await,
		// Admitted by the table but not routed here: the table and this match
		// disagree, which is a bug, but it fails closed.
		| _ => Err(Reject::code("UnknownKind", "no handler for this kind and subtype")),
	}
}

/// A refused request, in the vocabulary of the wire format's error codes.
#[derive(Debug)]
pub(crate) struct Reject {
	code: &'static str,
	message: String,
	extra: Value,
	/// The connection is closed (1008) after this error is sent.
	closes_connection: bool,
}

impl Reject {
	fn code(code: &'static str, message: impl Into<String>) -> Self {
		Self { code, message: message.into(), extra: Value::Null, closes_connection: false }
	}

	fn with_extra(code: &'static str, message: impl Into<String>, extra: Value) -> Self {
		Self { code, message: message.into(), extra, closes_connection: false }
	}

	/// The device already holds `max` connections; this one is turned away
	/// and, on a WebSocket, closed.
	fn too_many_connections(max: u32) -> Self {
		Self {
			code: "TooManyConnections",
			message: format!("this device already holds {max} wbf connections; close one before opening another"),
			extra: json!({ "max_connections": max }),
			closes_connection: true,
		}
	}

	pub(crate) fn into_pack(self, id: u64, seq: u32) -> Vec<u8> {
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
			| UploadError::Truncated(stored) => Self::with_extra(
				"Truncated",
				format!(
					"upload hit the size limit after {} chunks, {} bytes; it is finished as incomplete and may be sealed",
					stored.received_count, stored.total_len
				),
				json!({
					"received": stored.received_count,
					"total_len": stored.total_len,
					"finished": stored.finished,
					"truncated": stored.truncated,
				}),
			),
			| UploadError::OutOfOrder { expected } =>
				Self::with_extra("OutOfOrder", format!("expected chunk {expected}"), json!({ "expected_seq": expected })),
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

/// The answer to `Hello`: what this server speaks. The client's own meta
/// (its name, its feature list) is not read; nothing here depends on it yet.
fn hello(services: &Services, ctx: &PackContext<'_>, view: &PackView<'_>) -> Vec<u8> {
	ack(
		view.header.id,
		view.header.seq,
		json!({
			"protocol": 1,
			"server": services.globals.server_name(),
			"engine": tuwunel_core::version::name(),
			"engine_version": tuwunel_core::version::version(),
			"features": ["upload", "download", "recent", "batch", "seq", "attachments", "login", "push"],
			// For debugging; 0 over HTTP. A client need not use it.
			"connection_id": ctx.connection,
			"recent_default_limit": services.config.wbf_recent_default_limit,
			"recent_max_limit": services.config.wbf_recent_max_limit,
			"recent_default_batch": services.config.wbf_recent_default_batch,
			"recent_max_batch": services.config.wbf_recent_max_batch,
			"max_connections_per_device": services.config.wbf_ws_max_connections_per_device,
			"chunk_size_default": services.config.media_chunk_size_default,
			"chunk_size_large": services.config.media_chunk_size_large,
			"data_max_bytes": services.config.wbf_data_max_bytes,
		}),
		Vec::new(),
	)
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
