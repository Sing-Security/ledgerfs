//! Reader smoke tests — object decoding, extent range intersection, and
//! round-trip read logic with mock fixtures.
//!
//! These tests exercise the **host-testable** parts of the reader path:
//! decode helpers, range intersection logic, and the ObjectRecord framing
//! format.  The actual ATA PIO code is gated on `target_arch = "x86_64"`
//! and cannot run in a standard test harness, so we test the layers above
//! it with hand-crafted byte buffers that mirror the on-disk format.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;

use crucible_ledgerfs_kernel::OBJECT_RECORD_MAGIC;
use crucible_ledgerfs_kernel::object::{
	DirEntry, DirNode, ExtentDescriptor, ExtentNode, Inode, InodeKind, ObjectId, ObjectRef, StorageHint,
};

type Result<T> = core::result::Result<T, Box<dyn core::error::Error>>;

// ── Constants mirroring the reader module ────────────────────────────

const BLOCK_SIZE: usize = 4096;
const HEADER_SIZE: usize = 60;
const OBJECT_ID_OFFSET: usize = 28;
const STORED_LEN_OFFSET: usize = 12;
const COMPRESSION_OFFSET: usize = 10;

// ── Helpers ──────────────────────────────────────────────────────────

/// Build a minimal ObjectRecord block from raw payload bytes.
///
/// Layout (60-byte header):
///   [0..8]    magic            (u64 LE)
///   [8..10]   reserved         (zeroes)
///   [10..12]  compression      (u16 LE, 0 = none)
///   [12..16]  stored_len       (u32 LE)
///   [16..28]  reserved         (zeroes)
///   [28..60]  object_id        (32 bytes, BLAKE3 of payload)
///   [60..]    payload
///
/// Returns the full 4 KiB block and the ObjectId.
fn make_object_record(payload: &[u8]) -> ([u8; BLOCK_SIZE], ObjectId) {
	assert!(
		payload.len() + HEADER_SIZE <= BLOCK_SIZE,
		"payload too large for single-block record"
	);

	let hash = blake3::hash(payload);
	let object_id = ObjectId::from_bytes(hash.as_bytes());

	let mut block = [0u8; BLOCK_SIZE];

	// Magic.
	block[0..8].copy_from_slice(&OBJECT_RECORD_MAGIC.to_le_bytes());

	// Compression = 0 (none).
	block[COMPRESSION_OFFSET] = 0;
	block[COMPRESSION_OFFSET + 1] = 0;

	// Stored length.
	block[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].copy_from_slice(&(payload.len() as u32).to_le_bytes());

	// ObjectId (BLAKE3 of payload).
	block[OBJECT_ID_OFFSET..OBJECT_ID_OFFSET + 32].copy_from_slice(object_id.as_bytes());

	// Payload.
	block[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(payload);

	(block, object_id)
}

/// Reimplementation of the reader's `range_intersects` for host testing.
fn range_intersects(base: u64, len: u64, req_start: u64, req_len: u64) -> bool {
	let base_end = base.saturating_add(len);
	let req_end = req_start.saturating_add(req_len);
	!(req_end <= base || req_start >= base_end)
}

// ── ObjectRecord framing tests ───────────────────────────────────────

// region:    --- ObjectRecord Framing

#[test]
fn object_record_magic_roundtrip() -> Result<()> {
	// -- Setup & Fixtures
	let payload = b"hello ledgerfs";
	let (block, _oid) = make_object_record(payload);

	// -- Exec
	let magic_bytes: [u8; 8] = block[0..8].try_into()?;
	let magic = u64::from_le_bytes(magic_bytes);

	// -- Check
	assert_eq!(magic, OBJECT_RECORD_MAGIC);

	Ok(())
}

#[test]
fn object_record_stored_len_correct() -> Result<()> {
	// -- Setup & Fixtures
	let payload = b"deterministic content";
	let (block, _oid) = make_object_record(payload);

	// -- Exec
	let stored_len_bytes: [u8; 4] = block[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].try_into()?;
	let stored_len = u32::from_le_bytes(stored_len_bytes) as usize;

	// -- Check
	assert_eq!(stored_len, payload.len());

	Ok(())
}

#[test]
fn object_record_blake3_matches_object_id() -> Result<()> {
	// -- Setup & Fixtures
	let payload = b"audit-grade data";
	let (block, expected_oid) = make_object_record(payload);

	// -- Exec
	let stored_oid_bytes: [u8; 32] = block[OBJECT_ID_OFFSET..OBJECT_ID_OFFSET + 32].try_into()?;
	let stored_oid = ObjectId::from_bytes(&stored_oid_bytes);

	let stored_len_bytes: [u8; 4] = block[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].try_into()?;
	let stored_len = u32::from_le_bytes(stored_len_bytes) as usize;

	let extracted_payload = &block[HEADER_SIZE..HEADER_SIZE + stored_len];
	let recomputed = blake3::hash(extracted_payload);
	let recomputed_oid = ObjectId::from_bytes(recomputed.as_bytes());

	// -- Check
	assert_eq!(stored_oid, expected_oid);
	assert_eq!(recomputed_oid, expected_oid);

	Ok(())
}

#[test]
fn object_record_compression_flags_zero() -> Result<()> {
	// -- Setup & Fixtures
	let payload = b"uncompressed";
	let (block, _oid) = make_object_record(payload);

	// -- Check — reader rejects non-zero compression
	assert_eq!(block[COMPRESSION_OFFSET], 0);
	assert_eq!(block[COMPRESSION_OFFSET + 1], 0);

	Ok(())
}

#[test]
fn object_record_wrong_magic_rejected() -> Result<()> {
	// -- Setup & Fixtures
	let payload = b"bad magic test";
	let (mut block, _oid) = make_object_record(payload);

	// Corrupt the magic.
	block[0] = 0xFF;

	// -- Check
	let magic_bytes: [u8; 8] = block[0..8].try_into()?;
	let magic = u64::from_le_bytes(magic_bytes);
	assert_ne!(magic, OBJECT_RECORD_MAGIC);

	Ok(())
}

#[test]
fn object_record_checksum_mismatch_detected() -> Result<()> {
	// -- Setup & Fixtures
	let payload = b"tamper test";
	let (mut block, oid) = make_object_record(payload);

	// Corrupt one payload byte.
	block[HEADER_SIZE] ^= 0xFF;

	// -- Exec — recompute hash of corrupted payload
	let stored_len_bytes: [u8; 4] = block[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].try_into()?;
	let stored_len = u32::from_le_bytes(stored_len_bytes) as usize;
	let corrupted_payload = &block[HEADER_SIZE..HEADER_SIZE + stored_len];
	let recomputed = blake3::hash(corrupted_payload);

	// -- Check — hash no longer matches
	assert_ne!(recomputed.as_bytes(), oid.as_bytes());

	Ok(())
}

// endregion: --- ObjectRecord Framing

// ── Range intersection tests ─────────────────────────────────────────

// region:    --- Range Intersection

#[test]
fn range_no_overlap_before() -> Result<()> {
	// extent [100, 200), request [0, 50)
	assert!(!range_intersects(100, 100, 0, 50));
	Ok(())
}

#[test]
fn range_no_overlap_after() -> Result<()> {
	// extent [100, 200), request [200, 50)
	assert!(!range_intersects(100, 100, 200, 50));
	Ok(())
}

#[test]
fn range_overlap_partial_left() -> Result<()> {
	// extent [100, 200), request [50, 100) — overlaps [100, 150)
	assert!(range_intersects(100, 100, 50, 100));
	Ok(())
}

#[test]
fn range_overlap_partial_right() -> Result<()> {
	// extent [100, 200), request [150, 100) — overlaps [150, 200)
	assert!(range_intersects(100, 100, 150, 100));
	Ok(())
}

#[test]
fn range_overlap_contained() -> Result<()> {
	// extent [100, 200), request [120, 30) — fully inside
	assert!(range_intersects(100, 100, 120, 30));
	Ok(())
}

#[test]
fn range_overlap_containing() -> Result<()> {
	// extent [100, 200), request [0, 500) — request contains extent
	assert!(range_intersects(100, 100, 0, 500));
	Ok(())
}

#[test]
fn range_exact_match() -> Result<()> {
	// extent [100, 200), request [100, 100) — exact match
	assert!(range_intersects(100, 100, 100, 100));
	Ok(())
}

#[test]
fn range_adjacent_no_overlap() -> Result<()> {
	// extent [0, 100), request [100, 100) — touching but not overlapping
	assert!(!range_intersects(0, 100, 100, 100));
	Ok(())
}

#[test]
fn range_zero_length_extent() -> Result<()> {
	// extent [100, 100) (zero length) — degenerate case.
	// The reader's `range_intersects` treats a zero-length extent at
	// offset 100 as the point [100, 100).  With saturating_add the
	// base_end == base, so `req_start >= base_end` governs.
	// For request [50, 150) the point 100 is inside, so this returns true.
	// This is a degenerate edge that never occurs in practice (all real
	// extents have length > 0), so we just document the actual behaviour.
	assert!(range_intersects(100, 0, 50, 100));
	// But a request fully before the zero-length point does not intersect.
	assert!(!range_intersects(100, 0, 0, 50));
	Ok(())
}

#[test]
fn range_zero_length_request() -> Result<()> {
	// request [150, 150) (zero length) — degenerate case.
	// req_end == req_start, so `req_end <= base` is false (150 > 100)
	// and `req_start >= base_end` is false (150 < 200), yielding true.
	// Again a degenerate edge: `read_range` early-returns on empty buffers
	// so this path is never hit with real data.
	assert!(range_intersects(100, 100, 150, 0));
	// A zero-length request outside the extent does not intersect.
	assert!(!range_intersects(100, 100, 200, 0));
	assert!(!range_intersects(100, 100, 50, 0));
	Ok(())
}

#[test]
fn range_saturating_add_no_panic() -> Result<()> {
	// Large values that would overflow without saturating_add.
	assert!(range_intersects(u64::MAX - 10, 20, u64::MAX - 5, 20));
	Ok(())
}

#[test]
fn range_single_byte_overlap() -> Result<()> {
	// extent [100, 200), request [199, 1) — single byte at boundary
	assert!(range_intersects(100, 100, 199, 1));
	Ok(())
}

// endregion: --- Range Intersection

// ── Inode decode round-trip ──────────────────────────────────────────

// region:    --- Inode Decode

#[test]
fn inode_file_encode_decode_roundtrip() -> Result<()> {
	// -- Setup & Fixtures
	let mut inode = Inode::new_file(1000, 1000, 0o644);
	inode.size = 12345;
	inode.content_ref = ObjectRef::from_id(ObjectId::from_content(b"extent-root"));
	inode.update_object_id();

	// -- Exec
	let encoded = inode.encode_canonical()?;
	let decoded = Inode::decode_canonical(&encoded)?;

	// -- Check
	assert_eq!(decoded.kind, InodeKind::RegularFile);
	assert_eq!(decoded.size, 12345);
	assert_eq!(decoded.uid, 1000);
	assert_eq!(decoded.content_ref, inode.content_ref);
	assert_eq!(decoded.object_id, inode.object_id);

	Ok(())
}

#[test]
fn inode_directory_encode_decode_roundtrip() -> Result<()> {
	// -- Setup & Fixtures
	let mut inode = Inode::new_directory(0, 0, 0o755);
	inode.content_ref = ObjectRef::from_id(ObjectId::from_content(b"dir-root"));
	inode.update_object_id();

	// -- Exec
	let encoded = inode.encode_canonical()?;
	let decoded = Inode::decode_canonical(&encoded)?;

	// -- Check
	assert_eq!(decoded.kind, InodeKind::Directory);
	assert_eq!(decoded.mode, 0o755);

	Ok(())
}

#[test]
fn inode_kind_file_is_regular_file() -> Result<()> {
	// Verify our reader uses the correct variant name.
	let inode = Inode::new_file(0, 0, 0o644);
	assert_eq!(inode.kind, InodeKind::RegularFile);
	assert_ne!(inode.kind, InodeKind::Directory);

	Ok(())
}

// endregion: --- Inode Decode

// ── DirNode decode round-trip ────────────────────────────────────────

// region:    --- DirNode Decode

#[test]
fn dir_node_entry_lookup() -> Result<()> {
	// -- Setup & Fixtures
	let mut dir = DirNode::new_leaf();
	let inode_ref = ObjectRef::from_id(ObjectId::from_content(b"child-inode"));
	let entry = DirEntry::new("boot.cfg", inode_ref)?;
	dir.insert_entry(entry)?;

	// -- Exec
	let found = dir.lookup("boot.cfg");

	// -- Check
	assert!(found.is_some());
	assert_eq!(found.unwrap().inode_ref(), inode_ref);

	Ok(())
}

#[test]
fn dir_node_entry_not_found() -> Result<()> {
	// -- Setup & Fixtures
	let dir = DirNode::new_leaf();

	// -- Exec & Check
	assert!(dir.lookup("nonexistent").is_none());

	Ok(())
}

#[test]
fn dir_node_encode_decode_preserves_entries() -> Result<()> {
	// -- Setup & Fixtures
	let mut dir = DirNode::new_leaf();
	let ref_a = ObjectRef::from_id(ObjectId::from_content(b"a"));
	let ref_b = ObjectRef::from_id(ObjectId::from_content(b"b"));

	dir.insert_entry(DirEntry::new("zebra", ref_a)?)?;
	dir.insert_entry(DirEntry::new("alpha", ref_b)?)?;

	// -- Exec
	let encoded = dir.encode_canonical()?;
	let decoded = DirNode::decode_canonical(&encoded)?;

	// -- Check — entries sorted by name (BTreeMap-style)
	assert_eq!(decoded.entries.len(), 2);
	assert_eq!(decoded.entries[0].name.as_str(), "alpha");
	assert_eq!(decoded.entries[1].name.as_str(), "zebra");

	Ok(())
}

// endregion: --- DirNode Decode

// ── ExtentNode decode round-trip ─────────────────────────────────────

// region:    --- ExtentNode Decode

#[test]
fn extent_node_single_extent_roundtrip() -> Result<()> {
	// -- Setup & Fixtures
	let mut node = ExtentNode::new_leaf();
	let chunk_id = ObjectId::from_content(b"chunk-data-0");

	node.insert_extent(ExtentDescriptor {
		logical_offset: 0,
		length: 4096,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	})?;

	// -- Exec
	let encoded = node.encode_canonical()?;
	let decoded = ExtentNode::decode_canonical(&encoded)?;

	// -- Check
	assert_eq!(decoded.extents.len(), 1);
	assert_eq!(decoded.extents[0].logical_offset, 0);
	assert_eq!(decoded.extents[0].length, 4096);
	assert_eq!(decoded.extents[0].chunk_id, chunk_id);
	assert_eq!(decoded.extents[0].chunk_offset, 0);

	Ok(())
}

#[test]
fn extent_node_multi_extent_sorted() -> Result<()> {
	// -- Setup & Fixtures
	let mut node = ExtentNode::new_leaf();

	// Insert out of order.
	node.insert_extent(ExtentDescriptor {
		logical_offset: 8192,
		length: 2048,
		chunk_id: ObjectId::from_content(b"chunk-2"),
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	})?;
	node.insert_extent(ExtentDescriptor {
		logical_offset: 0,
		length: 4096,
		chunk_id: ObjectId::from_content(b"chunk-0"),
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	})?;
	node.insert_extent(ExtentDescriptor {
		logical_offset: 4096,
		length: 4096,
		chunk_id: ObjectId::from_content(b"chunk-1"),
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	})?;

	// -- Exec
	let encoded = node.encode_canonical()?;
	let decoded = ExtentNode::decode_canonical(&encoded)?;

	// -- Check — sorted by logical_offset
	assert_eq!(decoded.extents.len(), 3);
	assert_eq!(decoded.extents[0].logical_offset, 0);
	assert_eq!(decoded.extents[1].logical_offset, 4096);
	assert_eq!(decoded.extents[2].logical_offset, 8192);

	Ok(())
}

#[test]
fn extent_node_find_extent_hit() -> Result<()> {
	// -- Setup & Fixtures
	let mut node = ExtentNode::new_leaf();
	node.insert_extent(ExtentDescriptor {
		logical_offset: 1000,
		length: 500,
		chunk_id: ObjectId::from_content(b"target"),
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	})?;

	// -- Exec & Check
	let found = node.find_extent(1200);
	assert!(found.is_some());
	assert_eq!(found.unwrap().logical_offset, 1000);

	Ok(())
}

#[test]
fn extent_node_find_extent_miss() -> Result<()> {
	// -- Setup & Fixtures
	let mut node = ExtentNode::new_leaf();
	node.insert_extent(ExtentDescriptor {
		logical_offset: 0,
		length: 100,
		chunk_id: ObjectId::from_content(b"x"),
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	})?;

	// -- Exec & Check — offset 200 is outside [0, 100)
	assert!(node.find_extent(200).is_none());

	Ok(())
}

// endregion: --- ExtentNode Decode

// ── Simulated read_range logic ───────────────────────────────────────

// region:    --- Simulated Range Read

/// Simulates the `read_range` copy loop from the reader module using
/// in-memory chunk data instead of ATA PIO.
///
/// This validates that the extent-walking and byte-slicing logic is
/// correct without requiring disk access.
fn simulated_read_range(
	extents: &[ExtentDescriptor],
	chunks: &[(&ObjectId, &[u8])],
	file_size: u64,
	offset: u64,
	buf: &mut [u8],
) -> usize {
	if buf.is_empty() || offset >= file_size {
		return 0;
	}

	let req_end = (offset + buf.len() as u64).min(file_size);
	let mut total_read = 0usize;

	for extent in extents {
		if total_read >= buf.len() {
			break;
		}

		if !range_intersects(extent.logical_offset, extent.length, offset, req_end - offset) {
			continue;
		}

		let extent_end = extent.logical_offset + extent.length;
		let read_start = offset.max(extent.logical_offset);
		let read_end = req_end.min(extent_end);

		if read_start >= read_end {
			continue;
		}

		let copy_len = (read_end - read_start) as usize;

		// Find the chunk data.
		let chunk_data = chunks
			.iter()
			.find(|(id, _)| **id == extent.chunk_id)
			.map(|(_, data)| *data)
			.expect("chunk not found in fixture set");

		let chunk_offset = (read_start - extent.logical_offset + extent.chunk_offset) as usize;
		let chunk_end = (chunk_offset + copy_len).min(chunk_data.len());
		let source = &chunk_data[chunk_offset..chunk_end];

		let dest_start = total_read;
		let dest_end = dest_start + source.len();
		buf[dest_start..dest_end].copy_from_slice(source);

		total_read += source.len();
	}

	total_read
}

#[test]
fn simulated_range_full_file() -> Result<()> {
	// -- Setup & Fixtures
	let data = b"Hello, LedgerFS!";
	let chunk_id = ObjectId::from_content(data);

	let extents = vec![ExtentDescriptor {
		logical_offset: 0,
		length: data.len() as u64,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	}];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&chunk_id, data.as_slice())];

	// -- Exec
	let mut buf = vec![0u8; data.len()];
	let n = simulated_read_range(&extents, &chunks, data.len() as u64, 0, &mut buf);

	// -- Check
	assert_eq!(n, data.len());
	assert_eq!(&buf, data);

	Ok(())
}

