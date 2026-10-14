//! Compression support for LedgerFS chunks
//!
//! Per §28 of ledgerfs-spec-v7.md:
//! - Supports None, LZ4, ZSTD (level 3 default), ZSTD_HIGH
//! - Compression threshold: size > 4 KiB and ratio > 1.1
//! - Stored bytes are compressed, canonical bytes remain uncompressed for ObjectId

use crate::{Error, Result};
use alloc::vec::Vec;

/// Compression threshold in bytes (4 KiB)
pub const COMPRESSION_THRESHOLD_SIZE: usize = 4096;

/// Minimum compression ratio (1.1 = 10% reduction)
pub const COMPRESSION_MIN_RATIO: f32 = 1.1;

/// Default ZSTD compression level
pub const ZSTD_DEFAULT_LEVEL: i32 = 3;

/// High ZSTD compression level
pub const ZSTD_HIGH_LEVEL: i32 = 19;

/// Compression algorithm discriminator (§28.1)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionAlgorithm {
	/// No compression
	None = 0x00,
	/// LZ4 fast compression
	Lz4 = 0x01,
	/// ZSTD balanced compression (level 3)
	Zstd = 0x02,
	/// ZSTD high compression (level 19)
	ZstdHigh = 0x03,
}

impl CompressionAlgorithm {
	/// Convert from u8
	pub fn from_u8(val: u8) -> Result<Self> {
		match val {
			0x00 => Ok(Self::None),
			0x01 => Ok(Self::Lz4),
			0x02 => Ok(Self::Zstd),
			0x03 => Ok(Self::ZstdHigh),
			_ => Err(Error::invalid_encoding(alloc::format!(
				"Invalid compression algorithm: {}",
				val
			))),
		}
	}

	/// Convert to u8
	pub fn to_u8(self) -> u8 {
		self as u8
	}

	/// Get human-readable name
	pub fn name(self) -> &'static str {
		match self {
			Self::None => "none",
			Self::Lz4 => "lz4",
			Self::Zstd => "zstd",
			Self::ZstdHigh => "zstd-high",
		}
	}
}

/// Compress data with the specified algorithm
///
/// # Returns
///
/// - `Ok(Some(compressed_data))` if compression succeeded and met threshold
/// - `Ok(None)` if compression failed threshold or not beneficial
/// - `Err(...)` on compression error
pub fn compress(data: &[u8], algorithm: CompressionAlgorithm) -> Result<Option<Vec<u8>>> {
	// Check size threshold
	if data.len() <= COMPRESSION_THRESHOLD_SIZE {
		return Ok(None);
	}

	match algorithm {
		CompressionAlgorithm::None => Ok(None),

		CompressionAlgorithm::Lz4 => {
			// `crate::lz4_block` is the block format in safe Rust over `alloc`,
			// so this works in ring 0. It replaced a `std`-gated binding to the
			// C library that made LZ4 unavailable in every build the kernel
			// and fs-cbl actually use — the algorithm was reachable only from
			// tests. Format-compatible with the reference implementation;
			// `lz4_block::tests::test_interop_with_reference_lz4` pins that.
			let compressed = crate::lz4_block::compress(data);
			check_compression_ratio(data.len(), compressed.len(), compressed)
		}

		CompressionAlgorithm::Zstd => {
			#[cfg(feature = "std")]
			{
				let compressed = zstd::bulk::compress(data, ZSTD_DEFAULT_LEVEL)
					.map_err(|e| Error::invalid_encoding(alloc::format!("ZSTD compression failed: {:?}", e)))?;

				check_compression_ratio(data.len(), compressed.len(), compressed)
			}
			#[cfg(not(feature = "std"))]
			{
				// No `no_std` ZSTD encoder is available here. Report "did not
				// compress" rather than an error: `compress` returning `None`
				// means the caller stores the data uncompressed and records
				// `CompressionAlgorithm::None`, which is a correct object.
				// Erroring would instead fail the whole write, because
				// `ObjectRecordWriter` propagates with `?`.
				let _ = data;
				Ok(None)
			}
		}

		CompressionAlgorithm::ZstdHigh => {
			#[cfg(feature = "std")]
			{
				let compressed = zstd::bulk::compress(data, ZSTD_HIGH_LEVEL)
					.map_err(|e| Error::invalid_encoding(alloc::format!("ZSTD compression failed: {:?}", e)))?;

				check_compression_ratio(data.len(), compressed.len(), compressed)
			}
			#[cfg(not(feature = "std"))]
			{
				// As above: degrade to "stored uncompressed", never fail the write.
				let _ = data;
				Ok(None)
			}
		}
	}
}

/// Decompress data with the specified algorithm
pub fn decompress(compressed_data: &[u8], algorithm: CompressionAlgorithm, expected_size: usize) -> Result<Vec<u8>> {
	match algorithm {
		CompressionAlgorithm::None => {
			// No decompression needed
			Ok(compressed_data.to_vec())
		}

		CompressionAlgorithm::Lz4 => {
			// See the compress arm: available in every build, not just `std`.
			crate::lz4_block::decompress(compressed_data, expected_size)
		}

		CompressionAlgorithm::Zstd | CompressionAlgorithm::ZstdHigh => {
			#[cfg(feature = "std")]
			{
				let decompressed = zstd::bulk::decompress(compressed_data, expected_size)
					.map_err(|e| Error::invalid_encoding(alloc::format!("ZSTD decompression failed: {:?}", e)))?;

				if decompressed.len() != expected_size {
					return Err(Error::invalid_encoding(alloc::format!(
						"ZSTD decompressed size mismatch: expected {}, got {}",
						expected_size,
						decompressed.len()
					)));
				}

				Ok(decompressed)
			}
			#[cfg(not(feature = "std"))]
			{
				Err(Error::invalid_encoding("ZSTD requires std feature"))
			}
		}
	}
}

