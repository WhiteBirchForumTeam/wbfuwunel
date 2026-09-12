//! Drafts (`0x02 Stream`): the throttles a draft's pieces pass through
//! (`docs/design/streaming-messages.md` §7).
//!
//! Everything else about a draft is either persistent (the anchoring event,
//! read from the timeline) or momentary (a piece, broadcast to the room's
//! subscribers and forgotten). ⭐ **The server keeps no draft state** — not
//! the text, not who is writing, not how far along it is; the truth is the
//! anchor, and every piece is checked against it afresh. So what lives here
//! is only what a rate limit cannot do without: how recently each device
//! sent something.
//!
//! Counted by **device**, not by address: several of a user's devices, and
//! several users, share an address routinely, and one bot writing quickly
//! should not throttle the person sitting next to it.

use std::time::Duration;

use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use tuwunel_core::utils::rate_limit::TokenBuckets;

/// Which device is sending, for the throttles to count by.
type Writer = (OwnedUserId, OwnedDeviceId);

pub struct Drafts {
	/// `Keypoint`, `Delta` and `Append`: frequent and small.
	pieces: TokenBuckets<Writer>,
	/// `Demand`: rare, and its cost is paid by the author's connection
	/// rather than the asker's, so it is throttled far harder.
	demands: TokenBuckets<Writer>,
}

impl Default for Drafts {
	fn default() -> Self { Self::new() }
}

impl Drafts {
	#[must_use]
	pub fn new() -> Self {
		Self { pieces: TokenBuckets::new(), demands: TokenBuckets::new() }
	}

	/// Args:
	///     user: who is writing, example: @alice:localhost
	///     device: which of their devices, example: PHONE
	///     per_second: `wbf_draft_pieces_per_second`, example: 30.0; 0 is off
	///     burst: `wbf_draft_pieces_burst`, example: 60.0
	/// Return:
	///     Result<(), Duration>  Ok when the piece may go out (and when the
	///     throttle is off); Err with how long until the next one may.
	pub fn take_piece(
		&self,
		user: &UserId,
		device: &DeviceId,
		per_second: f64,
		burst: f64,
	) -> Result<(), Duration> {
		self.pieces
			.take((user.to_owned(), device.to_owned()), per_second, burst)
	}

	/// Args:
	///     user: who is asking, example: @bob:localhost
	///     device: which of their devices, example: DESK
	///     per_second: `wbf_draft_demands_per_second`, example: 1.0; 0 is off
	///     burst: `wbf_draft_demands_burst`, example: 3.0
	/// Return:
	///     Result<(), Duration>  Ok when the demand may go out (and when the
	///     throttle is off); Err with how long until the next one may.
	pub fn take_demand(
		&self,
		user: &UserId,
		device: &DeviceId,
		per_second: f64,
		burst: f64,
	) -> Result<(), Duration> {
		self.demands
			.take((user.to_owned(), device.to_owned()), per_second, burst)
	}
}

#[cfg(test)]
mod tests {
	use ruma::{device_id, user_id};

	use super::Drafts;

	#[test]
	fn one_devices_pieces_do_not_spend_anothers_allowance() {
		// The reason this is keyed by device and not by address: two devices
		// of one user, and two users behind one NAT, are the ordinary case.
		let drafts = Drafts::new();
		let alice = user_id!("@alice:localhost");

		assert!(drafts.take_piece(alice, device_id!("PHONE"), 1.0, 2.0).is_ok());
		assert!(drafts.take_piece(alice, device_id!("PHONE"), 1.0, 2.0).is_ok());
		assert!(
			drafts
				.take_piece(alice, device_id!("PHONE"), 1.0, 2.0)
				.is_err(),
			"that device has spent its burst"
		);
		assert!(
			drafts.take_piece(alice, device_id!("DESK"), 1.0, 2.0).is_ok(),
			"and the other one has not"
		);
	}

	#[test]
	fn the_two_throttles_are_separate_tables() {
		// A demand must not be refused because the same device is writing
		// quickly: they are throttled for different reasons and at rates two
		// orders of magnitude apart.
		let drafts = Drafts::new();
		let alice = user_id!("@alice:localhost");
		let phone = device_id!("PHONE");

		assert!(drafts.take_piece(alice, phone, 1.0, 1.0).is_ok());
		assert!(drafts.take_piece(alice, phone, 1.0, 1.0).is_err());

		assert!(drafts.take_demand(alice, phone, 1.0, 1.0).is_ok());
	}

	#[test]
	fn a_rate_of_zero_is_no_throttle_at_all() {
		let drafts = Drafts::new();
		let alice = user_id!("@alice:localhost");

		for _ in 0..1000 {
			assert!(drafts.take_piece(alice, device_id!("PHONE"), 0.0, 0.0).is_ok());
		}
	}
}
