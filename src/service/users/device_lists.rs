//! Whose device lists changed, as one user sees it, and the `CryptoState`
//! pushes that tell a connected device (docs/design/wbf-e2ee.md §3).
//!
//! `/sync`, the channel's catch-up and `/keys/changes` answer the same
//! question, so the two layers that decide the answer live here once
//! (§3.4.1, decision 6): which users' keys changed (`list_key_changes_seen_by`)
//! and whether a join or a leave changed who shares an encrypted room
//! (`shares_encrypted_room`). What differs between them is only where "who
//! joined, who left" comes from: `/sync` already has it from the rooms it is
//! building, everyone else reads the membership counts
//! (`list_device_list_changes`).
//!
//! 🚨 Leaving a change out is the unsafe side — Alice keeps encrypting for
//! Bob's old devices and nobody can tell — so every unknown here counts as a
//! change.

use std::collections::{BTreeSet, HashMap, HashSet};

use futures::StreamExt;
use ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use serde_json::Value;
use tuwunel_core::{
	implement,
	utils::stream::{BroadbandExt, ReadyExt},
};

use crate::streams::CryptoState;

/// The two lists of `device_lists`, as one user sees them.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct DeviceListChanges {
	/// Users whose keys changed, or who now share an encrypted room with
	/// this user and did not before.
	pub changed: BTreeSet<OwnedUserId>,
	/// Users who no longer share any encrypted room with this user.
	pub left: BTreeSet<OwnedUserId>,
}

/// A membership change the push hook reports on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipChange {
	Join,
	Leave,
}

/// Users whose device keys changed between two positions, as `user_id` sees
/// it: their own, and anyone's in a room `user_id` is in now.
///
/// Args:
///     user_id: who is asking, example: "@alice:localhost"
///     from: exclusive, example: 400
///     to: inclusive; None is "up to now"
/// Return:
///     HashSet<OwnedUserId>  example: {"@bob:localhost"}; empty when nothing
///     changed
#[implement(super::Service)]
pub async fn list_key_changes_seen_by(&self, user_id: &UserId, from: u64, to: Option<u64>) -> HashSet<OwnedUserId> {
	let mut changed: HashSet<OwnedUserId> = self
		.keys_changed(user_id, from, to)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let rooms: Vec<OwnedRoomId> = self
		.services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;
	for room_id in &rooms {
		self.room_keys_changed(room_id, from, to)
			.ready_for_each(|(changed_user, _)| {
				changed.insert(changed_user.to_owned());
			})
			.await;
	}

	changed
}

/// Args:
///     user_a: example: "@alice:localhost"
///     user_b: example: "@bob:localhost"
///     ignore_room: a room not to count, example: the one just joined
/// Return:
///     bool  true when both are joined to an encrypted room other than
///     `ignore_room`
#[implement(super::Service)]
pub async fn shares_encrypted_room(&self, user_a: &UserId, user_b: &UserId, ignore_room: Option<&RoomId>) -> bool {
	self.services
		.state_cache
		.get_shared_rooms(user_a, user_b)
		.ready_filter(|&room_id| Some(room_id) != ignore_room)
		.map(ToOwned::to_owned)
		.broad_any(async |other_room_id| {
			self.services
				.state_accessor
				.is_encrypted_room(&other_room_id)
				.await
		})
		.await
}

