//! The index answers "is this media still referenced" by seeking one mxc
//! prefix. A key that cannot be reached from its own prefix reads as
//! unreferenced, and that failure releases media, so the key bytes are
//! asserted directly rather than through behaviour.

use ruma::{event_id, user_id};
use tuwunel_database::{Interfix, serialize_key};

use super::{Holder, describe_holder};

#[test]
fn event_key_starts_with_its_own_mxc_prefix() {
	let mxc = "mxc://example.org/abc";

	let key = serialize_key((mxc, u8::from(Holder::Event), event_id!("$one:example.org")))
		.expect("key serializes");
	let prefix = serialize_key((mxc, Interfix)).expect("prefix serializes");

	assert!(key.starts_with(&prefix), "key must be reachable from its own prefix");
}

#[test]
fn avatar_key_starts_with_the_same_mxc_prefix() {
	let mxc = "mxc://example.org/abc";

	let key = serialize_key((mxc, u8::from(Holder::Avatar), user_id!("@one:example.org")))
		.expect("key serializes");
	let prefix = serialize_key((mxc, Interfix)).expect("prefix serializes");

	assert!(
		key.starts_with(&prefix),
		"one prefix must answer every holder kind, or a kind becomes invisible"
	);
}

#[test]
fn holder_kinds_do_not_collide() {
	let mxc = "mxc://example.org/abc";

	let as_event = serialize_key((mxc, u8::from(Holder::Event), "same-id")).expect("serializes");
	let as_avatar =
		serialize_key((mxc, u8::from(Holder::Avatar), "same-id")).expect("serializes");

	assert_ne!(as_event, as_avatar, "the same id under two kinds must be two rows");
}

#[test]
fn a_longer_mxc_does_not_answer_a_shorter_prefix() {
	let prefix = serialize_key(("mxc://example.org/abc", Interfix)).expect("prefix serializes");
	let longer = serialize_key((
		"mxc://example.org/abcdef",
		u8::from(Holder::Event),
		event_id!("$one:example.org"),
	))
	.expect("key serializes");

	assert!(
		!longer.starts_with(&prefix),
		"the separator is what keeps one media item's rows out of another's answer"
	);
}

#[test]
fn holder_discriminants_are_stable() {
	// Changing these renames rows already on disk into rows nothing can find.
	assert_eq!(u8::from(Holder::Event), 0x01);
	assert_eq!(u8::from(Holder::Avatar), 0x02);
	assert_eq!(Holder::find_name(0x01), Some("event"));
	assert_eq!(Holder::find_name(0x02), Some("avatar"));
	assert_eq!(Holder::find_name(0xFF), None, "an unknown kind must not be guessed at");
}

/// Serializing a key and reading it back is one round trip, and the earlier
/// tests only walk the first half of it. A key that serializes but cannot be
/// read is exactly what shipped and panicked once, so the trip is asserted
/// whole.
#[test]
fn a_stored_event_key_describes_its_own_holder() {
	let mxc = "mxc://example.org/abc";

	let prefix = serialize_key((mxc, Interfix)).expect("prefix serializes");
	let key = serialize_key((mxc, u8::from(Holder::Event), event_id!("$one:example.org")))
		.expect("key serializes");

	assert_eq!(
		describe_holder(&key, prefix.len()),
		Some("event $one:example.org".to_owned())
	);
}

#[test]
fn a_stored_avatar_key_describes_its_own_holder() {
	let mxc = "mxc://example.org/abc";

	let prefix = serialize_key((mxc, Interfix)).expect("prefix serializes");
	let key = serialize_key((mxc, u8::from(Holder::Avatar), user_id!("@one:example.org")))
		.expect("key serializes");

	assert_eq!(
		describe_holder(&key, prefix.len()),
		Some("avatar @one:example.org".to_owned())
	);
}

#[test]
fn an_unreadable_key_is_skipped_rather_than_guessed_at() {
	let mxc = "mxc://example.org/abc";
	let prefix_len = serialize_key((mxc, Interfix))
		.expect("prefix serializes")
		.len();

	let unknown_kind = serialize_key((mxc, 0xFF_u8, "who")).expect("key serializes");
	assert_eq!(describe_holder(&unknown_kind, prefix_len), None, "unknown kind");

	let truncated = serialize_key((mxc, Interfix)).expect("prefix serializes");
	assert_eq!(describe_holder(&truncated, prefix_len), None, "no tail at all");

	assert_eq!(describe_holder(b"short", prefix_len), None, "key shorter than its prefix");
}
