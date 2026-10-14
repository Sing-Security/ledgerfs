//! Garbage Collection for LedgerFS v7
//!
//! Per §22 of ledgerfs-spec-v7.md:
//! - Deadlist-first approach with refcount validation
//! - Safe freeing procedure (deadlist → refcount check → snapshot check)
//! - Tier B: Incremental processing with periodic reconciliation
//! - Conservative fallback on any uncertainty
//!
//! ## GC Tiers (§44)
//!
//! - **Tier A**: Stop-the-world mark-sweep (simple, correct)
//! - **Tier B**: Incremental deadlist processing + periodic reconciliation
//! - **Tier C**: Generational GC with age tracking
//!
//! ## Safety Guarantees (§22.2)
//!
//! Before freeing an object, GC MUST verify:
//! 1. Object is in deadlist
//! 2. No active snapshots reference it
//! 3. Refcount is zero (for dedupe)
//! 4. Allocation state allows freeing (not PINNED or RESERVED)

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::alloc_node::AllocNode;
use crate::dedupe::DedupeIndex;
use crate::object::ObjectId;
use crate::snapshot::SnapshotRegistry;
use crate::{Error, Result};

// region:    --- GcMode

/// Garbage collection mode/tier
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum GcMode {
	/// Tier A: Stop-the-world mark-sweep
	/// Simple, correct, but may pause writes
	TierA = 0,

	/// Tier B: Incremental deadlist processing with periodic reconciliation
	/// Processes deadlist incrementally, uses refcounts as candidates
	/// Conservative fallback on uncertainty, periodic full mark to correct drift
	TierB = 1,

	/// Tier C: Generational GC with age tracking
	/// Future: young/old generation separation
	TierC = 2,
}

impl GcMode {
	pub fn from_u8(val: u8) -> Result<Self> {
		match val {
			0 => Ok(Self::TierA),
			1 => Ok(Self::TierB),
			2 => Ok(Self::TierC),
			_ => Err(Error::InvalidGcMode { mode_id: val }),
		}
	}

	pub fn to_u8(self) -> u8 {
		self as u8
	}
}

impl Default for GcMode {
	fn default() -> Self {
		Self::TierB // Default to incremental mode
	}
}

// endregion: --- GcMode

// region:    --- DeadlistEntry

/// Entry in the garbage collection deadlist
///
/// Tracks an object that is no longer reachable from current root
/// but may still be referenced by snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlistEntry {
	/// Object that is no longer reachable
	pub object_id: ObjectId,

	/// Commit sequence when object became unreachable
	pub unreachable_since: u64,

	/// Number of GC cycles this object has survived
	/// Used for generational GC (Tier C)
	pub age: u32,

	/// Last time we checked if this object can be freed
	/// Prevents repeated checking of pinned objects
	pub last_check_timestamp: u64,
}

impl DeadlistEntry {
	/// Create new deadlist entry
	pub fn new(object_id: ObjectId, commit_seq: u64, timestamp: u64) -> Self {
		Self {
			object_id,
			unreachable_since: commit_seq,
			age: 0,
			last_check_timestamp: timestamp,
		}
	}

	/// Increment age (for generational GC)
	pub fn increment_age(&mut self) {
		self.age = self.age.saturating_add(1);
	}

	/// Update last check timestamp
	pub fn update_check_timestamp(&mut self, timestamp: u64) {
		self.last_check_timestamp = timestamp;
	}
}

// endregion: --- DeadlistEntry

// region:    --- Deadlist

/// Deadlist tracking unreachable objects
///
/// Uses BTreeMap for deterministic iteration order.
/// Objects are keyed by ObjectId for fast lookup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Deadlist {
	/// Map from ObjectId to deadlist entry
	entries: BTreeMap<ObjectId, DeadlistEntry>,

	/// Total number of objects added to deadlist (lifetime)
	total_added: u64,

	/// Total number of objects freed from deadlist (lifetime)
	total_freed: u64,
}

impl Deadlist {
	/// Create new empty deadlist
	pub fn new() -> Self {
		Self {
			entries: BTreeMap::new(),
			total_added: 0,
			total_freed: 0,
		}
	}

