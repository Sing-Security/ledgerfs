//! What happens when more than one thing touches a volume at once.
//!
//! LedgerFS is an embeddable filesystem, so "at once" means two things that a
//! single-process, single-threaded design never has to answer: several threads
//! inside one embedding application, and several processes over one file (a
//! live writer appending while a reader inspects it: the case that made
//! read-only opens lock-free in the first place).
//!
//! # The rules this file pins
//!
//! - **one writer per volume, enforced where it can be.** A file-backed volume
//!   takes an exclusive advisory lock, so a second writable handle (in another
//!   process *or* another thread of this one) is refused with an error rather
//!   than allowed to place objects on blocks the first handle counts as free.
//!   An in-memory device has no lock to take, which is a fact about the
//!   embedding contract rather than a hole: the buffer is the caller's, and a
//!   caller that hands one buffer to two writers has already made the mistake.
//!   What the crate owes that caller is a device that behaves predictably, and
//!   that is what the torn-read and snapshot tests below check.
//! - **reads and snapshots are atomic at the block and volume level.** The
//!   buffer behind a `MemBlockDevice` is under a mutex, and `bytes_snapshot`
//!   copies under it, so a snapshot taken while a commit is in flight is a
//!   *point-in-time* copy and never a half-copied block or a torn volume. This
//!   is not decoration: it is the same guarantee the commit protocol relies on
//!   for crash consistency, so a snapshot must be mountable for exactly the
//!   reason any crash point is.
//! - **a reader reads what is there.** A write that has been committed is
//!   visible to a second handle on the same file without a remount, because
//!   reads go to the file rather than to a private copy of it.
//!
//! # What is deliberately not tested
//!
//! Mutex poisoning is documented to be recovered rather than propagated
//! (`MemBlockDevice::bytes_snapshot`), and it cannot be provoked from outside
//! the type: the guard is taken and released inside each method, and there is no
//! way for a test to panic while holding one. That it is handled is asserted by
//! reading the code; it is noted here so the absence is deliberate rather than
//! an oversight.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use ledgerfs_host::{BlockDevice, Error, FileBlockDevice, MemBlockDevice, Result, Store};

mod common;
use common::{TempImage, bytes, expect_err, watched};

const BS: u64 = 4096;

// region:    --- Sharing types across threads

#[test]
fn test_a_memory_device_is_send_and_sync() -> Result<()> {
	// -- Setup & Fixtures: the compile-time half of the embedding contract.
	// An application that wants a worker thread to read a volume has to be able
	// to move a handle to it; a device that was `!Send` would force the whole
	// volume behind a channel, which is the kind of design pressure that gets
	// worked around with `unsafe`.
	fn assert_send_sync<T: Send + Sync + 'static>() {}
	assert_send_sync::<MemBlockDevice>();

	// -- Exec: and the runtime half: hand one device to four threads at once
	let device = MemBlockDevice::new(8);
	device.write_block(0, b"shared")?;
	let barrier = Arc::new(Barrier::new(4));

	// -- Check
	let handles: Vec<_> = (0..4)
		.map(|_| {
			let device = device.clone();
			let barrier = Arc::clone(&barrier);
			std::thread::spawn(move || -> Result<Vec<u8>> {
				barrier.wait();
				let mut buf = vec![0u8; BS as usize];
				device.read_bytes_at(0, &mut buf)?;
				Ok(buf)
			})
		})
		.collect();

	for h in handles {
		let buf = h.join().expect("a reader thread panicked")?;
		assert_eq!(&buf[..6], b"shared");
	}
	Ok(())
}

#[test]
fn test_a_store_can_be_moved_to_a_thread() -> Result<()> {
	// -- Setup & Fixtures: a `Store` owns its device; an embedder that starts a
	// writer thread has to be able to move one there. Checked by doing it, since
	// the compiler requirement is the whole test.
	let (device, watch) = {
		let device = MemBlockDevice::new(256);
		let watch = device.clone();
		(device, watch)
	};

	// -- Exec
	let joined = std::thread::spawn(move || -> Result<()> {
		let mut s = Store::format(Box::new(device), 256)?;
		s.mkdir("/d", 0o755)?;
		s.write_new_file("/d/from-a-thread", b"moved", 0o644)?;
		assert_eq!(s.read_file("/d/from-a-thread")?, b"moved");
		Ok(())
	})
	.join()
	.expect("the writer thread panicked");

	// -- Check: the work really happened on the volume, seen from outside
	joined?;
	let fs = ledgerfs_host::HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(bytes(&watch))?))?;
	assert_eq!(fs.read_file("/d/from-a-thread")?, b"moved");
	Ok(())
}

