//! The `0x05 Invite` kind: a user's pending invites over the channel
//! (`/docs/design/events/invites-on-the-wire.md`).
//!
//! Shaped like `0x16 Device` — `Subscribe` to be pushed, `Fetch` to catch up —
//! because it answers the same requirement: an invite must not be missed, so
//! the catch-up runs **to completion** (`r = 0`) rather than stopping at a
//! window. That is why it is not a subtype of `0x14 Event`, whose catch-up may
//! legitimately never finish (same doc, §4).
//!
//! 🚨 There is deliberately no waterline the client keeps between
//! connections. `ci_seq` resumes an interrupted pass and nothing more: it can
//! only find invites that were **added**, while an invite that was withdrawn
//! leaves no row to find. A complete pass is what makes the answer a snapshot,
//! and a snapshot is what tells the client the withdrawn one is gone
//! (same doc, §3.3).

use futures::StreamExt;
use ruma::{OwnedRoomId, RoomId, UserId};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tuwunel_core::{
	debug_warn,
	wbf::{
		Flags, Kind, PackBuilder, PackError, PackView, RejectCode,
		events::{framed_len, length_prefixed, list_pack_ranges},
		get_invite_fields,
	},
};
use tuwunel_service::Services;

use super::{Failure, PackContext, Reject, Reply, ack, parse_meta};

pub(super) const FETCH: u8 = 0x01;
pub(super) const BATCH: u8 = 0x02;
pub(super) const SUBSCRIBE: u8 = 0x04;
pub(super) const UNSUBSCRIBE: u8 = 0x05;

/// Rooms one `Batch` pack carries when the client does not choose. Only pack
/// granularity: `wbf_data_max_bytes` is what actually bounds a pack, so this
/// needs no setting of its own (/docs/design/events/invites-on-the-wire.md §5).
const BATCH_DEFAULT: usize = 64;

#[derive(Default, Deserialize)]
struct FetchMeta {
	/// Where an interrupted pass left off: the `ns` of the last `Batch` it
	/// received. Absent is the ordinary call.
	#[serde(default)]
	ci_seq: Option<u64>,
	#[serde(default)]
	batch: Option<usize>,
}

/// One pending invite in the pass, before its state has been read.
struct Pending {
	room: OwnedRoomId,
	count: u64,
}

/// One pending invite as it goes out.
struct Entry {
	count: u64,
	json: Vec<u8>,
}

/// `Invite/Subscribe`: be pushed this user's invites as they arrive.
///
/// Args:
///     view: meta example: `{}`
/// Return:
///     Result<(), Failure>  Ack meta `{latest_count}` — the position the
///     server was at when it registered the subscription, so a client knows
///     which pushes its following `Fetch` already covers. `Unsupported` over
///     HTTP: there is nothing to push to.
pub(super) async fn handle_invite_subscribe(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let session = ctx.get_session()?;
	let queue = reply.websocket_queue().ok_or_else(|| {
		Reject::code(RejectCode::Unsupported, "the invite stream needs the WebSocket channel")
	})?;

	// 🚨 Refused rather than ignored: the awaits above mean this can be in
	// flight when its session is logged out, and subscribing then would push a
	// dead session somebody's invites (/docs/design/wire/session-teardown.md §4.4).
	if services
		.streams
		.subscribe_invites(ctx.connection, &session.user, queue, view.header.id)
		.is_err()
	{
		return Err(Reject::code(
			RejectCode::Unauthorized,
			"this session ended while the Subscribe was in flight",
		)
		.into());
	}

	// Read after registering: anything from here on is pushed, so a client
	// that stores this cannot conclude it has seen an invite it has not.
	let latest_count = services.globals.current_count();

	reply
		.send(ack(
			view.header.id,
			view.header.seq,
			json!({ "latest_count": latest_count }),
			Vec::new(),
		))
		.await?;

	Ok(())
}

/// `Invite/Unsubscribe`: stop being pushed invites. The silent way out (the
/// connection ending) is the guard's job.
pub(super) async fn handle_invite_unsubscribe(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	services
		.streams
		.unsubscribe_invites(ctx.connection);

	reply
		.send(ack(view.header.id, view.header.seq, json!({}), Vec::new()))
		.await?;

	Ok(())
}

