//! The filesystem facade: mount an image, then walk it by path.
//!
//! # What the superblock actually points at
//!
//! `superblock.active_root_id` names the root directory's **`Inode`**, not a
//! [`Root`] object. A `Root` is never written to disk at all, and every reader
//! agrees with that: the kernel crate's `reader::walk_path` and a boot
//! reader's `walk_path` each take `root_inode_ref: &ObjectRef` and start from
//! it.
//!
//! [`Root`] is the in-memory descriptor the CoW transaction model carries
//! (commit_seq, alloc/dedupe/deadlist roots); it is not a stored object. The
//! query half of `cow::operations` reflects that — `read_dir` goes through
//! `readdir`, which takes a `&Root` but reads no field of it.
//! [`HostFs::query_root`] builds the minimal one that signature needs.
//!
//! File reads go through `cow::operations::read_inode` instead of `read`: this
//! module resolves the file's `Inode` on the way to the path, and `read` would
//! fetch the very same object again to learn the size and content reference the
//! walk is already holding.

use std::collections::HashSet;
use std::path::Path;

use ledgerfs::cow::{ObjectStore, read_inode, readdir};
use ledgerfs::object::{DirNode, Inode, InodeKind, ObjectRef, Root};

use crate::device::{BlockDevice, FileBlockDevice};
use crate::error::{Error, Result};
use crate::image::{Partition, PartitionKind, find_partition};
use crate::objstore::DiskObjectStore;
use crate::volume::Volume;

/// One directory entry, with the metadata a listing wants to show.
#[derive(Debug, Clone)]
pub struct Entry {
	/// The entry's name within its parent directory.
	pub name: String,
	/// What the entry is: directory, regular file, or symlink.
	pub kind: InodeKind,
	/// Content size in bytes, from the entry's `Inode`.
	pub size: u64,
	/// Permission bits recorded on the entry's `Inode`.
	pub mode: u32,
	/// Reference to the entry's `Inode`, which carries its remaining metadata.
	pub inode_ref: ObjectRef,
}

/// A mounted LedgerFS volume addressed by `/`-separated path.
pub struct HostFs {
	/// The mounted volume: its superblock, decoded copies, and mount decision.
	pub volume: Volume,
	/// The disk-backed object store every path lookup resolves through.
	pub store: DiskObjectStore,

	/// The active root directory's `Inode` — where the superblock points.
	pub root_inode_ref: ObjectRef,
}

// region:    --- Open

impl HostFs {
	/// Open the partition of the given kind inside `image`.
	pub fn open(image: &Path, kind: PartitionKind) -> Result<Self> {
		let partition = find_partition(image, kind)?;
		Self::open_partition(image, &partition)
	}

	/// Open an already-parsed partition, which must have a readable root.
	pub fn open_partition(image: &Path, partition: &Partition) -> Result<Self> {
		let device = FileBlockDevice::open(image, partition.start_bytes(), partition.size_bytes())?;
		let fs = Self::mount_device(Box::new(device))?;
		fs.require_root()?;
		Ok(fs)
	}

	/// Mount a volume from a device that is already open.
	///
	/// Unlike [`open_partition`](Self::open_partition) this does **not** call
	/// [`require_root`](Self::require_root): a caller that only wants to
	/// inspect a volume should not have to care whether it has a root.
	/// [`Store::open`](crate::Store::open) is that caller, and it is the one
	/// place that decides what a rootless volume means.
	///
	/// A volume that has never been committed to does not reach here at all —
	/// `Volume::mount` refuses it with [`Error::VolumeUncommitted`], since
	/// `mount_select` needs at least one copy with a committed sequence. So
	/// [`active_root`](Self::active_root) answering `None` covers only the
	/// committed-but-rootless case, which is what a torn write to a superblock
	/// copy leaves behind.
	pub fn mount_device(device: Box<dyn BlockDevice>) -> Result<Self> {
		let volume = Volume::mount(device.as_ref())?;

		let store = DiskObjectStore::new(device, volume.total_blocks(), volume.superblock.max_scan_blocks());

		let root_inode_ref = if volume.superblock.active_root_id == [0u8; 32] {
			ObjectRef::zero()
		} else {
			volume.root_ref()
		};

		Ok(Self {
			volume,
			store,
			root_inode_ref,
		})
	}

