//! Where a LedgerFS volume physically lives.
//!
//! [`BlockDevice`] is the seam. A volume is a flat array of 4 KiB blocks and
//! nothing above this module cares what backs it, so the same store runs over
//! an OS file, a partition inside a disk image, an in-memory buffer, or
//! whatever else an embedding application has. That is the difference between
//! "a drive can be formatted" and "an application can embed it".
//!
//! # Why every method takes `&self`
//!
//! Positioned I/O does not need a mutable handle: `pread`/`pwrite` on Unix and
//! `seek_read`/`seek_write` on Windows both take `&self`, and a `Mutex` gives
//! the same for a buffer. Keeping
//! `&self` throughout means a device can be shared behind an `&`, which is
//! what `ObjectStore`'s `&self` methods require, and it keeps the read and
//! write halves on one handle instead of the two parallel ones the kernel
//! crate needs (`commit::BlockDevice` is write-only; `reader.rs` reads
//! separately).
//!
//! # Offsets
//!
//! Offsets are **volume-relative**: `base_offset` is added on the way down to
//! the underlying storage. Callers work in blocks (`lba * 4096`) or
//! volume-relative bytes, never in absolute file offsets. A whole-file volume
//! is simply `base_offset = 0`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ledgerfs::BLOCK_SIZE;

use crate::error::{Error, Result};

// region:    --- Positioned I/O

/// Positioned read: reads at an explicit offset without consulting or moving
/// the file cursor, so concurrent positioned I/O on one handle is safe.
#[cfg(unix)]
fn positioned_read(file: &File, at: u64, buf: &mut [u8]) -> io::Result<usize> {
	use std::os::unix::fs::FileExt;
	file.read_at(buf, at)
}

/// Windows' `seek_read` carries its offset as an argument (an `OVERLAPPED`
/// under the hood). The docs warn that it also moves the handle's file
/// pointer; that is irrelevant here, because nothing in this crate ever reads
/// the cursor, every byte moves through `positioned_read` and
/// `positioned_write`, both of which carry their offset explicitly.
#[cfg(windows)]
fn positioned_read(file: &File, at: u64, buf: &mut [u8]) -> io::Result<usize> {
	use std::os::windows::fs::FileExt;
	file.seek_read(buf, at)
}

/// Positioned write: the write-side twin of [`positioned_read`].
#[cfg(unix)]
fn positioned_write(file: &File, at: u64, data: &[u8]) -> io::Result<usize> {
	use std::os::unix::fs::FileExt;
	file.write_at(data, at)
}

#[cfg(windows)]
fn positioned_write(file: &File, at: u64, data: &[u8]) -> io::Result<usize> {
	use std::os::windows::fs::FileExt;
	file.seek_write(data, at)
}

// endregion: --- Positioned I/O

// region:    --- BlockDevice

/// What makes a volume *this* volume, for telling whether a hint cache found
/// beside it still describes it.
///
/// The size alone is not enough: a `compact` writes a rebuilt file that is
/// usually smaller but can be the same size, and a re-format in place is the
/// same size by definition. The `(device, inode)` pair settles both: a
/// `rename` into place produces a new inode, and a truncating re-format in
/// place is handled at the source (`Store::format` removes the sidecar).
///
/// Compared for equality; nothing about it is a security boundary, because
/// every hint is re-verified against the block it names before it is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeIdentity {
	/// The volume's size in bytes.
	pub size_bytes: u64,
	/// Device id of the file behind the volume; zero where the platform has
	/// none.
	pub dev: u64,
	/// Inode number of the file behind the volume; the file's creation time on
	/// Windows.
	pub ino: u64,
}

