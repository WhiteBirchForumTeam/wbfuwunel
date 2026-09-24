//! Tasks that outlive the request that started them, and so must be joined
//! before `Services` is dropped.
//!
//! A request handler borrows `Services` through the router's `State`, a raw
//! pointer whose safety argument is "every request has finished before
//! `Services` goes". A WebSocket breaks that: after the `101` the request is
//! over, but the task serving the socket keeps running. Such tasks are
//! spawned here so that `Services::stop` can wait for them; a task that would
//! start while shutdown is already under way is refused instead.
//!
//! This is also where connections are counted, twice over: a `ConnectionSlot`
//! is one connection's place in its device's count (§2.1) and an `AddressSlot`
//! is its place in its source address's count (§2.2). Both are taken before
//! the connection is accepted and given back when the slot is dropped,
//! whichever way the connection ended (`docs/design/wbf-pack-pipeline.md`).
//!
//! ⭐ The two answer different questions and neither replaces the other: the
//! device count needs an identity, so it cannot see a connection that has not
//! logged in; the address count is the only one that reaches those.

use std::{
	collections::HashMap,
	net::IpAddr,
	sync::{Arc, Mutex},
	time::Duration,
};

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use tokio::task::JoinSet;
use tuwunel_core::{error, info, warn};

/// How long `close_and_join` waits for connections to end on their own
/// before aborting the rest. Each connection's loop returns as soon as it
/// sees the server stopping, so this only ever runs out when a task is stuck
/// inside one call that never returns; then it must not hold the whole
/// shutdown hostage (review of PR #28, rumia).
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

/// One device's open connections, keyed by who the connection is.
type SlotTable = Arc<Mutex<HashMap<(OwnedUserId, OwnedDeviceId), u32>>>;

/// Open connections per source address, keyed by `to_address_group`.
type AddressTable = Arc<Mutex<HashMap<IpAddr, u32>>>;

pub struct Connections {
	/// `None` once `close_and_join` has begun: nothing may start after that.
	tasks: Mutex<Option<JoinSet<()>>>,
	/// How many connections each (user, device) has right now. The only
	/// count there is: a slot is taken here and given back by its drop.
	slots: SlotTable,
	/// How many connections each source address group has right now, on the
	/// same terms as `slots`.
	addresses: AddressTable,
}

/// What an address counts as, for the per-address limit.
///
/// Args:
///     address: the peer as the transport resolved it, example: 203.0.113.7
/// Return:
///     IpAddr  IPv4 unchanged; IPv6 with everything below its /64 zeroed,
///     so every address in one /64 shares a count.
///
/// ⚠️ IPv6 is grouped because a household is normally given a whole /64 (often
/// a /56): counting exact addresses would let a client take a fresh one per
/// connection at no cost, and the limit would bound nothing.
///
/// 🚨 The address is canonicalized first, and that line is load-bearing: a
/// dual-stack listener (`[::]`, which `router/serve.rs` sets up on purpose)
/// hands IPv4 peers over as `::ffff:a.b.c.d`, whose IPv4 part lives in the
/// very bytes the /64 mask clears — without it every IPv4 client in the world
/// would share the group `::`, and the limit would silently become one for
/// the whole server (PR #85 review, cirno). `peer_is_trusted` in
/// `api/router/client_ip.rs` canonicalizes for the same reason.
#[must_use]
pub fn to_address_group(address: IpAddr) -> IpAddr {
	match address.to_canonical() {
		| IpAddr::V4(v4) => IpAddr::V4(v4),
		| IpAddr::V6(v6) => {
			let mut octets = v6.octets();
			octets[8..].fill(0);
			IpAddr::V6(octets.into())
		},
	}
}

/// One connection's place in its source address's count. Dropping it gives
/// the place back, exactly as `ConnectionSlot` does.
pub struct AddressSlot {
	table: AddressTable,
	group: IpAddr,
}

impl Drop for AddressSlot {
	fn drop(&mut self) {
		// A poisoned lock still holds a usable count: a panic elsewhere must
		// not leak this connection's place for the server's whole life
		// (CLAUDE.md P), so the guard is taken either way.
		let mut table = match self.table.lock() {
			| Ok(table) => table,
			| Err(poisoned) => poisoned.into_inner(),
		};
		match table.get_mut(&self.group) {
			| Some(count) if *count > 1 => *count -= 1,
			| _ => {
				table.remove(&self.group);
			},
		}
	}
}

