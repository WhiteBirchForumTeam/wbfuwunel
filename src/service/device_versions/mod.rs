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
	Result, debug_warn, err, error, implement,
	matrix::Event,
	utils::{
		MutexMap,
		hash::sha256,
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
///     written back (§3.3), with `UNHASHABLE` when its keys cannot be read
///     or hashed. Err only when the stored one cannot be read.
#[implement(Service)]
pub async fn get_device_version(&self, user_id: &UserId) -> Result<DeviceVersion> {
	if let Some(version) = to_found_or_none(self.find_stored_version(user_id).await)? {
		return Ok(version);
	}

	let _user_guard = self.user_locks.lock(user_id).await;
	if let Some(version) = to_found_or_none(self.find_stored_version(user_id).await)? {
		return Ok(version);
	}

	// ⚠️ Not the current position: that would move every room the account is
	// in, and refuse their senders once for a change that never happened.
	let version = DeviceVersion {
		seq: 1,
		hash: self.get_hash_or_unhashable(user_id).await,
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
	let version = DeviceVersion {
		seq,
		hash: self.get_hash_or_unhashable(user_id).await,
		pos,
	};
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
	{
		let mut room_versions = self.room_versions.write().expect("room versions lock poisoned");
		for room_id in &rooms {
			room_versions.remove(room_id);
		}
	}

	// Still under the account's lock, so two changes of one account are
	// announced in the order they were made (§6).
	self.announce_device_change(user_id, &version, rooms)
		.await;

	version
}

/// F3 (§6): tells the connections that declared device versions and listen
/// to a room `user_id` is in that the account's devices changed, with each
/// such room's new version. Only the rooms somebody would be told about are
/// computed; a room whose version cannot be computed is left out, and the
/// send check (F4) still guards it.
///
/// Args:
///     user_id: whose devices changed, example: "@bob:localhost"
///     version: the account's new version
///     rooms: the rooms the account is in
#[implement(Service)]
async fn announce_device_change(&self, user_id: &UserId, version: &DeviceVersion, rooms: Vec<OwnedRoomId>) {
	let streams = &self.services.streams;
	let mut room_versions = Vec::new();
	for room_id in rooms {
		if !streams.is_listened_by_device_versions(&room_id) {
			continue;
		}
		match self.get_room_device_version(&room_id).await {
			| Ok(room_version) => room_versions.push((room_id, room_version)),
			| Err(e) => debug_warn!(%room_id, "no room version to announce a device change with: {e}"),
		}
	}

	if !room_versions.is_empty() {
		streams.push_device_changed(user_id, &version.to_wire(), &room_versions);
	}
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

/// §4.1: a hash over who currently holds this room's keys — every `join`,
/// `leave` and `ban` member of the room **state**, and the joined members'
/// device versions.
///
/// 🚨 **A hash, not the largest position, and that is the whole point.** The
/// only comparison anywhere is `expected != room_device_version`
/// (`api/client/send.rs`): nothing ever orders these numbers. A maximum was
/// only ever an *indirect* way of saying "the set changed", and it had a hole
/// — take the largest of a set and the largest can fall when a member leaves
/// the set. `leave` → re-`invite` did exactly that: the leave is replaced in
/// the room state by an invite, which is not counted, so the contribution that
/// was holding the maximum up vanished and the version fell back to a value a
/// client may still be holding. It would then pass the gate with a stale
/// member list and send the room key to someone who had left — the very leak
/// §4.2 reasoned about, through a door that reasoning missed (external review
/// 2026-09-29). A hash cannot fall back: a different set is a different value.
///
/// 📎 This is what the per-account device version has always done, and the
/// input here is the same string clients are given in `/members`
/// (`DeviceVersion::to_wire`), so the value is a fingerprint of exactly what
/// the client saw.
///
/// The framing follows `device_keys_hash`: sort, then length-prefix every item
/// so that no two different sets can concatenate to the same bytes.
///
/// Args:
///     member_events: every `m.room.member` event of one room state, the one
///       the caller also answers from (`/members` reads it once for both)
/// Return:
///     Result<u64>  example: 9843172550163464192; Err for a member event
///     without readable content, rather than a version that skipped it.
#[implement(Service)]
pub async fn compute_room_device_version<E: Event>(&self, member_events: &[E]) -> Result<u64> {
	let mut items: Vec<(String, Vec<u8>)> = Vec::new();

	for event in member_events {
		let content: RoomMemberEventContent = event.get_content()?;
		if !is_membership_counted(&content.membership) {
			continue;
		}

		let member = event
			.state_key()
			.and_then(|key| UserId::parse(key).ok())
			.ok_or_else(|| err!(Database("member event {} has no user", event.event_id())))?;

		// The membership itself is in the item, so a user moving between two
		// counted states (`join` → `leave`, `leave` → `ban`) changes the hash
		// even though the same user is still in the set.
		let mut item = content.membership.to_string().into_bytes();
		if content.membership == MembershipState::Join {
			item.push(0xFF);
			item.extend_from_slice(
				self.get_device_version(&member)
					.await?
					.to_wire()
					.as_bytes(),
			);
		}
		items.push((member.as_str().to_owned(), item));
	}

	to_room_version_hash(items)
}

/// The set-to-number half of [`Service::compute_room_device_version`], kept
/// separate so it can be tested without `Services` — and the property worth
/// testing is exactly here: **a different set is a different number**.
///
/// Args:
///     items: one `(user_id, membership ‖ device version)` pair per counted
///         member, in any order, example: `[("@a:l", b"join\xFF3-abc")]`
/// Return:
///     Result<u64>  the first eight bytes of the SHA-256, big-endian; Err only
///     for an item too large to length-prefix.
fn to_room_version_hash(mut items: Vec<(String, Vec<u8>)>) -> Result<u64> {
	items.sort_by(|left, right| left.0.cmp(&right.0));

	// Length-prefixed like `device_keys_hash`: without it `("ab", "c")` and
	// `("a", "bc")` would frame to the same bytes.
	let mut framed: Vec<u8> = Vec::new();
	for (user, item) in items {
		for part in [user.as_bytes(), item.as_slice()] {
			let len = u32::try_from(part.len()).map_err(|_| err!("a member item too large to hash"))?;
			framed.extend_from_slice(&len.to_be_bytes());
			framed.extend_from_slice(part);
		}
	}

	let digest = sha256::hash(&framed);
	let head: [u8; 8] = digest
		.get(..8)
		.and_then(|bytes| bytes.try_into().ok())
		.ok_or_else(|| err!("sha256 is shorter than eight bytes"))?;

	Ok(u64::from_be_bytes(head))
}

/// `invite` and `knock` do not change who holds the room key; the `join`
/// that may follow does (§4.1).
fn is_membership_counted(membership: &MembershipState) -> bool {
	matches!(membership, MembershipState::Join | MembershipState::Leave | MembershipState::Ban)
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
///
/// 🚨 Only a key that is not there is left out. A read that fails, or a
/// stored key that is not JSON, fails the whole hash: a hash of the keys that
/// happened to be readable would look like a fingerprint of the account and be
/// one of something else (PR #73 review).
#[implement(Service)]
async fn hash_current_keys(&self, user_id: &UserId) -> Result<String> {
	let users = &self.services.users;
	let only_own_signatures = |_: &UserId| false;

	let master_key = to_found_or_none(
		users
			.get_master_key(None, user_id, &only_own_signatures)
			.await,
	)?
	.map(|raw| to_key_json(raw.json()))
	.transpose()?;
	let self_signing_key = to_found_or_none(
		users
			.get_self_signing_key(None, user_id, &only_own_signatures)
			.await,
	)?
	.map(|raw| to_key_json(raw.json()))
	.transpose()?;

	let device_ids: Vec<OwnedDeviceId> = users
		.all_device_ids(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;
	let mut device_keys = Vec::with_capacity(device_ids.len());
	for device_id in device_ids {
		// A device that never uploaded keys is not in /keys/query either.
		if let Some(raw) = to_found_or_none(users.get_device_keys(user_id, &device_id).await)? {
			device_keys.push((device_id, to_key_json(raw.json())?));
		}
	}

	device_keys_hash(user_id, master_key.as_ref(), self_signing_key.as_ref(), &device_keys)
}

/// Args:
///     read: a key read, example: `Err(NotFound)` for an account with no
///       master key
/// Return:
///     Result<Option<T>>  Some for a key, None only when it is not there; any
///     other error stays an error.
fn to_found_or_none<T>(read: Result<T>) -> Result<Option<T>> {
	match read {
		| Ok(found) => Ok(Some(found)),
		| Err(e) if e.is_not_found() => Ok(None),
		| Err(e) => Err(e),
	}
}

/// Return:
///     Result<Value>  the stored key as JSON; Err when it is not JSON.
fn to_key_json(raw: &serde_json::value::RawValue) -> Result<Value> {
	serde_json::from_str(raw.get()).map_err(|e| err!(Database("a stored key is not JSON: {e}")))
}

/// For the version's hash: the fingerprint, or `UNHASHABLE` when the keys
/// cannot be read or hashed, so the version is still written and still moves.
#[implement(Service)]
async fn get_hash_or_unhashable(&self, user_id: &UserId) -> String {
	self.hash_current_keys(user_id)
		.await
		.unwrap_or_else(|e| {
			error!(%user_id, "keys that cannot be hashed: {e}");
			UNHASHABLE.to_owned()
		})
}

#[cfg(test)]
mod tests {
	use ruma::events::room::member::MembershipState;

	use tuwunel_core::err;

	use super::{DeviceVersion, is_membership_counted, to_found_or_none, to_room_version_hash};

	/// 🚨 PR #73 review: only a key that is not there may be left out of the
	/// hash. A read that failed any other way must not look like "no key",
	/// or the hash is taken over whatever happened to be readable.
	#[test]
	fn only_a_missing_key_is_left_out_and_every_other_failure_stays_a_failure() {
		assert_eq!(to_found_or_none(Ok(7)).ok(), Some(Some(7)));
		assert_eq!(to_found_or_none::<u8>(Err(err!(Request(NotFound("no master key"))))).ok(), Some(None));

		assert!(to_found_or_none::<u8>(Err(err!(Database("the row could not be read")))).is_err());
		assert!(to_found_or_none::<u8>(Err(err!("anything else"))).is_err());
	}

	#[test]
	fn only_join_leave_and_ban_move_a_room() {
		assert!(is_membership_counted(&MembershipState::Join));
		assert!(is_membership_counted(&MembershipState::Leave));
		assert!(is_membership_counted(&MembershipState::Ban));
		assert!(!is_membership_counted(&MembershipState::Invite));
		assert!(!is_membership_counted(&MembershipState::Knock));
	}

	/// 🚨 The regression the hash exists for (external review 2026-09-29).
	/// Bob joins, leaves, is re-invited — and a re-invite drops him out of the
	/// counted set entirely, because `invite` is not counted. Under the old
	/// "largest position" rule that made the number **fall back** to what it
	/// had been before he left, so a client still holding that number passed
	/// the gate with a member list that still had Bob in it, and sent him the
	/// room key after he had left.
	///
	/// ⭐ The assertion is not "it goes up" — nothing orders these numbers
	/// (`send.rs` only ever asks `!=`). It is that **no two of the three sets
	/// share a value**, which is the property a maximum could not give.
	#[test]
	fn a_re_invite_cannot_bring_back_an_earlier_room_version() {
		let others = || ("@alice:l".to_owned(), b"join\xFF7-aaaaaaaaaa".to_vec());

		let bob_joined = to_room_version_hash(vec![others(), ("@bob:l".to_owned(), b"join\xFF3-bbbbbbbbbb".to_vec())]).expect("hashes");
		let bob_left = to_room_version_hash(vec![others(), ("@bob:l".to_owned(), b"leave".to_vec())]).expect("hashes");
		// Re-invited: `invite` is not counted, so Bob is simply absent.
		let bob_reinvited = to_room_version_hash(vec![others()]).expect("hashes");

		assert_ne!(bob_joined, bob_left, "leaving must change the room version");
		assert_ne!(bob_left, bob_reinvited, "a re-invite must change it again");
		assert_ne!(
			bob_joined, bob_reinvited,
			"a re-invite must not land back on the value from before Bob left"
		);
	}

	#[test]
	fn the_order_members_arrive_in_does_not_change_the_room_version() {
		let a = ("@alice:l".to_owned(), b"join\xFF1-aaaaaaaaaa".to_vec());
		let b = ("@bob:l".to_owned(), b"join\xFF2-bbbbbbbbbb".to_vec());

		assert_eq!(
			to_room_version_hash(vec![a.clone(), b.clone()]).expect("hashes"),
			to_room_version_hash(vec![b, a]).expect("hashes")
		);
	}

	/// 📎 Why every part is length-prefixed: without it these two different
	/// sets would frame to the same bytes.
	#[test]
	fn a_user_and_their_item_cannot_run_together() {
		assert_ne!(
			to_room_version_hash(vec![("@ab:l".to_owned(), b"join".to_vec())]).expect("hashes"),
			to_room_version_hash(vec![("@a".to_owned(), b"b:ljoin".to_vec())]).expect("hashes")
		);
	}

	#[test]
	fn the_wire_form_is_seq_dash_hash() {
		let version = DeviceVersion { seq: 3, hash: "810b7c3be4".to_owned(), pos: 81234 };

		assert_eq!(version.to_wire(), "3-810b7c3be4");
	}
}
