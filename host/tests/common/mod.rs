//! Shared fixtures for the host crate's integration tests.
//!
//! Everything here builds a *volume*, because that is the only way to reach the
//! crate's behaviour: there is no way to unit-test "what a mount does to a
//! half-written superblock" without bytes on a device.
//!
//! # Why the sizes are what they are
//!
//! [`BLOCKS`] is 4 MiB. LedgerFS reserves the first 6 blocks (superblock copies
//! at 0 and 1, the allocator snapshot at 5) and the last 2 (superblock copies
//! at N-2 and N-1), so object space starts at block 6 and a volume of 8 blocks
//! or fewer cannot be formatted at all. 4 MiB then leaves ~1000 commits of room
//! — a commit costs ~5 blocks of allocator snapshot plus one block per object on
//! the mutated path — while staying small enough that the reader's linear
//! `scan`, which is O(blocks) on a cold mount, is cheap when a test does it
//! hundreds of times.
//!
//! [`BIG_BLOCKS`] is 64 MiB, for the tests that churn a volume hard enough
//! (thousands of commits, or a 1000-entry directory) to exhaust 4 MiB.

#![allow(dead_code)] // each test binary uses a subset of these

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ledgerfs_host::{
	BlockDevice, Error, FileBlockDevice, HostFs, InodeKind, MemBlockDevice, Result as FsResult, Store, WalkEntry,
};

/// The rust10x test result: `?` on anything that implements `std::error::Error`.
pub type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

/// Blocks in the default test volume — 4 MiB.
pub const BLOCKS: u64 = 1024;

/// Blocks in the volumes that need room to churn — 64 MiB.
pub const BIG_BLOCKS: u64 = 16384;

/// The smallest volume `Store::format` actually completes on.
///
/// `format`'s own guard only requires `total_blocks > 8` (the 6 reserved header
/// blocks plus the 2 trailer blocks), but a 9–13 block volume then fails inside
/// the genesis commit — "read-only safe mode" at 9–10 and "out of space" at
/// 11–13 — because the volume has no room for the empty root's objects plus the
/// allocator snapshot that follows them. 14 is the first size that commits.
///
/// The guard and the commit therefore disagree, and the guard is the wrong one
/// of the two: it admits volumes that cannot work. The lifecycle tests pin the
/// real boundary, so the disagreement is visible rather than a caller's
/// surprise.
pub const MIN_BLOCKS: u64 = 14;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique(tag: &str) -> String {
	format!(
		"ledgerfs-test-{}-{}-{}",
		std::process::id(),
		SEQ.fetch_add(1, Ordering::SeqCst),
		tag
	)
}

// region:    --- Volumes in memory

/// A zeroed in-memory device of `blocks` blocks.
pub fn mem(blocks: u64) -> MemBlockDevice {
	MemBlockDevice::new(blocks)
}

/// A fresh volume of `blocks` blocks, formatted, with a genesis root.
pub fn store(blocks: u64) -> FsResult<Store> {
	Store::format(Box::new(mem(blocks)), blocks)
}

/// A fresh application store, so the `.ledgerfs/` identity markers exist.
pub fn app_store(blocks: u64, app: &str) -> FsResult<Store> {
	Store::format_app(Box::new(mem(blocks)), app)
}

/// A formatted volume *and* a live handle on the same buffer.
///
/// This is how a test reaches the bytes: a `Store` owns its device, so the way
/// to snapshot a volume — before a risky operation, or after one, to re-mount it
/// — is to keep the clone `MemBlockDevice` is documented to provide.
pub fn watched(blocks: u64) -> FsResult<(Store, MemBlockDevice)> {
	let device = mem(blocks);
	let watch = device.clone();
	Ok((Store::format(Box::new(device), blocks)?, watch))
}

/// A [`MemBlockDevice`] that counts the device traffic asked of it.
///
/// Every block-level helper on [`BlockDevice`] funnels into `read_bytes_at` and
/// `write_bytes_at`, so counting those two counts the volume's whole I/O. This
/// is here rather than inline in one test because the read path's cost is a
/// *count* and not a duration: a test that timed a read would measure the
/// machine it ran on, and this crate's test suite runs on whatever machine
/// happens to be running it.
#[derive(Clone)]
pub struct CountingDevice {
	inner: MemBlockDevice,
	reads: Arc<AtomicU64>,
	writes: Arc<AtomicU64>,
}

