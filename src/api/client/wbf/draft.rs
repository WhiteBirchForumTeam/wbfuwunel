//! `0x02 Stream`: a message that is still being written
//! (`docs/design/streaming-messages.md`).
//!
//! A draft is anchored by one real, persisted event — `Draft` writes it, and
//! its `g_seq` is the draft's id — and everything after that is momentary:
//! `Keypoint`, `Delta` and `Append` are broadcast to the room's subscribers
//! and never stored, so the history of a draft is two entries, the redacted
//! anchor and the finished message, rather than one event per keystroke.
//!
//! ⭐ **The server keeps no draft state.** Who may write to a draft, and
//! whether it is still open, are read from the anchor on every piece. That is
//! one point read per piece, deliberately not cached: a cache here *is* draft
//! state on the server, and the version of this design that had server state
//! was thrown away for it.
//!
//! The server does not read a piece's `data` (it is ciphertext in an
//! encrypted room) and does not interpret the piece counter in the header's
//! `seq`. It carries them to the room unchanged. What it does guarantee is
//! **who wrote them**: a piece from anyone but the anchor's sender is refused
//! and never forwarded, so a receiver does not have to check.

use ruma::{
	EventId, OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, UserId,
	events::MessageLikeEventType,
	serde::Raw,
};
use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Event, debug,
	matrix::pdu::{PduCount, PduId, RawPduId},
	wbf::{Flags, Kind, PackBuilder, PackView, RejectCode},
};
use tuwunel_service::Services;

use super::{Failure, PackContext, Reject, Reply, ack};
use crate::client::{
	send::{SendMessageEvent, send_message_event},
	utils::redact_event_as,
};

/// `Stream` subtypes.
pub(super) mod stream {
	pub(in super::super) const DRAFT: u8 = 0x01;
	pub(in super::super) const ABANDON: u8 = 0x02;
	pub(in super::super) const KEYPOINT: u8 = 0x03;
	pub(in super::super) const DELTA: u8 = 0x04;
	pub(in super::super) const APPEND: u8 = 0x05;
	pub(in super::super) const DEMAND: u8 = 0x10;
}

/// The anchor's event type, and its `msgtype`. ⚠️ Not `m.room.message`: a
/// plaintext message event in an encrypted room makes other clients warn
/// about an unencrypted message, while an unknown type is simply not shown.
const DRAFT_EVENT_TYPE: &str = "org.wbftw.wbfuwunel.draft";

/// What the anchor's body says, for tools that do not know the type.
const DRAFT_BODY: &str = "(draft)";

/// Routes one `Stream` pack. Every subtype needs the room, and every subtype
/// but `Draft` needs the anchor, so the shared parsing happens here once.
pub(super) async fn handle(
	services: &Services,
	ctx: &PackContext<'_>,
	subtype: u8,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let room_id = parse_room_id(view)?;
	refuse_unless_flagless(view)?;

	match subtype {
		| stream::DRAFT => open_draft(services, ctx, &room_id, view, reply).await,
		| stream::ABANDON => abandon_draft(services, ctx, &room_id, view, reply).await,
		| stream::KEYPOINT | stream::DELTA | stream::APPEND =>
			relay_piece(services, ctx, &room_id, view).await,
		| stream::DEMAND => relay_demand(services, ctx, &room_id, view).await,
		// The admission table let a subtype through that this match does not
		// route: they disagree, which is a bug, but it fails closed.
		| _ => Err(Reject::code(RejectCode::UnknownKind, "no handler for this Stream subtype").into()),
	}
}

