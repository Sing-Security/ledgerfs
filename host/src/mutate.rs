//! Turning a path-level mutation into a fully consistent `TxnPlan`.
//!
//! # Why this module exists
//!
//! `cow::operations` is deliberately leaf-level. `mkdir` and `create_file`
//! CoW-copy their immediate parent and stop there — their own doc comments say
//! so, calling it a known follow-up rather than a solved problem. `write` does
//! not even do that: it has no parent reference at all, only the file inode it
//! is updating, so it never learns which directory entry points at that inode.
//!
//! The consequence is a trap, not a rough edge. `new_root.file_tree_root` after
//! a nested `mkdir("/a/b/c")` names the CoW copy of `/a/b` — the top of *the
//! modified chain*, not the volume root. Commit that transaction and the
//! filesystem is silently re-rooted at `/a/b`: every path outside it stops
//! resolving, and the volume still mounts, because the superblock it publishes
//! is perfectly valid and simply points somewhere wrong.
//!
//! So something has to walk the rest of the way up, re-pointing each ancestor's
//! entry at its CoW-copied child until the chain reaches the real root. A
//! volume driver inside an OS keeps that ancestor-cascade step next to its
//! create/write transaction builders. This module is the same three pieces for
//! a host process.
//!
//! # Not a reimplementation
//!
//! They are not written from scratch and they do not derive a single object
//! themselves: every `DirNode`, `Inode`, `ExtentNode` and `Chunk` is produced by
//! the kernel crate's own `cow` functions. What is duplicated is only the
//! ordering — which parent gets re-pointed at which child, deepest first — and
//! it is duplicated because the other implementation of it lives inside an OS,
//! wired to interfaces a host-side store cannot depend on.

use ledgerfs::cow::{
	NewObject, ObjectStore, TxnPlan, cow_update_dir_entry, create_file, create_file_with_data, mkdir, rename, unlink,
	write,
};
use ledgerfs::object::{Inode, ObjectRef, Root};
use ledgerfs::{Error as LedgerFsError, Result as LedgerFsResult};

use crate::error::{Error, Result};

/// A mutation target as `(the directory an entry lives in, the entry's own
/// name)`, one pair per path component.
///
/// Built by [`HostFs::resolve_chain`](crate::HostFs::resolve_chain), whose last
/// pair is the target itself paired with the directory it would live in — the
/// entry deliberately not looked up, since for a create it does not exist yet.
pub(crate) type Chain = [(ObjectRef, String)];

// region:    --- Transaction builders

/// The transaction that creates `chain.last()` — a directory when `is_dir`,
/// otherwise an empty regular file — cascaded to a fully consistent new root.
///
/// Does not check whether the target already exists; the caller does, because
/// the caller is the one that knows whether that is an error or a no-op.
pub(crate) fn build_create_txn<S: ObjectStore>(
	store: &S,
	current_root: &Root,
	chain: &Chain,
	uid: u32,
	gid: u32,
	mode: u32,
	is_dir: bool,
) -> Result<TxnPlan> {
	let (parent_ref, name) = last_of(chain)?;

	let mut txn = if is_dir {
		mkdir(store, current_root, parent_ref, &name, uid, gid, mode)?
	} else {
		create_file(store, current_root, parent_ref, &name, uid, gid, mode)?
	};

	// `mkdir`/`create_file` have already CoW-copied the immediate parent and
	// put it in `new_root.file_tree_root`. Everything above it still has to
	// follow, or the volume gets re-rooted one level too deep.
	let new_parent_ref = txn.new_root.file_tree_root;
	cascade_dir_entry_update(store, &mut txn, parent_chain(chain)?, new_parent_ref)?;

	Ok(txn)
}

