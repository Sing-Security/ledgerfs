//! Integration Tests: Scrubber + Repair Integration
//!
//! Tests for background scrubber with corruption detection and repair.

extern crate alloc;

use alloc::boxed::Box;
use ledgerfs::object::ObjectId;
use ledgerfs_pending::dedupe::{DedupeIndex, PhysicalLocator, StorageTier};
use ledgerfs_pending::scrubber::{CorruptionType, Scrubber, ThrottleConfig};

type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

// region:    --- Scrubber Lifecycle Tests

#[test]
fn test_scrubber_start_stop() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();

	// -- Exec
	scrubber.start(1000);

	// -- Check
	assert_eq!(scrubber.stats().last_scrub_start, 1000);

	// -- Exec
	scrubber.stop(2000);

	// -- Check
	assert_eq!(scrubber.stats().last_scrub_complete, 2000);

	Ok(())
}

#[test]
fn test_scrubber_pause_resume() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	scrubber.start(1000);

	// -- Exec - Pause
	scrubber.pause();

	// -- Check - Scrub batch should return 0 when paused
	let dedupe_index = DedupeIndex::new();
	let scanned = scrubber.scrub_batch(&dedupe_index, 2000, 0)?;
	assert_eq!(scanned, 0);

	// -- Exec - Resume
	scrubber.resume();

	Ok(())
}

// endregion: --- Scrubber Lifecycle Tests

// region:    --- Verification Tests

#[test]
fn test_verify_object_success() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let data = b"test data for verification";
	let hash = blake3::hash(data);
	let object_id = ObjectId::from_content(data);

	// -- Exec
	let verified = scrubber.verify_object(&object_id, data, hash.as_bytes(), 1000)?;

	// -- Check
	assert!(verified);
	assert_eq!(scrubber.stats().objects_scanned, 1);
	assert_eq!(scrubber.stats().objects_verified, 1);
	assert_eq!(scrubber.stats().corruptions_detected, 0);

	Ok(())
}

#[test]
fn test_verify_object_hash_mismatch() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let data = b"test data";
	let wrong_hash = [0xFFu8; 32]; // Wrong hash
	let object_id = ObjectId::from_bytes(&wrong_hash);

	// -- Exec
	let verified = scrubber.verify_object(&object_id, data, &wrong_hash, 1000)?;

	// -- Check - Corruption detected
	assert!(!verified);
	assert_eq!(scrubber.stats().objects_scanned, 1);
	assert_eq!(scrubber.stats().objects_verified, 0);
	assert_eq!(scrubber.stats().corruptions_detected, 1);

	// Corruption event should be created
	let events: alloc::vec::Vec<_> = scrubber.corruption_events().collect();
	assert_eq!(events.len(), 1);
	assert_eq!(events[0].1.corruption_type, CorruptionType::HashMismatch);

	Ok(())
}

#[test]
fn test_verify_object_id_mismatch() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let data = b"test data";
	let hash = blake3::hash(data);

	// Create object_id that doesn't match the hash
	let wrong_id = ObjectId::from_bytes(&[0xAAu8; 32]);

	// -- Exec - Verify with correct hash but wrong object_id
	let verified = scrubber.verify_object(&wrong_id, data, hash.as_bytes(), 1000)?;

	// -- Check - ObjectId mismatch detected
	assert!(!verified);
	assert_eq!(scrubber.stats().corruptions_detected, 1);

	// Corruption event should be created
	let events: alloc::vec::Vec<_> = scrubber.corruption_events().collect();
	assert_eq!(events.len(), 1);
	assert_eq!(events[0].1.corruption_type, CorruptionType::ObjectIdMismatch);

	Ok(())
}

// endregion: --- Verification Tests

// region:    --- Throttling Tests

#[test]
fn test_throttle_config_default() -> TestResult<()> {
	// -- Setup & Fixtures
	let config = ThrottleConfig::default();

	// -- Check
	assert_eq!(config.sleep_ms_per_object, 10);
	assert_eq!(config.objects_per_batch, 100);
	assert_eq!(config.pause_threshold_queue_depth, 50);
	assert_eq!(config.resume_threshold_queue_depth, 10);

	Ok(())
}

