//! LedgerFS shared reader — kernel-mode read-only access to on-disk objects.
//!
//! This module provides a minimal, read-only path into LedgerFS for the
//! kernel boot sequence and any post-boot kernel code that needs to read
//! files from a mounted LedgerFS partition.
//!
//! # ATA PIO
//!
//! Disk I/O is performed via inline 28-bit LBA PIO to the primary ATA bus.
//! All access is serialized through `ATA_LOCK` (a `spin::Mutex`) so that
//! concurrent callers (e.g. boot + a future kernel file-read syscall) never
//! interleave port I/O sequences.
//!
//! # Safety
//!
//! Every public function in this module is `unsafe` because it performs raw
//! I/O port operations.  Callers must ensure:
//! - Ring 0 execution with I/O port access enabled.
//! - SSE initialised (required for BLAKE3 on x86_64).
//! - The partition LBA arguments point to a valid LedgerFS partition.
//!
//! # Feature gate
//!
//! This module is compiled only when `feature = "reader"` AND
//! `target_arch = "x86_64"`.  See `lib.rs`:
//! ```ignore
//! #[cfg(all(feature = "reader", target_arch = "x86_64"))]
//! pub mod reader;
//! ```

extern crate alloc;

use alloc::vec::Vec;
use blake3;
use core::cmp::min;
use core::convert::TryInto;

use crate::object::{DirNode, ExtentDescriptor, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef};

// ── Error type ───────────────────────────────────────────────────────

/// LedgerFS reader error type for kernel use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderError {
	/// Low-level disk I/O failure.
	Io,
	/// Object not found in the scanned block range.
	NotFound,
	/// Object header could not be parsed.
	InvalidObject,
	/// BLAKE3 content hash did not match the expected ObjectId.
	ChecksumMismatch,
	/// A path component was not found in a directory node.
	PathNotFound,
	/// Expected a file but got a directory (or vice-versa).
	TypeMismatch,
	/// File exceeds the in-kernel transfer safety bound.
	FileTooLarge,
}

// ── On-disk layout constants ─────────────────────────────────────────

/// LedgerFS block size (4 KiB).
const BLOCK_SIZE: usize = 4096;

/// ObjectRecord header size (bytes before payload).
const HEADER_SIZE: usize = 60;

/// Byte offset of the 32-byte ObjectId within the header.
const OBJECT_ID_OFFSET: usize = 28;

/// Byte offset of the `stored_len` (u32 LE) within the header.
const STORED_LEN_OFFSET: usize = 12;

/// Byte offset of the 2-byte compression flags within the header.
///
/// Byte 10 is the compression algorithm, byte 11 the encryption flags. They are
/// read separately: a compressed record is now readable, an encrypted one still
/// is not.
const COMPRESSION_OFFSET: usize = 10;

/// Byte offset of the `logical_len` (u32 LE) — the size *before* compression,
/// and the length `decompress` must reproduce.
const LOGICAL_LEN_OFFSET: usize = 16;

/// First block to scan for user objects.
const START_BLOCK: u64 = crate::RESERVED_HEADER_BLOCKS;

/// Fallback maximum number of blocks to scan from START_BLOCK, used only when
/// the mounted superblock carries no `max_scan_blocks`.
///
/// This is a *floor*, not the real bound: a compiled-in constant cannot know
/// how big the image it is reading is. Successive bumps (4096 → 16384) each
/// held only until the next larger image, and the failure mode is silent and
/// misleading — an object written past the limit is simply reported
/// `NOT FOUND`, indistinguishable from corruption. Installing a 63 MB guest
/// ISO put objects at block 22394 and broke it again.
///
/// The real bound now comes from the superblock, which `forge-builder` sizes
/// to the highest block it actually wrote (see its
/// `fs_init::rewrite_superblock_limits`), so an image can no longer be built
/// whose own reader cannot reach the end of it.
const MAX_BLOCKS: u64 = 16384;

/// ATA sectors per 4 KiB LedgerFS block (4096 / 512).
const SECTORS_PER_BLOCK: u32 = 8;

/// Maximum single-object payload size the reader will accept (256 KiB).
///
/// This caps how many consecutive blocks `try_read_block` will read for a
/// multi-block ObjectRecord.  ExtentNodes for large files can reach ~44 KiB
/// (1090 extents), so the cap must be well above that.  256 KiB allows up
/// to ~6300 extents in one ExtentNode while bounding memory use.
use crate::MAX_OBJECT_SIZE;

/// Maximum in-kernel transfer per `read_range` / `read_file` (safety bound).
///
/// Raised from 1 MiB → 8 MiB so that `ProcessCreateFromFd` can load
/// larger ELF binaries (e.g. `temper-cbl` at ~4.2 MiB) in a single
/// `read_range` / `read_file` pass without hitting `FileTooLarge`.
const MAX_RANGE_READ: usize = 8 * 1024 * 1024; // 8 MiB

// ── ATA PIO constants and shared primitives ─────────────────────────
//
// The port addresses, register offsets, status bits, `AtaError`, the
// crate-level `ATA_LOCK`, and the raw `outb`/`inb`/`inw`/`outw` + wait
// helpers all now live in `crate::ata_io` so the write path (`writer.rs`)
// serializes through the exact same bus lock instead of a second, separate
// one — see that module's doc comment for why two locks would be a real
// correctness hazard, not just a style preference.
use crate::ata_io::{
	ATA_DATA, ATA_DRIVE_HEAD, ATA_LBA_HIGH, ATA_LBA_LOW, ATA_LBA_MID, ATA_LOCK, ATA_PRIMARY_BASE, ATA_SECTOR_COUNT,
	ATA_STATUS_BSY, ATA_STATUS_CMD, ATA_STATUS_DRQ, ATA_STATUS_ERR, AtaError, MAX_28BIT_LBA, SECTOR_SIZE, ata_wait_not_busy,
	ata_wait_ready, inw, outb,
};

/// READ SECTORS command (28-bit LBA PIO). Read-specific, stays local.
const ATA_CMD_READ_SECTORS: u8 = 0x20;

/// READ SECTORS EXT — the 48-bit LBA counterpart of `ATA_CMD_READ_SECTORS`.
const ATA_CMD_READ_SECTORS_EXT: u8 = 0x24;

// ── Configurable operating limits ────────────────────────────────────
//
// This crate is a dependency of `forge`/`crucible-kernel`, not the other
// way around — `reader.rs` cannot reach `ledgerfs_mount::get_mount()` (that
// lives in the higher-level crate). Instead, whoever parses the superblock
// (currently `forge-boot`'s `verify_superblock`, right where it also calls
// `ledgerfs_mount::set_mount`) pushes the resolved limits down into this
// module via `set_limits`. `None` (before boot, or in a test/example
// context that never calls `set_limits`) resolves to the same hardcoded
// values this module used before `LedgerFsLimits` existed.
static LIMITS: spin::Mutex<Option<crate::superblock::LedgerFsLimits>> = spin::Mutex::new(None);

// region:    --- Object-location index

