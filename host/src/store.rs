//! A writable LedgerFS volume on the host.
//!
//! `HostFs` reads a volume through [`ObjectStore`]. This module closes the
//! loop: [`Store`] owns the commit path, so a host process can create,
//! format, write and re-mount a LedgerFS volume — on a disk image, on a bare
//! file, or in memory.
//!
//! # What the kernel crate does and does not do
//!
//! `CommitEngine::commit` writes the transaction's **objects** and issues the
//! barriers between them. It deliberately stops there: it returns the updated
//! [`AllocNode`] and a [`CommitResult`] and expects the *caller* to own
//! persistence and session state, because only the caller knows what survives
//! a restart. [`Store`] is that caller in a host process.
//!
//! So three things happen here that `commit()` cannot do for itself:
//!
//! 1. **Feed the placement hints back into the object cache.** `CommitResult`'s
//!    own doc comment explains why this is not optional: CoW rewrites the root
//!    inode on every commit, so the root is always the highest-block object and
//!    the first thing any path resolution fetches — but the reference built by
//!    `cow::operations` carries hint `0`. Without the hints, every resolution
//!    after every commit falls back to a full linear scan, and the cost grows
//!    with each commit.
//! 2. **Persist the superblock.** The allocator snapshot is not this module's
//!    to write any more: the engine writes it inside its own write phase, under
//!    the same barrier as the objects it placed. So a superblock written here
//!    can never name a root whose snapshot is not already durable, and the
//!    ordering the volume needs holds by construction rather than by a second
//!    barrier. The reverse is concrete: a root published with *no* snapshot
//!    behind it is unrecoverable, because a mount with no snapshot resumes
//!    read-only and a read-only volume cannot write the snapshot that would fix
//!    it. A volume that is formatted and then never written to would be bricked
//!    by having been formatted.
//! 3. **Write all four superblock copies.** The format has four so that one
//!    damaged copy is survivable, and a writer that updates only the primary
//!    one leaves the other three naming a root the volume no longer has. See
//!    this crate's `lib.rs` note on copies that disagree for exactly that
//!    reason.
//!
//! # Which field names the root
//!
//! `new_root.file_tree_root` — the root directory's **`Inode`** — is what goes
//! into `active_root_id`. Not `new_root.object_id`, which is the id of the
//! in-memory [`Root`] descriptor and names nothing on disk, and not
//! `CommitResult::root_id`, which carries that same descriptor id. The other
//! readers (`reader::walk_path` and a boot reader's) decode the active root as
//! an `Inode`, so any writer that persisted the descriptor id would produce a
//! volume they cannot mount.
//! [`Store::persisted_root_ref`] is the single place that decision is made.

use ledgerfs::alloc_node::AllocNode;
use ledgerfs::alloc_state::{AllocationState, PhysicalExtent};
use ledgerfs::allocator::{Allocator, AllocatorConfig};
use ledgerfs::commit::{CommitEngine, CommitResult};
use ledgerfs::cow::{InMemoryObjectStore, ObjectStore, TxnPlan, mkdir};
use ledgerfs::object::{Inode, InodeKind, ObjectRef, Root, StorageHint};
use ledgerfs::superblock::Superblock;
use ledgerfs::{ALLOC_SNAPSHOT_BLOCK, BLOCK_SIZE, RESERVED_HEADER_BLOCKS, RESERVED_TRAILER_BLOCKS};
use ledgerfs::{Error as LedgerFsError, Result as LedgerFsResult};

use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::image::PartitionKind;
use crate::mutate::{Chain, build_create_txn, build_rename_txn, build_unlink_txn};
use crate::objstore::to_ledgerfs;
use crate::path::{Entry, HostFs};
use crate::staging::StagedFold;
use crate::volume::{publish_root_into, superblock_copy_lbas};

use std::path::Path;

/// How a volume was obtained, which is what tells a caller whether it can be
/// written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
	/// Created by [`Store::format`] in this process.
	Formatted,
	/// Re-opened with allocator state restored from the on-disk snapshot.
	/// Writes are accepted and survive a re-open.
	ResumedWritable,
	/// Re-opened with no usable allocator snapshot. Content is readable;
	/// [`Store::commit`] refuses.
	ReadOnly,
}

pub struct Store {
	/// The read half — path resolution, listing, and the object cache the
	/// commit path fills.
	fs: HostFs,

	commit_engine: CommitEngine,

	/// The current CoW root descriptor. `file_tree_root` names the root
	/// directory's `Inode`; `commit_seq` is the sequence this state is at.
	root: Root,

	/// `None` on a read-only volume. Both are set together or neither is:
	/// an allocator without a tree cannot place blocks and a tree without an
	/// allocator cannot track them.
	alloc_node: Option<AllocNode>,
	allocator: Option<Allocator>,

	origin: Origin,

	/// The volume's own configured ceiling, from its superblock's limits.
	/// The reader silently skips a record above this, so an object committed
	/// over it would report success and then read back as missing.
	max_object_bytes: usize,
}

// region:    --- Contract

/// The version of the application-store contract this crate implements.
///
/// The store, not the app, owns this number: it is what [`Store::format_app`]
/// records under [`STORE_DIR`] and what [`Store::contract_version`] reports
/// back, so an app can refuse a volume written against a contract it does not
/// know. See `EMBEDDING.md`.
pub const STORE_CONTRACT_VERSION: &str = "1";

/// The single top-level name an application store reserves for itself.
///
/// Everything under it belongs to the store; an app keeps its own data in any
/// other top-level directory. Reserving exactly one name, rather than a
/// scattering of dot-files, is what lets tooling — the export path, `lfs-read`,
/// a future fsck — recognise a volume's own metadata without knowing which app
/// wrote it.
pub const STORE_DIR: &str = "/.ledgerfs";

/// The file under [`STORE_DIR`] naming the app that owns the volume.
pub const STORE_APP_FILE: &str = "/.ledgerfs/APP";

/// The file under [`STORE_DIR`] recording the contract version.
pub const STORE_VERSION_FILE: &str = "/.ledgerfs/VERSION";

// endregion: --- Contract

// region:    --- Format

impl Store {
	/// Format a whole device as a LedgerFS volume, taking its size from the
	/// device itself.
	///
	/// The embedding entry point. An application that has a [`BlockDevice`] in
	/// its own heap has already said how big the volume is — repeating that
	/// number alongside the device is one more chance for the two to disagree,
	/// and `MemBlockDevice::new(n)`/`format(device, n)` makes the caller write
	/// `n` twice for no reason. [`format`](Self::format) keeps the explicit
	/// size for the caller that wants a volume *smaller* than the device
	/// holding it.
	pub fn format_device(device: Box<dyn BlockDevice>) -> Result<Self> {
		let total_blocks = device.block_count();
		Self::format(device, total_blocks)
	}

	/// Format a device as the application store for `app`, so the volume
	/// identifies its owner and contract version to whoever opens it next.
	///
	/// [`format_device`](Self::format_device) plus three commits that lay down
	/// [`STORE_DIR`] and its two marker files. A store formatted this way is
	/// still an ordinary LedgerFS volume — the markers are ordinary files, not
	/// a format change — so a plain `lfs-read` lists them and a plain
	/// [`Store::open`] reads them. What they add is that an app can ask a volume
	/// whether it is *its* volume before trusting what it finds there; see
	/// [`app`](Self::app) and [`contract_version`](Self::contract_version).
	pub fn format_app(device: Box<dyn BlockDevice>, app: &str) -> Result<Self> {
		if app.is_empty() {
			return Err(Error::Usage("format_app: the app name must not be empty".to_string()));
		}
		let mut store = Self::format_device(device)?;
		store.mkdir(STORE_DIR, 0o755)?;
		store.write_new_file(STORE_VERSION_FILE, STORE_CONTRACT_VERSION.as_bytes(), 0o644)?;
		store.write_new_file(STORE_APP_FILE, app.as_bytes(), 0o644)?;
		Ok(store)
	}

