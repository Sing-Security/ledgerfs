//! Folding several path-level mutations into **one** transaction.
//!
//! `cow::operations`' builders are single-operation: `write` produces one
//! transaction for one piece of data, and a caller that has more than one
//! piece has to decide what the pieces mean to each other. Sequentially
//! committed, they are several durability points, and worse, several
//! out-of-space verdicts, so a five-piece write that runs out of room on the
//! third piece leaves the first two behind as a file the caller never asked
//! for.
//!
//! This module is the other answer. A [`StagedFold`] runs the single-operation
//! builders one after another against an [`OverlayStore`] (the volume, plus
//! every object a previous step has staged) and threads the resulting root
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
//! A staged `write` or `create` re-places every object on its path (that is
//! what CoW is), so a fold of five writes holds five generations of the extent
//! node, the inode and the directory pair. (An append does not: it extends a
//! draft, and the seal builds that file's objects once.) Only the last
//! generation is reachable from the final root. `seal_plan` walks the final
//! root through the overlay and keeps exactly the staged objects it reaches,
//! dropping the intermediate generations: the commit carries N pieces' chunks
//! plus **one** copy of each metadata object, rather than one per piece.
//!
//! # Why an append does not build an object
//!
//! The eager route costs O(history) per append, twice over. The file's
//! `ExtentNode` is the one object whose canonical encoding grows with the
//! file, so every staged append re-encodes and re-hashes all of it, and
//! because each generation is refused by nothing and kept until the seal, a
//! session of N appends held O(N²) bytes in RAM. A capture loop that keeps one
//! stream open is exactly the workload staging exists for, and it was the one
//! that scaled worst.
//!
//! A [`FileDraft`] is that state kept once instead: the decoded inode and the
//! extent list, extended in place. Appending is a chunk hash (of the appended
//! bytes only) and a push. The objects are built when something needs their
//! identity (a seal, a `stat`, a `read_dir`), and not before, because an
//! object's id *is* the hash of its encoded bytes and an extent node's bytes
//! are the whole file's history.
//!
//! # The draft invariants
//!
//! - A draft is keyed by the **inode the tree still names** while the draft is
//!   open, and that is exactly how a read finds it: resolve the path, then ask
//!   whether that inode has a draft. A path string is only a cache in front of
//!   that lookup, so a clearer spelling that misses the cache still resolves
//!   correctly.
//! - Reads of contents (`read_file`, `read_at`) serve a draft without
//!   materializing it. Reads that expose an object identity or a size
//!   (`stat`, `read_dir`) materialize first, so nothing a caller can see names
//!   an object the session does not hold.
//! - Only appends keep a draft alive. A staged remove of the path drops it
//!   (the file is leaving the tree, so its pending bytes are dead), and the
//!   seal materializes whatever is left into the ordinary plan.
//! - Materializing re-resolves the path through the overlay rather than reusing
//!   a reference captured when the draft opened: any other staged operation
//!   since then may have re-placed the directories above it.
//! - Materializing builds the *same* objects the eager route would have built
//!   from the same extents (one leaf node, the inode with the draft's size and
//!   the node's id, the entry re-pointed and the ancestors cascaded), so a
//!   sealed session is still indistinguishable from the equivalent sequence of
//!   commits. That equality is what makes draft-vs-eager a performance choice
//!   and not a format one.
//!
//! # What a fold does not touch
//!
//! No device block is allocated and nothing reaches the disk until the
//! returned plan is committed; placement is `CommitEngine`'s, at commit
//! time. Path resolution still reads the committed volume, as any verb does;
//! what the fold adds is that every *staged* object resolves out of memory,
//! and that the prune walk stops at the committed boundary rather than
//! descending into objects the volume already has.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ledgerfs::cow::{NewObject, ObjectStore, TxnPlan, write};
use ledgerfs::object::{
	Chunk, DirNode, ExtentDescriptor, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef, Root, StorageHint,
};
use ledgerfs::{Error as LedgerFsError, Result as LedgerFsResult};

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
/// Two operations in one fold routinely produce the *same* object (the same
/// bytes appended twice, or an ancestor directory two writes both CoW-copy
/// without change), and content addressing makes those one entry here, as
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

	/// Whether `id` names a staged object, i.e. one the volume does not have
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

// region:    --- Drafts

/// What one staged extent costs a draft's memory: the descriptor itself, which
/// is also the order of what it will occupy in the node it is materialized
/// into.
const DRAFT_EXTENT_BYTES: u64 = core::mem::size_of::<ExtentDescriptor>() as u64;

/// A file the session has appended to since it was last materialized.
///
/// The inode is the one the staged tree currently names: the draft's content
/// is *ahead* of it, and `size` says by how much. `chunks` holds the canonical
/// bytes of the chunks this draft introduced, so a staged read of the drafted
/// tail answers from memory; extents that predate the draft resolve through
/// the overlay as any staged extent does.
pub(crate) struct FileDraft {
	/// The key: the inode the tree names while this draft is open. A read
	/// resolves the path to this inode and finds the draft; the tree itself is
	/// not touched until materialization re-points the entry.
	base_inode: ObjectId,

