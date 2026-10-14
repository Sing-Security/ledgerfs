//! Snapshot integration tests with crash simulation
//!
//! Tests for Atomic snapshot operations and crash recovery

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crucible_ledgerfs_kernel::{
	alloc_node::AllocNode,
	alloc_state::{AllocationState, PhysicalExtent},
	dedupe::DedupeIndex,
	object::{ObjectRef, Root},
	snapshot::*,
};

type Result<T> = core::result::Result<T, Box<dyn core::error::Error>>;

// region:    --- Basic Snapshot Operations

#[test]
fn test_snapshot_create_basic() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();

	// Add allocated extents
	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated));
	alloc_node.add_extent(PhysicalExtent::new(2000, 200, AllocationState::Allocated));
	alloc_node.recompute_stats();

	let current_root = Root::new();

	// -- Exec
	let (metadata, new_root) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		Some("test-snapshot".into()),
		Some("Test snapshot description".into()),
		1234567890,
	)?;

	// -- Check
	assert_eq!(metadata.snapshot_id, SnapshotId::new(1));
	assert_eq!(metadata.name, Some("test-snapshot".into()));
	assert_eq!(metadata.description, Some("Test snapshot description".into()));
	assert_eq!(registry.count(), 1);
	assert_eq!(new_root.commit_seq, 1);

	// All allocated extents should be pinned
	for extent in alloc_node.extents.values() {
		assert_eq!(extent.state, AllocationState::Pinned);
	}

	Ok(())
}

#[test]
fn test_snapshot_delete_basic() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();

	// Create snapshot first
	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated));
	alloc_node.recompute_stats();

	let current_root = Root::new();
	let (metadata, _) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		None,
		None,
		1234567890,
	)?;

	let snapshot_id = metadata.snapshot_id;

	// -- Exec
	let deleted = delete_snapshot(&mut registry, &mut alloc_node, &mut dedupe_index, snapshot_id)?;

	// -- Check
	assert_eq!(deleted.snapshot_id, snapshot_id);
	assert_eq!(registry.count(), 0);

	// Extents should be unpinned (PINNED → PendingFree)
	for extent in alloc_node.extents.values() {
		assert_eq!(extent.state, AllocationState::PendingFree);
	}

	Ok(())
}

#[test]
fn test_snapshot_rollback_basic() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let snapshot_id = registry.allocate_id()?;
	let snapshot_root = ObjectRef::zero();

	let metadata = SnapshotMetadata::new(snapshot_id, snapshot_root, 100, 1234567890);
	registry.register(metadata)?;

	let mut current_root = Root::new();
	current_root.commit_seq = 200; // Advanced past snapshot

	// -- Exec
	let rolled_back = rollback_to_snapshot(&registry, snapshot_id, &current_root)?;

	// -- Check
	assert_eq!(rolled_back.commit_seq, 201); // Next commit from current
	assert_eq!(rolled_back.file_tree_root, snapshot_root);

	Ok(())
}

// endregion: --- Basic Snapshot Operations

// region:    --- Multiple Snapshots

#[test]
fn test_snapshot_multiple_lifecycle() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();

	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated));
	alloc_node.recompute_stats();

	let current_root = Root::new();

	// -- Exec: Create 3 snapshots
	let (snap1, _) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		Some("snapshot-1".into()),
		None,
		1000,
	)?;

	let (snap2, _) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		Some("snapshot-2".into()),
		None,
		2000,
	)?;

	let (snap3, _) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		Some("snapshot-3".into()),
		None,
		3000,
	)?;

	// -- Check
	assert_eq!(registry.count(), 3);
	assert_eq!(snap1.snapshot_id, SnapshotId::new(1));
	assert_eq!(snap2.snapshot_id, SnapshotId::new(2));
	assert_eq!(snap3.snapshot_id, SnapshotId::new(3));

	// Delete middle snapshot
	delete_snapshot(&mut registry, &mut alloc_node, &mut dedupe_index, snap2.snapshot_id)?;
	assert_eq!(registry.count(), 2);

	// Verify remaining snapshots
	assert!(registry.exists(snap1.snapshot_id));
	assert!(!registry.exists(snap2.snapshot_id));
	assert!(registry.exists(snap3.snapshot_id));

	// List snapshots should be in order
	let ids = registry.list_ids();
	assert_eq!(ids.len(), 2);
	assert_eq!(ids[0], snap1.snapshot_id);
	assert_eq!(ids[1], snap3.snapshot_id);

	Ok(())
}