/// The meta of every `Stream` pack is the room id itself — UTF-8 text, not
/// JSON, because the only other thing a piece needs (which draft) is already
/// the header's `id`.
///
/// Args:
///     view: meta example: `!abc:localhost`
/// Return:
///     Result<OwnedRoomId, Reject>  `InvalidRequest` when the meta is not a
///     room id. ⚠️ The design document says `Conflict` here; it predates the
///     error vocabulary (wire-format §3.4), where a field that is not what
///     the subtype takes is `InvalidRequest` and `Conflict` is for state.
fn parse_room_id(view: &PackView<'_>) -> Result<OwnedRoomId, Reject> {
	let text = std::str::from_utf8(view.meta)
		.map_err(|_| Reject::code(RejectCode::InvalidRequest, "Stream meta is not UTF-8 text"))?;

	RoomId::parse(text).map_err(|error| {
		Reject::code(
			RejectCode::InvalidRequest,
			format!("Stream meta is the room id itself, not JSON: {error}"),
		)
	})
}

/// Every `Stream` pack has `flags = 0`, both directions (維護者 2026-09-12).
///
/// ⚠️ All four defined flags say something about the pack's relationship to
/// the one receiving it, and for this kind every one of them is false:
/// `META_ENCRYPTED` cannot be set because the meta is the room id the server
/// has to read; pieces are never acknowledged; a piece is nobody's response;
/// and a draft has no observable last piece. Refusing them here rather than
/// masking them on relay keeps "relayed as it is" literally true — a pack
/// that would have to be changed is not relayed at all — and tells the
/// client with the mistake, instead of passing it to the whole room.
fn refuse_unless_flagless(view: &PackView<'_>) -> Result<(), Reject> {
	if view.header.flags == Flags::default() {
		return Ok(());
	}

	Err(Reject::code(
		RejectCode::InvalidRequest,
		"a Stream pack sets no flags: its meta is plaintext, it is not a response, it is not \
		 acknowledged, and a draft has no last piece",
	))
}

/// `Stream/Draft`: write the anchor and answer with the draft's id.
async fn open_draft(
	services: &Services,
	ctx: &PackContext<'_>,
	room_id: &RoomId,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let session = ctx
		.session
		.ok_or_else(|| Reject::code(RejectCode::Unauthorized, "log in first: this connection has no session"))?;

	refuse_oversized_payload(view)?;
	refuse_room_too_large(services, room_id).await?;

	// The anchor goes through the ordinary send: same idempotency, same
	// append, so it has an `r_seq` and a `g_seq`, is pushed to subscribers,
	// and `Recent` returns it like any other event.
	//
	// ⚠️ The transaction id is this connection and request, not a client's:
	// `Stream/Draft` has no `txn_id` field. Two Drafts are two drafts, which
	// is why it must not collide with an earlier one — a collision silently
	// answers with an anchor that may already be abandoned, or be in another
	// room entirely.
	//
	// The connection's name here is `connection_tag`, not the bare number:
	// transaction ids are stored, the numbers restart with the process, and
	// the first draft after a restart was answering with the one from before
	// it (external review 2026-09-12, R1). Within one run the name is stable,
	// so re-sending a request with the same `seq` is still the replay the
	// design document describes.
	let txn_id: OwnedTransactionId = format!(
		"wbf-draft-{}-{}",
		services.streams.connection_tag(ctx.connection),
		view.header.seq
	)
	.into();
	let content = to_raw_value(&json!({ "msgtype": DRAFT_EVENT_TYPE, "body": DRAFT_BODY }))
		.map_err(|error| Reject::code(RejectCode::Internal, format!("draft anchor content: {error}")))?;
	let content = Raw::from_json(content);
	let event_type = MessageLikeEventType::from(DRAFT_EVENT_TYPE);

	let event_id = send_message_event(services, SendMessageEvent {
		sender_user: &session.user,
		sender_device: Some(&session.device),
		appservice_info: None,
		room_id,
		event_type: &event_type,
		txn_id: &txn_id,
		content: &content,
		timestamp: None,
		declared_attachments: Vec::new(),
		via_legacy_http: false,
		// The one caller that may: this is the command the anchor's policy
		// (the room-size cap) lives in.
		may_write_reserved_type: true,
	})
	.await
	.map_err(Reject::from)?;

	let g_seq = read_g_seq(services, &event_id).await?;

	reply
		.send(ack(
			view.header.id,
			view.header.seq,
			json!({ "event_id": event_id, "g_seq": g_seq }),
			Vec::new(),
		))
		.await?;
	Ok(())
}

