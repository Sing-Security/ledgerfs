//! Folding several path-level mutations into **one** transaction.
//!
//! `cow::operations`' builders are single-operation: `write` produces one
//! transaction for one piece of data, and a caller that has more than one
//! piece has to decide what the pieces mean to each other. Sequentially
//! committed, they are several durability points — and worse, several
//! out-of-space verdicts, so a five-piece write that runs out of room on the
//! third piece leaves the first two behind as a file the caller never asked
//! for.
//!
//! This module is the other answer. A [`StagedFold`] runs the single-operation
//! builders one after another against an [`OverlayStore`] — the volume, plus
//! every object a previous step has staged — and threads the resulting root
//! descriptor forward, so each step sees the previous steps' objects exactly
//! as a commit would have shown them. [`into_plan`](StagedFold::into_plan)
//! then hands back **one** [`TxnPlan`], in the ordinary shape every commit
//! takes.
//!
//! # Why not `cow::apply_ops_to_root`
//!
//! The kernel has a list-taking fold, and this module deliberately does not
//! use it: `apply_ops_to_root` threads only the *root descriptor* between
//! operations and reads every operation through one immutable store, so
//! operation N+1 cannot see operation N's objects. An append followed by
//! another append to the same file would build both pieces on top of the
//! original inode, and only the last would survive. The overlay is the
//! difference: it is the one immutable store *plus* everything staged so far.
//!
//! # The prune
//!
//! Each staged operation re-places every object on its path — that is what
//! CoW is — so a five-append fold holds five generations of the extent node,
//! the inode and the directory pair. Only the last generation is reachable
//! from the final root. `into_plan` walks the final root through the overlay
//! and keeps exactly the staged objects it reaches, dropping the intermediate
//! generations: the commit carries N pieces' chunks plus **one** copy of each
//! metadata object, rather than one per piece.
//!
//! # What a fold does not touch
//!
//! No device block is allocated and nothing reaches the disk until the
//! returned plan is committed — placement is `CommitEngine`'s, at commit
//! time. Path resolution still reads the committed volume, as any verb does;
//! what the fold adds is that every *staged* object resolves out of memory,
//! and that the prune walk stops at the committed boundary rather than
//! descending into objects the volume already has.

use std::collections::{BTreeMap, BTreeSet};

use ledgerfs::Result as LedgerFsResult;
use ledgerfs::cow::{NewObject, ObjectStore, TxnPlan};
use ledgerfs::object::{Chunk, DirNode, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef, Root};

use crate::error::{Error, Result};
use crate::mutate::{build_create_with_data_txn, build_write_txn};
use crate::objstore::DiskObjectStore;
use crate::path::{NodeStore, lookup_child_in, resolve_chain_in};

// region:    --- Staged objects

/// The objects a fold has staged so far, keyed by content id.
///
/// Two operations in one fold routinely produce the *same* object — the same
/// bytes appended twice, or an ancestor directory two writes both CoW-copy
/// without change — and content addressing makes those one entry here, as
/// they will be one object on the volume.
#[derive(Default)]
pub(crate) struct StagedObjects {
	objects: BTreeMap<ObjectId, NewObject>,
}

impl StagedObjects {
	/// Absorb every new object a staged operation produced.
	pub(crate) fn absorb(&mut self, plan: &TxnPlan) {
		for (id, object) in &plan.new_objects {
			self.objects.insert(*id, object.clone());
		}
	}

	/// Whether `id` names a staged object — i.e. one the volume does not have
	/// yet.
	pub(crate) fn contains(&self, id: &ObjectId) -> bool {
		self.objects.contains_key(id)
	}
}

// endregion: --- Staged objects

// region:    --- Overlay

/// The committed volume **plus** everything a fold has staged: an
/// [`ObjectStore`] whose lookups answer from the staged set first and fall
/// through to the volume.
///
/// This is the piece that lets operation N+1 see operation N's objects — see
/// the module doc for the trap it avoids. Resolution is by content id, which
/// is why a staged lookup needs no hint: staged objects have no placement
/// until the fold is committed.
pub(crate) struct OverlayStore<'a> {
	base: &'a DiskObjectStore,
	staged: &'a StagedObjects,
}

impl<'a> OverlayStore<'a> {
	pub(crate) fn new(base: &'a DiskObjectStore, staged: &'a StagedObjects) -> Self {
		Self { base, staged }
	}

	/// The staged bytes for `id`, when it is staged.
	pub(crate) fn staged_bytes(&self, id: &ObjectId) -> Option<&Vec<u8>> {
		self.staged.objects.get(id).map(|o| &o.canonical_bytes)
	}
}