// endregion: --- Multiple Snapshots

// region:    --- Determinism Tests

#[test]
fn test_snapshot_registry_deterministic_serialization() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry1 = SnapshotRegistry::new();
	let mut registry2 = SnapshotRegistry::new();

	// Add snapshots in different order
	let id1 = SnapshotId::new(1);
	let id2 = SnapshotId::new(2);
	let id3 = SnapshotId::new(3);

	// Registry 1: Add in order 3, 1, 2
	registry1.register(SnapshotMetadata::new(id3, ObjectRef::zero(), 300, 3000))?;
	registry1.register(SnapshotMetadata::new(id1, ObjectRef::zero(), 100, 1000))?;
	registry1.register(SnapshotMetadata::new(id2, ObjectRef::zero(), 200, 2000))?;

	// Registry 2: Add in order 2, 3, 1
	registry2.register(SnapshotMetadata::new(id2, ObjectRef::zero(), 200, 2000))?;
	registry2.register(SnapshotMetadata::new(id3, ObjectRef::zero(), 300, 3000))?;
	registry2.register(SnapshotMetadata::new(id1, ObjectRef::zero(), 100, 1000))?;

	// -- Exec
	let bytes1 = registry1.encode_canonical()?;
	let bytes2 = registry2.encode_canonical()?;

	// -- Check: Must be identical (BTreeMap ensures sorted order)
	assert_eq!(bytes1, bytes2);

	Ok(())
}

#[test]
fn test_snapshot_list_ids_sorted() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();

	// Add snapshots in non-sequential order
	registry.register(SnapshotMetadata::new(SnapshotId::new(5), ObjectRef::zero(), 500, 5000))?;
	registry.register(SnapshotMetadata::new(SnapshotId::new(2), ObjectRef::zero(), 200, 2000))?;
	registry.register(SnapshotMetadata::new(SnapshotId::new(8), ObjectRef::zero(), 800, 8000))?;
	registry.register(SnapshotMetadata::new(SnapshotId::new(1), ObjectRef::zero(), 100, 1000))?;

	// -- Exec
	let ids = registry.list_ids();

	// -- Check: Must be in sorted order
	assert_eq!(ids.len(), 4);
	assert_eq!(ids[0], SnapshotId::new(1));
	assert_eq!(ids[1], SnapshotId::new(2));
	assert_eq!(ids[2], SnapshotId::new(5));
	assert_eq!(ids[3], SnapshotId::new(8));

	Ok(())
}

// endregion: --- Determinism Tests

// region:    --- Edge Cases

#[test]
fn test_snapshot_create_empty_filesystem() -> Result<()> {
	// -- Setup & Fixtures: Empty allocation node (no extents)
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();
	let current_root = Root::new();

	// -- Exec
	let (metadata, _) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		Some("empty-snapshot".into()),
		None,
		1234567890,
	)?;

	// -- Check: Should succeed with 0 pinned blocks
	assert_eq!(
		metadata.custom_metadata.get("pinned_blocks"),
		Some(&alloc::format!("{}", 0))
	);
	assert_eq!(registry.count(), 1);

	Ok(())
}

#[test]
fn test_snapshot_delete_nonexistent() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();

	let invalid_id = SnapshotId::new(999);

	// -- Exec & Check
	let result = delete_snapshot(&mut registry, &mut alloc_node, &mut dedupe_index, invalid_id);
	assert!(result.is_err());

	Ok(())
}

#[test]
fn test_snapshot_id_overflow_detection() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();

	// Set next_snapshot_id to max value
	registry.next_snapshot_id = u32::MAX;

	// -- Exec & Check: Should return error on overflow
	let result = registry.allocate_id();
	assert!(result.is_err());

	Ok(())
}

// endregion: --- Edge Cases

// region:    --- Crash Simulation Tests

