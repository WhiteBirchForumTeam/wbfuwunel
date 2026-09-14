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
///     data_max: `wbf_data_max_bytes`, example: 2101248
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

/// Where a window stops: the one rule `Event/Recent`, `Device/Fetch` and the
/// catch-up of both subscriptions share. A window is gathered whole before
/// its first pack goes out, so what it may hold is what one request may cost
/// in memory — and that is counted in bytes, not only in events: 500 events
/// of a pack's width each would be a gigabyte.
///
/// 🚨 **Bytes are asked first.** A window full by bytes stops even when it
/// holds fewer than `count_max` events, and that is exactly the case a client
/// must be told about, because "fewer than I asked for" used to mean "there
/// are no more". Whoever uses this reports `is_cut_short` on the wire.
///
/// ⚠️ The first event always joins, whatever its size: a window that could
/// refuse its first event would answer every later request with the same
/// empty window, and the client would page forever or give up with events
/// still ahead of it. The configuration keeps that event inside the budget
/// anyway (`wbf_window_max_bytes` is at least `wbf_data_max_bytes`).
///
/// 🚨 **Once it refuses, it refuses everything after.** The event it refused
/// is the next one the client pages to; a smaller event slipping in behind it
/// would put a hole in the window that the cursor has already walked past.
// Not `Copy` on purpose: a copy is a second counter that has not seen what
// the first one admitted, and a window checked against it would overflow.
#[expect(missing_copy_implementations)]
#[derive(Debug)]
pub struct WindowBudget {
	count_max: usize,
	bytes_max: usize,
	count: usize,
	bytes: usize,
	is_closed: bool,
}

impl WindowBudget {
	/// Args:
	///     count_max: the request's `limit`, example: 320
	///     bytes_max: `wbf_window_max_bytes`, example: 8388608
	#[must_use]
	pub const fn new(count_max: usize, bytes_max: usize) -> Self { Self { count_max, bytes_max, count: 0, bytes: 0, is_closed: false } }

	/// Args:
	///     event_len: the event's JSON length, example: 1200
	/// Return:
	///     bool  true when the event joins the window (and is counted); false
	///     when the window is full by bytes or by count, and then nothing is
	///     counted and every later call is false too.
	pub fn try_admit(&mut self, event_len: usize) -> bool {
		let framed = framed_len(event_len);
		let is_over_bytes = self.count > 0 && self.bytes.saturating_add(framed) > self.bytes_max;
		if self.is_closed || is_over_bytes || self.is_count_full() {
			self.is_closed = true;
			return false;
		}

		self.bytes = self.bytes.saturating_add(framed);
		self.count = self.count.saturating_add(1);
		true
	}

	/// Return:
	///     bool  true once `count_max` events have joined (at once for a
	///     `count_max` of 0).
	#[must_use]
	pub const fn is_count_full(&self) -> bool { self.count >= self.count_max }

	/// Return:
	///     bool  true when the window stopped at one of its caps, so there may
	///     be more behind it (the wire's `more`); false when it has not been
	///     refused and is not full, which, once the caller has run out of
	///     events, means it ran out before the caps did. ⚠️ Always false for
	///     a `count_max` of 0: nothing was asked for, so nothing was cut.
	#[must_use]
	pub const fn is_cut_short(&self) -> bool {
		// 🚨 `limit: 0` means "none" (PR #43) and is full before it starts.
		// Reporting that as cut short told the client to ask again for the
		// nothing it had just been given, and made a catch-up configured to
		// zero mark a `gap` that the next unrelated live push carried (PR #53
		// review, rumia).
		if self.count_max == 0 {
			return false;
		}

		self.is_closed || self.is_count_full()
	}
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
	use super::{WindowBudget, framed_len, length_prefixed, list_pack_ranges, split_length_prefixed};

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

	#[test]
	fn a_window_full_by_bytes_stops_before_its_count() {
		// Room for two events by bytes, twenty by count: the third is refused
		// although the count is nowhere near, and so is anything after it.
		let mut budget = WindowBudget::new(20, 2 * framed_len(100));
		assert!(budget.try_admit(100));
		assert!(budget.try_admit(100));
		assert!(!budget.try_admit(100), "bytes are asked first");
		assert!(!budget.is_count_full(), "and the count was not what stopped it");
		assert!(budget.is_cut_short(), "so the client must be told there may be more");
	}

	#[test]
	fn a_refused_window_stays_refused_even_for_an_event_that_would_fit() {
		// 4 bytes of room are left after the first event; the 100-byte one is
		// refused, and the 0-byte one behind it must not slip into the hole.
		let mut budget = WindowBudget::new(20, framed_len(100) + framed_len(0));
		assert!(budget.try_admit(100));
		assert!(!budget.try_admit(100));
		assert!(!budget.try_admit(0), "the cursor is already past the refused event");
	}

	#[test]
	fn a_window_full_by_count_refuses_even_a_tiny_event() {
		let mut budget = WindowBudget::new(2, 1 << 20);
		assert!(budget.try_admit(1));
		assert!(budget.try_admit(1));
		assert!(budget.is_count_full());
		assert!(!budget.try_admit(1));
		assert!(budget.is_cut_short());
	}

	#[test]
	fn a_window_that_ran_out_of_events_is_not_cut_short() {
		let mut budget = WindowBudget::new(10, 1 << 20);
		assert!(!budget.is_cut_short(), "nothing asked for yet");
		assert!(budget.try_admit(1));
		assert!(budget.try_admit(1));
		assert!(!budget.is_cut_short(), "two of ten, both caps far away");
	}

	#[test]
	fn the_first_event_joins_whatever_its_size() {
		// Refusing it would give every later request the same empty window.
		let mut budget = WindowBudget::new(10, 100);
		assert!(budget.try_admit(9_000));
		assert!(!budget.try_admit(1), "but nothing joins after it");
	}

	#[test]
	fn a_count_of_zero_is_full_before_anything_joins() {
		let mut budget = WindowBudget::new(0, 1 << 20);
		assert!(budget.is_count_full());
		assert!(!budget.try_admit(1));
		assert!(
			!budget.is_cut_short(),
			"asking for none is answered, not cut short: `more: true` would send the client back for nothing, \
			 and a catch-up would mark a gap the next live push carries"
		);
	}
}
