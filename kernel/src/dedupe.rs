//! Dedupe Index for content-addressed object deduplication
//!
//! Per §21 of ledgerfs-spec-v7.md:
//! - DedupeIndex maps ObjectId (BLAKE3 hash) to physical locator
//! - Supports hint-miss fallback during reads
//! - Refcount tracking for GC safety
//! - Atomic updates during transaction commits
//!
//! Extensions:
//! - Storage tier metadata for hot/warm/cold/archive classification
//! - Relocation primitives for tier migration
//! - Stale hint handling with verify-by-hash
//! - Repair paths for corrupt locations with fallback

use alloc::collections::BTreeMap;
use serde::{Deserialize, Serialize};

use crate::object::ObjectId;
use crate::{Error, Result};

// region:    --- StorageTier

/// Storage tier classification for object placement
///
/// Tiers allow different storage media with different performance/cost characteristics.
/// No policy is enforced at this layer - policy is handled by higher-level components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum StorageTier {
	/// Hot tier - NVMe/SSD, frequently accessed data
	Hot = 0,

	/// Warm tier - SSD/HDD, moderately accessed data  
	Warm = 1,

	/// Cold tier - HDD, infrequently accessed data
	Cold = 2,

	/// Archive tier - tape/object storage, rarely accessed data
	Archive = 3,
}

impl StorageTier {
	/// Convert from u8
	pub fn from_u8(val: u8) -> Result<Self> {
		match val {
			0 => Ok(Self::Hot),
			1 => Ok(Self::Warm),
			2 => Ok(Self::Cold),
			3 => Ok(Self::Archive),
			_ => Err(Error::InvalidStorageTier { tier_id: val }),
		}
	}

	/// Convert to u8
	pub fn to_u8(self) -> u8 {
		self as u8
	}
}

impl Default for StorageTier {
	fn default() -> Self {
		Self::Hot
	}
}

// endregion: --- StorageTier

// region:    --- DedupeNode

/// Physical locator for an object (LBA + length + tier metadata)
///
/// This represents where an object is physically stored on disk.
/// extension: Added tier, verified flag, and last_verified timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PhysicalLocator {
	/// Logical block address where object is stored
	pub lba: u64,

	/// Stored length of the object record (including header)
	pub record_stored_len: u32,

	/// Storage tier
	#[serde(default)]
	pub tier: StorageTier,

	/// Location verified by hash check
	/// If false, this is an unverified hint
	#[serde(default)]
	pub verified: bool,

	/// Timestamp of last verification (unix epoch seconds, 0 = never)
	#[serde(default)]
	pub last_verified: u64,
}

impl PhysicalLocator {
	/// Create new physical locator with default tier (Hot, unverified)
	pub fn new(lba: u64, record_stored_len: u32) -> Self {
		Self {
			lba,
			record_stored_len,
			tier: StorageTier::Hot,
			verified: false,
			last_verified: 0,
		}
	}

	/// Create new physical locator with specific tier
	pub fn with_tier(lba: u64, record_stored_len: u32, tier: StorageTier) -> Self {
		Self {
			lba,
			record_stored_len,
			tier,
			verified: false,
			last_verified: 0,
		}
	}

	/// Create verified locator (marked as verified with current timestamp)
	pub fn verified(lba: u64, record_stored_len: u32, tier: StorageTier, timestamp: u64) -> Self {
		Self {
			lba,
			record_stored_len,
			tier,
			verified: true,
			last_verified: timestamp,
		}
	}

	/// Zero locator (invalid/unallocated)
	pub const fn zero() -> Self {
		Self {
			lba: 0,
			record_stored_len: 0,
			tier: StorageTier::Hot,
			verified: false,
			last_verified: 0,
		}
	}

	/// Check if this is a zero locator
	pub fn is_zero(&self) -> bool {
		self.lba == 0 && self.record_stored_len == 0
	}

	/// Mark this locator as verified at given timestamp
	pub fn mark_verified(&mut self, timestamp: u64) {
		self.verified = true;
		self.last_verified = timestamp;
	}

