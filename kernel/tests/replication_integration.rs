//! Integration tests for W8 replication (send/receive)

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use crucible_ledgerfs_kernel::object::{ObjectId, Root};
use crucible_ledgerfs_kernel::replication::*;

type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

// region:    --- Send Stream Integration Tests

#[test]
fn test_integration_full_send_deterministic() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut root1 = Root::new();
	root1.commit_seq = 1;
	root1.file_tree_root.object_id = ObjectId::from_content(b"file_tree_1");
	root1.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_tree_1");
	root1.update_object_id();

	// -- Exec: Create same stream twice
	let stream1 = create_full_send_stream(&root1)?;
	let stream2 = create_full_send_stream(&root1)?;

	// -- Check: Streams should be identical
	assert_eq!(stream1.metadata.target_root_id, stream2.metadata.target_root_id);
	assert_eq!(stream1.metadata.object_count, stream2.metadata.object_count);
	assert_eq!(stream1.objects.len(), stream2.objects.len());

	// Check objects are in same order
	let ids1: Vec<_> = stream1.objects.keys().collect();
	let ids2: Vec<_> = stream2.objects.keys().collect();
	assert_eq!(ids1, ids2);

	Ok(())
}

#[test]
fn test_integration_incremental_send_deterministic() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut base_root = Root::new();
	base_root.commit_seq = 1;
	base_root.file_tree_root.object_id = ObjectId::from_content(b"base_tree");
	base_root.update_object_id();

	let mut target_root = base_root.clone();
	target_root.commit_seq = 2;
	target_root.file_tree_root.object_id = ObjectId::from_content(b"target_tree");
	target_root.update_object_id();

	// -- Exec: Create same incremental stream twice
	let stream1 = create_incremental_send_stream(&target_root, &base_root)?;
	let stream2 = create_incremental_send_stream(&target_root, &base_root)?;

	// -- Check: Streams should be identical
	assert_eq!(stream1.metadata.target_root_id, stream2.metadata.target_root_id);
	assert_eq!(stream1.metadata.base_root_id, stream2.metadata.base_root_id);
	assert_eq!(stream1.metadata.object_count, stream2.metadata.object_count);

	Ok(())
}

#[test]
fn test_integration_incremental_smaller_than_full() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut base_root = Root::new();
	base_root.commit_seq = 1;
	base_root.file_tree_root.object_id = ObjectId::from_content(b"base_tree");
	base_root.alloc_tree_root.object_id = ObjectId::from_content(b"base_alloc");
	base_root.dedupe_index_root.object_id = ObjectId::from_content(b"base_dedupe");
	base_root.deadlist_root.object_id = ObjectId::from_content(b"base_deadlist");
	base_root.update_object_id();

	// Small change: only file tree modified
	let mut target_root = base_root.clone();
	target_root.commit_seq = 2;
	target_root.file_tree_root.object_id = ObjectId::from_content(b"target_tree");
	target_root.update_object_id();

	// -- Exec
	let full_stream = create_full_send_stream(&target_root)?;
	let incremental_stream = create_incremental_send_stream(&target_root, &base_root)?;

	// -- Check: Incremental should be smaller
	assert!(incremental_stream.metadata.object_count <= full_stream.metadata.object_count);
	assert!(incremental_stream.metadata.total_bytes <= full_stream.metadata.total_bytes);

	Ok(())
}

#[test]
fn test_integration_send_stream_proportionality() -> TestResult<()> {
	// -- Setup & Fixtures: Base root
	let mut base_root = Root::new();
	base_root.commit_seq = 1;
	base_root.file_tree_root.object_id = ObjectId::from_content(b"base_tree");
	base_root.alloc_tree_root.object_id = ObjectId::from_content(b"base_alloc");
	base_root.update_object_id();

	// Small change: one tree modified
	let mut small_root = base_root.clone();
	small_root.commit_seq = 2;
	small_root.file_tree_root.object_id = ObjectId::from_content(b"small_tree");
	small_root.update_object_id();

	// Medium change: two trees modified
	let mut medium_root = base_root.clone();
	medium_root.commit_seq = 3;
	medium_root.file_tree_root.object_id = ObjectId::from_content(b"medium_tree");
	medium_root.alloc_tree_root.object_id = ObjectId::from_content(b"medium_alloc");
	medium_root.update_object_id();

	// Large change: three trees modified
	let mut large_root = base_root.clone();
	large_root.commit_seq = 4;
	large_root.file_tree_root.object_id = ObjectId::from_content(b"large_tree");
	large_root.alloc_tree_root.object_id = ObjectId::from_content(b"large_alloc");
	large_root.dedupe_index_root.object_id = ObjectId::from_content(b"large_dedupe");
	large_root.update_object_id();

	// -- Exec
	let small_stream = create_incremental_send_stream(&small_root, &base_root)?;
	let medium_stream = create_incremental_send_stream(&medium_root, &base_root)?;
	let large_stream = create_incremental_send_stream(&large_root, &base_root)?;

	// -- Check: Size should be proportional to changes
	assert!(small_stream.metadata.object_count <= medium_stream.metadata.object_count);
	assert!(medium_stream.metadata.object_count <= large_stream.metadata.object_count);

	Ok(())
}

