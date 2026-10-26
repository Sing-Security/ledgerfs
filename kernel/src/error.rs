//! Error types for LedgerFS
//!
//! Following the Rust10x error pattern without thiserror/anyhow.

use core::fmt;

/// Result type for LedgerFS operations.
pub type Result<T> = core::result::Result<T, Error>;

/// LedgerFS error types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
	// -- Encoding Errors
	/// Invalid magic number
	InvalidMagic {
		/// Magic value the decoder expected.
		expected: u64,
		/// Magic value actually read from the buffer.
		found: u64,
	},
	/// Checksum verification failed
	ChecksumMismatch,
	/// Record length out of bounds
	InvalidRecordLength {
		/// Length value that was rejected as out of range.
		len: u32,
	},
	/// Buffer too small
	BufferTooSmall {
		/// Number of bytes the operation needed.
		required: usize,
		/// Number of bytes the provided buffer actually holds.
		available: usize,
	},

	// -- Record Errors
	/// Invalid object type
	InvalidObjectType {
		/// Discriminator that is not a known object type (§7.2).
		type_id: u16,
	},
	/// Stored hash does not match computed hash
	StoredHashMismatch,
	/// Object ID verification failed
	ObjectIdMismatch,

	// -- Superblock Errors
	/// Superblock checksum invalid
	SuperblockChecksumInvalid,
	/// Superblock format version this build does not implement (§43.1)
	UnsupportedSuperblockVersion {
		/// Version the superblock declares.
		found: u32,
		/// Version this build writes and accepts.
		supported: u32,
	},
	/// No valid superblock found
	NoValidSuperblock,
	/// RLR entry checksum invalid
	RlrEntryChecksumInvalid {
		/// Index of the ring entry whose checksum failed.
		index: usize,
	},
	/// Invalid RLR head/tail pointers
	InvalidRlrPointers {
		/// Ring head pointer that was read.
		head: u32,
		/// Ring tail pointer that was read.
		tail: u32,
	},

	// -- Intent Log Errors
	/// Intent log sequence collision detected
	IntentLogSeqCollision {
		/// Sequence number carried by the conflicting records.
		seq: u64,
	},
	/// Intent log unreliable
	IntentLogUnreliable,
	/// Invalid intent record type
	InvalidIntentRecordType {
		/// Discriminator that is not a known intent record type (§16.2).
		type_id: u16,
	},
	/// Transaction ID mismatch
	TxIdMismatch {
		/// Transaction ID the operation expected.
		expected: u64,
		/// Transaction ID actually found in the record.
		found: u64,
	},

	// -- Mount Errors
	/// Commit sequence continuity violated
	CommitSeqContinuityViolated {
		/// Commit sequence the continuity check expected.
		expected: u64,
		/// Commit sequence actually found.
		found: u64,
	},
	/// Split-brain detected (same seq, different roots)
	SplitBrainDetected {
		/// Commit sequence at which two differing roots were found.
		seq: u64,
	},
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
			Self::UnsupportedSuperblockVersion { found, supported } => {
				write!(
					f,
					"Superblock format version {} is not supported by this build (which writes and accepts {})",
					found, supported
				)
			}
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

// Implement core::error::Error unconditionally; `core::error::Error` is
// no_std compatible (stable since Rust 1.81) and integration tests import
// the library in non-test mode where #[cfg(test)] does not apply.
impl core::error::Error for Error {}