	/// Mark this locator as stale (unverified)
	pub fn mark_stale(&mut self) {
		self.verified = false;
	}

	/// Check if locator needs reverification (older than threshold)
	pub fn needs_reverification(&self, current_time: u64, threshold_secs: u64) -> bool {
		if !self.verified {
			return true;
		}
		if self.last_verified == 0 {
			return true;
		}
		current_time.saturating_sub(self.last_verified) > threshold_secs
	}
}

/// Flags for dedupe entry behavior
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupeFlags {
	/// Object is pinned (cannot be freed even if refcount reaches 0)
	pub pinned: bool,

	/// Object is marked for deletion (pending GC)
	pub pending_delete: bool,

	/// Object location is verified (not just a hint)
	pub verified: bool,
}

impl DedupeFlags {
	/// Create new flags with all false
	pub const fn new() -> Self {
		Self {
			pinned: false,
			pending_delete: false,
			verified: false,
		}
	}

	/// Create verified flags
	pub const fn verified() -> Self {
		Self {
			pinned: false,
			pending_delete: false,
			verified: true,
		}
	}
}

impl Default for DedupeFlags {
	fn default() -> Self {
		Self::new()
	}
}

/// Entry in the dedupe index mapping ObjectId to physical location
///
/// Per §21.1 of spec:
/// - Key: BLAKE3 hash (ObjectId)
/// - Value: { lba, record_stored_len, refcount, flags }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupeNode {
	/// Content-addressed object ID (BLAKE3 hash)
	pub object_id: ObjectId,

	/// Physical location of the object
	pub locator: PhysicalLocator,

	/// Reference count (number of references to this object)
	/// MUST be > 0 while object is in dedupe index
	pub refcount: u32,

	/// Behavioral flags
	pub flags: DedupeFlags,
}

impl DedupeNode {
	/// Create new dedupe node with refcount=1
	pub fn new(object_id: ObjectId, locator: PhysicalLocator) -> Self {
		Self {
			object_id,
			locator,
			refcount: 1,
			flags: DedupeFlags::verified(),
		}
	}

	/// Create dedupe node with specific refcount
	pub fn with_refcount(object_id: ObjectId, locator: PhysicalLocator, refcount: u32) -> Self {
		Self {
			object_id,
			locator,
			refcount,
			flags: DedupeFlags::verified(),
		}
	}

	/// Increment refcount (returns error on overflow)
	pub fn increment_refcount(&mut self) -> Result<()> {
		if self.refcount == u32::MAX {
			return Err(Error::RefcountOverflow {
				object_id: self.object_id,
			});
		}
		self.refcount += 1;
		Ok(())
	}

	/// Decrement refcount (returns error on underflow)
	pub fn decrement_refcount(&mut self) -> Result<()> {
		if self.refcount == 0 {
			return Err(Error::RefcountUnderflow {
				object_id: self.object_id,
			});
		}
		self.refcount -= 1;
		Ok(())
	}

	/// Check if object can be freed (refcount is 0 and not pinned)
	pub fn can_free(&self) -> bool {
		self.refcount == 0 && !self.flags.pinned
	}
}

// endregion: --- DedupeNode

// region:    --- RelocationRecord

/// Record of an object relocation operation
///
/// Tracks the movement of an object from one location to another,
/// typically for tier migration or defragmentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocationRecord {
	/// Object being relocated
	pub object_id: ObjectId,

	/// Old location (now stale)
	pub old_locator: PhysicalLocator,

	/// New location (current)
	pub new_locator: PhysicalLocator,

	/// Timestamp when relocation occurred
	pub timestamp: u64,

	/// Reason for relocation (diagnostic)
	pub reason: RelocationReason,
}

/// Reason for object relocation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum RelocationReason {
	/// Tier migration (hot → warm, etc.)
	TierMigration = 0,

	/// Defragmentation
	Defragmentation = 1,

	/// Corruption repair (found alternate valid copy)
	CorruptionRepair = 2,

	/// Rebalancing across devices
	Rebalancing = 3,
}

