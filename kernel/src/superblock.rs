//! Superblock format with RLR and mount selection
//!
//! Per §10 and §18 of FORMAT.md:
//! - Superblock is 4096 bytes (one block)
//! - Four copies: blocks 0, 1, N-1, N
//! - Root Locator Ring (RLR) with 56 entries
//! - Deterministic mount selection algorithm

use crate::{
	BLAKE3_HASH_SIZE, BLOCK_SIZE, Error, FsMode, NUM_SUPERBLOCK_COPIES, RLR_SIZE, Result, SUPERBLOCK_MAGIC,
	encoding::{blake3_checksum, read_u32, read_u64, verify_checksum, write_u32, write_u64},
	intent_log::ReplayDecision,
};
use alloc::vec::Vec;

/// RLR entry flags (§10.3)
pub const RLR_FLAG_NON_MONOTONIC: u32 = 0x01;

/// `features_incompat` bit: every object block on this volume is stored
/// XTS-AES-256 encrypted (see `crate::data_crypto`).
///
/// Deliberately an **incompat** flag rather than `compat`/`ro_compat`: a
/// reader that doesn't understand this bit cannot safely do *anything* with
/// the volume — not even read-only. Every block it fetched would decode as
/// garbage, and the object layer's `blake3(payload) == object_id` check
/// would reject it as corruption rather than as "encrypted, and I lack the
/// key" — a confusing and actively misleading failure. Refusing the mount
/// outright on an unrecognized incompat bit (standard ext4/btrfs-style
/// feature-flag semantics) is the honest behavior.
///
/// Set once at genesis-format time and never changed afterward: there is no
/// in-place convert-existing-volume path (that would mean rewriting every
/// block on the disk with no crash-consistency story, well beyond what this
/// feature attempts). Encrypting an existing plaintext volume means
/// reformatting it.
pub const FEATURE_INCOMPAT_DATA_ENCRYPTED: u64 = 0x0000_0000_0000_0001;

/// Root slot identifier (A or B)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSlot {
	SlotA,
	SlotB,
}

/// RLR Entry (Root Locator Ring entry) (§10.1)
#[derive(Debug, Clone)]
pub struct RlrEntry {
	pub root_id: [u8; BLAKE3_HASH_SIZE],
	pub commit_seq: u64,
	pub timestamp: u64,
	pub flags: u32,
	pub entry_checksum: u64,
}

impl RlrEntry {
	/// Entry size in bytes
	pub const SIZE: usize = BLAKE3_HASH_SIZE + 8 + 8 + 4 + 8; // 60 bytes

	/// Create a new RLR entry
	pub fn new(root_id: [u8; BLAKE3_HASH_SIZE], commit_seq: u64, timestamp: u64, flags: u32) -> Self {
		let mut entry = Self {
			root_id,
			commit_seq,
			timestamp,
			flags,
			entry_checksum: 0,
		};
		entry.compute_checksum();
		entry
	}

	/// Compute and update entry checksum
	pub fn compute_checksum(&mut self) {
		let mut data = Vec::new();
		data.extend_from_slice(&self.root_id);
		data.extend_from_slice(&self.commit_seq.to_le_bytes());
		data.extend_from_slice(&self.timestamp.to_le_bytes());
		data.extend_from_slice(&self.flags.to_le_bytes());

		self.entry_checksum = blake3_checksum(&data);
	}

	/// Verify entry checksum
	pub fn verify(&self) -> Result<()> {
		let mut data = Vec::new();
		data.extend_from_slice(&self.root_id);
		data.extend_from_slice(&self.commit_seq.to_le_bytes());
		data.extend_from_slice(&self.timestamp.to_le_bytes());
		data.extend_from_slice(&self.flags.to_le_bytes());

		verify_checksum(&data, self.entry_checksum)
	}

	/// Decode from buffer
	pub fn decode(buf: &[u8]) -> Result<Self> {
		if buf.len() < Self::SIZE {
			return Err(Error::BufferTooSmall {
				required: Self::SIZE,
				available: buf.len(),
			});
		}

		let mut root_id = [0u8; BLAKE3_HASH_SIZE];
		root_id.copy_from_slice(&buf[0..BLAKE3_HASH_SIZE]);

		let commit_seq = read_u64(&buf[BLAKE3_HASH_SIZE..])?;
		let timestamp = read_u64(&buf[BLAKE3_HASH_SIZE + 8..])?;
		let flags = read_u32(&buf[BLAKE3_HASH_SIZE + 16..])?;
		let entry_checksum = read_u64(&buf[BLAKE3_HASH_SIZE + 20..])?;

		let entry = Self {
			root_id,
			commit_seq,
			timestamp,
			flags,
			entry_checksum,
		};

		// Skip verification for empty entries (commit_seq == 0)
		if commit_seq > 0 {
			entry.verify()?;
		}

		Ok(entry)
	}

	/// Encode to buffer
	pub fn encode(&self, buf: &mut [u8]) -> Result<()> {
		if buf.len() < Self::SIZE {
			return Err(Error::BufferTooSmall {
				required: Self::SIZE,
				available: buf.len(),
			});
		}

		buf[0..BLAKE3_HASH_SIZE].copy_from_slice(&self.root_id);
		write_u64(&mut buf[BLAKE3_HASH_SIZE..], self.commit_seq)?;
		write_u64(&mut buf[BLAKE3_HASH_SIZE + 8..], self.timestamp)?;
		write_u32(&mut buf[BLAKE3_HASH_SIZE + 16..], self.flags)?;
		write_u64(&mut buf[BLAKE3_HASH_SIZE + 20..], self.entry_checksum)?;

		Ok(())
	}
}

/// Fixed byte offset of `ext_rlr_lba` within the 4 KiB superblock block.
///
/// Layout: 80 (fixed header) + 3 360 (56 RLR entries × 60 bytes) + 24 (feature flags)
///         = 3 464 bytes from block start.
const EXT_RLR_LBA_OFFSET: usize = 3464;

/// Fixed byte offset of `ext_rlr_block_count` within the 4 KiB superblock block.
///
/// Immediately follows `ext_rlr_lba` (u64, 8 bytes):
/// EXT_RLR_LBA_OFFSET + 8 = 3 472 bytes from block start.
const EXT_RLR_BLOCK_COUNT_OFFSET: usize = 3472;

