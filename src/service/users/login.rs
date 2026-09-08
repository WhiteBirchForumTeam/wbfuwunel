//! Issuing, refreshing and ending a session: the part of logging in that
//! comes after the credentials have been checked. `POST /login`,
//! `POST /refresh`, `POST /logout` and the wbf channel's `Session` packs all
//! call these, so there is one place that decides how a device gets its
//! tokens (`docs/design/wbf-wire-format.md` §6.3).

use std::{net::IpAddr, time::Duration};

use futures::StreamExt;
use ruma::{
	DeviceId, OwnedDeviceId, OwnedUserId, UserId,
	api::error::{ErrorKind, UnknownTokenErrorData},
};
use tuwunel_core::{
	Err, Error, Result, debug_info, implement, info,
	utils::{
		future::OptionFutureExt,
		rate_limit::limit_exceeded,
		stream::ReadyExt,
		time::timepoint_has_passed,
		BoolExt,
	},
};

use super::device::{RefreshToken, generate_refresh_token, resolve_device_id};

/// What a successful login hands the client: the fields of the Matrix
/// `/login` response that name the session.
pub struct IssuedSession {
	pub user_id: OwnedUserId,
	pub device_id: OwnedDeviceId,
	pub access_token: String,
	pub refresh_token: Option<String>,
	pub expires_in: Option<Duration>,
}

/// What a successful refresh hands the client: the fields of the Matrix
/// `/refresh` response, plus whose session it is (the caller on the channel
/// needs that to become the session, without a second lookup).
pub struct RefreshedSession {
	pub user_id: OwnedUserId,
	pub device_id: OwnedDeviceId,
	pub access_token: String,
	pub refresh_token: Option<String>,
	pub expires_in: Option<Duration>,
}

/// One login attempt from `client` against the shared login throttle.
///
/// Args:
///     client: the peer address, example: 203.0.113.7
/// Return:
///     Result  Err 429 `M_LIMIT_EXCEEDED` (with `retry_after_ms`) when the
///     address has used up `login_rc_burst_count` and must wait for
///     `login_rc_per_second` to refill; Ok otherwise, including when the
///     throttle is disabled (`login_rc_per_second = 0`).
#[implement(super::Service)]
pub fn check_login_rate(&self, client: IpAddr) -> Result {
	let config = &self.services.config;
	self.login_limiter
		.take(client, f64::from(config.login_rc_per_second), f64::from(config.login_rc_burst_count))
		.map_err(|retry_after| limit_exceeded("Too many login attempts from this address.", retry_after))
}

/// The caller's last word on whether a session may be issued to this user
/// on this device, asked once the device is known and before any token is
/// written. An `Err` refuses the login with no token minted or replaced, so
/// the device's existing sessions are untouched. The wbf channel uses it for
/// its per-device connection limit; HTTP passes `admit_any`.
pub type AdmitSession<'a> = &'a mut (dyn FnMut(&UserId, &DeviceId) -> Result + Send);

/// The gate that refuses nobody, for callers without a connection to count.
pub fn admit_any(_user_id: &UserId, _device_id: &DeviceId) -> Result { Ok(()) }

/// Gives an authenticated user a session on a device: a new access token
/// (and refresh token when asked), on the named device if the user has it
/// or on a newly created one otherwise. The caller has already checked the
/// credentials; this checks the account is not locked.
///
/// Args:
///     user_id: example: @alice:localhost
///     device_id: the device the client wants to keep using, example: Some("RJYKSTBOIE"); None or unknown creates one
///     initial_device_display_name: example: Some("wbf desktop")
///     want_refresh_token: the request's `refresh_token: true`
///     client_ip: recorded as the new device's last seen address
///     admit: asked with the final (user, device) before any token is
///         written, example: `&mut admit_any`
/// Return:
///     Result<IssuedSession>  Err 401 `M_USER_LOCKED` for a locked account;
///     `admit`'s own Err, unchanged, when it refuses.
#[implement(super::Service)]
pub async fn issue_session(
	&self,
	user_id: &UserId,
	device_id: Option<&DeviceId>,
	initial_device_display_name: Option<&str>,
	want_refresh_token: bool,
	client_ip: Option<IpAddr>,
	admit: AdmitSession<'_>,
) -> Result<IssuedSession> {
	self.locked_check(user_id).await?;

	let existing_device = match device_id {
		| Some(device_id)
			if self
				.all_device_ids(user_id)
				.ready_any(|known| known == device_id)
				.await =>
			Some(device_id.to_owned()),
		| _ => None,
	};
	// A device the user does not have yet is created under the id the client
	// asked for, or a fresh one (`resolve_device_id`, the same choice
	// `create_device` makes); either way that is the device the session will
	// be on, so that is what the gate is asked about.
	let final_device_id: OwnedDeviceId = existing_device
		.clone()
		.unwrap_or_else(|| resolve_device_id(device_id));
	admit(user_id, &final_device_id)?;

	let (access_token, expires_in) = self.generate_access_token(want_refresh_token);
	let refresh_token = expires_in.is_some().then(generate_refresh_token);

	let device_id = if existing_device.is_some() {
		self.set_access_token(user_id, &final_device_id, &access_token, expires_in, refresh_token.as_deref())
			.await?;

		final_device_id
	} else {
		self.create_device(
			user_id,
			Some(&final_device_id),
			(Some(&access_token), expires_in),
			refresh_token.as_deref(),
			initial_device_display_name,
			client_ip,
		)
		.await?
	};

	info!("{user_id} logged in");

	Ok(IssuedSession {
		user_id: user_id.to_owned(),
		device_id,
		access_token,
		refresh_token,
		expires_in,
	})
}

