//! Tier metadata and relocation integration tests
//!
//! Tests for tier metadata and relocation framework including:
//! - Object relocation across tiers
//! - Stale hint handling and verify-by-hash
//! - Fallback to alternate locations on corruption
//! - Repair paths with multiple copies

extern crate alloc;

use alloc::boxed::Box;
use alloc::format; // For format! macro in no_std
use ledgerfs::object::ObjectId;
use ledgerfs_pending::dedupe::{DedupeIndex, PhysicalLocator, RelocationReason, StorageTier};

type Result<T> = core::result::Result<T, Box<dyn core::error::Error>>;

// region:    --- Relocation Tests

#[test]
fn test_relocation_hot_to_warm() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"test data for tier migration");

	// Object starts in Hot tier
	let hot_locator = PhysicalLocator::with_tier(1000, 8192, StorageTier::Hot);
	index.insert(object_id, hot_locator)?;

	// -- Exec: Migrate to Warm tier
	let warm_locator = PhysicalLocator::with_tier(50000, 8192, StorageTier::Warm);
	let record = index.relocate(&object_id, warm_locator, 1234567890, RelocationReason::TierMigration)?;

	// -- Check
	assert_eq!(record.old_locator.tier, StorageTier::Hot);
	assert_eq!(record.new_locator.tier, StorageTier::Warm);
	assert_eq!(record.reason, RelocationReason::TierMigration);

	// Verify index reflects new location
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.locator.tier, StorageTier::Warm);
	assert_eq!(node.locator.lba, 50000);

	Ok(())
}

#[test]
fn test_relocation_warm_to_cold() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"infrequently accessed data");

	let warm_locator = PhysicalLocator::with_tier(5000, 16384, StorageTier::Warm);
	index.insert(object_id, warm_locator)?;

	// -- Exec: Migrate to Cold tier for cost optimization
	let cold_locator = PhysicalLocator::with_tier(100000, 16384, StorageTier::Cold);
	let record = index.relocate(&object_id, cold_locator, 9999999, RelocationReason::TierMigration)?;

	// -- Check
	assert_eq!(record.old_locator.tier, StorageTier::Warm);
	assert_eq!(record.new_locator.tier, StorageTier::Cold);

	Ok(())
}

#[test]
fn test_relocation_cold_to_archive() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"rarely accessed archive data");

	let cold_locator = PhysicalLocator::with_tier(200000, 4096, StorageTier::Cold);
	index.insert(object_id, cold_locator)?;

	// -- Exec: Archive to tape/object storage
	let archive_locator = PhysicalLocator::with_tier(999999, 4096, StorageTier::Archive);
	let record = index.relocate(&object_id, archive_locator, 7777777, RelocationReason::TierMigration)?;

	// -- Check
	assert_eq!(record.old_locator.tier, StorageTier::Cold);
	assert_eq!(record.new_locator.tier, StorageTier::Archive);

	Ok(())
}

#[test]
fn test_relocation_multiple_objects() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let objects: alloc::vec::Vec<_> = (0..10)
		.map(|i| {
			let obj = ObjectId::from_content(&format!("object {}", i).into_bytes());
			let loc = PhysicalLocator::with_tier(i as u64 * 1000, 4096, StorageTier::Hot);
			(obj, loc)
		})
		.collect();

	for (obj, loc) in &objects {
		index.insert(*obj, *loc)?;
	}

	// -- Exec: Migrate all to Warm tier
	for (obj, _) in &objects {
		let new_loc = PhysicalLocator::with_tier(500000, 4096, StorageTier::Warm);
		index.relocate(obj, new_loc, 11111, RelocationReason::TierMigration)?;
	}

	// -- Check
	let warm_count: usize = index.objects_in_tier(StorageTier::Warm).count();
	assert_eq!(warm_count, 10);

	let hot_count: usize = index.objects_in_tier(StorageTier::Hot).count();
	assert_eq!(hot_count, 0);

	Ok(())
}

// endregion: --- Relocation Tests

// region:    --- Stale Hint Tests

