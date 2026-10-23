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
//! as a commit would have shown them. [`seal_plan`](StagedFold::seal_plan)
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
//! from the final root. `seal_plan` walks the final root through the overlay
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
use ledgerfs::cow::{NewObject, ObjectStore, TxnPlan, write};
use ledgerfs::object::{Chunk, DirNode, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef, Root};

use crate::error::{Error, Result};
use crate::mutate::{
	build_create_txn, build_create_with_data_txn, build_replace_txn, build_unlink_txn, build_write_txn,
	written_inode_ref,
};
use crate::objstore::DiskObjectStore;
use crate::path::{
	NodeStore, lookup_child_in, read_at_in, read_dir_in, read_file_in, resolve_chain_in, resolve_in, stat_in,
};

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

	/// The canonical bytes these objects hold, which is what a staging
	/// session's capacity is measured in. Intermediate generations count:
	/// they occupy memory until the session seals, even though the prune drops
	/// them from the plan.
	bytes: u64,
}

impl StagedObjects {
	/// Absorb every new object a staged operation produced, returning the ids
	/// that were not already staged.
	///
	/// The returned list is the operation's undo journal: content-addressed
	/// objects are immutable, so removing exactly the ids this operation
	/// *introduced* restores the set it started from. An id that was already
	/// present belongs to an earlier operation and must not be removed.
	pub(crate) fn absorb(&mut self, plan: &TxnPlan) -> Vec<ObjectId> {
		let mut added = Vec::new();
		for (id, object) in &plan.new_objects {
			if self.objects.contains_key(id) {
				continue;
			}
			self.bytes += object.canonical_bytes.len() as u64;
			self.objects.insert(*id, object.clone());
			added.push(*id);
		}
		added
	}

	/// Drop the objects an operation introduced, restoring both the set and its
	/// byte count.
	pub(crate) fn undo(&mut self, added: &[ObjectId]) {
		for id in added {
			if let Some(object) = self.objects.remove(id) {
				self.bytes -= object.canonical_bytes.len() as u64;
			}
		}
	}

	/// Whether `id` names a staged object — i.e. one the volume does not have
	/// yet.
	pub(crate) fn contains(&self, id: &ObjectId) -> bool {
		self.objects.contains_key(id)
	}

	/// The canonical bytes staged so far.
	pub(crate) fn bytes(&self) -> u64 {
		self.bytes
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
/// time; finished with [`seal_plan`](Self::seal_plan), whose result goes
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

	/// The session's capacity in canonical bytes, or `0` for unlimited.
	capacity: u64,

	/// How many operations this session has staged. A session that staged
	/// nothing has nothing to seal; see [`StagedState::ops`].
	ops: u64,

	/// The undo journal for the operation in progress: ids it introduced, and
	/// the root before it. Both are empty/`None` between operations.
	op_added: Vec<ObjectId>,
	op_root: Option<Root>,
}

impl<'a> StagedFold<'a> {
	/// A one-shot fold from the committed root, with no capacity limit.
	pub(crate) fn new(base: &'a DiskObjectStore, root: &Root) -> Self {
		Self::resume(base, root, root.clone(), StagedObjects::default(), 0, 0)
	}

	/// A fold continuing a staging session: the committed root is where the
	/// session started, `staged_root` is where the last operation left it, and
	/// `staged` holds every object the volume does not have yet.
	pub(crate) fn resume(
		base: &'a DiskObjectStore,
		committed_root: &Root,
		staged_root: Root,
		staged: StagedObjects,
		capacity: u64,
		ops: u64,
	) -> Self {
		Self {
			base,
			staged,
			root: staged_root,
			base_root: committed_root.clone(),
			capacity,
			ops,
			op_added: Vec::new(),
			op_root: None,
		}
	}