impl VolumeIdentity {
	/// The identity of an open file, or of a plain size for a device with no
	/// file behind it.
	pub(crate) fn of_file(file: &File, size_bytes: u64) -> Self {
		// The platform's best *stable* identity pair: `(device, inode)` on
		// Unix, the file's creation time on Windows (`file_index`, the closer
		// analogue of an inode, is still unstable `windows_by_handle`). Unix's
		// inode is the stronger of the two: a rebuilt-and-renamed volume can
		// keep its creation time on NTFS, so there the check can miss a
		// `compact` that lands on the same size. It is an efficiency measure
		// either way: every hint is re-verified against the block it names
		// before use, so a stale cache costs one failed read, never a wrong
		// answer.
		match file.metadata() {
			Ok(m) => {
				#[cfg(unix)]
				use std::os::unix::fs::MetadataExt;
				#[cfg(unix)]
				let (dev, ino) = (m.dev(), m.ino());
				#[cfg(windows)]
				use std::os::windows::fs::MetadataExt;
				#[cfg(windows)]
				let (dev, ino) = (0, m.creation_time());
				#[cfg(not(any(unix, windows)))]
				let (dev, ino) = (0, 0);
				Self { size_bytes, dev, ino }
			}
			Err(_) => Self {
				size_bytes,
				dev: 0,
				ino: 0,
			},
		}
	}
}

/// Where a device keeps its persistent hint cache, if it keeps one.
///
/// A name a caller could derive for itself, but returning it from the device
/// keeps the knowledge where the file path already is, and lets a device with
/// no file behind it answer `None` and opt out of the cache entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintCache {
	/// The sidecar file.
	pub path: PathBuf,
	/// The volume the cache describes.
	pub volume: VolumeIdentity,
}

/// Block-addressed storage a LedgerFS volume can live on.
///
/// The read/write pair plus [`barrier`](BlockDevice::barrier) is the whole
/// contract; `read_block`/`write_blocks` and friends are provided in terms of
/// them. `barrier` is the host equivalent of the kernel's
/// `storage_barrier`: without it a "committed" transaction can still be
/// sitting in a page cache, which is exactly the window the commit protocol
/// exists to close.
pub trait BlockDevice: Send + Sync {
	/// Size of the volume in bytes.
	fn size_bytes(&self) -> u64;

	/// Read exactly `buf.len()` bytes at volume-relative `offset`.
	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

	/// Write all of `data` at volume-relative `offset`.
	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> Result<()>;

	/// Make everything written so far durable.
	fn barrier(&self) -> Result<()>;

	/// Whether this device will accept a write.
	///
	/// Not decoration. A handle opened read-only does not fail where it is
	/// opened; it fails several layers down, as `EBADF` from the first
	/// `write_at`, naming neither the device nor the reason, and for a volume
	/// being modified that is a confusing way to learn a file was opened with
	/// the wrong flags. Answering the question here lets the store refuse up
	/// front, with something the caller can act on.
	fn is_writable(&self) -> bool {
		true
	}

	/// The persistent hint cache beside this device's volume, or `None` when
	/// there is nowhere to put one.
	///
	/// The default is `None`: a buffer in the caller's heap has no name, and
	/// needs none: its id→block map lives exactly as long as the handle that
	/// built it, so there is no cold start to pay for. A file-backed device
	/// returns a sidecar beside the volume, which is what turns the ~0.8 s
	/// whole-volume scan of a first open into a file read. See `hints.rs`.
	fn hint_cache(&self) -> Option<HintCache> {
		None
	}

	/// Size of the volume in whole blocks. A trailing partial block is not
	/// addressable and is not counted.
	fn block_count(&self) -> u64 {
		self.size_bytes() / BLOCK_SIZE as u64
	}

	/// The volume-relative byte offset of block `lba`, or an out-of-bounds
	/// refusal when there is no such block.
	///
	/// Every block-addressed method goes through here rather than multiplying
	/// for itself, because `lba * BLOCK_SIZE` overflows for an LBA near
	/// `u64::MAX` and *why* it must not is the whole point of this method:
	///
	/// - with overflow checks on, the multiply panics. The LBA a read asks for
	///   comes from the volume (an object reference, an extent node, a hint),
	///   so for a volume of unknown provenance it is attacker-chosen, and a
	///   panic is the process that mounted the volume going down. A read must
	///   be able to say "no";
	/// - with them off, the product wraps, and the wrapped value can land
	///   *inside* the volume, so the bounds check that follows would approve a
	///   read of a block the caller never named. A silent wrong answer is worse
	///   than either.
	///
	/// The comparison is against `block_count()` rather than a saturating
	/// multiply, so the numbers in the refusal are the caller's own LBA and the
	/// volume's real size, and no arithmetic on the way to the error can itself
	/// misbehave.
	fn offset_of_block(&self, lba: u64) -> Result<u64> {
		if lba >= self.block_count() {
			return Err(Error::OutOfBounds {
				offset: lba.saturating_mul(BLOCK_SIZE as u64),
				len: BLOCK_SIZE as usize,
				size_bytes: self.size_bytes(),
			});
		}
		Ok(lba * BLOCK_SIZE as u64)
	}

