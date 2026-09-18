//! The hash half of a device version (`docs/design/wbf-room-device-version.md`
//! §3.4): a fingerprint of an account's keys that any client can recompute
//! from what `/keys/query` shows it.
//!
//! 🚨 Only what every querier sees goes in. The user-signing key is shown only
//! to its owner, and a signature someone else made (Alice's on Bob's master key
//! after verifying him) only to the one who made it; with either in, Carol
//! could not arrive at the value Bob's own client does. So a key keeps only its
//! owner's signatures, and loses `unsigned` (display names, not key material).

use ruma::{OwnedDeviceId, UserId, canonical_json::to_canonical_value};
use serde_json::Value;
use tuwunel_core::{Result, err, utils::hash::sha256};

/// How many hex digits of the SHA-256 a version carries.
const HASH_HEX_LEN: usize = 10;

/// Args:
///     owner: whose keys, example: "@bob:localhost"
///     master_key: `None` when the account has none yet
///     self_signing_key: `None` when the account has none yet
///     device_keys: every device that has uploaded keys, in any order
/// Return:
///     Result<String>  10 lowercase hex digits, example: "3f9a0c21bd"; Err
///     only for a key that is not valid canonical JSON (a float, a number too
///     large).
pub fn device_keys_hash(
	owner: &UserId,
	master_key: Option<&Value>,
	self_signing_key: Option<&Value>,
	device_keys: &[(OwnedDeviceId, Value)],
) -> Result<String> {
	let mut devices: Vec<&(OwnedDeviceId, Value)> = device_keys.iter().collect();
	devices.sort_by(|left, right| left.0.cmp(&right.0));

	// An absent key is an empty item, not a skipped one: "no master key, one
	// device" and "a master key, no devices" must not line up.
	let items = [master_key, self_signing_key]
		.into_iter()
		.chain(devices.into_iter().map(|(_, keys)| Some(keys)));

	let mut framed = Vec::new();
	for item in items {
		let canonical = match item {
			| None => Vec::new(),
			| Some(key) => to_canonical_json(&only_what_everyone_sees(owner, key))?,
		};
		let len = u32::try_from(canonical.len()).map_err(|_| err!("a key too large to hash"))?;
		framed.extend_from_slice(&len.to_be_bytes());
		framed.extend_from_slice(&canonical);
	}

	Ok(sha256::hash(&framed)
		.iter()
		.take(HASH_HEX_LEN / 2)
		.map(|byte| format!("{byte:02x}"))
		.collect())
}

fn only_what_everyone_sees(owner: &UserId, key: &Value) -> Value {
	let mut key = key.clone();
	if let Some(fields) = key.as_object_mut() {
		fields.remove("unsigned");
		if let Some(signatures) = fields
			.get_mut("signatures")
			.and_then(Value::as_object_mut)
		{
			signatures.retain(|signer, _| signer == owner.as_str());
		}
	}

	key
}

fn to_canonical_json(key: &Value) -> Result<Vec<u8>> {
	let canonical = to_canonical_value(key).map_err(|error| err!("a key is not canonical JSON: {error}"))?;

	Ok(canonical.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
	use ruma::{owned_device_id, user_id};
	use serde_json::json;

	use super::device_keys_hash;

	fn master() -> serde_json::Value {
		json!({
			"user_id": "@bob:localhost",
			"usage": ["master"],
			"keys": { "ed25519:MASTER": "bWFzdGVy" },
			"signatures": { "@bob:localhost": { "ed25519:DEV1": "c2VsZg" } }
		})
	}

	fn self_signing() -> serde_json::Value {
		json!({
			"user_id": "@bob:localhost",
			"usage": ["self_signing"],
			"keys": { "ed25519:SSK": "c3Nr" },
			"signatures": { "@bob:localhost": { "ed25519:MASTER": "bXNr" } }
		})
	}

	fn device(id: &str, key: &str) -> serde_json::Value {
		json!({
			"user_id": "@bob:localhost",
			"device_id": id,
			"algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
			"keys": { format!("curve25519:{id}"): key, format!("ed25519:{id}"): key },
			"signatures": { "@bob:localhost": { format!("ed25519:{id}"): "ZGV2" } }
		})
	}

	fn devices() -> Vec<(ruma::OwnedDeviceId, serde_json::Value)> {
		vec![(owned_device_id!("DEV2"), device("DEV2", "dHdv")), (owned_device_id!("DEV1"), device("DEV1", "b25l"))]
	}

	/// The vector a client checks its own implementation against
	/// (docs/design/wbf-room-device-version.md §3.4). It was computed a second
	/// time outside Rust, from the algorithm as written there.
	#[test]
	fn the_documented_vector() {
		let hash = device_keys_hash(user_id!("@bob:localhost"), Some(&master()), Some(&self_signing()), &devices()).unwrap();

		assert_eq!(hash, "810b7c3be4");
	}

	#[test]
	fn a_signature_by_someone_else_does_not_move_the_hash() {
		let bob = user_id!("@bob:localhost");
		let before = device_keys_hash(bob, Some(&master()), Some(&self_signing()), &devices()).unwrap();

		let mut signed_by_alice = master();
		signed_by_alice["signatures"]["@alice:localhost"] = json!({ "ed25519:ALICE_USK": "YWxpY2U" });
		let after = device_keys_hash(bob, Some(&signed_by_alice), Some(&self_signing()), &devices()).unwrap();

		assert_eq!(before, after);
	}

	#[test]
	fn unsigned_does_not_move_the_hash() {
		let bob = user_id!("@bob:localhost");
		let before = device_keys_hash(bob, Some(&master()), Some(&self_signing()), &devices()).unwrap();

		let mut named = devices();
		named[0].1["unsigned"] = json!({ "device_display_name": "Bob's phone" });
		let after = device_keys_hash(bob, Some(&master()), Some(&self_signing()), &named).unwrap();

		assert_eq!(before, after);
	}

	#[test]
	fn the_order_devices_arrive_in_does_not_matter() {
		let bob = user_id!("@bob:localhost");
		let mut reversed = devices();
		reversed.reverse();

		assert_eq!(
			device_keys_hash(bob, Some(&master()), Some(&self_signing()), &devices()).unwrap(),
			device_keys_hash(bob, Some(&master()), Some(&self_signing()), &reversed).unwrap()
		);
	}

	#[test]
	fn a_new_device_an_own_signature_or_a_missing_key_each_move_the_hash() {
		let bob = user_id!("@bob:localhost");
		let base = device_keys_hash(bob, Some(&master()), Some(&self_signing()), &devices()).unwrap();

		let mut three = devices();
		three.push((owned_device_id!("DEV3"), device("DEV3", "dGhyZWU")));
		let mut resigned = master();
		resigned["signatures"]["@bob:localhost"]["ed25519:DEV2"] = json!("c2lnMg");

		assert_ne!(base, device_keys_hash(bob, Some(&master()), Some(&self_signing()), &three).unwrap());
		assert_ne!(base, device_keys_hash(bob, Some(&resigned), Some(&self_signing()), &devices()).unwrap());
		assert_ne!(base, device_keys_hash(bob, None, Some(&self_signing()), &devices()).unwrap());
	}

	#[test]
	fn no_keys_at_all_is_still_a_hash() {
		let hash = device_keys_hash(user_id!("@bob:localhost"), None, None, &[]).unwrap();

		assert_eq!(hash.len(), 10);
		assert!(hash.chars().all(|digit| digit.is_ascii_hexdigit() && !digit.is_ascii_uppercase()));
	}
}