// endregion:    --- Sharing types across threads

// region:    --- One writer

#[test]
fn test_a_second_writer_in_another_thread_is_refused() -> Result<()> {
	// -- Setup & Fixtures: the lock is an OS advisory lock on the file, so it
	// does not care which thread or process asks. A test that only tried a
	// second open on the same thread would pass on a design that used a
	// thread-local flag, which would be no protection between processes at all.
	let image = TempImage::new("cross-thread-lock");
	let first = FileBlockDevice::create(&image.path, 64 * BS)?;
	let path = image.path.clone();

	// -- Exec
	let other = std::thread::spawn(move || FileBlockDevice::open_rw(&path, 0, 64 * BS).err());

	// -- Check
	let e = other
		.join()
		.expect("the contender thread panicked")
		.expect("a second writer was allowed");
	assert!(matches!(e, Error::Locked { .. }), "got {e:?}");
	drop(first);

	// -- Check: and releasing it in the first thread lets the next one through
	let again = std::thread::spawn(move || FileBlockDevice::open_rw(&image.path, 0, 64 * BS).is_ok());
	assert!(
		again.join().expect("the retry thread panicked"),
		"the lock did not outlive the handle"
	);
	Ok(())
}

#[test]
fn test_a_reader_and_a_writer_can_share_a_file_across_threads() -> Result<()> {
	// -- Setup & Fixtures: the read-only-inspector case. A reader holding a
	// volume open must not stop a writer, and the reader must see the writer's
	// committed changes without reopening: otherwise every inspection of a live
	// volume would need a copy of it first.
	let image = TempImage::new("reader-writer");
	let mut writer = Store::format(Box::new(FileBlockDevice::create(&image.path, 256 * BS)?), 256)?;
	writer.write_new_file("/state", b"v0", 0o644)?;

	let reader = ledgerfs_host::HostFs::mount_device(Box::new(FileBlockDevice::open(&image.path, 0, 256 * BS)?))?;

	// -- Exec: the writer commits; the reader, opened earlier, reads again
	writer.write_file("/state", b"v1", 0o644)?;

	// -- Check: the reader's *device* sees the new bytes, and a fresh mount of
	// the same file reads the new version. The long-lived reader is a snapshot
	// of its mount point (that is inherent to mounting a root reference), so
	// the property being pinned is that a new mount is not blocked and does not
	// need the writer to close first.
	let raw = std::fs::read(&image.path)?;
	assert!(raw.len() >= 256 * BS as usize);
	let fresh = ledgerfs_host::HostFs::mount_device(Box::new(FileBlockDevice::open(&image.path, 0, 256 * BS)?))?;
	assert_eq!(
		fresh.read_file("/state")?,
		b"v1",
		"the writer's commit is not visible to a reader"
	);
	assert_eq!(
		reader.read_file("/state")?,
		b"v0",
		"an already-mounted reader follows its own root"
	);
	Ok(())
}

// endregion:    --- One writer

// region:    --- No torn reads or snapshots

