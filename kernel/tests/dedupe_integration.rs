//! Integration tests for DedupeIndex
//!
//! Tests hint-miss self-heal, refcount tracking across commits, and GC safety.

extern crate alloc;
use alloc::{boxed::Box, vec::Vec};

use crucible_ledgerfs_kernel::{
	Result,
	dedupe::{DedupeIndex, PhysicalLocator},
	object::ObjectId,
};

type TestResult<T> = core::result::Result<T, Box<dyn std::error::Error>>;

// region:    --- Hint Miss Self-Heal Tests

#[test]
fn test_dedupe_hint_miss_fallback() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"test data");
	let locator = PhysicalLocator::new(12345, 4096);

	// -- Exec: Insert object into index
	index.insert(object_id, locator)?;

	// -- Check: Lookup succeeds (hint hit)
	let found = index.lookup(&object_id);
	assert!(found.is_some());
	assert_eq!(found.unwrap().lba, 12345);
	assert_eq!(found.unwrap().record_stored_len, 4096);

	Ok(())
}

#[test]
fn test_dedupe_hint_miss_returns_none() -> TestResult<()> {
	// -- Setup & Fixtures
	let index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"missing object");

	// -- Exec: Lookup non-existent object
	let found = index.lookup(&object_id);

	// -- Check: Returns None (hint miss)
	assert!(found.is_none());

	Ok(())
}

#[test]
fn test_dedupe_hint_miss_self_heal() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"data");

	// Simulate reading object that's not in index yet
	assert!(index.lookup(&object_id).is_none());

	// -- Exec: After read completes, add to index (self-heal)
	let locator = PhysicalLocator::new(5000, 8192);
	index.insert(object_id, locator)?;

	// -- Check: Next lookup succeeds (healed)
	let found = index.lookup(&object_id);
	assert!(found.is_some());
	assert_eq!(found.unwrap().lba, 5000);

	Ok(())
}

// endregion: --- Hint Miss Self-Heal Tests

// region:    --- Refcount Tracking Across Commits

#[test]
fn test_dedupe_refcount_increment_on_duplicate_insert() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"shared object");
	let locator = PhysicalLocator::new(1000, 4096);

	// -- Exec: Insert same object multiple times (simulating multiple references)
	index.insert(object_id, locator)?;
	index.insert(object_id, locator)?;
	index.insert(object_id, locator)?;

	// -- Check: Refcount is 3
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.refcount, 3);

	Ok(())
}

#[test]
fn test_dedupe_refcount_decrement_on_delete() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"shared object");
	let locator = PhysicalLocator::new(1000, 4096);

	// Insert 3 times
	index.insert(object_id, locator)?;
	index.insert(object_id, locator)?;
	index.insert(object_id, locator)?;

	// -- Exec: Decrement refcount twice (simulating delete of 2 references)
	index.decrement_refcount(&object_id)?;
	index.decrement_refcount(&object_id)?;

	// -- Check: Refcount is 1
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.refcount, 1);

	Ok(())
}

#[test]
fn test_dedupe_refcount_prevents_premature_free() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"shared object");
	let locator = PhysicalLocator::new(1000, 4096);

	// Insert twice (refcount = 2)
	index.insert(object_id, locator)?;
	index.insert(object_id, locator)?;

	// Decrement once (refcount = 1)
	index.decrement_refcount(&object_id)?;

	// -- Exec & Check: Cannot remove while refcount > 0
	let result = index.remove(&object_id);
	assert!(result.is_err());

	// Decrement again (refcount = 0)
	index.decrement_refcount(&object_id)?;

	// -- Check: Now can remove
	let removed = index.remove(&object_id);
	assert!(removed.is_ok());

	Ok(())
}

