//! Media holders: who keeps a media item alive, and the collector and sweep
//! that remove what nobody holds.
//!
//! A media item is not counted, it is **held**: a set of foreign keys, one per
//! thing that references it (`Holder`: an event by room and `g_seq`, the
//! retained original of a redacted event, a local user's avatar). Adding and
//! removing holders are set operations, so doing either twice is a no-op;
//! there is no "exactly once" to get right, which is what sank the counter
//! this replaces (`docs/design/media-holders.md` §1).
//!
//! This module is the only writer of the five tables (`mxc_holder`,
//! `holder_mxc`, `room_mxc`, `mxc_room`, `mxc_managed`). Every path that makes or
//! breaks a reference calls one of the entry points below inside its own
//! transaction; the list of those paths is `media-holders.md` §4.
//!
//! Where an event's references come from: plaintext content names its media
//! and the server reads it; encrypted content names nothing the server can
//! see, so the sender declares the media with the send (`attachments`,
//! checked in `attachments.rs`). The union is what the event holds.
//!
//! Removal: whenever a holder is removed, the media is handed to the
//! collector once the transaction has committed; the collector removes it if
//! nothing holds it any more. Media nothing ever held is removed by the sweep
//! after the protection period. Both decide under the media's lock, and both
//! only touch media with a `mxc_managed` row: media from before this existed
//! is never removed automatically.

pub mod attachments;
mod collect;
pub mod holder;
#[cfg(test)]
mod tests;

use std::sync::{Arc, RwLock as StdRwLock};

use async_trait::async_trait;
use futures::StreamExt;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, OwnedRoomId, RoomId, UserId};
use tokio::sync::mpsc;
use tuwunel_core::{
	Result, debug, implement,
	matrix::list_content_mxc_uris,
	utils::{MutexMap, MutexMapGuard, stream::TryIgnore},
};
use tuwunel_database::{Ignore, Map, Txn};

pub use self::{
	attachments::AttachmentError,
	holder::{Holder, KIND_AVATAR, KIND_BACKUP, KIND_EVENT, bias_g_seq, unbias_g_seq},
};

/// Holds one media's lock; dropping it releases the media.
pub type MediaHold = MutexMapGuard<String, ()>;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Data,
	/// One lock per media, shared by whoever adds a holder and the collector
	/// or sweep deciding whether to remove it.
	mxc_locks: MutexMap<String, ()>,
	/// Where a release sends the media it released, once the releasing
	/// transaction has committed. Absent while the collector is not running.
	released: StdRwLock<Option<mpsc::UnboundedSender<String>>>,
}

