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
//! is one connection's place in its device's count (/docs/design/wire/pack-pipeline.md §2.1) and
//! an `AddressSlot`
//! is its place in its source address's count (§2.2). Both are taken before
//! the connection is accepted and given back when the slot is dropped,
//! whichever way the connection ended (`/docs/design/wire/pack-pipeline.md`).
//!
//! ⭐ The two answer different questions and neither replaces the other: the
//! device count needs an identity, so it cannot see a connection that has not
//! logged in; the address count is the only one that reaches those.

use std::{
	collections::HashMap,
	net::IpAddr,
	sync::{Arc, Mutex, MutexGuard, Weak},
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

/// The tracked connection tasks. Shared because each task reaches back into it
/// as its last act, to take the finished ones out — see `spawn`.
type TaskSet = Arc<Mutex<Option<JoinSet<()>>>>;

/// What a task holds to reach the set it is itself in.
///
/// 🚨 **`Weak`, never `Arc`.** The set owns the task's future and the future would
/// own the set back — a cycle, so the allocation could never be freed, and, the
/// part that actually bites, **`JoinSet`'s own `Drop` (which aborts everything
/// still running) could never fire while any task was alive**. That abort is the
/// mechanical half of this module's safety argument: a handler borrows `Services`
/// through a raw pointer, so a task still being polled after `Services` is gone
/// is a use-after-free. Dropping `Connections` has to be able to stop these
/// tasks, and it cannot if the thing that stops them is held by the tasks
/// themselves (cirno, PR #101).
type WeakTaskSet = Weak<Mutex<Option<JoinSet<()>>>>;

pub struct Connections {
	/// `None` once `close_and_join` has begun: nothing may start after that.
	tasks: TaskSet,
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
/// the whole server (PR #85 review, cirno).
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

/// The task set's lock, with a poisoned lock treated as usable.
///
/// 🚨 A poisoned lock must not take the process down: refusing every new
/// connection for the rest of the server's life is worse than the panic that
/// poisoned it (CLAUDE.md P). ⚠️ Nothing may be awaited while this is held —— the
/// guard is not `Send`, and `close_and_join` depends on the lock being free
/// while it waits.
///
/// Args:
///     tasks: the shared task set
/// Return:
///     MutexGuard  the set, or `None` inside it once shutdown took it
fn lock_tasks(tasks: &TaskSet) -> MutexGuard<'_, Option<JoinSet<()>>> {
	match tasks.lock() {
		| Ok(guard) => guard,
		| Err(poisoned) => poisoned.into_inner(),
	}
}

/// Takes the finished tasks out of the set without waiting for anything.
///
/// Args:
///     set: the tracked connection tasks
fn reap_finished(set: &mut JoinSet<()>) {
	while let Some(joined) = set.try_join_next() {
		log_if_abnormal(joined);
	}
}

/// One place for this message, so reaping a finished task reports a panic the
/// same way shutdown does — 🚨 a reap that swallowed it would turn a connection
/// task's panic into silence, and this is the only thing that speaks for it.
///
/// Args:
///     joined: what the `JoinSet` handed back for one finished task
fn log_if_abnormal(joined: Result<(), tokio::task::JoinError>) {
	if let Err(e) = joined {
		error!(?e, "A connection task ended abnormally.");
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
		// Same reasoning as `AddressSlot::drop`: a panic elsewhere must not cost
		// this device a place for the rest of the server's life (CLAUDE.md P).
		let mut table = match self.table.lock() {
			| Ok(table) => table,
			| Err(poisoned) => poisoned.into_inner(),
		};
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
			tasks: Arc::new(Mutex::new(Some(JoinSet::new()))),
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
		let mut table = match self.slots.lock() {
			| Ok(table) => table,
			| Err(poisoned) => poisoned.into_inner(),
		};
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
	/// cannot see those at all (/docs/design/wire/pack-pipeline.md §2.2).
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
		match self.slots.lock() {
			| Ok(table) => table,
			| Err(poisoned) => poisoned.into_inner(),
		}
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
		let mut tasks = lock_tasks(&self.tasks);
		match tasks.as_mut() {
			| Some(set) => {
				// 🚨 A `JoinSet` keeps an entry for every task it ever started until
				// someone joins it, and the only join used to be at shutdown — so a
				// server that had served a million connections carried a million
				// entries for its whole life. The entry is small, but `len()` is the
				// number shutdown logs and measures against its timeout, so it was
				// also reporting a million open connections when three were open
				// (external review 2026-09-29 #7).
				//
				// Reaped in two places, and the second is what bounds it:
				//   - here, so a burst of new connections clears the last burst;
				//   - at the end of every task (below), so connections closing
				//     clear each other even while nothing new arrives.
				// ⚠️ A task cannot take out *its own* entry — it is still running, so
				// `try_join_next` will not return it. One stale entry can therefore
				// remain, belonging to whichever task finished last. Getting to zero
				// would mean not using `JoinSet`, and `JoinSet` is what makes
				// shutdown able to wait for the connections at all.
				reap_finished(set);

				let tasks: WeakTaskSet = Arc::downgrade(&self.tasks);
				set.spawn(async move {
					task.await;

					// The task's own last act. Its entry stays until someone else
					// comes along; everything that finished before it goes now.
					// Gone means `Connections` was dropped while this ran — there is
					// no set left to tidy, and nothing to say about it.
					let Some(tasks) = tasks.upgrade() else {
						return;
					};
					if let Some(set) = lock_tasks(&tasks).as_mut() {
						reap_finished(set);
					}
				});
				true
			},
			| None => false,
		}
	}

	/// Return:
	///     usize  how many connection tasks are tracked right now —— live ones
	///     plus any that finished since the last `spawn`; 0 once shutdown has
	///     taken the set.
	#[must_use]
	pub fn count_tracked_tasks(&self) -> usize {
		lock_tasks(&self.tasks)
			.as_ref()
			.map_or(0, JoinSet::len)
	}

	/// Refuses new connections from now on and waits for the running ones to
	/// end. Each connection's loop ends on its own when it sees the server
	/// stopping, so this normally returns once they have all noticed; one that
	/// has not after `JOIN_TIMEOUT` is aborted, which drops its future and
	/// with it every borrow of `Services`.
	pub async fn close_and_join(&self) {
		// The guard is taken in its own scope: it is not `Send`, and what
		// follows is awaited. Poisoned here would mean shutdown never waits for
		// the open connections — the one path where panicking helps least
		// (CLAUDE.md P).
		let taken = lock_tasks(&self.tasks).take();
		let Some(mut set) = taken else {
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
				log_if_abnormal(joined);
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

#[cfg(test)]
mod task_tracking_tests {
	use super::*;

	/// 🚨 The leak this guards: a `JoinSet` keeps an entry for every task it ever
	/// started until someone joins it, and the only join is at shutdown — so
	/// without reaping, the count here would be one per connection the server has
	/// ever served, for its whole life.
	///
	/// ⭐ The promise is **at most one stale entry**, and it does not depend on new
	/// connections arriving: every task reaps as its last act, so the ones closing
	/// clear each other. The one that can remain belongs to whichever task
	/// finished last — it was still running when it looked, so it could not take
	/// out its own entry.
	#[tokio::test]
	async fn tasks_that_finished_stop_being_tracked_without_a_new_connection() {
		let connections = Connections::new();

		// Started together, so nothing after this is a `spawn` that could reap.
		for _ in 0..8 {
			assert!(connections.spawn(async {}), "the set is open");
		}

		// Let them all run to their end, where each one reaps.
		for _ in 0..8 {
			tokio::task::yield_now().await;
		}

		let tracked = connections.count_tracked_tasks();

		assert!(
			tracked <= 1,
			"eight finished tasks left {tracked} entries; at most the last one should remain",
		);
	}

	/// 🚨 Why the reap in `spawn` is not redundant with the one at the end of a
	/// task: **a task that panics never reaches its own last step.** It leaves its
	/// entry and it does not clear anyone else's, so a run of panicking
	/// connections would pile up with nothing to collect them — until the next
	/// connection arrives and `spawn` does.
	#[tokio::test]
	async fn a_new_connection_clears_tasks_that_panicked_and_so_never_reaped() {
		let connections = Connections::new();

		// Started together: a `spawn` in between would reap, which is the very
		// thing being shown to be necessary.
		for _ in 0..8 {
			assert!(connections.spawn(async { panic!("a connection task that fails") }));
		}
		for _ in 0..8 {
			tokio::task::yield_now().await;
		}

		let piled_up = connections.count_tracked_tasks();

		assert_eq!(piled_up, 8, "a panicking task cannot reap: all eight entries are still here");

		// The next connection is what clears them.
		assert!(connections.spawn(async {}));

		assert_eq!(
			connections.count_tracked_tasks(),
			1,
			"only the connection just started should be tracked",
		);
	}

	/// ⚠️ The other direction, so the reap cannot be "join everything": a task
	/// still running must stay tracked, or shutdown would not wait for it.
	#[tokio::test]
	async fn a_task_still_running_stays_tracked() {
		let connections = Connections::new();
		let (release, held) = tokio::sync::oneshot::channel::<()>();

		assert!(connections.spawn(async move {
			let _waited = held.await;
		}));
		assert!(connections.spawn(async {}));
		tokio::task::yield_now().await;
		assert!(connections.spawn(async {}));

		assert_eq!(
			connections.count_tracked_tasks(),
			2,
			"the one waiting on the channel and the one just spawned",
		);

		let _released = release.send(());
	}

	/// 🚨 **Dropping `Connections` must still abort the tasks still running.**
	///
	/// That is `JoinSet`'s own `Drop`, and it is the mechanical half of this
	/// module's safety argument — a handler borrows `Services` through a raw
	/// pointer, so a task polled after `Services` is gone is a use-after-free.
	/// ⚠️ Holding the set by `Arc` from inside a task silently removes it: the set
	/// owns the future, the future owns the set, and nothing can ever be dropped
	/// (cirno, PR #101). The task reaches back with a `Weak` for exactly this.
	#[tokio::test]
	async fn dropping_connections_aborts_what_was_still_running() {
		let ran_past_the_await = Arc::new(Mutex::new(false));
		let flag = ran_past_the_await.clone();
		let (release, held) = tokio::sync::oneshot::channel::<()>();

		let connections = Connections::new();
		assert!(connections.spawn(async move {
			let _waited = held.await;
			if let Ok(mut flag) = flag.lock() {
				*flag = true;
			}
		}));
		tokio::task::yield_now().await;

		drop(connections);

		// ⚠️ An abort is a request, not an event: the task is dropped the next time
		// the runtime looks at it. Asserting straight after the drop would be
		// asserting that cancellation is synchronous, which it is not.
		for _ in 0..4 {
			tokio::task::yield_now().await;
		}

		// Its future is gone and with it the receiver — a send that finds nobody
		// home is the proof, and it does not depend on the task running anything.
		assert!(release.send(()).is_err(), "the task should have been aborted with the set");

		tokio::task::yield_now().await;

		assert_eq!(
			*ran_past_the_await
				.lock()
				.expect("a test-local lock"),
			false,
			"the task kept running after `Connections` was dropped",
		);
	}

	/// And the count says 0 once shutdown has taken the set, rather than panicking
	/// or reporting the tasks it already waited for.
	#[tokio::test]
	async fn the_count_is_zero_after_shutdown_took_the_set() {
		let connections = Connections::new();

		assert!(connections.spawn(async {}));
		connections.close_and_join().await;

		assert_eq!(connections.count_tracked_tasks(), 0);
		assert!(!connections.spawn(async {}), "and nothing new is started");
	}
}
