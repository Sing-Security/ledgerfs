//! Allocation state tracking for LedgerFS v7
//!
//! Per §14 and §23 of FORMAT.md:
//! - Tracks allocation state of physical blocks
//! - State machine: Free → Allocated → PendingFree/Pinned → Free
//! - Reserved blocks for emergency operations
//! - Pinned blocks protected by snapshots (§24)
//! - Deterministic extent tracking with BTreeMap

use serde::{Deserialize, Serialize};

/// Allocation state for an extent (§23)
///
/// State transitions per spec §23:
/// 1. FREE → ALLOCATED: New block allocated for write
/// 2. ALLOCATED → PENDINGFREE: Object no longer in current root
/// 3. PENDINGFREE → FREE: GC completes, no snapshots hold reference
/// 4. ALLOCATED → PINNED: Snapshot created (§24.1)
/// 5. PINNED → PENDINGFREE: Snapshot deleted, object not in current root
/// 6. PINNED → FREE: Snapshot deleted, special fast path
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum AllocationState {
	/// Available for allocation
	Free = 0x00,

	/// In use by current filesystem state
	Allocated = 0x01,

	/// Pending free (in deadlist, waiting for GC)
	/// Previously called DEAD in spec §14.1
	PendingFree = 0x02,

	/// Protected from allocation (emergency reserve)
	Reserved = 0x03,

	/// Protected by snapshot (§24, §22.3)
	/// Cannot be freed even if in deadlist until all snapshots deleted
	Pinned = 0x04,
}

impl AllocationState {
	/// Convert from u8
	pub fn from_u8(val: u8) -> Option<Self> {
		match val {
			0x00 => Some(Self::Free),
			0x01 => Some(Self::Allocated),
			0x02 => Some(Self::PendingFree),
			0x03 => Some(Self::Reserved),
			0x04 => Some(Self::Pinned),
			_ => None,
		}
	}

	/// Convert to u8
	pub fn to_u8(self) -> u8 {
		self as u8
	}

	/// Check if state allows allocation
	pub fn is_allocatable(&self) -> bool {
		matches!(self, Self::Free)
	}

	/// Check if state is reserved
	pub fn is_reserved(&self) -> bool {
		matches!(self, Self::Reserved)
	}

	/// Check if state is pinned by snapshot (§22.3)
	pub fn is_pinned(&self) -> bool {
		matches!(self, Self::Pinned)
	}

	/// Check if block can be freed (not pinned or reserved)
	pub fn can_free(&self) -> bool {
		!matches!(self, Self::Pinned | Self::Reserved)
	}
}

/// Physical extent descriptor
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PhysicalExtent {
	/// Starting LBA (Logical Block Address)
	pub lba: u64,

	/// Length in 512-byte blocks
	pub block_count: u32,

	/// Allocation state
	pub state: AllocationState,
}

impl PhysicalExtent {
	/// Create new extent
	pub fn new(lba: u64, block_count: u32, state: AllocationState) -> Self {
		Self {
			lba,
			block_count,
			state,
		}
	}

	/// Get ending LBA (exclusive)
	pub fn end_lba(&self) -> u64 {
		self.lba + self.block_count as u64
	}

	/// Check if extent contains LBA
	pub fn contains(&self, lba: u64) -> bool {
		lba >= self.lba && lba < self.end_lba()
	}

	/// Check if extent overlaps with range
	pub fn overlaps(&self, start_lba: u64, block_count: u32) -> bool {
		let end = start_lba + block_count as u64;
		!(end <= self.lba || start_lba >= self.end_lba())
	}

	/// Size in bytes (assuming 512-byte blocks)
	pub fn size_bytes(&self) -> u64 {
		self.block_count as u64 * 512
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_alloc_state_from_u8() -> TestResult<()> {
		// -- Check
		assert_eq!(AllocationState::from_u8(0x00), Some(AllocationState::Free));
		assert_eq!(AllocationState::from_u8(0x01), Some(AllocationState::Allocated));
		assert_eq!(AllocationState::from_u8(0x02), Some(AllocationState::PendingFree));
		assert_eq!(AllocationState::from_u8(0x03), Some(AllocationState::Reserved));
		assert_eq!(AllocationState::from_u8(0x04), Some(AllocationState::Pinned));
		assert_eq!(AllocationState::from_u8(0xFF), None);

		Ok(())
	}

	#[test]
	fn test_alloc_state_is_allocatable() -> TestResult<()> {
		// -- Check
		assert!(AllocationState::Free.is_allocatable());
		assert!(!AllocationState::Allocated.is_allocatable());
		assert!(!AllocationState::PendingFree.is_allocatable());
		assert!(!AllocationState::Reserved.is_allocatable());
		assert!(!AllocationState::Pinned.is_allocatable());

		Ok(())
	}

	#[test]
	fn test_alloc_state_is_pinned() -> TestResult<()> {
		// -- Check
		assert!(!AllocationState::Free.is_pinned());
		assert!(!AllocationState::Allocated.is_pinned());
		assert!(!AllocationState::PendingFree.is_pinned());
		assert!(!AllocationState::Reserved.is_pinned());
		assert!(AllocationState::Pinned.is_pinned());

		Ok(())
	}

	#[test]
	fn test_alloc_state_can_free() -> TestResult<()> {
		// -- Check - Pinned and Reserved cannot be freed
		assert!(AllocationState::Free.can_free());
		assert!(AllocationState::Allocated.can_free());
		assert!(AllocationState::PendingFree.can_free());
		assert!(!AllocationState::Reserved.can_free());
		assert!(!AllocationState::Pinned.can_free());

		Ok(())
	}

	#[test]
	fn test_physical_extent_basic() -> TestResult<()> {
		// -- Setup & Fixtures
		let extent = PhysicalExtent::new(1000, 100, AllocationState::Free);

		// -- Check
		assert_eq!(extent.lba, 1000);
		assert_eq!(extent.block_count, 100);
		assert_eq!(extent.end_lba(), 1100);
		assert_eq!(extent.size_bytes(), 51200); // 100 * 512

		Ok(())
	}

	#[test]
	fn test_physical_extent_contains() -> TestResult<()> {
		// -- Setup & Fixtures
		let extent = PhysicalExtent::new(1000, 100, AllocationState::Free);

		// -- Check
		assert!(extent.contains(1000));
		assert!(extent.contains(1050));
		assert!(extent.contains(1099));
		assert!(!extent.contains(999));
		assert!(!extent.contains(1100));

		Ok(())
	}

	#[test]
	fn test_physical_extent_overlaps() -> TestResult<()> {
		// -- Setup & Fixtures
		let extent = PhysicalExtent::new(1000, 100, AllocationState::Free);

		// -- Check
		assert!(extent.overlaps(1000, 100)); // Exact match
		assert!(extent.overlaps(1050, 10)); // Inside
		assert!(extent.overlaps(900, 150)); // Overlaps start
		assert!(extent.overlaps(1050, 100)); // Overlaps end
		assert!(!extent.overlaps(900, 100)); // Before
		assert!(!extent.overlaps(1100, 100)); // After

		Ok(())
	}

	#[test]
	fn test_physical_extent_ordering() -> TestResult<()> {
		// -- Setup & Fixtures
		let e1 = PhysicalExtent::new(1000, 100, AllocationState::Free);
		let e2 = PhysicalExtent::new(2000, 100, AllocationState::Free);
		let e3 = PhysicalExtent::new(1000, 100, AllocationState::Allocated);

		// -- Check - Ordering by LBA, then state
		assert!(e1 < e2); // Different LBA
		assert!(e1 < e3); // Same LBA, different state

		Ok(())
	}
}