impl NodeStore for OverlayStore<'_> {
	fn inode_of(&self, obj_ref: ObjectRef) -> Result<Inode> {
		match self.staged_bytes(&obj_ref.object_id) {
			Some(bytes) => Inode::decode_canonical(bytes).map_err(Error::from),
			None => self.base.inode(obj_ref),
		}
	}

	fn dir_node_of(&self, obj_ref: ObjectRef) -> Result<DirNode> {
		match self.staged_bytes(&obj_ref.object_id) {
			Some(bytes) => DirNode::decode_canonical(bytes).map_err(Error::from),
			None => self.base.dir_node(obj_ref),
		}
	}
}

impl ObjectStore for OverlayStore<'_> {
	fn get_inode(&self, obj_ref: ObjectRef) -> LedgerFsResult<Inode> {
		match self.staged_bytes(&obj_ref.object_id) {
			Some(bytes) => Inode::decode_canonical(bytes),
			None => self.base.get_inode(obj_ref),
		}
	}

	fn get_dir_node(&self, obj_ref: ObjectRef) -> LedgerFsResult<DirNode> {
		match self.staged_bytes(&obj_ref.object_id) {
			Some(bytes) => DirNode::decode_canonical(bytes),
			None => self.base.get_dir_node(obj_ref),
		}
	}

	fn get_extent_node(&self, obj_ref: ObjectRef) -> LedgerFsResult<ExtentNode> {
		match self.staged_bytes(&obj_ref.object_id) {
			Some(bytes) => ExtentNode::decode_canonical(bytes),
			None => self.base.get_extent_node(obj_ref),
		}
	}

	fn get_chunk(&self, obj_ref: ObjectRef) -> LedgerFsResult<Chunk> {
		match self.staged_bytes(&obj_ref.object_id) {
			Some(bytes) => Chunk::decode_canonical(bytes),
			None => self.base.get_chunk(obj_ref),
		}
	}

	fn record_hint(&self, object_id: ObjectId, physical_lba: u64) {
		// Only a commit places objects, and a commit records its own hints
		// into the volume's cache directly; a fold never places. Forwarding
		// keeps the trait's contract ("callers should invoke it for every
		// entry in a CommitResult's storage_hints") honest should an engine
		// ever run under an overlay.
		self.base.record_hint(object_id, physical_lba);
	}
}

// endregion: --- Overlay

// region:    --- Fold

/// A sequence of staged operations over one volume, folded into one plan.
///
/// Built against the volume's committed state; driven one operation at a
/// time; finished with [`into_plan`](Self::into_plan), whose result goes
/// through [`Store::commit`](crate::Store::commit) like any other
/// transaction.
pub(crate) struct StagedFold<'a> {
	base: &'a DiskObjectStore,

	staged: StagedObjects,

	/// The root descriptor as the last staged operation left it: its
	/// `file_tree_root` names the newest generation of the root directory's
	/// `Inode`, and every operation after the first resolves its path
	/// through the objects this descriptor names.
	root: Root,

	/// The root descriptor the fold started from — the merged plan's
	/// `prev_root`.
	base_root: Root,
}

impl<'a> StagedFold<'a> {
	pub(crate) fn new(base: &'a DiskObjectStore, root: &Root) -> Self {
		Self {
			base,
			staged: StagedObjects::default(),
			root: root.clone(),
			base_root: root.clone(),
		}
	}

	/// Stage `data` as the content of a **new** file at `path`.
	///
	/// The caller has already checked the name is free (`ensure_absent`);
	/// this runs the create-with-content builder against the overlay so any
	/// staged ancestor generation is the one it copies.
	pub(crate) fn create_with_data(&mut self, path: &str, data: &[u8], mode: u32) -> Result<()> {
		let chain = self.resolve_chain(path)?;
		let txn = build_create_with_data_txn(&self.overlay(), &self.root, &chain, data, 0, 0, mode)?;
		self.absorb(txn);
		Ok(())
	}

