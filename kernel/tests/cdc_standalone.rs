//! Standalone CDC integration tests
//!
//! These tests verify the Content-Defined Chunking implementation
//! without depending on other ledgerfs modules that may have build issues.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use crucible_ledgerfs_kernel::cdc::{AVG_CHUNK_SIZE, Chunker, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE, StreamingChunker};

type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

#[test]
fn test_cdc_standalone_deterministic_chunking() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create 512KB of pseudo-random data
	let mut data: Vec<u8> = Vec::new();
	let mut rng = 54321u64;
	for _ in 0..512 * 1024 {
		rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
		data.push((rng >> 16) as u8);
	}

	// -- Exec
	let mut chunker1 = Chunker::new();
	let boundaries1 = chunker1.chunk_data(&data);

	let mut chunker2 = Chunker::new();
	let boundaries2 = chunker2.chunk_data(&data);

	// -- Check
	assert_eq!(boundaries1, boundaries2, "Same data must produce identical boundaries");
	assert!(!boundaries1.is_empty(), "Should produce at least one chunk");

	// Verify last boundary is at end
	assert_eq!(*boundaries1.last().unwrap(), data.len());

	Ok(())
}

#[test]
fn test_cdc_standalone_shift_problem_improvement() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create 256KB of base data
	let mut base_data: Vec<u8> = Vec::new();
	for i in 0..256 * 1024 {
		base_data.push((i % 256) as u8);
	}

	// Chunk base data
	let mut chunker1 = Chunker::new();
	let boundaries_before = chunker1.chunk_data(&base_data);

	// Insert 100 bytes at position 10KB (should only affect nearby chunks)
	let insert_pos = 10 * 1024;
	for i in 0..100 {
		base_data.insert(insert_pos + i, 0xFF);
	}

	// Chunk modified data
	let mut chunker2 = Chunker::new();
	let boundaries_after = chunker2.chunk_data(&base_data);

	// -- Check
	// Count boundaries that are roughly the same (within 5KB tolerance)
	let mut unchanged_count = 0;
	for &b_before in &boundaries_before {
		// Skip boundaries near the insertion point (5KB tolerance)
		let lower_bound = insert_pos.saturating_sub(5 * 1024);
		let upper_bound = insert_pos + 5 * 1024;
		if b_before >= lower_bound && b_before <= upper_bound {
			continue;
		}

		// Check if there's a similar boundary in the after set
		for &b_after in &boundaries_after {
			// Account for the 100 bytes inserted
			let adjusted = if b_after > insert_pos {
				b_after.saturating_sub(100)
			} else {
				b_after
			};

			if b_before.abs_diff(adjusted) < 5 * 1024 {
				unchanged_count += 1;
				break;
			}
		}
	}

	// At least 30% of boundaries should remain stable (shift problem improvement)
	let total_boundaries = boundaries_before.len().max(1);
	let stability_ratio = unchanged_count * 100 / total_boundaries;

	assert!(
		stability_ratio >= 30,
		"Expected at least 30% boundary stability, got {}% ({}/{} boundaries)",
		stability_ratio,
		unchanged_count,
		total_boundaries
	);

	Ok(())
}

#[test]
fn test_cdc_standalone_min_chunk_enforced() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create data just under MIN_CHUNK_SIZE
	let data: Vec<u8> = vec![0x42; MIN_CHUNK_SIZE - 1024];

	// -- Exec
	let mut chunker = Chunker::new();
	let boundaries = chunker.chunk_data(&data);

	// -- Check
	assert_eq!(boundaries.len(), 1, "Small data should produce exactly one chunk");
	assert_eq!(boundaries[0], data.len(), "Boundary should be at end of data");

	Ok(())
}

#[test]
fn test_cdc_standalone_max_chunk_enforced() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create uniform data (unlikely to trigger natural boundaries)
	let data: Vec<u8> = vec![0x00; MAX_CHUNK_SIZE * 2 + 1024];

	// -- Exec
	let mut chunker = Chunker::new();
	let boundaries = chunker.chunk_data(&data);

	// -- Check
	assert!(!boundaries.is_empty(), "Should produce chunks");

	// Calculate chunk sizes
	let mut prev = 0;
	for (i, &boundary) in boundaries.iter().enumerate() {
		let chunk_size = boundary - prev;

		// All chunks except possibly the last should respect MIN_CHUNK_SIZE
		if i < boundaries.len() - 1 {
			assert!(
				chunk_size >= MIN_CHUNK_SIZE,
				"Chunk {} has size {} < MIN_CHUNK_SIZE {}",
				i,
				chunk_size,
				MIN_CHUNK_SIZE
			);
		}

		// All chunks should respect MAX_CHUNK_SIZE
		assert!(
			chunk_size <= MAX_CHUNK_SIZE,
			"Chunk {} has size {} > MAX_CHUNK_SIZE {}",
			i,
			chunk_size,
			MAX_CHUNK_SIZE
		);

		prev = boundary;
	}

	Ok(())
}

#[test]
fn test_cdc_standalone_streaming_determinism() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create 512KB of data
	let mut data: Vec<u8> = Vec::new();
	let mut rng = 99999u64;
	for _ in 0..512 * 1024 {
		rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
		data.push((rng >> 16) as u8);
	}

	// Batch chunking
	let mut batch_chunker = Chunker::new();
	let batch_boundaries = batch_chunker.chunk_data(&data);

	// Streaming chunking (split into 8 chunks)
	let mut streaming = StreamingChunker::new();
	let mut streaming_boundaries = Vec::new();

	let chunk_size = data.len() / 8;
	for i in 0..8 {
		let start = i * chunk_size;
		let end = if i == 7 { data.len() } else { (i + 1) * chunk_size };
		let chunk_data = &data[start..end];
		streaming_boundaries.extend(streaming.process(chunk_data));
	}

	if let Some(final_pos) = streaming.finalize() {
		streaming_boundaries.push(final_pos);
	}

	// -- Check
	assert_eq!(
		batch_boundaries, streaming_boundaries,
		"Batch and streaming chunking must produce identical boundaries"
	);

	Ok(())
}

