//! The on-disk format of LedgerFS and the copy-on-write operations that
//! produce it.
//!
//! This crate is the `no_std` half of the system. It defines every byte a
//! volume contains (superblock, object framing, the object model, the
//! allocator, the commit protocol) and the operations that turn paths and
//! bytes into new generations of those objects. It performs **no I/O of its
//! own**: reads and writes go through the [`cow::ObjectStore`] trait, which
//! the host crate implements over a `BlockDevice` and tests implement over a
//! `Vec<u8>`. The same format code therefore executes in a kernel, in a
//! userspace process and in a test harness.
//!
//! [`ledgerfs-host`](https://crates.io/crates/ledgerfs-host) is the crate to
//! select for an application that opens a volume and writes files to it; it
//! supplies the `ObjectStore` implementation, the mount and the `Store`
//! methods. This crate is for a consumer that already owns a byte-addressed
//! store, or that is implementing the filesystem from the bottom: a boot
//! loader, an operating system, or a tool that inspects a volume.
//!
//! # The format in brief
//!
//! - **Every stored thing is an object.** An immutable blob is named by the
//!   BLAKE3 hash of its canonical encoding: a [`object::Chunk`] for content,
//!   an [`object::Inode`] for file metadata, a [`object::DirNode`] for a
//!   directory, an [`object::ExtentNode`] for a file's extent list, and the
//!   [`object::Root`] that names the tree. Identical content has one
//!   identifier, and a reader verifies an object against its name before
//!   returning it.
//! - **No object is modified in place.** An operation writes new generations
//!   of every object on the path and returns one [`cow::TxnPlan`] naming them
//!   and the new root. [`commit::CommitEngine`] places those objects and
//!   writes them in a fixed order: objects and the allocator snapshot, a
//!   barrier, the four superblock copies naming the new root, a barrier. That
//!   order is the crash-consistency argument (`FORMAT.md` §15): a crash leaves
//!   the previous root live or the next one, never a blend.
//! - **The superblock names the root.** [`superblock::mount_select`] selects
//!   the highest verifying `commit_seq` among the four copies, uses the Root
//!   Locator Ring when the newest copy is damaged, and reports disagreement
//!   among the copies rather than choosing between roots without evidence.
//! - **The allocator is persisted beside the tree, not inside it.** Its
//!   snapshot occupies a fixed reserved block, because storing it through the
//!   allocator would consume a block and change the state being recorded (see
//!   [`ALLOC_SNAPSHOT_BLOCK`]).
//!
//! [`FORMAT.md`](https://github.com/Sing-Security/ledgerfs/blob/main/FORMAT.md)
//! is the normative description of these bytes, and this crate cites it by
//! section;
//! [`TERMINOLOGY.md`](https://github.com/Sing-Security/ledgerfs/blob/main/TERMINOLOGY.md)
//! defines the vocabulary. Where this documentation and the format document
//! disagree, the code is correct and the document contains a defect.
//!
//! # What is here, and what is not
//!
//! The format defines more than the shipping host path drives. The subsystems
//! that `FORMAT.md` §44 designates Tier B are not available to a consumer.
//! Deduplication at write time, snapshots, the scrubber, garbage-collection
//! execution and content-defined chunking are implemented and tested in the
//! separate, unpublished `ledgerfs-pending` crate. Replication, the audit
//! chain and record sealing are present in this crate, and no shipping path
//! calls them. `FORMAT.md` §44.1 and `KNOWN_ISSUES.md` §11 state the boundary
//! and its reasons.
//!
//! # Modules
//!
//! | | |
//! |---|---|
//! | [`encoding`] | canonical little-endian encoding, BLAKE3 hashing, strict decode rejection |
//! | [`record`] | `ObjectRecord` framing: magic, length, checksum, verification |
//! | [`object`] | the object model: [`ObjectId`](object::ObjectId), [`Chunk`](object::Chunk), [`Inode`](object::Inode), [`DirNode`](object::DirNode), [`ExtentNode`](object::ExtentNode), [`Root`](object::Root) |
//! | [`superblock`] | the 4 KiB superblock, its four copies, the Root Locator Ring, mount selection |
//! | [`intent_log`] | the ring buffer of commit-intent records and the replay decision |
//! | [`alloc_state`], [`alloc_node`], [`allocator`] | the allocation tree, block states, and the allocator with its emergency reserves |
//! | [`placement`] | turns a [`cow::TxnPlan`] into placed `ObjectRecord`s |
//! | [`commit`] | [`CommitEngine`](commit::CommitEngine): the ordered write of objects → allocator snapshot → superblocks |
//! | [`cow`] | the operations (read, readdir, mkdir, create, write, unlink, rename) and the [`ObjectStore`](cow::ObjectStore) trait they are generic over |
//! | [`compression`], [`lz4_block`] | the per-chunk compression policies |
//! | [`mode`] | [`FsMode`]: read-write versus the write-embargoed read-only-safe mode |
//! | [`keyslot`] | passphrase-derived key slots, sealed with the volume's key material |
//! | `data_crypto` | transparent XTS-AES-256 over raw blocks, with the `data-encryption` feature |
//!
//! The ATA PIO path (`reader`, `writer`) and the modules under it are compiled
//! only with the `reader` feature on x86-64; they are ring-0 code and do not
//! belong in a userspace binary.
//!
//! # Features
//!
//! | feature | effect |
//! |---|---|
//! | *(none)* | the format and the copy-on-write operations, `no_std` + `alloc` |
//! | `std` | pulls the `lz4` and `zstd` decoders that [`compression`] gates, so a record written under either policy decodes |
//! | `reader` | compiles the ATA PIO reader, writer and boot self-test. x86-64 only, off by default |
//! | `data-encryption` | transparent XTS-AES-256 over a volume's raw blocks, applied below the object model and chosen at format time |
//! | `temper_sealing` | ML-DSA-65 and SLH-DSA sealing of audit records, via the `temper-rs` crate |

