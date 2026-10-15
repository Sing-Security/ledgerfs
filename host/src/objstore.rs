//! A disk-backed [`ObjectStore`].
//!
//! `cow::object_store`'s own module doc says the trait exists so there can be
//! "an in-memory store (for tests) and **a disk-backed store (for runtime)**".
//! `InMemoryObjectStore` is that first half. This is the second: it resolves
//! an [`ObjectRef`] to the `ObjectRecord` sitting on a real volume and hands
//! the verified payload back.
//!
//! Everything above this trait — `read`, `readdir`, `mkdir`, `write`,
//! `unlink`, `rename` in `cow::operations` — is generic over `ObjectStore` and
//! therefore already works once this compiles. None of it is reimplemented
//! here.
//!
//! # How an object is found
//!
//! In order:
//!
//! 1. **The reference's own hint.** `ObjectRef.hint` is the physical block the
//!    placement engine allocated, and `DirEntry` carries it alongside the id,
//!    so a directory walk is O(1) reads per level. The block is only trusted
//!    after the record decoded there reports the *requested* id.
//! 2. **The hint cache.** Filled by [`ObjectStore::record_hint`] after a
//!    commit and by every successful scan below.
//! 3. **A linear scan** from `RESERVED_HEADER_BLOCKS` to whichever comes first
//!    of the superblock's `max_scan_blocks` limit and the last non-trailer
//!    block, reading each block's 60-byte header and comparing its `object_id`.
//!
//! The scan is the same shape as the kernel reader's and the boot-time
//! reader's, deliberately: a volume has to be findable the same way from all
//! three readers or the format has three definitions.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ledgerfs::cow::ObjectStore;
use ledgerfs::object::{Chunk, DirNode, ExtentNode, Inode, ObjectId, ObjectRef};
use ledgerfs::record::{ObjectRecordHeader, ObjectRecordReader};
use ledgerfs::{BLOCK_SIZE, RESERVED_HEADER_BLOCKS, RESERVED_TRAILER_BLOCKS};
use ledgerfs::{Error as LedgerFsError, Result as LedgerFsResult};

use crate::device::{BlockDevice, HintCache};
use crate::error::{Error, Result};

pub struct DiskObjectStore {
	/// The volume's backing storage. A trait object rather than a type
	/// parameter: `HostFs`, `Volume` and every commit-path signature stay
	/// non-generic, and the dispatch costs nothing next to the I/O it wraps.
	device: Box<dyn BlockDevice>,

	/// Object id → physical block, learned from hints and scans.
	///
	/// Behind a `Mutex` because [`ObjectStore::record_hint`] takes `&self`
	/// (the trait's read methods do too), so interior mutability is the only
	/// way to cache at all. Purely an accelerator: a missed cache entry costs
	/// a scan, never correctness.
	///
	/// Poisoning is recovered, not propagated: a poisoned cache holds at worst
	/// a stale hint, and every hint is re-verified against the block it names
	/// before it is trusted — see [`read_object`](Self::read_object).
	hints: Mutex<HashMap<ObjectId, u64>>,

	/// Upper bound on a linear scan, from the superblock's resolved limits.
	max_scan_blocks: u64,

	/// Total blocks in the volume, from the superblock.
	total_blocks: u64,

	/// Whether the full id→block index has been built for this mount. A
	/// reopened volume has no persisted hints for anything but the root, so the
	/// first lookup that misses the bounded scan builds the whole index once;
	/// every later lookup is then O(1) instead of O(scan).
	indexed: AtomicBool,

	/// Where this handle persists its map, when the device has somewhere to put
	/// it. `None` for an in-memory device, whose map is only ever as old as the
	/// handle. See `hints.rs`.
	hint_cache: Option<HintCache>,

	/// Whether the sidecar has been consulted. One shot per handle.
	cache_loaded: AtomicBool,

	/// Whether the map holds anything not yet written to the sidecar.
	cache_dirty: AtomicBool,
}

// region:    --- Construction

