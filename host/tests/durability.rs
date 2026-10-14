//! Durability: what survives a restart, and what a crash in the wrong place
//! costs.
//!
//! LedgerFS is copy-on-write with a four-copy superblock and a snapshot-first
//! commit protocol. Each of those is a promise, and each promise has a test
//! here:
//!
//! - **CoW**: a mutation publishes a new tree and leaves the old one intact,
//!   so a crash rolls back rather than tearing. Simulated by mounting the bytes
//!   as they were *during* a commit.
//! - **Four copies**: any one of them can be destroyed and the volume still
//!   mounts. Simulated by zeroing each in turn.
//! - **Snapshot first**: the allocator snapshot is written before the
//!   superblock that names it, so the dangerous intermediate state — a
//!   superblock naming a tree whose allocations the allocator does not know
//!   about — cannot occur.
//!
//! Plus the accounting that an always-on store depends on: the commit sequence
//! advances by a known amount per verb, free space never rises, and a write
//! refused for want of space leaves the previous tree exactly as it was.

mod common;

use common::*;
use crucible_ledgerfs_host::{Error, InodeKind, Store};
use std::collections::BTreeMap;

// region:    --- Superblock copies

#[test]
fn test_every_commit_persists_all_four_superblock_copies() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec: a handful of mutations, checking after each — a writer that
	// maintained the primary copy only (which `fs-cbl`'s runtime writer does,
	// per this crate's `lib.rs`) would fail this on the first assertion.
	for step in 0..5 {
		store.mkdir(&format!("/d{step}"), 0o755)?;

		// -- Check
		let status = store.fs().volume.copy_status();
		assert_eq!(status.decoded, [true; 4], "step {step}: a copy stopped decoding");
		assert_eq!(status.agree, [true; 4], "step {step}: a copy names a different root");
		assert_eq!(status.decoded_count(), 4);
		assert!(status.is_healthy());
		for (i, seq) in status.commit_seqs.iter().enumerate() {
			assert_eq!(*seq, Some(store.commit_seq()), "copy {i} lags the live sequence");
		}
	}
	Ok(())
}

#[test]
fn test_the_four_superblock_copies_are_byte_identical_after_a_commit() -> Result<()> {
	// -- Setup & Fixtures: redundancy is only redundancy if the copies say the
	// same thing. Four copies that disagree are four different rollback points.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"content", 0o644)?;
	drop(store);

	// -- Exec
	let raw = bytes(&watch);
	let lbas = copy_lbas(BLOCKS);
	let first = &raw[block_offset(lbas[0])..block_offset(lbas[0] + 1)];

	// -- Check
	for lba in &lbas[1..] {
		assert_eq!(
			&raw[block_offset(*lba)..block_offset(*lba + 1)],
			first,
			"superblock copy at block {lba} differs from the primary"
		);
	}
	Ok(())
}

#[test]
fn test_any_single_superblock_copy_can_be_lost() -> Result<()> {
	// -- Setup & Fixtures
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"survives", 0o644)?;
	let expected_seq = store.commit_seq();
	drop(store);
	let good = bytes(&watch);

	// -- Exec & Check: destroy each copy in turn; the other three must carry the
	// mount. This is the whole reason there are four.
	for (i, lba) in copy_lbas(BLOCKS).iter().enumerate() {
		let mut damaged = good.clone();
		damaged[block_offset(*lba)..block_offset(*lba + 1)].fill(0);

		let back = open_bytes(damaged)?;
		assert_eq!(back.commit_seq(), expected_seq, "copy {i} lost: wrong root");
		assert_eq!(back.read_file("/d/f")?, b"survives", "copy {i} lost");
		assert_eq!(back.read_dir("/")?.len(), 1, "copy {i} lost");
	}
	Ok(())
}

#[test]
fn test_garbage_in_one_copy_is_stepped_over_like_a_dead_one() -> Result<()> {
	// -- Setup & Fixtures: a copy that decodes by checksum but is nonsense is a
	// different hazard from a blank one — the decode has to reject it, or the
	// mount would trust it.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.write_new_file("/f", b"payload", 0o644)?;
	drop(store);

	// -- Exec: a copy full of 0xFF
	let mut damaged = bytes(&watch);
	let lba = copy_lbas(BLOCKS)[0];
	damaged[block_offset(lba)..block_offset(lba + 1)].fill(0xFF);

	// -- Check: refused *as a copy*, and the volume still mounts from the rest
	let back = open_bytes(damaged)?;
	assert_eq!(back.read_file("/f")?, b"payload");
	assert_eq!(back.fs().volume.copy_status().decoded_count(), 3);
	Ok(())
}

