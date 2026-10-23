//! Content-Defined Chunking (CDC) with Gear hash algorithm.
//!
//! This module implements deterministic content-defined chunking using the
//! Gear hash algorithm (an improved rolling hash). CDC enables deduplication
//! by creating chunk boundaries based on content, not position.
//!
//! # Chunking Parameters (§29.1)
//!
//! - **Minimum chunk size**: 16 KiB (prevents tiny chunks)
//! - **Average chunk size**: 64 KiB (target via gear mask)
//! - **Maximum chunk size**: 256 KiB (hard cutoff)
//!
//! # Determinism Guarantee
//!
//! The Gear hash seed is deterministic (derived from a fixed constant, not random).
//! Same input data will ALWAYS produce the same chunk boundaries across:
//! - Different platforms (x86_64, ARM64, etc.)
//! - Different runs
//! - Different machines
//!
//! # Shift Problem Prevention
//!
//! CDC improves over fixed-size chunking by minimizing re-chunking when data
//! is inserted/deleted. Inserting 1 byte at the start should only affect
//! the chunk containing the insertion, not all subsequent chunks.

extern crate alloc;

use alloc::vec::Vec;

/// Minimum chunk size: 16 KiB (prevents tiny chunks from edge cases)
pub const MIN_CHUNK_SIZE: usize = 16 * 1024;

/// Average target chunk size: 64 KiB (controlled by gear mask)
pub const AVG_CHUNK_SIZE: usize = 64 * 1024;

/// Maximum chunk size: 256 KiB (hard cutoff to prevent unbounded chunks)
pub const MAX_CHUNK_SIZE: usize = 256 * 1024;

/// Gear hash mask for 64 KiB average chunk size.
///
/// Mask determines chunk boundary probability:
/// - More bits set → HARDER to match (hash & mask == 0) → LARGER chunks
/// - Fewer bits set → EASIER to match → SMALLER chunks
///
/// For 64 KiB average: After MIN (16KB), need ~48KB more.
/// Probability = 1 in 49152 ≈ 2^16, so need mask where matching is 1 in 2^16
/// This means we need mask with 16 bits set in the low positions
const GEAR_MASK: u64 = 0x0000_0000_0000_ffff; // 16 bits set → 1/65536 match → ~64KB avg

/// Deterministic seed for Gear hash table generation.
///
/// This seed MUST be fixed (not random) to ensure deterministic chunking
/// across all platforms and runs. Changing this value will change all chunk
/// boundaries in existing data.
const GEAR_SEED: u64 = 0x4C45_4447_4552_4653; // "LEDGERFS" in ASCII

/// Gear hash lookup table (256 entries, one per byte value).
///
/// Precomputed using deterministic seed. Each entry is a 64-bit hash
/// value that represents the "gear" contribution of that byte.
struct GearTable {
	table: [u64; 256],
}

impl GearTable {
	/// Generate deterministic Gear hash table from fixed seed.
	///
	/// Uses a simple LCG (Linear Congruential Generator) for deterministic
	/// pseudo-random values. Same seed always produces same table.
	fn new() -> Self {
		let mut table = [0u64; 256];
		let mut rng = GEAR_SEED;

		for slot in &mut table {
			// Simple LCG: X_{n+1} = (a * X_n + c) mod m
			// Constants from Numerical Recipes (good period, fast)
			rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			*slot = rng;
		}

		Self { table }
	}

	/// Get hash value for a byte.
	#[inline]
	fn hash(&self, byte: u8) -> u64 {
		self.table[byte as usize]
	}
}

/// Lazy static Gear table (computed once, shared across all chunkers).
///
/// In a no_std environment, we use UnsafeCell for interior mutability.
/// Since the seed is fixed, the table is always the same.
use core::cell::UnsafeCell;

struct GearTableCell(UnsafeCell<Option<GearTable>>);
// SAFETY: the wrapped cell is only ever written by `get_gear_table` on its
// first call and is read-only thereafter, and that initialization runs
// single-threaded in the boot path (see that function's `# Safety` notes).
// Since the value is only published once fully constructed, sharing the cell
// across threads cannot expose a torn or partially-initialized table.
unsafe impl Sync for GearTableCell {}

static GEAR_TABLE: GearTableCell = GearTableCell(UnsafeCell::new(None));

/// Get the global Gear hash table (initializes on first call).
///
/// # Safety
///
/// This uses UnsafeCell which is safe in this context because:
/// 1. Initialization is idempotent (same table every time)
/// 2. Table is read-only after initialization
/// 3. No concurrent modification (single-threaded kernel context during init)
fn get_gear_table() -> &'static GearTable {
	// SAFETY: see this function's `# Safety` section above; initialization
	// is idempotent, runs single-threaded in the boot path, and the table is
	// read-only once published.
	unsafe {
		let ptr = GEAR_TABLE.0.get();
		if (*ptr).is_none() {
			*ptr = Some(GearTable::new());
		}
		(*ptr).as_ref().unwrap()
	}
}