impl CountingDevice {
	pub fn new(blocks: u64) -> Self {
		Self {
			inner: MemBlockDevice::new(blocks),
			reads: Arc::new(AtomicU64::new(0)),
			writes: Arc::new(AtomicU64::new(0)),
		}
	}

	/// Device reads since the last [`reset`](Self::reset).
	pub fn reads(&self) -> u64 {
		self.reads.load(Ordering::Relaxed)
	}

	/// Device writes since the last [`reset`](Self::reset).
	pub fn writes(&self) -> u64 {
		self.writes.load(Ordering::Relaxed)
	}

	/// Zero both counters — so a test can measure one operation rather than the
	/// setup that preceded it.
	pub fn reset(&self) {
		self.reads.store(0, Ordering::Relaxed);
		self.writes.store(0, Ordering::Relaxed);
	}
}

impl BlockDevice for CountingDevice {
	fn size_bytes(&self) -> u64 {
		self.inner.size_bytes()
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> FsResult<()> {
		self.reads.fetch_add(1, Ordering::Relaxed);
		self.inner.read_bytes_at(offset, buf)
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> FsResult<()> {
		self.writes.fetch_add(1, Ordering::Relaxed);
		self.inner.write_bytes_at(offset, data)
	}

	fn barrier(&self) -> FsResult<()> {
		self.inner.barrier()
	}

	fn is_writable(&self) -> bool {
		self.inner.is_writable()
	}

	fn hint_cache(&self) -> Option<ledgerfs_host::HintCache> {
		self.inner.hint_cache()
	}
}

/// Mount `bytes` as a volume, the way a reopen after a restart does.
pub fn open_bytes(bytes: Vec<u8>) -> FsResult<Store> {
	Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))
}

/// Mount `bytes` and keep a live handle on the buffer it mounted.
///
/// The same trick as [`watched`], for a test that reopens a volume and then
/// keeps writing to it: a store is `moved` into `open`, so without the clone a
/// test would have to snapshot the bytes again at every step — and a snapshot
/// taken from the *old* handle would silently be stale, since the reopened
/// store is writing into its own copy.
pub fn open_bytes_watched(bytes: Vec<u8>) -> FsResult<(Store, MemBlockDevice)> {
	let device = MemBlockDevice::from_bytes(bytes)?;
	let watch = device.clone();
	Ok((Store::open(Box::new(device))?, watch))
}

/// Mount `bytes` read-only, erroring where a mount should (the `HostFs` path
/// that insists on a usable root).
pub fn mount_bytes(bytes: Vec<u8>) -> FsResult<HostFs> {
	HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(bytes)?))
}

/// The volume's bytes, as a valid volume file.
pub fn bytes(device: &MemBlockDevice) -> Vec<u8> {
	device.bytes_snapshot()
}

/// Rewrite one byte of `bytes` to `value` (0 when `value` is `None`, else the
/// XOR of the existing byte with it) — the smallest possible corruption.
pub fn patch(bytes: &mut [u8], offset: usize, value: u8) {
	bytes[offset] = value;
}

/// Read the four bytes at `offset` as a little-endian `u32`, for asserting on a
/// field the crate does not expose a getter for.
pub fn u32_at(bytes: &[u8], offset: usize) -> u32 {
	u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
}

/// The physical byte offset of block `lba`.
pub fn block_offset(lba: u64) -> usize {
	lba as usize * 4096
}

/// The four block addresses holding superblock copies, in the order the format
/// numbers them: two at the head of the volume and two at the tail.
///
/// Spelled out here rather than read from the crate because the tests that need
/// it are the ones that damage a copy behind the crate's back — the addresses
/// are part of the on-disk format, which is exactly what those tests are
/// pinning.
pub fn copy_lbas(total_blocks: u64) -> [u64; 4] {
	[0, 1, total_blocks - 2, total_blocks - 1]
}

// endregion:    --- Volumes in memory

// region:    --- Volumes in files

/// A temp image path, removed on drop.
///
/// The whole point is the `Drop`: a test that fails must not leave a 64 MiB
/// image in `/tmp` for the next run to step over. Removal is best-effort —
/// on Linux unlinking a file a `FileBlockDevice` still holds open is legal, and
/// the handle keeps reading its (now anonymous) inode.
pub struct TempImage {
	pub path: PathBuf,
}

impl TempImage {
	pub fn new(tag: &str) -> Self {
		Self {
			path: std::env::temp_dir().join(format!("{}.img", unique(tag))),
		}
	}

