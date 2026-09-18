//! Device versions (`docs/design/wbf-room-device-version.md`): one per account,
//! `seq-hash`, which moves whenever the account's keys do (§3), and one per
//! room, derived from its member events and its members' device versions (§4).
//! A client that sends with a room's version is refused (1506) once it has
//! moved: who it would hand the room key to has changed since it looked.

mod keys_hash;

use std::{
	collections::HashMap,
	pin::pin,
	sync::{
		Arc, RwLock as StdRwLock,
		atomic::{AtomicU64, Ordering},
	},
};

use futures::StreamExt;
use ruma::{
	OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId,
	events::{
		TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tuwunel_core::{
	Result, err, error, implement,
	matrix::{Event, PduCount},
	utils::{
		MutexMap,
		stream::{ReadyExt, TryIgnore},
	},
};
use tuwunel_database::{Deserialized, Json, Map};

pub use self::keys_hash::device_keys_hash;
use crate::rooms::short::ShortStateHash;

/// The hash of a version whose keys could not be read or hashed: an explicit
/// word, so nobody mistakes it for a fingerprint.
pub const UNHASHABLE: &str = "unhashable";

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
	/// One per account: a version is read and written back under it, so two
	/// changes at once cannot both write `seq + 1`.
	user_locks: MutexMap<OwnedUserId, ()>,
	/// `room → (the room state it was computed from, its version)`. Only a
	/// cache: every entry can be computed again, so a restart loses nothing.
	room_versions: StdRwLock<HashMap<OwnedRoomId, (ShortStateHash, u64)>>,
	/// Moves on every device-version change; an entry computed while it moved
	/// may have read a member's old position and is not kept.
	device_changes: AtomicU64,
}

struct Data {
	/// `user → {seq, hash, pos}`.
	userid_wbfdeviceversion: Arc<Map>,
	keychangeid_userid: Arc<Map>,
}

/// One account's device version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceVersion {
	/// How many times the account's keys have changed, from 1.
	pub seq: u64,
	/// `device_keys_hash` of the keys as they are now.
	pub hash: String,
	/// The server-wide position of the last change: the same counter as
	/// `g_seq`, so a room's version can take the larger of the two.
	pub pos: u64,
}

impl DeviceVersion {
	/// Return:
	///     String  the form clients see, example: "3-810b7c3be4"
	#[must_use]
	pub fn to_wire(&self) -> String { format!("{}-{}", self.seq, self.hash) }
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				userid_wbfdeviceversion: args.db["userid_wbfdeviceversion"].clone(),
				keychangeid_userid: args.db["keychangeid_userid"].clone(),
			},
			user_locks: MutexMap::new(),
			room_versions: StdRwLock::new(HashMap::new()),
			device_changes: AtomicU64::new(0),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Args:
///     user_id: example: "@bob:localhost"
/// Return:
///     Result<DeviceVersion>  the stored one; an account without one gets
///     `seq` 1 and the position of its last key change (0 when it has none),
///     written back (§3.3). Err when its keys cannot be read or hashed.
#[implement(Service)]
pub async fn get_device_version(&self, user_id: &UserId) -> Result<DeviceVersion> {
	if let Ok(version) = self.find_stored_version(user_id).await {
		return Ok(version);
	}

	let _user_guard = self.user_locks.lock(user_id).await;
	if let Ok(version) = self.find_stored_version(user_id).await {
		return Ok(version);
	}

	// ⚠️ Not the current position: that would move every room the account is
	// in, and refuse their senders once for a change that never happened.
	let version = DeviceVersion {
		seq: 1,
		hash: self.hash_current_keys(user_id).await?,
		pos: self.find_last_key_change_position(user_id).await,
	};
	self.db
		.userid_wbfdeviceversion
		.put(user_id, Json(&version));

	Ok(version)
}

