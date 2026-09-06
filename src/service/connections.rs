//! Tasks that outlive the request that started them, and so must be joined
//! before `Services` is dropped.
//!
//! A request handler borrows `Services` through the router's `State`, a raw
//! pointer whose safety argument is "every request has finished before
//! `Services` goes". A WebSocket breaks that: after the `101` the request is
//! over, but the task serving the socket keeps running. Such tasks are
//! spawned here so that `Services::stop` can wait for them; a task that would
//! start while shutdown is already under way is refused instead.

use std::sync::Mutex;

use tokio::task::JoinSet;
use tuwunel_core::{error, info};

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
	/// stopping, so this returns once they have all noticed.
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
		while let Some(joined) = set.join_next().await {
			if let Err(e) = joined {
				error!(?e, "A connection task ended abnormally.");
			}
		}
		if open > 0 {
			info!(open, "Long-lived connections ended.");
		}
	}
}