	/// The active root directory's `Inode`, or `None` on a volume that has
	/// never been committed to.
	///
	/// Named apart from the [`root_inode_ref`](Self::root_inode_ref) field it
	/// reads so the two cannot be confused at a call site.
	pub fn active_root(&self) -> Option<ObjectRef> {
		if self.root_inode_ref.is_zero() {
			None
		} else {
			Some(self.root_inode_ref)
		}
	}

	/// Fail here, naming the problem, rather than on the first path lookup.
	///
	/// Also the check that the object the superblock names as the root really
	/// is a directory: every reader decodes the active root as an `Inode`, so a
	/// superblock pointed at anything else is a volume whose paths can never
	/// resolve.
	pub fn require_root(&self) -> Result<()> {
		let root_inode_ref = self.active_root().ok_or_else(|| Error::RootUnreadable {
			detail: "the volume has no root — nothing has ever been committed to it".into(),
		})?;

		let inode = self
			.store
			.inode(root_inode_ref)
			.map_err(|e| Error::RootUnreadable { detail: e.to_string() })?;
		if inode.kind != InodeKind::Directory {
			return Err(Error::RootUnreadable {
				detail: format!(
					"the object the superblock names as the root is a {:?}, not a directory",
					inode.kind
				),
			});
		}

		Ok(())
	}
}

// endregion: --- Open

// region:    --- Path resolution

impl HostFs {
	/// Resolve `/`-separated `path` to an object reference.
	///
	/// Mirrors `reader::walk_path`: each component is looked up in the current
	/// directory's `DirNode`, and the entry's own `inode_hint` is carried
	/// forward so the next level is one read rather than a scan. An empty path
	/// or `/` is the root itself.
	pub fn resolve(&self, path: &str) -> Result<ObjectRef> {
		resolve_in(&self.store, self.root_inode_ref, path)
	}

	/// The mutation chain for `path`: one `(directory, name)` pair per
	/// component, ending with the target paired with the directory it lives in.
	///
	/// The target itself is **not** looked up, and that is the only difference
	/// from [`resolve`](Self::resolve). It is what makes this usable for a
	/// create: the entry does not exist yet, so a resolve that insisted on
	/// finding it would make every create a resolve-then-create with a gap in
	/// between.
	///
	/// The shape is the one the ancestor cascade consumes — walking the chain in
	/// reverse and re-pointing each parent's entry at its CoW-copied child is
	/// how a nested mutation reaches the volume root. See the `mutate` module for
	/// why that has to happen at all.
	pub fn resolve_chain(&self, path: &str) -> Result<Vec<(ObjectRef, String)>> {
		resolve_chain_in(&self.store, self.root_inode_ref, path)
	}

	/// The [`Inode`] at `path`.
	pub fn stat(&self, path: &str) -> Result<Inode> {
		stat_in(&self.store, self.root_inode_ref, path)
	}

	/// `name` inside the directory at `dir_ref`, or `None` if the directory has
	/// no such entry.
	///
	/// Distinguishes "absent" from "the parent is not a directory", which a
	/// create needs: the first is the normal case, the second is an error.
	pub(crate) fn lookup_child(&self, path: &str, dir_ref: ObjectRef, name: &str) -> Result<Option<ObjectRef>> {
		lookup_child_in(&self.store, path, dir_ref, name)
	}
}

// endregion: --- Path resolution (HostFs)

// region:    --- Node reads

/// The object reads a path walk needs, in this crate's error type.
///
/// Mirrors kernel `ObjectStore`'s four lookups for the two kinds a walk
/// decodes, and exists because the two differ at exactly the place a caller
/// notices: `ObjectStore` answers in the kernel's error, which loses the
/// `ObjectNotFound { object_id }` this crate reports for a reference that
/// names nothing. Both implementations — the volume, and the volume plus a
/// fold's staged objects — can say which id was missing.
pub(crate) trait NodeStore {
	fn inode_of(&self, obj_ref: ObjectRef) -> Result<Inode>;
	fn dir_node_of(&self, obj_ref: ObjectRef) -> Result<DirNode>;
}

impl NodeStore for DiskObjectStore {
	fn inode_of(&self, obj_ref: ObjectRef) -> Result<Inode> {
		self.inode(obj_ref)
	}

	fn dir_node_of(&self, obj_ref: ObjectRef) -> Result<DirNode> {
		self.dir_node(obj_ref)
	}
}

// endregion: --- Node reads

// region:    --- Path resolution (free functions)

