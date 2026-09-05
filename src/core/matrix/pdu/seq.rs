//! The per-room sequence number a stored PDU carries in its `unsigned`.
//!
//! Every event that enters a room's timeline gets one: forward events count
//! 1, 2, 3… in arrival order; history backfilled from before the room's
//! first known event counts 0, −1, −2… (see `docs/design/room-seq-and-recent.md`).
//! The number lives in the stored JSON under [`SEQ_KEY`], so every path that
//! serves the stored event carries it without knowing about it. The two
//! things that rewrite that JSON, redaction and the federation wire format,
//! are the only readers here.

use ruma::{CanonicalJsonObject, CanonicalJsonValue};

use crate::{Result, err};

/// The `unsigned` key. Namespaced to this fork; other servers and clients
/// ignore keys they do not know.
pub const SEQ_KEY: &str = "org.wbftw.wbfuwunel.seq";

/// A room's counters, stored as one 16-byte value under the room id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeqBounds {
	/// The seq of the newest forward event; 0 when the room has none yet.
	pub last_forward: i64,
	/// The seq the next backfilled event receives; starts at 0 and goes down.
	pub next_backfilled: i64,
}

impl SeqBounds {
	/// Two big-endian i64: `last_forward` then `next_backfilled`.
	pub const ENCODED_LEN: usize = 16;

	/// Assigns the next forward seq and advances the counter.
	pub fn take_forward(&mut self) -> i64 {
		self.last_forward = self.last_forward.saturating_add(1);
		self.last_forward
	}

	/// Assigns the next backfilled seq and advances the counter downward.
	pub fn take_backfilled(&mut self) -> i64 {
		let seq = self.next_backfilled;
		self.next_backfilled = seq.saturating_sub(1);
		seq
	}

	/// The stored value for `roomid_seqbounds`.
	#[must_use]
	pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
		let mut bytes = [0_u8; Self::ENCODED_LEN];
		bytes[..8].copy_from_slice(&self.last_forward.to_be_bytes());
		bytes[8..].copy_from_slice(&self.next_backfilled.to_be_bytes());
		bytes
	}

	/// Args:
	///     bytes: the stored value, example: the 16 bytes `encode` produced
	/// Return:
	///     Result<SeqBounds>  Err when the length is not 16.
	pub fn decode(bytes: &[u8]) -> Result<Self> {
		let bytes: &[u8; Self::ENCODED_LEN] = bytes
			.try_into()
			.map_err(|_| err!(Database("seq bounds must be {} bytes, got {}", Self::ENCODED_LEN, bytes.len())))?;

		Ok(Self {
			last_forward: i64::from_be_bytes(bytes[..8].try_into().expect("8 bytes")),
			next_backfilled: i64::from_be_bytes(bytes[8..].try_into().expect("8 bytes")),
		})
	}
}

/// Args:
///     json: a stored PDU object, example: the value of `pduid_pdu`
/// Return:
///     Option<i64>  None when the event carries no seq (an outlier, or a
///     database from before this fork numbered its rooms).
#[must_use]
pub fn get_json_seq(json: &CanonicalJsonObject) -> Option<i64> {
	match json.get("unsigned")? {
		| CanonicalJsonValue::Object(unsigned) => match unsigned.get(SEQ_KEY)? {
			| CanonicalJsonValue::Integer(seq) => Some(i64::from(*seq)),
			| _ => None,
		},
		| _ => None,
	}
}

/// Writes `seq` into `json["unsigned"]`, creating the object when absent and
/// replacing a non-object `unsigned` outright.
pub fn set_json_seq(json: &mut CanonicalJsonObject, seq: i64) {
	let unsigned = json
		.entry("unsigned".into())
		.or_insert_with(|| CanonicalJsonValue::Object(CanonicalJsonObject::new()));

	if !matches!(unsigned, CanonicalJsonValue::Object(_)) {
		*unsigned = CanonicalJsonValue::Object(CanonicalJsonObject::new());
	}

	if let CanonicalJsonValue::Object(unsigned) = unsigned {
		// Canonical JSON integers are bounded to ±2^53; a seq will never get
		// there, and clamping is the honest fallback if one somehow did.
		let seq = ruma::Int::new_saturating(seq);
		unsigned.insert(SEQ_KEY.into(), CanonicalJsonValue::Integer(seq));
	}
}

/// Removes the seq from `json["unsigned"]`, for copies that leave this server.
pub fn remove_json_seq(json: &mut CanonicalJsonObject) {
	if let Some(CanonicalJsonValue::Object(unsigned)) = json.get_mut("unsigned") {
		unsigned.remove(SEQ_KEY);
	}
}

#[cfg(test)]
mod tests {
	use ruma::CanonicalJsonValue;

	use super::*;

	#[test]
	fn forward_counts_from_one_and_backfilled_from_zero_downward() {
		let mut bounds = SeqBounds::default();
		assert_eq!(bounds.take_forward(), 1);
		assert_eq!(bounds.take_forward(), 2);
		assert_eq!(bounds.take_backfilled(), 0);
		assert_eq!(bounds.take_backfilled(), -1);
		assert_eq!(bounds, SeqBounds { last_forward: 2, next_backfilled: -2 });
	}

	#[test]
	fn bounds_round_trip_through_sixteen_bytes() {
		let bounds = SeqBounds { last_forward: 12345, next_backfilled: -7 };
		let bytes = bounds.encode();
		assert_eq!(bytes.len(), SeqBounds::ENCODED_LEN);
		assert_eq!(SeqBounds::decode(&bytes).unwrap(), bounds);
		assert!(SeqBounds::decode(&bytes[..15]).is_err());
	}

	#[test]
	fn seq_is_written_read_and_removed_under_unsigned() {
		let mut json = CanonicalJsonObject::new();
		assert_eq!(get_json_seq(&json), None);

		set_json_seq(&mut json, 42);
		assert_eq!(get_json_seq(&json), Some(42));
		let Some(CanonicalJsonValue::Object(unsigned)) = json.get("unsigned") else {
			panic!("unsigned is an object")
		};
		assert_eq!(unsigned.get(SEQ_KEY), Some(&CanonicalJsonValue::Integer(42.into())));

		set_json_seq(&mut json, -3);
		assert_eq!(get_json_seq(&json), Some(-3));

		remove_json_seq(&mut json);
		assert_eq!(get_json_seq(&json), None);
		assert!(matches!(json.get("unsigned"), Some(CanonicalJsonValue::Object(_))));
	}

	#[test]
	fn existing_unsigned_members_survive_and_a_non_object_unsigned_is_replaced() {
		let mut json = CanonicalJsonObject::new();
		let mut unsigned = CanonicalJsonObject::new();
		unsigned.insert("age".into(), CanonicalJsonValue::Integer(5.into()));
		json.insert("unsigned".into(), CanonicalJsonValue::Object(unsigned));

		set_json_seq(&mut json, 1);
		let Some(CanonicalJsonValue::Object(unsigned)) = json.get("unsigned") else {
			panic!("unsigned is an object")
		};
		assert_eq!(unsigned.get("age"), Some(&CanonicalJsonValue::Integer(5.into())));
		assert_eq!(get_json_seq(&json), Some(1));

		json.insert("unsigned".into(), CanonicalJsonValue::String("bogus".into()));
		assert_eq!(get_json_seq(&json), None);
		set_json_seq(&mut json, 9);
		assert_eq!(get_json_seq(&json), Some(9));
	}
}