impl RelocationReason {
	pub fn from_u8(val: u8) -> Result<Self> {
		match val {
			0 => Ok(Self::TierMigration),
			1 => Ok(Self::Defragmentation),
			2 => Ok(Self::CorruptionRepair),
			3 => Ok(Self::Rebalancing),
			_ => Err(Error::InvalidRelocationReason { reason_id: val }),
		}
	}

	pub fn to_u8(self) -> u8 {
		self as u8
	}
}

// endregion: --- RelocationRecord

// region:    --- DedupeIndex

/// In-memory index mapping ObjectId to physical locations
///
/// This is the minimal dedupe index for:
/// - Hint-miss fallback during reads
/// - Refcount tracking for GC safety
/// - Future deduplication support
///
/// Uses BTreeMap for deterministic iteration order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DedupeIndex {
	/// Map from ObjectId to DedupeNode
	/// BTreeMap ensures deterministic serialization
	entries: BTreeMap<ObjectId, DedupeNode>,
}

impl DedupeIndex {
	/// Create new empty dedupe index
	pub fn new() -> Self {
		Self {
			entries: BTreeMap::new(),
		}
	}

	/// Insert or update a dedupe entry
	///
	/// If object already exists, increments refcount.
	/// Otherwise, creates new entry with refcount=1.
	pub fn insert(&mut self, object_id: ObjectId, locator: PhysicalLocator) -> Result<()> {
		if let Some(node) = self.entries.get_mut(&object_id) {
			// Object exists, increment refcount
			node.increment_refcount()?;
		} else {
			// New object, create entry
			let node = DedupeNode::new(object_id, locator);
			self.entries.insert(object_id, node);
		}
		Ok(())
	}

	/// Lookup physical locator for an object
	///
	/// Returns None if object is not in index (hint miss).
	/// This is the hint-miss fallback path.
	pub fn lookup(&self, object_id: &ObjectId) -> Option<PhysicalLocator> {
		self.entries.get(object_id).map(|node| node.locator)
	}

	/// Get full dedupe node for an object
	pub fn get(&self, object_id: &ObjectId) -> Option<&DedupeNode> {
		self.entries.get(object_id)
	}

	/// Get mutable dedupe node for an object
	pub fn get_mut(&mut self, object_id: &ObjectId) -> Option<&mut DedupeNode> {
		self.entries.get_mut(object_id)
	}

