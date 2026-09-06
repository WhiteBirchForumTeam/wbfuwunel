//! Tasks that outlive the request that started them, and so must be joined
//! before `Services` is dropped.
//!
//! A request handler borrows `Services` through the router's `State`, a raw
//! pointer whose safety argument is "every request has finished before
//! `Services` goes". A WebSocket breaks that: after the `101` the request is
//! over, but the task serving the socket keeps running. Such tasks are
//! spawned here so that `Services::stop` can wait for them; a task that would
//! start while shutdown is already under way is refused instead.

use std::{sync::Mutex, time::Duration};

use tokio::task::JoinSet;
use tuwunel_core::{error, info, warn};

/// How long `close_and_join` waits for connections to end on their own
/// before aborting the rest. Each connection's loop returns as soon as it
/// sees the server stopping, so this only ever runs out when a task is stuck
/// inside one call that never returns; then it must not hold the whole
/// shutdown hostage (review of PR #28, rumia).
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Connections {
	/// `None` once `close_and_join` has begun: nothing may start after that.
	tasks: Mutex<Option<JoinSet<()>>>,
}

impl Default for Connections {
	fn default() -> Self { Self::new() }
}

impl Connections {
	#[must_use]
	pub fn new() -> Self { Self { tasks: Mutex::new(Some(JoinSet::new())) } }

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
