//! Shared low-level ATA PIO primitives (ports, registers, wait helpers, the
//! crate-level bus lock) used by both the read path (`reader.rs`) and the
//! write path (`writer.rs`).
//!
//! # Why this exists as its own module
//!
//! ATA PIO is a single shared bus: only one command sequence (register
//! setup → command → per-sector data transfer) may be in flight at a time,
//! enforced by [`ATA_LOCK`]. Reads and writes MUST serialize through the
//! *same* lock — two independent locks guarding the same physical bus would
//! let a read and a write interleave their port I/O on the wire, corrupting
//! both. Before this module existed, `reader.rs` owned a private copy of
//! this lock and all these primitives; adding a write path required either
//! duplicating them (the correctness hazard above) or factoring them out
//! (this module).
//!
//! # Safety
//!
//! Every function here is `unsafe` because it performs raw x86 port I/O.
//! Callers must ensure ring 0 execution with I/O port access enabled and
//! (for anything beyond the bare `outb`/`inb`/`inw`/`outw` primitives) that
//! `ATA_LOCK` is held for the duration of a whole command sequence.

// region: --- Constants

/// Primary ATA bus base port.
pub(crate) const ATA_PRIMARY_BASE: u16 = 0x1F0;

/// ATA register offsets.
pub(crate) const ATA_DATA: u16 = 0;
pub(crate) const ATA_SECTOR_COUNT: u16 = 2;
pub(crate) const ATA_LBA_LOW: u16 = 3;
pub(crate) const ATA_LBA_MID: u16 = 4;
pub(crate) const ATA_LBA_HIGH: u16 = 5;
pub(crate) const ATA_DRIVE_HEAD: u16 = 6;
pub(crate) const ATA_STATUS_CMD: u16 = 7;

/// ATA status bits.
pub(crate) const ATA_STATUS_BSY: u8 = 0x80;
pub(crate) const ATA_STATUS_DRQ: u8 = 0x08;
pub(crate) const ATA_STATUS_ERR: u8 = 0x01;

/// Sector size (bytes).
pub(crate) const SECTOR_SIZE: usize = 512;

/// Maximum 28-bit LBA value.
pub(crate) const MAX_28BIT_LBA: u32 = 0x0FFF_FFFF;