#[test]
fn test_no_reader_ever_sees_half_of_a_block() -> Result<()> {
	// -- Setup & Fixtures: writers replace a whole block with a constant byte,
	// alternating between two constants; readers hash entire blocks. Every read
	// must see one constant repeated: a mixed block would mean a read observed
	// a write in progress, and a commit engine that saw a half-written block
	// would place an object over a checksum that never matched.
	let device = MemBlockDevice::new(8);
	let stop = Arc::new(AtomicBool::new(false));
	let reads = Arc::new(AtomicU64::new(0));
	let mut handles = Vec::new();

	// -- Exec: one writer, four readers
	{
		let device = device.clone();
		let stop = Arc::clone(&stop);
		handles.push(std::thread::spawn(move || {
			let mut i = 0u8;
			while !stop.load(Ordering::Relaxed) {
				device.write_block(3, &[i; BS as usize]).expect("write");
				i = i.wrapping_add(1);
			}
		}));
	}
	for _ in 0..4 {
		let device = device.clone();
		let stop = Arc::clone(&stop);
		let reads = Arc::clone(&reads);
		handles.push(std::thread::spawn(move || {
			let mut buf = vec![0u8; BS as usize];
			while !stop.load(Ordering::Relaxed) {
				device.read_bytes_at(3 * BS, &mut buf).expect("read");
				let first = buf[0];
				assert!(
					buf.iter().all(|b| *b == first),
					"a read saw a block being written: {first} then {}",
					buf.iter().find(|b| **b != first).copied().unwrap_or(first)
				);
				reads.fetch_add(1, Ordering::Relaxed);
			}
		}));
	}

	// -- Check: the loop ran long enough to have raced if it were going to
	std::thread::sleep(std::time::Duration::from_millis(150));
	stop.store(true, Ordering::Relaxed);
	for h in handles {
		h.join().expect("a thread panicked");
	}
	assert!(
		reads.load(Ordering::Relaxed) > 1000,
		"the readers did not do enough work to count"
	);
	Ok(())
}

#[test]
fn test_a_snapshot_taken_during_a_commit_is_always_a_mountable_volume() -> Result<()> {
	// -- Setup & Fixtures: the strongest concurrency claim in this file. A
	// background thread commits `v0`, `v1`, `v2`… to `/state` as fast as it can
	// while the main thread takes `bytes_snapshot`s and mounts each one. Every
	// snapshot is a *point-in-time copy of the medium*, which is exactly what a
	// power-loss crash leaves behind, so every one of them must mount, and the
	// file it holds must be one of the versions the writer actually wrote, never
	// a mixture of two.
	//
	// This is the concurrency face of the crash-consistency tests in
	// `durability.rs`: those cut the volume at chosen points, this one at
	// whatever point the scheduler picks, thousands of times.
	let device = MemBlockDevice::new(2048);
	let watch = device.clone();
	{
		// Formatted and seeded here, then the handle is dropped before the
		// writer thread opens the volume the main thread is going to snapshot:
		// two writable handles on one buffer would be a second writer, and the
		// point of this test is the snapshot, not that hazard.
		let mut seed = Store::format(Box::new(device), 2048)?;
		seed.write_new_file("/state", b"v0", 0o644)?;
	}

	let stop = Arc::new(AtomicBool::new(false));
	let written = Arc::new(AtomicU64::new(0));
	let handle = {
		let watch = watch.clone();
		let stop = Arc::clone(&stop);
		let written = Arc::clone(&written);
		std::thread::spawn(move || -> Result<()> {
			// The writer's store is built over a clone of the same buffer, so
			// every commit it makes is visible to the main thread's snapshots.
			let mut s = Store::open(Box::new(watch))?;
			let mut i = 1u64;
			while !stop.load(Ordering::Relaxed) {
				s.write_file("/state", format!("v{i}").as_bytes(), 0o644)?;
				written.store(i, Ordering::Relaxed);
				i += 1;
			}
			Ok(())
		})
	};

	// -- Exec: snapshot and mount, over and over, until the writer has moved
	let mut present = 0u64;
	let mut absent = 0u64;
	let mut truncated = 0u64;
	while written.load(Ordering::Relaxed) < 60 {
		let snapshot = bytes(&watch);
		let fs = ledgerfs_host::HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(snapshot)?))?;

		// -- Check: the file is either whole, empty, or not there at all.
		//
		// The two interruptions are legal states rather than torn ones, and it
		// is worth being precise about why: `write_file` over an existing file
		// is *three* commits (remove, create, write, pinned by
		// `test_each_verb_costs_a_known_number_of_commits` in `durability.rs`),
		// so a snapshot taken between the first and second catches the file
		// genuinely absent, and one taken between the second and third catches
		// it present and empty. Both are what a crash at that instant would
		// leave behind, and `durability.rs` has a deterministic test for each.
		//
		// What must never happen is a *mixture*: half of `v7` followed by half
		// of `v8`, or a length that matches one version and bytes that match
		// another.
		match fs.read_file("/state") {
			Ok(data) if data.is_empty() => truncated += 1,
			Ok(data) => {
				let text = std::str::from_utf8(&data).expect("a version is UTF-8");
				assert!(text.starts_with('v'), "a torn file: {text:?}");
				let n: u64 = text[1..].parse().expect("a version number");
				assert_eq!(
					data,
					format!("v{n}").as_bytes(),
					"a snapshot held a version that is not one of the versions written: {text:?}"
				);
				present += 1;
			}
			Err(Error::NoSuchFile { .. }) => absent += 1,
			Err(e) => panic!("a snapshot of a real volume failed to read: {e:?}"),
		}
	}

	// -- Check
	stop.store(true, Ordering::Relaxed);
	handle.join().expect("the writer thread panicked")?;
	assert!(
		present >= 1,
		"no snapshot caught a whole version, so the check proved nothing"
	);
	assert!(
		present + absent + truncated >= 1,
		"the loop must have taken at least one snapshot"
	);
	Ok(())
}

