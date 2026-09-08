//! The `Session` kind: `Login`, `Refresh`, `Logout` over the channel
//! (`docs/design/wbf-wire-format.md` §6.3).
//!
//! A `Login` pack's meta is the body of a Matrix `POST /login`, decoded by
//! the same ruma type and checked by the same handlers the HTTP route uses;
//! what it changes is the connection's `Session`, which is why these packs
//! are refused over HTTP (there is no connection to change). Credentials are
//! checked after the shared login throttle, so the channel is not a faster
//! road for guessing than `/login` is.
//!
//! A `Login` or `Refresh` that resolves to a session also takes that
//! session's place in its device's connection count, through the `admit`
//! gate the users service asks before it writes any token: a device at
//! `wbf_ws_max_connections_per_device` is refused with `TooManyConnections`,
//! nothing is minted or replaced, and the connection is closed (pipeline
//! §2.1).

use std::net::IpAddr;

use http::{StatusCode, header::CONTENT_TYPE};
use ruma::api::{
	IncomingRequest,
	client::session::login::v3::{LoginInfo, Request as LoginRequest},
	error::{ErrorKind, LimitExceededErrorData, RetryAfter},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tuwunel_core::{Error, debug, wbf::PackView};
use tuwunel_service::Services;

use tuwunel_service::connections::ConnectionSlot;

use super::{CloseReason, Failure, PackContext, Reject, Reply, Session, SessionChange, ack, reserve_connection_slot};
use crate::client::session::{password, token};

pub(super) const LOGIN: u8 = 0x01;
pub(super) const REFRESH: u8 = 0x02;
pub(super) const LOGOUT: u8 = 0x03;

/// Dispatches one `Session` pack.
///
/// Args:
///     ctx: the connection's session so far (None while it has not logged
///         in), the peer address for the throttle, the transport
///     subtype: `LOGIN`, `REFRESH` or `LOGOUT`
///     reply: where the Ack goes
/// Return:
///     Result<SessionChange, Failure>  what happens to the connection's
///     session once the Ack is queued; Err(Reject) is the refusal to send back.
pub(super) async fn handle(
	services: &Services,
	ctx: &PackContext<'_>,
	subtype: u8,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<SessionChange, Failure> {
	match subtype {
		| LOGIN => login(services, ctx, view, reply).await,
		| REFRESH => refresh(services, ctx, view, reply).await,
		| LOGOUT => logout(services, ctx.session, view, reply).await,
		| _ => Err(Reject::code("UnknownKind", "no such Session operation").into()),
	}
}

/// `Login`: the meta is a `/login` request body. Only `m.login.password` and
/// `m.login.token` are accepted here; the flows that need a browser or an
/// appservice stay on HTTP.
async fn login(services: &Services, ctx: &PackContext<'_>, view: &PackView<'_>, reply: &mut Reply) -> Result<SessionChange, Failure> {
	let client: IpAddr = ctx.client;
	// The throttle comes before the credentials: a flood must not cost a
	// database lookup per attempt. A locked account behind an empty bucket
	// therefore reads as RateLimited, not as locked.
	services
		.users
		.check_login_rate(client)
		.map_err(refuse_login)?;

	let request = parse_login_request(view.meta)?;
	let user_id = match &request.login_info {
		| LoginInfo::Password(info) if services.config.login_with_password =>
			password::handle_login(services, &request, info)
				.await
				.map_err(refuse_login)?,
		| LoginInfo::Token(info) =>
			token::handle_login(services, &request, info)
				.await
				.map_err(refuse_login)?,
		| _ =>
			return Err(Reject::code(
				"Forbidden",
				"only m.login.password and m.login.token are accepted on the channel; other flows use HTTP /login",
			)
			.into()),
	};

	let mut gate = SlotGate::new(services, ctx);
	let issued = services
		.users
		.issue_session(
			&user_id,
			request.device_id.as_deref(),
			request.initial_device_display_name.as_deref(),
			request.refresh_token,
			Some(client),
			&mut |user, device| gate.admit(user, device),
		)
		.await;
	let slot = gate.outcome()?;
	let issued = issued.map_err(refuse_login)?;

	let session = Session::with_slot(issued.user_id.clone(), issued.device_id.clone(), issued.access_token.clone(), slot);

	debug!(user = %issued.user_id, device = %issued.device_id, "logged in over the wbf channel");

	let mut meta = Map::new();
	meta.insert("user_id".into(), Value::String(issued.user_id.to_string()));
	meta.insert("device_id".into(), Value::String(issued.device_id.to_string()));
	meta.insert("access_token".into(), Value::String(issued.access_token));
	insert_token_lifetime(&mut meta, issued.refresh_token, issued.expires_in);

	reply
		.send(ack(view.header.id, view.header.seq, Value::Object(meta), Vec::new()))
		.await?;

	Ok(SessionChange::Replace(session))
}

#[derive(Deserialize)]
struct RefreshMeta {
	refresh_token: String,
}

/// The connection-limit gate handed to the users service: reserves the place
/// in the device's count when the service knows who the session will be, and
/// keeps the exact refusal so it reaches the client as `TooManyConnections`
/// rather than as whatever the service's error type can carry.
struct SlotGate<'a> {
	services: &'a Services,
	ctx: &'a PackContext<'a>,
	reserved: Option<Result<Option<ConnectionSlot>, Reject>>,
}

impl<'a> SlotGate<'a> {
	fn new(services: &'a Services, ctx: &'a PackContext<'a>) -> Self { Self { services, ctx, reserved: None } }

	/// The service calls this once, before writing any token.
	fn admit(&mut self, user: &ruma::UserId, device: &ruma::DeviceId) -> tuwunel_core::Result {
		let outcome = reserve_connection_slot(self.services, self.ctx.transport, self.ctx.session, user, device);
		let admitted = outcome.is_ok();
		self.reserved = Some(outcome);
		// The exact refusal is kept in `reserved`; this error only stops the
		// service and is never shown to anyone.
		admitted
			.then_some(())
			.ok_or_else(|| Error::BadRequest(ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None }), "connection limit"))
	}

	/// Return:
	///     Result<Option<ConnectionSlot>, Failure>  the reserved place (None when
	///     nothing is counted, or when the service never asked because it
	///     refused the credentials first); Err(TooManyConnections) when the gate
	///     refused.
	fn outcome(self) -> Result<Option<ConnectionSlot>, Failure> {
		match self.reserved {
			| Some(Err(refused)) => Err(refused.into()),
			| Some(Ok(slot)) => Ok(slot),
			| None => Ok(None),
		}
	}
}

