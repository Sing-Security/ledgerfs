//! Placement engine for LedgerFS v7
//!
//! Turns TxnPlan into placed ObjectRecords with physical locations.
//! Integrates with allocator for space management and generates StorageHints.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::alloc_node::AllocNode;
use crate::allocator::{Allocator, BLOCK_SIZE_BYTES};
use crate::cow::TxnPlan;
use crate::error::{Error, Result};
use crate::object::ObjectId;
use crate::record::ObjectRecord;

/// Physical storage hint for an object
///
/// Per §20 of ledgerfs-spec-v7.md:
/// - Advisory location for data placement
/// - Used by allocator to reduce fragmentation
/// - Must not be trusted for correctness
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StorageHint {
	/// Object ID this hint is for
	pub object_id: ObjectId,

	/// Suggested physical LBA
	pub physical_lba: u64,

	/// Size in blocks
	pub block_count: u32,
}

/// Placed object with physical location
#[derive(Debug, Clone)]
pub struct PlacedObject {
	/// Object ID (content-addressed)
	pub object_id: ObjectId,

	/// Canonical bytes of the object
	pub canonical_bytes: Vec<u8>,

	/// Physical LBA where this will be written
	pub physical_lba: u64,

	/// Number of blocks allocated
	pub block_count: u32,
}

/// Result of placement operation
#[derive(Debug, Clone)]
pub struct PlacementResult {
	/// Objects in write order (respecting dependencies)
	pub placed_objects: Vec<PlacedObject>,

	/// Storage hints for all placed objects
	/// Uses BTreeMap for deterministic iteration
	pub storage_hints: BTreeMap<ObjectId, StorageHint>,

	/// Updated allocation node (with new allocations marked)
	pub updated_alloc_node: AllocNode,
}

/// Placement engine
pub struct PlacementEngine;

impl PlacementEngine {
	/// Place objects from transaction plan
	///
	/// # Arguments
	/// * `txn_plan` - Transaction plan with objects to place
	/// * `alloc_node` - Current allocation tree node
	/// * `allocator` - Allocator for finding free space
	///
	/// # Returns
	/// PlacementResult with placed objects and hints
	pub fn place_transaction(
		txn_plan: &TxnPlan,
		mut alloc_node: AllocNode,
		allocator: &mut Allocator,
	) -> Result<PlacementResult> {
		let mut placed_objects = Vec::new();
		let mut storage_hints = BTreeMap::new();

		// Get objects in dependency order
		let ordered_objects = txn_plan.objects_in_dependency_order()?;

		// Place each object
		for new_obj in ordered_objects {
			// Allocate space for the *record*, not just the payload.
			//
			// `commit()` writes `record.encode(&mut buf)` into a buffer of
			// `block_count * BLOCK_SIZE`, and an `ObjectRecord` is a 60-byte
			// header followed by the data. Sizing the allocation from
			// `canonical_bytes.len()` alone therefore under-allocates by the
			// header whenever the payload lands within 60 bytes of a block
			// boundary, and the commit dies with `BufferTooSmall`.
			//
			// That is what capped a `/data` directory at ~103 entries: the
			// directory node's canonical form reached 4,059 bytes, the
			// allocator rounded that to one 4,096-byte block, and the writer
			// needed 4,119. It surfaced as an opaque `Os { code: 4 }` at the
			// caller and was read as a "400-row table ceiling" — the real
			// ceiling was wherever the payload happened to cross a block.
			//
			// Compression can only shrink the stored form (`ObjectRecordWriter`
			// falls back to uncompressed when compression does not help), so
			// header + payload is a safe upper bound.
			let size_bytes =
				(new_obj.canonical_bytes.len() + crate::record::ObjectRecordHeader::SIZE) as u64;
			let (physical_lba, block_count) = allocator.allocate(&alloc_node, size_bytes, false)?;

			// Update allocation tree - mark exactly block_count blocks as allocated,
			// leaving the remainder of the extent free.
			alloc_node.split_and_allocate(physical_lba, block_count)?;

			// Create placed object
			let placed = PlacedObject {
				object_id: new_obj.object_id,
				canonical_bytes: new_obj.canonical_bytes.clone(),
				physical_lba,
				block_count,
			};

			// Create storage hint
			let hint = StorageHint {
				object_id: new_obj.object_id,
				physical_lba,
				block_count,
			};

			placed_objects.push(placed);
			storage_hints.insert(new_obj.object_id, hint);
		}

		Ok(PlacementResult {
			placed_objects,
			storage_hints,
			updated_alloc_node: alloc_node,
		})
	}

