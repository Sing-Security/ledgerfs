//! Object identifiers and storage hints for LedgerFS

use serde::{Deserialize, Serialize};

/// Content-addressed object identifier (BLAKE3 hash)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
	/// Create ObjectId from bytes
	pub fn from_bytes(bytes: &[u8; 32]) -> Self {
		Self(*bytes)
	}

	/// Get bytes
	pub fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// Create ObjectId from BLAKE3 hash of content
	pub fn from_content(data: &[u8]) -> Self {
		let hash = blake3::hash(data);
		Self(*hash.as_bytes())
	}

	/// Zero ObjectId (for null references)
	pub const fn zero() -> Self {
		Self([0u8; 32])
	}

	/// Check if this is the zero ObjectId
	pub fn is_zero(&self) -> bool {
		self.0.iter().all(|&b| b == 0)
	}

	/// Get hex representation
	pub fn to_hex(&self) -> heapless::String<64> {
		let mut s = heapless::String::new();
		for byte in &self.0 {
			let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{:02x}", byte));
		}
		s
	}
}

impl Default for ObjectId {
	fn default() -> Self {
		Self::zero()
	}
}

/// Storage hint for physical placement.
///
/// The current implementation accepts a hint as an input but does not act on
/// it for actual placement; hints are produced and carried, not yet consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StorageHint(pub u64);

impl StorageHint {
	/// Zero hint (no hint provided)
	pub const fn zero() -> Self {
		Self(0)
	}

	/// Check if this is a zero hint
	pub fn is_zero(&self) -> bool {
		self.0 == 0
	}
}

impl Default for StorageHint {
	fn default() -> Self {
		Self::zero()
	}
}

/// Reference to an object with placement hint
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
	/// Content-addressed object ID
	pub object_id: ObjectId,

	/// Physical storage hint (may be 0)
	pub hint: StorageHint,
}

impl ObjectRef {
	/// Create new object reference
	pub fn new(object_id: ObjectId, hint: StorageHint) -> Self {
		Self { object_id, hint }
	}

	/// Create object reference with no hint
	pub fn from_id(object_id: ObjectId) -> Self {
		Self {
			object_id,
			hint: StorageHint::zero(),
		}
	}

	/// Zero reference (null)
	pub const fn zero() -> Self {
		Self {
			object_id: ObjectId::zero(),
			hint: StorageHint::zero(),
		}
	}

	/// Check if this is a zero reference
	pub fn is_zero(&self) -> bool {
		self.object_id.is_zero()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;

	type Result<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_objectid_from_content_deterministic() -> Result<()> {
		// -- Setup & Fixtures
		let data = b"test data";

		// -- Exec
		let id1 = ObjectId::from_content(data);
		let id2 = ObjectId::from_content(data);

		// -- Check
		assert_eq!(id1, id2);
		assert!(!id1.is_zero());

		Ok(())
	}

	#[test]
	fn test_objectid_zero() -> Result<()> {
		// -- Setup & Fixtures
		let zero = ObjectId::zero();

		// -- Check
		assert!(zero.is_zero());
		assert_eq!(zero.0, [0u8; 32]);

		Ok(())
	}

	#[test]
	fn test_objectid_hex() -> Result<()> {
		// -- Setup & Fixtures
		let id = ObjectId::from_content(b"test");

		// -- Exec
		let hex = id.to_hex();

		// -- Check
		assert_eq!(hex.len(), 64); // 32 bytes * 2 hex chars

		Ok(())
	}

	#[test]
	fn test_objectref_serialization() -> Result<()> {
		// -- Setup & Fixtures
		let obj_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint(12345));

		// -- Exec
		let serialized = postcard::to_allocvec(&obj_ref)?;
		let deserialized: ObjectRef = postcard::from_bytes(&serialized)?;

		// -- Check
		assert_eq!(obj_ref, deserialized);

		Ok(())
	}
}
