use ruma::{EventId, OwnedEventId, RoomId, UserId, events::room::redaction::RoomRedactionEventContent};
use tuwunel_core::{Err, Event, Result, matrix::pdu::PduBuilder, warn};
use tuwunel_service::Services;

pub(crate) async fn invite_check(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
) -> Result {
	if services.config.block_non_admin_invites && !services.admin.user_is_admin(sender_user).await
	{
		warn!("{sender_user} is not an admin and attempted to send an invite to {room_id}");
		return Err!(Request(Forbidden("Invites are not allowed on this server.")));
	}

	Ok(())
}

pub(crate) async fn is_self_redaction(
	services: &Services,
	user_id: &UserId,
	event_id: &EventId,
) -> bool {
	services
		.timeline
		.get_pdu(event_id)
		.await
		.is_ok_and(|target| target.sender() == user_id)
}

/// Redacts one event as `sender_user`, with the policy both redaction paths
/// share: the server-wide switch, and suspension (which still lets a
/// suspended account take back its own event).
///
/// ⚠️ One implementation on purpose: `PUT …/redact/…` and the wbf channel's
/// `Stream/Abandon` are two doors into the same act, and a server that
/// disabled local redactions means it for both of them.
///
/// Args:
///     sender_user: who is redacting, example: @alice:localhost
///     room_id: the event's room
///     event_id: what to redact
///     reason: example: Some("abandoned draft".to_owned())
/// Return:
///     Result<OwnedEventId>  the redaction event's id; `Forbidden` when
///     redactions are disabled here, `UserSuspended` when a suspended
///     account redacts somebody else's event.
pub(crate) async fn redact_event_as(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	event_id: &EventId,
	reason: Option<String>,
) -> Result<OwnedEventId> {
	if services.config.disable_local_redactions
		&& !services.admin.user_is_admin(sender_user).await
	{
		warn!(
			%sender_user,
			%event_id,
			"Local redactions are disabled, non-admin user attempted to redact an event"
		);
		return Err!(Request(Forbidden("Redactions are disabled on this server.")));
	}

	if services.users.is_suspended(sender_user).await
		&& !is_self_redaction(services, sender_user, event_id).await
	{
		return Err!(Request(UserSuspended("Account is suspended.")));
	}

	let state_lock = services.state.mutex.lock(room_id).await;
	let redaction_id = services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				redacts: Some(event_id.to_owned()),
				..PduBuilder::timeline(&RoomRedactionEventContent {
					redacts: Some(event_id.to_owned()),
					reason,
				})
			},
			sender_user,
			room_id,
			&state_lock,
		)
		.await?;
	drop(state_lock);

	Ok(redaction_id)
}
