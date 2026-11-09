#![cfg(test)]

use std::{fs::remove_dir_all, process::id as process_id};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{DeviceId, UserId, device_id, user_id},
};
use futures::future::join;
use tuwunel_service::{
	Services,
	users::{Register, device::generate_refresh_token, login::admit_any},
};

/// The two things that make an access token and a refresh token different
/// credentials rather than two strings: a refresh token cannot authenticate a
/// request, and logging in again on a device ends that device's previous session
/// (/docs/design/accounts/session-tokens.md §2, §3).
///
/// 🚨 Both were real before this gate, confirmed against a running server: the
/// refresh token answered `whoami` and wrote account data, and after a re-login
/// the previous refresh token still minted fresh token pairs for ever.
#[test]
fn refresh_tokens_do_not_authenticate_and_a_relogin_ends_the_old_session() -> Result {
	let db_path = format!("/tmp/tuwunel-test-session-tokens-{}", process_id());

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path=\"{db_path}\""));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		let outcome = two_credentials_two_jobs(&services).await;

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

async fn two_credentials_two_jobs(services: &Services) -> Result {
	let alice: &UserId = user_id!("@alice:localhost");
	let device: &DeviceId = device_id!("DEVICEONE");
	let other_device: &DeviceId = device_id!("DEVICETWO");
	// The device the OIDC-shaped path logs in on, and the one two logins race for.
	let oidc_device: &DeviceId = device_id!("DEVICEOIDC");
	let race_device: &DeviceId = device_id!("DEVICERACE");

	services
		.users
		.full_register(Register {
			user_id: Some(alice),
			password: Some("correct-horse-battery"),
			..Default::default()
		})
		.await?;

	let first = services
		.users
		.issue_session(alice, Some(device), Some("first"), true, None, &mut admit_any)
		.await?;

	let Some(first_refresh) = first.refresh_token.clone() else {
		return Err!("asking for a refresh token did not produce one, so this test would pass vacuously");
	};

	// ---- §2: the refresh token is not an access token ----

	if services
		.users
		.find_from_token(&first_refresh)
		.await
		.is_ok()
	{
		return Err!("the refresh token authenticates like an access token");
	}

	// ... while the access token still does, and the refresh token is still
	// found by whoever asks for what it is.
	if services
		.users
		.find_from_token(&first.access_token)
		.await
		.is_err()
	{
		return Err!("the access token stopped authenticating, so the gate refuses the wrong thing");
	}
	if services
		.users
		.find_from_refresh_token(&first_refresh)
		.await
		.is_err()
	{
		return Err!("the refresh token is not in the refresh table, so /refresh cannot work");
	}

	// RFC7009 revocation takes either kind, so it has its own lookup; that one
	// must still resolve both.
	for (what, token) in
		[("an access token", &first.access_token), ("a refresh token", &first_refresh)]
	{
		if services
			.users
			.find_from_access_or_refresh_token(token)
			.await
			.is_err()
		{
			return Err!("the either-kind lookup does not resolve {what}, so revocation would do nothing");
		}
	}

	// ⭐ The second gate, and the only way to reach it: `set_access_token` does
	// not care what a token looks like, so this writes a refresh-shaped string
	// into the access table exactly as a future caller might by mistake. The
	// prefix refusal in `find_from_token` is what has to catch it.
	let planted = format!("refresh_{}", "p".repeat(32));
	services
		.users
		.set_access_token(alice, other_device, &planted, None, None)
		.await?;

	if services.users.find_from_token(&planted).await.is_ok() {
		return Err!("a refresh-prefixed token sitting in the access table authenticates");
	}

	// ---- §2: /refresh still works, and still rotates ----

	let refreshed = services
		.users
		.refresh_session(&first_refresh, &mut admit_any)
		.await?;

	let Some(second_refresh) = refreshed.refresh_token.clone() else {
		return Err!("a refresh did not return the next refresh token");
	};
	if second_refresh == first_refresh {
		return Err!("the refresh token did not rotate");
	}
	if services
		.users
		.find_from_token(&refreshed.access_token)
		.await
		.is_err()
	{
		return Err!("the access token minted by /refresh does not authenticate");
	}
	if services
		.users
		.find_from_token(&second_refresh)
		.await
		.is_ok()
	{
		return Err!("the rotated refresh token authenticates like an access token");
	}

	// ---- §3: logging in again on the same device ends the previous session ----

	let again = services
		.users
		.issue_session(alice, Some(device), Some("again"), true, None, &mut admit_any)
		.await?;

	if again.device_id != device {
		return Err!("the re-login did not land on the same device, so the rest proves nothing");
	}

	for (what, token) in [
		("the access token from the first login", &first.access_token),
		("the access token minted by the refresh", &refreshed.access_token),
	] {
		if services.users.find_from_token(token).await.is_ok() {
			return Err!("{what} still authenticates after a re-login on its device");
		}
	}

	// 🚨 The sharpest half: the old refresh token must not be able to mint a new
	// pair. Before this gate it could, for ever, after any number of re-logins.
	if services
		.users
		.refresh_session(&second_refresh, &mut admit_any)
		.await
		.is_ok()
	{
		return Err!("the previous session's refresh token still mints tokens after a re-login");
	}

	// The new session works, and the device is still there: a login is not a
	// logout.
	if services
		.users
		.find_from_token(&again.access_token)
		.await
		.is_err()
	{
		return Err!("the token from the re-login does not authenticate");
	}
	if !services
		.users
		.device_exists(alice, device)
		.await
	{
		return Err!("the re-login removed the device instead of its tokens");
	}

	// ⭐ And the other device is untouched: this is the "did not revoke too much"
	// half. Without it, a gate that cleared every token of the account would pass
	// everything above.
	if services
		.users
		.find_from_token(&planted)
		.await
		.is_ok()
	{
		return Err!("the planted token is readable, which this test already ruled out");
	}
	let elsewhere = services
		.users
		.issue_session(alice, Some(other_device), Some("elsewhere"), true, None, &mut admit_any)
		.await?;
	let _ = services
		.users
		.issue_session(alice, Some(device), Some("third"), true, None, &mut admit_any)
		.await?;

	if services
		.users
		.find_from_token(&elsewhere.access_token)
		.await
		.is_err()
	{
		return Err!("logging in on one device revoked another device's token");
	}

	// ---- §3: the same promise on the path that does not go through `issue_session` ----

	// 🚨 This is the shape of the OIDC authorization code grant: it issues a
	// token through `create_device` with a `device_id` the client asked for,
	// which may be one the user already has. Before `start_session` it revoked
	// nothing, so the previous session survived (review of PR #126, rumia and
	// oliver).
	//
	// ⭐ And the token it leaves behind is the worst kind: a login that did not
	// ask for a refresh token has no expiry at all, so it would authenticate for
	// ever rather than for a week.
	let no_expiry = services
		.users
		.issue_session(alice, Some(oidc_device), Some("before oidc"), false, None, &mut admit_any)
		.await?;

	if no_expiry.expires_in.is_some() {
		return Err!("this login was supposed to produce a token with no expiry, so the point below is lost");
	}

	let (oidc_token, oidc_expires) = services.users.generate_access_token(true);
	services
		.users
		.create_device(
			alice,
			Some(oidc_device),
			(Some(&oidc_token), oidc_expires),
			Some(&generate_refresh_token()),
			Some("OIDC Client"),
			None,
		)
		.await?;

	if services
		.users
		.find_from_token(&no_expiry.access_token)
		.await
		.is_ok()
	{
		return Err!("issuing a token through create_device left the device's previous session alive");
	}
	if services
		.users
		.find_from_token(&oidc_token)
		.await
		.is_err()
	{
		return Err!("the token create_device issued does not authenticate");
	}

	// ---- §3.7: two logins at once must not both end up valid ----

	// ⭐ The lock is what makes this deterministic: both logins revoke and write
	// under it, so whichever goes second leaves exactly one live token. Without
	// it they interleave as revoke/revoke/write/write and both survive.
	let (first_race, second_race) = join(
		services
			.users
			.issue_session(alice, Some(race_device), Some("race one"), true, None, &mut admit_any),
		services
			.users
			.issue_session(alice, Some(race_device), Some("race two"), true, None, &mut admit_any),
	)
	.await;

	let first_race = first_race?;
	let second_race = second_race?;

	let mut survivors = 0_usize;
	for token in [&first_race.access_token, &second_race.access_token] {
		if services.users.find_from_token(token).await.is_ok() {
			survivors = survivors.saturating_add(1);
		}
	}

	if survivors != 1 {
		return Err!("two concurrent logins on one device left {survivors} live tokens, not 1");
	}

	Ok(())
}