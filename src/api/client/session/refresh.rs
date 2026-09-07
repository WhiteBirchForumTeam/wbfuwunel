use axum::extract::State;
use ruma::api::client::session::refresh_token::v3::{Request, Response};
use tuwunel_core::Result;

use crate::{ClientIp, Ruma};

/// # `POST /_matrix/client/v3/refresh`
///
/// Refresh an access token. The rules (rotation, expiry, replay) live in
/// `users::refresh_session`, shared with the wbf channel's `Refresh` pack.
///
/// <https://spec.matrix.org/v1.15/client-server-api/#post_matrixclientv3refresh>
#[tracing::instrument(skip_all, fields(%client), name = "refresh_token")]
pub(crate) async fn refresh_token_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	services.users.check_login_rate(client)?;

	let refreshed = services
		.users
		// HTTP has no connection to count.
		.refresh_session(&body.body.refresh_token, &mut tuwunel_service::users::login::admit_any)
		.await?;

	Ok(Response {
		access_token: refreshed.access_token,
		refresh_token: refreshed.refresh_token,
		expires_in_ms: refreshed.expires_in,
	})
}