/// `Invite/Fetch`: every pending invite, oldest first, as `Batch` packs.
///
/// Args:
///     view: meta example: `{}`, `{"batch":16}` or `{"ci_seq":4711}`
/// Return:
///     Result<(), Failure>  a `Batch` per pack, `r = 0` on the last one; one
///     empty `Batch` when there is nothing, because "no invites" and "no
///     answer" must not look alike. `Unsupported` over HTTP.
///
/// ⚠️ `tc` is how many were pending **when the request arrived**. An invite
/// that ends while the pass is running simply does not appear; the pass is
/// bounded above by the position at that moment, so it terminates, and
/// anything newer arrives as a push (same doc, §3.3).
pub(super) async fn handle_invite_fetch(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let session = ctx.get_session()?;
	// ⚠️ `admission` already refuses this over HTTP, so this branch is not
	// reachable today. It is here because the handler should not depend on a
	// table elsewhere staying the way it is: an admission row edited later
	// would otherwise have this queueing a run of `Batch` packs at a reply
	// that can only carry one.
	if reply.websocket_queue().is_none() {
		return Err(Reject::code(
			RejectCode::Unsupported,
			"Invite/Fetch is the subscription's catch-up and needs the WebSocket channel",
		)
		.into());
	}
	let meta: FetchMeta = parse_meta(view, "Invite/Fetch")?;
	let batch = meta.batch.unwrap_or(BATCH_DEFAULT).max(1);

	// The upper bound is fixed here, which is what makes the pass finite:
	// invites that arrive while it runs are the push's job, not this one's.
	let upper_bound = services.globals.current_count();
	let pending = list_pending(services, &session.user, meta.ci_seq, upper_bound).await;
	let total = pending.len();

	// 🚨 Counted over the invites the pass **planned** to serve, not over the
	// entries it managed to build. An invite that ends between the index pass
	// and its state being read is served by nobody — and if the skipped ones
	// did not count here, `r` would never reach 0 and the client would wait
	// for a pack that is never coming.
	let mut dealt_with: usize = 0;
	let mut seq: u32 = 0;
	let mut chunks = pending.chunks(batch);
	loop {
		let (planned, entries) = match chunks.next() {
			| Some(chunk) => (
				chunk.len(),
				read_entries(services, &session.user, chunk, services.config.wbf_data_max_bytes).await,
			),
			// An empty pass is still answered, by one empty Batch that ends it.
			| None if seq == 0 => (0, Vec::new()),
			| None => break,
		};
		let skipped = planned.saturating_sub(entries.len());

		let mut ranges = list_pack_ranges(
			entries.iter().map(|entry| entry.json.len()),
			batch,
			services.config.wbf_data_max_bytes,
		);
		if ranges.is_empty() {
			ranges.push(0..0);
		}

		let last = ranges.len().saturating_sub(1);
		for (position, range) in ranges.into_iter().enumerate() {
			// `list_pack_ranges` covers exactly these entries, so this cannot
			// miss — and if it ever did, an empty pack is a pass that still
			// ends rather than a panic inside a connection's task.
			let in_pack = entries.get(range).unwrap_or(&[]);
			dealt_with = dealt_with.saturating_add(in_pack.len());
			if position == last {
				dealt_with = dealt_with.saturating_add(skipped);
			}
			reply
				.send(batch_pack(
					view.header.id,
					seq,
					total,
					in_pack,
					total.saturating_sub(dealt_with),
				)?)
				.await?;
			seq = seq.saturating_add(1);
		}
	}

	Ok(())
}

/// Every invite the pass will serve, oldest first.
///
/// Args:
///     ci_seq: only invites newer than this (an interrupted pass resuming)
///     upper_bound: only invites no newer than this
/// Return:
///     Vec<Pending>  sorted by count, oldest first.
///
/// ⭐ Only the room and the count are collected here, not the stripped state:
/// the state is read a batch at a time, so a user with thousands of invites
/// costs thousands of small keys rather than every state at once. Sorting is
/// what the index cannot do — `userroomid_invitestate` is ordered by room, and
/// the count lives in `roomuserid_invitecount` — and it is the same per-room
/// lookup `/sync` does on every single sync (`collect_invited_rooms`).
async fn list_pending(
	services: &Services,
	user: &UserId,
	ci_seq: Option<u64>,
	upper_bound: u64,
) -> Vec<Pending> {
	let rooms: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_invited(user)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut pending: Vec<Pending> = Vec::with_capacity(rooms.len());
	for room in rooms {
		let Ok(count) = services
			.state_cache
			.get_invite_count(&room, user)
			.await
		else {
			// No count means nothing to order it by, and serving it would put
			// an entry the client cannot place in its pass.
			debug_warn!(%user, %room, "wbf invite fetch: a pending invite has no count; skipped");
			continue;
		};
		if count > upper_bound || ci_seq.is_some_and(|resumed| count <= resumed) {
			continue;
		}
		pending.push(Pending { room, count });
	}

	pending.sort_unstable_by_key(|invite| invite.count);
	pending
}

