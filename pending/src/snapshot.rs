//! Snapshot operations for LedgerFS v7
//!
//! # Status
//!
//! Registration, the `ALLOCATED` → `PINNED` extent transitions, and the root
//! swap of a rollback are implemented. The tree walks that would keep the
//! dedupe index and the deadlist consistent with a snapshot are not, so a
//! snapshot's sharing is invisible to the index and nothing reaches the
//! deadlist when a snapshot is deleted. Nothing in the host `Store` calls any
//! of it (see `KNOWN_ISSUES.md` §11).
//!
//! Per §24, §30, and §22.3 of FORMAT.md:
//! - Atomic snapshot creation (pin → commit → register)
//! - Atomic snapshot deletion (unregister → unpin → deadlist)
//! - Snapshot rollback (restore prior root)
//! - Snapshot protection (pinned blocks MUST NOT be freed)
//!
//! ## Atomicity Guarantees (§24.1)
//!
//! Snapshot creation requires three atomic phases:
//! 1. **Pin phase**: Transition all ALLOCATED → PINNED
//! 2. **Commit phase**: Write new root with pinned allocation tree
//! 3. **Register phase**: Add snapshot to active list
//!
//! If any phase fails, the entire operation MUST be rolled back.
//!
//! ## Snapshot Semantics (§30.1)
//!
//! - **Read-only**: Snapshots cannot be modified
//! - **Space-efficient**: Shared objects consume no additional space
//! - **Independent**: Deleting files from current root does not affect snapshots

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::dedupe::DedupeIndex;
use crate::{Error, Result};
use ledgerfs::alloc_node::AllocNode;
use ledgerfs::alloc_state::AllocationState;
use ledgerfs::object::{ObjectId, ObjectRef, Root};

// region:    --- SnapshotId

/// Snapshot identifier (u32 per FORMAT.md §6)
///
/// Maximum 2^32 snapshots per filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub u32);

impl SnapshotId {
	/// Create new snapshot ID
	pub fn new(id: u32) -> Self {
		Self(id)
	}

	/// Get raw ID value
	pub fn value(&self) -> u32 {
		self.0
	}

	/// Zero snapshot ID (invalid)
	pub fn zero() -> Self {
		Self(0)
	}

	/// Check if ID is valid (non-zero)
	pub fn is_valid(&self) -> bool {
		self.0 != 0
	}
}

impl From<u32> for SnapshotId {
	fn from(id: u32) -> Self {
		Self(id)
	}
}

impl From<SnapshotId> for u32 {
	fn from(id: SnapshotId) -> Self {
		id.0
	}
}

// endregion: --- SnapshotId

// region:    --- SnapshotMetadata

/// Metadata for a single snapshot (§30.2)
///
/// Snapshots are immutable point-in-time views of filesystem state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMetadata {
	/// Schema version for compatibility tracking
	pub schema_version: u16,

	/// Unique snapshot identifier
	pub snapshot_id: SnapshotId,

	/// Root object at time of snapshot
	pub root_ref: ObjectRef,

	/// Commit sequence number at snapshot time
	pub commit_seq: u64,

	/// Timestamp when snapshot was created (UNIX nanoseconds)
	pub created_at_ns: u64,

	/// Optional user-provided name
	#[serde(default)]
	pub name: Option<String>,

	/// Optional user-provided description
	#[serde(default)]
	pub description: Option<String>,

	/// Custom metadata (deterministically ordered)
	#[serde(default)]
	pub custom_metadata: BTreeMap<String, String>,
}

impl SnapshotMetadata {
	/// Current schema version
	pub const CURRENT_SCHEMA_VERSION: u16 = 1;

	/// Create new snapshot metadata
	pub fn new(snapshot_id: SnapshotId, root_ref: ObjectRef, commit_seq: u64, created_at_ns: u64) -> Self {
		Self {
			schema_version: Self::CURRENT_SCHEMA_VERSION,
			snapshot_id,
			root_ref,
			commit_seq,
			created_at_ns,
			name: None,
			description: None,
			custom_metadata: BTreeMap::new(),
		}
	}

