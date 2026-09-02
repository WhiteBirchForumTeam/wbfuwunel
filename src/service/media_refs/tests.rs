//! What this service reads out of an event before it counts it. The counter
//! itself is folded by the database engine and tested there; these cover the
//! one pure step that decides which media an event references at all.

use ruma::CanonicalJsonObject;

use super::list_event_mxc_uris_in;

fn event(json: &str) -> CanonicalJsonObject {
	serde_json::from_str(json).expect("test event parses")
}

#[test]
fn a_live_event_names_its_media() {
	let event = event(
		r#"{
			"type": "m.room.message",
			"content": { "msgtype": "m.image", "body": "a", "url": "mxc://a.example/one" }
		}"#,
	);

	assert_eq!(list_event_mxc_uris_in(&event), vec!["mxc://a.example/one"]);
}

#[test]
fn a_redacted_event_names_nothing() {
	// What redact_in_place leaves of an m.image: an empty content object.
	let event = event(r#"{ "type": "m.room.message", "content": {} }"#);

	assert!(list_event_mxc_uris_in(&event).is_empty());
}

#[test]
fn an_event_without_content_names_nothing() {
	let event = event(r#"{ "type": "m.room.message" }"#);

	assert!(list_event_mxc_uris_in(&event).is_empty());
}

#[test]
fn content_that_is_not_an_object_names_nothing() {
	// Fails closed: nothing is counted, so nothing can be released later.
	let event = event(r#"{ "type": "m.room.message", "content": "not an object" }"#);

	assert!(list_event_mxc_uris_in(&event).is_empty());
}