/// Turns a refresh token into a fresh access token (and a rotated refresh
/// token), with the Matrix rules for expired, replayed and unknown tokens.
///
/// Args:
///     presented: the client's refresh token, example: "refresh_..."
///     admit: asked with the token's (user, device) before anything is
///         rotated or written, example: `&mut admit_any`
/// Return:
///     Result<RefreshedSession>  Err 403 for a malformed or unknown token;
///     Err 401 `M_UNKNOWN_TOKEN` for an expired one or a replay after
///     rotation (the device is removed when the configuration says so);
///     Err 401 `M_USER_LOCKED` for a locked account, which may not mint
///     tokens either (review of PR #30: the old `/refresh` skipped this);
///     `admit`'s own Err, unchanged, when it refuses (the presented token
///     is then still current and usable elsewhere).
#[implement(super::Service)]
pub async fn refresh_session(&self, presented: &str, admit: AdmitSession<'_>) -> Result<RefreshedSession> {
	if !presented.starts_with("refresh_") {
		return Err!(Request(Forbidden("Refresh token is malformed.")));
	}

	match self.classify_refresh_token(presented).await {
		| RefreshToken::Current { user_id, device_id, expires_at } => {
			if expires_at.is_some_and(timepoint_has_passed) {
				let hard = self.services.server.config.refresh_token_hard_logout;
				hard.then_async(|| self.remove_device(&user_id, &device_id))
					.unwrap_or_else_async(async || {
						self.remove_refresh_token(&user_id, &device_id)
							.await
							.ok();
					})
					.await;

				return Err(Error::BadRequest(
					ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout: !hard }),
					"Refresh token has expired.",
				));
			}

			self.locked_check(&user_id).await?;
			admit(&user_id, &device_id)?;

			let refresh_token = Some(generate_refresh_token());
			let (access_token, expires_in) = self.generate_access_token(true);
			self.set_access_token(&user_id, &device_id, &access_token, expires_in, refresh_token.as_deref())
				.await?;

			debug_info!(?user_id, ?device_id, ?expires_in, "refreshed their access_token");

			Ok(RefreshedSession { user_id, device_id, access_token, refresh_token, expires_in })
		},

		| RefreshToken::Replayed { user_id, device_id, current, grace } if grace => {
			// Benign double-submit: re-issue an access token for the unchanged
			// refresh token rather than rotating it.
			self.locked_check(&user_id).await?;
			admit(&user_id, &device_id)?;

			let (access_token, expires_in) = self.generate_access_token(true);
			self.set_access_token(&user_id, &device_id, &access_token, expires_in, None)
				.await?;

			Ok(RefreshedSession { user_id, device_id, access_token, refresh_token: Some(current), expires_in })
		},

		| RefreshToken::Replayed { user_id, device_id, .. } => {
			let revoke = self.services.server.config.refresh_token_reuse_revoke;
			debug_info!(?user_id, ?device_id, revoke, "refresh token reused after rotation");

			if revoke {
				self.remove_device(&user_id, &device_id).await;
			}

			Err(Error::BadRequest(
				ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout: !revoke }),
				"Refresh token has already been used.",
			))
		},

		| RefreshToken::Unknown => Err!(Request(Forbidden("Refresh token is unrecognized."))),
	}
}

/// Ends a session: removes `device_id` (its tokens, metadata and to-device
/// queue), or every device of the user when `all_devices` is set.
///
/// Args:
///     user_id: example: @alice:localhost
///     device_id: the device logging out, example: "RJYKSTBOIE"
///     all_devices: `/logout/all` semantics
#[implement(super::Service)]
pub async fn end_session(&self, user_id: &UserId, device_id: &DeviceId, all_devices: bool) {
	if all_devices {
		self.all_device_ids(user_id)
			.for_each(|device| self.remove_device(user_id, device))
			.await;
	} else {
		self.remove_device(user_id, device_id).await;
	}
}