#[test]
fn test_losing_both_leading_copies_is_a_named_mount_failure() -> Result<()> {
	// -- Setup & Fixtures: the first two copies are read for the geometry, so
	// with both gone there is no way to find the trailing pair either.
	let (_, watch) = watched(BLOCKS)?;
	let mut damaged = bytes(&watch);
	for lba in [0u64, 1] {
		damaged[block_offset(lba)..block_offset(lba + 1)].fill(0);
	}

	// -- Exec
	let r = open_bytes(damaged);

	// -- Check: a named error that quotes both failures, not a panic and not a
	// mount of half a volume.
	match expect_err("lose copies 0 and 1", r) {
		Error::SuperblockUnreadable { copy_lba, detail } => {
			assert_eq!(copy_lba, 0);
			assert!(detail.contains("copy 0"), "{detail}");
			assert!(detail.contains("copy 1"), "{detail}");
		}
		other => panic!("expected SuperblockUnreadable, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_volume_whose_copies_name_different_roots_mounts_the_newest() -> Result<()> {
	// -- Setup & Fixtures: the state a single-copy writer leaves behind — the
	// scenario `lib.rs` documents for the shipped `/data` partition. Built here
	// by committing a mutation and then restoring the *trailing* copies from
	// before it, so copy 0 is ahead of the rest.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.write_new_file("/old", b"before the split", 0o644)?;
	let before = bytes(&watch);

	store.write_new_file("/new", b"after the split", 0o644)?;
	let after = bytes(&watch);
	drop(store);

	let mut mixed = after.clone();
	for lba in &copy_lbas(BLOCKS)[1..] {
		let at = block_offset(*lba);
		mixed[at..at + 4096].copy_from_slice(&before[at..at + 4096]);
	}

	// -- Exec
	let back = open_bytes(mixed)?;

	// -- Check: `mount_select` takes the highest `commit_seq`, so the volume
	// reads at its newest state rather than its oldest — the same behaviour an
	// operator of the shipped image gets. `/old` is present in both trees;
	// `/new` exists only in the newer one, so it is what distinguishes a
	// newest-wins mount from an oldest-wins one.
	assert_eq!(back.read_file("/new")?, b"after the split");
	assert_eq!(back.read_file("/old")?, b"before the split");
	assert_eq!(
		names(&back.read_dir("/")?),
		vec!["new".to_string(), "old".to_string()],
		"the mounted tree is the newer one, whose listing has both files"
	);

	// -- Check: and the disagreement is *reported* rather than averaged over —
	// this is the audit signal an operator needs, and losing it would be worse
	// than the disagreement itself.
	let status = back.fs().volume.copy_status();
	assert!(!status.is_healthy(), "three copies naming an older root is not a healthy volume");
	assert_eq!(status.decoded_count(), 4, "all four are readable");
	assert_eq!(status.selected, 0, "the newest copy is the one read from");
	let agrees = status.agree.iter().filter(|a| **a).count();
	assert_eq!(agrees, 1, "only the selected copy agrees with itself");
	Ok(())
}

// endregion:    --- Superblock copies

// region:    --- Crash points

#[test]
fn test_a_crash_between_the_snapshot_and_the_superblock_keeps_the_old_tree() -> Result<()> {
	// -- Setup & Fixtures: the one crash window the commit order leaves open.
	// `commit` writes the allocator snapshot, *then* the superblock, so the
	// reachable intermediate state is "new allocator, old root". Built by
	// splicing: the bytes after a mutation, with the four superblock copies
	// taken from before it.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/kept", 0o755)?;
	store.write_new_file("/kept/old.txt", b"old contents", 0o644)?;
	let before = bytes(&watch);
	let before_seq = store.commit_seq();

	store.mkdir("/added", 0o755)?;
	store.write_new_file("/added/new.txt", b"new contents", 0o644)?;
	let after = bytes(&watch);
	drop(store);
	assert_ne!(before, after, "the mutation has to have changed the bytes for this test to mean anything");

	let mut mid_crash = after;
	for lba in copy_lbas(BLOCKS) {
		let at = block_offset(lba);
		mid_crash[at..at + 4096].copy_from_slice(&before[at..at + 4096]);
	}

	// -- Exec: a restart onto those bytes
	let back = open_bytes(mid_crash)?;

	// -- Check: the volume rolls *back*, not forward and not into a tear. The
	// old tree is whole, at the old sequence, and the half-committed mutation
	// is simply not there.
	assert_eq!(back.commit_seq(), before_seq);
	assert_eq!(back.read_file("/kept/old.txt")?, b"old contents");
	assert!(is_no_such_file(&expect_err("read the uncommitted dir", back.stat("/added")), "/added"));
	assert_eq!(names(&back.read_dir("/")?), vec!["kept".to_string()]);

	// -- Check: and the allocator that ran ahead is still usable — the store
	// must not have lost writability to the rollback, or a crash would cost the
	// volume its ability to take writes.
	assert_eq!(back.origin(), crucible_ledgerfs_host::Origin::ResumedWritable);
	let mut back = back;
	back.mkdir("/added", 0o755)?;
	assert_eq!(back.stat("/added")?.kind, InodeKind::Directory);
	Ok(())
}

#[test]
fn test_every_prefix_of_a_mutation_session_mounts_to_a_whole_tree() -> Result<()> {
	// -- Setup & Fixtures: the crash-consistency sweep. After each of a series
	// of mutations, the bytes are captured; every one of those snapshots must
	// mount to a complete, readable tree at the sequence it was captured at.
	// This is the same guarantee as above, applied at every step rather than at
	// one chosen window.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	let mut snapshots: Vec<(Vec<u8>, u64, Vec<String>)> = Vec::new();

	// -- Exec
	for step in 0..12u32 {
		let name = format!("/file-{step:02}");
		store.write_new_file(&name, format!("contents of step {step}").as_bytes(), 0o644)?;
		snapshots.push((bytes(&watch), store.commit_seq(), names(&store.read_dir("/")?)));
	}
	drop(store);

	// -- Check
	for (i, (bytes_at, seq, expected_names)) in snapshots.iter().enumerate() {
		let back = open_bytes(bytes_at.clone())?;
		assert_eq!(back.commit_seq(), *seq, "snapshot {i}");
		assert_eq!(names(&back.read_dir("/")?), *expected_names, "snapshot {i}");
		// Every file the snapshot claimed to have is still readable, with the
		// content that step wrote — no snapshot is a half-written tree.
		for step in 0..=i {
			let name = format!("/file-{step:02}");
			assert_eq!(
				back.read_file(&name)?,
				format!("contents of step {step}").as_bytes(),
				"snapshot {i}, file {step}"
			);
		}
	}
	Ok(())
}

// endregion:    --- Crash points

// region:    --- Commit sequence

#[test]
fn test_each_verb_costs_a_known_number_of_commits() -> Result<()> {
	// -- Setup & Fixtures: a caller that reasons about how much a write costs
	// (an append-only log, a rotation policy) needs these numbers to be stable,
	// and `write_new_file` being *two* transactions is the one that surprises.
	let mut store = store(BLOCKS)?;

	// -- Check: `format` is itself one commit — the genesis root.
	assert_eq!(store.commit_seq(), 1, "format commits the genesis root");

	let expect = |store: &Store, n: u64, what: &str| {
		assert_eq!(store.commit_seq(), n, "after {what}");
	};

	// -- Exec
	store.mkdir("/d", 0o755)?;
	expect(&store, 2, "mkdir");

	store.create_file("/empty", 0o644)?;
	expect(&store, 3, "create_file");

	store.write_new_file("/f", b"data", 0o644)?;
	expect(&store, 5, "write_new_file (create + write)");

	store.append("/f", b"more", 0o644)?;
	expect(&store, 6, "append");

	store.write_at("/f", 8, b"tail")?;
	expect(&store, 7, "write_at");

	store.remove("/f")?;
	expect(&store, 8, "remove");

	store.write_new_file("/f", b"again", 0o644)?;
	expect(&store, 10, "write_new_file on a now-free name");

	// -- Exec: replacing an existing file is remove + create + write
	store.write_file("/f", b"replaced", 0o644)?;
	expect(&store, 13, "write_file over an existing file");

	// -- Exec: creating a fresh one is create + write
	store.write_file("/g", b"fresh", 0o644)?;
	expect(&store, 15, "write_file creating");
	Ok(())
}

#[test]
fn test_replacing_a_file_has_a_window_where_it_is_empty() -> Result<()> {
	// -- Setup & Fixtures: the previous test pinned *that* replacing an existing
	// file costs three commits — remove, create, write. This one pins what those
	// three commits mean to a reader that arrives in the middle, and it is not
	// what a caller replacing a file would expect.
	//
	// A replace is not atomic. A crash — or in the embedding case, any other
	// reader of a snapshot — between the remove and the create sees the file
	// gone; between the create and the write sees it present and *empty*. The
	// second is the dangerous one, because "no such file" is a state a caller
	// handles and "a zero-byte file" is a state it is likely to treat as
	// *content*. A store that reads its own state file and finds it empty will
	// decide the state is empty; one that finds it missing will use the default.
	//
	// This test drives the same three states through public verbs, one commit at
	// a time, so the window is demonstrated deterministically rather than raced
	// for. `rename` onto an existing name is a single commit and does not have
	// the window — see `test_a_rename_onto_an_existing_file_...` in
	// `security_paths.rs` — which is the shape a caller who needs an atomic
	// replace should use (write a new file, then rename it over the old one).
	let (mut store, watch) = watched(1024)?;
	store.write_new_file("/state", b"{\"n\": 1}", 0o644)?;
	let committed = bytes(&watch);

	// -- Exec: the first commit of a replace — the file is removed
	store.remove("/state")?;
	let after_remove = bytes(&watch);

	// -- Check: a reader of that state finds the name gone, which is honest
	// and is what a caller can handle
	assert!(
		matches!(open_bytes(after_remove)?.read_file("/state"), Err(Error::NoSuchFile { .. })),
		"the remove must be visible as a missing file"
	);

	// -- Exec: the second commit — the file is back, with no content yet
	store.create_file("/state", 0o644)?;
	let after_create = bytes(&watch);

	// -- Check: and a reader of *that* state finds a file that exists and is
	// empty. This is the assertion the whole test exists for: it is not an
	// error, it is not the old contents, and it is not the new contents. It is a
	// plausible-looking empty file, and anything that reads it as data reads
	// nothing.
	let mid = open_bytes(after_create)?;
	assert_eq!(mid.read_file("/state")?, b"", "a crash between create and write leaves an empty file");
	assert_eq!(mid.stat("/state")?.size, 0, "and the inode agrees it is empty");

	// -- Exec: the third commit completes the replace
	store.append("/state", b"", 0o644).ok();
	store.write_at("/state", 0, b"{\"n\": 2}")?;
	let after_write = bytes(&watch);

	// -- Check: and now it is whole. The three states are distinct, which is
	// what makes the window real rather than theoretical — a reader cannot tell
	// the empty state from a file that was written empty on purpose.
	let done = open_bytes(after_write)?;
	assert_eq!(done.read_file("/state")?, b"{\"n\": 2}");

	// -- Check: and the volume itself is fine throughout. The window is a
	// durability property, not corruption: every one of the three states mounts
	// and mounts to a whole tree.
	let _ = open_bytes(committed)?;
	Ok(())
}

#[test]
fn test_a_reader_never_sees_the_old_and_the_new_contents_at_once() -> Result<()> {
	// -- Setup & Fixtures: the other half of the window above. Between the three
	// commits of a replace there is no state in which `read_file` returns a
	// mixture — the old bytes with the new length, or the new bytes with a stale
	// tail. That would be worse than either interrupted state, because it looks
	// like a successful read of a plausible file.
	//
	// Driven by hand so each intermediate state can be mounted and read.
	let (mut store, watch) = watched(BLOCKS)?;
	let old = vec![b'A'; 9000];
	let new = vec![b'B'; 3000];
	store.write_new_file("/f", &old, 0o644)?;

	// -- Exec: the three commits, snapshotted after each
	let mut states = vec![bytes(&watch)];
	store.remove("/f")?;
	states.push(bytes(&watch));
	store.create_file("/f", 0o644)?;
	states.push(bytes(&watch));
	store.write_file("/f", &new, 0o644)?;
	states.push(bytes(&watch));

	// -- Check: every state holds one of exactly three things — the whole old
	// version, the whole new version, or nothing. Never a slice of one with the
	// length of the other, which is what a reader who caught a half-applied
	// replace would see.
	let expect = [
		(&old[..], "the old version"),
		(&[][..], "nothing"),
		(&[][..], "nothing"),
		(&new[..], "the new version"),
	];
	for (i, (state, want)) in states.into_iter().zip(expect).enumerate() {
		let label = want.1;
		let s = open_bytes(state)?;
		match s.read_file("/f") {
			Ok(data) => {
				let uniform = data.is_empty() || data.iter().all(|b| *b == data[0]);
				assert!(uniform, "state {i} is a mixture of the two contents");
				assert!(
					data.iter().all(|b| *b == b'A') || data.iter().all(|b| *b == b'B') || data.is_empty(),
					"state {i} holds a mixture: len {}, first byte {:?}",
					data.len(),
					data.first()
				);
				assert_eq!(data.len(), want.0.len(), "state {i} should be {label}");
			}
			Err(Error::NoSuchFile { .. }) => {
				assert!(want.0.is_empty(), "state {i} should be {label}, not absent")
			}
			Err(e) => panic!("state {i}: {e:?}"),
		}
	}
	Ok(())
}

#[test]
fn test_an_empty_append_is_not_a_commit() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/log", b"line\n", 0o644)?;
	let seq = store.commit_seq();
	let free = store.free_bytes();

	// -- Exec: repeated no-op appends, as a poll loop that found nothing new
	for _ in 0..20 {
		store.append("/log", b"", 0o644)?;
	}

	// -- Check: no sequence movement and no space consumed
	assert_eq!(store.commit_seq(), seq);
	assert_eq!(store.free_bytes(), free);
	Ok(())
}

#[test]
fn test_commit_sequence_never_goes_backwards_across_a_remount() -> Result<()> {
	// -- Setup & Fixtures: a monotonically increasing sequence is what lets a
	// caller detect a rollback, so a mount must never present an older one.
	//
	// Each remount takes over the `watch` handle as well: a reopened store
	// writes into its own copy of the bytes, so the previous handle would go
	// stale and this test would be checking a volume nobody is writing to.
	let device = mem(BIG_BLOCKS);
	let mut watch = device.clone();
	let mut store = Store::format(Box::new(device), BIG_BLOCKS)?;
	let mut last = store.commit_seq();

	// -- Exec & Check: alternating mutations and fresh mounts of the current
	// bytes, twenty times.
	for i in 0..20u32 {
		store.write_file("/state", format!("v{i}").as_bytes(), 0o644)?;
		assert!(store.commit_seq() > last, "iteration {i}: the sequence did not advance");
		last = store.commit_seq();
		drop(store);

		let (reopened, next_watch) = open_bytes_watched(bytes(&watch))?;
		store = reopened;
		watch = next_watch;

		assert_eq!(store.commit_seq(), last, "iteration {i}: the remount lost a commit");
		assert_eq!(store.read_file("/state")?, format!("v{i}").as_bytes());
		// The write half must be usable after a remount, or a restart would
		// cost the volume its ability to be written to at all.
		assert!(store.is_writable());
	}
	Ok(())
}

// endregion:    --- Commit sequence

// region:    --- The root locator

#[test]
fn test_the_active_root_hint_is_published_and_survives_a_remount() -> Result<()> {
	// -- Setup & Fixtures: `active_root_hint` exists so that fetching the root
	// inode is one block read instead of a scan. It is an accelerator, so a
	// wrong one is not corruption — but a *missing* one is the difference
	// between a mount that is instant and one that is O(volume).
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;

	// -- Check: `format` publishes a hint, and it points past the reserved
	// header (0 would mean "scan", which is the state every image built by
	// `forge-builder` is in).
	let at_format = store.fs().volume.active_root_hint();
	assert!(at_format >= 6, "a hint of {at_format} means 'no hint' or a reserved block");

	// -- Exec
	store.mkdir("/d", 0o755)?;
	let after_mutation = store.fs().volume.active_root_hint();
	drop(store);

	// -- Check: a mutation moves it (the root is a new object, somewhere else),
	// and the hint is on the device rather than only in memory.
	assert_ne!(after_mutation, at_format, "the new root is at a new block");
	let back = open_bytes(bytes(&watch))?;
	assert_eq!(back.fs().volume.active_root_hint(), after_mutation);
	assert_eq!(back.fs().volume.root_ref().hint.0, after_mutation, "the reference agrees with the superblock");
	assert_eq!(back.fs().volume.root_ref(), back.persisted_root_ref());
	Ok(())
}

#[test]
fn test_the_published_root_reference_is_the_one_the_volume_mounted_at() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;

	// -- Exec & Check
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"x", 0o644)?;

	let published = store.persisted_root_ref();
	assert_eq!(store.fs().root_inode_ref, published, "the read half follows the write half");
	assert_eq!(
		open_bytes(bytes(&watch))?.persisted_root_ref(),
		published,
		"the remount lands on the same root"
	);

	// The root is a directory, as `require_root` insists — a superblock pointed
	// at anything else is a volume whose paths can never resolve.
	assert_eq!(store.fs().store.inode(published)?.kind, InodeKind::Directory);
	Ok(())
}

