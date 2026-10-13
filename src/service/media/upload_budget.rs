//! The byte budget for traditional Matrix uploads.
//!
//! A traditional upload sends the whole file in one request body, which the
//! server buffers in memory before it is parsed. The per-request size limit
//! bounds one of them; this bounds all of them at once.
//!
//! See `/docs/design/media/legacy-upload-concurrency.md` §2. The chunked wbf
//! upload never holds more than one chunk and does not come through here.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// How many bytes of upload body the whole server may hold at once.
///
/// ⚠️ A permit is one byte, so the number of uploads running together is not
/// configured — it falls out of what they declare. A small upload is never
/// starved by a large one because it only ever books its own size.
pub struct UploadBudget {
	budget: Arc<Semaphore>,
	/// What the budget started at, so a request larger than the whole budget
	/// is refused instead of waiting for room that can never exist.
	capacity_bytes: usize,
}

/// Bytes booked out of the budget, given back when this is dropped.
///
/// ⚠️ 🚫 Nothing releases this by hand. The body is held for as long as the
/// handler holds the request, and a request can end in more ways than it can
/// succeed — the client disconnects, the body times out, parsing fails, the
/// handler returns an error. `Drop` is the only release that covers all of
/// them.
#[derive(Debug)]
pub struct Reservation {
	_room: Option<OwnedSemaphorePermit>,
	bytes: usize,
}

/// Why an upload did not get its bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
	/// The server is already holding as much upload body as it may. The
	/// caller answers 429 and the client retries.
	Full,
	/// This one request is larger than the whole budget, so no amount of
	/// waiting or retrying would ever make room. 🚨 Told apart from `Full`
	/// because retrying is pointless: it is a configuration the client
	/// cannot satisfy, and answering 429 would invite a retry loop.
	LargerThanBudget,
}

impl Reservation {
	/// Args:
	///     (none)
	/// Return:
	///     usize  the bytes booked; 0 when the request declared nothing to
	///     read.
	///
	/// ⭐ The caller reads the body with **this** as the limit rather than
	/// the configured maximum, so a client that under-declares its length is
	/// cut off at what it booked (design doc §2.1).
	#[inline]
	#[must_use]
	pub fn bytes(&self) -> usize { self.bytes }
}

impl UploadBudget {
	/// Args:
	///     bytes: the whole server's budget, example: 1073741824
	/// Return:
	///     UploadBudget
	#[must_use]
	pub fn new(bytes: usize) -> Self {
		let capacity_bytes = bytes.max(1);

		Self {
			budget: Arc::new(Semaphore::new(capacity_bytes)),
			capacity_bytes,
		}
	}

	/// Args:
	///     bytes: what this request may read, example: 25165824
	/// Return:
	///     Result<Reservation, BudgetError>  Err(Full) when the budget is
	///     spent, Err(LargerThanBudget) when this request alone exceeds it.
	///
	/// ⚠️ Never waits: an upload that cannot have the memory now is told so
	/// now. Waiting here would hold the connection open while it holds
	/// nothing, which is the shape this budget exists to prevent.
	pub fn reserve(&self, bytes: usize) -> Result<Reservation, BudgetError> {
		if bytes == 0 {
			return Ok(Reservation { _room: None, bytes: 0 });
		}

		if bytes > self.capacity_bytes {
			return Err(BudgetError::LargerThanBudget);
		}

		// The budget is capped at the capacity above, and a semaphore counts
		// permits in u32: a budget larger than u32 cannot be booked in one go.
		let permits = u32::try_from(bytes).map_err(|_| BudgetError::LargerThanBudget)?;

		match Arc::clone(&self.budget).try_acquire_many_owned(permits) {
			| Ok(room) => Ok(Reservation { _room: Some(room), bytes }),
			// ⚠️ `Closed` cannot happen — nothing ever closes this budget, it
			// lives as long as the process — but it is the same answer either
			// way: no bytes, so no upload. 🚫 Not `unreachable!`; a refusal is
			// a result, a panic would take the connection with it.
			| Err(TryAcquireError::NoPermits | TryAcquireError::Closed) =>
				Err(BudgetError::Full),
		}
	}

	/// Args:
	///     (none)
	/// Return:
	///     usize  the configured budget, for the message that says what was
	///     too large.
	#[inline]
	#[must_use]
	pub fn capacity_bytes(&self) -> usize { self.capacity_bytes }

	/// Args:
	///     (none)
	/// Return:
	///     usize  bytes not booked right now. For diagnostics only: it is
	///     stale the moment it is read, so 🚫 never decide anything with it
	///     (that is what `reserve` is for, and why it is atomic).
	#[inline]
	#[must_use]
	pub fn available_bytes(&self) -> usize { self.budget.available_permits() }
}

#[cfg(test)]
mod tests {
	use super::{BudgetError, UploadBudget};

	#[test]
	fn a_reservation_takes_its_bytes_out_of_the_budget() {
		let budget = UploadBudget::new(100);
		let room = budget.reserve(40).expect("40 of 100 fits");

		assert_eq!(room.bytes(), 40);
		assert_eq!(budget.available_bytes(), 60);
	}

	#[test]
	fn a_full_budget_refuses_rather_than_waits() {
		let budget = UploadBudget::new(100);
		let _first = budget.reserve(80).expect("80 of 100 fits");

		assert_eq!(budget.reserve(40).err(), Some(BudgetError::Full));
	}

	// 🚨 This is the one the whole feature rests on: every way a request can
	// end has to give the bytes back, and `Drop` is what makes that true.
	#[test]
	fn dropping_a_reservation_gives_the_bytes_back() {
		let budget = UploadBudget::new(100);
		let first = budget.reserve(80).expect("80 of 100 fits");
		assert!(budget.reserve(40).is_err(), "the budget is spent while the first one lives");

		// ⭐ Named rather than left to the end of a scope: the release is the
		// behaviour under test, so it should be a line you can point at.
		drop(first);

		assert_eq!(budget.available_bytes(), 100);
		assert!(budget.reserve(40).is_ok(), "and it is spendable again afterwards");
	}

	#[test]
	fn a_request_larger_than_the_whole_budget_is_told_so_instead_of_being_asked_to_retry() {
		let budget = UploadBudget::new(100);

		assert_eq!(budget.reserve(101).err(), Some(BudgetError::LargerThanBudget));
	}

	#[test]
	fn a_request_that_declares_nothing_books_nothing() {
		let budget = UploadBudget::new(100);
		let room = budget.reserve(0).expect("nothing to read is not a refusal");

		assert_eq!(room.bytes(), 0);
		assert_eq!(budget.available_bytes(), 100, "an empty body does not spend the budget");
	}

	#[test]
	fn the_budget_is_never_zero_so_a_misconfiguration_cannot_refuse_everything() {
		let budget = UploadBudget::new(0);

		assert_eq!(budget.capacity_bytes(), 1);
	}
}
