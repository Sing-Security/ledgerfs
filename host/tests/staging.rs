//! Staging sessions: many staged writes, one seal, and the two read tiers.
//!
//! The property under test is a trade the application makes explicitly: while a
//! session is open its writes are RAM only, so they are fast and they are not
//! durable; the seal turns all of them into **one** ordinary commit, after
//! which they have exactly the guarantee every write in this store has. These
//! tests pin both halves: that nothing reaches the device before the seal, and
//! that after it a plain mount sees a complete, byte-exact tree.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{
	BIG_BLOCKS, BLOCKS, CountingDevice, Result, bytes, expect_err, is_no_such_file, names, open_bytes, store, watched,
};
use ledgerfs_host::{BlockDevice, Error, HintCache, MemBlockDevice, Result as FsResult, Store};

// region:    --- One seal, one commit

#[test]
fn test_staged_appends_seal_as_one_commit() -> Result<()> {
	// -- Setup & Fixtures: two twin volumes, the same five appends
	let (mut staged, staged_watch) = watched(BLOCKS)?;
	let mut committed = store(BLOCKS)?;
	staged.mkdir("/log", 0o755)?;
	committed.mkdir("/log", 0o755)?;

	let start = staged.commit_seq();
	let free_start = staged.free_bytes().expect("writable");

	// -- Exec: staged; five appends, then one seal
	staged.begin_staging(0)?;
	for i in 0..5 {
		staged.staged_append("/log/stream", format!("record {i}\n").as_bytes(), 0o644)?;
	}

	// -- Check: nothing committed, and the staged tier already reads back
	assert_eq!(staged.commit_seq(), start, "staging must not commit");
	assert_eq!(
		staged.read_file("/log/stream")?,
		b"record 0\nrecord 1\nrecord 2\nrecord 3\nrecord 4\n"
	);

	let sealed = staged.seal_staged()?;
	assert_eq!(staged.commit_seq(), start + 1, "a seal is one commit");
	assert_eq!(sealed.commit_seq, start + 1);
	assert!(!staged.staging_is_open(), "a successful seal closes the session");

	// -- Exec: committed; the same five appends, one commit each
	for i in 0..5 {
		committed.append("/log/stream", format!("record {i}\n").as_bytes(), 0o644)?;
	}

	// -- Check: same bytes, and the staged volume paid less volume for them
	assert_eq!(
		committed.read_file("/log/stream")?,
		staged.read_file("/log/stream")?,
		"the two volumes must hold the same file"
	);
	assert_eq!(committed.commit_seq(), start + 5, "five appends, five commits");

	let staged_used = free_start - staged.free_bytes().expect("writable");
	let committed_used = free_start - committed.free_bytes().expect("writable");
	assert!(
		staged_used < committed_used,
		"one seal must cost less volume than five commits: staged used {staged_used} B, \
		 committed used {committed_used} B"
	);

	// -- Check: and a plain reopen sees the sealed tree, verified as always
	let reopened = open_bytes(bytes(&staged_watch))?;
	assert_eq!(
		reopened.read_file("/log/stream")?,
		b"record 0\nrecord 1\nrecord 2\nrecord 3\nrecord 4\n"
	);
	assert_eq!(reopened.commit_seq(), start + 1);
	Ok(())
}

// region:    --- The deferred seal

/// A deferred seal is a commit the *volume* has not heard of yet.
///
/// Everything it wrote is on the device: the commit engine's barrier covered
/// the objects and the allocator snapshot, but the superblock naming the new
/// root is held back. So the handle reads its own seal immediately, while a
/// mount of the same bytes sees the root from before it.
#[test]
fn test_deferred_seal_is_invisible_until_sync() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/log", 0o755)?;
	let start = store.commit_seq();
	store.begin_staging(0)?;
	store.staged_append("/log/stream", b"first\n", 0o644)?;
	store.staged_append("/log/stream", b"second\n", 0o644)?;

	// -- Exec
	let sealed = store.seal_staged_deferred()?;

	// -- Check: committed to this handle, not to the volume
	assert_eq!(sealed.commit_seq, start + 1);
	assert_eq!(store.commit_seq(), start + 1);
	assert!(store.has_pending_sync(), "a deferred seal leaves work for sync");
	assert_eq!(store.read_file("/log/stream")?, b"first\nsecond\n");
	let mounted = open_bytes(bytes(&watch))?;
	assert_eq!(mounted.commit_seq(), start, "the volume still names the old root");
	assert!(
		is_no_such_file(
			&expect_err(
				"a deferred seal must not be on the volume",
				mounted.read_file("/log/stream")
			),
			"/log/stream"
		),
		"a deferred seal must not appear in a mount of the volume's bytes"
	);

	// -- Exec: the durability point
	store.sync()?;

	// -- Check: now the volume names it
	assert!(!store.has_pending_sync());
	let mounted = open_bytes(bytes(&watch))?;
	assert_eq!(mounted.commit_seq(), start + 1);
	assert_eq!(mounted.read_file("/log/stream")?, b"first\nsecond\n");
	Ok(())
}

