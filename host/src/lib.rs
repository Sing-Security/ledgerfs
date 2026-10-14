//! Host-side (Linux userspace) reader for LedgerFS volumes.
//!
//! # Why this exists
//!
//! `crucible-ledgerfs-kernel` owns the on-disk format, but every read path in
//! the tree is either ring-0 (`reader.rs`, over ATA PIO port I/O) or firmware
//! (`forge-boot`'s allocation-free reader). Nothing could open a LedgerFS
//! partition from a host process. This crate is that path: point it at an
//! image and a partition, and walk the filesystem.
//!
//! # What it is not
//!
//! It does not reimplement the filesystem. `cow::operations` — `read`,
//! `readdir`, `mkdir`, `write`, `unlink`, `rename` — is already generic over
//! `ObjectStore`, and this crate's whole contribution is one implementation of
//! that trait ([`DiskObjectStore`]) plus the mount and path plumbing around it.
//! The format layer (`superblock`, `record`, `object`) is used as-is; nothing
//! here re-derives an offset or a checksum.
//!
//! # What this does, and does not, do yet
//!
//! Both directions. [`Store`] mounts, reads, mutates (`mkdir`, `create_file`,
//! [`write_new_file`](Store::write_new_file)) and commits through the kernel
//! crate's real `CommitEngine`, and [`Store::format`] is `mkfs`: it lays down
//! the superblock copies and commits a genesis root. Two things worth stating
//! plainly:
//!
//! - **`data-encryption` is not enabled.** `Superblock::decode` refuses a
//!   volume whose `features_incompat` names `FEATURE_INCOMPAT_DATA_ENCRYPTED`
//!   when the cipher is not compiled in, so an encrypted volume fails the
//!   mount here with a clear message rather than decoding ciphertext as
//!   garbage. That refusal is the correct behaviour, not a gap to paper over.
//! - **A partition straight out of `fs_init` has nothing to mount.**
//!   `fs_init` writes `active_root_id = 0` and `active_commit_seq = 0` — no
//!   root, because nothing has committed yet — so `mount_select` finds no
//!   candidate. That state has its own error, [`Error::VolumeUncommitted`], so
//!   it reads as "initialised but never formatted" rather than as damage. A
//!   partition becomes mountable once something gives it a root: `populate.rs`
//!   patches one into all four of `/system`'s superblock copies, `fs-cbl`'s
//!   `MountedVolume::open_or_bootstrap` bootstraps a genesis root into `/data`
//!   on the image's first boot, and [`Store::format`] now writes a *complete*
//!   set of four copies plus a committed root. The first two are true of
//!   `artifacts-x86_64/crucible-install.img`, which is what this crate was
//!   verified against.
//!
//! # An auditor's note: `/data`'s superblock copies disagree
//!
//! On the image above, `/data`'s four superblock copies do not hold the same
//! state:
//!
//! | copy | block | `commit_seq` | `active_root_id` |
//! |------|-------|-------------:|------------------|
//! | 0 | `0` | 1 | `39a13a7b…`, an empty directory |
//! | 1 | `1` | 0 | null |
//! | 2 | `N-2` | 0 | null |
//! | 3 | `N-1` | 0 | null |
//!
//! The volume still mounts, and mounts correctly — `mount_select` takes the
//! highest `commit_seq`, so copy 0 wins. But the four-copy redundancy the
//! format is designed around is not being maintained at run time. The only
//! writer in the tree that updates a superblock after the image is built is
//! `fs-cbl/src/disk.rs`'s `persist_superblock`, and its own doc comment says it
//! writes the primary copy only. Copies 1–3 are therefore still what `fs_init`
//! left there, and the practical consequence is that damage to block 0 rolls
//! `/data` back to a null root — the volume reads back empty rather than
//! falling through to a surviving copy.
//!
//! This is reported rather than repaired: [`Volume::copy_status`] returns the
//! per-copy state and `lfs-read info` prints it. A host-side reader that
//! silently averaged over the disagreement would be hiding the one thing an
//! audit of this filesystem most wants to see.
//!
//! The host write path does not repeat that mistake — [`Store::commit`] persists
//! all four copies, which is why `lfs-write info` reports `4/4 decoded, all
//! agree` after a mutation. The observation above is a property of the shipped
//! image, and remains true of it.
//!
//! # Where a volume lives
//!
//! [`BlockDevice`] is what makes this crate embeddable rather than merely a
//! partition inspector. A volume is a flat array of 4 KiB blocks; whether that
//! array is a disk image, a bare file, or a `Vec` in the caller's heap is the
//! device's business, not the filesystem's. [`FileBlockDevice`] covers the
//! first two (a whole-file volume is `base_offset = 0`) and
//! [`MemBlockDevice`] covers the third, including handing the buffer back as
//! a valid volume file once the store is done with it.
//!
//! # The application-store contract
//!
//! [`Store`] is not only `mkfs` for a Crucible partition. It is also the
//! surface an ordinary application programs against when it wants a real
//! filesystem inside its own process — [`Store::format_app`] marks a volume as
//! an app's store, and the verbs [`append`](Store::append),
//! [`write_file`](Store::write_file), [`write_at`](Store::write_at),
//! [`remove`](Store::remove) and [`rename`](Store::rename) are the operations
//! such an app needs. The rules that go with them — one writer per volume, the
//! meaning of a commit, why free space only falls, how to recognise a volume
//! that belongs to you — are the contract, and they are written down in
//! `EMBEDDING.md` in the crate root. `examples/app_store.rs` is the contract
//! as running code.
//!
//! # Anatomy
//!
//! Modules are private and named here as plain text, since nothing outside the
//! crate can link to them; the types they export are the public surface.
//!
//! - `device`: [`BlockDevice`], plus a file-backed and a memory-backed impl.
//! - `image`: MBR parsing — which partitions a Crucible image has.
//! - `volume`: superblock copies + `mount_select`.
//! - `objstore`: [`DiskObjectStore`], the disk-backed `ObjectStore`.
//! - `path`: [`HostFs`], the mount-and-walk facade.
//! - `store`: [`Store`] — format, mount, and the commit protocol.
//! - `mutate`: the ancestor cascade a nested mutation needs. `cow::operations`
//!   is deliberately leaf-level and its output would re-root the volume one
//!   level deep; this module supplies the missing half.
//! - `error`: [`Error`], one hand-written `Display` arm per variant.