struct Data {
	/// `mxc ‖ kind ‖ id → ()`: who holds M. Empty prefix = nobody.
	mxc_holder: Arc<Map>,
	/// `kind ‖ room ‖ g_seq ‖ mxc → ()` (avatars: `a ‖ localpart ‖ mxc`): what a holder holds.
	holder_mxc: Arc<Map>,
	/// `room ‖ mxc → ()`: every media a room holds or held; the room-deletion accelerator.
	room_mxc: Arc<Map>,
	/// `mxc ‖ room → ()`: reverse of `room_mxc`, read when the media is removed.
	mxc_room: Arc<Map>,
	/// `mxc → created millis`: media this model manages. No row, never removed.
	mxc_managed: Arc<Map>,
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
				mxc_holder: args.db["mxc_holder"].clone(),
				holder_mxc: args.db["holder_mxc"].clone(),
				room_mxc: args.db["room_mxc"].clone(),
				mxc_room: args.db["mxc_room"].clone(),
				mxc_managed: args.db["mxc_managed"].clone(),
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

/// Locks every media in `mxc_uris`, for the caller to hold until the
/// transaction adding their holder has committed.
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

/// Locks one media, for the caller to hold until its holder change has
/// committed, or until its removal is done.
#[implement(Service)]
pub async fn hold_media(&self, mxc: &str) -> MediaHold { self.mxc_locks.lock(mxc).await }

/// Adds `holder` to `mxc`, in `txn`. Adding a holder that is already there
/// changes nothing. The caller holds the media (`hold_media_list`) until
/// `txn` has committed.
#[implement(Service)]
pub fn hold(&self, txn: &mut Txn, mxc: &str, holder: &Holder) {
	txn.insert_raw(&self.db.mxc_holder, holder.mxc_holder_key(mxc), []);
	txn.insert_raw(&self.db.holder_mxc, holder.holder_mxc_key(mxc), []);
	if let Some(room) = holder.room() {
		txn.put_raw(&self.db.room_mxc, (room, mxc), []);
		txn.put_raw(&self.db.mxc_room, (mxc, room), []);
	}
}

/// Removes `holder` from `mxc`, in `txn`, and hands the media to the
/// collector once `txn` commits. Removing a holder that is not there
/// changes nothing (the collector then finds the media still held, or
/// already gone).
#[implement(Service)]
pub fn release(&self, txn: &mut Txn, mxc: &str, holder: &Holder) {
	self.del_holder_rows(txn, mxc, holder);
	self.hand_to_collector(txn, vec![mxc.to_owned()]);
}

/// Replaces `from` by `to` on `mxc` in one transaction; the media is never
/// unheld in between, so nothing is handed to the collector.
#[implement(Service)]
pub fn swap(&self, txn: &mut Txn, mxc: &str, from: &Holder, to: &Holder) {
	self.del_holder_rows(txn, mxc, from);
	self.hold(txn, mxc, to);
}

#[implement(Service)]
fn del_holder_rows(&self, txn: &mut Txn, mxc: &str, holder: &Holder) {
	txn.del_raw(&self.db.mxc_holder, holder.mxc_holder_key(mxc));
	txn.del_raw(&self.db.holder_mxc, holder.holder_mxc_key(mxc));
	// `room_mxc`/`mxc_room` are left in place on purpose: knowing whether
	// anything of the room still holds the media would need a scan here.
	// They go when the room is deleted or when the media is removed
	// (`forget_media`), so they are bounded by live media times rooms.
}

/// Drops every `room_mxc`/`mxc_room` row of `mxc`, once the media itself is
/// gone. Idempotent; called by the collector and the sweep after a removal.
#[implement(Service)]
pub(super) async fn forget_media(&self, mxc: &str) {
	let rooms: Vec<OwnedRoomId> = self
		.db
		.mxc_room
		.keys_prefix(&(mxc,))
		.ignore_err()
		.filter_map(|(_, room): (Ignore, &str)| {
			let parsed = RoomId::parse(room).ok();
			async move { parsed }
		})
		.collect()
		.await;
	if rooms.is_empty() {
		return;
	}

	let mut txn = self.services.db.txn();
	for room in &rooms {
		txn.del(&self.db.room_mxc, (room, mxc));
		txn.del(&self.db.mxc_room, (mxc, room));
	}
	txn.execute();
}

/// Lists the media `holder` holds, from the reverse index.
///
/// Args:
///     holder: example: `Holder::event(room, 4711)`
/// Return:
///     Vec<String>  the mxc URIs; empty when the holder holds nothing.
#[implement(Service)]
pub async fn list_mxcs_of(&self, holder: &Holder) -> Vec<String> {
	match holder {
		| Holder::Event { room, g_seq } | Holder::Backup { room, g_seq } => {
			let prefix = (holder.kind(), room.as_str(), bias_g_seq(*g_seq));
			self.db
				.holder_mxc
				.keys_prefix(&prefix)
				.ignore_err()
				.map(|(_, _, _, mxc): (Ignore, Ignore, Ignore, &str)| mxc.to_owned())
				.collect()
				.await
		},
		| Holder::Avatar { localpart } => {
			let prefix = (holder.kind(), localpart.as_str());
			self.db
				.holder_mxc
				.keys_prefix(&prefix)
				.ignore_err()
				.map(|(_, _, mxc): (Ignore, Ignore, &str)| mxc.to_owned())
				.collect()
				.await
		},
	}
}

/// Removes `holder` from everything it holds, in `txn`; the media are handed
/// to the collector once `txn` commits.
///
/// Return:
///     Vec<String>  the media it held (for the caller's log).
#[implement(Service)]
pub async fn release_all_of(&self, txn: &mut Txn, holder: &Holder) -> Vec<String> {
	let mxc_uris = self.list_mxcs_of(holder).await;
	for mxc in &mxc_uris {
		self.del_holder_rows(txn, mxc, holder);
	}
	self.hand_to_collector(txn, mxc_uris.clone());
	mxc_uris
}

/// Replaces `from` by `to` on everything `from` holds, in `txn`. Used when a
/// redacted event's retained original takes over its media.
#[implement(Service)]
pub async fn swap_all_of(&self, txn: &mut Txn, from: &Holder, to: &Holder) -> usize {
	let mxc_uris = self.list_mxcs_of(from).await;
	for mxc in &mxc_uris {
		self.swap(txn, mxc, from, to);
	}
	mxc_uris.len()
}

/// Removes every `Event` and `Backup` holder of `room` with `g_seq < until`,
/// in `txn`: what `purge_history` purges. Reads the reverse index, never the
/// events. The media are handed to the collector once `txn` commits.
///
/// Return:
///     usize  holders removed.
#[implement(Service)]
pub async fn release_range(&self, txn: &mut Txn, room: &RoomId, until_g_seq: i64) -> usize {
	let until = bias_g_seq(until_g_seq);
	let mut released: Vec<String> = Vec::new();
	let mut removed: usize = 0;

	for kind in [KIND_EVENT, KIND_BACKUP] {
		let prefix = (kind, room.as_str());
		// Keys are ordered by g_seq, so the scan stops at the boundary rather
		// than walking the whole room.
		let mut rows: Vec<(u64, String)> = Vec::new();
		{
			let keys = self
				.db
				.holder_mxc
				.keys_prefix::<(Ignore, Ignore, u64, &str), _>(&prefix)
				.ignore_err();
			futures::pin_mut!(keys);
			while let Some((_, _, biased, mxc)) = keys.next().await {
				if biased >= until {
					break;
				}
				rows.push((biased, mxc.to_owned()));
			}
		}

		for (biased, mxc) in rows {
			let g_seq = unbias_g_seq(biased);
			let holder = if kind == KIND_EVENT { Holder::event(room, g_seq) } else { Holder::backup(room, g_seq) };
			self.del_holder_rows(txn, &mxc, &holder);
			removed = removed.saturating_add(1);
			released.push(mxc);
		}
	}

	released.sort_unstable();
	released.dedup();
	self.hand_to_collector(txn, released);
	removed
}

/// Removes every holder `room` ever had on any media, in `txn`: room
/// deletion. Walks `room_mxc` (one row per media the room ever held) and,
/// for each media, its holders of this room; never the events. The media
/// are handed to the collector once `txn` commits.
///
/// Return:
///     usize  media the room held.
#[implement(Service)]
pub async fn release_room(&self, txn: &mut Txn, room: &RoomId) -> usize {
	let media: Vec<String> = self
		.db
		.room_mxc
		.keys_prefix(&(room.as_str(),))
		.ignore_err()
		.map(|(_, mxc): (Ignore, &str)| mxc.to_owned())
		.collect()
		.await;

	for mxc in &media {
		for kind in [KIND_EVENT, KIND_BACKUP] {
			let prefix = (mxc.as_str(), kind, room.as_str());
			let positions: Vec<u64> = self
				.db
				.mxc_holder
				.keys_prefix(&prefix)
				.ignore_err()
				.map(|(_, _, _, biased): (Ignore, Ignore, Ignore, u64)| biased)
				.collect()
				.await;
			for biased in positions {
				let g_seq = unbias_g_seq(biased);
				let holder = if kind == KIND_EVENT { Holder::event(room, g_seq) } else { Holder::backup(room, g_seq) };
				self.del_holder_rows(txn, mxc, &holder);
			}
		}
		txn.del(&self.db.room_mxc, (room, mxc.as_str()));
		txn.del(&self.db.mxc_room, (mxc.as_str(), room));
	}

	self.hand_to_collector(txn, media.clone());
	media.len()
}

/// Moves `user_id`'s avatar holder from `old_mxc` to `new_mxc` in `txn`, and
/// hands the old media to the collector once `txn` commits.
///
/// Equal values write nothing, so a profile update leaving the avatar alone
/// cannot release its own holder. The caller holds `new_mxc` (`hold_media`)
/// until `txn` has committed.
#[implement(Service)]
pub fn set_avatar_ref(&self, txn: &mut Txn, user_id: &UserId, old_mxc: Option<&str>, new_mxc: Option<&str>) {
	if old_mxc == new_mxc {
		return;
	}

	let holder = Holder::avatar(user_id);
	if let Some(new_mxc) = new_mxc {
		self.hold(txn, new_mxc, &holder);
	}
	if let Some(old_mxc) = old_mxc {
		self.release(txn, old_mxc, &holder);
	}
}

/// Lists who holds `mxc`, for the admin command.
#[implement(Service)]
pub async fn list_holders(&self, mxc: &str) -> Vec<Holder> {
	let mut holders: Vec<Holder> = Vec::new();

	for kind in [KIND_EVENT, KIND_BACKUP] {
		let prefix = (mxc, kind);
		let rows: Vec<(OwnedRoomId, i64)> = self
			.db
			.mxc_holder
			.keys_prefix(&prefix)
			.ignore_err()
			.filter_map(|(_, _, room, biased): (Ignore, Ignore, &str, u64)| {
				let parsed = RoomId::parse(room)
					.ok()
					.map(|room| (room, unbias_g_seq(biased)));
				async move { parsed }
			})
			.collect()
			.await;
		for (room, g_seq) in rows {
			holders.push(if kind == KIND_EVENT {
				Holder::Event { room, g_seq }
			} else {
				Holder::Backup { room, g_seq }
			});
		}
	}

	let avatars: Vec<String> = self
		.db
		.mxc_holder
		.keys_prefix(&(mxc, KIND_AVATAR))
		.ignore_err()
		.map(|(_, _, localpart): (Ignore, Ignore, &str)| localpart.to_owned())
		.collect()
		.await;
	holders.extend(
		avatars
			.into_iter()
			.map(|localpart| Holder::Avatar { localpart }),
	);

	holders
}

/// Whether anything holds `mxc`: one prefix seek.
#[implement(Service)]
pub async fn has_holders(&self, mxc: &str) -> bool {
	let prefix = (mxc,);
	let first = self.db.mxc_holder.keys_prefix_raw(&prefix);
	futures::pin_mut!(first);
	first.next().await.is_some()
}

/// When `mxc` was stored, if this model manages it.
///
/// Return:
///     Option<u64>  creation time in milliseconds; None for media from before
///     the model existed (never removed automatically).
#[implement(Service)]
pub async fn managed_since(&self, mxc: &str) -> Option<u64> {
	let bytes = self.db.mxc_managed.get(mxc).await.ok()?;
	bytes.as_ref().try_into().ok().map(u64::from_be_bytes)
}

/// Arranges for `mxc_uris` to reach the collector after `txn` commits.
///
/// Registered on the transaction rather than sent now, so the collector can
/// never look before the release has been applied. With no collector running
/// (startup, shutdown) nothing is sent: the sweep finds what was missed.
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
