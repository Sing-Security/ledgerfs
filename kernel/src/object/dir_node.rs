//! DirNode - B-tree for mapping directory entry names to inode references

extern crate alloc;

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use super::object_id::{ObjectId, ObjectRef, StorageHint};
use crate::error::{Error, Result};

/// Directory entry - maps name to inode reference
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DirEntry {
	/// Entry name (filename)
	pub name: heapless::String<256>,

	/// Inode object ID
	pub inode_id: ObjectId,

	/// Storage hint for inode
	pub inode_hint: StorageHint,
}

impl DirEntry {
	/// Create new directory entry
	pub fn new(name: &str, inode_ref: ObjectRef) -> Result<Self> {
		let name_str: heapless::String<256> =
			name.try_into().map_err(|_| Error::invalid_encoding("filename too long"))?;

		Ok(Self {
			name: name_str,
			inode_id: inode_ref.object_id,
			inode_hint: inode_ref.hint,
		})
	}

	/// Get inode reference
	pub fn inode_ref(&self) -> ObjectRef {
		ObjectRef::new(self.inode_id, self.inode_hint)
	}
}

/// DirNode - B-tree node for directory entries
///
/// Entries are sorted by name for deterministic ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirNode {
	/// Content-addressed ID of this node — `#[serde(skip)]`, matching
	/// `AllocNode`'s convention: see `Inode::object_id`'s doc comment for
	/// why a self-referential field can't be part of its own hash input.
	#[serde(skip)]
	pub object_id: ObjectId,

	/// Is this a leaf node?
	pub is_leaf: bool,

	/// Directory entries (sorted by name)
	pub entries: Vec<DirEntry>,

	/// Child node references (for internal nodes)
	/// Length is entries.len() + 1 for internal nodes
	pub children: Vec<ObjectRef>,
}

impl DirNode {
	/// Maximum canonical size (128 KiB as per FORMAT.md)
	pub const MAX_CANONICAL_SIZE: usize = 128 * 1024;

	/// Approximate max entries to stay under MAX_CANONICAL_SIZE
	/// Each entry is ~300 bytes, so ~400 entries max
	pub const MAX_ENTRIES: usize = 400;

	/// Create new empty leaf node
	pub fn new_leaf() -> Self {
		let mut node = Self {
			object_id: ObjectId::zero(),
			is_leaf: true,
			entries: Vec::new(),
			children: Vec::new(),
		};
		node.update_object_id();
		node
	}

	/// Create new internal node
	pub fn new_internal(entries: Vec<DirEntry>, children: Vec<ObjectRef>) -> Self {
		let mut node = Self {
			object_id: ObjectId::zero(),
			is_leaf: false,
			entries,
			children,
		};
		node.update_object_id();
		node
	}

	/// Insert directory entry into leaf node
	pub fn insert_entry(&mut self, entry: DirEntry) -> Result<()> {
		if !self.is_leaf {
			return Err(Error::invalid_operation("Cannot insert into internal node"));
		}

		// Find insertion point (maintain sorted order by name)
		let pos = self
			.entries
			.binary_search_by(|e| e.name.cmp(&entry.name))
			.unwrap_or_else(|pos| pos);

		// Check for duplicate
		if pos < self.entries.len() && self.entries[pos].name == entry.name {
			return Err(Error::invalid_operation("Duplicate entry name"));
		}

		self.entries.insert(pos, entry);
		self.update_object_id();

		Ok(())
	}

	/// Remove directory entry by name
	pub fn remove_entry(&mut self, name: &str) -> Result<DirEntry> {
		if !self.is_leaf {
			return Err(Error::invalid_operation("Cannot remove from internal node"));
		}

		let name_str: heapless::String<256> =
			name.try_into().map_err(|_| Error::invalid_encoding("filename too long"))?;

		let pos = self
			.entries
			.binary_search_by(|e| e.name.cmp(&name_str))
			.map_err(|_| Error::btree_key_not_found(name))?;

		let removed = self.entries.remove(pos);
		self.update_object_id();

		Ok(removed)
	}

	/// Lookup entry by name
	pub fn lookup(&self, name: &str) -> Option<&DirEntry> {
		let name_str: heapless::String<256> = name.try_into().ok()?;
		self.entries.iter().find(|e| e.name == name_str)
	}

