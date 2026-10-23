//! Security tests for the block-device layer: the boundary a volume's own
//! bytes cross on their way in and out.
//!
//! A `BlockDevice` is the trust boundary of the whole crate. Everything above it
//! (superblock decoding, the object store, path resolution, every filesystem
//! verb) assumes that a read either fills the buffer or fails, that a write
//! either lands inside the volume or fails, and that failing is how "no" is
//! said. Nothing above it re-checks any of that. So the properties pinned here
//! are the ones the rest of the crate is *built on* rather than merely using:
//!
//! - **an out-of-range access is an error, never a panic and never a short
//!   read**. The LBAs a read asks for come from on-disk metadata, which for a
//!   volume of unknown provenance is attacker-controlled. A corrupt extent node
//!   naming block `u64::MAX` must produce a refusal, because a panic in a
//!   program that mounts volumes is a denial of service on that program.
//! - **the geometry checks are arithmetic, not convention**. `offset + len` is
//!   done in 128 bits precisely so that a caller passing `u64::MAX` cannot make
//!   the sum wrap into the valid range.
//! - **a volume cannot reach outside itself**. A partition opened inside an
//!   image is bounded by its own size even when the file is much larger, and a
//!   region that runs off the end of the file is refused at open time rather
//!   than half-read.
//! - **one writer**. Two writable handles on one volume would each believe the
//!   other's blocks are free; the second must be refused, while readers are
//!   allowed so a CLI can inspect a volume a server holds.
//!
//! The tests are written against the public trait surface (`read_block`,
//! `write_blocks`, `check_bounds`' callers) rather than against either
//! implementation's internals, because both implementations have to answer the
//! same questions: that is what makes them substitutable.

use ledgerfs_host::{BlockDevice, Error, FileBlockDevice, MemBlockDevice, Result};

mod common;

use common::expect_err;

const BS: u64 = 4096;

/// A block buffer, full size.
fn block() -> [u8; BS as usize] {
	[0u8; BS as usize]
}

// region:    --- Bounds

#[test]
fn test_a_read_that_starts_past_the_end_is_refused() -> Result<()> {
	// -- Setup & Fixtures
	let device = MemBlockDevice::new(4);

	// -- Exec
	let e = device.read_bytes_at(4 * BS, &mut block()).expect_err("past the end");

	// -- Check: named with the numbers the caller needs. A bounds error that
	// does not say what the bound *was* leaves the caller guessing whether it
	// mis-addressed by a block or by a kilobyte.
	match e {
		Error::OutOfBounds {
			offset,
			len,
			size_bytes,
		} => {
			assert_eq!(offset, 4 * BS);
			assert_eq!(len, BS as usize);
			assert_eq!(size_bytes, 4 * BS);
		}
		other => panic!("expected OutOfBounds, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_read_that_ends_exactly_at_the_end_is_allowed() -> Result<()> {
	// -- Setup & Fixtures: the last block is addressable. An off-by-one in the
	// bounds check that rejected the final block would be invisible until a
	// volume happened to have data there.
	let device = MemBlockDevice::new(4);
	device.write_block(3, b"last")?;

	// -- Exec
	let mut buf = block();
	device.read_bytes_at(3 * BS, &mut buf)?;

	// -- Check
	assert_eq!(&buf[..4], b"last");
	Ok(())
}

#[test]
fn test_a_read_that_straddles_the_end_is_refused_in_full() -> Result<()> {
	// -- Setup & Fixtures: two blocks of buffer starting one block before the
	// end. Half of this read is legal, and half is not. The device must refuse
	// the whole request rather than fill the legal half: a caller that gets a
	// short read back has no way to tell it apart from a legitimate one.
	let device = MemBlockDevice::new(4);
	device.write_block(3, b"at the end")?;

	// -- Exec
	let mut buf = [0u8; (BS * 2) as usize];
	let e = device.read_bytes_at(3 * BS, &mut buf).expect_err("straddles the end");

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { offset, len, .. } if offset == 3 * BS && len == (BS * 2) as usize));
	assert!(
		buf.iter().all(|b| *b == 0),
		"a refused read must not have written part of the buffer"
	);
	Ok(())
}

