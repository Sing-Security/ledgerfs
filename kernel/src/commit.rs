//! Commit protocol for LedgerFS v7
//!
//! Per §15 and §16 of FORMAT.md:
//! - Crash-consistent commit with intent log
//! - Integration with placement engine and allocation tree
//!
//! The write order is the whole crash-consistency argument, so it is documented
//! where it is executed rather than here: see [`CommitEngine::commit`] for which
//! parts of §15.1's protocol this engine actually performs, which it builds but
//! does not persist, and which belong to the caller.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::alloc_node::AllocNode;
use crate::allocator::Allocator;
use crate::cow::TxnPlan;
use crate::error::{Error, Result};
use crate::intent_log::{IntentLogRecord, IntentRecordType};
use crate::object::ObjectId;
use crate::placement::PlacementEngine;
use crate::record::ObjectRecord;

/// Abstraction over a persistent storage device.
///
/// This trait is the hook point for storage barrier semantics (§15.1 + §15.3).
pub trait BlockDevice {
	/// Write `data` to the device starting at the given logical block address.
	fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<()>;

	/// Issue a storage barrier (flush + FUA or equivalent).
	///
	/// Guarantees all preceding writes are durable before any subsequent write.
	/// Per §15.3: when NT stores precede this call, the caller MUST have
	/// already issued `sfence()` before calling `storage_barrier()`.
	fn storage_barrier(&mut self) -> Result<()>;
}

/// No-op block device for testing and stub builds.
pub struct NullBlockDevice;

impl BlockDevice for NullBlockDevice {
	fn write_blocks(&mut self, _lba: u64, _data: &[u8]) -> Result<()> {
		Ok(())
	}

	fn storage_barrier(&mut self) -> Result<()> {
		Ok(())
	}
}

/// Real ATA-PIO-backed [`BlockDevice`] for the primary bus: the
/// implementation this trait's doc comment has been waiting on.
///
/// `lba` in [`BlockDevice::write_blocks`] is a **partition-relative
/// LedgerFS block index** (4 KiB units), matching every other LBA in this
/// crate's public API (e.g. `PlacedObject::physical_lba`,
/// `reader::read_object`'s partition-relative addressing). This device
/// converts that into an absolute ATA sector address the same way
/// `reader.rs`'s callers already do: `partition_start_lba + block_idx *
/// SECTORS_PER_BLOCK`.
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub struct AtaBlockDevice {
	partition_start_lba: u32,
}

#[cfg(all(feature = "reader", target_arch = "x86_64"))]
impl AtaBlockDevice {
	/// `partition_start_lba` is the partition's starting ATA sector
	/// address (not a LedgerFS block index): the same value threaded
	/// through `reader.rs`'s public functions and published via
	/// the mount layer's reported partition start LBA.
	pub fn new(partition_start_lba: u32) -> Self {
		Self { partition_start_lba }
	}
}

