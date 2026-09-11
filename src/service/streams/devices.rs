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

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde_json::json;
use tokio::sync::mpsc::Sender;
use tuwunel_core::{
	debug,
	wbf::{
		CONTROL_ERROR_SUBTYPE, Flags, Kind, PackBuilder, PackError, RejectCode,
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
		queue: Sender<Outgoing>,
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
	use tuwunel_core::wbf::{CONTROL_ERROR_SUBTYPE, Kind, decode, events::split_length_prefixed};

	use super::{DEVICE_PUSH_SUBTYPE, PushedItem};
	use crate::streams::{Outgoing, Streams};

	fn queue(capacity: usize) -> (mpsc::Sender<Outgoing>, mpsc::Receiver<Outgoing>) { mpsc::channel(capacity) }

	fn next_pack(rx: &mut mpsc::Receiver<Outgoing>) -> Vec<u8> {
		match rx.try_recv().expect("a pack was queued") {
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