#[test]
fn test_stale_hints_after_relocation() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"object with stale hints");

	// Object verified at old location
	let old_locator = PhysicalLocator::verified(1000, 4096, StorageTier::Hot, 555555);
	index.insert(object_id, old_locator)?;

	// -- Exec: Relocate to new location
	let new_locator = PhysicalLocator::with_tier(8000, 4096, StorageTier::Warm);
	index.relocate(&object_id, new_locator, 666666, RelocationReason::Defragmentation)?;

	// Old hint is now stale (new location not yet verified)
	let node = index.get(&object_id).unwrap();
	assert!(!node.locator.verified);
	assert_eq!(node.locator.last_verified, 0);

	// -- Check: Mark new location as verified after hash check
	index.mark_location_verified(&object_id, 666666)?;

	let node = index.get(&object_id).unwrap();
	assert!(node.locator.verified);
	assert_eq!(node.locator.last_verified, 666666);

	Ok(())
}

#[test]
fn test_explicit_stale_marking() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"suspected corrupt location");

	let locator = PhysicalLocator::verified(2000, 8192, StorageTier::Hot, 111111);
	index.insert(object_id, locator)?;

	// -- Exec: Suspect corruption, mark as stale
	index.mark_location_stale(&object_id)?;

	// -- Check
	let node = index.get(&object_id).unwrap();
	assert!(!node.locator.verified);

	// Next read must verify-by-hash
	Ok(())
}

#[test]
fn test_reverification_needed() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"object needing reverification");

	// Object verified 10 hours ago
	let old_timestamp = 1000000;
	let locator = PhysicalLocator::verified(3000, 4096, StorageTier::Cold, old_timestamp);
	index.insert(object_id, locator)?;

	// -- Check: With 24-hour threshold, no reverification needed yet
	let current_time = old_timestamp + (5 * 3600); // 5 hours later
	let threshold = 24 * 3600; // 24 hours

	let needs_verify: alloc::vec::Vec<_> = index.objects_needing_verification(current_time, threshold).collect();
	assert_eq!(needs_verify.len(), 0);

	// -- Check: With 4-hour threshold, reverification needed
	let threshold = 4 * 3600; // 4 hours
	let needs_verify: alloc::vec::Vec<_> = index.objects_needing_verification(current_time, threshold).collect();
	assert_eq!(needs_verify.len(), 1);
	assert_eq!(needs_verify[0].object_id, object_id);

	Ok(())
}

// endregion: --- Stale Hint Tests

// region:    --- Fallback & Repair Tests

#[test]
fn test_corruption_repair_relocation() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"object with corrupted primary location");

	// Primary location (corrupted)
	let primary_locator = PhysicalLocator::verified(5000, 4096, StorageTier::Hot, 222222);
	index.insert(object_id, primary_locator)?;

	// Simulate: Primary location corrupt, found alternate valid copy via mirror/parity
	// Alternate location at different LBA
	let alternate_locator = PhysicalLocator::verified(75000, 4096, StorageTier::Hot, 222223);

	// -- Exec: Repair by relocating to alternate location
	let record = index.relocate(
		&object_id,
		alternate_locator,
		222223,
		RelocationReason::CorruptionRepair,
	)?;

	// -- Check
	assert_eq!(record.reason, RelocationReason::CorruptionRepair);
	assert_eq!(record.old_locator.lba, 5000);
	assert_eq!(record.new_locator.lba, 75000);

	// Location now points to valid copy
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.locator.lba, 75000);
	assert!(node.locator.verified);

	Ok(())
}

#[test]
fn test_fallback_multiple_locations() -> Result<()> {
	// -- Setup & Fixtures
	// Simulate scenario: object has primary hint + multiple possible locations
	// (In real system, this might be from RAID mirrors or parity reconstruction)

	let object_id = ObjectId::from_content(b"multi-copy object");
	let mut index = DedupeIndex::new();

	// Primary hint (might be stale or corrupt)
	let primary = PhysicalLocator::with_tier(1000, 4096, StorageTier::Hot);
	index.insert(object_id, primary)?;

	// Simulate: Primary failed verify-by-hash
	index.mark_location_stale(&object_id)?;

	// Try alternate locations (in practice, these would come from RAID/mirrors)
	let alternate_locations = alloc::vec![
		PhysicalLocator::with_tier(2000, 4096, StorageTier::Hot),
		PhysicalLocator::with_tier(3000, 4096, StorageTier::Hot),
		PhysicalLocator::with_tier(4000, 4096, StorageTier::Hot),
	];

	// -- Exec: Find and use valid alternate (simulate found at index 1)
	let valid_alternate = alternate_locations[1];
	index.update_location(&object_id, valid_alternate)?;
	index.mark_location_verified(&object_id, 333333)?;

	// -- Check
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.locator.lba, 3000);
	assert!(node.locator.verified);

	Ok(())
}

