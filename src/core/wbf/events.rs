//! The data layout shared by every pack that carries events: `Event/Batch`
//! (a `Recent` window) and `Event/Push` (a subscription), see
//! `docs/design/room-seq-and-recent.md` §2.1 and `docs/design/wbf-event-push.md`
//! §2. Each event is a big-endian u32 length followed by its JSON bytes, so a
//! receiver slices by length and never scans for separators.

use std::ops::Range;

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

/// Cuts a run of events into packs under both caps at once, the one rule
/// `Event/Batch` and `Event/Push` share: a pack ends when it holds
/// `count_max` events or when the next event would take it past `data_max`.
///
/// Args:
///     event_lens: each event's JSON length, in wire order, example: [7, 4096]
///     count_max: most events in one pack, example: 10 (0 is read as 1)
///     data_max: `wbf_data_max_bytes`, example: 16777216
/// Return:
///     Vec<Range<usize>>  one range per pack, in order, together covering
///     every event; empty when there are no events. An event that alone is
///     over `data_max` gets a pack of its own — callers drop those before
///     they get here.
#[must_use]
pub fn list_pack_ranges<I>(event_lens: I, count_max: usize, data_max: usize) -> Vec<Range<usize>>
where
	I: IntoIterator<Item = usize>,
{
	let count_max = count_max.max(1);
	let mut ranges = Vec::new();
	let mut start: usize = 0;
	let mut count: usize = 0;
	let mut data_len: usize = 0;

	for (position, event_len) in event_lens.into_iter().enumerate() {
		let framed = framed_len(event_len);
		let is_count_full = count >= count_max;
		let is_over_budget = count > 0 && data_len.saturating_add(framed) > data_max;
		if is_count_full || is_over_budget {
			ranges.push(start..position);
			start = position;
			count = 0;
			data_len = 0;
		}

		data_len = data_len.saturating_add(framed);
		count += 1;
	}
	if count > 0 {
		ranges.push(start..start + count);
	}

	ranges
}

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
	use super::{framed_len, length_prefixed, list_pack_ranges, split_length_prefixed};

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

	#[test]
	fn pack_ranges_cut_on_whichever_cap_comes_first() {
		assert!(list_pack_ranges([], 10, 1024).is_empty(), "no events, no packs");
		assert_eq!(list_pack_ranges([2, 2, 2, 2, 2], 2, 1024), vec![0..2, 2..4, 4..5], "the count cap");

		// Three events that fit two to a pack by bytes while the count cap
		// (10) is nowhere near: the byte cap has to be the one that cuts.
		let data_max = 2 * framed_len(100);
		assert_eq!(list_pack_ranges([100, 100, 100], 10, data_max), vec![0..2, 2..3]);
	}

	#[test]
	fn a_pack_range_holds_one_event_too_big_for_the_budget() {
		// Callers drop these before they get here; splitting an event across
		// packs is not on the wire, so it goes out alone rather than with a
		// neighbour.
		assert_eq!(list_pack_ranges([4, 9_000, 4], 10, 100), vec![0..1, 1..2, 2..3]);
	}
}