/// `Refresh`: rotates the refresh token and issues a new access token, then
/// the connection continues as the session the new token resolves to.
async fn refresh(services: &Services, ctx: &PackContext<'_>, view: &PackView<'_>, reply: &mut Reply) -> Result<SessionChange, Failure> {
	// Same bucket and same order as `login`: throttle first.
	services
		.users
		.check_login_rate(ctx.client)
		.map_err(refuse_login)?;

	let meta: RefreshMeta = serde_json::from_slice(view.meta)
		.map_err(|e| Reject::code("Corrupt", format!("Refresh meta must be {{\"refresh_token\"}}: {e}")))?;

	let mut gate = SlotGate::new(services, ctx);
	let refreshed = services
		.users
		.refresh_session(&meta.refresh_token, &mut |user, device| gate.admit(user, device))
		.await;
	let slot = gate.outcome()?;
	let refreshed = refreshed.map_err(refuse_login)?;

	// The connection is now the session the new token belongs to. Usually the
	// same device as before, so the place in the count is inherited rather
	// than taken; a refresh token of another device counts as that device.
	let session = Session::with_slot(refreshed.user_id, refreshed.device_id, refreshed.access_token.clone(), slot);

	let mut meta = Map::new();
	meta.insert("access_token".into(), Value::String(refreshed.access_token));
	insert_token_lifetime(&mut meta, refreshed.refresh_token, refreshed.expires_in);

	reply
		.send(ack(view.header.id, view.header.seq, Value::Object(meta), Vec::new()))
		.await?;

	Ok(SessionChange::Replace(session))
}