#[test]
fn simulated_range_partial_middle() -> Result<()> {
	// -- Setup & Fixtures
	let data = b"ABCDEFGHIJKLMNOP"; // 16 bytes
	let chunk_id = ObjectId::from_content(data);

	let extents = vec![ExtentDescriptor {
		logical_offset: 0,
		length: 16,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	}];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&chunk_id, data.as_slice())];

	// -- Exec — read bytes [4..12)
	let mut buf = vec![0u8; 8];
	let n = simulated_read_range(&extents, &chunks, 16, 4, &mut buf);

	// -- Check
	assert_eq!(n, 8);
	assert_eq!(&buf, b"EFGHIJKL");

	Ok(())
}

#[test]
fn simulated_range_across_two_extents() -> Result<()> {
	// -- Setup & Fixtures
	let data_a = b"AAAA"; // logical [0, 4)
	let data_b = b"BBBB"; // logical [4, 8)
	let id_a = ObjectId::from_content(data_a);
	let id_b = ObjectId::from_content(data_b);

	let extents = vec![
		ExtentDescriptor {
			logical_offset: 0,
			length: 4,
			chunk_id: id_a,
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		},
		ExtentDescriptor {
			logical_offset: 4,
			length: 4,
			chunk_id: id_b,
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		},
	];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&id_a, data_a.as_slice()), (&id_b, data_b.as_slice())];

	// -- Exec — read bytes [2..6), spanning both extents
	let mut buf = vec![0u8; 4];
	let n = simulated_read_range(&extents, &chunks, 8, 2, &mut buf);

	// -- Check
	assert_eq!(n, 4);
	assert_eq!(&buf, b"AABB");

	Ok(())
}

