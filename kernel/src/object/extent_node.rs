//! ExtentNode - B-tree for mapping logical file offsets to chunk references

extern crate alloc;

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use super::object_id::{ObjectId, ObjectRef, StorageHint};
use crate::error::{Error, Result};

/// Extent descriptor - maps a logical range to a chunk
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtentDescriptor {
	/// Logical offset in file where this extent starts
	pub logical_offset: u64,

	/// Length of this extent in bytes
	pub length: u64,

	/// Chunk object ID
	pub chunk_id: ObjectId,

	/// Storage hint for chunk placement
	pub chunk_hint: StorageHint,

	/// Offset within the chunk where this extent's data starts
	pub chunk_offset: u64,
}

/// ExtentNode - B-tree node for extent mapping
///
/// Keys are logical offsets, values are chunk references.
/// Maintains sorted order for efficient range queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtentNode {
	/// Content-addressed ID of this node — `#[serde(skip)]`, matching
	/// `AllocNode`'s convention: see `Inode::object_id`'s doc comment for
	/// why a self-referential field can't be part of its own hash input.
	#[serde(skip)]
	pub object_id: ObjectId,

	/// Is this a leaf node?
	pub is_leaf: bool,

	/// Extent entries (sorted by logical_offset)
	pub extents: Vec<ExtentDescriptor>,

	/// Child node references (for internal nodes)
	/// Length is extents.len() + 1 for internal nodes
	pub children: Vec<ObjectRef>,
}

impl ExtentNode {
	/// Maximum canonical size (128 KiB as per FORMAT.md)
	pub const MAX_CANONICAL_SIZE: usize = 128 * 1024;

	/// Approximate max entries to stay under MAX_CANONICAL_SIZE
	/// Each extent is ~80 bytes, so ~1600 entries max
	pub const MAX_ENTRIES: usize = 1500;

	/// Create new empty leaf node
	pub fn new_leaf() -> Self {
		let mut node = Self {
			object_id: ObjectId::zero(),
			is_leaf: true,
			extents: Vec::new(),
			children: Vec::new(),
		};
		node.update_object_id();
		node
	}

	/// Create new internal node
	pub fn new_internal(extents: Vec<ExtentDescriptor>, children: Vec<ObjectRef>) -> Self {
		let mut node = Self {
			object_id: ObjectId::zero(),
			is_leaf: false,
			extents,
			children,
		};
		node.update_object_id();
		node
	}

	/// Insert extent into leaf node, replacing any existing coverage of the
	/// same logical range.
	///
	/// Every existing extent that overlaps `[extent.logical_offset,
	/// extent.logical_offset + extent.length)` is trimmed or dropped first:
	/// a range fully inside the new extent is removed outright, a range
	/// only partially overlapping is trimmed to whatever's left outside the
	/// new extent (splitting into two pieces if the new extent lands in the
	/// middle of it), and a range with no overlap is untouched. Without
	/// this, a second write to a range already covered by an earlier write
	/// (the overwhelmingly common case — `O_TRUNC`+write, `echo >`, any
	/// config-file update) used to just append a second, overlapping
	/// extent alongside the first rather than replacing it; the read path
	/// (`fill_from_extent_node`) has no arbitration logic for that case and
	/// silently let whichever entry happened to land later in the vec win
	/// — which, because insertion at an exact-offset match shifts the new
	/// entry *before* the existing one, meant the **older** write silently
	/// won reads, not the newer one. See this crate's
	/// `test_insert_extent_overwrite_replaces_not_appends`.
	pub fn insert_extent(&mut self, extent: ExtentDescriptor) -> Result<()> {
		self.insert_extent_deferred(extent)?;
		self.update_object_id();
		Ok(())
	}

