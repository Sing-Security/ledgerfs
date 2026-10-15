//! The four x86 port-I/O instructions LedgerFS issues, and nothing else.
//!
//! # Why these live here rather than in a dependency
//!
//! `reader.rs` and `writer.rs` drive an ATA controller through programmed I/O:
//! two register accessors (`inb`, `outb`) and the two bulk string transfers
//! (`rep insw`, `rep outsw`) that make a 512-byte sector one instruction
//! instead of 256. Those four are the crate's entire hardware surface, and a
//! filesystem that owns an on-disk format should not take a dependency on its
//! host OS's assembly crate to reach them — the dependency would be the only
//! thing tying this crate to its host OS.
//!
//! The ATA task-file registers are 8-bit; the only 16-bit access the path needs
//! is a sector's worth of words through the data register, which the two bulk
//! transfers cover. The scalar 16-bit forms (`inw`, `outw`) were dropped rather
//! than carried unused.
//!
//! # The constraint annotations, and why they are not decoration
//!
//! * `nomem` — the instruction neither reads nor writes memory the compiler
//!   models. True for port I/O: the I/O address space is disjoint from memory,
//!   so the compiler may keep values in registers across the instruction. This
//!   is a statement about the compiler's *model*, not about hardware side
//!   effects, which is why the safety docs below still say "know the device".
//! * `nostack` — does not push, pop, or otherwise use the stack.
//! * `preserves_flags` — leaves RFLAGS untouched. True of `in`/`out`.
//!
//! The two bulk transfers **must not** claim `nomem`: unlike the scalar
//! accessors they move through a pointer the compiler does know about, so
//! claiming it would let the compiler move unrelated memory operations across
//! the read or write.

use core::arch::asm;

/// Write a byte to an I/O port.
///
/// # Safety
///
/// Writing an arbitrary port can reconfigure or wedge hardware. The caller
/// must know which device owns `port`.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) unsafe fn outb(port: u16, val: u8) {
	// SAFETY: `out` to a port the caller has identified; `nomem`/`nostack`/
	// `preserves_flags` all hold for it.
	unsafe { asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags)) }
}

/// Read a byte from an I/O port.
///
/// # Safety
///
/// A port read is not always side-effect free — draining a UART receive
/// register consumes the byte, for instance. The caller must know the device.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) unsafe fn inb(port: u16) -> u8 {
	let val: u8;
	// SAFETY: see `outb`; `in` from a port the caller has identified.
	unsafe { asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags)) }
	val
}

/// Read `count` 16-bit words from `port` into `dst` with one `rep insw`.
///
/// Worth having as a primitive rather than a loop of single-word reads: under
/// virtualization each `in` is its own VM exit, so a 512-byte sector read
/// costs 256 exits word-at-a-time and one exit as a string operation.
///
/// # Safety
///
/// `dst` must be valid for writes of `count * 2` bytes, and must not alias
/// anything the caller still needs.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) unsafe fn rep_insw(port: u16, dst: *mut u8, count: usize) {
	// SAFETY: touches memory through `dst`, so `nomem` is deliberately absent;
	// the caller guarantees `dst` is valid for `count * 2` bytes.
	unsafe {
		asm!(
			"rep insw",
			in("dx") port,
			inout("rdi") dst => _,
			inout("rcx") count => _,
			options(nostack, preserves_flags),
		)
	}
}

/// Write `count` 16-bit words from `src` to `port` with one `rep outsw`.
///
/// # Safety
///
/// `src` must be valid for reads of `count * 2` bytes.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) unsafe fn rep_outsw(port: u16, src: *const u8, count: usize) {
	// SAFETY: reads memory through `src`, so `nomem` is deliberately absent;
	// the caller guarantees `src` is valid for `count * 2` bytes.
	unsafe {
		asm!(
			"rep outsw",
			in("dx") port,
			inout("rsi") src => _,
			inout("rcx") count => _,
			options(nostack, preserves_flags),
		)
	}
}