/// Highest LBA the 48-bit commands can address (2^48 - 1).
pub(crate) const MAX_48BIT_LBA: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Program the ATA taskfile for `lba`/`sector_count`, choosing the 28- or
/// 48-bit form, and return the command byte the caller must issue.
///
/// 28-bit addressing tops out at 128 GiB (`MAX_28BIT_LBA` sectors of 512 B),
/// which is a real ceiling rather than a theoretical one: LedgerFS is the only
/// path to its own partitions, so a disk larger than that had most of itself
/// permanently unreachable. The 28-bit form is still used whenever the request
/// fits, so nothing about existing volumes changes and drives that predate
/// 48-bit addressing keep working.
///
/// The two forms differ in more than width. 28-bit smuggles LBA bits 24-27
/// into the low nibble of the drive/head register; 48-bit writes each of the
/// same four registers *twice*, high-order byte first, and leaves drive/head
/// carrying no address at all.
///
/// # Safety
///
/// Caller holds the ATA bus lock and is running in ring 0 with I/O access.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn program_taskfile(
	lba: u64,
	sector_count: u32,
	cmd_28: u8,
	cmd_48: u8,
) -> Result<u8, AtaError> {
	if sector_count == 0 {
		return Err(AtaError::InvalidBufferSize);
	}
	let end = lba.checked_add(sector_count as u64).ok_or(AtaError::InvalidLba)?;

	// SAFETY: every call below is a raw `out` to the primary ATA taskfile;
	// the caller holds the bus lock for the whole command sequence.
	unsafe {
		// A sector count of 256 is encoded as 0 in the 8-bit field, so it is
		// the largest a 28-bit command can carry; anything above needs the
		// 16-bit field the 48-bit form provides.
		if end <= MAX_28BIT_LBA as u64 && sector_count <= 256 {
			outb(ATA_PRIMARY_BASE + ATA_DRIVE_HEAD, 0xE0 | ((lba >> 24) & 0x0F) as u8);
			outb(ATA_PRIMARY_BASE + ATA_SECTOR_COUNT, sector_count as u8);
			outb(ATA_PRIMARY_BASE + ATA_LBA_LOW, lba as u8);
			outb(ATA_PRIMARY_BASE + ATA_LBA_MID, (lba >> 8) as u8);
			outb(ATA_PRIMARY_BASE + ATA_LBA_HIGH, (lba >> 16) as u8);
			return Ok(cmd_28);
		}

		if end > MAX_48BIT_LBA || sector_count > 65_536 {
			return Err(AtaError::InvalidLba);
		}

		// 0x40 selects the master drive in LBA mode. Unlike the 28-bit form
		// this register carries no address bits.
		outb(ATA_PRIMARY_BASE + ATA_DRIVE_HEAD, 0x40);
		// High-order bytes first...
		outb(ATA_PRIMARY_BASE + ATA_SECTOR_COUNT, (sector_count >> 8) as u8);
		outb(ATA_PRIMARY_BASE + ATA_LBA_LOW, (lba >> 24) as u8);
		outb(ATA_PRIMARY_BASE + ATA_LBA_MID, (lba >> 32) as u8);
		outb(ATA_PRIMARY_BASE + ATA_LBA_HIGH, (lba >> 40) as u8);
		// ...then the low-order bytes into the very same registers. A count of
		// 65536 lands as 0 in both halves, which is exactly how the spec
		// encodes it.
		outb(ATA_PRIMARY_BASE + ATA_SECTOR_COUNT, sector_count as u8);
		outb(ATA_PRIMARY_BASE + ATA_LBA_LOW, lba as u8);
		outb(ATA_PRIMARY_BASE + ATA_LBA_MID, (lba >> 8) as u8);
		outb(ATA_PRIMARY_BASE + ATA_LBA_HIGH, (lba >> 16) as u8);
		Ok(cmd_48)
	}
}

// endregion: --- Constants

// region: --- ATA error type

/// Minimal ATA error (mirrors forge-boot `ata::AtaError`).
#[derive(Debug, Clone, Copy)]
pub enum AtaError {
	/// Drive did not respond within the polling timeout.
	Timeout,
	/// Drive reported an error condition.
	DriveError,
	/// LBA, or the sector count at that LBA, is outside what the drive can
	/// be asked to address.
	InvalidLba,
	/// Buffer length is not a multiple of 512 bytes.
	InvalidBufferSize,
}

// endregion: --- ATA error type

// region: --- Crate-level ATA lock

/// Serializes all ATA PIO accesses across the kernel — reads AND writes.
///
/// Because ATA PIO is a shared bus with a multi-step port I/O protocol,
/// concurrent callers must be prevented from interleaving commands.
/// A `spin::Mutex<()>` is the lightest possible lock that works in
/// `#[no_std]` bare-metal contexts without allocation.
pub(crate) static ATA_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Is the interrupt flag (RFLAGS.IF) currently set on this core?
#[cfg(target_arch = "x86_64")]
fn interrupts_enabled() -> bool {
	let flags: u64;
	// SAFETY: Reads RFLAGS via the architectural push/pop sequence. No
	// memory is accessed beyond the stack slot this instruction pair owns.
	unsafe {
		core::arch::asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
	}
	(flags & (1 << 9)) != 0
}

/// Guard returned by [`lock_ata_bus`]: holds [`ATA_LOCK`] **with interrupts
/// disabled on the current core**, restoring the previous interrupt state on
/// drop.
///
/// Field order is load-bearing: Rust drops fields in declaration order, so
/// the `spin::MutexGuard` releases the lock *before* `Drop for AtaBusGuard`
/// re-enables interrupts. Re-enabling first would reopen the exact
/// preemption window this type exists to close, for the handful of
/// instructions before the unlock.
pub(crate) struct AtaBusGuard {
	_lock: spin::MutexGuard<'static, ()>,
	irq_was_enabled: bool,
}