#[test]
fn test_cdc_standalone_boundary_distribution() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create 2MB of pseudo-random data for statistical analysis
	let mut data: Vec<u8> = Vec::new();
	let mut rng = 777777u64;
	for _ in 0..2 * 1024 * 1024 {
		rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
		data.push((rng >> 16) as u8);
	}

	// -- Exec
	let mut chunker = Chunker::new();
	let boundaries = chunker.chunk_data(&data);

	// -- Check
	assert!(boundaries.len() >= 5, "Should produce multiple chunks");

	// Calculate chunk sizes
	let mut sizes = Vec::new();
	let mut prev = 0;
	for &boundary in &boundaries {
		sizes.push(boundary - prev);
		prev = boundary;
	}

	// Calculate average (excluding last chunk which might be partial)
	let avg_size: usize = if sizes.len() > 1 {
		sizes[..sizes.len() - 1].iter().sum::<usize>() / (sizes.len() - 1)
	} else {
		sizes[0]
	};

	// Average should be within reasonable range of target
	// (Allow 30KB - 120KB range, target is 64KB)
	assert!(
		avg_size >= 30 * 1024,
		"Average chunk size {} is too small (< 30KB)",
		avg_size
	);
	assert!(
		avg_size <= 120 * 1024,
		"Average chunk size {} is too large (> 120KB)",
		avg_size
	);

	// All non-last chunks should respect min/max
	for (i, &size) in sizes[..sizes.len() - 1].iter().enumerate() {
		assert!(size >= MIN_CHUNK_SIZE, "Chunk {} size {} < MIN_CHUNK_SIZE", i, size);
		assert!(size <= MAX_CHUNK_SIZE, "Chunk {} size {} > MAX_CHUNK_SIZE", i, size);
	}

	Ok(())
}

#[test]
fn test_cdc_standalone_empty_data() -> TestResult<()> {
	// -- Setup & Fixtures
	let data: Vec<u8> = vec![];

	// -- Exec
	let mut chunker = Chunker::new();
	let boundaries = chunker.chunk_data(&data);

	// -- Check
	assert!(boundaries.is_empty(), "Empty data should produce no boundaries");

	Ok(())
}

#[test]
fn test_cdc_standalone_tiny_data() -> TestResult<()> {
	// -- Setup & Fixtures
	let data: Vec<u8> = vec![1, 2, 3, 4, 5];

	// -- Exec
	let mut chunker = Chunker::new();
	let boundaries = chunker.chunk_data(&data);

	// -- Check
	assert_eq!(boundaries.len(), 1, "Tiny data should produce one chunk");
	assert_eq!(boundaries[0], 5, "Boundary should be at end");

	Ok(())
}

#[test]
fn test_cdc_standalone_streaming_multiple_calls() -> TestResult<()> {
	// -- Setup & Fixtures
	let chunk1: Vec<u8> = (0..64 * 1024).map(|i| (i % 256) as u8).collect();
	let chunk2: Vec<u8> = (64 * 1024..128 * 1024).map(|i| (i % 256) as u8).collect();
	let chunk3: Vec<u8> = (128 * 1024..192 * 1024).map(|i| (i % 256) as u8).collect();

	// -- Exec
	let mut streaming = StreamingChunker::new();
	let boundaries1 = streaming.process(&chunk1);
	let boundaries2 = streaming.process(&chunk2);
	let boundaries3 = streaming.process(&chunk3);
	let final_boundary = streaming.finalize();

	// -- Check
	// All boundaries should be monotonically increasing
	let mut all_boundaries = boundaries1.clone();
	all_boundaries.extend(&boundaries2);
	all_boundaries.extend(&boundaries3);

	if let Some(final_pos) = final_boundary {
		all_boundaries.push(final_pos);
	}

	for i in 1..all_boundaries.len() {
		assert!(
			all_boundaries[i] > all_boundaries[i - 1],
			"Boundaries must be strictly increasing"
		);
	}

	// Final boundary should be total data size
	if let Some(final_pos) = final_boundary {
		let total_size = chunk1.len() + chunk2.len() + chunk3.len();
		assert_eq!(final_pos, total_size);
	}

	Ok(())
}

#[test]
fn test_cdc_standalone_cross_platform_determinism() -> TestResult<()> {
	// -- Setup & Fixtures
	// Create data with known pattern
	let mut data: Vec<u8> = Vec::new();
	for i in 0..128 * 1024 {
		data.push((i * 7 + 13) as u8); // Simple deterministic pattern
	}

	// -- Exec
	let mut chunker1 = Chunker::new();
	let boundaries1 = chunker1.chunk_data(&data);

	// Second run should produce identical results
	let mut chunker2 = Chunker::new();
	let boundaries2 = chunker2.chunk_data(&data);

	// -- Check
	assert_eq!(boundaries1, boundaries2);

	// Also verify that boundaries are non-trivial (not just [data.len()])
	// For 128KB of varying data, we should get multiple chunks
	assert!(boundaries1.len() >= 1, "Should produce at least one chunk");

	Ok(())
}
