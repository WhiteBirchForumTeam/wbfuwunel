//! The fork's own wire protocol: one binary pack per request, response,
//! chunk or fragment, carried over a WebSocket channel or, for testing, one
//! per HTTP request. The pack neither encrypts nor decrypts; it only knows
//! its own layout and checksums. See `docs/design/wbf-wire-format.md`.

pub mod file_info;
pub mod pack;

pub use self::{
	file_info::{ENCRYPTED_FILE_INFO_LEN, EncryptedFileInfo, FileInfoError},
	pack::{
		Flags, HEADER_LEN, Kind, PackBuilder, PackError, PackHeader, PackView, TRAILER_LEN, VERSION,
		decode,
	},
};
