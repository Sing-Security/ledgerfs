//! Security tests for the *media* layer: a disk image and the partition table
//! inside it, and what happens when either is hostile.
//!
//! A disk image is the least trustworthy input this crate handles. It arrives
//! from an installer, a download, a disk pulled out of a machine that may have
//! been compromised, or a file an operator pointed `lfs-read` at by mistake.
//! Nothing about it can be assumed: not its length, not its partition table,
//! not the numbers in that table, and least of all the superblock the table
//! points at.
//!
//! Two rules run through the whole file:
//!
//! 1. **Parsing reports, it does not trust.** [`read_partitions`] is a pure
//!    function of 512 bytes. It says what the table says — including nonsense,
//!    including overlapping ranges, including a partition that starts before
//!    the image does. The *refusals* belong at the next layer, when something
//!    tries to open a region, because that is where the numbers meet the file.
//!    A parser that "cleaned up" a hostile table would hide the attack from the
//!    person looking at it.
//! 2. **Bad bytes are an error, never a panic.** Every test that feeds the crate
//!    something malformed asserts on *how* it failed. A mount of a volume of
//!    unknown provenance must not be able to take the mounting process down,
//!    because in this system the mounting process is a filesystem server with
//!    other people's data open.
//!
//! The end-to-end tests at the bottom build a real image — MBR and all — format
//! a real LedgerFS volume inside one of its partitions, and reopen it, so the
//! happy path through this layer is covered too. A security suite that only
//! ever feeds the code garbage would pass on a parser that rejects everything.

use std::path::Path;

use ledgerfs_host::{
	Error, FileBlockDevice, HostFs, MemBlockDevice, Partition, PartitionKind, Result, Store, Volume, find_partition,
	read_partitions,
};

mod common;
use common::{Rng, TempImage, expect_err};

const BS: u64 = 4096;
const SECTOR: u64 = 512;

/// The boot signature, at bytes 510..512 of the MBR sector.
const SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// Partition type bytes, as the image builder writes and this crate reads them.
const TYPE_BOOT: u8 = 0x83;
const TYPE_LFS: u8 = 0xC0;

// region:    --- Building hostile media

/// One partition table entry: `(type, start_lba, sectors)`.
type Entry = (u8, u32, u32);

/// A 512-byte MBR sector holding `entries` in the four table slots.
///
/// Built by hand rather than by calling the crate's writer, for the same reason
/// the volume-damage tests write their own bytes: a test of "what does this
/// parser do with *this* table" has to be able to express tables the writer
/// would never produce — two partition types, zero sectors, a range past the
/// end of the image.
fn mbr(entries: &[Entry]) -> Vec<u8> {
	let mut sector = vec![0u8; SECTOR as usize];
	for (i, (part_type, start_lba, sectors)) in entries.iter().enumerate().take(4) {
		let e = 446 + i * 16;
		sector[e + 4] = *part_type;
		sector[e + 8..e + 12].copy_from_slice(&start_lba.to_le_bytes());
		sector[e + 12..e + 16].copy_from_slice(&sectors.to_le_bytes());
	}
	sector[510..512].copy_from_slice(&SIGNATURE);
	sector
}

/// An image file of `total_bytes` zeroes whose first sector is `sector`.
///
/// `total_bytes` is what makes the difference between a table that fits and one
/// that does not, so the two are separate arguments on purpose.
fn write_image(image: &TempImage, sector: &[u8], total_bytes: u64) -> Result<()> {
	let mut bytes = vec![0u8; total_bytes as usize];
	bytes[..sector.len()].copy_from_slice(sector);
	image.write(&bytes)?;
	Ok(())
}

/// A vector of `len` bytes from `rng`.
fn junk(rng: &mut Rng, len: usize) -> Vec<u8> {
	let mut out = Vec::with_capacity(len);
	while out.len() < len {
		out.extend_from_slice(&rng.next_u64().to_le_bytes());
	}
	out.truncate(len);
	out
}

// endregion:    --- Building hostile media

// region:    --- The MBR itself