	/// Where the file was last seen. Materialization re-resolves it, so this
	/// only has to be a path that still names the file, and no staged verb
	/// renames one.
	path: String,

	/// The inode as the tree has it, with `size` overridden by the draft.
	inode: Inode,

	/// The file's extents, in logical order, extended by appends.
	extents: Vec<ExtentDescriptor>,

	/// Canonical bytes of the chunks this draft introduced, by id. A chunk some
	/// other staged object already holds is not copied.
	chunks: BTreeMap<ObjectId, Vec<u8>>,

	/// The file's size once the draft is materialized.
	size: u64,

	/// What this draft is holding, which is what the session's capacity is
	/// charged for: its own chunks plus one descriptor per extent.
	bytes: u64,
}

/// What one push changed, so a refused operation can put the draft back
/// exactly as it was without re-deriving anything.
pub(crate) struct PushUndo {
	key: ObjectId,
	extents: usize,
	size: u64,
	bytes: u64,
	added_chunks: Vec<ObjectId>,
}

impl FileDraft {
	fn new(base_inode: ObjectId, path: String, inode: Inode, extents: Vec<ExtentDescriptor>) -> Self {
		let bytes = extents.len() as u64 * DRAFT_EXTENT_BYTES;
		Self {
			base_inode,
			path,
			size: inode.size,
			inode,
			extents,
			chunks: BTreeMap::new(),
			bytes,
		}
	}

	/// Append `data` (split at `piece`, the volume's object ceiling, exactly
	/// as the eager builders split it) and return the journal a capacity
	/// refusal needs.
	///
	/// A chunk the staged set already holds is not copied into the draft: it
	/// resolves through the overlay like any other staged object, so appending
	/// the same record twice costs one copy of it, as it would on the volume.
	fn push(&mut self, staged: &StagedObjects, data: &[u8], piece: usize) -> Result<PushUndo> {
		// The whole push, including a partial one that failed on a later piece,
		// is undone here rather than by the caller: a `PushUndo` only exists for
		// a push that returned, so the caller has nothing to put back.
		let undo = PushUndo {
			key: self.base_inode,
			extents: self.extents.len(),
			size: self.size,
			bytes: self.bytes,
			added_chunks: Vec::new(),
		};
		let mut added_chunks = Vec::new();

		for piece in data.chunks(piece.max(1)) {
			let chunk = Chunk::new(piece.to_vec());
			let canonical = match chunk.encode_canonical() {
				Ok(canonical) => canonical,
				Err(e) => {
					self.undo(PushUndo { added_chunks, ..undo });
					return Err(e.into());
				}
			};
			let chunk_id = ObjectId::from_content(&canonical);

			if !self.chunks.contains_key(&chunk_id) && !staged.contains(&chunk_id) {
				self.bytes += canonical.len() as u64;
				self.chunks.insert(chunk_id, canonical);
				added_chunks.push(chunk_id);
			}

			self.extents.push(ExtentDescriptor {
				logical_offset: self.size,
				length: piece.len() as u64,
				chunk_id,
				chunk_hint: StorageHint::zero(),
				chunk_offset: 0,
			});
			self.size += piece.len() as u64;
			self.bytes += DRAFT_EXTENT_BYTES;
		}

		Ok(PushUndo { added_chunks, ..undo })
	}

	/// Undo a [`push`](Self::push) that a refused operation has to forget.
	fn undo(&mut self, undo: PushUndo) {
		self.extents.truncate(undo.extents);
		self.size = undo.size;
		self.bytes = undo.bytes;
		for id in undo.added_chunks {
			self.chunks.remove(&id);
		}
	}

	/// Read `len` bytes at `offset` from the draft's view of the file.
	///
	/// Shaped like the kernel's `fill_from_extent_node`: extents outside the
	/// window are skipped, and a chunk the draft introduced decodes from memory
	/// while every other one resolves through `store` (the overlay, so commits
	/// and other staged objects are both in reach).
	fn read_range<S: ObjectStore>(&self, store: &S, offset: u64, len: usize) -> Result<Vec<u8>> {
		let mut out = vec![0u8; len];
		let end = offset.saturating_add(len as u64);

		for extent in &self.extents {
			let extent_end = extent.logical_offset.saturating_add(extent.length);
			if extent_end <= offset || extent.logical_offset >= end {
				continue;
			}

			let overlap_start = offset.max(extent.logical_offset);
			let overlap_end = end.min(extent_end);
			let chunk_start = (overlap_start - extent.logical_offset + extent.chunk_offset) as usize;
			let chunk_end = (overlap_end - extent.logical_offset + extent.chunk_offset) as usize;
			let copy_len = (overlap_end - overlap_start) as usize;
			let at = (overlap_start - offset) as usize;

			let chunk = match self.chunks.get(&extent.chunk_id) {
				Some(canonical) => Chunk::decode_canonical(canonical)?,
				None => store.get_chunk(ObjectRef::from_id(extent.chunk_id))?,
			};

			if chunk_end > chunk.data.len() || at + copy_len > out.len() {
				return Err(Error::Format(LedgerFsError::custom(
					"draft extent metadata is inconsistent with its chunk",
				)));
			}
			out[at..at + copy_len].copy_from_slice(&chunk.data[chunk_start..chunk_end]);
		}

		Ok(out)
	}

