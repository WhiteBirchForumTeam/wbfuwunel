//! One pack: a fixed header, a variable meta section, a variable data section,
//! and a CRC-32C after each variable section.
//!
//! ```text
//! offset  size  field
//! 0       1     version      1
//! 1       1     kind
//! 2       1     subtype
//! 3       1     flags
//! 4       8     id           u64 BE
//! 12      4     seq          u32 BE
//! 16      4     meta_len     u32 BE
//! 20      m     meta
//! 20+m    4     meta_crc     CRC-32C of bytes 0 .. 20+m
//! 24+m    4     data_len     u32 BE
//! 28+m    n     data
//! 28+m+n  4     data_crc     CRC-32C of data only
//! ```
//!
//! Decoding is fixed offsets plus two CRC passes; nothing is scanned, parsed
//! or copied. The view it returns borrows the caller's buffer, so a receiver
//! decrypts `data` in place. The builder hands out the data section as a
//! writable slice before the pack is finished, so a sender encrypts straight
//! into it. Encryption itself is not this module's business.

use std::fmt;

use serde_json::Value;

/// The one wire version this code speaks.
pub const VERSION: u8 = 1;

/// Bytes before `meta_len`.
pub const HEADER_LEN: usize = 16;

/// Fixed bytes around the two variable sections: header, two lengths, two
/// CRCs.
pub const OVERHEAD: usize = HEADER_LEN + 4 + 4 + 4 + 4;

/// Kept for callers that size buffers by "header plus trailer"; the trailer
/// is the two length fields and two CRCs.
pub const TRAILER_LEN: usize = OVERHEAD - HEADER_LEN;

/// Message families. Numbers are the on-wire byte and never change; see
/// `docs/design/wbf-wire-format.md` §3.3 for the reserved ranges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
	/// The channel's own business: hello, ack, error, ping.
	Control = 0x01,
	/// Streaming messages.
	Stream = 0x02,
	/// Chunked upload.
	Upload = 0x03,
	/// Chunked download.
	Download = 0x04,
	/// Login, logout, refresh, register.
	Session = 0x10,
	/// Account data, profile, third-party ids, password.
	Account = 0x11,
	/// Sync and filters.
	Sync = 0x12,
	/// Room lifecycle and membership, aliases, directory, spaces.
	Room = 0x13,
	/// Sending and reading events: send, redact, state, context, relations.
	Event = 0x14,
	/// Read markers, receipts, typing, presence.
	Receipt = 0x15,
	/// Devices, to-device messages, dehydrated devices.
	Device = 0x16,
	/// End-to-end encryption keys, backups, cross-signing.
	Keys = 0x17,
	/// Pushers, push rules, notifications.
	Push = 0x18,
	/// The compatibility media path: whole-file upload, thumbnails, previews.
	Media = 0x19,
	/// Search and the user directory.
	Search = 0x1A,
	/// TURN and RTC.
	Voip = 0x1B,
	/// Everything else: capabilities, versions, well-known, tags, reports.
	Misc = 0x1C,
	/// Administration commands and the admin API.
	Admin = 0x20,
}

impl TryFrom<u8> for Kind {
	type Error = PackError;

	fn try_from(byte: u8) -> Result<Self, PackError> {
		Ok(match byte {
			| 0x01 => Self::Control,
			| 0x02 => Self::Stream,
			| 0x03 => Self::Upload,
			| 0x04 => Self::Download,
			| 0x10 => Self::Session,
			| 0x11 => Self::Account,
			| 0x12 => Self::Sync,
			| 0x13 => Self::Room,
			| 0x14 => Self::Event,
			| 0x15 => Self::Receipt,
			| 0x16 => Self::Device,
			| 0x17 => Self::Keys,
			| 0x18 => Self::Push,
			| 0x19 => Self::Media,
			| 0x1A => Self::Search,
			| 0x1B => Self::Voip,
			| 0x1C => Self::Misc,
			| 0x20 => Self::Admin,
			| unknown => return Err(PackError::UnknownKind(unknown)),
		})
	}
}

/// The flags byte. Reserved bits must be zero on the wire.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Flags(
	/// The raw bits as they appear on the wire.
	pub u8,
);

impl Flags {
	/// `meta` is ciphertext for another client; do not read it as JSON.
	pub const META_ENCRYPTED: Self = Self(0b0000_0001);
	/// The sender wants an `Ack` for this pack.
	pub const WANT_ACK: Self = Self(0b0000_0010);
	/// This pack answers the request with the same `id` and `seq`.
	pub const IS_RESPONSE: Self = Self(0b0000_0100);
	/// The last pack of its ordered sequence: the final chunk of an upload,
	/// the final fragment of a stream.
	pub const IS_LAST: Self = Self(0b0000_1000);
	const KNOWN: u8 = 0b0000_1111;