// region:    --- Modules

mod device;
mod error;
mod hints;
mod image;
mod mutate;
mod objstore;
mod path;
mod rebuild;
mod store;
mod volume;

// `HintCache`/`VolumeIdentity` are named in `BlockDevice::hint_cache`, so a
// caller implementing the trait to put a volume somewhere unusual — or reading
// the sidecar a file-backed one maintains — has to be able to name them.
pub use device::{BlockDevice, FileBlockDevice, HintCache, MemBlockDevice, VolumeIdentity};
pub use error::{Error, Result};
pub use image::{Partition, PartitionKind, find_partition, read_partitions};
pub use objstore::DiskObjectStore;
// `components_of` is exported because validating a path is a thing a caller
// needs to do *before* it has a store to hand it to: an application taking a
// name from a user, or building one out of a request, wants to know that the
// path is expressible in this format — no `..`, no components the reader would
// drop — while it can still say no with its own error instead of a
// `NoSuchFile` it then has to interpret. The store calls the same function, so
// a caller that checks first and a store that checks second cannot disagree.
pub use path::{Entry, HostFs, components_of};
pub use rebuild::{RebuildStats, WalkEntry, rebuild_into};
pub use store::{
	Origin, Store, STORE_APP_FILE, STORE_CONTRACT_VERSION, STORE_DIR, STORE_VERSION_FILE,
};
pub use volume::{CopyStatus, Volume};

// Re-exported so callers of this crate name the format types through it rather
// than needing a second direct dependency on the kernel crate.
//
// Every type that appears in a *public signature* here belongs in this list —
// `Store::commit` takes a `TxnPlan` and returns a `CommitResult`, and
// `DiskObjectStore::read_record`/`dir_node`/`extent_node`/`chunk` hand back
// `ObjectRecordHeader`/`DirNode`/`ExtentNode`/`Chunk`. Without the re-export an
// embedder cannot name its own arguments without adding the kernel as a direct
// dependency, which is exactly the coupling this crate exists to absorb.
pub use crucible_ledgerfs_kernel::commit::CommitResult;
pub use crucible_ledgerfs_kernel::cow::TxnPlan;
pub use crucible_ledgerfs_kernel::object::{
	Chunk, DirEntry, DirNode, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef,
};
pub use crucible_ledgerfs_kernel::record::ObjectRecordHeader;
pub use crucible_ledgerfs_kernel::BLOCK_SIZE;

// endregion: --- Modules