#[cfg(target_arch = "x86_64")]
impl Drop for AtaBusGuard {
	fn drop(&mut self) {
		if self.irq_was_enabled {
			// SAFETY: Restoring the interrupt flag we ourselves cleared in
			// `lock_ata_bus`; only reached when it was set on entry.
			unsafe {
				core::arch::asm!("sti", options(nomem, nostack));
			}
		}
	}
}

/// Acquire the ATA bus for one whole command sequence.
///
/// **Interrupts are disabled for the lifetime of the returned guard.** This
/// is not incidental hardening — it is required for correctness on a
/// single-core system. [`ATA_LOCK`] is a bare `spin::Mutex`: a waiter busy-
/// spins and never yields. If the timer interrupt preempts a task that is
/// mid-command-sequence (holding the lock) and the scheduler runs a
/// different task that also touches the ATA bus, that second task spins
/// forever on a lock whose holder can never be rescheduled to release it —
/// a hard, unrecoverable deadlock, not a slowdown. That is exactly what a
/// `/data` open was hitting: a `read_object` block read would hang the
/// whole disk path permanently, midway through a path walk whose earlier
/// reads of the *same* blocks had succeeded moments before.
///
/// Clearing IF before taking the lock (not after) closes the same window on
/// the acquisition side. Interrupts stay off only for a single command
/// sequence — one block, not a whole scan — so a linear scan still services
/// interrupts between blocks.
#[cfg(target_arch = "x86_64")]
pub(crate) fn lock_ata_bus() -> AtaBusGuard {
	let irq_was_enabled = interrupts_enabled();
	if irq_was_enabled {
		// SAFETY: Disabling interrupts on this core; paired with the `sti`
		// in `Drop for AtaBusGuard`, which is the only path out of here.
		unsafe {
			core::arch::asm!("cli", options(nomem, nostack));
		}
	}
	AtaBusGuard { _lock: ATA_LOCK.lock(), irq_was_enabled }
}

// endregion: --- Crate-level ATA lock

// region: --- Inline port I/O

// The four scalar accessors, re-exported under their conventional names so the
// call sites below and in `reader.rs`/`writer.rs` read as ATA code rather than
// as port-I/O plumbing. See `arch::x86_port` for the instructions themselves
// and their constraint annotations.

#[cfg(target_arch = "x86_64")]
pub(crate) use crate::arch::x86_port::{inb, inw, outb, outw};

// The bulk string ops keep their local names; `rep insw`/`rep outsw` are what
// make a sector read one VM exit instead of 256.
#[cfg(target_arch = "x86_64")]
pub(crate) use crate::arch::x86_port::rep_insw as insw_bulk;
#[cfg(target_arch = "x86_64")]
pub(crate) use crate::arch::x86_port::rep_outsw as outsw_bulk;

// endregion: --- Inline port I/O

// region: --- ATA PIO helpers

/// Wait for the drive to clear BSY and set DRQ.
///
/// # Safety
///
/// Caller must hold `ATA_LOCK` and ensure the ATA controller is initialised.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn ata_wait_ready() -> Result<(), AtaError> {
	for _ in 0..1_000_000u32 {
		// SAFETY: Reading ATA status port (0x1F7) during PIO.
		let status = unsafe { inb(ATA_PRIMARY_BASE + ATA_STATUS_CMD) };
		if (status & ATA_STATUS_ERR) != 0 {
			return Err(AtaError::DriveError);
		}
		if (status & ATA_STATUS_BSY) == 0 && (status & ATA_STATUS_DRQ) != 0 {
			return Ok(());
		}
	}
	Err(AtaError::Timeout)
}


/// Wait for the drive to clear BSY.
///
/// # Safety
///
/// Caller must hold `ATA_LOCK` and ensure the ATA controller is initialised.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn ata_wait_not_busy() -> Result<(), AtaError> {
	for _ in 0..1_000_000u32 {
		// SAFETY: Reading ATA status port.
		let status = unsafe { inb(ATA_PRIMARY_BASE + ATA_STATUS_CMD) };
		if (status & ATA_STATUS_ERR) != 0 {
			return Err(AtaError::DriveError);
		}
		if (status & ATA_STATUS_BSY) == 0 {
			return Ok(());
		}
	}
	Err(AtaError::Timeout)
}

// endregion: --- ATA PIO helpers