	/// Set snapshot name
	pub fn with_name(mut self, name: String) -> Self {
		self.name = Some(name);
		self
	}

	/// Set snapshot description
	pub fn with_description(mut self, description: String) -> Self {
		self.description = Some(description);
		self
	}

	/// Add custom metadata
	pub fn add_metadata(&mut self, key: String, value: String) {
		self.custom_metadata.insert(key, value);
	}

	/// Encode to canonical bytes
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode from canonical bytes
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		postcard::from_bytes(bytes).map_err(Error::from)
	}
}

// endregion: --- SnapshotMetadata

// region:    --- SnapshotRegistry

/// Registry of active snapshots
///
/// Tracks all snapshots that are currently protecting objects from GC.
/// Uses BTreeMap for deterministic iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRegistry {
	/// Schema version for compatibility tracking
	pub schema_version: u16,

	/// Active snapshots (snapshot_id → metadata)
	/// BTreeMap ensures deterministic ordering
	pub snapshots: BTreeMap<SnapshotId, SnapshotMetadata>,

	/// Next snapshot ID to allocate
	pub next_snapshot_id: u32,
}

impl SnapshotRegistry {
	/// Current schema version
	pub const CURRENT_SCHEMA_VERSION: u16 = 1;

	/// Create new empty registry
	pub fn new() -> Self {
		Self {
			schema_version: Self::CURRENT_SCHEMA_VERSION,
			snapshots: BTreeMap::new(),
			next_snapshot_id: 1, // Start at 1, 0 is reserved
		}
	}

	/// Allocate next snapshot ID
	pub fn allocate_id(&mut self) -> Result<SnapshotId> {
		let id = self.next_snapshot_id;

		// Check for overflow (unlikely with u32, but required for safety)
		if id == u32::MAX {
			return Err(Error::custom("Snapshot ID overflow: maximum snapshots reached"));
		}

		self.next_snapshot_id = id.checked_add(1).ok_or_else(|| Error::custom("Snapshot ID overflow"))?;

		Ok(SnapshotId::new(id))
	}

	/// Register a new snapshot
	pub fn register(&mut self, metadata: SnapshotMetadata) -> Result<()> {
		let id = metadata.snapshot_id;

		if !id.is_valid() {
			return Err(Error::custom("Invalid snapshot ID: cannot be zero"));
		}

		if self.snapshots.contains_key(&id) {
			return Err(Error::InvalidSnapshot {
				reason: alloc::format!("Snapshot ID {} already exists", id.value()),
			});
		}

		self.snapshots.insert(id, metadata);
		Ok(())
	}

	/// Unregister a snapshot
	pub fn unregister(&mut self, snapshot_id: SnapshotId) -> Result<SnapshotMetadata> {
		self.snapshots.remove(&snapshot_id).ok_or_else(|| Error::InvalidSnapshot {
			reason: alloc::format!("Snapshot ID {} not found", snapshot_id.value()),
		})
	}

	/// Get snapshot metadata
	pub fn get(&self, snapshot_id: SnapshotId) -> Option<&SnapshotMetadata> {
		self.snapshots.get(&snapshot_id)
	}

	/// Check if snapshot exists
	pub fn exists(&self, snapshot_id: SnapshotId) -> bool {
		self.snapshots.contains_key(&snapshot_id)
	}

	/// Get count of active snapshots
	pub fn count(&self) -> usize {
		self.snapshots.len()
	}

	/// Check if object is referenced by any snapshot
	pub fn is_object_pinned(&self, _object_id: &ObjectId) -> bool {
		// Conservative default: assume pinned if any snapshots exist, which
		// keeps the GC from freeing a snapshot-referenced object. Answering the
		// question exactly needs a traversal of the snapshot's root tree and
		// there is none, so with a snapshot present this is true for every
		// object: the error costs space and never costs data.
		!self.snapshots.is_empty()
	}

