use std::collections::BTreeMap;

use axum::{extract::State, http::HeaderMap};
use futures::{FutureExt, future::try_join4};
use ruma::{
	DeviceId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, TransactionId, UserId,
	api::client::message::send_message_event,
	events::{
		AnyMessageLikeEventContent, MessageLikeEventType,
		reaction::ReactionEventContent,
		room::{encrypted::Relation, redaction::RoomRedactionEventContent},
	},
	serde::Raw,
};
use serde::Deserialize;
use serde_json::from_str;
use tuwunel_core::{
	Err, Result, debug_warn, err,
	matrix::{Event, pdu::PduBuilder},
	utils::{self},
	warn,
};
use tuwunel_service::{Services, appservice::RegistrationInfo};

use crate::{Ruma, client::utils::is_self_redaction};

#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

/// # `PUT /_matrix/client/v3/rooms/{roomId}/send/{eventType}/{txnId}`
///
/// Send a message event into the room.
///
/// - Is a NOOP if the txn id was already used before and returns the same event
///   id again
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is
///   allowed
/// - `X-Wbf-Attachments: mxc://a,mxc://b` declares the media the event
///   attaches, which the server cannot read out of encrypted content; see
///   `docs/design/media-attachments.md`.
pub(crate) async fn send_message_event_route(
	State(services): State<crate::State>,
	body: Ruma<send_message_event::v3::Request>,
) -> Result<send_message_event::v3::Response> {
	let declared = declared_attachments_from_header(&body.headers);
	let send = SendMessageEvent {
		sender_user: body.sender_user(),
		sender_device: body.sender_device.as_deref(),
		appservice_info: body.appservice_info.as_ref(),
		room_id: &body.room_id,
		event_type: &body.event_type,
		txn_id: &body.txn_id,
		content: &body.body.body,
		timestamp: body.timestamp,
		declared_attachments: declared,
		via_legacy_http: true,
		may_write_reserved_type: false,
	};

	let event_id = send_message_event(&services, send).await?;

	Ok(send_message_event::v3::Response { event_id })
}

/// The header a client sets on the legacy send endpoint to declare the media
/// the event attaches: comma-separated `mxc://` URIs.
pub(crate) const ATTACHMENTS_HEADER: &str = "x-wbf-attachments";

/// Args:
///     headers: the request headers, example: `X-Wbf-Attachments: mxc://a/1, mxc://a/2`
/// Return:
///     Vec<String>  the entries, trimmed, empty ones dropped; empty when the
///     header is absent or not valid text.
pub(crate) fn declared_attachments_from_header(headers: &HeaderMap) -> Vec<String> {
	headers
		.get_all(ATTACHMENTS_HEADER)
		.iter()
		.filter_map(|value| value.to_str().ok())
		.flat_map(|value| value.split(','))
		.map(str::trim)
		.filter(|entry| !entry.is_empty())
		.map(ToOwned::to_owned)
		.collect()
}

/// One send, from either transport (the legacy HTTP route or the wbf
/// `Event/Send` pack); both end up here so there is one set of rules.
pub(crate) struct SendMessageEvent<'a> {
	pub(crate) sender_user: &'a UserId,
	pub(crate) sender_device: Option<&'a DeviceId>,
	pub(crate) appservice_info: Option<&'a RegistrationInfo>,
	pub(crate) room_id: &'a RoomId,
	pub(crate) event_type: &'a MessageLikeEventType,
	pub(crate) txn_id: &'a TransactionId,
	pub(crate) content: &'a Raw<AnyMessageLikeEventContent>,
	pub(crate) timestamp: Option<MilliSecondsSinceUnixEpoch>,
	/// The media the sender says this event attaches; checked here.
	pub(crate) declared_attachments: Vec<String>,
	/// Whether this came through the legacy HTTP endpoint: an encrypted send
	/// from there with nothing declared is what the one-time warning is for.
	pub(crate) via_legacy_http: bool,
	/// Set only by `Stream/Draft`, which is the one caller allowed to write a
	/// draft anchor. See `RESERVED_EVENT_TYPES`.
	pub(crate) may_write_reserved_type: bool,
}