	/// Turn the draft into the transaction the eager builders would have
	/// produced: the extent node, the inode, and the directory entry re-pointed
	/// at it, cascaded to the volume root.
	///
	/// `chain` is the target's mutation chain, resolved by the caller against
	/// the *current* staged root, not the one the draft opened on.
	fn materialize<S: ObjectStore>(&self, store: &S, root: &Root, chain: &[(ObjectRef, String)]) -> Result<TxnPlan> {
		// A leaf with exactly the extents an eager node would have after the
		// same appends. Built directly rather than through `new_leaf` so the
		// node is encoded once, by `seal_canonical`.
		let mut node = ExtentNode {
			object_id: ObjectId::zero(),
			is_leaf: true,
			extents: self.extents.clone(),
			children: Vec::new(),
		};
		let extent_bytes = node.seal_canonical()?;

		// The eager `cow::write` changes exactly these two inode fields, so a
		// draft that ends where the eager route would end is the same inode
		// object, and therefore the same id.
		let mut inode = self.inode.clone();
		inode.size = self.size;
		inode.content_ref = ObjectRef::from_id(node.object_id);
		let inode_bytes = inode.seal_canonical()?;

		let mut txn = build_replace_txn(store, root, chain, ObjectRef::from_id(inode.object_id))?;

		// The chunks this draft introduced, then the node over them, then the
		// inode: the same dependency order `cow::write` declares, so placement
		// can order the writes the way the eager commit would have.
		let dependencies: Vec<ObjectId> = self.chunks.keys().copied().collect();
		for (id, canonical) in &self.chunks {
			txn.add_object(NewObject::new(*id, canonical.clone()));
		}
		txn.add_object(NewObject::new(node.object_id, extent_bytes).with_dependencies(dependencies));
		txn.add_object(NewObject::new(inode.object_id, inode_bytes).with_dependency(node.object_id));

		Ok(txn)
	}
}

/// The session's drafts, and the path index in front of them.
///
/// The map is keyed by the inode the tree names, which is the identity a read
/// can resolve to; `by_path` is only a cache, so a differently spelled path
/// costs one resolution instead of a wrong answer.
#[derive(Default)]
pub(crate) struct Drafts {
	by_inode: BTreeMap<ObjectId, FileDraft>,
	by_path: BTreeMap<String, ObjectId>,

	/// Bumped whenever a draft is opened, dropped or extended. A staged
	/// operation that leaves the root where it was but moves this number
	/// changed something, and the session has work to seal.
	revision: u64,
}

impl Drafts {
	fn insert(&mut self, draft: FileDraft) {
		// A draft is keyed by the inode its path named, and no staged verb
		// renames a file: a path can only come to name a different inode through
		// a remove (which drops the draft with it) followed by a create. The
		// guard is therefore defensive, but without it the stale draft would sit
		// in `by_inode` until the seal and then be materialized *over* the file
		// that replaced it.
		if let Some(stale) = self.by_path.insert(draft.path.clone(), draft.base_inode)
			&& stale != draft.base_inode
		{
			self.by_inode.remove(&stale);
		}
		self.by_inode.insert(draft.base_inode, draft);
		self.revision += 1;
	}

	fn remove(&mut self, key: &ObjectId) -> Option<FileDraft> {
		let draft = self.by_inode.remove(key)?;
		if self.by_path.get(&draft.path) == Some(key) {
			self.by_path.remove(&draft.path);
		}
		self.revision += 1;
		Some(draft)
	}

	fn key_for_path(&self, path: &str) -> Option<ObjectId> {
		self.by_path.get(path).copied()
	}

	fn get(&self, key: &ObjectId) -> Option<&FileDraft> {
		self.by_inode.get(key)
	}

	fn get_mut(&mut self, key: &ObjectId) -> Option<&mut FileDraft> {
		self.by_inode.get_mut(key)
	}

	fn keys(&self) -> Vec<ObjectId> {
		self.by_inode.keys().copied().collect()
	}

	fn revision(&self) -> u64 {
		self.revision
	}

	fn touch(&mut self) {
		self.revision += 1;
	}

	/// The bytes the drafts are holding, which the session's capacity is
	/// charged for alongside its materialized objects.
	fn bytes(&self) -> u64 {
		self.by_inode.values().map(|draft| draft.bytes).sum()
	}
}

/// One staged operation's draft changes, so an error or a capacity refusal can
/// put the session back exactly as it was.
///
/// A staged operation touches at most one file (an append extends one draft,
/// a remove drops one), so the journal holds a push list and at most one
/// whole-draft removal.
#[derive(Default)]
struct DraftJournal {
	/// One entry per push, undone in reverse.
	pushes: Vec<PushUndo>,