	/// [`insert_extent`](Self::insert_extent) without recomputing
	/// `object_id`.
	///
	/// This node is the one object whose canonical encoding grows with a
	/// file's history, and the builders that append to it are about to
	/// encode it anyway — it is the object they are writing. Hashing here
	/// and again at the call site encoded an O(history) node twice per
	/// append, which is what made a long staged stream quadratic in
	/// practice as well as in name. `object_id` is stale after this call;
	/// [`seal_canonical`](Self::seal_canonical) is the one-step way for a
	/// caller that needs both the id and the bytes.
	pub fn insert_extent_deferred(&mut self, extent: ExtentDescriptor) -> Result<()> {
		if !self.is_leaf {
			return Err(Error::invalid_operation("Cannot insert into internal node"));
		}

		let new_start = extent.logical_offset;
		let new_end = extent.logical_offset + extent.length;

		let mut kept = Vec::with_capacity(self.extents.len() + 1);
		for old in self.extents.drain(..) {
			let old_start = old.logical_offset;
			let old_end = old.logical_offset + old.length;

			if old_end <= new_start || old_start >= new_end {
				// No overlap at all.
				kept.push(old);
				continue;
			}

			// Keep the part of `old` before the new extent, if any.
			if old_start < new_start {
				kept.push(ExtentDescriptor {
					logical_offset: old_start,
					length: new_start - old_start,
					chunk_id: old.chunk_id,
					chunk_hint: old.chunk_hint,
					chunk_offset: old.chunk_offset,
				});
			}

			// Keep the part of `old` after the new extent, if any.
			if old_end > new_end {
				kept.push(ExtentDescriptor {
					logical_offset: new_end,
					length: old_end - new_end,
					chunk_id: old.chunk_id,
					chunk_hint: old.chunk_hint,
					chunk_offset: old.chunk_offset + (new_end - old_start),
				});
			}
			// Otherwise `old` is fully covered by the new extent — dropped.
		}

		let pos = kept.partition_point(|e| e.logical_offset < new_start);
		kept.insert(pos, extent);
		self.extents = kept;

		Ok(())
	}

	/// Check if node needs splitting
	pub fn needs_split(&self) -> bool {
		self.extents.len() > Self::MAX_ENTRIES
			|| self.encode_canonical().map(|b| b.len()).unwrap_or(0) > Self::MAX_CANONICAL_SIZE
	}

	/// Split node at median (deterministic split policy)
	pub fn split(&self) -> Result<(ExtentNode, ExtentNode)> {
		if !self.needs_split() {
			return Err(Error::invalid_operation("Node does not need splitting"));
		}

		let median = self.extents.len() / 2;

		let left_extents = self.extents[..median].to_vec();
		let right_extents = self.extents[median..].to_vec();

		let (left_children, right_children) = if self.is_leaf {
			(Vec::new(), Vec::new())
		} else {
			let left_children = self.children[..=median].to_vec();
			let right_children = self.children[median..].to_vec();
			(left_children, right_children)
		};

		let left = ExtentNode::new_internal(left_extents, left_children);
		let right = ExtentNode::new_internal(right_extents, right_children);

		Ok((left, right))
	}

	/// Find extent containing given logical offset
	pub fn find_extent(&self, logical_offset: u64) -> Option<&ExtentDescriptor> {
		self.extents
			.iter()
			.find(|e| e.logical_offset <= logical_offset && logical_offset < e.logical_offset + e.length)
	}

	/// Update object ID based on current content. See `Inode::update_object_id`'s
	/// doc comment — safe in any order since `object_id` is `#[serde(skip)]`.
	pub fn update_object_id(&mut self) {
		let canonical = self.encode_canonical().unwrap();
		self.object_id = ObjectId::from_content(&canonical);
	}

	/// Encode, adopt the object id those bytes hash to, and hand the bytes
	/// back.
	///
	/// The pair every builder needs: `update_object_id` followed by
	/// `encode_canonical` encodes the node twice. On the extent node that is
	/// O(history) work per append.
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

	#[test]
	fn test_extentnode_new_leaf() -> TestResult<()> {
		// -- Setup & Exec
		let node = ExtentNode::new_leaf();

		// -- Check
		assert!(node.is_leaf);
		assert!(node.extents.is_empty());
		assert!(node.children.is_empty());
		assert!(!node.object_id.is_zero());

		Ok(())
	}