/// Reads one chunk's stripped states and builds their entries.
///
/// Args:
///     chunk: the invites this batch covers, oldest first
///     data_max: `wbf_data_max_bytes`
/// Return:
///     Vec<Entry>  in the same order; an invite that ended since the pass
///     began is left out rather than served empty.
async fn read_entries(
	services: &Services,
	user: &UserId,
	chunk: &[Pending],
	data_max: usize,
) -> Vec<Entry> {
	let mut entries = Vec::with_capacity(chunk.len());
	for invite in chunk {
		let Ok(state) = services
			.state_cache
			.invite_state(user, &invite.room)
			.await
		else {
			continue;
		};
		let state: Vec<Vec<u8>> = state
			.iter()
			.map(|event| event.json().get().as_bytes().to_vec())
			.collect();

		let mut json = to_entry(&invite.room, &state, user).to_string().into_bytes();
		if framed_len(json.len()) > data_max {
			// One room whose state alone does not fit a pack: the invite is
			// still served, without it. `tc` and `r` stay honest, and the
			// client gets the same answer the push gave (`sc: 0`)
			// (/docs/design/events/invites-on-the-wire.md §5.3).
			debug_warn!(
				%user,
				room = %invite.room,
				"wbf invite fetch: this room's stripped state does not fit a pack; serving the invite without it"
			);
			json = to_entry(&invite.room, &[], user).to_string().into_bytes();
		}

		entries.push(Entry { count: invite.count, json });
	}

	entries
}

/// One room's entry: the stripped state as stored, plus the three fields a
/// client would otherwise have to dig out of it.
///
/// Args:
///     room: example: "!r:localhost"
///     state: the stripped state as `invite_state` stored it
///     user: the invited user, whose member event carries the three fields
/// Return:
///     Value  `{ "room_id", "inviter"?, "is_direct", "reason"?, "state": [...] }`
fn to_entry(room: &RoomId, state: &[Vec<u8>], user: &UserId) -> Value {
	let fields = get_invite_fields(state.iter().map(Vec::as_slice), user.as_str());

	let mut entry = Map::new();
	entry.insert("room_id".into(), json!(room));
	if let Some(inviter) = fields.inviter.as_deref() {
		entry.insert("inviter".into(), json!(inviter));
	}
	entry.insert("is_direct".into(), json!(fields.is_direct));
	if let Some(reason) = fields.reason.as_deref() {
		entry.insert("reason".into(), json!(reason));
	}
	entry.insert(
		"state".into(),
		Value::Array(
			state
				.iter()
				.filter_map(|event| serde_json::from_slice::<Value>(event).ok())
				.collect(),
		),
	);

	Value::Object(entry)
}

fn batch_pack(
	id: u64,
	seq: u32,
	tc: usize,
	entries: &[Entry],
	remaining: usize,
) -> Result<Vec<u8>, PackError> {
	let counts: Vec<u64> = entries.iter().map(|entry| entry.count).collect();
	let data = length_prefixed(entries.iter().map(|entry| entry.json.as_slice()))?;

	let mut meta = Map::new();
	meta.insert("tc".into(), json!(tc));
	meta.insert("bc".into(), json!(entries.len()));
	// 🚨 Left out of an empty batch rather than sent as 0. A client advances
	// its resume cursor to `ns`, and an empty batch happens mid-pass whenever a
	// whole chunk's invites ended while the pass was running — a zero there
	// would walk the cursor backwards to the beginning
	// (external review 2026-10-09, oliver and salvia). A placeholder that reads
	// as data is worse than an absent field.
	if let (Some(oldest), Some(newest)) = (counts.first(), counts.last()) {
		meta.insert("os".into(), json!(oldest));
		meta.insert("ns".into(), json!(newest));
	}
	meta.insert("counts".into(), json!(counts));
	meta.insert("r".into(), json!(remaining));

	Ok(PackBuilder::new(Kind::Invite, BATCH, Flags::IS_RESPONSE, id, seq)
		.json_meta(&Value::Object(meta))?
		.data(&data)?
		.finish())
}