// endregion:    --- The root locator

// region:    --- Space exhaustion

#[test]
fn test_a_commit_that_does_not_fit_leaves_the_previous_tree_intact() -> Result<()> {
	// -- Setup & Fixtures: a small volume with a real tree in it, then an
	// append far too big for what is left. `append` on an existing file is one
	// `write_chunks` loop, so a refusal is a refusal of that transaction — not
	// the create-then-write artefact where the *create* half commits and the
	// content half fails (`test_a_failed_write_new_file_leaves_an_empty_file` in
	// `verbs.rs` covers that one).
	let mut store = store(MIN_BLOCKS + 40)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/keep.txt", b"must survive", 0o644)?;
	let seq = store.commit_seq();
	let free = store.free_bytes().expect("free bytes");

	// -- Exec
	let e = expect_err("an oversized append", store.append("/d/keep.txt", &vec![0u8; 10_000_000], 0o644));

	// -- Check: refused on space
	match e {
		Error::Format(e) => assert!(e.to_string().contains("space"), "{e}"),
		other => panic!("expected an out-of-space refusal, got {other:?}"),
	}

	// -- Check: the tree, the sequence and the free-space reading are all as
	// they were. A failed commit must not have half-applied its allocations —
	// that would leak blocks on every retry, and a caller that retried in a
	// loop would walk the volume capacity down to nothing.
	assert_eq!(store.read_file("/d/keep.txt")?, b"must survive", "a refused commit changed the file");
	assert_eq!(store.commit_seq(), seq, "a refused commit must not advance the sequence");
	assert_eq!(store.free_bytes().expect("free bytes"), free, "a refused commit must not leak space");

	// -- Exec & Check: and the volume is still perfectly usable
	store.write_new_file("/d/small", b"fits", 0o644)?;
	assert_eq!(store.read_file("/d/small")?, b"fits");
	assert_eq!(store.read_file("/d/keep.txt")?, b"must survive");
	Ok(())
}