#[cfg(all(feature = "reader", target_arch = "x86_64"))]
impl BlockDevice for AtaBlockDevice {
	fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<()> {
		const BLOCK_SIZE: usize = crate::BLOCK_SIZE as usize;
		const SECTORS_PER_BLOCK: u64 = 8; // 4096 / 512, matches reader.rs

		if !data.len().is_multiple_of(BLOCK_SIZE) {
			return Err(Error::InvalidOperation(alloc::format!(
				"write_blocks: data length {} is not a multiple of the block size {BLOCK_SIZE}",
				data.len()
			)));
		}

		for (block_offset, chunk) in data.chunks(BLOCK_SIZE).enumerate() {
			let block_lba = lba + block_offset as u64;
			// Kept as u64: the PIO helpers pick the 28- or 48-bit command
			// form themselves, so narrowing here would reimpose the 128 GiB
			// ceiling the 48-bit path exists to remove.
			let abs_sector: u64 = self.partition_start_lba as u64 + block_lba * SECTORS_PER_BLOCK;

			// Transparent at-rest encryption, when a volume key has been
			// activated for THIS partition and this block is in the
			// encrypted object region, and a no-op otherwise (including for
			// every non-data-volume partition and every plaintext-header
			// block). Encrypts a copy rather than in place: `data` is the
			// caller's buffer and `write_blocks` must not mutate it. See
			// `crate::data_crypto`.
			#[cfg(feature = "data-encryption")]
			let owned;
			#[allow(unused_mut, unused_assignments)]
			let mut chunk: &[u8] = chunk;
			#[cfg(feature = "data-encryption")]
			if crate::data_crypto::is_active_for(self.partition_start_lba) {
				owned = {
					let mut buf = chunk.to_vec();
					crate::data_crypto::maybe_encrypt_block(self.partition_start_lba, block_lba, &mut buf);
					buf
				};
				chunk = &owned;
			}

			// SAFETY: caller of `commit()` guarantees ring 0 execution with
			// I/O port access enabled and that `partition_start_lba` names
			// a real, valid LedgerFS partition: the same contract every
			// other unsafe ATA call in this crate operates under.
			unsafe { crate::writer::ata_pio_write(abs_sector, chunk) }
				.map_err(|e| Error::InvalidOperation(alloc::format!("write_blocks: ATA write failed: {e:?}")))?;
		}

		Ok(())
	}

	fn storage_barrier(&mut self) -> Result<()> {
		// SAFETY: same contract as write_blocks above.
		unsafe { crate::writer::ata_cache_flush() }
			.map_err(|e| Error::InvalidOperation(alloc::format!("storage_barrier: ATA cache flush failed: {e:?}")))
	}
}

#[cfg(all(feature = "reader", target_arch = "x86_64"))]
impl AtaBlockDevice {
	/// Read raw blocks starting at partition-relative block index `lba`
	/// into `buf` (`buf.len()` must be a nonzero multiple of the LedgerFS
	/// block size). The read counterpart to [`BlockDevice::write_blocks`]
	/// deliberately not part of the `BlockDevice` trait itself (which
	/// only models the write side `CommitEngine` needs), since this exists
	/// for a different caller: userspace fetching raw bytes (e.g. a
	/// `Superblock`) it can't reach any other way, via the
	/// `LedgerFsReadBlocks` syscall.
	pub fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<()> {
		const BLOCK_SIZE: usize = crate::BLOCK_SIZE as usize;
		const SECTORS_PER_BLOCK: u64 = 8; // 4096 / 512, matches reader.rs

		if buf.is_empty() || !buf.len().is_multiple_of(BLOCK_SIZE) {
			return Err(Error::InvalidOperation(alloc::format!(
				"read_blocks: buffer length {} is not a nonzero multiple of the block size {BLOCK_SIZE}",
				buf.len()
			)));
		}

		for (block_offset, chunk) in buf.chunks_mut(BLOCK_SIZE).enumerate() {
			let block_lba = lba + block_offset as u64;
			// Kept as u64: the PIO helpers pick the 28- or 48-bit command
			// form themselves, so narrowing here would reimpose the 128 GiB
			// ceiling the 48-bit path exists to remove.
			let abs_sector: u64 = self.partition_start_lba as u64 + block_lba * SECTORS_PER_BLOCK;

			// SAFETY: same contract as write_blocks above: caller
			// guarantees ring 0, I/O port access enabled, and a valid
			// LedgerFS partition at `partition_start_lba`.
			unsafe { crate::reader::ata_pio_read(abs_sector, chunk) }
				.map_err(|e| Error::InvalidOperation(alloc::format!("read_blocks: ATA read failed: {e:?}")))?;

			// Exact inverse of the write path's encryption step: a block
			// that was passed through untouched on write is passed through
			// untouched here too. See `crate::data_crypto`.
			#[cfg(feature = "data-encryption")]
			crate::data_crypto::maybe_decrypt_block(self.partition_start_lba, block_lba, chunk);
		}

		Ok(())
	}
}