	/// Format `total_blocks` blocks of `device` as a LedgerFS volume and return
	/// a writable store on it.
	///
	/// This is `mkfs`. The order matters and is the one the runtime volume
	/// driver's genesis-bootstrap path arrived at, with an erase pass added
	/// after the first step:
	///
	/// 1. superblock with a null root, so the volume is a recognisable
	///    LedgerFS volume from the first byte written (and a crash mid-format
	///    leaves a whole volume that simply has no root, not a half-formatted
	///    one);
	/// 2. erase every other block, so what the device held before is gone;
	/// 3. a genesis transaction — an empty root directory — through the real
	///    commit engine;
	/// 4. the allocator snapshot;
	/// 5. the superblock again, now naming the genesis root.
	///
	/// The erase comes *after* the null-root superblock rather than before it,
	/// and that ordering is the point: a crash during a leading erase would
	/// leave a blank device that cannot be told apart from one that was never
	/// formatted, whereas every crash point in the order above still leaves a
	/// volume that mounts, reports [`VolumeUncommitted`](Error::VolumeUncommitted)
	/// and can simply be formatted again.
	///
	/// The genesis transaction is built against an empty
	/// [`InMemoryObjectStore`], not against the device. It has to be: nothing
	/// has committed yet, so the device has no root and no directory to read,
	/// and `mkdir` with a zero parent reference creates the root inode without
	/// consulting the store at all. Mounting the volume first — which is what
	/// this used to do — cannot work, because `mount_select` builds its
	/// candidate list from copies with `active_commit_seq > 0` and a
	/// freshly-initialised volume has none.
	pub fn format(device: Box<dyn BlockDevice>, total_blocks: u64) -> Result<Self> {
		if total_blocks <= RESERVED_HEADER_BLOCKS + RESERVED_TRAILER_BLOCKS {
			return Err(Error::Format(LedgerFsError::custom(
				"format: volume is too small to hold any object data past the reserved header and trailer blocks",
			)));
		}
		if total_blocks * BLOCK_SIZE as u64 > device.size_bytes() {
			return Err(Error::Format(LedgerFsError::custom(
				"format: volume claims more blocks than the device holds",
			)));
		}
		// First, before a byte is written: formatting a read-only handle would
		// otherwise get as far as `EBADF` from the first superblock copy.
		if !device.is_writable() {
			return Err(Error::ReadOnly {
				detail: "the device was opened read-only, so it cannot be formatted".to_string(),
			});
		}

		// A hint cache beside this volume describes blocks that are about to
		// stop holding what it says. Entries are re-verified on use, so a stale
		// one could never be *read* as the wrong object — but a volume
		// re-formatted in place keeps its inode, so the identity check in the
		// header would not notice, and every lookup would pay a failed hint read
		// before falling back. Removing it here is the source-side half of the
		// validity rule in `hints.rs`; the other half is a `compact`'s rename,
		// which changes the inode.
		if let Some(cache) = device.hint_cache() {
			crate::hints::remove(&cache.path);
		}

		// -- Step 1: an empty superblock, in all four copies.
		let mut superblock = Superblock::new(total_blocks);
		let max_object_bytes = superblock.max_object_bytes();
		write_all_superblock_copies(device.as_ref(), &superblock)?;
		device.barrier()?;

		// -- Step 2: erase the rest of the device. `mkfs` means the medium now
		// holds this filesystem and nothing else: a volume that still carried
		// its predecessor's blocks would disclose them to whoever could read the
		// device, and would leave stale `ObjectRecord`s for a hint-less reader's
		// scan to trip over. The four superblock copies just written are the only
		// blocks that survive — and `ALLOC_SNAPSHOT_BLOCK`, the journal and the
		// audit are rewritten below, so erasing them here loses nothing.
		erase_except_superblocks(device.as_ref(), total_blocks)?;
		device.barrier()?;

		// -- Step 3: the genesis transaction. A zero parent reference means
		// "create the root", which reads nothing from the store — see this
		// function's doc comment.
		let root = Root::new();
		let txn = mkdir(&InMemoryObjectStore::new(), &root, ObjectRef::zero(), "", 0, 0, 0o755)?;

		// -- Step 4: commit it, through the real engine, to the real device.
		let (alloc_node, mut allocator) = genesis_allocator(total_blocks)?;
		let mut commit_engine = CommitEngine::new();
		commit_engine.set_max_object_bytes(max_object_bytes);

		let (result, alloc_node) = run_commit(
			&mut commit_engine,
			device.as_ref(),
			&txn,
			alloc_node,
			&mut allocator,
			root.commit_seq,
		)?;
		let new_root = root_with_hint(&txn, &result);

		// -- Step 5: persist, snapshot first. See this module's doc comment.
		persist_alloc_snapshot(device.as_ref(), &alloc_node)?;
		publish_root_into(&mut superblock, &new_root);
		persist_superblock(device.as_ref(), &superblock)?;

		// -- Step 6: mount what was just written, so the read half sees
		// exactly what the commit half published.
		let fs = HostFs::mount_device(device)?;

		Ok(Self {
			fs,
			commit_engine,
			root: new_root,
			alloc_node: Some(alloc_node),
			allocator: Some(allocator),
			origin: Origin::Formatted,
			max_object_bytes,
		})
	}

	/// Open an existing volume, read-write when it can be.
	///
	/// A volume with no allocator snapshot opens [`Origin::ReadOnly`] rather
	/// than failing: its content is still perfectly readable, and saying so is
	/// more useful than refusing to mount it.
	///
	/// A volume that has never been committed to does not get this far —
	/// [`Volume::mount`](crate::Volume::mount) refuses it with
	/// [`VolumeUncommitted`](Error::VolumeUncommitted), which names the actual
	/// state rather than reporting a mount failure.
	pub fn open(device: Box<dyn BlockDevice>) -> Result<Self> {
		let fs = HostFs::mount_device(device)?;
		let total_blocks = fs.volume.total_blocks();
		let superblock = fs.volume.superblock.clone();

		// The allocator snapshot is what makes a volume writable: without it
		// there is no way to know which blocks are free, and a wrong answer
		// would place an object on top of live data.
		let (alloc_node, allocator, origin) = match read_alloc_snapshot(fs.store.device(), total_blocks)? {
			Some(node) => {
				let mut allocator = Allocator::new(AllocatorConfig::new(total_blocks));
				allocator.init_from_tree(&node).map_err(Error::from)?;
				(Some(node), Some(allocator), Origin::ResumedWritable)
			}
			None => (None, None, Origin::ReadOnly),
		};

		let mut root = Root::new();
		root.commit_seq = superblock.active_commit_seq;
		root.file_tree_root = fs.root_inode_ref;

		let max_object_bytes = superblock.max_object_bytes();
		let mut commit_engine = CommitEngine::new();
		commit_engine.set_max_object_bytes(max_object_bytes);

		Ok(Self {
			fs,
			commit_engine,
			root,
			alloc_node,
			allocator,
			origin,
			max_object_bytes,
		})
	}

	/// Open the LedgerFS partition `kind` inside a partitioned disk `image`,
	/// read-write.
	pub fn open_partition(image: &Path, kind: PartitionKind) -> Result<Self> {
		let partition = crate::image::find_partition(image, kind)?;
		let device = crate::device::FileBlockDevice::open_rw(image, partition.start_bytes(), partition.size_bytes())?;
		Self::open(Box::new(device))
	}
}

// endregion: --- Format

// region:    --- Commit

impl Store {
	/// How this volume was obtained.
	pub fn origin(&self) -> Origin {
		self.origin
	}

	/// Whether [`commit`](Self::commit) will accept a transaction.
	pub fn is_writable(&self) -> bool {
		self.write_refusal().is_none()
	}

	/// Why a commit would be refused right now, or `None` if it would be
	/// accepted.
	///
	/// [`commit`](Self::commit) reports this as its error and `lfs-write info`
	/// prints it, so what a caller is told before writing is the same sentence
	/// it gets if it tries anyway — the two cannot drift apart.
	pub fn write_refusal(&self) -> Option<String> {
		if !self.fs.store.device().is_writable() {
			return Some("the device was opened read-only".to_string());
		}
		if self.alloc_node.is_none() || self.allocator.is_none() {
			return Some(
				"there is no allocator snapshot on the volume, so there is no way to know \
				 which blocks are free"
					.to_string(),
			);
		}
		None
	}

	/// The volume's current commit sequence.
	pub fn commit_seq(&self) -> u64 {
		self.root.commit_seq
	}

	/// The object reference of the root directory's `Inode`, as it would be
	/// recorded in the superblock.
	///
	/// Takes the hint from this transaction's placement when the engine
	/// reported one, so the persisted `active_root_hint` turns the next mount's
	/// root lookup into a single block read instead of a scan.
	pub fn persisted_root_ref(&self) -> ObjectRef {
		self.root.file_tree_root
	}