/// An ordinary commit after a deferred seal makes both durable: its own
/// superblock write names a newer root, and its predecessor's objects were
/// flushed by the engine barrier underneath it. Nothing is left pending.
#[test]
fn test_deferred_seal_is_superseded_by_an_ordinary_commit() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/log", 0o755)?;
	let start = store.commit_seq();
	store.begin_staging(0)?;
	store.staged_append("/log/stream", b"staged\n", 0o644)?;
	store.seal_staged_deferred()?;
	assert!(store.has_pending_sync());

	// -- Exec
	store.append("/log/stream", b"ordinary\n", 0o644)?;

	// -- Check
	assert!(
		!store.has_pending_sync(),
		"a durable commit persists everything above it"
	);
	let mounted = open_bytes(bytes(&watch))?;
	assert_eq!(mounted.commit_seq(), start + 2);
	assert_eq!(mounted.read_file("/log/stream")?, b"staged\nordinary\n");
	Ok(())
}

/// Dropping a handle with a deferred seal pending loses exactly that seal's
/// work and leaves a volume that mounts: the old-or-new rule, avoided rather
/// than stressed.
#[test]
fn test_deferred_seal_without_sync_is_lost_cleanly() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/log", 0o755)?;
	let start = store.commit_seq();
	store.begin_staging(0)?;
	store.staged_append("/log/stream", b"volatile\n", 0o644)?;
	store.seal_staged_deferred()?;

	// -- Exec: no sync; the handle goes away
	drop(store);

	// -- Check: the volume is the pre-seal tree, and it is intact
	let mounted = open_bytes(bytes(&watch))?;
	assert_eq!(mounted.commit_seq(), start);
	assert!(mounted.read_dir("/log")?.is_empty());
	assert!(
		is_no_such_file(
			&expect_err(
				"a deferred seal must not be on the volume",
				mounted.read_file("/log/stream")
			),
			"/log/stream"
		),
		"a deferred seal must not appear in a mount of the volume's bytes"
	);
	Ok(())
}

/// `sync` with nothing pending writes nothing at all: the call is safe to
/// make at every durability point without paying for it.
#[test]
fn test_sync_without_a_deferred_seal_is_a_no_op() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/log", 0o755)?;
	store.write_new_file("/log/stream", b"durable\n", 0o644)?;
	let before = bytes(&watch);
	assert!(!store.has_pending_sync());

	// -- Exec
	store.sync()?;

	// -- Check
	assert_eq!(
		bytes(&watch),
		before,
		"sync must not rewrite the superblock for nothing"
	);
	Ok(())
}

#[test]
fn test_intermediate_object_generations_are_pruned_at_seal() -> Result<()> {
	// -- Setup & Fixtures: the same file, appended to five times inside one
	// session. Each append re-places the file inode, its extent node and the
	// directory pair, so the session holds five generations of each. Only the
	// last is reachable from the sealed root, and only the last may be written.
	let mut store = store(BLOCKS)?;
	store.mkdir("/log", 0o755)?;
	store.begin_staging(0)?;
	for i in 0..5 {
		store.staged_append("/log/stream", format!("record {i}\n").as_bytes(), 0o644)?;
	}

	// -- Exec
	let result = store.seal_staged()?;

	// -- Check: five chunks, plus **one** generation of every metadata object:
	// one extent node, the file inode, the /log DirNode and its inode, and the
	// root's DirNode and inode. Committing each append separately would carry
	// five of each: 35 objects rather than these 11.
	assert_eq!(
		result.objects_written, 11,
		"the seal must write the staged chunks plus one generation of metadata, not one per op"
	);
	Ok(())
}

// endregion: --- One seal, one commit

// region:    --- The staged tier is RAM only

#[test]
fn test_a_crash_before_the_seal_loses_the_staged_writes_exactly() -> Result<()> {
	// -- Setup & Fixtures: one committed file, then a session that appends to
	// it and creates another
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/a", b"committed", 0o644)?;
	store.begin_staging(0)?;
	store.staged_append("/a", b" + staged", 0o644)?;
	store.staged_write_new_file("/b", b"staged only", 0o644)?;

	// -- Check: the staged tier sees both writes
	assert_eq!(store.read_file("/a")?, b"committed + staged");
	assert_eq!(store.read_file("/b")?, b"staged only");

	// -- Exec: snapshot the device while the session is open, and mount it as a
	// crash would
	let crashed = open_bytes(bytes(&watch))?;

	// -- Check: the committed tier is exactly what was committed: the append
	// and the new file are gone, whole, not partially applied
	assert_eq!(crashed.read_file("/a")?, b"committed");
	assert!(
		is_no_such_file(
			&expect_err("the staged create must be lost", crashed.read_file("/b")),
			"/b"
		),
		"a staged create must not appear in a committed mount"
	);

	// -- Exec: seal, then crash-reopen again
	store.seal_staged()?;
	let after = open_bytes(bytes(&watch))?;

	// -- Check: now both are durable, byte for byte
	assert_eq!(after.read_file("/a")?, b"committed + staged");
	assert_eq!(after.read_file("/b")?, b"staged only");
	Ok(())
}