/// Fixed byte offset of the [`LedgerFsLimits`] block within the 4 KiB
/// superblock block.
///
/// **Not** immediately after `ext_rlr_block_count` (3476) — that byte range
/// is claimed by a human-readable label string (`LABEL_OFFSET = 3476`, a
/// 4-byte length prefix + up to 256 bytes of text, ending at 3736) that is
/// written out-of-band and does not go through `Superblock::encode` at all.
/// It is only free *after* the label region. Confirmed the hard way: an
/// earlier version of this offset at 3476 collided with the label and got
/// overwritten by it, caught by a test asserting that limits written into
/// the superblock at build time are read back verbatim.
///
/// Note that the label is **not** preserved by [`Superblock::encode`], which
/// zeroes the block and re-emits only the fields on this struct — so a commit
/// erases it. Recorded in `KNOWN_ISSUES.md` §9.
///
/// `pub` so callers that read superblock bytes directly instead of going
/// through `Superblock::decode` (e.g. an allocation-free boot parser) can
/// locate this field without duplicating the offset.
pub const LIMITS_OFFSET: usize = 3736;

/// Fixed byte offset of `active_root_hint` within the 4 KiB superblock
/// block — immediately after [`LIMITS_OFFSET`]'s 28-byte block.
///
/// `0` means "absent": the root object's physical block is unknown and a
/// reader must fall back to a full linear scan to find it. Every existing
/// volume decodes as `0` here, so no migration is needed — the only cost
/// is that the *first* root lookup after mounting such a volume is a scan
/// rather than a single hinted block read.
pub const ACTIVE_ROOT_HINT_OFFSET: usize = 3764;

/// Configurable operating limits for the boot and kernel LedgerFS readers.
///
/// These are the subset of "TCB size"-class limits that are safe to make
/// runtime-configurable from an on-disk value: each one only gates a simple
/// `if actual > limit` comparison or a scan-loop upper bound, never the
/// compile-time capacity of a fixed-size stack buffer. (A handful of
/// closely-related limits — a boot reader's max extent count, max
/// directory-entry count, max name length, and max path depth — are
/// deliberately **not** here: they size `heapless::Vec`/`heapless::String`
/// buffers via const generics, which is exactly what lets boot code guarantee
/// zero allocation and fail closed on a
/// too-large object instead of risking an unbounded read. Trusting an
/// on-disk value for those would mean either allocating at boot — the thing
/// this design deliberately avoids — or silently truncating without the
/// caller knowing the buffer was undersized for whatever the disk claims its
/// own limit to be. Those four instead have their own build-time mechanism,
/// wired into the boot reader's build configuration.)
///
/// A value of `0` means "absent" — the reader falls back to its compiled-in
/// default, exactly like `ext_rlr_lba`/`ext_rlr_block_count` above. This
/// keeps existing volumes (which have this whole region zeroed) decoding
/// with identical behavior to before this field existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerFsLimits {
	/// Kernel reader: max size of one `ObjectRecord` (was `reader.rs`'s
	/// hardcoded `MAX_OBJECT_SIZE`, default 256 KiB).
	pub max_object_bytes: u32,
	/// Kernel reader: max total bytes returned by one ranged read (was
	/// `reader.rs`'s hardcoded `MAX_RANGE_READ`, default 8 MiB).
	pub max_range_read_bytes: u32,
	/// Kernel reader: max blocks scanned looking for an object (was
	/// `reader.rs`'s hardcoded `MAX_BLOCKS`, default 16384).
	pub max_scan_blocks: u64,
	/// Boot reader: max size of one file read at boot (was a boot reader's
	/// hardcoded `MAX_BOOT_FILE_SIZE`, default 1 MiB) — this is the cap that
	/// actually governs "how big can a TCB binary be".
	pub max_boot_file_bytes: u32,
	/// Boot reader: max blocks scanned looking for an object at boot (was a
	/// boot reader's hardcoded `MAX_BLOCKS`, default 131072).
	pub max_boot_scan_blocks: u64,
}

impl LedgerFsLimits {
	/// Byte size of the encoded form. The region from `LIMITS_OFFSET` to the
	/// keyslots is reserved so fields can be appended to this struct without
	/// moving the offset.
	pub const SIZE: usize = 4 + 4 + 8 + 4 + 8;

	/// Decode from a `Self::SIZE`-byte buffer.
	///
	/// `pub` so external callers that only have raw superblock bytes and
	/// don't want to pull in the rest of `Superblock::decode`'s stricter
	/// checksum-verified path (e.g. a minimal, allocation-free boot
	/// `verify_superblock`) can still decode this one field
	/// consistently with how it's written, rather than re-implementing the
	/// byte layout.
	pub fn decode(buf: &[u8]) -> Result<Self> {
		Ok(Self {
			max_object_bytes: read_u32(&buf[0..])?,
			max_range_read_bytes: read_u32(&buf[4..])?,
			max_scan_blocks: read_u64(&buf[8..])?,
			max_boot_file_bytes: read_u32(&buf[16..])?,
			max_boot_scan_blocks: read_u64(&buf[20..])?,
		})
	}

	/// Encode into a `Self::SIZE`-byte buffer. See [`Self::decode`] for why
	/// this is `pub`.
	pub fn encode(&self, buf: &mut [u8]) -> Result<()> {
		write_u32(&mut buf[0..], self.max_object_bytes)?;
		write_u32(&mut buf[4..], self.max_range_read_bytes)?;
		write_u64(&mut buf[8..], self.max_scan_blocks)?;
		write_u32(&mut buf[16..], self.max_boot_file_bytes)?;
		write_u64(&mut buf[20..], self.max_boot_scan_blocks)?;
		Ok(())
	}
}