	/// Stage `data` as the content of a **new** file at `path`.
	///
	/// The caller has already checked the name is free (`ensure_absent`);
	/// this runs the create-with-content builder against the overlay so any
	/// staged ancestor generation is the one it copies.
	///
	/// A payload longer than `chunk` is staged as a create plus further writes:
	/// the first object is the create-with-content, and every later one is a
	/// kernel `write` (no entry to re-point — the entry was created by the
	/// first piece and resolved through the overlay thereafter).
	pub(crate) fn create_with_data(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		self.staged_op(|fold| fold.create_with_data_inner(path, data, mode, chunk))
	}

	/// Stage `data` as the **replacement content** for the file at `path`,
	/// which must exist.
	///
	/// Two phases in one plan, which is what makes the replace atomic:
	///
	/// 1. the new file is built whole — inode, chunks, extent tree — with no
	///    directory entry naming it (`create_file`/`write` take a zero parent
	///    reference for exactly this), so nothing reachable from the volume
	///    root has changed yet;
	/// 2. the entry the path names is re-pointed at the new inode and the
	///    ancestors are cascaded.
	///
	/// Committed together, the old inode stops being reachable in the same
	/// instant the new one becomes reachable. The old content is not freed —
	/// nothing is, see `KNOWN_ISSUES.md` §1 — it simply stops being named.
	pub(crate) fn replace_with_data(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		self.staged_op(|fold| fold.replace_with_data_inner(path, data, mode, chunk))
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
		self.staged_op(|fold| fold.write_inner(path, offset, data))
	}

	/// Stage an `append`: extend an existing file, or create one with `data`
	/// when the name is free.
	///
	/// The same shape [`Store::append`](crate::Store::append) has, with one
	/// difference that matters inside a session: the file's size is read
	/// through the overlay, so an append after an append continues from the
	/// staged size rather than from the committed one.
	pub(crate) fn append(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		self.staged_op(|fold| match fold.stat(path) {
			Ok(inode) => {
				if inode.kind == InodeKind::Directory {
					return Err(Error::IsADirectory { path: path.to_string() });
				}
				fold.write_at_inner(path, inode.size, data, chunk)
			}
			Err(Error::NoSuchFile { .. }) => fold.create_with_data_inner(path, data, mode, chunk),
			Err(e) => Err(e),
		})
	}

	/// Stage a `remove` of the file at `path`. A directory is refused, exactly
	/// as [`Store::remove`](crate::Store::remove) refuses one.
	pub(crate) fn remove(&mut self, path: &str) -> Result<()> {
		self.staged_op(|fold| {
			let inode = fold.stat(path)?;
			if inode.kind == InodeKind::Directory {
				return Err(Error::IsADirectory { path: path.to_string() });
			}
			let chain = fold.resolve_chain(path)?;
			let txn = build_unlink_txn(&fold.overlay(), &fold.root, &chain)?;
			fold.absorb_txn(txn);
			Ok(())
		})
	}

	/// The [`Inode`] at `path` as the staged view sees it.
	pub(crate) fn stat(&mut self, path: &str) -> Result<Inode> {
		stat_in(&self.overlay(), self.root.file_tree_root, path)
	}

	/// Fold the staged operations into the one [`TxnPlan`] a commit takes.
	///
	/// `committed_seq` is the volume's current commit sequence; the merged
	/// root carries `committed_seq + 1`, because the fold is one commit no
	/// matter how many operations built it — the per-operation increments the
	/// builders threaded are discarded here.
	pub(crate) fn seal_plan(&self, committed_seq: u64) -> Result<TxnPlan> {
		// The merged root is a *copy*: the fold's own root keeps the sequence
		// the last operation threaded, so a seal that fails can hand the
		// session back exactly as it was and be retried.
		let mut new_root = self.root.clone();
		new_root.commit_seq = committed_seq + 1;
		new_root.update_object_id();

		// Prune to reachability from the final root. This is what makes the
		// commit linear in the staged bytes: without it, every generation of
		// every re-placed object would ride along, and a many-piece write
		// would carry quadratic metadata for nothing.
		let tree_root = new_root.file_tree_root;
		let overlay = self.overlay();
		let mut keep = BTreeSet::new();
		retain_reachable_inode(&overlay, tree_root, &self.staged, &mut keep)?;

		let mut plan = TxnPlan::new(ObjectRef::from_id(self.base_root.object_id), new_root);
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

	/// Hand the session back with the staged objects it holds.
	pub(crate) fn into_state(self) -> StagedState {
		StagedState {
			objects: self.staged,
			root: self.root,
			capacity: self.capacity,
			ops: self.ops,
		}
	}

	fn overlay(&self) -> OverlayStore<'_> {
		OverlayStore::new(self.base, &self.staged)
	}