#![deny(missing_docs)]
#![no_std]

extern crate alloc;

// region:    --- Modules

pub mod alloc_node;
pub mod alloc_state;
pub mod allocator;
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub(crate) mod arch;
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub(crate) mod ata_io;
pub mod audit;
pub mod commit;
pub mod compression;
pub mod cow;
#[cfg(feature = "data-encryption")]
pub mod data_crypto;
pub mod encoding;
pub mod error;
pub mod intent_log;
pub mod keyslot;
pub mod lz4_block;
pub mod mode;
pub mod nt_write;
pub mod object;
pub mod placement;
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub mod reader;
pub mod record;
pub mod replication;
pub mod seal_record;
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub mod selftest;
pub mod superblock;
#[cfg(all(feature = "reader", target_arch = "x86_64"))]
pub mod writer;

pub use error::{Error, Result};
pub use mode::FsMode;

// endregion: --- Modules

// region:    --- Constants

/// Magic number for ObjectRecord (8 bytes fitting in u64)
pub const OBJECT_RECORD_MAGIC: u64 = 0x004C_4547_4552_4653; // "LEGERFS"

/// Magic number for Superblock (8 bytes fitting in u64)
pub const SUPERBLOCK_MAGIC: u64 = 0x5355504552424C4B; // "SUPERBLK"

/// Superblock format version this build writes and accepts (§43.1).
///
/// This is the on-disk compatibility gate. `Superblock::decode` refuses a
/// copy that declares any other value, so a volume written by a newer format
/// is refused rather than decoded optimistically. A change that alters the
/// meaning or layout of any persisted field bumps this constant; additive
/// changes that an older reader can ignore belong in the feature flags
/// instead (§43.2).
pub const SUPERBLOCK_VERSION: u32 = 1;

/// Magic number for Intent Log Record (8 bytes fitting in u64)
pub const INTENT_LOG_MAGIC: u64 = 0x494E54454E544C47; // "INTENTLG"

/// Minimum record length for intent log (64 bytes, 8-byte aligned)
pub const MIN_RECORD_LEN: u32 = 64;

/// Block size (4 KiB)
pub const BLOCK_SIZE: u32 = 4096;