	/// Write placed objects to storage (conceptual - actual I/O would be external)
	///
	/// Returns list of ObjectRecords to write.
	pub fn prepare_object_records(placed_objects: &[PlacedObject]) -> Result<Vec<ObjectRecord>> {
		let mut records = Vec::new();

		for placed in placed_objects {
			// Create ObjectRecord wrapper
			// LZ4 as the write policy.
			//
			// This was hardcoded to 0 (no compression), which meant the whole
			// compression subsystem was unreachable from the write path even
			// where it worked. It is safe to turn on now that
			// `crate::lz4_block` gives a `no_std` encoder: before it, LZ4
			// existed only behind the `std` feature, so selecting it here would
			// have made every write fail on target.
			//
			// `compress` returns "not worth it" for data that does not shrink
			// and the record is then stored uncompressed with
			// `CompressionAlgorithm::None` in its header, so incompressible
			// objects cost only the attempt. The allocation sized in
			// `place_transaction` is from the *uncompressed* length plus the
			// header, which stays a safe upper bound when the stored form is
			// smaller.
			let record = ObjectRecord::from_object_id(
				placed.object_id,
				&placed.canonical_bytes,
				crate::record::ObjectType::Data, // TODO: Determine actual type
				crate::compression::CompressionAlgorithm::Lz4 as u8,
				0, // No encryption
			)?;

			records.push(record);
		}

		Ok(records)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::alloc_state::{AllocationState, PhysicalExtent};
	use crate::allocator::AllocatorConfig;
	use crate::cow::NewObject;
	use crate::object::{Chunk, ObjectRef, Root};

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_placement_simple() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Create simple transaction plan
		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk = Chunk::new(alloc::vec![1, 2, 3, 4, 5]);
		let canonical = chunk.encode_canonical()?;
		let new_obj = NewObject::new(chunk.object_id, canonical);
		txn_plan.add_object(new_obj);

		// -- Exec
		let result = PlacementEngine::place_transaction(&txn_plan, alloc_node, &mut allocator)?;

		// -- Check
		assert_eq!(result.placed_objects.len(), 1);
		assert_eq!(result.storage_hints.len(), 1);
		assert!(result.storage_hints.contains_key(&chunk.object_id));

		Ok(())
	}

	#[test]
	fn test_placement_allocates_room_for_the_record_header() -> TestResult<()> {
		// An object whose canonical payload lands just under a block boundary
		// still has a 60-byte `ObjectRecordHeader` written in front of it by
		// `commit()`. Sizing the allocation from the payload alone
		// under-allocates by exactly that, and the commit dies with
		// `BufferTooSmall` — which is what capped a `/data` directory at ~103
		// entries (payload 4,059, one 4,096-byte block allocated, 4,119
		// needed).
		//
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);
		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;
		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Build a chunk whose *canonical* encoding is just under one block.
		let block = crate::BLOCK_SIZE as usize;
		let header = crate::record::ObjectRecordHeader::SIZE;
		let mut payload_len = block - 1;
		let chunk = loop {
			let c = Chunk::new(alloc::vec![0xA5u8; payload_len]);
			let n = c.encode_canonical()?.len();
			if n < block && n > block - header {
				break c; // within a header's width of the boundary
			}
			if n >= block {
				payload_len -= 1;
			} else {
				payload_len += 1;
			}
			assert!(payload_len > 0 && payload_len < block, "could not land near the boundary");
		};
		let canonical = chunk.encode_canonical()?;
		let canonical_len = canonical.len();
		assert!(canonical_len < block, "payload must fit a block on its own");
		assert!(canonical_len + header > block, "payload + header must not");

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		txn_plan.add_object(NewObject::new(chunk.object_id, canonical));

		// -- Exec
		let result = PlacementEngine::place_transaction(&txn_plan, alloc_node, &mut allocator)?;

		// -- Check: the buffer `commit()` will build must hold the whole record.
		let placed = &result.placed_objects[0];
		let buf_len = placed.block_count as usize * block;
		assert!(
			buf_len >= canonical_len + header,
			"allocated {buf_len} bytes for a record needing {} ({canonical_len} payload + {header} header)",
			canonical_len + header
		);