	/// Whether every bit of `other` is set.
	#[must_use]
	pub const fn contains(self, other: Self) -> bool { self.0 & other.0 == other.0 }

	/// Both sets of bits.
	#[must_use]
	pub const fn union(self, other: Self) -> Self { Self(self.0 | other.0) }

	/// Whether `META_ENCRYPTED` is set.
	#[must_use]
	pub const fn is_meta_encrypted(self) -> bool { self.contains(Self::META_ENCRYPTED) }

	/// Whether `WANT_ACK` is set.
	#[must_use]
	pub const fn wants_ack(self) -> bool { self.contains(Self::WANT_ACK) }

	/// Whether `IS_RESPONSE` is set.
	#[must_use]
	pub const fn is_response(self) -> bool { self.contains(Self::IS_RESPONSE) }

	/// Whether `IS_LAST` is set.
	#[must_use]
	pub const fn is_last(self) -> bool { self.contains(Self::IS_LAST) }

	const fn has_reserved_bits(self) -> bool { self.0 & !Self::KNOWN != 0 }
}

/// The fixed header, decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackHeader {
	/// Wire version; always `VERSION` after a successful decode.
	pub version: u8,
	/// Message family.
	pub kind: Kind,
	/// Operation within the family; meaning defined per kind.
	pub subtype: u8,
	/// Flag bits.
	pub flags: Flags,
	/// Session or object identity: an upload id, a stream id; zero for none.
	pub id: u64,
	/// Sequence within `id` for ordered kinds; a request number otherwise.
	pub seq: u32,
}

/// A decoded pack borrowing the buffer it was decoded from.
///
/// `meta` and `data` point into that buffer; a receiver that must decrypt
/// does so in place and never copies the section out.
#[derive(Debug)]
pub struct PackView<'a> {
	/// The decoded fixed header.
	pub header: PackHeader,
	/// The meta section, in place in the decoded buffer.
	pub meta: &'a mut [u8],
	/// The data section, in place in the decoded buffer.
	pub data: &'a mut [u8],
}

impl PackView<'_> {
	/// Parses `meta` as JSON. Only meaningful when the flags say it is not
	/// encrypted; on an encrypted meta this fails like any other non-JSON.
	pub fn meta_json(&self) -> Result<Value, PackError> {
		serde_json::from_slice(self.meta).map_err(|_| PackError::MetaNotJson)
	}
}

/// Why a buffer is not a pack. Each variant names the part that failed, so a
/// receiver can tell a damaged chunk from a damaged frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackError {
	/// Fewer bytes than the fixed overhead.
	TooShort {
		/// The buffer's length.
		len: usize,
	},
	/// The version byte is not one this code speaks.
	UnsupportedVersion(u8),
	/// The kind byte is not assigned.
	UnknownKind(u8),
	/// A reserved flag bit is set.
	ReservedFlags(u8),
	/// A length field claims more than the buffer holds.
	Truncated {
		/// Bytes the pack would need.
		needed: usize,
		/// Bytes the buffer has.
		len: usize,
	},
	/// The buffer holds more than the pack describes.
	TrailingBytes {
		/// Bytes the pack describes.
		expected: usize,
		/// Bytes the buffer has.
		len: usize,
	},
	/// The header or meta section does not match its checksum.
	MetaCrc {
		/// The checksum on the wire.
		expected: u32,
		/// The checksum of what arrived.
		actual: u32,
	},
	/// The data section does not match its checksum.
	DataCrc {
		/// The checksum on the wire.
		expected: u32,
		/// The checksum of what arrived.
		actual: u32,
	},
	/// The meta section was asked for as JSON and is not.
	MetaNotJson,
	/// A section handed to the builder exceeds `u32::MAX`.
	SectionTooLarge {
		/// The section's length.
		len: usize,
	},
	/// `data_slot` was called twice, or after `finish` semantics were violated.
	DataAlreadyReserved,
}

impl fmt::Display for PackError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Debug::fmt(self, f) }
}

impl std::error::Error for PackError {}

