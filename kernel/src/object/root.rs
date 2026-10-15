//! Root object - filesystem root descriptor with all tree references

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use super::object_id::{ObjectId, ObjectRef};
use crate::audit::AuditChainHead;
use crate::error::{Error, Result};

/// Root object - top-level descriptor for filesystem state
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
	/// Content-addressed ID of this root
	pub object_id: ObjectId,

	/// Schema version (increment when format changes)
	pub schema_version: u16,

	/// Format version (LedgerFS version)
	pub format_version: u16,

	/// Transaction sequence number (monotonically increasing)
	pub commit_seq: u64,

	/// Root of file tree (directory B-tree)
	pub file_tree_root: ObjectRef,

	/// Root of allocation tree (free space tracking)
	pub alloc_tree_root: ObjectRef,

	/// Root of dedupe index (content hash -> location)
	pub dedupe_index_root: ObjectRef,

	/// Root of deadlist (objects to garbage collect)
	pub deadlist_root: ObjectRef,

	/// Audit chain head pointer
	#[serde(default)]
	pub audit_chain_head: AuditChainHead,

	/// Timestamp of this commit (UNIX nanoseconds)
	pub timestamp_ns: u64,
}

impl Root {
	/// Current schema version
	pub const CURRENT_SCHEMA_VERSION: u16 = 1;

	/// Current format version (LedgerFS v0.7)
	pub const CURRENT_FORMAT_VERSION: u16 = 7;

	/// Create new root with zero commit_seq
	pub fn new() -> Self {
		let mut root = Self {
			object_id: ObjectId::zero(),
			schema_version: Self::CURRENT_SCHEMA_VERSION,
			format_version: Self::CURRENT_FORMAT_VERSION,
			commit_seq: 0,
			file_tree_root: ObjectRef::zero(),
			alloc_tree_root: ObjectRef::zero(),
			dedupe_index_root: ObjectRef::zero(),
			deadlist_root: ObjectRef::zero(),
			audit_chain_head: AuditChainHead::new(),
			timestamp_ns: 0,
		};
		root.update_object_id();
		root
	}

	/// Create next commit (increment commit_seq)
	pub fn next_commit(&self) -> Self {
		let mut next = self.clone();
		next.commit_seq += 1;
		next.update_object_id();
		next
	}

	/// Update object ID based on current content
	pub fn update_object_id(&mut self) {
		self.object_id = ObjectId::zero();
		let canonical = self.encode_canonical().unwrap();
		self.object_id = ObjectId::from_content(&canonical);
	}

	/// Encode root to canonical bytes
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode root from canonical bytes
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		postcard::from_bytes(bytes).map_err(Error::from)
	}

	/// Verify schema and format versions are compatible
	pub fn verify_versions(&self) -> Result<()> {
		if self.schema_version > Self::CURRENT_SCHEMA_VERSION {
			return Err(Error::invalid_encoding(format!(
				"Unsupported schema version: {} (current: {})",
				self.schema_version,
				Self::CURRENT_SCHEMA_VERSION
			)));
		}

		if self.format_version > Self::CURRENT_FORMAT_VERSION {
			return Err(Error::invalid_encoding(format!(
				"Unsupported format version: {} (current: {})",
				self.format_version,
				Self::CURRENT_FORMAT_VERSION
			)));
		}

		Ok(())
	}
}

impl Default for Root {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_root_new() -> TestResult<()> {
		// -- Setup & Exec
		let root = Root::new();

		// -- Check
		assert_eq!(root.schema_version, Root::CURRENT_SCHEMA_VERSION);
		assert_eq!(root.format_version, Root::CURRENT_FORMAT_VERSION);
		assert_eq!(root.commit_seq, 0);
		assert!(!root.object_id.is_zero());

		Ok(())
	}

	#[test]
	fn test_root_next_commit() -> TestResult<()> {
		// -- Setup & Fixtures
		let root1 = Root::new();

		// -- Exec
		let root2 = root1.next_commit();
		let root3 = root2.next_commit();

		// -- Check
		assert_eq!(root1.commit_seq, 0);
		assert_eq!(root2.commit_seq, 1);
		assert_eq!(root3.commit_seq, 2);

		// Object IDs should be different
		assert_ne!(root1.object_id, root2.object_id);
		assert_ne!(root2.object_id, root3.object_id);

		Ok(())
	}

	#[test]
	fn test_root_encode_decode_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let root1 = Root::new();
		let root2 = Root::new();

		// -- Exec
		let encoded1 = root1.encode_canonical()?;
		let encoded2 = root2.encode_canonical()?;

		// -- Check
		assert_eq!(encoded1, encoded2); // Deterministic encoding

		let decoded1 = Root::decode_canonical(&encoded1)?;
		let decoded2 = Root::decode_canonical(&encoded2)?;

		assert_eq!(decoded1, decoded2);
		assert_eq!(decoded1.object_id, root1.object_id);

		Ok(())
	}

	#[test]
	fn test_root_verify_versions() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut root = Root::new();

		// -- Check - Current versions should verify
		root.verify_versions()?;

		// Future schema version should fail
		root.schema_version = Root::CURRENT_SCHEMA_VERSION + 1;
		assert!(root.verify_versions().is_err());

		// Reset and test format version
		root.schema_version = Root::CURRENT_SCHEMA_VERSION;
		root.format_version = Root::CURRENT_FORMAT_VERSION + 1;
		assert!(root.verify_versions().is_err());

		Ok(())
	}

	#[test]
	fn test_root_schema_version_in_encoding() -> TestResult<()> {
		// -- Setup & Fixtures
		let root = Root::new();

		// -- Exec
		let encoded = root.encode_canonical()?;

		// -- Check - Schema version should be present in encoded data
		let decoded = Root::decode_canonical(&encoded)?;
		assert_eq!(decoded.schema_version, Root::CURRENT_SCHEMA_VERSION);

		Ok(())
	}

	#[test]
	fn test_root_commit_seq_monotonic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut root = Root::new();
		let mut prev_seq = root.commit_seq;

		// -- Exec & Check - Multiple commits should be monotonic
		for _ in 0..10 {
			root = root.next_commit();
			assert!(root.commit_seq > prev_seq);
			prev_seq = root.commit_seq;
		}

		assert_eq!(root.commit_seq, 10);

		Ok(())
	}
}