/// One connection's place in the per-device count. Dropping it gives the
/// place back, so a connection holds it for exactly as long as it lives,
/// however it ends.
pub struct ConnectionSlot {
	table: SlotTable,
	key: (OwnedUserId, OwnedDeviceId),
}

impl ConnectionSlot {
	/// Whether this slot counts for `user`'s `device`.
	#[must_use]
	pub fn is_for(&self, user: &UserId, device: &DeviceId) -> bool { self.key.0 == user && self.key.1 == device }
}

impl Drop for ConnectionSlot {
	fn drop(&mut self) {
		let mut table = self.table.lock().expect("connection slots lock poisoned");
		match table.get_mut(&self.key) {
			| Some(count) if *count > 1 => *count -= 1,
			| _ => {
				table.remove(&self.key);
			},
		}
	}
}

impl Default for Connections {
	fn default() -> Self { Self::new() }
}

impl Connections {
	#[must_use]
	pub fn new() -> Self {
		Self {
			tasks: Mutex::new(Some(JoinSet::new())),
			slots: Arc::new(Mutex::new(HashMap::new())),
			addresses: Arc::new(Mutex::new(HashMap::new())),
		}
	}

	/// Takes one place in `user`'s `device` count, if there is one left.
	///
	/// Args:
	///     user: example: @alice:localhost
	///     device: example: RJYKSTBOIE
	///     max: `wbf_ws_max_connections_per_device`, example: 4; 0 means no
	///         limit and no slot is taken
	/// Return:
	///     Option<Option<ConnectionSlot>>  None when the device already has
	///     `max` connections (the caller refuses the new one); Some(None) when
	///     `max` is 0; Some(Some(slot)) otherwise, held for the connection's life.
	pub fn take_slot(&self, user: &UserId, device: &DeviceId, max: u32) -> Option<Option<ConnectionSlot>> {
		if max == 0 {
			return Some(None);
		}

		let key = (user.to_owned(), device.to_owned());
		let mut table = self.slots.lock().expect("connection slots lock poisoned");
		let count = table.entry(key.clone()).or_insert(0);
		if *count >= max {
			return None;
		}
		*count += 1;
		drop(table);

		Some(Some(ConnectionSlot { table: Arc::clone(&self.slots), key }))
	}

	/// Takes one place in `address`'s count, if there is one left.
	///
	/// ⭐ Unlike `take_slot` this is asked **before** the token is read, so it
	/// also bounds connections that never log in — the per-device count
	/// cannot see those at all (§2.2).
	///
	/// Args:
	///     address: the peer as the transport resolved it, example:
	///         203.0.113.7; IPv6 is counted per /64 (`to_address_group`)
	///     max: `wbf_ws_max_connections_per_address`, example: 40; 0 means no
	///         limit and no slot is taken
	/// Return:
	///     Option<Option<AddressSlot>>  None when the address group is already
	///     at `max` (the caller refuses the new connection and leaves the open
	///     ones alone); Some(None) when `max` is 0; Some(Some(slot)) otherwise,
	///     held for the connection's life.
	pub fn take_address_slot(&self, address: IpAddr, max: u32) -> Option<Option<AddressSlot>> {
		if max == 0 {
			return Some(None);
		}

		let group = to_address_group(address);
		let mut table = match self.addresses.lock() {
			| Ok(table) => table,
			| Err(poisoned) => poisoned.into_inner(),
		};
		let count = table.entry(group).or_insert(0);
		if *count >= max {
			return None;
		}
		*count += 1;
		drop(table);

		Some(Some(AddressSlot { table: Arc::clone(&self.addresses), group }))
	}

	/// How many connections `address`'s group holds right now; for tests and
	/// the admin room.
	#[must_use]
	pub fn count_for_address(&self, address: IpAddr) -> u32 {
		match self.addresses.lock() {
			| Ok(table) => table,
			| Err(poisoned) => poisoned.into_inner(),
		}
		.get(&to_address_group(address))
		.copied()
		.unwrap_or(0)
	}