/// Transaction ID (u64)
pub type TxId = u64;

/// Commit sequence number (u64)
pub type CommitSeq = u64;

/// Intent log entry for ADD_OBJECT
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddObjectIntent {
	/// Object ID being added
	pub object_id: ObjectId,

	/// Physical LBA where object is written
	pub physical_lba: u64,

	/// Size in blocks
	pub block_count: u32,
}

/// Intent log entry for PREPARE_ROOT
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareRootIntent {
	/// New root object ID
	pub root_id: ObjectId,

	/// Commit sequence number
	pub commit_seq: CommitSeq,

	/// Previous commit_seq (for continuity check)
	pub base_commit_seq: CommitSeq,
}

/// Commit operation result
#[derive(Debug, Clone)]
pub struct CommitResult {
	/// New commit sequence number
	pub commit_seq: CommitSeq,

	/// New root object ID
	pub root_id: ObjectId,

	/// Intent log records generated
	pub intent_records: Vec<IntentLogRecord>,

	/// Number of objects written
	pub objects_written: usize,

	/// Bytes written
	pub bytes_written: u64,

	/// Where each object in this transaction was physically placed.
	///
	/// The placement engine already computes these; before this field existed
	/// they were computed and then discarded, which meant **every** later
	/// lookup of a just-written object had no storage hint and fell back to
	/// `reader::read_object`'s linear scan over the whole object region.
	///
	/// That is quadratically bad in practice, because CoW rewrites the root
	/// inode on every commit and the root is therefore always the
	/// most-recently-placed (highest-block) object, while being the *first*
	/// thing every path resolution fetches. So each commit made the next
	/// path resolution scan further, and a write that should be milliseconds
	/// grew without bound. Returning the hints lets a caller keep them beside
	/// its open volume and turn those lookups into a single O(1) hinted read,
	/// which is what the host crate's sidecar hint cache does.
	pub storage_hints: BTreeMap<ObjectId, crate::placement::StorageHint>,
}

/// Commit engine
pub struct CommitEngine {
	/// Next transaction ID
	next_tx_id: TxId,

	/// Global sequence counter for intent log
	next_seq: u64,

	/// Largest object this engine will commit; see
	/// [`set_max_object_bytes`](Self::set_max_object_bytes).
	max_object_bytes: usize,
}

impl CommitEngine {
	/// Create new commit engine
	pub fn new() -> Self {
		Self {
			next_tx_id: 1,
			next_seq: 1,
			max_object_bytes: crate::MAX_OBJECT_SIZE,
		}
	}

	/// Set the largest object this engine will accept, from the volume's
	/// superblock `LedgerFsLimits` (`0` means "use the compiled-in default").
	///
	/// This bound is a **read-side** limit that has to be enforced on the
	/// write side, because the two disagree silently otherwise:
	/// `reader::read_object`'s scan skips any record whose `stored_len`
	/// exceeds the limit, so an object committed above it is simply never
	/// found again. The commit reports success and the data is gone: the
	/// worst shape a filesystem bug can take. Refusing the commit turns that
	/// into a clean, immediate error naming the object that was too big.
	pub fn set_max_object_bytes(&mut self, max_object_bytes: usize) {
		self.max_object_bytes = if max_object_bytes == 0 {
			crate::MAX_OBJECT_SIZE
		} else {
			max_object_bytes
		};
	}