#[test]
fn test_a_refused_commit_does_not_brick_the_handle() -> Result<()> {
	// -- Setup & Fixtures: the regression this guards is not "the write failed",
	// it is "the write failed and the store never worked again". A failed commit
	// used to leave the handle owning no allocator at all, so every later write —
	// however small, however much room the volume had — was refused with "there
	// is no allocator snapshot on the volume". For a long-lived embedder (the
	// `bugs` relay is one) a single out-of-space write was therefore fatal for
	// the life of the process, and the message pointed at the volume rather than
	// at the bug.
	//
	// Everything here is phrased in terms of "after the refusal", because that is
	// the state the bug showed up in.
	//   The volume is small enough that a 10 MB append cannot fit, but roomy
	// enough that the writes *after* the refusals can — a full volume would fail
	// them for the wrong reason and hide what this test is about.
	let (mut store, watch) = watched(MIN_BLOCKS + 60)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/keep.txt", b"must survive", 0o644)?;

	// -- Exec: refuse, then refuse again. Once is what it takes to break a
	// take-once-and-forget handle; twice proves the handle recovers to a state
	// that can be refused *again*, rather than recovering exactly once.
	for attempt in 0..3 {
		let e = expect_err("an oversized append", store.append("/d/keep.txt", &vec![0u8; 10_000_000], 0o644));
		match e {
			Error::Format(e) => assert!(e.to_string().contains("space"), "attempt {attempt}: {e}"),
			other => panic!("attempt {attempt}: expected an out-of-space refusal, got {other:?}"),
		}
	}

	// -- Check: the handle has not talked itself out of writing. `is_writable` is
	// the same answer `write_refusal` would give a caller that asked before
	// trying, so this is the sentence `bugs` would log.
	assert!(
		store.is_writable(),
		"a refused commit left the store refusing everything: {:?}",
		store.write_refusal()
	);
	assert_eq!(
		store.origin(),
		crucible_ledgerfs_host::Origin::Formatted,
		"a refusal must not change how the volume was obtained"
	);

	// -- Check: and it can still do real work — a write small enough to fit goes
	// through, and the tree it lands in is the one from before the refusals.
	store.write_new_file("/d/after.txt", b"still here", 0o644)?;
	assert_eq!(store.read_file("/d/after.txt")?, b"still here");
	assert_eq!(store.read_file("/d/keep.txt")?, b"must survive", "the refusals damaged the tree");

	// -- Check: and the damage does not outlive the handle. A remount must see a
	// volume that is intact *and* writable — the in-memory repair is only worth
	// anything if the state it repaired to matches what is on the volume. If the
	// failed transactions had persisted any of their work, this remount is where
	// it would show up as a lost or duplicated file.
	let (mut reopened, _watch) = open_bytes_watched(bytes(&watch))?;
	assert_eq!(reopened.read_file("/d/keep.txt")?, b"must survive");
	assert_eq!(reopened.read_file("/d/after.txt")?, b"still here");
	assert!(reopened.is_writable(), "the volume itself was left read-only");
	reopened.write_new_file("/d/reopened.txt", b"yes", 0o644)?;
	assert_eq!(reopened.read_file("/d/reopened.txt")?, b"yes");
	Ok(())
}