	/// Write `bytes` to the path, so an image can be handed to code that takes
	/// a path rather than a device.
	pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
		std::fs::write(&self.path, bytes)
	}

	/// A formatted volume in a file, opened read-write.
	pub fn store(&self, blocks: u64) -> FsResult<Store> {
		let device = FileBlockDevice::create(&self.path, blocks * 4096)?;
		Store::format(Box::new(device), blocks)
	}
}

impl Drop for TempImage {
	fn drop(&mut self) {
		let _ = std::fs::remove_file(&self.path);
	}
}

/// A file-backed device opened read-only over `bytes`.
pub fn ro_file(image: &TempImage, bytes: &[u8]) -> FsResult<FileBlockDevice> {
	image.write(bytes)?;
	FileBlockDevice::open(&image.path, 0, bytes.len() as u64)
}

/// A file-backed device opened read-write over `bytes`.
pub fn rw_file(image: &TempImage, bytes: &[u8]) -> FsResult<FileBlockDevice> {
	image.write(bytes)?;
	FileBlockDevice::open_rw(&image.path, 0, bytes.len() as u64)
}

// endregion:    --- Volumes in files

// region:    --- Assertions

/// Every path in the tree, as the walk reports them, sorted.
pub fn paths(entries: &[WalkEntry]) -> Vec<String> {
	let mut out: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
	out.sort();
	out
}

/// The names in one directory listing, sorted — the shape most tests want to
/// assert on, since `read_dir` makes no ordering promise.
pub fn names(entries: &[ledgerfs_host::Entry]) -> Vec<String> {
	let mut out: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
	out.sort();
	out
}

/// A directory's listing as `(name, kind)` pairs, sorted by name.
///
/// Takes a [`HostFs`] rather than a [`Store`] because the read path is shared:
/// a test that mounts a volume without writing to it (the read-only cases) has
/// no `Store` to hand, and both must produce the same listing. A `Store` call
/// site passes `store.fs()`.
pub fn listing(fs: &HostFs, path: &str) -> FsResult<Vec<(String, InodeKind)>> {
	let mut out: Vec<(String, InodeKind)> = fs.read_dir(path)?.into_iter().map(|e| (e.name, e.kind)).collect();
	out.sort_by(|a, b| a.0.cmp(&b.0));
	Ok(out)
}

/// The error a `Result` carried, panicking with `what` when it succeeded.
///
/// Generic over the error type rather than downcasting, so a test can hand it
/// either a `ledgerfs_host::Result` or a `Box<dyn Error>` one.
///
/// Tests assert on the *variant*, not on a `to_string()`: a message is for a
/// human and is allowed to change, while `Error::NoSuchFile` naming the missing
/// parent is a contract callers depend on.
pub fn expect_err<T, E: std::fmt::Debug>(what: &str, r: core::result::Result<T, E>) -> E {
	match r {
		Ok(_) => panic!("{what}: expected an error, got Ok"),
		Err(e) => e,
	}
}

/// A `NoSuchFile` that names `path` — the shape callers match on.
pub fn is_no_such_file(e: &Error, path: &str) -> bool {
	matches!(e, Error::NoSuchFile { path: p } if p == path)
}

// endregion:    --- Assertions

// region:    --- Fuzz support

/// xorshift64*, so a corruption sweep is reproducible from its seed without
/// pulling in a randomness crate. Determinism is the point: when a seed finds
/// something, that number goes in the test.
pub struct Rng(u64);

impl Rng {
	pub fn new(seed: u64) -> Self {
		// 0 is xorshift's fixed point; any other value has a full period.
		Self(seed | 1)
	}

	pub fn next_u64(&mut self) -> u64 {
		let mut x = self.0;
		x ^= x >> 12;
		x ^= x << 25;
		x ^= x >> 27;
		self.0 = x;
		x.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}

	/// A value in `0..bound`.
	pub fn below(&mut self, bound: usize) -> usize {
		if bound == 0 {
			return 0;
		}
		(self.next_u64() % bound as u64) as usize
	}

	/// A `u8` that is never the one at `byte` — so a "flip" always changes it.
	pub fn different_byte(&mut self, byte: u8) -> u8 {
		let delta = 1 + (self.next_u64() % 255) as u8;
		byte.wrapping_add(delta)
	}
}

// endregion:    --- Fuzz support