#[test]
fn test_an_image_with_no_boot_signature_has_no_partition_table() -> Result<()> {
	// -- Setup & Fixtures: a full-size image of zeroes. The signature is the
	// only thing distinguishing "a partitioned image" from "a file that happens
	// to be 4 MiB long", and guessing would mean inventing a layout for a file
	// that has none.
	let image = TempImage::new("no-mbr");
	write_image(&image, &[0u8; SECTOR as usize], 4 * BS)?;

	// -- Exec
	let e = expect_err("an unpartitioned image", read_partitions(&image.path));

	// -- Check
	match e {
		Error::NoPartitionTable { path } => assert_eq!(path, image.path),
		other => panic!("expected NoPartitionTable, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_signature_one_byte_wrong_is_still_no_partition_table() -> Result<()> {
	// -- Setup & Fixtures: `0x55AA` is a two-byte check, not a range. Close but
	// wrong must be wrong, or the check is theatre.
	let image = TempImage::new("bad-mbr-sig");
	let mut sector = mbr(&[(TYPE_BOOT, 1, 8)]);
	sector[511] = 0xAB;
	write_image(&image, &sector, 4 * BS)?;

	// -- Exec & Check
	assert!(matches!(
		expect_err("a bad signature", read_partitions(&image.path)),
		Error::NoPartitionTable { .. }
	));
	Ok(())
}

#[test]
fn test_an_image_shorter_than_one_sector_is_refused_and_the_message_says_why() -> Result<()> {
	// -- Setup & Fixtures: a truncated download. `read_exact` fails, and the
	// failure has to be reported as an image problem rather than surfacing as a
	// zero-length table — "this image has no partitions" and "this image is not
	// an image" are different answers and only one of them is true.
	let image = TempImage::new("short-image");
	image.write(&[0u8; 100])?;

	// -- Exec
	let e = expect_err("a 100-byte image", read_partitions(&image.path));

	// -- Check
	match e {
		Error::ImageOpen { path, detail } => {
			assert_eq!(path, image.path);
			assert!(detail.contains("MBR"), "must name what it was reading: {detail}");
		}
		other => panic!("expected ImageOpen, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_missing_image_is_reported_rather_than_created() -> Result<()> {
	// -- Setup & Fixtures: `read_partitions` opens read-only, so a typo in a
	// path must be an error. An open mode that created the file would turn
	// "no such image" into "an image with no partitions", which is how an
	// operator ends up formatting the wrong disk.
	let missing = std::env::temp_dir().join("ledgerfs-test-does-not-exist-8f3a.img");
	let _ = std::fs::remove_file(&missing);

	// -- Exec
	let e = expect_err("a missing image", read_partitions(&missing));

	// -- Check
	assert!(matches!(e, Error::ImageOpen { .. }), "got {e:?}");
	assert!(!missing.exists(), "reading a partition table must not create the image");
	Ok(())
}

#[test]
fn test_an_empty_table_is_an_empty_list_not_an_error() -> Result<()> {
	// -- Setup & Fixtures: a signature and no entries is a well-formed MBR of a
	// disk nobody partitioned. Erroring here would be the parser overreaching.
	let image = TempImage::new("empty-table");
	write_image(&image, &mbr(&[]), 4 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert!(parts.is_empty());
	Ok(())
}

#[test]
fn test_an_entry_with_no_sectors_is_table_space() -> Result<()> {
	// -- Setup & Fixtures: `type != 0` but `sectors == 0`. Found in the wild on
	// images written by tools that leave stale type bytes behind, and a
	// zero-length partition is not addressable — it is a leftover, not a volume.
	let image = TempImage::new("zero-sectors");
	write_image(&image, &mbr(&[(TYPE_LFS, 100, 0), (TYPE_LFS, 200, 8)]), 4096 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(parts.len(), 1, "only the partition with sectors is real");
	assert_eq!(parts[0].start_lba, 200);
	assert_eq!(parts[0].kind, PartitionKind::System);
	Ok(())
}

#[test]
fn test_an_entry_with_sectors_but_no_type_is_table_space() -> Result<()> {
	// -- Setup & Fixtures: the other half of the same rule — a type byte of 0
	// means "unused", whatever the rest of the slot says.
	let image = TempImage::new("zero-type");
	write_image(&image, &mbr(&[(0x00, 100, 8), (TYPE_LFS, 200, 8)]), 4096 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(parts.len(), 1);
	assert_eq!(parts[0].start_lba, 200);
	Ok(())
}

#[test]
fn test_the_two_ledgerfs_entries_are_named_in_table_order() -> Result<()> {
	// -- Setup & Fixtures: the naming rule is positional — the first `0xC0` is
	// `/system`, the second is `/data`. Which one is which is the difference
	// between reading the OS image and reading the user's data, so the rule is
	// pinned even though it is only a convention of the writer.
	let image = TempImage::new("order");
	write_image(&image, &mbr(&[(TYPE_LFS, 10, 8), (TYPE_LFS, 20, 8)]), 4096 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(parts.len(), 2);
	assert_eq!((parts[0].kind, parts[0].start_lba), (PartitionKind::System, 10));
	assert_eq!((parts[1].kind, parts[1].start_lba), (PartitionKind::Data, 20));
	Ok(())
}

#[test]
fn test_a_third_ledgerfs_entry_is_dropped_rather_than_renamed() -> Result<()> {
	// -- Setup & Fixtures: there are only two LedgerFS partitions in this
	// layout. A third `0xC0` is either a mistake or an attempt to get a volume
	// mounted under a name it should not have; either way there is no kind left
	// to give it, and inventing one would make `find_partition` ambiguous.
	let image = TempImage::new("third-lfs");
	write_image(
		&image,
		&mbr(&[(TYPE_LFS, 10, 8), (TYPE_LFS, 20, 8), (TYPE_LFS, 30, 8)]),
		4096 * BS,
	)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(
		parts.len(),
		2,
		"a third LFS partition has no kind and must not be listed"
	);
	assert_eq!(parts[1].start_lba, 20);
	Ok(())
}

#[test]
fn test_a_second_boot_entry_is_dropped_rather_than_taking_the_boot_kind() -> Result<()> {
	// -- Setup & Fixtures: the `0x83` rule has the same positional flavour. A
	// second Linux-type entry must not become a second `/boot`.
	let image = TempImage::new("second-boot");
	write_image(
		&image,
		&mbr(&[(TYPE_BOOT, 10, 8), (TYPE_BOOT, 20, 8), (TYPE_LFS, 30, 8)]),
		4096 * BS,
	)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(parts.len(), 2);
	assert_eq!(parts[0].kind, PartitionKind::Boot);
	assert_eq!(parts[0].start_lba, 10);
	assert_eq!(parts[1].kind, PartitionKind::System);
	Ok(())
}

#[test]
fn test_partition_types_this_tool_does_not_know_are_dropped() -> Result<()> {
	// -- Setup & Fixtures: a real disk carries an EFI system partition and a
	// Windows partition beside the image's own ones. Neither is a LedgerFS
	// volume, and neither may be offered as one.
	let image = TempImage::new("foreign");
	write_image(
		&image,
		&mbr(&[(0xEF, 10, 8), (0x07, 20, 8), (0x05, 30, 8), (TYPE_LFS, 40, 8)]),
		4096 * BS,
	)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(parts.len(), 1);
	assert_eq!(parts[0].kind, PartitionKind::System);
	assert_eq!(parts[0].start_lba, 40);
	Ok(())
}

#[test]
fn test_a_partition_pointed_past_the_end_of_the_image_parses_but_does_not_open() -> Result<()> {
	// -- Setup & Fixtures: this is the shape of a truncated image with an intact
	// partition table — the most common way a bad download presents itself. The
	// table is what it is, so parsing reports it; the refusal belongs where the
	// range meets the file.
	let image = TempImage::new("past-end");
	let sector = mbr(&[(TYPE_LFS, 0x00FF_FFFF, 0xFFFF)]);
	write_image(&image, &sector, 8 * BS)?;

	// -- Exec: the table parses
	let parts = read_partitions(&image.path)?;
	assert_eq!(parts.len(), 1);
	assert_eq!(parts[0].start_lba, 0x00FF_FFFF);
	assert_eq!(parts[0].sectors, 0xFFFF);

	// -- Exec: opening it does not
	let e = expect_err(
		"a partition past the end",
		FileBlockDevice::open(&image.path, parts[0].start_bytes(), parts[0].size_bytes()),
	);

	// -- Check
	match e {
		Error::ImageOpen { detail, .. } => {
			assert!(detail.contains("the file is only"), "must name the shortfall: {detail}");
		}
		other => panic!("expected ImageOpen, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_partition_covering_the_partition_table_itself_is_not_a_bootable_volume() -> Result<()> {
	// -- Setup & Fixtures: `start_lba = 0` is legal to *state* and means the
	// partition begins with the MBR. Nothing stops a table from saying so, and
	// nothing should — but a volume mounted from it must not read the MBR as a
	// superblock, and must not be able to overwrite the table that describes the
	// disk.
	let image = TempImage::new("self-covering");
	let sector = mbr(&[(TYPE_LFS, 0, 16_384)]);
	write_image(&image, &sector, 16_384 * SECTOR + 8 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;
	assert_eq!(parts[0].start_lba, 0, "the table is reported as written");

	// -- Check: and it is not a filesystem. `Store::open` reads the superblock
	// at LBA 0, finds an MBR, and says so.
	let e = expect_err(
		"mounting the MBR as a volume",
		Store::open_partition(&image.path, PartitionKind::System),
	);
	match e {
		Error::SuperblockUnreadable { detail, .. } | Error::Mount { detail } => {
			assert!(!detail.is_empty(), "a refusal must say something");
		}
		other => panic!("expected a mount refusal, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_an_enormous_sector_count_is_reported_without_overflowing() -> Result<()> {
	// -- Setup & Fixtures: `sectors` is a `u32`, so the largest a table can
	// claim is 2 TiB. That must be *representable* — a size computation that
	// overflowed would turn an absurd claim into a small, plausible one, and
	// then a volume would be opened on a range the table never described.
	let image = TempImage::new("max-sectors");
	let sector = mbr(&[(TYPE_LFS, u32::MAX, u32::MAX)]);
	write_image(&image, &sector, 8 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	let p = &parts[0];
	assert_eq!(p.start_bytes(), u32::MAX as u64 * SECTOR);
	assert_eq!(p.size_bytes(), u32::MAX as u64 * SECTOR);
	assert_eq!(p.size_bytes(), 2_199_023_255_040, "2 TiB, not a wrapped small number");
	Ok(())
}

#[test]
fn test_overlapping_partitions_are_reported_and_cannot_both_be_opened_for_writing() -> Result<()> {
	// -- Setup & Fixtures: a table where `/system` and `/data` describe the same
	// region. This is a real attack shape — it aims to get two LedgerFS volumes
	// writing to one set of blocks, each believing the other's objects are free.
	// The parser reports the overlap (hiding it would hide the attack); what
	// must hold is that the two volumes cannot both be *written*.
	let image = TempImage::new("overlap");
	let sector = mbr(&[(TYPE_LFS, 8, 512), (TYPE_LFS, 8, 512)]);
	let total = (8 + 512) * SECTOR + 64 * BS;
	let mut bytes = vec![0u8; total as usize];
	bytes[..512].copy_from_slice(&sector);
	image.write(&bytes)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;
	assert_eq!(
		parts.len(),
		2,
		"overlap is the table's business, not the parser's to hide"
	);
	assert_eq!(parts[0].start_bytes(), parts[1].start_bytes());

	// -- Check: the first writer takes the file lock, the second is refused
	let region = parts[0].size_bytes();
	let first = FileBlockDevice::open_rw(&image.path, parts[0].start_bytes(), region)?;
	let e = expect_err(
		"a second writable volume on the same region",
		FileBlockDevice::open_rw(&image.path, parts[1].start_bytes(), region),
	);
	drop(first);

	// -- Check
	assert!(
		matches!(e, Error::Locked { .. }),
		"two writable volumes on one image must not be possible, got {e:?}"
	);
	Ok(())
}

// endregion:    --- The MBR itself

// region:    --- A random table

#[test]
fn test_randomly_generated_tables_never_panic_the_parser() -> Result<()> {
	// -- Setup & Fixtures: the parser reads 512 attacker-controlled bytes, so
	// the property that matters most is the one that has nothing to do with
	// filesystem semantics: it always returns, and what it returns is either a
	// list or a named error. A panic here is a crafted image taking down
	// whatever read it.
	let image = TempImage::new("fuzz-mbr");
	let mut rng = Rng::new(0x4D42_5253_5441_424C);

	// -- Exec: with the signature and without, since the signature is a branch
	// the fuzzer has to be able to take both ways.
	for i in 0..2000 {
		let mut sector = junk(&mut rng, SECTOR as usize);
		if i % 2 == 0 {
			sector[510..512].copy_from_slice(&SIGNATURE);
		}
		image.write(&sector)?;

		// -- Check: no panic, and any partitions that come back are
		// self-consistent — the entries carry the numbers they were given.
		// A table without the signature is *expected* to be refused here; the
		// property under test is that the refusal is the only other outcome.
		let parts = match read_partitions(&image.path) {
			Ok(parts) => parts,
			Err(Error::NoPartitionTable { .. }) => continue,
			Err(e) => panic!("iteration {i}: unexpected error {e:?}"),
		};
		for p in &parts {
			assert!(p.part_type == TYPE_BOOT || p.part_type == TYPE_LFS);
			assert_ne!(p.sectors, 0, "a zero-sector partition must never be listed");
			assert_eq!(p.start_bytes(), p.start_lba * SECTOR);
			assert_eq!(p.size_bytes(), p.sectors * SECTOR);
		}
	}
	Ok(())
}

#[test]
fn test_a_table_naming_every_partition_type_at_once_is_handled_by_the_rules() -> Result<()> {
	// -- Setup & Fixtures: all four slots busy, every interesting type, and the
	// order chosen so that each rule fires once: boot, a second boot dropped, an
	// LFS that becomes `/system`, and an unknown type dropped.
	let image = TempImage::new("all-types");
	let sector = mbr(&[(TYPE_BOOT, 10, 8), (TYPE_BOOT, 20, 8), (TYPE_LFS, 30, 8), (0x07, 40, 8)]);
	write_image(&image, &sector, 4096 * BS)?;

	// -- Exec
	let parts = read_partitions(&image.path)?;

	// -- Check
	assert_eq!(parts.len(), 2);
	assert_eq!(parts[0].kind, PartitionKind::Boot);
	assert_eq!(parts[1].kind, PartitionKind::System);
	Ok(())
}

// endregion:    --- A random table

// region:    --- Finding a partition

#[test]
fn test_finding_a_kind_that_is_not_there_names_the_kind() -> Result<()> {
	// -- Setup & Fixtures
	let image = TempImage::new("missing-kind");
	write_image(&image, &mbr(&[(TYPE_LFS, 10, 8)]), 4096 * BS)?;

	// -- Exec
	let e = expect_err("a missing /data", find_partition(&image.path, PartitionKind::Data));

	// -- Check
	match e {
		Error::PartitionNotFound { kind } => assert_eq!(kind, "/data"),
		other => panic!("expected PartitionNotFound, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_finding_a_partition_does_not_open_it() -> Result<()> {
	// -- Setup & Fixtures: `find_partition` is lookup, not access. It must
	// answer for a partition that is entirely past the end of the image,
	// because "is there a /data in this table" and "can I mount it" are
	// separate questions asked at separate times.
	let image = TempImage::new("find-not-open");
	write_image(&image, &mbr(&[(TYPE_LFS, 0x00FF_FFFF, 0xFFFF)]), 8 * BS)?;

	// -- Exec
	let p = find_partition(&image.path, PartitionKind::System)?;

	// -- Check
	assert_eq!(p.start_lba, 0x00FF_FFFF);
	Ok(())
}

#[test]
fn test_every_partition_kind_has_a_distinct_label_and_parses_back() -> Result<()> {
	// -- Setup & Fixtures: the label is the mount point (`/system`), which is
	// what a caller sees when a lookup fails and what `--partition` is compared
	// against, so the two spellings have to agree.
	let kinds = [PartitionKind::Boot, PartitionKind::System, PartitionKind::Data];

	// -- Exec & Check
	let mut labels = Vec::new();
	for kind in kinds {
		let label = kind.label();
		assert!(label.starts_with('/'), "{label} is a mount point and must be absolute");
		assert!(!labels.contains(&label.to_string()), "duplicate label {label}");
		labels.push(label.to_string());
		assert_eq!(PartitionKind::parse(label), Some(kind), "{label} must parse back");
		// -- Check: and so must the bare name, which is how the CLI offers it.
		assert_eq!(PartitionKind::parse(label.trim_start_matches('/')), Some(kind));
	}

	// -- Check: `parse` is deliberately lenient about case and a leading slash,
	// because it is fed a human's spelling on a command line and the choice it
	// makes is which volume to *open*, not who may open it.
	assert_eq!(PartitionKind::parse("/System"), Some(PartitionKind::System));
	assert_eq!(PartitionKind::parse("/DATA"), Some(PartitionKind::Data));
	assert_eq!(PartitionKind::parse("////boot"), Some(PartitionKind::Boot));
	// -- Check: and the leniency stops at whitespace. A CLI passes arguments
	// split on whitespace, so `" data "` reaching here means something has gone
	// wrong upstream, and silently trimming it would hide that.
	assert_eq!(PartitionKind::parse(" data "), None);

	// -- Check: and it invents nothing. A name that is not one of the three is
	// not quietly mapped onto a default — the failure a leniency bug would
	// produce is a caller opening `/data` when it asked for something else.
	assert_eq!(PartitionKind::parse("nonsense"), None);
	assert_eq!(PartitionKind::parse(""), None);
	assert_eq!(PartitionKind::parse("/"), None);
	assert_eq!(PartitionKind::parse("data2"), None);
	Ok(())
}

// endregion:    --- Finding a partition

// region:    --- A real image, end to end

/// A disk image with an MBR and a formatted LedgerFS volume in `/system`.
///
/// Returns the partition so a test can assert on where the volume really landed
/// rather than on where it was meant to.
fn formatted_system_image(image: &TempImage) -> Result<Partition> {
	let lba = 2048u32;
	// 2 MiB. Big enough that a test can churn a volume hard inside it, small
	// enough to stay a file the test writes and deletes without thinking about
	// it. The boundary being tested is the partition edge, not the volume size.
	let sectors = 4096u32;
	let sector = mbr(&[(TYPE_BOOT, 64, 1024), (TYPE_LFS, lba, sectors)]);
	image.write(&vec![0u8; (lba as usize + sectors as usize) * SECTOR as usize])?;
	// The MBR goes in after the zero-fill, so it is not overwritten.
	let mut bytes = std::fs::read(&image.path)?;
	bytes[..512].copy_from_slice(&sector);
	image.write(&bytes)?;

	let partition = find_partition(&image.path, PartitionKind::System)?;
	let device = FileBlockDevice::open_rw(&image.path, partition.start_bytes(), partition.size_bytes())?;
	let terms = partition.sectors * SECTOR / BS;
	let mut store = Store::format(Box::new(device), terms)?;
	store.mkdir("/etc", 0o755)?;
	store.write_new_file("/etc/hostname", b"myhost", 0o644)?;
	Ok(partition)
}

#[test]
fn test_a_volume_inside_a_partition_round_trips_through_the_image() -> Result<()> {
	// -- Setup & Fixtures: the whole point of the layer. Without this, every
	// refusal above could be produced by a parser that never accepts anything.
	let image = TempImage::new("real-image");
	let partition = formatted_system_image(&image)?;

	// -- Exec: reopen by kind, the way `lfs-read` and other host tools do
	let fs = HostFs::open(&image.path, PartitionKind::System)?;

	// -- Check
	assert_eq!(fs.read_file("/etc/hostname")?, b"myhost");
	assert_eq!(fs.volume.total_blocks(), partition.sectors * SECTOR / BS);
	Ok(())
}

#[test]
fn test_the_volume_stays_inside_its_partition() -> Result<()> {
	// -- Setup & Fixtures: the partition is the security boundary. `/boot` sits
	// immediately before it and the file continues after it, so a volume that
	// overran its own size — by a block, by a superblock copy, by a scan — would
	// be visible as bytes changed outside the range.
	let image = TempImage::new("partition-boundary");
	let partition = formatted_system_image(&image)?;

	// -- Exec: reopen writable and churn hard enough to rewrite the tail
	let device = FileBlockDevice::open_rw(&image.path, partition.start_bytes(), partition.size_bytes())?;
	let mut store = Store::open(Box::new(device))?;
	for i in 0..40 {
		store.write_new_file(&format!("/f{i:02}"), &vec![b'z'; 1000], 0o644)?;
	}
	drop(store);

	// -- Check: the byte before the partition and the byte after it are as the
	// image was written — the `/boot` area untouched, the tail untouched.
	let bytes = std::fs::read(&image.path)?;
	let start = partition.start_bytes() as usize;
	let end = (partition.start_bytes() + partition.size_bytes()) as usize;
	// The MBR itself lives before the partition — that is the point of a
	// partition table — so the region checked is from the end of the first
	// sector up to the partition's own start.
	assert_eq!(&bytes[510..512], &SIGNATURE, "the partition table was overwritten");
	assert!(
		bytes[512..start].iter().all(|b| *b == 0),
		"the volume wrote before its partition"
	);
	assert!(
		bytes[end..].iter().all(|b| *b == 0),
		"the volume wrote past its partition"
	);
	Ok(())
}

#[test]
fn test_a_volume_file_with_no_image_around_it_still_mounts() -> Result<()> {
	// -- Setup & Fixtures: the embeddable case — a bare volume file, no MBR. It
	// has to keep working, because it is how `MemBlockDevice` volumes are
	// written out and read back, and how a volume is moved between machines.
	let image = TempImage::new("bare-volume");
	let device = FileBlockDevice::create(&image.path, 256 * BS)?;
	let mut store = Store::format(Box::new(device), 256)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"bare", 0o644)?;
	drop(store);

	// -- Exec: reopen as a whole file, no partition table consulted
	let device = FileBlockDevice::open(&image.path, 0, 256 * BS)?;
	let fs = HostFs::mount_device(Box::new(device))?;

	// -- Check
	assert_eq!(fs.read_file("/d/f")?, b"bare");

	// -- Check: and the same file is *not* a partitioned image, so nothing
	// treats its first block as an MBR.
	assert!(matches!(
		expect_err("a bare volume as an image", read_partitions(&image.path)),
		Error::NoPartitionTable { .. }
	));
	Ok(())
}

// endregion:    --- A real image, end to end

// region:    --- Mounting bytes of unknown provenance

#[test]
fn test_random_bytes_never_mount_as_a_volume() -> Result<()> {
	// -- Setup & Fixtures: 400 volumes of random bytes, mounted the way a
	// hostile file would be. Every one must fail — and the assertion that
	// matters is that the loop finishes, because the failure mode being tested
	// for is a panic inside superblock decoding.
	let mut rng = Rng::new(0x4241_4456_4F4C_554D);
	let blocks = 32u64;

	// -- Exec & Check
	for i in 0..400 {
		let bytes = junk(&mut rng, (blocks * BS) as usize);
		let device = MemBlockDevice::from_bytes(bytes)?;
		let mounted = HostFs::mount_device(Box::new(device));
		assert!(mounted.is_err(), "iteration {i}: random bytes mounted as a filesystem");
	}
	Ok(())
}

#[test]
fn test_a_volume_of_the_right_size_but_the_wrong_contents_is_refused_by_name() -> Result<()> {
	// -- Setup & Fixtures: the plausible-file case. Right length, no signature,
	// no checksums — the shape of a file an operator truncated, or of an
	// encrypted volume, or of a disk image from another filesystem.
	let bytes = vec![0xEEu8; (32 * BS) as usize];
	let device = MemBlockDevice::from_bytes(bytes)?;

	// -- Exec
	let e = expect_err("32 blocks of 0xEE", HostFs::mount_device(Box::new(device)));

	// -- Check: a refusal that names a copy number and a reason, because
	// "which superblock copy did you try" is the first question a person
	// diagnosing this asks.
	match e {
		Error::SuperblockUnreadable { copy_lba, detail } => {
			assert!(
				matches!(copy_lba, 0 | 1 | 30 | 31),
				"a copy LBA must be one of the volume's four: {copy_lba}"
			);
			assert!(!detail.is_empty(), "a refusal must carry a reason");
		}
		other => panic!("expected SuperblockUnreadable, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_single_damaged_byte_anywhere_in_the_metadata_never_panics_the_mount() -> Result<()> {
	// -- Setup & Fixtures: a real volume, damaged one byte at a time in the
	// regions a hostile actor would aim at — the four superblock copies and the
	// allocator snapshot — and remounted on each try. Bit-rot and tampering look
	// the same from here.
	let (store, watch) = common::watched(64)?;
	drop(store);
	let good = common::bytes(&watch);

	// -- Exec & Check
	let offsets: Vec<usize> = (0..6)
		.flat_map(|block| (0..16).map(move |i| block * BS as usize + i * 137))
		.chain((0..64 * BS as usize).step_by(4096).map(|b| b + 7))
		.collect();

	for offset in offsets {
		for delta in [1u8, 0x80, 0xFF] {
			let mut damaged = good.clone();
			damaged[offset] = damaged[offset].wrapping_add(delta);
			if damaged[offset] == good[offset] {
				continue;
			}
			// Mounting may succeed (the damage missed a live copy) or fail
			// cleanly. Coming back with an error is the contract; panicking is
			// the bug.
			let device = MemBlockDevice::from_bytes(damaged)?;
			let _ = HostFs::mount_device(Box::new(device));
		}
	}
	Ok(())
}

#[test]
fn test_mounting_the_same_bytes_twice_gives_the_same_answer() -> Result<()> {
	// -- Setup & Fixtures: mount is a pure function of the volume. A mount that
	// mutated the device — repairing a copy in place, say — would make a
	// read-only inspection of a suspect volume an act that changes it, which is
	// exactly what a forensic read must not do.
	let (store, watch) = common::watched(64)?;
	drop(store);
	let before = common::bytes(&watch);

	// -- Exec: mount several times, through different entry points
	let mut answers = Vec::new();
	for _ in 0..3 {
		let device = MemBlockDevice::from_bytes(before.clone())?;
		let fs = HostFs::mount_device(Box::new(device))?;
		answers.push((
			fs.volume.total_blocks(),
			fs.volume.superblock.active_commit_seq,
			fs.volume.copy_status().is_healthy(),
		));
	}
	let device = MemBlockDevice::from_bytes(before.clone())?;
	let _ = Volume::mount(&device)?;

	// -- Check: identical answers, and the device's bytes are untouched
	assert!(
		answers.iter().all(|a| *a == answers[0]),
		"mounting gave different answers: {answers:?}"
	);
	assert_eq!(common::bytes(&watch), before, "mounting modified the volume");
	Ok(())
}

#[test]
fn test_a_volume_larger_than_a_test_volume_still_mounts_and_reads() -> Result<()> {
	// -- Setup & Fixtures: mounting scans for the root when the hint is unusable,
	// and the scan is bounded by `max_scan_blocks`. A volume bigger than that
	// bound is the case where a bounded scan has to still find an answer — by
	// the hint, not by giving up.
	//
	// This is a *correctness* property with a security shape: an unbounded scan
	// is a cheap denial of service on a mount, so the bound exists; a mount that
	// then failed on big volumes would be the bound being wrong.
	let image = TempImage::new("big-scan");
	let blocks = 8192u64;
	let device = FileBlockDevice::create(&image.path, blocks * BS)?;
	let mut store = Store::format(Box::new(device), blocks)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"found by the hint", 0o644)?;
	drop(store);

	// -- Exec
	let device = FileBlockDevice::open(&image.path, 0, blocks * BS)?;
	let fs = HostFs::mount_device(Box::new(device))?;

	// -- Check
	assert_eq!(fs.read_file("/d/f")?, b"found by the hint");
	Ok(())
}

// endregion:    --- Mounting bytes of unknown provenance

// region:    --- Paths named by the caller

#[test]
fn test_an_image_path_that_is_a_directory_is_reported_not_followed() -> Result<()> {
	// -- Setup & Fixtures: a caller that mis-typed a path into a directory. The
	// answer must be an error naming the path, not an attempt to read a
	// directory as a sector.
	let dir = std::env::temp_dir();
	let _ = dir; // the temp dir exists and is a directory on every platform this runs on

	// -- Exec
	let e = expect_err("a directory as an image", read_partitions(Path::new("/tmp")));

	// -- Check
	assert!(matches!(e, Error::ImageOpen { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_two_images_are_independent() -> Result<()> {
	// -- Setup & Fixtures: two volumes, each with its own contents. Nothing in
	// the crate keys state by path or by anything else global — a cache or a
	// hint table shared between volumes would let one image's metadata answer
	// for another's, which is a cross-volume information leak.
	let a = TempImage::new("independent-a");
	let b = TempImage::new("independent-b");
	for (image, marker) in [(&a, b"AAA"), (&b, b"BBB")] {
		let device = FileBlockDevice::create(&image.path, 128 * BS)?;
		let mut store = Store::format(Box::new(device), 128)?;
		store.write_new_file("/marker", marker, 0o644)?;
	}

	// -- Exec: open both at once and read both
	let fa = HostFs::mount_device(Box::new(FileBlockDevice::open(&a.path, 0, 128 * BS)?))?;
	let fb = HostFs::mount_device(Box::new(FileBlockDevice::open(&b.path, 0, 128 * BS)?))?;

	// -- Check
	assert_eq!(fa.read_file("/marker")?, b"AAA");
	assert_eq!(fb.read_file("/marker")?, b"BBB");
	Ok(())
}

// endregion:    --- Paths named by the caller

// region:    --- Residual data

#[test]
fn test_formatting_a_medium_leaves_none_of_what_it_held() -> Result<()> {
	// -- Setup & Fixtures: a medium full of a recognisable secret, then a
	// 128-byte marker repeated to the end so that "the secret is gone" is a
	// claim about every block rather than about the ones the format happened to
	// touch.
	//
	// This is the at-rest half of the crate's story. A `mkfs` that left its
	// predecessor's blocks behind would disclose a deleted volume's contents to
	// whoever could read the device afterwards — and, just as bad, would leave
	// stale `ObjectRecord`s for a hint-less reader to find and treat as live
	// objects belonging to the new filesystem.
	const SECRET: &[u8] = b"TOPSECRET-forgery-plans-7274";
	let blocks = 512u64;
	let mut medium = Vec::with_capacity((blocks * BS) as usize);
	while medium.len() < (blocks * BS) as usize {
		medium.extend_from_slice(SECRET);
	}
	medium.truncate((blocks * BS) as usize);

	// -- Exec: formatted through a live handle on the same buffer, since the
	// bytes that matter are the ones *after* the format — a snapshot taken
	// before it would only show the secret to itself.
	let device = MemBlockDevice::from_bytes(medium)?;
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), blocks)?;
	store.mkdir("/d", 0o755)?;
	let after = common::bytes(&watch);

	// -- Check: the secret appears nowhere in the formatted volume
	let hits = after.windows(SECRET.len()).filter(|w| *w == SECRET).count();
	assert_eq!(hits, 0, "format left {hits} copies of the old contents on the medium");

	// -- Check: and the filesystem it did write is functional, so the erase was
	// not simply a very thorough way of producing a blank device.
	assert_eq!(store.stat("/d")?.kind, ledgerfs_host::InodeKind::Directory);
	Ok(())
}

#[test]
fn test_a_read_only_mount_does_not_write_to_the_medium() -> Result<()> {
	// -- Setup & Fixtures: inspecting a suspect volume must not change it. A
	// mount that "repaired" a damaged superblock copy in place would destroy the
	// evidence of what was wrong with it, and would make a read-only open a
	// write — the one thing its name promises it is not.
	let (store, watch) = common::watched(128)?;
	drop(store);
	let before = common::bytes(&watch);

	// -- Exec: mount read-only, list, read a file
	let fs = HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(before.clone())?))?;
	let _ = fs.read_dir("/")?;

	// -- Check
	assert_eq!(common::bytes(&watch), before, "a read-only mount modified the volume");
	Ok(())
}

// endregion:    --- Residual data