#[cfg(test)]
mod tests {
	use ruma::{room_id, user_id};
	use serde_json::Value;
	use tuwunel_core::wbf::{Kind, decode, events::split_length_prefixed};

	use super::{BATCH, Entry, batch_pack, to_entry};

	fn entry(count: u64) -> Entry {
		Entry { count, json: format!(r#"{{"room_id":"!r{count}:localhost"}}"#).into_bytes() }
	}

	#[test]
	fn a_batch_says_how_many_are_left_and_which_counts_it_carried() {
		let entries = [entry(7), entry(9)];

		let mut pack = batch_pack(42, 0, 5, &entries, 3).expect("the pack builds");

		let view = decode(&mut pack).expect("it decodes");
		let meta: Value = view.meta_json().expect("the meta is JSON");
		assert_eq!(view.header.kind, Kind::Invite);
		assert_eq!(view.header.subtype, BATCH);
		assert_eq!(meta["tc"].as_u64(), Some(5));
		assert_eq!(meta["bc"].as_u64(), Some(2));
		assert_eq!(meta["os"].as_u64(), Some(7), "oldest first");
		assert_eq!(meta["ns"].as_u64(), Some(9));
		assert_eq!(meta["r"].as_u64(), Some(3));
		assert_eq!(
			split_length_prefixed(view.data).expect("the data splits").len(),
			2,
			"one item per room, so bc and the data have to agree"
		);
	}

	/// 🚨 "No invites" and "no answer" must not look alike, so an empty pass
	/// still sends one `Batch` — and its `r` ends the pass.
	#[test]
	fn an_empty_pass_is_still_answered() {
		let mut pack = batch_pack(42, 0, 0, &[], 0).expect("the pack builds");

		let view = decode(&mut pack).expect("it decodes");
		let meta: Value = view.meta_json().expect("the meta is JSON");
		assert_eq!(meta["tc"].as_u64(), Some(0));
		assert_eq!(meta["bc"].as_u64(), Some(0));
		assert_eq!(meta["r"].as_u64(), Some(0), "r = 0 is what says the pass is complete");
		assert!(view.data.is_empty());
		assert_eq!(
			(meta.get("os"), meta.get("ns")),
			(None, None),
			"and it carries no cursor: a 0 there would walk a client's resume cursor backwards"
		);
	}

	#[test]
	fn an_entry_carries_the_derived_fields_beside_the_state() {
		let state = [
			br#"{"type":"m.room.name","state_key":"","content":{"name":"r"}}"#.to_vec(),
			br#"{"type":"m.room.member","state_key":"@bob:localhost","sender":"@alice:localhost","content":{"membership":"invite","is_direct":true}}"#.to_vec(),
		];

		let entry = to_entry(room_id!("!r:localhost"), &state, user_id!("@bob:localhost"));

		assert_eq!(entry["room_id"].as_str(), Some("!r:localhost"));
		assert_eq!(entry["inviter"].as_str(), Some("@alice:localhost"));
		assert_eq!(entry["is_direct"].as_bool(), Some(true));
		assert_eq!(entry["state"].as_array().map(Vec::len), Some(2));
		assert!(entry.get("reason").is_none(), "absent rather than null");
	}

	/// The degraded entry of §5.3: the invite is still named, and the client
	/// can tell because `state` is empty.
	#[test]
	fn an_entry_without_its_state_still_names_the_room() {
		let entry = to_entry(room_id!("!r:localhost"), &[], user_id!("@bob:localhost"));

		assert_eq!(entry["room_id"].as_str(), Some("!r:localhost"));
		assert_eq!(entry["state"].as_array().map(Vec::len), Some(0));
		assert_eq!(entry["is_direct"].as_bool(), Some(false));
	}
}