	/// Run one transaction: place and write its objects, then persist the
	/// allocator snapshot and the superblock behind it.
	///
	/// Returns the engine's own [`CommitResult`] for inspection. The two
	/// persistence steps are not optional and not separable — see this
	/// module's doc comment.
	pub fn commit(&mut self, txn: TxnPlan) -> Result<CommitResult> {
		// Before the transaction is touched: both of these are checked here
		// rather than left to fail on the first write, one as `EBADF` from the
		// device and the other as a panic on an unwrapped allocator.
		if let Some(refusal) = self.write_refusal() {
			return Err(Error::ReadOnly { detail: refusal });
		}

		let alloc_node = self.alloc_node.take().ok_or_else(|| Error::ReadOnly {
			detail: "there is no allocator snapshot on the volume".to_string(),
		})?;
		let allocator = self.allocator.as_mut().ok_or_else(|| Error::ReadOnly {
			detail: "there is no allocator snapshot on the volume".to_string(),
		})?;

		// The engine takes the kernel crate's write-only `BlockDevice`; this
		// crate's device does both directions.
		let (result, updated_alloc_node) = match run_commit(
			&mut self.commit_engine,
			self.fs.store.device(),
			&txn,
			alloc_node,
			allocator,
			self.root.commit_seq,
		) {
			Ok(pair) => pair,
			Err(e) => {
				// A transaction that fails must cost this handle nothing. Two
				// things are wrong if it does, and both were live bugs:
				//
				// - the allocator node above was taken *out* of `self`, so an
				//   early return here left the store owning no snapshot at all —
				//   every later write then answered "there is no allocator
				//   snapshot on the volume" about a volume that has one, which
				//   bricked the handle for the life of the process;
				// - the placement orchestrator advances the allocator in place
				//   as it places objects, so a failure part-way left it
				//   remembering placements no reader will ever find, and the
				//   free-space reading never came back.
				//
				// Rebuilding from the snapshot the volume itself holds restores
				// both, and the volume is the authority: it has not been told
				// about anything this transaction did.
				self.restore_allocator_from_snapshot();
				return Err(e);
			}
		};

		// Tell the object cache where everything landed. Without this the next
		// path resolution scans the volume for objects this process just wrote.
		for (object_id, hint) in &result.storage_hints {
			self.fs.store.record_hint(*object_id, hint.physical_lba);
		}

		let new_root = root_with_hint(&txn, &result);
		self.alloc_node = Some(updated_alloc_node);

		// -- Persist. The allocator snapshot is already durable: the engine
		// wrote it inside its own write phase, under the same barrier as the
		// objects, so the ordering the superblock below depends on is already
		// satisfied. See this module's doc comment.
		let device = self.fs.store.device();

		self.root = new_root;

		// Move the read half to the root just committed. Without this the
		// store keeps resolving paths through the root it mounted at, so the
		// very transaction that succeeded is invisible to `read_dir`/`resolve`
		// for the rest of the session — and the failure is a `NoSuchFile` for
		// the directory that was just created.
		self.fs.root_inode_ref = self.root.file_tree_root;
		self.fs.volume.publish_root(&self.root);
		persist_superblock(device, &self.fs.volume.superblock)?;

		Ok(result)
	}

	/// Put this handle's in-memory allocator back where the volume says it is,
	/// after a transaction failed part-way through placing objects.
	///
	/// Best-effort by design: it runs on the error path of [`commit`](Self::commit),
	/// and a repair that itself fails must not replace the error the caller asked
	/// about. A failure here leaves the handle as it was — which is exactly the
	/// state this exists to avoid — so it also leaves `alloc_node` as `None` and
	/// the handle refusing further writes, rather than guessing at a placement
	/// table and writing over live data.
	///
	/// A genuinely read-only volume is unaffected: it has no snapshot to read
	/// back, so it stays read-only.
	fn restore_allocator_from_snapshot(&mut self) {
		let total_blocks = self.fs.volume.total_blocks();
		// Dropped on any failure below, so the handle does not hold a snapshot
		// the allocator was not rebuilt from.
		self.alloc_node = None;
		self.allocator = None;

		let node = match read_alloc_snapshot(self.fs.store.device(), total_blocks) {
			Ok(Some(node)) => node,
			// Nothing to restore from. The volume is read-only, or its snapshot
			// is unreadable — either way the refusal stands.
			Ok(None) | Err(_) => return,
		};

		let mut allocator = Allocator::new(AllocatorConfig::new(total_blocks));
		if allocator.init_from_tree(&node).is_err() {
			return;
		}

		self.alloc_node = Some(node);
		self.allocator = Some(allocator);
	}
}

// endregion: --- Commit

// region:    --- Filesystem verbs

impl Store {
	/// Read a directory. Delegates to the read path.
	pub fn read_dir(&self, path: &str) -> Result<Vec<Entry>> {
		self.fs.read_dir(path)
	}