/// `Stream/Abandon`: redact the anchor. Subscribers learn of it the ordinary
/// way, from the redaction's own `Push`.
async fn abandon_draft(
	services: &Services,
	ctx: &PackContext<'_>,
	room_id: &RoomId,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let user = ctx.user()?;
	refuse_oversized_payload(view)?;
	refuse_unless_member(services, user, room_id).await?;
	let anchor = read_open_draft(services, room_id, view.header.id).await?;
	refuse_unless_author(&anchor, user)?;

	let redaction_event_id = redact_event_as(services, user, room_id, &anchor.event_id, None)
		.await
		.map_err(Reject::from)?;

	reply
		.send(ack(
			view.header.id,
			view.header.seq,
			json!({ "redaction_event_id": redaction_event_id }),
			Vec::new(),
		))
		.await?;
	Ok(())
}

/// `Stream/Keypoint`, `Delta`, `Append`: the author's content, carried to the
/// room unchanged and never stored.
///
/// ⚠️ The order of the checks is part of the design, not an accident:
/// **membership comes before the anchor is read**. Reading first and refusing
/// afterwards told a stranger, through the difference between `NotFound`,
/// `Conflict` and `Forbidden`, whether a given `(room, g_seq)` exists and
/// whether it is an open draft — in a room they cannot read (external review
/// 2026-09-12, R7).
async fn relay_piece(
	services: &Services,
	ctx: &PackContext<'_>,
	room_id: &RoomId,
	view: &PackView<'_>,
) -> Result<(), Failure> {
	let session = ctx
		.session
		.ok_or_else(|| Reject::code(RejectCode::Unauthorized, "log in first: this connection has no session"))?;

	if view.data.len() > services.config.wbf_draft_max_piece_bytes {
		return Err(Reject::code(
			RejectCode::TooLarge,
			format!(
				"a draft piece carries at most {} bytes, this one carries {}",
				services.config.wbf_draft_max_piece_bytes,
				view.data.len()
			),
		)
		.into());
	}
	refuse_unless_chained(view)?;
	refuse_unless_member(services, &session.user, room_id).await?;

	let anchor = read_open_draft(services, room_id, view.header.id).await?;
	refuse_unless_author(&anchor, &session.user)?;
	refuse_unless_allowed_to_speak(services, &session.user, room_id).await?;

	services
		.drafts
		.take_piece(
			&session.user,
			&session.device,
			services.config.wbf_draft_pieces_per_second,
			services.config.wbf_draft_pieces_burst,
		)
		.map_err(|retry_after| retry_later("too many draft pieces", retry_after))?;

	broadcast(services, room_id, &session.user, view, view.data).await;
	Ok(())
}

/// `Stream/Demand`: anyone in the room may ask for the whole draft again.
/// It is broadcast like every other piece — the device writing the draft
/// answers with a `Keypoint`, everyone else ignores it — because routing it
/// to the author alone would be a second rule for no gain.
async fn relay_demand(
	services: &Services,
	ctx: &PackContext<'_>,
	room_id: &RoomId,
	view: &PackView<'_>,
) -> Result<(), Failure> {
	let session = ctx
		.session
		.ok_or_else(|| Reject::code(RejectCode::Unauthorized, "log in first: this connection has no session"))?;

	refuse_oversized_payload(view)?;
	refuse_unless_member(services, &session.user, room_id).await?;

	// The anchor is read after membership (R7) and for the same reason as a
	// piece's: an id nobody can resolve is `NotFound`, and a closed draft is
	// not worth a broadcast.
	let _anchor = read_open_draft(services, room_id, view.header.id).await?;

	services
		.drafts
		.take_demand(
			&session.user,
			&session.device,
			services.config.wbf_draft_demands_per_second,
			services.config.wbf_draft_demands_burst,
		)
		.map_err(|retry_after| retry_later("too many draft demands", retry_after))?;

	// ⚠️ Relayed with **no data**: a `Demand` asks for something, it does not
	// carry anything, and whatever a client put there would otherwise be
	// copied to every connection in the room (external review 2026-09-12,
	// R3). Stripping is unconditional; a payload large enough to be an
	// attempt rather than a slip is refused above.
	broadcast(services, room_id, &session.user, view, &[]).await;
	Ok(())
}

