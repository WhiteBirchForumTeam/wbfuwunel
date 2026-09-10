//! The vocabulary of `Control/Error` codes (`docs/design/wbf-wire-format.md`
//! §3.4). Every code a pack may carry is a variant here, and a variant is the
//! only way to name one: both wire fields — the number a program compares and
//! the name a person reads — come from this table, so a call site cannot
//! invent a code the specification does not list. That is the whole point of
//! the type: the codes drifted (`Conflict` meaning "your JSON is malformed"
//! in eleven places) exactly because each call site wrote its own string.
//!
//! Adding a code is two edits in this order: the row in §3.4 first, then the
//! variant here.

use super::PackError;

/// One `Control/Error` code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectCode {
	/// 1001: the pack's version byte is not one this server speaks.
	UnsupportedVersion,
	/// 1002: this pack does not decode — checksum, truncation, reserved
	/// flags, or a text frame where a pack was expected.
	Corrupt,
	/// 1101: no handler for this `(kind, subtype)`.
	UnknownKind,
	/// 1102: there is a handler, but not on this transport.
	Unsupported,
	/// 1103: past `wbf_meta_max_bytes`, `wbf_data_max_bytes`, or an upload's
	/// declared size.
	TooLarge,
	/// 1201: the pack decodes, but its meta or data is not what this subtype
	/// takes — bad JSON, wrong type, missing field, value out of range.
	InvalidRequest,
	/// 1301: not logged in, or no longer valid (expired, revoked, locked).
	Unauthorized,
	/// 1302: authenticated but not allowed.
	Forbidden,
	/// 1401: too fast; carries `retry_after_ms`.
	RateLimited,
	/// 1402: this device holds as many connections as it may.
	TooManyConnections,
	/// 1501: the named thing does not exist.
	NotFound,
	/// 1502: a legal request that the current state refuses.
	Conflict,
	/// 1503: an ordered kind's `seq` is not the one expected; carries
	/// `expected_seq`.
	OutOfOrder,
	/// 1504: an upload hit the size limit and was finished as incomplete.
	Truncated,
	/// 1901: the server's own fault.
	Internal,
}

impl RejectCode {
	/// Every code, so a test can hold the whole table at once.
	pub const ALL: [Self; 15] = [
		Self::UnsupportedVersion,
		Self::Corrupt,
		Self::UnknownKind,
		Self::Unsupported,
		Self::TooLarge,
		Self::InvalidRequest,
		Self::Unauthorized,
		Self::Forbidden,
		Self::RateLimited,
		Self::TooManyConnections,
		Self::NotFound,
		Self::Conflict,
		Self::OutOfOrder,
		Self::Truncated,
		Self::Internal,
	];

	/// Return:
	///     u16  the wire's `code_id`, example: 1201. Always 1000 or above, so
	///     a missing field or a defaulted `0` can never read as a real code;
	///     the hundreds say which family it is (§3.4).
	#[must_use]
	pub const fn id(self) -> u16 {
		match self {
			| Self::UnsupportedVersion => 1001,
			| Self::Corrupt => 1002,
			| Self::UnknownKind => 1101,
			| Self::Unsupported => 1102,
			| Self::TooLarge => 1103,
			| Self::InvalidRequest => 1201,
			| Self::Unauthorized => 1301,
			| Self::Forbidden => 1302,
			| Self::RateLimited => 1401,
			| Self::TooManyConnections => 1402,
			| Self::NotFound => 1501,
			| Self::Conflict => 1502,
			| Self::OutOfOrder => 1503,
			| Self::Truncated => 1504,
			| Self::Internal => 1901,
		}
	}

	/// Return:
	///     &'static str  the wire's `code`, example: "InvalidRequest".
	#[must_use]
	pub const fn name(self) -> &'static str {
		match self {
			| Self::UnsupportedVersion => "UnsupportedVersion",
			| Self::Corrupt => "Corrupt",
			| Self::UnknownKind => "UnknownKind",
			| Self::Unsupported => "Unsupported",
			| Self::TooLarge => "TooLarge",
			| Self::InvalidRequest => "InvalidRequest",
			| Self::Unauthorized => "Unauthorized",
			| Self::Forbidden => "Forbidden",
			| Self::RateLimited => "RateLimited",
			| Self::TooManyConnections => "TooManyConnections",
			| Self::NotFound => "NotFound",
			| Self::Conflict => "Conflict",
			| Self::OutOfOrder => "OutOfOrder",
			| Self::Truncated => "Truncated",
			| Self::Internal => "Internal",
		}
	}

	/// The code for a pack that failed to decode.
	///
	/// Args:
	///     error: example: `PackError::MetaCrc { .. }`
	/// Return:
	///     RejectCode  `UnsupportedVersion`, `UnknownKind` or `TooLarge` when
	///     the error says which; `Corrupt` for every other way a pack can
	///     fail to decode.
	#[must_use]
	pub const fn for_pack_error(error: &PackError) -> Self {
		match error {
			| PackError::UnsupportedVersion(_) => Self::UnsupportedVersion,
			| PackError::UnknownKind(_) => Self::UnknownKind,
			| PackError::SectionTooLarge { .. } => Self::TooLarge,
			| _ => Self::Corrupt,
		}
	}

	/// Whether this code says the frame itself could not be read, which is
	/// what the connection health counter counts (§2.1). A code that means
	/// "the request was wrong" does not: the peer speaks the protocol.
	#[must_use]
	pub const fn is_undecodable_frame(self) -> bool {
		matches!(self, Self::Corrupt | Self::UnsupportedVersion)
	}
}

#[cfg(test)]
mod tests {
	use super::RejectCode;

	#[test]
	fn every_code_has_its_own_number_and_its_own_name() {
		let mut ids: Vec<u16> = RejectCode::ALL.iter().map(|code| code.id()).collect();
		let mut names: Vec<&str> = RejectCode::ALL.iter().map(|code| code.name()).collect();
		let total = RejectCode::ALL.len();

		ids.sort_unstable();
		ids.dedup();
		names.sort_unstable();
		names.dedup();

		assert_eq!(ids.len(), total, "two codes share a code_id");
		assert_eq!(names.len(), total, "two codes share a name");
	}

	#[test]
	fn no_code_id_can_be_confused_with_a_missing_field() {
		for code in RejectCode::ALL {
			assert!(code.id() >= 1000, "{} is low enough to collide with a default", code.name());
		}
	}

	#[test]
	fn only_the_frame_level_codes_count_against_the_connection() {
		assert!(RejectCode::Corrupt.is_undecodable_frame());
		assert!(RejectCode::UnsupportedVersion.is_undecodable_frame());
		// The peer speaks the protocol; this one request is wrong.
		assert!(!RejectCode::InvalidRequest.is_undecodable_frame());
		assert!(!RejectCode::Unauthorized.is_undecodable_frame());
		assert!(!RejectCode::UnknownKind.is_undecodable_frame());
	}
}