	/// Read a whole file. Delegates to the read path.
	pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
		self.fs.read_file(path)
	}

	/// A byte range of a file — at most `len` bytes from `offset`.
	///
	/// For a caller watching an append-only file (a log, a transcript) without
	/// holding the whole thing: remember how many bytes you have seen, ask for
	/// the rest. A range past the end is an empty vector, not an error.
	pub fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
		self.fs.read_at(path, offset, len)
	}

	/// Stat a path. Delegates to the read path.
	pub fn stat(&self, path: &str) -> Result<Inode> {
		self.fs.stat(path)
	}

	/// Create the directory at `path`.
	///
	/// One transaction, cascaded by `build_create_txn` so the published root
	/// is the volume root and not the CoW copy of `path`'s parent — see the
	/// `mutate` module for what goes wrong otherwise.
	pub fn mkdir(&mut self, path: &str, mode: u32) -> Result<()> {
		let chain = self.fs.resolve_chain(path)?;
		self.ensure_absent(path, &chain)?;
		let txn = build_create_txn(&self.fs.store, &self.root, &chain, 0, 0, mode, true)?;
		self.commit(txn)?;
		Ok(())
	}

	/// Create the empty regular file at `path`.
	pub fn create_file(&mut self, path: &str, mode: u32) -> Result<()> {
		let chain = self.fs.resolve_chain(path)?;
		self.ensure_absent(path, &chain)?;
		let txn = build_create_txn(&self.fs.store, &self.root, &chain, 0, 0, mode, false)?;
		self.commit(txn)?;
		Ok(())
	}

	/// Write `data` to a new file at `path`, creating it.
	///
	/// **One transaction**, whatever the length. The file inode, every chunk
	/// of content, the extent tree over them and the directory entry are
	/// placed and committed together: the commit is this volume's durability
	/// point, so a caller storing a record waits on one of them, and a payload
	/// larger than one object no longer costs one commit per object — nor
	/// leaves a prefix of itself behind when a later piece runs out of room.
	///
	/// The pieces are still one object each. That ceiling belongs to the
	/// format, not to this transaction: a single larger object would be refused
	/// by `CommitEngine` (and skipped silently by readers) because it exceeds
	/// the volume's own limit — [`Superblock::max_object_bytes`], which resolves
	/// an all-zero `LedgerFsLimits` to the compiled-in default rather than to
	/// zero.
	pub fn write_new_file(&mut self, path: &str, data: &[u8], mode: u32) -> Result<()> {
		let chunk = self.max_object_bytes.max(1);
		let chain = self.fs.resolve_chain(path)?;
		self.ensure_absent(path, &chain)?;

		// An empty `data` is the plain create: there is no content to build a
		// chunk and an extent out of, and a file created empty is exactly what
		// `create_file` produces.
		if data.is_empty() {
			let txn = build_create_txn(&self.fs.store, &self.root, &chain, 0, 0, mode, false)?;
			self.commit(txn)?;
			return Ok(());
		}

		let mut fold = StagedFold::new(&self.fs.store, &self.root);
		let head = data.len().min(chunk);
		fold.create_with_data(path, &data[..head], mode)?;
		for (i, piece) in data[head..].chunks(chunk).enumerate() {
			fold.write(path, (head + i * chunk) as u64, piece)?;
		}
		let txn = fold.into_plan(self.root.commit_seq)?;
		self.commit(txn)?;
		Ok(())
	}

	/// Write `data` to `path`, replacing whatever was there.
	///
	/// Two commits: an existing file is removed first, then the new content is
	/// created and written. Deliberately not atomic — a crash between the two
	/// leaves `path` absent — which is stated here because a caller that needs
	/// the replace to be atomic (a `state.json` rewrite, say) should instead
	/// build the new content under a scratch name and [`rename`](Self::rename)
	/// it into place.
	pub fn write_file(&mut self, path: &str, data: &[u8], mode: u32) -> Result<()> {
		match self.stat(path) {
			Ok(inode) if inode.kind == InodeKind::Directory => {
				return Err(Error::IsADirectory { path: path.to_string() });
			}
			Ok(_) => self.remove(path)?,
			Err(Error::NoSuchFile { .. }) => {}
			Err(e) => return Err(e),
		}
		self.write_new_file(path, data, mode)
	}

	/// Append `data` to the file at `path`, creating the file if it is absent.
	///
	/// The append is a `write` at the file's current size, which is what makes
	/// LedgerFS suited to an append-only log like a relay transcript: the new
	/// bytes go into a fresh chunk object and a fresh extent at the end of the
	/// extent tree, and nothing already written is rewritten. Creating the file
	/// when it is missing is the logging case, not POSIX `O_APPEND` — a caller
	/// that wants "must already exist" can [`stat`](Self::stat) first.
	///
	/// **One transaction**, however many objects `data` spans: the pieces are
	/// folded into a single commit by the same machinery
	/// [`write_new_file`](Self::write_new_file) uses.
	pub fn append(&mut self, path: &str, data: &[u8], mode: u32) -> Result<()> {
		let size = match self.stat(path) {
			Ok(inode) => {
				if inode.kind == InodeKind::Directory {
					return Err(Error::IsADirectory { path: path.to_string() });
				}
				inode.size
			}
			Err(Error::NoSuchFile { .. }) => {
				// Creating the file and writing its first record is one logical
				// operation, so it is one commit — the same combined create the
				// `write_new_file` path uses.
				self.write_new_file(path, data, mode)?;
				return Ok(());
			}
			Err(e) => return Err(e),
		};
		self.write_chunks(path, size, data)
	}

	/// Write `data` into the existing file at `path` starting at `offset`,
	/// extending the file.
	///
	/// `offset` must be at or past the file's current end. LedgerFS's `write`
	/// builds a fresh extent for the bytes and does not split an existing one,
	/// so overwriting bytes that are already stored cannot be done through this
	/// call — the store refuses it with [`Error::Unsupported`] rather than
	/// leaving the extent tree with two overlapping ranges. Use
	/// [`write_file`](Self::write_file) to replace a file's content, or
	/// [`append`](Self::append) to extend it.
	pub fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
		let inode = self.stat(path)?;
		if inode.kind == InodeKind::Directory {
			return Err(Error::IsADirectory { path: path.to_string() });
		}
		if offset < inode.size {
			return Err(Error::Unsupported {
				detail: format!(
					"write_at: offset {offset} is inside {path}, which is {} bytes; overwriting \
					 stored bytes is not supported — use write_file to replace it",
					inode.size
				),
			});
		}
		self.write_chunks(path, offset, data)
	}

	/// Delete the file at `path`.
	///
	/// A directory is refused. In a content-addressed model there is no "free
	/// the node" step: the removed inode simply becomes unreachable from the
	/// new root once the transaction commits, and is eligible for GC then — see
	/// the kernel's `cow::operations::unlink`, whose semantics this follows.
	pub fn remove(&mut self, path: &str) -> Result<()> {
		let inode = self.stat(path)?;
		if inode.kind == InodeKind::Directory {
			return Err(Error::IsADirectory { path: path.to_string() });
		}

		let chain = self.fs.resolve_chain(path)?;
		let txn = build_unlink_txn(&self.fs.store, &self.root, &chain)?;
		self.commit(txn)?;
		Ok(())
	}

	/// Rename `from` to `to`, both in the same directory.
	///
	/// A same-directory rename is one CoW chain and one transaction: the one
	/// place LedgerFS gives an app an atomic replace, which is why the
	/// recommended way to rewrite a file in place is to write a scratch name
	/// and rename it over the target. Overwriting an existing destination is
	/// permitted when it is a file, matching `cow::operations::rename`.
	///
	/// A **cross-directory** move is refused with [`Error::Unsupported`] rather
	/// than performed: `rename`'s two chains can share an ancestor, and this
	/// crate's cascade cannot yet reconcile them — see `build_rename_txn`'s doc
	/// comment in the `mutate` module. Move the content with
	/// [`write_file`](Self::write_file) plus [`remove`](Self::remove) if a
	/// cross-directory move is needed today.
	pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
		let from_chain = self.fs.resolve_chain(from)?;
		let to_chain = self.fs.resolve_chain(to)?;

		let (from_parent, old_name) = from_chain.last().cloned().expect("a resolved chain is never empty");
		let (to_parent, _) = to_chain.last().cloned().expect("a resolved chain is never empty");

		if from_parent.object_id != to_parent.object_id {
			return Err(Error::Unsupported {
				detail: format!(
					"rename: {from} and {to} are in different directories; only a same-directory \
					 rename is supported (the two CoW chains a cross-directory move needs are not \
					 yet reconciled by the ancestor cascade)"
				),
			});
		}

		// Name the missing source rather than letting `rename` report it as an
		// invalid operation, and refuse a directory move here for the same
		// reason `remove` does.
		let source = self
			.fs
			.lookup_child(from, from_parent, &old_name)?
			.ok_or_else(|| Error::NoSuchFile { path: from.to_string() })?;
		if self.fs.store.inode(source)?.kind == InodeKind::Directory {
			return Err(Error::Unsupported {
				detail: format!("rename: {from} is a directory; moving directories is not supported"),
			});
		}

		let txn = build_rename_txn(&self.fs.store, &self.root, &to_chain, &old_name)?;
		self.commit(txn)?;
		Ok(())
	}

	/// Write `data` into the existing file at `path` beginning at
	/// `start_offset`, as **one** transaction however many objects it spans.
	///
	/// The pieces are folded, not committed one at a time: each `write`
	/// replaces the file's inode with a CoW copy under a new object id, and the
	/// fold re-resolves the file through an overlay of the pieces already
	/// staged, so piece N+1 builds on piece N. The merged plan is pruned to
	/// what the final root reaches, so the intermediate generations of the
	/// inode and extent node are not written at all — the device bytes are the
	/// chunks plus one copy of the metadata, not one copy per piece.
	fn write_chunks(&mut self, path: &str, start_offset: u64, data: &[u8]) -> Result<()> {
		if data.is_empty() {
			return Ok(());
		}
		// `chunks(0)` panics, so this cannot be allowed to be zero even if a
		// volume's limits somehow resolve to it.
		let chunk = self.max_object_bytes.max(1);

		let mut fold = StagedFold::new(&self.fs.store, &self.root);
		for (i, piece) in data.chunks(chunk).enumerate() {
			fold.write(path, start_offset + (i * chunk) as u64, piece)?;
		}
		let txn = fold.into_plan(self.root.commit_seq)?;
		self.commit(txn)?;
		Ok(())
	}

	/// Refuse a create whose name is already taken.
	///
	/// `cow::operations`' insert does not check, and a second entry with the
	/// same name in a sorted `DirEntry` list is a corrupt directory rather than
	/// a failed create.
	fn ensure_absent(&self, path: &str, chain: &Chain) -> Result<()> {
		let (parent_ref, name) = chain.last().expect("a resolved chain is never empty");
		if self.fs.lookup_child(path, *parent_ref, name)?.is_some() {
			return Err(Error::AlreadyExists { path: path.to_string() });
		}
		Ok(())
	}

	/// The app that owns this volume, if it is an application store.
	///
	/// `Ok(None)` is an ordinary LedgerFS volume with no marker — a store a
	/// plain `mkfs` produced, or one another system formatted. An `Err` is only
	/// for a marker that exists and cannot be read as text: a volume that
	/// claims to be an app store and then contradicts itself is worth
	/// surfacing, not treating as unmarked.
	pub fn app(&self) -> Result<Option<String>> {
		self.read_marker(STORE_APP_FILE)
	}

	/// The application-store contract version recorded on this volume, or
	/// `None` if the volume carries no marker. See [`STORE_CONTRACT_VERSION`].
	pub fn contract_version(&self) -> Result<Option<String>> {
		self.read_marker(STORE_VERSION_FILE)
	}

	/// Read a one-line text marker, distinguishing "absent" from "unreadable".
	fn read_marker(&self, path: &str) -> Result<Option<String>> {
		match self.read_file(path) {
			Ok(bytes) => {
				let text = String::from_utf8(bytes).map_err(|e| Error::MarkerUnreadable {
					path: path.to_string(),
					detail: e.to_string(),
				})?;
				Ok(Some(text))
			}
			// A missing marker file and a missing parent directory are the same
			// answer here: this is not an app store.
			Err(Error::NoSuchFile { .. }) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// The object cache and volume, for callers that want the read path's
	/// types directly.
	pub fn fs(&self) -> &HostFs {
		&self.fs
	}
}

// endregion: --- Filesystem verbs

// region:    --- Space

impl Store {
	/// The volume's total size, in blocks.
	pub fn total_blocks(&self) -> u64 {
		self.fs.volume.total_blocks()
	}

	/// The volume's total size, in bytes.
	pub fn capacity_bytes(&self) -> u64 {
		self.total_blocks() * BLOCK_SIZE as u64
	}

	/// Bytes an app can still write, or `None` when the volume is read-only.
	///
	/// The allocator's user-visible count: it excludes the reserve the
	/// allocator keeps for its own use, so it is the number an app should treat
	/// as its budget. It falls on every commit and, as of this crate, never
	/// rises — a CoW write commits a new version of every object a transaction
	/// touches and frees nothing, because GC is not wired up. An always-on
	/// store therefore fills monotonically, and one that runs for long enough
	/// has to watch this number and rotate. That is precisely why a free-space
	/// reading is part of the contract and not an optional extra.
	pub fn free_bytes(&self) -> Option<u64> {
		self.allocator.as_ref().map(Allocator::user_free_bytes)
	}

	/// Bytes held in the allocator's reserve, or `None` when read-only.
	pub fn reserved_bytes(&self) -> Option<u64> {
		self.allocator.as_ref().map(Allocator::reserved_free_bytes)
	}

	/// Free bytes including the reserve, or `None` when read-only.
	pub fn total_free_bytes(&self) -> Option<u64> {
		self.allocator.as_ref().map(Allocator::total_free_bytes)
	}
}

// endregion: --- Space

// region:    --- Commit device bridge

/// Bridges this crate's read/write [`BlockDevice`] to the kernel crate's
/// write-only `commit::BlockDevice`.
///
/// The kernel trait models exactly what `CommitEngine` needs — `write_blocks`
/// plus a barrier — and has no read method, because the kernel reads through a
/// separate path entirely (`reader.rs`, over ATA PIO). One host device serves
/// both directions, so the two halves meet here instead of in a second device
/// type.
struct CommitDevice<'a>(&'a dyn BlockDevice);