/// Superblock structure (§10.1)
///
/// On-disk layout (4096 bytes = one block):
/// - Bytes    0–79:   Fixed header (magic, version, block_size, total_blocks,
///   active_root_id, active_commit_seq, active_checksum,
///   rlr_head, rlr_tail)
/// - Bytes   80–3439: RLR entries (56 × 60 bytes = 3 360 bytes)
/// - Bytes 3440–3463: Feature flags (features_compat, features_ro_compat,
///   features_incompat — 3 × 8 bytes = 24 bytes)
/// - Bytes 3464–3471: ext_rlr_lba       (u64, fixed offset)
/// - Bytes 3472–3475: ext_rlr_block_count (u32, fixed offset)
/// - Bytes 3476–3735: Human-readable label (4-byte length prefix + up to 256
///   bytes of text), written out-of-band by whichever tool formats the volume
///   and **not** encoded by `Superblock::encode` itself. This struct has no
///   field for it, so a re-encode clears it: treat 3476..3736 as claimed on
///   read. See `KNOWN_ISSUES.md` §9 for the consequence on write.
/// - Bytes 3736–3763: limits (`LedgerFsLimits`, `LIMITS_OFFSET`, 28 bytes)
/// - Bytes 3764–3771: active_root_hint (u64, `ACTIVE_ROOT_HINT_OFFSET`)
/// - Bytes 3772–4027: keyslots (2 × 128 bytes, `keyslot::KEYSLOTS_OFFSET`)
/// - Bytes 4028–4087: Reserved slack (zero on write, ignored on read)
/// - Bytes 4088–4095: superblock_checksum (u64, last 8 bytes of block)
///
/// Zero-value ext_rlr_lba / ext_rlr_block_count means "absent" —
/// the embedded 56-entry RLR is authoritative.  Existing volumes that
/// have those bytes as zero will decode correctly with no migration needed.
#[derive(Debug, Clone)]
pub struct Superblock {
	pub magic: u64,
	pub version: u32,
	pub block_size: u32,
	pub total_blocks: u64,

	// Active root pointer
	pub active_root_id: [u8; BLAKE3_HASH_SIZE],
	pub active_commit_seq: u64,
	pub active_checksum: u64,

	// Root Locator Ring
	pub rlr_head: u32,
	pub rlr_tail: u32,
	pub rlr_entries: Vec<RlrEntry>,

	// Feature flags
	pub features_compat: u64,
	pub features_ro_compat: u64,
	pub features_incompat: u64,

	// Extended RLR fields — fixed at bytes 3464 and 3472 of the block.
	// Value 0 means "absent" — embedded 56-entry RLR is authoritative.
	/// LBA of the Extended RLR region (0 = absent).
	pub ext_rlr_lba: u64,

	/// Size of the Extended RLR in 4 KiB blocks (0 = absent).
	pub ext_rlr_block_count: u32,

	/// Configurable boot/kernel reader operating limits — fixed at byte
	/// offset [`LIMITS_OFFSET`] of the block. All-zero (the default for a
	/// freshly created superblock) means "absent, use compiled-in defaults".
	pub limits: LedgerFsLimits,

	/// Physical block index of the object named by `active_root_id`.
	///
	/// Purely an accelerator, and safe to be wrong: `read_object` verifies
	/// the hinted block's stored `ObjectId` before trusting it and falls
	/// back to a linear scan on any mismatch. Without it, *every* mount of
	/// a populated volume pays a full `max_scan_blocks` scan just to fetch
	/// its own root inode — with a 32 768-block scan limit over ATA PIO
	/// that is tens of seconds, which is what made opens on the data volume
	/// appear to hang outright. `0` = absent (see [`ACTIVE_ROOT_HINT_OFFSET`]).
	pub active_root_hint: u64,

	/// Volume keyslots — see [`crate::keyslot`].
	///
	/// Carried on the struct rather than left as raw slack because
	/// [`Superblock::encode`] begins with `buf.fill(0)`: anything not
	/// re-emitted by `encode` is erased on every superblock write. Since a
	/// commit rewrites the superblock, keyslots left out of this struct would
	/// be wiped by the first write after provisioning — turning an encrypted
	/// volume permanently unopenable while reporting success.
	pub keyslots: [crate::keyslot::KeySlot; crate::keyslot::SLOT_COUNT],

	// Superblock checksum
	pub superblock_checksum: u64,
}

impl Superblock {
	/// Create a new superblock
	pub fn new(total_blocks: u64) -> Self {
		let mut sb = Self {
			magic: SUPERBLOCK_MAGIC,
			version: 1,
			block_size: BLOCK_SIZE,
			total_blocks,
			active_root_id: [0u8; BLAKE3_HASH_SIZE],
			active_commit_seq: 0,
			active_checksum: 0,
			rlr_head: 0,
			rlr_tail: 0,
			rlr_entries: Vec::new(),
			features_compat: 0,
			features_ro_compat: 0,
			features_incompat: 0,
			ext_rlr_lba: 0,
			ext_rlr_block_count: 0,
			limits: LedgerFsLimits::default(),
			active_root_hint: 0,
			keyslots: [crate::keyslot::KeySlot::empty(); crate::keyslot::SLOT_COUNT],
			superblock_checksum: 0,
		};

		// Initialize RLR with empty entries
		for _ in 0..RLR_SIZE {
			sb.rlr_entries.push(RlrEntry {
				root_id: [0u8; BLAKE3_HASH_SIZE],
				commit_seq: 0,
				timestamp: 0,
				flags: 0,
				entry_checksum: 0,
			});
		}

		sb.compute_checksums();
		sb
	}

	/// Every `features_incompat` bit this build actually understands.
	///
	/// A volume whose `features_incompat` contains any bit *outside* this
	/// mask was written by a newer/different build and must not be mounted
	/// — see [`check_incompat_supported`](Self::check_incompat_supported).
	/// `FEATURE_INCOMPAT_DATA_ENCRYPTED` is only listed here when the
	/// `data-encryption` feature is compiled in: a build without the cipher
	/// genuinely cannot read such a volume, and must say so rather than
	/// silently reading ciphertext as if it were plaintext.
	pub const fn supported_incompat_features() -> u64 {
		#[cfg(feature = "data-encryption")]
		{
			FEATURE_INCOMPAT_DATA_ENCRYPTED
		}
		#[cfg(not(feature = "data-encryption"))]
		{
			0
		}
	}

	/// Whether this volume's blocks are stored encrypted
	/// ([`FEATURE_INCOMPAT_DATA_ENCRYPTED`]).
	pub const fn is_data_encrypted(&self) -> bool {
		self.features_incompat & FEATURE_INCOMPAT_DATA_ENCRYPTED != 0
	}

