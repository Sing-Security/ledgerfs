//! Chunk object type - plaintext data chunks with optional compression/encryption flags

extern crate alloc;

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use super::object_id::ObjectId;
use crate::error::{Error, Result};

/// Plaintext data chunk (canonical encoding)
///
/// Compression and encryption flags are reserved for future use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
	/// Content-addressed ID of this chunk
	pub object_id: ObjectId,

	/// Actual data bytes (plaintext for Phase 2)
	pub data: Vec<u8>,

	/// Compression flags (reserved, must be 0)
	pub compression_flags: u8,

	/// Encryption flags (reserved, must be 0)
	pub encryption_flags: u8,
}

impl Chunk {
	/// Create new chunk from data
	pub fn new(data: Vec<u8>) -> Self {
		let object_id = ObjectId::from_content(&data);
		Self {
			object_id,
			data,
			compression_flags: 0,
			encryption_flags: 0,
		}
	}

	/// Encode chunk to canonical bytes — **the raw data, and nothing else**.
	///
	/// # Why a chunk is not postcard-framed like every other object
	///
	/// `Inode`/`DirNode`/`ExtentNode` are structured records whose canonical
	/// form has to be self-describing, so they serialize with postcard. A
	/// chunk is not a record: it is a span of file content, and every reader
	/// of it wants exactly those bytes.
	///
	/// This used to postcard-serialize the whole struct
	/// (`[object_id][len][data][flags][flags]`), which put three independent
	/// consumers into disagreement — `forge-builder`'s `populate.rs` and the
	/// kernel's `reader.rs` both treat a chunk object's stored payload as raw
	/// file bytes, while the CoW write path wrote it framed. Nothing caught
	/// it: the record's `object_id` really is `blake3(canonical_bytes)`, so
	/// verification passed and only the *interpretation* was wrong. A
	/// 31-byte file written through `/data` read back as the first 31 bytes
	/// of the chunk's own BLAKE3 id — garbage of exactly the right length.
	///
	/// Making raw the canonical form settles it in favour of the two
	/// consumers that already agreed, needs no on-disk change to `/system`,
	/// and makes `object_id == blake3(canonical) == blake3(data)` hold
	/// universally — the same identity [`new`](Self::new) and
	/// [`verify`](Self::verify) already use.
	///
	/// `compression_flags`/`encryption_flags` are consequently not persisted
	/// here. They are declared reserved-and-zero, and the `ObjectRecord`
	/// header that wraps every object on disk already carries its own
	/// compression/encryption flag bytes (`reader.rs` checks them before
	/// accepting a record) — that header, not a duplicate inside the
	/// payload, is where this metadata belongs.
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		Ok(self.data.clone())
	}

	/// Decode chunk from canonical bytes — see [`encode_canonical`](Self::encode_canonical).
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		Ok(Self::new(bytes.to_vec()))
	}

	/// Get size of chunk data
	pub fn size(&self) -> usize {
		self.data.len()
	}

	/// Verify object ID matches content
	pub fn verify(&self) -> bool {
		let expected_id = ObjectId::from_content(&self.data);
		self.object_id == expected_id
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	#[test]
	fn test_chunk_new() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![1, 2, 3, 4, 5];

		// -- Exec
		let chunk = Chunk::new(data.clone());

		// -- Check
		assert_eq!(chunk.data, data);
		assert_eq!(chunk.compression_flags, 0);
		assert_eq!(chunk.encryption_flags, 0);
		assert!(chunk.verify());

		Ok(())
	}

	/// Regression test for the `/data` garbled-content bug: a chunk's stored
	/// bytes must be *exactly* its data, with no framing.
	///
	/// Three independent consumers read chunk objects — `forge-builder`'s
	/// `populate.rs` (writes `/system`), the kernel's `reader.rs` (reads
	/// both partitions), and `cow::operations`/`fs-cbl` (writes and reads
	/// `/data`). The first two treat the stored payload as raw file bytes.
	/// When `encode_canonical` postcard-framed the whole struct instead, a
	/// file written through `/data` read back as the leading bytes of its
	/// own BLAKE3 id — and the record-level hash check could not catch it,
	/// because the framed bytes really did hash to the stored id. Asserting
	/// on the exact byte sequence, not just a round trip, is what pins the
	/// convention: a round-trip-only test passes under either encoding.
	#[test]
	fn test_chunk_canonical_bytes_are_the_raw_data_with_no_framing() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = b"phase1e-reboot-survival-payload".to_vec();
		let chunk = Chunk::new(data.clone());

		// -- Exec
		let canonical = chunk.encode_canonical()?;

		// -- Check
		assert_eq!(canonical, data, "stored bytes must be the file content itself");
		assert_eq!(
			chunk.object_id,
			ObjectId::from_content(&canonical),
			"object_id must equal blake3(canonical) — the invariant ObjectRecordReader::read enforces"
		);
		// And what a raw-bytes writer (forge-builder) stores decodes back to
		// the same chunk a CoW writer would have produced.
		let decoded = Chunk::decode_canonical(&data)?;
		assert_eq!(decoded.data, data);
		assert_eq!(decoded.object_id, chunk.object_id);
		assert!(decoded.verify());

		Ok(())
	}

	#[test]
	fn test_chunk_encode_decode_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![10, 20, 30, 40, 50];
		let chunk1 = Chunk::new(data.clone());
		let chunk2 = Chunk::new(data.clone());

		// -- Exec
		let encoded1 = chunk1.encode_canonical()?;
		let encoded2 = chunk2.encode_canonical()?;

		// -- Check
		assert_eq!(encoded1, encoded2); // Deterministic encoding

		let decoded1 = Chunk::decode_canonical(&encoded1)?;
		let decoded2 = Chunk::decode_canonical(&encoded2)?;

		assert_eq!(decoded1, decoded2);
		assert_eq!(decoded1.object_id, chunk1.object_id);

		Ok(())
	}

	#[test]
	fn test_chunk_object_id_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![100, 200];

		// -- Exec
		let chunk1 = Chunk::new(data.clone());
		let chunk2 = Chunk::new(data.clone());

		// -- Check
		assert_eq!(chunk1.object_id, chunk2.object_id);

		Ok(())
	}

	#[test]
	fn test_chunk_verify() -> TestResult<()> {
		// -- Setup & Fixtures
		let chunk = Chunk::new(vec![1, 2, 3]);

		// -- Check
		assert!(chunk.verify());

		// Corrupt the ID
		let mut corrupted = chunk.clone();
		corrupted.object_id = ObjectId::zero();
		assert!(!corrupted.verify());

		Ok(())
	}
}
