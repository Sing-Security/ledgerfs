//! Allocation tree node for LedgerFS v7
//!
//! Per §14 of FORMAT.md:
//! - Allocation tree is a CoW B-tree tracking block states
//! - Uses BTreeMap for deterministic extent ordering
//! - Content-addressed using BLAKE3
//! - Part of immutable object model

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::alloc_state::{AllocationState, PhysicalExtent};
use crate::error::{Error, Result};
use crate::object::ObjectId;

/// Allocation tree node
///
/// Tracks physical extent allocation states.
/// Uses BTreeMap for deterministic iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocNode {
	/// Schema version for compatibility
	pub schema_version: u16,

	/// Object ID of this node (content-addressed)
	#[serde(skip)]
	pub object_id: ObjectId,

	/// Extents tracked by this node
	/// Key: start LBA
	/// Value: extent descriptor
	/// BTreeMap ensures deterministic serialization
	pub extents: BTreeMap<u64, PhysicalExtent>,

	/// Child node references (if this is an internal node)
	/// Empty for leaf nodes
	pub children: BTreeMap<u64, ObjectId>,

	/// Total free blocks in this subtree
	pub free_blocks: u64,

	/// Total allocated blocks in this subtree
	pub allocated_blocks: u64,

	/// Total reserved blocks in this subtree (emergency reserve)
	pub reserved_blocks: u64,
}

impl AllocNode {
	const CURRENT_SCHEMA_VERSION: u16 = 1;

	/// Create new empty allocation node
	pub fn new() -> Self {
		Self {
			schema_version: Self::CURRENT_SCHEMA_VERSION,
			object_id: ObjectId::zero(),
			extents: BTreeMap::new(),
			children: BTreeMap::new(),
			free_blocks: 0,
			allocated_blocks: 0,
			reserved_blocks: 0,
		}
	}

	/// Create empty leaf node (for testing)
	pub fn new_empty_leaf() -> Self {
		Self::new()
	}

	/// Create leaf node with initial extents
	pub fn new_leaf(extents: BTreeMap<u64, PhysicalExtent>) -> Self {
		let mut node = Self::new();
		node.extents = extents;
		node.update_statistics();
		node
	}

	/// Check if this is a leaf node
	pub fn is_leaf(&self) -> bool {
		self.children.is_empty()
	}

	/// Add extent to this node
	pub fn add_extent(&mut self, extent: PhysicalExtent) -> Result<()> {
		// Check for overlaps
		for existing in self.extents.values() {
			if existing.overlaps(extent.lba, extent.block_count) {
				return Err(Error::invalid_operation("Extent overlaps with existing extent"));
			}
		}

		let lba = extent.lba;
		self.extents.insert(lba, extent);
		self.coalesce_around(lba);
		self.update_statistics();
		Ok(())
	}