/// Content-defined chunker using Gear hash algorithm.
///
/// Processes data and returns chunk boundaries based on content.
/// Boundaries are deterministic (same data → same boundaries).
pub struct Chunker {
	/// Current rolling hash value
	hash: u64,

	/// Bytes processed in current chunk
	chunk_size: usize,

	/// Gear hash table reference
	table: &'static GearTable,
}

impl Chunker {
	/// Create a new chunker.
	pub fn new() -> Self {
		Self {
			hash: 0,
			chunk_size: 0,
			table: get_gear_table(),
		}
	}

	/// Process data and return chunk boundary positions.
	///
	/// Returns a Vec of positions where chunks should end. The positions
	/// are relative to the start of the input data slice.
	///
	/// # Example
	///
	/// ```ignore
	/// let chunker = Chunker::new();
	/// let boundaries = chunker.chunk_data(&data)?;
	/// // boundaries might be [65536, 131072, ...] for 64KB-ish chunks
	/// ```
	pub fn chunk_data(&mut self, data: &[u8]) -> Vec<usize> {
		let mut boundaries = Vec::new();

		for (i, &byte) in data.iter().enumerate() {
			// Update rolling hash with new byte
			self.hash = (self.hash << 1).wrapping_add(self.table.hash(byte));
			self.chunk_size += 1;

			// Check for chunk boundary
			if self.should_cut(i + 1 == data.len()) {
				boundaries.push(i + 1); // Position after this byte
				self.reset();
			}
		}

		boundaries
	}

	/// Determine if we should cut a chunk at current position.
	///
	/// Boundary conditions:
	/// 1. End of data: always cut (handles tiny data correctly)
	/// 2. At MAX_CHUNK_SIZE: always cut (hard limit)
	/// 3. Below MIN_CHUNK_SIZE: never cut (except end of data)
	/// 4. Otherwise: cut if hash matches mask (probabilistic)
	#[inline]
	fn should_cut(&self, is_last_byte: bool) -> bool {
		// End of data: always cut (even if below MIN_CHUNK_SIZE)
		// This ensures tiny data still produces a chunk
		if is_last_byte {
			return true;
		}

		// Hard maximum: always cut at MAX_CHUNK_SIZE
		if self.chunk_size >= MAX_CHUNK_SIZE {
			return true;
		}

		// Hard minimum: never cut before MIN_CHUNK_SIZE (except end of data above)
		if self.chunk_size < MIN_CHUNK_SIZE {
			return false;
		}

		// Probabilistic cut: check if hash matches mask
		// With GEAR_MASK having ~50 bits set, probability is ~1/16384 per byte
		// Expected chunk size = MIN_CHUNK_SIZE + 1/P ≈ 16KB + 16KB ≈ 32KB
		// But with the mask tuned for 64KB average, we get the right distribution
		(self.hash & GEAR_MASK) == 0
	}

	/// Reset chunker state for next chunk.
	fn reset(&mut self) {
		self.hash = 0;
		self.chunk_size = 0;
	}
}

impl Default for Chunker {
	fn default() -> Self {
		Self::new()
	}
}

/// Streaming chunker for incremental processing.
///
/// Unlike `Chunker`, this maintains state across multiple `process()` calls,
/// enabling streaming operation for direct-channel writes and large files.
///
/// # Example
///
/// ```ignore
/// let mut chunker = StreamingChunker::new();
///
/// // Process data in multiple calls
/// let boundaries1 = chunker.process(&chunk1)?;
/// let boundaries2 = chunker.process(&chunk2)?;
///
/// // Finalize to get last chunk
/// let final_boundary = chunker.finalize()?;
/// ```
pub struct StreamingChunker {
	/// Current rolling hash
	hash: u64,

	/// Bytes in current chunk
	chunk_size: usize,

	/// Total bytes processed so far (for boundary positions)
	total_processed: usize,

	/// Gear hash table
	table: &'static GearTable,
}

impl StreamingChunker {
	/// Create a new streaming chunker.
	pub fn new() -> Self {
		Self {
			hash: 0,
			chunk_size: 0,
			total_processed: 0,
			table: get_gear_table(),
		}
	}

