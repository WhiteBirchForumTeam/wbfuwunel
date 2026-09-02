//! Media reference index.
//!
//! Answers one question: **does anything still hold a reference to this
//! media?** Rows are `mxc || holder kind || holder id` with an empty value, so
//! seeking one mxc prefix answers it regardless of what kind of thing holds the
//! reference. The rows are the count; nothing here stores a number.
//!
//! Writers that have a transaction take the caller's, which keeps a reference
//! row and the write that justifies it in one atomic batch. Reads are not
//! available inside a transaction, which is why a counter would not fit here at
//! all.
//!
//! Event references are read from event content, which redaction strips. That
//! is why an event's reference is removed while its unredacted content is still
//! in hand, and why rebuilding from stored events is correct: an event whose
//! content no longer names media has no reference to record.

#[cfg(test)]
mod tests;

use std::{pin::pin, sync::Arc};

use futures::StreamExt;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, UserId};
use tuwunel_core::{
	Result, error, implement,
	matrix::list_content_mxc_uris,
	utils::stream::{ReadyExt, TryIgnore},
};
use tuwunel_database::{Interfix, Map, Txn, serialize_key};

pub struct Service {
	db: Data,
}

struct Data {
	mxc_holder: Arc<Map>,
}

/// What kind of thing holds a media reference.
///
/// The discriminants are a stable on-disk format and must stay distinct.
/// Adding a kind here is how a new holder becomes visible to the index, and
/// **every kind of holder must be represented before any media may be deleted**
/// — an unrepresented holder reads as nobody.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
	/// A stored event whose content names the media.
	Event = 0x01,
	/// A local user's profile avatar.
	Avatar = 0x02,
}

impl From<Holder> for u8 {
	#[inline]
	fn from(holder: Holder) -> Self {
		match holder {
			| Holder::Event => 0x01,
			| Holder::Avatar => 0x02,
		}
	}
}

impl Holder {
	/// Names the holder kind for a stored discriminant, or `None` for a byte
	/// this build does not know.
	#[must_use]
	fn find_name(discriminant: u8) -> Option<&'static str> {
		match discriminant {
			| 0x01 => Some("event"),
			| 0x02 => Some("avatar"),
			| _ => None,
		}
	}
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data { mxc_holder: args.db["mxc_holder"].clone() },
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Records in `txn` that `event_id` references the media its content names.
///
/// Content naming no media writes nothing.
#[implement(Service)]
pub(crate) fn add_event_refs(
	&self,
	txn: &mut Txn,
	event_id: &EventId,
	event_json: &CanonicalJsonObject,
) {
	for mxc in self.list_event_mxc_uris(event_json) {
		txn.put_raw(
			&self.db.mxc_holder,
			(mxc.as_str(), u8::from(Holder::Event), event_id),
			[],
		);
	}
}

/// Removes in `txn` the reference rows `event_id` holds on `mxc_uris`.
///
/// The caller supplies the list because redaction strips the content it would
/// otherwise be read from. Take it with [`list_event_mxc_uris`] before the
/// event is stripped, and remove the rows only once the strip is stored: a row
/// outliving its event holds media that could have been released, while an
/// event outliving its row points at media that could be removed.
#[implement(Service)]
pub(crate) fn del_event_refs(&self, txn: &mut Txn, event_id: &EventId, mxc_uris: &[String]) {
	for mxc in mxc_uris {
		txn.del(
			&self.db.mxc_holder,
			(mxc.as_str(), u8::from(Holder::Event), event_id),
		);
	}
}

/// Lists the `mxc://` URIs named by one event's `content`.
///
/// Returns an empty vector for an event without a content object, which
/// includes every redacted event.
#[implement(Service)]
pub(crate) fn list_event_mxc_uris(&self, event_json: &CanonicalJsonObject) -> Vec<String> {
	event_json
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.map(list_content_mxc_uris)
		.unwrap_or_default()
}

/// Moves `user_id`'s avatar reference from `old_mxc` to `new_mxc` in `txn`.
///
/// Equal values write nothing, so a profile update leaving the avatar alone
/// cannot drop its own reference. Sharing the caller's transaction is what
/// keeps the reference and the profile it describes from disagreeing; ordering
/// them across two batches has no safe order, because one direction leaves a
/// profile pointing at media nothing holds.
#[implement(Service)]
pub fn set_avatar_ref(
	&self,
	txn: &mut Txn,
	user_id: &UserId,
	old_mxc: Option<&str>,
	new_mxc: Option<&str>,
) {
	if old_mxc == new_mxc {
		return;
	}

	if let Some(new_mxc) = new_mxc {
		txn.put_raw(
			&self.db.mxc_holder,
			(new_mxc, u8::from(Holder::Avatar), user_id),
			[],
		);
	}

	if let Some(old_mxc) = old_mxc {
		txn.del(
			&self.db.mxc_holder,
			(old_mxc, u8::from(Holder::Avatar), user_id),
		);
	}
}

/// Returns whether anything still references `mxc`.
///
/// A read error answers `true`. A caller uses this to decide whether media may
/// be removed, so an unreadable index must hold the media, not release it.
#[implement(Service)]
pub async fn is_mxc_referenced(&self, mxc: &str) -> bool {
	let prefix = (mxc, Interfix);
	let mut rows = pin!(self.db.mxc_holder.keys_prefix_raw(&prefix));

	match rows.next().await {
		| None => false,
		| Some(Ok(_)) => true,
		| Some(Err(e)) => {
			error!(?mxc, ?e, "Media reference index unreadable; treating media as referenced.");
			true
		},
	}
}

/// Lists what references `mxc`, each as `kind id`, empty when nothing does.
///
/// Unreadable rows, holder kinds this build does not know, and ids that are not
/// text are skipped, so this reports at most what the index holds. It is for
/// inspection; use [`is_mxc_referenced`] to decide whether media may be
/// removed.
#[implement(Service)]
pub async fn list_mxc_holders(&self, mxc: &str) -> Vec<String> {
	let prefix = (mxc, Interfix);

	// The key tail is read as bytes rather than deserialized: the record codec
	// serializes a `u8` but has no deserializer for one, and reaching for it
	// panics rather than failing.
	let Ok(prefix_len) = serialize_key(&prefix).map(|prefix| prefix.len()) else {
		error!(?mxc, "Media reference prefix unserializable; reporting no holders.");
		return Vec::new();
	};

	self.db
		.mxc_holder
		.keys_prefix_raw(&prefix)
		.ignore_err()
		.ready_filter_map(move |key| describe_holder(key, prefix_len))
		.collect()
		.await
}

/// Describes the holder a `mxc_holder` key names, as `kind id`.
///
/// The key tail after `prefix_len` is `kind || separator || id`. Returns `None`
/// for a tail that is too short, a kind this build does not know, or an id that
/// is not text.
fn describe_holder(key: &[u8], prefix_len: usize) -> Option<String> {
	let tail = key.get(prefix_len..)?;
	let (discriminant, id) = tail.split_first()?;
	let kind = Holder::find_name(*discriminant)?;
	let id = std::str::from_utf8(id.get(1..)?).ok()?;

	Some(format!("{kind} {id}"))
}
