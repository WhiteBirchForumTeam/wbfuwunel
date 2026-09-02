//! Merge operators for column families whose value is folded from operands.
//!
//! A write batch cannot read, so a counter kept as a plain value could not be
//! incremented inside the transaction that justifies the increment. RocksDB's
//! merge operator closes that gap: `merge(key, operand)` is a write, queued in
//! the batch like any other, and the operands are folded into the stored value
//! on read and on compaction using the function registered here.
//!
//! Only a full merge is provided. The fold's result depends on whether a base
//! value exists at all, so two operands cannot be combined ahead of time
//! without knowing that base; the partial merge therefore declines, and RocksDB
//! keeps the operands until a full merge can see the base.

use rocksdb::MergeOperands;

/// Counter value meaning "predates the counter; never count or collect it".
///
/// A row that reaches the fold with no stored value and receives an `Add`
/// becomes this, and every later `Add` leaves it unchanged. Only `Set` moves
/// it, which is how a rebuild replaces it with a real count.
pub const COUNTER_SENTINEL: i64 = i64::MIN;

/// One queued change to a counter row.
///
/// Encoded as nine bytes: one tag byte followed by the big-endian `i64`
/// argument, which `Init` carries as zero and ignores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CounterOperand {
	/// Opens the row at zero when it does not exist; leaves an existing row
	/// alone, so a later thumbnail of already counted media cannot reset it.
	Init,
	/// Adds to the count. On a row that does not exist this marks the row as
	/// predating the counter instead, and on such a row it does nothing.
	Add(i64),
	/// Replaces the count outright, whatever the row held.
	Set(i64),
}

// Tags inside an operand's own bytes. They share values with the record tags
// in `txn.rs` (`Tag::Merge` is also 0x02) but live in a different byte: the
// record tag says "this is a merge", these say which merge it is.
const TAG_INIT: u8 = 0x01;
const TAG_ADD: u8 = 0x02;
const TAG_SET: u8 = 0x03;
const OPERAND_LEN: usize = 1 + size_of::<i64>();

impl CounterOperand {
	/// Encodes the operand for `Txn::merge`.
	#[must_use]
	pub fn to_bytes(self) -> [u8; OPERAND_LEN] {
		let (tag, value) = match self {
			| Self::Init => (TAG_INIT, 0),
			| Self::Add(delta) => (TAG_ADD, delta),
			| Self::Set(value) => (TAG_SET, value),
		};

		let mut bytes = [0u8; OPERAND_LEN];
		bytes[0] = tag;
		bytes[1..].copy_from_slice(&value.to_be_bytes());
		bytes
	}

	/// Decodes an operand, or `None` for bytes of the wrong length or an
	/// unknown tag. A malformed operand is skipped by the fold rather than
	/// guessed at.
	#[must_use]
	pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
		let (tag, value) = bytes.split_first()?;
		let value = i64::from_be_bytes(value.try_into().ok()?);

		match *tag {
			| TAG_INIT => Some(Self::Init),
			| TAG_ADD => Some(Self::Add(value)),
			| TAG_SET => Some(Self::Set(value)),
			| _ => None,
		}
	}
}

/// Folds operands into a counter state.
///
/// `None` is a row that has never been written. The result is `None` only when
/// nothing was applied to a nonexistent row.
#[must_use]
pub(crate) fn fold_counter(
	state: Option<i64>,
	operands: impl IntoIterator<Item = CounterOperand>,
) -> Option<i64> {
	use CounterOperand::{Add, Init, Set};

	operands
		.into_iter()
		.fold(state, |state, operand| match (state, operand) {
			| (None, Init) => Some(0),
			| (Some(count), Init) => Some(count),
			| (None, Add(_)) => Some(COUNTER_SENTINEL),
			| (Some(COUNTER_SENTINEL), Add(_)) => Some(COUNTER_SENTINEL),
			// Saturation keeps the sentinel a floor: a count one above it that
			// loses two lands on the sentinel instead of wrapping to i64::MAX.
			| (Some(count), Add(delta)) => Some(count.saturating_add(delta)),
			| (_, Set(value)) => Some(value),
		})
}