/// The transaction that creates `chain.last()` as a regular file already
/// containing `data`, cascaded to a fully consistent new root.
///
/// [`build_create_txn`] and [`build_write_txn`] are two commits; this is one.
/// That matters here and not only in the abstract, because the commit is the
/// volume's durability point: a caller storing a 256-byte record and waiting for
/// the write to be durable was paying two of them, and the inode and directory
/// node the first wrote were rewritten by the second.
///
/// The cascade is [`build_create_txn`]'s verbatim — the kernel side leaves
/// `file_tree_root` at the CoW copy of the immediate parent, exactly as
/// `mkdir`/`create_file` do, and everything above it still has to follow.
///
/// `data` must fit in a single object; the caller splits a longer payload, as
/// it does for [`build_write_txn`].
pub(crate) fn build_create_with_data_txn<S: ObjectStore>(
	store: &S,
	current_root: &Root,
	chain: &Chain,
	data: &[u8],
	uid: u32,
	gid: u32,
	mode: u32,
) -> Result<TxnPlan> {
	let (parent_ref, name) = last_of(chain)?;

	let mut txn = create_file_with_data(
		store,
		current_root,
		parent_ref,
		&name,
		Inode::new_file(uid, gid, mode),
		data,
	)?;

	let new_parent_ref = txn.new_root.file_tree_root;
	cascade_dir_entry_update(store, &mut txn, parent_chain(chain)?, new_parent_ref)?;

	Ok(txn)
}

/// The transaction that writes `data` at `offset` into the existing file at
/// `target_ref`, cascaded to a fully consistent new root.
pub(crate) fn build_write_txn<S: ObjectStore>(
	store: &S,
	current_root: &Root,
	chain: &Chain,
	target_ref: ObjectRef,
	offset: u64,
	data: &[u8],
) -> Result<TxnPlan> {
	let mut txn = write(store, current_root, target_ref, offset, data)?;

	// `write` hands back the new file inode nowhere else, and it has to be
	// named to re-point the parent's entry at it. It is the last object in
	// dependency order because nothing in this transaction depends on the
	// updated inode — the chain runs chunk, then extent node, then inode.
	let new_inode_id = txn
		.objects_in_dependency_order()?
		.last()
		.ok_or_else(|| Error::Format(LedgerFsError::custom("write produced an empty TxnPlan")))?
		.object_id;
	let new_inode_ref = ObjectRef::from_id(new_inode_id);

	let (parent_ref, name) = last_of(chain)?;

	// The "propagate to parent" half `write` leaves out by design.
	let (dir_node, parent_inode) = cow_update_dir_entry(store, parent_ref, &name, new_inode_ref)?;
	add_dir_pair(&mut txn, &dir_node, &parent_inode)?;

	// And `cow_update_dir_entry` is itself one level, so the rest of the chain
	// follows from the parent it produced.
	let new_parent_ref = ObjectRef::from_id(parent_inode.object_id);
	cascade_dir_entry_update(store, &mut txn, parent_chain(chain)?, new_parent_ref)?;

	Ok(txn)
}

/// The transaction that removes `chain.last()` — a file — cascaded to a fully
/// consistent new root.
///
/// `unlink` already CoW-copies the immediate parent, exactly as `mkdir`/
/// `create_file` do, so the cascade that follows is the same one
/// [`build_create_txn`] runs: the parent's own ancestors still have to be
/// re-pointed at their copies or the removal is invisible from the volume root.
pub(crate) fn build_unlink_txn<S: ObjectStore>(store: &S, current_root: &Root, chain: &Chain) -> Result<TxnPlan> {
	let (parent_ref, name) = last_of(chain)?;

	let mut txn = unlink(store, current_root, parent_ref, &name)?;
	let new_parent_ref = txn.new_root.file_tree_root;
	cascade_dir_entry_update(store, &mut txn, parent_chain(chain)?, new_parent_ref)?;

	Ok(txn)
}

/// The transaction that renames `old_name` to `chain.last()`'s name, both in
/// the same directory, cascaded to a fully consistent new root.
///
/// # Why only the same directory
///
/// `kernel`'s `rename` handles a cross-directory move, but its own doc comment
/// says it can only point `file_tree_root` at one of the **two** one-level CoW
/// chains such a move touches: it names the destination side, and the caller is
/// left to propagate the source side. Doing that here is not impossible, it is
/// merely not done — the two chains can share an ancestor, and re-pointing a
/// shared ancestor twice from stale references produces two roots rather than
/// one. Rather than ship a cascade that is silently wrong for that case,
/// [`Store::rename`](crate::Store::rename) refuses a cross-directory move and
/// this builder is only ever handed a same-directory pair, where the two chains
/// are one chain and the cascade above is the ordinary one.
pub(crate) fn build_rename_txn<S: ObjectStore>(
	store: &S,
	current_root: &Root,
	chain: &Chain,
	old_name: &str,
) -> Result<TxnPlan> {
	let (parent_ref, new_name) = last_of(chain)?;

	// Same parent on both sides, so the two references `rename` takes are one.
	let mut txn = rename(store, current_root, parent_ref, old_name, parent_ref, &new_name)?;
	let new_parent_ref = txn.new_root.file_tree_root;
	cascade_dir_entry_update(store, &mut txn, parent_chain(chain)?, new_parent_ref)?;

	Ok(txn)
}

