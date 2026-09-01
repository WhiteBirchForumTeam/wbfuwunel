//! Media references carried by event content.
//!
//! One reader answers "which media does this event point at", so every caller
//! that needs the answer agrees on which content keys count as a reference.

use ruma::{CanonicalJsonObject, CanonicalJsonValue};

/// Content keys naming a media repository item, in the order they are read.
///
/// The list is the whole definition of "this event references media". Adding a
/// key here changes what the reference index records, so a new key needs an
/// index rebuild to take effect for events already stored.
const MXC_CONTENT_PATHS: &[&[&str]] = &[
	// Unencrypted body: m.image, m.file, m.video, m.audio, m.room.avatar.
	&["url"],
	// Thumbnail of either body, encrypted or not.
	&["info", "thumbnail_url"],
	// Encrypted body: the EncryptedFile object holds the url instead.
	&["file", "url"],
	// Encrypted thumbnail: an EncryptedFile of its own.
	&["info", "thumbnail_file", "url"],
];

/// Lists the `mxc://` URIs referenced by one event's `content`, deduplicated
/// and in `MXC_CONTENT_PATHS` order.
///
/// Values that are absent, not strings, or not `mxc://` URIs are skipped, so a
/// hostile or malformed event yields fewer entries rather than an error.
/// Returns an empty vector for content referencing no media.
#[must_use]
pub fn list_content_mxc_uris(content: &CanonicalJsonObject) -> Vec<String> {
	let mut uris: Vec<String> = Vec::new();

	for path in MXC_CONTENT_PATHS {
		let Some(uri) = find_string_at_path(content, path) else {
			continue;
		};

		if !uri.starts_with("mxc://") {
			continue;
		}

		if uris.iter().any(|seen| seen == uri) {
			continue;
		}

		uris.push(uri.to_owned());
	}

	uris
}

/// Finds the string at `path` within `content`, or `None` when any step is
/// missing or is not of the expected type.
fn find_string_at_path<'a>(
	content: &'a CanonicalJsonObject,
	path: &[&str],
) -> Option<&'a str> {
	let (last, parents) = path.split_last()?;

	let mut current = content;
	for step in parents {
		current = current.get(*step).and_then(CanonicalJsonValue::as_object)?;
	}

	current.get(*last).and_then(CanonicalJsonValue::as_str)
}

#[cfg(test)]
mod tests {
	use ruma::CanonicalJsonObject;

	use super::list_content_mxc_uris;

	fn content(json: &str) -> CanonicalJsonObject {
		serde_json::from_str(json).expect("test content parses")
	}

	#[test]
	fn no_media_yields_nothing() {
		let content = content(r#"{"msgtype":"m.text","body":"hi"}"#);
		assert!(list_content_mxc_uris(&content).is_empty());
	}

	#[test]
	fn plain_body_url_is_listed() {
		let content = content(r#"{"msgtype":"m.image","url":"mxc://a.example/one"}"#);
		assert_eq!(list_content_mxc_uris(&content), vec!["mxc://a.example/one"]);
	}

	#[test]
	fn encrypted_body_and_thumbnails_are_listed() {
		let content = content(
			r#"{
				"file": { "url": "mxc://a.example/body" },
				"info": {
					"thumbnail_url": "mxc://a.example/thumb",
					"thumbnail_file": { "url": "mxc://a.example/enc-thumb" }
				}
			}"#,
		);

		assert_eq!(list_content_mxc_uris(&content), vec![
			"mxc://a.example/thumb",
			"mxc://a.example/body",
			"mxc://a.example/enc-thumb",
		]);
	}

	#[test]
	fn same_uri_twice_is_listed_once() {
		let content = content(
			r#"{ "url": "mxc://a.example/one", "info": { "thumbnail_url": "mxc://a.example/one" } }"#,
		);
		assert_eq!(list_content_mxc_uris(&content), vec!["mxc://a.example/one"]);
	}

	#[test]
	fn non_mxc_scheme_is_skipped() {
		let content = content(r#"{"url":"https://a.example/one"}"#);
		assert!(list_content_mxc_uris(&content).is_empty());
	}

	#[test]
	fn wrong_types_are_skipped_rather_than_failing() {
		let content = content(r#"{"url": 7, "info": "not-an-object", "file": []}"#);
		assert!(list_content_mxc_uris(&content).is_empty());
	}

	#[test]
	fn redacted_content_yields_nothing() {
		// What redact_in_place leaves behind for an m.image.
		let content = content(r#"{}"#);
		assert!(list_content_mxc_uris(&content).is_empty());
	}
}
