//! CoW filesystem operation implementations for LedgerFS.
//!
//! All mutation operations return a [`TxnPlan`] listing newly-created objects;
//! the caller commits the plan by writing those objects to storage and updating
//! the active root.  Read operations (`read`, `readdir`) are pure queries:
//! they do not modify any state.
//!
//! Every operation that reads existing objects requires an [`ObjectStore`]
//! implementation so it can resolve [`ObjectRef`]s to their decoded types.

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::object_store::ObjectStore;
use super::transaction::{NewObject, TxnPlan};
use crate::error::{Error, Result};
use crate::object::{
	Chunk, DirEntry, DirNode, ExtentDescriptor, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef, Root, StorageHint,
};

// region:    --- Mutation operations

/// Create a new directory under `parent_dir_ref`.
///
/// Produces a new dir inode and an empty [`DirNode`] leaf.  When
/// `parent_dir_ref` is non-zero, the parent's [`DirNode`] and [`Inode`] are
/// also CoW-copied with the new entry inserted, and `new_root.file_tree_root`
/// is updated to point at the (CoW-copied) parent inode.
///
/// # Single-level limitation
///
/// This only CoW-copies **one level up**: `parent_dir_ref`'s own inode and
/// dir-node; it does not walk further up to rewrite *its* parent, and so
/// on to the real root. For a flat tree (root directory with files/dirs
/// directly under it, `parent_dir_ref` == the root's own dir inode) this is
/// already fully correct. For a deeper tree, a caller mutating something
/// nested under multiple directory levels must itself re-apply this same
/// one-level CoW repeatedly, walking from the mutated node back up to the
/// real root, threading `new_root` forward each step (the way
/// [`apply_ops_to_root`] already threads `current_root` between batched
/// operations): this function does not do that multi-level cascade for
/// you. Fixing that generally is tracked as a known follow-up, not solved
/// here.
pub fn mkdir<S: ObjectStore>(
	store: &S,
	root: &Root,
	parent_dir_ref: ObjectRef,
	name: &str,
	uid: u32,
	gid: u32,
	mode: u32,
) -> Result<TxnPlan> {
	// Create new directory inode + empty dir tree.
	let dir_tree = DirNode::new_leaf();
	let mut dir_inode = Inode::new_directory(uid, gid, mode);
	dir_inode.content_ref = ObjectRef::from_id(dir_tree.object_id);
	dir_inode.update_object_id();

	let mut txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());

	txn.add_object(NewObject::new(dir_tree.object_id, dir_tree.encode_canonical()?));
	txn.add_object(
		NewObject::new(dir_inode.object_id, dir_inode.encode_canonical()?).with_dependency(dir_tree.object_id),
	);

	// Update parent directory when a real parent was supplied, and point
	// `file_tree_root` at whatever ends up being the top of the modified
	// chain; see this function's doc comment for the single-level caveat.
	if !parent_dir_ref.is_zero() {
		let (parent_dir_node, parent_inode) =
			cow_insert_dir_entry(store, parent_dir_ref, name, ObjectRef::from_id(dir_inode.object_id))?;

		txn.add_object(
			NewObject::new(parent_dir_node.object_id, parent_dir_node.encode_canonical()?)
				.with_dependency(dir_inode.object_id),
		);
		txn.add_object(
			NewObject::new(parent_inode.object_id, parent_inode.encode_canonical()?)
				.with_dependency(parent_dir_node.object_id),
		);
		txn.new_root.file_tree_root = ObjectRef::from_id(parent_inode.object_id);
	} else {
		txn.new_root.file_tree_root = ObjectRef::from_id(dir_inode.object_id);
	}

	Ok(txn)
}

/// Create a new regular file under `parent_dir_ref`.
///
/// Produces a new file inode with no content.  When `parent_dir_ref` is
/// non-zero, the parent's [`DirNode`] and [`Inode`] are CoW-copied with the
/// new entry inserted, and `new_root.file_tree_root` is updated to point at
/// the (CoW-copied) parent inode.
///
/// See [`mkdir`]'s doc comment for the single-level CoW-cascade caveat;
/// it applies identically here.
pub fn create_file<S: ObjectStore>(
	store: &S,
	root: &Root,
	parent_dir_ref: ObjectRef,
	name: &str,
	uid: u32,
	gid: u32,
	mode: u32,
) -> Result<TxnPlan> {
	let file_inode = Inode::new_file(uid, gid, mode);

	let mut txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());
	txn.add_object(NewObject::new(file_inode.object_id, file_inode.encode_canonical()?));

	// Update parent directory when a real parent was supplied.
	if !parent_dir_ref.is_zero() {
		let (parent_dir_node, parent_inode) =
			cow_insert_dir_entry(store, parent_dir_ref, name, ObjectRef::from_id(file_inode.object_id))?;

		txn.add_object(
			NewObject::new(parent_dir_node.object_id, parent_dir_node.encode_canonical()?)
				.with_dependency(file_inode.object_id),
		);
		txn.add_object(
			NewObject::new(parent_inode.object_id, parent_inode.encode_canonical()?)
				.with_dependency(parent_dir_node.object_id),
		);
		txn.new_root.file_tree_root = ObjectRef::from_id(parent_inode.object_id);
	} else {
		txn.new_root.file_tree_root = ObjectRef::from_id(file_inode.object_id);
	}

	Ok(txn)
}

/// Write `data` to a file at `offset`, returning a [`TxnPlan`] with CoW copies
/// of the affected chunk, extent node, and inode.
///
/// The updated inode in the plan has a new `content_ref` pointing to the
/// updated extent tree and a `size` equal to `max(old_size, offset + len)`.
///
/// # `file_tree_root` is NOT updated by this function
///
/// Unlike [`mkdir`]/[`create_file`], `write` has no `parent_dir_ref`: it
/// only knows the file inode it's updating, not what directory entry (if
/// any) points at it. A caller that wants the tree to stay internally
/// consistent after a write must separately call [`cow_update_dir_entry`]
/// with the file's parent + name to re-point the parent's entry at the new
/// inode this function produces, merge that into the same [`TxnPlan`], and
/// set `new_root.file_tree_root` itself; this function deliberately
/// doesn't guess at that on its own.
pub fn write<S: ObjectStore>(
	store: &S,
	root: &Root,
	file_inode_ref: ObjectRef,
	offset: u64,
	data: &[u8],
) -> Result<TxnPlan> {
	if file_inode_ref.is_zero() {
		return Err(Error::invalid_operation("file_inode_ref is zero"));
	}

	let mut inode = store.get_inode(file_inode_ref)?;
	if inode.kind != InodeKind::RegularFile {
		return Err(Error::invalid_operation("not a regular file"));
	}

	// Pack data into a new chunk. The chunk's *stored* object id must be
	// derived from its canonical (postcard-wrapped) bytes, not
	// `chunk.object_id` (which is `blake3` of the raw payload only):
	// `ObjectRecordReader::read` verifies `blake3(logical_payload) ==
	// header.object_id` against whatever bytes actually got stored, i.e.
	// the canonical wrapper. Using the wrong one here would make every
	// written chunk fail to decode on read-back. Caught and documented in
	// `commit.rs`'s `test_commit_engine_actually_writes_decodable_object_data`.
	let chunk = Chunk::new(data.to_vec());
	let chunk_canonical = chunk.encode_canonical()?;
	let chunk_stored_id = ObjectId::from_content(&chunk_canonical);

	let extent = ExtentDescriptor {
		logical_offset: offset,
		length: data.len() as u64,
		chunk_id: chunk_stored_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	};

	// CoW-copy the extent node (or create a fresh leaf if there was none).
	let mut extent_node = if inode.content_ref.is_zero() {
		ExtentNode::new_leaf()
	} else {
		store.get_extent_node(inode.content_ref)?
	};
	extent_node.insert_extent_deferred(extent)?;
	let extent_bytes = extent_node.seal_canonical()?;

	// Update inode metadata.
	let new_end = offset + data.len() as u64;
	inode.size = inode.size.max(new_end);
	inode.content_ref = ObjectRef::from_id(extent_node.object_id);
	let inode_bytes = inode.seal_canonical()?;

	let mut txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());
	txn.add_object(NewObject::new(chunk_stored_id, chunk_canonical));
	txn.add_object(NewObject::new(extent_node.object_id, extent_bytes).with_dependency(chunk_stored_id));
	txn.add_object(NewObject::new(inode.object_id, inode_bytes).with_dependency(extent_node.object_id));

	Ok(txn)
}