/// Everyone in the rooms `user_id` itself left between two positions: the
/// "left" candidates of decision 7 (wbf-e2ee.md §6), which `/sync` did not
/// report before.
///
/// Args:
///     user_id: example: "@alice:localhost"
///     since: exclusive, example: 400
///     to: inclusive; None is "up to now"
/// Return:
///     HashSet<OwnedUserId>  the rooms' members and those who left them after
///     `since`, without `user_id`; a room whose leave position cannot be read
///     is included
#[implement(super::Service)]
pub async fn list_members_of_rooms_left(&self, user_id: &UserId, since: u64, to: Option<u64>) -> HashSet<OwnedUserId> {
	let state_cache = &self.services.state_cache;
	let rooms: Vec<OwnedRoomId> = state_cache
		.rooms_left(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut members = HashSet::new();
	for room_id in &rooms {
		let is_in_window = state_cache
			.get_left_count(room_id, user_id)
			.await
			.map_or(true, |count| count > since && to.is_none_or(|to| count <= to));
		if !is_in_window {
			continue;
		}
		state_cache
			.room_members(room_id)
			.chain(state_cache.room_members_left_after(room_id, since))
			.ready_for_each(|member| {
				members.insert(member.to_owned());
			})
			.await;
	}
	members.remove(user_id);
	members
}

/// The device-list changes `user_id` has not heard of since `since`, read
/// from the indexes rather than from a `/sync` (wbf-e2ee.md §3.4.1): the
/// catch-up of `Device/Subscribe{dl_seq}` and the answer of `/keys/changes`.
///
/// Args:
///     user_id: example: "@alice:localhost"
///     since: exclusive, example: 400; 0 is a first look, which like an
///         initial `/sync` reports key changes only
///     to: inclusive for key changes; memberships are read as they are now
/// Return:
///     DeviceListChanges  both lists empty when nothing changed
#[implement(super::Service)]
pub async fn list_device_list_changes(&self, user_id: &UserId, since: u64, to: Option<u64>) -> DeviceListChanges {
	let mut changes = DeviceListChanges {
		changed: self
			.list_key_changes_seen_by(user_id, since, to)
			.await
			.into_iter()
			.collect(),
		left: BTreeSet::new(),
	};
	if since == 0 {
		return changes;
	}

	let state_cache = &self.services.state_cache;
	let rooms: Vec<OwnedRoomId> = state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut joined: Vec<(OwnedUserId, OwnedRoomId)> = Vec::new();
	let mut left_candidates = self.list_members_of_rooms_left(user_id, since, to).await;
	for room_id in &rooms {
		// Joined after `since` (or unknown): everyone in it may be new to me.
		let is_room_new_to_me = state_cache
			.get_joined_count(room_id, user_id)
			.await
			.map_or(true, |count| count > since);
		let members: Vec<OwnedUserId> = if is_room_new_to_me {
			state_cache.room_members(room_id).map(ToOwned::to_owned).collect().await
		} else {
			state_cache
				.room_members_joined_after(room_id, since)
				.map(ToOwned::to_owned)
				.collect()
				.await
		};
		joined.extend(members.into_iter().map(|member| (member, room_id.clone())));

		state_cache
			.room_members_left_after(room_id, since)
			.ready_for_each(|member| {
				left_candidates.insert(member.to_owned());
			})
			.await;
	}

	for (member, room_id) in joined {
		if member != user_id && !self.shares_encrypted_room(user_id, &member, Some(&room_id)).await {
			changes.changed.insert(member);
		}
	}
	for member in left_candidates {
		if member != user_id && !self.shares_encrypted_room(user_id, &member, None).await {
			changes.left.insert(member);
		}
	}

	changes
}

/// Pushes a `CryptoState` to the connection holding this device's queue, if
/// one does: the device's current key counts and the lists given.
///
/// Args:
///     changed: example: ["@bob:localhost"]; empty for a counts-only push
///     left: example: []
#[implement(super::Service)]
pub async fn push_crypto_state(
	&self,
	user_id: &UserId,
	device_id: &DeviceId,
	changed: &[OwnedUserId],
	left: &[OwnedUserId],
) {
	let streams = &self.services.streams;
	if streams.device_holder(user_id, device_id).is_none() {
		return;
	}

	let otk_counts: Value = serde_json::to_value(self.count_one_time_keys(user_id, device_id).await)
		.unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
	let unused_fallback_key_types: Vec<String> = self
		.unused_fallback_key_algorithms(user_id, device_id)
		.map(|algorithm| algorithm.to_string())
		.collect()
		.await;

	streams.push_crypto_state(
		user_id,
		device_id,
		&CryptoState {
			otk_counts: &otk_counts,
			unused_fallback_key_types: &unused_fallback_key_types,
			changed,
			left,
		},
		self.services.config.wbf_meta_max_bytes,
	);
}

/// After `user_id`'s keys changed: tells every connected device of everyone
/// who can see the change — the user and the local members of `rooms`, the
/// rooms `mark_device_key_update` wrote it under.
///
/// Args:
///     rooms: example: ["!lobby:localhost"]
#[implement(super::Service)]
pub async fn push_key_change(&self, user_id: &UserId, rooms: &[OwnedRoomId]) {
	let streams = &self.services.streams;
	if !streams.is_any_device_held() {
		return;
	}

	let mut recipients: HashSet<OwnedUserId> = HashSet::from([user_id.to_owned()]);
	for room_id in rooms {
		self.services
			.state_cache
			.local_users_in_room(room_id)
			.ready_for_each(|member| {
				recipients.insert(member.to_owned());
			})
			.await;
	}

	let changed = [user_id.to_owned()];
	for (recipient, device) in streams.list_held_devices(&recipients) {
		self.push_crypto_state(&recipient, &device, &changed, &[])
			.await;
	}
}

/// After `user_id` joined or left `room_id`: tells every connected local
/// member whose sharing of an encrypted room with `user_id` changed, and
/// `user_id`'s own devices which members it now shares one with or no
/// longer does (wbf-e2ee.md §3.4, decision 7 for its own leave).
///
/// Args:
///     room_id: example: "!lobby:localhost"
///     user_id: who joined or left, example: "@carol:localhost"
///     change: Join or Leave
#[implement(super::Service)]
pub async fn push_membership_change(&self, room_id: &RoomId, user_id: &UserId, change: MembershipChange) {
	let streams = &self.services.streams;
	if !streams.is_any_device_held() {
		return;
	}
	let state_cache = &self.services.state_cache;

	let members: Vec<OwnedUserId> = state_cache
		.room_members(room_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;
	let mut locals: HashSet<OwnedUserId> = members
		.iter()
		.filter(|member| self.services.globals.user_is_local(member))
		.cloned()
		.collect();
	if self.services.globals.user_is_local(user_id) {
		locals.insert(user_id.to_owned());
	}

	let mut devices_by_user: HashMap<OwnedUserId, Vec<OwnedDeviceId>> = HashMap::new();
	for (holder, device) in streams.list_held_devices(&locals) {
		devices_by_user.entry(holder).or_default().push(device);
	}

	for (holder, devices) in devices_by_user {
		let mut users = Vec::new();
		if holder == user_id {
			for member in members.iter().filter(|member| *member != user_id) {
				if self.is_sharing_changed_by(user_id, member, room_id, change).await {
					users.push(member.clone());
				}
			}
		} else if self.is_sharing_changed_by(&holder, user_id, room_id, change).await {
			users.push(user_id.to_owned());
		}
		if users.is_empty() {
			continue;
		}

		let (changed, left) = match change {
			| MembershipChange::Join => (users, Vec::new()),
			| MembershipChange::Leave => (Vec::new(), users),
		};
		for device in devices {
			self.push_crypto_state(&holder, &device, &changed, &left)
				.await;
		}
	}
}

/// Args:
///     observer: whose view, example: "@alice:localhost"
///     other: the other user, example: "@carol:localhost"
///     room_id: the room the join or leave happened in
/// Return:
///     bool  on a join, true when that room is the only encrypted room they
///     share; on a leave, true when they no longer share any
#[implement(super::Service)]
async fn is_sharing_changed_by(&self, observer: &UserId, other: &UserId, room_id: &RoomId, change: MembershipChange) -> bool {
	match change {
		| MembershipChange::Join => !self.shares_encrypted_room(observer, other, Some(room_id)).await,
		| MembershipChange::Leave => !self.shares_encrypted_room(observer, other, None).await,
	}
}