	fn resolve_chain(&self, path: &str) -> Result<Vec<(ObjectRef, String)>> {
		let overlay = self.overlay();
		resolve_chain_in(&overlay, self.root.file_tree_root, path)
	}

	// -- Operation bodies, each without the journal wrapper, so one can call
	// another without nesting a second begin/end pair.

	fn create_with_data_inner(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		let chain = self.resolve_chain(path)?;
		self.build_content(&chain, path, data, mode, chunk).map(|_| ())
	}

	fn replace_with_data_inner(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		let chain = self.resolve_chain(path)?;
		let name = chain
			.last()
			.map(|(_, name)| name.clone())
			.ok_or_else(|| Error::Usage("a mutation needs at least one path component".into()))?;

		// The single-entry chain whose parent is the zero reference: the kernel
		// builders take that to mean "build the file, name it nowhere". Only
		// the create uses it — a content `write` needs no chain at all, and
		// the entry is re-pointed in phase 2 with the real one.
		let unnamed = [(ObjectRef::zero(), name)];
		let file_ref = self.build_content(&unnamed, path, data, mode, chunk)?;

		// Phase 2: put the entry where the path says it is.
		let txn = build_replace_txn(&self.overlay(), &self.root, &chain, file_ref)?;
		self.absorb_txn(txn);
		Ok(())
	}

	fn write_inner(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
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
		self.absorb_txn(txn);
		Ok(())
	}

	fn write_at_inner(&mut self, path: &str, offset: u64, data: &[u8], chunk: usize) -> Result<()> {
		if data.is_empty() {
			return Ok(());
		}
		let chunk = chunk.max(1);
		for (i, piece) in data.chunks(chunk).enumerate() {
			self.write_inner(path, offset + (i * chunk) as u64, piece)?;
		}
		Ok(())
	}

	/// Build the content for a file under `chain` and absorb it, returning the
	/// file inode's final reference.
	///
	/// `chain` may be the real path (a create) or the zero-parent "name it
	/// nowhere" chain (a replace's first phase). Empty `data` is a bare file
	/// inode: there is no chunk to create it with.
	fn build_content(
		&mut self,
		chain: &[(ObjectRef, String)],
		path: &str,
		data: &[u8],
		mode: u32,
		chunk: usize,
	) -> Result<ObjectRef> {
		let (parent_ref, _) = chain
			.last()
			.cloned()
			.ok_or_else(|| Error::Usage("a mutation needs at least one path component".into()))?;

		if data.is_empty() {
			let txn = build_create_txn(&self.overlay(), &self.root, chain, 0, 0, mode, false)?;
			self.absorb_txn(txn);
			return self.staged_file_ref(parent_ref, path);
		}

		let chunk = chunk.max(1);
		let head = data.len().min(chunk);
		let txn = build_create_with_data_txn(&self.overlay(), &self.root, chain, &data[..head], 0, 0, mode)?;
		self.absorb_txn(txn);
		let mut file_ref = self.staged_file_ref(parent_ref, path)?;

		for (i, piece) in data[head..].chunks(chunk).enumerate() {
			let offset = (head + i * chunk) as u64;
			let txn = if parent_ref.is_zero() {
				// A replace's first phase has no directory entry naming the
				// file, so the kernel `write` is enough: it threads its own
				// root and the caller re-points the entry once, at the end.
				write(&self.overlay(), &self.root, file_ref, offset, piece)?
			} else {
				// A create's entry names the inode the *first* piece made, and
				// every piece makes a new one. The entry has to follow, and so
				// does the cascade above it — which is why the chain is
				// re-resolved each time: the previous piece CoW-copied the
				// parent and its ancestors, so the chain resolved before the
				// loop names objects the fold has already replaced.
				let chain = self.resolve_chain(path)?;
				build_write_txn(&self.overlay(), &self.root, &chain, file_ref, offset, piece)?
			};
			file_ref = written_inode_ref(&txn)?;
			self.absorb_txn(txn);
			// ...but a plan that re-pointed the entry also carries the
			// directory pair and the cascade above it, and the last object in
			// dependency order is then the top of that cascade, not the file
			// inode. Resolve the path again instead.
			if !parent_ref.is_zero() {
				file_ref = self.staged_file_ref(parent_ref, path)?;
			}
		}
		Ok(file_ref)
	}