#[test]
fn test_repair_path_workflow() -> Result<()> {
	// -- Setup & Fixtures: Complete repair workflow
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"repair workflow test");

	// 1. Object initially at primary location
	let primary = PhysicalLocator::verified(10000, 8192, StorageTier::Hot, 100000);
	index.insert(object_id, primary)?;

	// 2. Read attempt fails checksum verify
	// Mark location as stale
	index.mark_location_stale(&object_id)?;

	// 3. Attempt fallback to mirror location
	// (In real system: read from RAID mirror or reconstruct from parity)
	let mirror = PhysicalLocator::verified(20000, 8192, StorageTier::Hot, 100001);

	// 4. Mirror verify succeeds, relocate to mirror
	let record = index.relocate(&object_id, mirror, 100001, RelocationReason::CorruptionRepair)?;

	// 5. Verify repair metadata
	assert_eq!(record.reason, RelocationReason::CorruptionRepair);
	assert_eq!(record.old_locator.lba, 10000);
	assert_eq!(record.new_locator.lba, 20000);

	// 6. Object now readable from mirror location
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.locator.lba, 20000);
	assert!(node.locator.verified);

	// 7. Old location can be marked for scrubbing/repair
	// (Not part of dedupe index - handled by scrubber)

	Ok(())
}

// endregion: --- Fallback & Repair Tests

// region:    --- Tier Statistics Tests

#[test]
fn test_tier_statistics_after_migrations() -> Result<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();

	// Create objects in different tiers
	for i in 0..5 {
		let obj = ObjectId::from_content(&format!("hot{}", i).into_bytes());
		index.insert(obj, PhysicalLocator::with_tier(i as u64, 4096, StorageTier::Hot))?;
	}

	for i in 0..3 {
		let obj = ObjectId::from_content(&format!("warm{}", i).into_bytes());
		index.insert(obj, PhysicalLocator::with_tier(100 + i as u64, 4096, StorageTier::Warm))?;
	}

	// -- Check initial stats
	let stats = index.tier_statistics();
	assert_eq!(stats.hot_count, 5);
	assert_eq!(stats.warm_count, 3);
	assert_eq!(stats.cold_count, 0);
	assert_eq!(stats.archive_count, 0);

	// -- Exec: Migrate 2 hot objects to cold
	let hot_objects: alloc::vec::Vec<_> =
		index.objects_in_tier(StorageTier::Hot).take(2).map(|n| n.object_id).collect();

	for obj_id in hot_objects {
		let cold_loc = PhysicalLocator::with_tier(5000, 4096, StorageTier::Cold);
		index.relocate(&obj_id, cold_loc, 99999, RelocationReason::TierMigration)?;
	}

	// -- Check updated stats
	let stats = index.tier_statistics();
	assert_eq!(stats.hot_count, 3);
	assert_eq!(stats.warm_count, 3);
	assert_eq!(stats.cold_count, 2);
	assert_eq!(stats.archive_count, 0);

	Ok(())
}

// endregion: --- Tier Statistics Tests

// region:    --- Determinism Tests

#[test]
fn test_relocation_deterministic() -> Result<()> {
	// -- Setup & Fixtures
	let mut index1 = DedupeIndex::new();
	let mut index2 = DedupeIndex::new();

	let objects = alloc::vec![
		ObjectId::from_content(b"zebra"),
		ObjectId::from_content(b"alpha"),
		ObjectId::from_content(b"delta"),
	];

	// Insert in different order
	for obj in &objects {
		index1.insert(*obj, PhysicalLocator::with_tier(1000, 4096, StorageTier::Hot))?;
	}

	for obj in objects.iter().rev() {
		index2.insert(*obj, PhysicalLocator::with_tier(1000, 4096, StorageTier::Hot))?;
	}

	// Relocate all to Warm
	for obj in &objects {
		index1.relocate(
			obj,
			PhysicalLocator::with_tier(2000, 4096, StorageTier::Warm),
			11111,
			RelocationReason::TierMigration,
		)?;
		index2.relocate(
			obj,
			PhysicalLocator::with_tier(2000, 4096, StorageTier::Warm),
			11111,
			RelocationReason::TierMigration,
		)?;
	}

	// -- Exec: Serialize both
	let bytes1 = postcard::to_allocvec(&index1)?;
	let bytes2 = postcard::to_allocvec(&index2)?;

	// -- Check: Same serialization (BTreeMap ensures determinism)
	assert_eq!(bytes1, bytes2);

	Ok(())
}

// endregion: --- Determinism Tests