/// Number of superblock copies
pub const NUM_SUPERBLOCK_COPIES: usize = 4;

/// Number of blocks at the **start** of a partition reserved for
/// non-object metadata: superblock copies (blocks 0-1), intent log
/// (blocks 2-3), genesis audit record (block 4), plus one block of slack
/// (block 5). No `ObjectRecord` is ever placed here. `reader::START_BLOCK`
/// (the boot-time object scan's starting point) and any `AllocNode`
/// initialization that marks a fresh volume's free space (e.g. the runtime
/// volume driver's volume bootstrap) must both respect this, or the
/// placement engine can allocate a real object on top of the superblock
/// itself. Found live: a genesis commit placed its root directory's
/// `Inode` at block 0, which a later superblock-persist call then
/// silently overwrote, permanently orphaning the object.
pub const RESERVED_HEADER_BLOCKS: u64 = 6;

/// Number of blocks at the **end** of a partition reserved for the two
/// trailing superblock backup copies (`total_blocks - 2`, `total_blocks -
/// 1`; see `write_superblocks`' `copy_lbas`). Same rationale as
/// [`RESERVED_HEADER_BLOCKS`].
pub const RESERVED_TRAILER_BLOCKS: u64 = 2;

/// Block index (within a partition) of the **allocator snapshot**: the
/// serialized `AllocNode` describing which blocks are free, allocated, or
/// reserved.
///
/// This is the "one block of slack" already set aside by
/// [`RESERVED_HEADER_BLOCKS`], so using it needs no on-disk layout change
/// and no rebuild of existing images: a volume that has never written a
/// snapshot simply reads zeros here, fails the magic check, and resumes
/// read-only exactly as before.
///
/// # Why the allocator has to be persisted at all
///
/// Nothing else on disk records which blocks are in use. The object graph
/// reachable from the root implies it, but reconstructing that requires
/// walking the whole partition. Without a snapshot, a volume that already
/// has content can only be resumed **read-only**, which meant the data
/// volume was writable exactly once per disk image, on the boot that
/// formatted it.
///
/// # Why a fixed block rather than a normal object
///
/// The allocator's own state cannot be stored through the allocator: doing
/// so consumes a block, which changes the state being written. A fixed,
/// pre-reserved location breaks that cycle, the same way the superblock's
/// does.
pub const ALLOC_SNAPSHOT_BLOCK: u64 = 5;

/// Largest object the reader will accept, in bytes.
///
/// Load-bearing on the **write** side too: `reader::read_object`'s scan
/// silently skips any record whose `stored_len` exceeds this, so an object
/// committed above the limit reads back as *not found*: a successful write
/// followed by vanished data. `CommitEngine` therefore refuses to place one
/// (see `CommitEngine::set_max_object_bytes`). A volume may lower this via
/// its superblock `LedgerFsLimits`; this is the compiled-in default.
pub const MAX_OBJECT_SIZE: usize = 256 * 1024;

/// Magic at the head of the [`ALLOC_SNAPSHOT_BLOCK`], "ALLOCSNP".
pub const ALLOC_SNAPSHOT_MAGIC: u64 = 0x414C4C4F43534E50;

/// Number of RLR entries in superblock
///
/// Sizing: 56 entries × 60 bytes = 3 360 bytes, ending at byte 3 440.
/// The block is laid out 80 (header) + 3 360 (RLR) + 24 (feature flags) = 3 464
/// bytes, after which come fixed-offset extensions: `ext_rlr_lba` and
/// `ext_rlr_block_count` at 3 464, a human-readable label at 3 476, the
/// `LedgerFsLimits` block at 3 736, `active_root_hint` at 3 764, the keyslots,
/// and the checksum in the last 8 bytes. See `superblock` for the full table;
/// the slack this constant's sizing leaves is not contiguous.
pub const RLR_SIZE: usize = 56;

/// BLAKE3 hash size (32 bytes)
pub const BLAKE3_HASH_SIZE: usize = 32;

/// Checksum size (BLAKE3 truncated to 64 bits)
pub const CHECKSUM_SIZE: usize = 8;

// endregion: --- Constants