/// Resolve `/`-separated `path` to an object reference, starting the walk at
/// `root_inode_ref` and reading every object through `store`.
///
/// The store-generic form of [`HostFs::resolve`] — the same walk over any
/// [`ObjectStore`], which is what lets the multi-write fold resolve paths
/// through objects a previous step only *staged*, not yet committed. An empty
/// path or `/` is the root itself.
pub(crate) fn resolve_in<S: NodeStore>(store: &S, root_inode_ref: ObjectRef, path: &str) -> Result<ObjectRef> {
	let components = components_of(path)?;
	if components.is_empty() {
		return Ok(root_inode_ref);
	}

	let chain = resolve_chain_in(store, root_inode_ref, path)?;
	let (parent_ref, name) = chain.last().expect("a non-empty path yields a non-empty chain");
	lookup_child_in(store, path, *parent_ref, name)?.ok_or_else(|| Error::NoSuchFile { path: path.to_string() })
}

/// The store-generic form of [`HostFs::resolve_chain`]: the mutation chain for
/// `path` read through `store`, starting at `root_inode_ref`.
pub(crate) fn resolve_chain_in<S: NodeStore>(
	store: &S,
	root_inode_ref: ObjectRef,
	path: &str,
) -> Result<Vec<(ObjectRef, String)>> {
	let components = components_of(path)?;
	if components.is_empty() {
		return Err(Error::Usage(format!(
			"{path} names the volume root, which is not something that can be created or written"
		)));
	}

	let mut chain = Vec::with_capacity(components.len());
	let mut current = root_inode_ref;
	let mut prefix = String::new();

	for (i, component) in components.iter().enumerate() {
		chain.push((current, component.to_string()));

		// The target's own parent is `current`, and there is nothing below
		// the target to resolve.
		if i + 1 == components.len() {
			break;
		}
		if !prefix.is_empty() {
			prefix.push('/');
		}
		prefix.push_str(component);
		// Name the component that is actually missing. A create of
		// `relay/transcript.jsonl` with no `relay/` is a missing
		// *directory*, and a `NoSuchFile` naming the tail sends the caller
		// hunting for the wrong thing — the failure mode that once left a
		// whole fleet of writers recording nothing.
		current = lookup_child_in(store, path, current, component)?
			.ok_or_else(|| Error::NoSuchFile { path: prefix.clone() })?;
	}

	Ok(chain)
}

/// The store-generic `name` lookup inside the directory at `dir_ref`: `None`
/// when the directory has no such entry, an error when `dir_ref` is not a
/// directory at all.
pub(crate) fn lookup_child_in<S: NodeStore>(
	store: &S,
	path: &str,
	dir_ref: ObjectRef,
	name: &str,
) -> Result<Option<ObjectRef>> {
	let inode = store.inode_of(dir_ref)?;
	if inode.kind != InodeKind::Directory {
		return Err(Error::NotADirectory { path: path.to_string() });
	}
	if inode.content_ref.is_zero() {
		return Ok(None);
	}

	let node = store.dir_node_of(inode.content_ref)?;
	lookup(store, &node, name, 0)
}

// endregion: --- Path resolution (free functions)

// region:    --- Query

impl HostFs {
	/// List a directory, delegating to `cow::operations::readdir`.
	///
	/// Read each entry's Inode so the listing carries kind/size/mode: a
	/// `DirEntry` alone is only a name and a reference.
	pub fn read_dir(&self, path: &str) -> Result<Vec<Entry>> {
		read_dir_in(
			&self.store,
			self.root_inode_ref,
			self.volume.superblock.active_commit_seq,
			path,
		)
	}

	/// Read a whole file, delegating to `cow::operations::read_inode`.
	///
	/// The inode fetched here is the one handed on: `read` would fetch it again
	/// to learn the same size and content reference this function has just read
	/// it for, which on a CoW volume is a second device read of the same block.
	pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
		read_file_in(&self.store, self.root_inode_ref, path)
	}

	/// A byte range of a file: at most `len` bytes from `offset`.
	///
	/// **Clamped to the file, never an error.** An offset at or past the end
	/// yields an empty vector, and a range that runs off the end yields what is
	/// there. That is the behaviour a tail-follower wants: it remembers how many
	/// bytes it has already read and asks for "the rest", and a file that has
	/// grown by less than it asked for is the normal case, not a failure. The
	/// caller detects "no more" by the empty result, exactly as it would at EOF.
	pub fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
		read_at_in(&self.store, self.root_inode_ref, path, offset, len)
	}
}

// endregion: --- Query

// region:    --- Query (free functions)