	/// Check if node needs splitting
	pub fn needs_split(&self) -> bool {
		self.entries.len() > Self::MAX_ENTRIES
			|| self.encode_canonical().map(|b| b.len()).unwrap_or(0) > Self::MAX_CANONICAL_SIZE
	}

	/// Split node at median (deterministic split policy)
	pub fn split(&self) -> Result<(DirNode, DirNode)> {
		if !self.needs_split() {
			return Err(Error::invalid_operation("Node does not need splitting"));
		}

		let median = self.entries.len() / 2;

		let left_entries = self.entries[..median].to_vec();
		let right_entries = self.entries[median..].to_vec();

		let (left_children, right_children) = if self.is_leaf {
			(Vec::new(), Vec::new())
		} else {
			let left_children = self.children[..=median].to_vec();
			let right_children = self.children[median..].to_vec();
			(left_children, right_children)
		};

		let left = Self::new_internal(left_entries, left_children);
		let right = Self::new_internal(right_entries, right_children);

		Ok((left, right))
	}

	/// Update object ID based on current content. See `Inode::update_object_id`'s
	/// doc comment — safe in any order since `object_id` is `#[serde(skip)]`.
	pub fn update_object_id(&mut self) {
		let canonical = self.encode_canonical().unwrap();
		self.object_id = ObjectId::from_content(&canonical);
	}

	/// Encode, adopt the object id those bytes hash to, and hand the bytes
	/// back — the one-pass form of `update_object_id` + `encode_canonical`.
	pub fn seal_canonical(&mut self) -> Result<Vec<u8>> {
		let canonical = self.encode_canonical()?;
		self.object_id = ObjectId::from_content(&canonical);
		Ok(canonical)
	}

	/// Encode node to canonical bytes. `object_id` itself is never part of
	/// the output (`#[serde(skip)]`).
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode node from canonical bytes, recomputing `object_id` directly
	/// from `bytes` — see `Inode::decode_canonical`'s doc comment.
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		let mut node: Self = postcard::from_bytes(bytes).map_err(Error::from)?;
		node.object_id = ObjectId::from_content(bytes);
		Ok(node)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	/// What does an entry actually cost, and how many really fit?
	///
	/// `MAX_ENTRIES` is 400, derived from a comment estimating "~300 bytes"
	/// per entry against a 128 KiB canonical ceiling. `needs_split` enforces
	/// **both** bounds, so whichever binds first is the real limit — and if
	/// the byte estimate is wrong, the advertised entry count is fiction.
	/// This measures it instead of estimating.
	#[test]
	fn test_dirnode_real_entry_capacity() -> TestResult<()> {
		extern crate std;
		for name_len in [5usize, 16, 64, 255] {
			let mut node = DirNode::new_leaf();
			let mut fits = 0usize;
			for i in 0..(DirNode::MAX_ENTRIES + 50) {
				let mut name = alloc::format!("{i:0width$}", width = name_len);
				name.truncate(name_len);
				let e = DirEntry {
					name: name.as_str().try_into().map_err(|_| "name too long")?,
					inode_id: ObjectId::from_content(name.as_bytes()),
					inode_hint: StorageHint::default(),
				};
				node.entries.push(e);
				if node.needs_split() {
					break;
				}
				fits = i + 1;
			}
			let bytes = node.encode_canonical()?.len();
			std::println!(
				"name_len={name_len}: {fits} entries fit, encoded {bytes} B, {} B/entry",
				bytes / fits.max(1)
			);
		}
		Ok(())
	}

	#[test]
	fn test_dirnode_new_leaf() -> TestResult<()> {
		// -- Setup & Exec
		let node = DirNode::new_leaf();

		// -- Check
		assert!(node.is_leaf);
		assert!(node.entries.is_empty());
		assert!(node.children.is_empty());
		assert!(!node.object_id.is_zero());

		Ok(())
	}

	/// One pass, not two: the id must be the hash of exactly the bytes the
	/// caller will store.
	#[test]
	fn test_seal_canonical_matches_update_then_encode() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut eager = DirNode::new_leaf();
		let entry = DirEntry::new(
			"sealed",
			ObjectRef::new(ObjectId::from_content(b"child"), StorageHint::zero()),
		)?;
		eager.insert_entry(entry)?;
		let mut sealed = eager.clone();

		// -- Exec
		eager.update_object_id();
		let eager_bytes = eager.encode_canonical()?;
		let sealed_bytes = sealed.seal_canonical()?;

		// -- Check
		assert_eq!(sealed.object_id, eager.object_id);
		assert_eq!(sealed_bytes, eager_bytes);

