//! Error types for LedgerFS
//!
//! Following the Rust10x error pattern without thiserror/anyhow.

use alloc::string::String;
use core::fmt;

pub type Result<T> = core::result::Result<T, Error>;

/// LedgerFS error types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
	// -- Encoding Errors
	/// Invalid magic number
	InvalidMagic { expected: u64, found: u64 },
	/// Checksum verification failed
	ChecksumMismatch,
	/// Record length out of bounds
	InvalidRecordLength { len: u32 },
	/// Buffer too small
	BufferTooSmall { required: usize, available: usize },

	// -- Record Errors
	/// Invalid object type
	InvalidObjectType { type_id: u16 },
	/// Stored hash does not match computed hash
	StoredHashMismatch,
	/// Object ID verification failed
	ObjectIdMismatch,

	// -- Superblock Errors
	/// Superblock checksum invalid
	SuperblockChecksumInvalid,
	/// No valid superblock found
	NoValidSuperblock,
	/// RLR entry checksum invalid
	RlrEntryChecksumInvalid { index: usize },
	/// Invalid RLR head/tail pointers
	InvalidRlrPointers { head: u32, tail: u32 },

	// -- Intent Log Errors
	/// Intent log sequence collision detected
	IntentLogSeqCollision { seq: u64 },
	/// Intent log unreliable
	IntentLogUnreliable,
	/// Invalid intent record type
	InvalidIntentRecordType { type_id: u16 },
	/// Transaction ID mismatch
	TxIdMismatch { expected: u64, found: u64 },

	// -- Mount Errors
	/// Commit sequence continuity violated
	CommitSeqContinuityViolated { expected: u64, found: u64 },
	/// Split-brain detected (same seq, different roots)
	SplitBrainDetected { seq: u64 },
	/// Root verification failed
	RootVerificationFailed,
	/// Replay precondition failed
	ReplayPreconditionFailed,

	// -- Allocation Errors
	/// Invalid operation on allocation
	InvalidAllocationOperation(&'static str),
	/// Extent not found
	ExtentNotFound,
	/// Item not found
	NotFound(&'static str),

	// -- Serialization Errors
	/// Serialization failed
	SerializationFailed(&'static str),
	/// Deserialization failed
	DeserializationFailed(&'static str),
	/// Invalid encoding
	InvalidEncoding(alloc::string::String),
	/// Invalid operation
	InvalidOperation(alloc::string::String),
	/// B-tree key not found
	BTreeKeyNotFound(alloc::string::String),

	// -- Dedupe Errors
	/// Refcount overflow (would exceed u32::MAX)
	RefcountOverflow { object_id: crate::object::ObjectId },
	/// Refcount underflow (would go below 0)
	RefcountUnderflow { object_id: crate::object::ObjectId },
	/// Dedupe entry not found
	DedupeEntryNotFound { object_id: crate::object::ObjectId },
	/// Cannot free object with non-zero refcount
	CannotFreeObjectWithRefcount {
		object_id: crate::object::ObjectId,
		refcount: u32,
	},
	/// Cannot free pinned object
	CannotFreePinnedObject { object_id: crate::object::ObjectId },

	// -- Storage Tier Errors
	/// Invalid storage tier ID
	InvalidStorageTier { tier_id: u8 },
	/// Invalid relocation reason
	InvalidRelocationReason { reason_id: u8 },

	// -- Snapshot Errors
	/// Invalid snapshot operation
	InvalidSnapshot { reason: String },

	// -- GC Errors
	/// Invalid GC mode
	InvalidGcMode { mode_id: u8 },

	// -- Scrubber Errors
	/// Corruption detected during scrub
	CorruptionDetected {
		object_id: crate::object::ObjectId,
		reason: String,
	},
	/// Hash verification failed during scrub
	HashVerificationFailed { object_id: crate::object::ObjectId },

	// -- Generic
	/// Custom error with message
	Custom(&'static str),
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidMagic { expected, found } => {
				write!(f, "Invalid magic: expected 0x{:016X}, found 0x{:016X}", expected, found)
			}
			Self::ChecksumMismatch => write!(f, "Checksum mismatch"),
			Self::InvalidRecordLength { len } => {
				write!(f, "Invalid record length: {}", len)
			}
			Self::BufferTooSmall { required, available } => {
				write!(f, "Buffer too small: required {}, available {}", required, available)
			}
			Self::InvalidObjectType { type_id } => {
				write!(f, "Invalid object type: 0x{:04X}", type_id)
			}
			Self::StoredHashMismatch => write!(f, "Stored hash does not match computed hash"),
			Self::ObjectIdMismatch => write!(f, "Object ID verification failed"),
			Self::SuperblockChecksumInvalid => write!(f, "Superblock checksum invalid"),
			Self::NoValidSuperblock => write!(f, "No valid superblock found"),
			Self::RlrEntryChecksumInvalid { index } => {
				write!(f, "RLR entry {} checksum invalid", index)
			}
			Self::InvalidRlrPointers { head, tail } => {
				write!(f, "Invalid RLR pointers: head={}, tail={}", head, tail)
			}
			Self::IntentLogSeqCollision { seq } => {
				write!(f, "Intent log sequence collision at seq {}", seq)
			}
			Self::IntentLogUnreliable => write!(f, "Intent log deemed unreliable"),
			Self::InvalidIntentRecordType { type_id } => {
				write!(f, "Invalid intent record type: 0x{:04X}", type_id)
			}
			Self::TxIdMismatch { expected, found } => {
				write!(f, "Transaction ID mismatch: expected {}, found {}", expected, found)
			}
			Self::CommitSeqContinuityViolated { expected, found } => {
				write!(
					f,
					"Commit sequence continuity violated: expected {}, found {}",
					expected, found
				)
			}
			Self::SplitBrainDetected { seq } => {
				write!(f, "Split-brain detected at commit_seq {}", seq)
			}
			Self::RootVerificationFailed => write!(f, "Root verification failed"),
			Self::ReplayPreconditionFailed => write!(f, "Replay precondition failed"),
			Self::InvalidAllocationOperation(msg) => write!(f, "Invalid allocation operation: {}", msg),
			Self::ExtentNotFound => write!(f, "Extent not found"),
			Self::NotFound(msg) => write!(f, "Not found: {}", msg),
			Self::SerializationFailed(msg) => write!(f, "Serialization failed: {}", msg),
			Self::DeserializationFailed(msg) => write!(f, "Deserialization failed: {}", msg),
			Self::InvalidEncoding(msg) => write!(f, "Invalid encoding: {}", msg),
			Self::InvalidOperation(msg) => write!(f, "Invalid operation: {}", msg),
			Self::BTreeKeyNotFound(key) => write!(f, "B-tree key not found: {}", key),
			Self::RefcountOverflow { object_id } => {
				write!(f, "Refcount overflow for object {}", object_id.to_hex())
			}
			Self::RefcountUnderflow { object_id } => {
				write!(f, "Refcount underflow for object {}", object_id.to_hex())
			}
			Self::DedupeEntryNotFound { object_id } => {
				write!(f, "Dedupe entry not found for object {}", object_id.to_hex())
			}
			Self::CannotFreeObjectWithRefcount { object_id, refcount } => {
				write!(
					f,
					"Cannot free object {} with refcount {}",
					object_id.to_hex(),
					refcount
				)
			}
			Self::CannotFreePinnedObject { object_id } => {
				write!(f, "Cannot free pinned object {}", object_id.to_hex())
			}
			Self::InvalidStorageTier { tier_id } => {
				write!(f, "Invalid storage tier: {}", tier_id)
			}
			Self::InvalidRelocationReason { reason_id } => {
				write!(f, "Invalid relocation reason: {}", reason_id)
			}
			Self::InvalidSnapshot { reason } => {
				write!(f, "Invalid snapshot operation: {}", reason)
			}
			Self::InvalidGcMode { mode_id } => {
				write!(f, "Invalid GC mode: {}", mode_id)
			}
			Self::CorruptionDetected { object_id, reason } => {
				write!(f, "Corruption detected in object {}: {}", object_id.to_hex(), reason)
			}
			Self::HashVerificationFailed { object_id } => {
				write!(f, "Hash verification failed for object {}", object_id.to_hex())
			}
			Self::Custom(msg) => write!(f, "{}", msg),
		}
	}
}

impl Error {
	/// Create a custom error
	pub fn custom(msg: &'static str) -> Self {
		Self::Custom(msg)
	}

	/// Create invalid operation error (allocation-related)
	pub fn invalid_allocation_operation(msg: &'static str) -> Self {
		Self::InvalidAllocationOperation(msg)
	}

	/// Create not found error
	pub fn not_found(msg: &'static str) -> Self {
		Self::NotFound(msg)
	}

	/// Create an invalid encoding error
	pub fn invalid_encoding(msg: impl Into<alloc::string::String>) -> Self {
		Self::InvalidEncoding(msg.into())
	}

	/// Create an invalid operation error (general)
	pub fn invalid_operation(msg: impl Into<alloc::string::String>) -> Self {
		Self::InvalidOperation(msg.into())
	}

	/// Create a btree key not found error
	pub fn btree_key_not_found(key: impl Into<alloc::string::String>) -> Self {
		Self::BTreeKeyNotFound(key.into())
	}
}

// Implement From<postcard::Error> for Error
impl From<postcard::Error> for Error {
	fn from(e: postcard::Error) -> Self {
		Self::InvalidEncoding(alloc::format!("Postcard error: {:?}", e))
	}
}

// Implement core::error::Error unconditionally — `core::error::Error` is
// no_std compatible (stable since Rust 1.81) and integration tests import
// the library in non-test mode where #[cfg(test)] does not apply.
impl core::error::Error for Error {}