		Ok(())
	}

	#[test]
	fn test_placement_respects_dependencies() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Create transaction with dependencies
		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		let chunk1 = Chunk::new(alloc::vec![1, 2, 3]);
		let chunk2 = Chunk::new(alloc::vec![4, 5, 6]);

		let obj1 = NewObject::new(chunk1.object_id, chunk1.encode_canonical()?);
		let obj2 = NewObject::new(chunk2.object_id, chunk2.encode_canonical()?).with_dependency(chunk1.object_id);

		// Add in reverse order
		txn_plan.add_object(obj2);
		txn_plan.add_object(obj1);

		// -- Exec
		let result = PlacementEngine::place_transaction(&txn_plan, alloc_node, &mut allocator)?;

		// -- Check - Should be placed in dependency order
		assert_eq!(result.placed_objects.len(), 2);
		assert_eq!(result.placed_objects[0].object_id, chunk1.object_id);
		assert_eq!(result.placed_objects[1].object_id, chunk2.object_id);

		Ok(())
	}

	#[test]
	fn test_placement_updates_alloc_tree() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;
		let initial_free = alloc_node.free_blocks;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk = Chunk::new(alloc::vec![1, 2, 3, 4, 5]);
		let canonical = chunk.encode_canonical()?;
		let new_obj = NewObject::new(chunk.object_id, canonical);
		txn_plan.add_object(new_obj);

		// -- Exec
		let result = PlacementEngine::place_transaction(&txn_plan, alloc_node, &mut allocator)?;

		// -- Check - Allocation tree should show allocated blocks
		assert!(result.updated_alloc_node.allocated_blocks > 0);
		assert_eq!(
			result.updated_alloc_node.free_blocks + result.updated_alloc_node.allocated_blocks,
			initial_free
		);

		Ok(())
	}

	#[test]
	fn test_storage_hints_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Create transaction with multiple objects
		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		let chunk1 = Chunk::new(alloc::vec![1]);
		let chunk2 = Chunk::new(alloc::vec![2]);
		let chunk3 = Chunk::new(alloc::vec![3]);

		txn_plan.add_object(NewObject::new(chunk2.object_id, chunk2.encode_canonical()?));
		txn_plan.add_object(NewObject::new(chunk1.object_id, chunk1.encode_canonical()?));
		txn_plan.add_object(NewObject::new(chunk3.object_id, chunk3.encode_canonical()?));

		// -- Exec
		let result = PlacementEngine::place_transaction(&txn_plan, alloc_node, &mut allocator)?;

		// -- Check - Storage hints should be in BTreeMap (deterministic order)
		let keys: Vec<_> = result.storage_hints.keys().copied().collect();
		assert_eq!(keys.len(), 3);

		// Keys should be sorted
		for i in 1..keys.len() {
			assert!(keys[i - 1] < keys[i]);
		}

		Ok(())
	}

	/// Regression test for a real bug: `Allocator::allocate` used to compute
	/// `block_count` in 512-byte units (`allocator::BLOCK_SIZE_BYTES` was
	/// hardcoded to 512, an ATA sector size) while `AllocNode`/
	/// `find_free_extent`/`split_and_allocate` — and this crate's own
	/// `physical_lba`/`block_count` contract downstream in `commit.rs`
	/// (`buf_len = block_count * crate::BLOCK_SIZE`, `crate::BLOCK_SIZE ==
	/// 4096`) — all operate in 4 KiB block units. Any object over 512 bytes
	/// was placed (and its write buffer sized) as if it needed up to 8x more
	/// space than it actually did, silently reserving — and then zeroing out
	/// via the oversized write — whatever real content sat in the extra
	/// blocks immediately after it. A single ~600-byte object (comfortably
	/// under one real 4 KiB block, but over the old, wrong 512-byte
	/// threshold) is enough to trigger the bug were it still present: this
	/// asserts it places in exactly one block, and that two such objects in
	/// the same transaction land in *adjacent* blocks (not 2 blocks apart),
	/// proving no phantom extra space is being reserved between them.
	#[test]
	fn test_placement_object_over_512_bytes_uses_true_4kib_block_granularity() -> TestResult<()> {
		// -- Setup & Fixtures
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// 600 bytes: over the old (wrong) 512-byte unit, comfortably under
		// one real 4096-byte block.
		let big1 = Chunk::new(alloc::vec![0xAAu8; 600]);
		let big2 = Chunk::new(alloc::vec![0xBBu8; 600]);

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		txn_plan.add_object(NewObject::new(big1.object_id, big1.encode_canonical()?));
		txn_plan.add_object(NewObject::new(big2.object_id, big2.encode_canonical()?).with_dependency(big1.object_id));

		// -- Exec
		let result = PlacementEngine::place_transaction(&txn_plan, alloc_node, &mut allocator)?;

		// -- Check
		assert_eq!(result.placed_objects.len(), 2);
		for placed in &result.placed_objects {
			assert_eq!(
				placed.block_count, 1,
				"a 600-byte object must place in exactly 1 real 4 KiB block, not {} (512-byte-unit leftover)",
				placed.block_count
			);
		}
		let lba0 = result.placed_objects[0].physical_lba;
		let lba1 = result.placed_objects[1].physical_lba;
		assert_eq!(
			(lba0 as i64 - lba1 as i64).unsigned_abs(),
			1,
			"the two objects must land in adjacent 4 KiB blocks (lba0={lba0}, lba1={lba1}) — a gap here means \
			 phantom extra space was reserved per object"
		);

		Ok(())
	}
}