#[test]
fn test_the_two_read_tiers_disagree_mid_session() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/state", b"v1", 0o644)?;
	store.begin_staging(0)?;
	store.staged_append("/state", b"-staged", 0o644)?;

	// -- Check: this handle reads the staged tier; the committed tier is the
	// last published root
	assert_eq!(store.read_file("/state")?, b"v1-staged");
	assert_eq!(store.read_file_committed("/state")?, b"v1");
	assert_eq!(store.stat("/state")?.size, 9);
	assert_eq!(
		names(&store.read_dir("/")?),
		vec!["state"],
		"a root listing carries the file"
	);

	// -- Check: a second mount of the same bytes sees committed only, by
	// construction, since staging wrote no device bytes at all
	let other = open_bytes(bytes(&watch))?;
	assert_eq!(other.read_file("/state")?, b"v1");
	Ok(())
}

#[test]
fn test_a_staged_read_touches_no_device_blocks() -> Result<()> {
	// -- Setup & Fixtures: one committed file, and one built entirely inside
	// the session
	let device = CountingDevice::new(BLOCKS);
	let counts = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.write_new_file("/committed", &vec![b'c'; 2000], 0o644)?;
	store.begin_staging(0)?;
	store.staged_write_new_file("/staged", &vec![b's'; 2500], 0o644)?;

	// -- Exec: read the staged file through the staged tier
	counts.reset();
	let staged_bytes_read = store.read_file("/staged")?;

	// -- Check: every object on that path is staged, so the read is zero device
	// reads, the staged root inode included
	assert_eq!(staged_bytes_read.len(), 2500);
	assert_eq!(
		counts.reads(),
		0,
		"a staged read must not touch the device: staged objects are in memory"
	);

	// -- Exec: the same read through the committed tier, for contrast
	counts.reset();
	let committed_bytes_read = store.read_file_committed("/committed")?;

	// -- Check: which does read the volume, as any committed read does
	assert_eq!(committed_bytes_read.len(), 2000);
	assert!(
		counts.reads() > 0,
		"a committed read resolves through the volume and must read blocks"
	);
	Ok(())
}

// endregion: --- The staged tier is RAM only

// region:    --- Interleaved operations and the seal

#[test]
fn test_interleaved_staged_operations_seal_byte_exactly() -> Result<()> {
	// -- Setup & Fixtures: a directory with two committed files
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/logs", 0o755)?;
	store.write_new_file("/logs/a", b"AAA", 0o644)?;
	store.write_new_file("/logs/d", b"delete me", 0o644)?;
	store.begin_staging(0)?;

	// -- Exec: appends and creates interleaved with a staged remove
	store.staged_append("/logs/a", b"BBB", 0o644)?;
	store.staged_write_new_file("/logs/c", b"CCC", 0o644)?;
	store.staged_append("/logs/a", b"DDD", 0o644)?;
	store.staged_remove("/logs/d")?;

	// -- Check: the staged view already has the final shape
	let expected: Vec<u8> = b"AAABBBDDD".to_vec();
	assert_eq!(store.read_file("/logs/a")?, expected);
	assert_eq!(names(&store.read_dir("/logs")?), vec!["a", "c"]);

	// -- Exec
	store.seal_staged()?;

	// -- Check: a fresh mount agrees, and the committed bytes are exact
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.read_file("/logs/a")?, expected);
	assert_eq!(reopened.read_file("/logs/c")?, b"CCC");
	assert!(
		is_no_such_file(
			&expect_err("the staged remove must be durable", reopened.read_file("/logs/d")),
			"/logs/d"
		),
		"a staged remove must be part of the sealed root"
	);
	assert_eq!(names(&reopened.read_dir("/logs")?), vec!["a", "c"]);
	Ok(())
}

#[test]
fn test_a_staged_remove_only_session_is_sealable() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/gone", b"bye", 0o644)?;
	store.begin_staging(0)?;
	store.staged_remove("/gone")?;

	// -- Exec
	store.seal_staged()?;

	// -- Check
	let reopened = open_bytes(bytes(&watch))?;
	assert!(is_no_such_file(
		&expect_err("the remove is sealed", reopened.read_file("/gone")),
		"/gone"
	));
	Ok(())
}