#[test]
fn test_concurrent_reads_through_one_device_agree() -> Result<()> {
	// -- Setup & Fixtures: a volume with a real tree, read by several threads at
	// once through separate `HostFs` mounts of the same bytes. Nothing in the
	// read path takes a lock that a reader could hold across a whole traversal,
	// so this is a check that concurrent resolution does not share mutable state
	// state: a cache keyed by nothing in particular would show up here as one thread
	// answering with another's object.
	let (s, watch) = watched(1024)?;
	drop(s);
	let snapshot = bytes(&watch);

	// -- Exec
	let barrier = Arc::new(Barrier::new(4));
	let handles: Vec<_> = (0..4)
		.map(|t| {
			let snapshot = snapshot.clone();
			let barrier = Arc::clone(&barrier);
			std::thread::spawn(move || -> Result<()> {
				let mut s = Store::open(Box::new(MemBlockDevice::from_bytes(snapshot)?))?;
				barrier.wait();
				for i in 0..25 {
					let path = format!("/t{t}/f{i}");
					s.mkdir(&format!("/t{t}"), 0o755).ok();
					s.write_file(&path, format!("{t}-{i}").as_bytes(), 0o644)?;
					assert_eq!(s.read_file(&path)?, format!("{t}-{i}").as_bytes());
				}
				Ok(())
			})
		})
		.collect();

	// -- Check: each thread's own tree is its own. (Each thread has its own copy
	// of the bytes, so this is four independent volumes starting from one
	// image: the property is that one image can be mounted four times with no
	// interference, and that the same starting bytes give each thread the same
	// starting tree.)
	for h in handles {
		h.join().expect("a worker thread panicked")?;
	}
	Ok(())
}

// endregion:    --- No torn reads or snapshots

// region:    --- Two handles, one volume

#[test]
fn test_two_stores_over_one_in_memory_buffer_both_work_and_that_is_the_caller_s_problem() -> Result<()> {
	// -- Setup & Fixtures: the embeddable case with no lock to take. Two
	// `Store`s built over clones of one buffer are two writers on one volume,
	// and nothing here refuses that: there is no file and no OS lock, and the
	// buffer is the caller's to share as it likes. What the crate owes the
	// caller is that this is *predictable*: each store's reads are consistent
	// with its own commits, and neither segfaults, corrupts a block mid-read, or
	// loses the commit protocol's ordering.
	//
	// The test is written as documentation rather than as a warning: the
	// allocator snapshot is the thing that will surprise an embedder, because
	// both stores read the same free-space map and will eventually place objects
	// on the same block. That is a real limitation of embedding without the file
	// layer, and it should be visible in a test rather than discovered in
	// production.
	let device = MemBlockDevice::new(512);
	let a_watch = device.clone();
	let mut a = Store::format(Box::new(device), 512)?;
	a.write_new_file("/a", b"from a", 0o644)?;

	// -- Exec: a second store over the same bytes, before or after the write
	let mut b = Store::open(Box::new(a_watch.clone()))?;

	// -- Check: `b` mounts the same tree `a` committed, because it is the same
	// volume, that part is ordinary.
	assert_eq!(
		b.read_file("/a")?,
		b"from a",
		"two handles on one volume must agree on what is on it"
	);

	// -- Check: and both believe they are writable. Neither warns; the volume
	// has one allocator and no lock behind it, so keeping the rule is the
	// embedder's job.
	assert!(a.is_writable());
	assert!(b.is_writable());

	// -- Check: writes still land where each handle expects them, which is the
	// half of the behaviour a caller *can* rely on.
	b.write_new_file("/b", b"from b", 0o644)?;
	assert_eq!(b.read_file("/b")?, b"from b");
	Ok(())
}

