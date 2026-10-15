//! Background Scrubber for LedgerFS v7
//!
//! # Status
//!
//! Scheduling, throttling, and the per-object corruption bookkeeping are real.
//! The verification is not: `Scrubber::scrub_batch` walks the dedupe index
//! without reading anything, and `attempt_repair` relocates without reading the
//! alternate location, so both count work done rather than confirm it. None of
//! the three verification kinds listed below is performed. Nothing in the host
//! `Store` calls either entry point — `KNOWN_ISSUES.md` §11.
//!
//! Per §40 of FORMAT.md:
//! - Verify stored_hash and ObjectId checksums
//! - Detect and report corruption
//! - Integrate with repair hooks (mirror/parity reads)
//! - Emit audit events on corruption
//! - Throttle scheduling to prevent writer starvation
//!
//! ## Scrubber Operation (§40.1)
//!
//! The scrubber performs three types of verification:
//! 1. **Checksum verification**: Read and verify all object checksums
//! 2. **Tree walk**: Validate B-tree structure integrity
//! 3. **Reference validation**: Verify refcounts match actual references
//!
//! ## Scheduling (§40.2)
//!
//! - **Weekly**: Default schedule for production systems
//! - **On-demand**: Operator-initiated scrub
//! - **Adaptive**: More frequent scrub if errors detected
//!
//! ## Throttling
//!
//! Scrubber must not starve writers. Uses:
//! - Configurable sleep intervals between objects
//! - Adaptive throttling based on write queue depth
//! - Pause/resume API for critical write workloads

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::dedupe::{DedupeIndex, PhysicalLocator, RelocationReason};
use crate::{Error, Result};
use ledgerfs::object::ObjectId;

// region:    --- ScrubberMode

/// Scrubber operation mode
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum ScrubberMode {
	/// Idle - scrubber not running
	#[default]
	Idle = 0,

	/// Active - scrubber running
	Active = 1,

	/// Paused - temporarily suspended
	Paused = 2,
}

// endregion: --- ScrubberMode

// region:    --- CorruptionEvent

/// Corruption event detected by scrubber
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorruptionEvent {
	/// Schema version
	pub schema_version: u16,

	/// Object with corruption
	pub object_id: ObjectId,

	/// Timestamp when corruption detected (unix epoch seconds)
	pub timestamp: u64,

	/// Type of corruption detected
	pub corruption_type: CorruptionType,

	/// Detailed reason
	pub reason: String,

	/// Whether repair was attempted
	pub repair_attempted: bool,

	/// Whether repair was successful
	pub repair_successful: bool,
}

impl CorruptionEvent {
	const SCHEMA_VERSION: u16 = 1;

	/// Create new corruption event
	pub fn new(object_id: ObjectId, timestamp: u64, corruption_type: CorruptionType, reason: String) -> Self {
		Self {
			schema_version: Self::SCHEMA_VERSION,
			object_id,
			timestamp,
			corruption_type,
			reason,
			repair_attempted: false,
			repair_successful: false,
		}
	}

	/// Mark repair as attempted
	pub fn mark_repair_attempted(&mut self, successful: bool) {
		self.repair_attempted = true;
		self.repair_successful = successful;
	}
}

/// Type of corruption detected
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum CorruptionType {
	/// Stored hash mismatch
	HashMismatch = 0,

	/// ObjectId mismatch (content hash doesn't match ID)
	ObjectIdMismatch = 1,

	/// Read I/O error
	ReadError = 2,

	/// Invalid object structure
	InvalidStructure = 3,
}

// endregion: --- CorruptionEvent

// region:    --- ScrubStats

/// Scrubber statistics
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubStats {
	/// Total objects scanned
	pub objects_scanned: u64,

	/// Objects verified successfully
	pub objects_verified: u64,

	/// Corruption events detected
	pub corruptions_detected: u64,

	/// Repairs attempted
	pub repairs_attempted: u64,

	/// Repairs successful
	pub repairs_successful: u64,

	/// Last scrub start timestamp
	pub last_scrub_start: u64,

	/// Last scrub completion timestamp
	pub last_scrub_complete: u64,
}

