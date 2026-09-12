//! The `0x16 Device` kind: a device's to-device queue over the channel
//! (`docs/design/wbf-to-device.md`).
//!
//! Shaped like `Event` — `Subscribe` to be pushed, `Fetch` to fill a gap —
//! with one thing no room stream has: **the client destroys what it has
//! taken**. The queue is the only copy of a Megolm key, so the server keeps
//! every item until the device says it is safely stored, and `ItemsDestroy`
//! is a command with a result, not an acknowledgement of receipt.
//!
//! Two rules follow from that, and both are refusals rather than repairs:
//! only the connection whose session holds the device may subscribe to its
//! queue (§4), and only the connection holding it may destroy from it — a
//! second connection deleting items the first is still importing would lose
//! them for good.

use ruma::{DeviceId, OwnedDeviceId, UserId};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::{
	debug_warn,
	wbf::{
		Flags, Kind, PackBuilder, PackError, PackView, RejectCode,
		events::{length_prefixed, list_pack_ranges},
	},
};
use tuwunel_service::{
	Services,
	streams::PushedItem,
};

use super::{Failure, PackContext, Reject, Reply, ack, parse_meta};

/// Bytes of one count in an `ItemsDestroy`/`ItemsDestroyed` data section.
const COUNT_LEN: usize = 8;

pub(super) const FETCH: u8 = 0x01;
pub(super) const BATCH: u8 = 0x02;
pub(super) const ITEMS_DESTROY: u8 = 0x03;
pub(super) const SUBSCRIBE: u8 = 0x04;
pub(super) const UNSUBSCRIBE: u8 = 0x05;
pub(super) const ITEMS_DESTROYED: u8 = 0x07;

#[derive(Default, Deserialize)]
struct SubscribeMeta {
	#[serde(default)]
	device_id: Option<OwnedDeviceId>,
	#[serde(default)]
	cd_seq: Option<u64>,
}

#[derive(Default, Deserialize)]
struct FetchMeta {
	#[serde(default)]
	limit: Option<usize>,
	#[serde(default)]
	cd_seq: Option<u64>,
}

#[derive(Default, Deserialize)]
struct DestroyMeta {
	#[serde(default)]
	tc: usize,
}

/// One item as it goes out: its count and its stored JSON.
struct Item {
	count: u64,
	json: Vec<u8>,
}

/// `Device/Subscribe`: take this device's queue and be pushed what arrives.
///
/// Args:
///     ctx: the connection and its session — the session's device is the
///         only identity; the meta's `device_id` is the client saying which
///         one it thinks it is
///     view: meta example: `{"device_id":"PHONE","cd_seq":4711}`
/// Return:
///     Result<(), Failure>  Ack meta `{latest_cd_seq}`; `Forbidden` when the
///     named device is not this session's. Another connection already
///     holding the queue is not a refusal: this one takes it over, and that
///     one is sent `Superseded` (1505).
pub(super) async fn handle_device_subscribe(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let session = ctx
		.session
		.ok_or_else(|| Reject::code(RejectCode::Unauthorized, "log in first: this connection has no session"))?;
	let queue = reply
		.websocket_queue()
		.ok_or_else(|| Reject::code(RejectCode::Unsupported, "the to-device queue needs the WebSocket channel"))?;
	let meta: SubscribeMeta = parse_meta(view, "Device/Subscribe")?;

	// The session says who this is; the meta only says who the client thinks
	// it is. They must agree, or the client is about to import another
	// device's keys into its own store — and destroy them there.
	let device = meta
		.device_id
		.ok_or_else(|| Reject::code(RejectCode::InvalidRequest, "Device/Subscribe needs `device_id`"))?;
	if device != session.device {
		return Err(Reject::code(
			RejectCode::Forbidden,
			format!(
				"this connection's session is device `{}`, not `{device}`",
				session.device
			),
		)
		.into());
	}

	// Whoever held this device's queue has been displaced and told so by the
	// registry (`Superseded`, 1505). There is no refusal to handle here: the
	// rule is the registry's, enforced in the same transaction as the entry,
	// so no `if` at this call site can be right or wrong about it.
	services
		.streams
		.subscribe_device(ctx.connection, &session.user, &device, queue, view.header.id);

	// Read before the window, like `Recent`: a client that stores it never
	// misses an item added while the window was being read.
	let latest_cd_seq = services.globals.current_count();

	reply
		.send(ack(
			view.header.id,
			view.header.seq,
			json!({ "latest_cd_seq": latest_cd_seq }),
			Vec::new(),
		))
		.await?;

	// Registered first, so anything arriving from here on is pushed; the
	// catch-up may therefore repeat an item, which costs the client an
	// idempotent import.
	if let Some(cd_seq) = meta.cd_seq {
		let items = read_items(services, &session.user, &device, Some(cd_seq), services.config.wbf_device_fetch_default_limit).await;
		let pushed: Vec<PushedItem<'_>> = items
			.iter()
			.map(|item| PushedItem { count: item.count, json: &item.json })
			.collect();
		services.streams.push_to_device(
			&session.user,
			&device,
			&pushed,
			services.config.wbf_push_max_events_per_pack,
			services.config.wbf_data_max_bytes,
		);
	}

	Ok(())
}

