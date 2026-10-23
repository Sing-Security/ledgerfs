//! ObjectStore trait and InMemoryObjectStore for LedgerFS CoW operations.
//!
//! All CoW read operations need a way to resolve an [`ObjectRef`] to its
//! canonical bytes so they can decode [`Inode`], [`DirNode`], [`ExtentNode`],
//! and [`Chunk`] objects.  This trait abstracts that access, allowing both
//! an in-memory store (for tests) and a disk-backed store (for runtime).

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::cow::transaction::TxnPlan;
use crate::error::{Error, Result};
use crate::object::{Chunk, DirNode, ExtentNode, Inode, ObjectId, ObjectRef};

// region:    --- ObjectStore trait

/// Trait for resolving [`ObjectRef`]s to their decoded objects.
///
/// Implementations must be deterministic: the same [`ObjectRef`] always
/// resolves to the same bytes (objects are content-addressed).
pub trait ObjectStore {
	/// Load and decode an [`Inode`] by reference.
	///
	/// # Errors
	///
	/// Returns [`Error::NotFound`] when `obj_ref` is not present in this store.
	/// Returns an encoding error if the stored bytes are not a valid [`Inode`].
	fn get_inode(&self, obj_ref: ObjectRef) -> Result<Inode>;

	/// Load and decode a [`DirNode`] by reference.
	///
	/// # Errors
	///
	/// Returns [`Error::NotFound`] when `obj_ref` is not present in this store.
	/// Returns an encoding error if the stored bytes are not a valid [`DirNode`].
	fn get_dir_node(&self, obj_ref: ObjectRef) -> Result<DirNode>;

	/// Load and decode an [`ExtentNode`] by reference.
	///
	/// # Errors
	///
	/// Returns [`Error::NotFound`] when `obj_ref` is not present in this store.
	/// Returns an encoding error if the stored bytes are not a valid [`ExtentNode`].
	fn get_extent_node(&self, obj_ref: ObjectRef) -> Result<ExtentNode>;

	/// Load and decode a [`Chunk`] by reference.
	///
	/// # Errors
	///
	/// Returns [`Error::NotFound`] when `obj_ref` is not present in this store.
	/// Returns an encoding error if the stored bytes are not a valid [`Chunk`].
	fn get_chunk(&self, obj_ref: ObjectRef) -> Result<Chunk>;

	/// Record where `object_id` was physically placed by a commit, so later
	/// lookups can go straight to that block instead of scanning for it.
	///
	/// Defaults to a no-op: only stores whose lookups are actually
	/// placement-sensitive (a real disk-backed one) have anything to do
	/// here; [`InMemoryObjectStore`] finds every object by hash directly.
	/// Because it's purely an accelerator, a store that ignores this stays
	/// correct; it just pays a scan per lookup. Callers should invoke it
	/// for every entry in a `CommitResult`'s `storage_hints`.
	fn record_hint(&self, _object_id: ObjectId, _physical_lba: u64) {}
}

// endregion: --- ObjectStore trait

// region:    --- InMemoryObjectStore

/// In-memory [`ObjectStore`] backed by a [`BTreeMap`].
///
/// Used in tests and for single-process usage where all objects fit in RAM.
/// Objects are keyed by [`ObjectId`] (content-addressed).
#[derive(Debug, Default, Clone)]
pub struct InMemoryObjectStore {
	objects: BTreeMap<ObjectId, Vec<u8>>,
}

impl InMemoryObjectStore {
	/// Create a new, empty store.
	pub fn new() -> Self {
		Self {
			objects: BTreeMap::new(),
		}
	}

	/// Insert raw canonical bytes for an object.
	pub fn insert(&mut self, id: ObjectId, bytes: Vec<u8>) {
		self.objects.insert(id, bytes);
	}

	/// Absorb all new objects from a committed [`TxnPlan`] into this store.
	///
	/// Call this after successfully committing a transaction so that subsequent
	/// reads can resolve the new objects.
	pub fn apply_txn(&mut self, plan: &TxnPlan) {
		for (id, obj) in &plan.new_objects {
			self.objects.insert(*id, obj.canonical_bytes.clone());
		}
	}
}

impl ObjectStore for InMemoryObjectStore {
	fn get_inode(&self, obj_ref: ObjectRef) -> Result<Inode> {
		let bytes = self
			.objects
			.get(&obj_ref.object_id)
			.ok_or_else(|| Error::not_found("inode not in store"))?;
		Inode::decode_canonical(bytes)
	}

	fn get_dir_node(&self, obj_ref: ObjectRef) -> Result<DirNode> {
		let bytes = self
			.objects
			.get(&obj_ref.object_id)
			.ok_or_else(|| Error::not_found("dir_node not in store"))?;
		DirNode::decode_canonical(bytes)
	}

	fn get_extent_node(&self, obj_ref: ObjectRef) -> Result<ExtentNode> {
		let bytes = self
			.objects
			.get(&obj_ref.object_id)
			.ok_or_else(|| Error::not_found("extent_node not in store"))?;
		ExtentNode::decode_canonical(bytes)
	}

	fn get_chunk(&self, obj_ref: ObjectRef) -> Result<Chunk> {
		let bytes = self
			.objects
			.get(&obj_ref.object_id)
			.ok_or_else(|| Error::not_found("chunk not in store"))?;
		Chunk::decode_canonical(bytes)
	}
}

// endregion: --- InMemoryObjectStore

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use crate::object::Root;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	#[test]
	fn test_object_store_insert_and_get_chunk() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let chunk = Chunk::new(vec![1, 2, 3, 4, 5]);
		let canonical = chunk.encode_canonical()?;
		store.insert(chunk.object_id, canonical);

		// -- Exec
		let retrieved = store.get_chunk(ObjectRef::from_id(chunk.object_id))?;

		// -- Check
		assert_eq!(retrieved.data, chunk.data);
		assert_eq!(retrieved.object_id, chunk.object_id);

		Ok(())
	}

	#[test]
	fn test_object_store_missing_ref_returns_error() -> TestResult<()> {
		// -- Setup & Fixtures
		let store = InMemoryObjectStore::new();
		let missing_ref = ObjectRef::from_id(ObjectId::zero());

		// -- Exec
		let result = store.get_inode(missing_ref);

		// -- Check
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_object_store_apply_txn() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();

		// Create a txn that adds a chunk
		let mut plan = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());
		let chunk = Chunk::new(vec![10, 20, 30]);
		plan.add_object(crate::cow::transaction::NewObject::new(
			chunk.object_id,
			chunk.encode_canonical()?,
		));

		// -- Exec
		store.apply_txn(&plan);

		// -- Check
		let retrieved = store.get_chunk(ObjectRef::from_id(chunk.object_id))?;
		assert_eq!(retrieved.data, chunk.data);

		Ok(())
	}
}

// endregion: --- Tests