// endregion: --- ScrubStats

// region:    --- ThrottleConfig

/// Throttling configuration for scrubber
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleConfig {
	/// Sleep duration in milliseconds after each object
	pub sleep_ms_per_object: u64,

	/// Maximum objects to scrub per batch before checking write queue
	pub objects_per_batch: usize,

	/// Minimum write queue depth to trigger pause
	pub pause_threshold_queue_depth: usize,

	/// Resume scrubbing when queue depth drops below this
	pub resume_threshold_queue_depth: usize,
}

impl Default for ThrottleConfig {
	fn default() -> Self {
		Self {
			sleep_ms_per_object: 10,          // 10ms per object
			objects_per_batch: 100,           // Check every 100 objects
			pause_threshold_queue_depth: 50,  // Pause if 50+ writes queued
			resume_threshold_queue_depth: 10, // Resume when < 10 writes queued
		}
	}
}

impl ThrottleConfig {
	/// Create aggressive throttling (slow scrubbing, writer-friendly)
	pub fn aggressive() -> Self {
		Self {
			sleep_ms_per_object: 50,
			objects_per_batch: 50,
			pause_threshold_queue_depth: 20,
			resume_threshold_queue_depth: 5,
		}
	}

	/// Create minimal throttling (fast scrubbing, may impact writers)
	pub fn minimal() -> Self {
		Self {
			sleep_ms_per_object: 1,
			objects_per_batch: 500,
			pause_threshold_queue_depth: 100,
			resume_threshold_queue_depth: 50,
		}
	}
}

// endregion: --- ThrottleConfig

// region:    --- ScrubberState

/// Scrubber internal state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrubberState {
	/// Current mode
	pub mode: ScrubberMode,

	/// Statistics
	pub stats: ScrubStats,

	/// Throttling configuration
	pub throttle_config: ThrottleConfig,

	/// Corruption events (keyed by object_id for determinism)
	pub corruption_events: BTreeMap<ObjectId, CorruptionEvent>,

	/// Objects scheduled for verification
	/// (BTreeMap for deterministic iteration)
	pub verification_queue: BTreeMap<ObjectId, PhysicalLocator>,

	/// Last object scanned (for resumable scrubbing)
	pub last_scanned_object: Option<ObjectId>,
}

impl ScrubberState {
	/// Create new scrubber state
	pub fn new() -> Self {
		Self {
			mode: ScrubberMode::Idle,
			stats: ScrubStats::default(),
			throttle_config: ThrottleConfig::default(),
			corruption_events: BTreeMap::new(),
			verification_queue: BTreeMap::new(),
			last_scanned_object: None,
		}
	}

	/// With custom throttle config
	pub fn with_throttle_config(mut self, config: ThrottleConfig) -> Self {
		self.throttle_config = config;
		self
	}
}

impl Default for ScrubberState {
	fn default() -> Self {
		Self::new()
	}
}

// endregion: --- ScrubberState

// region:    --- Scrubber

/// Issue PREFETCHNTA for the pointer provided.
///
/// Prevents the scrubber's sequential scan from evicting hot BTree metadata
/// from L1/L2. No-op on non-x86_64 targets and in test builds.
#[inline]
fn prefetch_nta(ptr: *const u8) {
	#[cfg(all(target_arch = "x86_64", not(test)))]
	// SAFETY: prefetch is a pure hint — it never faults and has no observable
	// side-effects beyond cache state. The pointer need not be valid for a read.
	unsafe {
		core::arch::x86_64::_mm_prefetch(ptr as *const i8, core::arch::x86_64::_MM_HINT_NTA);
	}
	// Suppress unused-variable warning on non-x86_64 / test targets.
	let _ = ptr;
}