impl DiskObjectStore {
	pub fn new(device: Box<dyn BlockDevice>, total_blocks: u64, max_scan_blocks: u64) -> Self {
		// Ask the device where a sidecar would go before it is boxed away.
		let hint_cache = device.hint_cache();
		Self {
			device,
			hints: Mutex::new(HashMap::new()),
			max_scan_blocks,
			total_blocks,
			indexed: AtomicBool::new(false),
			hint_cache,
			cache_loaded: AtomicBool::new(false),
			cache_dirty: AtomicBool::new(false),
		}
	}

	/// The underlying device, for callers that need raw block access.
	pub fn device(&self) -> &dyn BlockDevice {
		self.device.as_ref()
	}

	pub fn total_blocks(&self) -> u64 {
		self.total_blocks
	}
}

// endregion: --- Construction

// region:    --- Record access

impl DiskObjectStore {
	/// Read and fully verify the `ObjectRecord` at physical block `lba`.
	///
	/// "Fully" is the whole of it: `ObjectRecordReader::read` checks the header
	/// magic, the BLAKE3 checksum over the stored bytes, decompresses if the
	/// header says so, and confirms `blake3(payload) == header.object_id`. A
	/// payload that comes back from here has been verified end to end.
	pub fn read_record(&self, lba: u64) -> Result<(ObjectRecordHeader, Vec<u8>)> {
		// The header is 60 bytes at the head of the record; one block covers
		// it, and for most objects covers the whole record.
		let mut first = vec![0u8; BLOCK_SIZE as usize];
		self.device.read_block(lba, &mut first)?;

		let header = ObjectRecordHeader::decode(&first)?;
		let total = ObjectRecordHeader::SIZE + header.stored_len as usize;

		let buf = if total <= BLOCK_SIZE as usize {
			first
		} else {
			// A record may span many blocks (`MAX_OBJECT_SIZE` is 256 KiB —
			// 64 blocks). Read the exact length, not a rounded block count.
			let mut whole = vec![0u8; total];
			self.device.read_bytes_at(lba * BLOCK_SIZE as u64, &mut whole)?;
			whole
		};

		ObjectRecordReader::read(&buf[..total]).map_err(Error::from)
	}

	/// Read the payload of the object named by `obj_ref`.
	///
	/// # One block read per candidate
	///
	/// A candidate is the reference's own storage hint, then the hint cache, then
	/// a scan. Each is **read once**: the read that fetches the payload is the
	/// same read that checks the record's header names the id, so a candidate
	/// that turns out not to hold the object costs one block read rather than a
	/// verifying read followed by a fetching read.
	///
	/// This is where the hint's safety lives. A hint or cache entry that names
	/// the wrong block is not detectable without reading it, and reading it is
	/// the fetch — so the check is not skippable, and it costs nothing. What must
	/// never happen is accepting a *header* without the rest of the record, and
	/// `read_record` verifies the whole thing end to end: magic, checksum,
	/// decompression and `blake3(payload) == header.object_id`.
	pub fn read_object(&self, obj_ref: ObjectRef) -> Result<Vec<u8>> {
		let id = obj_ref.object_id;

		// A zero ref is "no object", not a lookup that failed.
		if id.is_zero() {
			return Err(Error::ObjectNotFound {
				object_id: id.to_hex().to_string(),
			});
		}

		if !obj_ref.hint.is_zero()
			&& let Ok((header, payload)) = self.read_record(obj_ref.hint.0)
			&& header.object_id == *id.as_bytes()
		{
			self.remember(id, obj_ref.hint.0);
			return Ok(payload);
		}

		if let Some(lba) = self.cached_lba(id)
			&& let Ok((header, payload)) = self.read_record(lba)
			&& header.object_id == *id.as_bytes()
		{
			self.remember(id, lba);
			return Ok(payload);
		}

		let lba = self.scan(id)?;
		self.remember(id, lba);
		Ok(self.read_record(lba)?.1)
	}

	/// The cached block for `id`, if the cache has one.
	///
	/// Loads the disk-backed half of the cache first: on a reopen that is what
	/// turns the whole-volume scan into a map hit. The map is a *claim*, not an
	/// answer — the read that follows verifies the block it names, so a stale
	/// entry costs one record read and a fall-through rather than a wrong
	/// answer. (A record may span blocks, so "one record read" is more bytes
	/// than the header-only probe this replaced; it is not more reads, and on a
	/// volume the writer has only ever allocated forward into, a stale entry is
	/// a bug rather than a state to optimise for.)
	fn cached_lba(&self, id: ObjectId) -> Option<u64> {
		self.ensure_cache_loaded();
		self.hints
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.get(&id)
			.copied()
	}