/// `Device/Unsubscribe`: give the queue back, so another connection can take
/// it. The silent way out (the connection ending) is the guard's job.
pub(super) async fn handle_device_unsubscribe(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	services
		.streams
		.unsubscribe_device(ctx.connection);

	reply
		.send(ack(view.header.id, view.header.seq, json!({}), Vec::new()))
		.await?;

	Ok(())
}

/// `Device/Fetch`: a window of the queue, oldest first, as `Batch` packs.
///
/// Args:
///     view: meta example: `{"limit":1000,"cd_seq":4711}` or `{}`
/// Return:
///     Result<(), Failure>  a `Batch` per pack, `r = 0` on the last one; one
///     empty `Batch` when there is nothing.
pub(super) async fn handle_device_fetch(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let session = ctx
		.session
		.ok_or_else(|| Reject::code(RejectCode::Unauthorized, "log in first: this connection has no session"))?;
	let meta: FetchMeta = parse_meta(view, "Device/Fetch")?;
	// ⚠️ `limit: 0` means none, and answers with one empty `Batch` — the same
	// as `Event/Recent`, which does not raise a zero either. Clamping it up to
	// one instead returned an item nobody asked for and hid the client bug
	// that sent a zero (PR #43 review, rumia).
	let limit = meta
		.limit
		.unwrap_or(services.config.wbf_device_fetch_default_limit)
		.min(services.config.wbf_device_fetch_max_limit);

	let items = read_items(services, &session.user, &session.device, meta.cd_seq, limit).await;
	for pack in build_batches(
		view.header.id,
		&items,
		services.config.wbf_device_default_batch,
		services.config.wbf_data_max_bytes,
	)? {
		reply.send(pack).await?;
	}

	Ok(())
}

/// `Device/ItemsDestroy`: destroy the named items and say which are gone.
///
/// Args:
///     view: meta `{"tc": n}`, data `n` × 8 bytes, each a big-endian count
/// Return:
///     Result<(), Failure>  an `Ack` (the command arrived) and then one
///     `ItemsDestroyed` carrying the counts confirmed gone — an empty list
///     when none are, because "none" and "no answer" must not look alike.
pub(super) async fn handle_device_items_destroy(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let session = ctx
		.session
		.ok_or_else(|| Reject::code(RejectCode::Unauthorized, "log in first: this connection has no session"))?;
	let meta: DestroyMeta = parse_meta(view, "Device/ItemsDestroy")?;

	// Destroying is what makes the subscription exclusive, so it is refused
	// from anywhere but the holder: another connection deleting items this
	// one is still importing would lose them for good.
	let holder = services
		.streams
		.device_holder(&session.user, &session.device);
	if holder != Some(ctx.connection) {
		return Err(Reject::code(
			RejectCode::Forbidden,
			"only the connection holding this device's to-device queue may destroy from it",
		)
		.into());
	}

	let counts = parse_counts(meta.tc, view.data)?;
	reply
		.send(ack(view.header.id, view.header.seq, json!({}), Vec::new()))
		.await?;

	let destroyed = services
		.users
		.destroy_to_device_items(&session.user, &session.device, &counts)
		.await;

	reply
		.send(items_destroyed_pack(view.header.id, meta.tc, &destroyed)?)
		.await?;

	Ok(())
}

