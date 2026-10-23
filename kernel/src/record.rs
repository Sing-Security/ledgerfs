//! ObjectRecord framing with BLAKE3 verification
//!
//! Per §7 and §9 of FORMAT.md:
//! - ObjectRecord wraps all stored objects
//! - Content-addressed using BLAKE3
//! - 8-byte aligned with length prefix
//! - Checksum covers entire record
//! - Compression support per §28

use crate::{
	BLAKE3_HASH_SIZE, Error, OBJECT_RECORD_MAGIC, Result,
	compression::{CompressionAlgorithm, compress, decompress},
	encoding::{
		blake3_checksum, blake3_hash, read_u16, read_u32, read_u64, verify_checksum, write_u16, write_u32, write_u64,
	},
	nt_write::{nt_write_payload, sfence},
};
use alloc::vec::Vec;

/// Object type discriminator (§7.2)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ObjectType {
	/// File content chunk.
	Data = 0x01,
	/// File, directory, or symlink metadata.
	Inode = 0x02,
	/// Internal B-tree node.
	BTreeNode = 0x03,
	/// B-tree leaf.
	BTreeLeaf = 0x04,
	/// Logical-offset to chunk mapping (§19.2).
	ExtentMap = 0x05,
	/// Deduplication index entry (§21.1).
	DedupeEntry = 0x06,
	/// The commit root.
	Root = 0x07,
}

impl ObjectType {
	/// Decode a discriminator, refusing values outside the set defined in
	/// §7.2.
	pub fn from_u16(val: u16) -> Result<Self> {
		match val {
			0x01 => Ok(Self::Data),
			0x02 => Ok(Self::Inode),
			0x03 => Ok(Self::BTreeNode),
			0x04 => Ok(Self::BTreeLeaf),
			0x05 => Ok(Self::ExtentMap),
			0x06 => Ok(Self::DedupeEntry),
			0x07 => Ok(Self::Root),
			_ => Err(Error::InvalidObjectType { type_id: val }),
		}
	}

	/// The on-disk discriminant for this object type.
	pub fn to_u16(self) -> u16 {
		self as u16
	}
}

/// ObjectRecord header (§7.1)
#[derive(Debug, Clone)]
pub struct ObjectRecordHeader {
	/// Record magic, `OBJECT_RECORD_MAGIC` (§7.1).
	pub magic: u64,
	/// Object kind, one of the discriminators defined in §7.2.
	pub object_type: ObjectType,
	/// Compression algorithm applied to the payload, or 0 for none (§7.1,
	/// §28.1).
	pub compression: u8,
	/// Encryption flags for the payload; 0 means plaintext (§7.1).
	pub encryption: u8,
	/// Payload length as stored, after any compression (§7.1).
	pub stored_len: u32,
	/// Payload length before compression; object identity is computed over
	/// these bytes, so compressing an object does not change its name (§7.1).
	pub logical_len: u32,
	/// Truncated BLAKE3 over the record from `object_type` through the
	/// payload, excluding `magic` and `checksum` itself (§7.1).
	pub checksum: u64,
	/// Full BLAKE3 hash of the canonical, uncompressed payload (§7.1).
	pub object_id: [u8; BLAKE3_HASH_SIZE],
}

impl ObjectRecordHeader {
	/// Header size in bytes
	pub const SIZE: usize = 8 + 2 + 1 + 1 + 4 + 4 + 8 + BLAKE3_HASH_SIZE; // 60 bytes