	/// Increment refcount for an object
	///
	/// Returns error if object not found or refcount would overflow.
	pub fn increment_refcount(&mut self, object_id: &ObjectId) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;
		node.increment_refcount()
	}

	/// Decrement refcount for an object
	///
	/// Returns error if object not found or refcount would underflow.
	/// If refcount reaches 0, entry remains in index for GC to process.
	pub fn decrement_refcount(&mut self, object_id: &ObjectId) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;
		node.decrement_refcount()
	}

	/// Remove entry from index (only if refcount is 0)
	///
	/// Returns error if object is pinned or refcount > 0.
	pub fn remove(&mut self, object_id: &ObjectId) -> Result<DedupeNode> {
		let node = self
			.entries
			.get(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;

		if node.refcount > 0 {
			return Err(Error::CannotFreeObjectWithRefcount {
				object_id: *object_id,
				refcount: node.refcount,
			});
		}

		if node.flags.pinned {
			return Err(Error::CannotFreePinnedObject { object_id: *object_id });
		}

		Ok(self.entries.remove(object_id).unwrap())
	}

	/// Mark object as pinned (cannot be freed)
	pub fn pin(&mut self, object_id: &ObjectId) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;
		node.flags.pinned = true;
		Ok(())
	}

	/// Unmark object as pinned
	pub fn unpin(&mut self, object_id: &ObjectId) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;
		node.flags.pinned = false;
		Ok(())
	}

	/// Get number of entries in index
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	/// Check if index is empty
	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// Get all objects with refcount == 0 (candidates for GC)
	pub fn gc_candidates(&self) -> impl Iterator<Item = &DedupeNode> {
		self.entries.values().filter(|node| node.refcount == 0)
	}

	/// Get all pinned objects
	pub fn pinned_objects(&self) -> impl Iterator<Item = &DedupeNode> {
		self.entries.values().filter(|node| node.flags.pinned)
	}

	// region:    --- Relocation Methods

	/// Relocate object to new location
	///
	/// Updates the dedupe entry to point to new location.
	/// Old location becomes stale hint (if verify_old is false).
	/// New location is marked as verified (if verify_new is true).
	///
	/// Returns RelocationRecord for audit trail.
	pub fn relocate(
		&mut self,
		object_id: &ObjectId,
		new_locator: PhysicalLocator,
		timestamp: u64,
		reason: RelocationReason,
	) -> Result<RelocationRecord> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;

		let old_locator = node.locator;
		node.locator = new_locator;

		Ok(RelocationRecord {
			object_id: *object_id,
			old_locator,
			new_locator,
			timestamp,
			reason,
		})
	}

	/// Update location for object (atomic location update)
	///
	/// Similar to relocate() but simpler API for direct location updates.
	pub fn update_location(&mut self, object_id: &ObjectId, new_locator: PhysicalLocator) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;

		node.locator = new_locator;
		Ok(())
	}

	/// Mark object location as stale (unverified hint)
	///
	/// Used when we suspect the hint may be incorrect.
	/// Next read will need to verify-by-hash.
	pub fn mark_location_stale(&mut self, object_id: &ObjectId) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;

		node.locator.mark_stale();
		Ok(())
	}

	/// Mark object location as verified
	///
	/// Called after successful verify-by-hash check.
	pub fn mark_location_verified(&mut self, object_id: &ObjectId, timestamp: u64) -> Result<()> {
		let node = self
			.entries
			.get_mut(object_id)
			.ok_or_else(|| Error::DedupeEntryNotFound { object_id: *object_id })?;

		node.locator.mark_verified(timestamp);
		Ok(())
	}

	/// Get objects that need reverification
	///
	/// Returns objects with unverified or old verification timestamps.
	pub fn objects_needing_verification(
		&self,
		current_time: u64,
		threshold_secs: u64,
	) -> impl Iterator<Item = &DedupeNode> + '_ {
		self.entries
			.values()
			.filter(move |node| node.locator.needs_reverification(current_time, threshold_secs))
	}

	/// Get objects in a specific tier
	///
	/// Useful for tier migration policies.
	pub fn objects_in_tier(&self, tier: StorageTier) -> impl Iterator<Item = &DedupeNode> + '_ {
		self.entries.values().filter(move |node| node.locator.tier == tier)
	}

	/// Count objects per tier (for monitoring/metrics)
	pub fn tier_statistics(&self) -> TierStatistics {
		let mut stats = TierStatistics::default();

		for node in self.entries.values() {
			match node.locator.tier {
				StorageTier::Hot => stats.hot_count += 1,
				StorageTier::Warm => stats.warm_count += 1,
				StorageTier::Cold => stats.cold_count += 1,
				StorageTier::Archive => stats.archive_count += 1,
			}
		}

		stats
	}

	/// Iterate over all entries (deterministic order)
	pub fn iter(&self) -> impl Iterator<Item = (&ObjectId, &DedupeNode)> {
		self.entries.iter()
	}

	// endregion: --- Relocation Methods
}

/// Tier distribution statistics
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierStatistics {
	pub hot_count: usize,
	pub warm_count: usize,
	pub cold_count: usize,
	pub archive_count: usize,
}