/// In-memory map from object id to the block that holds it.
///
/// Maps object id to the block holding it.
///
/// `read_object`'s fallback is a linear scan of the whole store, and each
/// block costs an `ata_pio_read`. That is affordable once and ruinous
/// repeated — a lookup *miss* would otherwise re-sweep the entire store.
///
/// The scan already parses every block header to compare ids; this keeps what
/// it learns. Only an 8-byte id prefix is stored. A prefix collision is not a
/// correctness problem: the block is still read and its full 32-byte id
/// compared by `try_read_block`.
struct ObjectIndex {
	/// (id prefix, block index), in scan order.
	entries: Vec<(u64, u64)>,
	/// True once a sweep has covered the whole scan range without an early
	/// exit, so a lookup miss can be answered `NotFound` without re-scanning.
	///
	/// Note the bound is the *scan range* (`resolved_max_blocks()`), not the
	/// store: an object written beyond that range is invisible to the sweep,
	/// exactly as it was invisible to the un-indexed scan this replaces. The
	/// builder now sizes `max_scan_blocks` to the image it wrote (see
	/// `forge-builder`'s `rewrite_superblock_limits`), which is what makes
	/// the two coincide.
	complete: bool,
}

static OBJECT_INDEX: spin::Mutex<Option<ObjectIndex>> = spin::Mutex::new(None);

/// First 8 bytes of an object id, as the index key.
#[inline]
fn id_prefix(id: &ObjectId) -> u64 {
	let b = id.as_bytes();
	u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Blocks whose indexed prefix matches `id`, best candidate first.
fn index_candidates(id: &ObjectId) -> Vec<u64> {
	let want = id_prefix(id);
	match OBJECT_INDEX.lock().as_ref() {
		Some(ix) => ix.entries.iter().filter(|(p, _)| *p == want).map(|(_, b)| *b).collect(),
		None => Vec::new(),
	}
}

/// True when the index covers the whole store, so a miss is authoritative.
fn index_is_complete() -> bool {
	OBJECT_INDEX.lock().as_ref().map(|ix| ix.complete).unwrap_or(false)
}

/// Fold the results of a sweep into the index.
///
/// `entries` must be in ascending block order, which is how the scan produces
/// them.
fn index_install(entries: Vec<(u64, u64)>, complete: bool) {
	let mut guard = OBJECT_INDEX.lock();

	// Never downgrade a complete index to a partial one: an early-exiting scan
	// (it found its object and stopped) has seen strictly less than a full
	// sweep, and overwriting with it would resurrect the rescan-on-miss cost
	// this exists to remove.
	if let Some(existing) = guard.as_ref() {
		if existing.complete && !complete {
			return;
		}
	}

	// Merge rather than replace. A sweep that exits early covers only
	// START_BLOCK..hit, so replacing outright would *shrink* an index built by
	// an earlier full sweep and silently send later blocks back to scanning —
	// which is the case after any write, since a write clears `complete` and
	// the next lookup is likely to exit early. Both lists are in ascending
	// block order, so this is a linear merge; on a tie the newer entry wins,
	// as it reflects the more recent read of that block.
	let merged = match guard.take() {
		Some(existing) => merge_by_block(existing.entries, entries),
		None => entries,
	};

	*guard = Some(ObjectIndex { entries: merged, complete });
}

/// Merge two block-ordered `(id_prefix, block)` lists, `new` winning ties.
fn merge_by_block(old: Vec<(u64, u64)>, new: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
	let mut out = Vec::with_capacity(old.len().max(new.len()));
	let (mut i, mut j) = (0usize, 0usize);
	while i < old.len() && j < new.len() {
		match old[i].1.cmp(&new[j].1) {
			core::cmp::Ordering::Less => {
				out.push(old[i]);
				i += 1;
			}
			core::cmp::Ordering::Greater => {
				out.push(new[j]);
				j += 1;
			}
			core::cmp::Ordering::Equal => {
				out.push(new[j]);
				i += 1;
				j += 1;
			}
		}
	}
	out.extend_from_slice(&old[i..]);
	out.extend_from_slice(&new[j..]);
	out
}

/// Mark the index as no longer covering the whole store. Must be called by
/// anything that writes to it.
///
/// The recorded *entries* deliberately survive. A write can make an entry
/// stale — the block it names may now hold something else — but every
/// candidate is re-read and its full 32-byte id compared before being
/// returned, so a stale entry costs one wasted read and can never yield wrong
/// data. What a write genuinely invalidates is only the `complete` claim: the
/// object just written is not in the index, so a miss is no longer
/// authoritative.
///
/// Dropping the entries as well (the first version of this) was
/// self-defeating: this kernel writes to disk during boot, so the very next
/// lookup miss paid for another full sweep, and the O(n^2) behaviour this
/// index exists to remove came straight back. Observed live: two full 16384-
/// block sweeps in a single boot.
pub fn invalidate_object_index() {
	if let Some(ix) = OBJECT_INDEX.lock().as_mut() {
		ix.complete = false;
	}
}

// endregion: --- Object-location index

/// Publish the operating limits extracted from the mounted partition's
/// superblock. Called once, from `forge-boot`'s superblock verification,
/// right alongside `ledgerfs_mount::set_mount`.
pub fn set_limits(limits: crate::superblock::LedgerFsLimits) {
	*LIMITS.lock() = Some(limits);
}

/// Resolved `max_object_bytes` — see `LedgerFsLimits::max_object_bytes` doc
/// comment for what this bounds. Falls back to the pre-config hardcoded
/// default (256 KiB) if `set_limits` was never called or the volume's
/// value is 0 ("absent").
fn resolved_max_object_bytes() -> usize {
	match LIMITS.lock().as_ref().map(|l| l.max_object_bytes) {
		Some(v) if v != 0 => v as usize,
		_ => MAX_OBJECT_SIZE,
	}
}

/// Resolved `max_range_read_bytes`. Falls back to 8 MiB.
fn resolved_max_range_read() -> usize {
	match LIMITS.lock().as_ref().map(|l| l.max_range_read_bytes) {
		Some(v) if v != 0 => v as usize,
		_ => MAX_RANGE_READ,
	}
}

/// Resolved `max_scan_blocks`. Falls back to 16384.
fn resolved_max_blocks() -> u64 {
	match LIMITS.lock().as_ref().map(|l| l.max_scan_blocks) {
		Some(v) if v != 0 => v,
		_ => MAX_BLOCKS,
	}
}

// ── Core ATA PIO read (lock-holding) ─────────────────────────────────

/// Read sectors from disk using 28-bit LBA PIO mode.
///
/// **This function acquires `ATA_LOCK` internally.** Callers do not need
/// (and must not already hold) the lock.
///
/// # Arguments
///
/// * `lba` — Starting Logical Block Address (28-bit, 0..0x0FFF_FFFF).
/// * `buf` — Output buffer; length must be a multiple of 512 bytes.
///
/// # Safety
///
/// Caller must ensure:
/// - Ring 0 execution with I/O port access enabled.
/// - The ATA primary bus is present and initialised.
/// - `lba` + sector_count does not exceed drive capacity.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn ata_pio_read(lba: u64, buf: &mut [u8]) -> Result<(), AtaError> {
	if buf.len() % SECTOR_SIZE != 0 {
		return Err(AtaError::InvalidBufferSize);
	}

	let sector_count = (buf.len() / SECTOR_SIZE) as u32;

	// Acquire the crate-level ATA lock for the duration of the command.
	let _guard = crate::ata_io::lock_ata_bus();

	// SAFETY: Lock held; ATA controller initialised per caller contract.
	// All port I/O and helper calls below are unsafe because they perform
	// raw x86 `in`/`out` instructions.  The lock ensures serialisation.
	unsafe {
		ata_wait_not_busy()?;

// SAFETY: bus lock held; picks the 28- or 48-bit form and programs
		// the taskfile accordingly.
		let cmd = crate::ata_io::program_taskfile(
			lba,
			sector_count,
			ATA_CMD_READ_SECTORS,
			ATA_CMD_READ_SECTORS_EXT,
		)?;

		// SAFETY: Issuing the read command to register 0x1F7.
		outb(ATA_PRIMARY_BASE + ATA_STATUS_CMD, cmd);

		for sector_idx in 0..sector_count {
			ata_wait_ready()?;

			let sector_offset = (sector_idx as usize) * SECTOR_SIZE;
			let sector_buf = &mut buf[sector_offset..sector_offset + SECTOR_SIZE];

			// SAFETY: DRQ asserted, and `sector_buf` is exactly SECTOR_SIZE
			// bytes, matching the word count passed here.
			crate::ata_io::insw_bulk(
				ATA_PRIMARY_BASE + ATA_DATA,
				sector_buf.as_mut_ptr(),
				SECTOR_SIZE / 2,
			);
		}
	}

	Ok(())
}