	/// Stage one `write` of `data` at `offset` into the file at `path`.
	///
	/// The path is resolved through the overlay on **every** call, and it has
	/// to be: each staged write produces a new generation of the file's
	/// inode, and the reference from the previous call names an object that
	/// exists only in the staged set. Building on a stale reference would
	/// leave only the last write standing — the same trap committing each
	/// piece separately had, at fold speed.
	pub(crate) fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
		let chain = self.resolve_chain(path)?;
		let (parent_ref, name) = chain
			.last()
			.cloned()
			.ok_or_else(|| Error::Usage("a mutation needs at least one path component".into()))?;
		let file_ref = {
			let overlay = self.overlay();
			lookup_child_in(&overlay, path, parent_ref, &name)?
				.ok_or_else(|| Error::NoSuchFile { path: path.to_string() })?
		};
		let txn = build_write_txn(&self.overlay(), &self.root, &chain, file_ref, offset, data)?;
		self.absorb(txn);
		Ok(())
	}

	/// Fold the staged operations into the one [`TxnPlan`] a commit takes.
	///
	/// `committed_seq` is the volume's current commit sequence; the merged
	/// root carries `committed_seq + 1`, because the fold is one commit no
	/// matter how many operations built it — the per-operation increments the
	/// builders threaded are discarded here.
	pub(crate) fn into_plan(mut self, committed_seq: u64) -> Result<TxnPlan> {
		self.root.commit_seq = committed_seq + 1;
		self.root.update_object_id();

		// Prune to reachability from the final root. This is what makes the
		// commit linear in the staged bytes: without it, every generation of
		// every re-placed object would ride along, and a many-piece write
		// would carry quadratic metadata for nothing.
		let tree_root = self.root.file_tree_root;
		let overlay = self.overlay();
		let mut keep = BTreeSet::new();
		retain_reachable_inode(&overlay, tree_root, &self.staged, &mut keep)?;

		let mut plan = TxnPlan::new(ObjectRef::from_id(self.base_root.object_id), self.root.clone());
		for (id, object) in &self.staged.objects {
			if !keep.contains(id) {
				continue;
			}
			// A kept object's declared dependencies are the operands it was
			// built from. An intermediate generation is one of those operands
			// and is *not* kept, so the edge has to go with it: a dependency
			// the plan does not carry is not an error to `CommitEngine`, but
			// leaving it in would describe a transaction that references an
			// object nobody writes.
			let dependencies: Vec<ObjectId> =
				object.dependencies.iter().copied().filter(|dep| keep.contains(dep)).collect();
			let mut kept = NewObject::new(*id, object.canonical_bytes.clone());
			kept.dependencies = dependencies;
			plan.add_object(kept);
		}
		Ok(plan)
	}

	fn overlay(&self) -> OverlayStore<'_> {
		OverlayStore::new(self.base, &self.staged)
	}

	fn resolve_chain(&self, path: &str) -> Result<Vec<(ObjectRef, String)>> {
		let overlay = self.overlay();
		resolve_chain_in(&overlay, self.root.file_tree_root, path)
	}

	/// Absorb one staged operation's plan and thread its root forward.
	fn absorb(&mut self, txn: TxnPlan) {
		self.staged.absorb(&txn);
		self.root = txn.new_root;
	}
}

// endregion: --- Fold

// region:    --- Reachability

/// Mark `inode_ref` — and everything under it that is staged — as kept.
///
/// The walk stops at the committed/staged boundary: a committed object's
/// subtree is already on the volume and reachable, so none of it is the
/// plan's business. That boundary is also what bounds the walk by the staged
/// set rather than by the volume.
fn retain_reachable_inode<S: ObjectStore + NodeStore>(
	store: &S,
	inode_ref: ObjectRef,
	staged: &StagedObjects,
	keep: &mut BTreeSet<ObjectId>,
) -> Result<()> {
	let id = inode_ref.object_id;
	if !staged.contains(&id) || !keep.insert(id) {
		return Ok(());
	}

	let inode = store.inode_of(inode_ref)?;
	if inode.kind == InodeKind::Directory {
		if !inode.content_ref.is_zero() {
			retain_reachable_dir_node(store, inode.content_ref, staged, keep)?;
		}
	} else if !inode.content_ref.is_zero() {
		retain_reachable_extent_node(store, inode.content_ref, staged, keep)?;
	}
	Ok(())
}

/// Mark a staged directory's node and everything its entries name.
fn retain_reachable_dir_node<S: ObjectStore + NodeStore>(
	store: &S,
	node_ref: ObjectRef,
	staged: &StagedObjects,
	keep: &mut BTreeSet<ObjectId>,
) -> Result<()> {
	let id = node_ref.object_id;
	if !staged.contains(&id) || !keep.insert(id) {
		return Ok(());
	}

	let node = store.dir_node_of(node_ref)?;
	for entry in &node.entries {
		retain_reachable_inode(store, entry.inode_ref(), staged, keep)?;
	}
	for child in &node.children {
		if !child.is_zero() {
			retain_reachable_dir_node(store, *child, staged, keep)?;
		}
	}
	Ok(())
}

/// Mark a staged extent tree and every chunk it names.
fn retain_reachable_extent_node<S: ObjectStore + NodeStore>(
	store: &S,
	node_ref: ObjectRef,
	staged: &StagedObjects,
	keep: &mut BTreeSet<ObjectId>,
) -> Result<()> {
	let id = node_ref.object_id;
	if !staged.contains(&id) || !keep.insert(id) {
		return Ok(());
	}

	let node = store.get_extent_node(node_ref).map_err(Error::from)?;
	for extent in &node.extents {
		if staged.contains(&extent.chunk_id) {
			keep.insert(extent.chunk_id);
		}
	}
	for child in &node.children {
		if !child.is_zero() {
			retain_reachable_extent_node(store, *child, staged, keep)?;
		}
	}
	Ok(())
}

// endregion: --- Reachability
