//! Media reference counting, and the collector and sweep that act on it.
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
//! counted and never collected.
//!
//! **Where an event's references come from.** Plaintext content names its
//! media (`url`, `file.url`, thumbnails) and the server reads it. Encrypted
//! content names nothing the server can see, so the sender declares the
//! media with the send (`attachments`), and the server checks the claim
//! before counting it. The union of the two is written to `eventid_mxcs`
//! with the event, and that row, not the content, is what a later release
//! reads: redaction strips content, and a declared reference was never in
//! it. Releasing removes the row, so nothing can release twice. An event
//! without a row (stored before this existed) falls back to its content.
//!
//! A redacted event keeps its unredacted original for the retention period,
//! and the reference is released when that original is dropped, not when the
//! event is stripped, so media outlives the redacted message exactly as long
//! as the message's original does.
//!
//! Every release hands the media it released to the collector once the
//! releasing transaction has committed. The collector reads the count back
//! and removes local media whose count is `MIN < count <= 0`. Media whose
//! count never left zero (uploaded, never attached to anything the server
//! was told about) is removed by the periodic sweep once it is older than
//! the protection period; see `collect.rs`.
//!
//! One lock per media closes the window between a reader seeing a count of
//! zero and removing the bytes: whoever adds a reference holds the media's
//! lock from before the increment until it has committed, and the collector
//! and the sweep hold it from the read through the removal.

pub mod attachments;
mod collect;
#[cfg(test)]
mod tests;

use std::sync::{Arc, RwLock as StdRwLock};

use async_trait::async_trait;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, EventId, UserId};
use tokio::sync::mpsc;
use tuwunel_core::{
	Result, debug, error, implement,
	matrix::list_content_mxc_uris,
	utils::{MutexMap, MutexMapGuard},
};
use tuwunel_database::{COUNTER_SENTINEL, CounterOperand, Json, Map, Txn, decode_counter};

pub use self::attachments::AttachmentError;

/// Holds one media's lock; dropping it releases the media.
pub type MediaHold = MutexMapGuard<String, ()>;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
	/// One lock per media, shared by whoever adds a reference and the
	/// collector or sweep deciding whether to remove it.
	mxc_locks: MutexMap<String, ()>,
	/// Where a release sends the media it released, once the releasing
	/// transaction has committed. Absent while the collector is not running.
	released: StdRwLock<Option<mpsc::UnboundedSender<String>>>,
}

struct Data {
	mxc_refcount: Arc<Map>,
	/// `event_id → JSON [mxc]`, see the module documentation.
	eventid_mxcs: Arc<Map>,
	/// `user_id → ()`: told once about undeclared attachments.
	userid_attachmentwarned: Arc<Map>,
	/// `user_id → u64 millis`: last upload through the legacy endpoints.
	userid_lastlegacyupload: Arc<Map>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Data {
				mxc_refcount: args.db["mxc_refcount"].clone(),
				eventid_mxcs: args.db["eventid_mxcs"].clone(),
				userid_attachmentwarned: args.db["userid_attachmentwarned"].clone(),
				userid_lastlegacyupload: args.db["userid_lastlegacyupload"].clone(),
			},
			mxc_locks: MutexMap::new(),
			released: StdRwLock::new(None),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let (sender, mut receiver) = mpsc::unbounded_channel();
		_ = self
			.released
			.write()
			.expect("locked for writing")
			.insert(sender);

		let mut sweep = self.sweep_interval();
		loop {
			tokio::select! {
				released = receiver.recv() => match released {
					| Some(mxc) => self.collect(&mxc).await,
					| None => break,
				},
				_ = sweep.tick() => self.sweep_unreferenced().await,
				() = self.services.server.until_shutdown() => break,
			}
		}