/// Stub for non-x86_64 targets (compile gate only — never callable).
#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn ata_pio_read(_lba: u64, _buf: &mut [u8]) -> Result<(), AtaError> {
	Err(AtaError::InvalidLba)
}

// ── Object I/O ───────────────────────────────────────────────────────

/// Try to read and validate an object from a single block.
///
/// Returns `Ok(payload)` if the block contains a valid, uncompressed
/// `ObjectRecord` whose embedded ObjectId matches `object_id` and whose
/// BLAKE3 checksum verifies.  Returns `Err(...)` otherwise.
///
/// # Safety
///
/// Read one LedgerFS block by partition-relative block index, transparently
/// decrypting it when this partition has an active volume key.
///
/// Every object-path read in this module goes through here rather than
/// calling [`ata_pio_read`] directly, because `reader.rs` bypasses
/// [`crate::commit::AtaBlockDevice`] entirely (it does its own raw
/// sector-address arithmetic for the scan/hint fast paths). Without this
/// wrapper the write side would encrypt while this side kept reading raw
/// ciphertext — every object would fail its `blake3(payload) == object_id`
/// check and the volume would look universally corrupt. A no-op when the
/// `data-encryption` feature is off, when no key is active for this
/// partition, or for plaintext-header blocks — see `crate::data_crypto`.
///
/// # Safety
///
/// Same preconditions as [`ata_pio_read`]: ring 0, I/O ports accessible,
/// and `partition_start_lba` names a real LedgerFS partition.
unsafe fn read_block_decrypted(partition_start_lba: u32, block_idx: u64, buf: &mut [u8]) -> Result<(), AtaError> {
	// No `as u32` here: that silently truncated the sector number, and the
	// PIO path now addresses 48 bits.
	let block_sector = partition_start_lba as u64 + block_idx * SECTORS_PER_BLOCK as u64;
	// SAFETY: forwarded from this function's own safety contract.
	unsafe { ata_pio_read(block_sector, buf) }?;
	#[cfg(feature = "data-encryption")]
	crate::data_crypto::maybe_decrypt_block(partition_start_lba, block_idx, buf);
	Ok(())
}

/// Same preconditions as [`read_object`].
unsafe fn try_read_block(
	partition_start_lba: u32,
	block_idx: u64,
	object_id: &ObjectId,
) -> Result<Vec<u8>, ReaderError> {
	let mut block_buf = [0u8; BLOCK_SIZE];
	let block_sector = (partition_start_lba as u64 + block_idx * SECTORS_PER_BLOCK as u64);

	log::trace!(
		"[ledgerfs-reader] try_read_block: block_idx={} sector={} expect_id={:02x}{:02x}{:02x}{:02x}...",
		block_idx,
		block_sector,
		object_id.as_bytes()[0],
		object_id.as_bytes()[1],
		object_id.as_bytes()[2],
		object_id.as_bytes()[3],
	);

	// SAFETY: Caller guarantees partition_start_lba is valid and ATA is accessible.
	unsafe { read_block_decrypted(partition_start_lba, block_idx, &mut block_buf) }.map_err(|e| {
		log::warn!(
			"[ledgerfs-reader] try_read_block: ATA read failed at sector={}: {:?}",
			block_sector,
			e
		);
		ReaderError::Io
	})?;

	if block_buf.len() < HEADER_SIZE {
		return Err(ReaderError::NotFound);
	}

	// Check magic.
	let magic_bytes: [u8; 8] = block_buf[0..8].try_into().unwrap();
	let magic = u64::from_le_bytes(magic_bytes);
	if magic != crate::OBJECT_RECORD_MAGIC {
		log::trace!(
			"[ledgerfs-reader] try_read_block: block {} bad magic {:#018x} (first 8 bytes: {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x})",
			block_idx,
			magic,
			block_buf[0],
			block_buf[1],
			block_buf[2],
			block_buf[3],
			block_buf[4],
			block_buf[5],
			block_buf[6],
			block_buf[7],
		);
		return Err(ReaderError::NotFound);
	}

	// Encrypted records are still unreadable here; compressed ones are not.
	//
	// This used to reject both, which is why turning compression on at the
	// write path made every object unreadable — writes succeeded and the
	// read-back failed, surfacing as a create failure a hundred rows into a
	// directory. The algorithm byte is now carried down to the payload
	// assembly below.
	let compression_algo = block_buf[COMPRESSION_OFFSET];
	if block_buf[COMPRESSION_OFFSET + 1] != 0 {
		log::trace!(
			"[ledgerfs-reader] try_read_block: block {} encrypted — skipping",
			block_idx
		);
		return Err(ReaderError::NotFound);
	}

	// Read stored payload length.
	let stored_len_bytes: [u8; 4] = block_buf[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].try_into().unwrap();
	let stored_len = u32::from_le_bytes(stored_len_bytes) as usize;

	// Safety cap: reject absurdly large payloads (corrupt header).
	if stored_len > resolved_max_object_bytes() {
		log::warn!(
			"[ledgerfs-reader] try_read_block: block {} stored_len {} exceeds safety cap {}",
			block_idx,
			stored_len,
			resolved_max_object_bytes(),
		);
		return Err(ReaderError::NotFound);
	}

	// Match ObjectId.
	let stored_object_id = &block_buf[OBJECT_ID_OFFSET..OBJECT_ID_OFFSET + 32];
	if stored_object_id != object_id.as_bytes() {
		log::debug!(
			"[ledgerfs-reader] try_read_block: block {} ObjectId MISMATCH — stored={:02x}{:02x}{:02x}{:02x}.. expected={:02x}{:02x}{:02x}{:02x}.. (obj_type={:02x}{:02x})",
			block_idx,
			stored_object_id[0],
			stored_object_id[1],
			stored_object_id[2],
			stored_object_id[3],
			object_id.as_bytes()[0],
			object_id.as_bytes()[1],
			object_id.as_bytes()[2],
			object_id.as_bytes()[3],
			block_buf[8],
			block_buf[9],
		);
		return Err(ReaderError::NotFound);
	}

	// ── Assemble the full payload (single-block or multi-block) ──────
	let total_record_len = HEADER_SIZE + stored_len;

	let payload = if total_record_len <= BLOCK_SIZE {
		// Single-block fast path — payload fits entirely in the first block.
		let payload_end = HEADER_SIZE + stored_len;
		if payload_end > block_buf.len() {
			return Err(ReaderError::NotFound);
		}
		block_buf[HEADER_SIZE..payload_end].to_vec()
	} else {
		// Multi-block path — payload spans consecutive blocks.
		let extra_blocks = (total_record_len - BLOCK_SIZE + BLOCK_SIZE - 1) / BLOCK_SIZE;
		log::debug!(
			"[ledgerfs-reader] try_read_block: block {} multi-block record — stored_len={} total_blocks={}",
			block_idx,
			stored_len,
			1 + extra_blocks,
		);

		// Start with the payload bytes from the first block.
		let mut full_payload = Vec::with_capacity(stored_len);
		full_payload.extend_from_slice(&block_buf[HEADER_SIZE..]);

		// Read subsequent blocks.
		for i in 1..=(extra_blocks as u64) {
			let next_sector = partition_start_lba as u64 + (block_idx + i) * SECTORS_PER_BLOCK as u64;
			let mut extra_buf = [0u8; BLOCK_SIZE];
			// SAFETY: Caller guarantees partition_start_lba is valid.
			unsafe { read_block_decrypted(partition_start_lba, block_idx + i, &mut extra_buf) }.map_err(|e| {
				log::warn!(
					"[ledgerfs-reader] try_read_block: ATA read failed at continuation block {}+{} sector={}: {:?}",
					block_idx,
					i,
					next_sector,
					e
				);
				ReaderError::Io
			})?;

			let remaining = stored_len - full_payload.len();
			let copy_len = remaining.min(BLOCK_SIZE);
			full_payload.extend_from_slice(&extra_buf[..copy_len]);
		}

		if full_payload.len() < stored_len {
			log::error!(
				"[ledgerfs-reader] try_read_block: block {} multi-block assembly incomplete: got {} expected {}",
				block_idx,
				full_payload.len(),
				stored_len,
			);
			return Err(ReaderError::NotFound);
		}

		// Truncate to exact stored_len (last block may have padding).
		full_payload.truncate(stored_len);
		full_payload
	};

	// Undo compression before hashing.
	//
	// `ObjectRecordWriter` computes the ObjectId over the *logical* data and
	// stores the compressed form, so the identity check below is only correct
	// against the decompressed bytes. Getting this the wrong way round would
	// reject every compressed object with a BLAKE3 mismatch.
	let payload = if compression_algo != 0 {
		let logical_len_bytes: [u8; 4] =
			block_buf[LOGICAL_LEN_OFFSET..LOGICAL_LEN_OFFSET + 4].try_into().unwrap();
		let logical_len = u32::from_le_bytes(logical_len_bytes) as usize;
		if logical_len > resolved_max_object_bytes() {
			log::warn!(
				"[ledgerfs-reader] try_read_block: block {} logical_len {} exceeds safety cap",
				block_idx,
				logical_len,
			);
			return Err(ReaderError::NotFound);
		}
		let algo = crate::compression::CompressionAlgorithm::from_u8(compression_algo)
			.map_err(|_| ReaderError::NotFound)?;
		match crate::compression::decompress(&payload, algo, logical_len) {
			Ok(d) => d,
			Err(e) => {
				log::error!(
					"[ledgerfs-reader] try_read_block: block {} decompress failed (algo={}): {:?}",
					block_idx,
					compression_algo,
					e,
				);
				return Err(ReaderError::NotFound);
			}
		}
	} else {
		payload
	};

	// Verify BLAKE3.
	let computed_hash = blake3::hash(&payload);
	if computed_hash.as_bytes() != object_id.as_bytes() {
		log::error!(
			"[ledgerfs-reader] try_read_block: block {} BLAKE3 MISMATCH — hash={:02x}{:02x}{:02x}{:02x}.. header={:02x}{:02x}{:02x}{:02x}.. (stored_len={})",
			block_idx,
			computed_hash.as_bytes()[0],
			computed_hash.as_bytes()[1],
			computed_hash.as_bytes()[2],
			computed_hash.as_bytes()[3],
			object_id.as_bytes()[0],
			object_id.as_bytes()[1],
			object_id.as_bytes()[2],
			object_id.as_bytes()[3],
			stored_len,
		);
		return Err(ReaderError::ChecksumMismatch);
	}

	log::trace!(
		"[ledgerfs-reader] try_read_block: block {} OK (stored_len={}{})",
		block_idx,
		stored_len,
		if total_record_len > BLOCK_SIZE {
			" multi-block"
		} else {
			""
		},
	);
	Ok(payload)
}