#[test]
fn test_throttle_config_aggressive() -> TestResult<()> {
	// -- Setup & Fixtures
	let config = ThrottleConfig::aggressive();

	// -- Check - More aggressive throttling
	assert!(config.sleep_ms_per_object > ThrottleConfig::default().sleep_ms_per_object);
	assert!(config.objects_per_batch < ThrottleConfig::default().objects_per_batch);
	assert!(config.pause_threshold_queue_depth < ThrottleConfig::default().pause_threshold_queue_depth);

	Ok(())
}

#[test]
fn test_should_pause_on_high_write_queue() -> TestResult<()> {
	// -- Setup & Fixtures
	let scrubber = Scrubber::with_throttle_config(ThrottleConfig::default());

	// -- Check - Should pause when queue depth exceeds threshold
	assert!(!scrubber.should_pause(49)); // Below threshold
	assert!(scrubber.should_pause(50)); // At threshold
	assert!(scrubber.should_pause(100)); // Well above threshold

	Ok(())
}

#[test]
fn test_should_resume_on_low_write_queue() -> TestResult<()> {
	// -- Setup & Fixtures
	let scrubber = Scrubber::with_throttle_config(ThrottleConfig::default());

	// -- Check - Should resume when queue depth drops
	assert!(scrubber.should_resume(9)); // Below threshold
	assert!(!scrubber.should_resume(10)); // At threshold
	assert!(!scrubber.should_resume(50)); // Well above threshold

	Ok(())
}

#[test]
fn test_adaptive_throttling_workflow() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	scrubber.start(1000);

	let write_queue_depth_high = 60;
	let write_queue_depth_low = 5;

	// -- Exec - High write queue should trigger pause
	assert!(scrubber.should_pause(write_queue_depth_high));
	scrubber.pause();

	// -- Check - Paused state
	let dedupe_index = DedupeIndex::new();
	let scanned = scrubber.scrub_batch(&dedupe_index, 2000, 0)?;
	assert_eq!(scanned, 0); // No scrubbing while paused

	// -- Exec - Low write queue should trigger resume
	assert!(scrubber.should_resume(write_queue_depth_low));
	scrubber.resume();

	Ok(())
}

// endregion: --- Throttling Tests

// region:    --- Repair Integration Tests

#[test]
fn test_repair_path_mark_stale() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let mut dedupe_index = DedupeIndex::new();

	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let primary_locator = PhysicalLocator::verified(1000, 100, StorageTier::Hot, 1000);
	dedupe_index.insert(object_id, primary_locator)?;

	// Detect corruption
	let data = b"test";
	let wrong_hash = [0xFFu8; 32];
	scrubber.verify_object(&object_id, data, &wrong_hash, 2000)?;

	// -- Exec - Attempt repair without alternate
	let repaired = scrubber.attempt_repair(
		&object_id,
		&mut dedupe_index,
		None, // No alternate location
		2000,
	)?;

	// -- Check - Repair failed but location marked stale
	assert!(!repaired);
	assert_eq!(scrubber.stats().repairs_attempted, 1);
	assert_eq!(scrubber.stats().repairs_successful, 0);

	Ok(())
}

#[test]
fn test_repair_path_with_alternate() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let mut dedupe_index = DedupeIndex::new();

	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let primary_locator = PhysicalLocator::verified(1000, 100, StorageTier::Hot, 1000);
	let alternate_locator = PhysicalLocator::verified(2000, 100, StorageTier::Warm, 2000);

	dedupe_index.insert(object_id, primary_locator)?;

	// Detect corruption
	let data = b"test";
	let wrong_hash = [0xFFu8; 32];
	scrubber.verify_object(&object_id, data, &wrong_hash, 2000)?;

	// -- Exec - Attempt repair with alternate location
	let repaired = scrubber.attempt_repair(&object_id, &mut dedupe_index, Some(alternate_locator), 2000)?;

	// -- Check - Repair succeeded
	assert!(repaired);
	assert_eq!(scrubber.stats().repairs_attempted, 1);
	assert_eq!(scrubber.stats().repairs_successful, 1);

	// Location should be updated and verified
	let node = dedupe_index.get(&object_id).unwrap();
	assert_eq!(node.locator.lba, 2000); // Relocated to alternate
	assert!(node.locator.verified);

	Ok(())
}