// endregion: --- DedupeIndex

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;
	use alloc::format; // needed for format! macro

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_dedupe_physical_locator_basic() -> TestResult<()> {
		// -- Setup & Fixtures
		let locator = PhysicalLocator::new(1234, 4096);

		// -- Check
		assert_eq!(locator.lba, 1234);
		assert_eq!(locator.record_stored_len, 4096);
		assert!(!locator.is_zero());

		Ok(())
	}

	#[test]
	fn test_dedupe_physical_locator_zero() -> TestResult<()> {
		// -- Setup & Fixtures
		let locator = PhysicalLocator::zero();

		// -- Check
		assert!(locator.is_zero());
		assert_eq!(locator.lba, 0);
		assert_eq!(locator.record_stored_len, 0);

		Ok(())
	}

	#[test]
	fn test_dedupe_node_refcount_increment() -> TestResult<()> {
		// -- Setup & Fixtures
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		let mut node = DedupeNode::new(object_id, locator);

		// -- Exec
		assert_eq!(node.refcount, 1);
		node.increment_refcount()?;

		// -- Check
		assert_eq!(node.refcount, 2);

		Ok(())
	}

	#[test]
	fn test_dedupe_node_refcount_decrement() -> TestResult<()> {
		// -- Setup & Fixtures
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		let mut node = DedupeNode::with_refcount(object_id, locator, 2);

		// -- Exec
		node.decrement_refcount()?;

		// -- Check
		assert_eq!(node.refcount, 1);
		assert!(!node.can_free());

		Ok(())
	}

	#[test]
	fn test_dedupe_node_can_free() -> TestResult<()> {
		// -- Setup & Fixtures
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		let mut node = DedupeNode::with_refcount(object_id, locator, 1);

		// -- Exec
		node.decrement_refcount()?;

		// -- Check
		assert_eq!(node.refcount, 0);
		assert!(node.can_free());

		Ok(())
	}

	#[test]
	fn test_dedupe_node_cannot_free_pinned() -> TestResult<()> {
		// -- Setup & Fixtures
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		let mut node = DedupeNode::with_refcount(object_id, locator, 1);
		node.flags.pinned = true;

		// -- Exec
		node.decrement_refcount()?;

		// -- Check
		assert_eq!(node.refcount, 0);
		assert!(!node.can_free()); // Pinned, cannot free

		Ok(())
	}

	#[test]
	fn test_dedupe_index_insert_new() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);

		// -- Exec
		index.insert(object_id, locator)?;

		// -- Check
		assert_eq!(index.len(), 1);
		let node = index.get(&object_id).unwrap();
		assert_eq!(node.refcount, 1);
		assert_eq!(node.locator, locator);

		Ok(())
	}

	#[test]
	fn test_dedupe_index_insert_duplicate() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);

		// -- Exec
		index.insert(object_id, locator)?;
		index.insert(object_id, locator)?; // Same object again

		// -- Check
		assert_eq!(index.len(), 1); // Still only one entry
		let node = index.get(&object_id).unwrap();
		assert_eq!(node.refcount, 2); // Refcount incremented

		Ok(())
	}

	#[test]
	fn test_dedupe_index_lookup_hit() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		index.insert(object_id, locator)?;

		// -- Exec
		let found = index.lookup(&object_id);

		// -- Check
		assert!(found.is_some());
		assert_eq!(found.unwrap(), locator);

		Ok(())
	}

	#[test]
	fn test_dedupe_index_lookup_miss() -> TestResult<()> {
		// -- Setup & Fixtures
		let index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");

		// -- Exec
		let found = index.lookup(&object_id);

		// -- Check
		assert!(found.is_none()); // Hint miss

		Ok(())
	}

	#[test]
	fn test_dedupe_index_increment_refcount() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		index.insert(object_id, locator)?;

		// -- Exec
		index.increment_refcount(&object_id)?;

		// -- Check
		let node = index.get(&object_id).unwrap();
		assert_eq!(node.refcount, 2);

		Ok(())
	}

	#[test]
	fn test_dedupe_index_decrement_refcount() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		index.insert(object_id, locator)?;
		index.increment_refcount(&object_id)?;

		// -- Exec
		index.decrement_refcount(&object_id)?;

		// -- Check
		let node = index.get(&object_id).unwrap();
		assert_eq!(node.refcount, 1);

		Ok(())
	}

	#[test]
	fn test_dedupe_index_remove_with_refcount() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		index.insert(object_id, locator)?;

		// -- Exec & Check
		let result = index.remove(&object_id);
		assert!(result.is_err()); // Cannot remove with refcount > 0

		Ok(())
	}

	#[test]
	fn test_dedupe_index_remove_success() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		index.insert(object_id, locator)?;
		index.decrement_refcount(&object_id)?; // Refcount now 0

		// -- Exec
		let removed = index.remove(&object_id)?;

		// -- Check
		assert_eq!(removed.refcount, 0);
		assert_eq!(index.len(), 0);

		Ok(())
	}

	#[test]
	fn test_dedupe_index_pin_unpin() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"test data");
		let locator = PhysicalLocator::new(100, 4096);
		index.insert(object_id, locator)?;

		// -- Exec
		index.pin(&object_id)?;
		assert!(index.get(&object_id).unwrap().flags.pinned);

		index.unpin(&object_id)?;
		assert!(!index.get(&object_id).unwrap().flags.pinned);

		Ok(())
	}

	#[test]
	fn test_dedupe_index_gc_candidates() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let obj1 = ObjectId::from_content(b"object 1");
		let obj2 = ObjectId::from_content(b"object 2");
		let obj3 = ObjectId::from_content(b"object 3");
		let locator = PhysicalLocator::new(100, 4096);

		index.insert(obj1, locator)?;
		index.insert(obj2, locator)?;
		index.insert(obj3, locator)?;

		index.decrement_refcount(&obj1)?; // obj1 refcount = 0
		index.decrement_refcount(&obj3)?; // obj3 refcount = 0

		// -- Exec
		let candidates: alloc::vec::Vec<_> = index.gc_candidates().collect();

		// -- Check
		assert_eq!(candidates.len(), 2);
		assert!(candidates.iter().any(|n| n.object_id == obj1));
		assert!(candidates.iter().any(|n| n.object_id == obj3));

		Ok(())
	}

	#[test]
	fn test_dedupe_index_deterministic_serialization() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index1 = DedupeIndex::new();
		let mut index2 = DedupeIndex::new();

		let obj1 = ObjectId::from_content(b"zebra");
		let obj2 = ObjectId::from_content(b"alpha");
		let locator = PhysicalLocator::new(100, 4096);

		// Insert in different order
		index1.insert(obj1, locator)?;
		index1.insert(obj2, locator)?;

		index2.insert(obj2, locator)?;
		index2.insert(obj1, locator)?;

		// -- Exec (use postcard instead of json for no_std compatibility)
		let bytes1 = postcard::to_allocvec(&index1)?;
		let bytes2 = postcard::to_allocvec(&index2)?;

		// -- Check (BTreeMap ensures same serialization order)
		assert_eq!(bytes1, bytes2);

		Ok(())
	}

	// region:    --- Tests

	#[test]
	fn test_storage_tier_enum() -> TestResult<()> {
		// -- Check
		assert_eq!(StorageTier::Hot.to_u8(), 0);
		assert_eq!(StorageTier::Warm.to_u8(), 1);
		assert_eq!(StorageTier::Cold.to_u8(), 2);
		assert_eq!(StorageTier::Archive.to_u8(), 3);

		assert_eq!(StorageTier::from_u8(0)?, StorageTier::Hot);
		assert_eq!(StorageTier::from_u8(1)?, StorageTier::Warm);
		assert_eq!(StorageTier::from_u8(2)?, StorageTier::Cold);
		assert_eq!(StorageTier::from_u8(3)?, StorageTier::Archive);

		assert!(StorageTier::from_u8(4).is_err());

		Ok(())
	}

	#[test]
	fn test_physical_locator_with_tier() -> TestResult<()> {
		// -- Setup & Fixtures
		let locator = PhysicalLocator::with_tier(1000, 8192, StorageTier::Warm);

		// -- Check
		assert_eq!(locator.lba, 1000);
		assert_eq!(locator.record_stored_len, 8192);
		assert_eq!(locator.tier, StorageTier::Warm);
		assert!(!locator.verified);
		assert_eq!(locator.last_verified, 0);

		Ok(())
	}

	#[test]
	fn test_physical_locator_verified() -> TestResult<()> {
		// -- Setup & Fixtures
		let timestamp = 1234567890;
		let locator = PhysicalLocator::verified(2000, 16384, StorageTier::Cold, timestamp);

		// -- Check
		assert_eq!(locator.lba, 2000);
		assert_eq!(locator.tier, StorageTier::Cold);
		assert!(locator.verified);
		assert_eq!(locator.last_verified, timestamp);

		Ok(())
	}

	#[test]
	fn test_mark_verified() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut locator = PhysicalLocator::new(500, 4096);
		assert!(!locator.verified);

		// -- Exec
		locator.mark_verified(999999);

		// -- Check
		assert!(locator.verified);
		assert_eq!(locator.last_verified, 999999);

		Ok(())
	}

	#[test]
	fn test_mark_stale() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut locator = PhysicalLocator::verified(500, 4096, StorageTier::Hot, 12345);
		assert!(locator.verified);

		// -- Exec
		locator.mark_stale();

		// -- Check
		assert!(!locator.verified);

		Ok(())
	}

	#[test]
	fn test_needs_reverification() -> TestResult<()> {
		// -- Setup & Fixtures
		let timestamp = 1000;
		let mut locator = PhysicalLocator::verified(500, 4096, StorageTier::Hot, timestamp);

		// -- Check
		// Within threshold - no reverification needed
		assert!(!locator.needs_reverification(timestamp + 100, 3600));

		// Beyond threshold - reverification needed
		assert!(locator.needs_reverification(timestamp + 7200, 3600));

		// Unverified always needs reverification
		locator.mark_stale();
		assert!(locator.needs_reverification(timestamp, 3600));

		Ok(())
	}

	#[test]
	fn test_relocate_object() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"relocate test");
		let old_locator = PhysicalLocator::with_tier(100, 4096, StorageTier::Hot);
		let new_locator = PhysicalLocator::with_tier(5000, 4096, StorageTier::Warm);

		index.insert(object_id, old_locator)?;

		// -- Exec
		let record = index.relocate(&object_id, new_locator, 1234567890, RelocationReason::TierMigration)?;

		// -- Check
		assert_eq!(record.object_id, object_id);
		assert_eq!(record.old_locator, old_locator);
		assert_eq!(record.new_locator, new_locator);
		assert_eq!(record.timestamp, 1234567890);
		assert_eq!(record.reason, RelocationReason::TierMigration);

		// Verify index updated
		let node = index.get(&object_id).unwrap();
		assert_eq!(node.locator, new_locator);

		Ok(())
	}

	#[test]
	fn test_update_location() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"update test");
		let old_locator = PhysicalLocator::new(200, 8192);
		let new_locator = PhysicalLocator::new(3000, 8192);

		index.insert(object_id, old_locator)?;

		// -- Exec
		index.update_location(&object_id, new_locator)?;

		// -- Check
		let node = index.get(&object_id).unwrap();
		assert_eq!(node.locator, new_locator);

		Ok(())
	}

	#[test]
	fn test_mark_location_stale() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"stale test");
		let locator = PhysicalLocator::verified(300, 4096, StorageTier::Hot, 11111);

		index.insert(object_id, locator)?;
		assert!(index.get(&object_id).unwrap().locator.verified);

		// -- Exec
		index.mark_location_stale(&object_id)?;

		// -- Check
		assert!(!index.get(&object_id).unwrap().locator.verified);

		Ok(())
	}

	#[test]
	fn test_mark_location_verified() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let object_id = ObjectId::from_content(b"verify test");
		let locator = PhysicalLocator::new(400, 4096);

		index.insert(object_id, locator)?;
		assert!(!index.get(&object_id).unwrap().locator.verified);

		// -- Exec
		index.mark_location_verified(&object_id, 77777)?;

		// -- Check
		let node = index.get(&object_id).unwrap();
		assert!(node.locator.verified);
		assert_eq!(node.locator.last_verified, 77777);

		Ok(())
	}

	#[test]
	fn test_objects_needing_verification() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let obj1 = ObjectId::from_content(b"obj1");
		let obj2 = ObjectId::from_content(b"obj2");
		let obj3 = ObjectId::from_content(b"obj3");

		// obj1: verified recently. Must be inside the threshold window used
		// below (current_time 2000 - threshold 500 = 1500), or it is stale by
		// the test's own rule and belongs in the expected set. It was 1000,
		// i.e. 1000 seconds old against a 500-second threshold, so this
		// asserted 2 while `needs_reverification` correctly reported 3.
		let loc1 = PhysicalLocator::verified(100, 4096, StorageTier::Hot, 1800);
		index.insert(obj1, loc1)?;

		// obj2: verified long ago (stale)
		let loc2 = PhysicalLocator::verified(200, 4096, StorageTier::Hot, 100);
		index.insert(obj2, loc2)?;

		// obj3: never verified
		let loc3 = PhysicalLocator::new(300, 4096);
		index.insert(obj3, loc3)?;

		// -- Exec
		let current_time = 2000;
		let threshold = 500;
		let needs_verify: alloc::vec::Vec<_> = index.objects_needing_verification(current_time, threshold).collect();

		// -- Check
		assert_eq!(needs_verify.len(), 2); // obj2 and obj3
		assert!(needs_verify.iter().any(|n| n.object_id == obj2));
		assert!(needs_verify.iter().any(|n| n.object_id == obj3));

		Ok(())
	}

	#[test]
	fn test_objects_in_tier() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();
		let obj1 = ObjectId::from_content(b"hot obj");
		let obj2 = ObjectId::from_content(b"warm obj");
		let obj3 = ObjectId::from_content(b"cold obj");

		index.insert(obj1, PhysicalLocator::with_tier(100, 4096, StorageTier::Hot))?;
		index.insert(obj2, PhysicalLocator::with_tier(200, 4096, StorageTier::Warm))?;
		index.insert(obj3, PhysicalLocator::with_tier(300, 4096, StorageTier::Cold))?;

		// -- Exec
		let warm_objects: alloc::vec::Vec<_> = index.objects_in_tier(StorageTier::Warm).collect();

		// -- Check
		assert_eq!(warm_objects.len(), 1);
		assert_eq!(warm_objects[0].object_id, obj2);

		Ok(())
	}

	#[test]
	fn test_tier_statistics() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut index = DedupeIndex::new();

		for i in 0..3 {
			let obj = ObjectId::from_content(&format!("hot{}", i).into_bytes());
			index.insert(obj, PhysicalLocator::with_tier(i as u64, 4096, StorageTier::Hot))?;
		}

		for i in 0..2 {
			let obj = ObjectId::from_content(&format!("warm{}", i).into_bytes());
			index.insert(obj, PhysicalLocator::with_tier(100 + i as u64, 4096, StorageTier::Warm))?;
		}

		for i in 0..4 {
			let obj = ObjectId::from_content(&format!("cold{}", i).into_bytes());
			index.insert(obj, PhysicalLocator::with_tier(200 + i as u64, 4096, StorageTier::Cold))?;
		}

		let obj = ObjectId::from_content(b"archive");
		index.insert(obj, PhysicalLocator::with_tier(500, 4096, StorageTier::Archive))?;

		// -- Exec
		let stats = index.tier_statistics();

		// -- Check
		assert_eq!(stats.hot_count, 3);
		assert_eq!(stats.warm_count, 2);
		assert_eq!(stats.cold_count, 4);
		assert_eq!(stats.archive_count, 1);

		Ok(())
	}

	#[test]
	fn test_relocation_reason_enum() -> TestResult<()> {
		// -- Check
		assert_eq!(RelocationReason::TierMigration.to_u8(), 0);
		assert_eq!(RelocationReason::Defragmentation.to_u8(), 1);
		assert_eq!(RelocationReason::CorruptionRepair.to_u8(), 2);
		assert_eq!(RelocationReason::Rebalancing.to_u8(), 3);

		assert_eq!(RelocationReason::from_u8(0)?, RelocationReason::TierMigration);
		assert_eq!(RelocationReason::from_u8(1)?, RelocationReason::Defragmentation);
		assert_eq!(RelocationReason::from_u8(2)?, RelocationReason::CorruptionRepair);
		assert_eq!(RelocationReason::from_u8(3)?, RelocationReason::Rebalancing);

		assert!(RelocationReason::from_u8(4).is_err());

		Ok(())
	}

	// endregion: --- Tests
}

// endregion: --- Tests