impl ledgerfs::commit::BlockDevice for CommitDevice<'_> {
	fn write_blocks(&mut self, lba: u64, data: &[u8]) -> LedgerFsResult<()> {
		self.0.write_blocks(lba, data).map_err(to_ledgerfs)
	}

	fn storage_barrier(&mut self) -> LedgerFsResult<()> {
		self.0.barrier().map_err(to_ledgerfs)
	}
}

// endregion: --- Commit device bridge

// region:    --- Support

/// Run `txn` through the commit engine against `device`.
///
/// The engine is write-only by design (see [`CommitDevice`]), so this is where
/// the two halves of one host [`BlockDevice`] meet. Nothing is persisted here:
/// placement, object writes and the barriers between them are all the engine
/// does, and the allocator snapshot and superblock are the caller's job — see
/// this module's doc comment.
fn run_commit(
	engine: &mut CommitEngine,
	device: &dyn BlockDevice,
	txn: &TxnPlan,
	alloc_node: AllocNode,
	allocator: &mut Allocator,
	commit_seq: u64,
) -> Result<(CommitResult, AllocNode)> {
	let mut commit_device = CommitDevice(device);
	engine
		.commit(txn, alloc_node, allocator, commit_seq, &mut commit_device)
		.map_err(Error::from)
}

/// The transaction's new root descriptor, with `file_tree_root` carrying the
/// placement the engine reported.
///
/// `cow::operations` builds its object references through `ObjectRef::from_id`,
/// which sets the hint to `0` — meaning "scan for it". For the root that is the
/// most expensive possible answer: CoW rewrites the root inode on **every**
/// commit, so it is always the highest-block object *and* the first object every
/// path resolution fetches. See [`CommitResult`]'s own doc comment.
fn root_with_hint(txn: &TxnPlan, result: &CommitResult) -> Root {
	let mut new_root = txn.new_root.clone();
	let root_inode_id = new_root.file_tree_root.object_id;
	let root_hint = result.storage_hints.get(&root_inode_id).map(|h| h.physical_lba).unwrap_or(0);
	new_root.file_tree_root = ObjectRef::new(root_inode_id, StorageHint(root_hint));
	new_root
}

/// Write the allocator's free-space tree to its snapshot block and make it
/// durable.
///
/// [`Store::format`]'s initial snapshot is the only caller. Every later one is
/// written by `CommitEngine`, inside its write phase and under the barrier that
/// also covers the transaction's objects — see this module's doc comment.
fn persist_alloc_snapshot(device: &dyn BlockDevice, alloc_node: &AllocNode) -> Result<()> {
	let snapshot = alloc_node.encode_snapshot_block(BLOCK_SIZE as usize)?;
	device.write_block(ALLOC_SNAPSHOT_BLOCK, &snapshot)?;
	device.barrier()
}

/// Zero every block of the volume except the four superblock copies.
///
/// Written in passes rather than one buffer, because a volume can be far larger
/// than it is reasonable to allocate, and skipping the copies rather than
/// rewriting them afterwards keeps the erase from racing the step that made the
/// volume recognisable.
fn erase_except_superblocks(device: &dyn BlockDevice, total_blocks: u64) -> Result<()> {
	const PASS_BLOCKS: u64 = 1024;

	let protected = superblock_copy_lbas(total_blocks);
	let is_protected = |lba: u64| protected.contains(&lba);

	let zeros = vec![0u8; PASS_BLOCKS as usize * BLOCK_SIZE as usize];

	let mut lba = 0u64;
	while lba < total_blocks {
		if is_protected(lba) {
			lba += 1;
			continue;
		}

		// The longest unprotected run starting here, capped at one pass. A run
		// is never empty: `lba` is unprotected and `total_blocks` is past it.
		let mut run = 0u64;
		while lba + run < total_blocks && run < PASS_BLOCKS && !is_protected(lba + run) {
			run += 1;
		}

		device.write_blocks(lba, &zeros[..run as usize * BLOCK_SIZE as usize])?;
		lba += run;
	}

	Ok(())
}

/// The free-space tree a freshly formatted volume starts with.
///
/// `[RESERVED_HEADER_BLOCKS, total_blocks - RESERVED_TRAILER_BLOCKS)` is free
/// and the two ends are reserved. Not cosmetic: `bootstrap_genesis`'s own doc
/// comment records that without it the first genesis commit placed its root
/// `Inode` at block 0, which the superblock write then overwrote, orphaning the
/// object — the superblock, the alloc snapshot and the genesis audit live in
/// the header, and the backup superblock copies live in the trailer.
fn genesis_allocator(total_blocks: u64) -> Result<(AllocNode, Allocator)> {
	let free_start = RESERVED_HEADER_BLOCKS;
	let free_end = total_blocks.saturating_sub(RESERVED_TRAILER_BLOCKS);
	if free_end <= free_start {
		return Err(Error::Format(LedgerFsError::custom(
			"format: no usable object region between the reserved header and trailer",
		)));
	}

	let mut alloc_node = AllocNode::new();
	alloc_node.add_extent(PhysicalExtent::new(0, free_start as u32, AllocationState::Reserved))?;
	alloc_node.add_extent(PhysicalExtent::new(
		free_start,
		(free_end - free_start) as u32,
		AllocationState::Free,
	))?;
	alloc_node.add_extent(PhysicalExtent::new(
		free_end,
		(total_blocks - free_end) as u32,
		AllocationState::Reserved,
	))?;

	let mut allocator = Allocator::new(AllocatorConfig::new(total_blocks));
	allocator.init_from_tree(&alloc_node)?;

	Ok((alloc_node, allocator))
}

/// Read the allocator snapshot, if the volume has a usable one.
fn read_alloc_snapshot(device: &dyn BlockDevice, total_blocks: u64) -> Result<Option<AllocNode>> {
	if ALLOC_SNAPSHOT_BLOCK >= total_blocks {
		return Ok(None);
	}
	let mut buf = vec![0u8; BLOCK_SIZE as usize];
	device.read_block(ALLOC_SNAPSHOT_BLOCK, &mut buf)?;
	// `decode_snapshot_block` answers `None` for both "never written" and
	// "corrupt", which is the same thing here: neither can be trusted to say
	// which blocks are free.
	Ok(AllocNode::decode_snapshot_block(&buf)?)
}