/// Background scrubber engine
pub struct Scrubber {
	/// Scrubber state
	state: ScrubberState,
}

impl Scrubber {
	/// Create new scrubber
	pub fn new() -> Self {
		Self {
			state: ScrubberState::new(),
		}
	}

	/// Create scrubber with custom throttle config
	pub fn with_throttle_config(config: ThrottleConfig) -> Self {
		Self {
			state: ScrubberState::new().with_throttle_config(config),
		}
	}

	/// Start scrubbing
	pub fn start(&mut self, timestamp: u64) {
		self.state.mode = ScrubberMode::Active;
		self.state.stats.last_scrub_start = timestamp;
	}

	/// Pause scrubbing (for critical write workloads)
	pub fn pause(&mut self) {
		if self.state.mode == ScrubberMode::Active {
			self.state.mode = ScrubberMode::Paused;
		}
	}

	/// Resume scrubbing
	pub fn resume(&mut self) {
		if self.state.mode == ScrubberMode::Paused {
			self.state.mode = ScrubberMode::Active;
		}
	}

	/// Stop scrubbing
	pub fn stop(&mut self, timestamp: u64) {
		self.state.mode = ScrubberMode::Idle;
		self.state.stats.last_scrub_complete = timestamp;
	}

	/// Verify single object (hash check)
	///
	/// Per §40.1: Verify stored_hash and ObjectId match actual data.
	/// On mismatch, emit corruption event and attempt repair.
	pub fn verify_object(
		&mut self,
		object_id: &ObjectId,
		stored_data: &[u8],
		stored_hash: &[u8; 32],
		timestamp: u64,
	) -> Result<bool> {
		self.state.stats.objects_scanned += 1;

		// Verify stored hash matches data
		let computed_hash = blake3::hash(stored_data);
		if computed_hash.as_bytes() != stored_hash {
			// Corruption detected: hash mismatch
			let event = CorruptionEvent::new(
				*object_id,
				timestamp,
				CorruptionType::HashMismatch,
				"Stored hash does not match computed hash".into(),
			);

			self.state.corruption_events.insert(*object_id, event);
			self.state.stats.corruptions_detected += 1;

			return Ok(false);
		}

		// Verify ObjectId matches content hash
		if computed_hash.as_bytes() != object_id.as_bytes() {
			// Corruption detected: ObjectId mismatch
			let event = CorruptionEvent::new(
				*object_id,
				timestamp,
				CorruptionType::ObjectIdMismatch,
				"ObjectId does not match content hash".into(),
			);

			self.state.corruption_events.insert(*object_id, event);
			self.state.stats.corruptions_detected += 1;

			return Ok(false);
		}

		// Verification successful
		self.state.stats.objects_verified += 1;
		Ok(true)
	}

	/// Attempt repair of corrupted object
	///
	/// Marks the primary location stale, then relocates to the alternate
	/// locator supplied. Steps 2 and 3 of the intended sequence — read the
	/// alternate, verify its hash — are not performed, so the relocation is
	/// recorded as a successful repair on the strength of the locator alone,
	/// and a `true` return means an attempt was made rather than a repair
	/// confirmed. See `KNOWN_ISSUES.md` §11.
	pub fn attempt_repair(
		&mut self,
		object_id: &ObjectId,
		dedupe_index: &mut DedupeIndex,
		alternate_locator: Option<PhysicalLocator>,
		timestamp: u64,
	) -> Result<bool> {
		self.state.stats.repairs_attempted += 1;

		// Get corruption event
		let event = self
			.state
			.corruption_events
			.get_mut(object_id)
			.ok_or(Error::Custom("Corruption event not found"))?;

		event.repair_attempted = true;

		// Mark primary location as stale
		dedupe_index.mark_location_stale(object_id)?;

		// If no alternate location provided, repair failed
		let Some(alt_locator) = alternate_locator else {
			return Ok(false);
		};

		// The alternate location is not read, so nothing is verified: the
		// relocation below is recorded as a successful repair on the strength
		// of the locator alone. `repairs_successful` therefore counts repairs
		// that were attempted, not repairs that were confirmed — see
		// `KNOWN_ISSUES.md` §11.

		// Relocate to alternate location
		dedupe_index.relocate(object_id, alt_locator, timestamp, RelocationReason::CorruptionRepair)?;

		// Mark as verified
		dedupe_index.mark_location_verified(object_id, timestamp)?;

		// Update event
		event.repair_successful = true;
		self.state.stats.repairs_successful += 1;

		Ok(true)
	}