/// Create a new regular file under `parent_dir_ref`, already containing
/// `data`.
///
/// [`create_file`] and [`write()`] as one transaction. They are one operation
/// here because a caller that wants a file *with* content pays twice
/// otherwise: the create writes a file inode and a directory node that the
/// write immediately rewrites, and on a volume whose durability point is the
/// commit, two commits are two durability points for one logical operation.
///
/// `data` must fit in a single object. Splitting a longer payload across
/// objects is the caller's job, exactly as it is for [`write()`].
///
/// `file_inode` is the file's identity: `Inode::new_file(uid, gid, mode)`,
/// the same inode [`create_file`] builds, taken **in its unwritten state**:
/// this function fills in `size` and `content_ref` and computes the final
/// `object_id`. It is a parameter rather than three (`uid`, `gid`, `mode`)
/// because the owner and mode are the caller's and this function's job is the
/// half that depends on `data`; passing them separately would also put the
/// function over the argument-count limit the rest of this crate keeps to.
/// Anything but a regular file is refused, so the signature cannot be used to
/// build a directory holding a chunk.
///
/// `new_root.file_tree_root` ends up at the CoW copy of the **immediate
/// parent**, not at the volume root: the same single-level caveat
/// [`create_file`] documents.
pub fn create_file_with_data<S: ObjectStore>(
	store: &S,
	root: &Root,
	parent_dir_ref: ObjectRef,
	name: &str,
	mut file_inode: Inode,
	data: &[u8],
) -> Result<TxnPlan> {
	if file_inode.kind != InodeKind::RegularFile {
		return Err(Error::invalid_operation(
			"create_file_with_data: the inode must name a regular file",
		));
	}

	// Same construction as `write`, and the same trap: the chunk's *stored*
	// id must come from its canonical (postcard-wrapped) bytes, never from
	// `chunk.object_id`, or every read of it fails to decode. See `write`'s
	// comment for the full account.
	let chunk = Chunk::new(data.to_vec());
	let chunk_canonical = chunk.encode_canonical()?;
	let chunk_stored_id = ObjectId::from_content(&chunk_canonical);

	let extent = ExtentDescriptor {
		logical_offset: 0,
		length: data.len() as u64,
		chunk_id: chunk_stored_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	};

	let mut extent_node = ExtentNode::new_leaf();
	extent_node.insert_extent_deferred(extent)?;
	let extent_bytes = extent_node.seal_canonical()?;

	// The caller's inode, taken straight to its written state rather than
	// created empty and rewritten.
	file_inode.size = data.len() as u64;
	file_inode.content_ref = ObjectRef::from_id(extent_node.object_id);
	let file_inode_bytes = file_inode.seal_canonical()?;

	let mut txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());
	txn.add_object(NewObject::new(chunk_stored_id, chunk_canonical));
	txn.add_object(NewObject::new(extent_node.object_id, extent_bytes).with_dependency(chunk_stored_id));
	txn.add_object(NewObject::new(file_inode.object_id, file_inode_bytes).with_dependency(extent_node.object_id));

	if !parent_dir_ref.is_zero() {
		let (parent_dir_node, parent_inode) =
			cow_insert_dir_entry(store, parent_dir_ref, name, ObjectRef::from_id(file_inode.object_id))?;

		txn.add_object(
			NewObject::new(parent_dir_node.object_id, parent_dir_node.encode_canonical()?)
				.with_dependency(file_inode.object_id),
		);
		txn.add_object(
			NewObject::new(parent_inode.object_id, parent_inode.encode_canonical()?)
				.with_dependency(parent_dir_node.object_id),
		);
		txn.new_root.file_tree_root = ObjectRef::from_id(parent_inode.object_id);
	} else {
		txn.new_root.file_tree_root = ObjectRef::from_id(file_inode.object_id);
	}

	Ok(txn)
}

/// CoW-copy `parent_inode_ref`'s directory to re-point an **existing**
/// entry named `name` at `new_child_ref`, without inserting or removing an
/// entry. This is [`write()`]'s missing "propagate to parent" half (see
/// `write`'s doc comment), and is also reused by [`rename`] internally.
///
/// Errors if `name` doesn't exist in the parent (nothing to update).
pub fn cow_update_dir_entry<S: ObjectStore>(
	store: &S,
	parent_inode_ref: ObjectRef,
	name: &str,
	new_child_ref: ObjectRef,
) -> Result<(DirNode, Inode)> {
	let mut parent_inode = store.get_inode(parent_inode_ref)?;
	if parent_inode.kind != InodeKind::Directory {
		return Err(Error::invalid_operation("parent is not a directory"));
	}
	if parent_inode.content_ref.is_zero() {
		return Err(Error::invalid_operation("entry not found"));
	}
	let mut dir_node = store.get_dir_node(parent_inode.content_ref)?;
	let pos = dir_node
		.entries
		.binary_search_by(|e| e.name.as_str().cmp(name))
		.map_err(|_| Error::invalid_operation("entry not found"))?;
	dir_node.entries[pos].inode_id = new_child_ref.object_id;
	dir_node.entries[pos].inode_hint = new_child_ref.hint;
	dir_node.update_object_id();

	parent_inode.content_ref = ObjectRef::from_id(dir_node.object_id);
	parent_inode.update_object_id();

	Ok((dir_node, parent_inode))
}

/// Remove `name` from the directory at `parent_dir_ref`.
///
/// The semantics are the POSIX ones: a missing source is an error (not a
/// silent no-op), and unlinking a directory is refused; use a dedicated
/// `rmdir` for that, not implemented in this module.
///
/// There is no separate "free the node" step here: the removed inode simply
/// becomes unreachable from
/// the new root once this txn commits (nothing in `new_objects`
/// references it), which is what makes it eligible for GC: there's no
/// flat node table to leak from in a content-addressed model.
pub fn unlink<S: ObjectStore>(store: &S, root: &Root, parent_dir_ref: ObjectRef, name: &str) -> Result<TxnPlan> {
	if parent_dir_ref.is_zero() {
		return Err(Error::invalid_operation("parent_dir_ref is zero"));
	}

	let parent_inode = store.get_inode(parent_dir_ref)?;
	if parent_inode.kind != InodeKind::Directory {
		return Err(Error::invalid_operation("parent is not a directory"));
	}
	if parent_inode.content_ref.is_zero() {
		return Err(Error::invalid_operation("unlink: source not found"));
	}

	let mut dir_node = store.get_dir_node(parent_inode.content_ref)?;
	let pos = dir_node
		.entries
		.binary_search_by(|e| e.name.as_str().cmp(name))
		.map_err(|_| Error::invalid_operation("unlink: source not found"))?;

	let removed_inode = store.get_inode(ObjectRef::from_id(dir_node.entries[pos].inode_id))?;
	if removed_inode.kind == InodeKind::Directory {
		return Err(Error::invalid_operation("unlink: is a directory"));
	}

	dir_node.entries.remove(pos);
	dir_node.update_object_id();

	let mut parent_inode = parent_inode;
	parent_inode.content_ref = ObjectRef::from_id(dir_node.object_id);
	parent_inode.update_object_id();

	let mut txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());
	txn.add_object(NewObject::new(dir_node.object_id, dir_node.encode_canonical()?));
	txn.add_object(
		NewObject::new(parent_inode.object_id, parent_inode.encode_canonical()?).with_dependency(dir_node.object_id),
	);
	txn.new_root.file_tree_root = ObjectRef::from_id(parent_inode.object_id);

	Ok(txn)
}