/// Read a raw object by ID from disk, validating its BLAKE3 checksum.
///
/// When `hint` is `Some(block_idx)` and `block_idx > 0`, the hinted block
/// is tried **first** as a single O(1) ATA PIO read.  If the hint misses
/// (wrong ID, bad magic, I/O error) the function falls back to the linear
/// scan over `START_BLOCK..(START_BLOCK + MAX_BLOCKS)`.
///
/// Pass `None` (or `Some(0)`) when no hint is available — e.g. for the
/// root inode whose hint is not yet known.
///
/// Compressed and encrypted records are rejected (reader is plaintext-only).
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` points to a valid LedgerFS
/// partition and that all ATA preconditions are met (ring 0, x86_64).
pub unsafe fn read_object(
	partition_start_lba: u32,
	object_id: &ObjectId,
	hint: Option<u64>,
) -> Result<Vec<u8>, ReaderError> {
	log::debug!(
		"[ledgerfs-reader] read_object: id={:02x}{:02x}{:02x}{:02x}.. hint={:?} part_lba={}",
		object_id.as_bytes()[0],
		object_id.as_bytes()[1],
		object_id.as_bytes()[2],
		object_id.as_bytes()[3],
		hint,
		partition_start_lba,
	);

	// ── Fast path: try the hinted block first ────────────────────────
	if let Some(block_idx) = hint {
		// Bounds-check before use: an out-of-range hint would overflow the
		// u64 sector calculation (block_idx * 8), producing a wrapped u32 LBA
		// that issues an ATA command to a wrong sector.  ATA then sets the ERR
		// bit in its status register and all subsequent reads fail with DriveError.
		if block_idx >= START_BLOCK && block_idx < START_BLOCK + resolved_max_blocks() {
			// SAFETY: Caller guarantees partition_start_lba is valid.
			match unsafe { try_read_block(partition_start_lba, block_idx, object_id) } {
				Ok(payload) => {
					log::debug!("[ledgerfs-reader] read_object: HINT HIT at block {}", block_idx);
					return Ok(payload);
				}
				Err(ReaderError::ChecksumMismatch) => {
					log::error!(
						"[ledgerfs-reader] read_object: HINT block {} checksum mismatch — aborting",
						block_idx
					);
					return Err(ReaderError::ChecksumMismatch);
				}
				Err(e) => {
					log::warn!(
						"[ledgerfs-reader] read_object: HINT MISS at block {} ({:?}) — falling back to scan",
						block_idx,
						e,
					);
				}
			}
		}
	}

	// ── Indexed lookup: answer from a previous sweep ─────────────────
	//
	// Tried before the scan and after the caller's own hint, so a correct hint
	// still costs exactly one read. Each candidate is verified by
	// `try_read_block`, which compares the full 32-byte id, so an 8-byte
	// prefix collision costs one extra read rather than returning wrong data.
	for cand in index_candidates(object_id) {
		if cand < START_BLOCK || cand >= START_BLOCK + resolved_max_blocks() {
			continue;
		}
		// SAFETY: Caller guarantees partition_start_lba is valid.
		match unsafe { try_read_block(partition_start_lba, cand, object_id) } {
			Ok(payload) => {
				log::debug!("[ledgerfs-reader] read_object: INDEX HIT at block {}", cand);
				return Ok(payload);
			}
			// A stale or colliding entry: fall through to the next candidate,
			// and ultimately to the scan.
			Err(_) => continue,
		}
	}

	// A complete index has seen every object in the store, so a miss here is
	// authoritative — re-sweeping 16384 blocks to rediscover that is exactly
	// the cost this index exists to remove.
	if index_is_complete() {
		log::debug!("[ledgerfs-reader] read_object: NOT FOUND (complete index, no scan)");
		return Err(ReaderError::NotFound);
	}

	// ── Slow path: linear scan ───────────────────────────────────────
	log::debug!(
		"[ledgerfs-reader] read_object: linear scan blocks {}..{} for id={:02x}{:02x}{:02x}{:02x}...",
		START_BLOCK,
		START_BLOCK + resolved_max_blocks(),
		object_id.as_bytes()[0],
		object_id.as_bytes()[1],
		object_id.as_bytes()[2],
		object_id.as_bytes()[3],
	);
	let mut saw_io_error = false;
	let mut blocks_with_magic = 0u64;
	// Every object this sweep passes over, so the next lookup need not repeat
	// it. Sized up front: one entry per block is the ceiling.
	let mut found_entries: Vec<(u64, u64)> = Vec::new();

	for block_idx in START_BLOCK..START_BLOCK + resolved_max_blocks() {
		let mut block_buf = [0u8; BLOCK_SIZE];

		let block_sector = (partition_start_lba as u64 + block_idx * SECTORS_PER_BLOCK as u64);

		// SAFETY: Caller guarantees partition_start_lba is valid and ATA is accessible.
		match unsafe { read_block_decrypted(partition_start_lba, block_idx, &mut block_buf) } {
			Ok(()) => {}
			Err(_) => {
				saw_io_error = true;
				continue;
			}
		}

		if block_buf.len() < HEADER_SIZE {
			continue;
		}

		// Check magic.
		let magic_bytes: [u8; 8] = block_buf[0..8].try_into().unwrap();
		let magic = u64::from_le_bytes(magic_bytes);
		if magic != crate::OBJECT_RECORD_MAGIC {
			continue;
		}

		blocks_with_magic += 1;

		// Record it regardless of whether it is the object being looked for —
		// capturing the sweep is the whole point.
		{
			let seen = &block_buf[OBJECT_ID_OFFSET..OBJECT_ID_OFFSET + 8];
			let prefix = u64::from_le_bytes([
				seen[0], seen[1], seen[2], seen[3], seen[4], seen[5], seen[6], seen[7],
			]);
			found_entries.push((prefix, block_idx));
		}

		// Encrypted records are skipped; compressed ones are decoded below.
		// (This function duplicates `try_read_block`'s header parsing rather
		// than calling it, so the same handling has to exist twice — worth
		// collapsing, but not while changing what it does.)
		let compression_algo = block_buf[COMPRESSION_OFFSET];
		if block_buf[COMPRESSION_OFFSET + 1] != 0 {
			continue;
		}

		// Read stored payload length.
		let stored_len_bytes: [u8; 4] = block_buf[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].try_into().unwrap();
		let stored_len = u32::from_le_bytes(stored_len_bytes) as usize;

		if stored_len > resolved_max_object_bytes() {
			continue;
		}

		// Match ObjectId.
		let stored_object_id = &block_buf[OBJECT_ID_OFFSET..OBJECT_ID_OFFSET + 32];
		if stored_object_id != object_id.as_bytes() {
			continue;
		}

		// ── Assemble payload (single-block or multi-block) ───────────
		let total_record_len = HEADER_SIZE + stored_len;

		let payload: Vec<u8> = if total_record_len <= BLOCK_SIZE {
			// Single-block: payload is entirely within this block.
			let payload_end = HEADER_SIZE + stored_len;
			if payload_end > block_buf.len() {
				continue;
			}
			block_buf[HEADER_SIZE..payload_end].to_vec()
		} else {
			// Multi-block: read consecutive continuation blocks.
			let extra_blocks = (total_record_len - BLOCK_SIZE + BLOCK_SIZE - 1) / BLOCK_SIZE;
			log::debug!(
				"[ledgerfs-reader] read_object: scan block {} is multi-block (stored_len={}, {} extra blocks)",
				block_idx,
				stored_len,
				extra_blocks,
			);
			let mut full_payload = Vec::with_capacity(stored_len);
			full_payload.extend_from_slice(&block_buf[HEADER_SIZE..]);

			let mut read_ok = true;
			for i in 1..=(extra_blocks as u64) {
				let next_block = block_idx + i;
				let next_sector = (partition_start_lba as u64 + next_block * SECTORS_PER_BLOCK as u64);
				let mut extra_buf = [0u8; BLOCK_SIZE];
				// SAFETY: Caller guarantees partition_start_lba is valid.
				match unsafe { read_block_decrypted(partition_start_lba, next_block, &mut extra_buf) } {
					Ok(()) => {
						let remaining = stored_len - full_payload.len();
						let copy_len = remaining.min(BLOCK_SIZE);
						full_payload.extend_from_slice(&extra_buf[..copy_len]);
					}
					Err(_) => {
						saw_io_error = true;
						read_ok = false;
						break;
					}
				}
			}
			if !read_ok || full_payload.len() < stored_len {
				continue;
			}
			full_payload.truncate(stored_len);
			full_payload
		};

		// Decompress before hashing: the ObjectId is over the logical bytes.
		let payload = if compression_algo != 0 {
			let logical_len_bytes: [u8; 4] =
				block_buf[LOGICAL_LEN_OFFSET..LOGICAL_LEN_OFFSET + 4].try_into().unwrap();
			let logical_len = u32::from_le_bytes(logical_len_bytes) as usize;
			if logical_len > resolved_max_object_bytes() {
				continue;
			}
			let Ok(algo) = crate::compression::CompressionAlgorithm::from_u8(compression_algo) else {
				continue;
			};
			match crate::compression::decompress(&payload, algo, logical_len) {
				Ok(d) => d,
				Err(_) => continue,
			}
		} else {
			payload
		};

		let computed_hash = blake3::hash(&payload);
		if computed_hash.as_bytes() != object_id.as_bytes() {
			log::error!(
				"[ledgerfs-reader] read_object: scan found ID match at block {} but BLAKE3 mismatch",
				block_idx,
			);
			return Err(ReaderError::ChecksumMismatch);
		}

		log::debug!(
			"[ledgerfs-reader] read_object: SCAN HIT at block {} (scanned {} blocks with magic)",
			block_idx,
			blocks_with_magic
		);
		// Partial: this sweep stopped here, so it has not seen the whole store
		// and cannot answer a future miss authoritatively.
		index_install(found_entries, false);
		return Ok(payload);
	}

	log::error!(
		"[ledgerfs-reader] read_object: NOT FOUND after scanning {} blocks ({} had valid magic, io_errors={})",
		resolved_max_blocks(),
		blocks_with_magic,
		saw_io_error,
	);

	// The sweep ran to the end, so every readable object is now indexed.
	// Only claim completeness if no block failed to read: an I/O error means a
	// block was skipped, and an object could be hiding there.
	index_install(found_entries, !saw_io_error);

	if saw_io_error {
		Err(ReaderError::Io)
	} else {
		Err(ReaderError::NotFound)
	}
}

// ── Decode helpers (postcard canonical) ──────────────────────────────

fn decode_inode(data: &[u8]) -> Result<Inode, ReaderError> {
	postcard::from_bytes(data).map_err(|_| ReaderError::InvalidObject)
}

fn decode_dir_node(data: &[u8]) -> Result<DirNode, ReaderError> {
	postcard::from_bytes(data).map_err(|_| ReaderError::InvalidObject)
}

fn decode_extent_node(data: &[u8]) -> Result<ExtentNode, ReaderError> {
	postcard::from_bytes(data).map_err(|_| ReaderError::InvalidObject)
}

// ── Path traversal ───────────────────────────────────────────────────

/// Walk an absolute path (`"/a/b/c"`) from the given root inode reference,
/// returning the resolved `Inode` **and** the `ObjectRef` that was used to
/// locate it on disk.
///
/// The returned `ObjectRef` contains the storage hint (block number) for the
/// inode itself, which callers should cache (e.g. in `FileDescriptor::LedgerFs`)
/// so that subsequent `read_range` calls can skip the linear scan.
///
/// Each path component is looked up in the corresponding `DirNode`.
/// The root itself is returned when the path is `"/"` or empty.
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` is valid.
pub unsafe fn walk_path(
	partition_start_lba: u32,
	root_inode_ref: &ObjectRef,
	path: &str,
) -> Result<(Inode, ObjectRef), ReaderError> {
	let trimmed = path.trim_start_matches('/');
	let components: alloc::vec::Vec<&str> = trimmed.split('/').filter(|c| !c.is_empty()).collect();

	/// Convert a `StorageHint` to an `Option<u64>` suitable for `read_object`.
	fn hint_opt(h: &crate::object::StorageHint) -> Option<u64> {
		if h.is_zero() { None } else { Some(h.0) }
	}

	if components.is_empty() {
		// SAFETY: Caller guarantees partition_start_lba is valid.
		let buf = unsafe {
			read_object(
				partition_start_lba,
				&root_inode_ref.object_id,
				hint_opt(&root_inode_ref.hint),
			)
		}?;
		let inode = decode_inode(&buf)?;
		return Ok((inode, *root_inode_ref));
	}

	let mut current_ref = *root_inode_ref;

	log::debug!(
		"[ledgerfs-reader] walk_path: '{}' ({} components) root_id={:02x}{:02x}{:02x}{:02x}.. root_hint={}",
		path,
		components.len(),
		root_inode_ref.object_id.as_bytes()[0],
		root_inode_ref.object_id.as_bytes()[1],
		root_inode_ref.object_id.as_bytes()[2],
		root_inode_ref.object_id.as_bytes()[3],
		root_inode_ref.hint.0,
	);

	for component in components.iter() {
		// SAFETY: Caller guarantees partition_start_lba is valid.
		let inode_bytes =
			unsafe { read_object(partition_start_lba, &current_ref.object_id, hint_opt(&current_ref.hint)) }?;
		let inode = decode_inode(&inode_bytes)?;

		if inode.kind != InodeKind::Directory {
			return Err(ReaderError::TypeMismatch);
		}

		// SAFETY: Caller guarantees partition_start_lba is valid.
		let dir_bytes = unsafe {
			read_object(
				partition_start_lba,
				&inode.content_ref.object_id,
				hint_opt(&inode.content_ref.hint),
			)
		}?;
		let dir = decode_dir_node(&dir_bytes)?;

		log::debug!(
			"[ledgerfs-reader] walk_path: component '{}' — dir has {} entries",
			component,
			dir.entries.len(),
		);

		let Some(entry) = dir.entries.iter().find(|e| e.name.as_str() == *component) else {
			log::warn!(
				"[ledgerfs-reader] walk_path: component '{}' NOT FOUND in dir (entries: {:?})",
				component,
				dir.entries.iter().map(|e| e.name.as_str()).collect::<alloc::vec::Vec<_>>(),
			);
			return Err(ReaderError::PathNotFound);
		};

		let entry_ref = entry.inode_ref();
		log::debug!(
			"[ledgerfs-reader] walk_path: '{}' → inode_id={:02x}{:02x}{:02x}{:02x}.. hint={}",
			component,
			entry_ref.object_id.as_bytes()[0],
			entry_ref.object_id.as_bytes()[1],
			entry_ref.object_id.as_bytes()[2],
			entry_ref.object_id.as_bytes()[3],
			entry_ref.hint.0,
		);
		current_ref = entry_ref;
	}

	// SAFETY: Caller guarantees partition_start_lba is valid.
	log::debug!(
		"[ledgerfs-reader] walk_path: reading final inode id={:02x}{:02x}{:02x}{:02x}.. hint={}",
		current_ref.object_id.as_bytes()[0],
		current_ref.object_id.as_bytes()[1],
		current_ref.object_id.as_bytes()[2],
		current_ref.object_id.as_bytes()[3],
		current_ref.hint.0,
	);
	let final_bytes = unsafe { read_object(partition_start_lba, &current_ref.object_id, hint_opt(&current_ref.hint)) }?;
	let inode = decode_inode(&final_bytes)?;
	log::debug!(
		"[ledgerfs-reader] walk_path: final inode OK — kind={:?} size={} obj_id={:02x}{:02x}{:02x}{:02x}...",
		inode.kind,
		inode.size,
		inode.object_id.as_bytes()[0],
		inode.object_id.as_bytes()[1],
		inode.object_id.as_bytes()[2],
		inode.object_id.as_bytes()[3],
	);
	Ok((inode, current_ref))
}