/// Event types this server writes itself and refuses from clients.
///
/// 🚨 A draft anchor is not just an event: it is a standing permission to
/// broadcast into its room until it is abandoned, and `Stream/Draft` is where
/// the policy for handing that out lives (the room-size cap). A client that
/// could write the same type through the ordinary send path would hold the
/// permission without ever passing the policy — the cap would bind only the
/// clients that chose to ask for it (external review 2026-09-12, R6).
///
/// ⚠️ Federation is not a hole here: an anchor written elsewhere carries a
/// remote sender, and a piece is refused unless its sender is the anchor's.
const RESERVED_EVENT_TYPES: [&str; 1] = ["org.wbftw.wbfuwunel.draft"];

/// Args:
///     send: see `SendMessageEvent`
/// Return:
///     Result<OwnedEventId>  the event id (the earlier one for a repeated
///     transaction id); Err for a refused declaration (400, naming the
///     attachment), or anything the event itself is refused for.
pub(crate) async fn send_message_event(
	services: &Services,
	send: SendMessageEvent<'_>,
) -> Result<OwnedEventId> {
	let SendMessageEvent {
		sender_user,
		sender_device,
		appservice_info,
		room_id,
		event_type,
		txn_id,
		content: body,
		timestamp,
		declared_attachments,
		via_legacy_http,
		may_write_reserved_type,
	} = send;

	// Forbid m.room.encrypted if encryption is disabled
	if *event_type == MessageLikeEventType::RoomEncrypted && !services.config.allow_encryption {
		return Err!(Request(Forbidden("Encryption has been disabled")));
	}

	if !may_write_reserved_type && RESERVED_EVENT_TYPES.contains(&event_type.to_string().as_str()) {
		return Err!(Request(Forbidden(
			"This event type is written by the server for its own protocol; send it through the \
			 command that owns it."
		)));
	}

	// A repeated transaction id answers with the stored event before anything
	// else is looked at, so a retry stays idempotent even if what it declares
	// has changed or been removed since the first send.
	if let Some(existing) = check_existing_txnid(services, sender_user, sender_device, txn_id).await {
		return existing.map(|response| response.event_id);
	}

	// Checked before anything is written: a refused attachment refuses the
	// whole send, so a client bug shows up here and not as a message pointing
	// at media that will be swept.
	let attachments = services
		.media_refs
		.check_attachments(sender_user, &declared_attachments)
		.await
		.map_err(|error| err!(Request(InvalidParam("{error}"))))?;

	// MSC4169: clients sending m.room.redaction via /send put `redacts` in
	// `content`. Pre-v11 auth rules read it from the top level; lift it so
	// `redacts_id(...)` resolves regardless of room version. Mirrors the
	// /redact handler.
	let redaction_content = || {
		body.deserialize_as_unchecked::<RoomRedactionEventContent>()
			.inspect_err(|_| {
				debug_warn!(
					%sender_user,
					event = %body.json(),
					"Client sent invalid redaction event"
				);
			})
			.ok()
	};

	let redacts_id = event_type
		.eq(&MessageLikeEventType::RoomRedaction)
		.then(redaction_content)
		.flatten()
		.and_then(|content| content.redacts);

	if *event_type == MessageLikeEventType::RoomRedaction
		&& services.config.disable_local_redactions
		&& !services.admin.user_is_admin(sender_user).await
	{
		warn!(
			%sender_user,
			?redacts_id,
			"Local redactions are disabled, non-admin user attempted to redact an event"
		);

		return Err!(Request(Forbidden("Redactions are disabled on this server.")));
	}

	if services.users.is_suspended(sender_user).await {
		if *event_type != MessageLikeEventType::RoomRedaction {
			return Err!(Request(UserSuspended(
				"Cannot send non-redaction events while suspended."
			)));
		}

		let is_self = match &redacts_id {
			| None => false,
			| Some(redacts_id) => is_self_redaction(services, sender_user, redacts_id).await,
		};

		if !is_self {
			return Err!(Request(UserSuspended("Can only redact own events while suspended.")));
		}
	}

	let state_lock = services.state.mutex.lock(room_id).await;

	let (existing_txnid, ..) = try_join4(
		check_existing_txnid(services, sender_user, sender_device, txn_id).map(Ok),
		check_duplicate_reaction(services, event_type, sender_user, body),
		check_public_call_invite(services, event_type, room_id),
		check_nested_thread(services, body),
	)
	.await?;

	if let Some(existing_txnid) = existing_txnid {
		return existing_txnid.map(|response| response.event_id);
	}

	let mut unsigned = BTreeMap::new();
	unsigned.insert("transaction_id".to_owned(), txn_id.to_string().into());

	let content = from_str(body.json().get())
		.map_err(|e| err!(Request(BadJson("Invalid JSON body: {e}"))))?;

	let is_undeclared_encrypted = via_legacy_http
		&& *event_type == MessageLikeEventType::RoomEncrypted
		&& attachments.is_empty();

	let event_id = services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: event_type.clone().into(),
				content,
				unsigned: Some(unsigned),
				timestamp: appservice_info.and(timestamp),
				redacts: redacts_id,
				attachments,
				..Default::default()
			},
			sender_user,
			room_id,
			&state_lock,
		)
		.await?;

	services.transaction_ids.add_txnid(
		sender_user,
		sender_device,
		txn_id,
		event_id.as_bytes(),
	);

	drop(state_lock);

	// After the send, never instead of it: a client that does not declare is
	// told once, if it looks like it just attached something.
	if is_undeclared_encrypted {
		services
			.media_refs
			.warn_if_undeclared_attachments(sender_user)
			.await;
	}

	Ok(event_id)
}

