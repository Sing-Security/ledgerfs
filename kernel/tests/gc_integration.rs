//! Integration Tests: Online GC (Tier B)
//!
//! Tests for incremental garbage collection with periodic reconciliation.

extern crate alloc;

use alloc::boxed::Box;
use crucible_ledgerfs_kernel::alloc_node::AllocNode;
use crucible_ledgerfs_kernel::dedupe::{DedupeIndex, DedupeNode, PhysicalLocator, StorageTier};
use crucible_ledgerfs_kernel::gc::{GcEngine, GcMode};
use crucible_ledgerfs_kernel::object::ObjectId;
use crucible_ledgerfs_kernel::snapshot::{SnapshotMetadata, SnapshotRegistry};

type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

// region:    --- Incremental GC Tests

#[test]
fn test_gc_mode_tier_b() -> TestResult<()> {
	// -- Setup & Fixtures
	let engine = GcEngine::new(GcMode::TierB);

	// -- Check
	assert_eq!(engine.stats().mode, GcMode::TierB);

	Ok(())
}

#[test]
fn test_deadlist_add_remove() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB);
	let object_id = ObjectId::from_bytes(&[1u8; 32]);

	// -- Exec - Add to deadlist
	engine.add_to_deadlist(object_id, 100, 1000);

	// -- Check
	let stats = engine.stats();
	assert_eq!(stats.deadlist_entries, 1);

	Ok(())
}

#[test]
fn test_incremental_cycle_skips_nonzero_refcount() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB);
	let mut dedupe_index = DedupeIndex::new();
	let snapshot_registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new();

	// Add object with refcount > 0
	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let locator = PhysicalLocator::new(1000, 100);
	let node = DedupeNode::with_refcount(object_id, locator, 2); // refcount = 2
	dedupe_index.insert(object_id, locator)?;
	dedupe_index.increment_refcount(&object_id)?; // refcount = 3

	// Add to deadlist
	engine.add_to_deadlist(object_id, 100, 1000);

	// -- Exec - Try to free (should skip due to refcount > 0)
	let result = engine.incremental_cycle(
		&mut dedupe_index,
		&snapshot_registry,
		&mut alloc_node,
		200,  // current_commit_seq
		2000, // current_timestamp
	)?;

	// -- Check - Should skip, not free
	assert_eq!(result.objects_freed, 0);
	assert!(result.skipped_refcount > 0);

	Ok(())
}

#[test]
fn test_incremental_cycle_frees_zero_refcount() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB);
	let mut dedupe_index = DedupeIndex::new();
	let snapshot_registry = SnapshotRegistry::new(); // No snapshots
	let mut alloc_node = AllocNode::new();

	// Add object with refcount = 1, then decrement to 0
	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let locator = PhysicalLocator::new(1000, 100);
	dedupe_index.insert(object_id, locator)?;
	dedupe_index.decrement_refcount(&object_id)?; // refcount = 0

	// Add to deadlist
	engine.add_to_deadlist(object_id, 100, 1000);

	// -- Exec - Free object
	let result = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - Should free
	assert_eq!(result.objects_freed, 1);
	assert_eq!(result.skipped_refcount, 0);

	// Object should be removed from dedupe index
	assert!(dedupe_index.get(&object_id).is_none());

	Ok(())
}

#[test]
fn test_conservative_skips_when_snapshots_exist() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB);
	let mut dedupe_index = DedupeIndex::new();
	let mut snapshot_registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new();

	// Create and register a snapshot
	use crucible_ledgerfs_kernel::object::ObjectRef;
	let snapshot_id = snapshot_registry.allocate_id()?;
	let metadata = SnapshotMetadata::new(
		snapshot_id,
		ObjectRef::zero(),
		100,  // commit_seq
		1000, // created_at_ns
	);
	snapshot_registry.register(metadata)?;

	// Add object with refcount = 0
	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let locator = PhysicalLocator::new(1000, 100);
	dedupe_index.insert(object_id, locator)?;
	dedupe_index.decrement_refcount(&object_id)?;

	// Add to deadlist
	engine.add_to_deadlist(object_id, 100, 1000);

	// -- Exec - Try to free (should skip due to snapshot)
	let result = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - Should skip (conservative: snapshots exist)
	assert_eq!(result.objects_freed, 0);
	assert!(result.skipped_snapshot > 0);

	Ok(())
}

