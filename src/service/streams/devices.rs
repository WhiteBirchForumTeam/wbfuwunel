//! The to-device queue (`0x16 Device`): one topic per device, the rule that
//! only one connection may hold it, and the `Push` pack's shape
//! (`docs/design/wbf-to-device.md`).
//!
//! What makes this different from the room channels is that the client
//! **destroys** what it has taken: the queue is the only copy, and the server
//! keeps each item until the device says it is safely stored. So the
//! subscription is exclusive — two connections of one device both taking and
//! destroying would let one of them delete an item the other was still
//! importing, and that item is gone for good.

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde_json::json;
use tokio::sync::mpsc::Sender;
use tuwunel_core::{
	debug,
	wbf::{
		Flags, Kind, PackBuilder, PackError,
		events::{length_prefixed, list_pack_ranges},
	},
};

use super::{ConnectionId, Outgoing, Streams};

/// `Device/Push`, server to client only.
pub const DEVICE_PUSH_SUBTYPE: u8 = 0x06;

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

/// Why a `Device/Subscribe` was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceSubscribeError {
	/// Another connection is already taking this device's queue.
	TakenByAnotherConnection,
}

impl DeviceTopic {
	fn new(user: &UserId, device: &DeviceId) -> Self {
		Self { user: user.to_owned(), device: device.to_owned() }
	}
}

impl Streams {
	/// Binds `connection` to this device's queue and subscribes it.
	///
	/// The caller has already checked that `device` is the one this
	/// connection's session holds: the session is the only source of
	/// identity, and a connection asking for another device's queue is
	/// refused before it gets here.
	///
	/// Args:
	///     connection: example: 7
	///     user: who the connection is
	///     device: whose queue, from the session
	///     queue: the connection's send queue
	///     id: the client's `Subscribe` id, example: 42
	/// Return:
	///     Result<(), DeviceSubscribeError>  refused when another connection
	///     already holds this device's queue — 🚫 the later one does not
	///     take it over, because the holder may be mid-import and the items
	///     it is importing would be destroyed under it.
	pub fn subscribe_device(
		&self,
		connection: ConnectionId,
		user: &UserId,
		device: &DeviceId,
		queue: Sender<Outgoing>,
		id: u64,
	) -> Result<(), DeviceSubscribeError> {
		let topic = DeviceTopic::new(user, device);
		let holder = self.devices.listeners(&topic);
		let is_taken = holder
			.iter()
			.any(|(other, _)| *other != connection);
		if is_taken {
			return Err(DeviceSubscribeError::TakenByAnotherConnection);
		}

		self.devices
			.subscribe(connection, user, queue, id, &[topic]);

		Ok(())
	}

	/// Releases this connection's hold on its device queue, if it has one.
	/// Both ways out must do this — the spoken one (`Unsubscribe`) and the
	/// silent one (the connection ending) — or the device stays taken and no
	/// other connection can ever subscribe to it.
	pub fn unsubscribe_device(&self, connection: ConnectionId) {
		self.devices.remove_connection(connection);
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
	use ruma::{device_id, user_id};
	use tokio::sync::mpsc;
	use tuwunel_core::wbf::{Kind, decode, events::split_length_prefixed};

	use super::{DEVICE_PUSH_SUBTYPE, DeviceSubscribeError, PushedItem};
	use crate::streams::{Outgoing, Streams};

	fn queue(capacity: usize) -> (mpsc::Sender<Outgoing>, mpsc::Receiver<Outgoing>) { mpsc::channel(capacity) }

	#[test]
	fn a_second_connection_is_refused_and_the_first_keeps_the_queue() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (first, _rx) = queue(4);
		let (second, _rx2) = queue(4);

		streams
			.subscribe_device(1, alice, phone, first, 10)
			.expect("nobody holds it yet");
		let refused = streams.subscribe_device(2, alice, phone, second, 11);

		assert_eq!(refused, Err(DeviceSubscribeError::TakenByAnotherConnection));
		assert_eq!(streams.device_holder(alice, phone), Some(1), "the holder did not change");
	}

	#[test]
	fn the_holder_may_subscribe_again_and_the_queue_frees_when_it_leaves() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, _rx) = queue(4);
		let (again, _rx2) = queue(4);
		let (other, _rx3) = queue(4);

		streams
			.subscribe_device(1, alice, phone, tx, 10)
			.expect("free");
		streams
			.subscribe_device(1, alice, phone, again, 12)
			.expect("the same connection is not somebody else");

		streams.unsubscribe_device(1);

		assert_eq!(streams.device_holder(alice, phone), None);
		streams
			.subscribe_device(2, alice, phone, other, 13)
			.expect("the next connection can have it");
	}

	#[test]
	fn a_push_carries_each_item_count_oldest_first() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let (tx, mut rx) = queue(4);
		streams
			.subscribe_device(1, alice, phone, tx, 42)
			.expect("free");

		let items = [
			PushedItem { count: 500, json: b"{\"a\":1}" },
			PushedItem { count: 501, json: b"{\"b\":2}" },
		];
		streams.push_to_device(alice, phone, &items, 10, 1024);

		let mut pack = match rx.try_recv().expect("a pack was queued") {
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
	fn nothing_is_queued_when_no_connection_holds_the_queue() {
		let streams = Streams::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");

		// The ordinary case: the device is offline.
		streams.push_to_device(alice, phone, &[PushedItem { count: 1, json: b"{}" }], 10, 1024);

		assert_eq!(streams.device_holder(alice, phone), None);
	}
}