/// Assembles one pack into a single buffer, sized once.
///
/// Call order: `new` → optional `meta` → optional `data_slot` → `finish`.
/// Whoever encrypts writes ciphertext straight into the slice `data_slot`
/// returns; `finish` computes both CRCs and returns the bytes.
#[derive(Debug)]
pub struct PackBuilder {
	buf: Vec<u8>,
	meta_end: usize,
	data_start: Option<usize>,
}

impl PackBuilder {
	/// Starts a pack with the given header and an empty meta section.
	#[must_use]
	pub fn new(kind: Kind, subtype: u8, flags: Flags, id: u64, seq: u32) -> Self {
		let mut buf = Vec::with_capacity(OVERHEAD);
		buf.push(VERSION);
		buf.push(kind as u8);
		buf.push(subtype);
		buf.push(flags.0);
		buf.extend_from_slice(&id.to_be_bytes());
		buf.extend_from_slice(&seq.to_be_bytes());
		buf.extend_from_slice(&0u32.to_be_bytes()); // meta_len, patched by meta()

		Self { buf, meta_end: HEADER_LEN + 4, data_start: None }
	}

	/// Sets the meta section. Plaintext JSON or ciphertext; the builder does
	/// not care. Must precede `data_slot`.
	pub fn meta(mut self, meta: &[u8]) -> Result<Self, PackError> {
		if self.data_start.is_some() {
			return Err(PackError::DataAlreadyReserved);
		}

		let len = u32::try_from(meta.len()).map_err(|_| PackError::SectionTooLarge { len: meta.len() })?;
		self.buf.truncate(HEADER_LEN);
		self.buf.extend_from_slice(&len.to_be_bytes());
		self.buf.extend_from_slice(meta);
		self.meta_end = self.buf.len();

		Ok(self)
	}

	/// Sets the meta section from a JSON value.
	pub fn json_meta(self, meta: &Value) -> Result<Self, PackError> {
		let bytes = serde_json::to_vec(meta).map_err(|_| PackError::MetaNotJson)?;
		self.meta(&bytes)
	}

	/// Reserves `len` bytes for data and returns them for the caller to fill,
	/// typically by encrypting into them. Zeroed until written.
	pub fn data_slot(&mut self, len: usize) -> Result<&mut [u8], PackError> {
		if self.data_start.is_some() {
			return Err(PackError::DataAlreadyReserved);
		}

		let len_field = u32::try_from(len).map_err(|_| PackError::SectionTooLarge { len })?;
		self.buf.reserve(4 + 4 + len + 4);
		self.buf.extend_from_slice(&[0; 4]); // meta_crc, patched by finish()
		self.buf.extend_from_slice(&len_field.to_be_bytes());
		let start = self.buf.len();
		self.buf.resize(start + len, 0);
		self.data_start = Some(start);

		Ok(&mut self.buf[start..])
	}

	/// Copies `data` in. For callers that already hold the bytes; a sender
	/// that encrypts should write into `data_slot` instead.
	pub fn data(mut self, data: &[u8]) -> Result<Self, PackError> {
		self.data_slot(data.len())?.copy_from_slice(data);
		Ok(self)
	}

	/// Computes both CRCs and returns the finished pack.
	#[must_use]
	pub fn finish(mut self) -> Vec<u8> {
		let data_start = match self.data_start {
			| Some(start) => start,
			| None => {
				self.buf.extend_from_slice(&[0; 4]); // meta_crc
				self.buf.extend_from_slice(&0u32.to_be_bytes()); // data_len
				self.buf.len()
			},
		};

		let meta_crc = crc32c::crc32c(&self.buf[..self.meta_end]);
		self.buf[self.meta_end..self.meta_end + 4].copy_from_slice(&meta_crc.to_be_bytes());

		let data_crc = crc32c::crc32c(&self.buf[data_start..]);
		self.buf.extend_from_slice(&data_crc.to_be_bytes());

		self.buf
	}
}

