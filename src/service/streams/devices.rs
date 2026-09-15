//! The to-device queue (`0x16 Device`): one topic per device, the rule that
//! one connection at a time holds it, and the `Push` pack's shape
//! (`docs/design/wbf-to-device.md`).
//!
//! What makes this different from the room channels is that the client
//! **destroys** what it has taken: the queue is the only copy, and the server
//! keeps each item until the device says it is safely stored. So the
//! subscription is exclusive — two connections of one device both taking and
//! destroying would let one of them delete an item the other was still
//! importing, and that item is gone for good.
//!
//! Exclusive, but **not first come first served**: the latest `Subscribe`
//! takes the queue over and the connection it displaces is told so. A binding
//! the newest connection cannot take is a device that stops receiving keys
//! whenever its last connection died without saying so — and the registry
//! only learns that at the idle timeout, if ever.

use std::collections::HashSet;

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde_json::{Value, json};

use tuwunel_core::{
	debug,
	wbf::{
		CONTROL_ERROR_SUBTYPE, Flags, Kind, PackBuilder, PackError, RejectCode,
		events::{length_prefixed, list_pack_ranges},
	},
};

use super::{ConnectionId, Outgoing, PackQueue, Streams};

/// `Device/Push`, server to client only.
pub const DEVICE_PUSH_SUBTYPE: u8 = 0x06;

/// `Device/CryptoState`, server to client only (docs/design/wbf-e2ee.md §3).
pub const DEVICE_CRYPTO_STATE_SUBTYPE: u8 = 0x08;

/// What one `CryptoState` reports for a device, before it is cut to fit packs.
pub struct CryptoState<'a> {
	/// example: `{"signed_curve25519": 42}`
	pub otk_counts: &'a Value,
	/// Always sent, empty or not: `[]` means "all used", absent would mean
	/// "not supported". example: `["signed_curve25519"]`
	pub unused_fallback_key_types: &'a [String],
	/// example: `["@bob:example.org"]`
	pub changed: &'a [OwnedUserId],
	pub left: &'a [OwnedUserId],
}

/// One to-device item on the wire: its count (the position the client acks
/// and later destroys by) and its JSON as stored.
pub struct PushedItem<'a> {
	pub count: u64,
	pub json: &'a [u8],
}

/// Whose queue this is. The device is only meaningful under its user, so the
/// topic is the pair — never the device id alone.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct DeviceTopic {
	user: OwnedUserId,
	device: OwnedDeviceId,
}

impl DeviceTopic {
	fn new(user: &UserId, device: &DeviceId) -> Self {
		Self { user: user.to_owned(), device: device.to_owned() }
	}
}

impl Streams {
	/// Binds `connection` to this device's queue and subscribes it, taking
	/// the queue over from whatever connection of the same device held it.
	///
	/// The caller has already checked that `device` is the one this
	/// connection's session holds: the session is the only source of
	/// identity, and a connection asking for another device's queue is
	/// refused before it gets here.
	///
	/// ⚠️ **The later connection wins** (維護者 2026-09-12). Refusing it
	/// instead looks safer — the holder may be mid-import — but the holder
	/// can be a connection that is already gone: a half-open socket is not
	/// noticed until the idle timeout, and until then the device cannot
	/// subscribe at all. A queue that no live connection can take is a
	/// device whose keys never arrive, and if the holder is wedged rather
	/// than merely slow, that device id is finished for good. Losing an
	/// import that will be re-pushed is the smaller failure, and the
	/// displaced connection is told rather than left to wait.
	///
	/// Args:
	///     connection: example: 7
	///     user: who the connection is
	///     device: whose queue, from the session
	///     queue: the connection's send queue
	///     id: the client's `Subscribe` id, example: 42
	/// Return:
	///     Option<ConnectionId>  the connection that was holding this
	///     device's queue and has now been told it no longer is; None when
	///     nobody held it, or when this connection is re-subscribing to a
	///     queue it already holds.
	pub fn subscribe_device(
		&self,
		connection: ConnectionId,
		user: &UserId,
		device: &DeviceId,
		queue: PackQueue,
		id: u64,
	) -> Option<ConnectionId> {
		let topic = DeviceTopic::new(user, device);
		let entered = self
			.devices
			.subscribe(connection, user, queue, id, &[topic]);

		// The registry took them out of the topic; ending their conversation
		// is this layer's job, because the pack is this kind's.
		let mut superseded = None;
		for loser in entered.displaced {
			debug!(
				"wbf device queue of {user}:{device} taken over by connection {connection}, was {}",
				loser.connection
			);
			match superseded_pack(loser.id, loser.seq) {
				| Ok(pack) => {
					// Never wait on a connection we are cutting off: a full
					// queue means it is not reading, and it finds out when
					// its socket closes either way.
					drop(loser.queue.try_send(Outgoing::Pack(pack)));
				},
				| Err(error) => debug!("wbf superseded notice not encoded: {error}"),
			}
			superseded = Some(loser.connection);
		}
		superseded
	}