#[test]
fn test_filling_a_volume_exhausts_it_predictably_and_still_reads() -> Result<()> {
	// -- Setup & Fixtures: append until the volume refuses. An always-on store
	// reaches this state in production; the question is whether it degrades
	// into an error or into damage.
	let mut store = store(MIN_BLOCKS + 60)?;
	store.write_new_file("/log", b"start\n", 0o644)?;
	let mut expected = b"start\n".to_vec();
	let mut last_ok = 0u32;
	let mut wall: Option<Error> = None;
	let mut free_at_wall = 0u64;
	let mut free_before_wall = 0u64;

	// -- Exec: append until the volume will not take another line. The loop
	// keeps what it appended in step with what the store accepted, because a
	// refusal in the middle must not be recorded as a write that happened.
	//
	// Free space is read before *every* attempt, not only the successful ones, so
	// the numbers either side of the refusal can be compared at the end.
	for i in 0..500u32 {
		let line = format!("line {i:04}\n");
		let free_before = store.free_bytes().expect("a committed volume reports free space");
		match store.append("/log", line.as_bytes(), 0o644) {
			Ok(()) => {
				expected.extend_from_slice(line.as_bytes());
				last_ok = i + 1;
				free_before_wall = free_before;
			}
			Err(e) => {
				free_at_wall = free_before;
				wall = Some(e);
				break;
			}
		}
	}

	// -- Check: the volume hit a wall, and it was a *space* wall — the signal a
	// caller rotates on, not a silent truncation and not an internal error.
	let e = wall.expect("a small volume must run out of room");
	match e {
		Error::Format(ref e) => assert!(e.to_string().contains("space"), "{e}"),
		ref other => panic!("expected an out-of-space refusal, got {other:?}"),
	}
	assert!(last_ok > 0, "but not before writing anything");

	// -- Check: everything written before the wall is still there, in order,
	// and the file is readable in full.
	let got = store.read_file("/log")?;
	assert_eq!(got, expected, "the log is exactly the lines that were accepted");
	assert_eq!(store.stat("/log")?.size, expected.len() as u64);

	// -- Check: and the volume is still readable — exhausted is not the same as
	// broken. A store that could no longer answer a read after filling up would
	// have thrown away everything a log is for.
	assert_eq!(store.read_at("/log", 0, 6)?, b"start\n");

	// -- Check: the failed transaction left no trace. It had placed some of its
	// objects before running out of room, so a store that "remembered" those
	// placements would report less free space than the volume actually has —
	// and the shortfall grows with every failed attempt until the store believes
	// a volume with room to spare is full. The reading either side of the refusal
	// must therefore be the same number, not merely both small.
	let free_after = store.free_bytes().expect("an exhausted but writable volume still reports free space");
	assert_eq!(free_after, free_at_wall, "a refused append must not leak space");
	assert!(free_before_wall > free_at_wall, "the push toward the wall must actually consume space");

	// -- Check: and an exhausted volume is a *full* volume, not a dead handle.
	// Appending again fails, and it fails for the same reason as the first time:
	// out of space, not "this volume has no allocator". The distinction is the
	// whole point of the recovery path — see `Store::restore_allocator_from_snapshot`.
	let again = expect_err("appending to a full volume", store.append("/log", b"more\n", 0o644));
	match again {
		Error::Format(ref e) => assert!(e.to_string().contains("space"), "{e}"),
		ref other => panic!("expected out of space again, got {other:?}"),
	}
	assert!(store.is_writable(), "a full volume is still a writable volume");
	Ok(())
}