	/// Linear scan for `id`, from the first object block to the scan limit.
	///
	/// # Two passes
	///
	/// The superblock's `max_scan_blocks` is an advisory bound meant to cap the
	/// cost of finding an object near the head of the volume. It is **not** a
	/// correctness bound: only the *root*'s placement is persisted (in the
	/// superblock), so on a freshly remounted volume every other object has hint
	/// `0` and must be scanned for. CoW rewrites a path's whole ancestor chain on
	/// every write, so on a volume that has seen many commits the objects a read
	/// actually needs sit at the *tail* — past `max_scan_blocks`. Scanning only
	/// the bounded window then reports `ObjectNotFound` for objects that are
	/// present, which is how a migrated volume reads as corrupt. So the bounded
	/// pass runs first (cheap when the target is near the head), then the
	/// remainder of the volume.
	fn scan(&self, id: ObjectId) -> Result<u64> {
		let start = RESERVED_HEADER_BLOCKS;
		// Never scan into the trailing superblock copies: those two blocks hold
		// a superblock, not an object, and a scan that reached them would be
		// reading metadata as if it were content.
		let last = self.total_blocks.saturating_sub(RESERVED_TRAILER_BLOCKS);
		let bound = start.saturating_add(self.max_scan_blocks).min(last);

		if let Some(lba) = self.scan_range(id, start, bound)? {
			return Ok(lba);
		}

		// Build the full id→block index once. A reopened volume persists only the
		// root's hint, so every other object would otherwise re-scan from the
		// head on each lookup; one pass over the volume turns all later lookups
		// into O(1) map hits.
		if !self.indexed.swap(true, Ordering::SeqCst) {
			{
				let mut hints = self.hints.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
				let mut block = vec![0u8; BLOCK_SIZE as usize];
				for lba in start..last {
					if self.device.read_block(lba, &mut block).is_err() {
						break;
					}
					if let Ok(header) = ObjectRecordHeader::decode(&block) {
						hints.entry(ObjectId::from_bytes(&header.object_id)).or_insert(lba);
					}
				}
			}
			// The scan is the expensive half of this module — ~0.8 s on a
			// 1 GiB volume — and it has just produced the complete index. Making
			// it durable is the whole point of the sidecar: the next open of this
			// volume, in this process or another, loads that index instead of
			// walking the volume again.
			self.cache_dirty.store(true, Ordering::SeqCst);
			self.flush_cache();
		}
		if let Some(lba) = self
			.hints
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.get(&id)
			.copied()
		{
			return Ok(lba);
		}

		if bound < last
			&& let Some(lba) = self.scan_range(id, bound, last)?
		{
			return Ok(lba);
		}

		Err(Error::ObjectNotFound {
			object_id: id.to_hex().to_string(),
		})
	}

	/// Scan `[start, end)` for the record `id` starts. `Ok(None)` when it is not
	/// there.
	fn scan_range(&self, id: ObjectId, start: u64, end: u64) -> Result<Option<u64>> {
		let mut block = vec![0u8; BLOCK_SIZE as usize];
		for lba in start..end {
			if self.device.read_block(lba, &mut block).is_err() {
				// Past the partition, or an I/O error: nothing further to find.
				break;
			}
			// A block that is not the start of a record fails the magic check
			// and is skipped — free space is zeroed, so this is the common case.
			if let Ok(header) = ObjectRecordHeader::decode(&block)
				&& header.object_id == *id.as_bytes()
			{
				return Ok(Some(lba));
			}
		}
		Ok(None)
	}

	fn remember(&self, id: ObjectId, lba: u64) {
		self.hints
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.insert(id, lba);
		self.cache_dirty.store(true, Ordering::Relaxed);
	}
}

// endregion: --- Record access

// region:    --- The persistent hint cache

