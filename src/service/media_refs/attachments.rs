//! Declared attachments: the sender's claim that an event references media
//! the server cannot see inside the content, checked before it is counted.
//!
//! Also the one-time warning for clients that never declare: a user who
//! uploaded through the legacy endpoints and then sent an encrypted event
//! without a declaration is told, once, that such attachments do not last.

use std::fmt;

use ruma::{Mxc, UserId};
use tuwunel_core::{debug, implement, utils::time::now_millis, warn};

use super::Service;

/// Why a declaration was refused; the whole send is refused with it.
#[derive(Debug)]
pub enum AttachmentError {
	/// More entries than `attachments_max_per_event`.
	TooMany { declared: usize, max: usize },
	/// Not an `mxc://server/id` URI.
	NotMxc(String),
	/// Media of another server; only local uploads can be attached.
	NotLocal(String),
	/// No such media here (never uploaded, still uploading, or removed).
	NotFound(String),
	/// Uploaded by someone other than the sender.
	NotOwner(String),
}

impl fmt::Display for AttachmentError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::TooMany { declared, max } => {
				write!(f, "{declared} attachments declared; at most {max} per event")
			},
			| Self::NotMxc(mxc) => write!(f, "attachment {mxc:?} is not an mxc:// URI"),
			| Self::NotLocal(mxc) => write!(f, "attachment {mxc} is not media of this server"),
			| Self::NotFound(mxc) => write!(f, "attachment {mxc} is not media this server has"),
			| Self::NotOwner(mxc) => write!(f, "attachment {mxc} was not uploaded by the sender"),
		}
	}
}

impl std::error::Error for AttachmentError {}

/// Checks a sender's declared attachments and returns them deduplicated.
///
/// Args:
///     sender: example: @alice:localhost
///     declared: example: ["mxc://localhost/abc", "mxc://localhost/abc"]
/// Return:
///     Result<Vec<String>, AttachmentError>  the distinct valid URIs; Err on
///     the first entry that is not local, existing, and the sender's own.
///     Fails closed: an unreadable lookup counts as not found.
#[implement(Service)]
pub async fn check_attachments(
	&self,
	sender: &UserId,
	declared: &[String],
) -> Result<Vec<String>, AttachmentError> {
	let max = self.services.config.attachments_max_per_event;
	if declared.len() > max {
		return Err(AttachmentError::TooMany { declared: declared.len(), max });
	}

	let mut checked: Vec<String> = Vec::with_capacity(declared.len());
	for raw in declared {
		if checked.iter().any(|seen| seen == raw) {
			continue;
		}

		let mxc = Mxc::try_from(raw.as_str()).map_err(|_| AttachmentError::NotMxc(raw.clone()))?;
		if !self.services.media.is_local(&mxc) {
			return Err(AttachmentError::NotLocal(raw.clone()));
		}

		// One lookup answers "exists" and "not removed": removed media is
		// GONE there, which is as good as absent for attaching.
		if self.services.media.media_info(&mxc).await.is_err() {
			return Err(AttachmentError::NotFound(raw.clone()));
		}

		match self.services.media.uploader_of(&mxc).await {
			| Some(uploader) if uploader == sender => {},
			| _ => return Err(AttachmentError::NotOwner(raw.clone())),
		}

		checked.push(raw.clone());
	}

	Ok(checked)
}

/// Records that `user` just uploaded through a legacy Matrix endpoint, for
/// `warn_if_undeclared_attachments` to consult.
#[implement(Service)]
pub fn note_legacy_upload(&self, user: &UserId) {
	self.db
		.userid_lastlegacyupload
		.insert(user, now_millis().to_be_bytes());
}

/// How long after a legacy upload an undeclared encrypted send is taken to
/// be "that upload, attached": long enough for a slow composer, short
/// enough not to fire on unrelated messages days later.
const LEGACY_UPLOAD_LINK_WINDOW_MILLIS: u64 = 24 * 60 * 60 * 1000;

/// Tells `user`, once, that attachments in encrypted rooms need a
/// declaration, when an encrypted event arrives without one shortly after a
/// legacy upload. Never blocks the send; errors are logged.
///
/// Args:
///     user: the sender of an m.room.encrypted event that came through the
///     legacy send endpoint with no `X-Wbf-Attachments`, example: @bob:localhost
/// Return:
///     bool  whether a warning was sent this time.
#[implement(Service)]
pub async fn warn_if_undeclared_attachments(&self, user: &UserId) -> bool {
	let last_upload = match self.db.userid_lastlegacyupload.get(user).await {
		| Ok(bytes) => bytes
			.as_ref()
			.try_into()
			.map(u64::from_be_bytes)
			.unwrap_or(0),
		| Err(_) => return false,
	};
	if now_millis().saturating_sub(last_upload) > LEGACY_UPLOAD_LINK_WINDOW_MILLIS {
		return false;
	}

	if self
		.db
		.userid_attachmentwarned
		.get(user)
		.await
		.is_ok()
	{
		return false;
	}

	// Marked before sending, so a burst of encrypted sends warns once even if
	// the notice itself is slow.
	self.db.userid_attachmentwarned.insert(user, []);

	match self.services.admin.send_attachment_warning(user).await {
		| Ok(()) => {
			debug!(%user, "Told the user once that undeclared attachments do not last.");
			true
		},
		| Err(error) => {
			warn!(%user, ?error, "Could not send the undeclared-attachments warning.");
			false
		},
	}
}

/// The warning's text, in one place so the design document can quote it.
pub const UNDECLARED_ATTACHMENTS_WARNING: &str = "This server keeps an uploaded file only while a message is known to \
                                                 use it. Your client just sent an encrypted message without telling \
                                                 the server which uploaded files it attaches, so files you attach in \
                                                 encrypted rooms from this client will be deleted by the server's \
                                                 cleanup shortly after upload. To keep attachments in encrypted \
                                                 rooms, use a client that supports this server's attachment \
                                                 declaration (wbf).";

#[cfg(test)]
mod tests {
	use super::AttachmentError;

	#[test]
	fn every_refusal_names_the_offending_attachment() {
		let mxc = "mxc://localhost/abc".to_owned();
		for error in [
			AttachmentError::NotMxc(mxc.clone()),
			AttachmentError::NotLocal(mxc.clone()),
			AttachmentError::NotFound(mxc.clone()),
			AttachmentError::NotOwner(mxc.clone()),
		] {
			assert!(error.to_string().contains("mxc://localhost/abc"), "{error}");
		}
		assert!(
			AttachmentError::TooMany { declared: 40, max: 32 }
				.to_string()
				.contains("32")
		);
	}
}