/// One resolved entry from [`read_dir_entries`] — a directory child's name
/// plus whether it is itself a directory (resolved from the child's own
/// `Inode.kind`, not inferred from its contents).
#[derive(Debug, Clone)]
pub struct DirListEntry {
	/// Entry name (not a full path).
	pub name: heapless::String<256>,
	/// `true` if the child inode's kind is `Directory`.
	pub is_directory: bool,
}

/// List the direct children of a directory, given the `ObjectRef` of its
/// **already-resolved** directory inode (e.g. from [`walk_path`] or the
/// kernel bridge's `FileMeta`).
///
/// Unlike [`walk_path`], which only ever needs to know whether an
/// intermediate component is a directory, this resolves every child's own
/// inode too, so the caller can report `d` vs `f` per entry the same way
/// `readdir(3)` callers expect — without that, every LedgerFS-backed
/// directory listing has to fall back to a filesystem with no knowledge of
/// this tree at all (see `forge/src/syscall/handlers.rs`'s `/system/`
/// dispatch comment for why that fallback existed and what it broke).
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` is valid.
pub unsafe fn read_dir_entries(
	partition_start_lba: u32,
	dir_inode_ref: &ObjectRef,
) -> Result<Vec<DirListEntry>, ReaderError> {
	fn hint_opt(h: &crate::object::StorageHint) -> Option<u64> {
		if h.is_zero() { None } else { Some(h.0) }
	}

	// SAFETY: caller guarantees partition_start_lba is valid.
	let inode_bytes =
		unsafe { read_object(partition_start_lba, &dir_inode_ref.object_id, hint_opt(&dir_inode_ref.hint)) }?;
	let inode = decode_inode(&inode_bytes)?;
	if inode.kind != InodeKind::Directory {
		return Err(ReaderError::TypeMismatch);
	}

	// SAFETY: caller guarantees partition_start_lba is valid.
	let dir_bytes = unsafe {
		read_object(
			partition_start_lba,
			&inode.content_ref.object_id,
			hint_opt(&inode.content_ref.hint),
		)
	}?;
	let dir = decode_dir_node(&dir_bytes)?;

	let mut out = Vec::with_capacity(dir.entries.len());
	for entry in &dir.entries {
		let entry_ref = entry.inode_ref();
		// SAFETY: caller guarantees partition_start_lba is valid.
		let child_bytes =
			unsafe { read_object(partition_start_lba, &entry_ref.object_id, hint_opt(&entry_ref.hint)) }?;
		let child_inode = decode_inode(&child_bytes)?;
		out.push(DirListEntry {
			name: entry.name.clone(),
			is_directory: child_inode.kind == InodeKind::Directory,
		});
	}
	Ok(out)
}

// ── Whole-file read ──────────────────────────────────────────────────

/// Read a complete file into memory (bounded by `MAX_RANGE_READ`).
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` is valid.
pub unsafe fn read_file(partition_start_lba: u32, file_inode_ref: &ObjectRef) -> Result<Vec<u8>, ReaderError> {
	/// Convert a `StorageHint` to an `Option<u64>` suitable for `read_object`.
	fn hint_opt(h: &crate::object::StorageHint) -> Option<u64> {
		if h.is_zero() { None } else { Some(h.0) }
	}

	// SAFETY: Caller guarantees partition_start_lba is valid.
	let inode_bytes = unsafe {
		read_object(
			partition_start_lba,
			&file_inode_ref.object_id,
			hint_opt(&file_inode_ref.hint),
		)
	}?;
	let inode = decode_inode(&inode_bytes)?;

	if inode.kind != InodeKind::RegularFile {
		return Err(ReaderError::TypeMismatch);
	}

	if inode.size as usize > resolved_max_range_read() {
		return Err(ReaderError::FileTooLarge);
	}

	// SAFETY: Caller guarantees partition_start_lba is valid.
	let extent_bytes = unsafe {
		read_object(
			partition_start_lba,
			&inode.content_ref.object_id,
			hint_opt(&inode.content_ref.hint),
		)
	}?;
	let extent_node = decode_extent_node(&extent_bytes)?;

	let mut file_data = Vec::new();
	file_data.resize(inode.size as usize, 0);

	for extent in extent_node.extents.iter() {
		// SAFETY: Caller guarantees partition_start_lba is valid.
		let chunk_hint = hint_opt(&extent.chunk_hint);
		let chunk_bytes = unsafe { read_object(partition_start_lba, &extent.chunk_id, chunk_hint) }?;
		let chunk_len = min(extent.length as usize, chunk_bytes.len());
		let start = extent.logical_offset as usize;
		let end = start + chunk_len;
		if end > file_data.len() {
			return Err(ReaderError::FileTooLarge);
		}
		let chunk_offset = extent.chunk_offset as usize;
		let chunk_slice_end = min(chunk_offset + chunk_len, chunk_bytes.len());
		file_data[start..end].copy_from_slice(&chunk_bytes[chunk_offset..chunk_slice_end]);
	}

	Ok(file_data)
}