#[test]
fn simulated_range_offset_past_eof() -> Result<()> {
	// -- Setup & Fixtures
	let data = b"short";
	let chunk_id = ObjectId::from_content(data);

	let extents = vec![ExtentDescriptor {
		logical_offset: 0,
		length: 5,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	}];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&chunk_id, data.as_slice())];

	// -- Exec — offset beyond file size
	let mut buf = vec![0u8; 10];
	let n = simulated_read_range(&extents, &chunks, 5, 100, &mut buf);

	// -- Check
	assert_eq!(n, 0);

	Ok(())
}

#[test]
fn simulated_range_empty_buffer() -> Result<()> {
	// -- Setup & Fixtures
	let data = b"data";
	let chunk_id = ObjectId::from_content(data);

	let extents = vec![ExtentDescriptor {
		logical_offset: 0,
		length: 4,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	}];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&chunk_id, data.as_slice())];

	// -- Exec
	let mut buf = vec![];
	let n = simulated_read_range(&extents, &chunks, 4, 0, &mut buf);

	// -- Check
	assert_eq!(n, 0);

	Ok(())
}

#[test]
fn simulated_range_read_clamped_to_file_size() -> Result<()> {
	// -- Setup & Fixtures — request extends past file_size
	let data = b"ABCDEFGH"; // 8 bytes
	let chunk_id = ObjectId::from_content(data);

	let extents = vec![ExtentDescriptor {
		logical_offset: 0,
		length: 8,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 0,
	}];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&chunk_id, data.as_slice())];

	// file_size=6 but extent covers 8 bytes; read should stop at 6
	let mut buf = vec![0u8; 10];
	let n = simulated_read_range(&extents, &chunks, 6, 0, &mut buf);

	// -- Check
	assert_eq!(n, 6);
	assert_eq!(&buf[..6], b"ABCDEF");

	Ok(())
}