	/// How many connections `user`'s `device` holds right now; for tests and
	/// the admin room.
	#[must_use]
	pub fn count_for(&self, user: &UserId, device: &DeviceId) -> u32 {
		self.slots
			.lock()
			.expect("connection slots lock poisoned")
			.get(&(user.to_owned(), device.to_owned()))
			.copied()
			.unwrap_or(0)
	}

	/// Starts `task` as a tracked connection.
	///
	/// Args:
	///     task: the future serving one connection to its end, example: the
	///         WebSocket read-answer loop
	/// Return:
	///     bool  true when started; false when shutdown has begun and the task
	///     was not started (the caller drops the connection).
	#[must_use = "false means the task was not started; the caller has to drop the connection"]
	pub fn spawn<F>(&self, task: F) -> bool
	where
		F: Future<Output = ()> + Send + 'static,
	{
		let mut tasks = self.tasks.lock().expect("connections lock poisoned");
		match tasks.as_mut() {
			| Some(set) => {
				set.spawn(task);
				true
			},
			| None => false,
		}
	}

	/// Refuses new connections from now on and waits for the running ones to
	/// end. Each connection's loop ends on its own when it sees the server
	/// stopping, so this normally returns once they have all noticed; one that
	/// has not after `JOIN_TIMEOUT` is aborted, which drops its future and
	/// with it every borrow of `Services`.
	pub async fn close_and_join(&self) {
		let Some(mut set) = self
			.tasks
			.lock()
			.expect("connections lock poisoned")
			.take()
		else {
			return;
		};

		// Logged at info: it happens once per shutdown, and it is the line an
		// operator (or an end-to-end test) reads to know the connections were
		// waited for rather than abandoned.
		let open = set.len();
		if open > 0 {
			info!(open, "Waiting for long-lived connections to end...");
		}
		let drain = async {
			while let Some(joined) = set.join_next().await {
				if let Err(e) = joined {
					error!(?e, "A connection task ended abnormally.");
				}
			}
		};
		if tokio::time::timeout(JOIN_TIMEOUT, drain).await.is_err() {
			let stuck = set.len();
			warn!(stuck, timeout = ?JOIN_TIMEOUT, "Connections still running after the timeout; aborting them.");
			set.abort_all();
			while set.join_next().await.is_some() {}
		}
		if open > 0 {
			info!(open, "Long-lived connections ended.");
		}
	}
}

#[cfg(test)]
mod tests {
	use ruma::{device_id, user_id};

	use super::Connections;

	#[test]
	fn the_limit_counts_per_device_and_a_dropped_slot_frees_its_place() {
		let connections = Connections::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");
		let desk = device_id!("DESK");

		let first = connections.take_slot(alice, phone, 2).expect("first fits").expect("a slot");
		let second = connections.take_slot(alice, phone, 2).expect("second fits").expect("a slot");
		assert!(connections.take_slot(alice, phone, 2).is_none(), "third is refused");
		assert_eq!(connections.count_for(alice, phone), 2);

		// Another device of the same user is its own count.
		let other = connections.take_slot(alice, desk, 2).expect("other device fits").expect("a slot");
		assert_eq!(connections.count_for(alice, desk), 1);

		drop(second);
		assert_eq!(connections.count_for(alice, phone), 1);
		let third = connections.take_slot(alice, phone, 2).expect("fits again after a drop");
		assert!(third.is_some());

		drop(first);
		drop(third);
		drop(other);
		assert_eq!(connections.count_for(alice, phone), 0, "the table forgets an idle device");
		assert_eq!(connections.count_for(alice, desk), 0);
	}

	#[test]
	fn zero_means_unlimited_and_takes_no_slot() {
		let connections = Connections::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");

		for _ in 0..10 {
			assert!(connections.take_slot(alice, phone, 0).expect("never refused").is_none());
		}
		assert_eq!(connections.count_for(alice, phone), 0);
	}

	#[test]
	fn a_slot_knows_who_it_is_for() {
		let connections = Connections::new();
		let slot = connections
			.take_slot(user_id!("@alice:localhost"), device_id!("PHONE"), 1)
			.expect("fits")
			.expect("a slot");
		assert!(slot.is_for(user_id!("@alice:localhost"), device_id!("PHONE")));
		assert!(!slot.is_for(user_id!("@alice:localhost"), device_id!("DESK")));
		assert!(!slot.is_for(user_id!("@bob:localhost"), device_id!("PHONE")));
	}
}

