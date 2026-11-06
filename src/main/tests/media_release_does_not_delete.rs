#![cfg(test)]

use std::{env::set_var, fs::remove_dir_all, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	config::MEDIA_UNREFERENCED_GRACE_ENV,
	ruma::{Mxc, OwnedMxcUri, UserId, user_id},
};
use tuwunel_service::Services;

/// Releasing a holder releases it and nothing else: the media is still there
/// afterwards, even though nothing holds it any more, because it is inside the
/// protection period. The sweep is what removes it, and only once it is past
/// that period (/docs/design/media/premature-deletion.md §2.4).
///
/// 🚨 This is the third-party attack from §2.1 end to end: the attacker never
/// uploaded anything, and the two steps are the two profile writes any user can
/// make. Before this gate, step 2 deleted the victim's fresh upload.
#[test]
fn releasing_a_holder_keeps_media_inside_the_protection_period() -> Result {
	let db_path = format!("/tmp/tuwunel-test-media-release-{}", process_id());

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path=\"{db_path}\""));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		let outcome = release_leaves_removal_to_the_sweep(&services).await;

		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);

	remove_dir_all(&db_path).ok();

	result
}

async fn release_leaves_removal_to_the_sweep(services: &Services) -> Result {
	let victim: &UserId = user_id!("@victim:localhost");
	let attacker: &UserId = user_id!("@attacker:localhost");
	let keeper: &UserId = user_id!("@keeper:localhost");
	// Built, not written out: `mxc.to_string()` is the key the upload records in
	// `mxc_managed` and the string a client would put in a profile, so building
	// both sides from one `Mxc` is what keeps the test asking about one media.
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: "victims-fresh-upload",
	};
	let key = mxc.to_string();

	// The victim uploads and does nothing else with it: managed, held by
	// nothing, seconds old. The state the protection period exists for.
	services
		.media
		.create(&mxc, Some(victim), None, Some("image/png"), b"victim bytes")
		.await?;

	if services.media_refs.managed_since(&key).await.is_none() {
		return Err!("the upload did not get an mxc_managed row, so this test would pass vacuously");
	}

	// Step 1: the attacker points their own avatar at it. Allowed on purpose —
	// holding someone else's media only keeps it alive longer.
	let as_avatar: OwnedMxcUri = key.clone().into();
	services
		.profile
		.set_avatar_url(attacker, Some(&as_avatar), None)
		.await?;

	if !services.media_refs.has_holders(&key).await {
		return Err!("the attacker's avatar did not become a holder, so step 2 releases nothing");
	}

	// Step 2: and takes it away again. This is the release that used to delete.
	services
		.profile
		.set_avatar_url(attacker, None, None)
		.await?;

	if services.media_refs.has_holders(&key).await {
		return Err!("the avatar was not released, so this test would pass for the wrong reason");
	}
	if let Some(tombstone) = services.media.find_tombstone(&mxc).await {
		return Err!("the release deleted the victim's media inside the protection period: {tombstone:?}");
	}
	if services.media_refs.managed_since(&key).await.is_none() {
		return Err!("the release dropped the victim's mxc_managed row");
	}

	// ⭐ One holder of two: the first release must leave the media held, the
	// second must leave it unheld. This used to be visible in e2e9 ([1.3], a
	// shared attachment redacted twice) because the second release deleted the
	// file; now that neither release deletes anything, a client cannot see the
	// difference at all and this is the only place it is pinned.
	let shared = Mxc { server_name: services.globals.server_name(), media_id: "two-holders" };
	let shared_key = shared.to_string();
	let shared_uri: OwnedMxcUri = shared_key.clone().into();
	services
		.media
		.create(&shared, Some(victim), None, Some("image/png"), b"shared bytes")
		.await?;
	for holder in [attacker, keeper] {
		services
			.profile
			.set_avatar_url(holder, Some(&shared_uri), None)
			.await?;
	}
	services
		.profile
		.set_avatar_url(attacker, None, None)
		.await?;
	if !services.media_refs.has_holders(&shared_key).await {
		return Err!("releasing one of two holders left the media unheld");
	}
	services
		.profile
		.set_avatar_url(keeper, None, None)
		.await?;
	if services.media_refs.has_holders(&shared_key).await {
		return Err!("releasing the second of two holders left a holder behind");
	}

	// The control: this one keeps its holder all the way through the sweep.
	let kept = Mxc { server_name: services.globals.server_name(), media_id: "still-held" };
	let kept_uri: OwnedMxcUri = kept.to_string().into();
	services
		.media
		.create(&kept, Some(victim), None, Some("image/png"), b"kept bytes")
		.await?;
	services
		.profile
		.set_avatar_url(keeper, Some(&kept_uri), None)
		.await?;

	// And now the other half: with the media past the protection period the
	// sweep does remove it. Without this, everything above would also pass on a
	// server that had simply stopped collecting anything at all.
	//
	// ⚠️ The environment override exists for exactly this: the configured period
	// is floored at seven days, which a test cannot wait out.
	unsafe {
		set_var(MEDIA_UNREFERENCED_GRACE_ENV, "0");
	}

	services.media_refs.sweep_unreferenced().await;

	if services.media.find_tombstone(&mxc).await.is_none() {
		return Err!("the sweep left media that nothing holds and that is past the protection period");
	}
	if services.media.find_tombstone(&shared).await.is_none() {
		return Err!("the sweep left the media whose two holders had both been released");
	}
	if let Some(tombstone) = services.media.find_tombstone(&kept).await {
		return Err!("the sweep removed media an avatar still holds, so it is not deciding anything: {tombstone:?}");
	}

	Ok(())
}