#[test]
fn test_a_write_that_starts_past_the_end_is_refused_and_changes_nothing() -> Result<()> {
	// -- Setup & Fixtures
	let device = MemBlockDevice::new(4);
	device.write_block(0, b"untouched")?;

	// -- Exec
	let e = device.write_bytes_at(8 * BS, b"nowhere").expect_err("past the end");

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { .. }), "got {e:?}");
	let mut buf = block();
	device.read_block(0, &mut buf)?;
	assert_eq!(
		&buf[..9],
		b"untouched",
		"a refused write must not have landed somewhere else"
	);
	Ok(())
}

#[test]
fn test_offset_plus_length_that_overflows_u64_is_refused_not_wrapped() -> Result<()> {
	// -- Setup & Fixtures: the exact shape of the classic integer-overflow
	// bounds-check bug. `u64::MAX + 16` wraps to 15, and 15 is inside a 4-block
	// volume, so a check done in `u64` would *approve* this read and then index
	// 16 bytes of nothing. The check is done in `u128` for this reason, and this
	// test is the reason it has to stay that way.
	let device = MemBlockDevice::new(4);

	// -- Exec
	let e = device.read_bytes_at(u64::MAX, &mut [0u8; 16]).expect_err("overflowing end");

	// -- Check
	assert!(
		matches!(e, Error::OutOfBounds { offset, .. } if offset == u64::MAX),
		"got {e:?}"
	);
	Ok(())
}