/// Write `superblock` to all four copies.
///
/// `Superblock::encode` takes `&mut self` because it recomputes the checksums
/// as it writes them, so this encodes a copy rather than mutating the caller's
/// — a writer should not have the volume's mounted state changed underneath it
/// by an I/O call.
fn write_all_superblock_copies(device: &dyn BlockDevice, superblock: &Superblock) -> Result<()> {
	let mut encoded = superblock.clone();
	let mut buf = vec![0u8; BLOCK_SIZE as usize];
	encoded.encode(&mut buf)?;
	for lba in superblock_copy_lbas(encoded.total_blocks) {
		device.write_block(lba, &buf)?;
	}
	Ok(())
}

/// Write an already-published `superblock` to all four copies and make it
/// durable.
///
/// The publishing itself — setting the active slot and recomputing its
/// checksum — is [`Volume::publish_root`]'s job for a mounted volume, and
/// [`Superblock`]'s own for [`Store::format`]. Splitting it that way keeps the
/// in-memory view and the on-disk copies updated by the same step instead of
/// two that can drift.
///
/// The other three copies exist so a damaged primary is survivable. A writer
/// that updates only the primary leaves them naming a root the volume no longer
/// has — see this crate's `lib.rs` on the state the data volume is in for
/// exactly that reason.
fn persist_superblock(device: &dyn BlockDevice, superblock: &Superblock) -> Result<()> {
	write_all_superblock_copies(device, superblock)?;
	device.barrier()
}