	/// Execute commit protocol.
	///
	/// # What is real I/O here, and what is not
	///
	/// **Real:** place this transaction's objects, encode each `ObjectRecord`
	/// and write it with `device.write_blocks` at the location the allocator
	/// just chose, write the updated allocator snapshot to
	/// [`ALLOC_SNAPSHOT_BLOCK`](crate::ALLOC_SNAPSHOT_BLOCK), then
	/// `storage_barrier()` once. That single barrier is the whole guarantee
	/// this call makes on its own (nothing may name an object, nor resume from
	/// that snapshot, until their bytes are durable), and it is the only
	/// barrier issued here. The snapshot shares the barrier rather than
	/// following it with one of its own; see the comment at the write for why
	/// that is the same guarantee at half the cost.
	///
	/// **Not real:** the intent log. `FORMAT.md` §15.1's protocol brackets the
	/// commit with `PREPARE_ROOT` and `COMMIT_ROOT` records in an on-disk ring,
	/// and this engine does build those `IntentLogRecord`s and return them in
	/// `CommitResult::intent_records`, but nothing writes a ring, so there is
	/// no record on the device for the protocol's ordering barriers to be
	/// ordering against. Issuing them anyway cost two device barriers per
	/// commit to guard writes that never happen; they are gone.
	///
	/// **Not this function's job:** the superblock and the RLR. There is no
	/// `Superblock` parameter to write through, so the caller updates both:
	/// `ledgerfs-host`'s `Store::commit` is where this engine's write phase and
	/// the volume's commit point actually meet.
	///
	/// # Returns
	/// CommitResult with new commit_seq and root_id
	pub fn commit(
		&mut self,
		txn_plan: &TxnPlan,
		mut alloc_node: AllocNode,
		allocator: &mut Allocator,
		current_commit_seq: CommitSeq,
		device: &mut dyn BlockDevice,
	) -> Result<(CommitResult, AllocNode)> {
		let tx_id = self.next_tx_id;
		self.next_tx_id += 1;

		let new_commit_seq = current_commit_seq + 1;

		let mut intent_records = Vec::new();

		// Step 0: BEGIN intent
		let begin_intent = IntentLogRecord::new(
			self.next_seq,
			tx_id,
			IntentRecordType::Begin,
			Vec::new(), // Empty payload
		);
		self.next_seq += 1;
		intent_records.push(begin_intent);

		// Reject anything the reader could never read back, *before* any of
		// it reaches the disk. See `set_max_object_bytes`: an over-limit
		// object is silently skipped by the reader's scan, so committing one
		// would report success and then behave as if the data had vanished.
		for object in txn_plan.new_objects.values() {
			if object.canonical_bytes.len() > self.max_object_bytes {
				return Err(Error::custom(
					"CommitEngine: object exceeds max_object_bytes and would be unreadable",
				));
			}
		}

		// Step 1: Place objects and prepare ObjectRecords
		let placement_result = PlacementEngine::place_transaction(txn_plan, alloc_node, allocator)?;
		alloc_node = placement_result.updated_alloc_node;

		// Step 2: Record ADD_OBJECT intents for each placed object
		for placed in &placement_result.placed_objects {
			let add_intent = AddObjectIntent {
				object_id: placed.object_id,
				physical_lba: placed.physical_lba,
				block_count: placed.block_count,
			};

			let payload = postcard::to_allocvec(&add_intent)?;
			let intent = IntentLogRecord::new(self.next_seq, tx_id, IntentRecordType::AddObject, payload);
			self.next_seq += 1;
			intent_records.push(intent);
		}

		// Step 3: Prepare ObjectRecords and actually write them to `device`.
		//
		// Each placed object already has a real physical location
		// (`physical_lba`/`block_count`) from the allocator: write the
		// encoded record into a `block_count`-block-sized buffer (zero-
		// padded past the record's real length, matching how the reader
		// tolerates trailing zero padding within an allocated extent) and
		// hand the whole thing to `device.write_blocks` in one call per
		// object. This is the actual "flush all data" step §15.1 describes;
		// before this, `commit()` only ever *built* `ObjectRecord`s in
		// memory and never called `write_blocks` at all, regardless of
		// which `BlockDevice` was passed in.
		let object_records = PlacementEngine::prepare_object_records(&placement_result.placed_objects)?;

		for (placed, record) in placement_result.placed_objects.iter().zip(object_records.iter()) {
			let buf_len = placed.block_count as usize * crate::BLOCK_SIZE as usize;
			let mut buf = alloc::vec![0u8; buf_len];
			let written = record.encode(&mut buf)?;
			if written > buf_len {
				return Err(Error::BufferTooSmall {
					required: written,
					available: buf_len,
				});
			}
			device.write_blocks(placed.physical_lba, &buf)?;
		}

		// Calculate bytes written
		let bytes_written: u64 = object_records.iter().map(|r: &ObjectRecord| r.total_size() as u64).sum();

		// Step 3: the allocator snapshot, written here, before the barrier,
		// so that one barrier covers it and the objects together.
		//
		// What this buys is the superblock's ordering requirement at no extra
		// cost: a superblock is only ever written after this barrier, so the
		// snapshot is durable before any root that depends on these placements
		// can become live. A snapshot that lands *ahead* of the published root
		// only leaks space, because this filesystem never frees
		// (`KNOWN_ISSUES.md` §1): it can never mark a block free that a live
		// root still uses. Writing it after the barrier instead would cost a
		// second device barrier and buy nothing.
		let snapshot = alloc_node.encode_snapshot_block(crate::BLOCK_SIZE as usize)?;
		device.write_blocks(crate::ALLOC_SNAPSHOT_BLOCK, &snapshot)?;

		// The one barrier this engine issues: nothing above or below may name
		// these objects, or resume from this snapshot, until their bytes are
		// durable. (An `sfence` for NT stores, if the object writes used them,
		// was already issued there.)
		device.storage_barrier()?;

		// PREPARE_ROOT intent: built and returned, not persisted. See this
		// function's docs for why no barrier follows it.
		let root_id = txn_plan.new_root.object_id;
		let prepare_intent = PrepareRootIntent {
			root_id,
			commit_seq: new_commit_seq,
			base_commit_seq: current_commit_seq,
		};

		let payload = postcard::to_allocvec(&prepare_intent)?;
		let intent = IntentLogRecord::new(self.next_seq, tx_id, IntentRecordType::PrepareRoot, payload);
		self.next_seq += 1;
		intent_records.push(intent);

		// The superblock update that makes this root live is the caller's:
		// `CommitResult::root_id` and `commit_seq` are what it writes.

		// COMMIT_ROOT intent: built and returned, not persisted.
		let commit_intent = IntentLogRecord::new(
			self.next_seq,
			tx_id,
			IntentRecordType::CommitRoot,
			Vec::new(), // Empty payload
		);
		self.next_seq += 1;
		intent_records.push(commit_intent);

		Ok((
			CommitResult {
				commit_seq: new_commit_seq,
				root_id,
				intent_records,
				objects_written: placement_result.placed_objects.len(),
				bytes_written,
				storage_hints: placement_result.storage_hints,
			},
			alloc_node,
		))
	}
}

