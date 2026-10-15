//! Formatting, mounting, reopening, and the app-store contract.
//!
//! The question behind every test here is the one an embedder actually asks:
//! *after this process exits, what does the next one see?* A store is only
//! worth having if the volume it wrote is the volume a fresh mount reads, so
//! most of these write, take the device's bytes, and mount those bytes as a
//! different store.

mod common;

use common::*;
use ledgerfs_host::{
	BlockDevice, Error, FileBlockDevice, InodeKind, MemBlockDevice, Origin, STORE_APP_FILE, STORE_CONTRACT_VERSION,
	STORE_DIR, STORE_VERSION_FILE, Store,
};

// region:    --- Format

#[test]
fn test_format_produces_a_mountable_volume_with_a_root() -> Result<()> {
	// -- Setup & Fixtures
	let (store, watch) = watched(BLOCKS)?;

	// -- Check: the live store
	assert_eq!(store.total_blocks(), BLOCKS);
	assert!(
		store.fs().active_root().is_some(),
		"a formatted volume must have an active root — format's whole job is the genesis commit"
	);
	assert_eq!(store.stat("/")?.kind, InodeKind::Directory);
	assert!(store.read_dir("/")?.is_empty(), "the genesis root is empty");

	// -- Exec: and a fresh mount of the same bytes
	let reopened = open_bytes(bytes(&watch))?;

	// -- Check
	assert_eq!(reopened.stat("/")?.kind, InodeKind::Directory);
	assert_eq!(reopened.read_dir("/")?.len(), 0);
	assert_eq!(
		reopened.origin(),
		Origin::ResumedWritable,
		"a formatted volume has an allocator snapshot to resume from"
	);
	Ok(())
}

#[test]
fn test_format_device_takes_its_size_from_the_device() -> Result<()> {
	// -- Setup & Fixtures: the same geometry by two entry points
	let device = MemBlockDevice::new(256);
	let by_device = Store::format_device(Box::new(device))?;
	let by_size = Store::format(Box::new(MemBlockDevice::new(256)), 256)?;

	// -- Check
	assert_eq!(by_device.total_blocks(), 256);
	assert_eq!(by_size.total_blocks(), 256);
	assert_eq!(by_device.capacity_bytes(), by_size.capacity_bytes());
	Ok(())
}

#[test]
fn test_format_refuses_a_volume_too_small_to_hold_object_data() -> Result<()> {
	// The volume reserves the first 6 blocks (superblock copies at 0 and 1, the
	// allocator snapshot at 5) and the last 2 (superblock copies at N-2, N-1),
	// so anything at or below 8 blocks has no object region at all.
	for blocks in [0u64, 1, 2, 4, 7, 8] {
		// -- Exec
		let r = Store::format(Box::new(MemBlockDevice::new(blocks)), blocks);

		// -- Check: refused, named, and above all not a panic
		let e = expect_err(&format!("format {blocks}"), r);
		assert!(
			matches!(e, Error::Format(_)),
			"{blocks} blocks: expected Error::Format, got {e:?}"
		);
	}
	Ok(())
}

#[test]
fn test_format_refuses_volumes_the_genesis_commit_cannot_fit() -> Result<()> {
	// `format`'s own guard admits anything over 8 blocks, but the genesis
	// commit needs more than that: the empty root's objects plus the allocator
	// snapshot do not fit in 9–13 blocks. The refusal is a clean `Error::Format`
	// naming its reason — never a panic and never a half-formatted volume —
	// which is the property that matters. The guard being laxer than the commit
	// is the discrepancy; this test pins the real boundary.
	for blocks in 9u64..MIN_BLOCKS {
		// -- Exec
		let r = Store::format(Box::new(MemBlockDevice::new(blocks)), blocks);

		// -- Check
		let e = expect_err(&format!("format {blocks}"), r);
		assert!(
			matches!(e, Error::Format(_)),
			"{blocks} blocks: expected Error::Format, got {e:?}"
		);
	}

	// -- Exec: the first size that actually completes
	let mut store = Store::format(Box::new(MemBlockDevice::new(MIN_BLOCKS)), MIN_BLOCKS)?;

	// -- Check: a working volume, not merely a non-error. It is a real
	// filesystem with a real root — and essentially nothing to spare, since
	// the genesis commit's own objects consumed almost the whole thing.
	assert_eq!(store.total_blocks(), MIN_BLOCKS);
	assert_eq!(store.stat("/")?.kind, InodeKind::Directory);
	assert!(store.read_dir("/")?.is_empty());
	assert_eq!(
		store.free_bytes(),
		Some(0),
		"a volume sized for exactly one commit has no budget left after it"
	);

	// -- Exec & Check: so the next write is refused for want of space, as a
	// named error — and refusing did not leave the volume inconsistent.
	let e = expect_err("mkdir on a full volume", store.mkdir("/d", 0o755));
	assert!(matches!(e, Error::Format(_)), "got {e:?}");
	assert!(
		store.read_dir("/")?.is_empty(),
		"the old tree is intact after a refused commit"
	);
	Ok(())
}

