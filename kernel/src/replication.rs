//! Send and Receive - Replication for LedgerFS v7
//!
//! # Status
//!
//! The stream framing, ordering, and the diff between two roots are
//! implemented, and the determinism guarantees below hold. What is not
//! implemented is the transfer: reachability is one level deep, and each
//! entry's payload is a placeholder rather than an object, so a receive
//! reconstructs nothing. Nothing in the host `Store` calls any of it —
//! `KNOWN_ISSUES.md` §11.
//!
//! Per §31 of FORMAT.md:
//! - **Send streams**: Encode incremental changes between snapshots
//! - **Receive validation**: Verify stream checksums and activate new Root atomically
//!
//! ## Send Stream Encoder (object-set diff)
//!
//! Walk reachable set for target Root, diff with base Root, stream objects
//! in dependency order with advisory locality hints.
//!
//! ## Receive Decoder + Placement + Activation
//!
//! Verify each object, dedupe incoming, place using receiver allocation,
//! activate new Root at end.
//!
//! ## Determinism Guarantees
//!
//! - Use BTreeMap for object ordering (dependency-sorted, deterministic)
//! - Use BTreeSet for visited tracking (sorted enumeration)
//! - Same input roots → same byte-identical send stream

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::object::{ObjectId, Root};
use crate::{Error, Result};

// region:    --- Send Stream Types

/// Send stream format version
pub const SEND_STREAM_VERSION: u16 = 1;

/// Send stream mode (full vs incremental)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendMode {
	/// Full send - transmit entire snapshot content
	Full = 0,
	/// Incremental send - transmit only changed objects since base
	Incremental = 1,
}

/// Send stream metadata (deterministic header)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendStreamMetadata {
	/// Schema version for compatibility tracking
	pub schema_version: u16,

	/// Send mode (full or incremental)
	pub mode: SendMode,

	/// Target root being sent
	pub target_root_id: ObjectId,

	/// Base root for incremental (zero for full send)
	pub base_root_id: ObjectId,

	/// Total object count in stream
	pub object_count: u64,

	/// Total bytes in stream (sum of all object sizes)
	pub total_bytes: u64,

	/// Timestamp when stream was created (UNIX nanoseconds)
	pub timestamp_ns: u64,
}

impl SendStreamMetadata {
	/// Create new send stream metadata
	pub fn new(mode: SendMode, target_root_id: ObjectId, base_root_id: ObjectId) -> Self {
		Self {
			schema_version: SEND_STREAM_VERSION,
			mode,
			target_root_id,
			base_root_id,
			object_count: 0,
			total_bytes: 0,
			// Zero means unset. This crate is `no_std` and has no clock, so the
			// caller that knows the time sets the field after construction; it
			// is `pub` and nothing in this module reads it back.
			timestamp_ns: 0,
		}
	}
}

/// Object in send stream with dependency ordering
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendStreamObject {
	/// Object ID
	pub object_id: ObjectId,

	/// Object data (canonical encoding)
	pub data: Vec<u8>,

	/// Advisory storage hint (may be zero)
	pub hint: u64,

	/// Object dependencies (must appear before this object in stream)
	pub dependencies: Vec<ObjectId>,
}

impl SendStreamObject {
	/// Create new send stream object
	pub fn new(object_id: ObjectId, data: Vec<u8>) -> Self {
		Self {
			object_id,
			data,
			hint: 0,
			dependencies: Vec::new(),
		}
	}

	/// Add dependency
	pub fn add_dependency(&mut self, dep_id: ObjectId) {
		if !self.dependencies.contains(&dep_id) {
			self.dependencies.push(dep_id);
		}
	}
}

/// Complete send stream (metadata + objects)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendStream {
	/// Stream metadata
	pub metadata: SendStreamMetadata,

	/// Objects in dependency order (BTreeMap for determinism)
	pub objects: BTreeMap<ObjectId, SendStreamObject>,
}

impl SendStream {
	/// Create new send stream
	pub fn new(metadata: SendStreamMetadata) -> Self {
		Self {
			metadata,
			objects: BTreeMap::new(),
		}
	}

	/// Add object to stream
	pub fn add_object(&mut self, obj: SendStreamObject) {
		self.metadata.object_count += 1;
		self.metadata.total_bytes += obj.data.len() as u64;
		self.objects.insert(obj.object_id, obj);
	}

	/// Get objects in dependency order (deterministic)
	pub fn ordered_objects(&self) -> Vec<&SendStreamObject> {
		// BTreeMap iteration is already sorted by ObjectId
		// Dependencies ensure correct ordering
		self.objects.values().collect()
	}
}

// endregion: --- Send Stream Types

