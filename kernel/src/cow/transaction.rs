//! Transaction plan for CoW operations
//!
//! TxnPlan lists newly-created objects with their canonical bytes.
//! Phase 1 will later place/write them as ObjectRecords and produce StorageHints.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::object::{ObjectId, ObjectRef, Root};

/// New object to be written in transaction
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewObject {
	/// Object ID (content-addressed)
	pub object_id: ObjectId,

	/// Canonical bytes of the object
	pub canonical_bytes: Vec<u8>,

	/// Objects this depends on (must be written first)
	pub dependencies: Vec<ObjectId>,
}

impl NewObject {
	/// Create new object entry
	pub fn new(object_id: ObjectId, canonical_bytes: Vec<u8>) -> Self {
		Self {
			object_id,
			canonical_bytes,
			dependencies: Vec::new(),
		}
	}

	/// Add dependency
	pub fn with_dependency(mut self, dep: ObjectId) -> Self {
		self.dependencies.push(dep);
		self
	}

	/// Add multiple dependencies
	pub fn with_dependencies(mut self, deps: Vec<ObjectId>) -> Self {
		self.dependencies.extend(deps);
		self
	}
}

/// Transaction plan for CoW operations
///
/// Contains all new objects to write and updated root.
/// Phase 1 will use this to actually place objects on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxnPlan {
	/// Previous root (before this transaction)
	pub prev_root: ObjectRef,

	/// New root (after this transaction)
	pub new_root: Root,

	/// Newly created objects (ordered by dependencies)
	/// Uses BTreeMap for deterministic iteration order
	pub new_objects: BTreeMap<ObjectId, NewObject>,

	/// Total size of all new objects (bytes)
	pub total_size: u64,
}

impl TxnPlan {
	/// Create new transaction plan
	pub fn new(prev_root: ObjectRef, new_root: Root) -> Self {
		Self {
			prev_root,
			new_root,
			new_objects: BTreeMap::new(),
			total_size: 0,
		}
	}

	/// Add object to transaction
	pub fn add_object(&mut self, object: NewObject) {
		self.total_size += object.canonical_bytes.len() as u64;
		self.new_objects.insert(object.object_id, object);
	}

	/// Get objects in dependency order
	///
	/// Returns objects sorted so that dependencies come before dependents.
	/// Uses topological sort.
	pub fn objects_in_dependency_order(&self) -> Result<Vec<&NewObject>> {
		let mut result = Vec::new();
		let mut visited = BTreeMap::new();
		let mut in_progress = BTreeMap::new();

		// Topological sort via DFS
		for obj_id in self.new_objects.keys() {
			self.visit_object(*obj_id, &mut visited, &mut in_progress, &mut result)?;
		}

		Ok(result)
	}

	fn visit_object<'a>(
		&'a self,
		obj_id: ObjectId,
		visited: &mut BTreeMap<ObjectId, ()>,
		in_progress: &mut BTreeMap<ObjectId, ()>,
		result: &mut Vec<&'a NewObject>,
	) -> Result<()> {
		if visited.contains_key(&obj_id) {
			return Ok(());
		}

		if in_progress.contains_key(&obj_id) {
			return Err(Error::invalid_operation("Circular dependency detected"));
		}

		in_progress.insert(obj_id, ());

		// Visit dependencies first
		if let Some(obj) = self.new_objects.get(&obj_id) {
			for dep_id in &obj.dependencies {
				self.visit_object(*dep_id, visited, in_progress, result)?;
			}

			result.push(obj);
			visited.insert(obj_id, ());
		}

		in_progress.remove(&obj_id);

		Ok(())
	}

	/// Encode transaction plan to bytes
	pub fn encode(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode transaction plan from bytes
	pub fn decode(bytes: &[u8]) -> Result<Self> {
		postcard::from_bytes(bytes).map_err(Error::from)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::object::Chunk;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	#[test]
	fn test_txnplan_new() -> TestResult<()> {
		// -- Setup & Fixtures
		let prev_root = ObjectRef::zero();
		let new_root = Root::new();

		// -- Exec
		let plan = TxnPlan::new(prev_root, new_root.clone());

		// -- Check
		assert_eq!(plan.prev_root, prev_root);
		assert_eq!(plan.new_root, new_root);
		assert!(plan.new_objects.is_empty());
		assert_eq!(plan.total_size, 0);

		Ok(())
	}

	#[test]
	fn test_txnplan_add_object() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		let chunk = Chunk::new(vec![1, 2, 3, 4, 5]);
		let canonical = chunk.encode_canonical()?;
		let new_obj = NewObject::new(chunk.object_id, canonical);

		// -- Exec
		plan.add_object(new_obj.clone());

		// -- Check
		assert_eq!(plan.new_objects.len(), 1);
		assert!(plan.new_objects.contains_key(&chunk.object_id));
		assert!(plan.total_size > 0);

		Ok(())
	}

	#[test]
	fn test_txnplan_dependency_order_simple() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		let chunk1 = Chunk::new(vec![1, 2, 3]);
		let chunk2 = Chunk::new(vec![4, 5, 6]);

		let obj1 = NewObject::new(chunk1.object_id, chunk1.encode_canonical()?);
		let obj2 = NewObject::new(chunk2.object_id, chunk2.encode_canonical()?).with_dependency(chunk1.object_id);

		// Add in reverse order
		plan.add_object(obj2);
		plan.add_object(obj1);

		// -- Exec
		let ordered = plan.objects_in_dependency_order()?;

		// -- Check - chunk1 should come before chunk2
		assert_eq!(ordered.len(), 2);
		assert_eq!(ordered[0].object_id, chunk1.object_id);
		assert_eq!(ordered[1].object_id, chunk2.object_id);

		Ok(())
	}

	#[test]
	fn test_txnplan_encode_decode() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		let chunk = Chunk::new(vec![1, 2, 3]);
		let obj = NewObject::new(chunk.object_id, chunk.encode_canonical()?);
		plan.add_object(obj);

		// -- Exec
		let encoded = plan.encode()?;
		let decoded = TxnPlan::decode(&encoded)?;

		// -- Check
		assert_eq!(decoded.new_objects.len(), plan.new_objects.len());
		assert_eq!(decoded.total_size, plan.total_size);

		Ok(())
	}

	#[test]
	fn test_txnplan_btreemap_iteration_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut plan = TxnPlan::new(ObjectRef::zero(), Root::new());

		// Add objects in random order
		let chunk1 = Chunk::new(vec![1]);
		let chunk2 = Chunk::new(vec![2]);
		let chunk3 = Chunk::new(vec![3]);

		plan.add_object(NewObject::new(chunk2.object_id, chunk2.encode_canonical()?));
		plan.add_object(NewObject::new(chunk1.object_id, chunk1.encode_canonical()?));
		plan.add_object(NewObject::new(chunk3.object_id, chunk3.encode_canonical()?));

		// -- Exec - Iterate twice
		let keys1: Vec<_> = plan.new_objects.keys().copied().collect();
		let keys2: Vec<_> = plan.new_objects.keys().copied().collect();

		// -- Check - Should be identical (BTreeMap guarantees sorted order)
		assert_eq!(keys1, keys2);

		Ok(())
	}
}