	#[test]
	fn test_extentnode_object_id_matches_blake3_of_its_own_stored_bytes() -> TestResult<()> {
		// -- Setup & Fixtures: regression test for the same real bug
		// documented in `Inode::test_inode_object_id_matches_blake3_of_its_own_stored_bytes`
		// — `object_id` used to be a normal serialized field, making
		// `update_object_id()` self-referentially inconsistent with what
		// actually gets stored on disk.
		let node = ExtentNode::new_leaf();

		// -- Exec
		let canonical = node.encode_canonical()?;

		// -- Check
		assert_eq!(
			node.object_id,
			ObjectId::from_content(&canonical),
			"object_id must equal blake3 of its own canonical encoding"
		);
		let decoded = ExtentNode::decode_canonical(&canonical)?;
		assert_eq!(decoded.object_id, node.object_id);
		assert_eq!(decoded, node);

		Ok(())
	}

	/// The builders use [`ExtentNode::insert_extent_deferred`] and
	/// [`ExtentNode::seal_canonical`] instead of the rehashing pair, because
	/// encoding a node that grows with a file's history twice per append is
	/// the quadratic half of a staged append. The two routes must agree
	/// **byte for byte**: the object id is a hash of those bytes, so a
	/// divergence would rewrite every extent node the volume ever stored.
	#[test]
	fn test_deferred_insert_seals_identically() -> TestResult<()> {
		// -- Setup & Fixtures
		let extent = |i: u64| ExtentDescriptor {
			logical_offset: i * 64,
			length: 64,
			chunk_id: ObjectId::from_content(&i.to_le_bytes()),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};
		let mut eager = ExtentNode::new_leaf();
		let mut deferred = ExtentNode::new_leaf();

		// -- Exec
		for i in 0..8 {
			eager.insert_extent(extent(i))?;
			deferred.insert_extent_deferred(extent(i))?;
		}
		let eager_bytes = eager.encode_canonical()?;
		let sealed_bytes = deferred.seal_canonical()?;

		// -- Check
		assert_eq!(deferred.object_id, eager.object_id);
		assert_eq!(sealed_bytes, eager_bytes);
		assert_eq!(deferred, eager);

		Ok(())
	}

	#[test]
	fn test_extentnode_insert_extent() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = ExtentNode::new_leaf();
		let extent = ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"chunk1"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		// -- Exec
		node.insert_extent(extent.clone())?;

		// -- Check
		assert_eq!(node.extents.len(), 1);
		assert_eq!(node.extents[0], extent);

