//! Per-client-IP token buckets: one table, one question ("may this address
//! do one more right now?"), shared by every throttle that keys on the peer
//! address (OIDC endpoints, login over HTTP and over the wbf channel).

use std::{
	collections::HashMap,
	net::IpAddr,
	sync::Mutex,
	time::{Duration, Instant},
};

use http::StatusCode;
use ruma::api::error::{ErrorKind, LimitExceededErrorData, RetryAfter};

use crate::Error;

/// The Matrix error a refused attempt turns into: 429 `M_LIMIT_EXCEEDED`
/// carrying how long the client should wait.
///
/// Args:
///     message: example: "Too many login attempts."
///     retry_after: from `IpTokenBuckets::take`, example: 700 ms
/// Return:
///     Error  status 429 with `retry_after_ms` set.
#[must_use]
pub fn limit_exceeded(message: &'static str, retry_after: Duration) -> Error {
	Error::Request(
		ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: Some(RetryAfter::Delay(retry_after)) }),
		message.into(),
		StatusCode::TOO_MANY_REQUESTS,
	)
}

/// Cap on the table; fully refilled buckets are pruned once it is reached, so
/// a spray of source addresses cannot grow it without bound.
const TABLE_CAP: usize = 1 << 16;

/// Last refill instant and tokens left, per client address.
pub struct IpTokenBuckets {
	buckets: Mutex<HashMap<IpAddr, (Instant, f64)>>,
}

impl Default for IpTokenBuckets {
	fn default() -> Self { Self::new() }
}

impl IpTokenBuckets {
	/// An empty table: every address starts with a full bucket.
	#[must_use]
	pub fn new() -> Self { Self { buckets: Mutex::new(HashMap::new()) } }

	/// Takes one token from `client`'s bucket.
	///
	/// Args:
	///     client: the peer address, example: 203.0.113.7
	///     rate_per_second: refill rate, example: 1.0
	///     burst: bucket depth, example: 10.0
	/// Return:
	///     Result<(), Duration>  Ok when a token was taken, or when the
	///     throttle is disabled (rate or burst not positive); Err with how long
	///     until one token is back when the bucket is empty.
	pub fn take(&self, client: IpAddr, rate_per_second: f64, burst: f64) -> Result<(), Duration> {
		if rate_per_second <= 0.0 || burst <= 0.0 {
			return Ok(());
		}

		let now = Instant::now();
		// A poisoned lock only means a holder panicked between two plain
		// arithmetic writes; the numbers inside are still usable, and refusing
		// every login forever would be the worse failure.
		let mut buckets = self
			.buckets
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);

		// A fully refilled bucket equals an absent one.
		if buckets.len() >= TABLE_CAP {
			buckets.retain(|_, (last, tokens)| {
				now.duration_since(*last)
					.as_secs_f64()
					.mul_add(rate_per_second, *tokens)
					< burst
			});
		}

		let (last, tokens) = buckets.entry(client).or_insert((now, burst));
		let refilled = now
			.duration_since(*last)
			.as_secs_f64()
			.mul_add(rate_per_second, *tokens)
			.min(burst);

		if refilled < 1.0 {
			let wait_seconds = (1.0 - refilled) / rate_per_second;
			return Err(Duration::from_secs_f64(wait_seconds));
		}

		*last = now;
		*tokens = refilled - 1.0;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::net::{IpAddr, Ipv4Addr};

	use super::IpTokenBuckets;

	const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
	const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));

	#[test]
	fn a_burst_is_allowed_and_the_next_one_waits() {
		let buckets = IpTokenBuckets::new();
		assert!(buckets.take(CLIENT, 1.0, 2.0).is_ok());
		assert!(buckets.take(CLIENT, 1.0, 2.0).is_ok());
		let wait = buckets.take(CLIENT, 1.0, 2.0).expect_err("bucket is empty");
		// One token comes back in about a second at one per second.
		assert!(wait.as_secs_f64() > 0.9 && wait.as_secs_f64() <= 1.0, "{wait:?}");
	}

	#[test]
	fn addresses_do_not_share_a_bucket() {
		let buckets = IpTokenBuckets::new();
		assert!(buckets.take(CLIENT, 1.0, 1.0).is_ok());
		assert!(buckets.take(CLIENT, 1.0, 1.0).is_err());
		assert!(buckets.take(OTHER, 1.0, 1.0).is_ok());
	}

	#[test]
	fn zero_rate_or_burst_disables_the_throttle() {
		let buckets = IpTokenBuckets::new();
		for _ in 0..100 {
			assert!(buckets.take(CLIENT, 0.0, 10.0).is_ok());
			assert!(buckets.take(CLIENT, 1.0, 0.0).is_ok());
		}
	}
}