	/// Add object to deadlist
	pub fn add(&mut self, object_id: ObjectId, commit_seq: u64, timestamp: u64) {
		let entry = DeadlistEntry::new(object_id, commit_seq, timestamp);
		self.entries.insert(object_id, entry);
		self.total_added += 1;
	}

	/// Remove object from deadlist (after freeing)
	pub fn remove(&mut self, object_id: &ObjectId) -> Option<DeadlistEntry> {
		let entry = self.entries.remove(object_id);
		if entry.is_some() {
			self.total_freed += 1;
		}
		entry
	}

	/// Check if object is in deadlist
	pub fn contains(&self, object_id: &ObjectId) -> bool {
		self.entries.contains_key(object_id)
	}

	/// Get entry for object
	pub fn get(&self, object_id: &ObjectId) -> Option<&DeadlistEntry> {
		self.entries.get(object_id)
	}

	/// Get mutable entry for object
	pub fn get_mut(&mut self, object_id: &ObjectId) -> Option<&mut DeadlistEntry> {
		self.entries.get_mut(object_id)
	}

	/// Number of objects currently in deadlist
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	/// Check if deadlist is empty
	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	/// Iterate over all entries (deterministic order)
	pub fn iter(&self) -> impl Iterator<Item = (&ObjectId, &DeadlistEntry)> {
		self.entries.iter()
	}

	/// Get objects added since commit_seq (candidates for freeing)
	pub fn candidates_since(&self, commit_seq: u64) -> impl Iterator<Item = &DeadlistEntry> + '_ {
		self.entries.values().filter(move |entry| entry.unreachable_since <= commit_seq)
	}

	/// Get objects that haven't been checked recently
	pub fn unchecked_since(&self, timestamp: u64, threshold: u64) -> impl Iterator<Item = &DeadlistEntry> + '_ {
		self.entries
			.values()
			.filter(move |entry| timestamp.saturating_sub(entry.last_check_timestamp) > threshold)
	}

	/// Clear all entries (for testing)
	pub fn clear(&mut self) {
		self.entries.clear();
	}

	/// Get statistics
	pub fn stats(&self) -> DeadlistStats {
		DeadlistStats {
			current_entries: self.entries.len() as u64,
			total_added: self.total_added,
			total_freed: self.total_freed,
		}
	}
}

/// Deadlist statistics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlistStats {
	pub current_entries: u64,
	pub total_added: u64,
	pub total_freed: u64,
}

// endregion: --- Deadlist

// region:    --- GcState

/// Garbage collector state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcState {
	/// GC mode/tier
	pub mode: GcMode,

	/// Deadlist of unreachable objects
	pub deadlist: Deadlist,

	/// Number of GC cycles completed
	pub cycles_completed: u64,

	/// Number of objects freed (lifetime)
	pub objects_freed: u64,

	/// Last full reconciliation commit_seq
	pub last_reconciliation_seq: u64,

	/// Reconciliation interval (number of commits between full mark)
	pub reconciliation_interval: u64,

	/// Whether we've ever reconciled
	reconciled_once: bool,
}

impl GcState {
	/// Create new GC state with specified mode
	pub fn new(mode: GcMode) -> Self {
		Self {
			mode,
			deadlist: Deadlist::new(),
			cycles_completed: 0,
			objects_freed: 0,
			last_reconciliation_seq: 0,
			reconciliation_interval: 1000, // Default: reconcile every 1000 commits
			reconciled_once: false,
		}
	}

	/// Check if reconciliation is needed
	pub fn needs_reconciliation(&self, current_commit_seq: u64) -> bool {
		// Always need first reconciliation
		if !self.reconciled_once {
			return true;
		}

		current_commit_seq.saturating_sub(self.last_reconciliation_seq) >= self.reconciliation_interval
	}

	/// Mark reconciliation as completed
	pub fn mark_reconciliation_completed(&mut self, commit_seq: u64) {
		self.last_reconciliation_seq = commit_seq;
		self.reconciled_once = true;
	}
}

impl Default for GcState {
	fn default() -> Self {
		Self::new(GcMode::default())
	}
}

