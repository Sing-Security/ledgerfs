//! An embedded filesystem: a volume you format, write files to and read back,
//! inside your own process.
//!
//! `ledgerfs-host` is the crate an application depends on. It supplies the
//! piece the format crate deliberately leaves out — an object store over a
//! byte-addressed device — plus the mount, the path plumbing and [`Store`],
//! whose verbs *are* the filesystem API. You hand it a [`BlockDevice`], it
//! gives you files.
//!
//! You can use it as your application's database: it holds your records and
//! gives them back. What it is not is a database **engine** — no schema, no
//! query language, no indexes, no in-place update. It is a different solution
//! to the same problem, and the difference is what the storage being a
//! filesystem buys: the whole store is one file that copies and ships as one
//! artifact, nothing has to run beside it, every object is content-addressed
//! and verified on the way out, and every write is atomic. What it costs is
//! some speed on the smallest durable writes — a commit is two barriers, and
//! that order is the crash argument — which a staging session amortizes over a
//! stream. `README.md`, `TERMINOLOGY.md` and `BENCHMARKS.md` in the repository
//! say more, and the vocabulary is normative: `TERMINOLOGY.md` is the one to
//! read when a word seems to mean two things.
//!
//! # Holding a volume
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
//! `examples/custom_device.rs` implements the trait from scratch in about
//! twenty lines: the four required methods are the whole contract, and the
//! filesystem works on top of any of them unchanged.
//!
//! # The write path
//!
//! [`Store::format`] is `mkfs`: it lays down the four superblock copies and
//! commits a genesis root. After that, the verbs are the ordinary ones —
//! [`mkdir`](Store::mkdir), [`create_file`](Store::create_file),
//! [`write_new_file`](Store::write_new_file), [`append`](Store::append),
//! [`write_file`](Store::write_file), [`write_at`](Store::write_at),
//! [`remove`](Store::remove), [`rename`](Store::rename) — and each is one
//! transaction, committed through the kernel crate's real `CommitEngine`.
//! Nothing here re-derives an offset or a checksum; the format layer is used
//! as-is.
//!
//! One commit per verb is the right shape for an application store, and the
//! wrong shape for a capture loop. For that there is a **staging session**:
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
//! Staged writes touch no device block, no allocator state and no barrier. The
//! **seal** is the durability point, and it is an ordinary commit of the
//! ordinary shape: after it returns, the data has exactly the guarantee every
//! other write has, and a plain mount recovers it with no new recovery code. A
//! crash before the seal loses the staged work and nothing else. That is the
//! whole trade — the application chooses how often the point of truth moves,
//! and reads choose which side of it to answer from. `TERMINOLOGY.md` §3 names
//! the terms; `EMBEDDING.md` §5 is the API contract.
//!
//! # The application-store contract
//!
//! [`Store`] is not only `mkfs` for a volume. It is also the surface an
//! ordinary application programs against when it wants a real filesystem
//! inside its own process — [`Store::format_app`] marks a volume as an app's
//! store. The rules that go with the verbs — one writer per volume, the
//! meaning of a commit, why free space only falls, how to recognise a volume
//! that belongs to you — are the contract, and they are written down in
//! `EMBEDDING.md` in the crate root. `examples/app_store.rs` is the contract
//! as running code.
//!
//! # What this crate does not do
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
//!   formatted" rather than as damage. [`Store::format`] writes a *complete*
//!   set of four copies plus a committed root, which is the state a mount
//!   needs.
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