		Ok(())
	}

	async fn interrupt(&self) {
		_ = self
			.released
			.write()
			.expect("locked for writing")
			.take();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Lists every media an event being stored references: what the sender
/// declared plus what its plaintext content names, deduplicated and sorted.
///
/// Args:
///     event_json: the event as it will be stored, example: an m.room.encrypted
///     declared: the checked `attachments`, example: ["mxc://localhost/abc"]
/// Return:
///     Vec<String>  empty for an event referencing no media.
#[must_use]
pub fn list_event_refs_at_store(event_json: &CanonicalJsonObject, declared: &[String]) -> Vec<String> {
	let mut mxc_uris = list_event_mxc_uris_in(event_json);
	mxc_uris.extend(declared.iter().cloned());
	mxc_uris.sort_unstable();
	mxc_uris.dedup();
	mxc_uris
}

/// Locks every media in `mxc_uris`, for the caller to hold until the
/// transaction counting them has committed.
///
/// Taken in sorted order without repeats (`list_event_refs_at_store` gives
/// that), so two events naming the same media in different orders cannot
/// wait on each other.
#[implement(Service)]
pub async fn hold_media_list(&self, mxc_uris: &[String]) -> Vec<MediaHold> {
	let mut holds = Vec::with_capacity(mxc_uris.len());
	for mxc in mxc_uris {
		holds.push(self.hold_media(mxc).await);
	}

	holds
}

/// Locks one media, for the caller to hold until its reference change has
/// committed, or until its removal is done.
#[implement(Service)]
pub async fn hold_media(&self, mxc: &str) -> MediaHold { self.mxc_locks.lock(mxc).await }

/// Counts, in `txn`, one reference to each media in `mxc_uris` and records
/// the list under `event_id`, so the release later reads the same list.
///
/// An empty list writes nothing. The caller holds the media
/// (`hold_media_list`) until `txn` has committed.
#[implement(Service)]
pub(crate) fn add_event_refs(&self, txn: &mut Txn, event_id: &EventId, mxc_uris: &[String]) {
	if mxc_uris.is_empty() {
		return;
	}

	for mxc in mxc_uris {
		txn.merge(&self.db.mxc_refcount, mxc.as_str(), CounterOperand::Add(1).to_bytes());
	}

	txn.raw_put(&self.db.eventid_mxcs, event_id, Json(mxc_uris));
}

/// Lists the media an already stored event references, for releasing them:
/// the row written when it was stored, or, for an event from before the row
/// existed, what its content names now.
///
/// Args:
///     event_id: example: $abc:localhost
///     event_json: the stored event, example: the value of `pduid_pdu`
/// Return:
///     Vec<String>  empty when nothing is referenced or the row is unreadable
///     (logged; media held one count too long is recoverable, released one
///     count too early is not).
#[implement(Service)]
pub async fn list_event_refs(&self, event_id: &EventId, event_json: &CanonicalJsonObject) -> Vec<String> {
	match self.db.eventid_mxcs.get(event_id).await {
		| Ok(row) => match serde_json::from_slice::<Vec<String>>(&row) {
			| Ok(mxc_uris) => mxc_uris,
			| Err(error) => {
				error!(?event_id, ?error, "eventid_mxcs row unreadable; its references stay held.");
				Vec::new()
			},
		},
		| Err(error) if error.is_not_found() => list_event_mxc_uris_in(event_json),
		| Err(error) => {
			error!(?event_id, ?error, "eventid_mxcs unreadable; the event's references stay held.");
			Vec::new()
		},
	}
}

/// Releases, in `txn`, one reference to each of `mxc_uris`, removes the
/// event's row so nothing releases them again, and hands them to the
/// collector once `txn` commits.
///
/// The caller supplies the list (`list_event_refs`) because redaction strips
/// the content it would otherwise be read from. Release only once the event,
/// or its retained original, is truly gone: a count that stays high holds
/// media that could be released, while a count that drops early releases
/// media something still points at.
///
/// A count never goes below zero when every release pairs with a count:
/// a negative count read back is a caller releasing what it never counted, or
/// releasing the same event twice, and is the bug to find.
#[implement(Service)]
pub(crate) fn del_event_refs(&self, txn: &mut Txn, event_id: &EventId, mxc_uris: &[String]) {
	txn.del_raw(&self.db.eventid_mxcs, event_id);

	for mxc in mxc_uris {
		txn.merge(&self.db.mxc_refcount, mxc.as_str(), CounterOperand::Add(-1).to_bytes());
	}

	self.hand_to_collector(txn, mxc_uris.to_vec());
}

/// Lists the `mxc://` URIs named by one event's plaintext `content`.
///
/// Returns an empty vector for an event without a content object, which
/// includes every redacted event and every encrypted one.
fn list_event_mxc_uris_in(event_json: &CanonicalJsonObject) -> Vec<String> {
	event_json
		.get("content")
		.and_then(CanonicalJsonValue::as_object)
		.map(list_content_mxc_uris)
		.unwrap_or_default()
}

/// Moves `user_id`'s avatar reference from `old_mxc` to `new_mxc` in `txn`,
/// and hands the old one to the collector once `txn` commits.
///
/// Equal values write nothing, so a profile update leaving the avatar alone
/// cannot release its own reference. Sharing the caller's transaction is what
/// keeps the count and the profile it describes from disagreeing. The caller
/// holds `new_mxc` (`hold_media`) until `txn` has committed.
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
		self.hand_to_collector(txn, vec![old_mxc.to_owned()]);
	}
}

/// Arranges for `mxc_uris` to reach the collector after `txn` commits.
///
/// Registered on the transaction rather than sent now, so the collector can
/// never read a count the release has not yet been applied to. With no
/// collector running (startup, shutdown) nothing is sent: the sweep finds
/// what was missed.
#[implement(Service)]
fn hand_to_collector(&self, txn: &mut Txn, mxc_uris: Vec<String>) {
	if mxc_uris.is_empty() {
		return;
	}

	let sender = self
		.released
		.read()
		.expect("locked for reading")
		.clone();

	let Some(sender) = sender else {
		debug!(?mxc_uris, "No collector running; released media waits for the sweep.");
		return;
	};

	txn.on_execute(move || {
		for mxc in mxc_uris {
			// A closed receiver means the collector is shutting down; the
			// sweep covers what it drops.
			_ = sender.send(mxc);
		}
	});
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