#[test]
fn test_a_staged_write_new_file_refuses_a_taken_name() -> Result<()> {
	// -- Setup & Fixtures: one name committed, one staged in the same session
	let mut store = store(BLOCKS)?;
	store.write_new_file("/committed", b"x", 0o644)?;
	store.begin_staging(0)?;
	store.staged_write_new_file("/staged", b"y", 0o644)?;

	// -- Check: both are taken, and the refusal is the ordinary one
	let e = expect_err("committed name", store.staged_write_new_file("/committed", b"z", 0o644));
	assert!(matches!(e, Error::AlreadyExists { ref path } if path == "/committed"));
	let e = expect_err("staged name", store.staged_write_new_file("/staged", b"z", 0o644));
	assert!(matches!(e, Error::AlreadyExists { ref path } if path == "/staged"));

	// -- Check: and the refusals left the session intact
	assert_eq!(store.read_file("/staged")?, b"y");
	Ok(())
}

#[test]
fn test_a_staged_remove_of_a_directory_is_refused() -> Result<()> {
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.begin_staging(0)?;
	let e = expect_err("staged rmdir", store.staged_remove("/d"));
	assert!(matches!(e, Error::IsADirectory { .. }));
	Ok(())
}

// endregion:    --- Interleaved operations and the seal

// region:    --- Capacity and lifecycle

#[test]
fn test_staging_full_refuses_the_operation_and_keeps_the_session() -> Result<()> {
	// -- Setup & Fixtures: a budget that one append fits in and a much larger
	// one does not
	let mut store = store(BLOCKS)?;
	store.begin_staging(20_000)?;
	store.staged_append("/a", &vec![b'a'; 4000], 0o644)?;
	let held = store.staged_bytes().expect("open");

	// -- Exec
	let e = expect_err(
		"a 40 KiB append over a 20 KiB budget",
		store.staged_append("/a", &vec![b'b'; 40_000], 0o644),
	);

	// -- Check: the error names the budget, the session did not grow, and the
	// operation it refused is not partially visible
	assert!(matches!(e, Error::StagingFull { capacity: 20_000, .. }), "got {e:?}");
	assert_eq!(
		store.staged_bytes(),
		Some(held),
		"a refused op must leave the session as it was"
	);
	assert_eq!(
		store.read_file("/a")?,
		vec![b'a'; 4000],
		"no partial write may be visible"
	);

	// -- Check: and the session still seals
	store.seal_staged()?;
	assert_eq!(store.read_file("/a")?, vec![b'a'; 4000]);
	Ok(())
}

#[test]
fn test_an_empty_seal_is_refused_and_end_staging_discards() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/kept", b"kept", 0o644)?;
	let seq = store.commit_seq();

	// -- Exec
	store.begin_staging(0)?;
	let e = expect_err("empty seal", store.seal_staged());

	// -- Check: nothing to seal is a usage error, not a commit that changed no
	// path, and the session stays open rather than being silently closed
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");
	assert!(store.staging_is_open());
	assert_eq!(store.commit_seq(), seq);

	// -- Exec: stage something, then discard it
	store.staged_write_new_file("/discarded", b"never lands", 0o644)?;
	store.end_staging();
	assert!(!store.staging_is_open());
	assert_eq!(store.commit_seq(), seq, "discarding must not commit");

	// -- Check: the volume holds only what it held, and the discarded write is
	// nowhere in the bytes a mount reads
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.read_file("/kept")?, b"kept");
	assert!(is_no_such_file(
		&expect_err("discarded", reopened.read_file("/discarded")),
		"/discarded"
	));
	Ok(())
}

#[test]
fn test_a_second_session_is_refused_until_the_first_ends() -> Result<()> {
	let mut store = store(BLOCKS)?;
	store.begin_staging(0)?;
	let e = expect_err("second session", store.begin_staging(0));
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");

	store.end_staging();
	store.begin_staging(0)?;
	Ok(())
}

#[test]
fn test_the_scoped_wrapper_seals_on_seal_and_discards_on_drop() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;

	// -- Exec: a scoped session that seals
	{
		let mut staging = store.staging(0)?;
		staging.append("/sealed", b"one", 0o644)?;
		staging.append("/sealed", b" two", 0o644)?;
		assert_eq!(staging.store().read_file("/sealed")?, b"one two");
		staging.seal()?;
	}

	// -- Check
	assert!(!store.staging_is_open());
	assert_eq!(store.read_file("/sealed")?, b"one two");

	// -- Exec: a scoped session that is dropped without sealing
	{
		let mut staging = store.staging(0)?;
		staging.write_new_file("/dropped", b"never lands", 0o644)?;
	}

	// -- Check: the drop discarded it, and the sealed write is untouched
	assert!(!store.staging_is_open());
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.read_file("/sealed")?, b"one two");
	assert!(is_no_such_file(
		&expect_err("dropped", reopened.read_file("/dropped")),
		"/dropped"
	));
	Ok(())
}

