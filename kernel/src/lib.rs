//! LedgerFS v7 Core On-Disk Substrate
//!
//! This crate implements the on-disk format and mount/replay core
//! for LedgerFS v7 as specified in FORMAT.md.
//!
//! # Architecture
//!
//! LedgerFS is a copy-on-write filesystem with:
//! - Content-addressed objects using BLAKE3
//! - Four-copy superblock with Root Locator Ring (RLR)
//! - Intent log for crash-consistent commit protocol
//! - Deterministic mount selection and replay
//!
//! # Modules
//!
//! - `encoding`: Canonical encoding with strict decode rejection
//! - `record`: ObjectRecord framing with BLAKE3 verification
//! - `superblock`: Superblock format with RLR and mount selection
//! - `intent_log`: Intent log with O(N) ring parsing
//! - `mode`: Filesystem mode (ReadWrite vs ReadOnlySafe)
//! - `alloc_state`: Allocation state tracking (Free/Allocated/PendingFree/Reserved)
//! - `alloc_node`: Allocation tree B-tree nodes
//! - `allocator`: Block allocator with emergency reserves
//! - `placement`: Placement engine turning TxnPlan into placed ObjectRecords
//! - `object`: Object model (Chunk, Inode, DirNode, ExtentNode, Root)
//! - `cow`: Copy-on-write operations and transaction planning

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
/// (block 5). No `ObjectRecord` is ever placed here — `reader::START_BLOCK`
/// (the boot-time object scan's starting point) and any `AllocNode`
/// initialization that marks a fresh volume's free space (e.g. the runtime
/// volume driver's volume bootstrap) must both respect this, or the
/// placement engine can allocate a real object on top of the superblock
/// itself — found live: a genesis commit placed its root directory's
/// `Inode` at block 0, which a later superblock-persist call then
/// silently overwrote, permanently orphaning the object.
pub const RESERVED_HEADER_BLOCKS: u64 = 6;

/// Number of blocks at the **end** of a partition reserved for the two
/// trailing superblock backup copies (`total_blocks - 2`, `total_blocks -
/// 1` — see `write_superblocks`' `copy_lbas`). Same rationale as
/// [`RESERVED_HEADER_BLOCKS`].
pub const RESERVED_TRAILER_BLOCKS: u64 = 2;

/// Block index (within a partition) of the **allocator snapshot** — the
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
/// has content can only be resumed **read-only** — which meant the data
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
/// committed above the limit reads back as *not found* — a successful write
/// followed by vanished data. `CommitEngine` therefore refuses to place one
/// (see `CommitEngine::set_max_object_bytes`). A volume may lower this via
/// its superblock `LedgerFsLimits`; this is the compiled-in default.
pub const MAX_OBJECT_SIZE: usize = 256 * 1024;

/// Magic at the head of the [`ALLOC_SNAPSHOT_BLOCK`] — "ALLOCSNP".
pub const ALLOC_SNAPSHOT_MAGIC: u64 = 0x414C4C4F43534E50;

/// Number of RLR entries in superblock
///
/// Sizing: 56 entries × 60 bytes = 3 360 bytes, ending at byte 3 440.
/// The block is laid out 80 (header) + 3 360 (RLR) + 24 (feature flags) = 3 464
/// bytes, after which come fixed-offset extensions — `ext_rlr_lba` and
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