/// Decodes a stored counter value.
///
/// Bytes of the wrong length read as no value, so a corrupt row behaves like
/// one that predates the counter rather than like a live count.
#[must_use]
pub fn decode_counter(bytes: &[u8]) -> Option<i64> {
	bytes.try_into().ok().map(i64::from_be_bytes)
}

/// Full merge registered for counter column families.
pub(super) fn counter_full_merge(
	_key: &[u8],
	existing: Option<&[u8]>,
	operands: &MergeOperands,
) -> Option<Vec<u8>> {
	let state = existing.and_then(decode_counter);
	let operands = operands.iter().filter_map(CounterOperand::from_bytes);

	fold_counter(state, operands).map(|count| count.to_be_bytes().to_vec())
}

/// Partial merge registered for counter column families: always declines.
///
/// The fold's result depends on whether a base row exists, which a partial
/// merge cannot see, so operands wait for a full merge.
pub(super) fn counter_partial_merge(
	_key: &[u8],
	_existing: Option<&[u8]>,
	_operands: &MergeOperands,
) -> Option<Vec<u8>> {
	None
}

#[cfg(test)]
mod tests {
	use super::{COUNTER_SENTINEL, CounterOperand, decode_counter, fold_counter};
	use CounterOperand::{Add, Init, Set};

	#[test]
	fn init_opens_a_missing_row_at_zero() {
		assert_eq!(fold_counter(None, [Init]), Some(0));
	}

	#[test]
	fn init_leaves_an_existing_row_alone() {
		// A thumbnail generated after the media was counted must not reset it.
		assert_eq!(fold_counter(Some(3), [Init]), Some(3));
		assert_eq!(fold_counter(Some(COUNTER_SENTINEL), [Init]), Some(COUNTER_SENTINEL));
	}

	#[test]
	fn add_on_a_missing_row_marks_it_as_predating_the_counter() {
		assert_eq!(fold_counter(None, [Add(1)]), Some(COUNTER_SENTINEL));
		assert_eq!(fold_counter(None, [Add(-1)]), Some(COUNTER_SENTINEL));
	}

	#[test]
	fn the_sentinel_swallows_every_add() {
		assert_eq!(fold_counter(Some(COUNTER_SENTINEL), [Add(1), Add(1), Add(-5)]), Some(COUNTER_SENTINEL));
	}

	#[test]
	fn counts_add_and_subtract() {
		assert_eq!(fold_counter(Some(0), [Add(1)]), Some(1));
		assert_eq!(fold_counter(Some(1), [Add(-1)]), Some(0));
		assert_eq!(fold_counter(None, [Init, Add(1), Add(1), Add(-1)]), Some(1));
	}

	#[test]
	fn set_replaces_anything_including_the_sentinel() {
		assert_eq!(fold_counter(Some(COUNTER_SENTINEL), [Set(0)]), Some(0));
		assert_eq!(fold_counter(Some(7), [Set(2)]), Some(2));
		assert_eq!(fold_counter(None, [Set(4)]), Some(4));
	}

	#[test]
	fn add_saturates_instead_of_wrapping() {
		assert_eq!(fold_counter(Some(i64::MAX), [Add(1)]), Some(i64::MAX));
		// Subtracting below the sentinel would wrap into a live count.
		assert_eq!(fold_counter(Some(COUNTER_SENTINEL + 1), [Add(-2)]), Some(COUNTER_SENTINEL));
	}

	#[test]
	fn operands_round_trip_through_bytes() {
		for operand in [Init, Add(1), Add(-1), Set(COUNTER_SENTINEL), Set(i64::MAX)] {
			assert_eq!(CounterOperand::from_bytes(&operand.to_bytes()), Some(operand));
		}
	}

	#[test]
	fn malformed_operands_are_rejected_rather_than_guessed_at() {
		assert_eq!(CounterOperand::from_bytes(&[]), None);
		assert_eq!(CounterOperand::from_bytes(&[0x02, 1, 2]), None, "short argument");
		assert_eq!(CounterOperand::from_bytes(&[0xFF, 0, 0, 0, 0, 0, 0, 0, 0]), None, "unknown tag");
	}

	#[test]
	fn a_corrupt_stored_value_reads_as_no_value() {
		assert_eq!(decode_counter(&[1, 2, 3]), None);
		assert_eq!(decode_counter(&5i64.to_be_bytes()), Some(5));
	}
}
