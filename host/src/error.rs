//! Error type for the host-side LedgerFS reader.
//!
//! Hand-written `Display` with one match arm per variant, no
//! `thiserror`/`anyhow`: the Rust10x error pattern as this repo already
//! practises it in `ledgerfs/src/error.rs`.

use std::fmt::{self, Display};
use std::path::PathBuf;

use ledgerfs::Error as LedgerFsError;

/// The crate's result type: [`Error`] as the failure half.
pub type Result<T> = core::result::Result<T, Error>;

/// Everything that can fail while opening, mounting, reading, or writing a
/// volume from the host side.
#[derive(Debug)]
pub enum Error {
	// -- Command line
	/// The arguments given to `lfs-read`/`lfs-write` do not form a command.
	Usage(String),

	// -- Image & partition geometry
	/// The image file could not be opened, or is not the size it claims.
	ImageOpen {
		/// The image file the caller named.
		path: PathBuf,
		/// The open or read failure, as the operating system reported it.
		detail: String,
	},
	/// No MBR signature at byte 510: not a partitioned image.
	NoPartitionTable {
		/// The image file whose MBR sector was read.
		path: PathBuf,
	},
	/// The image has no partition of the requested kind.
	PartitionNotFound {
		/// The missing partition's mount-point label (`/system`, `/data`, ...).
		kind: String,
	},

	// -- Mount
	/// A superblock copy could not be turned into a `Superblock`.
	SuperblockUnreadable {
		/// Physical block of the copy that failed to decode.
		copy_lba: u64,
		/// The decode failure(s), joined when both head copies were tried.
		detail: String,
	},
	/// `mount_select` found no usable root, or the volume's geometry
	/// disagrees with its partition.
	Mount {
		/// What mount selection or the geometry check refused, and why.
		detail: String,
	},
	/// The superblocks are valid and say nothing has ever been committed to
	/// this volume, which is what the image initialiser leaves behind, not
	/// damage.
	VolumeUncommitted {
		/// The block count the valid superblock advertises.
		total_blocks: u64,
	},
	/// The active root object was located but would not decode.
	RootUnreadable {
		/// Why the root object could not be located or decoded.
		detail: String,
	},
	/// The volume's superblock declares `FEATURE_INCOMPAT_DATA_ENCRYPTED`, and
	/// this open supplied no credential.
	///
	/// Distinct from [`Mount`](Self::Mount) and from a decode failure because
	/// nothing is damaged: the volume is intact and unreadable on purpose. The
	/// caller's next step is [`Store::open_encrypted`](crate::Store::open_encrypted),
	/// not a repair.
	EncryptedVolume {
		/// Where the volume said it was encrypted, and what to do instead.
		detail: String,
	},

	// -- Encryption
	/// An encrypted-volume operation was asked of a store that was opened
	/// without a key.
	VolumeNotEncrypted {
		/// Which operation needed a key, and why this store has none.
		detail: String,
	},
	/// No keyslot opened with the credential that was supplied.
	///
	/// Covers a wrong passphrase, a wrong key-encryption key, and a credential
	/// whose kind has no slot on this volume. Deliberately one answer for all
	/// three: distinguishing "wrong passphrase" from "no such slot" tells an
	/// attacker which volumes are worth guessing against.
	WrongCredential {
		/// Which credential kind was tried.
		detail: String,
	},
	/// Every keyslot is occupied, so a new credential has nowhere to go.
	NoFreeKeySlot {
		/// How many slots the volume has, and what to do instead.
		detail: String,
	},
	/// A keyslot operation named a slot that holds nothing.
	KeySlotEmpty {
		/// Index of the empty slot.
		index: usize,
	},
	/// Revoking this slot would leave the volume with no credential at all.
	LastKeySlot {
		/// Index of the slot that was refused.
		index: usize,
	},
	/// A write to an encrypted volume was not a whole number of blocks.
	///
	/// Reads accept any range; writes do not, because a partial write would
	/// become a read-modify-write the caller did not ask for. See
	/// [`EncryptedDevice`](crate::EncryptedDevice).
	EncryptedWriteMisaligned {
		/// Byte offset of the refused write.
		offset: u64,
		/// Its length in bytes.
		len: usize,
		/// The block size it had to be a multiple of.
		block_size: u32,
	},
	/// The operating system's random source failed.
	///
	/// Fatal for the operation that needed it (a master key, a salt, a wrap
	/// IV): falling back to a weaker source would produce a volume that looks
	/// encrypted and is not.
	Entropy {
		/// What the OS reported.
		detail: String,
	},

