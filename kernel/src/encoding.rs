//! Canonical encoding with strict decode rejection rules
//!
//! Per §5 of ledgerfs-spec-v7.md:
//! - All multi-byte integers are little-endian
//! - UTF-8 for text strings
//! - BLAKE3 for hashing

use crate::{BLAKE3_HASH_SIZE, Error, Result};

/// Compute BLAKE3 full hash (32 bytes)
pub fn blake3_hash(data: &[u8]) -> [u8; BLAKE3_HASH_SIZE] {
	let hash = blake3::hash(data);
	*hash.as_bytes()
}

/// Compute BLAKE3 checksum (truncated to 64 bits / 8 bytes)
pub fn blake3_checksum(data: &[u8]) -> u64 {
	let hash = blake3::hash(data);
	let bytes = hash.as_bytes();
	u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// Read u16 from buffer (little-endian)
pub fn read_u16(buf: &[u8]) -> Result<u16> {
	if buf.len() < 2 {
		return Err(Error::BufferTooSmall {
			required: 2,
			available: buf.len(),
		});
	}
	Ok(u16::from_le_bytes([buf[0], buf[1]]))
}

/// Read u32 from buffer (little-endian)
pub fn read_u32(buf: &[u8]) -> Result<u32> {
	if buf.len() < 4 {
		return Err(Error::BufferTooSmall {
			required: 4,
			available: buf.len(),
		});
	}
	Ok(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// Read u64 from buffer (little-endian)
pub fn read_u64(buf: &[u8]) -> Result<u64> {
	if buf.len() < 8 {
		return Err(Error::BufferTooSmall {
			required: 8,
			available: buf.len(),
		});
	}
	Ok(u64::from_le_bytes([
		buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
	]))
}

/// Write u16 to buffer (little-endian)
pub fn write_u16(buf: &mut [u8], val: u16) -> Result<()> {
	if buf.len() < 2 {
		return Err(Error::BufferTooSmall {
			required: 2,
			available: buf.len(),
		});
	}
	buf[0..2].copy_from_slice(&val.to_le_bytes());
	Ok(())
}

/// Write u32 to buffer (little-endian)
pub fn write_u32(buf: &mut [u8], val: u32) -> Result<()> {
	if buf.len() < 4 {
		return Err(Error::BufferTooSmall {
			required: 4,
			available: buf.len(),
		});
	}
	buf[0..4].copy_from_slice(&val.to_le_bytes());
	Ok(())
}

/// Write u64 to buffer (little-endian)
pub fn write_u64(buf: &mut [u8], val: u64) -> Result<()> {
	if buf.len() < 8 {
		return Err(Error::BufferTooSmall {
			required: 8,
			available: buf.len(),
		});
	}
	buf[0..8].copy_from_slice(&val.to_le_bytes());
	Ok(())
}

/// Verify checksum matches expected value
pub fn verify_checksum(data: &[u8], expected: u64) -> Result<()> {
	let computed = blake3_checksum(data);
	if computed != expected {
		return Err(Error::ChecksumMismatch);
	}
	Ok(())
}

/// Verify hash matches expected value
pub fn verify_hash(data: &[u8], expected: &[u8; BLAKE3_HASH_SIZE]) -> Result<()> {
	let computed = blake3_hash(data);
	if &computed != expected {
		return Err(Error::StoredHashMismatch);
	}
	Ok(())
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_encoding_blake3_hash_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = b"test data";

		// -- Exec
		let hash1 = blake3_hash(data);
		let hash2 = blake3_hash(data);

		// -- Check
		assert_eq!(hash1, hash2);
		assert_eq!(hash1.len(), BLAKE3_HASH_SIZE);

		Ok(())
	}

	#[test]
	fn test_encoding_blake3_checksum_truncated() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = b"test data";

		// -- Exec
		let checksum = blake3_checksum(data);

		// -- Check
		// Checksum should be 64 bits (deterministic)
		let checksum2 = blake3_checksum(data);
		assert_eq!(checksum, checksum2);

		Ok(())
	}

	#[test]
	fn test_encoding_u16_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = [0u8; 4];
		let val = 0x1234u16;

		// -- Exec
		write_u16(&mut buf, val)?;
		let decoded = read_u16(&buf)?;

		// -- Check
		assert_eq!(val, decoded);
		// Verify little-endian encoding
		assert_eq!(buf[0], 0x34);
		assert_eq!(buf[1], 0x12);

		Ok(())
	}

	#[test]
	fn test_encoding_u32_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = [0u8; 8];
		let val = 0x12345678u32;

		// -- Exec
		write_u32(&mut buf, val)?;
		let decoded = read_u32(&buf)?;

		// -- Check
		assert_eq!(val, decoded);
		// Verify little-endian encoding
		assert_eq!(buf[0], 0x78);
		assert_eq!(buf[1], 0x56);
		assert_eq!(buf[2], 0x34);
		assert_eq!(buf[3], 0x12);

		Ok(())
	}

	#[test]
	fn test_encoding_u64_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = [0u8; 16];
		let val = 0x123456789ABCDEF0u64;

		// -- Exec
		write_u64(&mut buf, val)?;
		let decoded = read_u64(&buf)?;

		// -- Check
		assert_eq!(val, decoded);
		// Verify little-endian encoding
		assert_eq!(buf[0], 0xF0);
		assert_eq!(buf[1], 0xDE);

		Ok(())
	}

	#[test]
	fn test_encoding_buffer_too_small() -> TestResult<()> {
		// -- Setup & Fixtures
		let buf = [0u8; 1];

		// -- Exec & Check
		let result = read_u16(&buf);
		assert!(matches!(result, Err(Error::BufferTooSmall { .. })));

		Ok(())
	}

	#[test]
	fn test_encoding_verify_checksum_valid() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = b"test data";
		let checksum = blake3_checksum(data);

		// -- Exec & Check
		verify_checksum(data, checksum)?;

		Ok(())
	}

	#[test]
	fn test_encoding_verify_checksum_invalid() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = b"test data";
		let wrong_checksum = 0x1234567890ABCDEFu64;

		// -- Exec & Check
		let result = verify_checksum(data, wrong_checksum);
		assert!(matches!(result, Err(Error::ChecksumMismatch)));

		Ok(())
	}
}

// endregion: --- Tests