	/// Mark this volume as storing encrypted blocks. Intended for
	/// genesis-format/`mkfs` time only — see
	/// [`FEATURE_INCOMPAT_DATA_ENCRYPTED`]'s doc comment on why there is no
	/// in-place conversion path for an already-populated volume. The caller
	/// is responsible for recomputing checksums (`compute_checksums`)
	/// before encoding.
	pub fn set_data_encrypted(&mut self) {
		self.features_incompat |= FEATURE_INCOMPAT_DATA_ENCRYPTED;
	}

	/// Refuse volumes carrying an `features_incompat` bit this build doesn't
	/// implement.
	///
	/// Standard ext4/btrfs-style feature-flag semantics: an *incompat* bit
	/// means "you cannot correctly interpret this volume's bytes without
	/// understanding this feature", so the only safe response to an
	/// unrecognized one is to refuse the mount entirely rather than read
	/// data that will decode as garbage. Called from [`Superblock::decode`](Self::decode),
	/// after the copy's two checksums verify.
	pub fn check_incompat_supported(&self) -> Result<()> {
		let unsupported = self.features_incompat & !Self::supported_incompat_features();
		if unsupported != 0 {
			return Err(Error::invalid_operation(
				"superblock declares features_incompat bits this build does not support (volume may be encrypted by a build with `data-encryption` enabled, or written by a newer version)",
			));
		}
		Ok(())
	}

	/// Compute active_checksum
	fn compute_active_checksum(&self) -> u64 {
		let mut data = Vec::new();
		data.extend_from_slice(&self.active_root_id);
		data.extend_from_slice(&self.active_commit_seq.to_le_bytes());
		blake3_checksum(&data)
	}

	/// Compute superblock_checksum
	fn compute_superblock_checksum(&self, buf: &[u8]) -> u64 {
		// Checksum covers entire superblock except the checksum field itself
		// The checksum field is at the end of the block
		let checksum_offset = BLOCK_SIZE as usize - 8;
		let mut data = Vec::new();
		data.extend_from_slice(&buf[0..checksum_offset]);
		blake3_checksum(&data)
	}

	/// Compute all checksums
	pub fn compute_checksums(&mut self) {
		self.active_checksum = self.compute_active_checksum();
		// Note: superblock_checksum must be computed during encode
	}

	/// Decode superblock from 4096-byte block
	pub fn decode(buf: &[u8]) -> Result<Self> {
		if buf.len() < BLOCK_SIZE as usize {
			return Err(Error::BufferTooSmall {
				required: BLOCK_SIZE as usize,
				available: buf.len(),
			});
		}

		// Read magic
		let magic = read_u64(&buf[0..])?;
		if magic != SUPERBLOCK_MAGIC {
			return Err(Error::InvalidMagic {
				expected: SUPERBLOCK_MAGIC,
				found: magic,
			});
		}

		// Read basic fields
		let version = read_u32(&buf[8..])?;
		let block_size = read_u32(&buf[12..])?;
		let total_blocks = read_u64(&buf[16..])?;

		// Read active root
		let mut active_root_id = [0u8; BLAKE3_HASH_SIZE];
		active_root_id.copy_from_slice(&buf[24..24 + BLAKE3_HASH_SIZE]);

		let active_commit_seq = read_u64(&buf[56..])?;
		let active_checksum = read_u64(&buf[64..])?;

		// Read RLR pointers
		let rlr_head = read_u32(&buf[72..])?;
		let rlr_tail = read_u32(&buf[76..])?;

		// Validate RLR pointers
		if rlr_head >= RLR_SIZE as u32 || rlr_tail >= RLR_SIZE as u32 {
			return Err(Error::InvalidRlrPointers {
				head: rlr_head,
				tail: rlr_tail,
			});
		}

		// Read RLR entries
		let mut rlr_entries = Vec::new();
		let mut offset = 80;
		for _ in 0..RLR_SIZE {
			let entry = RlrEntry::decode(&buf[offset..offset + RlrEntry::SIZE])?;
			rlr_entries.push(entry);
			offset += RlrEntry::SIZE;
		}

		// Read feature flags (after RLR entries)
		let features_compat = read_u64(&buf[offset..])?;
		let features_ro_compat = read_u64(&buf[offset + 8..])?;
		let features_incompat = read_u64(&buf[offset + 16..])?;

		// Read extended RLR fields at their fixed offsets within the slack region.
		// EXT_RLR_LBA_OFFSET = 80 (header) + 3360 (RLR) + 24 (feature flags) = 3464.
		// Zero values mean "absent" — backward-compatible with existing volumes.
		let ext_rlr_lba = read_u64(&buf[EXT_RLR_LBA_OFFSET..])?;
		let ext_rlr_block_count = read_u32(&buf[EXT_RLR_BLOCK_COUNT_OFFSET..])?;

		// Read configurable operating limits at their fixed offset.
		// All-zero means "absent" — same backward-compatible convention as
		// ext_rlr_lba/ext_rlr_block_count above.
		let limits = LedgerFsLimits::decode(&buf[LIMITS_OFFSET..LIMITS_OFFSET + LedgerFsLimits::SIZE])?;

		// Read the root placement hint at its fixed offset (0 = absent).
		let active_root_hint = read_u64(&buf[ACTIVE_ROOT_HINT_OFFSET..])?;

		// Volume keyslots. A zeroed region decodes as "no slots", so volumes
		// written before keyslots existed decode unchanged.
		let keyslots = crate::keyslot::decode_slots(buf)?;

		// Read superblock checksum (last 8 bytes)
		let superblock_checksum = read_u64(&buf[BLOCK_SIZE as usize - 8..])?;

		let sb = Self {
			magic,
			version,
			block_size,
			total_blocks,
			active_root_id,
			active_commit_seq,
			active_checksum,
			rlr_head,
			rlr_tail,
			rlr_entries,
			features_compat,
			features_ro_compat,
			features_incompat,
			ext_rlr_lba,
			ext_rlr_block_count,
			limits,
			active_root_hint,
			keyslots,
			superblock_checksum,
		};

		// Verify active checksum
		let expected_active_checksum = sb.compute_active_checksum();
		if active_checksum != expected_active_checksum {
			return Err(Error::SuperblockChecksumInvalid);
		}

		// Verify superblock checksum
		let expected_sb_checksum = sb.compute_superblock_checksum(buf);
		if superblock_checksum != expected_sb_checksum {
			return Err(Error::SuperblockChecksumInvalid);
		}

		// Refuse a volume declaring an incompat feature this build lacks.
		//
		// Enforced here rather than at each mount site because it was enforced
		// at *none* of them: `check_incompat_supported` existed, was documented
		// as the thing that stops a build without `data-encryption` reading an
		// encrypted volume as plaintext, and had tests — which were its only
		// callers. Both mount paths (the kernel reader's boot scan and the host
		// `Volume`) went straight from decode to use.
		// A check every caller must remember is a check that gets forgotten;
		// this one is now impossible to bypass.
		sb.check_incompat_supported()?;

		Ok(sb)
	}