/// The anchor of an open draft: what the checks need, read fresh every time.
struct DraftAnchor {
	event_id: OwnedEventId,
	sender: OwnedUserId,
}

/// Reads the draft `g_seq` names in `room_id` and refuses anything that is
/// not an open draft.
///
/// ⚠️ There is no server-wide `g_seq → event` index, which is why the room is
/// in every pack's meta: `(room, g_seq)` is the key of a point read.
///
/// Args:
///     room_id: from the pack's meta
///     g_seq: the pack header's `id`, example: 123
/// Return:
///     Result<DraftAnchor, Reject>  `NotFound` when no event of the room has
///     that `g_seq`; `Conflict` when it is not a draft anchor, or has been
///     abandoned.
async fn read_open_draft(services: &Services, room_id: &RoomId, g_seq: u64) -> Result<DraftAnchor, Reject> {
	let shortroomid = services
		.short
		.get_shortroomid(room_id)
		.await
		.map_err(|_| Reject::code(RejectCode::NotFound, "no such room"))?;

	// A draft's anchor is always an event this server wrote, so its count is
	// positive; the backfilled half of the range cannot name one.
	let pdu_id: RawPduId = PduId { shortroomid, count: PduCount::Normal(g_seq) }.into();

	let anchor = services
		.timeline
		.get_pdu_from_id(&pdu_id)
		.await
		.map_err(|_| Reject::code(RejectCode::NotFound, "no event of this room has that g_seq"))?;

	if anchor.kind().to_string() != DRAFT_EVENT_TYPE {
		return Err(Reject::code(RejectCode::Conflict, "that event is not a draft"));
	}
	if anchor.is_redacted() {
		return Err(Reject::code(RejectCode::Conflict, "that draft was abandoned"));
	}

	Ok(DraftAnchor {
		event_id: anchor.event_id().to_owned(),
		sender: anchor.sender().to_owned(),
	})
}

/// ⭐ The guarantee a receiver relies on: the server, not the client, decides
/// that a piece came from the draft's author, so a forwarded piece never has
/// to be checked again at the far end.
fn refuse_unless_author(anchor: &DraftAnchor, user: &UserId) -> Result<(), Reject> {
	if anchor.sender != user {
		return Err(Reject::code(
			RejectCode::Forbidden,
			"only the author of a draft may write to it",
		));
	}
	Ok(())
}

/// The chain a receiver reads before applying a piece: its `data` starts with
/// a big-endian u32 naming the piece it follows, and its `seq` is its own
/// number (`docs/design/streaming-messages.md` §3.0).
///
/// ⭐ Why the server enforces a field it never reads the meaning of: without
/// these three checks the chain is a convention, and a convention that
/// nothing rejects is one that some client eventually does not follow — at
/// which point the receiver cannot tell "I missed a piece" from "this sender
/// numbers differently", which is the whole thing `prev` exists to answer.
///
/// Args:
///     view: a `Keypoint`, `Delta` or `Append`
/// Return:
///     Result<(), Reject>  `InvalidRequest` when the data is too short to
///     carry `prev`, when `seq` is 0 (reserved, so that `prev = 0` can mean
///     "no base" and nothing else), or when a `Keypoint` names a base — it
///     replaces the whole buffer, so there is nothing for it to follow.
fn refuse_unless_chained(view: &PackView<'_>) -> Result<(), Reject> {
	let Some(prev) = view.data.get(..4) else {
		return Err(Reject::code(
			RejectCode::InvalidRequest,
			"a draft piece starts with four bytes naming the piece it follows",
		));
	};
	let prev = u32::from_be_bytes(prev.try_into().expect("four bytes"));

	if view.header.seq == 0 {
		return Err(Reject::code(
			RejectCode::InvalidRequest,
			"a draft piece is numbered from 1; 0 is reserved for `prev` meaning no base",
		));
	}
	if view.header.subtype == stream::KEYPOINT && prev != 0 {
		return Err(Reject::code(
			RejectCode::InvalidRequest,
			format!("a Keypoint replaces the whole draft, so its `prev` is 0, not {prev}"),
		));
	}

	Ok(())
}