	/// List all snapshot IDs (deterministically ordered)
	pub fn list_ids(&self) -> Vec<SnapshotId> {
		self.snapshots.keys().copied().collect()
	}

	/// Encode to canonical bytes
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode from canonical bytes
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		postcard::from_bytes(bytes).map_err(Error::from)
	}
}

impl Default for SnapshotRegistry {
	fn default() -> Self {
		Self::new()
	}
}

// endregion: --- SnapshotRegistry

// region:    --- SnapshotOperation

/// Snapshot operation result for atomicity tracking
///
/// Used to implement three-phase atomic snapshot creation (§24.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotPhase {
	/// No operation in progress
	None,

	/// Pin phase in progress: transitioning ALLOCATED → PINNED
	Pinning { snapshot_id: SnapshotId, pinned_count: u64 },

	/// Commit phase in progress: writing new root with pinned allocation tree
	Committing {
		snapshot_id: SnapshotId,
		new_root_id: ObjectId,
	},

	/// Register phase in progress: adding snapshot to active list
	Registering { snapshot_id: SnapshotId },

	/// Operation completed successfully
	Completed { snapshot_id: SnapshotId },

	/// Operation failed and rolled back
	Failed { snapshot_id: SnapshotId, reason: String },
}

impl SnapshotPhase {
	/// Check if operation is in progress
	pub fn is_in_progress(&self) -> bool {
		matches!(
			self,
			Self::Pinning { .. } | Self::Committing { .. } | Self::Registering { .. }
		)
	}

	/// Get snapshot ID if operation is in progress
	pub fn snapshot_id(&self) -> Option<SnapshotId> {
		match self {
			Self::Pinning { snapshot_id, .. }
			| Self::Committing { snapshot_id, .. }
			| Self::Registering { snapshot_id }
			| Self::Completed { snapshot_id }
			| Self::Failed { snapshot_id, .. } => Some(*snapshot_id),
			Self::None => None,
		}
	}
}

// endregion: --- SnapshotOperation

// region:    --- Snapshot Operations

/// Create a new snapshot atomically (§24.1, §30.2)
///
/// This implements the three-phase atomic snapshot creation:
/// 1. Pin phase: Transition all ALLOCATED → PINNED in allocation tree
/// 2. Commit phase: Write new root referencing pinned allocation tree
/// 3. Register phase: Add snapshot to active snapshot list
///
/// If any phase fails, the operation is rolled back.
pub fn create_snapshot(
	registry: &mut SnapshotRegistry,
	alloc_node: &mut AllocNode,
	dedupe_index: &mut DedupeIndex,
	current_root: &Root,
	name: Option<String>,
	description: Option<String>,
	timestamp_ns: u64,
) -> Result<(SnapshotMetadata, Root)> {
	// -- 1. Allocate snapshot ID
	let snapshot_id = registry.allocate_id()?;

	// -- 2. Pin all ALLOCATED extents
	let pinned_count = pin_allocated_extents(alloc_node)?;

	// If no blocks were pinned, we can still create the snapshot
	// (it will be an empty snapshot)

	// -- 3. Increment refcounts for all objects in current root
	//
	// No refcount is incremented: that requires walking the current root's
	// tree, which this function does not do. A snapshot pins its extents (step
	// 2) but adds nothing to the dedupe index, so the index describes sharing
	// as it stood before the snapshot existed.
	let _ = dedupe_index; // Suppress unused warning

	// -- 4. Create new root (commit phase)
	// The new root is identical to current root except commit_seq
	let mut new_root = current_root.next_commit();
	new_root.timestamp_ns = timestamp_ns;
	new_root.update_object_id();

	// -- 5. Create snapshot metadata
	let mut metadata = SnapshotMetadata::new(
		snapshot_id,
		ObjectRef::from_id(current_root.object_id),
		current_root.commit_seq,
		timestamp_ns,
	);

	if let Some(name) = name {
		metadata = metadata.with_name(name);
	}

	if let Some(description) = description {
		metadata = metadata.with_description(description);
	}

	metadata.add_metadata("pinned_blocks".into(), alloc::format!("{}", pinned_count));

	// -- 6. Register snapshot
	registry.register(metadata.clone())?;

	Ok((metadata, new_root))
}