	/// Encode superblock to 4096-byte block
	pub fn encode(&mut self, buf: &mut [u8]) -> Result<()> {
		if buf.len() < BLOCK_SIZE as usize {
			return Err(Error::BufferTooSmall {
				required: BLOCK_SIZE as usize,
				available: buf.len(),
			});
		}

		// Zero buffer first
		buf.fill(0);

		// Write basic fields
		write_u64(&mut buf[0..], self.magic)?;
		write_u32(&mut buf[8..], self.version)?;
		write_u32(&mut buf[12..], self.block_size)?;
		write_u64(&mut buf[16..], self.total_blocks)?;

		// Write active root
		buf[24..24 + BLAKE3_HASH_SIZE].copy_from_slice(&self.active_root_id);
		write_u64(&mut buf[56..], self.active_commit_seq)?;

		// Compute and write active checksum
		self.active_checksum = self.compute_active_checksum();
		write_u64(&mut buf[64..], self.active_checksum)?;

		// Write RLR pointers
		write_u32(&mut buf[72..], self.rlr_head)?;
		write_u32(&mut buf[76..], self.rlr_tail)?;

		// Write RLR entries
		let mut offset = 80;
		for entry in &self.rlr_entries {
			entry.encode(&mut buf[offset..offset + RlrEntry::SIZE])?;
			offset += RlrEntry::SIZE;
		}

		// Write feature flags
		write_u64(&mut buf[offset..], self.features_compat)?;
		write_u64(&mut buf[offset + 8..], self.features_ro_compat)?;
		write_u64(&mut buf[offset + 16..], self.features_incompat)?;

		// Write extended RLR fields at their fixed offsets.
		// EXT_RLR_LBA_OFFSET = 3464, EXT_RLR_BLOCK_COUNT_OFFSET = 3472.
		write_u64(&mut buf[EXT_RLR_LBA_OFFSET..], self.ext_rlr_lba)?;
		write_u32(&mut buf[EXT_RLR_BLOCK_COUNT_OFFSET..], self.ext_rlr_block_count)?;

		// Write configurable operating limits at their fixed offset.
		self.limits
			.encode(&mut buf[LIMITS_OFFSET..LIMITS_OFFSET + LedgerFsLimits::SIZE])?;

		// Write the root placement hint at its fixed offset.
		write_u64(&mut buf[ACTIVE_ROOT_HINT_OFFSET..], self.active_root_hint)?;

		// Re-emit the keyslots. `buf.fill(0)` above cleared them; without
		// this every superblock write would destroy the volume's key
		// material. See the `keyslots` field doc.
		crate::keyslot::encode_slots(&self.keyslots, buf)?;

		// Compute and write superblock checksum (last 8 bytes)
		self.superblock_checksum = self.compute_superblock_checksum(buf);
		write_u64(&mut buf[BLOCK_SIZE as usize - 8..], self.superblock_checksum)?;

		Ok(())
	}

	/// Add entry to RLR
	pub fn add_rlr_entry(&mut self, entry: RlrEntry) {
		let idx = self.rlr_head as usize;
		self.rlr_entries[idx] = entry;
		self.rlr_head = (self.rlr_head + 1) % RLR_SIZE as u32;
	}

	// region: --- Resolved limits (0 in `self.limits` = absent = compiled-in default)
	//
	// These mirror the hardcoded consts `reader.rs` and the boot reader used
	// before this field existed, so a superblock with `limits` all-zero
	// (every volume built before this change, and any freshly created one
	// that doesn't opt into custom limits) behaves identically to before.

	/// Kernel reader: max size of one `ObjectRecord`. Default 256 KiB.
	pub fn max_object_bytes(&self) -> usize {
		if self.limits.max_object_bytes == 0 {
			256 * 1024
		} else {
			self.limits.max_object_bytes as usize
		}
	}

	/// Kernel reader: max total bytes returned by one ranged read. Default 8 MiB.
	pub fn max_range_read_bytes(&self) -> usize {
		if self.limits.max_range_read_bytes == 0 {
			8 * 1024 * 1024
		} else {
			self.limits.max_range_read_bytes as usize
		}
	}

	/// Kernel reader: max blocks scanned looking for an object. Default 16384.
	pub fn max_scan_blocks(&self) -> u64 {
		if self.limits.max_scan_blocks == 0 {
			16384
		} else {
			self.limits.max_scan_blocks
		}
	}

	/// Boot reader: max size of one file read at boot. Default 1 MiB — the
	/// cap that actually governs "how big can a TCB binary be".
	pub fn max_boot_file_bytes(&self) -> usize {
		if self.limits.max_boot_file_bytes == 0 {
			1024 * 1024
		} else {
			self.limits.max_boot_file_bytes as usize
		}
	}

	/// Boot reader: max blocks scanned looking for an object at boot. Default 131072.
	pub fn max_boot_scan_blocks(&self) -> u64 {
		if self.limits.max_boot_scan_blocks == 0 {
			131_072
		} else {
			self.limits.max_boot_scan_blocks
		}
	}

	// endregion: --- Resolved limits
}