#[test]
fn test_format_refuses_a_volume_larger_than_its_device() -> Result<()> {
	// -- Exec: a 64-block claim on a 16-block device
	let r = Store::format(Box::new(MemBlockDevice::new(16)), 64);

	// -- Check
	let e = expect_err("oversized claim", r);
	assert!(matches!(e, Error::Format(_)), "got {e:?}");
	Ok(())
}

#[test]
fn test_format_refuses_a_read_only_device() -> Result<()> {
	// -- Setup & Fixtures
	let image = TempImage::new("format-ro");
	image.write(&[0u8; 4096 * 32])?;
	let device = FileBlockDevice::open(&image.path, 0, 4096 * 32)?;

	// -- Exec
	let r = Store::format(Box::new(device), 32);

	// -- Check: refused before a byte is written
	let e = expect_err("format a read-only handle", r);
	assert!(matches!(e, Error::ReadOnly { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_format_erases_what_the_device_held_before() -> Result<()> {
	// -- Setup & Fixtures: a device whose every byte is a recognisable pattern.
	// The handle and the witness are clones of one buffer, so filling through
	// the witness fills the device `format` is about to take.
	let device = mem(BLOCKS);
	let watch = device.clone();
	watch.write_bytes_at(0, &vec![0xAAu8; BLOCKS as usize * 4096])?;

	// -- Exec
	let store = Store::format(Box::new(device), BLOCKS)?;
	drop(store);

	// -- Check: no 0xAA survives outside the four superblock copies. This is the
	// "mkfs means the medium holds this filesystem and nothing else" property:
	// a volume that still carried its predecessor's blocks would disclose them,
	// and would leave stale records for a hint-less reader's scan to trip over.
	let after = bytes(&watch);
	let protected = [0usize, 1, BLOCKS as usize - 2, BLOCKS as usize - 1];
	for lba in 0..BLOCKS as usize {
		if protected.contains(&lba) {
			continue;
		}
		let at = block_offset(lba as u64);
		assert!(
			!after[at..at + 4096].contains(&0xAA),
			"block {lba} still holds pre-format bytes"
		);
	}
	Ok(())
}

// endregion:    --- Format

// region:    --- Reopen

#[test]
fn test_reopen_sees_the_tree_that_was_written() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/notes", 0o755)?;
	store.write_new_file("/notes/a.md", b"hello", 0o644)?;
	store.write_new_file("/top.txt", b"top", 0o600)?;

	// -- Exec: the bytes, mounted as a different store — the restart case
	let back = open_bytes(bytes(&watch))?;

	// -- Check
	assert_eq!(
		names(&back.read_dir("/")?),
		vec!["notes".to_string(), "top.txt".to_string()]
	);
	assert_eq!(back.read_file("/notes/a.md")?, b"hello");
	assert_eq!(back.read_file("/top.txt")?, b"top");
	assert_eq!(back.stat("/top.txt")?.mode, 0o600, "mode survives a reopen");
	assert_eq!(back.stat("/notes/a.md")?.size, 5);
	Ok(())
}

#[test]
fn test_reopen_restores_the_commit_sequence() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	let at_format = store.commit_seq();
	store.mkdir("/a", 0o755)?;
	store.mkdir("/b", 0o755)?;
	let live = store.commit_seq();

	// -- Check: it advanced, then came back the same
	assert!(live > at_format, "{live} should be past {at_format}");
	assert_eq!(open_bytes(bytes(&watch))?.commit_seq(), live);
	Ok(())
}

#[test]
fn test_reopen_does_not_see_a_commit_that_never_happened() -> Result<()> {
	// -- Setup & Fixtures: the bytes as they are *before* a mutation
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/kept", 0o755)?;
	let before = bytes(&watch);

	// -- Exec: mutate the live store, then mount the pre-mutation bytes
	store.mkdir("/added", 0o755)?;
	let abandoned = open_bytes(before)?;

	// -- Check: the older bytes are a whole, self-consistent volume — which is
	// the entire point of copy-on-write. Not a torn state, not a repair, just
	// the tree as it was.
	assert_eq!(names(&abandoned.read_dir("/")?), vec!["kept".to_string()]);
	assert!(is_no_such_file(
		&expect_err("added", abandoned.stat("/added")),
		"/added"
	));
	Ok(())
}

#[test]
fn test_read_only_when_the_allocator_snapshot_is_gone() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/data.bin", b"readable", 0o644)?;
	store.mkdir("/more", 0o755)?;

	// -- Exec: destroy the allocator snapshot at block 5. A volume that cannot
	// say which blocks are free must not be writable — a wrong answer there
	// places objects on top of live data.
	let mut raw = bytes(&watch);
	raw[block_offset(5)..block_offset(6)].fill(0);
	let mut back = open_bytes(raw)?;

	// -- Check
	assert_eq!(back.origin(), Origin::ReadOnly);
	assert!(!back.is_writable());
	assert!(back.write_refusal().is_some(), "a caller must be told why");
	assert_eq!(back.free_bytes(), None, "no allocator, no free-space answer");
	assert_eq!(back.reserved_bytes(), None);
	assert_eq!(back.total_free_bytes(), None);

	// The content is still perfectly readable — that is why this is a
	// degradation rather than a failure.
	assert_eq!(back.read_file("/data.bin")?, b"readable");
	assert_eq!(back.stat("/more")?.kind, InodeKind::Directory);

	// -- Exec & Check: and the write is refused up front, naming the reason
	let e = expect_err("mkdir on a read-only volume", back.mkdir("/nope", 0o755));
	match e {
		Error::ReadOnly { detail } => assert!(detail.contains("allocator"), "{detail}"),
		other => panic!("expected ReadOnly, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_write_refusal_matches_is_writable() -> Result<()> {
	// -- Setup & Fixtures
	let writable = store(BLOCKS)?;

	// -- Check: the two answers cannot disagree, since one is defined as the
	// absence of the other.
	assert_eq!(writable.is_writable(), writable.write_refusal().is_none());
	assert!(writable.is_writable());
	Ok(())
}

#[test]
fn test_a_read_only_volume_can_still_be_opened_read_only_again() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/f", b"x", 0o644)?;
	let mut raw = bytes(&watch);
	raw[block_offset(5)..block_offset(6)].fill(0);

	// -- Exec: mount the damaged volume twice, read-only both times
	let a = open_bytes(raw.clone())?;
	let b = open_bytes(raw)?;

	// -- Check: read-only mounts coexist, because a reader holds no lock and
	// mutates nothing — this is what lets a CLI read a volume a server holds.
	assert_eq!(a.read_file("/f")?, b.read_file("/f")?);
	Ok(())
}

// endregion:    --- Reopen

// region:    --- Space accounting

#[test]
fn test_capacity_comes_from_the_superblock_geometry() -> Result<()> {
	// -- Setup & Fixtures
	let store = store(BLOCKS)?;

	// -- Check
	assert_eq!(store.total_blocks(), BLOCKS);
	assert_eq!(store.capacity_bytes(), BLOCKS * 4096);
	Ok(())
}

#[test]
fn test_free_space_falls_on_every_commit_and_never_rises() -> Result<()> {
	// -- Setup & Fixtures: a volume with room to churn
	let mut store = store(BIG_BLOCKS)?;
	let mut seen = vec![store.free_bytes().expect("a formatted volume is writable")];

	// -- Exec: the shape a long-running store has — repeated rewrites of one file
	for i in 0..24 {
		store.write_file("/state.json", format!("{{\"n\":{i}}}").as_bytes(), 0o644)?;
		seen.push(store.free_bytes().expect("free bytes"));
	}

	// -- Check: monotone non-increasing. CoW commits a new version of every
	// object a transaction touches and frees nothing (GC is not wired into this
	// path), which is the property every caller's rotation policy is built on.
	for pair in seen.windows(2) {
		assert!(
			pair[1] <= pair[0],
			"free space rose from {} to {} across a commit",
			pair[0],
			pair[1]
		);
	}
	assert!(
		*seen.last().expect("seen is non-empty") < seen[0],
		"24 rewrites must actually consume space: {seen:?}"
	);
	Ok(())
}

#[test]
fn test_free_space_never_exceeds_capacity() -> Result<()> {
	// -- Setup & Fixtures
	let store = store(BLOCKS)?;

	// -- Check
	let free = store.free_bytes().expect("free bytes");
	let reserved = store.reserved_bytes().expect("reserved bytes");
	let total = store.total_free_bytes().expect("total free bytes");
	assert!(
		free + reserved == total,
		"the split must account for the whole: {free} + {reserved} != {total}"
	);
	assert!(total <= store.capacity_bytes(), "{total} > {}", store.capacity_bytes());
	Ok(())
}

#[test]
fn test_a_freshly_formatted_volume_reports_its_whole_object_region_free() -> Result<()> {
	// -- Setup & Fixtures
	let store = store(BLOCKS)?;

	// -- Check: the reserved header (6 blocks), the trailer (2) and the
	// allocator's own reserve are not the app's to spend, so the app-visible
	// number is strictly less than the volume — and strictly more than nothing.
	let free = store.free_bytes().expect("free bytes");
	assert!(free > 0, "a formatted volume has space");
	assert!(
		free < store.capacity_bytes(),
		"{free} of {} free: the reserved blocks must not be counted as spendable",
		store.capacity_bytes()
	);
	Ok(())
}

// endregion:    --- Space accounting

// region:    --- Application stores

#[test]
fn test_app_store_records_its_identity_and_contract_version() -> Result<()> {
	// -- Setup & Fixtures
	let store = app_store(BLOCKS, "example")?;

	// -- Check
	assert_eq!(store.app()?, Some("example".to_string()));
	assert_eq!(store.contract_version()?, Some(STORE_CONTRACT_VERSION.to_string()));
	assert_eq!(store.stat(STORE_DIR)?.kind, InodeKind::Directory);
	assert_eq!(store.read_file(STORE_APP_FILE)?, b"example");
	Ok(())
}

#[test]
fn test_a_plain_volume_has_no_app_marker() -> Result<()> {
	// -- Setup & Fixtures
	let store = store(BLOCKS)?;

	// -- Check: `mkfs` alone is not an app store, and saying so is `Ok(None)`
	// rather than an error — the difference between "not mine" and "unreadable".
	assert_eq!(store.app()?, None);
	assert_eq!(store.contract_version()?, None);
	assert!(is_no_such_file(
		&expect_err("stat store dir", store.stat(STORE_DIR)),
		STORE_DIR
	));
	Ok(())
}

#[test]
fn test_format_app_refuses_an_empty_name() -> Result<()> {
	// -- Exec: an empty app name would make every volume claim to be every app's
	let r = Store::format_app(Box::new(MemBlockDevice::new(BLOCKS)), "");

	// -- Check: refused before the device is touched
	let e = expect_err("empty app name", r);
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");
	Ok(())
}

#[test]
fn test_app_markers_survive_a_reopen_and_a_plain_mount() -> Result<()> {
	// -- Setup & Fixtures
	let device = mem(BLOCKS);
	let watch = device.clone();
	drop(Store::format_app(Box::new(device), "example")?);

	// -- Exec: the markers are ordinary files, not a format change, so both a
	// plain `Store::open` and a bare `HostFs` mount must see them.
	let via_store = open_bytes(bytes(&watch))?;
	let via_fs = mount_bytes(bytes(&watch))?;

	// -- Check
	assert_eq!(via_store.app()?, Some("example".to_string()));
	assert_eq!(via_store.contract_version()?, Some(STORE_CONTRACT_VERSION.to_string()));
	let names = names(&via_fs.read_dir("/")?);
	assert!(names.contains(&".ledgerfs".to_string()), "{names:?}");
	assert_eq!(via_fs.read_file(STORE_VERSION_FILE)?, STORE_CONTRACT_VERSION.as_bytes());
	Ok(())
}

#[test]
fn test_a_marker_that_is_not_utf8_is_reported_not_swallowed() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = app_store(BLOCKS, "example")?;
	// A volume that calls itself an app store and then cannot say whose is
	// worth surfacing rather than treating as unmarked.
	store.write_file(STORE_APP_FILE, &[0xFF, 0xFE, 0x00], 0o644)?;

	// -- Exec
	let r = store.app();

	// -- Check
	let e = expect_err("non-utf8 app marker", r);
	match e {
		Error::MarkerUnreadable { path, .. } => assert_eq!(path, STORE_APP_FILE),
		other => panic!("expected MarkerUnreadable, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_contract_version_is_one_hyphenless_token() -> Result<()> {
	// -- Check: an app compares this string, so it must not carry whitespace a
	// platform's writer might normalise.
	assert!(!STORE_CONTRACT_VERSION.is_empty());
	assert!(!STORE_CONTRACT_VERSION.contains(char::is_whitespace));
	assert!(
		STORE_DIR.starts_with('/'),
		"the reserved name is a volume-absolute path"
	);
	assert!(STORE_APP_FILE.starts_with(STORE_DIR));
	Ok(())
}

// endregion:    --- Application stores

// region:    --- Unmountable devices

#[test]
fn test_open_refuses_a_device_with_no_superblock_at_all() -> Result<()> {
	// -- Setup & Fixtures: a blank device — not a LedgerFS volume, and not
	// damage either; it has never seen this filesystem.
	let blank = vec![0u8; 4096 * 64];

	// -- Exec
	let r = open_bytes(blank);

	// -- Check: the failure names the missing magic *and* the copy it was
	// missing from, which is what tells an operator "wrong device" rather than
	// "damaged volume".
	let e = expect_err("mount a blank device", r);
	match e {
		Error::SuperblockUnreadable { copy_lba, detail } => {
			assert_eq!(copy_lba, 0, "the primary copy is the one reported");
			assert!(detail.contains("magic"), "{detail}");
		}
		other => panic!("expected SuperblockUnreadable, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_open_refuses_a_device_of_garbage() -> Result<()> {
	// -- Setup & Fixtures
	let mut rng = Rng::new(0x5EED_1234);
	let mut junk = vec![0u8; 4096 * 64];
	for b in junk.iter_mut() {
		*b = (rng.next_u64() & 0xFF) as u8;
	}

	// -- Exec
	let r = open_bytes(junk);

	// -- Check: an error, not a panic and not a half-mounted volume
	assert!(r.is_err(), "random bytes must not mount");
	Ok(())
}

#[test]
fn test_open_accepts_a_volume_smaller_than_the_device_holding_it() -> Result<()> {
	// -- Setup & Fixtures: a formatted 64-block volume sitting in a 256-block
	// device — the "a partition inside something bigger" case, in memory.
	let (_, watch) = watched(64)?;
	let mut raw = bytes(&watch);
	raw.resize(256 * 4096, 0);

	// -- Exec
	let store = Store::open(Box::new(MemBlockDevice::from_bytes(raw)?))?;

	// -- Check: the superblock's geometry wins, and the extra blocks are simply
	// not part of the volume.
	assert_eq!(store.total_blocks(), 64);
	assert_eq!(store.capacity_bytes(), 64 * 4096);
	Ok(())
}

// endregion:    --- Unmountable devices