impl Default for CommitEngine {
	fn default() -> Self {
		Self::new()
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

	/// A `BlockDevice` that records every `write_blocks` call instead of
	/// discarding it (unlike `NullBlockDevice`) or touching real hardware
	/// (unlike `AtaBlockDevice`, which needs a real ATA bus and so can only
	/// be verified live under a VM monitor such as KVM, not in a host-run unit test).
	/// Lets a test assert on the *actual bytes* `commit()` hands to the
	/// device, closing the gap `test_commit_engine_basic` left open: it
	/// only checked `bytes_written > 0`, never that `write_blocks` was
	/// called at all, let alone with content that decodes back correctly.
	#[derive(Default)]
	struct RecordingBlockDevice {
		writes: alloc::vec::Vec<(u64, alloc::vec::Vec<u8>)>,
		barrier_count: usize,
	}

	impl BlockDevice for RecordingBlockDevice {
		fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<()> {
			self.writes.push((lba, data.to_vec()));
			Ok(())
		}

		fn storage_barrier(&mut self) -> Result<()> {
			self.barrier_count += 1;
			Ok(())
		}
	}

	#[test]
	fn test_commit_engine_actually_writes_decodable_object_data() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);
		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;
		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		// A chunk's canonical form is its raw data (see
		// `Chunk::encode_canonical`), so `chunk.object_id`,
		// `blake3(canonical)`, and `blake3(payload)` are all the same value
		// here: the invariant `ObjectRecordReader::read` enforces
		// (`blake3(stored_bytes) == header.object_id`). Any framing applied
		// on the way to disk breaks it and silently corrupts every read from
		// the data volume, so the assertion below goes through
		// `blake3_hash(&canonical)`
		// rather than reusing `chunk.object_id`.
		let payload = alloc::vec![0xABu8; 100];
		let chunk = Chunk::new(payload);
		let canonical = chunk.encode_canonical()?;
		let expected_object_id = crate::encoding::blake3_hash(&canonical);
		txn_plan.add_object(NewObject::new(chunk.object_id, canonical.clone()));