/// Root candidate for mount selection
#[derive(Debug, Clone)]
pub struct RootCandidate {
	pub root_id: [u8; BLAKE3_HASH_SIZE],
	pub commit_seq: u64,
	pub source: CandidateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSource {
	ActiveSlot { copy: usize },
	RlrEntry { copy: usize, index: usize },
}

/// Mount selection result
#[derive(Debug)]
pub struct MountSelection {
	pub selected_root: RootCandidate,
	pub mode: FsMode,
	pub replay_decision: ReplayDecision,
}

/// Deterministic mount selection algorithm (§18)
pub fn mount_select(superblocks: &[Option<Superblock>; NUM_SUPERBLOCK_COPIES]) -> Result<MountSelection> {
	mount_select_with_intent_log(superblocks, None)
}

/// Mount selection with optional intent log for replay decision (§18.5)
pub fn mount_select_with_intent_log(
	superblocks: &[Option<Superblock>; NUM_SUPERBLOCK_COPIES],
	intent_log_ring: Option<&[u8]>,
) -> Result<MountSelection> {
	use crate::intent_log::{build_tx_views, determine_replay, parse_ring};

	// Step 1: Find all valid superblock copies
	let mut valid_copies = Vec::new();
	for (idx, sb_opt) in superblocks.iter().enumerate() {
		if let Some(sb) = sb_opt {
			valid_copies.push((idx, sb));
		}
	}

	if valid_copies.is_empty() {
		return Err(Error::NoValidSuperblock);
	}

	// Step 2: Extract all root candidates from active slots and RLR
	let mut candidates = Vec::new();

	for (copy_idx, sb) in &valid_copies {
		// Add active root candidate
		if sb.active_commit_seq > 0 {
			candidates.push(RootCandidate {
				root_id: sb.active_root_id,
				commit_seq: sb.active_commit_seq,
				source: CandidateSource::ActiveSlot { copy: *copy_idx },
			});
		}

		// Add RLR candidates
		for (rlr_idx, entry) in sb.rlr_entries.iter().enumerate() {
			if entry.commit_seq > 0 {
				candidates.push(RootCandidate {
					root_id: entry.root_id,
					commit_seq: entry.commit_seq,
					source: CandidateSource::RlrEntry {
						copy: *copy_idx,
						index: rlr_idx,
					},
				});
			}
		}
	}

	if candidates.is_empty() {
		return Err(Error::NoValidSuperblock);
	}

	// Step 3: Find highest commit_seq
	let max_seq = candidates.iter().map(|c| c.commit_seq).max().unwrap();

	// Step 4: Check for split-brain (same seq, different root_id)
	let max_seq_candidates: Vec<_> = candidates.iter().filter(|c| c.commit_seq == max_seq).collect();

	if max_seq_candidates.len() > 1 {
		// Check if all have same root_id
		let first_id = &max_seq_candidates[0].root_id;
		let all_same = max_seq_candidates.iter().all(|c| &c.root_id == first_id);

		if !all_same {
			// Split-brain detected - enter read-only safe mode
			return Ok(MountSelection {
				selected_root: max_seq_candidates[0].clone(),
				mode: FsMode::ReadOnlySafe,
				replay_decision: ReplayDecision::NoReplay,
			});
		}
	}

	// Step 5: Select the highest seq candidate
	// Prefer RLR source over active slot (per FORMAT.md candidate precedence)
	let selected = max_seq_candidates
		.into_iter()
		.min_by_key(|c| match c.source {
			CandidateSource::RlrEntry { .. } => 0, // RLR preferred
			CandidateSource::ActiveSlot { .. } => 1,
		})
		.unwrap()
		.clone();

	let selected_commit_seq = selected.commit_seq;

	// Step 6: Check continuity (§18.4 / §18.7)
	// Find highest commit_seq < selected_commit_seq from all candidates
	let prev_known_seq = candidates
		.iter()
		.filter(|c| c.commit_seq < selected_commit_seq)
		.map(|c| c.commit_seq)
		.max();

	let mut mode = FsMode::ReadWrite;
	let mut force_no_replay = false;

	if let Some(prev_seq) = prev_known_seq {
		if selected_commit_seq != prev_seq + 1 {
			// Gap detected - enter read-only safe mode (§18.7)
			mode = FsMode::ReadOnlySafe;
			force_no_replay = true;
		}
	} else {
		// No previous seq found - continuity unknown
		// Allow mount to proceed but log audit-level warning
		// (Would emit audit log here in full implementation)
	}

	// Step 7: Determine replay decision from intent log (§18.5)
	let replay_decision = if force_no_replay {
		ReplayDecision::NoReplay
	} else if let Some(ring) = intent_log_ring {
		// Parse intent log and check for valid PREPARE_ROOT at N+1
		match parse_ring(ring, ring.len()) {
			Ok(records) => {
				let tx_views = build_tx_views(&records);
				determine_replay(selected_commit_seq, &tx_views).unwrap_or(ReplayDecision::NoReplay)
			}
			Err(_) => {
				// Intent log unreliable (e.g., seq collision) - no replay
				ReplayDecision::NoReplay
			}
		}
	} else {
		ReplayDecision::NoReplay
	};

	Ok(MountSelection {
		selected_root: selected,
		mode,
		replay_decision,
	})
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	// `vec!` is only used by this module's fixtures, so the macro is imported
	// here rather than at the parent's scope.
	use alloc::boxed::Box;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_superblock_rlr_entry_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; RlrEntry::SIZE];
		let root_id = [42u8; BLAKE3_HASH_SIZE];
		let entry = RlrEntry::new(root_id, 123, 456789, 0);

		// -- Exec
		entry.encode(&mut buf)?;
		let decoded = RlrEntry::decode(&buf)?;

		// -- Check
		assert_eq!(decoded.root_id, entry.root_id);
		assert_eq!(decoded.commit_seq, entry.commit_seq);
		assert_eq!(decoded.timestamp, entry.timestamp);
		assert_eq!(decoded.flags, entry.flags);

