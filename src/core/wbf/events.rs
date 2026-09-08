//! The data layout shared by every pack that carries events: `Event/Batch`
//! (a `Recent` window) and `Event/Push` (a subscription), see
//! `docs/design/room-seq-and-recent.md` §2.1 and `docs/design/wbf-event-push.md`
//! §2. Each event is a big-endian u32 length followed by its JSON bytes, so a
//! receiver slices by length and never scans for separators.

use super::PackError;

/// Bytes of length prefix in front of each event.
pub const EVENT_LEN_PREFIX: usize = 4;

/// Args:
///     events: the events' JSON, in the order they go on the wire, example:
///         two `m.room.message` events newest first
/// Return:
///     Result<Vec<u8>, PackError>  `SectionTooLarge` when one event's length
///     does not fit the u32 prefix.
pub fn length_prefixed<I>(events: I) -> Result<Vec<u8>, PackError>
where
	I: IntoIterator,
	I::Item: AsRef<[u8]>,
{
	let mut data = Vec::new();
	for event in events {
		let event = event.as_ref();
		let len = u32::try_from(event.len()).map_err(|_| PackError::SectionTooLarge { len: event.len() })?;
		data.extend_from_slice(&len.to_be_bytes());
		data.extend_from_slice(event);
	}

	Ok(data)
}

/// Bytes one event costs inside the data section: its prefix plus itself.
#[must_use]
pub const fn framed_len(event_len: usize) -> usize { EVENT_LEN_PREFIX.saturating_add(event_len) }

/// The inverse, for tests and tooling.
///
/// Args:
///     data: a data section written by `length_prefixed`
/// Return:
///     Result<Vec<&[u8]>, PackError>  `Truncated` when a prefix promises more
///     bytes than remain.
pub fn split_length_prefixed(data: &[u8]) -> Result<Vec<&[u8]>, PackError> {
	let mut events = Vec::new();
	let mut at = 0;
	while at < data.len() {
		let prefix = data
			.get(at..at + EVENT_LEN_PREFIX)
			.ok_or(PackError::Truncated { needed: at + EVENT_LEN_PREFIX, len: data.len() })?;
		let len = u32::from_be_bytes(prefix.try_into().expect("4 bytes")) as usize;
		at += EVENT_LEN_PREFIX;
		let event = data
			.get(at..at + len)
			.ok_or(PackError::Truncated { needed: at + len, len: data.len() })?;
		events.push(event);
		at += len;
	}

	Ok(events)
}

#[cfg(test)]
mod tests {
	use super::{length_prefixed, split_length_prefixed};

	#[test]
	fn round_trips_and_keeps_order() {
		let data = length_prefixed([b"{\"a\":1}".as_slice(), b"".as_slice(), b"{\"b\":22}".as_slice()]).expect("fits");
		assert_eq!(data.len(), 4 + 7 + 4 + 0 + 4 + 8);
		let back = split_length_prefixed(&data).expect("splits");
		assert_eq!(back, vec![b"{\"a\":1}".as_slice(), b"".as_slice(), b"{\"b\":22}".as_slice()]);
	}

	#[test]
	fn a_short_tail_is_refused_not_read_past() {
		let mut data = length_prefixed([b"abcdef".as_slice()]).expect("fits");
		data.truncate(data.len() - 2);
		assert!(split_length_prefixed(&data).is_err());
	}

	#[test]
	fn empty_data_is_no_events() {
		assert!(split_length_prefixed(&[]).expect("empty").is_empty());
	}
}
