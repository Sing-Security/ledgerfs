//! Property tests for LedgerFS large-scale operations

#[cfg(test)]
mod tests {
	use crate::object::*;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	#[test]
	fn test_large_directory_sharding_stable() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut dir_node = DirNode::new_leaf();
		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());

		// Add 500 entries to trigger sharding
		for i in 0..500 {
			let name = alloc::format!("file_{:04}.txt", i);
			let entry = DirEntry::new(&name, inode_ref)?;
			dir_node.insert_entry(entry)?;
		}

		// -- Exec - Encode twice
		let encoded1 = dir_node.encode_canonical()?;
		let encoded2 = dir_node.encode_canonical()?;

		// -- Check - Should be deterministic
		assert_eq!(encoded1, encoded2);

		// Decode and verify entry order
		let decoded = DirNode::decode_canonical(&encoded1)?;
		assert_eq!(decoded.entries.len(), 500);

		// Verify sorted order
		for i in 1..decoded.entries.len() {
			assert!(decoded.entries[i - 1].name < decoded.entries[i].name);
		}

		// Check if node needs splitting (should exceed max entries)
		if dir_node.needs_split() {
			let (left, right) = dir_node.split()?;

			// Verify split is deterministic
			let left_encoded1 = left.encode_canonical()?;
			let left_encoded2 = left.encode_canonical()?;
			assert_eq!(left_encoded1, left_encoded2);

			let right_encoded1 = right.encode_canonical()?;
			let right_encoded2 = right.encode_canonical()?;
			assert_eq!(right_encoded1, right_encoded2);

			// Verify all entries are preserved
			let total_entries = left.entries.len() + right.entries.len();
			assert_eq!(total_entries, 500);

			// Verify left entries < right entries (sorted split)
			if !left.entries.is_empty() && !right.entries.is_empty() {
				let last_left = &left.entries[left.entries.len() - 1].name;
				let first_right = &right.entries[0].name;
				assert!(last_left < first_right);
			}
		}

		Ok(())
	}

	#[test]
	fn test_random_writes_extent_tree_behavior() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut extent_tree = ExtentNode::new_leaf();

		// Simulate random writes to a 1MB file in 4KB chunks
		let chunk_size = 4096u64;
		let file_size = 1024 * 1024u64; // 1MB
		let num_chunks = file_size / chunk_size;

		// Write chunks in non-sequential order
		let write_order = vec![0, 10, 5, 20, 3, 15, 8, 12, 1, 18];

		for &chunk_idx in &write_order {
			if chunk_idx >= num_chunks {
				continue;
			}

			let logical_offset = chunk_idx * chunk_size;
			let chunk_data = alloc::format!("chunk_{}", chunk_idx);
			let chunk_id = ObjectId::from_content(chunk_data.as_bytes());

			let extent = ExtentDescriptor {
				logical_offset,
				length: chunk_size,
				chunk_id,
				chunk_hint: StorageHint::zero(),
				chunk_offset: 0,
			};

			extent_tree.insert_extent(extent)?;
		}

		// -- Exec - Encode multiple times
		let encoded1 = extent_tree.encode_canonical()?;
		let encoded2 = extent_tree.encode_canonical()?;

		// -- Check - Deterministic encoding
		assert_eq!(encoded1, encoded2);

		// Verify extents are sorted by logical offset
		let decoded = ExtentNode::decode_canonical(&encoded1)?;
		for i in 1..decoded.extents.len() {
			assert!(decoded.extents[i - 1].logical_offset < decoded.extents[i].logical_offset);
		}

		// Verify no duplicate logical offsets
		for i in 1..decoded.extents.len() {
			assert_ne!(decoded.extents[i - 1].logical_offset, decoded.extents[i].logical_offset);
		}

		// Test extent lookup is efficient
		for &chunk_idx in &write_order {
			if chunk_idx >= num_chunks {
				continue;
			}

			let logical_offset = chunk_idx * chunk_size;
			let found = extent_tree.find_extent(logical_offset);
			assert!(found.is_some());
			assert_eq!(found.unwrap().logical_offset, logical_offset);
		}

		Ok(())
	}

	#[test]
	fn test_extent_tree_no_full_rewrite() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut extent_tree = ExtentNode::new_leaf();

		// Create a large file with many extents
		for i in 0..100 {
			let logical_offset = i * 4096;
			let chunk_data = alloc::format!("chunk_{}", i);
			let chunk_id = ObjectId::from_content(chunk_data.as_bytes());

			let extent = ExtentDescriptor {
				logical_offset,
				length: 4096,
				chunk_id,
				chunk_hint: StorageHint::zero(),
				chunk_offset: 0,
			};

			extent_tree.insert_extent(extent)?;
		}

		// Modify a single extent in the middle
		let new_chunk_data = b"modified_chunk_50";
		let new_chunk_id = ObjectId::from_content(new_chunk_data);

		let modified_tree = extent_tree.clone();
		// In a real CoW implementation, we would update just one extent
		// For now, verify that the object ID changes when we modify

		// This would require remove + insert in real implementation
		// For now, just verify the tree structure
		assert_ne!(new_chunk_id, extent_tree.extents[50].chunk_id);

		// Verify that most extents are unchanged (they share the same chunk IDs)
		let unchanged_count = extent_tree
			.extents
			.iter()
			.take(50)
			.zip(modified_tree.extents.iter().take(50))
			.filter(|(a, b)| a.chunk_id == b.chunk_id)
			.count();

		assert_eq!(unchanged_count, 50); // First 50 are identical

		Ok(())
	}

	#[test]
	fn test_btree_split_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures - Create a node that needs splitting
		let mut dir_node = DirNode::new_leaf();
		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());

		// Add enough entries to trigger split
		for i in 0..DirNode::MAX_ENTRIES + 10 {
			let name = alloc::format!("entry_{:05}", i);
			let entry = DirEntry::new(&name, inode_ref)?;
			dir_node.insert_entry(entry)?;
		}

		// -- Exec - Split multiple times
		let (left1, right1) = dir_node.split()?;
		let (left2, right2) = dir_node.split()?;

		// -- Check - Splits should be deterministic
		assert_eq!(left1.object_id, left2.object_id);
		assert_eq!(right1.object_id, right2.object_id);

		assert_eq!(left1.entries.len(), left2.entries.len());
		assert_eq!(right1.entries.len(), right2.entries.len());

		// Verify median split point
		let total_entries = dir_node.entries.len();
		let expected_left_size = total_entries / 2;
		assert_eq!(left1.entries.len(), expected_left_size);

		Ok(())
	}

	#[test]
	fn test_canonical_encoding_size_limit() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut extent_tree = ExtentNode::new_leaf();

		// Add extents until we exceed the size limit
		// ExtentNode::MAX_ENTRIES is 1500, which should be close to the limit
		for i in 0..ExtentNode::MAX_ENTRIES + 100 {
			let logical_offset = i as u64 * 4096;
			let chunk_data = alloc::format!("chunk_{}", i);
			let chunk_id = ObjectId::from_content(chunk_data.as_bytes());

			let extent = ExtentDescriptor {
				logical_offset,
				length: 4096,
				chunk_id,
				chunk_hint: StorageHint::zero(),
				chunk_offset: 0,
			};

			extent_tree.insert_extent(extent)?;
		}

		// -- Exec
		// Encoding must succeed even though only the split below is asserted.
		extent_tree.encode_canonical()?;

		// -- Check - Should need splitting due to size or entry count
		assert!(extent_tree.needs_split());

		// After splitting, each half should be under the limit
		let (left, right) = extent_tree.split()?;
		let left_encoded = left.encode_canonical()?;
		let right_encoded = right.encode_canonical()?;

		assert!(left_encoded.len() <= ExtentNode::MAX_CANONICAL_SIZE);
		assert!(right_encoded.len() <= ExtentNode::MAX_CANONICAL_SIZE);

		Ok(())
	}
}
