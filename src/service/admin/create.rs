use std::collections::BTreeMap;

use futures::FutureExt;
use ruma::{
	RoomId, RoomVersionId,
	events::room::{
		canonical_alias::RoomCanonicalAliasEventContent,
		create::RoomCreateEventContent,
		guest_access::{GuestAccess, RoomGuestAccessEventContent},
		history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
		join_rules::{JoinRule, RoomJoinRulesEventContent},
		name::RoomNameEventContent,
		power_levels::RoomPowerLevelsEventContent,
		preview_url::RoomPreviewUrlsEventContent,
		topic::{RoomTopicEventContent, TopicContentBlock},
	},
};
use tuwunel_core::{Err, Result, matrix::room_version, pdu::PduBuilder, warn};

use crate::{Services, profile::Propagation};

/// The `global` key naming the server user this server created itself. An
/// account under the server user's name without it was made by someone else
/// (`docs/design/server-user.md` §4).
const SERVER_USER_MARKER: &[u8] = b"server_user";

/// Creates the server user: the marker, the account, its displayname. The
/// only place it is created.
///
/// Return:
///     Result  Err when an account of that name already exists: creating
///     over it would reset its password and make its owner an admin
pub async fn create_server_user(services: &Services) -> Result {
	let server_user = services.globals.server_user.as_ref();
	if services.users.exists(server_user).await {
		return Err!("{server_user} already exists; the server user is not created over it");
	}

	// The marker goes first: a crash after it and before the account leaves a
	// name the next start creates, never an account the next start refuses.
	services.db["global"].insert(SERVER_USER_MARKER, server_user.as_str());

	services
		.users
		.create(server_user, None, None)
		.await?;

	services
		.profile
		.set_displayname(server_user, Some(&services.globals.server_user_displayname()), Some(Propagation::None))
		.await
}

/// Startup gate for the server user, after migrations and before any worker:
/// creates it when missing, so its name is taken from the first moment;
/// refuses to start when an account of that name was not created by this
/// server; brings its displayname up to date.
///
/// Return:
///     Result  Err (the server does not start) when the account exists
///     without this server's marker
pub async fn ensure_server_user(services: &Services) -> Result {
	let server_user = services.globals.server_user.as_ref();
	let read_only = services.globals.is_read_only();

	if !services.users.exists(server_user).await {
		if read_only {
			warn!(%server_user, "The server user does not exist and cannot be created in read-only mode");
			return Ok(());
		}

		return create_server_user(services).await;
	}

	let is_created_by_this_server = services.db["global"]
		.get(SERVER_USER_MARKER)
		.await
		.is_ok_and(|marker| *marker == *server_user.as_bytes());
	if !is_created_by_this_server {
		return Err!(
			"{server_user} is an account this server did not create, and the server user would make its \
			 owner an admin. Refusing to start. See docs/design/server-user.md"
		);
	}

	let displayname = services.globals.server_user_displayname();
	if services.profile.displayname(server_user).await.ok().as_deref() == Some(displayname.as_str()) {
		return Ok(());
	}

	if read_only {
		warn!(%server_user, %displayname, "The server user's displayname is out of date and cannot be set in read-only mode");
		return Ok(());
	}

	services
		.profile
		.set_displayname(server_user, Some(&displayname), Some(Propagation::All))
		.await
}

/// Create the admin room.
///
/// Users in this room are considered admins by tuwunel, and the room can be
/// used to issue admin commands by talking to the server user inside it.
pub async fn create_admin_room(services: &Services) -> Result {
	let room_id = RoomId::new_v1(services.globals.server_name());
	let room_version_id = RoomVersionId::V11;

	let room_version_rules = room_version::rules(&room_version_id)?;

	let _short_id = services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.state.mutex.lock(&room_id).await;

	// Create a user for the server
	let server_user = services.globals.server_user.as_ref();
	if !services.users.exists(server_user).await {
		create_server_user(services).await?;
	}

	let create_content = if !room_version_rules
		.authorization
		.use_room_create_sender
	{
		RoomCreateEventContent::new_v1(server_user.into())
	} else {
		RoomCreateEventContent::new_v11()
	};

	// 1. The room create event
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCreateEventContent {
				federate: services.config.federate_admin_room,
				predecessor: None,
				room_version: room_version_id.clone(),
				..create_content
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 2. Make server user/bot join
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::from(server_user),
				&services.globals.server_user_join(),
			),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 3. Power levels
	let users = BTreeMap::from_iter([(server_user.into(), 69420.into())]);

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomPowerLevelsEventContent {
				users,
				..Default::default()
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4.1 Join Rules
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Invite)),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4.2 History Visibility
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared),
			),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 4.3 Guest Access
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(
				String::new(),
				&RoomGuestAccessEventContent::new(GuestAccess::Forbidden),
			),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 5. Events implied by name and topic
	let room_name = format!("{} Admin Room", services.config.server_name);
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomNameEventContent::new(room_name)),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomTopicEventContent {
				topic_block: TopicContentBlock::default(),
				topic: format!("Manage {} | Run commands prefixed with `!admin` | Run `!admin -h` for help | Documentation: https://matrix-construct.github.io/tuwunel", services.config.server_name),
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	// 6. Room alias
	let alias = &services.admin.admin_alias;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomCanonicalAliasEventContent {
				alias: Some(alias.clone()),
				alt_aliases: Vec::new(),
			}),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	services.alias.set_alias(alias, &room_id)?;

	// 7. (ad-hoc) Disable room URL previews for everyone by default
	services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &RoomPreviewUrlsEventContent { disabled: true }),
			server_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	Ok(())
}