/// The counts an `ItemsDestroy` names.
///
/// Args:
///     tc: the meta's count of items
///     data: `tc` × 8 bytes, big-endian
/// Return:
///     Result<Vec<u64>, Reject>  `InvalidRequest` when the two do not agree —
///     and then nothing is destroyed, because one of the two ends encoded
///     this wrong and guessing which would be guessing what to delete.
fn parse_counts(tc: usize, data: &[u8]) -> Result<Vec<u64>, Reject> {
	let declared = tc.saturating_mul(COUNT_LEN);
	if declared != data.len() {
		return Err(Reject::code(
			RejectCode::InvalidRequest,
			format!(
				"Device/ItemsDestroy says {tc} items ({declared} bytes) but carries {} bytes of data",
				data.len()
			),
		));
	}

	Ok(data
		.chunks_exact(COUNT_LEN)
		.map(|count| u64::from_be_bytes(count.try_into().expect("eight bytes")))
		.collect())
}

/// The queue's items after `cd_seq`, oldest first.
async fn read_items(
	services: &Services,
	user: &UserId,
	device: &DeviceId,
	cd_seq: Option<u64>,
	limit: usize,
) -> Vec<Item> {
	use futures::StreamExt;

	services
		.users
		.get_to_device_events(user, device, cd_seq, None)
		.take(limit)
		.map(|(count, event)| Item { count, json: event.json().get().as_bytes().to_vec() })
		.collect()
		.await
}

/// Cuts a window into `Batch` packs answering request `id`.
///
/// Args:
///     items: oldest first
///     batch: `wbf_device_default_batch`
///     data_max: `wbf_data_max_bytes`; a pack is cut here too
/// Return:
///     Result<Vec<Vec<u8>>, PackError>  the packs in order, `seq` 0, 1, 2…;
///     exactly one (empty) pack for an empty window, and the last one has
///     `r = 0`.
fn build_batches(id: u64, items: &[Item], batch: usize, data_max: usize) -> Result<Vec<Vec<u8>>, PackError> {
	let total = items.len();
	if items.is_empty() {
		return Ok(vec![batch_pack(id, 0, total, &[], 0)?]);
	}

	let mut packs = Vec::new();
	let mut seq: u32 = 0;
	let mut sent: usize = 0;
	for range in list_pack_ranges(items.iter().map(|item| item.json.len()), batch, data_max) {
		let in_batch = &items[range];
		sent = sent.saturating_add(in_batch.len());
		packs.push(batch_pack(id, seq, total, in_batch, total.saturating_sub(sent))?);
		seq = seq.saturating_add(1);
	}

	Ok(packs)
}

fn batch_pack(id: u64, seq: u32, tc: usize, items: &[Item], remaining: usize) -> Result<Vec<u8>, PackError> {
	let counts: Vec<u64> = items.iter().map(|item| item.count).collect();
	let data = length_prefixed(items.iter().map(|item| item.json.as_slice()))?;

	Ok(PackBuilder::new(Kind::Device, BATCH, Flags::IS_RESPONSE, id, seq)
		.json_meta(&json!({
			"tc": tc,
			"bc": items.len(),
			"ot": counts.first().copied().unwrap_or(0),
			"nt": counts.last().copied().unwrap_or(0),
			"counts": counts,
			"r": remaining,
		}))?
		.data(&data)?
		.finish())
}

fn items_destroyed_pack(id: u64, tc: usize, destroyed: &[u64]) -> Result<Vec<u8>, PackError> {
	let mut data = Vec::with_capacity(destroyed.len().saturating_mul(COUNT_LEN));
	for count in destroyed {
		data.extend_from_slice(&count.to_be_bytes());
	}
	if destroyed.len() < tc {
		debug_warn!(
			asked = tc,
			destroyed = destroyed.len(),
			"to-device items were not destroyed; the client keeps them and will ask again"
		);
	}

	Ok(PackBuilder::new(Kind::Device, ITEMS_DESTROYED, Flags::IS_RESPONSE, id, 0)
		.json_meta(&json!({ "tc": tc, "bc": destroyed.len() }))?
		.data(&data)?
		.finish())
}