/// Moves the account's device version: the one place it moves, called from
/// `users::mark_device_key_update` after the keys are written.
///
/// Args:
///     user_id: whose keys changed, example: "@bob:localhost"
///     pos: the position `mark_device_key_update` took for this change
/// Return:
///     DeviceVersion  the new version, always a step forward: keys that
///     cannot be hashed get `UNHASHABLE` rather than a version that stayed.
#[implement(Service)]
pub async fn bump_device_version(&self, user_id: &UserId, pos: u64) -> DeviceVersion {
	let _user_guard = self.user_locks.lock(user_id).await;

	let seq = match self.find_stored_version(user_id).await {
		| Ok(stored) => stored.seq.saturating_add(1),
		| Err(e) if e.is_not_found() => 1,
		// Still forward: `pos` moves on every change of every account, so it
		// is above any `seq` this account reached.
		| Err(e) => {
			error!(%user_id, "unreadable device version, restarting from the position: {e}");
			pos
		},
	};

	// 🚨 A version that did not move would let a client that saw the old keys
	// through; one that moved with a placeholder only costs it a refetch.
	let hash = self
		.hash_current_keys(user_id)
		.await
		.unwrap_or_else(|e| {
			error!(%user_id, "keys that cannot be hashed: {e}");
			UNHASHABLE.to_owned()
		});
	let version = DeviceVersion { seq, hash, pos };
	self.db
		.userid_wbfdeviceversion
		.put(user_id, Json(&version));

	// In this order: an entry computed from the old position sees the counter
	// move and is not kept, and the entries already kept are removed.
	self.device_changes.fetch_add(1, Ordering::SeqCst);
	let rooms: Vec<OwnedRoomId> = self
		.services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;
	let mut room_versions = self.room_versions.write().expect("room versions lock poisoned");
	for room_id in &rooms {
		room_versions.remove(room_id);
	}

	version
}

/// The room's device version (§4.1), cached by the room state it came from.
///
/// Args:
///     room_id: example: "!r:localhost"
/// Return:
///     Result<u64>  example: 81234; Err when the room has no state or a member
///     event cannot be placed.
#[implement(Service)]
pub async fn get_room_device_version(&self, room_id: &RoomId) -> Result<u64> {
	let shortstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id)
		.await?;

	let cached = self
		.room_versions
		.read()
		.expect("room versions lock poisoned")
		.get(room_id)
		.copied();
	if let Some((computed_from, version)) = cached
		&& computed_from == shortstatehash
	{
		return Ok(version);
	}

	let device_changes_before = self.device_changes.load(Ordering::SeqCst);
	let member_events: Vec<_> = self
		.services
		.state_accessor
		.state_full_pdus(shortstatehash)
		.ready_filter(|pdu| *pdu.kind() == TimelineEventType::RoomMember)
		.collect()
		.await;
	let version = self
		.compute_room_device_version(&member_events)
		.await?;

	if self.device_changes.load(Ordering::SeqCst) == device_changes_before {
		self.room_versions
			.write()
			.expect("room versions lock poisoned")
			.insert(room_id.to_owned(), (shortstatehash, version));
	}

	Ok(version)
}

/// §4.1: the largest of the positions of the `join`, `leave` and `ban` member
/// events, and of the joined members' device-version positions. 🚨 From the
/// room **state**, never from `state_cache`'s membership rows: `forget` deletes
/// a leave there and the version would fall back (§4.2).
///
/// Args:
///     member_events: every `m.room.member` event of one room state, the one
///       the caller also answers from (`/members` reads it once for both)
/// Return:
///     Result<u64>  example: 81234; Err for a member event without a
///     position or readable content, rather than a version that skipped it.
#[implement(Service)]
pub async fn compute_room_device_version<E: Event>(&self, member_events: &[E]) -> Result<u64> {
	let mut version = 0_u64;
	for event in member_events {
		let content: RoomMemberEventContent = event.get_content()?;
		if !is_membership_counted(&content.membership) {
			continue;
		}

		let position = self
			.services
			.timeline
			.get_pdu_count(event.event_id())
			.await
			.map_err(|e| err!(Database("member event {} has no position: {e}", event.event_id())))?;
		version = version.max(to_room_position(position));

		if content.membership == MembershipState::Join {
			let member = event
				.state_key()
				.and_then(|key| UserId::parse(key).ok())
				.ok_or_else(|| err!(Database("member event {} has no user", event.event_id())))?;
			version = version.max(self.get_device_version(&member).await?.pos);
		}
	}

	Ok(version)
}