#[test]
fn test_free_space_never_rises_across_a_long_session() -> Result<()> {
	// -- Setup & Fixtures: CoW commits with no GC, so free space is a strictly
	// non-increasing number. A caller's rotation policy is built on that, and a
	// regression that made it fluctuate would silently break it.
	let mut store = store(BIG_BLOCKS)?;
	let mut last = store.free_bytes().expect("free bytes");

	// -- Exec
	for i in 0..60u32 {
		store.write_file("/state", format!("{{\"n\": {i}, \"pad\": \"{}\"}}", "x".repeat(100)).as_bytes(), 0o644)?;
		store.append("/log", format!("event {i}\n").as_bytes(), 0o644)?;

		let now = store.free_bytes().expect("free bytes");
		assert!(now <= last, "iteration {i}: free space rose from {last} to {now}");
		last = now;
	}

	// -- Check: and it actually moved, so the loop was doing work
	assert!(last < store.capacity_bytes());
	Ok(())
}

// endregion:    --- Space exhaustion

// region:    --- Model-based session

/// What a directory tree should look like, tracked in memory.
///
/// The point of a model is that it is written from the *test's* intent rather
/// than from the crate's behaviour: if the store and the model disagree, the
/// store is wrong.
#[derive(Default)]
struct Model {
	/// path → content, for regular files.
	files: BTreeMap<String, Vec<u8>>,
}

