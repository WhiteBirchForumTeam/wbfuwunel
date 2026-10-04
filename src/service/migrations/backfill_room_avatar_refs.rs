//! One-time holder for the per-room avatars a database already holds.
//!
//! `Holder::RoomAvatar{room, user}` is written when an `m.room.member` event
//! becomes current state, so a database written before it existed has current
//! member events whose `avatar_url` nobody holds. 🚨 Those pictures are swept
//! seven days after upload with nobody having done anything — exactly the
//! symptom the holder was added to remove
//! (/docs/design/media/purge-release-set.md §9.6).
//!
//! ⭐ This walks **current member state**, not the timeline: one entry per
//! `(room, member)` instead of one per historical membership change, and the
//! set it produces is by construction the set the append path would hold.

use futures::StreamExt;
use ruma::{OwnedRoomId, UserId, events::StateEventType};
use tuwunel_core::{Result, info, utils::stream::TryIgnore, warn};

use crate::Services;

pub(super) async fn backfill_room_avatar_refs(services: &Services) -> Result {
	warn!("Giving every current per-room avatar its media holder (one-time)");

	let db = &services.db;
	let _cork = db.cork_and_sync();

	let room_ids: Vec<OwnedRoomId> = services
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.collect()
		.await;

	let mut held: usize = 0;
	let mut unnamed: usize = 0;
	for room_id in room_ids {
		// Collected per room rather than streamed: the writes below take the
		// media lock and a transaction, and a borrow of the state stream must not
		// be alive across them.
		let state_keys: Vec<_> = services
			.state_accessor
			.room_state_keys(&room_id, &StateEventType::RoomMember)
			.ignore_err()
			.collect()
			.await;

		for state_key in state_keys {
			// 🚫 Not `expect`: a migration that takes the server down on one
			// unparseable state key leaves the database half walked, and the
			// marker below unwritten (issue #83, CLAUDE.md P). A member nobody can
			// name gets no holder, which is the same place it was already in.
			let Ok(user_id) = UserId::parse(state_key.as_str()) else {
				unnamed = unnamed.saturating_add(1);
				warn!(%room_id, ?state_key, "Member state key does not parse; no per-room avatar holder for it.");
				continue;
			};

			let Ok(member) = services
				.state_accessor
				.get_member(&room_id, &user_id)
				.await
			else {
				continue;
			};

			// No `avatar_url` means no holder to write: a member who never set one,
			// and a `leave` or `ban` that dropped it, are both already in the state
			// the append path would leave them in.
			let Some(avatar) = member.avatar_url else {
				continue;
			};

			// ⚠️ Hold the picture until the holder has committed, the way the append
			// path and `profile::set_profile_values` do: the collector decides
			// between a look at who holds a media and the removal, and that
			// decision is only safe while whoever adds a holder waits on the same
			// lock (`hold()`'s contract).
			let media_held = services
				.media_refs
				.hold_media(avatar.as_str())
				.await;

			let mut txn = db.txn();
			services
				.media_refs
				.set_room_avatar_ref(&mut txn, &room_id, &user_id, Some(avatar.as_str()))
				.await;
			txn.execute();
			drop(media_held);

			held = held.saturating_add(1);
		}
	}

	info!(%held, %unnamed, "Gave every current per-room avatar its media holder");

	db["global"].insert(b"backfill_room_avatar_refs", []);
	Ok(())
}