// endregion:    --- Capacity and lifecycle

// region:    --- Refusals

#[test]
fn test_normal_verbs_are_refused_during_a_session() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/existing", b"x", 0o644)?;
	store.begin_staging(0)?;

	// -- Check: every commit-taking verb is refused, and none of them is
	// silently applied to the committed root
	for (what, result) in [
		("mkdir", store.mkdir("/newdir", 0o755)),
		("create_file", store.create_file("/newfile", 0o644)),
		("write_new_file", store.write_new_file("/newfile", b"y", 0o644)),
		("write_file", store.write_file("/existing", b"z", 0o644)),
		("append", store.append("/existing", b"z", 0o644)),
		("write_at", store.write_at("/existing", 1, b"z")),
		("remove", store.remove("/existing")),
		("rename", store.rename("/existing", "/renamed")),
	] {
		let e = expect_err(what, result);
		assert!(matches!(e, Error::Usage(_)), "{what} must be refused, got {e:?}");
	}

	// -- Check: the committed file is untouched
	assert_eq!(store.read_file_committed("/existing")?, b"x");
	Ok(())
}

#[test]
fn test_rebuild_into_is_refused_during_a_session() -> Result<()> {
	let mut src = store(BLOCKS)?;
	src.write_new_file("/f", b"data", 0o644)?;
	let mut dst = store(BLOCKS)?;

	src.begin_staging(0)?;
	let e = expect_err(
		"rebuild with a staged source",
		ledgerfs_host::rebuild_into(&src, &mut dst),
	);
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");
	src.end_staging();

	dst.begin_staging(0)?;
	let e = expect_err(
		"rebuild into a staged destination",
		ledgerfs_host::rebuild_into(&src, &mut dst),
	);
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");
	Ok(())
}

// endregion:    --- Refusals

// region:    --- A failed seal

/// A memory volume that can be told to refuse writes, so a seal can be made to
/// fail at a known point without arithmetic about how full the volume is.
#[derive(Clone)]
struct FailingDevice {
	inner: MemBlockDevice,
	fail: Arc<AtomicBool>,
}

impl FailingDevice {
	fn new(blocks: u64) -> Self {
		Self {
			inner: MemBlockDevice::new(blocks),
			fail: Arc::new(AtomicBool::new(false)),
		}
	}

	fn start_failing(&self) {
		self.fail.store(true, Ordering::SeqCst);
	}

	fn stop_failing(&self) {
		self.fail.store(false, Ordering::SeqCst);
	}
}