	/// Releases this connection's hold on its device queue, if it has one.
	/// Both ways out must do this — the spoken one (`Unsubscribe`) and the
	/// silent one (the connection ending) — or the device stays taken and no
	/// other connection can ever subscribe to it.
	pub fn unsubscribe_device(&self, connection: ConnectionId) {
		self.devices.remove_connection(connection);
		self.device_list_positions
			.write()
			.expect("device list positions lock poisoned")
			.remove(&connection);
	}

	/// Records where this connection's device-list catch-up starts from, so
	/// every `CryptoState` of its subscription carries it as `dl_seq`.
	///
	/// 🚨 It is one number for the whole subscription, not the count of each
	/// change pushed. A change is written before it is pushed, and pushes are
	/// not ordered by count: stamping each push with its own count lets a
	/// client store 101 while the push for 100 is still in flight, and a
	/// reconnect from 101 never hears of 100. Taken after the connection holds
	/// the queue and before the catch-up reads, every change is either read
	/// by the catch-up or pushed to a connection already listening; a
	/// reconnect from here repeats some of them, which costs a key query.
	///
	/// Args:
	///     connection: the connection that holds the device's queue, example: 7
	///     dl_seq: `globals.current_count()` read after `subscribe_device`
	pub fn set_device_list_position(&self, connection: ConnectionId, dl_seq: u64) {
		self.device_list_positions
			.write()
			.expect("device list positions lock poisoned")
			.insert(connection, dl_seq);
	}

	/// Which of these users' devices a connection holds right now.
	///
	/// Return:
	///     Vec<(OwnedUserId, OwnedDeviceId)>  empty when none of them is
	///     connected, which is the case to make cheap.
	#[must_use]
	pub fn list_held_devices(&self, users: &HashSet<OwnedUserId>) -> Vec<(OwnedUserId, OwnedDeviceId)> {
		if users.is_empty() {
			return Vec::new();
		}
		self.devices
			.list_topics_where(|topic| users.contains(&topic.user))
			.into_iter()
			.map(|topic| (topic.user, topic.device))
			.collect()
	}

	/// Whether any connection holds any device's queue; lets a hook skip its
	/// work entirely on a server nobody is subscribed to.
	#[must_use]
	pub fn is_any_device_held(&self) -> bool { self.devices.is_any_topic_listened() }

	/// Pushes a `CryptoState` to whoever holds this device's queue, cut into
	/// as many packs as the user lists need to fit `meta_max`. Every pack
	/// carries the counts, so any one of them is a complete report of those.
	///
	/// Args:
	///     state: what to report
	///     meta_max: `wbf_meta_max_bytes`
	pub fn push_crypto_state(&self, user: &UserId, device: &DeviceId, state: &CryptoState<'_>, meta_max: usize) {
		let topic = DeviceTopic::new(user, device);
		let holder: Vec<ConnectionId> = self
			.devices
			.listeners(&topic)
			.into_iter()
			.map(|(connection, _)| connection)
			.collect();

		for connection in holder {
			// Not caught up yet: the catch-up it is about to run reads this
			// change, because the change was written before this push.
			let Some(dl_seq) = self.find_device_list_position(connection) else {
				continue;
			};
			let budget = meta_max.saturating_sub(crypto_state_meta_overhead(state));
			for (changed, left) in split_user_lists(state.changed, state.left, budget) {
				self.devices.push_with(Some(&topic), &[connection], |id, seq, gap| {
					crypto_state_pack(id, seq, &crypto_state_meta(state, changed, left, dl_seq, gap))
				});
			}
		}
	}