/// `invite` and `knock` do not change who holds the room key; the `join`
/// that may follow does (§4.1).
fn is_membership_counted(membership: &MembershipState) -> bool {
	matches!(membership, MembershipState::Join | MembershipState::Leave | MembershipState::Ban)
}

/// A backfilled event is older than every live one: 0.
fn to_room_position(count: PduCount) -> u64 {
	match count {
		| PduCount::Normal(position) => position,
		| PduCount::Backfilled(_) => 0,
	}
}

#[implement(Service)]
async fn find_stored_version(&self, user_id: &UserId) -> Result<DeviceVersion> {
	self.db
		.userid_wbfdeviceversion
		.get(user_id)
		.await
		.deserialized()
}

/// The last `(user, position)` row `mark_device_key_update` wrote for this
/// account, or 0 when it never wrote one.
#[implement(Service)]
async fn find_last_key_change_position(&self, user_id: &UserId) -> u64 {
	type KeyChange<'a> = (&'a str, u64);

	let newest_first = self
		.db
		.keychangeid_userid
		.rev_keys_from(&(user_id.as_str(), u64::MAX))
		.ignore_err()
		.ready_take_while(|(prefix, _): &KeyChange<'_>| *prefix == user_id.as_str())
		.map(|(_, position): KeyChange<'_>| position);

	pin!(newest_first).next().await.unwrap_or(0)
}

/// `device_keys_hash` of what `/keys/query` would show anyone: the master
/// and self-signing keys with only the owner's signatures, and the keys of
/// every device that has uploaded some.
#[implement(Service)]
async fn hash_current_keys(&self, user_id: &UserId) -> Result<String> {
	let users = &self.services.users;
	let only_own_signatures = |_: &UserId| false;
	let to_json = |raw: &str| serde_json::from_str::<Value>(raw).ok();

	let master_key = users
		.get_master_key(None, user_id, &only_own_signatures)
		.await
		.ok()
		.and_then(|raw| to_json(raw.json().get()));
	let self_signing_key = users
		.get_self_signing_key(None, user_id, &only_own_signatures)
		.await
		.ok()
		.and_then(|raw| to_json(raw.json().get()));

	let device_ids: Vec<OwnedDeviceId> = users
		.all_device_ids(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;
	let mut device_keys = Vec::with_capacity(device_ids.len());
	for device_id in device_ids {
		if let Some(keys) = users
			.get_device_keys(user_id, &device_id)
			.await
			.ok()
			.and_then(|raw| to_json(raw.json().get()))
		{
			device_keys.push((device_id, keys));
		}
	}

	device_keys_hash(user_id, master_key.as_ref(), self_signing_key.as_ref(), &device_keys)
}

#[cfg(test)]
mod tests {
	use ruma::events::room::member::MembershipState;
	use tuwunel_core::matrix::PduCount;

	use super::{DeviceVersion, is_membership_counted, to_room_position};

	#[test]
	fn only_join_leave_and_ban_move_a_room() {
		assert!(is_membership_counted(&MembershipState::Join));
		assert!(is_membership_counted(&MembershipState::Leave));
		assert!(is_membership_counted(&MembershipState::Ban));
		assert!(!is_membership_counted(&MembershipState::Invite));
		assert!(!is_membership_counted(&MembershipState::Knock));
	}

	#[test]
	fn a_backfilled_member_event_is_older_than_any_live_one() {
		assert_eq!(to_room_position(PduCount::Normal(81234)), 81234);
		assert_eq!(to_room_position(PduCount::Backfilled(-5)), 0);
	}

	#[test]
	fn the_wire_form_is_seq_dash_hash() {
		let version = DeviceVersion { seq: 3, hash: "810b7c3be4".to_owned(), pos: 81234 };

		assert_eq!(version.to_wire(), "3-810b7c3be4");
	}
}