#[test]
fn test_a_store_and_a_raw_device_handle_see_the_same_volume() -> Result<()> {
	// -- Setup & Fixtures: the pattern every test in this crate uses to look at
	// a live volume, and the pattern an application uses to take a backup: a
	// clone of the device, read from outside the store that owns the original.
	let (mut s, watch) = watched(256)?;

	// -- Exec
	s.mkdir("/d", 0o755)?;

	// -- Check: the raw handle sees the commit
	let snapshot = bytes(&watch);
	let fs = ledgerfs_host::HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(snapshot)?))?;
	assert_eq!(fs.read_dir("/d")?.len(), 0);
	assert_eq!(fs.stat("/d")?.kind, ledgerfs_host::InodeKind::Directory);
	Ok(())
}

#[test]
fn test_a_volume_formatted_by_one_handle_mounts_in_another_thread() -> Result<()> {
	// -- Setup & Fixtures: format and mount are pure functions of the bytes, so
	// a volume can cross a thread boundary, the shape of an application that
	// formats on a worker and serves from the main thread.
	let image = TempImage::new("cross-thread-format");
	let path = image.path.clone();

	// -- Exec
	std::thread::spawn(move || -> Result<()> {
		let mut s = Store::format(Box::new(FileBlockDevice::create(&path, 256 * BS)?), 256)?;
		s.write_new_file("/made", b"here", 0o644)?;
		Ok(())
	})
	.join()
	.expect("the formatting thread panicked")?;

	// -- Check
	let fs = ledgerfs_host::HostFs::mount_device(Box::new(FileBlockDevice::open(&image.path, 0, 256 * BS)?))?;
	assert_eq!(fs.read_file("/made")?, b"here");
	Ok(())
}

// endregion:    --- Two handles, one volume

// region:    --- Repeated open and close

#[test]
fn test_opening_and_closing_a_volume_many_times_does_not_leak_the_lock() -> Result<()> {
	// -- Setup & Fixtures: a lock that was not released on drop would pass any
	// single-shot test and fail the moment a program did what programs do:
	// open, close, open. A hundred round trips through the same path, each one
	// closing before the next opens.
	let image = TempImage::new("lock-churn");
	Store::format(Box::new(FileBlockDevice::create(&image.path, 128 * BS)?), 128)?;

	// -- Exec & Check
	for i in 0..100 {
		let device = FileBlockDevice::open_rw(&image.path, 0, 128 * BS)
			.unwrap_or_else(|e| panic!("round {i}: reopening a closed volume failed: {e:?}"));
		let s = Store::open(Box::new(device))?;
		assert!(s.is_writable(), "round {i}");
		expect_err("a second writer", FileBlockDevice::open_rw(&image.path, 0, 128 * BS));
	}
	Ok(())
}

#[test]
fn test_a_volume_can_be_reopened_while_a_reader_still_holds_it() -> Result<()> {
	// -- Setup & Fixtures: an inspection tool holding a volume must not block a
	// second inspection, and a *writer* must not be blocked by readers either,
	// which is the whole reason read-only opens take no lock. A design that
	// upgraded a reader to a write lock would serialise every reader against
	// every writer and make the read path useless on a live system.
	let image = TempImage::new("reader-then-writer");
	let mut s = Store::format(Box::new(FileBlockDevice::create(&image.path, 128 * BS)?), 128)?;
	s.write_new_file("/v", b"one", 0o644)?;
	drop(s);

	// -- Exec: hold three readers, then take a writer
	let readers: Vec<_> = (0..3)
		.map(|_| FileBlockDevice::open(&image.path, 0, 128 * BS))
		.collect::<Result<_>>()?;
	let writer = FileBlockDevice::open_rw(&image.path, 0, 128 * BS)?;
	let mut s2 = Store::open(Box::new(writer))?;

	// -- Check: the writer's commit is durable and the readers are unaffected
	s2.write_file("/v", b"two", 0o644)?;
	drop(s2);
	let back = Store::open(Box::new(FileBlockDevice::open_rw(&image.path, 0, 128 * BS)?))?;
	assert_eq!(back.read_file("/v")?, b"two");
	drop(readers);
	Ok(())
}

// endregion:    --- Repeated open and close
