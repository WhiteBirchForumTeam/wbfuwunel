//! The fork's own wire protocol: one binary pack per request, response,
//! chunk or fragment, carried over a WebSocket channel or, for testing, one
//! per HTTP request. The pack neither encrypts nor decrypts; it only knows
//! its own layout and checksums. See `docs/design/wbf-wire-format.md`.

pub mod error_code;
pub mod events;
pub mod file_info;
pub mod id_type;
pub mod pack;
#[cfg(test)]
mod vectors;

pub use self::{
	error_code::{CONTROL_ERROR_SUBTYPE, RejectCode},
	file_info::{ENCRYPTED_FILE_INFO_LEN, EncryptedFileInfo, FileInfoError},
	id_type::{ID_VALUE_MAX, IdType, IdValueTooLarge, id_value},
	pack::{
		Flags, HEADER_LEN, Kind, PackBuilder, PackError, PackHeader, PackView, TRAILER_LEN, VERSION,
		decode,
	},
};