	fn find_device_list_position(&self, connection: ConnectionId) -> Option<u64> {
		self.device_list_positions
			.read()
			.expect("device list positions lock poisoned")
			.get(&connection)
			.copied()
	}

	/// Whether this device's queue is held by a connection, and by which.
	///
	/// Return:
	///     Option<ConnectionId>  None when nobody holds it.
	#[must_use]
	pub fn device_holder(&self, user: &UserId, device: &DeviceId) -> Option<ConnectionId> {
		self.devices
			.listeners(&DeviceTopic::new(user, device))
			.first()
			.map(|(connection, _)| *connection)
	}

	/// Pushes newly arrived items to whoever holds this device's queue.
	/// Nobody holding it is the ordinary case (the device is offline), and
	/// costs one lookup.
	///
	/// Args:
	///     user: whose queue
	///     device: which device's queue
	///     items: oldest first — the order the client imports and destroys in
	///     per_pack: `wbf_push_max_events_per_pack`
	///     data_max: `wbf_data_max_bytes`
	pub fn push_to_device(
		&self,
		user: &UserId,
		device: &DeviceId,
		items: &[PushedItem<'_>],
		per_pack: usize,
		data_max: usize,
	) {
		let topic = DeviceTopic::new(user, device);
		let holder: Vec<ConnectionId> = self
			.devices
			.listeners(&topic)
			.into_iter()
			.map(|(connection, _)| connection)
			.collect();
		if holder.is_empty() {
			return;
		}

		for range in list_pack_ranges(items.iter().map(|item| item.json.len()), per_pack, data_max) {
			self.push_items(&topic, &holder, &items[range]);
		}
	}

	/// Pushes the catch-up a `Device/Subscribe` with `cd_seq` asked for.
	///
	/// 🚨 Like the rooms' `push_window`: a window cut short by its count or its
	/// bytes is only the oldest part of what is waiting, so its first pack
	/// carries `gap: true` and the client `Fetch`es the rest — rather than
	/// taking the last `nt` it was pushed for the end of its queue.
	///
	/// Args:
	///     items: oldest first, example: the first 1000 after `cd_seq`
	///     is_cut_short: the window stopped at a cap
	pub fn push_device_window(
		&self,
		user: &UserId,
		device: &DeviceId,
		items: &[PushedItem<'_>],
		is_cut_short: bool,
		per_pack: usize,
		data_max: usize,
	) {
		if is_cut_short {
			let holder: Vec<ConnectionId> = self
				.devices
				.listeners(&DeviceTopic::new(user, device))
				.into_iter()
				.map(|(connection, _)| connection)
				.collect();
			self.devices.mark_gap(&holder);
		}
		self.push_to_device(user, device, items, per_pack, data_max);
	}

	fn push_items(&self, topic: &DeviceTopic, holder: &[ConnectionId], items: &[PushedItem<'_>]) {
		if items.is_empty() {
			return;
		}
		let Ok(data) = length_prefixed(items.iter().map(|item| item.json)) else {
			debug!("wbf device push skipped: an item does not fit a length prefix");
			self.devices.mark_gap(holder);
			return;
		};
		// Oldest first, so `ot` is the first and `nt` is the last — the
		// mirror image of the rooms, where `fs` is the newest. Different
		// names because the same names would read as the same thing.
		let ot = items.first().map_or(0, |item| item.count);
		let nt = items.last().map_or(0, |item| item.count);
		let counts: Vec<u64> = items.iter().map(|item| item.count).collect();
		let bc = items.len();

		self.devices
			.push_with(Some(topic), holder, |id, seq, gap| {
				device_push_pack(id, seq, bc, ot, nt, &counts, gap, &data)
			});
	}
}

/// The last pack of a subscription that was taken over: an `Error` carrying
/// the **displaced connection's own** `id` and the next `seq` of its
/// conversation, with `IS_LAST` — "that subscription of yours ends here", not
/// "your request failed" (wire-format §3.4).
fn superseded_pack(id: u64, seq: u32) -> Result<Vec<u8>, PackError> {
	let code = RejectCode::Superseded;
	Ok(
		PackBuilder::new(
			Kind::Control,
			CONTROL_ERROR_SUBTYPE,
			Flags::IS_RESPONSE.union(Flags::IS_LAST),
			id,
			seq,
		)
		.json_meta(&json!({
			"code_id": code.id(),
			"code": code.name(),
			"message": "another connection of this device took its to-device queue over",
		}))?
		.finish(),
	)
}

/// Args:
///     state: the counts, and the full lists (only their shape matters here)
///     changed: this pack's part of `state.changed`
///     left: this pack's part of `state.left`
/// Return:
///     Value  the `CryptoState` meta, every field present
fn crypto_state_meta(state: &CryptoState<'_>, changed: &[OwnedUserId], left: &[OwnedUserId], dl_seq: u64, gap: bool) -> Value {
	json!({
		"otk_counts": state.otk_counts,
		"unused_fallback_key_types": state.unused_fallback_key_types,
		"device_lists": { "changed": changed, "left": left },
		"dl_seq": dl_seq,
		"gap": gap,
	})
}

fn crypto_state_pack(id: u64, seq: u32, meta: &Value) -> Result<Vec<u8>, PackError> {
	Ok(PackBuilder::new(Kind::Device, DEVICE_CRYPTO_STATE_SUBTYPE, Flags::IS_RESPONSE, id, seq)
		.json_meta(meta)?
		.finish())
}

/// Bytes of a `CryptoState` meta with both lists empty and the widest
/// `dl_seq` and `gap`: what every pack spends before its user ids.
fn crypto_state_meta_overhead(state: &CryptoState<'_>) -> usize {
	serde_json::to_vec(&crypto_state_meta(state, &[], &[], u64::MAX, false)).map_or(usize::MAX, |meta| meta.len())
}

/// Cuts the two lists into consecutive parts whose user ids fit `budget`
/// bytes each, `changed` first.
///
/// Args:
///     budget: bytes of meta left for user ids in one pack, example: 65000
/// Return:
///     Vec<(&[OwnedUserId], &[OwnedUserId])>  at least one part, even when
///     both lists are empty (the counts are still reported); a user id
///     larger than `budget` gets a part of its own, and the pack it makes
///     fails to encode and marks the subscription's gap.
fn split_user_lists<'a>(
	changed: &'a [OwnedUserId],
	left: &'a [OwnedUserId],
	budget: usize,
) -> Vec<(&'a [OwnedUserId], &'a [OwnedUserId])> {
	// A quoted id and its comma; ids need no escaping.
	let cost = |user: &OwnedUserId| user.as_str().len().saturating_add(3);

	let mut parts = Vec::new();
	let (mut changed_at, mut left_at) = (0, 0);
	loop {
		let (changed_from, left_from) = (changed_at, left_at);
		let mut used = 0_usize;
		while changed_at < changed.len() && (changed_at == changed_from || used.saturating_add(cost(&changed[changed_at])) <= budget) {
			used = used.saturating_add(cost(&changed[changed_at]));
			changed_at += 1;
		}
		let is_part_empty = changed_at == changed_from;
		while changed_at == changed.len()
			&& left_at < left.len()
			&& ((is_part_empty && left_at == left_from) || used.saturating_add(cost(&left[left_at])) <= budget)
		{
			used = used.saturating_add(cost(&left[left_at]));
			left_at += 1;
		}
		parts.push((&changed[changed_from..changed_at], &left[left_from..left_at]));
		if changed_at == changed.len() && left_at == left.len() {
			return parts;
		}
	}
}

fn device_push_pack(
	id: u64,
	seq: u32,
	bc: usize,
	ot: u64,
	nt: u64,
	counts: &[u64],
	gap: bool,
	data: &[u8],
) -> Result<Vec<u8>, PackError> {
	Ok(PackBuilder::new(Kind::Device, DEVICE_PUSH_SUBTYPE, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({ "bc": bc, "ot": ot, "nt": nt, "counts": counts, "gap": gap }))?
		.data(data)?
		.finish())
}

#[cfg(test)]
mod tests {
	use ruma::{OwnedUserId, device_id, user_id};
	use tokio::sync::mpsc;
	use tuwunel_core::wbf::{CONTROL_ERROR_SUBTYPE, Kind, decode, events::split_length_prefixed};

	use super::{
		CryptoState, DEVICE_CRYPTO_STATE_SUBTYPE, DEVICE_PUSH_SUBTYPE, PushedItem, crypto_state_meta, crypto_state_pack,
		split_user_lists,
	};
	use crate::streams::{Outgoing, PackQueue, Queued, Streams};

	/// A test queue: the count is what these tests exercise, so the byte
	/// budget is set far above anything they send.
	fn queue(capacity: usize) -> (PackQueue, mpsc::Receiver<Queued>) { PackQueue::new(capacity, 16 * 1024 * 1024) }

	fn next_pack(rx: &mut mpsc::Receiver<Queued>) -> Vec<u8> {
		match rx.try_recv().expect("a pack was queued").outgoing {
			| Outgoing::Pack(pack) => pack,
			| Outgoing::Close { .. } => panic!("expected a pack, got a close"),
		}
	}

	#[test]
	fn a_later_connection_takes_the_queue_over_and_the_first_is_told() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (first, mut first_rx) = queue(4);
		let (second, _second_rx) = queue(4);

		assert_eq!(streams.subscribe_device(1, alice, phone, first, 10), None, "nobody held it");
		let displaced = streams.subscribe_device(2, alice, phone, second, 11);

		assert_eq!(displaced, Some(1), "the later connection wins");
		assert_eq!(streams.device_holder(alice, phone), Some(2));

		// ⚠️ Being cut off in silence is the failure this notice exists to
		// prevent: the old connection would sit waiting for keys forever.
		let mut pack = next_pack(&mut first_rx);
		let view = decode(&mut pack).expect("decodes");
		assert_eq!(view.header.kind, Kind::Control);
		assert_eq!(view.header.subtype, CONTROL_ERROR_SUBTYPE);
		assert_eq!(view.header.id, 10, "the displaced connection's own subscription id");
		assert!(view.header.flags.is_last(), "that conversation is over");
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["code_id"], 1505);
		assert_eq!(meta["code"], "Superseded");
	}