impl BlockDevice for FailingDevice {
	fn size_bytes(&self) -> u64 {
		self.inner.size_bytes()
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> FsResult<()> {
		self.inner.read_bytes_at(offset, buf)
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> FsResult<()> {
		if self.fail.load(Ordering::SeqCst) {
			return Err(Error::Io(std::io::Error::other("injected write failure")));
		}
		self.inner.write_bytes_at(offset, data)
	}

	fn barrier(&self) -> FsResult<()> {
		self.inner.barrier()
	}

	fn is_writable(&self) -> bool {
		self.inner.is_writable()
	}

	fn hint_cache(&self) -> Option<HintCache> {
		self.inner.hint_cache()
	}
}

#[test]
fn test_a_failed_seal_preserves_the_session_and_can_be_retried() -> Result<()> {
	// -- Setup & Fixtures
	let device = FailingDevice::new(BLOCKS);
	let handle = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.write_new_file("/base", b"base", 0o644)?;
	store.begin_staging(0)?;
	store.staged_append("/base", b"+staged", 0o644)?;

	// -- Exec: the commit fails part-way through placing the seal's objects
	handle.start_failing();
	let e = expect_err("a seal whose device refuses writes", store.seal_staged());
	// The kernel wraps the device's error, so the variant is the format one;
	// what matters is that the seal failed rather than silently succeeded.
	assert!(matches!(e, Error::Format(_)), "got {e:?}");

	// -- Check: the session survived whole, the volume is still the old root,
	// and the handle is still writable
	assert!(store.staging_is_open(), "a failed seal must not consume the session");
	assert_eq!(store.read_file("/base")?, b"base+staged", "the staged tier is intact");
	assert_eq!(store.read_file_committed("/base")?, b"base", "nothing was published");

	// -- Exec: the device recovers, and the same session seals
	handle.stop_failing();
	store.seal_staged()?;

	// -- Check
	assert!(!store.staging_is_open());
	assert_eq!(store.read_file("/base")?, b"base+staged");
	Ok(())
}

// endregion:    --- A failed seal

// region:    --- Multi-object staged data

#[test]
fn test_a_staged_write_spanning_objects_seals_as_one_commit() -> Result<()> {
	// -- Setup & Fixtures: more than one 256 KiB object, so the create alone is
	// several chunks. The bytes vary per position: three chunks of identical
	// bytes are one content-addressed object, and this test counts objects.
	let (mut store, watch) = watched(BIG_BLOCKS)?;
	let payload: Vec<u8> = (0..600_000u32).map(|i| (i % 251) as u8).collect();
	let seq = store.commit_seq();

	// -- Exec
	store.begin_staging(0)?;
	store.staged_write_new_file("/big", &payload, 0o644)?;
	let result = store.seal_staged()?;

	// -- Check: one commit, exact bytes from a fresh mount, and the seal wrote
	// three chunks rather than carrying the intermediate extent generations
	assert_eq!(store.commit_seq(), seq + 1);
	assert_eq!(
		result.objects_written, 7,
		"3 chunks, one extent node, one inode, one dir pair"
	);
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.read_file("/big")?, payload);
	Ok(())
}

#[test]
fn test_empty_and_multi_block_staged_appends_round_trip() -> Result<()> {
	let (mut store, watch) = watched(BIG_BLOCKS)?;
	store.write_new_file("/fresh", b"", 0o644)?;
	store.begin_staging(0)?;

	// An empty append to a file that exists changes nothing and must not count
	// as work to seal.
	store.staged_append("/fresh", b"", 0o644)?;
	let e = expect_err("an empty append staged no root change", store.seal_staged());
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");
	store.end_staging();

	// -- Exec: two appends, each spanning more than one object
	store.begin_staging(0)?;
	store.staged_append("/fresh", &vec![b'x'; 300_000], 0o644)?;
	store.staged_append("/fresh", &vec![b'y'; 300_000], 0o644)?;
	store.seal_staged()?;

	// -- Check
	let data = open_bytes(bytes(&watch))?.read_file("/fresh")?;
	assert_eq!(data.len(), 600_000);
	assert!(data[..300_000].iter().all(|b| *b == b'x'));
	assert!(data[300_000..].iter().all(|b| *b == b'y'));
	Ok(())
}

// endregion:    --- Multi-object staged data

// region:    --- Drafts

/// A session appends into a draft rather than building an object per append,
/// so the two routes must still produce the *same objects*: the seal is one
/// commit of the ordinary shape, and a session is indistinguishable from the
/// equivalent sequence of commits.
///
/// The file inode's id is the check that matters: it is the hash of the
/// inode's bytes, which name the extent node by id, which is the hash of every
/// extent in the file's history. Equality here means the draft route rebuilt
/// exactly what the eager `write` route would have.
#[test]
fn test_a_sealed_session_names_the_same_objects_as_the_eager_sequence() -> Result<()> {
	// -- Setup & Fixtures: twin volumes with the same seed file
	let mut staged = store(BLOCKS)?;
	let mut eager = store(BLOCKS)?;
	for twin in [&mut staged, &mut eager] {
		twin.mkdir("/log", 0o755)?;
		twin.write_new_file("/log/stream", b"seed\n", 0o644)?;
	}
	let records: Vec<Vec<u8>> = (0..6).map(|i| format!("record {i}\n").into_bytes()).collect();

	// -- Exec: one session of appends against one commit per append
	staged.begin_staging(0)?;
	for record in &records {
		staged.staged_append("/log/stream", record, 0o644)?;
	}
	staged.seal_staged()?;
	for record in &records {
		eager.append("/log/stream", record, 0o644)?;
	}

	// -- Check: same content, same sizes, and the same object ids all the way
	// down the file's subtree
	let staged_inode = staged.stat("/log/stream")?;
	let eager_inode = eager.stat("/log/stream")?;
	assert_eq!(staged_inode.size, eager_inode.size);
	assert_eq!(
		staged_inode.content_ref, eager_inode.content_ref,
		"the sealed extent node must be the one the eager appends built"
	);
	assert_eq!(
		staged_inode.object_id, eager_inode.object_id,
		"the sealed inode must be the one the eager appends built"
	);
	assert_eq!(staged.read_file("/log/stream")?, eager.read_file("/log/stream")?);
	Ok(())
}

/// The reason the draft exists: a session's cost is the bytes it appended, not
/// the history of the file it appended to.
///
/// Before the draft layer every staged append re-encoded the whole extent node
/// and kept the previous generation, so N appends held O(N²) bytes and the
/// per-append time grew with N. This pins the memory half: a session of 256
/// distinct records holds its payload plus one descriptor per record, and the
/// seal still writes one generation of each metadata object.
#[test]
fn test_a_draft_is_linear_in_the_appended_bytes_not_the_file_history() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BIG_BLOCKS)?;
	store.mkdir("/log", 0o755)?;
	store.write_new_file("/log/stream", b"", 0o644)?;
	const N: usize = 256;
	const RECORD: usize = 960;

	// -- Exec
	store.begin_staging(0)?;
	for i in 0..N {
		// Distinct bytes per record, so this measures history rather than
		// content-addressed dedupe.
		let mut record = [0u8; RECORD];
		record[..8].copy_from_slice(&(i as u64).to_le_bytes());
		store.staged_append("/log/stream", &record, 0o644)?;
	}

	// -- Check: the session holds the payload plus a descriptor per extent, and
	// no generation of anything else
	let payload = (N * RECORD) as u64;
	let staged = store.staged_bytes().expect("a session is open");
	assert!(
		staged <= payload + (N as u64 * 128),
		"a session of {N} appends must hold about its payload: {staged} bytes staged for {payload} bytes of records"
	);

	// -- Exec: seal
	let result = store.seal_staged()?;

	// -- Check: one chunk per record, one extent node, one inode, and the
	// directory pair the path lives under (plus the root's, for /log)
	assert_eq!(result.objects_written, N + 6);
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.stat("/log/stream")?.size, payload);
	let content = reopened.read_file("/log/stream")?;
	assert_eq!(content.len() as u64, payload);
	for i in 0..N {
		let at = i * RECORD;
		assert_eq!(&content[at..at + 8], &(i as u64).to_le_bytes());
	}
	Ok(())
}