/// How much data a subtype that declares none may carry before the server
/// stops treating it as a slip: `Draft`, `Abandon` and `Demand` have no data
/// at all, and a client that fills one has either a bug or an intention.
const NO_DATA_TOLERANCE: usize = 1024;

/// Refuses a pack that declares no data but carries a payload worth
/// noticing, and closes the connection with it (維護者 2026-09-12).
///
/// ⚠️ Why closing: a `Demand` is broadcast to the whole room, so anything it
/// carries is copied once per connection. The general pack limit is 16 MiB,
/// which at three demands and ten listeners is most of a gigabyte of copying
/// for a pack the specification says is empty (external review 2026-09-12,
/// R3). Below the tolerance the payload is simply dropped on relay; above it,
/// nothing about the sender is worth continuing with.
fn refuse_oversized_payload(view: &PackView<'_>) -> Result<(), Reject> {
	if view.data.len() <= NO_DATA_TOLERANCE {
		return Ok(());
	}

	Err(Reject::closing(
		RejectCode::InvalidRequest,
		format!(
			"this Stream subtype carries no data, and this one carries {} bytes",
			view.data.len()
		),
	))
}

/// Whether this account may put content into a room **right now** — the two
/// policies an anchor written earlier does not carry with it.
///
/// ⚠️ An open draft is permission-shaped: it lives until it is abandoned, so
/// without this an author keeps broadcasting after the server suspends them
/// or the room takes their voice away, while every other way of putting
/// something into that room refuses them (external review 2026-09-12, R2 and
/// R4). `Abandon` deliberately does not come through here: taking back your
/// own event is what a suspended account is still allowed to do.
async fn refuse_unless_allowed_to_speak(
	services: &Services,
	user: &UserId,
	room_id: &RoomId,
) -> Result<(), Reject> {
	if services.users.is_suspended(user).await {
		return Err(Reject::code(
			RejectCode::Forbidden,
			"this account is suspended; it may abandon its drafts but not write to them",
		));
	}

	// The anchor is an event of the draft type, so the question the room
	// answers is the one it already knows: may this user send that type here?
	let Ok(power_levels) = services.state_accessor.get_power_levels(room_id).await else {
		return Err(Reject::code(
			RejectCode::Forbidden,
			"this server cannot read the room's power levels",
		));
	};
	if !power_levels.user_can_send_message(user, MessageLikeEventType::from(DRAFT_EVENT_TYPE)) {
		return Err(Reject::code(
			RejectCode::Forbidden,
			"this room no longer allows this user to send messages",
		));
	}

	Ok(())
}

/// ⚠️ Membership is checked for **pieces as well as demands**, which the
/// design document asks for only on demands. Being the author of the anchor
/// is not the same as still being in the room: somebody who wrote a draft and
/// then left, or was removed, would otherwise keep broadcasting into it,
/// while every other way of putting something into a room refuses them.
async fn refuse_unless_member(services: &Services, user: &UserId, room_id: &RoomId) -> Result<(), Reject> {
	if !services.state_cache.is_joined(user, room_id).await {
		return Err(Reject::code(
			RejectCode::Forbidden,
			"only a member of the room may write to or demand a draft in it",
		));
	}
	Ok(())
}