// region:    --- Send Stream Encoder

/// Walk reachable object set from root
///
/// Returns the ids reachable from `root` in one step — the root's own
/// reference fields — in a `BTreeSet`, so iteration is deterministic. The queue
/// is not extended by following a reference into a child object, which would
/// need that object's canonical bytes read off a device this module does not
/// have. The name says reachable set; the behaviour is one level.
pub fn walk_reachable_set(root: &Root) -> BTreeSet<ObjectId> {
	let mut reachable = BTreeSet::new();
	// Start with root's tree references
	let mut queue = alloc::vec![
		root.file_tree_root.object_id,
		root.alloc_tree_root.object_id,
		root.dedupe_index_root.object_id,
		root.deadlist_root.object_id,
	];

	while let Some(obj_id) = queue.pop() {
		// Skip zero references and already visited
		if obj_id.is_zero() || reachable.contains(&obj_id) {
			continue;
		}

		reachable.insert(obj_id);

		// Only the root's own reference fields go into the queue. Following a
		// reference *into* a child object is not done, because doing it means
		// reading and decoding that object's canonical form and this module has
		// no block device to read from. So what this returns is the set
		// reachable from the root in one step, not the reachable set — which is
		// why `KNOWN_ISSUES.md` §11 lists replication as not wired in.
	}

	reachable
}

/// Diff two roots to find objects in target but not in base
///
/// Returns set of ObjectIds that need to be transmitted for incremental send.
/// Uses BTreeSet for deterministic ordering.
pub fn diff_roots(target_root: &Root, base_root: &Root) -> BTreeSet<ObjectId> {
	let target_set = walk_reachable_set(target_root);
	let base_set = walk_reachable_set(base_root);

	// Objects in target but not in base
	target_set.difference(&base_set).copied().collect()
}

/// Create send stream from target root (full send)
///
/// Enumerates the root's own references (see [`walk_reachable_set`]) and emits
/// one entry per id. **The entries are placeholders**: their payload is an id's
/// bytes rather than an object's contents, so a receiver reconstructs nothing.
/// The encoder is here for its shape — ordering, framing, the self-consistency
/// check — not as a working transfer. See `KNOWN_ISSUES.md` §11.
pub fn create_full_send_stream(target_root: &Root) -> Result<SendStream> {
	let metadata = SendStreamMetadata::new(SendMode::Full, target_root.object_id, ObjectId::zero());

	let mut stream = SendStream::new(metadata);
	let reachable = walk_reachable_set(target_root);

	// Add all reachable objects to stream.
	//
	// Each entry's payload is the bytes of an object id and its own id is the
	// hash of that payload, so the entry is self-consistent — `receive_object`
	// checks `blake3(data) == object_id` and the check passes. Neither the
	// payload nor the id is the object's, though: the object's canonical bytes
	// are not loaded from anywhere, because this module has no device to load
	// them from. A receiver of this stream reconstructs nothing.
	for obj_id in reachable {
		let data = obj_id.as_bytes().to_vec();
		let placeholder_id = ObjectId::from_content(&data);
		let obj = SendStreamObject::new(placeholder_id, data);
		stream.add_object(obj);
	}

	Ok(stream)
}

/// Create send stream from target and base roots (incremental send)
///
/// Diffs the two roots' one-step reference sets and emits the difference, with
/// the same placeholder payloads as [`create_full_send_stream`]. The diff is
/// the part that is real; what it hands to the receiver is not.
pub fn create_incremental_send_stream(target_root: &Root, base_root: &Root) -> Result<SendStream> {
	let metadata = SendStreamMetadata::new(SendMode::Incremental, target_root.object_id, base_root.object_id);

	let mut stream = SendStream::new(metadata);
	let diff_set = diff_roots(target_root, base_root);

	// Add only changed objects to stream.
	//
	// Same substitution as the full send above: the payload is an id's bytes
	// and the entry's id is that payload's hash, so the entry is consistent
	// with itself and with nothing else. The diff itself is also one step deep
	// — see `walk_reachable_set`.
	for obj_id in diff_set {
		let data = obj_id.as_bytes().to_vec();
		let placeholder_id = ObjectId::from_content(&data);
		let obj = SendStreamObject::new(placeholder_id, data);
		stream.add_object(obj);
	}

	Ok(stream)
}

// endregion: --- Send Stream Encoder

// region:    --- Receive Stream Types

/// Receive stream decoder state
#[derive(Debug)]
pub struct ReceiveStream {
	/// Stream metadata
	pub metadata: SendStreamMetadata,

	/// Received objects (for deduplication)
	received_objects: BTreeSet<ObjectId>,

