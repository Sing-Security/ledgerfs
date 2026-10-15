//! MBR partition table parsing.
//!
//! Reads the partitioning the image already carries rather than recomputing
//! the disk layout's formula from the disk size. The two agree today, but only
//! the former stays correct for an image written by an installer with an
//! explicit system-partition size, or a real disk formatted on target — and it
//! is ~20 lines.
//!
//! An image carries three entries: the boot partition as type `0x83`, then the
//! system and data volumes as type `0xC0` (`PART_TYPE_LEDGERFS`), in that
//! order.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};

/// Size of one MBR sector, and of every partition offset unit below.
pub const SECTOR_SIZE: u64 = 512;

/// Byte offset of the four 16-byte partition entries within the MBR sector.
const MBR_PART_TABLE_OFFSET: usize = 446;
const MBR_ENTRY_COUNT: usize = 4;
const MBR_ENTRY_SIZE: usize = 16;

/// `0x55AA` at bytes 510..512, the MBR boot signature.
const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// Partition type byte for `/boot` (Linux, per `PART_TYPE_BOOT`).
pub const PART_TYPE_BOOT: u8 = 0x83;

/// Partition type byte for a LedgerFS volume (`PART_TYPE_LEDGERFS`).
pub const PART_TYPE_LEDGERFS: u8 = 0xC0;

/// Which of the three image partitions an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKind {
	/// The FAT/EFI-bootable partition. Not a LedgerFS volume.
	Boot,
	/// The sealed system volume — populated and committed when the image is
	/// built.
	System,
	/// The writable data volume — formatted by the image initialiser, populated
	/// at boot.
	Data,
}

impl PartitionKind {
	/// The mount point this partition's volume is expected to appear at.
	pub fn label(self) -> &'static str {
		match self {
			Self::Boot => "/boot",
			Self::System => "/system",
			Self::Data => "/data",
		}
	}

	/// Parse the CLI spelling (`system`, `data`, `boot`).
	pub fn parse(s: &str) -> Option<Self> {
		match s.trim_start_matches('/').to_ascii_lowercase().as_str() {
			"boot" => Some(Self::Boot),
			"system" => Some(Self::System),
			"data" => Some(Self::Data),
			_ => None,
		}
	}
}

/// One MBR partition entry, with the kind it was inferred to be.
#[derive(Debug, Clone, Copy)]
pub struct Partition {
	pub kind: PartitionKind,
	pub part_type: u8,
	pub start_lba: u64,
	pub sectors: u64,
}

impl Partition {
	/// Byte offset of this partition within the image.
	pub fn start_bytes(&self) -> u64 {
		self.start_lba * SECTOR_SIZE
	}

	/// Size of this partition in bytes.
	pub fn size_bytes(&self) -> u64 {
		self.sectors * SECTOR_SIZE
	}
}

/// Read the MBR of `path` and return its non-empty partition entries.
///
/// Kinds are assigned by type byte and order: the first `0x83` entry is
/// `/boot`, then the first `0xC0` is `/system` and the second is `/data` —
/// the order the MBR writer emits them in.
///
/// "First" and "second" are limits, not a description of the likely case: an
/// entry past those two has no kind to carry, so it is dropped exactly as an
/// unrecognised type byte is. Nothing in this layout gives a third LedgerFS
/// partition a name, and naming one anyway would make `find_partition`
/// answer with whichever the table listed first.
pub fn read_partitions(path: &Path) -> Result<Vec<Partition>> {
	let mut mbr = [0u8; SECTOR_SIZE as usize];
	let mut file = File::open(path).map_err(|e| Error::ImageOpen {
		path: path.to_path_buf(),
		detail: e.to_string(),
	})?;
	file.read_exact(&mut mbr).map_err(|e| Error::ImageOpen {
		path: path.to_path_buf(),
		detail: format!("reading the MBR: {e}"),
	})?;

	if mbr[510..512] != MBR_SIGNATURE {
		return Err(Error::NoPartitionTable {
			path: path.to_path_buf(),
		});
	}

	let mut boot_taken = false;
	let mut lfs_seen = 0u32;
	let mut out = Vec::new();

	for i in 0..MBR_ENTRY_COUNT {
		let e = MBR_PART_TABLE_OFFSET + i * MBR_ENTRY_SIZE;
		let part_type = mbr[e + 4];
		let start_lba = u32::from_le_bytes(mbr[e + 8..e + 12].try_into().expect("4-byte MBR field")) as u64;
		let sectors = u32::from_le_bytes(mbr[e + 12..e + 16].try_into().expect("4-byte MBR field")) as u64;

		// An all-zero entry is unused table space, not a partition.
		if part_type == 0 || sectors == 0 {
			continue;
		}

		let kind = if part_type == PART_TYPE_BOOT && !boot_taken {
			boot_taken = true;
			PartitionKind::Boot
		} else if part_type == PART_TYPE_LEDGERFS && lfs_seen < 2 {
			// Two, and only two. This used to name every `0xC0` entry — the
			// first `/system`, and *everything after it* `/data` — so a table
			// with three of them produced two partitions claiming the same
			// kind. `find_partition` then silently answered with whichever
			// came first, and two callers given the same table could mount
			// different volumes under the same name. A table this tool did not
			// write gets reported for what it is: `/system`, `/data`, and an
			// entry with no kind that is dropped like any other unknown type.
			let k = if lfs_seen == 0 {
				PartitionKind::System
			} else {
				PartitionKind::Data
			};
			lfs_seen += 1;
			k
		} else {
			// An entry this tool does not know how to name. Keep it visible
			// rather than silently dropping it, but it is not a LedgerFS
			// volume the reader can open.
			continue;
		};

		out.push(Partition {
			kind,
			part_type,
			start_lba,
			sectors,
		});
	}

	Ok(out)
}

/// Find the partition of a given kind, or report which one was missing.
pub fn find_partition(path: &Path, kind: PartitionKind) -> Result<Partition> {
	read_partitions(path)?
		.into_iter()
		.find(|p| p.kind == kind)
		.ok_or_else(|| Error::PartitionNotFound {
			kind: kind.label().to_string(),
		})
}
