//! The one-time notice to a user whose client attaches media in encrypted
//! rooms without declaring it (see `media_refs::attachments`).
//!
//! Delivered as a plain `m.room.message` from the server user in a fresh
//! direct room, so any Matrix client shows it. Whether to send it, and only
//! once, is decided by `media_refs`; this only knows how to say it.

use futures::FutureExt;
use ruma::{
	RoomId, RoomVersionId, UserId,
	events::room::{
		create::RoomCreateEventContent,
		join_rules::{JoinRule, RoomJoinRulesEventContent},
		member::{MembershipState, RoomMemberEventContent},
		message::RoomMessageEventContent,
		name::RoomNameEventContent,
	},
};
use tuwunel_core::{Result, implement, info, matrix::pdu::PduBuilder};

use crate::media_refs::attachments::UNDECLARED_ATTACHMENTS_WARNING;

/// Opens a direct room from the server user to `user` and posts the warning.
///
/// Args:
///     user: example: @bob:localhost
/// Return:
///     Result  Err when any of the room events could not be built; the
///     caller logs it and moves on (the send it was triggered by is not
///     affected).
#[implement(super::Service)]
pub async fn send_attachment_warning(&self, user: &UserId) -> Result {
	let server_user = self.services.globals.server_user.as_ref();
	if !self.services.users.exists(server_user).await {
		super::create::create_server_user(&self.services).await?;
	}

	let room_id = RoomId::new_v1(self.services.globals.server_name());
	let _short_id = self
		.services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;
	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let append = |builder: PduBuilder| {
		self.services
			.timeline
			.build_and_append_pdu(builder, server_user, &room_id, &state_lock)
			.boxed()
	};

	append(PduBuilder::state(String::new(), &RoomCreateEventContent {
		federate: false,
		predecessor: None,
		room_version: RoomVersionId::V11,
		..RoomCreateEventContent::new_v11()
	}))
	.await?;
	append(PduBuilder::state(
		String::from(server_user),
		&RoomMemberEventContent::new(MembershipState::Join),
	))
	.await?;
	append(PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Invite))).await?;
	append(PduBuilder::state(
		String::new(),
		&RoomNameEventContent::new(format!("{} notices", self.services.config.server_name)),
	))
	.await?;
	append(PduBuilder::state(String::from(user), &RoomMemberEventContent {
		is_direct: true,
		..RoomMemberEventContent::new(MembershipState::Invite)
	}))
	.await?;
	append(PduBuilder::timeline(&RoomMessageEventContent::text_plain(UNDECLARED_ATTACHMENTS_WARNING))).await?;

	drop(state_lock);
	info!(%user, %room_id, "Sent the undeclared-attachments notice.");

	Ok(())
}
