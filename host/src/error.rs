//! Error type for the host-side LedgerFS reader.
//!
//! Hand-written `Display` with one match arm per variant, no
//! `thiserror`/`anyhow` — the Rust10x error pattern as this repo already
//! practises it in `crucible-ledgerfs-kernel/src/error.rs` and
//! `forge-builder/src/error.rs`.

use std::fmt::{self, Display};
use std::path::PathBuf;

use crucible_ledgerfs_kernel::Error as LedgerFsError;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
	// -- Command line
	/// The arguments given to `lfs-read`/`lfs-write` do not form a command.
	Usage(String),

	// -- Image & partition geometry
	/// The image file could not be opened, or is not the size it claims.
	ImageOpen { path: PathBuf, detail: String },
	/// No MBR signature at byte 510 — not a partitioned Crucible image.
	NoPartitionTable { path: PathBuf },
	/// The image has no partition of the requested kind.
	PartitionNotFound { kind: String },

	// -- Mount
	/// A superblock copy could not be turned into a `Superblock`.
	SuperblockUnreadable { copy_lba: u64, detail: String },
	/// `mount_select` found no usable root, or the volume's geometry
	/// disagrees with its partition.
	Mount { detail: String },
	/// The superblocks are valid and say nothing has ever been committed to
	/// this volume — which is what `fs_init` leaves behind, not damage.
	VolumeUncommitted { total_blocks: u64 },
	/// The active root object was located but would not decode.
	RootUnreadable { detail: String },

	// -- Object access
	/// No `ObjectRecord` on the volume carries the requested `ObjectId`.
	ObjectNotFound { object_id: String },
	/// A read would fall outside the partition this device was opened on.
	OutOfBounds {
		offset: u64,
		len: usize,
		size_bytes: u64,
	},
	/// The underlying file returned fewer bytes than `read_at` promised.
	ShortRead {
		offset: u64,
		want: usize,
		got: usize,
	},
	/// A length that has to be a whole number of blocks is not one.
	Misaligned {
		context: &'static str,
		len: usize,
		block_size: u32,
	},

	// -- Path resolution
	NotADirectory { path: String },
	IsADirectory { path: String },
	NoSuchFile { path: String },
	/// A create was asked for a name that is already taken.
	AlreadyExists { path: String },

	// -- Filesystem verbs
	/// An operation the store's contract does not cover, named as the caller
	/// called it. Currently the cross-directory `rename` and any `write_at`
	/// that would land inside a file's existing bytes — both refusals rather
	/// than corruptions, see [`Store::rename`](crate::Store::rename).
	Unsupported { detail: String },
	/// A file the store reads as text — an app-store marker — is not UTF-8.
	/// Surfaced rather than swallowed: a volume that calls itself an app store
	/// and then cannot say whose is worth a caller's attention.
	MarkerUnreadable { path: String, detail: String },

	// -- Verification
	/// A file read back out of the volume is not the file that was written.
	ContentMismatch {
		path: String,
		local: String,
		detail: String,
	},

	// -- Access
	/// A write was attempted on a volume that cannot take one — either the
	/// device was opened read-only, or there is no allocator snapshot on the
	/// volume. Both are stated in `detail`, because both arrive at the same
	/// place in `commit`.
	ReadOnly { detail: String },
	/// A second writer tried to open a volume an existing writer already holds.
	/// LedgerFS is **one writer per volume** — the allocator snapshot is not
	/// shareable, so two writable handles would place objects on the same
	/// "free" blocks. The lock that enforces this is an advisory OS file lock,
	/// taken by every writable [`FileBlockDevice`](crate::FileBlockDevice);
	/// a read-only open takes no lock and is always allowed.
	Locked { path: PathBuf },

	// -- Externals
	Io(std::io::Error),
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
				write!(f, "{} has no MBR partition table (bad signature at byte 510)", path.display())
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
				"this is a valid LedgerFS volume ({total_blocks} blocks) with no committed root — \
				 it has been initialised but never formatted, so there is nothing to mount"
			),
			Self::RootUnreadable { detail } => write!(f, "root object is unreadable: {detail}"),
			Self::ObjectNotFound { object_id } => {
				write!(f, "no object with id {object_id} on this volume")
			}
			Self::OutOfBounds { offset, len, size_bytes } => write!(
				f,
				"read of {len} bytes at offset {offset} falls outside the {size_bytes}-byte partition"
			),
			Self::ShortRead { offset, want, got } => {
				write!(f, "short read at offset {offset}: wanted {want} bytes, got {got}")
			}
			Self::Misaligned { context, len, block_size } => write!(
				f,
				"{context}: length {len} is not a multiple of the {block_size}-byte block size"
			),
			Self::NotADirectory { path } => write!(f, "{path} is not a directory"),
			Self::IsADirectory { path } => write!(f, "{path} is a directory"),
			Self::NoSuchFile { path } => write!(f, "no such file or directory: {path}"),
			Self::AlreadyExists { path } => write!(f, "already exists: {path}"),
			Self::Unsupported { detail } => write!(f, "unsupported: {detail}"),
			Self::MarkerUnreadable { path, detail } => {
				write!(f, "{path} is not readable text: {detail}")
			}
			Self::ContentMismatch { path, local, detail } => write!(
				f,
				"content mismatch: {path} does not match {local} — {detail}"
			),
			Self::ReadOnly { detail } => write!(f, "cannot write to this volume: {detail}"),
			Self::Locked { path } => write!(
				f,
				"{} is already open for writing by another handle — LedgerFS allows one \
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