/// Refuses a draft in a room too large to fan out to (§7). A draft is for
/// bots and small rooms; the cost of one in a large room is a server problem
/// rather than a design problem, and this is the fallback that says so.
async fn refuse_room_too_large(services: &Services, room_id: &RoomId) -> Result<(), Reject> {
	let cap = services.config.wbf_draft_max_room_members;
	if cap == 0 {
		return Ok(());
	}

	// ⚠️ Not `unwrap_or(0)`: a count this server cannot read is a reason to
	// refuse, not a reason to allow. The draft would be the one case where
	// not knowing the room's size lets through exactly the fan-out the cap
	// exists to prevent.
	let Ok(joined) = services.state_cache.room_joined_count(room_id).await else {
		return Err(Reject::code(
			RejectCode::Conflict,
			"this server cannot tell how many members the room has",
		));
	};
	if joined > cap as u64 {
		return Err(Reject::code(
			RejectCode::Conflict,
			format!("this room has {joined} members; drafts are allowed up to {cap}"),
		));
	}
	Ok(())
}

/// The draft id of the anchor just written: its `g_seq`, which is the count
/// its position in the room carries.
async fn read_g_seq(services: &Services, event_id: &EventId) -> Result<i64, Reject> {
	services
		.timeline
		.get_pdu_count(event_id)
		.await
		.map(PduCount::into_signed)
		.map_err(|_| Reject::code(RejectCode::Internal, "the draft anchor has no position"))
}

/// Sends the pack on to the room's subscribers, **including the connection it
/// came from**: the author's other devices need it, and telling one
/// connection apart would be a rule that buys nothing.
///
/// Two things are decided here rather than by the sender:
///
/// ⭐ **Who receives it.** Anyone who ignores `sender` does not — the same
/// answer they get for that person's ordinary messages, from the same
/// `user_is_ignored`. Without it, ignoring somebody silenced their messages
/// and not their drafts (external review 2026-09-12, R5). The membership of
/// every recipient is read once more inside `relay_to`, because this function
/// awaits the database between choosing them and sending.
///
/// ⚠️ **The flags are 0.** All four mean something about the pack's
/// relationship to whoever receives it — meta is encrypted (it is not: it is
/// the room id, which the server reads), acknowledge this, this answers your
/// request, this ends the sequence — and after a relay none of them is true
/// of the receiver (維護者 2026-09-12). A piece arriving with any flag set is
/// refused on the way in, so "relayed as it is" stays literally true.
///
/// Args:
///     sender: the user whose content this is — the anchor's author for a
///         piece, the asker for a `Demand`
///     data: what to carry; `&[]` strips a `Demand`'s payload
async fn broadcast(
	services: &Services,
	room_id: &RoomId,
	sender: &UserId,
	view: &PackView<'_>,
	data: &[u8],
) {
	let header = view.header;
	let pack = PackBuilder::new(Kind::Stream, header.subtype, Flags::default(), header.id, header.seq)
		.meta(view.meta)
		.and_then(|builder| builder.data(data))
		.map(PackBuilder::finish);
	let Ok(pack) = pack else {
		debug!("wbf draft piece not re-encoded, dropped");
		return;
	};

	let mut recipients = Vec::new();
	for (connection, listener) in services.streams.listeners(room_id) {
		if !services
			.users
			.user_is_ignored(sender, &listener)
			.await
		{
			recipients.push(connection);
		}
	}

	services.streams.relay_to(room_id, &recipients, &pack);
}

/// The throttles' refusal, in the shape §3.4 gives `RateLimited`: the client
/// is told how long to wait rather than left to guess.
fn retry_later(message: &'static str, retry_after: std::time::Duration) -> Reject {
	let retry_after_ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
	Reject::with_extra(
		RejectCode::RateLimited,
		message,
		json!({ "retry_after_ms": retry_after_ms }),
	)
}
