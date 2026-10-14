//! LZ4 block format, `no_std`.
//!
//! ## Why this exists
//!
//! LedgerFS's compression was implemented against the `lz4` and `zstd` crates,
//! both of which the `std` feature gates — and the kernel and `fs-cbl` take this
//! crate with `default-features = false`. So on target every algorithm except
//! `CompressionAlgorithm::None` returned `InvalidEncoding`, and because the
//! write path uses `compress(..)?` an object written with a non-`None` policy
//! would have failed the write outright. Compression was a documented feature
//! that could not run.
//!
//! `lz4` is a binding to the C library and needs libc; there is no vendored
//! `no_std` LZ4 available here. This is the block format implemented directly,
//! in safe Rust over `alloc`, so it works in ring 0.
//!
//! ## Format
//!
//! A block is a sequence of: a token byte, optional extended literal length,
//! the literals, a 2-byte little-endian match offset, and an optional extended
//! match length. The token's high nibble is the literal length and its low
//! nibble the match length minus 4 (the minimum match). A nibble of 15 means
//! "read more": additional bytes are summed, a 255 meaning "keep reading".
//!
//! Two end-of-block rules from the specification are load-bearing for
//! interoperability, and both are honoured by `compress`:
//!
//! - the last sequence is literals only, with no match after it;
//! - the final 5 bytes of a block are always literals, and no match may begin
//!   within the last 12 bytes.
//!
//! Decoding tolerates any conforming encoder, so blocks written by the C
//! library decode here and vice versa. `tests::interop` pins that against the
//! real `lz4` crate when built with `--features std`.

use alloc::vec::Vec;

use crate::error::{Error, Result};

/// Shortest match the format can encode (the token's low nibble is `len - 4`).
const MIN_MATCH: usize = 4;
/// No match may start within this many bytes of the end.
const LAST_MATCH_GUARD: usize = 12;
/// The final bytes of a block are always literals.
const END_LITERALS: usize = 5;
/// Offsets are 16-bit, so a match can reach back this far at most.
const MAX_OFFSET: usize = 0xFFFF;

/// Hash table size for the match finder. 4096 entries is a deliberate middle:
/// large enough to find most short-range matches in the record-sized buffers
/// LedgerFS writes, small enough to allocate and clear cheaply per call.
const HASH_BITS: usize = 12;
const HASH_LEN: usize = 1 << HASH_BITS;

#[inline]
fn hash4(v: u32) -> usize {
	// Knuth multiplicative; take the high bits, which mix best.
	((v.wrapping_mul(2_654_435_761)) >> (32 - HASH_BITS)) as usize
}