#[test]
fn test_a_drafted_read_crosses_the_committed_and_staged_boundary() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"abc", 0o644)?;
	store.begin_staging(0)?;
	store.staged_append("/f", b"defgh", 0o644)?;

	// -- Check: read_at is a byte window over both tiers, clamped as always
	assert_eq!(store.read_at("/f", 0, 3)?, b"abc", "a range inside the committed bytes");
	assert_eq!(store.read_at("/f", 2, 4)?, b"cdef", "a range across the boundary");
	assert_eq!(store.read_at("/f", 3, 100)?, b"defgh", "a range wholly in the draft");
	assert_eq!(store.read_at("/f", 8, 5)?, b"", "a range past the end is empty");
	assert_eq!(store.read_file("/f")?, b"abcdefgh");

	// -- Exec & Check: the same windows from the committed tier after the seal
	store.seal_staged()?;
	assert_eq!(store.read_at("/f", 2, 4)?, b"cdef");
	Ok(())
}

/// A `stat` asks for an inode, and a draft does not have one yet, so the read
/// materializes. It must not publish: the committed tier still answers what a
/// crash would leave, and the seal is still the only commit.
#[test]
fn test_stat_materializes_a_draft_without_publishing_it() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/state", b"old", 0o644)?;
	let seq = store.commit_seq();
	store.begin_staging(0)?;
	store.staged_append("/state", b"-new", 0o644)?;

	// -- Exec
	let inode = store.stat("/state")?;

	// -- Check: the staged size, a live content reference, and no commit
	assert_eq!(inode.size, 7);
	assert!(!inode.content_ref.is_zero(), "the materialized inode names its content");
	assert_eq!(store.read_file_committed("/state")?, b"old");
	assert_eq!(store.commit_seq(), seq, "a metadata read must not commit");

	// -- Check: the listing carries the same size, not the committed one
	let listing = store.read_dir("/")?;
	let entry = listing.iter().find(|entry| entry.name == "state").expect("state is listed");
	assert_eq!(entry.size, 7);
	assert_eq!(
		entry.inode_ref.object_id, inode.object_id,
		"the listing names the same inode"
	);

	// -- Exec & Check: the seal publishes what the stat already showed
	store.seal_staged()?;
	assert_eq!(store.read_file("/state")?, b"old-new");
	Ok(())
}

#[test]
fn test_a_refused_append_leaves_an_open_draft_untouched() -> Result<()> {
	// -- Setup & Fixtures: room for two 1 KiB appends and not for a 6 KiB one
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"seed", 0o644)?;
	store.begin_staging(6_000)?;
	store.staged_append("/f", &vec![b'a'; 1000], 0o644)?;
	store.staged_append("/f", &vec![b'b'; 1000], 0o644)?;
	let held = store.staged_bytes().expect("open");

	// -- Exec
	let e = expect_err(
		"a 6 KiB append over a 6 KiB budget",
		store.staged_append("/f", &vec![b'c'; 6000], 0o644),
	);

	// -- Check: the draft is where it was, and the refused bytes are nowhere
	assert!(matches!(e, Error::StagingFull { capacity: 6_000, .. }), "got {e:?}");
	assert_eq!(
		store.staged_bytes(),
		Some(held),
		"a refused append must not extend the draft"
	);
	let expected = [b"seed".as_slice(), &[b'a'; 1000], &[b'b'; 1000]].concat();
	assert_eq!(store.read_file("/f")?, expected);

	// -- Check: and the session still seals what it had
	store.seal_staged()?;
	assert_eq!(store.read_file("/f")?, expected);
	Ok(())
}

