//! Block allocator for LedgerFS v7
//!
//! Per §14 and §25 of FORMAT.md:
//! - Finds free regions for object placement
//! - Enforces 512-byte alignment
//! - Maintains emergency reserves (1% or 100MiB min)
//! - Triggers read-only mode on reserve exhaustion

extern crate alloc;

use crate::alloc_node::AllocNode;
use crate::error::{Error, Result};
use crate::mode::FsMode;

/// Block size in bytes.
///
/// Must equal [`crate::BLOCK_SIZE`] (4 KiB), the granularity every other
/// piece of this crate's write path allocates/addresses in:
/// `AllocNode`/`PhysicalExtent`'s block counts (see `MountedVolume::
/// bootstrap_genesis`'s `RESERVED_HEADER_BLOCKS`-based extent setup),
/// `PlacedObject::physical_lba`/`block_count`, and `AtaBlockDevice::
/// write_blocks`'s `lba` parameter (explicitly documented as "a
/// partition-relative LedgerFS block index (4 KiB units)") are all in this
/// same unit. This used to be hardcoded to `512` (an ATA *sector* size, a
/// stale leftover from an earlier sub-block-granular allocator design this
/// crate no longer implements); `Allocator::allocate`'s `size_bytes`→
/// `block_count` conversion then produced a count of 512-byte units that
/// `AllocNode::find_free_extent`/`split_and_allocate` and the actual
/// `write_blocks` call downstream both silently treated as a count of
/// 4 KiB units instead: any object over 512 bytes was allocated (and
/// then zero-padded-and-written) as if it were up to 8x larger than it
/// actually was, clobbering whatever real, still-referenced content
/// happened to sit in the extra blocks immediately after it. Found via
/// code audit while hardening the data volume's write path; no single
/// small test object triggered it, but any object over 512 bytes (an
/// `Inode` with a couple of xattrs, a `DirNode` with more than a couple of
/// entries) would.
pub const BLOCK_SIZE_BYTES: u64 = crate::BLOCK_SIZE as u64;

/// Minimum emergency reserve: 1% of capacity OR 100 MiB
const MIN_RESERVE_PERCENT: f64 = 0.01;
const MIN_RESERVE_BYTES: u64 = 100 * 1024 * 1024; // 100 MiB

/// Recommended emergency reserve: 5% of capacity
const RECOMMENDED_RESERVE_PERCENT: f64 = 0.05;

/// Block allocator configuration
#[derive(Debug, Clone)]
pub struct AllocatorConfig {
	/// Total capacity in blocks
	pub total_blocks: u64,

	/// Emergency reserve size in blocks (computed from total_blocks)
	pub reserve_blocks: u64,

	/// Warn threshold (e.g., 50% of reserve consumed)
	pub warn_threshold_blocks: u64,
}

impl AllocatorConfig {
	/// Create configuration with recommended defaults
	pub fn new(total_blocks: u64) -> Self {
		let total_bytes = total_blocks * BLOCK_SIZE_BYTES;

		// Reserve is max of 1% or 100 MiB, prefer 5% for production,
		// but never more than 25% of total so small / test filesystems
		// don't immediately enter ReadOnlySafe mode.
		let min_reserve = ((total_bytes as f64 * MIN_RESERVE_PERCENT) as u64).max(MIN_RESERVE_BYTES);
		let recommended_reserve = (total_bytes as f64 * RECOMMENDED_RESERVE_PERCENT) as u64;
		let reserve_bytes = recommended_reserve.max(min_reserve).min(total_bytes / 4);
		let reserve_blocks = reserve_bytes.div_ceil(BLOCK_SIZE_BYTES);

		// Warn at 50% reserve consumption
		let warn_threshold_blocks = reserve_blocks / 2;

		Self {
			total_blocks,
			reserve_blocks,
			warn_threshold_blocks,
		}
	}

	/// Create configuration with custom reserve size
	pub fn with_reserve_blocks(total_blocks: u64, reserve_blocks: u64) -> Self {
		let warn_threshold_blocks = reserve_blocks / 2;
		Self {
			total_blocks,
			reserve_blocks,
			warn_threshold_blocks,
		}
	}
}

/// Block allocator state
pub struct Allocator {
	/// Allocator configuration
	config: AllocatorConfig,

	/// Filesystem mode (ReadWrite or ReadOnlySafe)
	mode: FsMode,

	/// Total free blocks (excluding reserve)
	user_free_blocks: u64,

	/// Reserved blocks (for emergency operations)
	reserved_free_blocks: u64,
}