async fn check_public_call_invite(
	services: &Services,
	event_type: &MessageLikeEventType,
	room_id: &RoomId,
) -> Result {
	if *event_type != MessageLikeEventType::CallInvite {
		return Ok(());
	}

	if !services.directory.is_public_room(room_id).await {
		return Ok(());
	}

	Err!(Request(Forbidden("Room call invites are not allowed in public rooms")))
}

// Forbid duplicate reactions
async fn check_duplicate_reaction(
	services: &Services,
	event_type: &MessageLikeEventType,
	sender_user: &UserId,
	body: &Raw<AnyMessageLikeEventContent>,
) -> Result {
	if *event_type != MessageLikeEventType::Reaction {
		return Ok(());
	}

	let Ok(content) = body.deserialize_as_unchecked::<ReactionEventContent>() else {
		return Ok(());
	};

	if !services
		.pdu_metadata
		.event_has_relation(
			&content.relates_to.event_id,
			Some(sender_user),
			None,
			Some(&content.relates_to.key),
		)
		.await
	{
		return Ok(());
	}

	Err!(Request(DuplicateAnnotation("Duplicate reactions are not allowed.")))
}

// MSC3440/Matrix 1.4: a thread may only target an event which itself carries
// no rel_type; the spec assigns this rejection 400 M_UNKNOWN.
async fn check_nested_thread(
	services: &Services,
	body: &Raw<AnyMessageLikeEventContent>,
) -> Result {
	let Ok(ExtractRelatesTo { relates_to: Relation::Thread(thread) }) =
		body.deserialize_as_unchecked()
	else {
		return Ok(());
	};

	let Ok(root) = services.timeline.get_pdu(&thread.event_id).await else {
		return Ok(());
	};

	let nested = root
		.get_content()
		.is_ok_and(|content: ExtractRelatesTo| content.relates_to.rel_type().is_some());

	if !nested {
		return Ok(());
	}

	Err!(Request(Unknown("Cannot start threads from an event with a relation.")))
}

/// Check if this is a new transaction id. Returns Some when the transaction id
/// exists and the send must then be terminated by returning the contained
/// result.
async fn check_existing_txnid(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	txn_id: &TransactionId,
) -> Option<Result<send_message_event::v3::Response>> {
	let Ok(response) = services
		.transaction_ids
		.existing_txnid(sender_user, sender_device, txn_id)
		.await
	else {
		return None;
	};

	// The client might have sent a txnid of the /sendToDevice endpoint
	// This txnid has no response associated with it
	if response.is_empty() {
		return Some(Err!(Request(InvalidParam(
			"Tried to use txn_id already used for an incompatible endpoint."
		))));
	}

	let Ok(Ok(event_id)) = utils::string_from_bytes(&response).map(TryInto::try_into) else {
		return Some(Err!(Database("Invalid event_id in txn_id data: {response:?}.")));
	};

	Some(Ok(send_message_event::v3::Response { event_id }))
}