	/// Read one block at `lba`.
	fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<()> {
		if buf.len() < BLOCK_SIZE as usize {
			return Err(Error::OutOfBounds {
				offset: lba.saturating_mul(BLOCK_SIZE as u64),
				len: buf.len(),
				size_bytes: self.size_bytes(),
			});
		}
		let offset = self.offset_of_block(lba)?;
		self.read_bytes_at(offset, &mut buf[..BLOCK_SIZE as usize])
	}

	/// Read `buf.len()` bytes starting at block `lba` (may span many blocks).
	fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<()> {
		let offset = self.offset_of_block(lba)?;
		self.read_bytes_at(offset, buf)
	}

	/// Write one block at `lba`, padding `data` to a full block if shorter.
	fn write_block(&self, lba: u64, data: &[u8]) -> Result<()> {
		let offset = self.offset_of_block(lba)?;
		if data.len() >= BLOCK_SIZE as usize {
			return self.write_bytes_at(offset, &data[..BLOCK_SIZE as usize]);
		}
		let mut padded = vec![0u8; BLOCK_SIZE as usize];
		padded[..data.len()].copy_from_slice(data);
		self.write_bytes_at(offset, &padded)
	}

	/// Write `data` at `lba`. Must be a whole number of blocks: a partial
	/// final block would be an unaddressable half-written block, so it is
	/// refused rather than padded.
	fn write_blocks(&self, lba: u64, data: &[u8]) -> Result<()> {
		if !data.len().is_multiple_of(BLOCK_SIZE as usize) {
			return Err(Error::Misaligned {
				context: "write_blocks",
				len: data.len(),
				block_size: BLOCK_SIZE,
			});
		}
		// The LBA is resolved even for an empty write, so a caller that has lost
		// track of its position gets the bounds error rather than a silent
		// success at an offset nothing was written to.
		let offset = self.offset_of_block(lba)?;
		self.write_bytes_at(offset, data)
	}

	/// Volume-relative bounds check shared by the implementations.
	fn check_bounds(&self, offset: u64, len: usize) -> Result<()> {
		let end = (offset as u128) + (len as u128);
		if end > self.size_bytes() as u128 {
			return Err(Error::OutOfBounds {
				offset,
				len,
				size_bytes: self.size_bytes(),
			});
		}
		Ok(())
	}
}

// endregion: --- BlockDevice

// region:    --- FileBlockDevice

/// A volume inside an OS file, optionally at an offset: which is what makes
/// one type serve both a bare volume file and a partition inside a disk image.
pub struct FileBlockDevice {
	file: File,
	/// The path the file was opened at, kept for the hint cache's sidecar name
	/// and for the messages that have to name the file (see `from_file`).
	path: PathBuf,
	base_offset: u64,
	size_bytes: u64,
	/// Whether the file was opened with write access. `File` does not expose
	/// its own open mode, so it is carried here; see
	/// [`BlockDevice::is_writable`].
	writable: bool,
}

impl FileBlockDevice {
	/// Open an existing region read-only.
	pub fn open(path: &Path, base_offset: u64, size_bytes: u64) -> Result<Self> {
		let file = OpenOptions::new().read(true).open(path).map_err(|e| Error::ImageOpen {
			path: path.to_path_buf(),
			detail: e.to_string(),
		})?;
		Self::from_file(file, path, base_offset, size_bytes, false)
	}

	/// Open a region for reading and writing.
	pub fn open_rw(path: &Path, base_offset: u64, size_bytes: u64) -> Result<Self> {
		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.open(path)
			.map_err(|e| Error::ImageOpen {
				path: path.to_path_buf(),
				detail: e.to_string(),
			})?;
		Self::from_file(file, path, base_offset, size_bytes, true)
	}