/// A draft is opened by an append, and the file is still an ordinary path to
/// every other staged verb: a sibling create, a removal elsewhere, and further
/// appends may interleave freely.
#[test]
fn test_a_draft_interleaves_with_staged_creates_and_removes() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/gone", b"gone", 0o644)?;

	// -- Exec
	store.begin_staging(0)?;
	store.staged_append("/d/log", b"one\n", 0o644)?; // creates the file
	store.staged_append("/d/log", b"two\n", 0o644)?; // opens a draft
	store.staged_write_new_file("/d/other", b"other", 0o644)?;
	store.staged_append("/d/log", b"three\n", 0o644)?; // the draft outlives the sibling create
	store.staged_remove("/d/gone")?;
	store.staged_append("/d/log", b"four\n", 0o644)?;
	store.seal_staged()?;

	// -- Check: a fresh mount sees exactly the tree the verbs described
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.read_file("/d/log")?, b"one\ntwo\nthree\nfour\n");
	assert_eq!(reopened.read_file("/d/other")?, b"other");
	assert!(
		is_no_such_file(&expect_err("removed", reopened.read_file("/d/gone")), "/d/gone"),
		"the removal must survive the seal"
	);
	Ok(())
}

/// A file that is appended to and then removed in one session never needs its
/// appended bytes as objects: the draft is dropped with the entry, and the
/// seal carries only the removal.
#[test]
fn test_a_drafted_file_can_be_removed_before_the_seal() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.write_new_file("/doomed", b"original", 0o644)?;
	store.write_new_file("/kept", b"kept", 0o644)?;

	// -- Exec
	store.begin_staging(0)?;
	store.staged_append("/doomed", b" + appended", 0o644)?;
	store.staged_append("/kept", b" +", 0o644)?;
	store.staged_remove("/doomed")?;
	let result = store.seal_staged()?;

	// -- Check: /kept's chunk, its node, its inode and the root pair; nothing
	// for the drafted bytes of /doomed
	assert_eq!(result.objects_written, 5, "a removed draft must not be materialized");
	let reopened = open_bytes(bytes(&watch))?;
	assert_eq!(reopened.read_file("/kept")?, b"kept +");
	assert!(
		is_no_such_file(&expect_err("removed", reopened.read_file("/doomed")), "/doomed"),
		"the drafted file must be gone"
	);
	Ok(())
}

/// The deferred seal is the same seal without the superblock, and it has to
/// flush drafts exactly as the durable one does; otherwise a caller batching
/// seals would hold the last group's bytes in RAM.
#[test]
fn test_a_deferred_seal_materializes_the_drafts() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	store.begin_staging(0)?;
	store.staged_append("/a", b"first", 0o644)?;
	store.staged_append("/a", b"-second", 0o644)?;

	// -- Exec
	store.seal_staged_deferred()?;

	// -- Check: the handle reads it, the volume does not yet
	assert!(store.has_pending_sync());
	assert_eq!(store.read_file("/a")?, b"first-second");
	assert!(is_no_such_file(
		&expect_err("not published", open_bytes(bytes(&watch))?.read_file("/a")),
		"/a"
	));

	// -- Exec & Check: the durable point
	store.sync()?;
	assert_eq!(open_bytes(bytes(&watch))?.read_file("/a")?, b"first-second");
	Ok(())
}

// endregion:    --- Drafts

// region:    --- Sharing

/// A session lives inside the handle, so it must not take `Store`'s
/// `Send + Sync` away. The draft is read through an interior lock precisely
/// so an embedder that fans one handle out across threads keeps compiling.
#[test]
fn test_a_store_with_a_session_open_is_still_send_and_sync() -> Result<()> {
	// -- Setup & Fixtures: the compile-time half of the embedding contract
	fn assert_send_sync<T: Send + Sync>() {}
	assert_send_sync::<Store>();

	// -- Exec: open a session, so the type under assertion is the one that
	// exists while the session does
	let mut s = store(BLOCKS)?;
	s.begin_staging(0)?;
	s.staged_append("/a", b"staged", 0o644)?;

	// -- Check: the value is usable from a scoped thread while the session is
	// open on this one; the lock is what makes that legal
	let seen = std::thread::scope(|scope| {
		scope
			.spawn(|| {
				let bytes = s.read_file("/a")?;
				Ok::<_, Error>(bytes)
			})
			.join()
			.expect("thread")
	})?;
	assert_eq!(seen, b"staged");
	Ok(())
}

// endregion:    --- Sharing
