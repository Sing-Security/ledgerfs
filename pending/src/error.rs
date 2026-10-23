//! Error types for the Tier B subsystems.
//!
//! Same shape as `ledgerfs::Error` (a flat enum over a `Result` alias), and
//! deliberately a *different* type. These variants describe failures the core
//! format cannot produce: a refcount that would overflow, a missing dedupe
//! entry, a corrupt block found by the scrubber. While they lived in the
//! published crate's `Error` they were vocabulary no reachable code could
//! speak, which is the kind of thing that reads as an unfinished feature. Here
//! they are reachable, and there they are gone.

use alloc::string::String;
use core::fmt;

use ledgerfs::object::ObjectId;

pub type Result<T> = core::result::Result<T, Error>;

/// Errors from the dedupe index, snapshots, the scrubber and GC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
	// -- Dedupe Errors
	/// Refcount overflow (would exceed u32::MAX)
	RefcountOverflow { object_id: ObjectId },
	/// Refcount underflow (would go below 0)
	RefcountUnderflow { object_id: ObjectId },
	/// Dedupe entry not found
	DedupeEntryNotFound { object_id: ObjectId },
	/// Cannot free object with non-zero refcount
	CannotFreeObjectWithRefcount { object_id: ObjectId, refcount: u32 },
	/// Cannot free pinned object
	CannotFreePinnedObject { object_id: ObjectId },

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
	CorruptionDetected { object_id: ObjectId, reason: String },
	/// Hash verification failed during scrub
	HashVerificationFailed { object_id: ObjectId },

	// -- Serialization Errors
	/// Serialization or deserialization failed
	InvalidEncoding(String),

	// -- Generic
	/// Custom error with message
	Custom(&'static str),
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
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
			Self::InvalidEncoding(msg) => write!(f, "Invalid encoding: {}", msg),
			Self::Custom(msg) => write!(f, "{}", msg),
		}
	}
}

impl Error {
	/// Create a custom error
	pub fn custom(msg: &'static str) -> Self {
		Self::Custom(msg)
	}

	/// Create an invalid encoding error
	pub fn invalid_encoding(msg: impl Into<String>) -> Self {
		Self::InvalidEncoding(msg.into())
	}
}

// `postcard` is what the snapshot records round-trip through; the message text
// matches `ledgerfs::Error`'s so a failure reads the same on either side.
impl From<postcard::Error> for Error {
	fn from(e: postcard::Error) -> Self {
		Self::InvalidEncoding(alloc::format!("Postcard error: {:?}", e))
	}
}

impl core::error::Error for Error {}