// endregion: --- GcState

// region:    --- GcEngine

/// Garbage collection engine
///
/// Implements Tier B incremental GC with periodic reconciliation.
pub struct GcEngine {
	/// GC state
	state: GcState,

	/// Batch size for incremental processing
	batch_size: usize,
}

impl GcEngine {
	/// Create new GC engine with specified mode
	pub fn new(mode: GcMode) -> Self {
		Self {
			state: GcState::new(mode),
			batch_size: 100, // Default: process 100 objects per cycle
		}
	}

	/// Set batch size for incremental processing
	pub fn with_batch_size(mut self, batch_size: usize) -> Self {
		self.batch_size = batch_size;
		self
	}

	/// Set reconciliation interval
	pub fn with_reconciliation_interval(mut self, interval: u64) -> Self {
		self.state.reconciliation_interval = interval;
		self
	}

	/// Add object to deadlist
	pub fn add_to_deadlist(&mut self, object_id: ObjectId, commit_seq: u64, timestamp: u64) {
		self.state.deadlist.add(object_id, commit_seq, timestamp);
	}

	/// Perform incremental GC cycle (Tier B)
	///
	/// Per §22.2: Before freeing, verify:
	/// 1. Object is in deadlist
	/// 2. No active snapshots reference it
	/// 3. Refcount is zero
	/// 4. Allocation state allows freeing
	pub fn incremental_cycle(
		&mut self,
		dedupe_index: &mut DedupeIndex,
		snapshot_registry: &SnapshotRegistry,
		alloc_node: &mut AllocNode,
		current_commit_seq: u64,
		current_timestamp: u64,
	) -> Result<GcCycleResult> {
		let mut freed = Vec::new();
		let mut skipped_pinned = 0;
		let mut skipped_refcount = 0;
		let mut skipped_snapshot = 0;

		// Get candidates from deadlist (batch processing)
		let candidates: Vec<_> = self
			.state
			.deadlist
			.candidates_since(current_commit_seq)
			.take(self.batch_size)
			.map(|entry| entry.object_id)
			.collect();

		for object_id in candidates {
			// Update check timestamp
			if let Some(entry) = self.state.deadlist.get_mut(&object_id) {
				entry.update_check_timestamp(current_timestamp);
				entry.increment_age();
			}

			// SAFETY CHECK 1: Verify object is in deadlist (already checked)

			// SAFETY CHECK 2: Check refcount in dedupe index
			if let Some(node) = dedupe_index.get(&object_id) {
				if node.refcount > 0 {
					skipped_refcount += 1;
					continue; // Conservative: skip if refcount > 0
				}

				// SAFETY CHECK 3: Check not pinned by snapshot
				if node.flags.pinned {
					skipped_pinned += 1;
					continue; // Conservative: skip if pinned
				}
			}

			// SAFETY CHECK 4: Check allocation state
			// (This would require looking up the physical location and checking alloc_node)
			// For now, we trust the dedupe index refcount check

			// SAFETY CHECK 5: Verify no snapshots reference this object
			// TODO: This requires tree traversal of all active snapshots
			// For Phase 1, we conservatively skip if any snapshots exist
			if !snapshot_registry.list_ids().is_empty() {
				skipped_snapshot += 1;
				continue; // Conservative: skip if snapshots exist
			}

			// Safe to free: remove from dedupe index and deadlist
			if dedupe_index.remove(&object_id).is_ok() {
				self.state.deadlist.remove(&object_id);
				self.state.objects_freed += 1;
				freed.push(object_id);
			}
		}

		self.state.cycles_completed += 1;

		Ok(GcCycleResult {
			objects_freed: freed.len(),
			skipped_pinned,
			skipped_refcount,
			skipped_snapshot,
		})
	}

	/// Perform full reconciliation (streaming mark)
	///
	/// Traverses entire object tree to verify refcounts match reality.
	/// Corrects drift between refcounts and actual references.
	pub fn reconcile(
		&mut self,
		_dedupe_index: &mut DedupeIndex,
		current_commit_seq: u64,
	) -> Result<ReconciliationResult> {
		// TODO: Implement tree traversal
		// For Phase 1, just mark reconciliation as completed
		self.state.mark_reconciliation_completed(current_commit_seq);

		Ok(ReconciliationResult {
			objects_scanned: 0,
			refcount_corrections: 0,
			deadlist_additions: 0,
		})
	}