// ── Range read ───────────────────────────────────────────────────────

/// Convert a `StorageHint` to an `Option<u64>` suitable for `read_object`.
fn hint_opt(h: &crate::object::StorageHint) -> Option<u64> {
	if h.is_zero() { None } else { Some(h.0) }
}

/// Decoded, cacheable metadata for a file's extent map.
///
/// Fetching and decoding the inode + root extent-node requires two ATA
/// reads plus BLAKE3 verification each.  Callers that issue many small
/// `read_range` calls against the same file (e.g. a userspace `read()` loop
/// with a small stack buffer) should fetch this once via [`load_extent_map`]
/// and reuse it across calls with [`read_range_cached`] instead of paying
/// that cost on every call.
#[derive(Debug, Clone)]
pub struct ExtentMap {
	/// Total file size in bytes, from the inode.
	pub file_size: u64,
	/// Extent entries mapping logical offsets to on-disk chunks.
	pub extents: Vec<ExtentDescriptor>,
}

/// Fetch and decode the inode and root extent-node for `file_inode_ref`.
///
/// The result can be cached by the caller (e.g. per file descriptor) and
/// reused across multiple [`read_range_cached`] calls to avoid re-reading
/// the same metadata objects from disk.
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` is valid.
pub unsafe fn load_extent_map(partition_start_lba: u32, file_inode_ref: &ObjectRef) -> Result<ExtentMap, ReaderError> {
	log::debug!(
		"[ledgerfs-reader] load_extent_map: inode_id={:02x}{:02x}{:02x}{:02x}.. hint={}",
		file_inode_ref.object_id.as_bytes()[0],
		file_inode_ref.object_id.as_bytes()[1],
		file_inode_ref.object_id.as_bytes()[2],
		file_inode_ref.object_id.as_bytes()[3],
		file_inode_ref.hint.0,
	);

	// SAFETY: Caller guarantees partition_start_lba is valid.
	let inode_bytes = unsafe {
		read_object(
			partition_start_lba,
			&file_inode_ref.object_id,
			hint_opt(&file_inode_ref.hint),
		)
	}
	.map_err(|e| {
		log::error!("[ledgerfs-reader] load_extent_map: FAILED to read inode: {:?}", e);
		e
	})?;
	let inode = decode_inode(&inode_bytes)?;

	log::debug!(
		"[ledgerfs-reader] load_extent_map: inode decoded — kind={:?} size={} content_ref.id={:02x}{:02x}{:02x}{:02x}.. content_ref.hint={}",
		inode.kind,
		inode.size,
		inode.content_ref.object_id.as_bytes()[0],
		inode.content_ref.object_id.as_bytes()[1],
		inode.content_ref.object_id.as_bytes()[2],
		inode.content_ref.object_id.as_bytes()[3],
		inode.content_ref.hint.0,
	);

	if inode.kind != InodeKind::RegularFile {
		return Err(ReaderError::TypeMismatch);
	}

	// SAFETY: Caller guarantees partition_start_lba is valid.
	let extent_bytes = unsafe {
		read_object(
			partition_start_lba,
			&inode.content_ref.object_id,
			hint_opt(&inode.content_ref.hint),
		)
	}
	.map_err(|e| {
		log::error!("[ledgerfs-reader] load_extent_map: FAILED to read extent node: {:?}", e);
		e
	})?;
	let extent_node = decode_extent_node(&extent_bytes)?;

	Ok(ExtentMap { file_size: inode.size, extents: extent_node.extents })
}

/// Read a byte range from a file using a previously loaded [`ExtentMap`].
///
/// Only the extents that overlap `[offset, offset+buf.len())` are fetched
/// from disk — the inode and extent-node are not re-read, making this
/// cheap to call repeatedly for the same file.
///
/// Returns the number of bytes copied into `buf` (0 means EOF or
/// `offset` past end-of-file).
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` is valid.
pub unsafe fn read_range_cached(
	partition_start_lba: u32,
	map: &ExtentMap,
	offset: u64,
	buf: &mut [u8],
) -> Result<usize, ReaderError> {
	if buf.is_empty() {
		return Ok(0);
	}
	if buf.len() > resolved_max_range_read() {
		return Err(ReaderError::FileTooLarge);
	}
	if offset >= map.file_size {
		return Ok(0);
	}

	let mut total_read = 0usize;
	let mut remaining = buf.len();
	let req_end = (offset + buf.len() as u64).min(map.file_size);

	for extent in map.extents.iter() {
		if remaining == 0 {
			break;
		}

		if !range_intersects(extent.logical_offset, extent.length, offset, req_end - offset) {
			continue;
		}

		let extent_start = extent.logical_offset;
		let extent_end = extent.logical_offset + extent.length;
		let read_start = offset.max(extent_start);
		let read_end = req_end.min(extent_end);

		if read_start >= read_end {
			continue;
		}

		let copy_len = (read_end - read_start) as usize;
		// SAFETY: Caller guarantees partition_start_lba is valid.
		let chunk_hint = hint_opt(&extent.chunk_hint);
		let chunk_bytes = unsafe { read_object(partition_start_lba, &extent.chunk_id, chunk_hint) }?;
		let chunk_offset = (read_start - extent.logical_offset + extent.chunk_offset) as usize;
		let chunk_end = min(chunk_offset + copy_len, chunk_bytes.len());

		let source = &chunk_bytes[chunk_offset..chunk_end];
		// The destination position in `buf` must be derived from the
		// extent's own logical position relative to `offset` — NOT from
		// `total_read` (a running sum of bytes copied so far). Those
		// usually coincide for a gapless, in-order extent list, but
		// "usually" silently produces a corrupted (shifted) buffer instead
		// of a length mismatch whenever they don't — e.g. the first
		// intersecting extent overlapping less than expected. A ~11 MiB
		// file with ~2800 extents (the Alpine kernel image) hit exactly
		// this: bytes ended up shifted by a small, fixed number of bytes
		// partway through the buffer, corrupting the loaded kernel image
		// without any read ever reporting a short length.
		let dest_start = (read_start - offset) as usize;
		let dest_end = dest_start + source.len();
		buf[dest_start..dest_end].copy_from_slice(source);

		total_read = total_read.max(dest_end);
		remaining = buf.len().saturating_sub(total_read);
	}

	Ok(total_read)
}