// endregion: --- Support

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use crate::device::{FileBlockDevice, MemBlockDevice};

	type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

	/// 4 MiB — big enough for the reserved regions plus real object space.
	const BLOCKS: u64 = 1024;

	/// A directory's entry names, which is what most of these assertions are
	/// really about.
	fn names(store: &Store, path: &str) -> Result<Vec<String>> {
		Ok(store.read_dir(path)?.into_iter().map(|e| e.name).collect())
	}

	#[test]
	fn test_store_format_then_write_then_reopen() -> Result<()> {
		// -- Setup & Fixtures: keep a clone so the volume's bytes can be read
		// back after the store takes ownership of the device.
		let device = MemBlockDevice::new(BLOCKS);
		let mirror = device.clone();
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		assert_eq!(store.origin(), Origin::Formatted);
		assert!(store.is_writable(), "a freshly formatted volume must be writable");

		// -- Exec: create a directory, then a file inside it.
		store.mkdir("/data", 0o755)?;
		store.write_new_file("/data/hello.txt", b"ledgerfs from the host", 0o644)?;

		// -- Check: readable in the same session...
		assert_eq!(store.read_file("/data/hello.txt")?, b"ledgerfs from the host");
		let names: Vec<String> = store.read_dir("/data")?.into_iter().map(|e| e.name).collect();
		assert_eq!(names, vec!["hello.txt".to_string()]);

		// -- Exec: keep the bytes, reopen from scratch.
		let bytes = mirror.bytes_snapshot();
		let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;

		// -- Check: the write survived, and a formatted volume stays writable.
		assert_eq!(reopened.origin(), Origin::ResumedWritable);
		assert!(reopened.is_writable(), "a formatted volume must not reopen read-only");
		assert_eq!(reopened.read_file("/data/hello.txt")?, b"ledgerfs from the host");
		assert_eq!(reopened.read_dir("/")?.len(), 1);

		Ok(())
	}

	#[test]
	fn test_store_format_erases_what_the_device_held() -> Result<()> {
		// -- Setup & Fixtures: a device that already holds something, which is
		// the ordinary case — formatting over an existing volume, not into a
		// fresh one. Nothing of it may survive, or `mkfs` has left the previous
		// volume's contents readable to anyone who can read the device.
		let device = MemBlockDevice::from_bytes(vec![0xAB; BLOCKS as usize * BLOCK_SIZE as usize])?;

		// -- Exec
		let mut store = Store::format(Box::new(device), BLOCKS)?;

		// -- Check: nothing of the old pattern survived. Looked for as a *run*
		// rather than a single byte, because the genesis commit legitimately
		// writes objects of its own and a BLAKE3 hash will contain a 0xAB about
		// once every 256 bytes — a one-byte test would fail on new data. Sixteen
		// in a row is what only the old, uniformly-filled device can produce.
		let mut buf = [0u8; BLOCK_SIZE as usize];
		for lba in 0..BLOCKS {
			store.fs().store.device().read_block(lba, &mut buf)?;
			assert!(
				!buf.windows(16).any(|w| w.iter().all(|b| *b == 0xAB)),
				"block {lba} still holds what the device held before the format"
			);
		}

		// -- Check: and the erased volume is a working one, not just a clean one.
		store.mkdir("/data", 0o755)?;
		store.write_new_file("/data/after.txt", b"written over the old volume", 0o644)?;
		assert_eq!(store.read_file("/data/after.txt")?, b"written over the old volume");

		Ok(())
	}

	#[test]
	fn test_store_mutations_three_deep_do_not_re_root_the_volume() -> Result<()> {
		// The ancestor cascade's reason for existing. `mkdir("/a/b/c")` comes
		// back with `new_root.file_tree_root` naming the CoW copy of `/a/b` —
		// the top of the chain it modified, not the volume root — so committing
		// it as-is silently re-roots the filesystem at `/a/b`: `/a` disappears
		// from `/` and takes `sibling` with it. The volume still mounts, because
		// the superblock published is perfectly valid and merely points somewhere
		// wrong.
		let device = MemBlockDevice::new(BLOCKS);
		let mirror = device.clone();
		let mut store = Store::format(Box::new(device), BLOCKS)?;

		// -- Exec
		store.mkdir("/sibling", 0o755)?;
		store.mkdir("/a", 0o755)?;
		store.mkdir("/a/b", 0o755)?;
		store.mkdir("/a/b/c", 0o755)?;
		store.write_new_file("/a/b/c/deep.txt", b"three levels down", 0o644)?;

		// -- Check: every level lists what it should, up to and including `/`.
		assert_eq!(names(&store, "/")?, vec!["a", "sibling"]);
		assert_eq!(names(&store, "/a")?, vec!["b"]);
		assert_eq!(names(&store, "/a/b")?, vec!["c"]);
		assert_eq!(names(&store, "/a/b/c")?, vec!["deep.txt"]);
		assert_eq!(store.read_file("/a/b/c/deep.txt")?, b"three levels down");

		// -- Exec: the same must hold for a reader that mounts from scratch,
		// which is the state a re-rooted volume would have persisted.
		let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(mirror.bytes_snapshot())?))?;

		// -- Check
		assert_eq!(names(&reopened, "/")?, vec!["a", "sibling"]);
		assert_eq!(reopened.read_file("/a/b/c/deep.txt")?, b"three levels down");

		Ok(())
	}

	#[test]
	fn test_store_write_spans_multiple_objects() -> Result<()> {
		// A payload past the volume's object ceiling, so it takes more than one
		// transaction. Each write replaces the file's inode with a CoW copy under
		// a new object id, so a second write through the *first* write's
		// reference does not fail — it rebuilds the file from the original empty
		// inode and keeps only the last chunk, silently.
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;

		// -- Setup & Fixtures: deterministic, and not compressible or repetitive
		// enough for the byte comparison to pass by accident.
		let chunk = store.max_object_bytes;
		assert!(
			chunk > 0,
			"an all-zero LedgerFsLimits must resolve to the default, not to 0"
		);
		let payload: Vec<u8> = (0..(chunk + chunk / 4)).map(|i| (i % 251) as u8).collect();

		// -- Exec
		store.write_new_file("/big.bin", &payload, 0o644)?;

		// -- Check: every byte, and the size the inode claims.
		assert_eq!(store.stat("/big.bin")?.size, payload.len() as u64);
		assert_eq!(store.read_file("/big.bin")?, payload);

		Ok(())
	}

	#[test]
	fn test_store_refuses_a_duplicate_create() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		store.mkdir("/d", 0o755)?;

		// -- Exec
		let err = store.mkdir("/d", 0o755).expect_err("a duplicate create must be refused");

		// -- Check: refused, and the volume gained no second entry. `cow`'s
		// insert does not check, and two entries under one name is a corrupt
		// directory rather than a failed create.
		assert!(matches!(err, Error::AlreadyExists { .. }), "got {err:?}");
		assert_eq!(names(&store, "/")?, vec!["d"]);

		Ok(())
	}

	#[test]
	fn test_store_writes_every_superblock_copy() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let store = Store::format(Box::new(device), BLOCKS)?;

		// -- Exec
		let status = store.fs.volume.copy_status();

		// -- Check
		// All four, not just the primary: this is the property the runtime
		// volume driver's single-copy superblock writer loses, and the one that
		// leaves the data volume on a booted image with no redundancy at all.
		assert!(
			status.is_healthy(),
			"every copy must carry the committed root: {status:?}"
		);
		assert_eq!(status.decoded_count(), 4);
		assert!(status.agree.iter().all(|a| *a));

		Ok(())
	}

	#[test]
	fn test_store_refuses_commit_without_allocator_snapshot() -> Result<()> {
		// -- Setup & Fixtures: format, then blank the snapshot block to
		// simulate the bricking state — formatted, but with no allocator state
		// behind the published root.
		let device = MemBlockDevice::new(BLOCKS);
		let mirror = device.clone();
		// Formatting is what puts a root and a snapshot on the volume; the store
		// itself is then dropped so the bytes can be re-opened from scratch.
		let _formatted = Store::format(Box::new(device), BLOCKS)?;
		let mut bytes = mirror.bytes_snapshot();
		let at = ALLOC_SNAPSHOT_BLOCK as usize * BLOCK_SIZE as usize;
		bytes[at..at + BLOCK_SIZE as usize].fill(0);

		// -- Exec
		let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;

		// -- Check: readable, and honest about not being writable.
		assert_eq!(reopened.origin(), Origin::ReadOnly);
		assert!(!reopened.is_writable());
		assert_eq!(reopened.read_dir("/")?.len(), 0, "content must still be readable");

		Ok(())
	}

	#[test]
	fn test_store_format_device_takes_its_size_from_the_device() -> Result<()> {
		// The embedding path: the caller sizes the device once and never
		// repeats it, so there is no second number to disagree with the first.
		// See `examples/embed.rs`.
		let store = Store::format_device(Box::new(MemBlockDevice::new(BLOCKS)))?;

		assert_eq!(store.fs().volume.total_blocks(), BLOCKS);
		assert!(store.is_writable());
		assert_eq!(store.origin(), Origin::Formatted);

		Ok(())
	}

	#[test]
	fn test_store_refuses_a_write_on_a_read_only_device() -> Result<()> {
		// -- Setup & Fixtures: a real volume on disk, then a read-only handle
		// to it. This was not hypothetical — it is the state `lfs-write` sat in
		// when a mutating command opened its target with `FileBlockDevice::open`,
		// where the symptom was `EBADF` from inside `write_at`.
		let path = std::env::temp_dir().join(format!("ledgerfs-store-{}.img", std::process::id()));
		let size = BLOCKS * BLOCK_SIZE as u64;
		{
			let _formatted = Store::format(Box::new(FileBlockDevice::create(&path, size)?), BLOCKS)?;
		}

		let mut store = Store::open(Box::new(FileBlockDevice::open(&path, 0, size)?))?;

		// -- Check: two different questions, two different answers. The volume
		// is a healthy, resumable one — `origin` describes the volume — while
		// this handle is the reason it cannot be written.
		assert_eq!(store.origin(), Origin::ResumedWritable);
		assert!(!store.is_writable());
		assert_eq!(
			store.write_refusal().as_deref(),
			Some("the device was opened read-only")
		);

		// -- Exec: the refusal is the one a write actually gets.
		let err = store.mkdir("/data", 0o755).unwrap_err();

		// -- Check
		assert!(matches!(err, Error::ReadOnly { .. }), "got {err:?}");
		assert!(
			store.read_dir("/").is_ok(),
			"a read-only handle must still be usable for reading"
		);

		let _ = std::fs::remove_file(&path);
		Ok(())
	}

	#[test]
	fn test_store_refuses_a_volume_that_was_never_formatted() -> Result<()> {
		// -- Setup & Fixtures: a raw superblock and nothing else, which is
		// what the image initialiser alone leaves behind — every copy valid, no
		// commit ever made.
		let device = MemBlockDevice::new(BLOCKS);
		write_all_superblock_copies(&device, &Superblock::new(BLOCKS))?;

		// -- Exec
		let err = Store::open(Box::new(device))
			.err()
			.expect("an unformatted volume must not open");

		// -- Check: the volume is intact and simply has nothing committed to
		// it. Reporting `NoValidSuperblock` here — which is what the mount
		// selection itself returns — would call a format state damage.
		assert!(
			matches!(err, Error::VolumeUncommitted { total_blocks: BLOCKS }),
			"got {err:?}"
		);
		assert!(err.to_string().contains("never formatted"), "{err}");

		Ok(())
	}

	#[test]
	fn test_store_persists_a_root_hint() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		store.mkdir("/d", 0o755)?;

		// -- Exec
		let root_ref = store.persisted_root_ref();

		// -- Check: a real hint, not the `0` that means "scan for it". A root
		// written without its placement lands at 0, and a mount of such a
		// volume linearly scans to find its root.
		assert_ne!(
			root_ref.hint.0, 0,
			"the committed root must carry its placement, or every reopen scans"
		);

		Ok(())
	}

	#[test]
	fn test_store_append_creates_then_grows() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mirror = device.clone();
		let mut store = Store::format(Box::new(device), BLOCKS)?;

		// -- Exec: append to a path that does not exist yet, twice. This is the
		// relay-transcript shape — the log is created by its first line.
		store.append("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;
		store.append("/log.jsonl", b"{\"turn\":2}\n", 0o644)?;

		// -- Check: in order, and the size is the sum.
		assert_eq!(store.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
		assert_eq!(store.stat("/log.jsonl")?.size, 22);

		// -- Exec & Check: an append-only log is worthless if it does not
		// survive a restart.
		let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(mirror.bytes_snapshot())?))?;
		assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");

		Ok(())
	}

	#[test]
	fn test_store_append_spans_multiple_objects() -> Result<()> {
		// A single append past the volume's object ceiling has to be split, the
		// same way a whole-file write is, or `CommitEngine` refuses it.
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;

		let chunk = store.max_object_bytes;
		let payload: Vec<u8> = (0..(chunk + chunk / 3)).map(|i| (i % 251) as u8).collect();

		// -- Exec
		store.append("/big.log", &payload, 0o644)?;

		// -- Check: every byte, and the size the inode claims.
		assert_eq!(store.stat("/big.log")?.size, payload.len() as u64);
		assert_eq!(store.read_file("/big.log")?, payload);

		Ok(())
	}

	#[test]
	fn test_store_write_file_replaces_the_content() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		store.write_new_file("/state.json", b"{\"a\":1}", 0o644)?;

		// -- Exec: replace with *shorter* content. This is the case a plain
		// extend would get wrong — the old tail would survive.
		store.write_file("/state.json", b"{}", 0o644)?;

		// -- Check
		assert_eq!(store.read_file("/state.json")?, b"{}");
		assert_eq!(store.stat("/state.json")?.size, 2);

		Ok(())
	}

	#[test]
	fn test_store_remove_takes_the_name_back() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		store.mkdir("/d", 0o755)?;
		store.write_new_file("/d/f", b"x", 0o644)?;

		// -- Exec
		store.remove("/d/f")?;

		// -- Check: gone from the listing and from the read path.
		assert_eq!(names(&store, "/d")?, Vec::<String>::new());
		assert!(matches!(store.read_file("/d/f"), Err(Error::NoSuchFile { .. })));

		// -- Exec & Check: the name really was released, so it can be recreated.
		store.write_new_file("/d/f", b"y", 0o644)?;
		assert_eq!(store.read_file("/d/f")?, b"y");

		// -- Exec & Check: a missing source and a directory are both refusals.
		assert!(matches!(store.remove("/d/nope"), Err(Error::NoSuchFile { .. })));
		assert!(matches!(store.remove("/d"), Err(Error::IsADirectory { .. })));

		Ok(())
	}

	#[test]
	fn test_store_rename_within_a_directory() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		store.write_new_file("/a.txt", b"content", 0o644)?;

		// -- Exec & Check: a plain same-directory rename.
		store.rename("/a.txt", "/b.txt")?;
		assert_eq!(names(&store, "/")?, vec!["b.txt"]);
		assert_eq!(store.read_file("/b.txt")?, b"content");

		// -- Exec & Check: renaming over an existing *file* replaces it, which
		// is what makes write-scratch-then-rename an atomic replace.
		store.write_new_file("/c.txt", b"other", 0o644)?;
		store.rename("/b.txt", "/c.txt")?;
		assert_eq!(names(&store, "/")?, vec!["c.txt"]);
		assert_eq!(store.read_file("/c.txt")?, b"content");

		// -- Exec & Check: the three refusals.
		assert!(matches!(store.rename("/nope", "/x"), Err(Error::NoSuchFile { .. })));
		store.mkdir("/sub", 0o755)?;
		store.write_new_file("/sub/inner", b"i", 0o644)?;
		assert!(matches!(store.rename("/sub", "/moved"), Err(Error::Unsupported { .. })));
		assert!(matches!(
			store.rename("/c.txt", "/sub/c.txt"),
			Err(Error::Unsupported { .. })
		));

		// -- Check: and the refused moves changed nothing.
		assert_eq!(names(&store, "/")?, vec!["c.txt", "sub"]);
		assert_eq!(store.read_file("/sub/inner")?, b"i");

		Ok(())
	}

	#[test]
	fn test_store_write_at_extends_but_refuses_overwrite() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;
		store.write_new_file("/f", b"hello", 0o644)?;

		// -- Exec: extend at the end.
		store.write_at("/f", 5, b" world")?;
		assert_eq!(store.read_file("/f")?, b"hello world");

		// -- Exec & Check: writing *inside* the existing bytes is refused rather
		// than done wrongly — a second extent overlapping the first is not a
		// replace, it is a corrupt extent tree.
		let err = store.write_at("/f", 2, b"X").unwrap_err();
		assert!(matches!(err, Error::Unsupported { .. }), "got {err:?}");
		assert_eq!(store.read_file("/f")?, b"hello world");

		Ok(())
	}

	#[test]
	fn test_store_reports_free_space_that_only_falls() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mut store = Store::format(Box::new(device), BLOCKS)?;

		// -- Check: a formatted volume knows its geometry and its budget.
		assert_eq!(store.total_blocks(), BLOCKS);
		assert_eq!(store.capacity_bytes(), BLOCKS * BLOCK_SIZE as u64);
		let before = store.free_bytes().expect("a writable volume reports free space");
		assert!(before > 0 && before < store.capacity_bytes());

		// -- Exec
		store.write_new_file("/f", &vec![7u8; 64 * 1024], 0o644)?;

		// -- Check: writing consumes space, and nothing gives it back — CoW
		// frees nothing until GC is wired up. An always-on store has to watch
		// this number; that is why it is on the surface.
		let after = store.free_bytes().expect("still writable");
		assert!(after < before, "free space must fall: {after} !< {before}");

		// -- Check: a volume with no allocator snapshot cannot answer at all.
		let ro_bytes = {
			let fresh = MemBlockDevice::new(BLOCKS);
			let mirror = fresh.clone();
			Store::format(Box::new(fresh), BLOCKS)?;
			let mut bytes = mirror.bytes_snapshot();
			let at = ALLOC_SNAPSHOT_BLOCK as usize * BLOCK_SIZE as usize;
			bytes[at..at + BLOCK_SIZE as usize].fill(0);
			bytes
		};
		let ro = Store::open(Box::new(MemBlockDevice::from_bytes(ro_bytes)?))?;
		assert_eq!(ro.free_bytes(), None);

		Ok(())
	}

	#[test]
	fn test_store_format_app_records_identity() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(BLOCKS);
		let mirror = device.clone();

		// -- Exec
		let store = Store::format_app(Box::new(device), "example")?;

		// -- Check: the markers are ordinary files under the one reserved name.
		assert_eq!(store.app()?, Some("example".to_string()));
		assert_eq!(store.contract_version()?, Some(STORE_CONTRACT_VERSION.to_string()));
		let marker_names = names(&store, STORE_DIR)?;
		assert!(marker_names.contains(&"APP".to_string()));
		assert!(marker_names.contains(&"VERSION".to_string()));

		// -- Exec & Check: the identity survives a reopen — that is the whole
		// point of writing it down.
		let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(mirror.bytes_snapshot())?))?;
		assert_eq!(reopened.app()?, Some("example".to_string()));

		// -- Check: a plainly formatted volume is nobody's app store, which is
		// the answer a caller opening a stranger's volume needs.
		let plain = Store::format(Box::new(MemBlockDevice::new(BLOCKS)), BLOCKS)?;
		assert_eq!(plain.app()?, None);

		// -- Exec & Check: an empty app name is refused before a byte is written.
		assert!(Store::format_app(Box::new(MemBlockDevice::new(BLOCKS)), "").is_err());

		Ok(())
	}

	#[test]
	fn test_store_read_at_returns_a_clamped_range() -> Result<()> {
		// -- Setup & Fixtures
		let mut store = Store::format(Box::new(MemBlockDevice::new(BLOCKS)), BLOCKS)?;
		store.write_new_file("/log.jsonl", b"first\nsecond\nthird\n", 0o644)?;

		// -- Exec & Check: a range inside the file is exactly that range.
		assert_eq!(store.read_at("/log.jsonl", 0, 6)?, b"first\n");
		assert_eq!(store.read_at("/log.jsonl", 6, 7)?, b"second\n");

		// -- Check: a tail-follower asks for "the rest" and gets what is there,
		// even when the file grew by less than it asked for.
		assert_eq!(store.read_at("/log.jsonl", 13, 4096)?, b"third\n");

		// -- Check: at or past the end is empty, not an error — that is how the
		// follower learns there is nothing new rather than by matching on Err.
		assert_eq!(store.read_at("/log.jsonl", 19, 4096)?, b"");
		assert_eq!(store.read_at("/log.jsonl", 9999, 4096)?, b"");
		assert_eq!(store.read_at("/log.jsonl", 6, 0)?, b"");

		// -- Check: the ordinary whole-file read is still the whole file.
		assert_eq!(store.read_file("/log.jsonl")?, b"first\nsecond\nthird\n");

		Ok(())
	}

	#[test]
	fn test_store_refuses_dotdot_in_a_path() -> Result<()> {
		// A component that is silently dropped is worse than one that is
		// refused: `../etc/passwd` must not quietly become `etc/passwd` *inside*
		// the volume, which is a surprising answer to a path that asked to leave.
		let mut store = Store::format(Box::new(MemBlockDevice::new(BLOCKS)), BLOCKS)?;
		store.mkdir("/etc", 0o755)?;
		store.write_new_file("/etc/passwd", b"inside the volume", 0o644)?;

		for path in ["/../etc/passwd", "/etc/../etc/passwd", "..", "a/.."] {
			let err = store.read_file(path).expect_err("`..` must be refused");
			assert!(matches!(err, Error::Usage(_)), "{path}: got {err:?}");
		}

		// The same rule on the write side, so a create cannot escape either.
		assert!(store.mkdir("/../escape", 0o755).is_err());
		assert!(store.write_new_file("/a/../b", b"x", 0o644).is_err());

		// And `.`/`//` are still ordinary path noise, not refusals.
		assert_eq!(store.read_file("/etc/./passwd")?, b"inside the volume");
		assert_eq!(store.read_file("//etc//passwd")?, b"inside the volume");

		Ok(())
	}

	#[test]
	fn test_a_missing_parent_directory_is_named_in_the_error() -> Result<()> {
		// The error has to point at what is actually absent. A writer creating
		// `findings/FIND-0001.md` in a campaign that never made `findings/`
		// reported the *file* missing, and a caller that believes it is a file
		// problem retries the file forever. Naming the directory is the
		// difference between a one-second fix and an afternoon.
		let mut store = Store::format(Box::new(MemBlockDevice::new(BLOCKS)), BLOCKS)?;

		let err = store.create_file("findings/FIND-0001.md", 0o644).expect_err("no findings/ dir");
		match err {
			Error::NoSuchFile { ref path } => assert_eq!(path, "findings", "must name the directory"),
			other => panic!("expected NoSuchFile naming the parent, got {other:?}"),
		}

		// Two levels down, the deepest missing component is the one reported.
		store.mkdir("evidence", 0o755)?;
		let err = store
			.create_file("evidence/captures/run.json", 0o644)
			.expect_err("no captures/");
		match err {
			Error::NoSuchFile { ref path } => assert_eq!(path, "evidence/captures"),
			other => panic!("expected NoSuchFile naming evidence/captures, got {other:?}"),
		}

		// A missing *file* under a directory that does exist still names the file.
		let err = store.read_file("evidence/absent.json").expect_err("no such file");
		match err {
			Error::NoSuchFile { ref path } => assert_eq!(path, "evidence/absent.json"),
			other => panic!("expected NoSuchFile naming the file, got {other:?}"),
		}
		Ok(())
	}
}

// endregion: --- Tests
