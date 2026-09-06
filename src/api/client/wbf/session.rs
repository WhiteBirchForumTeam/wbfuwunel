//! The `Session` kind: `Login`, `Refresh`, `Logout` over the channel
//! (`docs/design/wbf-wire-format.md` §6.3).
//!
//! A `Login` pack's meta is the body of a Matrix `POST /login`, decoded by
//! the same ruma type and checked by the same handlers the HTTP route uses;
//! what it changes is the connection's `Session`, which is why these packs
//! are refused over HTTP (there is no connection to change). Credentials are
//! checked after the shared login throttle, so the channel is not a faster
//! road for guessing than `/login` is.

use std::net::IpAddr;

use http::{StatusCode, header::CONTENT_TYPE};
use ruma::api::{
	IncomingRequest,
	client::session::login::v3::{LoginInfo, Request as LoginRequest},
	error::{ErrorKind, RetryAfter},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tuwunel_core::{Error, debug, wbf::PackView};
use tuwunel_service::Services;

use super::{Handled, Reject, Session, SessionChange, ack};
use crate::client::session::{password, token};

pub(super) const LOGIN: u8 = 0x01;
pub(super) const REFRESH: u8 = 0x02;
pub(super) const LOGOUT: u8 = 0x03;

/// Dispatches one `Session` pack.
///
/// Args:
///     current: the connection's session, None while it has not logged in
///     client: the peer address, for the login throttle and the device's last seen ip
///     subtype: `LOGIN`, `REFRESH` or `LOGOUT`
/// Return:
///     Result<Handled, Reject>  the Ack and what happens to the connection's
///     session; Err is the refusal to send back.
pub(super) async fn handle(
	services: &Services,
	current: Option<&Session>,
	client: IpAddr,
	subtype: u8,
	view: &PackView<'_>,
) -> Result<Handled, Reject> {
	match subtype {
		| LOGIN => login(services, client, view).await,
		| REFRESH => refresh(services, client, view).await,
		| LOGOUT => logout(services, current, view).await,
		| _ => Err(Reject::code("UnknownKind", "no such Session operation")),
	}
}

/// `Login`: the meta is a `/login` request body. Only `m.login.password` and
/// `m.login.token` are accepted here; the flows that need a browser or an
/// appservice stay on HTTP.
async fn login(services: &Services, client: IpAddr, view: &PackView<'_>) -> Result<Handled, Reject> {
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
			)),
	};

	let issued = services
		.users
		.issue_session(
			&user_id,
			request.device_id.as_deref(),
			request.initial_device_display_name.as_deref(),
			request.refresh_token,
			Some(client),
		)
		.await
		.map_err(refuse_login)?;

	debug!(user = %issued.user_id, device = %issued.device_id, "logged in over the wbf channel");

	let session = Session {
		user: issued.user_id.clone(),
		device: issued.device_id.clone(),
		token: issued.access_token.clone(),
	};

	let mut meta = Map::new();
	meta.insert("user_id".into(), Value::String(issued.user_id.to_string()));
	meta.insert("device_id".into(), Value::String(issued.device_id.to_string()));
	meta.insert("access_token".into(), Value::String(issued.access_token));
	insert_token_lifetime(&mut meta, issued.refresh_token, issued.expires_in);

	Ok(Handled {
		reply: ack(view.header.id, view.header.seq, Value::Object(meta), Vec::new()),
		change: SessionChange::Replace(session),
	})
}

#[derive(Deserialize)]
struct RefreshMeta {
	refresh_token: String,
}

/// `Refresh`: rotates the refresh token and issues a new access token, then
/// the connection continues as the session the new token resolves to.
async fn refresh(services: &Services, client: IpAddr, view: &PackView<'_>) -> Result<Handled, Reject> {
	services
		.users
		.check_login_rate(client)
		.map_err(refuse_login)?;

	let meta: RefreshMeta = serde_json::from_slice(view.meta)
		.map_err(|e| Reject::code("Corrupt", format!("Refresh meta must be {{\"refresh_token\"}}: {e}")))?;

	let refreshed = services
		.users
		.refresh_session(&meta.refresh_token)
		.await
		.map_err(refuse_login)?;

	// The new access token names the session this connection now is.
	let (user, device, _expires_at) = services
		.users
		.find_from_token(&refreshed.access_token)
		.await
		.map_err(|_| Reject::code("Internal", "the refreshed token does not resolve to a device"))?;
	let session = Session { user, device, token: refreshed.access_token.clone() };

	let mut meta = Map::new();
	meta.insert("access_token".into(), Value::String(refreshed.access_token));
	insert_token_lifetime(&mut meta, refreshed.refresh_token, refreshed.expires_in);

	Ok(Handled {
		reply: ack(view.header.id, view.header.seq, Value::Object(meta), Vec::new()),
		change: SessionChange::Replace(session),
	})
}

#[derive(Default, Deserialize)]
struct LogoutMeta {
	#[serde(default)]
	all: bool,
}

/// `Logout`: ends this connection's session (or every device of the user with
/// `all`), acknowledges, and the connection is closed by the caller.
async fn logout(services: &Services, current: Option<&Session>, view: &PackView<'_>) -> Result<Handled, Reject> {
	let Some(session) = current else {
		return Err(Reject::code("Unauthorized", "this connection is not logged in"));
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

	Ok(Handled {
		reply: ack(view.header.id, view.header.seq, json!({}), Vec::new()),
		change: SessionChange::Close,
	})
}

/// Decodes the meta as the body of `POST /_matrix/client/v3/login`, with the
/// ruma type the HTTP route uses, so the two cannot drift apart.
fn parse_login_request(meta: &[u8]) -> Result<LoginRequest, Reject> {
	let http_request = http::Request::builder()
		.method(http::Method::POST)
		.uri("/_matrix/client/v3/login")
		.header(CONTENT_TYPE, "application/json")
		.body(meta.to_vec())
		.map_err(|e| Reject::code("Internal", format!("could not frame the login request: {e}")))?;

	let no_path_arguments: &[&str] = &[];
	LoginRequest::try_from_http_request(http_request, no_path_arguments)
		.map_err(|e| Reject::code("Corrupt", format!("Login meta is not a /login request body: {e}")))
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
/// `RateLimited` with how long to wait, a locked account or bad token (401)
/// to `Unauthorized`, everything else about the credentials to `Forbidden`.
fn refuse_login(error: Error) -> Reject {
	match error.status_code() {
		| StatusCode::TOO_MANY_REQUESTS => {
			let retry_after_ms = match error.kind() {
				| ErrorKind::LimitExceeded(data) => match data.retry_after {
					| Some(RetryAfter::Delay(delay)) => Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
					| _ => None,
				},
				| _ => None,
			};
			Reject {
				code: "RateLimited",
				message: error.to_string(),
				extra: json!({ "retry_after_ms": retry_after_ms }),
			}
		},
		| StatusCode::UNAUTHORIZED => Reject::code("Unauthorized", error.to_string()),
		| _ => Reject::code("Forbidden", error.to_string()),
	}
}