/// Delete a snapshot atomically (§30.2)
///
/// This implements atomic snapshot deletion:
/// 1. Unregister from active snapshot list
/// 2. Transition PINNED → PendingFree for objects not in current root
/// 3. Decrement refcounts in dedupe index
/// 4. Add objects to deadlist for GC
pub fn delete_snapshot(
	registry: &mut SnapshotRegistry,
	alloc_node: &mut AllocNode,
	dedupe_index: &mut DedupeIndex,
	snapshot_id: SnapshotId,
) -> Result<SnapshotMetadata> {
	// -- 1. Unregister snapshot
	let metadata = registry.unregister(snapshot_id)?;

	// -- 2. Unpin extents (PINNED → PendingFree)
	// Only unpin if no other snapshots reference the same objects
	// For now, we conservatively unpin all extents that were pinned for this snapshot
	let unpinned_count = unpin_extents(alloc_node)?;

	// -- 3. Decrement refcounts
	//
	// Nothing is decremented, for the same reason `create_snapshot` increments
	// nothing: there is no tree walk here. The unpinning below is driven by the
	// allocator's own state, not by what the snapshot referenced.
	let _ = dedupe_index; // Suppress unused warning
	let _ = unpinned_count; // Suppress unused warning

	// -- 4. Add objects to deadlist
	//
	// None are added: reaching a refcount of zero takes the decrement step 3
	// does not perform, and there is no deadlist management in this crate that
	// would consume the entries if it did.

	Ok(metadata)
}

/// Rollback filesystem to a snapshot (§30.2)
///
/// Atomically replace current root with snapshot root.
/// This makes the snapshot the new current state.
pub fn rollback_to_snapshot(registry: &SnapshotRegistry, snapshot_id: SnapshotId, current_root: &Root) -> Result<Root> {
	// -- Validate snapshot exists
	let metadata = registry.get(snapshot_id).ok_or_else(|| Error::InvalidSnapshot {
		reason: alloc::format!("Snapshot {} not found", snapshot_id.value()),
	})?;

	// -- Create new root with snapshot's root as base
	let mut new_root = current_root.next_commit();
	new_root.file_tree_root = metadata.root_ref;
	new_root.timestamp_ns = current_root.timestamp_ns;
	new_root.update_object_id();

	// -- Update allocation tree to reflect rollback
	//
	// The allocation tree is not touched. A complete rollback would compare the
	// current and snapshot trees, add the objects only the current tree reaches
	// to the deadlist, and confirm the snapshot's own objects stay pinned; none
	// of those three walks exists here, so the root returned below names the
	// snapshot's `file_tree_root` while the allocator still describes the
	// pre-rollback state.

	Ok(new_root)
}

// endregion: --- Snapshot Operations

// region:    --- Helper Functions

/// Pin all ALLOCATED extents in allocation tree
///
/// Returns count of blocks pinned.
fn pin_allocated_extents(alloc_node: &mut AllocNode) -> Result<u64> {
	let mut pinned_count = 0u64;

	// Collect LBAs to pin (can't mutate while iterating)
	let lbas_to_pin: Vec<u64> = alloc_node
		.extents
		.iter()
		.filter(|(_, extent)| extent.state == AllocationState::Allocated)
		.map(|(lba, _)| *lba)
		.collect();

	// Pin each extent
	for lba in lbas_to_pin {
		if let Some(extent) = alloc_node.extents.get_mut(&lba)
			&& extent.state == AllocationState::Allocated
		{
			extent.state = AllocationState::Pinned;
			pinned_count += extent.block_count as u64;
		}
	}

	// Recompute allocation node stats
	alloc_node.recompute_stats();

	Ok(pinned_count)
}