		Ok(())
	}

	#[test]
	fn test_extentnode_insert_maintains_sorted_order() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = ExtentNode::new_leaf();

		// Insert in non-sorted order
		let extent2 = ExtentDescriptor {
			logical_offset: 8192,
			length: 4096,
			chunk_id: ObjectId::from_content(b"chunk2"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		let extent1 = ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"chunk1"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		let extent3 = ExtentDescriptor {
			logical_offset: 4096,
			length: 4096,
			chunk_id: ObjectId::from_content(b"chunk3"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		// -- Exec
		node.insert_extent(extent2)?;
		node.insert_extent(extent1)?;
		node.insert_extent(extent3)?;

		// -- Check - Should be sorted by logical_offset
		assert_eq!(node.extents.len(), 3);
		assert_eq!(node.extents[0].logical_offset, 0);
		assert_eq!(node.extents[1].logical_offset, 4096);
		assert_eq!(node.extents[2].logical_offset, 8192);

		Ok(())
	}

	#[test]
	fn test_insert_extent_exact_overwrite_replaces_not_appends() -> TestResult<()> {
		// -- Setup & Fixtures: regression test for the silent-stale-data bug
		// this fix closes — a second write to the exact same range used to
		// add a second, overlapping extent instead of replacing the first,
		// and the read path's linear scan let the *older* write win.
		let mut node = ExtentNode::new_leaf();
		let old = ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"old-content"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};
		let new = ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"new-content"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		// -- Exec
		node.insert_extent(old)?;
		node.insert_extent(new.clone())?;

		// -- Check: exactly one extent survives, and it's the new one.
		assert_eq!(
			node.extents.len(),
			1,
			"the old, fully-overwritten extent must be dropped, not kept alongside the new one"
		);
		assert_eq!(node.extents[0], new);

		Ok(())
	}

	#[test]
	fn test_insert_extent_partial_overlap_trims_the_old_extent() -> TestResult<()> {
		// -- Setup & Fixtures: old extent covers [0, 4096); new write covers
		// [2048, 6144) — overlaps the back half of `old` and extends past it.
		let mut node = ExtentNode::new_leaf();
		let old = ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"old"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};
		let new = ExtentDescriptor {
			logical_offset: 2048,
			length: 4096,
			chunk_id: ObjectId::from_content(b"new"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		// -- Exec
		node.insert_extent(old)?;
		node.insert_extent(new.clone())?;

		// -- Check: `old` is trimmed to [0, 2048) — its untouched front
		// half — and `new` covers [2048, 6144) unchanged.
		assert_eq!(node.extents.len(), 2);
		assert_eq!(node.extents[0].logical_offset, 0);
		assert_eq!(node.extents[0].length, 2048);
		assert_eq!(
			node.extents[0].chunk_offset, 0,
			"trimmed-from-the-right piece keeps its original chunk_offset"
		);
		assert_eq!(node.extents[1], new);

		Ok(())
	}

	#[test]
	fn test_insert_extent_middle_overlap_splits_the_old_extent_in_two() -> TestResult<()> {
		// -- Setup & Fixtures: old extent covers [0, 8192); new write lands
		// entirely inside it, [2048, 4096) — must split `old` into a kept
		// head [0, 2048) and a kept tail [4096, 8192), both still pointing
		// at the same underlying chunk with correctly adjusted chunk_offset.
		let mut node = ExtentNode::new_leaf();
		let old = ExtentDescriptor {
			logical_offset: 0,
			length: 8192,
			chunk_id: ObjectId::from_content(b"old"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};
		let new = ExtentDescriptor {
			logical_offset: 2048,
			length: 2048,
			chunk_id: ObjectId::from_content(b"new"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		// -- Exec
		node.insert_extent(old)?;
		node.insert_extent(new.clone())?;

		// -- Check
		assert_eq!(node.extents.len(), 3);
		assert_eq!(node.extents[0].logical_offset, 0);
		assert_eq!(node.extents[0].length, 2048);
		assert_eq!(node.extents[0].chunk_offset, 0);
		assert_eq!(node.extents[1], new);
		assert_eq!(node.extents[2].logical_offset, 4096);
		assert_eq!(node.extents[2].length, 4096);
		assert_eq!(
			node.extents[2].chunk_offset, 4096,
			"trimmed-from-the-left piece's chunk_offset must advance by however much was cut from its front"
		);

		Ok(())
	}

	#[test]
	fn test_extentnode_encode_decode_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node1 = ExtentNode::new_leaf();
		let mut node2 = ExtentNode::new_leaf();

		let extent = ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"test"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		};

		node1.insert_extent(extent.clone())?;
		node2.insert_extent(extent)?;

		// -- Exec
		let encoded1 = node1.encode_canonical()?;
		let encoded2 = node2.encode_canonical()?;

		// -- Check
		assert_eq!(encoded1, encoded2); // Deterministic encoding

		let decoded1 = ExtentNode::decode_canonical(&encoded1)?;
		let decoded2 = ExtentNode::decode_canonical(&encoded2)?;

		assert_eq!(decoded1, decoded2);
		assert_eq!(decoded1.object_id, node1.object_id);

		Ok(())
	}

	#[test]
	fn test_extentnode_find_extent() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut node = ExtentNode::new_leaf();

		node.insert_extent(ExtentDescriptor {
			logical_offset: 0,
			length: 4096,
			chunk_id: ObjectId::from_content(b"chunk1"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		})?;

		node.insert_extent(ExtentDescriptor {
			logical_offset: 8192,
			length: 4096,
			chunk_id: ObjectId::from_content(b"chunk2"),
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		})?;

		// -- Exec & Check
		let found1 = node.find_extent(100);
		assert!(found1.is_some());
		assert_eq!(found1.unwrap().logical_offset, 0);

		let found2 = node.find_extent(9000);
		assert!(found2.is_some());
		assert_eq!(found2.unwrap().logical_offset, 8192);

		let not_found = node.find_extent(5000);
		assert!(not_found.is_none());

		Ok(())
	}
}
