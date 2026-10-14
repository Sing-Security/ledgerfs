//! Standalone compression tests
//!
//! Tests compression module in isolation since the main crate has unrelated build errors.

#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use crucible_ledgerfs_kernel::compression::{
	COMPRESSION_MIN_RATIO, COMPRESSION_THRESHOLD_SIZE, CompressionAlgorithm, compress, decompress,
};

type Result<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

#[test]
fn test_compression_threshold_size() -> Result<()> {
	// -- Setup & Fixtures
	let small_data = vec![0u8; 100]; // Below 4KiB threshold
	let large_data = vec![0u8; 8192]; // Above 4KiB threshold

	// -- Exec
	let small_result = compress(&small_data, CompressionAlgorithm::Zstd)?;
	let large_result = compress(&large_data, CompressionAlgorithm::Zstd)?;

	// -- Check
	assert!(small_result.is_none(), "Small data should not be compressed");
	assert!(large_result.is_some(), "Large compressible data should be compressed");

	Ok(())
}

#[test]
fn test_compression_roundtrip_zstd() -> Result<()> {
	// -- Setup & Fixtures
	let data = vec![0u8; 8192]; // Highly compressible

	// -- Exec
	let compressed = compress(&data, CompressionAlgorithm::Zstd)?.expect("Should compress");
	let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd, data.len())?;

	// -- Check
	assert_eq!(decompressed, data);
	assert!(compressed.len() < data.len(), "Should be smaller after compression");

	Ok(())
}

#[test]
fn test_compression_deterministic() -> Result<()> {
	// -- Setup & Fixtures
	let data = vec![0u8; 8192];

	// -- Exec
	let compressed1 = compress(&data, CompressionAlgorithm::Zstd)?.unwrap();
	let compressed2 = compress(&data, CompressionAlgorithm::Zstd)?.unwrap();

	// -- Check
	assert_eq!(compressed1, compressed2, "Compression should be deterministic");

	Ok(())
}

#[test]
fn test_compression_none_passthrough() -> Result<()> {
	// -- Setup & Fixtures
	let data = vec![1, 2, 3, 4, 5];

	// -- Exec
	let result = compress(&data, CompressionAlgorithm::None)?;

	// -- Check
	assert!(result.is_none());

	Ok(())
}

#[test]
fn test_decompression_none_passthrough() -> Result<()> {
	// -- Setup & Fixtures
	let data = vec![1, 2, 3, 4, 5];

	// -- Exec
	let result = decompress(&data, CompressionAlgorithm::None, data.len())?;

	// -- Check
	assert_eq!(result, data);

	Ok(())
}

#[test]
fn test_compression_constants() {
	// -- Check
	assert_eq!(COMPRESSION_THRESHOLD_SIZE, 4096);
	assert!((COMPRESSION_MIN_RATIO - 1.1).abs() < 0.01);
}