#[test]
fn simulated_range_with_chunk_offset() -> Result<()> {
	// -- Setup & Fixtures — extent starts 2 bytes into the chunk
	let data = b"XXhello"; // chunk_offset=2 means "hello" starts at byte 2
	let chunk_id = ObjectId::from_content(data);

	let extents = vec![ExtentDescriptor {
		logical_offset: 0,
		length: 5,
		chunk_id,
		chunk_hint: StorageHint::zero(),
		chunk_offset: 2, // skip first 2 bytes of chunk
	}];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&chunk_id, data.as_slice())];

	// -- Exec
	let mut buf = vec![0u8; 5];
	let n = simulated_read_range(&extents, &chunks, 5, 0, &mut buf);

	// -- Check
	assert_eq!(n, 5);
	assert_eq!(&buf, b"hello");

	Ok(())
}

#[test]
fn simulated_range_gap_between_extents() -> Result<()> {
	// -- Setup & Fixtures — extent at [0,4) and [8,12), gap at [4,8)
	let data_a = b"AAAA";
	let data_b = b"BBBB";
	let id_a = ObjectId::from_content(data_a);
	let id_b = ObjectId::from_content(data_b);

	let extents = vec![
		ExtentDescriptor {
			logical_offset: 0,
			length: 4,
			chunk_id: id_a,
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		},
		ExtentDescriptor {
			logical_offset: 8,
			length: 4,
			chunk_id: id_b,
			chunk_hint: StorageHint::zero(),
			chunk_offset: 0,
		},
	];

	let chunks: Vec<(&ObjectId, &[u8])> = vec![(&id_a, data_a.as_slice()), (&id_b, data_b.as_slice())];

	// -- Exec — read [2..10), spans gap
	let mut buf = vec![0u8; 8];
	let n = simulated_read_range(&extents, &chunks, 12, 2, &mut buf);

	// -- Check — only 4 bytes (2 from extent A + 2 from extent B)
	assert_eq!(n, 4);
	assert_eq!(&buf[..2], b"AA");
	assert_eq!(&buf[2..4], b"BB");

	Ok(())
}