	/// Scrub batch of objects from dedupe index
	///
	/// Returns the number of entries walked in this batch. No object is read,
	/// so that is not the number verified — the loop advances a cursor and
	/// counts. Respects throttling configuration: if `write_queue_depth` meets
	/// or exceeds `pause_threshold_queue_depth`, the scrubber pauses itself and
	/// returns 0 immediately.
	pub fn scrub_batch(
		&mut self,
		dedupe_index: &DedupeIndex,
		_current_timestamp: u64,
		write_queue_depth: usize,
	) -> Result<usize> {
		if self.state.mode != ScrubberMode::Active {
			return Ok(0);
		}

		// Auto-pause when write pressure is high (§40.2).
		if self.should_pause(write_queue_depth) {
			self.pause();
			return Ok(0);
		}

		let batch_size = self.state.throttle_config.objects_per_batch;
		let mut scanned = 0;

		// Get objects from dedupe index
		let objects: Vec<_> = dedupe_index
			.iter()
			.filter(|(id, _)| {
				// Skip if we've already scanned this object
				if let Some(last) = &self.state.last_scanned_object {
					id > &last
				} else {
					true
				}
			})
			.take(batch_size)
			.map(|(id, node)| (*id, node.locator))
			.collect();

		for (i, (object_id, _locator)) in objects.iter().enumerate() {
			// Prefetch 2 entries ahead (NTA = non-temporal, avoids L1/L2 pollution).
			if i + 2 < objects.len() {
				let ahead_ptr = &objects[i + 2].0 as *const _ as *const u8;
				prefetch_nta(ahead_ptr);
			}

			// No read is issued. This loop advances the scan cursor and counts
			// entries without looking at any of them, so the count it returns
			// is the number of entries walked, not the number verified — see
			// `KNOWN_ISSUES.md` §11.

			self.state.last_scanned_object = Some(*object_id);
			scanned += 1;
		}

		Ok(scanned)
	}

	/// Check if scrubber should pause based on write queue depth
	pub fn should_pause(&self, write_queue_depth: usize) -> bool {
		write_queue_depth >= self.state.throttle_config.pause_threshold_queue_depth
	}

	/// Check if scrubber should resume based on write queue depth
	pub fn should_resume(&self, write_queue_depth: usize) -> bool {
		write_queue_depth < self.state.throttle_config.resume_threshold_queue_depth
	}

	/// Get scrubber statistics
	pub fn stats(&self) -> &ScrubStats {
		&self.state.stats
	}

	/// Get a reference to the full scrubber state.
	pub fn state(&self) -> &ScrubberState {
		&self.state
	}

	/// Get corruption events (deterministic iteration)
	pub fn corruption_events(&self) -> impl Iterator<Item = (&ObjectId, &CorruptionEvent)> {
		self.state.corruption_events.iter()
	}

	/// Clear corruption events (after acknowledged)
	pub fn clear_corruption_events(&mut self) {
		self.state.corruption_events.clear();
	}
}

impl Default for Scrubber {
	fn default() -> Self {
		Self::new()
	}
}

