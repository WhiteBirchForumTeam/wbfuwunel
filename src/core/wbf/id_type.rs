//! What the header's `id` is (`docs/design/wbf-wire-format.md` §2.2): its
//! first byte says which kind of identifier the other seven carry.
//!
//! ⭐ Before this, one field held three unrelated things — a conversation
//! number the client chose, an upload id the server minted at random, and an
//! event's position in a room — and which one it was could only be inferred
//! from the pack's `kind`. So a client keeping one table of its conversations
//! could have two different things land in the same slot, and a server
//! answering a mismatched id could only say "not found" rather than "that is
//! an upload id, in the field for a subscription".
//!
//! Adding a type is two edits in this order, the same rule the error
//! vocabulary has: the row in §2.2 first, then the variant here.

/// The first byte of the header's `id`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdType {
	/// 0x00: this pack has no conversation. ⚠️ Then the **whole** id is 0,
	/// not just the type byte — a value under a `None` type would be a
	/// number nothing can resolve.
	None,
	/// 0x01: a conversation number the client picked.
	///
	/// ⚠️ The server checks the type and **not** whether the number is free:
	/// it cannot see the client's table. Collisions across types are the
	/// protocol's problem, collisions within this one belong to whoever
	/// minted them (§2.2).
	ClientConversation,
	/// 0x02: an event's position in its room (`g_seq`), which is how a draft
	/// names its anchor.
	EventPosition,
	/// 0x03: an upload. ⚠️ Its value, in hex, **is** the mxc's media id —
	/// there is no table mapping one to the other.
	Upload,
}

/// How many bits of the `id` are left for the value once the type has its
/// byte. Everything that goes in one has to fit, and 2⁵⁶ is 7.2 × 10¹⁶.
pub const ID_VALUE_BITS: u32 = 56;

/// The largest value an `id` can carry.
pub const ID_VALUE_MAX: u64 = (1 << ID_VALUE_BITS) - 1;

/// Why a value cannot be made into an `id` of the type asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdValueRefused {
	/// More than the seven bytes an `id` leaves under the type.
	///
	/// ⚠️ Returned rather than truncated: a silently shortened id names a
	/// different thing, and the caller would not know which.
	TooLargeForSevenBytes(u64),
	/// A value under the `None` type, whose whole id must be 0 (§2.2).
	///
	/// ⚠️ The dispatcher refuses such an id on arrival, so composing one
	/// could only ever produce a pack the next server rejects. It fails here
	/// instead, where the caller still knows what it meant.
	NoneTakesNoValue(u64),
}

impl IdType {
	/// Every type, so a test can hold the whole table at once.
	pub const ALL: [Self; 4] = [
		Self::None,
		Self::ClientConversation,
		Self::EventPosition,
		Self::Upload,
	];

	/// Return:
	///     u8  the byte on the wire, example: 0x02.
	#[must_use]
	pub const fn byte(self) -> u8 {
		match self {
			| Self::None => 0x00,
			| Self::ClientConversation => 0x01,
			| Self::EventPosition => 0x02,
			| Self::Upload => 0x03,
		}
	}

	/// Return:
	///     &'static str  for error messages and logs, example: "upload".
	#[must_use]
	pub const fn name(self) -> &'static str {
		match self {
			| Self::None => "none",
			| Self::ClientConversation => "a conversation the client named",
			| Self::EventPosition => "an event position",
			| Self::Upload => "an upload",
		}
	}

	/// The type a whole `id` declares.
	///
	/// Args:
	///     id: the header field, example: 0x0200_0000_0000_007B
	/// Return:
	///     Option<IdType>  None for a byte this table does not define —
	///     including the reserved range and the extension `0xFF`, which
	///     nothing implements yet. ⚠️ Unknown means refuse, never guess:
	///     the whole point of the byte is that an id says what it is.
	#[must_use]
	pub const fn of(id: u64) -> Option<Self> {
		match (id >> ID_VALUE_BITS) as u8 {
			| 0x00 => Some(Self::None),
			| 0x01 => Some(Self::ClientConversation),
			| 0x02 => Some(Self::EventPosition),
			| 0x03 => Some(Self::Upload),
			| _ => None,
		}
	}

	/// Builds the wire `id` for a value of this type.
	///
	/// Args:
	///     value: example: 123 (a `g_seq`); under `None`, 0 is the only one
	/// Return:
	///     Result<u64, IdValueRefused>  the composed id;
	///     `TooLargeForSevenBytes` when the value needs more than 56 bits;
	///     `NoneTakesNoValue` when the type is `None` and the value is not 0.
	pub const fn compose(self, value: u64) -> Result<u64, IdValueRefused> {
		if value > ID_VALUE_MAX {
			return Err(IdValueRefused::TooLargeForSevenBytes(value));
		}
		// ⚠️ `None` does not mean "no type byte", it means the whole id is 0
		// — which is what the dispatcher enforces on arrival. Composing a
		// value under it would hand back an id every server refuses, so this
		// refuses at the only point that still knows what was meant.
		if matches!(self, Self::None) && value != 0 {
			return Err(IdValueRefused::NoneTakesNoValue(value));
		}
		Ok(((self.byte() as u64) << ID_VALUE_BITS) | value)
	}
}