impl Allocator {
	/// Create new allocator
	pub fn new(config: AllocatorConfig) -> Self {
		Self {
			config,
			mode: FsMode::ReadWrite,
			user_free_blocks: 0,
			reserved_free_blocks: 0,
		}
	}

	/// Initialize allocator from allocation tree
	pub fn init_from_tree(&mut self, alloc_node: &AllocNode) -> Result<()> {
		// Count free blocks
		let total_free = alloc_node.free_blocks;

		// Reserve blocks for emergency
		if total_free < self.config.reserve_blocks {
			// Critically low space - enter read-only mode
			self.mode = FsMode::ReadOnlySafe;
			self.user_free_blocks = 0;
			self.reserved_free_blocks = total_free;
		} else {
			self.user_free_blocks = total_free - self.config.reserve_blocks;
			self.reserved_free_blocks = self.config.reserve_blocks;
		}

		Ok(())
	}

	/// Find free extent with specified size
	///
	/// Returns (start_lba, block_count) on success, both in [`BLOCK_SIZE_BYTES`]
	/// (4 KiB) units.
	pub fn allocate(&mut self, alloc_node: &AllocNode, size_bytes: u64, allow_reserve: bool) -> Result<(u64, u32)> {
		// Check mode
		if self.mode == FsMode::ReadOnlySafe && !allow_reserve {
			return Err(Error::custom("Filesystem in read-only safe mode"));
		}

		// Convert size to blocks (round up to a whole BLOCK_SIZE_BYTES block).
		let block_count = size_bytes.div_ceil(BLOCK_SIZE_BYTES) as u32;

		// Check if we have enough space
		if allow_reserve {
			// Emergency allocation - can use reserve
			if self.user_free_blocks + self.reserved_free_blocks < block_count as u64 {
				return Err(Error::custom("Out of space (including reserve)"));
			}
		} else {
			// Normal allocation - cannot use reserve
			if self.user_free_blocks < block_count as u64 {
				return Err(Error::custom("Out of space (user quota exceeded)"));
			}
		}

		// Find free extent in allocation tree. Every `start_lba` this returns
		// is already a whole `BLOCK_SIZE_BYTES` block index: this crate
		// never allocates at sub-block granularity, so there is no
		// additional alignment to verify here (a leftover check from an
		// earlier sub-block-granular design was removed; see
		// `BLOCK_SIZE_BYTES`'s doc comment).
		// The extent finder also returns how many contiguous blocks it found,
		// but `block_count` (the caller's request, already rounded up above)
		// is the only figure this path needs, so only the start is bound.
		let start_lba = alloc_node
			.find_free_extent(block_count)
			.ok_or_else(|| Error::not_found("No free extent available"))?
			.0;

		// Update free block counts
		if allow_reserve {
			// Consume from user first, then reserve
			let from_user = block_count.min(self.user_free_blocks as u32) as u64;
			let from_reserve = block_count as u64 - from_user;
			self.user_free_blocks -= from_user;
			self.reserved_free_blocks -= from_reserve;

			// Drawing the reserve down is not announced anywhere. This crate
			// has no audit sink and emits no diagnostics of its own, so the
			// condition is visible only as `reserved_free_blocks` falling; the
			// audit chain that would carry the warning exists in the format and
			// is not wired in (`KNOWN_ISSUES.md` §11).
		} else {
			self.user_free_blocks -= block_count as u64;
		}

		// Check if we need to enter read-only mode
		self.check_reserve_threshold();

		Ok((start_lba, block_count))
	}

	/// Mark blocks as freed (back to free pool)
	pub fn free(&mut self, block_count: u64) {
		// Add back to user free blocks
		self.user_free_blocks += block_count;
	}

	/// Check reserve threshold and update mode if needed
	fn check_reserve_threshold(&mut self) {
		// `warn_threshold_blocks` is not compared against here: crossing it is
		// a warning with no sink to warn through (`KNOWN_ISSUES.md` §11), so
		// the threshold is a value a caller can check this counter against
		// itself. What this method does act on is the transition below, which
		// is a mode change rather than a message.
		if self.reserved_free_blocks == 0 {
			// Reserve exhausted - enter read-only mode
			self.mode = FsMode::ReadOnlySafe;
		}
	}

	/// Get current filesystem mode
	pub fn mode(&self) -> FsMode {
		self.mode
	}

	/// Get user-visible free space (excluding reserve)
	pub fn user_free_bytes(&self) -> u64 {
		self.user_free_blocks * BLOCK_SIZE_BYTES
	}

	/// Get reserved free space
	pub fn reserved_free_bytes(&self) -> u64 {
		self.reserved_free_blocks * BLOCK_SIZE_BYTES
	}