	/// Merge the extent at `lba` with its immediate neighbours when they are
	/// adjacent *and* share its state.
	///
	/// Without this the map only ever grows. [`split_and_allocate`] turns one
	/// Free extent into an Allocated extent plus a Free remainder on every
	/// allocation, so N sequential allocations leave N separate Allocated
	/// entries describing what is really one contiguous run — and since the
	/// allocator hands blocks out in order, that is the normal case, not a
	/// pathological one.
	///
	/// It matters because the encoded node has to fit in a single 4 KiB
	/// snapshot block: `persist_alloc_snapshot` deliberately zeroes the
	/// snapshot when it does not, and a volume with no valid snapshot resumes
	/// **read-only for the rest of its life**. Measured before this existed,
	/// the node grew by a fixed ~1098 bytes per commit batch against a 4076
	/// byte budget, so the data volume became permanently read-only after about a
	/// dozen write transactions. Coalescing keeps a sequential allocation
	/// pattern at a handful of extents no matter how many commits it takes.
	///
	/// [`split_and_allocate`]: Self::split_and_allocate
	fn coalesce_around(&mut self, lba: u64) {
		// Merge forward first: absorbing the successor leaves `lba` still
		// valid as a key, whereas merging backward first would move it.
		loop {
			let Some(cur) = self.extents.get(&lba).copied() else {
				return;
			};
			let Some((&next_lba, &next)) = self.extents.range(lba + 1..).next() else {
				break;
			};
			if cur.lba + cur.block_count as u64 != next_lba || cur.state != next.state {
				break;
			}
			// block_count is a u32; refuse the merge rather than wrap. At 4 KiB
			// blocks this is 16 TiB, far past any partition here, but a wrapped
			// count would silently describe the wrong range.
			let Some(total) = cur.block_count.checked_add(next.block_count) else {
				break;
			};
			self.extents.remove(&next_lba);
			if let Some(e) = self.extents.get_mut(&lba) {
				e.block_count = total;
			}
		}

		// Then merge backward, repeatedly, since absorbing one predecessor can
		// make the next one adjacent too.
		let mut cur_lba = lba;
		loop {
			let Some(cur) = self.extents.get(&cur_lba).copied() else {
				return;
			};
			let Some((&prev_lba, &prev)) = self.extents.range(..cur_lba).next_back() else {
				return;
			};
			if prev.lba + prev.block_count as u64 != cur_lba || prev.state != cur.state {
				return;
			}
			let Some(total) = prev.block_count.checked_add(cur.block_count) else {
				return;
			};
			self.extents.remove(&cur_lba);
			if let Some(e) = self.extents.get_mut(&prev_lba) {
				e.block_count = total;
			}
			cur_lba = prev_lba;
		}
	}

	/// Update extent state
	pub fn update_extent_state(&mut self, lba: u64, new_state: AllocationState) -> Result<()> {
		let extent = self.extents.get_mut(&lba).ok_or_else(|| Error::not_found("Extent not found"))?;

		extent.state = new_state;
		self.update_statistics();
		Ok(())
	}

	/// Mark exactly `block_count` blocks starting at `lba` as Allocated, leaving
	/// the remainder of the original extent as Free.  This is the correct way to
	/// record a partial allocation inside an extent.
	pub fn split_and_allocate(&mut self, lba: u64, block_count: u32) -> Result<()> {
		// Remove the original extent (must exist and be Free)
		let orig = self
			.extents
			.remove(&lba)
			.ok_or_else(|| Error::not_found("Extent not found for split_and_allocate"))?;

		// Insert the allocated portion.
		self.extents
			.insert(lba, PhysicalExtent::new(lba, block_count, AllocationState::Allocated));

		// If there are leftover blocks, re-insert them as Free.
		let remainder = orig.block_count as u64 - block_count as u64;
		if remainder > 0 {
			let next_lba = lba + block_count as u64;
			self.extents.insert(
				next_lba,
				PhysicalExtent::new(next_lba, remainder as u32, AllocationState::Free),
			);
			// The remainder may abut a Free extent beyond it.
			self.coalesce_around(next_lba);
		}

		// The freshly allocated run is normally adjacent to the previous
		// allocation — this is the merge that keeps the node bounded.
		self.coalesce_around(lba);

		self.update_statistics();
		Ok(())
	}

	/// Find extent containing LBA
	pub fn find_extent(&self, lba: u64) -> Option<&PhysicalExtent> {
		// Use range to efficiently find extent that might contain LBA
		self.extents
			.range(..=lba)
			.rev()
			.map(|(_, extent)| extent)
			.find(|extent| extent.contains(lba))
	}

	/// Find free extent with at least the requested block count
	/// Returns (start_lba, available_blocks)
	pub fn find_free_extent(&self, min_blocks: u32) -> Option<(u64, u32)> {
		for extent in self.extents.values() {
			if extent.state == AllocationState::Free && extent.block_count >= min_blocks {
				return Some((extent.lba, extent.block_count));
			}
		}
		None
	}