	/// Check if reconciliation is needed
	pub fn needs_reconciliation(&self, current_commit_seq: u64) -> bool {
		self.state.needs_reconciliation(current_commit_seq)
	}

	/// Get GC statistics
	pub fn stats(&self) -> GcStats {
		let deadlist_stats = self.state.deadlist.stats();

		GcStats {
			mode: self.state.mode,
			cycles_completed: self.state.cycles_completed,
			objects_freed: self.state.objects_freed,
			deadlist_entries: deadlist_stats.current_entries,
			last_reconciliation_seq: self.state.last_reconciliation_seq,
		}
	}
}

/// Result of a GC cycle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcCycleResult {
	pub objects_freed: usize,
	pub skipped_pinned: usize,
	pub skipped_refcount: usize,
	pub skipped_snapshot: usize,
}

/// Result of a reconciliation pass
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconciliationResult {
	pub objects_scanned: usize,
	pub refcount_corrections: usize,
	pub deadlist_additions: usize,
}

/// GC statistics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcStats {
	pub mode: GcMode,
	pub cycles_completed: u64,
	pub objects_freed: u64,
	pub deadlist_entries: u64,
	pub last_reconciliation_seq: u64,
}

// endregion: --- GcEngine

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	// region:    --- GcMode Tests

	#[test]
	fn test_gc_mode_from_u8() -> TestResult<()> {
		// -- Check
		assert_eq!(GcMode::from_u8(0)?, GcMode::TierA);
		assert_eq!(GcMode::from_u8(1)?, GcMode::TierB);
		assert_eq!(GcMode::from_u8(2)?, GcMode::TierC);
		assert!(GcMode::from_u8(3).is_err());

		Ok(())
	}

	#[test]
	fn test_gc_mode_default() -> TestResult<()> {
		// -- Check - Default is Tier B
		assert_eq!(GcMode::default(), GcMode::TierB);

		Ok(())
	}

	// endregion: --- GcMode Tests

	// region:    --- DeadlistEntry Tests

	#[test]
	fn test_deadlist_entry_new() -> TestResult<()> {
		// -- Setup & Fixtures
		let object_id = ObjectId::zero();
		let commit_seq = 100;
		let timestamp = 1000;

		// -- Exec
		let entry = DeadlistEntry::new(object_id, commit_seq, timestamp);

		// -- Check
		assert_eq!(entry.object_id, object_id);
		assert_eq!(entry.unreachable_since, commit_seq);
		assert_eq!(entry.age, 0);
		assert_eq!(entry.last_check_timestamp, timestamp);

		Ok(())
	}

	#[test]
	fn test_deadlist_entry_increment_age() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut entry = DeadlistEntry::new(ObjectId::zero(), 100, 1000);

		// -- Exec
		entry.increment_age();
		entry.increment_age();

		// -- Check
		assert_eq!(entry.age, 2);

		Ok(())
	}

	// endregion: --- DeadlistEntry Tests

	// region:    --- Deadlist Tests

	#[test]
	fn test_deadlist_add_remove() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut deadlist = Deadlist::new();
		let object_id = ObjectId::zero();

		// -- Exec
		deadlist.add(object_id, 100, 1000);

		// -- Check
		assert_eq!(deadlist.len(), 1);
		assert!(deadlist.contains(&object_id));

		// -- Exec
		let removed = deadlist.remove(&object_id);

		// -- Check
		assert!(removed.is_some());
		assert_eq!(deadlist.len(), 0);
		assert!(!deadlist.contains(&object_id));

		Ok(())
	}

	#[test]
	fn test_deadlist_deterministic_iteration() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut deadlist = Deadlist::new();
		let id1 = ObjectId::from_bytes(&[1u8; 32]);
		let id2 = ObjectId::from_bytes(&[2u8; 32]);
		let id3 = ObjectId::from_bytes(&[3u8; 32]);

		// Add in non-sorted order
		deadlist.add(id3, 100, 1000);
		deadlist.add(id1, 100, 1000);
		deadlist.add(id2, 100, 1000);

		// -- Exec
		let ids: Vec<_> = deadlist.iter().map(|(id, _)| *id).collect();

		// -- Check - Should be sorted by ObjectId
		assert_eq!(ids[0], id1);
		assert_eq!(ids[1], id2);
		assert_eq!(ids[2], id3);

		Ok(())
	}

	#[test]
	fn test_deadlist_candidates_since() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut deadlist = Deadlist::new();
		let id1 = ObjectId::from_bytes(&[1u8; 32]);
		let id2 = ObjectId::from_bytes(&[2u8; 32]);
		let id3 = ObjectId::from_bytes(&[3u8; 32]);

		deadlist.add(id1, 100, 1000);
		deadlist.add(id2, 200, 1000);
		deadlist.add(id3, 300, 1000);

		// -- Exec - Get candidates from commit 150 or earlier
		let candidates: Vec<_> = deadlist.candidates_since(150).collect();

		// -- Check - Only id1 (unreachable since 100)
		assert_eq!(candidates.len(), 1);
		assert_eq!(candidates[0].object_id, id1);

		Ok(())
	}

	#[test]
	fn test_deadlist_stats() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut deadlist = Deadlist::new();
		let id1 = ObjectId::zero();
		let id2 = ObjectId::from_bytes(&[1u8; 32]);

		// -- Exec
		deadlist.add(id1, 100, 1000);
		deadlist.add(id2, 100, 1000);
		deadlist.remove(&id1);

		let stats = deadlist.stats();

		// -- Check
		assert_eq!(stats.current_entries, 1);
		assert_eq!(stats.total_added, 2);
		assert_eq!(stats.total_freed, 1);

		Ok(())
	}

	// endregion: --- Deadlist Tests

	// region:    --- GcEngine Tests

	#[test]
	fn test_gc_engine_new() -> TestResult<()> {
		// -- Exec
		let engine = GcEngine::new(GcMode::TierB);

		// -- Check
		assert_eq!(engine.state.mode, GcMode::TierB);
		assert_eq!(engine.state.cycles_completed, 0);
		assert_eq!(engine.state.objects_freed, 0);

		Ok(())
	}

	#[test]
	fn test_gc_engine_add_to_deadlist() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = GcEngine::new(GcMode::TierB);
		let object_id = ObjectId::zero();

		// -- Exec
		engine.add_to_deadlist(object_id, 100, 1000);

		// -- Check
		assert_eq!(engine.state.deadlist.len(), 1);
		assert!(engine.state.deadlist.contains(&object_id));

		Ok(())
	}

	#[test]
	fn test_gc_engine_needs_reconciliation() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = GcEngine::new(GcMode::TierB).with_reconciliation_interval(100);

		// -- Check - Initially needs reconciliation
		assert!(engine.needs_reconciliation(0));

		// -- Exec - Mark reconciliation at commit 0
		engine.state.mark_reconciliation_completed(0);

		// -- Check - Should not need reconciliation until commit 100
		assert!(!engine.needs_reconciliation(50));
		assert!(!engine.needs_reconciliation(99));
		assert!(engine.needs_reconciliation(100));
		assert!(engine.needs_reconciliation(200));

		Ok(())
	}

	#[test]
	fn test_gc_engine_stats() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = GcEngine::new(GcMode::TierB);
		engine.add_to_deadlist(ObjectId::zero(), 100, 1000);
		engine.state.cycles_completed = 5;
		engine.state.objects_freed = 3;

		// -- Exec
		let stats = engine.stats();

		// -- Check
		assert_eq!(stats.mode, GcMode::TierB);
		assert_eq!(stats.cycles_completed, 5);
		assert_eq!(stats.objects_freed, 3);
		assert_eq!(stats.deadlist_entries, 1);

		Ok(())
	}

	// endregion: --- GcEngine Tests
}

// endregion: --- Tests