/// The store-generic form of [`HostFs::read_dir`] — the listing as seen from
/// `root_inode_ref`, read through any [`ObjectStore`].
///
/// `commit_seq` is only there to build the `Root` descriptor `readdir`'s
/// signature takes; the kernel reads no field of it (its parameter is named
/// `_root`). A staging session passes its own staged root, so a listing during
/// a session sees staged children that no superblock names yet.
pub(crate) fn read_dir_in<S: ObjectStore + NodeStore>(
	store: &S,
	root_inode_ref: ObjectRef,
	commit_seq: u64,
	path: &str,
) -> Result<Vec<Entry>> {
	let dir_ref = resolve_in(store, root_inode_ref, path)?;
	let inode = store.get_inode(dir_ref).map_err(Error::from)?;
	if inode.kind != InodeKind::Directory {
		return Err(Error::NotADirectory { path: path.to_string() });
	}
	if !inode.content_ref.is_zero() {
		check_dir_depth(store, inode.content_ref)?;
	}

	let mut query_root = Root::new();
	query_root.commit_seq = commit_seq;
	query_root.file_tree_root = root_inode_ref;

	let entries = readdir(store, &query_root, dir_ref).map_err(Error::from)?;

	let mut out = Vec::with_capacity(entries.len());
	for entry in entries {
		let inode_ref = entry.inode_ref();
		let child = store.get_inode(inode_ref).map_err(Error::from)?;
		out.push(Entry {
			name: entry.name.as_str().to_string(),
			kind: child.kind,
			size: child.size,
			mode: child.mode,
			inode_ref,
		});
	}
	Ok(out)
}

/// The store-generic form of [`HostFs::read_file`].
pub(crate) fn read_file_in<S: ObjectStore + NodeStore>(
	store: &S,
	root_inode_ref: ObjectRef,
	path: &str,
) -> Result<Vec<u8>> {
	let file_ref = resolve_in(store, root_inode_ref, path)?;
	let inode = store.get_inode(file_ref).map_err(Error::from)?;
	if inode.kind == InodeKind::Directory {
		return Err(Error::IsADirectory { path: path.to_string() });
	}

	let len = usize::try_from(inode.size).map_err(|_| Error::RootUnreadable {
		detail: format!("{path}: file size {} does not fit in usize", inode.size),
	})?;

	read_inode(store, &inode, 0, len).map_err(Error::from)
}

/// The store-generic form of [`HostFs::read_at`], with the same clamping.
pub(crate) fn read_at_in<S: ObjectStore + NodeStore>(
	store: &S,
	root_inode_ref: ObjectRef,
	path: &str,
	offset: u64,
	len: usize,
) -> Result<Vec<u8>> {
	let file_ref = resolve_in(store, root_inode_ref, path)?;
	let inode = store.get_inode(file_ref).map_err(Error::from)?;
	if inode.kind == InodeKind::Directory {
		return Err(Error::IsADirectory { path: path.to_string() });
	}

	let available = inode.size.saturating_sub(offset);
	let len = len.min(usize::try_from(available).map_err(|_| Error::RootUnreadable {
		detail: format!("{path}: {} unread bytes do not fit in usize", available),
	})?);

	read_inode(store, &inode, offset, len).map_err(Error::from)
}

/// The [`Inode`] at `path`, read through any [`NodeStore`].
pub(crate) fn stat_in<S: NodeStore>(store: &S, root_inode_ref: ObjectRef, path: &str) -> Result<Inode> {
	store.inode_of(resolve_in(store, root_inode_ref, path)?)
}

// endregion: --- Query (free functions)

// region:    --- Support

/// The path components worth walking: `/a//b/.` and `/a/b` are the same path.
///
/// `..` is **refused**, not resolved. The store resolves paths downward from a
/// root object and keeps no parent pointers, so `a/../b` could only be honoured
/// by re-walking `a` — and a component that is silently dropped is worse than
/// one that is refused: `../etc/passwd` would name `etc/passwd` *inside* the
/// volume, which is a surprising answer to a path that asked to leave it.
pub fn components_of(path: &str) -> Result<Vec<&str>> {
	let mut out = Vec::new();
	for c in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
		if c == ".." {
			return Err(Error::Usage(format!(
				"{path} contains `..`, which LedgerFS does not resolve — build paths from \
				 the volume root instead"
			)));
		}
		out.push(c);
	}
	Ok(out)
}