	/// Decode header from buffer
	pub fn decode(buf: &[u8]) -> Result<Self> {
		if buf.len() < Self::SIZE {
			return Err(Error::BufferTooSmall {
				required: Self::SIZE,
				available: buf.len(),
			});
		}

		let magic = read_u64(&buf[0..])?;
		if magic != OBJECT_RECORD_MAGIC {
			return Err(Error::InvalidMagic {
				expected: OBJECT_RECORD_MAGIC,
				found: magic,
			});
		}

		let object_type_raw = read_u16(&buf[8..])?;
		let object_type = ObjectType::from_u16(object_type_raw)?;

		let compression = buf[10];
		let encryption = buf[11];
		let stored_len = read_u32(&buf[12..])?;
		let logical_len = read_u32(&buf[16..])?;
		let checksum = read_u64(&buf[20..])?;

		let mut object_id = [0u8; BLAKE3_HASH_SIZE];
		object_id.copy_from_slice(&buf[28..28 + BLAKE3_HASH_SIZE]);

		Ok(Self {
			magic,
			object_type,
			compression,
			encryption,
			stored_len,
			logical_len,
			checksum,
			object_id,
		})
	}

	/// Encode header to buffer
	pub fn encode(&self, buf: &mut [u8]) -> Result<()> {
		if buf.len() < Self::SIZE {
			return Err(Error::BufferTooSmall {
				required: Self::SIZE,
				available: buf.len(),
			});
		}

		write_u64(&mut buf[0..], self.magic)?;
		write_u16(&mut buf[8..], self.object_type.to_u16())?;
		buf[10] = self.compression;
		buf[11] = self.encryption;
		write_u32(&mut buf[12..], self.stored_len)?;
		write_u32(&mut buf[16..], self.logical_len)?;
		write_u64(&mut buf[20..], self.checksum)?;
		buf[28..28 + BLAKE3_HASH_SIZE].copy_from_slice(&self.object_id);

		Ok(())
	}
}

/// ObjectRecord reader for verification
pub struct ObjectRecordReader;

impl ObjectRecordReader {
	/// Read and verify an ObjectRecord from buffer
	/// Returns (header, decompressed_payload_slice)
	///
	/// Note: Decompressed data is returned in a new Vec if compression was used.
	/// The stored_hash in the header verifies the compressed bytes on disk.
	/// The object_id verifies the canonical (uncompressed) bytes.
	pub fn read(buf: &[u8]) -> Result<(ObjectRecordHeader, Vec<u8>)> {
		// Decode header
		let header = ObjectRecordHeader::decode(buf)?;

		// Calculate total record length
		let total_len = ObjectRecordHeader::SIZE + header.stored_len as usize;

		if buf.len() < total_len {
			return Err(Error::BufferTooSmall {
				required: total_len,
				available: buf.len(),
			});
		}

		// Extract stored payload (potentially compressed)
		let stored_payload = &buf[ObjectRecordHeader::SIZE..total_len];

		// Verify checksum over stored bytes (compressed or not)
		// Checksum covers: header (excluding checksum field) + stored_payload
		let mut checksum_data = Vec::new();
		checksum_data.extend_from_slice(&buf[0..20]); // magic through logical_len
		checksum_data.extend_from_slice(&buf[28..total_len]); // object_id through payload

		verify_checksum(&checksum_data, header.checksum)?;

		// Decompress if needed
		let compression_algo = CompressionAlgorithm::from_u8(header.compression)?;
		let logical_payload = if compression_algo == CompressionAlgorithm::None {
			stored_payload.to_vec()
		} else {
			decompress(stored_payload, compression_algo, header.logical_len as usize)?
		};

		// Verify object_id matches canonical (uncompressed) payload hash
		let computed_id = blake3_hash(&logical_payload);
		if computed_id != header.object_id {
			return Err(Error::ObjectIdMismatch);
		}

		Ok((header, logical_payload))
	}
}

/// ObjectRecord - in-memory representation of a stored object
///
/// This struct provides a high-level interface for working with ObjectRecords.
/// For low-level reading/writing, use ObjectRecordReader and ObjectRecordWriter.
#[derive(Debug, Clone)]
pub struct ObjectRecord {
	/// Object ID (content-addressed)
	pub object_id: [u8; BLAKE3_HASH_SIZE],

	/// Object type
	pub object_type: ObjectType,

	/// Compression algorithm used
	pub compression: u8,

	/// Encryption flags
	pub encryption: u8,

	/// Logical (uncompressed) data
	pub logical_data: Vec<u8>,

