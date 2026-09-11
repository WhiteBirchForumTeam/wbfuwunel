use futures::StreamExt;
use ruma::OwnedDeviceId;
use tuwunel_core::Result;

use crate::{admin_command, utils::parse_local_user_id};

/// What one device's queue holds.
struct Queue {
	device: OwnedDeviceId,
	items: usize,
	oldest: Option<u64>,
	newest: Option<u64>,
}

/// Prints how much to-device each of a user's devices is holding.
///
/// A to-device item lives until its device destroys it (`Device/ItemsDestroy`,
/// `docs/design/wbf-to-device.md` §6): there is no time limit, because
/// dropping key material silently costs more than the disk does. The price of
/// that decision is a queue nobody is ever coming back for — a phone that was
/// lost and never logged out — and this is how anyone sees one. Without it,
/// "kept forever" would be a promise nobody can check.
///
/// 📎 The positions are counts from the server's sequence, not timestamps:
/// this counter is monotonic and carries no clock, so a lower `oldest` means
/// "queued earlier", and nothing here says when.
#[admin_command]
pub(super) async fn to_device_queue(&self, user_id: String) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;

	let devices: Vec<OwnedDeviceId> = self
		.services
		.users
		.all_device_ids(&user_id)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut queues = Vec::with_capacity(devices.len());
	for device in devices {
		let counts: Vec<u64> = self
			.services
			.users
			.get_to_device_events(&user_id, &device, None, None)
			.map(|(count, _)| count)
			.collect()
			.await;

		queues.push(Queue {
			items: counts.len(),
			oldest: counts.first().copied(),
			newest: counts.last().copied(),
			device,
		});
	}

	let total: usize = queues.iter().map(|queue| queue.items).sum();
	write!(
		self,
		"To-device queues for {user_id} ({total} item(s) over {} device(s)):\n```\n",
		queues.len()
	)
	.await?;
	for queue in &queues {
		let held = match (queue.oldest, queue.newest) {
			| (Some(oldest), Some(newest)) => format!("oldest {oldest}, newest {newest}"),
			| _ => "empty".to_owned(),
		};
		writeln!(self, "{}\titems: {}\t{held}", queue.device, queue.items).await?;
	}
	write!(self, "```").await
}
