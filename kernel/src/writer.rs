//! LedgerFS shared writer: kernel-mode write access to on-disk objects.
//!
//! This is the write-side counterpart to `reader.rs`: the same 28-bit LBA
//! ATA PIO protocol, opposite data direction, sharing the exact same
//! `crate::ata_io::ATA_LOCK` bus lock (see `ata_io`'s module doc for why a
//! second, independent lock here would be a real correctness hazard, not
//! just a style choice).
//!
//! # Why this didn't exist until now
//!
//! `reader.rs`'s own module doc describes itself as "read-only access":
//! that wasn't an arbitrary restriction, it reflected that nothing in this
//! codebase ever needed to *write* a LedgerFS block from kernel/boot code.
//! `commit.rs`'s `BlockDevice` trait existed as the intended hook point
//! ("Currently stubs, no real block I/O") but had no real
//! implementation; `NullBlockDevice` was the only one, a deliberate no-op
//! for testing the commit *protocol* (barrier ordering, intent log
//! sequencing) without needing real disk I/O.
//!
//! # Safety
//!
//! Every function here is `unsafe` for the same reasons as `reader.rs`:
//! raw x86 port I/O, ring 0 execution required, caller must guarantee the
//! partition LBA arguments point to a valid LedgerFS partition.

extern crate alloc;

use crate::ata_io::{
	ATA_DATA, ATA_PRIMARY_BASE, ATA_STATUS_CMD, AtaError, SECTOR_SIZE, ata_wait_not_busy, ata_wait_ready, outb,
};

/// WRITE SECTORS command (28-bit LBA PIO).
const ATA_CMD_WRITE_SECTORS: u8 = 0x30;

/// WRITE SECTORS EXT: the 48-bit LBA counterpart of `ATA_CMD_WRITE_SECTORS`.
const ATA_CMD_WRITE_SECTORS_EXT: u8 = 0x34;

/// FLUSH CACHE command: forces the drive to write its internal write
/// cache out to stable media. This is what backs `BlockDevice::
/// storage_barrier()`: without it, a `write_blocks` call can return
/// success while the data is still sitting in the drive's volatile cache,
/// which defeats the entire point of the commit protocol's barrier
/// ordering (§15.1/§15.3): a power loss between "write returned Ok" and
/// "drive actually flushed its cache" would silently lose data the commit
/// protocol believed was already durable.
const ATA_CMD_FLUSH_CACHE: u8 = 0xE7;