/// Check if compression ratio meets threshold
fn check_compression_ratio(
	original_size: usize,
	compressed_size: usize,
	compressed_data: Vec<u8>,
) -> Result<Option<Vec<u8>>> {
	let ratio = original_size as f32 / compressed_size as f32;

	if ratio >= COMPRESSION_MIN_RATIO {
		Ok(Some(compressed_data))
	} else {
		// Compression not beneficial
		Ok(None)
	}
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_compression_algorithm_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let algorithms = [
			CompressionAlgorithm::None,
			CompressionAlgorithm::Lz4,
			CompressionAlgorithm::Zstd,
			CompressionAlgorithm::ZstdHigh,
		];

		// -- Exec & Check
		for algo in algorithms {
			let val = algo.to_u8();
			let decoded = CompressionAlgorithm::from_u8(val)?;
			assert_eq!(algo, decoded);
		}

		Ok(())
	}

	#[test]
	fn test_compression_invalid_algorithm() -> TestResult<()> {
		// -- Setup & Fixtures
		let invalid = 0xFFu8;

		// -- Exec & Check
		let result = CompressionAlgorithm::from_u8(invalid);
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_compression_none_returns_none() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![0u8; 8192]; // Above threshold

		// -- Exec
		let result = compress(&data, CompressionAlgorithm::None)?;

		// -- Check
		assert!(result.is_none());

		Ok(())
	}

	#[test]
	fn test_compression_small_data_returns_none() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![1, 2, 3, 4]; // Below threshold

		// -- Exec
		let result = compress(&data, CompressionAlgorithm::Zstd)?;

		// -- Check
		assert!(result.is_none());

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// The kernel and fs-cbl take this crate with `default-features = false`,
	// so compression is unavailable on target and this cannot run there.
	#[cfg(feature = "std")]
	fn test_compression_zstd_compressible_data() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![0u8; 8192]; // Highly compressible

		// -- Exec
		let compressed = compress(&data, CompressionAlgorithm::Zstd)?;

		// -- Check
		assert!(compressed.is_some());
		let compressed = compressed.unwrap();
		assert!(compressed.len() < data.len());

		// Verify decompression
		let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd, data.len())?;
		assert_eq!(decompressed, data);

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// The kernel and fs-cbl take this crate with `default-features = false`,
	// so compression is unavailable on target and this cannot run there.
	#[cfg(feature = "std")]
	fn test_compression_zstd_high_better_ratio() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![0u8; 8192];

		// -- Exec
		let compressed_normal = compress(&data, CompressionAlgorithm::Zstd)?.unwrap();
		let compressed_high = compress(&data, CompressionAlgorithm::ZstdHigh)?.unwrap();

		// -- Check
		// High compression should be equal or better
		assert!(compressed_high.len() <= compressed_normal.len());

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// The kernel and fs-cbl take this crate with `default-features = false`,
	// so compression is unavailable on target and this cannot run there.
	#[cfg(feature = "std")]
	fn test_compression_random_data_may_not_compress() -> TestResult<()> {
		// -- Setup & Fixtures
		// Pseudo-random data (not truly random, but varied)
		let mut data = Vec::new();
		for i in 0..8192 {
			data.push((i * 7 + 13) as u8);
		}

		// -- Exec
		let result = compress(&data, CompressionAlgorithm::Zstd)?;

		// -- Check
		// May or may not compress depending on threshold
		// Just verify it doesn't error
		if let Some(compressed) = result {
			let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd, data.len())?;
			assert_eq!(decompressed, data);
		}

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// The kernel and fs-cbl take this crate with `default-features = false`,
	// so compression is unavailable on target and this cannot run there.
	#[cfg(feature = "std")]
	fn test_compression_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![0u8; 8192];

		// -- Exec
		let compressed1 = compress(&data, CompressionAlgorithm::Zstd)?.unwrap();
		let compressed2 = compress(&data, CompressionAlgorithm::Zstd)?.unwrap();

		// -- Check
		assert_eq!(compressed1, compressed2);

		Ok(())
	}

	#[test]
	// Exercises LZ4/ZSTD, which `std = ["dep:lz4", "dep:zstd"]` gates.
	// The kernel and fs-cbl take this crate with `default-features = false`,
	// so compression is unavailable on target and this cannot run there.
	#[cfg(feature = "std")]
	fn test_decompression_size_mismatch_error() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![0u8; 8192];
		let compressed = compress(&data, CompressionAlgorithm::Zstd)?.unwrap();

		// -- Exec & Check
		let result = decompress(&compressed, CompressionAlgorithm::Zstd, 1024);
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_decompression_none_passes_through() -> TestResult<()> {
		// -- Setup & Fixtures
		let data = vec![1, 2, 3, 4, 5];

		// -- Exec
		let result = decompress(&data, CompressionAlgorithm::None, data.len())?;

		// -- Check
		assert_eq!(result, data);

		Ok(())
	}
}

// endregion: --- Tests