	/// Update statistics based on current extents
	fn update_statistics(&mut self) {
		self.free_blocks = 0;
		self.allocated_blocks = 0;
		self.reserved_blocks = 0;

		for extent in self.extents.values() {
			let blocks = extent.block_count as u64;
			match extent.state {
				AllocationState::Free => self.free_blocks += blocks,
				AllocationState::Allocated => self.allocated_blocks += blocks,
				AllocationState::Reserved => self.reserved_blocks += blocks,
				AllocationState::PendingFree => {} // Not counted in totals
				AllocationState::Pinned => {}      // Pinned blocks not counted in free/allocated totals
			}
		}
	}

	/// Recompute stats - public alias for update_statistics
	///
	/// This method recalculates free_blocks, allocated_blocks, and reserved_blocks
	/// based on the current extents. Should be called after modifying extent states.
	pub fn recompute_stats(&mut self) {
		self.update_statistics();
	}

	/// Add child node reference
	pub fn add_child(&mut self, key: u64, child_id: ObjectId) {
		self.children.insert(key, child_id);
	}

	/// Encode to canonical bytes using postcard
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode from canonical bytes
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		postcard::from_bytes(bytes).map_err(Error::from)
	}

	/// Update object ID based on canonical content
	pub fn update_object_id(&mut self) -> Result<()> {
		let canonical = self.encode_canonical()?;
		self.object_id = ObjectId::from_content(&canonical);
		Ok(())
	}

	/// Serialize this node into a whole 4 KiB **allocator snapshot block**
	/// (see [`crate::ALLOC_SNAPSHOT_BLOCK`]) for writing to a fixed on-disk
	/// location.
	///
	/// Block layout:
	/// - Bytes  0–7:  [`crate::ALLOC_SNAPSHOT_MAGIC`]
	/// - Bytes  8–11: payload length (u32 LE)
	/// - Bytes 12–19: BLAKE3-64 checksum of the payload
	/// - Bytes 20–..: postcard payload, zero-padded to the end of the block
	///
	/// The checksum is what makes a torn or half-written block detectable:
	/// resuming a volume with a *wrong* allocator state is worse than not
	/// resuming it at all, because the allocator would hand out blocks that
	/// already hold live objects. A snapshot that fails any check must be
	/// treated as absent.
	///
	/// # Errors
	///
	/// [`Error::Custom`] if the encoded node does not fit in one block. That
	/// is a real bound (a heavily fragmented volume has many extents), and
	/// the caller must respond by *invalidating* the on-disk snapshot rather
	/// than leaving a stale one behind — see
	/// `MountedVolume::persist_alloc_snapshot`.
	pub fn encode_snapshot_block(&self, block_size: usize) -> Result<Vec<u8>> {
		const HEADER: usize = 20;
		let payload = self.encode_canonical()?;
		if HEADER + payload.len() > block_size {
			return Err(Error::custom(
				"AllocNode: snapshot does not fit in one block (too many extents)",
			));
		}

		let mut buf = alloc::vec![0u8; block_size];
		buf[0..8].copy_from_slice(&crate::ALLOC_SNAPSHOT_MAGIC.to_le_bytes());
		buf[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
		buf[12..20].copy_from_slice(&crate::encoding::blake3_checksum(&payload).to_le_bytes());
		buf[HEADER..HEADER + payload.len()].copy_from_slice(&payload);
		Ok(buf)
	}

	/// Parse a block written by [`encode_snapshot_block`](Self::encode_snapshot_block).
	///
	/// Returns `Ok(None)` — not an error — when the block holds no valid
	/// snapshot (never written, wrong magic, implausible length, checksum
	/// mismatch, or an undecodable payload). "No snapshot" is an expected,
	/// recoverable state: every volume formatted before this existed reads
	/// zeros here. The caller resumes read-only, which is what it did
	/// unconditionally before.
	pub fn decode_snapshot_block(buf: &[u8]) -> Result<Option<Self>> {
		const HEADER: usize = 20;
		if buf.len() < HEADER {
			return Ok(None);
		}
		let magic = u64::from_le_bytes(
			buf[0..8]
				.try_into()
				.map_err(|_| Error::custom("AllocNode: short snapshot block"))?,
		);
		if magic != crate::ALLOC_SNAPSHOT_MAGIC {
			return Ok(None);
		}
		let len = u32::from_le_bytes(
			buf[8..12]
				.try_into()
				.map_err(|_| Error::custom("AllocNode: short snapshot block"))?,
		) as usize;
		if len == 0 || HEADER + len > buf.len() {
			return Ok(None);
		}
		let stored_checksum = u64::from_le_bytes(
			buf[12..20]
				.try_into()
				.map_err(|_| Error::custom("AllocNode: short snapshot block"))?,
		);
		let payload = &buf[HEADER..HEADER + len];
		if crate::encoding::blake3_checksum(payload) != stored_checksum {
			return Ok(None);
		}
		Ok(Self::decode_canonical(payload).ok())
	}
}