	/// Stored (possibly compressed) data
	/// If None, will be compressed on encode
	stored_data: Option<Vec<u8>>,
}

impl ObjectRecord {
	/// Create a new ObjectRecord
	///
	/// # Arguments
	///
	/// * `object_id` - Content-addressed object ID (as bytes array)
	/// * `data` - Logical (uncompressed) data
	/// * `object_type` - Type of object
	/// * `compression` - Compression algorithm (0 = none)
	/// * `encryption` - Encryption flags (0 = none)
	pub fn new(
		object_id: [u8; BLAKE3_HASH_SIZE],
		data: &[u8],
		object_type: ObjectType,
		compression: u8,
		encryption: u8,
	) -> Result<Self> {
		Ok(Self {
			object_id,
			object_type,
			compression,
			encryption,
			logical_data: data.to_vec(),
			stored_data: None,
		})
	}

	/// Create a new ObjectRecord from an ObjectId
	///
	/// # Arguments
	///
	/// * `object_id` - Content-addressed object ID
	/// * `data` - Logical (uncompressed) data
	/// * `object_type` - Type of object
	/// * `compression` - Compression algorithm (0 = none)
	/// * `encryption` - Encryption flags (0 = none)
	pub fn from_object_id(
		object_id: crate::object::ObjectId,
		data: &[u8],
		object_type: ObjectType,
		compression: u8,
		encryption: u8,
	) -> Result<Self> {
		Self::new(*object_id.as_bytes(), data, object_type, compression, encryption)
	}

	/// Get total size of the record when encoded
	///
	/// Returns header size + stored data size
	pub fn total_size(&self) -> usize {
		let stored_len = if let Some(ref stored) = self.stored_data {
			stored.len()
		} else {
			self.logical_data.len()
		};
		ObjectRecordHeader::SIZE + stored_len
	}

	/// Encode the record to a buffer
	///
	/// Returns the number of bytes written
	pub fn encode(&self, buf: &mut [u8]) -> Result<usize> {
		let compression_algo = CompressionAlgorithm::from_u8(self.compression)?;
		ObjectRecordWriter::write(
			buf,
			self.object_type,
			compression_algo,
			self.encryption,
			&self.logical_data,
		)
	}
}

/// ObjectRecord writer
pub struct ObjectRecordWriter;