	// -- Object access
	/// No `ObjectRecord` on the volume carries the requested `ObjectId`.
	ObjectNotFound {
		/// The missing object's id, in hex.
		object_id: String,
	},
	/// A read would fall outside the partition this device was opened on.
	OutOfBounds {
		/// Byte offset of the requested read or write.
		offset: u64,
		/// Length in bytes of the requested read or write.
		len: usize,
		/// Size of the partition the request must fall inside.
		size_bytes: u64,
	},
	/// The underlying file returned fewer bytes than `read_at` promised.
	ShortRead {
		/// Byte offset at which the short read occurred.
		offset: u64,
		/// Bytes still outstanding at that point.
		want: usize,
		/// Bytes actually delivered: zero when the file reported end-of-file.
		got: usize,
	},
	/// A length that has to be a whole number of blocks is not one.
	Misaligned {
		/// The operation that required block alignment.
		context: &'static str,
		/// The offending length in bytes.
		len: usize,
		/// The block size `len` had to be a multiple of.
		block_size: u32,
	},

	// -- Path resolution
	/// A path component that must be a directory turned out not to be one.
	NotADirectory {
		/// The path whose lookup hit the non-directory.
		path: String,
	},
	/// A file operation was handed a path to a directory.
	IsADirectory {
		/// The path that names a directory.
		path: String,
	},
	/// A path component does not exist.
	NoSuchFile {
		/// The path that could not be resolved.
		path: String,
	},
	/// A create was asked for a name that is already taken.
	AlreadyExists {
		/// The path whose creation was refused.
		path: String,
	},
	/// A staged operation would take a staging session past the capacity the
	/// session was opened with. The operation is refused and the session is
	/// left as it was: it is not a partial write, and it does not auto-seal.
	StagingFull {
		/// Session total in bytes the operation would have reached.
		wanted: u64,
		/// The session's byte budget, as it was opened.
		capacity: u64,
	},

	// -- Filesystem verbs
	/// An operation the store's contract does not cover, named as the caller
	/// called it. Currently the cross-directory `rename` and any `write_at`
	/// that would land inside a file's existing bytes; both refusals rather
	/// than corruptions, see [`Store::rename`](crate::Store::rename).
	Unsupported {
		/// What was refused, named in the caller's own terms.
		detail: String,
	},
	/// A file the store reads as text (an app-store marker) is not UTF-8.
	/// Surfaced rather than swallowed: a volume that calls itself an app store
	/// and then cannot say whose is worth a caller's attention.
	MarkerUnreadable {
		/// The marker file whose bytes were not valid UTF-8.
		path: String,
		/// The UTF-8 decoding failure.
		detail: String,
	},

	// -- Verification
	/// A file read back out of the volume is not the file that was written.
	ContentMismatch {
		/// The volume path whose content differed.
		path: String,
		/// The local file or image the read-back was compared against.
		local: String,
		/// What differed: sizes, the first differing byte, or entry counts.
		detail: String,
	},

	// -- Access
	/// A write was attempted on a volume that cannot take one: either the
	/// device was opened read-only, or there is no allocator snapshot on the
	/// volume. Both are stated in `detail`, because both arrive at the same
	/// place in `commit`.
	ReadOnly {
		/// Which precondition was missing: a read-only device, or no allocator
		/// snapshot on the volume.
		detail: String,
	},
	/// A second writer tried to open a volume an existing writer already holds.
	/// LedgerFS is **one writer per volume**: the allocator snapshot is not
	/// shareable, so two writable handles would place objects on the same
	/// "free" blocks. The lock that enforces this is an advisory OS file lock,
	/// taken by every writable [`FileBlockDevice`](crate::FileBlockDevice);
	/// a read-only open takes no lock and is always allowed.
	Locked {
		/// The volume file another handle already holds open for writing.
		path: PathBuf,
	},

	// -- Externals
	/// An operating-system I/O operation failed.
	Io(std::io::Error),
	/// The kernel-side LedgerFS code rejected the operation.
	Format(LedgerFsError),
}

