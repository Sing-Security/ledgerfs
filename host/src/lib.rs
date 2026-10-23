//! Host-side (Linux userspace) reader for LedgerFS volumes.
//!
//! # Why this exists
//!
//! `ledgerfs` owns the on-disk format, but its read paths are
//! ring-0 (`reader.rs`, over ATA PIO port I/O) or firmware (an allocation-free
//! boot reader). Nothing could open a
//! LedgerFS partition from a host process. This crate is that path: point it at
//! an image and a partition, and walk the filesystem.
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
//! - **A partition that has only been initialised has nothing to mount.** An
//!   initialiser lays down four superblock copies with `active_root_id = 0` and
//!   `active_commit_seq = 0` — no root, because nothing has committed yet — so
//!   `mount_select` finds no candidate. That state has its own error,
//!   [`Error::VolumeUncommitted`], so it reads as "initialised but never
//!   formatted" rather than as damage. A partition becomes mountable once
//!   something gives it a root, either by patching one into the superblock
//!   copies or by committing one. [`Store::format`] does the latter: it writes
//!   a *complete* set of four copies plus a committed root.
//!
//! # An auditor's note: the superblock copies can disagree
//!
//! The format keeps four superblock copies, and `mount_select` picks the
//! highest `commit_seq` among them, so a volume mounts correctly even when the
//! copies are not in step. Nothing in the format *forces* them to stay in step,
//! though: a writer that updates only the primary copy leaves the other three
//! at whatever they last held, and damage to the primary then rolls the volume
//! back to that older state rather than falling through to a surviving copy —
//! for a volume that has only been initialised, a null root, so it reads back
//! empty.
//!
//! This is reported rather than repaired: [`Volume::copy_status`] returns the
//! per-copy state and `lfs-read info` prints it. A host-side reader that
//! silently averaged over the disagreement would be hiding the one thing an
//! audit of this filesystem most wants to see.
//!
//! [`Store::commit`] persists all four copies on every commit, so a volume this
//! crate has written reports `4/4 decoded, all agree` — `lfs-write info` prints
//! exactly that after a mutation.
//!
//! # Where a volume lives
//!
//! [`BlockDevice`] is the seam that makes a volume's *location* the caller's
//! business rather than the filesystem's. A volume is a flat array of 4 KiB
//! blocks; whether that array is a partition on a real disk, a disk image, a
//! bare file, or a `Vec` in the caller's heap changes nothing above the device.
//! [`FileBlockDevice`] covers the first three — it opens a region at a byte
//! offset, and a whole-file volume is simply `base_offset = 0` — and
//! [`MemBlockDevice`] covers the fourth, including handing the buffer back as a
//! valid volume file once the store is done with it. A partition is the expected
//! deployment; the other three exist so the same code can be tested, embedded,
//! and carried around.
//!
//! # The application-store contract
//!
//! [`Store`] is not only `mkfs` for a volume. It is also the
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
//! - `image`: MBR parsing — which partitions an image has.
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
mod staging;
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
pub use staging::Staging;
pub use store::{Origin, STORE_APP_FILE, STORE_CONTRACT_VERSION, STORE_DIR, STORE_VERSION_FILE, Store};
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
pub use ledgerfs::BLOCK_SIZE;
pub use ledgerfs::commit::CommitResult;
pub use ledgerfs::cow::TxnPlan;
pub use ledgerfs::object::{Chunk, DirEntry, DirNode, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef};
pub use ledgerfs::record::ObjectRecordHeader;

// endregion: --- Modules