	/// Get total free space (including reserve)
	pub fn total_free_bytes(&self) -> u64 {
		(self.user_free_blocks + self.reserved_free_blocks) * BLOCK_SIZE_BYTES
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// `AllocationState`/`PhysicalExtent` are only referenced by this module's
	// fixtures, so they are imported here rather than at the parent's scope.
	use crate::alloc_state::{AllocationState, PhysicalExtent};

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_allocator_config_defaults() -> TestResult<()> {
		// -- Setup & Fixtures - 1GB capacity
		let total_blocks = (1024 * 1024 * 1024) / BLOCK_SIZE_BYTES;

		// -- Exec
		let config = AllocatorConfig::new(total_blocks);

		// -- Check - Should use recommended 5%
		let expected_reserve = (1024 * 1024 * 1024 / 20) / BLOCK_SIZE_BYTES; // 5% of 1GB
		assert!(config.reserve_blocks >= expected_reserve);

		Ok(())
	}

	#[test]
	fn test_allocator_config_small_capacity() -> TestResult<()> {
		// -- Setup & Fixtures - 500MB capacity (below 100MB threshold)
		let total_blocks = (500 * 1024 * 1024) / BLOCK_SIZE_BYTES;

		// -- Exec
		let config = AllocatorConfig::new(total_blocks);

		// -- Check - Should use minimum 100MB
		let min_reserve = (100 * 1024 * 1024) / BLOCK_SIZE_BYTES;
		assert_eq!(config.reserve_blocks, min_reserve);

		Ok(())
	}

	#[test]
	fn test_allocator_init_sufficient_space() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config.clone());

		// -- Exec
		allocator.init_from_tree(&alloc_node)?;

		// -- Check
		assert_eq!(allocator.mode(), FsMode::ReadWrite);
		assert!(allocator.user_free_blocks > 0);
		assert_eq!(allocator.reserved_free_blocks, config.reserve_blocks);

		Ok(())
	}

	#[test]
	fn test_allocator_init_low_space() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		// Only 100 blocks free - less than reserve
		alloc_node.add_extent(PhysicalExtent::new(0, 100, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);

		// -- Exec
		allocator.init_from_tree(&alloc_node)?;

		// -- Check - Should enter read-only mode
		assert_eq!(allocator.mode(), FsMode::ReadOnlySafe);
		assert_eq!(allocator.user_free_blocks, 0);

		Ok(())
	}

	#[test]
	fn test_allocator_allocate_normal() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let initial_free = allocator.user_free_blocks;

		// -- Exec - Allocate 10KB (3 blocks of BLOCK_SIZE_BYTES == 4096 bytes:
		// ceil(10240 / 4096) == 3)
		let (lba, blocks) = allocator.allocate(&alloc_node, 10 * 1024, false)?;

		// -- Check
		assert_eq!(lba, 0);
		assert_eq!(blocks, 3);
		assert_eq!(allocator.user_free_blocks, initial_free - 3);

		Ok(())
	}

	#[test]
	fn test_allocator_allocate_alignment() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// -- Exec - Allocate 1000 bytes (should round up to 1 whole
		// BLOCK_SIZE_BYTES block == 4096 bytes)
		let (_lba, blocks) = allocator.allocate(&alloc_node, 1000, false)?;

		// -- Check
		assert_eq!(blocks, 1);

		// A size that spans into a second block must round up to 2.
		let (_lba2, blocks2) = allocator.allocate(&alloc_node, BLOCK_SIZE_BYTES + 1, false)?;
		assert_eq!(blocks2, 2);

		Ok(())
	}

	#[test]
	fn test_allocator_read_only_mode_blocks_writes() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 100, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Should be in read-only mode
		assert_eq!(allocator.mode(), FsMode::ReadOnlySafe);

		// -- Exec
		let result = allocator.allocate(&alloc_node, 1024, false);

		// -- Check - Should fail
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_allocator_emergency_allocation() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 100, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Should be in read-only mode
		assert_eq!(allocator.mode(), FsMode::ReadOnlySafe);

		// -- Exec - Emergency allocation with allow_reserve=true
		let result = allocator.allocate(&alloc_node, 512, true);

		// -- Check - Should succeed using reserve
		assert!(result.is_ok());

		Ok(())
	}

	#[test]
	fn test_allocator_free() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		allocator.allocate(&alloc_node, 10 * 1024, false)?;
		let after_alloc = allocator.user_free_blocks;

		// -- Exec
		allocator.free(20); // Free the 20 blocks we allocated

		// -- Check
		assert_eq!(allocator.user_free_blocks, after_alloc + 20);

		Ok(())
	}
}