impl Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Usage(detail) => write!(f, "{detail}"),
			Self::ImageOpen { path, detail } => {
				write!(f, "cannot open image {}: {detail}", path.display())
			}
			Self::NoPartitionTable { path } => {
				write!(
					f,
					"{} has no MBR partition table (bad signature at byte 510)",
					path.display()
				)
			}
			Self::PartitionNotFound { kind } => {
				write!(f, "image has no {kind} partition")
			}
			Self::SuperblockUnreadable { copy_lba, detail } => {
				write!(f, "superblock copy at block {copy_lba} is unreadable: {detail}")
			}
			Self::Mount { detail } => write!(f, "mount failed: {detail}"),
			Self::VolumeUncommitted { total_blocks } => write!(
				f,
				"this is a valid LedgerFS volume ({total_blocks} blocks) with no committed root; \
				 it has been initialised but never formatted, so there is nothing to mount"
			),
			Self::RootUnreadable { detail } => write!(f, "root object is unreadable: {detail}"),
			Self::EncryptedVolume { detail } => {
				write!(f, "this volume is encrypted: {detail}")
			}
			Self::VolumeNotEncrypted { detail } => {
				write!(f, "this volume is not encrypted: {detail}")
			}
			Self::WrongCredential { detail } => write!(f, "cannot unlock the volume: {detail}"),
			Self::NoFreeKeySlot { detail } => write!(f, "no free keyslot: {detail}"),
			Self::KeySlotEmpty { index } => write!(f, "keyslot {index} is empty"),
			Self::LastKeySlot { index } => write!(
				f,
				"keyslot {index} is the volume's only credential; revoking it would leave the \
				 volume with no way to unlock"
			),
			Self::EncryptedWriteMisaligned {
				offset,
				len,
				block_size,
			} => write!(
				f,
				"a write of {len} bytes at offset {offset} to an encrypted volume is not a whole \
				 number of {block_size}-byte blocks; reads accept any range, writes do not"
			),
			Self::Entropy { detail } => write!(f, "the OS random source failed: {detail}"),
			Self::ObjectNotFound { object_id } => {
				write!(f, "no object with id {object_id} on this volume")
			}
			Self::OutOfBounds {
				offset,
				len,
				size_bytes,
			} => write!(
				f,
				"read of {len} bytes at offset {offset} falls outside the {size_bytes}-byte partition"
			),
			Self::ShortRead { offset, want, got } => {
				write!(f, "short read at offset {offset}: wanted {want} bytes, got {got}")
			}
			Self::Misaligned {
				context,
				len,
				block_size,
			} => write!(
				f,
				"{context}: length {len} is not a multiple of the {block_size}-byte block size"
			),
			Self::NotADirectory { path } => write!(f, "{path} is not a directory"),
			Self::IsADirectory { path } => write!(f, "{path} is a directory"),
			Self::NoSuchFile { path } => write!(f, "no such file or directory: {path}"),
			Self::AlreadyExists { path } => write!(f, "already exists: {path}"),
			Self::StagingFull { wanted, capacity } => write!(
				f,
				"staging is full: this operation would take the session to {wanted} bytes of its \
				 {capacity}-byte budget; seal or end the session, or open a larger one"
			),
			Self::Unsupported { detail } => write!(f, "unsupported: {detail}"),
			Self::MarkerUnreadable { path, detail } => {
				write!(f, "{path} is not readable text: {detail}")
			}
			Self::ContentMismatch { path, local, detail } => {
				write!(f, "content mismatch: {path} does not match {local}; {detail}")
			}
			Self::ReadOnly { detail } => write!(f, "cannot write to this volume: {detail}"),
			Self::Locked { path } => write!(
				f,
				"{} is already open for writing by another handle; LedgerFS allows one \
				 writer per volume (open it read-only to read a live volume)",
				path.display()
			),
			Self::Io(e) => write!(f, "io error: {e}"),
			Self::Format(e) => write!(f, "ledgerfs: {e}"),
		}
	}
}

// region:    --- From impls

impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		Self::Io(e)
	}
}

impl From<LedgerFsError> for Error {
	fn from(e: LedgerFsError) -> Self {
		Self::Format(e)
	}
}

// endregion: --- From impls

// region:    --- Error boilerplate

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::Io(e) => Some(e),
			Self::Format(e) => Some(e),
			_ => None,
		}
	}
}

// endregion: --- Error boilerplate