/// Bounded depth-first search: is `target` reachable by walking *down*
/// from `dir_inode_ref` (including `dir_inode_ref` itself)?
///
/// Used by [`rename`]'s cycle guard. This content-addressed model has no
/// `.parent` back-pointers, so there is no way to check "moving into own
/// descendant" by walking *up* from the destination: the only way to ask
/// "is B inside A?" is to walk down
/// from A looking for B. `depth_budget` is a defensive backstop against
/// pathological/malformed trees, not a realistic limit; it mirrors the
/// bounded-hop-count pattern already used elsewhere in this codebase for
/// the same "walk a structure that's supposed to be a DAG, but don't trust
/// that unconditionally" reason (e.g. symlink hop limits).
fn dir_contains_inode<S: ObjectStore>(
	store: &S,
	dir_inode_ref: ObjectRef,
	target: crate::object::ObjectId,
	depth_budget: usize,
) -> Result<bool> {
	if dir_inode_ref.object_id == target {
		return Ok(true);
	}
	if depth_budget == 0 {
		return Err(Error::invalid_operation(
			"rename: directory tree too deep to safely check for cycles",
		));
	}
	let inode = store.get_inode(dir_inode_ref)?;
	if inode.kind != InodeKind::Directory || inode.content_ref.is_zero() {
		return Ok(false);
	}
	// Flat-leaf assumption, matching this module's existing single-level/
	// unsplit-leaf limitations (see `mkdir`'s doc comment): internal
	// B-tree nodes don't exist in practice yet from anything in this file.
	let dir_node = store.get_dir_node(inode.content_ref)?;
	for entry in &dir_node.entries {
		if dir_contains_inode(store, ObjectRef::from_id(entry.inode_id), target, depth_budget - 1)? {
			return Ok(true);
		}
	}
	Ok(false)
}

/// Move (rename) `old_name` under `old_parent_dir_ref` to `new_name` under
/// `new_parent_dir_ref` (pass the same ref for both for a same-directory
/// rename).
///
/// A missing source is an error, overwriting an existing destination is only
/// permitted when neither side is a directory (the old destination node simply
/// becomes unreachable; see [`unlink`]'s doc comment on why there's no
/// separate free step), and moving a directory into itself or one of its own
/// descendants is refused, by the downward walk in `dir_contains_inode`.
///
/// # `file_tree_root` caveat for cross-directory renames
///
/// A same-parent rename only touches one one-level CoW chain, so
/// `file_tree_root` is fully correct for it (same caveat as `mkdir` et al).
/// A **cross-parent** rename touches *two* separate one-level chains (old
/// parent, new parent) and this function can only point `file_tree_root`
/// at one of them; it points at the destination side, since that's the
/// side gaining new content. If old and new parents aren't both already
/// the true tree root, the caller is responsible for propagating the
/// old-parent chain's update upward too (same caveat as `write`).
pub fn rename<S: ObjectStore>(
	store: &S,
	root: &Root,
	old_parent_dir_ref: ObjectRef,
	old_name: &str,
	new_parent_dir_ref: ObjectRef,
	new_name: &str,
) -> Result<TxnPlan> {
	if old_parent_dir_ref.is_zero() || new_parent_dir_ref.is_zero() {
		return Err(Error::invalid_operation("rename: parent ref is zero"));
	}
	if old_parent_dir_ref.object_id == new_parent_dir_ref.object_id && old_name == new_name {
		return Err(Error::invalid_operation("rename: source and destination are the same"));
	}

	let old_parent_inode = store.get_inode(old_parent_dir_ref)?;
	if old_parent_inode.kind != InodeKind::Directory || old_parent_inode.content_ref.is_zero() {
		return Err(Error::invalid_operation("rename: source not found"));
	}
	let old_dir_node = store.get_dir_node(old_parent_inode.content_ref)?;
	let old_pos = old_dir_node
		.entries
		.binary_search_by(|e| e.name.as_str().cmp(old_name))
		.map_err(|_| Error::invalid_operation("rename: source not found"))?;
	let moved_entry = old_dir_node.entries[old_pos].clone();
	let moved_ref = moved_entry.inode_ref();
	let moved_inode = store.get_inode(moved_ref)?;

	// Cycle guard: refuse moving a directory into itself or a descendant.
	// The question is "is the destination reachable by walking DOWN from
	// the thing being moved?"; not the other way around: we're asking
	// whether `new_parent_dir_ref` (the destination) is already inside
	// `moved_ref` (the directory being relocated).
	if moved_inode.kind == InodeKind::Directory
		&& dir_contains_inode(store, moved_ref, new_parent_dir_ref.object_id, 10_000)?
	{
		return Err(Error::invalid_operation(
			"rename: cannot move a directory into itself or a descendant",
		));
	}

	let new_parent_inode_current = store.get_inode(new_parent_dir_ref)?;
	if new_parent_inode_current.kind != InodeKind::Directory {
		return Err(Error::invalid_operation(
			"rename: destination parent is not a directory",
		));
	}
	let existing_new_dir_node = if new_parent_inode_current.content_ref.is_zero() {
		None
	} else {
		Some(store.get_dir_node(new_parent_inode_current.content_ref)?)
	};
	if let Some(ref dn) = existing_new_dir_node
		&& let Ok(existing_pos) = dn.entries.binary_search_by(|e| e.name.as_str().cmp(new_name))
	{
		let existing_entry = &dn.entries[existing_pos];
		if existing_entry.inode_id != moved_entry.inode_id {
			let existing_inode = store.get_inode(ObjectRef::from_id(existing_entry.inode_id))?;
			if existing_inode.kind == InodeKind::Directory || moved_inode.kind == InodeKind::Directory {
				return Err(Error::invalid_operation(
					"rename: destination exists and is a directory",
				));
			}
			// Overwriting a file is fine: the replaced node just
			// becomes unreachable from the new root once committed.
		}
	}

	// Remove from the old parent first.
	let mut new_old_dir_node = old_dir_node.clone();
	new_old_dir_node.entries.remove(old_pos);
	new_old_dir_node.update_object_id();
	let mut new_old_parent_inode = old_parent_inode.clone();
	new_old_parent_inode.content_ref = ObjectRef::from_id(new_old_dir_node.object_id);
	new_old_parent_inode.update_object_id();

	let mut txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());
	txn.add_object(NewObject::new(
		new_old_dir_node.object_id,
		new_old_dir_node.encode_canonical()?,
	));
	txn.add_object(
		NewObject::new(new_old_parent_inode.object_id, new_old_parent_inode.encode_canonical()?)
			.with_dependency(new_old_dir_node.object_id),
	);

	if old_parent_dir_ref.object_id == new_parent_dir_ref.object_id {
		// Same-parent rename: insert into the ALREADY-updated (post-removal)
		// dir node, not a stale re-fetch of the pre-removal one.
		let mut dn = new_old_dir_node.clone();
		if let Ok(pos) = dn.entries.binary_search_by(|e| e.name.as_str().cmp(new_name)) {
			dn.entries.remove(pos);
		}
		dn.insert_entry(DirEntry::new(new_name, moved_ref)?)?;
		dn.update_object_id();
		let mut pi = new_old_parent_inode.clone();
		pi.content_ref = ObjectRef::from_id(dn.object_id);
		pi.update_object_id();

		txn.add_object(
			NewObject::new(dn.object_id, dn.encode_canonical()?).with_dependency(new_old_dir_node.object_id),
		);
		txn.add_object(NewObject::new(pi.object_id, pi.encode_canonical()?).with_dependency(dn.object_id));
		txn.new_root.file_tree_root = ObjectRef::from_id(pi.object_id);
	} else {
		let mut new_dir_node = existing_new_dir_node.unwrap_or_else(DirNode::new_leaf);
		if let Ok(pos) = new_dir_node.entries.binary_search_by(|e| e.name.as_str().cmp(new_name)) {
			new_dir_node.entries.remove(pos);
		}
		new_dir_node.insert_entry(DirEntry::new(new_name, moved_ref)?)?;
		new_dir_node.update_object_id();
		let mut new_parent_inode = new_parent_inode_current;
		new_parent_inode.content_ref = ObjectRef::from_id(new_dir_node.object_id);
		new_parent_inode.update_object_id();

		txn.add_object(
			NewObject::new(new_dir_node.object_id, new_dir_node.encode_canonical()?)
				.with_dependency(moved_entry.inode_id),
		);
		txn.add_object(
			NewObject::new(new_parent_inode.object_id, new_parent_inode.encode_canonical()?)
				.with_dependency(new_dir_node.object_id),
		);
		txn.new_root.file_tree_root = ObjectRef::from_id(new_parent_inode.object_id);
	}

	Ok(txn)
}