	/// Process a chunk of data, returning absolute boundary positions.
	///
	/// The returned positions are absolute offsets from the start of the
	/// entire stream (not relative to this data chunk).
	///
	/// This method can be called multiple times with consecutive data chunks.
	pub fn process(&mut self, data: &[u8]) -> Vec<usize> {
		let mut boundaries = Vec::new();

		for &byte in data {
			// Update rolling hash
			self.hash = (self.hash << 1).wrapping_add(self.table.hash(byte));
			self.chunk_size += 1;
			self.total_processed += 1;

			// Check for boundary (but not on last byte, unless it's max chunk)
			if self.should_cut_streaming(false) {
				boundaries.push(self.total_processed);
				self.reset_chunk();
			}
		}

		boundaries
	}

	/// Finalize streaming and return position of final chunk boundary.
	///
	/// Call this when all data has been processed to ensure the last
	/// chunk is properly terminated.
	///
	/// Returns `Some(position)` if there's remaining data, `None` if
	/// the stream ended exactly at a boundary.
	pub fn finalize(self) -> Option<usize> {
		if self.chunk_size > 0 {
			Some(self.total_processed)
		} else {
			None
		}
	}

	/// Check if we should cut a chunk in streaming mode.
	#[inline]
	fn should_cut_streaming(&self, is_finalize: bool) -> bool {
		// Below minimum: never cut
		if self.chunk_size < MIN_CHUNK_SIZE {
			return false;
		}

		// At maximum: always cut
		if self.chunk_size >= MAX_CHUNK_SIZE {
			return true;
		}

		// Finalize: always cut remaining data
		if is_finalize {
			return true;
		}

		// Probabilistic boundary
		(self.hash & GEAR_MASK) == 0
	}

	/// Reset chunk-specific state (but keep total_processed).
	fn reset_chunk(&mut self) {
		self.hash = 0;
		self.chunk_size = 0;
	}
}

impl Default for StreamingChunker {
	fn default() -> Self {
		Self::new()
	}
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	#[test]
	fn test_cdc_constants() -> TestResult<()> {
		// -- Check
		assert_eq!(MIN_CHUNK_SIZE, 16 * 1024);
		assert_eq!(AVG_CHUNK_SIZE, 64 * 1024);
		assert_eq!(MAX_CHUNK_SIZE, 256 * 1024);

		Ok(())
	}

	#[test]
	fn test_cdc_gear_table_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let table1 = GearTable::new();
		let table2 = GearTable::new();

		// -- Check
		for i in 0..256 {
			assert_eq!(table1.table[i], table2.table[i]);
		}