impl Default for AllocNode {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	/// A sequential allocation pattern — what the allocator actually does —
	/// must not grow the node without bound. Before coalescing existed this
	/// added one extent per allocation, and the encoded node outgrew its
	/// single 4 KiB snapshot block after roughly a dozen commits, which left
	/// the data volume permanently read-only.
	#[test]
	fn sequential_allocations_stay_bounded() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		node.add_extent(PhysicalExtent::new(0, 6, AllocationState::Reserved))?;
		node.add_extent(PhysicalExtent::new(6, 10_000, AllocationState::Free))?;

		// -- Exec: 200 sequential allocations, as a long-lived volume performs.
		let mut lba = 6u64;
		for _ in 0..200 {
			node.split_and_allocate(lba, 4)?;
			lba += 4;
		}

		// -- Check: Reserved header, one merged Allocated run, one Free tail.
		assert_eq!(node.extents.len(), 3, "adjacent same-state extents must merge");
		let allocated = node.extents.get(&6).ok_or("allocated run missing")?;
		assert_eq!(allocated.state, AllocationState::Allocated);
		assert_eq!(allocated.block_count, 800, "the whole run should be one extent");

		// And the encoded form must still fit the snapshot block it has to
		// live in — the property that actually broke.
		let encoded = node.encode_snapshot_block(4096)?;
		assert_eq!(encoded.len(), 4096);
		Ok(())
	}

	/// Extents that are adjacent but differ in state must stay separate —
	/// merging them would report allocated blocks as free.
	#[test]
	fn adjacent_extents_of_different_states_do_not_merge() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		node.add_extent(PhysicalExtent::new(0, 10, AllocationState::Free))?;
		node.add_extent(PhysicalExtent::new(10, 10, AllocationState::Allocated))?;
		node.add_extent(PhysicalExtent::new(20, 10, AllocationState::Free))?;

		// -- Check
		assert_eq!(node.extents.len(), 3);
		assert_eq!(node.extents.get(&10).ok_or("missing")?.block_count, 10);
		Ok(())
	}

	/// Build a node shaped like a real freshly-formatted volume: reserved
	/// header, one big free span, reserved trailer.
	fn fixture_fresh_volume_node(total_blocks: u64) -> TestResult<AllocNode> {
		let free_start = crate::RESERVED_HEADER_BLOCKS;
		let free_end = total_blocks - crate::RESERVED_TRAILER_BLOCKS;
		let mut node = AllocNode::new();
		node.add_extent(PhysicalExtent::new(0, free_start as u32, AllocationState::Reserved))?;
		node.add_extent(PhysicalExtent::new(
			free_start,
			(free_end - free_start) as u32,
			AllocationState::Free,
		))?;
		node.add_extent(PhysicalExtent::new(
			free_end,
			(total_blocks - free_end) as u32,
			AllocationState::Reserved,
		))?;
		Ok(node)
	}

	#[test]
	fn test_alloc_snapshot_block_round_trips_a_realistic_volume() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = fixture_fresh_volume_node(131_072)?;
		node.split_and_allocate(crate::RESERVED_HEADER_BLOCKS, 12)?;
		let block_size = crate::BLOCK_SIZE as usize;

		// -- Exec
		let block = node.encode_snapshot_block(block_size)?;

		// -- Check
		assert_eq!(
			block.len(),
			block_size,
			"a snapshot must occupy exactly one whole block"
		);
		let restored = AllocNode::decode_snapshot_block(&block)?.expect("a freshly written snapshot must decode");
		assert_eq!(restored.extents, node.extents);
		assert_eq!(restored.free_blocks, node.free_blocks);
		assert_eq!(restored.allocated_blocks, node.allocated_blocks);

		Ok(())
	}

	/// Every one of these must read as "no snapshot", never as a decoded
	/// node: resuming a volume from a *wrong* allocator state is worse than
	/// not resuming it, because the allocator would hand out blocks that
	/// already hold live objects.
	#[test]
	fn test_alloc_snapshot_block_rejects_absent_torn_and_corrupt_blocks() -> TestResult<()> {
		// -- Setup & Fixtures
		let block_size = crate::BLOCK_SIZE as usize;
		let node = fixture_fresh_volume_node(131_072)?;
		let good = node.encode_snapshot_block(block_size)?;

		// -- Exec & Check: never written (all zeros) — the state of every
		// volume formatted before snapshots existed.
		assert!(AllocNode::decode_snapshot_block(&alloc::vec![0u8; block_size])?.is_none());

		// Wrong magic (block reused for something else).
		let mut wrong_magic = good.clone();
		wrong_magic[0] ^= 0xFF;
		assert!(AllocNode::decode_snapshot_block(&wrong_magic)?.is_none());

		// Payload corrupted after the header — a torn write. The length and
		// magic still look right, so only the checksum can catch this.
		let mut corrupt = good.clone();
		corrupt[25] ^= 0xFF;
		assert!(AllocNode::decode_snapshot_block(&corrupt)?.is_none());

		// Length field claiming more than the block holds.
		let mut bad_len = good.clone();
		bad_len[8..12].copy_from_slice(&(block_size as u32 + 1).to_le_bytes());
		assert!(AllocNode::decode_snapshot_block(&bad_len)?.is_none());

		// Zero length.
		let mut zero_len = good.clone();
		zero_len[8..12].copy_from_slice(&0u32.to_le_bytes());
		assert!(AllocNode::decode_snapshot_block(&zero_len)?.is_none());

		// Truncated block (shorter than the header).
		assert!(AllocNode::decode_snapshot_block(&good[..8])?.is_none());

		Ok(())
	}

	#[test]
	fn test_alloc_snapshot_block_errors_rather_than_truncating_when_too_fragmented() -> TestResult<()> {
		// -- Setup & Fixtures: enough alternating extents to overflow one
		// block. The caller must see an error here so it can invalidate the
		// on-disk snapshot; silently writing a truncated one would leave a
		// decodable-but-wrong allocator state.
		let mut node = AllocNode::new();
		for i in 0..4096u64 {
			let state = if i % 2 == 0 {
				AllocationState::Allocated
			} else {
				AllocationState::Free
			};
			node.add_extent(PhysicalExtent::new(i * 2, 1, state))?;
		}

		// -- Exec
		let result = node.encode_snapshot_block(crate::BLOCK_SIZE as usize);

		// -- Check
		assert!(
			result.is_err(),
			"an oversized snapshot must be an error, not a truncated block"
		);

		Ok(())
	}

	#[test]
	fn test_allocnode_new() -> TestResult<()> {
		// -- Exec
		let node = AllocNode::new();

		// -- Check
		assert_eq!(node.schema_version, 1);
		assert!(node.extents.is_empty());
		assert!(node.children.is_empty());
		assert!(node.is_leaf());
		assert_eq!(node.free_blocks, 0);

		Ok(())
	}

	#[test]
	fn test_allocnode_add_extent() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		let extent = PhysicalExtent::new(1000, 100, AllocationState::Free);

		// -- Exec
		node.add_extent(extent)?;

		// -- Check
		assert_eq!(node.extents.len(), 1);
		assert_eq!(node.free_blocks, 100);

		Ok(())
	}

	#[test]
	fn test_allocnode_overlap_rejected() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		let extent1 = PhysicalExtent::new(1000, 100, AllocationState::Free);
		let extent2 = PhysicalExtent::new(1050, 100, AllocationState::Free); // Overlaps

		node.add_extent(extent1)?;

		// -- Exec
		let result = node.add_extent(extent2);

		// -- Check
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_allocnode_update_extent_state() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		let extent = PhysicalExtent::new(1000, 100, AllocationState::Free);
		node.add_extent(extent)?;

		// -- Exec
		node.update_extent_state(1000, AllocationState::Allocated)?;

		// -- Check
		assert_eq!(node.free_blocks, 0);
		assert_eq!(node.allocated_blocks, 100);

		Ok(())
	}

	#[test]
	fn test_allocnode_find_extent() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		let extent = PhysicalExtent::new(1000, 100, AllocationState::Free);
		node.add_extent(extent)?;

		// -- Check
		assert!(node.find_extent(1050).is_some());
		assert!(node.find_extent(999).is_none());
		assert!(node.find_extent(1100).is_none());

		Ok(())
	}

	#[test]
	fn test_allocnode_find_free_extent() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		node.add_extent(PhysicalExtent::new(1000, 50, AllocationState::Allocated))?;
		node.add_extent(PhysicalExtent::new(2000, 100, AllocationState::Free))?;
		node.add_extent(PhysicalExtent::new(3000, 200, AllocationState::Free))?;

		// -- Check
		let result = node.find_free_extent(75);
		assert!(result.is_some());
		let (_, blocks) = result.unwrap();
		assert!(blocks >= 75);

		Ok(())
	}

	#[test]
	fn test_allocnode_encode_decode() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Free))?;
		node.add_extent(PhysicalExtent::new(2000, 50, AllocationState::Allocated))?;

		// -- Exec
		let encoded = node.encode_canonical()?;
		let decoded = AllocNode::decode_canonical(&encoded)?;

		// -- Check
		assert_eq!(decoded.extents.len(), node.extents.len());
		assert_eq!(decoded.free_blocks, node.free_blocks);
		assert_eq!(decoded.allocated_blocks, node.allocated_blocks);

		Ok(())
	}

	#[test]
	fn test_allocnode_deterministic_serialization() -> TestResult<()> {
		// -- Setup & Fixtures - Add extents in different order
		let mut node1 = AllocNode::new();
		node1.add_extent(PhysicalExtent::new(2000, 100, AllocationState::Free))?;
		node1.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Free))?;

		let mut node2 = AllocNode::new();
		node2.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Free))?;
		node2.add_extent(PhysicalExtent::new(2000, 100, AllocationState::Free))?;

		// -- Exec
		let encoded1 = node1.encode_canonical()?;
		let encoded2 = node2.encode_canonical()?;

		// -- Check - BTreeMap ensures same serialization regardless of insertion order
		assert_eq!(encoded1, encoded2);

		Ok(())
	}

	#[test]
	fn test_allocnode_statistics() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = AllocNode::new();
		node.add_extent(PhysicalExtent::new(1000, 100, AllocationState::Free))?;
		node.add_extent(PhysicalExtent::new(2000, 50, AllocationState::Allocated))?;
		node.add_extent(PhysicalExtent::new(3000, 25, AllocationState::Reserved))?;
		node.add_extent(PhysicalExtent::new(4000, 10, AllocationState::PendingFree))?;

		// -- Check
		assert_eq!(node.free_blocks, 100);
		assert_eq!(node.allocated_blocks, 50);
		assert_eq!(node.reserved_blocks, 25);

		Ok(())
	}
}
