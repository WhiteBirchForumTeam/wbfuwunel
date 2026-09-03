//! What an uploader declares about an encrypted file, as a fixed 16-byte
//! record: the plaintext facts the server runs a chunked upload by. The
//! file's name, type and keys are not here; they travel encrypted in the
//! `Create` pack's data section and the server never reads them.

use std::fmt;

/// The declaration a chunked upload starts with. Every number is about the
/// plaintext; the server learns nothing about the ciphertext from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncryptedFileInfo {
	/// Plaintext size of the whole file, in bytes.
	pub file_size: u64,
	/// Plaintext size of every chunk but the last. Zero asks for the server
	/// default.
	pub chunk_size: u32,
	/// How many chunks the uploader will send.
	pub chunk_count: u32,
}

/// Wire size of an `EncryptedFileInfo`: `file_size` u64, `chunk_size` u32,
/// `chunk_count` u32, all big-endian.
pub const ENCRYPTED_FILE_INFO_LEN: usize = 16;

/// Why bytes did not decode as an `EncryptedFileInfo`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileInfoError {
	/// The record must be exactly `ENCRYPTED_FILE_INFO_LEN` bytes.
	WrongLength {
		/// Bytes given.
		len: usize,
	},
}

impl fmt::Display for FileInfoError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::WrongLength { len } =>
				write!(f, "EncryptedFileInfo is {ENCRYPTED_FILE_INFO_LEN} bytes, got {len}"),
		}
	}
}

impl std::error::Error for FileInfoError {}

impl EncryptedFileInfo {
	/// The 16 bytes that go in a `Create` pack's meta section.
	#[must_use]
	pub fn encode(&self) -> [u8; ENCRYPTED_FILE_INFO_LEN] {
		let mut bytes = [0_u8; ENCRYPTED_FILE_INFO_LEN];
		bytes[0..8].copy_from_slice(&self.file_size.to_be_bytes());
		bytes[8..12].copy_from_slice(&self.chunk_size.to_be_bytes());
		bytes[12..16].copy_from_slice(&self.chunk_count.to_be_bytes());

		bytes
	}

	/// Reads the record back from exactly `ENCRYPTED_FILE_INFO_LEN` bytes.
	pub fn decode(bytes: &[u8]) -> Result<Self, FileInfoError> {
		if bytes.len() != ENCRYPTED_FILE_INFO_LEN {
			return Err(FileInfoError::WrongLength { len: bytes.len() });
		}

		Ok(Self {
			file_size: u64::from_be_bytes(bytes[0..8].try_into().expect("8 bytes")),
			chunk_size: u32::from_be_bytes(bytes[8..12].try_into().expect("4 bytes")),
			chunk_count: u32::from_be_bytes(bytes[12..16].try_into().expect("4 bytes")),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{ENCRYPTED_FILE_INFO_LEN, EncryptedFileInfo, FileInfoError};

	#[test]
	fn round_trips_in_sixteen_big_endian_bytes() {
		let info = EncryptedFileInfo { file_size: 0x0102_0304_0506_0708, chunk_size: 65536, chunk_count: 3 };
		let bytes = info.encode();

		assert_eq!(bytes.len(), ENCRYPTED_FILE_INFO_LEN);
		assert_eq!(&bytes[0..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
		assert_eq!(&bytes[8..12], &[0, 1, 0, 0]);
		assert_eq!(&bytes[12..16], &[0, 0, 0, 3]);
		assert_eq!(EncryptedFileInfo::decode(&bytes), Ok(info));
	}

	#[test]
	fn any_other_length_is_refused() {
		assert_eq!(EncryptedFileInfo::decode(&[0; 15]), Err(FileInfoError::WrongLength { len: 15 }));
		assert_eq!(EncryptedFileInfo::decode(&[0; 17]), Err(FileInfoError::WrongLength { len: 17 }));
		assert_eq!(EncryptedFileInfo::decode(&[]), Err(FileInfoError::WrongLength { len: 0 }));
	}
}