#[cfg(test)]
mod tests {
	use tuwunel_core::wbf::{Kind, decode, events::split_length_prefixed};

	use super::{BATCH, ITEMS_DESTROYED, Item, build_batches, items_destroyed_pack, parse_counts};

	fn item(count: u64, json: &str) -> Item { Item { count, json: json.as_bytes().to_vec() } }

	#[test]
	fn counts_are_eight_bytes_each_and_the_meta_must_agree() {
		let data = [0u8, 0, 0, 0, 0, 0, 1, 244, 0, 0, 0, 0, 0, 0, 1, 245];

		assert_eq!(parse_counts(2, &data).expect("two counts"), vec![500, 501]);

		// One end encoded this wrong; deleting whatever parses would be
		// deleting something nobody asked to delete.
		assert!(parse_counts(3, &data).is_err(), "more items than bytes");
		assert!(parse_counts(1, &data).is_err(), "fewer items than bytes");
		assert!(parse_counts(0, &[0u8; 3]).is_err(), "not a whole number of counts");
	}

	#[test]
	fn an_empty_window_is_one_empty_batch_that_ends_it() {
		let packs = build_batches(7, &[], 100, 4096).expect("builds");

		assert_eq!(packs.len(), 1);
		let mut pack = packs.into_iter().next().expect("one");
		let view = decode(&mut pack).expect("decodes");
		assert_eq!(view.header.kind, Kind::Device);
		assert_eq!(view.header.subtype, BATCH);
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["tc"], 0);
		assert_eq!(meta["bc"], 0);
		assert_eq!(meta["r"], 0, "an empty window is already over");
	}

	#[test]
	fn batches_run_oldest_first_and_carry_every_count() {
		let items: Vec<Item> = (0..5).map(|n| item(500 + n, "{\"a\":1}")).collect();

		let packs = build_batches(7, &items, 2, 4096).expect("builds");

		assert_eq!(packs.len(), 3, "five items, two to a pack");
		let mut remaining_seen = Vec::new();
		let mut counts_seen = Vec::new();
		for (expected_seq, mut pack) in packs.into_iter().enumerate() {
			let view = decode(&mut pack).expect("decodes");
			assert_eq!(view.header.seq as usize, expected_seq);
			let meta = view.meta_json().expect("meta");
			assert_eq!(meta["tc"], 5, "every pack says how big the window is");
			remaining_seen.push(meta["r"].as_u64().expect("r"));
			for count in meta["counts"].as_array().expect("counts") {
				counts_seen.push(count.as_u64().expect("count"));
			}
			assert_eq!(
				split_length_prefixed(view.data).expect("items").len(),
				meta["bc"].as_u64().expect("bc") as usize
			);
		}

		assert_eq!(counts_seen, vec![500, 501, 502, 503, 504], "oldest first, nothing lost");
		assert_eq!(remaining_seen, vec![3, 1, 0], "`r` counts down to zero on the last pack");
	}

	#[test]
	fn destroying_nothing_still_answers_with_an_empty_list() {
		// Otherwise "none of them were destroyed" and "the server never
		// answered" look the same to a client, and they are not the same:
		// one is do nothing, the other is try again.
		let mut pack = items_destroyed_pack(9, 2, &[]).expect("builds");

		let view = decode(&mut pack).expect("decodes");
		assert_eq!(view.header.subtype, ITEMS_DESTROYED);
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["tc"], 2, "what was asked");
		assert_eq!(meta["bc"], 0, "what is gone");
		assert!(view.data.is_empty());
	}

	#[test]
	fn destroyed_counts_go_back_as_eight_bytes_each() {
		let mut pack = items_destroyed_pack(9, 2, &[500, 501]).expect("builds");

		let view = decode(&mut pack).expect("decodes");
		assert_eq!(view.meta_json().expect("meta")["bc"], 2);
		assert_eq!(view.data, [0u8, 0, 0, 0, 0, 0, 1, 244, 0, 0, 0, 0, 0, 0, 1, 245]);
	}
}