#[test]
fn test_a_write_whose_end_overflows_u64_is_refused_not_wrapped() -> Result<()> {
	// -- Setup & Fixtures: the write half of the same check, because a wrapped
	// write is worse than a wrapped read: it lands on a real block.
	let device = MemBlockDevice::new(4);

	// -- Exec
	let e = device.write_bytes_at(u64::MAX - 4, b"boom").expect_err("overflowing end");

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_an_empty_range_at_the_very_end_is_a_no_op_and_past_it_is_not() -> Result<()> {
	// -- Setup & Fixtures: `offset == size` with `len == 0` is the empty range,
	// which is legal: a zero-length write at the end is what an append loop
	// issues when it has nothing left to append. The boundary is worth pinning
	// because the two ways to get it wrong both look reasonable: refusing the
	// empty range breaks append loops, and accepting it *anywhere* lets a
	// caller that has lost its position believe it wrote something.
	let device = MemBlockDevice::new(4);

	// -- Exec & Check: at the end, fine
	device.read_bytes_at(4 * BS, &mut [])?;
	device.write_bytes_at(4 * BS, &[])?;

	// -- Exec & Check: one byte past the end, refused even though it is empty
	let e = device
		.read_bytes_at(4 * BS + 1, &mut [])
		.expect_err("an empty range past the end");
	assert!(
		matches!(e, Error::OutOfBounds { offset, len: 0, .. } if offset == 4 * BS + 1),
		"got {e:?}"
	);
	let e = device
		.write_bytes_at(u64::MAX, &[])
		.expect_err("an empty range far past the end");
	assert!(matches!(e, Error::OutOfBounds { len: 0, .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_a_block_read_at_the_volume_s_end_block_fails_on_a_zero_block_volume() -> Result<()> {
	// -- Setup & Fixtures: block 0 of a volume with no blocks. The LBA is
	// arithmetically ordinary and there is simply no such block, so the refusal
	// has to come from the block count rather than from the byte arithmetic.
	let device = MemBlockDevice::from_bytes(Vec::new())?;

	// -- Exec
	let e = expect_err("block 0 of an empty volume", device.read_block(0, &mut block()));

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { size_bytes: 0, .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_a_block_read_into_an_undersized_buffer_is_refused() -> Result<()> {
	// -- Setup & Fixtures
	let device = MemBlockDevice::new(4);

	// -- Exec: `read_block` promises a whole block, so a buffer smaller than one
	// block is a caller error. Padding it silently would hand back a block with
	// an internal 4 KiB convention the caller never agreed to.
	let e = device.read_block(0, &mut [0u8; 100]).expect_err("short buffer");

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { len: 100, .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_a_block_that_far_past_the_end_is_refused_without_panicking() -> Result<()> {
	// -- Setup & Fixtures: the LBA here comes, in the real system, from an
	// object reference decoded out of a possibly-hostile volume. It is the
	// largest LBA that does not overflow `lba * BLOCK_SIZE`, so the arithmetic
	// itself is legal and the bounds check has to be what refuses it.
	let device = MemBlockDevice::new(4);
	let lba = u64::MAX / BS;

	// -- Exec
	let e = device.read_block(lba, &mut block()).expect_err("absurd lba");

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_an_lba_whose_byte_offset_would_overflow_is_refused_rather_than_panicking() -> Result<()> {
	// -- Setup & Fixtures: `read_block` computes `lba * BLOCK_SIZE` before it
	// checks anything. At this LBA that product does not fit in a `u64`, so a
	// device that multiplies first and checks second turns a corrupt on-disk
	// reference into a panic: a mount of a hostile volume killing the process
	// that mounted it.
	//
	// The property under test is "an error, not a panic". Which error is not
	// pinned: `OutOfBounds` from a checked multiply and `OutOfBounds` from a
	// wrapping multiply that then fails its bounds check are both fine.
	let device = MemBlockDevice::new(4);

	// -- Exec
	let e = device.read_block(u64::MAX, &mut block()).expect_err("lba out of range");

	// -- Check
	assert!(matches!(e, Error::OutOfBounds { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_every_out_of_range_lba_in_a_sweep_is_refused() -> Result<()> {
	// -- Setup & Fixtures: a handful of shapes of "too big" rather than one, so
	// a check that happens to special-case a single value is caught. `1 << 63`
	// and `u64::MAX` differ in how they wrap.
	let device = MemBlockDevice::new(16);
	let lbas = [
		16u64,
		17,
		1 << 32,
		1 << 52,
		1 << 62,
		u64::MAX / BS,
		u64::MAX / BS + 1,
		u64::MAX - 1,
		u64::MAX,
	];

	// -- Exec & Check
	for lba in lbas {
		let e = match device.read_block(lba, &mut block()) {
			Ok(()) => panic!("lba {lba} was accepted"),
			Err(e) => e,
		};
		assert!(matches!(e, Error::OutOfBounds { .. }), "lba {lba}: got {e:?}");
	}
	Ok(())
}

#[test]
fn test_a_misaligned_block_write_is_refused() -> Result<()> {
	// -- Setup & Fixtures
	let device = MemBlockDevice::new(4);

	// -- Exec: `write_blocks` is the "these are whole blocks" API. A partial
	// final block would be unaddressable, so it is refused rather than padded;
	// padding would look like success to the commit engine and lose the tail.
	let e = device.write_blocks(0, &[0u8; 100]).expect_err("not a block multiple");

	// -- Check
	assert!(
		matches!(
			e,
			Error::Misaligned {
				len: 100,
				block_size: 4096,
				..
			}
		),
		"got {e:?}"
	);
	Ok(())
}

#[test]
fn test_a_misaligned_block_write_at_the_end_is_refused_before_it_writes_anything() -> Result<()> {
	// -- Setup & Fixtures: a misaligned length *and* an out-of-range start. The
	// order of the two checks does not matter; that one of them fires before any
	// byte is written does.
	let device = MemBlockDevice::new(4);

	// -- Exec
	let e = device.write_blocks(4, &[0u8; 100]).expect_err("bad write");

	// -- Check
	assert!(
		matches!(e, Error::Misaligned { .. } | Error::OutOfBounds { .. }),
		"got {e:?}"
	);
	Ok(())
}

// endregion:    --- Bounds

// region:    --- Writing and read-back

#[test]
fn test_a_write_pads_a_short_block_past_its_data() -> Result<()> {
	// -- Setup & Fixtures
	let device = MemBlockDevice::new(4);

	// -- Exec: a block's worth of stale bytes first, so "the rest is zeroes" is
	// a real claim rather than an artefact of a fresh device.
	device.write_block(1, &[0xFFu8; BS as usize])?;
	device.write_block(1, b"short")?;

	// -- Check
	let mut buf = block();
	device.read_block(1, &mut buf)?;
	assert_eq!(&buf[..5], b"short");
	assert!(
		buf[5..].iter().all(|b| *b == 0),
		"the pad must be zeroes, not whatever the block held before"
	);
	Ok(())
}

#[test]
fn test_an_over_long_block_write_writes_exactly_one_block() -> Result<()> {
	// -- Setup & Fixtures: `write_block` takes one block's worth. Handed more,
	// it must not spill into the next block: a spill would be a silent write
	// outside the extent the caller allocated.
	let device = MemBlockDevice::new(4);
	device.write_block(2, &[0xAAu8; BS as usize])?;

	// -- Exec
	let mut data = vec![0xBBu8; (BS * 2) as usize];
	data[0] = b'X';
	device.write_block(1, &data)?;

	// -- Check
	let mut buf = block();
	device.read_block(1, &mut buf)?;
	assert_eq!(buf[0], b'X');
	assert!(buf[1..].iter().all(|b| *b == 0xBB));
	device.read_block(2, &mut buf)?;
	assert!(
		buf.iter().all(|b| *b == 0xAA),
		"write_block spilled into the next block"
	);
	Ok(())
}

#[test]
fn test_a_multi_block_write_round_trips() -> Result<()> {
	// -- Setup & Fixtures
	let device = MemBlockDevice::new(8);
	let data: Vec<u8> = (0..BS as usize * 3).map(|i| (i % 251) as u8).collect();

	// -- Exec
	device.write_blocks(2, &data)?;

	// -- Check: byte-for-byte, so a device that quietly reorders or drops a
	// block shows up here rather than as a checksum failure in a superblock.
	let mut buf = vec![0u8; data.len()];
	device.read_blocks(2, &mut buf)?;
	assert_eq!(buf, data);
	Ok(())
}

#[test]
fn test_the_device_reports_its_own_geometry_consistently() -> Result<()> {
	// -- Setup & Fixtures: the rest of the crate sizes its scans from
	// `block_count()`, so an implementation whose `size_bytes` and
	// `block_count` disagree would scan a block past its own end.
	let device = MemBlockDevice::new(7);

	// -- Check
	assert_eq!(device.size_bytes(), 7 * BS);
	assert_eq!(device.block_count(), 7);
	assert!(device.is_writable());
	assert!(device.read_block(6, &mut block()).map(|_| ()).is_ok());
	assert!(device.read_block(7, &mut block()).is_err());
	Ok(())
}

// endregion:    --- Writing and read-back

// region:    --- Read-only

#[test]
fn test_a_read_only_device_refuses_a_write_with_a_named_error() -> Result<()> {
	// -- Setup & Fixtures: a read-only `MemBlockDevice` is not constructible
	// directly, so this uses the file device, which is the case that matters
	// anyway, since it is how `lfs-read` and other read-only inspectors open a
	// volume a writer still holds.
	let image = common::TempImage::new("ro-device");
	let device = FileBlockDevice::create(&image.path, 8 * BS)?;
	device.write_block(0, b"durable")?;
	drop(device);

	let ro = FileBlockDevice::open(&image.path, 0, 8 * BS)?;

	// -- Check: it says so before anything is attempted
	assert!(!ro.is_writable());

	// -- Exec
	let e = ro.write_block(0, b"nope").expect_err("read-only write");

	// -- Check: a named refusal, not an OS `EBADF`. The kernel's is accurate and
	// tells the caller nothing about which file, opened how, by which call.
	match e {
		Error::ReadOnly { detail } => assert!(detail.contains("read-only"), "{detail}"),
		other => panic!("expected ReadOnly, got {other:?}"),
	}

	// -- Check: and the block it was told not to write is unchanged
	let mut buf = block();
	ro.read_block(0, &mut buf)?;
	assert_eq!(&buf[..7], b"durable");
	Ok(())
}

#[test]
fn test_a_read_only_open_sees_a_writable_one_s_writes() -> Result<()> {
	// -- Setup & Fixtures: the read-only-mount-while-writing case, which is the
	// documented reason read-only opens take no lock: a read-only inspector on a
	// volume a running writer holds.
	let image = common::TempImage::new("ro-sees-writes");
	let rw = FileBlockDevice::create(&image.path, 8 * BS)?;

	// -- Exec
	let ro = FileBlockDevice::open(&image.path, 0, 8 * BS)?;
	rw.write_block(3, b"written while open")?;

	// -- Check: positioned reads go to the file, not to a private copy, so the
	// reader sees it without a reopen.
	let mut buf = block();
	ro.read_block(3, &mut buf)?;
	assert_eq!(&buf[..18], b"written while open");
	Ok(())
}

// endregion:    --- Read-only

// region:    --- One writer

#[test]
fn test_a_second_writable_handle_on_one_volume_is_refused() -> Result<()> {
	// -- Setup & Fixtures: two writable handles would each hold a free-space
	// map that does not know about the other's allocations, so both would place
	// objects on the same blocks and one commit would silently overwrite the
	// other's tree. Refusing the second open is the only safe answer.
	let image = common::TempImage::new("locked");
	let first = FileBlockDevice::create(&image.path, 8 * BS)?;

	// -- Exec
	let e = expect_err("a second writer", FileBlockDevice::open_rw(&image.path, 0, 8 * BS));

	// -- Check: named, so a caller can tell "busy" from "permission denied"
	match e {
		Error::Locked { path } => assert_eq!(path, image.path),
		other => panic!("expected Locked, got {other:?}"),
	}

	// -- Exec & Check: and the lock is released with the handle, so a restart
	// after a crash can reopen the volume rather than needing manual cleanup.
	drop(first);
	FileBlockDevice::open_rw(&image.path, 0, 8 * BS)?;
	Ok(())
}

#[test]
fn test_any_number_of_read_only_handles_may_coexist() -> Result<()> {
	// -- Setup & Fixtures
	let image = common::TempImage::new("many-readers");
	let writer = FileBlockDevice::create(&image.path, 8 * BS)?;
	writer.write_block(1, b"shared")?;

	// -- Exec: several readers, plus the writer, all at once
	let readers: Vec<FileBlockDevice> = (0..4)
		.map(|_| FileBlockDevice::open(&image.path, 0, 8 * BS))
		.collect::<Result<_>>()?;

	// -- Check
	for reader in &readers {
		let mut buf = block();
		reader.read_block(1, &mut buf)?;
		assert_eq!(&buf[..6], b"shared");
	}
	Ok(())
}

#[test]
fn test_the_write_lock_is_a_lock_not_a_read_only_marker() -> Result<()> {
	// -- Setup & Fixtures: `MemBlockDevice` is not file-backed and has no lock
	// at all, so the "one writer" rule lives in `FileBlockDevice`. What must
	// hold for both is the *contract* behind it: a device that says it is
	// writable accepts a write.
	let device = MemBlockDevice::new(4);
	assert!(device.is_writable());

	// -- Exec
	device.write_block(0, b"accepted")?;

	// -- Check
	let mut buf = block();
	device.read_block(0, &mut buf)?;
	assert_eq!(&buf[..8], b"accepted");
	Ok(())
}

// endregion:    --- One writer

// region:    --- Geometry of a region inside a bigger file

#[test]
fn test_a_region_that_runs_off_the_end_of_the_file_is_refused_at_open() -> Result<()> {
	// -- Setup & Fixtures: a partition table on a truncated image is the
	// ordinary case for a corrupt download, and it must be refused where the
	// message can name the file, not on the first read, where it cannot.
	let image = common::TempImage::new("short-file");
	image.write(&vec![0u8; 8 * BS as usize])?;

	// -- Exec
	let e = expect_err(
		"a region past the end of the file",
		FileBlockDevice::open(&image.path, 4 * BS, 8 * BS),
	);

	// -- Check: the message carries all three numbers, because "does not fit" is
	// unactionable without knowing by how much.
	match e {
		Error::ImageOpen { path, detail } => {
			assert_eq!(path, image.path);
			assert!(detail.contains("32768"), "must name the end: {detail}");
			assert!(detail.contains("only 32768 bytes"), "must name the file size: {detail}");
		}
		other => panic!("expected ImageOpen, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_an_offset_plus_size_that_overflows_u64_is_refused_at_open() -> Result<()> {
	// -- Setup & Fixtures: the same overflow as the read path, one layer up: a
	// partition entry is a `u64` start plus a `u64` size, and hostile values can
	// sum past the end of the type.
	let image = common::TempImage::new("geometry-overflow");
	image.write(&vec![0u8; 8 * BS as usize])?;

	// -- Exec
	let e = expect_err(
		"an offset that overflows",
		FileBlockDevice::open(&image.path, u64::MAX - 100, 4096),
	);

	// -- Check
	match e {
		Error::ImageOpen { detail, .. } => assert!(detail.contains("overflow"), "{detail}"),
		other => panic!("expected ImageOpen, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_volume_inside_a_file_cannot_address_bytes_outside_itself() -> Result<()> {
	// -- Setup & Fixtures: a 32-block image with a 4-block region in the middle.
	// The region's offsets are bounded by the region, so "write past the end of
	// the volume" must mean "past the end of *this* volume"; an off-by-one here
	// writes over the neighbouring partition, which is the whole reason a
	// partition table exists.
	let image = common::TempImage::new("region");
	let mut whole = vec![0u8; 32 * BS as usize];
	for i in (0..whole.len()).step_by(BS as usize) {
		whole[i] = 0x5A; // one marker byte per block, so neighbours are visible
	}
	image.write(&whole)?;

	let region = FileBlockDevice::open_rw(&image.path, 8 * BS, 4 * BS)?;

	// -- Check
	assert_eq!(region.size_bytes(), 4 * BS);
	assert_eq!(region.block_count(), 4);
	assert_eq!(region.base_offset(), 8 * BS);
	assert!(
		region.read_block(3, &mut block()).is_ok(),
		"the last block of the region is addressable"
	);
	assert!(
		region.read_block(4, &mut block()).is_err(),
		"one block past the region must be refused"
	);

	// -- Exec: write the whole region, then check the bytes on either side
	let mut buf = block();
	region.read_block(0, &mut buf)?;
	assert_eq!(buf[0], 0x5A, "the region must start where it was told to");
	region.write_blocks(0, &vec![0x00u8; 4 * BS as usize])?;

	// -- Check: the file grew by nothing and the neighbours are intact
	assert_eq!(std::fs::metadata(&image.path)?.len(), 32 * BS);
	let after = std::fs::read(&image.path)?;
	assert_eq!(after[8 * BS as usize], 0x00, "the region was written");
	assert_eq!(
		after[7 * BS as usize],
		0x5A,
		"the block before the region was clobbered"
	);
	assert_eq!(
		after[12 * BS as usize],
		0x5A,
		"the block after the region was clobbered"
	);
	Ok(())
}

#[test]
fn test_a_region_at_a_misaligned_offset_is_allowed_but_still_bounded() -> Result<()> {
	// -- Setup & Fixtures: `FileBlockDevice` deliberately does not require
	// block-aligned geometry: a partition table is in 512-byte sectors, and a
	// volume need not begin on a 4 KiB boundary. The addressability rule is
	// about the volume's own size, not about the file's.
	let image = common::TempImage::new("misaligned-region");
	image.write(&vec![0x7Fu8; 16 * BS as usize])?;

	// -- Exec
	let region = FileBlockDevice::open(&image.path, 512, 4 * BS)?;

	// -- Check: four whole blocks are addressable from a 512-byte base, and the
	// base itself is reported so a caller can convert an absolute offset back.
	assert_eq!(region.base_offset(), 512);
	assert_eq!(region.block_count(), 4);
	assert!(region.read_block(3, &mut block()).is_ok());
	assert!(region.read_block(4, &mut block()).is_err());
	Ok(())
}

#[test]
fn test_creating_a_file_at_a_misaligned_size_is_refused() -> Result<()> {
	// -- Setup & Fixtures
	let image = common::TempImage::new("misaligned-create");

	// -- Exec: a volume whose size is not a block multiple would have a final
	// partial block that no LBA can address, so the device would report a size
	// it cannot actually read the tail of.
	let e = expect_err("a misaligned size", FileBlockDevice::create(&image.path, 4097));

	// -- Check
	match e {
		Error::Misaligned {
			context,
			len,
			block_size,
		} => {
			assert_eq!(context, "FileBlockDevice::create");
			assert_eq!(len, 4097);
			assert_eq!(block_size, 4096);
		}
		other => panic!("expected Misaligned, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_creating_a_device_does_not_erase_what_the_file_held() -> Result<()> {
	// -- Setup & Fixtures: `create` is documented to size a file, not wipe it,
	// and the distinction is load-bearing: erasing belongs to `Store::format`,
	// which does it for *every* device kind. If `create` also erased, a
	// partition formatted in place would still hold its old blocks, the one
	// case that has no `create` call at all.
	let image = common::TempImage::new("create-keeps");
	{
		let device = FileBlockDevice::create(&image.path, 8 * BS)?;
		device.write_block(5, b"old contents")?;
	}

	// -- Exec: reopen at the same size
	let device = FileBlockDevice::create(&image.path, 8 * BS)?;

	// -- Check
	let mut buf = block();
	device.read_block(5, &mut buf)?;
	assert_eq!(&buf[..12], b"old contents", "create must size, not erase");

	// -- Exec: but a *shrink* truncates, and whatever the removed blocks held is
	// gone, which is exactly why the erase cannot live here.
	drop(device);
	let shrunk = FileBlockDevice::create(&image.path, 4 * BS)?;
	assert_eq!(shrunk.block_count(), 4);
	assert!(
		shrunk.read_block(5, &mut buf).is_err(),
		"block 5 is outside the shrunken volume"
	);
	Ok(())
}

#[test]
fn test_a_zero_block_region_is_openable_and_empty() -> Result<()> {
	// -- Setup & Fixtures: an MBR entry with `sectors == 0` is skipped by the
	// partition reader, but the device itself must still behave if a caller
	// opens a zero-length region directly rather than through the table.
	let image = common::TempImage::new("zero-region");
	image.write(&vec![0u8; 4 * BS as usize])?;

	// -- Exec
	let region = FileBlockDevice::open(&image.path, 0, 0)?;

	// -- Check
	assert_eq!(region.block_count(), 0);
	assert_eq!(region.size_bytes(), 0);
	assert!(
		region.read_block(0, &mut block()).is_err(),
		"a zero-block volume has no block 0"
	);
	Ok(())
}

// endregion:    --- Geometry of a region inside a bigger file

// region:    --- Adopting an existing buffer

#[test]
fn test_adopting_a_buffer_whose_length_is_not_a_whole_number_of_blocks_is_refused() -> Result<()> {
	// -- Setup & Fixtures: the length *is* the volume's size, so a partial final
	// block would define a block count that does not match the bytes present.
	let e = expect_err("a ragged buffer", MemBlockDevice::from_bytes(vec![0u8; 4097]));

	// -- Check
	match e {
		Error::Misaligned {
			context,
			len,
			block_size,
		} => {
			assert_eq!(context, "MemBlockDevice::from_bytes");
			assert_eq!(len, 4097);
			assert_eq!(block_size, 4096);
		}
		other => panic!("expected Misaligned, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_an_empty_buffer_is_a_zero_block_volume_not_an_error() -> Result<()> {
	// -- Setup & Fixtures: zero is a multiple of 4096. It is a useless volume;
	// nothing can be mounted from it, but it is not a *malformed* one, and
	// refusing it here would mean the interesting failure ("this is not a
	// filesystem") never gets to be the one reported.
	let device = MemBlockDevice::from_bytes(Vec::new())?;

	// -- Check
	assert_eq!(device.block_count(), 0);
	assert!(device.read_bytes_at(0, &mut [0u8; 1]).is_err());
	Ok(())
}

#[test]
fn test_clones_of_a_device_share_one_buffer() -> Result<()> {
	// -- Setup & Fixtures: this is documented, and the whole crate's tests rely
	// on it to snapshot a volume while a `Store` owns the device. A clone that
	// deep-copied would make every test that reopens a volume silently observe
	// the pre-mount state instead of the current one.
	let device = MemBlockDevice::new(4);
	let clone = device.clone();

	// -- Exec
	device.write_block(2, b"through the original")?;

	// -- Check
	let mut buf = block();
	clone.read_block(2, &mut buf)?;
	assert_eq!(&buf[..20], b"through the original");

	// -- Exec & Check: and the other way, so it is one buffer rather than two
	// copies kept in step.
	clone.write_block(3, b"through the clone")?;
	device.read_block(3, &mut buf)?;
	assert_eq!(&buf[..17], b"through the clone");
	Ok(())
}

#[test]
fn test_a_snapshot_is_detached_from_later_writes() -> Result<()> {
	// -- Setup & Fixtures: `bytes_snapshot` is how a test (or an application
	// taking a backup) captures a volume. It must be a copy: a `Vec` that
	// tracked the device would make "the bytes before the crash" mean "the bytes
	// after it".
	let device = MemBlockDevice::new(4);
	device.write_block(1, b"before")?;

	// -- Exec
	let before = device.bytes_snapshot();
	device.write_block(1, b"after")?;

	// -- Check
	assert_eq!(&before[BS as usize..BS as usize + 6], b"before");
	let mut buf = block();
	device.read_block(1, &mut buf)?;
	assert_eq!(&buf[..5], b"after");
	assert_eq!(before.len(), 4 * BS as usize, "a snapshot is the whole volume");
	Ok(())
}

// endregion:    --- Adopting an existing buffer