		Ok(())
	}

	#[test]
	fn test_dirnode_object_id_matches_blake3_of_its_own_stored_bytes() -> TestResult<()> {
		// -- Setup & Fixtures: regression test for the same real bug
		// documented in `Inode::test_inode_object_id_matches_blake3_of_its_own_stored_bytes`
		// — `object_id` used to be a normal serialized field, making
		// `update_object_id()` self-referentially inconsistent with what
		// actually gets stored on disk. Found live via a genesis `DirNode`
		// that became permanently unreadable after a real commit.
		let node = DirNode::new_leaf();

		// -- Exec
		let canonical = node.encode_canonical()?;

		// -- Check
		assert_eq!(
			node.object_id,
			ObjectId::from_content(&canonical),
			"object_id must equal blake3 of its own canonical encoding"
		);
		let decoded = DirNode::decode_canonical(&canonical)?;
		assert_eq!(decoded.object_id, node.object_id);
		assert_eq!(decoded, node);

		Ok(())
	}

	#[test]
	fn test_dirnode_insert_entry() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = DirNode::new_leaf();
		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());
		let entry = DirEntry::new("test.txt", inode_ref)?;

		// -- Exec
		node.insert_entry(entry.clone())?;

		// -- Check
		assert_eq!(node.entries.len(), 1);
		assert_eq!(node.entries[0].name, "test.txt");

		Ok(())
	}

	#[test]
	fn test_dirnode_insert_maintains_sorted_order() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = DirNode::new_leaf();
		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());

		// Insert in non-sorted order
		let entry_zebra = DirEntry::new("zebra.txt", inode_ref)?;
		let entry_alpha = DirEntry::new("alpha.txt", inode_ref)?;
		let entry_middle = DirEntry::new("middle.txt", inode_ref)?;

		// -- Exec
		node.insert_entry(entry_zebra)?;
		node.insert_entry(entry_alpha)?;
		node.insert_entry(entry_middle)?;

		// -- Check - Should be sorted alphabetically
		assert_eq!(node.entries.len(), 3);
		assert_eq!(node.entries[0].name, "alpha.txt");
		assert_eq!(node.entries[1].name, "middle.txt");
		assert_eq!(node.entries[2].name, "zebra.txt");

		Ok(())
	}

	#[test]
	fn test_dirnode_encode_decode_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node1 = DirNode::new_leaf();
		let mut node2 = DirNode::new_leaf();

		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());
		let entry = DirEntry::new("test.txt", inode_ref)?;

		node1.insert_entry(entry.clone())?;
		node2.insert_entry(entry)?;

		// -- Exec
		let encoded1 = node1.encode_canonical()?;
		let encoded2 = node2.encode_canonical()?;

		// -- Check
		assert_eq!(encoded1, encoded2); // Deterministic encoding

		let decoded1 = DirNode::decode_canonical(&encoded1)?;
		let decoded2 = DirNode::decode_canonical(&encoded2)?;

		assert_eq!(decoded1, decoded2);
		assert_eq!(decoded1.object_id, node1.object_id);

		Ok(())
	}

	#[test]
	fn test_dirnode_lookup() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = DirNode::new_leaf();
		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());

		node.insert_entry(DirEntry::new("file1.txt", inode_ref)?)?;
		node.insert_entry(DirEntry::new("file2.txt", inode_ref)?)?;

		// -- Exec & Check
		let found1 = node.lookup("file1.txt");
		assert!(found1.is_some());
		assert_eq!(found1.unwrap().name, "file1.txt");

		let found2 = node.lookup("file2.txt");
		assert!(found2.is_some());

		let not_found = node.lookup("nonexistent.txt");
		assert!(not_found.is_none());

		Ok(())
	}

	#[test]
	fn test_dirnode_remove_entry() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = DirNode::new_leaf();
		let inode_ref = ObjectRef::new(ObjectId::from_content(b"test"), StorageHint::zero());

		node.insert_entry(DirEntry::new("file1.txt", inode_ref)?)?;
		node.insert_entry(DirEntry::new("file2.txt", inode_ref)?)?;

		// -- Exec
		let removed = node.remove_entry("file1.txt")?;

		// -- Check
		assert_eq!(removed.name, "file1.txt");
		assert_eq!(node.entries.len(), 1);
		assert_eq!(node.entries[0].name, "file2.txt");

		Ok(())
	}
}