/// Write sectors to disk using PIO, in 28- or 48-bit LBA form as needed.
///
/// **This function acquires `ATA_LOCK` internally.** Callers do not need
/// (and must not already hold) the lock; mirrors `reader::ata_pio_read`
/// exactly, opposite data direction.
///
/// # Arguments
///
/// * `lba`: Starting Logical Block Address. Addresses past the 28-bit
///   range automatically use the 48-bit commands.
/// * `buf`: Input buffer; length must be a multiple of 512 bytes.
///
/// # Safety
///
/// Caller must ensure:
/// - Ring 0 execution with I/O port access enabled.
/// - The ATA primary bus is present and initialised.
/// - `lba` + sector_count does not exceed drive capacity.
/// - `lba` genuinely belongs to this process's LedgerFS partition; this
///   function has no knowledge of partition boundaries and will happily
///   overwrite whatever is at the given LBA.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn ata_pio_write(lba: u64, buf: &[u8]) -> Result<(), AtaError> {
	if !buf.len().is_multiple_of(SECTOR_SIZE) {
		return Err(AtaError::InvalidBufferSize);
	}

	// A write can add an object the reader's block index has never seen, so
	// the index can no longer answer a miss authoritatively. Marked here
	// rather than at each call site because this is the single choke point
	// every write funnels through: there is no way for a caller to modify
	// the store while bypassing it.
	//
	// A stale *entry* is harmless on its own (`try_read_block` re-verifies the
	// full 32-byte object id and falls through on mismatch); what must not
	// survive a write is the `complete` flag, which lets a lookup answer
	// NotFound without scanning and would therefore hide a newly written
	// object.
	crate::reader::invalidate_object_index();

	let sector_count = (buf.len() / SECTOR_SIZE) as u32;

	// Acquire the crate-level ATA lock for the duration of the command;
	// the same lock `ata_pio_read` uses, so a read can never interleave
	// its port I/O with this write's command sequence.
	let _guard = crate::ata_io::lock_ata_bus();

	// SAFETY: Lock held; ATA controller initialised per caller contract.
	// All port I/O and helper calls below are unsafe because they perform
	// raw x86 `in`/`out` instructions. The lock ensures serialisation.
	unsafe {
		ata_wait_not_busy()?;

		// SAFETY: bus lock held; picks the 28- or 48-bit form and programs
		// the taskfile accordingly.
		let cmd = crate::ata_io::program_taskfile(lba, sector_count, ATA_CMD_WRITE_SECTORS, ATA_CMD_WRITE_SECTORS_EXT)?;

		// SAFETY: Issuing the write command to register 0x1F7.
		outb(ATA_PRIMARY_BASE + ATA_STATUS_CMD, cmd);

		for sector_idx in 0..sector_count {
			// Per the ATA PIO write protocol, the drive asserts DRQ to
			// signal it's ready to *receive* the next sector's data: same
			// wait primitive as the read path, opposite direction of the
			// transfer that follows it.
			ata_wait_ready()?;

			let sector_offset = (sector_idx as usize) * SECTOR_SIZE;
			let sector_buf = &buf[sector_offset..sector_offset + SECTOR_SIZE];

			// SAFETY: DRQ asserted, and `sector_buf` is exactly SECTOR_SIZE
			// bytes, matching the word count passed here.
			crate::ata_io::outsw_bulk(ATA_PRIMARY_BASE + ATA_DATA, sector_buf.as_ptr(), SECTOR_SIZE / 2);
		}

		// The drive processes the last sector's data after the final word
		// is written; wait for BSY to clear so a caller that immediately
		// issues another command (or a cache flush) doesn't race the
		// drive's internal completion of this one.
		ata_wait_not_busy()?;
	}

	Ok(())
}

/// Stub for non-x86_64 targets (compile gate only, never callable).
#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn ata_pio_write(_lba: u32, _buf: &[u8]) -> Result<(), AtaError> {
	Err(AtaError::InvalidLba)
}

/// Issue an ATA FLUSH CACHE command, forcing the drive's write cache out
/// to stable media.
///
/// **This function acquires `ATA_LOCK` internally.**
///
/// # Safety
///
/// Same preconditions as [`ata_pio_write`], minus the buffer/LBA
/// constraints (this command carries no data phase).
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn ata_cache_flush() -> Result<(), AtaError> {
	let _guard = crate::ata_io::lock_ata_bus();

	// SAFETY: Lock held; ATA controller initialised per caller contract.
	unsafe {
		ata_wait_not_busy()?;
		// SAFETY: Issuing FLUSH CACHE (0xE7) to command register 0x1F7. No
		// LBA/sector-count/drive-head setup needed: this command has no
		// data phase, it just asks the drive to flush its internal cache.
		outb(ATA_PRIMARY_BASE + ATA_STATUS_CMD, ATA_CMD_FLUSH_CACHE);
		// A flush can legitimately take longer than a single sector
		// transfer (it's waiting on physical media, not a port handshake),
		// but the polling budget in `ata_wait_not_busy` is a fixed
		// iteration count, not wall-clock time: same limitation the read
		// path already accepts for its own waits.
		ata_wait_not_busy()?;
	}

	Ok(())
}

/// Stub for non-x86_64 targets (compile gate only, never callable).
#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn ata_cache_flush() -> Result<(), AtaError> {
	Err(AtaError::InvalidLba)
}