	/// The file inode the fold's current root names at `path`.
	///
	/// A create named in a real directory leaves `new_root.file_tree_root`
	/// naming the volume root (the cascade put it there), so the file is found
	/// by resolving the path again over the overlay — after the create was
	/// absorbed. Re-resolving rather than reusing the chain's parent reference
	/// is deliberate: that reference names the *committed* directory, and the
	/// new entry lives in the CoW copy the cascade just made.
	///
	/// The zero-parent form — a replace's first phase — names nothing, and the
	/// kernel puts the bare inode in `file_tree_root` exactly so a caller can
	/// find it.
	fn staged_file_ref(&self, parent_ref: ObjectRef, path: &str) -> Result<ObjectRef> {
		if parent_ref.is_zero() {
			return Ok(self.root.file_tree_root);
		}
		let overlay = self.overlay();
		resolve_in(&overlay, self.root.file_tree_root, path)
	}

	/// Absorb one staged operation's plan and thread its root forward.
	fn absorb_txn(&mut self, txn: TxnPlan) {
		self.op_added.extend(self.staged.absorb(&txn));
		self.root = txn.new_root;
	}

	/// Run one public operation under the capacity journal: on any error, or on
	/// a capacity overrun, the session is left exactly as it was before the
	/// call.
	fn staged_op(&mut self, body: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
		let before = self.root.object_id;
		self.op_root = Some(self.root.clone());
		self.op_added.clear();

		match body(self) {
			Ok(()) if self.capacity != 0 && self.staged.bytes() > self.capacity => {
				let wanted = self.staged.bytes();
				self.abort_op();
				Err(Error::StagingFull {
					wanted,
					capacity: self.capacity,
				})
			}
			Ok(()) => {
				self.op_root = None;
				self.op_added.clear();
				// An operation that left the root where it was changed
				// nothing — an empty append to an existing file, say — and
				// must not count toward "there is something to seal".
				if self.root.object_id != before {
					self.ops += 1;
				}
				Ok(())
			}
			Err(e) => {
				self.abort_op();
				Err(e)
			}
		}
	}

	/// Undo everything the operation in progress staged.
	fn abort_op(&mut self) {
		self.staged.undo(&self.op_added);
		if let Some(root) = self.op_root.take() {
			self.root = root;
		}
		self.op_added.clear();
	}
}

// endregion: --- Fold

// region:    --- Session state

/// A staging session as [`Store`](crate::Store) holds it: the staged objects,
/// the staged root, and what the session is allowed to cost.
///
/// The state owns no borrow of the volume, which is why it can live in `Store`
/// while the fold that operates on it is built per call — see
/// [`StagedFold::resume`].
pub(crate) struct StagedState {
	objects: StagedObjects,
	root: Root,
	capacity: u64,
	ops: u64,
}

impl StagedState {
	/// Open a session on top of the committed `root`.
	pub(crate) fn begin(committed_root: &Root, capacity: u64) -> Self {
		Self {
			objects: StagedObjects::default(),
			root: committed_root.clone(),
			capacity,
			ops: 0,
		}
	}

	/// The canonical bytes staged so far, intermediates included.
	pub(crate) fn bytes(&self) -> u64 {
		self.objects.bytes()
	}

	/// Operations staged so far. Zero means the session has nothing to seal.
	pub(crate) fn ops(&self) -> u64 {
		self.ops
	}