/// Find `name` in `node`, descending into children for an internal node.
///
/// `DirNode::lookup` searches one node's own entries; an internal node holds
/// splitter keys and child references, so a miss there has to be resolved
/// against its children. `cow::operations`' `collect_dir_entries` walks the
/// same structure for the listing side.
///
/// `depth` is how many levels of children have already been followed; the
/// descent is refused once it passes [`MAX_DIR_DEPTH`], so a volume whose
/// `DirNode` references a long chain of further nodes is a returned error
/// rather than unbounded recursion. The chain is finite and content-addressed
/// (`blake3(payload) == object_id` holds for a hostile volume too, so a node
/// cannot name itself), but its *length* is bounded only by the volume size —
/// which is why the walk needs a bound of its own rather than relying on one.
fn lookup<S: NodeStore>(store: &S, node: &DirNode, name: &str, depth: usize) -> Result<Option<ObjectRef>> {
	if let Some(entry) = node.lookup(name) {
		return Ok(Some(entry.inode_ref()));
	}

	if node.is_leaf {
		return Ok(None);
	}

	if depth >= MAX_DIR_DEPTH {
		return Err(too_deep("a directory's DirNode children chain", depth));
	}

	for child in &node.children {
		if child.is_zero() {
			continue;
		}
		let child_node = store.dir_node_of(*child)?;
		if let Some(found) = lookup(store, &child_node, name, depth + 1)? {
			return Ok(Some(found));
		}
	}

	Ok(None)
}

/// Refuse a directory whose `DirNode` children chain is deeper than
/// [`MAX_DIR_DEPTH`].
///
/// `read_dir` gathers a directory's entries through `cow::operations`'
/// `readdir`, whose `collect_dir_entries` recurses down `DirNode::children`
/// with no depth bound of its own — the same shape as [`lookup`], on the
/// listing side. That recursion lives in the kernel crate, so it cannot be
/// bounded here directly; this walks the same structure *iteratively* (an
/// explicit stack, so measuring the depth cannot itself overflow) and refuses
/// before `readdir` is ever entered. The extra pass costs one read per node on
/// a well-formed directory — a single leaf — and only ever runs on the way to
/// a listing.
fn check_dir_depth<S: NodeStore>(store: &S, root: ObjectRef) -> Result<()> {
	let mut stack = vec![(root, 0usize)];
	// A node's own depth is the same however it is reached, so a node named by
	// two parents (which content-addressed storage makes ordinary — identical
	// nodes dedup to one id) is measured once. Without this a crafted volume
	// whose nodes each name the next twice would make this probe itself blow up
	// exponentially, turning the safety check into the denial of service.
	let mut seen = HashSet::new();
	while let Some((node_ref, depth)) = stack.pop() {
		if depth >= MAX_DIR_DEPTH {
			return Err(too_deep("a directory's DirNode children chain", depth));
		}
		if !seen.insert(node_ref.object_id) {
			continue;
		}
		let node = store.dir_node_of(node_ref)?;
		if node.is_leaf {
			continue;
		}
		for child in &node.children {
			if !child.is_zero() {
				stack.push((*child, depth + 1));
			}
		}
	}
	Ok(())
}

/// How many levels of nested `DirNode` children — or nested directories — a
/// walk in this module will follow before refusing the volume.
///
/// LedgerFS's `DirNode` is a B-tree in the format, but B-tree splitting is not
/// implemented: `DirNode::MAX_ENTRIES` is a hard ceiling and
/// `cow_insert_dir_entry` refuses the insert that would cross it — so an
/// internal node is, today, exactly one level deep and a real directory is a
/// single leaf. 64 is therefore far more headroom than the format can produce,
/// while still being far below the depth at which the recursive walks here
/// (`lookup`, `read_dir`'s `collect_dir_entries`, and `Store::walk`) would
/// exhaust a thread's stack on attacker-controlled metadata. The exact number
/// does not matter; that there *is* one does — without it a crafted volume is a
/// stack overflow, and hence process abort, on the program that mounted it.
pub(crate) const MAX_DIR_DEPTH: usize = 64;

/// The refusal for metadata that asks the reader to recurse deeper than any
/// well-formed volume can — see [`MAX_DIR_DEPTH`].
pub(crate) fn too_deep(what: &str, depth: usize) -> Error {
	Error::Unsupported {
		detail: format!(
			"{what} is nested past {MAX_DIR_DEPTH} levels (reached {depth}); no well-formed \
			 volume goes this deep, so this is crafted or corrupt metadata and this reader will \
			 not follow it"
		),
	}
}

// endregion: --- Support