// endregion: --- Mutation operations

// region:    --- Query operations

/// Read up to `length` bytes from file `file_inode_ref` starting at `offset`.
///
/// Returns the bytes read.  Returns an empty `Vec` when the file is empty or
/// `offset` is at/beyond the end of the file.
///
/// This is [`read_inode`] plus the fetch of the inode it needs. A caller that
/// already holds the inode (anything that resolved the path before deciding
/// how much to ask for) should call `read_inode` and save the device read.
pub fn read<S: ObjectStore>(
	store: &S,
	_root: &Root,
	file_inode_ref: ObjectRef,
	offset: u64,
	length: usize,
) -> Result<Vec<u8>> {
	if file_inode_ref.is_zero() || length == 0 {
		return Ok(Vec::new());
	}

	let inode = store.get_inode(file_inode_ref)?;
	if inode.kind != InodeKind::RegularFile {
		return Err(Error::invalid_operation("not a regular file"));
	}

	read_inode(store, &inode, offset, length)
}

/// Read up to `length` bytes from a file whose [`Inode`] the caller already has.
///
/// # Why this is a separate entry point
///
/// [`read`] fetches the inode to answer three questions (is it a regular file,
/// how long is it, and where is its content), and *then* reads the bytes. A
/// caller that has already resolved the inode to decide what to ask for (the
/// host's path walk holds one before it asks for a range) has those three
/// answers in hand, and re-fetching the same object is a second read of the
/// same block for information the caller is holding. `read` is this function
/// plus that fetch, so the two cannot drift.
///
/// The inode is still validated here rather than trusted: kind, empty content
/// and the clamped range are all checked exactly as [`read`] checks them, so a
/// caller that skips the wrapper gets the same errors and the same bytes.
pub fn read_inode<S: ObjectStore>(store: &S, inode: &Inode, offset: u64, length: usize) -> Result<Vec<u8>> {
	if inode.kind != InodeKind::RegularFile {
		return Err(Error::invalid_operation("not a regular file"));
	}

	// Empty file.
	if inode.content_ref.is_zero() || inode.size == 0 {
		return Ok(Vec::new());
	}

	// Clamp the read range to [offset, file_size).
	if offset >= inode.size || length == 0 {
		return Ok(Vec::new());
	}
	let read_end = (offset + length as u64).min(inode.size);
	let actual_len = (read_end - offset) as usize;

	let mut buf = vec![0u8; actual_len];
	let extent_root = store.get_extent_node(inode.content_ref)?;
	fill_from_extent_node(store, &extent_root, offset, read_end, &mut buf)?;

	Ok(buf)
}

/// List the entries of a directory inode.
///
/// Returns a sorted `Vec<DirEntry>`.  Returns an empty vec for empty or
/// zero-ref directories.
pub fn readdir<S: ObjectStore>(store: &S, _root: &Root, dir_inode_ref: ObjectRef) -> Result<Vec<DirEntry>> {
	if dir_inode_ref.is_zero() {
		return Ok(Vec::new());
	}

	let inode = store.get_inode(dir_inode_ref)?;
	if inode.kind != InodeKind::Directory {
		return Err(Error::invalid_operation("not a directory"));
	}

	if inode.content_ref.is_zero() {
		return Ok(Vec::new());
	}

	let dir_node = store.get_dir_node(inode.content_ref)?;
	collect_dir_entries(store, &dir_node)
}

// endregion: --- Query operations

// region:    --- Batch mutation

/// Apply a slice of [`Operation`]s to `root` in order, returning a single
/// merged [`TxnPlan`].
///
/// # What "in order" does and does not mean
///
/// The ops are applied in slice order and their plans are merged, but only the
/// **root descriptor** is threaded between them: every op reads through the
/// same immutable `store`. An op therefore cannot see the objects an earlier op
/// produced. Two writes to one file, or a create followed by a write of the
/// file it created, would each be built against the *original* inode, and the
/// merged plan would carry both versions with only the later one reachable:
/// silent data loss, not an error. This function is for ops that touch
/// disjoint objects; a caller that needs chained visibility has to supply it,
/// which is what the host crate's `StagedFold` does with an overlay store.
///
/// The merged `new_root.file_tree_root` is the **last** op's, and `mkdir` /
/// `create_file` leave theirs at the CoW copy of the immediate parent (their
/// own doc comments say so). Callers still owe the ancestor cascade; this
/// function does not add one.
pub fn apply_ops_to_root<S: ObjectStore>(store: &S, root: &Root, ops: &[Operation]) -> Result<TxnPlan> {
	let mut current_root = root.clone();
	let mut combined_txn = TxnPlan::new(ObjectRef::from_id(root.object_id), root.next_commit());

	for op in ops {
		let txn = match op {
			Operation::Mkdir {
				parent_dir_ref,
				name,
				uid,
				gid,
				mode,
			} => mkdir(store, &current_root, *parent_dir_ref, name, *uid, *gid, *mode)?,

			Operation::CreateFile {
				parent_dir_ref,
				name,
				uid,
				gid,
				mode,
			} => create_file(store, &current_root, *parent_dir_ref, name, *uid, *gid, *mode)?,

			Operation::Write {
				file_inode_ref,
				offset,
				data,
			} => write(store, &current_root, *file_inode_ref, *offset, data)?,
		};

		for (_obj_id, obj) in txn.new_objects {
			combined_txn.add_object(obj);
		}
		current_root = txn.new_root;
	}

	combined_txn.new_root = current_root;
	Ok(combined_txn)
}

// endregion: --- Batch mutation

// region:    --- Private helpers

/// CoW-copy the parent directory to insert a new [`DirEntry`].
///
/// Returns `(new_dir_node, new_parent_inode)`: both ready to be added to a
/// [`TxnPlan`].
fn cow_insert_dir_entry<S: ObjectStore>(
	store: &S,
	parent_inode_ref: ObjectRef,
	name: &str,
	child_ref: ObjectRef,
) -> Result<(DirNode, Inode)> {
	let mut parent_inode = store.get_inode(parent_inode_ref)?;
	if parent_inode.kind != InodeKind::Directory {
		return Err(Error::invalid_operation("parent is not a directory"));
	}

	let mut dir_node = if parent_inode.content_ref.is_zero() {
		DirNode::new_leaf()
	} else {
		store.get_dir_node(parent_inode.content_ref)?
	};

	let entry = DirEntry::new(name, child_ref)?;
	dir_node.insert_entry(entry)?;

	// This is the point where a real B-tree would split. `DirNode::split`
	// exists and works, but nothing here calls it: every directory is a
	// single flat leaf (a deliberate scope cut, see this module's
	// doc comment). Refuse the insert that would cross the leaf bound rather
	// than letting the node grow unbounded.
	//
	// Without this the limit is not merely unimplemented, it is *silent*:
	// entries keep appending until the encoded node exceeds the reader's
	// `max_object_bytes`, at which point `read_object`'s scan skips the
	// record entirely and the whole directory (every file in it) reads
	// back as not found. Erroring on the one insert that crosses the line
	// leaves every already-committed entry readable and names the missing
	// feature. The check is here rather than in `DirNode::insert_entry`
	// precisely so that `split()` can still be given an over-full node to
	// operate on when it is finally wired up.
	if dir_node.needs_split() {
		return Err(Error::invalid_operation(
			"directory is full: B-tree splitting is not implemented (see DirNode::MAX_ENTRIES / MAX_CANONICAL_SIZE)",
		));
	}

	dir_node.update_object_id();

	parent_inode.content_ref = ObjectRef::from_id(dir_node.object_id);
	parent_inode.update_object_id();

	Ok((dir_node, parent_inode))
}

