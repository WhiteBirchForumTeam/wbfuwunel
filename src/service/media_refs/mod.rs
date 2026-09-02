//! Media reference counting.
//!
//! Answers one question exactly: **how many things still hold a reference to
//! this media?** The count lives in `mxc_refcount` as a signed 64-bit value
//! folded by the counter merge operator, so an increment is a pure write that
//! shares the transaction of the event or profile change that justifies it.
//! Nothing here reads inside a transaction, because a write batch cannot.
//!
//! A row that does not exist is media created before the counter did. The
//! first operand such a row receives turns it into the sentinel, which every
//! later operand leaves alone, so media the counter never saw created is never
//! counted and never collected until an operator rebuilds the counts.
//!
//! Event references are read from event content, which redaction strips. A
//! redacted event keeps its unredacted original for the retention period, and
//! the reference is released when that original is dropped, not when the
//! event is stripped, so media outlives the redacted message exactly as long
//! as the message's original does.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, UserId};
use tuwunel_core::{Result, error, implement, matrix::list_content_mxc_uris};
use tuwunel_database::{COUNTER_SENTINEL, CounterOperand, Map, Txn, decode_counter};

pub struct Service {
	db: Data,
}

struct Data {
	mxc_refcount: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data { mxc_refcount: args.db["mxc_refcount"].clone() },
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Counts, in `txn`, one reference from `event_id` to each media its content
/// names.
///
/// Content naming no media writes nothing.
#[implement(Service)]
pub(crate) fn add_event_refs(
	&self,
	txn: &mut Txn,
	_event_id: &EventId,
	event_json: &CanonicalJsonObject,
) {
	for mxc in self.list_event_mxc_uris(event_json) {
		txn.merge(&self.db.mxc_refcount, mxc.as_str(), CounterOperand::Add(1).to_bytes());
	}
}

/// Releases, in `txn`, one reference from `event_id` to each of `mxc_uris`.
///
/// The caller supplies the list because redaction strips the content it would
/// otherwise be read from. Release only once the event, or its retained
/// original, is truly gone: a count that stays high holds media that could be
/// released, while a count that drops early releases media something still
/// points at.
///
/// A count never goes below zero when every release pairs with a count:
/// a negative count read back is a caller releasing what it never counted, or
/// releasing the same event twice, and is the bug to find.
#[implement(Service)]
pub(crate) fn del_event_refs(&self, txn: &mut Txn, _event_id: &EventId, mxc_uris: &[String]) {
	for mxc in mxc_uris {
		txn.merge(&self.db.mxc_refcount, mxc.as_str(), CounterOperand::Add(-1).to_bytes());
	}
}

/// Lists the `mxc://` URIs named by one event's `content`.
///
/// Returns an empty vector for an event without a content object, which
/// includes every redacted event.
#[implement(Service)]
pub(crate) fn list_event_mxc_uris(&self, event_json: &CanonicalJsonObject) -> Vec<String> {
	list_event_mxc_uris_in(event_json)
}

/// Lists the `mxc://` URIs named by one event's `content`.
///
/// Returns an empty vector for an event without a content object, which
/// includes every redacted event.
fn list_event_mxc_uris_in(event_json: &CanonicalJsonObject) -> Vec<String> {
	event_json
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.map(list_content_mxc_uris)
		.unwrap_or_default()
}

/// Moves `user_id`'s avatar reference from `old_mxc` to `new_mxc` in `txn`.
///
/// Equal values write nothing, so a profile update leaving the avatar alone
/// cannot release its own reference. Sharing the caller's transaction is what
/// keeps the count and the profile it describes from disagreeing.
#[implement(Service)]
pub fn set_avatar_ref(
	&self,
	txn: &mut Txn,
	_user_id: &UserId,
	old_mxc: Option<&str>,
	new_mxc: Option<&str>,
) {
	if old_mxc == new_mxc {
		return;
	}

	if let Some(new_mxc) = new_mxc {
		txn.merge(&self.db.mxc_refcount, new_mxc, CounterOperand::Add(1).to_bytes());
	}

	if let Some(old_mxc) = old_mxc {
		txn.merge(&self.db.mxc_refcount, old_mxc, CounterOperand::Add(-1).to_bytes());
	}
}

/// Reads the reference count of `mxc`.
///
/// `Ok(None)` is media the counter never saw created and that nothing has
/// touched since; `Ok(Some(COUNTER_SENTINEL))` is such media after something
/// touched it. Both mean "unknown", never "zero": a caller that treats
/// either as zero releases media it knows nothing about.
#[implement(Service)]
pub async fn refcount(&self, mxc: &str) -> Result<Option<i64>> {
	match self.db.mxc_refcount.get(mxc).await {
		| Ok(handle) => Ok(decode_counter(&handle)),
		| Err(e) if e.is_not_found() => Ok(None),
		| Err(e) => Err(e),
	}
}

/// Returns whether media may still be in use.
///
/// Unknown counts and read errors answer `true`: a caller uses this to decide
/// whether media may be removed, so uncertainty must hold the media, not
/// release it.
#[implement(Service)]
pub async fn is_mxc_referenced(&self, mxc: &str) -> bool {
	match self.refcount(mxc).await {
		| Ok(Some(count)) => count > 0 || count == COUNTER_SENTINEL,
		| Ok(None) => true,
		| Err(e) => {
			error!(?mxc, ?e, "Media reference count unreadable; treating media as referenced.");
			true
		},
	}
}