// endregion: --- Send Stream Integration Tests

// region:    --- Receive Stream Integration Tests

#[test]
fn test_integration_receive_complete_stream() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut root = Root::new();
	root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
	root.update_object_id();

	let stream = create_full_send_stream(&root)?;
	let mut receiver = ReceiveStream::new(stream.metadata.clone());

	// -- Exec: Receive all objects
	for obj in stream.ordered_objects() {
		receiver.receive_object(obj)?;
	}

	// -- Check: Stream should be complete
	assert!(receiver.is_complete());
	assert_eq!(receiver.received_count, stream.metadata.object_count);

	Ok(())
}

#[test]
fn test_integration_receive_idempotent() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut root = Root::new();
	root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
	root.update_object_id();

	let stream = create_full_send_stream(&root)?;
	let mut receiver = ReceiveStream::new(stream.metadata.clone());

	// -- Exec: Receive objects twice
	for obj in stream.ordered_objects() {
		receiver.receive_object(obj)?;
	}

	let first_count = receiver.received_count;

	for obj in stream.ordered_objects() {
		receiver.receive_object(obj)?; // Should dedupe
	}

	// -- Check: Count should not increase
	assert_eq!(receiver.received_count, first_count);

	Ok(())
}

#[test]
fn test_integration_receive_partial_then_resume() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut root = Root::new();
	root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
	root.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_tree");
	root.update_object_id();

	let stream = create_full_send_stream(&root)?;
	let mut receiver = ReceiveStream::new(stream.metadata.clone());

	let objects = stream.ordered_objects();
	let half = objects.len() / 2;

	// -- Exec: Receive first half
	for obj in &objects[..half] {
		receiver.receive_object(obj)?;
	}

	let partial_count = receiver.received_count;
	assert!(!receiver.is_complete());

	// Resume: Receive second half
	for obj in &objects[half..] {
		receiver.receive_object(obj)?;
	}

	// -- Check: Should now be complete
	assert!(receiver.is_complete());
	assert!(receiver.received_count > partial_count);

	Ok(())
}

#[test]
fn test_integration_receive_verify_all_objects() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut root = Root::new();
	root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
	root.update_object_id();

	let stream = create_full_send_stream(&root)?;
	let mut receiver = ReceiveStream::new(stream.metadata.clone());

	// -- Exec & Check: All objects should verify successfully
	for obj in stream.ordered_objects() {
		let result = receiver.receive_object(obj);
		assert!(result.is_ok(), "Object verification failed");
	}

	Ok(())
}

// endregion: --- Receive Stream Integration Tests

// region:    --- W8 End-to-End Tests

#[test]
fn test_w8_integration_full_send_receive_roundtrip() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut root = Root::new();
	root.commit_seq = 1;
	root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
	root.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_tree");
	root.update_object_id();

	// -- Exec: Send
	let send_stream = create_full_send_stream(&root)?;

	// Receive
	let mut receiver = ReceiveStream::new(send_stream.metadata.clone());
	for obj in send_stream.ordered_objects() {
		receiver.receive_object(obj)?;
	}

	// -- Check
	assert!(receiver.is_complete());
	assert_eq!(receiver.received_count, send_stream.metadata.object_count);
	assert_eq!(receiver.received_bytes, send_stream.metadata.total_bytes);

	Ok(())
}

#[test]
fn test_w8_integration_incremental_send_receive_roundtrip() -> TestResult<()> {
	// -- Setup & Fixtures
	let mut base_root = Root::new();
	base_root.commit_seq = 1;
	base_root.file_tree_root.object_id = ObjectId::from_content(b"base_tree");
	base_root.update_object_id();

	let mut target_root = base_root.clone();
	target_root.commit_seq = 2;
	target_root.file_tree_root.object_id = ObjectId::from_content(b"target_tree");
	target_root.update_object_id();

	// -- Exec: Send incremental
	let send_stream = create_incremental_send_stream(&target_root, &base_root)?;

	// Receive
	let mut receiver = ReceiveStream::new(send_stream.metadata.clone());
	for obj in send_stream.ordered_objects() {
		receiver.receive_object(obj)?;
	}

	// -- Check
	assert!(receiver.is_complete());
	assert_eq!(receiver.received_count, send_stream.metadata.object_count);

	Ok(())
}

// endregion: --- W8 End-to-End Tests