	/// Continue this session as a fold over the committed root.
	pub(crate) fn into_fold<'a>(self, base: &'a DiskObjectStore, committed_root: &Root) -> StagedFold<'a> {
		StagedFold::resume(base, committed_root, self.root, self.objects, self.capacity, self.ops)
	}

	fn overlay<'a>(&'a self, base: &'a DiskObjectStore) -> OverlayStore<'a> {
		OverlayStore::new(base, &self.objects)
	}

	/// Read a whole file from the staged view: staged objects answer from
	/// memory, everything else falls through to the volume.
	pub(crate) fn read_file(&self, base: &DiskObjectStore, path: &str) -> Result<Vec<u8>> {
		read_file_in(&self.overlay(base), self.root.file_tree_root, path)
	}

	/// Read a byte range from the staged view.
	pub(crate) fn read_at(&self, base: &DiskObjectStore, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
		read_at_in(&self.overlay(base), self.root.file_tree_root, path, offset, len)
	}

	/// Stat a path in the staged view, so it carries a staged size.
	pub(crate) fn stat(&self, base: &DiskObjectStore, path: &str) -> Result<Inode> {
		stat_in(&self.overlay(base), self.root.file_tree_root, path)
	}

	/// List a directory in the staged view, so it shows staged children.
	pub(crate) fn read_dir(&self, base: &DiskObjectStore, path: &str) -> Result<Vec<crate::path::Entry>> {
		read_dir_in(
			&self.overlay(base),
			self.root.file_tree_root,
			self.root.commit_seq,
			path,
		)
	}
}

// endregion: --- Session state

// region:    --- Scoped session

/// A staging session tied to a scope: open with
/// [`Store::staging`](crate::Store::staging), seal or discard explicitly, and a
/// drop that has not done either discards.
///
/// This is sugar over [`begin_staging`](crate::Store::begin_staging) and its
/// verbs, for the caller whose session is one block of code. It holds `&mut
/// Store`, so reads during the session go through [`store`](Self::store) while
/// no other verb on the store can run — which is the same one-writer rule the
/// session already enforces.
pub struct Staging<'a> {
	store: &'a mut crate::Store,
	open: bool,
}

impl crate::Store {
	/// Open a scoped staging session. See [`begin_staging`](Self::begin_staging).
	pub fn staging(&mut self, capacity_bytes: u64) -> Result<Staging<'_>> {
		self.begin_staging(capacity_bytes)?;
		Ok(Staging {
			store: self,
			open: true,
		})
	}
}

impl<'a> Staging<'a> {
	/// Stage an append. See [`Store::staged_append`](crate::Store::staged_append).
	pub fn append(&mut self, path: &str, data: &[u8], mode: u32) -> Result<()> {
		self.store.staged_append(path, data, mode)
	}

	/// Stage a new file. See
	/// [`Store::staged_write_new_file`](crate::Store::staged_write_new_file).
	pub fn write_new_file(&mut self, path: &str, data: &[u8], mode: u32) -> Result<()> {
		self.store.staged_write_new_file(path, data, mode)
	}

	/// Stage a removal. See [`Store::staged_remove`](crate::Store::staged_remove).
	pub fn remove(&mut self, path: &str) -> Result<()> {
		self.store.staged_remove(path)
	}

	/// The store, for reads — which, during a session, are the staged tier.
	pub fn store(&self) -> &crate::Store {
		self.store
	}

	/// The store, mutably — for anything the wrapper does not name.
	pub fn store_mut(&mut self) -> &mut crate::Store {
		self.store
	}

	/// Seal the session as one commit. See [`Store::seal_staged`](crate::Store::seal_staged).
	pub fn seal(mut self) -> Result<ledgerfs::commit::CommitResult> {
		self.open = false;
		self.store.seal_staged()
	}

	/// Discard the session without committing.
	pub fn discard(mut self) {
		self.open = false;
		self.store.end_staging();
	}
}

impl Drop for Staging<'_> {
	fn drop(&mut self) {
		if self.open {
			self.store.end_staging();
		}
	}
}

// endregion: --- Scoped session

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