impl Model {
	fn apply_write(&mut self, path: &str, data: &[u8]) {
		self.files.insert(path.to_string(), data.to_vec());
	}

	fn apply_append(&mut self, path: &str, data: &[u8]) {
		self.files.entry(path.to_string()).or_default().extend_from_slice(data);
	}

	fn apply_remove(&mut self, path: &str) {
		self.files.remove(path);
	}

	/// The names a listing of `dir` should contain — immediate children only,
	/// so a file under `/d` is not also offered as an entry of `/`.
	fn names_in(&self, dir: &str) -> Vec<String> {
		let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
		let mut out: Vec<String> = self
			.files
			.keys()
			.filter_map(|p| p.strip_prefix(&prefix))
			.filter(|rest| !rest.contains('/'))
			.map(str::to_string)
			.collect();
		out.sort();
		out
	}
}

fn check_against_model(store: &crucible_ledgerfs_host::Store, model: &Model, what: &str) -> Result<()> {
	// -- Root listing: the one directory plus every top-level file
	let mut want_root = model.names_in("/");
	want_root.push("d".to_string());
	want_root.sort();
	assert_eq!(names(&store.read_dir("/")?), want_root, "{what}: root listing");

	// -- The subdirectory
	assert_eq!(names(&store.read_dir("/d")?), model.names_in("/d"), "{what}: /d listing");

	// -- Every file, in full
	for (path, content) in &model.files {
		assert_eq!(store.stat(path)?.size, content.len() as u64, "{what}: size of {path}");
		assert_eq!(&store.read_file(path)?, content, "{what}: content of {path}");
	}

	// -- Nothing else is there: a file the model does not know about is a leak
	let mut stored: Vec<String> = names(&store.read_dir("/")?)
		.into_iter()
		.filter(|n| n != "d")
		.map(|n| format!("/{n}"))
		.collect();
	stored.extend(names(&store.read_dir("/d")?).into_iter().map(|n| format!("/d/{n}")));
	stored.sort();
	let mut modelled: Vec<String> = model.files.keys().cloned().collect();
	modelled.sort();
	assert_eq!(stored, modelled, "{what}: the volume holds exactly the modelled files");
	Ok(())
}