		Ok(())
	}

	#[test]
	fn test_superblock_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [42u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 100;

		// -- Exec
		sb.encode(&mut buf)?;
		let decoded = Superblock::decode(&buf)?;

		// -- Check
		assert_eq!(decoded.magic, sb.magic);
		assert_eq!(decoded.version, sb.version);
		assert_eq!(decoded.block_size, sb.block_size);
		assert_eq!(decoded.total_blocks, sb.total_blocks);
		assert_eq!(decoded.active_root_id, sb.active_root_id);
		assert_eq!(decoded.active_commit_seq, sb.active_commit_seq);

		Ok(())
	}

	#[test]
	fn test_data_encrypted_flag_defaults_off_and_round_trips_when_set() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		let sb = Superblock::new(1000);

		// -- Check: a freshly created volume is NOT encrypted — encryption
		// is strictly opt-in at format time, never a silent default.
		assert!(!sb.is_data_encrypted(), "a new superblock must default to unencrypted");

		// -- Exec: mark encrypted, re-checksum, round-trip through disk bytes.
		let mut sb = sb;
		sb.set_data_encrypted();
		sb.compute_checksums();
		sb.encode(&mut buf)?;

		// -- Check: what decoding it does depends on whether THIS build can
		// actually decrypt it, which is the entire point of an *incompat* bit.
		#[cfg(feature = "data-encryption")]
		{
			let decoded = Superblock::decode(&buf)?;
			// The flag survives encode/decode — it rides in the
			// already-persisted `features_incompat` field, no format change.
			assert!(
				decoded.is_data_encrypted(),
				"the encrypted flag must survive an encode/decode round trip"
			);
			assert_eq!(
				decoded.features_incompat & FEATURE_INCOMPAT_DATA_ENCRYPTED,
				FEATURE_INCOMPAT_DATA_ENCRYPTED
			);
		}
		#[cfg(not(feature = "data-encryption"))]
		{
			// A build that cannot decrypt must refuse the volume outright
			// rather than hand back a superblock a caller would then mount and
			// read as plaintext. `decode` enforces this so no mount site can
			// forget to; before that it was enforced nowhere but in tests.
			assert!(
				Superblock::decode(&buf).is_err(),
				"a build without `data-encryption` must refuse an encrypted volume, not decode it"
			);
		}

		Ok(())
	}

	#[test]
	fn test_unknown_incompat_feature_bit_is_refused() -> TestResult<()> {
		// -- Setup & Fixtures: a volume written by some future build that
		// uses an incompat bit this build has never heard of.
		let mut sb = Superblock::new(1000);
		sb.features_incompat |= 0x8000_0000_0000_0000;
		sb.compute_checksums();

		// -- Exec & Check: must refuse rather than mount and misread it.
		assert!(
			sb.check_incompat_supported().is_err(),
			"an unrecognized features_incompat bit must refuse the mount, not be ignored"
		);

		Ok(())
	}

	/// The safety-critical direction of the feature gate: a build compiled
	/// WITHOUT `data-encryption` must refuse an encrypted volume outright.
	/// If it instead mounted it, every block would decrypt to garbage and
	/// surface as "corruption" — actively misleading, and (worse) a write
	/// to such a volume would destroy real ciphertext.
	#[cfg(not(feature = "data-encryption"))]
	#[test]
	fn test_build_without_encryption_support_refuses_an_encrypted_volume() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000);
		sb.set_data_encrypted();
		sb.compute_checksums();

		// -- Exec & Check
		assert!(
			sb.check_incompat_supported().is_err(),
			"a build without the `data-encryption` feature must refuse an encrypted volume"
		);

		Ok(())
	}

	/// The mirror of the above: a build that DOES have the cipher accepts it.
	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_build_with_encryption_support_accepts_an_encrypted_volume() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000);
		sb.set_data_encrypted();
		sb.compute_checksums();

		// -- Exec & Check
		assert!(
			sb.check_incompat_supported().is_ok(),
			"a build with `data-encryption` must accept an encrypted volume"
		);
		// A plain (unencrypted) volume must still be accepted either way —
		// enabling the feature must not break reading existing volumes.
		let plain = Superblock::new(1000);
		assert!(
			plain.check_incompat_supported().is_ok(),
			"an unencrypted volume must still mount on an encryption-capable build"
		);

		Ok(())
	}

	#[test]
	fn test_superblock_ext_rlr_fields_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [7u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 42;
		sb.ext_rlr_lba = 0xDEAD_BEEF_CAFE_1234;
		sb.ext_rlr_block_count = 0x1234_5678;

		// -- Exec
		sb.encode(&mut buf)?;
		let decoded = Superblock::decode(&buf)?;

		// -- Check: new fields survive encode/decode round-trip.
		assert_eq!(decoded.ext_rlr_lba, 0xDEAD_BEEF_CAFE_1234);
		assert_eq!(decoded.ext_rlr_block_count, 0x1234_5678);

		Ok(())
	}

	#[test]
	fn test_superblock_ext_rlr_fields_zero_default() -> TestResult<()> {
		// -- Setup & Fixtures: decode a superblock encoded without ext_rlr fields
		// (simulates an existing volume — those bytes are zero from buf.fill(0)).
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [1u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 1;
		// ext_rlr_lba and ext_rlr_block_count are default 0.

		// -- Exec
		sb.encode(&mut buf)?;
		let decoded = Superblock::decode(&buf)?;

		// -- Check: zero means "absent" — backward compatible.
		assert_eq!(decoded.ext_rlr_lba, 0);
		assert_eq!(decoded.ext_rlr_block_count, 0);

		Ok(())
	}

	#[test]
	fn test_superblock_limits_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [9u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 3;
		sb.limits = LedgerFsLimits {
			max_object_bytes: 512 * 1024,
			max_range_read_bytes: 16 * 1024 * 1024,
			max_scan_blocks: 32768,
			max_boot_file_bytes: 2 * 1024 * 1024,
			max_boot_scan_blocks: 262_144,
		};

		// -- Exec
		sb.encode(&mut buf)?;
		let decoded = Superblock::decode(&buf)?;

		// -- Check: custom limits survive encode/decode round-trip and the
		// resolved accessors return the custom value, not the default.
		assert_eq!(decoded.limits, sb.limits);
		assert_eq!(decoded.max_object_bytes(), 512 * 1024);
		assert_eq!(decoded.max_range_read_bytes(), 16 * 1024 * 1024);
		assert_eq!(decoded.max_scan_blocks(), 32768);
		assert_eq!(decoded.max_boot_file_bytes(), 2 * 1024 * 1024);
		assert_eq!(decoded.max_boot_scan_blocks(), 262_144);

		Ok(())
	}

	#[test]
	fn test_superblock_limits_zero_default() -> TestResult<()> {
		// -- Setup & Fixtures: a freshly created superblock (or an existing
		// volume built before `limits` existed — those bytes are zero from
		// buf.fill(0)) must resolve to the exact values the hardcoded
		// consts used before this field existed.
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [2u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 1;
		// sb.limits left at its Default (all-zero).

		// -- Exec
		sb.encode(&mut buf)?;
		let decoded = Superblock::decode(&buf)?;

		// -- Check: zero means "absent" — resolves to the pre-existing hardcoded defaults.
		assert_eq!(decoded.limits, LedgerFsLimits::default());
		assert_eq!(decoded.max_object_bytes(), 256 * 1024);
		assert_eq!(decoded.max_range_read_bytes(), 8 * 1024 * 1024);
		assert_eq!(decoded.max_scan_blocks(), 16384);
		assert_eq!(decoded.max_boot_file_bytes(), 1024 * 1024);
		assert_eq!(decoded.max_boot_scan_blocks(), 131_072);

		Ok(())
	}

	#[test]
	fn test_superblock_invalid_magic() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; BLOCK_SIZE as usize];
		write_u64(&mut buf[0..], 0xDEADBEEF)?;

		// -- Exec & Check
		let result = Superblock::decode(&buf);
		assert!(matches!(result, Err(Error::InvalidMagic { .. })));

		Ok(())
	}

	#[test]
	fn test_superblock_add_rlr_entry() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000000);
		let entry1 = RlrEntry::new([1u8; BLAKE3_HASH_SIZE], 1, 100, 0);
		let entry2 = RlrEntry::new([2u8; BLAKE3_HASH_SIZE], 2, 200, 0);

		// -- Exec
		sb.add_rlr_entry(entry1.clone());
		sb.add_rlr_entry(entry2.clone());

		// -- Check
		assert_eq!(sb.rlr_head, 2);
		assert_eq!(sb.rlr_entries[0].commit_seq, 1);
		assert_eq!(sb.rlr_entries[1].commit_seq, 2);

		Ok(())
	}

	#[test]
	fn test_superblock_mount_select_single_copy() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [42u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 100;

		let superblocks = [Some(sb), None, None, None];

		// -- Exec
		let selection = mount_select(&superblocks)?;

		// -- Check
		assert_eq!(selection.selected_root.commit_seq, 100);
		assert_eq!(selection.mode, FsMode::ReadWrite);

		Ok(())
	}

	#[test]
	fn test_superblock_mount_select_split_brain() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb1 = Superblock::new(1000000);
		sb1.active_root_id = [1u8; BLAKE3_HASH_SIZE];
		sb1.active_commit_seq = 100;

		let mut sb2 = Superblock::new(1000000);
		sb2.active_root_id = [2u8; BLAKE3_HASH_SIZE]; // Different root!
		sb2.active_commit_seq = 100; // Same seq!

		let superblocks = [Some(sb1), Some(sb2), None, None];

		// -- Exec
		let selection = mount_select(&superblocks)?;

		// -- Check (should enter read-only safe mode)
		assert_eq!(selection.mode, FsMode::ReadOnlySafe);

		Ok(())
	}

	#[test]
	fn test_superblock_mount_select_continuity_gap() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [42u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 105; // Gap from 100 to 105

		// Add RLR entry at seq 100
		let entry = RlrEntry::new([1u8; BLAKE3_HASH_SIZE], 100, 1000, 0);
		sb.add_rlr_entry(entry);

		let superblocks = [Some(sb), None, None, None];

		// -- Exec
		let selection = mount_select(&superblocks)?;

		// -- Check (should enter read-only safe mode due to gap)
		assert_eq!(selection.selected_root.commit_seq, 105);
		assert_eq!(selection.mode, FsMode::ReadOnlySafe);
		assert_eq!(selection.replay_decision, ReplayDecision::NoReplay);

		Ok(())
	}

	#[test]
	fn test_superblock_mount_select_continuity_ok() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [42u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 101; // Follows from 100

		// Add RLR entry at seq 100
		let entry = RlrEntry::new([1u8; BLAKE3_HASH_SIZE], 100, 1000, 0);
		sb.add_rlr_entry(entry);

		let superblocks = [Some(sb), None, None, None];

		// -- Exec
		let selection = mount_select(&superblocks)?;

		// -- Check (should be read-write with no gap)
		assert_eq!(selection.selected_root.commit_seq, 101);
		assert_eq!(selection.mode, FsMode::ReadWrite);

		Ok(())
	}

	#[test]
	fn test_superblock_mount_select_with_replay() -> TestResult<()> {
		use crate::encoding::write_u64;
		use crate::intent_log::{IntentLog, IntentLogRecord, IntentRecordType};

		// -- Setup & Fixtures
		let mut sb = Superblock::new(1000000);
		sb.active_root_id = [42u8; BLAKE3_HASH_SIZE];
		sb.active_commit_seq = 100;

		let superblocks = [Some(sb), None, None, None];

		// Create intent log with PREPARE_ROOT at seq 101
		let ring_size = 4096;
		let mut ring = vec![0u8; ring_size];
		let log = IntentLog::new(ring_size);

		// BEGIN record
		let begin = IntentLogRecord::new(1, 42, IntentRecordType::Begin, vec![]);
		log.write_record(&mut ring, 0, &begin)?;

		// PREPARE_ROOT with commit_seq = 101 (N+1 where N=100)
		let mut payload = vec![0u8; 8];
		write_u64(&mut payload, 101)?;
		let prepare = IntentLogRecord::new(2, 42, IntentRecordType::PrepareRoot, payload);
		log.write_record(&mut ring, 128, &prepare)?;

		// -- Exec
		let selection = mount_select_with_intent_log(&superblocks, Some(&ring))?;

		// -- Check (should trigger replay to 101)
		assert_eq!(selection.selected_root.commit_seq, 100);
		assert_eq!(selection.mode, FsMode::ReadWrite);
		assert_eq!(selection.replay_decision, ReplayDecision::ReplayTo { commit_seq: 101 });

		Ok(())
	}
}

// endregion: --- Tests