#[inline]
fn read_u32(buf: &[u8], at: usize) -> u32 {
	u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Append a length that did not fit in a token nibble.
fn write_extended(out: &mut Vec<u8>, mut remaining: usize) {
	while remaining >= 255 {
		out.push(255);
		remaining -= 255;
	}
	out.push(remaining as u8);
}

/// Compress `src` into an LZ4 block.
///
/// Always succeeds. For incompressible input the output is the literals plus a
/// small framing overhead, which is why callers apply a ratio test and keep the
/// original when compression does not pay (see `compression::check_compression_ratio`).
pub fn compress(src: &[u8]) -> Vec<u8> {
	let mut out = Vec::with_capacity(src.len() + src.len() / 255 + 16);

	// Too short to hold a match plus the mandatory trailing literals: emit the
	// whole input as one literal-only sequence.
	if src.len() < LAST_MATCH_GUARD + MIN_MATCH {
		emit_last_literals(&mut out, src);
		return out;
	}

	let mut table = [0usize; HASH_LEN];
	let mut filled = [false; HASH_LEN];

	let mut anchor = 0usize; // start of the literals not yet emitted
	let mut pos = 0usize;
	let limit = src.len() - LAST_MATCH_GUARD;

	while pos < limit {
		let seq = read_u32(src, pos);
		let h = hash4(seq);
		let candidate = table[h];
		let seen = filled[h];
		table[h] = pos;
		filled[h] = true;

		// A candidate is usable only if it is in range and actually matches —
		// the hash is lossy, so this comparison is what makes it correct.
		if !seen
			|| pos <= candidate
			|| pos - candidate > MAX_OFFSET
			|| read_u32(src, candidate) != seq
		{
			pos += 1;
			continue;
		}

		// Extend the match as far as the guard allows.
		let mut len = MIN_MATCH;
		while pos + len < limit && src[candidate + len] == src[pos + len] {
			len += 1;
		}

		let literal_len = pos - anchor;
		let match_len_code = len - MIN_MATCH;

		let token = ((literal_len.min(15) as u8) << 4) | (match_len_code.min(15) as u8);
		out.push(token);
		if literal_len >= 15 {
			write_extended(&mut out, literal_len - 15);
		}
		out.extend_from_slice(&src[anchor..pos]);
		let offset = (pos - candidate) as u16;
		out.extend_from_slice(&offset.to_le_bytes());
		if match_len_code >= 15 {
			write_extended(&mut out, match_len_code - 15);
		}

		pos += len;
		anchor = pos;
	}

	emit_last_literals(&mut out, &src[anchor..]);
	out
}

/// The closing sequence: literals with no match after them.
fn emit_last_literals(out: &mut Vec<u8>, literals: &[u8]) {
	let n = literals.len();
	out.push((n.min(15) as u8) << 4);
	if n >= 15 {
		write_extended(out, n - 15);
	}
	out.extend_from_slice(literals);
}

/// Read an extended length introduced by a nibble of 15.
fn read_extended(src: &[u8], pos: &mut usize) -> Result<usize> {
	let mut total = 15usize;
	loop {
		let b = *src
			.get(*pos)
			.ok_or_else(|| Error::invalid_encoding("lz4: truncated length"))?;
		*pos += 1;
		total = total
			.checked_add(b as usize)
			.ok_or_else(|| Error::invalid_encoding("lz4: length overflow"))?;
		if b != 255 {
			return Ok(total);
		}
	}
}

/// Decompress an LZ4 block whose original length is known to be `expected_len`.
///
/// Every bound is checked: this decodes attacker-influenced bytes off disk, so
/// a malformed or truncated block must be an error rather than a panic or an
/// out-of-bounds read.
pub fn decompress(src: &[u8], expected_len: usize) -> Result<Vec<u8>> {
	let mut out: Vec<u8> = Vec::with_capacity(expected_len);
	let mut pos = 0usize;

	while pos < src.len() {
		let token = src[pos];
		pos += 1;

		// Literals.
		let mut literal_len = (token >> 4) as usize;
		if literal_len == 15 {
			literal_len = read_extended(src, &mut pos)?;
		}
		if literal_len > 0 {
			let end = pos
				.checked_add(literal_len)
				.ok_or_else(|| Error::invalid_encoding("lz4: literal length overflow"))?;
			if end > src.len() {
				return Err(Error::invalid_encoding("lz4: literals past end of block"));
			}
			out.extend_from_slice(&src[pos..end]);
			pos = end;
		}

		// A block ends on a literal-only sequence, so running out here is the
		// normal termination rather than an error.
		if pos >= src.len() {
			break;
		}

		// Match.
		if pos + 2 > src.len() {
			return Err(Error::invalid_encoding("lz4: truncated match offset"));
		}
		let offset = u16::from_le_bytes([src[pos], src[pos + 1]]) as usize;
		pos += 2;
		if offset == 0 || offset > out.len() {
			return Err(Error::invalid_encoding("lz4: match offset outside output"));
		}

		// `read_extended` already folds in the base 15, so its result *is* the
		// encoded match-length code; the real length adds the 4-byte minimum.
		let mut match_len = (token & 0x0F) as usize;
		if match_len == 15 {
			match_len = read_extended(src, &mut pos)?;
		}
		let match_len = match_len + MIN_MATCH;

		// Copied one byte at a time on purpose: an LZ4 match may overlap its
		// own output (offset < length) to express a run, so a bulk copy would
		// be wrong.
		let start = out.len() - offset;
		for i in 0..match_len {
			let b = out[start + i];
			out.push(b);
		}
	}

	if out.len() != expected_len {
		return Err(Error::invalid_encoding("lz4: decompressed length mismatch"));
	}
	Ok(out)
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	use super::*;

	fn roundtrip(data: &[u8]) -> TestResult<()> {
		let packed = compress(data);
		let out = decompress(&packed, data.len())?;
		assert_eq!(out, data, "round trip changed {} bytes of input", data.len());
		Ok(())
	}

	#[test]
	fn test_roundtrip_empty_and_tiny() -> TestResult<()> {
		// -- Exec / Check: below the match guard, everything is literals.
		roundtrip(b"")?;
		roundtrip(b"a")?;
		roundtrip(b"abcdefghijklmno")?;
		Ok(())
	}

	#[test]
	fn test_roundtrip_highly_repetitive() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = alloc::vec![0xABu8; 64 * 1024];

		// -- Exec
		let packed = compress(&data);

		// -- Check
		assert!(packed.len() < data.len() / 10, "64 KiB of one byte should compress hard, got {}", packed.len());
		assert_eq!(decompress(&packed, data.len())?, data);
		Ok(())
	}

	#[test]
	fn test_roundtrip_structured_text() -> TestResult<()> {
		// -- Setup & Fixtures: the shape of a directory node, which is what
		// motivated having compression at all.
		let mut data = Vec::new();
		for i in 0..2000 {
			data.extend_from_slice(alloc::format!("entry_{i:06}\0inode={i}\0mode=0644\0").as_bytes());
		}

		// -- Exec
		let packed = compress(&data);

		// -- Check
		assert!(packed.len() < data.len(), "repetitive records should shrink");
		assert_eq!(decompress(&packed, data.len())?, data);
		Ok(())
	}

	#[test]
	fn test_roundtrip_incompressible() -> TestResult<()> {
		// A counter-based LCG stands in for random data: no useful matches, so
		// the encoder must still produce something that round-trips.
		//
		// -- Setup & Fixtures
		let mut x = 0x2545_F491_4F6C_DD1Du64;
		let data: Vec<u8> = (0..8192)
			.map(|_| {
				x ^= x << 13;
				x ^= x >> 7;
				x ^= x << 17;
				(x >> 24) as u8
			})
			.collect();

		// -- Exec / Check
		roundtrip(&data)?;
		Ok(())
	}

	#[test]
	fn test_roundtrip_overlapping_match() -> TestResult<()> {
		// `abababab...` forces matches whose offset is smaller than their
		// length, which only decode correctly with a byte-at-a-time copy.
		//
		// -- Setup & Fixtures
		let data: Vec<u8> = core::iter::repeat(b"ab").take(4096).flatten().copied().collect();

		// -- Exec / Check
		roundtrip(&data)?;
		Ok(())
	}

	#[test]
	fn test_decompress_rejects_truncated_block() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = alloc::vec![0x11u8; 4096];
		let packed = compress(&data);

		// -- Exec: lop off the tail.
		let result = decompress(&packed[..packed.len() / 2], data.len());

		// -- Check: an error, never a panic or a short buffer.
		assert!(result.is_err(), "a truncated block must not decode");
		Ok(())
	}

	#[test]
	fn test_decompress_rejects_offset_past_output() -> TestResult<()> {
		// A match that reaches back further than anything decoded so far is the
		// shape of a corrupted or hostile block.
		//
		// -- Setup & Fixtures: 1 literal, then a match at offset 0x4141.
		let block = alloc::vec![0x1F, b'x', 0x41, 0x41];

		// -- Exec
		let result = decompress(&block, 64);

		// -- Check
		assert!(result.is_err(), "an out-of-range offset must be rejected");
		Ok(())
	}

	#[test]
	fn test_decompress_rejects_wrong_expected_length() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = alloc::vec![7u8; 300];
		let packed = compress(&data);

		// -- Exec / Check
		assert!(decompress(&packed, 299).is_err(), "a length mismatch must be caught");
		Ok(())
	}

	/// Cross-check against the reference C implementation.
	///
	/// Only buildable with `--features std`, which is what pulls in the `lz4`
	/// crate — but that is precisely the point: it pins this encoder to the
	/// same on-disk format the std path produces, so blocks written by either
	/// are readable by the other.
	#[cfg(feature = "std")]
	#[test]
	fn test_interop_with_reference_lz4() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut data = Vec::new();
		for i in 0..1000 {
			data.extend_from_slice(alloc::format!("record {i:05} payload payload payload\n").as_bytes());
		}

		// -- Exec: ours out, theirs in.
		let ours = compress(&data);
		let theirs_read = lz4::block::decompress(&ours, Some(data.len() as i32))?;

		// ...and theirs out, ours in.
		let theirs = lz4::block::compress(&data, None, false)?;
		let ours_read = decompress(&theirs, data.len())?;

		// -- Check
		assert_eq!(theirs_read, data, "reference decoder could not read our block");
		assert_eq!(ours_read, data, "we could not read the reference encoder's block");
		Ok(())
	}
}

// endregion: --- Tests