/// Decodes one pack in place.
///
/// Fixed offsets and two CRC passes; the returned view borrows `bytes`. The
/// buffer must hold exactly one pack: a WebSocket message is one pack, and
/// an HTTP body is one pack, so trailing bytes are an error rather than a
/// second pack.
pub fn decode(bytes: &mut [u8]) -> Result<PackView<'_>, PackError> {
	let len = bytes.len();
	if len < OVERHEAD {
		return Err(PackError::TooShort { len });
	}

	let version = bytes[0];
	if version != VERSION {
		return Err(PackError::UnsupportedVersion(version));
	}

	let kind = Kind::try_from(bytes[1])?;
	let subtype = bytes[2];
	let flags = Flags(bytes[3]);
	if flags.has_reserved_bits() {
		return Err(PackError::ReservedFlags(flags.0));
	}

	let id = u64::from_be_bytes(bytes[4..12].try_into().expect("8 bytes"));
	let seq = u32::from_be_bytes(bytes[12..16].try_into().expect("4 bytes"));

	// Each length field is capped by `len` before it is added to anything, so
	// the offsets below cannot overflow a 32-bit usize on a hostile pack.
	let meta_len = read_u32(bytes, HEADER_LEN) as usize;
	if meta_len > len {
		return Err(PackError::Truncated { needed: meta_len.saturating_add(OVERHEAD), len });
	}
	let meta_start = HEADER_LEN + 4;
	let meta_end = meta_start + meta_len;
	let data_len_at = meta_end + 4;
	if data_len_at + 4 > len {
		return Err(PackError::Truncated { needed: data_len_at + 4, len });
	}

	let meta_crc_expected = read_u32(bytes, meta_end);
	let meta_crc_actual = crc32c::crc32c(&bytes[..meta_end]);
	if meta_crc_expected != meta_crc_actual {
		return Err(PackError::MetaCrc { expected: meta_crc_expected, actual: meta_crc_actual });
	}

	let data_len = read_u32(bytes, data_len_at) as usize;
	if data_len > len {
		return Err(PackError::Truncated { needed: data_len.saturating_add(OVERHEAD), len });
	}
	let data_start = data_len_at + 4;
	let data_end = data_start + data_len;
	let total = data_end + 4;
	if total > len {
		return Err(PackError::Truncated { needed: total, len });
	}
	if total < len {
		return Err(PackError::TrailingBytes { expected: total, len });
	}

	let data_crc_expected = read_u32(bytes, data_end);
	let data_crc_actual = crc32c::crc32c(&bytes[data_start..data_end]);
	if data_crc_expected != data_crc_actual {
		return Err(PackError::DataCrc { expected: data_crc_expected, actual: data_crc_actual });
	}

	let (before_data, from_data) = bytes.split_at_mut(data_start);
	let meta = &mut before_data[meta_start..meta_end];
	let data = &mut from_data[..data_len];

	Ok(PackView {
		header: PackHeader { version, kind, subtype, flags, id, seq },
		meta,
		data,
	})
}