	/// Objects received so far
	pub received_count: u64,

	/// Bytes received so far
	pub received_bytes: u64,
}

impl ReceiveStream {
	/// Create new receive stream
	pub fn new(metadata: SendStreamMetadata) -> Self {
		Self {
			metadata,
			received_objects: BTreeSet::new(),
			received_count: 0,
			received_bytes: 0,
		}
	}

	/// Receive and verify object
	///
	/// Returns true if object is new (not a duplicate).
	pub fn receive_object(&mut self, obj: &SendStreamObject) -> Result<bool> {
		// Verify object hash matches content
		let computed_id = ObjectId::from_content(&obj.data);
		if computed_id != obj.object_id {
			return Err(Error::ObjectIdMismatch);
		}

		// Check for duplicate
		if self.received_objects.contains(&obj.object_id) {
			return Ok(false); // Duplicate, skip
		}

		// Add to received set
		self.received_objects.insert(obj.object_id);
		self.received_count += 1;
		self.received_bytes += obj.data.len() as u64;

		Ok(true) // New object
	}

	/// Check if stream is complete
	pub fn is_complete(&self) -> bool {
		self.received_count >= self.metadata.object_count
	}
}

// endregion: --- Receive Stream Types

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	// region:    --- Send Stream Tests

	#[test]
	fn test_send_stream_metadata_schema_version() -> TestResult<()> {
		// -- Setup & Fixtures
		let metadata = SendStreamMetadata::new(SendMode::Full, ObjectId::from_content(b"target"), ObjectId::zero());

		// -- Check
		assert_eq!(metadata.schema_version, SEND_STREAM_VERSION);

		Ok(())
	}

	#[test]
	fn test_send_stream_deterministic_object_ordering() -> TestResult<()> {
		// -- Setup & Fixtures
		let metadata = SendStreamMetadata::new(SendMode::Full, ObjectId::from_content(b"target"), ObjectId::zero());
		let mut stream = SendStream::new(metadata);

		// Add objects in non-sorted order
		let obj3 = SendStreamObject::new(ObjectId::from_content(b"zzz"), Vec::new());
		let obj1 = SendStreamObject::new(ObjectId::from_content(b"aaa"), Vec::new());
		let obj2 = SendStreamObject::new(ObjectId::from_content(b"mmm"), Vec::new());

		stream.add_object(obj3);
		stream.add_object(obj1);
		stream.add_object(obj2);

		// -- Exec
		let ordered = stream.ordered_objects();
		let ids: Vec<_> = ordered.iter().map(|o| o.object_id).collect();

		// -- Check: BTreeMap should return sorted order
		assert_eq!(ids.len(), 3);
		assert!(ids[0] < ids[1]);
		assert!(ids[1] < ids[2]);

		Ok(())
	}

	#[test]
	fn test_walk_reachable_set_basic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut root = Root::new();
		root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
		root.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_tree");

		// -- Exec
		let reachable = walk_reachable_set(&root);

		// -- Check: Should contain both tree roots
		assert!(reachable.contains(&root.file_tree_root.object_id));
		assert!(reachable.contains(&root.alloc_tree_root.object_id));

		Ok(())
	}

	#[test]
	fn test_diff_roots_incremental() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut base_root = Root::new();
		base_root.file_tree_root.object_id = ObjectId::from_content(b"base_tree");

		let mut target_root = base_root.clone();
		target_root.file_tree_root.object_id = ObjectId::from_content(b"target_tree");

		// -- Exec
		let diff = diff_roots(&target_root, &base_root);

		// -- Check: Should only contain changed tree
		assert!(diff.contains(&target_root.file_tree_root.object_id));
		assert!(!diff.contains(&base_root.file_tree_root.object_id));

		Ok(())
	}

	#[test]
	fn test_create_full_send_stream() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut root = Root::new();
		root.file_tree_root.object_id = ObjectId::from_content(b"file_tree");
		root.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_tree");

		// -- Exec
		let stream = create_full_send_stream(&root)?;

		// -- Check
		assert_eq!(stream.metadata.mode, SendMode::Full);
		assert_eq!(stream.metadata.target_root_id, root.object_id);
		assert!(stream.metadata.base_root_id.is_zero());
		assert!(stream.metadata.object_count > 0);

		Ok(())
	}

	#[test]
	fn test_create_incremental_send_stream() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut base_root = Root::new();
		base_root.file_tree_root.object_id = ObjectId::from_content(b"base_tree");

		let mut target_root = base_root.clone();
		target_root.commit_seq = 1;
		target_root.file_tree_root.object_id = ObjectId::from_content(b"target_tree");
		target_root.update_object_id();

		// -- Exec
		let stream = create_incremental_send_stream(&target_root, &base_root)?;

		// -- Check
		assert_eq!(stream.metadata.mode, SendMode::Incremental);
		assert_eq!(stream.metadata.target_root_id, target_root.object_id);
		assert_eq!(stream.metadata.base_root_id, base_root.object_id);

		Ok(())
	}

	#[test]
	fn test_incremental_size_proportionality() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut base_root = Root::new();
		base_root.file_tree_root.object_id = ObjectId::from_content(b"tree_1");
		base_root.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_1");

		// Small change: only one tree modified
		let mut small_change_root = base_root.clone();
		small_change_root.commit_seq = 1;
		small_change_root.file_tree_root.object_id = ObjectId::from_content(b"tree_2");
		small_change_root.update_object_id();

		// Large change: both trees modified
		let mut large_change_root = base_root.clone();
		large_change_root.commit_seq = 2;
		large_change_root.file_tree_root.object_id = ObjectId::from_content(b"tree_3");
		large_change_root.alloc_tree_root.object_id = ObjectId::from_content(b"alloc_2");
		large_change_root.update_object_id();

		// -- Exec
		let small_stream = create_incremental_send_stream(&small_change_root, &base_root)?;
		let large_stream = create_incremental_send_stream(&large_change_root, &base_root)?;

		// -- Check: Large change should have more objects
		assert!(large_stream.metadata.object_count >= small_stream.metadata.object_count);

		Ok(())
	}

	// endregion: --- Send Stream Tests

	// region:    --- Receive Stream Tests

	#[test]
	fn test_receive_object_verification() -> TestResult<()> {
		// -- Setup & Fixtures
		let metadata = SendStreamMetadata::new(SendMode::Full, ObjectId::from_content(b"target"), ObjectId::zero());
		let mut receiver = ReceiveStream::new(metadata);

		let data = b"test data".to_vec();
		let obj = SendStreamObject::new(ObjectId::from_content(&data), data);

		// -- Exec
		let is_new = receiver.receive_object(&obj)?;

		// -- Check
		assert!(is_new);
		assert_eq!(receiver.received_count, 1);

		Ok(())
	}

	#[test]
	fn test_receive_object_hash_mismatch() -> TestResult<()> {
		// -- Setup & Fixtures
		let metadata = SendStreamMetadata::new(SendMode::Full, ObjectId::from_content(b"target"), ObjectId::zero());
		let mut receiver = ReceiveStream::new(metadata);

		// Create object with mismatched hash
		let obj = SendStreamObject::new(ObjectId::from_content(b"wrong data"), b"actual data".to_vec());

		// -- Exec & Check
		let result = receiver.receive_object(&obj);
		assert!(result.is_err());
		assert!(matches!(result.unwrap_err(), Error::ObjectIdMismatch));

		Ok(())
	}

	#[test]
	fn test_receive_object_deduplication() -> TestResult<()> {
		// -- Setup & Fixtures
		let metadata = SendStreamMetadata::new(SendMode::Full, ObjectId::from_content(b"target"), ObjectId::zero());
		let mut receiver = ReceiveStream::new(metadata);

		let data = b"test data".to_vec();
		let obj = SendStreamObject::new(ObjectId::from_content(&data), data);

		// -- Exec: Receive same object twice
		let is_new_1 = receiver.receive_object(&obj)?;
		let is_new_2 = receiver.receive_object(&obj)?;

		// -- Check: First should be new, second should be duplicate
		assert!(is_new_1);
		assert!(!is_new_2);
		assert_eq!(receiver.received_count, 1); // Only counted once

		Ok(())
	}

	#[test]
	fn test_receive_stream_idempotency() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut metadata = SendStreamMetadata::new(SendMode::Full, ObjectId::from_content(b"target"), ObjectId::zero());
		metadata.object_count = 2;

		let mut receiver = ReceiveStream::new(metadata);

		let obj1 = SendStreamObject::new(ObjectId::from_content(b"obj1"), b"obj1".to_vec());
		let obj2 = SendStreamObject::new(ObjectId::from_content(b"obj2"), b"obj2".to_vec());

		// -- Exec: Receive objects multiple times
		receiver.receive_object(&obj1)?;
		receiver.receive_object(&obj2)?;
		receiver.receive_object(&obj1)?; // Duplicate
		receiver.receive_object(&obj2)?; // Duplicate

		// -- Check: Should only count unique objects
		assert_eq!(receiver.received_count, 2);
		assert!(receiver.is_complete());

		Ok(())
	}

	// endregion: --- Receive Stream Tests
}

// endregion: --- Tests