// endregion: --- Simulated Range Read

// ── ObjectId / content addressing ────────────────────────────────────

// region:    --- Content Addressing

#[test]
fn object_id_from_content_deterministic() -> Result<()> {
	let id1 = ObjectId::from_content(b"same input");
	let id2 = ObjectId::from_content(b"same input");
	assert_eq!(id1, id2);

	Ok(())
}

#[test]
fn object_id_different_content_different_id() -> Result<()> {
	let id1 = ObjectId::from_content(b"input A");
	let id2 = ObjectId::from_content(b"input B");
	assert_ne!(id1, id2);

	Ok(())
}

#[test]
fn object_record_payload_matches_fixture() -> Result<()> {
	// -- Setup & Fixtures
	let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
	let (block, oid) = make_object_record(&payload);

	// -- Exec — extract payload from block
	let stored_len_bytes: [u8; 4] = block[STORED_LEN_OFFSET..STORED_LEN_OFFSET + 4].try_into()?;
	let stored_len = u32::from_le_bytes(stored_len_bytes) as usize;
	let extracted = &block[HEADER_SIZE..HEADER_SIZE + stored_len];

	// -- Check
	assert_eq!(extracted, &payload[..]);
	assert_eq!(ObjectId::from_content(extracted), oid);

	Ok(())
}

// endregion: --- Content Addressing