/// Read a byte range from a file without loading the entire file.
///
/// Returns the number of bytes copied into `buf` (0 means EOF or
/// `offset` past end-of-file).
///
/// Only the extents that overlap `[offset, offset+buf.len())` are fetched
/// from disk, making this efficient for partial reads of large files.
///
/// This re-fetches and decodes the inode and extent-node on every call.
/// Callers issuing many reads against the same file should use
/// [`load_extent_map`] + [`read_range_cached`] instead.
///
/// # Safety
///
/// Caller must ensure `partition_start_lba` is valid.
pub unsafe fn read_range(
	partition_start_lba: u32,
	file_inode_ref: &ObjectRef,
	offset: u64,
	buf: &mut [u8],
) -> Result<usize, ReaderError> {
	if buf.is_empty() {
		return Ok(0);
	}
	if buf.len() > resolved_max_range_read() {
		return Err(ReaderError::FileTooLarge);
	}

	// SAFETY: Caller guarantees partition_start_lba is valid.
	let map = unsafe { load_extent_map(partition_start_lba, file_inode_ref) }?;

	// SAFETY: Caller guarantees partition_start_lba is valid.
	unsafe { read_range_cached(partition_start_lba, &map, offset, buf) }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Check whether two byte ranges overlap.
fn range_intersects(base: u64, len: u64, req_start: u64, req_len: u64) -> bool {
	let base_end = base.saturating_add(len);
	let req_end = req_start.saturating_add(req_len);
	!(req_end <= base || req_start >= base_end)
}

// region:    --- Tests

#[cfg(test)]
mod object_index_tests {
	use super::*;

	/// `OBJECT_INDEX` is one global shared by the whole crate, and the test
	/// harness runs tests in parallel — without this every test below would be
	/// racing the others for it. Held for the body of each test that touches
	/// the global; the pure `merge_by_block` tests do not need it.
	static INDEX_TEST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

	/// Take the serialising lock and reset the shared index, so each test
	/// starts from a known state.
	fn exclusive() -> spin::MutexGuard<'static, ()> {
		let guard = INDEX_TEST_LOCK.lock();
		*OBJECT_INDEX.lock() = None;
		guard
	}

	#[test]
	fn merge_by_block_unions_two_ordered_lists() {
		// -- Setup & Fixtures
		let old = alloc::vec![(0xAA, 10u64), (0xBB, 20), (0xCC, 30)];
		let new = alloc::vec![(0xDD, 15u64), (0xEE, 25)];

		// -- Exec
		let merged = merge_by_block(old, new);

		// -- Check: still ascending by block, nothing dropped.
		assert_eq!(merged, alloc::vec![(0xAA, 10), (0xDD, 15), (0xBB, 20), (0xEE, 25), (0xCC, 30)]);
	}

	#[test]
	fn merge_by_block_lets_the_newer_read_win_a_tie() {
		// -- Setup & Fixtures: block 20 was rewritten, so it now holds a
		// different object.
		let old = alloc::vec![(0xBB, 20u64)];
		let new = alloc::vec![(0x99, 20u64)];

		// -- Exec
		let merged = merge_by_block(old, new);

		// -- Check
		assert_eq!(merged, alloc::vec![(0x99, 20)]);
	}

	#[test]
	fn an_early_exiting_sweep_does_not_shrink_the_index() {
		// -- Setup & Fixtures: a full sweep, then a write clears `complete`
		// (which is what lets a later partial sweep install at all).
		let _serialised = exclusive();
		index_install(alloc::vec![(1, 10), (2, 20), (3, 30), (4, 40)], true);
		invalidate_object_index();
		assert!(!index_is_complete(), "a write must drop the completeness claim");

		// -- Exec: a lookup hits at block 20 and stops there, so its sweep only
		// covers blocks up to 20.
		index_install(alloc::vec![(1, 10), (2, 20)], false);

		// -- Check: the tail from the earlier full sweep survives. Replacing
		// outright here sent every object past block 20 back to scanning.
		let ix = OBJECT_INDEX.lock();
		let entries = &ix.as_ref().expect("index present").entries;
		assert_eq!(entries.len(), 4, "entries were shrunk to the partial sweep: {:?}", entries);
		assert!(entries.contains(&(4, 40)), "lost the entry beyond the partial sweep's reach");
	}

	#[test]
	fn a_write_keeps_entries_but_drops_completeness() {
		// -- Setup & Fixtures
		let _serialised = exclusive();
		index_install(alloc::vec![(7, 70), (8, 80)], true);

		// -- Exec
		invalidate_object_index();

		// -- Check: entries survive (they are re-verified on use, so a stale
		// one costs a wasted read, never wrong data), but a miss is no longer
		// authoritative.
		assert!(!index_is_complete());
		let ix = OBJECT_INDEX.lock();
		assert_eq!(ix.as_ref().expect("index present").entries.len(), 2);
	}

	#[test]
	fn a_partial_sweep_never_downgrades_a_complete_index() {
		// -- Setup & Fixtures
		let _serialised = exclusive();
		index_install(alloc::vec![(1, 10), (2, 20)], true);

		// -- Exec
		index_install(alloc::vec![(1, 10)], false);

		// -- Check
		assert!(index_is_complete(), "completeness must survive an early-exiting sweep");
	}

	#[test]
	fn candidates_match_on_the_id_prefix() {
		// -- Setup & Fixtures: two ids sharing their first 8 bytes must both
		// come back, so the caller can verify each in turn.
		let _serialised = exclusive();
		let mut bytes = [0u8; 32];
		bytes[..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
		let id = ObjectId::from_bytes(&bytes);
		let want = id_prefix(&id);
		index_install(alloc::vec![(want, 11), (want.wrapping_add(1), 12), (want, 13)], true);

		// -- Exec
		let cands = index_candidates(&id);

		// -- Check
		assert_eq!(cands, alloc::vec![11, 13]);
	}
}

// endregion: --- Tests