#[derive(Default, Deserialize)]
struct LogoutMeta {
	#[serde(default)]
	all: bool,
}

/// `Logout`: ends this connection's session (or every device of the user with
/// `all`), acknowledges, and the connection is closed by the caller.
async fn logout(services: &Services, current: Option<&Session>, view: &PackView<'_>, reply: &mut Reply) -> Result<SessionChange, Failure> {
	let Some(session) = current else {
		return Err(Reject::code("Unauthorized", "this connection is not logged in").into());
	};

	let meta: LogoutMeta = if view.meta.is_empty() {
		LogoutMeta::default()
	} else {
		serde_json::from_slice(view.meta)
			.map_err(|e| Reject::code("Corrupt", format!("Logout meta must be {{\"all\"?: bool}}: {e}")))?
	};

	services
		.users
		.end_session(&session.user, &session.device, meta.all)
		.await;

	debug!(user = %session.user, device = %session.device, all = meta.all, "logged out over the wbf channel");

	reply
		.send(ack(view.header.id, view.header.seq, json!({}), Vec::new()))
		.await?;

	Ok(SessionChange::Close(CloseReason::LoggedOut))
}

/// Decodes the meta as the body of `POST /_matrix/client/v3/login`, with the
/// ruma type the HTTP route uses, so the two cannot drift apart.
fn parse_login_request(meta: &[u8]) -> Result<LoginRequest, Failure> {
	let http_request = http::Request::builder()
		.method(http::Method::POST)
		.uri("/_matrix/client/v3/login")
		.header(CONTENT_TYPE, "application/json")
		.body(meta.to_vec())
		.map_err(|e| Reject::code("Internal", format!("could not frame the login request: {e}")))?;

	let no_path_arguments: &[&str] = &[];
	LoginRequest::try_from_http_request(http_request, no_path_arguments)
		.map_err(|e| Reject::code("Corrupt", format!("Login meta is not a /login request body: {e}")).into())
}

/// The optional tail of a login or refresh reply, present only when the
/// server issued them.
fn insert_token_lifetime(meta: &mut Map<String, Value>, refresh_token: Option<String>, expires_in: Option<std::time::Duration>) {
	if let Some(refresh_token) = refresh_token {
		meta.insert("refresh_token".into(), Value::String(refresh_token));
	}
	if let Some(expires_in) = expires_in {
		meta.insert("expires_in_ms".into(), json!(u64::try_from(expires_in.as_millis()).unwrap_or(u64::MAX)));
	}
}

/// Maps a login-path error to the wire's vocabulary: the throttle's 429 to
/// `RateLimited` with how long to wait; an unknown, expired or replayed
/// token (`M_UNKNOWN_TOKEN`, whatever its HTTP status) and a locked account
/// to `Unauthorized`, carrying Matrix's `soft_logout` when the error has
/// one so a client can tell "log in again" from "you are out"; everything
/// else about the credentials to `Forbidden`.
fn refuse_login(error: Error) -> Failure {
	if let ErrorKind::UnknownToken(data) = error.kind() {
		return Reject::with_extra("Unauthorized", error.to_string(), json!({ "soft_logout": data.soft_logout })).into();
	}

	match error.status_code() {
		| StatusCode::TOO_MANY_REQUESTS => {
			let retry_after_ms = match error.kind() {
				| ErrorKind::LimitExceeded(data) => match data.retry_after {
					| Some(RetryAfter::Delay(delay)) => Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
					| _ => None,
				},
				| _ => None,
			};
			Reject::with_extra("RateLimited", error.to_string(), json!({ "retry_after_ms": retry_after_ms })).into()
		},
		| StatusCode::UNAUTHORIZED => Reject::code("Unauthorized", error.to_string()).into(),
		| _ => Reject::code("Forbidden", error.to_string()).into(),
	}
}