/// The seven bytes under the type.
///
/// Args:
///     id: the header field, example: 0x0200_0000_0000_007B
/// Return:
///     u64  example: 123
#[must_use]
pub const fn id_value(id: u64) -> u64 { id & ID_VALUE_MAX }

#[cfg(test)]
mod tests {
	use super::{ID_VALUE_MAX, IdType, IdValueRefused, id_value};

	#[test]
	fn every_type_has_its_own_byte_and_its_own_name() {
		let mut bytes: Vec<u8> = IdType::ALL.iter().map(|kind| kind.byte()).collect();
		let mut names: Vec<&str> = IdType::ALL.iter().map(|kind| kind.name()).collect();
		let total = IdType::ALL.len();

		bytes.sort_unstable();
		bytes.dedup();
		names.sort_unstable();
		names.dedup();

		assert_eq!(bytes.len(), total, "two types share a byte");
		assert_eq!(names.len(), total, "two types share a name");
	}

	#[test]
	fn a_composed_id_reads_back_as_what_went_in() {
		// ⚠️ `None` is not in this loop on purpose: it is the one type that
		// carries no value, so "reads back as what went in" is not a property
		// it has. `none_is_a_plain_zero_and_takes_nothing_else` is its test.
		for kind in IdType::ALL.into_iter().filter(|kind| *kind != IdType::None) {
			let id = kind.compose(123).expect("123 fits");
			assert_eq!(IdType::of(id), Some(kind));
			assert_eq!(id_value(id), 123);
		}
	}

	#[test]
	fn none_is_a_plain_zero_and_takes_nothing_else() {
		// Zero is the one id every pack without a conversation sends, and the
		// one the error path reads out of a frame that did not decode.
		assert_eq!(IdType::None.compose(0), Ok(0));
		assert_eq!(IdType::of(0), Some(IdType::None));

		// 🚨 A value under `None` is the id the dispatcher refuses on
		// arrival, so this constructor must not be able to build one: an id
		// no server accepts is worse coming out of the type table than out of
		// a client, because the table is what everything else trusts
		// (PR #47 review, cirno).
		assert_eq!(IdType::None.compose(1), Err(IdValueRefused::NoneTakesNoValue(1)));
		assert_eq!(
			IdType::None.compose(ID_VALUE_MAX),
			Err(IdValueRefused::NoneTakesNoValue(ID_VALUE_MAX))
		);
	}

	#[test]
	fn a_value_too_big_for_seven_bytes_is_refused_not_cut() {
		// ⚠️ Truncating would hand back an id that names something else,
		// and nothing downstream could tell.
		assert_eq!(IdType::Upload.compose(ID_VALUE_MAX), Ok(0x03FF_FFFF_FFFF_FFFF));
		assert_eq!(
			IdType::Upload.compose(ID_VALUE_MAX + 1),
			Err(IdValueRefused::TooLargeForSevenBytes(ID_VALUE_MAX + 1))
		);
	}

	#[test]
	fn a_byte_this_table_does_not_define_is_not_guessed() {
		// The reserved range and the extension byte: refuse, so that adding
		// a type later cannot be mistaken for something already understood.
		for byte in [0x04u8, 0x7f, 0xF0, 0xFF] {
			let id = (u64::from(byte) << 56) | 9;
			assert_eq!(IdType::of(id), None, "0x{byte:02x} must not resolve");
		}
	}
}