	/// Make a file of `size_bytes` and open it as a whole volume.
	///
	/// The `mkfs` entry point, and the one place a volume's geometry is chosen
	/// rather than discovered. Creating and re-sizing are the same operation
	/// here, because "make a volume this big at this path" has no meaning that
	/// preserves the old size.
	///
	/// **This sizes the file; it does not erase it.** The erase belongs to
	/// [`Store::format`](crate::Store::format), which owns it for every kind of
	/// device: a partition formatted in place had no `create` call at all, so a
	/// guarantee made here would hold for bare volume files and silently not for
	/// partitions. Nor would `set_len` do it anyway: shrinking an 8-block file to
	/// 4 keeps block 3 and whatever it held.
	///
	/// A caller that wants to open an existing volume *at its own size* wants
	/// [`open`](Self::open) or [`open_rw`](Self::open_rw), which read the length
	/// off the file and leave it alone.
	pub fn create(path: &Path, size_bytes: u64) -> Result<Self> {
		if !size_bytes.is_multiple_of(BLOCK_SIZE as u64) {
			return Err(Error::Misaligned {
				context: "FileBlockDevice::create",
				len: size_bytes as usize,
				block_size: BLOCK_SIZE,
			});
		}

		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(path)
			.map_err(|e| Error::ImageOpen {
				path: path.to_path_buf(),
				detail: e.to_string(),
			})?;

		file.set_len(size_bytes).map_err(Error::Io)?;

		Self::from_file(file, path, 0, size_bytes, true)
	}

	fn from_file(file: File, path: &Path, base_offset: u64, size_bytes: u64, writable: bool) -> Result<Self> {
		let image_bytes = file.metadata().map_err(Error::Io)?.len();

		// Catch bad geometry at open time, where the message can name the
		// file, instead of on the first read, where it cannot.
		let end = base_offset.checked_add(size_bytes).ok_or_else(|| Error::ImageOpen {
			path: path.to_path_buf(),
			detail: "offset + size overflows u64".into(),
		})?;
		if end > image_bytes {
			return Err(Error::ImageOpen {
				path: path.to_path_buf(),
				detail: format!("volume spans bytes {base_offset}..{end} but the file is only {image_bytes} bytes"),
			});
		}

		// One writer per volume. A writable open takes an exclusive OS file
		// lock; a second writable handle (another process, or a second open in
		// this one) is refused with `Error::Locked` rather than allowed to
		// place objects on blocks the first handle still counts as free.
		//
		// Read-only opens take no lock. On Unix that lock is `flock`, which is
		// advisory, so a read-only inspector reads a volume a live writer
		// holds. On Windows the same call is `LockFileEx`, whose locks are
		// mandatory, so a read through a second handle is refused while the
		// writer exists (`KNOWN_ISSUES.md` §13). The refusal is the OS's, not
		// this crate's, and it is pinned by the unit test below rather than
		// left to this comment.
		if writable {
			match file.try_lock() {
				Ok(()) => {}
				Err(std::fs::TryLockError::WouldBlock) => {
					return Err(Error::Locked {
						path: path.to_path_buf(),
					});
				}
				Err(std::fs::TryLockError::Error(e)) => {
					return Err(Error::ImageOpen {
						path: path.to_path_buf(),
						detail: format!("could not lock for writing: {e}"),
					});
				}
			}
		}

		Ok(Self {
			file,
			path: path.to_path_buf(),
			base_offset,
			size_bytes,
			writable,
		})
	}

	/// Byte offset of the volume within the file.
	pub fn base_offset(&self) -> u64 {
		self.base_offset
	}
}

impl BlockDevice for FileBlockDevice {
	fn size_bytes(&self) -> u64 {
		self.size_bytes
	}