/// Recursively fill `buf` with data from an extent node tree covering
/// `[read_start, read_end)` in the file's logical address space.
fn fill_from_extent_node<S: ObjectStore>(
	store: &S,
	node: &ExtentNode,
	read_start: u64,
	read_end: u64,
	buf: &mut Vec<u8>,
) -> Result<()> {
	if node.is_leaf {
		for extent in &node.extents {
			let extent_end = extent.logical_offset + extent.length;

			// Skip extents that don't overlap with [read_start, read_end).
			if extent_end <= read_start || extent.logical_offset >= read_end {
				continue;
			}

			let chunk = store.get_chunk(ObjectRef::from_id(extent.chunk_id))?;

			let overlap_start = read_start.max(extent.logical_offset);
			let overlap_end = read_end.min(extent_end);

			let chunk_data_start = (overlap_start - extent.logical_offset + extent.chunk_offset) as usize;
			let chunk_data_end = (overlap_end - extent.logical_offset + extent.chunk_offset) as usize;
			let copy_len = (overlap_end - overlap_start) as usize;

			let buf_pos = (overlap_start - read_start) as usize;

			if chunk_data_end > chunk.data.len() || buf_pos + copy_len > buf.len() {
				return Err(Error::invalid_operation(
					"extent metadata inconsistent with chunk or read buffer size",
				));
			}
			buf[buf_pos..buf_pos + copy_len].copy_from_slice(&chunk.data[chunk_data_start..chunk_data_end]);
		}
	} else {
		// Internal node: recurse into children.
		for child_ref in &node.children {
			if !child_ref.is_zero() {
				let child = store.get_extent_node(*child_ref)?;
				fill_from_extent_node(store, &child, read_start, read_end, buf)?;
			}
		}
	}

	Ok(())
}

/// Recursively collect all [`DirEntry`]s from a [`DirNode`] sub-tree.
fn collect_dir_entries<S: ObjectStore>(store: &S, node: &DirNode) -> Result<Vec<DirEntry>> {
	if node.is_leaf {
		Ok(node.entries.clone())
	} else {
		// Internal node: gather children first, then this node's splitter keys.
		let mut entries = Vec::new();
		for child_ref in &node.children {
			if !child_ref.is_zero() {
				let child = store.get_dir_node(*child_ref)?;
				entries.extend(collect_dir_entries(store, &child)?);
			}
		}
		entries.extend(node.entries.clone());
		entries.sort(); // Deterministic ordering (DirEntry is Ord by name).
		Ok(entries)
	}
}

// endregion: --- Private helpers

// region:    --- Operation enum

/// A filesystem mutation that can be batched via [`apply_ops_to_root`].
#[derive(Debug, Clone)]
pub enum Operation {
	/// Create a directory (see [`mkdir`]).
	Mkdir {
		/// Reference to the parent directory's inode.
		parent_dir_ref: ObjectRef,
		/// Name of the new directory entry.
		name: String,
		/// Owner user ID.
		uid: u32,
		/// Owner group ID.
		gid: u32,
		/// Unix permission bits.
		mode: u32,
	},
	/// Create a regular file (see [`create_file`]).
	CreateFile {
		/// Reference to the parent directory's inode.
		parent_dir_ref: ObjectRef,
		/// Name of the new file entry.
		name: String,
		/// Owner user ID.
		uid: u32,
		/// Owner group ID.
		gid: u32,
		/// Unix permission bits.
		mode: u32,
	},
	/// Write bytes into an existing file (see [`write()`]).
	Write {
		/// Reference to the file inode to write to.
		file_inode_ref: ObjectRef,
		/// Byte offset in the file's logical address space.
		offset: u64,
		/// Bytes to write at `offset`.
		data: Vec<u8>,
	},
}

