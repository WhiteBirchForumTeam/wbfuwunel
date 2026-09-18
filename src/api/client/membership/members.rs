use axum::{Json, extract::State};
use futures::{FutureExt, StreamExt, pin_mut};
use ruma::{
	api::client::membership::{
		get_member_events::{self},
		joined_members::{self, v3::RoomMember},
	},
	UserId,
	events::{
		StateEvent, StateEventType,
		room::{
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	serde::Raw,
};
use serde_json::{Value, json};
use tuwunel_core::{
	Err, Result, at, err, is_equal_to, is_not_equal_to,
	matrix::Event,
	utils::{
		future::{BoolExt, TryExtExt},
		stream::ReadyExt,
	},
};

use crate::Ruma;

/// Where `get_member_events_route` is served: registered by hand because its
/// answer is not ruma's type, so these must stay the paths ruma lists (a test
/// holds them to it).
pub(crate) const MEMBER_EVENTS_PATHS: [&str; 2] =
	["/_matrix/client/r0/rooms/{room_id}/members", "/_matrix/client/v3/rooms/{room_id}/members"];

/// # `GET /_matrix/client/r0/rooms/{roomId}/members`
///
/// The room's member events, filtered by `membership` / `not_membership`
/// (`at` is ignored, as upstream). Two fields more than Matrix, which a client
/// that does not know them ignores (docs/design/wbf-room-device-version.md
/// §5): each joined member's `unsigned["org.wbftw.device_version"]`, and the
/// room's `org.wbftw.room_version` at the top.
///
/// - Only works if the user is currently joined
pub(crate) async fn get_member_events_route(
	State(services): State<crate::State>,
	body: Ruma<get_member_events::v3::Request>,
) -> Result<Json<Value>> {
	if !services
		.state_accessor
		.user_can_see_state_events(body.sender_user(), &body.room_id)
		.await
	{
		return Err!(Request(Forbidden(
			"You aren't a member of the room and weren't previously a member of the room."
		)));
	}

	let membership = body.membership.as_ref();
	let not_membership = body.not_membership.as_ref();
	let membership_filter = |content: &RoomMemberEventContent| {
		membership.is_none_or(is_equal_to!(&content.membership))
			&& not_membership.is_none_or(is_not_equal_to!(&content.membership))
	};

	// 🚨 One read of the room state for both the list and the room version,
	// so a client never gets a list of one moment with the version of another.
	let shortstatehash = services
		.state
		.get_room_shortstatehash(&body.room_id)
		.await
		.map_err(|e| err!(Database("Missing state for {:?}: {e:?}", body.room_id)))?;
	let member_events: Vec<_> = services
		.state_accessor
		.state_full(shortstatehash)
		.ready_filter(|((ty, _), _)| *ty == StateEventType::RoomMember)
		.map(|(_, pdu)| pdu)
		.collect()
		.boxed()
		.await;

	let room_version = services
		.device_versions
		.compute_room_device_version(&member_events)
		.await?;

	let mut chunk = Vec::new();
	for pdu in &member_events {
		let Ok(content) = pdu.get_content::<RoomMemberEventContent>() else {
			continue;
		};
		if !membership_filter(&content) {
			continue;
		}

		// The format the ruma answer used, so an event reads the same as before
		// but for the one field added.
		let event: Raw<StateEvent<RoomMemberEventContent>> = pdu.to_format();
		let mut event: Value = serde_json::from_str(event.json().get())?;
		if content.membership == MembershipState::Join {
			let member = pdu
				.state_key()
				.and_then(|key| UserId::parse(key).ok())
				.ok_or_else(|| err!(Database("member event {} has no user", pdu.event_id())))?;
			let device_version = services
				.device_versions
				.get_device_version(&member)
				.await?;
			event["unsigned"]["org.wbftw.device_version"] = json!(device_version.to_wire());
		}
		chunk.push(event);
	}

	Ok(Json(json!({ "chunk": chunk, "org.wbftw.room_version": room_version })))
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/joined_members`
///
/// Lists all members of a room.
///
/// - The sender user must be in the room
/// - TODO: An appservice just needs a puppet joined
pub(crate) async fn joined_members_route(
	State(services): State<crate::State>,
	body: Ruma<joined_members::v3::Request>,
) -> Result<joined_members::v3::Response> {
	let is_joined = services
		.state_cache
		.is_joined(body.sender_user(), &body.room_id);

	let is_world_readable = services
		.state_accessor
		.room_state_get_content(&body.room_id, &StateEventType::RoomHistoryVisibility, "")
		.map_ok_or(false, |c: RoomHistoryVisibilityEventContent| {
			c.history_visibility == HistoryVisibility::WorldReadable
		});

	pin_mut!(is_joined, is_world_readable);
	if !is_joined.or(is_world_readable).await {
		return Err!(Request(Forbidden("You aren't a member of the room.")));
	}

	Ok(joined_members::v3::Response {
		joined: services
			.state_accessor
			.room_state_full(&body.room_id)
			.ready_filter_map(Result::ok)
			.ready_filter(|((ty, _), _)| *ty == StateEventType::RoomMember)
			.map(at!(1))
			.ready_filter_map(|pdu| {
				let content = pdu.get_content::<RoomMemberEventContent>().ok()?;

				let matches = content.membership == MembershipState::Join;

				matches.then(|| {
					let sender = pdu.sender().to_owned();
					let member = RoomMember {
						display_name: content.displayname,
						avatar_url: content.avatar_url,
					};

					(sender, member)
				})
			})
			.collect()
			.boxed()
			.await,
	})
}

#[cfg(test)]
mod tests {
	use ruma::api::{Metadata, client::membership::get_member_events, path_builder::PathBuilder};

	use super::MEMBER_EVENTS_PATHS;

	/// The route is registered by hand with `MEMBER_EVENTS_PATHS`; if ruma
	/// adds or renames a path, the hand-written list must follow.
	#[test]
	fn the_hand_registered_paths_are_the_ones_ruma_lists() {
		let mut from_ruma: Vec<&str> = get_member_events::v3::Request::PATH_BUILDER
			.all_paths()
			.collect();
		let mut registered = MEMBER_EVENTS_PATHS.to_vec();
		from_ruma.sort_unstable();
		registered.sort_unstable();

		assert_eq!(from_ruma, registered);
	}
}
