//! The two positions a stored PDU carries in its `unsigned`.
//!
//! `r_seq` is the room's own sequence: forward events count 1, 2, 3… in
//! arrival order; history backfilled from before the room's first known
//! event counts 0, −1, −2…. `g_seq` is the server-wide count the
//! event was stored under, the same number a `/messages` token encodes: a
//! watermark a client keeps to ask "everything newer than this" across all
//! its rooms. Neither is contiguous for a reader (other rooms, invisible
//! events); `r_seq` is contiguous within a room. See
//! `docs/design/room-seq-and-recent.md`.
//!
//! Both live in the stored JSON, so every path that serves the stored event
//! carries them without knowing about them. The things that rewrite that
//! JSON, redaction and the federation wire format, are the only readers here.

use ruma::{CanonicalJsonObject, CanonicalJsonValue};

use crate::{Result, err};

/// The `unsigned` key of the per-room sequence number.
pub const R_SEQ_KEY: &str = "org.wbftw.wbfuwunel.r_seq";

/// The `unsigned` key of the server-wide position (the event's PduCount, in
/// its signed form: backfilled history is nonpositive).
pub const G_SEQ_KEY: &str = "org.wbftw.wbfuwunel.g_seq";

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

/// The two positions of one stored event, as written at append time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Positions {
	/// The per-room sequence number.
	pub r_seq: i64,
	/// The server-wide count, signed.
	pub g_seq: i64,
}

/// Args:
///     json: a stored PDU object, example: the value of `pduid_pdu`
/// Return:
///     Option<Positions>  None when either key is missing (an outlier, or a
///     database from before this fork numbered its rooms).
#[must_use]
pub fn get_json_positions(json: &CanonicalJsonObject) -> Option<Positions> {
	Some(Positions {
		r_seq: get_unsigned_integer(json, R_SEQ_KEY)?,
		g_seq: get_unsigned_integer(json, G_SEQ_KEY)?,
	})
}

/// Writes both positions into `json["unsigned"]`, creating the object when
/// absent and replacing a non-object `unsigned` outright.
pub fn set_json_positions(json: &mut CanonicalJsonObject, positions: Positions) {
	let unsigned = json
		.entry("unsigned".into())
		.or_insert_with(|| CanonicalJsonValue::Object(CanonicalJsonObject::new()));

	if !matches!(unsigned, CanonicalJsonValue::Object(_)) {
		*unsigned = CanonicalJsonValue::Object(CanonicalJsonObject::new());
	}

	if let CanonicalJsonValue::Object(unsigned) = unsigned {
		// Canonical JSON integers are bounded to ±2^53; neither number will
		// get there, and clamping is the honest fallback if one somehow did.
		unsigned.insert(R_SEQ_KEY.into(), CanonicalJsonValue::Integer(ruma::Int::new_saturating(positions.r_seq)));
		unsigned.insert(
			G_SEQ_KEY.into(),
			CanonicalJsonValue::Integer(ruma::Int::new_saturating(positions.g_seq)),
		);
	}
}

/// Removes both positions from `json["unsigned"]`, for copies that leave this
/// server.
pub fn remove_json_positions(json: &mut CanonicalJsonObject) {
	if let Some(CanonicalJsonValue::Object(unsigned)) = json.get_mut("unsigned") {
		unsigned.remove(R_SEQ_KEY);
		unsigned.remove(G_SEQ_KEY);
	}
}

fn get_unsigned_integer(json: &CanonicalJsonObject, key: &str) -> Option<i64> {
	match json.get("unsigned")? {
		| CanonicalJsonValue::Object(unsigned) => match unsigned.get(key)? {
			| CanonicalJsonValue::Integer(value) => Some(i64::from(*value)),
			| _ => None,
		},
		| _ => None,
	}
}

#[cfg(test)]
mod tests {
	use ruma::CanonicalJsonValue;

	use super::*;

	fn unsigned_of(json: &CanonicalJsonObject) -> &CanonicalJsonObject {
		match json.get("unsigned") {
			| Some(CanonicalJsonValue::Object(unsigned)) => unsigned,
			| _ => panic!("unsigned is an object"),
		}
	}

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
	fn positions_are_written_read_and_removed_under_unsigned() {
		let mut json = CanonicalJsonObject::new();
		assert_eq!(get_json_positions(&json), None);

		let positions = Positions { r_seq: 42, g_seq: 9001 };
		set_json_positions(&mut json, positions);
		assert_eq!(get_json_positions(&json), Some(positions));
		assert_eq!(unsigned_of(&json).get(R_SEQ_KEY), Some(&CanonicalJsonValue::Integer(42.into())));
		assert_eq!(unsigned_of(&json).get(G_SEQ_KEY), Some(&CanonicalJsonValue::Integer(9001.into())));

		set_json_positions(&mut json, Positions { r_seq: -3, g_seq: -77 });
		assert_eq!(get_json_positions(&json), Some(Positions { r_seq: -3, g_seq: -77 }));

		remove_json_positions(&mut json);
		assert_eq!(get_json_positions(&json), None);
		assert!(unsigned_of(&json).is_empty());
	}

	#[test]
	fn one_key_alone_is_not_a_position() {
		let mut json = CanonicalJsonObject::new();
		let mut unsigned = CanonicalJsonObject::new();
		unsigned.insert(R_SEQ_KEY.into(), CanonicalJsonValue::Integer(1.into()));
		json.insert("unsigned".into(), CanonicalJsonValue::Object(unsigned));
		assert_eq!(get_json_positions(&json), None);
	}

	#[test]
	fn existing_unsigned_members_survive_and_a_non_object_unsigned_is_replaced() {
		let mut json = CanonicalJsonObject::new();
		let mut unsigned = CanonicalJsonObject::new();
		unsigned.insert("age".into(), CanonicalJsonValue::Integer(5.into()));
		json.insert("unsigned".into(), CanonicalJsonValue::Object(unsigned));

		set_json_positions(&mut json, Positions { r_seq: 1, g_seq: 1 });
		assert_eq!(unsigned_of(&json).get("age"), Some(&CanonicalJsonValue::Integer(5.into())));
		assert_eq!(get_json_positions(&json).map(|p| p.r_seq), Some(1));

		json.insert("unsigned".into(), CanonicalJsonValue::String("bogus".into()));
		assert_eq!(get_json_positions(&json), None);
		set_json_positions(&mut json, Positions { r_seq: 9, g_seq: 8 });
		assert_eq!(get_json_positions(&json), Some(Positions { r_seq: 9, g_seq: 8 }));
	}
}