	/// Drafts this operation opened, dropped again on abort.
	opened: Vec<ObjectId>,

	/// A draft this operation dropped whole, restored on abort.
	removed: Option<FileDraft>,

	/// The drafts' revision before the operation.
	revision: u64,
}

/// Collect a file's extents in logical order, through any store.
///
/// Mirrors the read path's traversal: a leaf's extents are the data, and an
/// internal node's data is in its children. No writer in this release produces
/// an internal node, but a volume from elsewhere may carry one and a draft
/// must not lose its content.
fn flatten_extents<S: ObjectStore>(store: &S, node_ref: ObjectRef, out: &mut Vec<ExtentDescriptor>) -> Result<()> {
	if node_ref.is_zero() {
		return Ok(());
	}
	let node = store.get_extent_node(node_ref)?;
	if node.is_leaf {
		out.extend(node.extents);
	} else {
		for child in &node.children {
			if !child.is_zero() {
				flatten_extents(store, *child, out)?;
			}
		}
	}
	Ok(())
}

// endregion: --- Drafts

// region:    --- Overlay

/// The committed volume **plus** everything a fold has staged: an
/// [`ObjectStore`] whose lookups answer from the staged set first and fall
/// through to the volume.
///
/// This is the piece that lets operation N+1 see operation N's objects: see
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

// region:    --- Session inner

/// Everything a session is, apart from the volume it sits on.
///
/// Both the fold (which drives one operation) and [`StagedState`] (which owns
/// the session between operations) work on this, so the draft machinery has
/// one home: a fold is this plus an operation journal.
pub(crate) struct StagedInner {
	objects: StagedObjects,
	root: Root,
	drafts: Drafts,
	capacity: u64,
	ops: u64,
}

impl StagedInner {
	fn fresh(root: &Root, capacity: u64) -> Self {
		Self {
			objects: StagedObjects::default(),
			root: root.clone(),
			drafts: Drafts::default(),
			capacity,
			ops: 0,
		}
	}

	fn overlay<'a>(&'a self, base: &'a DiskObjectStore) -> OverlayStore<'a> {
		OverlayStore::new(base, &self.objects)
	}

	/// The canonical bytes the session is holding: staged objects plus drafts.
	fn total_bytes(&self) -> u64 {
		self.objects.bytes() + self.drafts.bytes()
	}

