//! A holder: one foreign key from a media item to the thing that keeps it
//! alive. See `docs/design/media-holders.md` §2.
//!
//! Keys are built here and nowhere else. `mxc_holder` answers "who holds M"
//! (prefix `M`); `holder_mxc` answers "what does this holder hold" (prefix
//! `kind, room[, g_seq]` or `a, localpart`); `room_mxc` is the deletion
//! accelerator for a whole room. Values are empty: the key is the fact.

use std::fmt;

use ruma::{OwnedRoomId, RoomId, UserId};

/// Who holds a media item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
	/// A timeline event, by room and server-wide position (`g_seq`).
	Event { room: OwnedRoomId, g_seq: i64 },
	/// The retained unredacted original of that event, once it was redacted.
	Backup { room: OwnedRoomId, g_seq: i64 },
	/// A local user's avatar, by localpart: the domain may change, and
	/// avatars of other servers' users are never counted.
	Avatar { localpart: String },
}

/// The kind byte-string in keys; short so a prefix stays cheap.
pub const KIND_EVENT: &str = "e";
pub const KIND_BACKUP: &str = "b";
pub const KIND_AVATAR: &str = "a";

impl Holder {
	#[must_use]
	pub fn event(room: &RoomId, g_seq: i64) -> Self { Self::Event { room: room.to_owned(), g_seq } }

	#[must_use]
	pub fn backup(room: &RoomId, g_seq: i64) -> Self { Self::Backup { room: room.to_owned(), g_seq } }

	#[must_use]
	pub fn avatar(user: &UserId) -> Self { Self::Avatar { localpart: user.localpart().to_owned() } }

	#[must_use]
	pub fn kind(&self) -> &'static str {
		match self {
			| Self::Event { .. } => KIND_EVENT,
			| Self::Backup { .. } => KIND_BACKUP,
			| Self::Avatar { .. } => KIND_AVATAR,
		}
	}

	/// The `mxc_holder` key: `mxc ‖ kind ‖ room ‖ g_seq` or `mxc ‖ a ‖ localpart`.
	#[must_use]
	pub fn mxc_holder_key(&self, mxc: &str) -> Vec<u8> {
		match self {
			| Self::Event { room, g_seq } | Self::Backup { room, g_seq } => {
				tuwunel_database::serialize_key((mxc, self.kind(), room.as_str(), bias_g_seq(*g_seq)))
					.expect("holder key serializes")
					.to_vec()
			},
			| Self::Avatar { localpart } => tuwunel_database::serialize_key((mxc, self.kind(), localpart.as_str()))
				.expect("holder key serializes")
				.to_vec(),
		}
	}

	/// The `holder_mxc` key: `kind ‖ room ‖ g_seq ‖ mxc` or `a ‖ localpart ‖ mxc`.
	#[must_use]
	pub fn holder_mxc_key(&self, mxc: &str) -> Vec<u8> {
		match self {
			| Self::Event { room, g_seq } | Self::Backup { room, g_seq } => {
				tuwunel_database::serialize_key((self.kind(), room.as_str(), bias_g_seq(*g_seq), mxc))
					.expect("holder key serializes")
					.to_vec()
			},
			| Self::Avatar { localpart } => tuwunel_database::serialize_key((self.kind(), localpart.as_str(), mxc))
				.expect("holder key serializes")
				.to_vec(),
		}
	}

	/// The room this holder lives in, for `room_mxc`; avatars have none.
	#[must_use]
	pub fn room(&self) -> Option<&RoomId> {
		match self {
			| Self::Event { room, .. } | Self::Backup { room, .. } => Some(room),
			| Self::Avatar { .. } => None,
		}
	}
}

impl fmt::Display for Holder {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Event { room, g_seq } => write!(f, "event g_seq {g_seq} in {room}"),
			| Self::Backup { room, g_seq } => write!(f, "retained original of g_seq {g_seq} in {room}"),
			| Self::Avatar { localpart } => write!(f, "avatar of @{localpart}"),
		}
	}
}

/// Offset-binary form of a signed `g_seq`, so key order is numeric order
/// (backfilled negatives sort below forward positives).
#[must_use]
pub fn bias_g_seq(g_seq: i64) -> u64 { g_seq.wrapping_sub(i64::MIN).cast_unsigned() }

/// Inverse of `bias_g_seq`.
#[must_use]
pub fn unbias_g_seq(biased: u64) -> i64 { biased.cast_signed().wrapping_add(i64::MIN) }

#[cfg(test)]
mod tests {
	use ruma::{room_id, user_id};

	use super::*;

	#[test]
	fn bias_keeps_numeric_order_and_round_trips() {
		let values = [i64::MIN, -5, -1, 0, 1, 7, i64::MAX];
		let biased: Vec<u64> = values.iter().map(|v| bias_g_seq(*v)).collect();
		assert!(biased.windows(2).all(|w| w[0] < w[1]), "{biased:?}");
		for value in values {
			assert_eq!(unbias_g_seq(bias_g_seq(value)), value);
		}
	}

	#[test]
	fn keys_start_with_their_prefixes() {
		let room = room_id!("!r:localhost");
		let event = Holder::event(room, 42);
		let key = event.mxc_holder_key("mxc://localhost/abc");
		let prefix = tuwunel_database::serialize_key(("mxc://localhost/abc", KIND_EVENT, room.as_str())).unwrap();
		assert!(key.starts_with(&prefix));

		let reverse = event.holder_mxc_key("mxc://localhost/abc");
		let reverse_prefix = tuwunel_database::serialize_key((KIND_EVENT, room.as_str(), bias_g_seq(42))).unwrap();
		assert!(reverse.starts_with(&reverse_prefix));

		let avatar = Holder::avatar(user_id!("@alice:localhost"));
		assert_eq!(avatar, Holder::Avatar { localpart: "alice".into() });
		assert_eq!(avatar.room(), None);
		assert_eq!(avatar.kind(), KIND_AVATAR);
	}

	#[test]
	fn a_media_prefix_does_not_match_a_longer_uri() {
		// `has_holders` and `forget_media` seek with `(mxc, Interfix)`: the
		// separator after the URI keeps `mxc://s/ab` from matching the rows of
		// `mxc://s/abc`. A bare `(mxc,)` prefix would.
		let room = room_id!("!r:localhost");
		let longer = Holder::event(room, 1).mxc_holder_key("mxc://localhost/abc");
		let with_separator = tuwunel_database::serialize_key(("mxc://localhost/ab", tuwunel_database::Interfix)).unwrap();
		let bare = tuwunel_database::serialize_key(("mxc://localhost/ab",)).unwrap();
		assert!(!longer.starts_with(&with_separator));
		assert!(longer.starts_with(&bare), "the bare prefix is the bug this guards against");
		let exact = tuwunel_database::serialize_key(("mxc://localhost/abc", tuwunel_database::Interfix)).unwrap();
		assert!(longer.starts_with(&exact));
	}

	#[test]
	fn event_and_backup_of_one_position_are_different_holders() {
		let room = room_id!("!r:localhost");
		assert_ne!(
			Holder::event(room, 1).mxc_holder_key("mxc://localhost/a"),
			Holder::backup(room, 1).mxc_holder_key("mxc://localhost/a")
		);
	}
}