	fn hint_cache(&self) -> Option<HintCache> {
		// A partition's sidecar is named for the region, not the file: two
		// volumes in one disk image are two different index spaces, and a
		// cache written for one must never be offered to the other. A
		// whole-file volume (`base_offset == 0`) gets the plain `.hints`
		// suffix, which is the common case and the one worth reading aloud.
		let mut name = self.path.file_name()?.to_os_string();
		if self.base_offset == 0 {
			name.push(".hints");
		} else {
			name.push(format!(".{}.hints", self.base_offset));
		}
		let path = self.path.with_file_name(name);
		Some(HintCache {
			path,
			volume: VolumeIdentity::of_file(&self.file, self.size_bytes),
		})
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
		self.check_bounds(offset, buf.len())?;

		// A positioned read may return short; a single call is not a full read.
		let mut done = 0usize;
		while done < buf.len() {
			let at = self.base_offset + offset + done as u64;
			let n = positioned_read(&self.file, at, &mut buf[done..]).map_err(Error::Io)?;
			if n == 0 {
				return Err(Error::ShortRead {
					offset: offset + done as u64,
					want: buf.len() - done,
					got: 0,
				});
			}
			done += n;
		}
		Ok(())
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> Result<()> {
		// Refused here rather than left to the OS: a write on a read-only
		// handle fails deep in the OS (`EBADF` on Unix,
		// `ERROR_ACCESS_DENIED` on Windows), which is accurate and tells the
		// caller nothing.
		if !self.writable {
			return Err(Error::ReadOnly {
				detail: "the device was opened read-only".to_string(),
			});
		}

		self.check_bounds(offset, data.len())?;

		let mut done = 0usize;
		while done < data.len() {
			let at = self.base_offset + offset + done as u64;
			let n = positioned_write(&self.file, at, &data[done..]).map_err(Error::Io)?;
			if n == 0 {
				return Err(Error::ShortRead {
					offset: offset + done as u64,
					want: data.len() - done,
					got: 0,
				});
			}
			done += n;
		}
		Ok(())
	}

	fn barrier(&self) -> Result<()> {
		self.file.sync_all().map_err(Error::Io)
	}

	fn is_writable(&self) -> bool {
		self.writable
	}
}

// endregion: --- FileBlockDevice

// region:    --- MemBlockDevice

/// A volume in a buffer, for embedding a store with no file behind it.
///
/// This is the "an application can easily embed it" case taken literally: a
/// process that wants a LedgerFS-backed store gets one in its own heap, with
/// the same commit protocol and the same on-disk format.
///
/// Cloneable, and the clones share one buffer: a [`Store`] takes ownership of
/// its device, so the way to get the bytes back afterwards (or to snapshot a
/// volume while a store is still live) is to keep a clone.
///
/// [`Store`]: crate::Store
/// [`bytes_snapshot`]: MemBlockDevice::bytes_snapshot
#[derive(Clone)]
pub struct MemBlockDevice {
	/// `Arc<Mutex<_>>`, not `RefCell`: the same interior mutability a `File`
	/// gets from positioned I/O, but also `Send + Sync`, so an embedded store
	/// can be shared and its buffer still reached from outside.
	bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemBlockDevice {
	/// A zeroed volume of `block_count` blocks.
	pub fn new(block_count: u64) -> Self {
		let bytes = vec![0u8; block_count as usize * BLOCK_SIZE as usize];
		Self {
			bytes: Arc::new(Mutex::new(bytes)),
		}
	}

	/// Adopt an existing buffer as a volume.
	///
	/// The buffer's length defines the volume size, so a truncated one is
	/// refused here rather than surfacing as a short read much later.
	pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
		if !bytes.len().is_multiple_of(BLOCK_SIZE as usize) {
			return Err(Error::Misaligned {
				context: "MemBlockDevice::from_bytes",
				len: bytes.len(),
				block_size: BLOCK_SIZE,
			});
		}
		Ok(Self {
			bytes: Arc::new(Mutex::new(bytes)),
		})
	}