#[inline]
fn read_u32(bytes: &[u8], at: usize) -> u32 {
	u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::{Flags, Kind, OVERHEAD, PackBuilder, PackError, VERSION, decode};

	fn sample() -> Vec<u8> {
		PackBuilder::new(Kind::Upload, 2, Flags::WANT_ACK, 0xDEAD_BEEF_0000_0001, 7)
			.meta(br#"{"a":1}"#)
			.expect("meta fits")
			.data(b"chunk bytes")
			.expect("data fits")
			.finish()
	}

	#[test]
	fn round_trips_header_meta_and_data() {
		let mut bytes = sample();
		let view = decode(&mut bytes).expect("decodes");

		assert_eq!(view.header.version, VERSION);
		assert_eq!(view.header.kind, Kind::Upload);
		assert_eq!(view.header.subtype, 2);
		assert!(view.header.flags.wants_ack());
		assert!(!view.header.flags.is_meta_encrypted());
		assert_eq!(view.header.id, 0xDEAD_BEEF_0000_0001);
		assert_eq!(view.header.seq, 7);
		assert_eq!(view.meta, br#"{"a":1}"#);
		assert_eq!(view.data, b"chunk bytes");
		assert_eq!(view.meta_json().expect("json"), json!({ "a": 1 }));
	}

	#[test]
	fn the_view_borrows_the_buffer_it_decoded() {
		let mut bytes = sample();
		let base = bytes.as_ptr() as usize;
		let view = decode(&mut bytes).expect("decodes");

		let data_ptr = view.data.as_ptr() as usize;
		assert!(data_ptr > base && data_ptr < base + bytes_len(&view), "data points into the buffer");

		// Writing through the view is writing into the buffer: in-place decrypt.
		view.data[0] = b'C';
		assert_eq!(&bytes[bytes.len() - 4 - 11..bytes.len() - 4], b"Chunk bytes");
	}

	fn bytes_len(view: &super::PackView<'_>) -> usize { OVERHEAD + view.meta.len() + view.data.len() }

	#[test]
	fn data_slot_is_written_after_reservation_and_still_checksums() {
		let mut builder = PackBuilder::new(Kind::Upload, 2, Flags::default(), 1, 0)
			.meta(b"{}")
			.expect("meta");
		let slot = builder.data_slot(4).expect("slot");
		slot.copy_from_slice(b"WXYZ"); // stands in for encrypt_in_place
		let mut bytes = builder.finish();

		let view = decode(&mut bytes).expect("decodes");
		assert_eq!(view.data, b"WXYZ");
	}

	#[test]
	fn a_damaged_meta_is_reported_as_meta() {
		let mut bytes = sample();
		bytes[21] ^= 0xFF; // inside meta

		assert!(matches!(decode(&mut bytes), Err(PackError::MetaCrc { .. })));
	}

	#[test]
	fn a_damaged_data_byte_is_reported_as_data_not_meta() {
		let mut bytes = sample();
		let len = bytes.len();
		bytes[len - 6] ^= 0xFF; // inside data

		assert!(matches!(decode(&mut bytes), Err(PackError::DataCrc { .. })));
	}

	#[test]
	fn a_damaged_header_is_reported_as_meta_crc() {
		// The meta CRC covers the header, so a flipped id byte is caught.
		let mut bytes = sample();
		bytes[5] ^= 0x01;

		assert!(matches!(decode(&mut bytes), Err(PackError::MetaCrc { .. })));
	}

	#[test]
	fn truncation_and_trailing_bytes_are_rejected() {
		let full = sample();

		let mut short = full[..full.len() - 1].to_vec();
		assert!(matches!(decode(&mut short), Err(PackError::Truncated { .. })));

		let mut tiny = full[..OVERHEAD - 1].to_vec();
		assert!(matches!(decode(&mut tiny), Err(PackError::TooShort { .. })));

		let mut long = full.clone();
		long.push(0);
		assert!(matches!(decode(&mut long), Err(PackError::TrailingBytes { .. })));
	}

	#[test]
	fn a_length_field_pointing_past_the_buffer_is_truncation_not_a_panic() {
		let mut bytes = sample();
		bytes[16..20].copy_from_slice(&u32::MAX.to_be_bytes());

		assert!(matches!(decode(&mut bytes), Err(PackError::Truncated { .. })));
	}

	#[test]
	fn version_zero_unknown_kind_and_reserved_flags_are_rejected() {
		let mut v0 = sample();
		v0[0] = 0;
		assert_eq!(decode(&mut v0).err(), Some(PackError::UnsupportedVersion(0)));

		let mut kind = sample();
		kind[1] = 0x7F;
		assert_eq!(decode(&mut kind).err(), Some(PackError::UnknownKind(0x7F)));

		let mut flags = sample();
		flags[3] = 0b1000_0000;
		assert_eq!(decode(&mut flags).err(), Some(PackError::ReservedFlags(0b1000_0000)));
	}

	#[test]
	fn empty_meta_and_empty_data_are_legal() {
		let mut bytes = PackBuilder::new(Kind::Control, 4, Flags::default(), 0, 9).finish();
		assert_eq!(bytes.len(), OVERHEAD);

		let view = decode(&mut bytes).expect("decodes");
		assert!(view.meta.is_empty());
		assert!(view.data.is_empty());
		assert_eq!(view.header.kind, Kind::Control);
		assert_eq!(view.header.seq, 9);
	}

	#[test]
	fn json_meta_helper_round_trips() {
		let mut bytes = PackBuilder::new(Kind::Download, 2, Flags::IS_RESPONSE, 0, 3)
			.json_meta(&json!({ "pos": 65552, "len": 65552 }))
			.expect("json")
			.finish();

		let view = decode(&mut bytes).expect("decodes");
		assert!(view.header.flags.is_response());
		assert_eq!(view.meta_json().expect("json")["pos"], 65552);
	}

	#[test]
	fn a_second_data_slot_is_refused() {
		let mut builder = PackBuilder::new(Kind::Upload, 2, Flags::default(), 1, 0);
		builder.data_slot(1).expect("first");

		assert_eq!(builder.data_slot(1).err(), Some(PackError::DataAlreadyReserved));
	}

	#[test]
	fn all_kind_bytes_round_trip() {
		for kind in [
			Kind::Control,
			Kind::Stream,
			Kind::Upload,
			Kind::Download,
			Kind::Session,
			Kind::Account,
			Kind::Sync,
			Kind::Room,
			Kind::Event,
			Kind::Receipt,
			Kind::Device,
			Kind::Keys,
			Kind::Push,
			Kind::Media,
			Kind::Search,
			Kind::Voip,
			Kind::Misc,
			Kind::Admin,
		] {
			assert_eq!(Kind::try_from(kind as u8), Ok(kind));
		}
	}
}
