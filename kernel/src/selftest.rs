//! Live ATA write-path round-trip self-test.
//!
//! `commit.rs`'s unit tests (`RecordingBlockDevice`) prove the *encoding*
//! logic is correct: `commit()` builds the right bytes and calls
//! `write_blocks` with them. They cannot prove the *hardware* protocol
//! itself is correct, since a host-run `cargo test` never executes real
//! `outb`/`inb` port I/O. This module is the live counterpart: write
//! a known pattern via the real [`crate::commit::AtaBlockDevice`], flush,
//! read it back via the real ATA read path, and compare byte-for-byte.
//!
//! # Intended usage
//!
//! Call [`write_read_roundtrip_test`] against a **disposable, scratch-only
//! disk image**, never against a real install image. It overwrites
//! whatever is at the given block unconditionally; it has no way to know
//! whether that block holds real data. A dedicated harness builds a minimal
//! throwaway image and boots it in a virtual machine specifically to run
//! this test.

extern crate alloc;

use alloc::vec;

use crate::Result;
use crate::commit::{AtaBlockDevice, BlockDevice};

/// Outcome of a round-trip test, detailed enough to log a useful PASS/FAIL
/// line without the caller needing to know this module's internals.
#[derive(Debug, Clone, Copy)]
pub struct RoundtripResult {
	/// `true` when the block read back matched the pattern written.
	pub passed: bool,
	/// Index of the first byte that differed (only meaningful if `!passed`).
	pub first_mismatch_offset: Option<usize>,
}

/// Write a deterministic, non-trivial test pattern to `block_idx` (relative
/// to `partition_start_lba`) via [`AtaBlockDevice`], flush, then read the
/// same block back via the real ATA read path and compare.
///
/// # Safety
///
/// Caller must ensure ring 0 execution with I/O port access enabled, that
/// `partition_start_lba` names a real ATA-accessible disk region, and,
/// critically, that `block_idx` is a genuinely disposable scratch block:
/// this function overwrites it unconditionally with no regard for whether
/// real data lives there.
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub unsafe fn write_read_roundtrip_test(partition_start_lba: u32, block_idx: u64) -> Result<RoundtripResult> {
	let block_size = crate::BLOCK_SIZE as usize;

	// A repeating-but-non-trivial pattern: not all-zero (which a buffer
	// that was never actually written could accidentally "pass" against,
	// e.g. reading uninitialised-but-zeroed disk), not all-ones, and not
	// block-position-independent (catches a class of bug where the wrong
	// block gets read back, e.g. an off-by-one in LBA arithmetic, since
	// the pattern still self-verifies but a reader would need to know
	// `block_idx` to reconstruct it exactly; it deliberately does so, via the
	// `block_idx` mix-in below).
	let mut pattern = vec![0u8; block_size];
	for (i, b) in pattern.iter_mut().enumerate() {
		*b = ((i as u64).wrapping_mul(2654435761).wrapping_add(block_idx) % 251) as u8;
	}

	let mut device = AtaBlockDevice::new(partition_start_lba);
	// SAFETY: caller contract above.
	device.write_blocks(block_idx, &pattern)?;
	device.storage_barrier()?;

	// Read back via the real ATA read path (not the write path's own
	// buffer): the whole point is to prove the two independently-coded
	// directions agree on what's actually on the wire/disk.
	let sectors_per_block: u64 = 8; // matches reader.rs / writer.rs convention (4096 / 512)
	let abs_sector_u64 = partition_start_lba as u64 + block_idx * sectors_per_block;
	// Stays u64: the PIO helpers select the 28- or 48-bit command form
	// themselves, so narrowing here would reimpose the 128 GiB ceiling.
	let abs_sector: u64 = abs_sector_u64;

	let mut readback = vec![0u8; block_size];
	// SAFETY: same contract as write_blocks above.
	unsafe { crate::reader::ata_pio_read(abs_sector, &mut readback) }
		.map_err(|e| crate::Error::InvalidOperation(alloc::format!("readback ATA read failed: {e:?}")))?;

	if readback == pattern {
		Ok(RoundtripResult {
			passed: true,
			first_mismatch_offset: None,
		})
	} else {
		let first_mismatch = readback.iter().zip(pattern.iter()).position(|(a, b)| a != b);
		Ok(RoundtripResult {
			passed: false,
			first_mismatch_offset: first_mismatch,
		})
	}
}