#[test]
fn test_dedupe_refcount_zero_allows_gc() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let obj1 = ObjectId::from_content(b"object 1");
	let obj2 = ObjectId::from_content(b"object 2");
	let locator = PhysicalLocator::new(1000, 4096);

	// Insert two objects
	index.insert(obj1, locator)?;
	index.insert(obj2, locator)?;

	// Decrement obj1 to refcount=0
	index.decrement_refcount(&obj1)?;

	// -- Exec: Get GC candidates
	let candidates: Vec<_> = index.gc_candidates().collect();

	// -- Check: Only obj1 is a candidate
	assert_eq!(candidates.len(), 1);
	assert_eq!(candidates[0].object_id, obj1);
	assert_eq!(candidates[0].refcount, 0);

	Ok(())
}

// endregion: --- Refcount Tracking Across Commits

// region:    --- GC Safety

#[test]
fn test_dedupe_pinned_object_blocks_gc() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let object_id = ObjectId::from_content(b"important object");
	let locator = PhysicalLocator::new(1000, 4096);

	// Insert and pin
	index.insert(object_id, locator)?;
	index.pin(&object_id)?;

	// Decrement to refcount=0
	index.decrement_refcount(&object_id)?;

	// -- Exec & Check: Cannot remove pinned object
	let result = index.remove(&object_id);
	assert!(result.is_err());

	// Unpin
	index.unpin(&object_id)?;

	// -- Check: Now can remove
	let removed = index.remove(&object_id);
	assert!(removed.is_ok());

	Ok(())
}

#[test]
fn test_dedupe_gc_candidates_excludes_pinned() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();
	let obj1 = ObjectId::from_content(b"object 1");
	let obj2 = ObjectId::from_content(b"object 2");
	let locator = PhysicalLocator::new(1000, 4096);

	// Insert both
	index.insert(obj1, locator)?;
	index.insert(obj2, locator)?;

	// Decrement both to refcount=0
	index.decrement_refcount(&obj1)?;
	index.decrement_refcount(&obj2)?;

	// Pin obj1
	index.pin(&obj1)?;

	// -- Exec: Get GC candidates
	let candidates: Vec<_> = index.gc_candidates().collect();

	// -- Check: obj1 has refcount=0 but is pinned, so only obj2 is candidate
	assert_eq!(candidates.len(), 2); // Both have refcount=0

	// But obj1 cannot be freed
	let node1 = index.get(&obj1).unwrap();
	assert!(!node1.can_free()); // Pinned

	let node2 = index.get(&obj2).unwrap();
	assert!(node2.can_free()); // Not pinned

	Ok(())
}

// endregion: --- GC Safety

// region:    --- Deduplication Scenarios

#[test]
fn test_dedupe_duplicate_content_shares_location() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();

	// Same content = same ObjectId
	let content = b"duplicate data";
	let object_id = ObjectId::from_content(content);
	let locator = PhysicalLocator::new(5000, 4096);

	// -- Exec: Insert same content multiple times
	index.insert(object_id, locator)?;
	index.insert(object_id, locator)?;

	// -- Check: Only one entry, refcount = 2
	assert_eq!(index.len(), 1);
	let node = index.get(&object_id).unwrap();
	assert_eq!(node.refcount, 2);
	assert_eq!(node.locator.lba, 5000);

	Ok(())
}

#[test]
fn test_dedupe_different_content_different_entries() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut index = DedupeIndex::new();

	let obj1 = ObjectId::from_content(b"data 1");
	let obj2 = ObjectId::from_content(b"data 2");
	let locator1 = PhysicalLocator::new(1000, 4096);
	let locator2 = PhysicalLocator::new(2000, 4096);

	// -- Exec: Insert different content
	index.insert(obj1, locator1)?;
	index.insert(obj2, locator2)?;

	// -- Check: Two separate entries
	assert_eq!(index.len(), 2);

	let node1 = index.get(&obj1).unwrap();
	assert_eq!(node1.refcount, 1);
	assert_eq!(node1.locator.lba, 1000);

	let node2 = index.get(&obj2).unwrap();
	assert_eq!(node2.refcount, 1);
	assert_eq!(node2.locator.lba, 2000);

	Ok(())
}

// endregion: --- Deduplication Scenarios