// endregion: --- Operation enum

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use crate::cow::object_store::InMemoryObjectStore;
	use alloc::string::ToString;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	// region:    --- Mutation tests

	#[test]
	fn test_operations_mkdir() -> TestResult<()> {
		// -- Setup & Fixtures
		let store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_ref = ObjectRef::zero();

		// -- Exec
		let txn = mkdir(&store, &root, parent_ref, "testdir", 1000, 1000, 0o755)?;

		// -- Check
		assert!(!txn.new_objects.is_empty());
		assert_eq!(txn.new_root.commit_seq, root.commit_seq + 1);

		Ok(())
	}

	#[test]
	fn test_operations_mkdir_sets_file_tree_root() -> TestResult<()> {
		// -- Setup & Fixtures: regression test; `mkdir`/`create_file`/`write`
		// used to only bump `commit_seq` and never populate
		// `new_root.file_tree_root` at all, leaving a committed root
		// permanently unable to find anything just created through it.
		let store = InMemoryObjectStore::new();
		let root = Root::new();

		// -- Exec: no parent: the new dir itself becomes the tree root.
		let txn = mkdir(&store, &root, ObjectRef::zero(), "toplevel", 0, 0, 0o755)?;

		// -- Check
		assert!(
			!txn.new_root.file_tree_root.is_zero(),
			"file_tree_root must be populated, not left zero"
		);
		let root_dir_inode = txn
			.new_objects
			.values()
			.find(|obj| obj.object_id == txn.new_root.file_tree_root.object_id)
			.expect("file_tree_root must point at an object actually in this txn");
		let decoded = Inode::decode_canonical(&root_dir_inode.canonical_bytes)?;
		assert_eq!(decoded.kind, InodeKind::Directory);

		Ok(())
	}

	#[test]
	fn test_operations_create_file_with_parent_sets_file_tree_root_to_parent() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);
		let parent_ref = ObjectRef::from_id(parent_txn.new_root.file_tree_root.object_id);

		// -- Exec
		let txn = create_file(&store, &parent_txn.new_root, parent_ref, "f.txt", 0, 0, 0o644)?;

		// -- Check: file_tree_root must point at the (CoW-copied) *parent*,
		// not the new file inode: the parent is what changed at the top of
		// this one-level chain.
		assert_ne!(
			txn.new_root.file_tree_root.object_id, parent_ref.object_id,
			"must be a NEW object id (CoW)"
		);
		let top = txn
			.new_objects
			.values()
			.find(|obj| obj.object_id == txn.new_root.file_tree_root.object_id)
			.expect("file_tree_root must point at an object actually in this txn");
		let decoded = Inode::decode_canonical(&top.canonical_bytes)?;
		assert_eq!(
			decoded.kind,
			InodeKind::Directory,
			"file_tree_root after create_file(with parent) must be the parent dir, not the file"
		);

		Ok(())
	}

	#[test]
	fn test_operations_create_file() -> TestResult<()> {
		// -- Setup & Fixtures
		let store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_ref = ObjectRef::zero();

		// -- Exec
		let txn = create_file(&store, &root, parent_ref, "test.txt", 1000, 1000, 0o644)?;

		// -- Check
		assert!(!txn.new_objects.is_empty());
		assert_eq!(txn.new_root.commit_seq, root.commit_seq + 1);

		Ok(())
	}

	#[test]
	fn test_operations_create_file_with_data_commits_the_file_already_written() -> TestResult<()> {
		// -- Setup & Fixtures: a committed parent to create into.
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let genesis = mkdir(&store, &root, ObjectRef::zero(), "", 0, 0, 0o755)?;
		store.apply_txn(&genesis);
		let parent_ref = ObjectRef::from_id(genesis.new_root.file_tree_root.object_id);
		let payload = b"one transaction, not two";

		// -- Exec
		let txn = create_file_with_data(
			&store,
			&genesis.new_root,
			parent_ref,
			"f.txt",
			Inode::new_file(1000, 1000, 0o644),
			payload,
		)?;

		// -- Check: follow the committed root down to the entry, the way a
		// reader would, and confirm the inode it reaches is already written.
		// This is the whole point of the operation: `create_file` + `write`
		// reaches the same shape in two transactions, the second of which
		// rewrites this inode and this directory node.
		let top = txn
			.new_objects
			.values()
			.find(|obj| obj.object_id == txn.new_root.file_tree_root.object_id)
			.expect("file_tree_root must point at an object actually in this txn");
		let top_inode = Inode::decode_canonical(&top.canonical_bytes)?;
		assert_eq!(
			top_inode.kind,
			InodeKind::Directory,
			"the root of this txn is the parent"
		);

		let dir_obj = txn
			.new_objects
			.values()
			.find(|obj| obj.object_id == top_inode.content_ref.object_id)
			.expect("the parent's directory node must be in this txn too");
		let dir = DirNode::decode_canonical(&dir_obj.canonical_bytes)?;
		let file_id = dir
			.entries
			.iter()
			.find(|e| e.name == "f.txt")
			.expect("the new file must be entered in its parent")
			.inode_id;

		let file_obj = txn
			.new_objects
			.values()
			.find(|obj| obj.object_id == file_id)
			.expect("the entry must name an object in this txn");
		let file_inode = Inode::decode_canonical(&file_obj.canonical_bytes)?;
		assert_eq!(file_inode.kind, InodeKind::RegularFile);
		assert_eq!(
			file_inode.size,
			payload.len() as u64,
			"the size is the write's, not zero"
		);
		assert!(
			!file_inode.content_ref.is_zero(),
			"a file created with data must point at its extent node"
		);

		// ...and once committed, the content is readable straight back through
		// the chain this one transaction placed.
		store.apply_txn(&txn);
		let read_back = read(&store, &txn.new_root, ObjectRef::from_id(file_id), 0, payload.len())?;
		assert_eq!(read_back, payload);

		Ok(())
	}

	#[test]
	fn test_operations_create_file_with_data_refuses_a_directory_inode() -> TestResult<()> {
		// -- Setup & Fixtures: an inode that is not a regular file.
		let store = InMemoryObjectStore::new();
		let root = Root::new();

		// -- Exec
		let result = create_file_with_data(
			&store,
			&root,
			ObjectRef::zero(),
			"d",
			Inode::new_directory(0, 0, 0o755),
			b"a directory cannot hold a chunk",
		);

		// -- Check: refused, rather than a directory that decodes as one and
		// carries content nothing will ever read.
		assert!(
			result.is_err(),
			"the inode parameter is the file's identity and must be a file's"
		);

		Ok(())
	}

	/// A directory that outgrows its single flat leaf must fail the insert
	/// that crosses the line, not keep growing.
	///
	/// The failure this prevents is the dangerous kind: an over-large
	/// `DirNode` still commits, but `reader::read_object`'s scan skips any
	/// record over `max_object_bytes`, so the directory and *every file in
	/// it* would read back as not found. Erroring keeps everything already
	/// committed readable.
	#[test]
	fn test_mkdir_refuses_to_grow_a_directory_past_its_unsplittable_leaf_bound() -> TestResult<()> {
		// -- Setup & Fixtures: a root directory filled to exactly the bound.
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let genesis = mkdir(&store, &root, ObjectRef::zero(), "", 0, 0, 0o755)?;
		store.apply_txn(&genesis);

		let mut current = genesis.new_root.clone();
		let mut parent = current.file_tree_root;
		for i in 0..DirNode::MAX_ENTRIES {
			let name = alloc::format!("f{:05}", i);
			let txn = create_file(&store, &current, parent, &name, 0, 0, 0o644)?;
			store.apply_txn(&txn);
			current = txn.new_root.clone();
			parent = current.file_tree_root;
		}

		// -- Exec: one more entry is one too many.
		let result = create_file(&store, &current, parent, "one-too-many", 0, 0, 0o644);

		// -- Check: a clean error, and everything already there still reads.
		assert!(
			result.is_err(),
			"the insert crossing the leaf bound must fail, not silently over-fill the node"
		);
		let entries = readdir(&store, &current, parent)?;
		assert_eq!(
			entries.len(),
			DirNode::MAX_ENTRIES,
			"every previously committed entry must remain readable"
		);

		Ok(())
	}

	/// An object larger than the reader's `max_object_bytes` must be refused
	/// at commit time, because the reader silently skips it: the commit
	/// would report success and the data would be unreachable.
	#[test]
	fn test_commit_refuses_an_object_the_reader_could_never_read_back() -> TestResult<()> {
		use crate::alloc_node::AllocNode;
		use crate::alloc_state::{AllocationState, PhysicalExtent};
		use crate::allocator::{Allocator, AllocatorConfig};
		use crate::commit::{CommitEngine, NullBlockDevice};
		use crate::cow::transaction::NewObject;

		// -- Setup & Fixtures
		let mut alloc_node = AllocNode::new();
		alloc_node.add_extent(PhysicalExtent::new(0, 8, AllocationState::Reserved))?;
		alloc_node.add_extent(PhysicalExtent::new(8, 4088, AllocationState::Free))?;
		let mut allocator = Allocator::new(AllocatorConfig::new(4096));
		allocator.init_from_tree(&alloc_node)?;

		let oversized = alloc::vec![0xAAu8; crate::MAX_OBJECT_SIZE + 1];
		let mut txn = TxnPlan::new(ObjectRef::zero(), Root::new());
		txn.add_object(NewObject::new(ObjectId::from_content(&oversized), oversized));

		// -- Exec
		let mut engine = CommitEngine::new();
		let result = engine.commit(&txn, alloc_node, &mut allocator, 0, &mut NullBlockDevice);

		// -- Check
		assert!(
			result.is_err(),
			"an object over max_object_bytes must be refused before it reaches the disk"
		);

		Ok(())
	}

	#[test]
	fn test_operations_write_chunk_survives_real_object_record_round_trip() -> TestResult<()> {
		// -- Setup & Fixtures: regression test for the `chunk.object_id` vs
		// `blake3(canonical_bytes)` trap documented in `commit.rs`'s
		// `test_commit_engine_actually_writes_decodable_object_data`.
		// `InMemoryObjectStore` alone can't catch this: it just stores
		// raw bytes keyed by whatever id you give it, so a self-consistent
		// but *wrong* id round-trips fine through it. Only the real
		// `ObjectRecordReader` (which independently recomputes
		// `blake3(payload)` and compares) actually exercises the invariant
		// that matters on disk.
		let root = Root::new();
		let mut store = InMemoryObjectStore::new();
		let file_txn = create_file(&store, &root, ObjectRef::zero(), "test.txt", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);
		let file_inode_ref = file_txn.new_objects.keys().next().copied().map(ObjectRef::from_id).unwrap();

		let payload = b"chunk object-id must match its canonical encoding";

		// -- Exec
		let txn = write(&store, &file_txn.new_root, file_inode_ref, 0, payload)?;

		// -- Check: find the chunk-shaped NewObject (smallest dependency
		// count / no dependencies of its own) and round-trip it through the
		// real on-disk record format.
		let chunk_obj = txn
			.new_objects
			.values()
			.find(|o| o.dependencies.is_empty())
			.expect("chunk NewObject must have no dependencies");
		let record = crate::record::ObjectRecord::from_object_id(
			chunk_obj.object_id,
			&chunk_obj.canonical_bytes,
			crate::record::ObjectType::Data,
			0,
			0,
		)?;
		let mut buf = vec![0u8; record.total_size()];
		record.encode(&mut buf)?;
		let (header, decoded) = crate::record::ObjectRecordReader::read(&buf)?;
		assert_eq!(
			header.object_id, chunk_obj.object_id.0,
			"stored object_id must match what decode independently recomputes"
		);
		assert_eq!(decoded, chunk_obj.canonical_bytes);

		Ok(())
	}

	#[test]
	fn test_operations_write_creates_three_objects() -> TestResult<()> {
		// -- Setup & Fixtures
		let root = Root::new();
		let mut store = InMemoryObjectStore::new();

		// Create a file to write to.
		let file_txn = create_file(&store, &root, ObjectRef::zero(), "test.txt", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);

		// Get the file inode ref from the txn.
		let file_inode_ref = file_txn.new_objects.keys().next().copied().map(ObjectRef::from_id).unwrap();

		let data = b"Hello, LedgerFS!";

		// -- Exec
		let txn = write(&store, &file_txn.new_root, file_inode_ref, 0, data)?;

		// -- Check: chunk + extent_node + updated inode = 3 new objects
		assert_eq!(txn.new_objects.len(), 3);
		assert_eq!(txn.new_root.commit_seq, file_txn.new_root.commit_seq + 1);

		Ok(())
	}

	#[test]
	fn test_operations_read_write_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let root = Root::new();
		let mut store = InMemoryObjectStore::new();

		// Create file.
		let file_txn = create_file(&store, &root, ObjectRef::zero(), "data.bin", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);
		let file_inode_key = *file_txn.new_objects.keys().next().unwrap();
		let file_inode_objref = ObjectRef::from_id(file_inode_key);

		// Write data.
		let payload = b"LedgerFS test payload";
		let write_txn = write(&store, &file_txn.new_root, file_inode_objref, 0, payload)?;
		store.apply_txn(&write_txn);

		// Get updated inode ref (write() creates a new inode with updated content_ref).
		let new_inode_objref = write_txn
			.new_objects
			.values()
			.find_map(|obj| {
				Inode::decode_canonical(&obj.canonical_bytes)
					.ok()
					.map(|i| ObjectRef::from_id(i.object_id))
			})
			.unwrap();

		// -- Exec
		let result = read(&store, &write_txn.new_root, new_inode_objref, 0, payload.len())?;

		// -- Check
		assert_eq!(result, payload);

		Ok(())
	}

	#[test]
	fn test_operations_read_partial() -> TestResult<()> {
		// -- Setup & Fixtures
		let root = Root::new();
		let mut store = InMemoryObjectStore::new();

		let file_txn = create_file(&store, &root, ObjectRef::zero(), "partial.bin", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);
		let file_inode_objref = ObjectRef::from_id(*file_txn.new_objects.keys().next().unwrap());

		let payload = b"Hello, World!"; // 13 bytes
		let write_txn = write(&store, &file_txn.new_root, file_inode_objref, 0, payload)?;
		store.apply_txn(&write_txn);

		let new_inode_objref = write_txn
			.new_objects
			.values()
			.find_map(|obj| {
				Inode::decode_canonical(&obj.canonical_bytes)
					.ok()
					.map(|i| ObjectRef::from_id(i.object_id))
			})
			.unwrap();

		// -- Exec: read only "World!" (6 bytes starting at offset 7)
		let result = read(&store, &write_txn.new_root, new_inode_objref, 7, 6)?;

		// -- Check
		assert_eq!(result, b"World!");

		Ok(())
	}

	#[test]
	fn test_operations_readdir_empty() -> TestResult<()> {
		// -- Setup & Fixtures
		let root = Root::new();
		let mut store = InMemoryObjectStore::new();

		let txn = mkdir(&store, &root, ObjectRef::zero(), "emptydir", 0, 0, 0o755)?;
		store.apply_txn(&txn);

		let dir_inode_ref = txn
			.new_objects
			.values()
			.find_map(|obj| {
				Inode::decode_canonical(&obj.canonical_bytes)
					.ok()
					.map(|i| ObjectRef::from_id(i.object_id))
			})
			.unwrap();

		// -- Exec
		let entries = readdir(&store, &txn.new_root, dir_inode_ref)?;

		// -- Check
		assert!(entries.is_empty());

		Ok(())
	}

	#[test]
	fn test_operations_mkdir_with_parent_lists_child() -> TestResult<()> {
		// -- Setup & Fixtures
		let root = Root::new();
		let mut store = InMemoryObjectStore::new();

		// Create a parent directory.
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "parent", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);

		// Find the parent dir inode from the txn (the inode with a content_ref set).
		let parent_inode_ref = parent_txn
			.new_objects
			.values()
			.find_map(|obj| {
				Inode::decode_canonical(&obj.canonical_bytes)
					.ok()
					.filter(|i| i.kind == InodeKind::Directory && !i.content_ref.is_zero())
					.map(|i| ObjectRef::from_id(i.object_id))
			})
			.unwrap();

		// -- Exec: create child under parent.
		let child_txn = mkdir(&store, &parent_txn.new_root, parent_inode_ref, "child", 0, 0, 0o755)?;
		store.apply_txn(&child_txn);

		// The plan must contain: child dir-tree, child inode, updated parent
		// dir-node, updated parent inode = at least 4 objects.
		assert!(
			child_txn.new_objects.len() >= 4,
			"expected >= 4 objects, got {}",
			child_txn.new_objects.len()
		);

		// -- Check: at least one directory inode in the txn should have "child"
		// in its readdir output (the updated parent inode).  The child inode
		// itself has an empty DirNode, so exactly one of the two directory inodes
		// will return a non-empty listing.
		let found = child_txn
			.new_objects
			.values()
			.filter_map(|obj| Inode::decode_canonical(&obj.canonical_bytes).ok())
			.filter(|i| i.kind == InodeKind::Directory && !i.content_ref.is_zero())
			.any(|i| {
				readdir(&store, &child_txn.new_root, ObjectRef::from_id(i.object_id))
					.ok()
					.map(|entries| entries.iter().any(|e| e.name.as_str() == "child"))
					.unwrap_or(false)
			});

		assert!(found, "'child' not found in any directory readdir from the txn");

		Ok(())
	}

	#[test]
	fn test_operations_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_ref = ObjectRef::zero();

		// -- Exec - Same operation twice should produce identical object IDs.
		let txn1 = create_file(&store, &root, parent_ref, "test.txt", 1000, 1000, 0o644)?;
		let txn2 = create_file(&store, &root, parent_ref, "test.txt", 1000, 1000, 0o644)?;

		// -- Check
		let keys1: Vec<_> = txn1.new_objects.keys().collect();
		let keys2: Vec<_> = txn2.new_objects.keys().collect();
		assert_eq!(keys1, keys2);

		Ok(())
	}

	#[test]
	fn test_apply_ops_to_root() -> TestResult<()> {
		// -- Setup & Fixtures
		let store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_ref = ObjectRef::zero();

		let ops = vec![
			Operation::Mkdir {
				parent_dir_ref: parent_ref,
				name: "dir1".to_string(),
				uid: 1000,
				gid: 1000,
				mode: 0o755,
			},
			Operation::CreateFile {
				parent_dir_ref: parent_ref,
				name: "file1.txt".to_string(),
				uid: 1000,
				gid: 1000,
				mode: 0o644,
			},
		];

		// -- Exec
		let txn = apply_ops_to_root(&store, &root, &ops)?;

		// -- Check
		assert!(txn.new_objects.len() >= 2);
		assert_eq!(txn.new_root.commit_seq, root.commit_seq + 2);

		Ok(())
	}

	#[test]
	fn test_operations_unlink_missing_source_is_an_error() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);
		let parent_ref = ObjectRef::from_id(parent_txn.new_root.file_tree_root.object_id);

		// -- Exec & Check: unlinking a name that was never created must fail,
		// not silently no-op.
		let result = unlink(&store, &parent_txn.new_root, parent_ref, "does-not-exist");
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_operations_unlink_refuses_directories() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);
		let parent_ref = ObjectRef::from_id(parent_txn.new_root.file_tree_root.object_id);

		let child_txn = mkdir(&store, &parent_txn.new_root, parent_ref, "childdir", 0, 0, 0o755)?;
		store.apply_txn(&child_txn);
		let parent_ref = ObjectRef::from_id(child_txn.new_root.file_tree_root.object_id);

		// -- Exec & Check: POSIX semantics: refuse to unlink a directory.
		let result = unlink(&store, &child_txn.new_root, parent_ref, "childdir");
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_operations_unlink_removes_the_entry_and_updates_file_tree_root() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);
		let parent_ref = ObjectRef::from_id(parent_txn.new_root.file_tree_root.object_id);

		let file_txn = create_file(&store, &parent_txn.new_root, parent_ref, "f.txt", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);
		let parent_ref = ObjectRef::from_id(file_txn.new_root.file_tree_root.object_id);

		// -- Exec
		let txn = unlink(&store, &file_txn.new_root, parent_ref, "f.txt")?;
		store.apply_txn(&txn);

		// -- Check: the new parent dir node no longer lists the entry.
		let new_parent_inode = store.get_inode(ObjectRef::from_id(txn.new_root.file_tree_root.object_id))?;
		let new_dir_node = store.get_dir_node(new_parent_inode.content_ref)?;
		assert!(new_dir_node.entries.iter().all(|e| e.name.as_str() != "f.txt"));

		Ok(())
	}

	#[test]
	fn test_operations_rename_missing_source_is_an_error() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);
		let parent_ref = ObjectRef::from_id(parent_txn.new_root.file_tree_root.object_id);

		// -- Exec & Check
		let result = rename(
			&store,
			&parent_txn.new_root,
			parent_ref,
			"nope",
			parent_ref,
			"still-nope",
		);
		assert!(result.is_err());

		Ok(())
	}

	#[test]
	fn test_operations_rename_same_parent_moves_the_entry() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let parent_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&parent_txn);
		let parent_ref = ObjectRef::from_id(parent_txn.new_root.file_tree_root.object_id);

		let file_txn = create_file(&store, &parent_txn.new_root, parent_ref, "old.txt", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);
		let parent_ref = ObjectRef::from_id(file_txn.new_root.file_tree_root.object_id);

		// -- Exec
		let txn = rename(&store, &file_txn.new_root, parent_ref, "old.txt", parent_ref, "new.txt")?;
		store.apply_txn(&txn);

		// -- Check
		let new_parent_inode = store.get_inode(ObjectRef::from_id(txn.new_root.file_tree_root.object_id))?;
		let new_dir_node = store.get_dir_node(new_parent_inode.content_ref)?;
		assert!(
			new_dir_node.entries.iter().all(|e| e.name.as_str() != "old.txt"),
			"old name must be gone"
		);
		assert!(
			new_dir_node.entries.iter().any(|e| e.name.as_str() == "new.txt"),
			"new name must be present"
		);

		Ok(())
	}

	#[test]
	fn test_operations_rename_cross_parent_moves_the_entry() -> TestResult<()> {
		// -- Setup & Fixtures: two sibling directories under one root.
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let root_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&root_txn);
		let root_ref = ObjectRef::from_id(root_txn.new_root.file_tree_root.object_id);

		let a_txn = mkdir(&store, &root_txn.new_root, root_ref, "a", 0, 0, 0o755)?;
		store.apply_txn(&a_txn);
		let root_ref = ObjectRef::from_id(a_txn.new_root.file_tree_root.object_id);

		let b_txn = mkdir(&store, &a_txn.new_root, root_ref, "b", 0, 0, 0o755)?;
		store.apply_txn(&b_txn);
		let root_ref = ObjectRef::from_id(b_txn.new_root.file_tree_root.object_id);
		let root_dir_node = store.get_dir_node(store.get_inode(root_ref)?.content_ref)?;
		let a_ref = root_dir_node
			.entries
			.iter()
			.find(|e| e.name.as_str() == "a")
			.unwrap()
			.inode_ref();
		let b_ref = root_dir_node
			.entries
			.iter()
			.find(|e| e.name.as_str() == "b")
			.unwrap()
			.inode_ref();

		let file_txn = create_file(&store, &b_txn.new_root, a_ref, "f.txt", 0, 0, 0o644)?;
		store.apply_txn(&file_txn);
		let a_ref = ObjectRef::from_id(file_txn.new_root.file_tree_root.object_id);

		// -- Exec: move a/f.txt -> b/g.txt
		let txn = rename(&store, &file_txn.new_root, a_ref, "f.txt", b_ref, "g.txt")?;
		store.apply_txn(&txn);

		// -- Check: destination side (file_tree_root) has the new entry.
		let new_b_inode = store.get_inode(ObjectRef::from_id(txn.new_root.file_tree_root.object_id))?;
		let new_b_dir_node = store.get_dir_node(new_b_inode.content_ref)?;
		assert!(new_b_dir_node.entries.iter().any(|e| e.name.as_str() == "g.txt"));

		Ok(())
	}

	#[test]
	fn test_operations_rename_refuses_directory_into_itself() -> TestResult<()> {
		// -- Setup & Fixtures: root/a/b: try to rename root/a into root/a/b.
		// Since mkdir only CoWs one level up (see its doc comment), creating
		// "b" under "a" does not automatically update root's entry for "a",
		// this test manually propagates that update via `cow_update_dir_entry`
		// (exactly the pattern a multi-level-aware caller is documented to
		// need), so the root handed to `rename` below is fully consistent
		// and the cycle actually exists to be detected.
		let mut store = InMemoryObjectStore::new();
		let root = Root::new();
		let root_txn = mkdir(&store, &root, ObjectRef::zero(), "root", 0, 0, 0o755)?;
		store.apply_txn(&root_txn);
		let root_ref = ObjectRef::from_id(root_txn.new_root.file_tree_root.object_id);

		let a_txn = mkdir(&store, &root_txn.new_root, root_ref, "a", 0, 0, 0o755)?;
		store.apply_txn(&a_txn);
		let root_ref = ObjectRef::from_id(a_txn.new_root.file_tree_root.object_id);
		let root_dir_node = store.get_dir_node(store.get_inode(root_ref)?.content_ref)?;
		let a_ref = root_dir_node
			.entries
			.iter()
			.find(|e| e.name.as_str() == "a")
			.unwrap()
			.inode_ref();

		let b_txn = mkdir(&store, &a_txn.new_root, a_ref, "b", 0, 0, 0o755)?;
		store.apply_txn(&b_txn);
		let a_ref = ObjectRef::from_id(b_txn.new_root.file_tree_root.object_id);

		// Manually propagate the updated "a" (now containing "b") up into
		// root, since mkdir itself only went one level.
		let (root_dir_node, root_inode) = cow_update_dir_entry(&store, root_ref, "a", a_ref)?;
		let mut propagate_txn = TxnPlan::new(
			ObjectRef::from_id(b_txn.new_root.object_id),
			b_txn.new_root.next_commit(),
		);
		propagate_txn.add_object(NewObject::new(
			root_dir_node.object_id,
			root_dir_node.encode_canonical()?,
		));
		propagate_txn.add_object(
			NewObject::new(root_inode.object_id, root_inode.encode_canonical()?)
				.with_dependency(root_dir_node.object_id),
		);
		propagate_txn.new_root.file_tree_root = ObjectRef::from_id(root_inode.object_id);
		store.apply_txn(&propagate_txn);
		let root_ref = ObjectRef::from_id(propagate_txn.new_root.file_tree_root.object_id);

		let a_dir_node = store.get_dir_node(store.get_inode(a_ref)?.content_ref)?;
		let b_ref = a_dir_node.entries.iter().find(|e| e.name.as_str() == "b").unwrap().inode_ref();

		// -- Exec & Check: moving "a" into "a/b" must be refused: it would
		// create a cycle unreachable from the real root.
		let result = rename(
			&store,
			&propagate_txn.new_root,
			root_ref,
			"a",
			b_ref,
			"a-moved-into-child",
		);
		assert!(result.is_err());

		Ok(())
	}

	// endregion: --- Mutation tests
}

// endregion: --- Tests