#[test]
fn test_batch_processing() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB).with_batch_size(2); // Process 2 objects per batch

	let mut dedupe_index = DedupeIndex::new();
	let snapshot_registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new();

	// Add 5 objects to deadlist, all with refcount = 0
	for i in 1..=5 {
		let mut bytes = [0u8; 32];
		bytes[0] = i;
		let object_id = ObjectId::from_bytes(&bytes);
		let locator = PhysicalLocator::new(1000 + i as u64 * 100, 100);
		dedupe_index.insert(object_id, locator)?;
		dedupe_index.decrement_refcount(&object_id)?;
		engine.add_to_deadlist(object_id, 100, 1000);
	}

	// -- Exec - First batch (should process 2)
	let result1 = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - First batch
	assert_eq!(result1.objects_freed, 2);

	// -- Exec - Second batch (should process 2 more)
	let result2 = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - Second batch
	assert_eq!(result2.objects_freed, 2);

	// -- Exec - Third batch (should process last 1)
	let result3 = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - Third batch
	assert_eq!(result3.objects_freed, 1);

	// Total freed should be 5
	let stats = engine.stats();
	assert_eq!(stats.objects_freed, 5);

	Ok(())
}

// endregion: --- Incremental GC Tests

// region:    --- Reconciliation Tests

#[test]
fn test_reconciliation_scheduling() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB).with_reconciliation_interval(100);

	// -- Check - Initially needs reconciliation
	assert!(engine.needs_reconciliation(0));

	// -- Exec - Run reconciliation
	let mut dedupe_index = DedupeIndex::new();
	engine.reconcile(&mut dedupe_index, 0)?;

	// -- Check - Should not need reconciliation for 100 commits
	assert!(!engine.needs_reconciliation(50));
	assert!(!engine.needs_reconciliation(99));
	assert!(engine.needs_reconciliation(100));
	assert!(engine.needs_reconciliation(150));

	Ok(())
}

#[test]
fn test_reconciliation_periodic() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB).with_reconciliation_interval(10);

	let mut dedupe_index = DedupeIndex::new();

	// -- Exec - Simulate 30 commits with periodic reconciliation
	let mut reconciliations = 0;
	for commit_seq in 0..30 {
		if engine.needs_reconciliation(commit_seq) {
			engine.reconcile(&mut dedupe_index, commit_seq)?;
			reconciliations += 1;
		}
	}

	// -- Check - Should have reconciled 3 times (at 0, 10, 20)
	assert_eq!(reconciliations, 3);

	Ok(())
}

// endregion: --- Reconciliation Tests

// region:    --- Safety Tests

#[test]
fn test_never_free_reachable() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB);
	let mut dedupe_index = DedupeIndex::new();
	let snapshot_registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new();

	// Add object that is still reachable (refcount > 0)
	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let locator = PhysicalLocator::new(1000, 100);
	dedupe_index.insert(object_id, locator)?;
	// Don't decrement refcount - still reachable!

	// Erroneously add to deadlist (simulating bug or race condition)
	engine.add_to_deadlist(object_id, 100, 1000);

	// -- Exec - Try to free
	let result = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - MUST NOT free (safety check prevents it)
	assert_eq!(result.objects_freed, 0);
	assert!(result.skipped_refcount > 0);

	// Object must still exist
	assert!(dedupe_index.get(&object_id).is_some());

	Ok(())
}

#[test]
fn test_never_free_pinned() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut engine = GcEngine::new(GcMode::TierB);
	let mut dedupe_index = DedupeIndex::new();
	let snapshot_registry = SnapshotRegistry::new();
	let mut alloc_node = AllocNode::new();

	// Add object and pin it
	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let locator = PhysicalLocator::new(1000, 100);
	dedupe_index.insert(object_id, locator)?;
	dedupe_index.decrement_refcount(&object_id)?; // refcount = 0
	dedupe_index.pin(&object_id)?; // But pinned!

	// Add to deadlist
	engine.add_to_deadlist(object_id, 100, 1000);

	// -- Exec - Try to free
	let result = engine.incremental_cycle(&mut dedupe_index, &snapshot_registry, &mut alloc_node, 200, 2000)?;

	// -- Check - MUST NOT free pinned object
	assert_eq!(result.objects_freed, 0);
	assert!(result.skipped_pinned > 0);

	// Object must still exist
	assert!(dedupe_index.get(&object_id).is_some());

	Ok(())
}

// endregion: --- Safety Tests