#[cfg(test)]
mod address_tests {
	use std::net::IpAddr;

	use super::{Connections, to_address_group};

	fn ip(text: &str) -> IpAddr { text.parse().expect("a literal address in a test") }

	/// The whole point of grouping: an IPv6 client that can pick a fresh
	/// address out of its own /64 must not get a fresh count with it.
	#[test]
	fn ipv6_addresses_in_one_slash_64_are_one_group_and_ipv4_is_not_grouped() {
		assert_eq!(
			to_address_group(ip("2001:db8:1:2::1")),
			to_address_group(ip("2001:db8:1:2:ffff:ffff:ffff:ffff"))
		);
		assert_eq!(to_address_group(ip("2001:db8:1:2::1")), ip("2001:db8:1:2::"));

		// A neighbouring /64 is somebody else.
		assert_ne!(to_address_group(ip("2001:db8:1:2::1")), to_address_group(ip("2001:db8:1:3::1")));

		// IPv4 keeps its exact address: there is no prefix a client picks from.
		assert_eq!(to_address_group(ip("203.0.113.7")), ip("203.0.113.7"));
		assert_ne!(to_address_group(ip("203.0.113.7")), to_address_group(ip("203.0.113.8")));
	}

	/// 🚨 A dual-stack listener hands IPv4 peers over as `::ffff:a.b.c.d`, and
	/// the /64 mask clears exactly the bytes the IPv4 address lives in. Without
	/// canonicalizing first, every one of them would group as `::` — one bucket
	/// for every IPv4 client there is (PR #85 review, cirno).
	#[test]
	fn an_ipv4_peer_arriving_mapped_groups_as_itself_not_as_the_whole_world() {
		assert_eq!(to_address_group(ip("::ffff:203.0.113.7")), ip("203.0.113.7"));
		assert_eq!(
			to_address_group(ip("::ffff:203.0.113.7")),
			to_address_group(ip("203.0.113.7")),
			"the same client counts once whether the listener reports it mapped or not"
		);

		// The failure this guards against: two unrelated IPv4 clients sharing a group.
		assert_ne!(to_address_group(ip("::ffff:203.0.113.7")), to_address_group(ip("::ffff:198.51.100.9")));
		assert_ne!(to_address_group(ip("::ffff:203.0.113.7")), ip("::"));

		// `::1` is loopback, not the mapped-IPv4 range, and keeps its own group.
		assert_eq!(to_address_group(ip("::1")), ip("::"));
	}

	#[test]
	fn the_limit_counts_per_group_and_a_dropped_slot_gives_its_place_back() {
		let connections = Connections::new();
		let first = ip("2001:db8:1:2::1");
		let second = ip("2001:db8:1:2::2");

		let one = connections.take_address_slot(first, 2).expect("fits").expect("a slot");
		// A different address, the same /64: it shares the count.
		let two = connections.take_address_slot(second, 2).expect("fits").expect("a slot");
		assert_eq!(connections.count_for_address(first), 2);
		assert!(connections.take_address_slot(second, 2).is_none(), "the group is full");

		drop(two);
		assert_eq!(connections.count_for_address(first), 1);
		let _three = connections
			.take_address_slot(second, 2)
			.expect("a place came back")
			.expect("a slot");

		drop(one);
		assert_eq!(connections.count_for_address(first), 1);
	}

	#[test]
	fn another_group_has_its_own_count() {
		let connections = Connections::new();
		let _full = connections
			.take_address_slot(ip("203.0.113.7"), 1)
			.expect("fits")
			.expect("a slot");
		assert!(connections.take_address_slot(ip("203.0.113.7"), 1).is_none());
		assert!(
			connections.take_address_slot(ip("203.0.113.8"), 1).is_some(),
			"a different address is not affected by a full one"
		);
	}

	#[test]
	fn zero_means_unlimited_and_takes_no_slot() {
		let connections = Connections::new();
		let address = ip("203.0.113.7");

		for _ in 0..10 {
			assert!(
				connections
					.take_address_slot(address, 0)
					.expect("never refused")
					.is_none()
			);
		}
		assert_eq!(connections.count_for_address(address), 0);
	}
}