impl DiskObjectStore {
	/// Read the sidecar into the map, once per handle.
	///
	/// Called from [`cached_lba`](Self::cached_lba) rather than from `new`, so a store
	/// that is opened and never read past its root pays nothing — and so the
	/// load happens on the same thread as the first lookup, keeping the whole
	/// cost on the caller's cold path instead of on the mount.
	///
	/// Entries already in the map win: this handle may have learned a placement
	/// from a commit, and that is at least as fresh as anything on disk.
	fn ensure_cache_loaded(&self) {
		if self.cache_loaded.swap(true, Ordering::SeqCst) {
			return;
		}
		let Some(cache) = &self.hint_cache else {
			return;
		};
		let Some(loaded) = crate::hints::load(&cache.path, cache.volume) else {
			return;
		};
		let mut hints = self.hints.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		for (id, lba) in loaded {
			hints.entry(id).or_insert(lba);
		}
	}

	/// Write the map back, if anything has been learned since the last write.
	///
	/// Best-effort: a sidecar that cannot be written costs the next open one
	/// scan. The dirty flag is cleared first, so a concurrent `remember` from a
	/// commit is not lost to this flush's window — it simply marks it dirty
	/// again and the next flush picks it up.
	fn flush_cache(&self) {
		let Some(cache) = &self.hint_cache else {
			return;
		};
		if !self.cache_dirty.swap(false, Ordering::SeqCst) {
			return;
		}
		let hints = self.hints.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let _ = crate::hints::save(&cache.path, cache.volume, &hints);
	}
}

impl Drop for DiskObjectStore {
	fn drop(&mut self) {
		self.flush_cache();
	}
}

// endregion: --- The persistent hint cache

// endregion: --- Record access

// region:    --- Typed decodes

impl DiskObjectStore {
	/// Decode an [`Inode`] by reference.
	pub fn inode(&self, obj_ref: ObjectRef) -> Result<Inode> {
		Ok(Inode::decode_canonical(&self.read_object(obj_ref)?)?)
	}

	/// Decode a [`DirNode`] by reference.
	pub fn dir_node(&self, obj_ref: ObjectRef) -> Result<DirNode> {
		Ok(DirNode::decode_canonical(&self.read_object(obj_ref)?)?)
	}

	/// Decode an [`ExtentNode`] by reference.
	pub fn extent_node(&self, obj_ref: ObjectRef) -> Result<ExtentNode> {
		Ok(ExtentNode::decode_canonical(&self.read_object(obj_ref)?)?)
	}

	/// Decode a [`Chunk`] by reference.
	pub fn chunk(&self, obj_ref: ObjectRef) -> Result<Chunk> {
		Ok(Chunk::decode_canonical(&self.read_object(obj_ref)?)?)
	}
}

// endregion: --- Typed decodes

// region:    --- ObjectStore impl

impl ObjectStore for DiskObjectStore {
	fn get_inode(&self, obj_ref: ObjectRef) -> LedgerFsResult<Inode> {
		self.inode(obj_ref).map_err(to_ledgerfs)
	}

	fn get_dir_node(&self, obj_ref: ObjectRef) -> LedgerFsResult<DirNode> {
		self.dir_node(obj_ref).map_err(to_ledgerfs)
	}

	fn get_extent_node(&self, obj_ref: ObjectRef) -> LedgerFsResult<ExtentNode> {
		self.extent_node(obj_ref).map_err(to_ledgerfs)
	}

	fn get_chunk(&self, obj_ref: ObjectRef) -> LedgerFsResult<Chunk> {
		self.chunk(obj_ref).map_err(to_ledgerfs)
	}

	fn record_hint(&self, object_id: ObjectId, physical_lba: u64) {
		self.remember(object_id, physical_lba);
	}
}

/// Map a host error onto the kernel crate's error type, which is what the
/// [`ObjectStore`] trait is written in terms of.
///
/// `not_found` is kept as its own variant rather than folded into
/// `invalid_operation`: the trait documents it as the "not present in this
/// store" signal, and a caller distinguishing a miss from a fault would
/// otherwise stop working.
pub(crate) fn to_ledgerfs(e: Error) -> LedgerFsError {
	match e {
		Error::ObjectNotFound { .. } => LedgerFsError::not_found("object not present on this volume"),
		other => LedgerFsError::InvalidOperation(other.to_string()),
	}
}

// endregion: --- ObjectStore impl