// endregion: --- Transaction builders

// region:    --- Cascade

/// Re-point each remaining ancestor's entry at its CoW-copied child, deepest
/// first, and leave `new_root.file_tree_root` naming the volume root.
///
/// `ancestors` is the chain with its final entry removed — that entry was
/// already consumed producing `new_immediate_parent_ref`. Iterating in reverse
/// is what makes it work: each step's new inode becomes the next step's child,
/// so the rewrite walks up one directory level per iteration and the last one
/// to run is the volume root.
///
/// A no-op when `ancestors` is empty, which is the common case of a target
/// directly under the root. It still assigns `new_root.file_tree_root`, so
/// callers can rely on this being the one place that decides what the new root
/// is.
pub(crate) fn cascade_dir_entry_update<S: ObjectStore>(
	store: &S,
	txn: &mut TxnPlan,
	ancestors: &Chain,
	new_immediate_parent_ref: ObjectRef,
) -> Result<()> {
	let mut new_child_ref = new_immediate_parent_ref;

	for (ancestor_ref, child_name) in ancestors.iter().rev() {
		let (dir_node, inode) = cow_update_dir_entry(store, *ancestor_ref, child_name, new_child_ref)?;
		add_dir_pair(txn, &dir_node, &inode)?;
		new_child_ref = ObjectRef::from_id(inode.object_id);
	}

	txn.new_root.file_tree_root = new_child_ref;

	Ok(())
}

// endregion: --- Cascade

// region:    --- Support

/// Add a CoW-copied directory as a `(DirNode, Inode)` pair, with the inode
/// depending on the node it points at.
///
/// The dependency is not decoration: `CommitEngine` orders its writes by it, and
/// an inode placed before the `DirNode` it references would be reachable from a
/// committed root while the node it names is not yet on disk.
fn add_dir_pair(
	txn: &mut TxnPlan,
	dir_node: &ledgerfs::object::DirNode,
	inode: &ledgerfs::object::Inode,
) -> LedgerFsResult<()> {
	txn.add_object(NewObject::new(dir_node.object_id, dir_node.encode_canonical()?));
	txn.add_object(NewObject::new(inode.object_id, inode.encode_canonical()?).with_dependency(dir_node.object_id));
	Ok(())
}

/// The chain's target: the directory it lives in, and its own name.
fn last_of(chain: &Chain) -> Result<(ObjectRef, String)> {
	chain
		.last()
		.cloned()
		.ok_or_else(|| Error::Usage("a mutation needs at least one path component".into()))
}

/// The chain without its target — what [`cascade_dir_entry_update`] consumes.
fn parent_chain(chain: &Chain) -> Result<&Chain> {
	chain
		.split_last()
		.map(|(_, ancestors)| ancestors)
		.ok_or_else(|| Error::Usage("a mutation needs at least one path component".into()))
}

// endregion: --- Support

// region:    --- Tests

#[cfg(test)]
mod tests {
	type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

	#[test]
	fn test_mutate_parent_chain_drops_the_target() -> Result<()> {
		// -- Setup & Fixtures
		use ledgerfs::object::ObjectRef;
		let chain = vec![
			(ObjectRef::zero(), "a".to_string()),
			(ObjectRef::zero(), "b".to_string()),
			(ObjectRef::zero(), "c".to_string()),
		];

		// -- Exec
		let ancestors = super::parent_chain(&chain)?;
		let (_, name) = super::last_of(&chain)?;

		// -- Check: the cascade must see `a` and `b`; `c` was already consumed
		// producing the immediate parent's new reference.
		assert_eq!(name, "c");
		assert_eq!(ancestors.len(), 2);
		assert_eq!(ancestors[0].1, "a");
		assert_eq!(ancestors[1].1, "b");

		// -- Exec & Check: a single-component chain (a target directly under
		// the root) cascades nothing.
		let top_level = vec![(ObjectRef::zero(), "only".to_string())];
		assert!(super::parent_chain(&top_level)?.is_empty());
		assert!(super::parent_chain(&[] as &super::Chain).is_err());

		Ok(())
	}
}

// endregion: --- Tests