	/// A copy of the volume's bytes: a valid volume file, writable to disk or
	/// re-openable with [`from_bytes`](MemBlockDevice::from_bytes).
	///
	/// The lock is recovered rather than propagated on poisoning: the buffer is
	/// a flat `Vec<u8>` whose every mutation is a bounds-checked slice copy, so
	/// there is no half-updated structure to protect, and turning "some other
	/// thread panicked once" into a panic *here* would take down a device that
	/// is still perfectly readable.
	pub fn bytes_snapshot(&self) -> Vec<u8> {
		self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
	}
}

impl BlockDevice for MemBlockDevice {
	fn size_bytes(&self) -> u64 {
		self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len() as u64
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
		self.check_bounds(offset, buf.len())?;
		let bytes = self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let start = offset as usize;
		buf.copy_from_slice(&bytes[start..start + buf.len()]);
		Ok(())
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> Result<()> {
		self.check_bounds(offset, data.len())?;
		let mut bytes = self.bytes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		let start = offset as usize;
		bytes[start..start + data.len()].copy_from_slice(data);
		Ok(())
	}

	fn barrier(&self) -> Result<()> {
		// Nothing to flush: the buffer *is* the durable copy.
		Ok(())
	}
}

// endregion: --- MemBlockDevice

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;

	type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

	#[test]
	fn test_device_mem_round_trips_blocks() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(4);

		// -- Exec
		device.write_block(2, b"ledgerfs")?;

		// -- Check
		let mut buf = [0u8; BLOCK_SIZE as usize];
		device.read_block(2, &mut buf)?;
		assert_eq!(&buf[..8], b"ledgerfs");
		assert!(buf[8..].iter().all(|b| *b == 0), "write_block must pad to a full block");
		assert_eq!(device.block_count(), 4);

		Ok(())
	}

	#[test]
	fn test_device_refuses_misaligned_write_blocks() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(4);

		// -- Exec
		let err = device.write_blocks(0, &[0u8; 100]).unwrap_err();

		// -- Check
		assert!(matches!(err, Error::Misaligned { len: 100, .. }), "got {err:?}");