impl ObjectRecordWriter {
	/// Write an ObjectRecord to buffer with automatic compression
	///
	/// # Arguments
	///
	/// * `buf` - Output buffer
	/// * `object_type` - Type of object
	/// * `compression_policy` - Desired compression algorithm
	/// * `encryption` - Encryption flags (0 = none)
	/// * `logical_data` - Canonical (uncompressed) data for ObjectId
	///
	/// # Returns
	///
	/// Total bytes written
	///
	/// # Compression Behavior
	///
	/// - ObjectId is always computed from logical_data (canonical bytes)
	/// - Compression is attempted if policy != None
	/// - If compression fails threshold, stored_data = logical_data
	/// - Checksum covers stored_data (compressed if applicable)
	pub fn write(
		buf: &mut [u8],
		object_type: ObjectType,
		compression_policy: CompressionAlgorithm,
		encryption: u8,
		logical_data: &[u8],
	) -> Result<usize> {
		// Attempt compression if policy != None
		let (stored_data_vec, actual_compression) = if compression_policy == CompressionAlgorithm::None {
			(logical_data.to_vec(), CompressionAlgorithm::None)
		} else {
			match compress(logical_data, compression_policy)? {
				Some(compressed) => (compressed, compression_policy),
				None => (logical_data.to_vec(), CompressionAlgorithm::None),
			}
		};

		let stored_data = stored_data_vec.as_slice();
		let stored_len = stored_data.len() as u32;
		let logical_len = logical_data.len() as u32;

		// Compute object_id from canonical (uncompressed) data
		let object_id = blake3_hash(logical_data);

		// Build header (without checksum first)
		let header = ObjectRecordHeader {
			magic: OBJECT_RECORD_MAGIC,
			object_type,
			compression: actual_compression.to_u8(),
			encryption,
			stored_len,
			logical_len,
			checksum: 0, // Placeholder
			object_id,
		};

		let total_len = ObjectRecordHeader::SIZE + stored_len as usize;

		if buf.len() < total_len {
			return Err(Error::BufferTooSmall {
				required: total_len,
				available: buf.len(),
			});
		}

		// Encode header without checksum
		header.encode(&mut buf[0..ObjectRecordHeader::SIZE])?;

		// Copy stored payload (compressed or not); use NT stores for large payloads
		// to avoid evicting hot BTree metadata from L1/L2 (§15.3).
		if stored_data.len() > 4096 {
			// SAFETY: buf[ObjectRecordHeader::SIZE..] is valid for stored_data.len() bytes
			// (validated by the BufferTooSmall check above). stored_data and buf do not
			// alias (distinct allocations). sfence() follows to order NT stores before
			// any subsequent storage barrier (§15.3).
			unsafe {
				nt_write_payload(
					buf[ObjectRecordHeader::SIZE..].as_mut_ptr(),
					stored_data.as_ptr(),
					stored_data.len(),
				);
			}
			sfence(); // MUST precede any subsequent storage barrier (§15.3)
		} else {
			buf[ObjectRecordHeader::SIZE..total_len].copy_from_slice(stored_data);
		}

		// Compute checksum over stored bytes (excludes checksum field)
		let mut checksum_data = Vec::new();
		checksum_data.extend_from_slice(&buf[0..20]); // magic through logical_len
		checksum_data.extend_from_slice(&buf[28..total_len]); // object_id through payload

		let checksum = blake3_checksum(&checksum_data);

		// Write checksum
		write_u64(&mut buf[20..], checksum)?;

		Ok(total_len)
	}

	/// Write an ObjectRecord with pre-determined stored data (for testing/advanced usage)
	///
	/// This variant allows passing already-compressed data directly.
	/// Useful for testing corruption scenarios.
	pub fn write_with_stored_data(
		buf: &mut [u8],
		object_type: ObjectType,
		compression: u8,
		encryption: u8,
		logical_data: &[u8],
		stored_data: &[u8],
	) -> Result<usize> {
		let stored_len = stored_data.len() as u32;
		let logical_len = logical_data.len() as u32;

		// Compute object_id from logical data
		let object_id = blake3_hash(logical_data);

		// Build header (without checksum first)
		let header = ObjectRecordHeader {
			magic: OBJECT_RECORD_MAGIC,
			object_type,
			compression,
			encryption,
			stored_len,
			logical_len,
			checksum: 0, // Placeholder
			object_id,
		};

		let total_len = ObjectRecordHeader::SIZE + stored_len as usize;

		if buf.len() < total_len {
			return Err(Error::BufferTooSmall {
				required: total_len,
				available: buf.len(),
			});
		}

		// Encode header without checksum
		header.encode(&mut buf[0..ObjectRecordHeader::SIZE])?;

		// Copy payload; use NT stores for large payloads (§15.3).
		if stored_data.len() > 4096 {
			// SAFETY: same invariants as ObjectRecordWriter::write; the ranges
			// are non-overlapping, both valid for stored_data.len() bytes, and
			// sfence follows.
			unsafe {
				nt_write_payload(
					buf[ObjectRecordHeader::SIZE..].as_mut_ptr(),
					stored_data.as_ptr(),
					stored_data.len(),
				);
			}
			sfence(); // MUST precede any subsequent storage barrier (§15.3)
		} else {
			buf[ObjectRecordHeader::SIZE..total_len].copy_from_slice(stored_data);
		}

		// Compute checksum (excludes checksum field)
		let mut checksum_data = Vec::new();
		checksum_data.extend_from_slice(&buf[0..20]); // magic through logical_len
		checksum_data.extend_from_slice(&buf[28..total_len]); // object_id through payload

		let checksum = blake3_checksum(&checksum_data);

		// Write checksum
		write_u64(&mut buf[20..], checksum)?;

		Ok(total_len)
	}
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	// `vec!` is only used by this module's fixtures, so the macro is imported
	// here rather than at the parent's scope.
	use alloc::boxed::Box;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_record_object_type_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let types = [
			ObjectType::Data,
			ObjectType::Inode,
			ObjectType::BTreeNode,
			ObjectType::BTreeLeaf,
			ObjectType::ExtentMap,
			ObjectType::DedupeEntry,
			ObjectType::Root,
		];