#[test]
fn test_a_long_random_session_matches_an_in_memory_model() -> Result<()> {
	// -- Setup & Fixtures: the strongest regression test in this file. A few
	// hundred pseudo-random verb calls are applied to both the store and a
	// plain `BTreeMap`, and the two must agree exactly — then again after a
	// remount of the bytes, which is where a hint, an extent tree or a
	// directory update that was right in memory but wrong on disk would show.
	let device = mem(BIG_BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BIG_BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	let mut model = Model::default();
	let mut rng = Rng::new(0xC0FF_EE01);

	// The names the session works with, grouped by directory: `rename` is
	// same-directory only, so a rename's destination is drawn from the source's
	// own group. A directory name is never in either pool, which is what makes
	// every operation here legal by construction — the test is about the read
	// path agreeing with the model, not about the refusals, which have their own
	// tests.
	let root_names = ["/a", "/b", "/c"];
	let sub_names = ["/d/x", "/d/y", "/d/z"];
	let all: Vec<&str> = root_names.iter().chain(sub_names.iter()).copied().collect();
	let siblings = |path: &str| -> &[&str] {
		if path.starts_with("/d/") {
			&sub_names
		} else {
			&root_names
		}
	};

	// -- Exec: enough operations to churn a volume without filling it — every
	// commit rewrites the whole directory node, so the cost is super-linear in
	// the number of steps.
	for step in 0..150 {
		let path = all[rng.below(all.len())];
		match rng.below(5) {
			0 => {
				// write (replace or create)
				let len = rng.below(600);
				let data: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
				store.write_file(path, &data, 0o644)?;
				model.apply_write(path, &data);
			}
			1 => {
				// append (creates when absent)
				let len = rng.below(200);
				let data: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
				store.append(path, &data, 0o644)?;
				model.apply_append(path, &data);
			}
			2 => {
				// remove, if the model says there is something to remove
				if model.files.contains_key(path) {
					store.remove(path)?;
					model.apply_remove(path);
				}
			}
			3 => {
				// rename within the same directory, overwriting a file target
				let group = siblings(path);
				let to = group[rng.below(group.len())];
				if model.files.contains_key(path) && path != to {
					store.rename(path, to)?;
					let content = model.files.remove(path).expect("checked above");
					model.files.insert(to.to_string(), content);
				}
			}
			_ => {
				// extend at the end, which is the only `write_at` that is allowed
				if let Some(existing) = model.files.get(path).cloned() {
					let data = format!("+{step}").into_bytes();
					let offset = existing.len() as u64;
					let mut joined = existing;
					joined.extend_from_slice(&data);
					store.write_at(path, offset, &data)?;
					model.apply_write(path, &joined);
				}
			}
		}

		// -- Check after every single step: a divergence is reported at the
		// operation that caused it, not at the end where it is unattributable.
		check_against_model(&store, &model, &format!("step {step}"))?;
	}

	// -- Exec: the same tree, read from a fresh mount of the bytes
	drop(store);
	let back = open_bytes(bytes(&watch))?;

	// -- Check
	check_against_model(&back, &model, "after a remount")?;

	// -- Exec & Check: and a second remount of the same bytes agrees with the
	// first, so nothing about a mount is order- or identity-dependent.
	let again = open_bytes(bytes(&watch))?;
	check_against_model(&again, &model, "after a second remount")?;
	Ok(())
}

#[test]
fn test_the_same_bytes_mount_to_the_same_tree_every_time() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"stable", 0o644)?;
	store.write_new_file("/g", &vec![5u8; 3000], 0o644)?;
	let raw = bytes(&watch);
	drop(store);

	// -- Exec & Check: three mounts of one byte string
	let a = open_bytes(raw.clone())?;
	let b = open_bytes(raw.clone())?;
	let c = open_bytes(raw)?;
	for (name, mount) in [("a", &a), ("b", &b), ("c", &c)] {
		assert_eq!(names(&mount.read_dir("/")?), vec!["d".to_string(), "g".to_string()], "mount {name}");
		assert_eq!(mount.read_file("/d/f")?, b"stable", "mount {name}");
		assert_eq!(mount.read_file("/g")?, vec![5u8; 3000], "mount {name}");
		assert_eq!(mount.commit_seq(), a.commit_seq(), "mount {name}");
	}
	Ok(())
}

// endregion:    --- Model-based session