		Ok(())
	}

	#[test]
	fn test_device_refuses_a_volume_past_the_end() -> Result<()> {
		// -- Setup & Fixtures
		let device = MemBlockDevice::new(1);

		// -- Exec
		let err = device.read_bytes_at(BLOCK_SIZE as u64, &mut [0u8; 1]).unwrap_err();

		// -- Check
		assert!(matches!(err, Error::OutOfBounds { .. }), "got {err:?}");

		Ok(())
	}

	#[test]
	fn test_device_from_bytes_rejects_a_truncated_volume() -> Result<()> {
		// -- Setup & Fixtures
		let bytes = vec![0u8; BLOCK_SIZE as usize - 1];

		// -- Exec
		let err = MemBlockDevice::from_bytes(bytes)
			.err()
			.expect("a truncated volume must be refused");

		// -- Check
		assert!(matches!(err, Error::Misaligned { .. }), "got {err:?}");

		Ok(())
	}

	/// A file path unique to one test, removed by that test.
	fn temp_path(tag: &str) -> std::path::PathBuf {
		std::env::temp_dir().join(format!("ledgerfs-{}-{tag}.img", std::process::id()))
	}

	#[test]
	fn test_device_file_round_trips_blocks_and_resizes() -> Result<()> {
		// -- Setup & Fixtures: `create` is mkfs, so it fixes the size.
		let path = temp_path("round-trip");
		let device = FileBlockDevice::create(&path, 8 * BLOCK_SIZE as u64)?;
		assert_eq!(device.block_count(), 8);

		// -- Exec
		device.write_block(3, b"file-backed")?;
		device.barrier()?;

		// -- Check: legible through a second, independent handle. On Unix a
		// read-only open takes no lock, so a reader coexists with the writer.
		// On Windows the writer's lock is mandatory and the read is refused
		// (`KNOWN_ISSUES.md` §13); both outcomes are asserted where they hold,
		// so a Windows run sees the documented limitation as a pass rather
		// than as an unexplained I/O error.
		let reopened = FileBlockDevice::open(&path, 0, device.size_bytes())?;
		let mut buf = [0u8; BLOCK_SIZE as usize];
		#[cfg(unix)]
		{
			reopened.read_block(3, &mut buf)?;
			assert_eq!(&buf[..11], b"file-backed");
		}
		#[cfg(windows)]
		{
			let e = reopened
				.read_block(3, &mut buf)
				.expect_err("a mandatory lock must refuse the read");
			assert!(matches!(e, Error::Io(_)), "got {e:?}");
		}

		// ...but a second *writable* handle on the same image is refused while
		// this one is held. One writer per volume, and the device cannot tell
		// where one volume's allocator ends and a neighbouring partition
		// begins, so the lock it takes is per file.
		let locked = FileBlockDevice::open_rw(&path, 0, device.size_bytes())
			.err()
			.expect("a second writable handle on a held image must be refused");
		assert!(matches!(locked, Error::Locked { .. }), "got {locked:?}");

		// -- Exec & Check: releasing the writer releases the lock, and a second
		// `create` then re-sizes. It is a sizing call and not `mkfs`: erasing
		// the medium is `Store::format`'s guarantee, so that it holds for a
		// partition too, and re-fixing the length is all that happens here.
		drop(device);
		let resized = FileBlockDevice::create(&path, 4 * BLOCK_SIZE as u64)?;
		assert_eq!(resized.block_count(), 4);

		let _ = std::fs::remove_file(&path);
		Ok(())
	}

	#[test]
	fn test_device_file_offsets_a_partition_window() -> Result<()> {
		// One type serving both a bare volume and a partition inside an image:
		// the only difference is `base_offset`.
		let path = temp_path("window");
		let image = FileBlockDevice::create(&path, 8 * BLOCK_SIZE as u64)?;
		image.write_block(0, b"not the volume")?;
		// The lock is per file, so the image-wide writer has to be released
		// before a window on it can be opened for writing, which is the point:
		// one writer per volume, and the device cannot see where a volume's
		// allocator ends and the next partition begins.
		drop(image);

		// -- Exec: `open_rw`, not `open`: a read-only handle turns the write
		// below into `EBADF` several layers down, from a device that opened
		// without complaint.
		let window = FileBlockDevice::open_rw(&path, 2 * BLOCK_SIZE as u64, 4 * BLOCK_SIZE as u64)?;
		window.write_block(0, b"volume block 0")?;
		window.barrier()?;
		drop(window);

		// -- Check: the write landed at the window's start, not the file's.
		let image = FileBlockDevice::open(&path, 0, 8 * BLOCK_SIZE as u64)?;
		let mut buf = [0u8; BLOCK_SIZE as usize];
		image.read_block(2, &mut buf)?;
		assert_eq!(&buf[..14], b"volume block 0");
		image.read_block(0, &mut buf)?;
		assert_eq!(&buf[..14], b"not the volume");

		let _ = std::fs::remove_file(&path);
		Ok(())
	}

	#[test]
	fn test_device_refuses_a_write_when_opened_read_only() -> Result<()> {
		// A read-only handle is legal (every reader in this crate uses one), so
		// so the failure has to come from the write, and it has to explain
		// itself. Left to the OS it arrives as `EBADF`, which says neither
		// which file was opened how nor that opening it differently would work.
		let path = temp_path("read-only");
		FileBlockDevice::create(&path, 4 * BLOCK_SIZE as u64)?;
		let device = FileBlockDevice::open(&path, 0, 4 * BLOCK_SIZE as u64)?;
		assert!(!device.is_writable());

		// -- Exec
		let err = device.write_block(0, b"nope").unwrap_err();

		// -- Check
		assert!(matches!(err, Error::ReadOnly { .. }), "got {err:?}");

		let _ = std::fs::remove_file(&path);
		Ok(())
	}

	#[test]
	fn test_device_file_refuses_a_window_past_the_end() -> Result<()> {
		// -- Setup & Fixtures
		let path = temp_path("geometry");
		FileBlockDevice::create(&path, 4 * BLOCK_SIZE as u64)?;

		// -- Exec: caught at open time, where the message can name the file,
		// rather than on the first read, where it cannot.
		let err = FileBlockDevice::open(&path, BLOCK_SIZE as u64, 8 * BLOCK_SIZE as u64)
			.err()
			.expect("a window past the end of the file must be refused");

		// -- Check
		assert!(matches!(err, Error::ImageOpen { .. }), "got {err:?}");

		let _ = std::fs::remove_file(&path);
		Ok(())
	}
}

// endregion: --- Tests