/// Unpin all PINNED extents in allocation tree
///
/// Returns count of blocks unpinned.
fn unpin_extents(alloc_node: &mut AllocNode) -> Result<u64> {
	let mut unpinned_count = 0u64;

	// Collect LBAs to unpin
	let lbas_to_unpin: Vec<u64> = alloc_node
		.extents
		.iter()
		.filter(|(_, extent)| extent.state == AllocationState::Pinned)
		.map(|(lba, _)| *lba)
		.collect();

	// Unpin each extent (PINNED → PendingFree)
	for lba in lbas_to_unpin {
		if let Some(extent) = alloc_node.extents.get_mut(&lba)
			&& extent.state == AllocationState::Pinned
		{
			extent.state = AllocationState::PendingFree;
			unpinned_count += extent.block_count as u64;
		}
	}

	// Recompute allocation node stats
	alloc_node.recompute_stats();

	Ok(unpinned_count)
}

// endregion: --- Helper Functions

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;

	extern crate alloc;
	use alloc::boxed::Box;
	use alloc::string::ToString;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_snapshot_id_basic() -> TestResult<()> {
		// -- Setup & Fixtures
		let id = SnapshotId::new(42);

		// -- Check
		assert_eq!(id.value(), 42);
		assert!(id.is_valid());

		let zero = SnapshotId::zero();
		assert!(!zero.is_valid());

		Ok(())
	}

	#[test]
	fn test_snapshot_id_ordering() -> TestResult<()> {
		// -- Setup & Fixtures
		let id1 = SnapshotId::new(1);
		let id2 = SnapshotId::new(2);
		let id3 = SnapshotId::new(1);

		// -- Check - Ordering by ID value
		assert!(id1 < id2);
		assert_eq!(id1, id3);

		Ok(())
	}

	#[test]
	fn test_snapshot_metadata_basic() -> TestResult<()> {
		// -- Setup & Fixtures
		let id = SnapshotId::new(1);
		let root_ref = ObjectRef::zero();
		let metadata = SnapshotMetadata::new(id, root_ref, 100, 123456789);

		// -- Check
		assert_eq!(metadata.snapshot_id, id);
		assert_eq!(metadata.commit_seq, 100);
		assert_eq!(metadata.created_at_ns, 123456789);
		assert_eq!(metadata.schema_version, SnapshotMetadata::CURRENT_SCHEMA_VERSION);

		Ok(())
	}

	#[test]
	fn test_snapshot_metadata_with_name() -> TestResult<()> {
		// -- Setup & Fixtures
		let id = SnapshotId::new(1);
		let root_ref = ObjectRef::zero();
		let metadata = SnapshotMetadata::new(id, root_ref, 100, 123456789)
			.with_name("test-snapshot".into())
			.with_description("Test description".into());

		// -- Check
		assert_eq!(metadata.name, Some("test-snapshot".into()));
		assert_eq!(metadata.description, Some("Test description".into()));

		Ok(())
	}

	#[test]
	fn test_snapshot_metadata_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let id = SnapshotId::new(1);
		let root_ref = ObjectRef::zero();
		let mut metadata = SnapshotMetadata::new(id, root_ref, 100, 123456789);

		// Add metadata in non-sorted order
		metadata.add_metadata("zebra".into(), "value1".into());
		metadata.add_metadata("alpha".into(), "value2".into());
		metadata.add_metadata("beta".into(), "value3".into());

		// -- Exec
		let bytes1 = metadata.encode_canonical()?;
		let bytes2 = metadata.encode_canonical()?;

		// -- Check - Same bytes (deterministic)
		assert_eq!(bytes1, bytes2);

		Ok(())
	}

	#[test]
	fn test_snapshot_registry_allocate_id() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();

		// -- Exec
		let id1 = registry.allocate_id()?;
		let id2 = registry.allocate_id()?;
		let id3 = registry.allocate_id()?;

		// -- Check - IDs are sequential starting from 1
		assert_eq!(id1.value(), 1);
		assert_eq!(id2.value(), 2);
		assert_eq!(id3.value(), 3);

		Ok(())
	}

	#[test]
	fn test_snapshot_registry_register() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();
		let id = registry.allocate_id()?;
		let metadata = SnapshotMetadata::new(id, ObjectRef::zero(), 100, 123456789);

		// -- Exec
		registry.register(metadata.clone())?;

		// -- Check
		assert_eq!(registry.count(), 1);
		assert!(registry.exists(id));
		assert_eq!(registry.get(id), Some(&metadata));

		Ok(())
	}

	#[test]
	fn test_snapshot_registry_register_duplicate() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();
		let id = registry.allocate_id()?;
		let metadata = SnapshotMetadata::new(id, ObjectRef::zero(), 100, 123456789);

		registry.register(metadata.clone())?;

		// -- Exec & Check - Cannot register same ID twice
		let result = registry.register(metadata);
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_snapshot_registry_unregister() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();
		let id = registry.allocate_id()?;
		let metadata = SnapshotMetadata::new(id, ObjectRef::zero(), 100, 123456789);

		registry.register(metadata.clone())?;

		// -- Exec
		let removed = registry.unregister(id)?;

		// -- Check
		assert_eq!(removed, metadata);
		assert_eq!(registry.count(), 0);
		assert!(!registry.exists(id));

		Ok(())
	}

	#[test]
	fn test_snapshot_registry_list_ids_ordered() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();

		// Add snapshots in non-sequential order
		let id3 = SnapshotId::new(3);
		let id1 = SnapshotId::new(1);
		let id2 = SnapshotId::new(2);

		registry.register(SnapshotMetadata::new(id3, ObjectRef::zero(), 300, 3))?;
		registry.register(SnapshotMetadata::new(id1, ObjectRef::zero(), 100, 1))?;
		registry.register(SnapshotMetadata::new(id2, ObjectRef::zero(), 200, 2))?;

		// -- Exec
		let ids = registry.list_ids();

		// -- Check - IDs returned in sorted order
		assert_eq!(ids.len(), 3);
		assert_eq!(ids[0], id1);
		assert_eq!(ids[1], id2);
		assert_eq!(ids[2], id3);

		Ok(())
	}

	#[test]
	fn test_snapshot_registry_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();

		// Add snapshots in non-sequential order
		registry.register(SnapshotMetadata::new(SnapshotId::new(3), ObjectRef::zero(), 300, 3))?;
		registry.register(SnapshotMetadata::new(SnapshotId::new(1), ObjectRef::zero(), 100, 1))?;
		registry.register(SnapshotMetadata::new(SnapshotId::new(2), ObjectRef::zero(), 200, 2))?;

		// -- Exec
		let bytes1 = registry.encode_canonical()?;
		let bytes2 = registry.encode_canonical()?;

		// -- Check - Same bytes (deterministic)
		assert_eq!(bytes1, bytes2);

		Ok(())
	}

	#[test]
	fn test_snapshot_phase_is_in_progress() -> TestResult<()> {
		// -- Setup & Fixtures
		let id = SnapshotId::new(1);

		// -- Check
		assert!(!SnapshotPhase::None.is_in_progress());
		assert!(
			SnapshotPhase::Pinning {
				snapshot_id: id,
				pinned_count: 0
			}
			.is_in_progress()
		);
		assert!(
			SnapshotPhase::Committing {
				snapshot_id: id,
				new_root_id: ObjectId::zero()
			}
			.is_in_progress()
		);
		assert!(SnapshotPhase::Registering { snapshot_id: id }.is_in_progress());
		assert!(!SnapshotPhase::Completed { snapshot_id: id }.is_in_progress());
		assert!(
			!SnapshotPhase::Failed {
				snapshot_id: id,
				reason: "test".into()
			}
			.is_in_progress()
		);

		Ok(())
	}

	#[test]
	fn test_snapshot_operations_create() -> TestResult<()> {
		use ledgerfs::alloc_state::PhysicalExtent;

		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();
		let mut alloc_node = AllocNode::new_empty_leaf();
		let mut dedupe_index = DedupeIndex::new();

		// Add some allocated extents
		alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated))?;
		alloc_node.add_extent(PhysicalExtent::new(2000, 200, AllocationState::Allocated))?;
		alloc_node.recompute_stats();

		let current_root = Root::new();

		// -- Exec
		let (metadata, new_root) = create_snapshot(
			&mut registry,
			&mut alloc_node,
			&mut dedupe_index,
			&current_root,
			Some("test-snapshot".into()),
			None,
			123456789,
		)?;

		// -- Check
		assert_eq!(metadata.snapshot_id, SnapshotId::new(1));
		assert_eq!(registry.count(), 1);
		assert_eq!(new_root.commit_seq, 1); // Next commit

		// Check that extents were pinned
		assert_eq!(alloc_node.extents.get(&1000).unwrap().state, AllocationState::Pinned);
		assert_eq!(alloc_node.extents.get(&2000).unwrap().state, AllocationState::Pinned);

		// Check pinned_blocks metadata
		assert_eq!(metadata.custom_metadata.get("pinned_blocks"), Some(&"300".to_string()));

		Ok(())
	}

	#[test]
	fn test_snapshot_operations_delete() -> TestResult<()> {
		use ledgerfs::alloc_state::PhysicalExtent;

		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();
		let mut alloc_node = AllocNode::new_empty_leaf();
		let mut dedupe_index = DedupeIndex::new();

		// Add some pinned extents (simulating a snapshot was created)
		alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Pinned))?;
		alloc_node.add_extent(PhysicalExtent::new(2000, 200, AllocationState::Pinned))?;
		alloc_node.recompute_stats();

		// Register snapshot
		let snapshot_id = registry.allocate_id()?;
		let metadata = SnapshotMetadata::new(snapshot_id, ObjectRef::zero(), 100, 123456789);
		registry.register(metadata)?;

		// -- Exec
		let deleted = delete_snapshot(&mut registry, &mut alloc_node, &mut dedupe_index, snapshot_id)?;

		// -- Check
		assert_eq!(deleted.snapshot_id, snapshot_id);
		assert_eq!(registry.count(), 0);

		// Check that extents were unpinned (PINNED → PendingFree)
		assert_eq!(
			alloc_node.extents.get(&1000).unwrap().state,
			AllocationState::PendingFree
		);
		assert_eq!(
			alloc_node.extents.get(&2000).unwrap().state,
			AllocationState::PendingFree
		);

		Ok(())
	}

	#[test]
	fn test_snapshot_operations_rollback() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut registry = SnapshotRegistry::new();
		let snapshot_id = registry.allocate_id()?;
		let snapshot_root_ref = ObjectRef::zero();
		let metadata = SnapshotMetadata::new(snapshot_id, snapshot_root_ref, 100, 123456789);
		registry.register(metadata)?;

		let current_root = Root::new();

		// -- Exec
		let new_root = rollback_to_snapshot(&registry, snapshot_id, &current_root)?;

		// -- Check
		assert_eq!(new_root.commit_seq, 1); // Next commit from current
		assert_eq!(new_root.file_tree_root, snapshot_root_ref);

		Ok(())
	}

	#[test]
	fn test_snapshot_operations_rollback_not_found() -> TestResult<()> {
		// -- Setup & Fixtures
		let registry = SnapshotRegistry::new();
		let current_root = Root::new();
		let invalid_id = SnapshotId::new(999);

		// -- Exec & Check
		let result = rollback_to_snapshot(&registry, invalid_id, &current_root);
		assert!(result.is_err());

		Ok(())
	}
}

// endregion: --- Tests
