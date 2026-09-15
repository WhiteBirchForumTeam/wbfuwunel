use axum::extract::State;
use ruma::api::client::keys::get_key_changes;
use tuwunel_core::{Result, err};

use crate::Ruma;

/// # `GET /_matrix/client/r0/keys/changes`
///
/// Gets a list of users who have updated their device identity keys since the
/// previous sync token, and the users who no longer share an encrypted room.
///
/// - Same answer as the device-list catch-up of the wbf channel
///   (`docs/design/wbf-e2ee.md` §3.4.1, decision 5): memberships are read as
///   they are now, key changes between `from` and `to`.
pub(crate) async fn get_key_changes_route(
	State(services): State<crate::State>,
	body: Ruma<get_key_changes::v3::Request>,
) -> Result<get_key_changes::v3::Response> {
	let sender_user = body.sender_user();

	let from = body
		.from
		.parse()
		.map_err(|_| err!(Request(InvalidParam("Invalid `from`."))))?;

	let to = body
		.to
		.parse()
		.map_err(|_| err!(Request(InvalidParam("Invalid `to`."))))?;

	let changes = services
		.users
		.list_device_list_changes(sender_user, from, Some(to))
		.await;

	Ok(get_key_changes::v3::Response {
		changed: changes.changed.into_iter().collect(),
		left: changes.left.into_iter().collect(),
	})
}
