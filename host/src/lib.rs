//! The `std` host of LedgerFS: a volume formatted, opened, modified and read
//! inside the calling process.
//!
//! `ledgerfs-host` is the crate an application links. It supplies the
//! `ObjectStore` implementation that the format crate leaves abstract, a
//! [`BlockDevice`] seam over files and memory, the mount, path resolution and
//! [`Store`], whose methods are the file operations. Nothing here re-derives
//! an offset or a checksum: every commit is performed by the kernel crate's
//! `CommitEngine` against the same format the specification describes.
//!
//! A volume can serve as an application's storage layer. It holds records and
//! returns them, and for a capture workload that is the whole requirement.
//! It is not a database engine: the crate defines no schema, no query
//! language and no secondary index, and it does not update stored bytes in
//! place. Those are layers above this one, and this release does not include
//! them. The properties it does provide are the ones that follow from the
//! format: the store is one file that copies and ships as one artifact,
//! nothing runs beside it, every object is content-addressed and verified on
//! the way out, and every write is atomic. The cost of that guarantee falls on
//! the smallest durable writes, because a commit issues two barriers and that
//! ordering is the crash argument; a staging session amortizes the commit over
//! a stream. The repository's
//! [`README.md`](https://github.com/Sing-Security/ledgerfs/blob/main/README.md)
//! states the position in full, and `TERMINOLOGY.md` resolves any term that
//! carries more than one meaning in this field.
//!
//! # The device seam
//!
//! [`BlockDevice`] is the seam that makes a volume's location the caller's
//! business rather than the filesystem's. A volume is a flat array of 4 KiB
//! blocks; whether that array is a partition on a real disk, a disk image, a
//! bare file, or a `Vec` in the caller's heap changes nothing above the device.
//! [`FileBlockDevice`] covers the first three: it opens a region at a byte
//! offset, and a whole-file volume is `base_offset = 0`. [`MemBlockDevice`]
//! covers the fourth, including returning the buffer as a valid volume file
//! once the store is finished with it. A partition is the expected deployment;
//! the other three exist so that the same code can be tested, embedded and
//! carried.
//!
//! `examples/custom_device.rs` implements the trait in about twenty lines. The
//! four required methods are the whole contract, and the filesystem runs on
//! top of any implementation unchanged.
//!
//! # The write path
//!
//! [`Store::format`] is `mkfs`: it lays down the four superblock copies and
//! commits a genesis root. After that, each verb ([`mkdir`](Store::mkdir),
//! [`create_file`](Store::create_file),
//! [`write_new_file`](Store::write_new_file), [`append`](Store::append),
//! [`write_file`](Store::write_file), [`write_at`](Store::write_at),
//! [`remove`](Store::remove), [`rename`](Store::rename)) is one transaction
//! committed through the kernel crate's real `CommitEngine`. Nothing here
//! re-derives an offset or a checksum; the format layer is used as-is.
//!
//! One commit per verb suits an application store and does not suit a capture
//! loop. For a stream, the crate provides a **staging session**:
//!
//! ```
//! # use ledgerfs_host::{MemBlockDevice, Result, Store};
//! # fn main() -> Result<()> {
//! let mut store = Store::format_app(Box::new(MemBlockDevice::new(512)), "example")?;
//! store.write_new_file("/log.jsonl", b"first\n", 0o644)?;
//!
//! store.begin_staging(1024 * 1024)?;
//! for turn in 2..=100 {
//!     store.staged_append("/log.jsonl", format!("turn {turn}\n").as_bytes(), 0o644)?;
//! }
//! // The owning handle can read its own staged writes...
//! assert!(store.read_file("/log.jsonl")?.len() > 5);
//! // ...or ask for just what is durable. The choice is per read.
//! assert_eq!(store.read_file_committed("/log.jsonl")?, b"first\n");
//!
//! let _seal = store.seal_staged()?; // one transaction, one commit, one commit_seq step
//! # Ok(())
//! # }
//! ```
//!
//! Staged writes touch no device block, no allocator state and no barrier.
//! The **seal** is the durability point, and it is an ordinary commit: after
//! it returns, the data carries the guarantee every other write carries, and a
//! plain mount recovers it with no additional recovery path. A crash before
//! the seal loses the staged work and nothing else. The application therefore
//! chooses when the point of truth moves, and each read chooses which side of
//! it to answer from.
//! [`TERMINOLOGY.md`](https://github.com/Sing-Security/ledgerfs/blob/main/TERMINOLOGY.md)
//! §3 defines the terms;
//! [`EMBEDDING.md`](https://github.com/Sing-Security/ledgerfs/blob/main/EMBEDDING.md)
//! §2 and §3 state the API contract;
//! [`BENCHMARKS.md`](https://github.com/Sing-Security/ledgerfs/blob/main/BENCHMARKS.md)
//! reports the cost of each interval.
//!
//! # The embedding contract
//!
//! [`Store`] is also the surface an application programs against when it
//! requires a file tree inside its own process; [`Store::format_app`] marks a
//! volume as an application store. The rules that accompany the methods (one
//! writer per volume, the meaning of a commit, why free space only falls, how
//! to recognise a volume that belongs to the caller) are stated in
//! `EMBEDDING.md`, and `examples/app_store.rs` expresses them as running
//! code.
//!
//! # Boundaries
//!
//! - **`data-encryption` is not enabled.** `Superblock::decode` refuses a
//!   volume whose `features_incompat` names `FEATURE_INCOMPAT_DATA_ENCRYPTED`
//!   when the cipher is not compiled in, so an encrypted volume fails the
//!   mount here with a diagnostic rather than decoding ciphertext as data.
//!   That refusal is the intended behaviour.
//! - **A partition that has only been initialised has nothing to mount.** An
//!   initialiser lays down four superblock copies with `active_root_id = 0` and
//!   `active_commit_seq = 0`, no root, because nothing has committed yet, so
//!   `mount_select` finds no candidate. That state has its own error,
//!   [`Error::VolumeUncommitted`], so it reads as "initialised but never
//!   formatted" rather than as damage. [`Store::format`] writes a *complete*
//!   set of four copies plus a committed root, which is the state a mount
//!   needs.
//!
//! # Audit consideration: superblock copies may disagree
//!
//! The format keeps four superblock copies, and `mount_select` picks the
//! highest `commit_seq` among them, so a volume mounts correctly even when the
//! copies are not in step. Nothing in the format *forces* them to stay in step,
//! though: a writer that updates only the primary copy leaves the other three
//! at whatever they last held, and damage to the primary then rolls the volume
//! back to that older state rather than falling through to a surviving copy.
//! For a volume that has only been initialised, that older state is a null
//! root, so it reads back empty.
//!
//! The disagreement is reported rather than repaired: [`Volume::copy_status`]
//! returns the per-copy state and `lfs-read info` prints it. A reader that
//! silently averaged over the copies would conceal the observation an audit of
//! this format is intended to make.
//!
//! [`Store::commit`] persists all four copies on every commit, so a volume this
//! crate has written reports `4/4 decoded, all agree`, and `lfs-write info` prints
//! exactly that after a mutation.
//!
//! # Crate structure
//!
//! Modules are private and named here as plain text, since nothing outside the
//! crate can link to them; the types they export are the public surface.
//!
//! - `device`: [`BlockDevice`], plus a file-backed and a memory-backed impl.
//! - `image`: MBR parsing, which partitions an image has.
//! - `volume`: superblock copies + `mount_select`.
//! - `objstore`: [`DiskObjectStore`], the disk-backed `ObjectStore`.
//! - `path`: [`HostFs`], the mount-and-walk facade.
//! - `store`: [`Store`], the format, mount and commit protocol.
//! - `mutate`: the ancestor cascade a nested mutation needs. `cow::operations`
//!   is deliberately leaf-level and its output would re-root the volume one
//!   level deep; this module supplies the missing half.
//! - `error`: [`Error`], one hand-written `Display` arm per variant.

#![deny(missing_docs)]

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
// caller implementing the trait to put a volume somewhere unusual (or reading
// the sidecar a file-backed one maintains) has to be able to name them.
pub use device::{BlockDevice, FileBlockDevice, HintCache, MemBlockDevice, VolumeIdentity};
pub use error::{Error, Result};
pub use image::{Partition, PartitionKind, find_partition, read_partitions};
pub use objstore::DiskObjectStore;
// `components_of` is exported because validating a path is a thing a caller
// needs to do *before* it has a store to hand it to: an application taking a
// name from a user, or building one out of a request, wants to know that the
// path is expressible in this format (no `..`, no components the reader would
// drop) while it can still say no with its own error instead of a
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
// Every type that appears in a *public signature* here belongs in this list.
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