		// -- Exec & Check
		for obj_type in types {
			let val = obj_type.to_u16();
			let decoded = ObjectType::from_u16(val)?;
			assert_eq!(obj_type, decoded);
		}

		Ok(())
	}

	#[test]
	fn test_record_invalid_object_type() -> TestResult<()> {
		// -- Setup & Fixtures
		let invalid_type = 0xFFFFu16;

		// -- Exec & Check
		let result = ObjectType::from_u16(invalid_type);
		assert!(matches!(result, Err(Error::InvalidObjectType { .. })));

		Ok(())
	}

	#[test]
	fn test_record_header_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; ObjectRecordHeader::SIZE];
		let object_id = blake3_hash(b"test data");

		let header = ObjectRecordHeader {
			magic: OBJECT_RECORD_MAGIC,
			object_type: ObjectType::Data,
			compression: 0,
			encryption: 0,
			stored_len: 100,
			logical_len: 100,
			checksum: 0x123456789ABCDEF0,
			object_id,
		};

		// -- Exec
		header.encode(&mut buf)?;
		let decoded = ObjectRecordHeader::decode(&buf)?;

		// -- Check
		assert_eq!(decoded.magic, header.magic);
		assert_eq!(decoded.object_type, header.object_type);
		assert_eq!(decoded.compression, header.compression);
		assert_eq!(decoded.encryption, header.encryption);
		assert_eq!(decoded.stored_len, header.stored_len);
		assert_eq!(decoded.logical_len, header.logical_len);
		assert_eq!(decoded.checksum, header.checksum);
		assert_eq!(decoded.object_id, header.object_id);

		Ok(())
	}

	#[test]
	fn test_record_write_and_read() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 1024];
		let logical_data = b"test payload data";

		// -- Exec
		let written = ObjectRecordWriter::write(
			&mut buf,
			ObjectType::Data,
			CompressionAlgorithm::None,
			0, // no encryption
			logical_data,
		)?;

		let (header, payload) = ObjectRecordReader::read(&buf[..written])?;

		// -- Check
		assert_eq!(header.object_type, ObjectType::Data);
		assert_eq!(header.stored_len, logical_data.len() as u32);
		assert_eq!(header.logical_len, logical_data.len() as u32);
		assert_eq!(payload, logical_data);

		// Verify object_id
		let expected_id = blake3_hash(logical_data);
		assert_eq!(header.object_id, expected_id);

		Ok(())
	}

	#[test]
	fn test_record_invalid_magic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; ObjectRecordHeader::SIZE];
		write_u64(&mut buf[0..], 0xDEADBEEF)?; // Invalid magic

		// -- Exec & Check
		let result = ObjectRecordHeader::decode(&buf);
		assert!(matches!(result, Err(Error::InvalidMagic { .. })));

		Ok(())
	}

	#[test]
	fn test_record_checksum_verification() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 1024];
		let logical_data = b"test data";

		// Write valid record
		let written =
			ObjectRecordWriter::write(&mut buf, ObjectType::Data, CompressionAlgorithm::None, 0, logical_data)?;

		// Corrupt checksum
		write_u64(&mut buf[20..], 0xDEADBEEF)?;

		// -- Exec & Check
		let result = ObjectRecordReader::read(&buf[..written]);
		assert!(matches!(result, Err(Error::ChecksumMismatch)));

		Ok(())
	}

	#[test]
	fn test_record_deterministic_output() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf1 = vec![0u8; 1024];
		let mut buf2 = vec![0u8; 1024];
		let data = b"test data";

		// -- Exec
		let len1 = ObjectRecordWriter::write(&mut buf1, ObjectType::Data, CompressionAlgorithm::None, 0, data)?;
		let len2 = ObjectRecordWriter::write(&mut buf2, ObjectType::Data, CompressionAlgorithm::None, 0, data)?;

		// -- Check (same input -> same output)
		assert_eq!(len1, len2);
		assert_eq!(&buf1[..len1], &buf2[..len2]);

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// This crate is built on target with `default-features = false`, so the
	// `lz4` and `zstd` decoders are absent there and this test cannot run.
	#[cfg(feature = "std")]
	fn test_record_compression_zstd() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 16384];
		let logical_data = vec![0u8; 8192]; // Highly compressible

		// -- Exec
		let written =
			ObjectRecordWriter::write(&mut buf, ObjectType::Data, CompressionAlgorithm::Zstd, 0, &logical_data)?;

		let (header, payload) = ObjectRecordReader::read(&buf[..written])?;

		// -- Check
		assert_eq!(header.compression, CompressionAlgorithm::Zstd.to_u8());
		assert!(header.stored_len < header.logical_len); // Compressed
		assert_eq!(payload, logical_data); // Decompressed correctly

		// Verify object_id from canonical data
		let expected_id = blake3_hash(&logical_data);
		assert_eq!(header.object_id, expected_id);

		Ok(())
	}

	#[test]
	fn test_record_compression_threshold_not_met() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 1024];
		let logical_data = vec![1, 2, 3, 4]; // Below 4KiB threshold

		// -- Exec
		let written =
			ObjectRecordWriter::write(&mut buf, ObjectType::Data, CompressionAlgorithm::Zstd, 0, &logical_data)?;

		let (header, _payload) = ObjectRecordReader::read(&buf[..written])?;

		// -- Check
		assert_eq!(header.compression, CompressionAlgorithm::None.to_u8());
		assert_eq!(header.stored_len, header.logical_len); // Not compressed

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// This crate is built on target with `default-features = false`, so the
	// `lz4` and `zstd` decoders are absent there and this test cannot run.
	#[cfg(feature = "std")]
	fn test_record_compression_ratio_not_met() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 16384];
		// Pseudo-random data that won't compress well
		let mut logical_data = Vec::new();
		for i in 0..8192 {
			logical_data.push((i * 7 + 13) as u8);
		}

		// -- Exec
		let written =
			ObjectRecordWriter::write(&mut buf, ObjectType::Data, CompressionAlgorithm::Zstd, 0, &logical_data)?;

		let (_header, payload) = ObjectRecordReader::read(&buf[..written])?;

		// -- Check
		// May or may not compress depending on actual ratio
		// Just verify round-trip works
		assert_eq!(payload, logical_data);

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// This crate is built on target with `default-features = false`, so the
	// `lz4` and `zstd` decoders are absent there and this test cannot run.
	#[cfg(feature = "std")]
	fn test_record_corrupted_compressed_data() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 16384];
		let logical_data = vec![0u8; 8192];

		// Write compressed record
		let written =
			ObjectRecordWriter::write(&mut buf, ObjectType::Data, CompressionAlgorithm::Zstd, 0, &logical_data)?;

		// Corrupt compressed payload (after header)
		buf[ObjectRecordHeader::SIZE + 10] ^= 0xFF;

		// Recalculate checksum to bypass checksum validation
		// This tests decompression error handling
		let mut checksum_data = Vec::new();
		checksum_data.extend_from_slice(&buf[0..20]);
		checksum_data.extend_from_slice(&buf[28..written]);
		let new_checksum = blake3_checksum(&checksum_data);
		write_u64(&mut buf[20..], new_checksum)?;

		// -- Exec & Check
		let result = ObjectRecordReader::read(&buf[..written]);
		// Should fail on decompression or object_id mismatch
		assert!(result.is_err());

		Ok(())
	}
}

// endregion: --- Tests
