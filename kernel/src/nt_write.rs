//! Non-temporal write helpers for LedgerFS cache-pollution mitigation.
//!
//! Per §15.3 of FORMAT.md:
//! - Object payload writes use NT stores to bypass L1/L2, keeping BTree metadata hot.
//! - Intent log ring writes use NT stores for the same reason.
//! - An SFENCE MUST be issued after any sequence of NT stores, before any
//!   subsequent storage barrier.
//!
//! On non-x86_64 targets and in test builds, all helpers are no-ops / regular copies.

/// Write `len` bytes from `src` to `dst` using non-temporal stores on x86_64,
/// or a regular memcpy on all other architectures.
///
/// # Safety
///
/// Caller must ensure:
/// - `dst` is valid for writes of `len` bytes.
/// - `src` is valid for reads of `len` bytes.
/// - `dst` and `src` do not overlap.
/// - On x86_64, the caller MUST issue `sfence()` after all NT writes are
///   complete, before any subsequent storage barrier (§15.3).
#[inline]
pub unsafe fn nt_write_payload(dst: *mut u8, src: *const u8, len: usize) {
	#[cfg(all(target_arch = "x86_64", not(test)))]
	{
		// SAFETY: caller guarantees dst/src valid for len bytes and non-overlapping.
		// _mm_stream_si64 emits MOVNTQ — a 64-bit non-temporal store.
		let chunks = len / 8;
		let tail_start = chunks * 8;

		for i in 0..chunks {
			// SAFETY: src + i*8 is within the valid source range (i < chunks = len/8).
			// dst + i*8 is within the valid destination range (same reasoning).
			// _mm_stream_si64 requires 8-byte alignment which is satisfied for
			// loop-aligned offsets when dst is 8-byte aligned.
			unsafe {
				let val = core::ptr::read_unaligned(src.add(i * 8) as *const i64);
				core::arch::x86_64::_mm_stream_si64(dst.add(i * 8) as *mut i64, val);
			}
		}

		// Tail bytes — regular stores (NT stores require 8-byte granularity).
		for i in tail_start..len {
			// SAFETY: i is within [tail_start, len), both within valid ranges.
			unsafe {
				*dst.add(i) = *src.add(i);
			}
		}
	}
	#[cfg(any(not(target_arch = "x86_64"), test))]
	{
		// SAFETY: caller guarantees non-overlapping src/dst valid for len bytes.
		unsafe {
			core::ptr::copy_nonoverlapping(src, dst, len);
		}
	}
}

/// Issue an x86_64 SFENCE instruction.
///
/// MUST be called after a sequence of NT stores and before any subsequent
/// storage barrier, per LedgerFS §15.3.
///
/// No-op on non-x86_64 targets and in test builds.
#[inline]
pub fn sfence() {
	#[cfg(all(target_arch = "x86_64", not(test)))]
	// SAFETY: SFENCE is a pure memory fence with no side-effects on registers
	// or memory — it only orders preceding NT stores with subsequent regular stores.
	unsafe {
		core::arch::asm!("sfence", options(nomem, nostack, preserves_flags));
	}
}