		Ok(())
	}

	#[test]
	fn test_cdc_chunker_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		// Create 512KB of data (should produce multiple chunks)
		let data: Vec<u8> = (0..512 * 1024).map(|i| (i % 256) as u8).collect();

		// -- Exec
		let mut chunker1 = Chunker::new();
		let boundaries1 = chunker1.chunk_data(&data);

		let mut chunker2 = Chunker::new();
		let boundaries2 = chunker2.chunk_data(&data);

		// -- Check
		assert_eq!(boundaries1, boundaries2);
		assert!(!boundaries1.is_empty());

		Ok(())
	}

	#[test]
	fn test_cdc_min_chunk_size_enforced() -> TestResult<()> {
		// -- Setup & Fixtures
		// Create data smaller than MIN_CHUNK_SIZE
		let small_data: Vec<u8> = vec![0; MIN_CHUNK_SIZE - 1];

		// -- Exec
		let mut chunker = Chunker::new();
		let boundaries = chunker.chunk_data(&small_data);

		// -- Check
		// Should have exactly one boundary at end (forced)
		assert_eq!(boundaries.len(), 1);
		assert_eq!(boundaries[0], small_data.len());

		Ok(())
	}

	#[test]
	fn test_cdc_max_chunk_size_enforced() -> TestResult<()> {
		// -- Setup & Fixtures
		// Create data with no natural boundaries (all zeros)
		// Zeros should not trigger gear hash boundaries easily
		let large_data: Vec<u8> = vec![0; MAX_CHUNK_SIZE * 2];

		// -- Exec
		let mut chunker = Chunker::new();
		let boundaries = chunker.chunk_data(&large_data);

		// -- Check
		// Should have at least one boundary due to MAX_CHUNK_SIZE
		assert!(!boundaries.is_empty());

		// First boundary should be at or before MAX_CHUNK_SIZE
		assert!(boundaries[0] <= MAX_CHUNK_SIZE);

		Ok(())
	}

	#[test]
	fn test_cdc_shift_problem() -> TestResult<()> {
		// -- Setup & Fixtures
		// Create base data
		let mut base_data: Vec<u8> = (0..128 * 1024).map(|i| (i % 256) as u8).collect();

		// Chunk base data
		let mut chunker1 = Chunker::new();
		let boundaries1 = chunker1.chunk_data(&base_data);

		// Insert 1 byte at position 1024
		base_data.insert(1024, 0xFF);

		// Chunk modified data
		let mut chunker2 = Chunker::new();
		let boundaries2 = chunker2.chunk_data(&base_data);

		// -- Check
		// Boundaries should differ, but most should remain similar
		// (shift problem: only chunks containing the insertion should change)

		// Count how many boundaries stayed roughly the same (within 10%)
		let mut similar_count = 0;
		for &b1 in &boundaries1 {
			for &b2 in &boundaries2 {
				if b1.abs_diff(b2) < b1 / 10 {
					similar_count += 1;
					break;
				}
			}
		}

		// At least half the boundaries should be similar (shift problem improvement)
		assert!(similar_count >= boundaries1.len() / 2);

		Ok(())
	}

	#[test]
	fn test_cdc_streaming_chunker_single_call() -> TestResult<()> {
		// -- Setup & Fixtures
		let data: Vec<u8> = (0..128 * 1024).map(|i| (i % 256) as u8).collect();

		// -- Exec
		let mut streaming = StreamingChunker::new();
		let boundaries = streaming.process(&data);
		let final_boundary = streaming.finalize();

		// -- Check
		assert!(!boundaries.is_empty() || final_boundary.is_some());

		if let Some(final_pos) = final_boundary {
			assert_eq!(final_pos, data.len());
		}

		Ok(())
	}

	#[test]
	fn test_cdc_streaming_chunker_multiple_calls() -> TestResult<()> {
		// -- Setup & Fixtures
		let chunk1: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
		let chunk2: Vec<u8> = (64 * 1024..128 * 1024).map(|i| (i % 256) as u8).collect();

		// -- Exec
		let mut streaming = StreamingChunker::new();
		let boundaries1 = streaming.process(&chunk1);
		let boundaries2 = streaming.process(&chunk2);
		let final_boundary = streaming.finalize();

		// -- Check
		// All boundaries should be absolute positions
		for &b in &boundaries1 {
			assert!(b <= chunk1.len());
		}

		for &b in &boundaries2 {
			assert!(b > chunk1.len());
			assert!(b <= chunk1.len() + chunk2.len());
		}

		if let Some(final_pos) = final_boundary {
			assert_eq!(final_pos, chunk1.len() + chunk2.len());
		}

		Ok(())
	}

	#[test]
	fn test_cdc_streaming_vs_batch_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let data: Vec<u8> = (0..256 * 1024).map(|i| (i % 256) as u8).collect();

		// Batch chunking
		let mut batch_chunker = Chunker::new();
		let batch_boundaries = batch_chunker.chunk_data(&data);

		// Streaming chunking (split into 4 chunks)
		let mut streaming = StreamingChunker::new();
		let mut streaming_boundaries = Vec::new();

		for chunk_data in data.chunks(64 * 1024) {
			streaming_boundaries.extend(streaming.process(chunk_data));
		}

		if let Some(final_pos) = streaming.finalize() {
			streaming_boundaries.push(final_pos);
		}

		// -- Check
		// Both should produce same boundaries
		assert_eq!(batch_boundaries, streaming_boundaries);

		Ok(())
	}

	#[test]
	fn test_cdc_boundary_distribution() -> TestResult<()> {
		// -- Setup & Fixtures
		// Create 1MB of pseudo-random data
		let mut data: Vec<u8> = Vec::with_capacity(1024 * 1024);
		let mut rng = 12345u64;
		for _ in 0..1024 * 1024 {
			rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
			data.push((rng >> 16) as u8);
		}

		// -- Exec
		let mut chunker = Chunker::new();
		let boundaries = chunker.chunk_data(&data);

		// -- Check
		assert!(!boundaries.is_empty());

		// Calculate average chunk size
		let mut prev = 0;
		let mut sizes = Vec::new();
		for &boundary in &boundaries {
			sizes.push(boundary - prev);
			prev = boundary;
		}

		let avg_size: usize = sizes.iter().sum::<usize>() / sizes.len();

		// Average should be within 50% of target (32KB - 96KB range)
		// Note: actual average may vary based on data, but should be reasonable
		assert!(avg_size >= AVG_CHUNK_SIZE / 2);
		assert!(avg_size <= AVG_CHUNK_SIZE * 2);

		// All chunks should respect min/max
		for &size in &sizes[..sizes.len() - 1] {
			// Exclude last chunk
			assert!(size >= MIN_CHUNK_SIZE);
			assert!(size <= MAX_CHUNK_SIZE);
		}

		Ok(())
	}
}

// endregion: --- Tests