	/// The draft serving `path`, if the path names a file the session has
	/// appended to.
	///
	/// The path cache answers when it can; otherwise the path is resolved
	/// through the overlay and the resulting inode is looked up. The second
	/// route is what makes a differently spelled path (`/a//b` for `/a/b`)
	/// correct rather than merely unlikely.
	fn draft_at<'s>(&'s self, base: &DiskObjectStore, path: &str) -> Result<Option<&'s FileDraft>> {
		if let Some(key) = self.drafts.key_for_path(path)
			&& let Some(draft) = self.drafts.get(&key)
		{
			return Ok(Some(draft));
		}
		let overlay = self.overlay(base);
		match resolve_in(&overlay, self.root.file_tree_root, path) {
			Ok(inode_ref) => Ok(self.drafts.get(&inode_ref.object_id)),
			Err(Error::NoSuchFile { .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Materialize the draft at `path`, if any. A path with no draft (or no
	/// file at all) is not an error here; the caller's own read reports it.
	fn materialize_at(&mut self, base: &DiskObjectStore, path: &str) -> Result<()> {
		let key = match self.drafts.key_for_path(path) {
			Some(key) => key,
			None => match resolve_in(&self.overlay(base), self.root.file_tree_root, path) {
				Ok(inode_ref) => inode_ref.object_id,
				Err(Error::NoSuchFile { .. }) => return Ok(()),
				Err(e) => return Err(e),
			},
		};
		if self.drafts.get(&key).is_none() {
			return Ok(());
		}
		self.materialize_key(base, key)
	}

	/// Materialize every draft the session holds.
	///
	/// A listing asks for the identity *and the size* of each entry, which is
	/// exactly what a draft does not have yet. Materializing here rather than
	/// per entry keeps one code path: the drafts a listing does not touch are
	/// materialized too, and stay materialized for the rest of the session.
	fn materialize_all(&mut self, base: &DiskObjectStore) -> Result<()> {
		for key in self.drafts.keys() {
			self.materialize_key(base, key)?;
		}
		Ok(())
	}

	/// Build and absorb the objects for one draft, re-pointing its path at the
	/// new inode. The draft is gone afterwards: its content is now the tree's.
	fn materialize_key(&mut self, base: &DiskObjectStore, key: ObjectId) -> Result<()> {
		let Some(path) = self.drafts.get(&key).map(|draft| draft.path.clone()) else {
			return Ok(());
		};

		// The chain has to be the one the *current* root names: a staged
		// operation since the draft opened may have re-placed the directories
		// above it.
		let txn = {
			let overlay = self.overlay(base);
			let chain = resolve_chain_in(&overlay, self.root.file_tree_root, &path)?;
			let draft = self.drafts.get(&key).expect("the draft was just looked up");
			draft.materialize(&overlay, &self.root, &chain)?
		};

		self.objects.absorb(&txn);
		self.root = txn.new_root;
		self.drafts.remove(&key);
		Ok(())
	}

	fn read_file(&self, base: &DiskObjectStore, path: &str) -> Result<Vec<u8>> {
		if let Some(draft) = self.draft_at(base, path)? {
			let len = usize::try_from(draft.size).map_err(|_| Error::RootUnreadable {
				detail: format!("{path}: file size {} does not fit in usize", draft.size),
			})?;
			return draft.read_range(&self.overlay(base), 0, len);
		}
		read_file_in(&self.overlay(base), self.root.file_tree_root, path)
	}

	fn read_at(&self, base: &DiskObjectStore, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
		if let Some(draft) = self.draft_at(base, path)? {
			let available = draft.size.saturating_sub(offset);
			let len = len.min(usize::try_from(available).map_err(|_| Error::RootUnreadable {
				detail: format!("{path}: {} unread bytes do not fit in usize", available),
			})?);
			return draft.read_range(&self.overlay(base), offset, len);
		}
		read_at_in(&self.overlay(base), self.root.file_tree_root, path, offset, len)
	}
}

// endregion: --- Session inner

// region:    --- Fold

/// A sequence of staged operations over one volume, folded into one plan.
///
/// Built against the volume's committed state; driven one operation at a
/// time; finished with [`seal_plan`](Self::seal_plan), whose result goes
/// through [`Store::commit`](crate::Store::commit) like any other
/// transaction.
pub(crate) struct StagedFold<'a> {
	base: &'a DiskObjectStore,

	inner: StagedInner,

	/// The root descriptor the fold started from: the merged plan's
	/// `prev_root`.
	base_root: Root,

	/// The undo journal for the operation in progress: ids it introduced, and
	/// the root before it. Both are empty/`None` between operations.
	op_added: Vec<ObjectId>,
	op_root: Option<Root>,
	op_drafts: DraftJournal,
}

impl<'a> StagedFold<'a> {
	/// A one-shot fold from the committed root, with no capacity limit.
	pub(crate) fn new(base: &'a DiskObjectStore, root: &Root) -> Self {
		Self::resume(base, root, StagedInner::fresh(root, 0))
	}

	/// A fold continuing a staging session: the committed root is where the
	/// session started, and `inner` is where the last operation left it.
	pub(crate) fn resume(base: &'a DiskObjectStore, committed_root: &Root, inner: StagedInner) -> Self {
		Self {
			base,
			inner,
			base_root: committed_root.clone(),
			op_added: Vec::new(),
			op_root: None,
			op_drafts: DraftJournal::default(),
		}
	}

	/// Stage `data` as the content of a **new** file at `path`.
	///
	/// The caller has already checked the name is free (`exists`); this runs
	/// the create-with-content builder against the overlay so any staged
	/// ancestor generation is the one it copies.
	///
	/// A payload longer than `chunk` is staged as a create plus further writes:
	/// the first object is the create-with-content, and every later one is a
	/// kernel `write` (no entry to re-point: the entry was created by the
	/// first piece and resolved through the overlay thereafter).
	pub(crate) fn create_with_data(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		self.staged_op(|fold| fold.create_with_data_inner(path, data, mode, chunk))
	}

	/// Stage `data` as the **replacement content** for the file at `path`,
	/// which must exist.
	///
	/// Two phases in one plan, which is what makes the replace atomic:
	///
	/// 1. the new file is built whole (inode, chunks, extent tree) with no
	///    directory entry naming it (`create_file`/`write` take a zero parent
	///    reference for exactly this), so nothing reachable from the volume
	///    root has changed yet;
	/// 2. the entry the path names is re-pointed at the new inode and the
	///    ancestors are cascaded.
	///
	/// Committed together, the old inode stops being reachable in the same
	/// instant the new one becomes reachable. The old content is not freed
	/// (nothing is, see `KNOWN_ISSUES.md` §1); it simply stops being named.
	pub(crate) fn replace_with_data(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		self.staged_op(|fold| fold.replace_with_data_inner(path, data, mode, chunk))
	}

	/// Stage one `write` of `data` at `offset` into the file at `path`.
	///
	/// The path is resolved through the overlay on **every** call, and it has
	/// to be: each staged write produces a new generation of the file's
	/// inode, and the reference from the previous call names an object that
	/// exists only in the staged set. Building on a stale reference would
	/// leave only the last write standing: the same trap committing each
	/// piece separately had, at fold speed.
	pub(crate) fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
		self.staged_op(|fold| fold.write_inner(path, offset, data))
	}

	/// Stage an `append`: extend an existing file, or create one with `data`
	/// when the name is free.
	///
	/// The same shape [`Store::append`](crate::Store::append) has, with two
	/// differences that matter inside a session: the file's size is the
	/// session's (the draft's), not the committed one, and the append is a
	/// push onto a [`FileDraft`] rather than a `write` transaction; see the
	/// module doc.
	pub(crate) fn append(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		self.staged_op(|fold| fold.append_inner(path, data, mode, chunk))
	}

	/// Stage a `remove` of the file at `path`. A directory is refused, exactly
	/// as [`Store::remove`](crate::Store::remove) refuses one.
	///
	/// A draft on the path is dropped without being materialized: its bytes
	/// are leaving the tree, so building objects for them would be work whose
	/// only product the prune would throw away.
	pub(crate) fn remove(&mut self, path: &str) -> Result<()> {
		self.staged_op(|fold| {
			let (file_ref, inode) = fold.lookup(path)?.ok_or_else(|| Error::NoSuchFile { path: path.to_string() })?;
			if inode.kind == InodeKind::Directory {
				return Err(Error::IsADirectory { path: path.to_string() });
			}
			let chain = fold.resolve_chain(path)?;
			let txn = build_unlink_txn(&fold.overlay(), &fold.inner.root, &chain)?;
			fold.absorb_txn(txn);
			fold.discard_draft(file_ref.object_id);
			Ok(())
		})
	}

	/// Whether anything (staged or committed) is named at `path`.
	///
	/// Resolves through the overlay and does not materialize: the answer is an
	/// existence bit, and the callers are the ones that refuse on it.
	pub(crate) fn exists(&self, path: &str) -> Result<bool> {
		match resolve_in(&self.overlay(), self.inner.root.file_tree_root, path) {
			Ok(_) => Ok(true),
			Err(Error::NoSuchFile { .. }) => Ok(false),
			Err(e) => Err(e),
		}
	}

	/// Fold the staged operations into the one [`TxnPlan`] a commit takes.
	///
	/// `committed_seq` is the volume's current commit sequence; the merged
	/// root carries `committed_seq + 1`, because the fold is one commit no
	/// matter how many operations built it; the per-operation increments the
	/// builders threaded are discarded here.
	pub(crate) fn seal_plan(&mut self, committed_seq: u64) -> Result<TxnPlan> {
		// Anything still in a draft becomes an object here: a commit has to
		// name what it writes, and a draft is by construction unnamed.
		self.inner.materialize_all(self.base)?;

		// The merged root is a *copy*: the fold's own root keeps the sequence
		// the last operation threaded, so a seal that fails can hand the
		// session back exactly as it was and be retried.
		let mut new_root = self.inner.root.clone();
		new_root.commit_seq = committed_seq + 1;
		new_root.update_object_id();

		// Prune to reachability from the final root. This is what makes the
		// commit linear in the staged bytes: without it, every generation of
		// every re-placed object would ride along, and a many-piece write
		// would carry quadratic metadata for nothing.
		let tree_root = new_root.file_tree_root;
		let overlay = self.overlay();
		let mut keep = BTreeSet::new();
		retain_reachable_inode(&overlay, tree_root, &self.inner.objects, &mut keep)?;

		let mut plan = TxnPlan::new(ObjectRef::from_id(self.base_root.object_id), new_root);
		for (id, object) in &self.inner.objects.objects {
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

	/// Hand the session back with everything it holds.
	pub(crate) fn into_state(self) -> StagedState {
		StagedState::new(self.inner)
	}

	fn overlay(&self) -> OverlayStore<'_> {
		self.inner.overlay(self.base)
	}

	fn resolve_chain(&self, path: &str) -> Result<Vec<(ObjectRef, String)>> {
		resolve_chain_in(&self.overlay(), self.inner.root.file_tree_root, path)
	}

	/// The object and inode `path` names in the staged view, or `None` when
	/// nothing is there.
	fn lookup(&self, path: &str) -> Result<Option<(ObjectRef, Inode)>> {
		let overlay = self.overlay();
		let inode_ref = match resolve_in(&overlay, self.inner.root.file_tree_root, path) {
			Ok(inode_ref) => inode_ref,
			Err(Error::NoSuchFile { .. }) => return Ok(None),
			Err(e) => return Err(e),
		};
		let inode = overlay.inode_of(inode_ref)?;
		Ok(Some((inode_ref, inode)))
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
		// the create uses it: a content `write` needs no chain at all, and
		// the entry is re-pointed in phase 2 with the real one.
		let unnamed = [(ObjectRef::zero(), name)];
		let file_ref = self.build_content(&unnamed, path, data, mode, chunk)?;

		// Phase 2: put the entry where the path says it is.
		let txn = build_replace_txn(&self.overlay(), &self.inner.root, &chain, file_ref)?;
		self.absorb_txn(txn);
		Ok(())
	}

	fn append_inner(&mut self, path: &str, data: &[u8], mode: u32, chunk: usize) -> Result<()> {
		match self.lookup(path)? {
			Some((file_ref, inode)) => match inode.kind {
				InodeKind::Directory => Err(Error::IsADirectory { path: path.to_string() }),
				InodeKind::RegularFile => {
					// An empty append to a file that exists changes nothing, and
					// and must not open a draft, or the session would report
					// work to seal that a seal would then have to materialize.
					if data.is_empty() {
						return Ok(());
					}
					self.append_to_draft(path, file_ref.object_id, inode, data, chunk)
				}
				// Anything else fails the way the eager route fails it: the
				// kernel's `write` refuses a non-regular file, rather than
				// growing a second, differently worded refusal here.
				_ => self.write_at_inner(path, inode.size, data, chunk),
			},
			None => self.create_with_data_inner(path, data, mode, chunk),
		}
	}

	/// Extend (or open) the draft for the file at `path` by `data`.
	///
	/// This is the append's whole cost: one chunk hash of the appended bytes,
	/// a `Vec` push per piece, and no encoding of the file's history.
	fn append_to_draft(&mut self, path: &str, key: ObjectId, inode: Inode, data: &[u8], chunk: usize) -> Result<()> {
		if self.inner.drafts.get(&key).is_none() {
			// The file's extents as the tree has them, flattened once. This is
			// the only O(history) step in opening a draft, and a session pays
			// it once per file rather than once per append.
			let mut extents = Vec::new();
			flatten_extents(&self.overlay(), inode.content_ref, &mut extents)?;
			self.inner.drafts.insert(FileDraft::new(key, path.to_string(), inode, extents));
			self.op_drafts.opened.push(key);
		}

		let undo = {
			let inner = &mut self.inner;
			let draft = inner.drafts.get_mut(&key).expect("the draft exists or was just opened");
			draft.push(&inner.objects, data, chunk)?
		};
		self.inner.drafts.touch();
		self.op_drafts.pushes.push(undo);
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
		let txn = build_write_txn(&self.overlay(), &self.inner.root, &chain, file_ref, offset, data)?;
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
			let txn = build_create_txn(&self.overlay(), &self.inner.root, chain, 0, 0, mode, false)?;
			self.absorb_txn(txn);
			return self.staged_file_ref(parent_ref, path);
		}

		let chunk = chunk.max(1);
		let head = data.len().min(chunk);
		let txn = build_create_with_data_txn(&self.overlay(), &self.inner.root, chain, &data[..head], 0, 0, mode)?;
		self.absorb_txn(txn);
		let mut file_ref = self.staged_file_ref(parent_ref, path)?;

		for (i, piece) in data[head..].chunks(chunk).enumerate() {
			let offset = (head + i * chunk) as u64;
			let txn = if parent_ref.is_zero() {
				// A replace's first phase has no directory entry naming the
				// file, so the kernel `write` is enough: it threads its own
				// root and the caller re-points the entry once, at the end.
				write(&self.overlay(), &self.inner.root, file_ref, offset, piece)?
			} else {
				// A create's entry names the inode the *first* piece made, and
				// every piece makes a new one. The entry has to follow, and so
				// does the cascade above it, which is why the chain is
				// re-resolved each time: the previous piece CoW-copied the
				// parent and its ancestors, so the chain resolved before the
				// loop names objects the fold has already replaced.
				let chain = self.resolve_chain(path)?;
				build_write_txn(&self.overlay(), &self.inner.root, &chain, file_ref, offset, piece)?
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
	/// by resolving the path again over the overlay after the create was
	/// absorbed. Re-resolving rather than reusing the chain's parent reference
	/// is deliberate: that reference names the *committed* directory, and the
	/// new entry lives in the CoW copy the cascade just made.
	///
	/// The zero-parent form (a replace's first phase) names nothing, and the
	/// kernel puts the bare inode in `file_tree_root` exactly so a caller can
	/// find it.
	fn staged_file_ref(&self, parent_ref: ObjectRef, path: &str) -> Result<ObjectRef> {
		if parent_ref.is_zero() {
			return Ok(self.inner.root.file_tree_root);
		}
		resolve_in(&self.overlay(), self.inner.root.file_tree_root, path)
	}

	/// Absorb one staged operation's plan and thread its root forward.
	fn absorb_txn(&mut self, txn: TxnPlan) {
		self.op_added.extend(self.inner.objects.absorb(&txn));
		self.inner.root = txn.new_root;
	}

	/// Drop the draft on `key` (the file is leaving the tree), keeping it in
	/// the journal so a refused operation can put it back.
	fn discard_draft(&mut self, key: ObjectId) {
		if let Some(draft) = self.inner.drafts.remove(&key) {
			self.op_drafts.removed = Some(draft);
		}
	}

	/// Run one public operation under the capacity journal: on any error, or on
	/// a capacity overrun, the session is left exactly as it was before the
	/// call.
	fn staged_op(&mut self, body: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
		let before_root = self.inner.root.object_id;
		let before_revision = self.inner.drafts.revision();
		self.op_root = Some(self.inner.root.clone());
		self.op_added.clear();
		self.op_drafts = DraftJournal {
			revision: before_revision,
			..Default::default()
		};

		match body(self) {
			Ok(()) if self.inner.capacity != 0 && self.inner.total_bytes() > self.inner.capacity => {
				let wanted = self.inner.total_bytes();
				let capacity = self.inner.capacity;
				self.abort_op();
				Err(Error::StagingFull { wanted, capacity })
			}
			Ok(()) => {
				// An operation that left both the root and the drafts where
				// they were changed nothing (an empty append to an existing
				// file, say) and must not count toward "there is something to
				// seal".
				let changed =
					self.inner.root.object_id != before_root || self.inner.drafts.revision() != before_revision;
				self.op_root = None;
				self.op_added.clear();
				self.op_drafts = DraftJournal::default();
				if changed {
					self.inner.ops += 1;
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
		for undo in self.op_drafts.pushes.drain(..).rev() {
			if let Some(draft) = self.inner.drafts.get_mut(&undo.key) {
				draft.undo(undo);
			}
		}
		for key in self.op_drafts.opened.drain(..) {
			self.inner.drafts.remove(&key);
		}
		if let Some(draft) = self.op_drafts.removed.take() {
			self.inner.drafts.insert(draft);
		}
		self.inner.drafts.revision = self.op_drafts.revision;
		self.op_drafts = DraftJournal::default();

		self.inner.objects.undo(&self.op_added);
		if let Some(root) = self.op_root.take() {
			self.inner.root = root;
		}
		self.op_added.clear();
	}
}

// endregion: --- Fold

// region:    --- Session state

/// A staging session as [`Store`](crate::Store) holds it: everything
/// [`StagedInner`] carries, behind a lock.
///
/// The lock is there for one reason: `stat` and `read_dir` are `&self` reads
/// that have to materialize a draft (an inode's identity cannot be answered
/// without the object), while `read_file` and `read_at` only need to look.
/// Locking keeps that asymmetry out of the public signatures, which stay
/// `&self` exactly as they are outside a session.
///
/// It is a `Mutex` rather than a `RefCell` for the reason [`MemBlockDevice`]
/// gives: `Store` stays `Sync`, so an embedder may keep reading a handle from
/// several threads. The lock is uncontended on the single-threaded path the
/// staging tier is for. (A poisoned lock is recovered from rather than
/// propagated: a panic elsewhere does not make the staged bytes wrong, and
/// the alternatives are a `Result` on six read paths or a session that dies
/// with the panic.)
///
/// [`MemBlockDevice`]: crate::MemBlockDevice
pub(crate) struct StagedState {
	inner: Mutex<StagedInner>,
}

impl StagedState {
	/// Open a session on top of the committed `root`.
	pub(crate) fn begin(committed_root: &Root, capacity: u64) -> Self {
		Self::new(StagedInner::fresh(committed_root, capacity))
	}

	fn new(inner: StagedInner) -> Self {
		Self {
			inner: Mutex::new(inner),
		}
	}

	/// Lock the session, recovering the state if a panic elsewhere poisoned
	/// the lock. See the type's own note on why that is the right answer.
	fn lock(&self) -> MutexGuard<'_, StagedInner> {
		self.inner.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// The canonical bytes staged so far: objects and drafts both.
	pub(crate) fn bytes(&self) -> u64 {
		self.lock().total_bytes()
	}

	/// Operations staged so far. Zero means the session has nothing to seal.
	pub(crate) fn ops(&self) -> u64 {
		self.lock().ops
	}

	/// Continue this session as a fold over the committed root.
	pub(crate) fn into_fold<'a>(self, base: &'a DiskObjectStore, committed_root: &Root) -> StagedFold<'a> {
		StagedFold::resume(
			base,
			committed_root,
			self.inner.into_inner().unwrap_or_else(PoisonError::into_inner),
		)
	}

	/// Read a whole file from the staged view: drafts answer from memory,
	/// staged objects resolve out of the overlay, everything else falls
	/// through to the volume.
	pub(crate) fn read_file(&self, base: &DiskObjectStore, path: &str) -> Result<Vec<u8>> {
		self.lock().read_file(base, path)
	}

	/// Read a byte range from the staged view.
	pub(crate) fn read_at(&self, base: &DiskObjectStore, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
		self.lock().read_at(base, path, offset, len)
	}

	/// Stat a path in the staged view, so it carries a staged size, and a
	/// content reference the session can actually produce.
	pub(crate) fn stat(&self, base: &DiskObjectStore, path: &str) -> Result<Inode> {
		let mut inner = self.lock();
		inner.materialize_at(base, path)?;
		stat_in(&inner.overlay(base), inner.root.file_tree_root, path)
	}

	/// List a directory in the staged view, so it shows staged children at
	/// their staged sizes.
	pub(crate) fn read_dir(&self, base: &DiskObjectStore, path: &str) -> Result<Vec<crate::path::Entry>> {
		let mut inner = self.lock();
		inner.materialize_all(base)?;
		read_dir_in(
			&inner.overlay(base),
			inner.root.file_tree_root,
			inner.root.commit_seq,
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
/// no other verb on the store can run, which is the same one-writer rule the
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

	/// The store, for reads, which, during a session, are the staged tier.
	pub fn store(&self) -> &crate::Store {
		self.store
	}

	/// The store, mutably, for anything the wrapper does not name.
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

/// Mark `inode_ref` (and everything under it that is staged) as kept.
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

	let node = store.get_extent_node(node_ref)?;
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