// endregion: --- Scrubber

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	// region:    --- ScrubberMode Tests

	#[test]
	fn test_scrubber_mode_default() -> TestResult<()> {
		// -- Check
		assert_eq!(ScrubberMode::default(), ScrubberMode::Idle);

		Ok(())
	}

	// endregion: --- ScrubberMode Tests

	// region:    --- CorruptionEvent Tests

	#[test]
	fn test_corruption_event_new() -> TestResult<()> {
		// -- Setup & Fixtures
		let object_id = ObjectId::zero();
		let timestamp = 1000;

		// -- Exec
		let event = CorruptionEvent::new(
			object_id,
			timestamp,
			CorruptionType::HashMismatch,
			"Test corruption".into(),
		);

		// -- Check
		assert_eq!(event.object_id, object_id);
		assert_eq!(event.timestamp, timestamp);
		assert_eq!(event.corruption_type, CorruptionType::HashMismatch);
		assert!(!event.repair_attempted);
		assert!(!event.repair_successful);

		Ok(())
	}

	#[test]
	fn test_corruption_event_mark_repair() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut event = CorruptionEvent::new(ObjectId::zero(), 1000, CorruptionType::HashMismatch, "Test".into());

		// -- Exec
		event.mark_repair_attempted(true);

		// -- Check
		assert!(event.repair_attempted);
		assert!(event.repair_successful);

		Ok(())
	}

	// endregion: --- CorruptionEvent Tests

	// region:    --- ThrottleConfig Tests

	#[test]
	fn test_throttle_config_default() -> TestResult<()> {
		// -- Exec
		let config = ThrottleConfig::default();

		// -- Check
		assert_eq!(config.sleep_ms_per_object, 10);
		assert_eq!(config.objects_per_batch, 100);

		Ok(())
	}

	#[test]
	fn test_throttle_config_aggressive() -> TestResult<()> {
		// -- Exec
		let config = ThrottleConfig::aggressive();

		// -- Check
		assert!(config.sleep_ms_per_object > ThrottleConfig::default().sleep_ms_per_object);
		assert!(config.objects_per_batch < ThrottleConfig::default().objects_per_batch);

		Ok(())
	}

	#[test]
	fn test_throttle_config_minimal() -> TestResult<()> {
		// -- Exec
		let config = ThrottleConfig::minimal();

		// -- Check
		assert!(config.sleep_ms_per_object < ThrottleConfig::default().sleep_ms_per_object);
		assert!(config.objects_per_batch > ThrottleConfig::default().objects_per_batch);

		Ok(())
	}

	// endregion: --- ThrottleConfig Tests

	// region:    --- Scrubber Tests

	#[test]
	fn test_scrubber_new() -> TestResult<()> {
		// -- Exec
		let scrubber = Scrubber::new();

		// -- Check
		assert_eq!(scrubber.state.mode, ScrubberMode::Idle);
		assert_eq!(scrubber.state.stats.objects_scanned, 0);

		Ok(())
	}

	#[test]
	fn test_scrubber_start_stop() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut scrubber = Scrubber::new();

		// -- Exec
		scrubber.start(1000);

		// -- Check
		assert_eq!(scrubber.state.mode, ScrubberMode::Active);
		assert_eq!(scrubber.state.stats.last_scrub_start, 1000);

		// -- Exec
		scrubber.stop(2000);

		// -- Check
		assert_eq!(scrubber.state.mode, ScrubberMode::Idle);
		assert_eq!(scrubber.state.stats.last_scrub_complete, 2000);

		Ok(())
	}

	#[test]
	fn test_scrubber_pause_resume() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut scrubber = Scrubber::new();
		scrubber.start(1000);

		// -- Exec
		scrubber.pause();

		// -- Check
		assert_eq!(scrubber.state.mode, ScrubberMode::Paused);

		// -- Exec
		scrubber.resume();

		// -- Check
		assert_eq!(scrubber.state.mode, ScrubberMode::Active);

		Ok(())
	}

	#[test]
	fn test_scrubber_verify_object_success() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut scrubber = Scrubber::new();
		let data = b"test data";
		let hash = blake3::hash(data);
		let object_id = ObjectId::from_bytes(hash.as_bytes());

		// -- Exec
		let verified = scrubber.verify_object(&object_id, data, hash.as_bytes(), 1000)?;

		// -- Check
		assert!(verified);
		assert_eq!(scrubber.state.stats.objects_scanned, 1);
		assert_eq!(scrubber.state.stats.objects_verified, 1);
		assert_eq!(scrubber.state.stats.corruptions_detected, 0);

		Ok(())
	}

	#[test]
	fn test_scrubber_verify_object_hash_mismatch() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut scrubber = Scrubber::new();
		let data = b"test data";
		let wrong_hash = [0u8; 32]; // Wrong hash
		let object_id = ObjectId::from_bytes(&wrong_hash);

		// -- Exec
		let verified = scrubber.verify_object(&object_id, data, &wrong_hash, 1000)?;

		// -- Check
		assert!(!verified);
		assert_eq!(scrubber.state.stats.objects_scanned, 1);
		assert_eq!(scrubber.state.stats.objects_verified, 0);
		assert_eq!(scrubber.state.stats.corruptions_detected, 1);

		// -- Check corruption event was created
		assert!(scrubber.state.corruption_events.contains_key(&object_id));

		Ok(())
	}

	#[test]
	fn test_scrubber_should_pause_resume() -> TestResult<()> {
		// -- Setup & Fixtures
		let scrubber = Scrubber::new();
		let config = scrubber.state.throttle_config;

		// -- Check - Should pause when queue depth exceeds threshold
		assert!(!scrubber.should_pause(config.pause_threshold_queue_depth - 1));
		assert!(scrubber.should_pause(config.pause_threshold_queue_depth));
		assert!(scrubber.should_pause(config.pause_threshold_queue_depth + 10));

		// -- Check - Should resume when queue depth drops
		assert!(scrubber.should_resume(config.resume_threshold_queue_depth - 1));
		assert!(!scrubber.should_resume(config.resume_threshold_queue_depth));
		assert!(!scrubber.should_resume(config.resume_threshold_queue_depth + 10));

		Ok(())
	}

	#[test]
	fn test_scrub_batch_auto_pauses_on_high_write_pressure() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut scrubber = Scrubber::new();
		scrubber.start(1000);
		let dedupe_index = DedupeIndex::new();

		// -- Exec - High write pressure (above pause_threshold_queue_depth = 50)
		let high_depth = 60;
		let scanned = scrubber.scrub_batch(&dedupe_index, 2000, high_depth)?;

		// -- Check
		assert_eq!(scanned, 0);
		assert_eq!(scrubber.state().mode, ScrubberMode::Paused);

		Ok(())
	}

	#[test]
	fn test_scrubber_corruption_events_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut scrubber = Scrubber::new();

		let id1 = ObjectId::from_bytes(&[1u8; 32]);
		let id2 = ObjectId::from_bytes(&[2u8; 32]);
		let id3 = ObjectId::from_bytes(&[3u8; 32]);

		// Add in non-sorted order
		scrubber.state.corruption_events.insert(
			id3,
			CorruptionEvent::new(id3, 1000, CorruptionType::HashMismatch, "3".into()),
		);
		scrubber.state.corruption_events.insert(
			id1,
			CorruptionEvent::new(id1, 1000, CorruptionType::HashMismatch, "1".into()),
		);
		scrubber.state.corruption_events.insert(
			id2,
			CorruptionEvent::new(id2, 1000, CorruptionType::HashMismatch, "2".into()),
		);

		// -- Exec
		let events: Vec<_> = scrubber.corruption_events().map(|(id, _)| *id).collect();

		// -- Check - Should be sorted by ObjectId
		assert_eq!(events[0], id1);
		assert_eq!(events[1], id2);
		assert_eq!(events[2], id3);

		Ok(())
	}

	// endregion: --- Scrubber Tests
}

// endregion: --- Tests