		let mut device = RecordingBlockDevice::default();

		// -- Exec
		let (_result, _) = engine.commit(&txn_plan, alloc_node, &mut allocator, 0, &mut device)?;

		// -- Check: `write_blocks` was actually called. Building the
		// ObjectRecords in memory without issuing the write leaves every
		// other assertion in this test passing.
		//
		// The device sees one write per placed object **plus** the allocator
		// snapshot, which `commit()` writes last, to its own fixed block, under
		// the same barrier as the objects; see its docs. Asserting that shape
		// rather than the raw count is what keeps this test honest: a raw count
		// would go green again if the snapshot write were dropped.
		let (snapshot_lba, snapshot) = device.writes.last().expect("commit() writes the allocator snapshot");
		assert_eq!(
			*snapshot_lba,
			crate::ALLOC_SNAPSHOT_BLOCK,
			"the final write is the allocator snapshot"
		);
		assert_eq!(
			snapshot.len(),
			crate::BLOCK_SIZE as usize,
			"a snapshot write is exactly one block"
		);
		let object_writes = &device.writes[..device.writes.len() - 1];
		assert_eq!(
			object_writes.len(),
			1,
			"commit() must call write_blocks exactly once per placed object"
		);
		assert!(
			device.barrier_count >= 1,
			"commit() must call storage_barrier at least once"
		);

		// -- Check: the bytes written actually decode back into a valid
		// ObjectRecord whose payload matches what was committed: not just
		// "some bytes were written", but the *right* bytes. (`decode`
		// already self-verifies `blake3(decoded_payload) ==
		// header.object_id` internally and returns `Err` if not, so a
		// successful decode here already proves that invariant; this
		// assertion is the belt-and-braces external check.)
		let (_lba, written_bytes) = &device.writes[0];
		let (header, decoded_payload) = crate::record::ObjectRecordReader::read(written_bytes)?;
		assert_eq!(
			header.object_id, expected_object_id,
			"stored object_id must be blake3 of the actual stored bytes"
		);
		assert_eq!(
			decoded_payload, canonical,
			"decoded payload must match the original committed (canonical) bytes"
		);