#[test]
fn test_corruption_repair_workflow() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let mut dedupe_index = DedupeIndex::new();

	let object_id = ObjectId::from_bytes(&[1u8; 32]);
	let primary = PhysicalLocator::verified(1000, 100, StorageTier::Hot, 1000);
	let mirror = PhysicalLocator::verified(3000, 100, StorageTier::Hot, 1000);

	dedupe_index.insert(object_id, primary)?;

	// -- Exec - Full workflow

	// 1. Detect corruption during scrub
	let data = b"corrupted data";
	let wrong_hash = [0xDEu8; 32];
	let verified = scrubber.verify_object(&object_id, data, &wrong_hash, 2000)?;
	assert!(!verified);

	// 2. Attempt repair with mirror location
	let repaired = scrubber.attempt_repair(&object_id, &mut dedupe_index, Some(mirror), 2000)?;
	assert!(repaired);

	// -- Check - Corruption event updated
	let events: alloc::vec::Vec<_> = scrubber.corruption_events().collect();
	assert_eq!(events.len(), 1);
	assert!(events[0].1.repair_attempted);
	assert!(events[0].1.repair_successful);

	// -- Check - Object relocated to mirror
	let node = dedupe_index.get(&object_id).unwrap();
	assert_eq!(node.locator.lba, 3000);
	assert!(node.locator.verified);

	Ok(())
}

// endregion: --- Repair Integration Tests

// region:    --- Determinism Tests

#[test]
fn test_corruption_events_deterministic_order() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();

	// Create corruption events in non-sorted order
	let id1 = ObjectId::from_bytes(&[1u8; 32]);
	let id2 = ObjectId::from_bytes(&[2u8; 32]);
	let id3 = ObjectId::from_bytes(&[3u8; 32]);

	let wrong_hash = [0xFFu8; 32];

	// Add in reverse order
	scrubber.verify_object(&id3, b"3", &wrong_hash, 1000)?;
	scrubber.verify_object(&id1, b"1", &wrong_hash, 1000)?;
	scrubber.verify_object(&id2, b"2", &wrong_hash, 1000)?;

	// -- Exec - Iterate corruption events
	let events: alloc::vec::Vec<_> = scrubber.corruption_events().map(|(id, _)| *id).collect();

	// -- Check - Should be sorted by ObjectId (deterministic)
	assert_eq!(events[0], id1);
	assert_eq!(events[1], id2);
	assert_eq!(events[2], id3);

	Ok(())
}

// endregion: --- Determinism Tests

// region:    --- Long-Run Simulation Tests

#[test]
fn test_long_run_no_corruption() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();
	let mut dedupe_index = DedupeIndex::new();

	// Add 100 objects
	for i in 1..=100 {
		let mut bytes = [0u8; 32];
		bytes[0] = (i % 256) as u8;
		bytes[1] = (i / 256) as u8;
		let object_id = ObjectId::from_bytes(&bytes);
		let locator = PhysicalLocator::new(1000 + i * 100, 100);
		dedupe_index.insert(object_id, locator)?;
	}

	scrubber.start(1000);

	// -- Exec - Scrub in batches
	let mut total_scanned = 0;
	for _ in 0..10 {
		let scanned = scrubber.scrub_batch(&dedupe_index, 2000, 0)?;
		total_scanned += scanned;
	}

	scrubber.stop(3000);

	// -- Check - All objects processed, no corruption
	assert!(total_scanned > 0);
	assert_eq!(scrubber.stats().corruptions_detected, 0);

	Ok(())
}

#[test]
fn test_injected_corruption_detection() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut scrubber = Scrubber::new();

	// Inject 10 corruptions
	for i in 1..=10 {
		let mut bytes = [0u8; 32];
		bytes[0] = i;
		let object_id = ObjectId::from_bytes(&bytes);

		// Intentionally corrupt data
		let data = b"correct data";
		let wrong_hash = [0xBAu8; 32];

		scrubber.verify_object(&object_id, data, &wrong_hash, 1000 + i as u64)?;
	}

	// -- Check - All 10 corruptions detected
	assert_eq!(scrubber.stats().objects_scanned, 10);
	assert_eq!(scrubber.stats().corruptions_detected, 10);
	assert_eq!(scrubber.stats().objects_verified, 0);

	Ok(())
}

// endregion: --- Long-Run Simulation Tests