	#[test]
	fn the_holder_may_subscribe_again_without_displacing_itself() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(4);
		let (again, _rx2) = queue(4);
		let (other, _rx3) = queue(4);

		assert_eq!(streams.subscribe_device(1, alice, phone, tx, 10), None);
		assert_eq!(
			streams.subscribe_device(1, alice, phone, again, 12),
			None,
			"a connection is not somebody else"
		);
		assert!(rx.try_recv().is_err(), "and it is not told its own subscription ended");

		streams.unsubscribe_device(1);

		assert_eq!(streams.device_holder(alice, phone), None);
		assert_eq!(
			streams.subscribe_device(2, alice, phone, other, 13),
			None,
			"a freed queue displaces nobody"
		);
	}

	#[test]
	fn a_push_carries_each_item_count_oldest_first() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(4);
		streams.subscribe_device(1, alice, phone, tx, 42);

		let items = [
			PushedItem { count: 500, json: b"{\"a\":1}" },
			PushedItem { count: 501, json: b"{\"b\":2}" },
		];
		streams.push_to_device(alice, phone, &items, 10, 1024);

		let mut pack = match rx.try_recv().expect("a pack was queued").outgoing {
			| Outgoing::Pack(pack) => pack,
			| Outgoing::Close { .. } => panic!("expected a pack, got a close"),
		};
		let view = decode(&mut pack).expect("decodes");
		assert_eq!(view.header.kind, Kind::Device);
		assert_eq!(view.header.subtype, DEVICE_PUSH_SUBTYPE);
		assert_eq!(view.header.id, 42, "the subscription's id, not the room's");
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["bc"], 2);
		assert_eq!(meta["ot"], 500, "oldest first");
		assert_eq!(meta["nt"], 501);
		assert_eq!(meta["counts"], serde_json::json!([500, 501]));
		assert_eq!(meta["gap"], false);
		assert_eq!(
			split_length_prefixed(view.data).expect("items"),
			vec![b"{\"a\":1}".as_slice(), b"{\"b\":2}".as_slice()]
		);
	}

	#[test]
	fn a_device_catch_up_cut_short_says_gap_on_its_first_pack() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(4);
		streams.subscribe_device(1, alice, phone, tx, 42);
		let items: Vec<PushedItem<'_>> = (0..3).map(|n| PushedItem { count: 500 + n, json: b"{}" }).collect();

		streams.push_device_window(alice, phone, &items, true, 2, 1024);

		let gaps: Vec<bool> = (0..2)
			.map(|_| {
				let mut pack = match rx.try_recv().expect("a pack was queued").outgoing {
					| Outgoing::Pack(pack) => pack,
					| Outgoing::Close { .. } => panic!("expected a pack, got a close"),
				};
				let view = decode(&mut pack).expect("decodes");
				view.meta_json().expect("meta")["gap"].as_bool().expect("gap")
			})
			.collect();
		assert_eq!(gaps, vec![true, false], "said once, on the first pack");
	}

	fn crypto_meta(rx: &mut mpsc::Receiver<Queued>) -> (u8, u64, u32, serde_json::Value) {
		let mut pack = next_pack(rx);
		let view = decode(&mut pack).expect("decodes");
		(view.header.subtype, view.header.id, view.header.seq, view.meta_json().expect("meta"))
	}

	#[test]
	fn a_crypto_state_carries_every_field_even_when_empty() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(4);
		streams.subscribe_device(1, alice, phone, tx, 42);
		streams.set_device_list_position(1, 900);

		let counts = serde_json::json!({});
		streams.push_crypto_state(
			alice,
			phone,
			&CryptoState { otk_counts: &counts, unused_fallback_key_types: &[], changed: &[], left: &[] },
			64 * 1024,
		);

		let (subtype, id, _, meta) = crypto_meta(&mut rx);
		assert_eq!(subtype, DEVICE_CRYPTO_STATE_SUBTYPE);
		assert_eq!(id, 42, "the Subscribe's id, like Push");
		assert_eq!(
			meta,
			serde_json::json!({
				"otk_counts": {},
				// ⚠️ `[]` is "all used"; leaving the field out would read as
				// "this server does not support fallback keys".
				"unused_fallback_key_types": [],
				"device_lists": { "changed": [], "left": [] },
				"dl_seq": 900,
				"gap": false,
			})
		);
	}

	#[test]
	fn a_connection_not_yet_caught_up_is_not_pushed_a_crypto_state() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(4);
		streams.subscribe_device(1, alice, phone, tx, 42);

		let counts = serde_json::json!({});
		let state = CryptoState { otk_counts: &counts, unused_fallback_key_types: &[], changed: &[], left: &[] };
		streams.push_crypto_state(alice, phone, &state, 64 * 1024);
		assert!(rx.try_recv().is_err(), "no dl_seq to stamp: its catch-up reads the change instead");

		streams.set_device_list_position(1, 7);
		streams.unsubscribe_device(1);
		let (again, mut again_rx) = queue(4);
		streams.subscribe_device(1, alice, phone, again, 43);
		streams.push_crypto_state(alice, phone, &state, 64 * 1024);
		assert!(again_rx.try_recv().is_err(), "an unsubscribe forgets the position; a new subscription starts over");
	}

	#[test]
	fn push_and_crypto_state_share_one_seq() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(8);
		streams.subscribe_device(1, alice, phone, tx, 42);
		streams.set_device_list_position(1, 1);

		let counts = serde_json::json!({"signed_curve25519": 3});
		let state = CryptoState { otk_counts: &counts, unused_fallback_key_types: &[], changed: &[], left: &[] };
		streams.push_to_device(alice, phone, &[PushedItem { count: 5, json: b"{}" }], 10, 1024);
		streams.push_crypto_state(alice, phone, &state, 64 * 1024);
		streams.push_to_device(alice, phone, &[PushedItem { count: 6, json: b"{}" }], 10, 1024);

		let seqs: Vec<(u8, u32)> = (0..3)
			.map(|_| {
				let mut pack = next_pack(&mut rx);
				let view = decode(&mut pack).expect("decodes");
				(view.header.subtype, view.header.seq)
			})
			.collect();
		assert_eq!(
			seqs,
			vec![(DEVICE_PUSH_SUBTYPE, 0), (DEVICE_CRYPTO_STATE_SUBTYPE, 1), (DEVICE_PUSH_SUBTYPE, 2)],
			"one subscription, one seq across both kinds"
		);
	}

	#[test]
	fn a_long_user_list_is_cut_into_packs_that_each_fit_and_lose_nobody() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(64);
		streams.subscribe_device(1, alice, phone, tx, 42);
		streams.set_device_list_position(1, 1);

		let changed: Vec<OwnedUserId> = (0..40).map(|n| format!("@changed{n:03}:localhost").try_into().expect("id")).collect();
		let left: Vec<OwnedUserId> = (0..25).map(|n| format!("@left{n:03}:localhost").try_into().expect("id")).collect();
		let counts = serde_json::json!({"signed_curve25519": 3});
		let meta_max = 400;
		streams.push_crypto_state(
			alice,
			phone,
			&CryptoState { otk_counts: &counts, unused_fallback_key_types: &[], changed: &changed, left: &left },
			meta_max,
		);

		let (mut seen_changed, mut seen_left) = (Vec::new(), Vec::new());
		while let Ok(queued) = rx.try_recv() {
			let Outgoing::Pack(mut pack) = queued.outgoing else { panic!("a pack") };
			let view = decode(&mut pack).expect("decodes");
			assert!(view.meta.len() <= meta_max, "a part of {} bytes", view.meta.len());
			let meta = view.meta_json().expect("meta");
			assert_eq!(meta["otk_counts"]["signed_curve25519"], 3, "every part carries the counts");
			for user in meta["device_lists"]["changed"].as_array().expect("changed") {
				seen_changed.push(user.as_str().expect("id").to_owned());
			}
			for user in meta["device_lists"]["left"].as_array().expect("left") {
				seen_left.push(user.as_str().expect("id").to_owned());
			}
		}
		assert_eq!(seen_changed, changed.iter().map(ToString::to_string).collect::<Vec<_>>());
		assert_eq!(seen_left, left.iter().map(ToString::to_string).collect::<Vec<_>>());
	}

	/// The golden vectors are what clients test their decoders against, so the
	/// two `CryptoState` vectors must be the bytes this server builds — not a
	/// hand-written example that happens to decode.
	#[test]
	fn the_crypto_state_vectors_are_what_the_server_builds() {
		const VECTORS: &str = include_str!("../../../docs/design/wbf-vectors.json");
		let vectors: serde_json::Value = serde_json::from_str(VECTORS).expect("the vectors file is JSON");
		let bytes_of = |name: &str| -> Vec<u8> {
			let hex = vectors["packs"]
				.as_array()
				.expect("a packs list")
				.iter()
				.find(|vector| vector["name"] == name)
				.unwrap_or_else(|| panic!("no vector named {name}"))["bytes_hex"]
				.as_str()
				.expect("hex")
				.to_owned();
			(0..hex.len())
				.step_by(2)
				.map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex digit"))
				.collect()
		};
		let conversation_30 = tuwunel_core::wbf::IdType::ClientConversation
			.compose(30)
			.expect("fits");

		let counts = serde_json::json!({"signed_curve25519": 42});
		let fallback = ["signed_curve25519".to_owned()];
		let changed: Vec<OwnedUserId> = vec!["@bob:example.org".try_into().expect("id")];
		let left: Vec<OwnedUserId> = vec!["@carol:example.org".try_into().expect("id")];
		let state = CryptoState { otk_counts: &counts, unused_fallback_key_types: &fallback, changed: &changed, left: &left };
		let built = crypto_state_pack(conversation_30, 1, &crypto_state_meta(&state, &changed, &left, 4730, false)).expect("builds");
		assert_eq!(built, bytes_of("device_crypto_state"));

		let empty_counts = serde_json::json!({});
		let empty = CryptoState { otk_counts: &empty_counts, unused_fallback_key_types: &[], changed: &[], left: &[] };
		let built = crypto_state_pack(conversation_30, 2, &crypto_state_meta(&empty, &[], &[], 4730, false)).expect("builds");
		assert_eq!(built, bytes_of("device_crypto_state_empty"));
	}

	#[test]
	fn splitting_always_makes_progress_and_at_least_one_part() {
		let one: Vec<OwnedUserId> = vec!["@a:localhost".try_into().expect("id")];
		assert_eq!(split_user_lists(&[], &[], 100).len(), 1, "empty lists still report the counts once");
		assert_eq!(split_user_lists(&one, &one, 0).len(), 2, "a budget too small for any id: one id per part, no endless loop");
		assert_eq!(split_user_lists(&one, &one, 1000), vec![(one.as_slice(), one.as_slice())]);
	}

	#[test]
	fn nothing_is_queued_when_no_connection_holds_the_queue() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");

		// The ordinary case: the device is offline.
		streams.push_to_device(alice, phone, &[PushedItem { count: 1, json: b"{}" }], 10, 1024);

		assert_eq!(streams.device_holder(alice, phone), None);
	}
}