		Ok(())
	}

	/// `Allocator::allocate`'s `block_count` must be in the same units as
	/// everything downstream: `AllocNode`'s free-space tracking and
	/// `buf_len = block_count * crate::BLOCK_SIZE`. If it counts 512-byte
	/// sectors instead of 4 KiB blocks, every object over 512 bytes is
	/// written as an oversized zero-padded buffer that clobbers the content
	/// in the blocks after it. See `allocator::BLOCK_SIZE_BYTES`.
	///
	/// Commits two ~600-byte objects, over that threshold, under one 4 KiB
	/// block each, in one transaction, and asserts the two `write_blocks`
	/// calls cover non-overlapping byte ranges.
	#[test]
	fn test_commit_engine_adjacent_objects_over_512_bytes_do_not_overlap() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();
		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);
		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;
		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk1 = Chunk::new(alloc::vec![0xAAu8; 600]);
		let chunk2 = Chunk::new(alloc::vec![0xBBu8; 600]);
		let canonical1 = chunk1.encode_canonical()?;
		let canonical2 = chunk2.encode_canonical()?;
		txn_plan.add_object(NewObject::new(chunk1.object_id, canonical1));
		txn_plan.add_object(NewObject::new(chunk2.object_id, canonical2).with_dependency(chunk1.object_id));

		let mut device = RecordingBlockDevice::default();

		// -- Exec
		let (_result, _) = engine.commit(&txn_plan, alloc_node, &mut allocator, 0, &mut device)?;

		// -- Check
		//
		// Two object writes plus the allocator snapshot, which is written last
		// and is not an object; see the sibling test above for why the shape is
		// asserted rather than the bare count.
		let (snapshot_lba, _) = device.writes.last().expect("commit() writes the allocator snapshot");
		assert_eq!(
			*snapshot_lba,
			crate::ALLOC_SNAPSHOT_BLOCK,
			"the final write is the allocator snapshot"
		);
		let object_writes = &device.writes[..device.writes.len() - 1];
		assert_eq!(
			object_writes.len(),
			2,
			"commit() must call write_blocks once per placed object"
		);
		let (lba0, buf0) = &device.writes[0];
		let (lba1, buf1) = &device.writes[1];

		// Each write must cover exactly one real 4 KiB block, not the
		// old bug's inflated 8-block (32 KiB) buffer for a <1 KiB object.
		assert_eq!(
			buf0.len(),
			crate::BLOCK_SIZE as usize,
			"write for a 600-byte object must be exactly one 4 KiB block"
		);
		assert_eq!(
			buf1.len(),
			crate::BLOCK_SIZE as usize,
			"write for a 600-byte object must be exactly one 4 KiB block"
		);

		// The two writes' absolute byte ranges must not overlap.
		let start0 = lba0 * crate::BLOCK_SIZE as u64;
		let end0 = start0 + buf0.len() as u64;
		let start1 = lba1 * crate::BLOCK_SIZE as u64;
		let end1 = start1 + buf1.len() as u64;
		assert!(
			end0 <= start1 || end1 <= start0,
			"overlapping writes: [{start0}, {end0}) vs [{start1}, {end1}): an oversized write from one object's \
			 placement clobbered the other's block"
		);

		Ok(())
	}

	#[test]
	fn test_commit_engine_basic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();

		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Create transaction
		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk = Chunk::new(alloc::vec![1, 2, 3, 4, 5]);
		let canonical = chunk.encode_canonical()?;
		let new_obj = NewObject::new(chunk.object_id, canonical);
		txn_plan.add_object(new_obj);

		// -- Exec
		let (result, _updated_alloc_node) =
			engine.commit(&txn_plan, alloc_node, &mut allocator, 0, &mut NullBlockDevice)?;

		// -- Check
		assert_eq!(result.commit_seq, 1); // Incremented from 0
		assert_eq!(result.objects_written, 1);
		assert!(result.bytes_written > 0);

		Ok(())
	}

	#[test]
	fn test_commit_engine_intent_log_sequence() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();

		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk = Chunk::new(alloc::vec![1, 2, 3]);
		txn_plan.add_object(NewObject::new(chunk.object_id, chunk.encode_canonical()?));

		// -- Exec
		let (result, _) = engine.commit(&txn_plan, alloc_node, &mut allocator, 0, &mut NullBlockDevice)?;

		// -- Check - Intent log should have: BEGIN, ADD_OBJECT, PREPARE_ROOT, COMMIT_ROOT
		assert!(result.intent_records.len() >= 4);
		assert_eq!(result.intent_records[0].record_type, IntentRecordType::Begin);
		assert_eq!(result.intent_records[1].record_type, IntentRecordType::AddObject);

		// Find PREPARE_ROOT and COMMIT_ROOT
		let has_prepare = result
			.intent_records
			.iter()
			.any(|r| r.record_type == IntentRecordType::PrepareRoot);
		let has_commit = result
			.intent_records
			.iter()
			.any(|r| r.record_type == IntentRecordType::CommitRoot);

		assert!(has_prepare);
		assert!(has_commit);

		Ok(())
	}

	#[test]
	fn test_commit_engine_multiple_objects() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();

		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		// Create transaction with multiple objects
		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		let chunk1 = Chunk::new(alloc::vec![1, 2, 3]);
		let chunk2 = Chunk::new(alloc::vec![4, 5, 6]);
		let chunk3 = Chunk::new(alloc::vec![7, 8, 9]);

		txn_plan.add_object(NewObject::new(chunk1.object_id, chunk1.encode_canonical()?));
		txn_plan.add_object(NewObject::new(chunk2.object_id, chunk2.encode_canonical()?));
		txn_plan.add_object(NewObject::new(chunk3.object_id, chunk3.encode_canonical()?));

		// -- Exec
		let (result, _) = engine.commit(&txn_plan, alloc_node, &mut allocator, 0, &mut NullBlockDevice)?;

		// -- Check
		assert_eq!(result.objects_written, 3);

		// Should have 3 ADD_OBJECT intents
		let add_object_count = result
			.intent_records
			.iter()
			.filter(|r| r.record_type == IntentRecordType::AddObject)
			.count();
		assert_eq!(add_object_count, 3);

		Ok(())
	}

	#[test]
	fn test_commit_engine_commit_seq_increment() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();

		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk = Chunk::new(alloc::vec![1]);
		txn_plan.add_object(NewObject::new(chunk.object_id, chunk.encode_canonical()?));

		// -- Exec - Commit at seq 5
		let (result, updated_alloc_node) =
			engine.commit(&txn_plan, alloc_node, &mut allocator, 5, &mut NullBlockDevice)?;

		// -- Check - Should increment to 6
		assert_eq!(result.commit_seq, 6);

		// Commit again at seq 6
		let chunk2 = Chunk::new(alloc::vec![2]);
		let mut txn_plan2 = TxnPlan::new(ObjectRef::zero(), Root::new());
		txn_plan2.add_object(NewObject::new(chunk2.object_id, chunk2.encode_canonical()?));

		let (result2, _) = engine.commit(&txn_plan2, updated_alloc_node, &mut allocator, 6, &mut NullBlockDevice)?;

		// Should increment to 7
		assert_eq!(result2.commit_seq, 7);

		Ok(())
	}

	#[test]
	fn test_commit_engine_tx_id_increment() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut engine = CommitEngine::new();

		let total_blocks = 10000;
		let config = AllocatorConfig::new(total_blocks);

		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 10000, AllocationState::Free))?;

		let mut allocator = Allocator::new(config);
		allocator.init_from_tree(&alloc_node)?;

		let mut txn_plan = TxnPlan::new(ObjectRef::zero(), Root::new());
		let chunk = Chunk::new(alloc::vec![1]);
		txn_plan.add_object(NewObject::new(chunk.object_id, chunk.encode_canonical()?));

		// -- Exec - First commit
		let (result1, updated_alloc_node) =
			engine.commit(&txn_plan, alloc_node, &mut allocator, 0, &mut NullBlockDevice)?;

		// Get tx_id from BEGIN record
		let tx_id_1 = result1.intent_records[0].tx_id;

		// Second commit
		let chunk2 = Chunk::new(alloc::vec![2]);
		let mut txn_plan2 = TxnPlan::new(ObjectRef::zero(), Root::new());
		txn_plan2.add_object(NewObject::new(chunk2.object_id, chunk2.encode_canonical()?));

		let (result2, _) = engine.commit(&txn_plan2, updated_alloc_node, &mut allocator, 1, &mut NullBlockDevice)?;
		let tx_id_2 = result2.intent_records[0].tx_id;

		// -- Check - tx_id should increment
		assert_eq!(tx_id_1, 1);
		assert_eq!(tx_id_2, 2);

		Ok(())
	}
}