/// Simulates crash during pin phase of snapshot creation
#[test]
fn test_snapshot_crash_during_pin_phase() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();

	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated));
	alloc_node.add_extent(PhysicalExtent::new(2000, 200, AllocationState::Allocated));
	alloc_node.recompute_stats();

	// Allocate snapshot ID
	let snapshot_id = registry.allocate_id()?;

	// Start pin phase (but don't complete)
	let lbas: Vec<u64> = alloc_node
		.extents
		.iter()
		.filter(|(_, e)| e.state == AllocationState::Allocated)
		.map(|(lba, _)| *lba)
		.collect();

	// Pin only first extent, then "crash"
	if let Some(extent) = alloc_node.extents.get_mut(&lbas[0]) {
		extent.state = AllocationState::Pinned;
	}
	// CRASH HERE - second extent still ALLOCATED, snapshot not registered

	// -- Recovery Check
	// Snapshot should not be in registry
	assert!(!registry.exists(snapshot_id));

	// Mixed state: one extent pinned, one allocated
	assert_eq!(alloc_node.extents.get(&1000).unwrap().state, AllocationState::Pinned);
	assert_eq!(alloc_node.extents.get(&2000).unwrap().state, AllocationState::Allocated);

	// Recovery: Must unpin incomplete snapshot
	for (_, extent) in alloc_node.extents.iter_mut() {
		if extent.state == AllocationState::Pinned {
			extent.state = AllocationState::Allocated;
		}
	}

	// After recovery: All back to allocated
	assert_eq!(alloc_node.extents.get(&1000).unwrap().state, AllocationState::Allocated);
	assert_eq!(alloc_node.extents.get(&2000).unwrap().state, AllocationState::Allocated);

	Ok(())
}

/// Simulates crash after pin but before register
#[test]
fn test_snapshot_crash_before_register() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();

	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated));
	alloc_node.recompute_stats();

	let snapshot_id = registry.allocate_id()?;

	// Pin all extents
	for (_, extent) in alloc_node.extents.iter_mut() {
		if extent.state == AllocationState::Allocated {
			extent.state = AllocationState::Pinned;
		}
	}

	// CRASH HERE - extents pinned but snapshot not registered

	// -- Recovery Check
	assert!(!registry.exists(snapshot_id));
	assert_eq!(alloc_node.extents.get(&1000).unwrap().state, AllocationState::Pinned);

	// Recovery: Unpin orphaned extents not in any registered snapshot
	let registered_ids: Vec<SnapshotId> = registry.list_ids();
	if !registered_ids.contains(&snapshot_id) {
		// This snapshot ID not registered, unpin its extents
		for (_, extent) in alloc_node.extents.iter_mut() {
			if extent.state == AllocationState::Pinned {
				extent.state = AllocationState::Allocated;
			}
		}
	}

	assert_eq!(alloc_node.extents.get(&1000).unwrap().state, AllocationState::Allocated);

	Ok(())
}

/// Simulates crash during snapshot deletion
#[test]
fn test_snapshot_crash_during_delete() -> Result<()> {
	// -- Setup & Fixtures
	let mut registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new_empty_leaf();
	let mut dedupe_index = DedupeIndex::new();

	// Create snapshot
	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Allocated));
	alloc_node.recompute_stats();

	let current_root = Root::new();
	let (metadata, _) = create_snapshot(
		&mut registry,
		&mut alloc_node,
		&mut dedupe_index,
		&current_root,
		None,
		None,
		1234567890,
	)?;

	let snapshot_id = metadata.snapshot_id;

	// Start deletion: unregister but don't unpin
	let _deleted = registry.unregister(snapshot_id)?;
	// CRASH HERE - snapshot unregistered but extents still pinned

	// -- Recovery Check
	assert!(!registry.exists(snapshot_id));
	assert_eq!(alloc_node.extents.get(&1000).unwrap().state, AllocationState::Pinned);

	// Recovery: Unpin orphaned extents
	let registered_ids = registry.list_ids();
	if registered_ids.is_empty() {
		// No snapshots, all pinned extents are orphaned
		for (_, extent) in alloc_node.extents.iter_mut() {
			if extent.state == AllocationState::Pinned {
				extent.state = AllocationState::PendingFree;
			}
		}
	}

	assert_eq!(
		alloc_node.extents.get(&1000).unwrap().state,
		AllocationState::PendingFree
	);

	Ok(())
}

/// Test that pinned extents are protected from GC
#[test]
fn test_snapshot_gc_protection() -> Result<()> {
	// -- Setup & Fixtures
	let mut alloc_node = AllocNode::new_empty_leaf();

	// Add extents in different states
	alloc_node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::PendingFree));
	alloc_node.add_extent(PhysicalExtent::new(2000, 100, AllocationState::Pinned));
	alloc_node.recompute_stats();

	// -- Check
	let pending_extent = alloc_node.extents.get(&1000).unwrap();
	let pinned_extent = alloc_node.extents.get(&2000).unwrap();

	// PendingFree can be freed
	assert!(pending_extent.state.can_free());

	// Pinned cannot be freed (snapshot protection)
	assert!(!pinned_extent.state.can_free());

	Ok(())
}

// endregion: --- Crash Simulation Tests
