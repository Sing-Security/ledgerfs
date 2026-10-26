//! The read path: `resolve`, `stat`, `read_dir`, `read_file`, `read_at`.
//!
//! The write side of this crate is what an app calls; the read side is what it
//! trusts. Every assertion here reads through `HostFs` rather than through a
//! `Store`, because the interesting question is not "did the write return Ok"
//! but "does a mount of these bytes answer the same way".
//!
//! Three properties get most of the attention:
//!
//! - **`read_at` never errors.** It is the tail-follower's primitive, so past
//!   EOF is an empty vector, not a failure.
//! - **A range read equals the slice of the whole read.** If it did not, a
//!   caller tailing a log would see different bytes from one that read the
//!   file, which is the whole point of having both.
//! - **`stat` reports what `read_dir` reports.** A listing's `size`/`mode` come
//!   from the same inode `stat` decodes, so a disagreement would mean one of
//!   the two is reading something else.

mod common;

use common::*;
use ledgerfs_host::{Error, HostFs, InodeKind, Store};

// region:    --- Mount

#[test]
fn test_mount_device_reports_its_active_root() -> Result<()> {
	// -- Setup & Fixtures
	let (_, watch) = watched(BLOCKS)?;

	// -- Exec
	let fs = mount_bytes(bytes(&watch))?;

	// -- Check
	let root = fs.active_root().expect("a formatted volume has an active root");
	assert!(!root.is_zero(), "the active root is a real object reference");
	fs.require_root()?;
	assert_eq!(fs.root_inode_ref, root, "the two spellings agree");
	Ok(())
}

#[test]
fn test_mount_device_accepts_a_store_opened_volume() -> Result<()> {
	// -- Setup & Fixtures: the two entry points must agree about the same bytes,
	// or `lfs-read` and an embedding app would see different filesystems.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"payload", 0o644)?;
	drop(store);

	// -- Exec
	let via_fs = mount_bytes(bytes(&watch))?;
	let via_store = open_bytes(bytes(&watch))?;

	// -- Check
	assert_eq!(via_fs.read_file("/d/f")?, via_store.read_file("/d/f")?);
	assert_eq!(names(&via_fs.read_dir("/")?), names(&via_store.read_dir("/")?));
	assert_eq!(via_fs.stat("/d/f")?.size, 7);
	Ok(())
}

#[test]
fn test_hostfs_mount_refuses_a_device_it_cannot_even_recognise() -> Result<()> {
	// -- Setup & Fixtures: the `HostFs` entry point is stricter than `Store`'s
	// in one way (it insists on a usable root), but both must first agree that
	// the device is a LedgerFS volume at all.
	//
	// The "valid volume, null root" case (what the image initialiser leaves
	// behind, and the reason `Error::VolumeUncommitted` exists) needs a crafted
	// superblock and lives in `security_media.rs`.

	// -- Exec
	let r = mount_bytes(vec![0u8; 4096 * 64]);

	// -- Check
	let e = expect_err("mount a blank device", r);
	assert!(matches!(e, Error::SuperblockUnreadable { .. }), "got {e:?}");
	Ok(())
}

// endregion:    --- Mount

// region:    --- Path shapes

#[test]
fn test_the_root_resolves_under_every_spelling_of_it() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/top", b"x", 0o644)?;

	// -- Exec & Check: all of these name the root, so all of them must resolve:
	// an empty path, `/`, repeated slashes, and a `.` component.
	for path in ["", "/", "//", "///", "/.", ".", "./"] {
		let stat = store.stat(path).unwrap_or_else(|e| panic!("stat {path:?}: {e:?}"));
		assert_eq!(stat.kind, InodeKind::Directory, "{path:?}");
		assert_eq!(
			names(&store.read_dir(path)?),
			vec!["top".to_string()],
			"read_dir {path:?}"
		);
	}
	Ok(())
}

#[test]
fn test_redundant_separators_and_dots_inside_a_path_are_dropped() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/a", 0o755)?;
	store.write_new_file("/a/f", b"payload", 0o644)?;

	// -- Exec & Check: `/a//./f`, `/a/./f` and `/a/f` are one path
	for path in ["/a/f", "/a//f", "/a/./f", "/a//./f", "a/f", "//a//f//"] {
		assert_eq!(store.read_file(path)?, b"payload", "{path:?}");
	}
	Ok(())
}

#[test]
fn test_a_trailing_slash_does_not_change_what_a_path_names() -> Result<()> {
	// -- Setup & Fixtures: there is no "must be a directory" rule here, so a
	// trailing separator is just another empty component, which is dropped.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"content", 0o644)?;

	// -- Exec & Check
	assert_eq!(store.read_file("/f/")?, b"content");
	assert_eq!(store.stat("/f/")?.kind, InodeKind::RegularFile);
	Ok(())
}

#[test]
fn test_a_relative_path_is_resolved_from_the_root() -> Result<()> {
	// -- Setup & Fixtures: `components_of` splits on `/` and never consults a
	// working directory, so `a/b` and `/a/b` are the same path. Pinned because
	// it is a deliberate choice, not an oversight: a volume has one root and no
	// process context.
	let mut store = store(BLOCKS)?;
	store.mkdir("/a", 0o755)?;
	store.write_new_file("/a/b", b"same", 0o644)?;

	// -- Exec & Check
	assert_eq!(store.read_file("a/b")?, b"same");
	assert_eq!(store.stat("a/b")?.size, 4);
	Ok(())
}

#[test]
fn test_a_dotdot_component_is_refused_wherever_it_appears() -> Result<()> {
	// -- Setup & Fixtures: the store keeps no parent pointers, so `..` could
	// only be honoured by re-walking, and a component that is *silently
	// dropped* would make `../escape` name `escape` inside the volume, which is
	// a surprising answer to a path that asked to leave it.
	let mut store = store(BLOCKS)?;
	store.mkdir("/a", 0o755)?;
	store.write_new_file("/a/f", b"x", 0o644)?;

	// -- Exec & Check
	for path in ["/..", "/a/..", "..", "/a/../f", "../f", "/a/f/.."] {
		let e = expect_err(path, store.stat(path));
		match e {
			Error::Usage(detail) => assert!(detail.contains(".."), "{path:?}: {detail}"),
			other => panic!("{path:?}: expected Usage, got {other:?}"),
		}
	}

	// -- Check: and nothing about the volume changed
	assert_eq!(store.read_file("/a/f")?, b"x");
	Ok(())
}

#[test]
fn test_every_verb_refuses_a_dotdot_component() -> Result<()> {
	// -- Setup & Fixtures: the refusal has to be in the path layer, so every
	// verb inherits it, including the mutating ones, where a dropped `..`
	// would land a write somewhere the caller did not name.
	let mut store = store(BLOCKS)?;

	// -- Exec & Check
	assert!(matches!(store.mkdir("/../d", 0o755), Err(Error::Usage(_))));
	assert!(matches!(store.create_file("/../f", 0o644), Err(Error::Usage(_))));
	assert!(matches!(store.write_file("/../f", b"x", 0o644), Err(Error::Usage(_))));
	assert!(matches!(store.append("/../f", b"x", 0o644), Err(Error::Usage(_))));
	assert!(matches!(store.remove("/../f"), Err(Error::Usage(_))));
	assert!(matches!(store.rename("/f", "/../g"), Err(Error::Usage(_))));
	assert!(matches!(store.read_at("/../f", 0, 1), Err(Error::Usage(_))));

	// -- Check: the volume is still empty
	assert!(store.read_dir("/")?.is_empty());
	Ok(())
}

#[test]
fn test_a_path_that_is_only_dots_and_slashes_is_the_root_and_cannot_be_written() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec & Check: `.` alone names the root, so a create at that path is
	// refused with `Usage` rather than being read as "create a file named ."
	assert_eq!(store.stat(".").expect("stat .").kind, InodeKind::Directory);
	let e = expect_err("mkdir .", store.mkdir(".", 0o755));
	assert!(matches!(e, Error::Usage(_)), "got {e:?}");
	Ok(())
}

// endregion:    --- Path shapes

// region:    --- stat & read_dir

#[test]
fn test_stat_reports_size_and_kind_for_both_node_types() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/dir", 0o755)?;
	store.write_new_file("/file", &vec![3u8; 4321], 0o640)?;

	// -- Check
	let dir = store.stat("/dir")?;
	assert_eq!(dir.kind, InodeKind::Directory);
	assert_eq!(dir.mode, 0o755);

	let file = store.stat("/file")?;
	assert_eq!(file.kind, InodeKind::RegularFile);
	assert_eq!(file.mode, 0o640);
	assert_eq!(file.size, 4321);
	Ok(())
}

#[test]
fn test_read_dir_reports_each_entry_with_its_metadata() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o750)?;
	store.write_new_file("/f", &vec![1u8; 1000], 0o644)?;

	// -- Exec
	let entries = store.read_dir("/")?;

	// -- Check: name, kind, size and mode all come from the child's inode, and
	// must agree with what `stat` says about the same child.
	for e in &entries {
		let inode = store.stat(&format!("/{}", e.name))?;
		assert_eq!(e.kind, inode.kind, "kind for {}", e.name);
		assert_eq!(e.size, inode.size, "size for {}", e.name);
		assert_eq!(e.mode, inode.mode, "mode for {}", e.name);
		assert!(!e.inode_ref.is_zero(), "every entry has a real inode reference");
	}
	assert_eq!(
		listing(store.fs(), "/")?,
		vec![
			("d".to_string(), InodeKind::Directory),
			("f".to_string(), InodeKind::RegularFile)
		]
	);
	Ok(())
}

#[test]
fn test_read_dir_makes_no_ordering_promise_but_is_stable_within_a_mount() -> Result<()> {
	// -- Setup & Fixtures: `readdir` returns whatever order the `DirNode`
	// yields. Callers sort; this test pins that the *same* mount answers the
	// same way twice, so a caller that sorts once does not have to think about
	// it again.
	let mut store = store(BLOCKS)?;
	for name in ["z", "a", "m", "b"] {
		store.write_new_file(&format!("/{name}"), name.as_bytes(), 0o644)?;
	}

	// -- Exec
	let first = names(&store.read_dir("/")?);
	let second = names(&store.read_dir("/")?);

	// -- Check
	assert_eq!(first, second);
	assert_eq!(
		first,
		vec!["a".to_string(), "b".to_string(), "m".to_string(), "z".to_string()]
	);
	Ok(())
}

#[test]
fn test_read_dir_of_a_file_is_not_a_directory() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"x", 0o644)?;

	// -- Exec
	let e = expect_err("read_dir a file", store.read_dir("/f"));

	// -- Check
	match e {
		Error::NotADirectory { path } => assert_eq!(path, "/f"),
		other => panic!("expected NotADirectory, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_read_file_of_a_directory_is_a_directory_error() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let e = expect_err("read_file a directory", store.read_file("/d"));

	// -- Check: the mirror of `read_dir` on a file, and notably *not* the
	// directory's own serialised bytes, which would be an information leak.
	match e {
		Error::IsADirectory { path } => assert_eq!(path, "/d"),
		other => panic!("expected IsADirectory, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_stat_of_a_missing_path_names_it() -> Result<()> {
	// -- Setup & Fixtures
	let mut with_dir = store(BLOCKS)?;
	with_dir.mkdir("/d", 0o755)?;

	// -- Exec & Check
	assert!(is_no_such_file(
		&expect_err("stat /missing", with_dir.stat("/missing")),
		"/missing"
	));
	assert!(is_no_such_file(
		&expect_err("stat /d/missing", with_dir.stat("/d/missing")),
		"/d/missing"
	));
	// A missing path under a *file* cannot be reached at all, so it is the same
	// answer as any other path through a non-directory.
	let mut with_file = store(BLOCKS)?;
	with_file.write_new_file("/f", b"x", 0o644)?;
	let e = expect_err("stat through a file", with_file.stat("/f/child"));
	assert!(matches!(e, Error::NotADirectory { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_a_directory_holds_exactly_four_hundred_entries() -> Result<()> {
	// -- Setup & Fixtures: **a hard, per-directory limit.** `DirNode` has no
	// B-tree splitting, so 400 entries is the ceiling for any one directory,
	// and the 401st insert is refused rather than stored, which is the only
	// safe answer given there is nowhere to put it.
	//
	// This is a real constraint on an embedder: a single directory cannot hold
	// a large flat set of files. It is pinned here because it is the kind of
	// limit that is discovered in production rather than from a doc comment.
	const CAP: usize = 400;
	let mut store = store(BIG_BLOCKS)?;
	let mut expected: Vec<String> = Vec::new();

	// -- Exec: fill it exactly
	for i in 0..CAP {
		let name = format!("item-{i:04}");
		store.write_new_file(&format!("/{name}"), name.as_bytes(), 0o644)?;
		expected.push(name);
	}
	expected.sort();

	let got = names(&store.read_dir("/")?);

	// -- Check: all 400 present, none duplicated
	assert_eq!(got.len(), CAP);
	assert_eq!(got, expected);

	// -- Exec: the entry that does not fit
	let e = expect_err("the 401st entry", store.write_new_file("/item-overflow", b"x", 0o644));

	// -- Check: a named refusal naming the reason, not a corrupt directory. The
	// host surfaces it as `Format` (the kernel's own `InvalidOperation`), which
	// is a thinner answer than the host's other refusals: there is no
	// `Error::DirectoryFull`.
	match e {
		Error::Format(e) => {
			let text = e.to_string();
			assert!(text.contains("directory is full"), "{text}");
			assert!(text.contains("B-tree"), "the reason is named: {text}");
		}
		other => panic!("expected the kernel's directory-full error, got {other:?}"),
	}

	// -- Check: and the directory is exactly as it was; the refused insert did
	// not half-apply
	assert_eq!(names(&store.read_dir("/")?), expected);
	assert_eq!(store.read_file("/item-0000")?, b"item-0000");
	assert_eq!(store.read_file("/item-0399")?, b"item-0399");
	Ok(())
}

#[test]
fn test_the_entry_ceiling_is_a_count_not_a_byte_budget() -> Result<()> {
	// -- Setup & Fixtures: the ceiling is 400 *entries* regardless of how long
	// they are: a `DirNode` holding 400 256-byte names is ~100 KiB, which fits
	// the 256 KiB object ceiling comfortably. So a caller cannot buy more
	// entries by using short names, and cannot lose entries by using long ones.
	let mut store = store(BIG_BLOCKS)?;

	// -- Exec
	for i in 0..400u32 {
		let stem = format!("i{i:04}");
		let name = format!("{stem}{}", "x".repeat(256 - stem.len()));
		store.create_file(&format!("/{name}"), 0o644)?;
	}

	// -- Check
	assert_eq!(store.read_dir("/")?.len(), 400);

	// -- Exec & Check: and the 401st is still refused for count, not for space
	let stem = "i0400";
	let name = format!("{stem}{}", "x".repeat(256 - stem.len()));
	let e = expect_err(
		"the 401st long-named entry",
		store.create_file(&format!("/{name}"), 0o644),
	);
	match e {
		Error::Format(e) => assert!(e.to_string().contains("directory is full"), "{e}"),
		other => panic!("expected the directory-full error, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_filling_a_directory_with_content_costs_far_more_than_the_entries() -> Result<()> {
	// -- Setup & Fixtures: the *cost* ceiling, which is the one that bites in
	// practice. Every commit rewrites the whole `DirNode`, so filling a
	// directory of n entries writes O(n²) bytes of directory data on top of the
	// content: a volume of this size runs out before 400 long-named files with
	// content, even though their content is a few hundred bytes each.
	//
	// This is why free space is part of the embedding contract and why a caller
	// with many files in one directory has to watch it. Measured, not assumed:
	// the test asserts the space consumed is far out of proportion to the
	// content written.
	//
	// The volume is sized here rather than taken from `BIG_BLOCKS`, because
	// `BIG_BLOCKS` no longer exhausts: a file's create and its write are one
	// transaction now, and the create-then-write pair used to rewrite the
	// directory node twice per file. At 64 MiB all 400 entries fit where they
	// previously did not. Half the volume restores the scenario this test is
	// about (running out on churn rather than on content) at the new cost.
	const CHURN_BLOCKS: u64 = BIG_BLOCKS / 2;
	let mut store = store(CHURN_BLOCKS)?;
	let content = b"d"; // 1 byte each
	let mut made = 0usize;
	let start_free = store.free_bytes().expect("free bytes");

	// -- Exec
	for i in 0..400u32 {
		let stem = format!("i{i:04}");
		let name = format!("{stem}{}", "x".repeat(256 - stem.len()));
		match store.write_new_file(&format!("/{name}"), content, 0o644) {
			Ok(()) => made += 1,
			Err(Error::Format(e)) => {
				assert!(e.to_string().contains("space"), "{e}");
				break;
			}
			Err(other) => panic!("entry {i}: unexpected error {other:?}"),
		}
	}

	// -- Check: it stopped short of the count ceiling, on space.
	assert!(
		made < 400,
		"the volume must run out before 400 long-named files (made {made})"
	);
	assert!(
		made > 200,
		"but only by the margin the DirNode churn costs (made {made})"
	);

	let spent = start_free - store.free_bytes().expect("free bytes");
	let content_written = made as u64; // one byte per file
	assert!(
		spent > content_written * 10_000,
		"{spent} bytes spent for {content_written} bytes of content; the directory rewrite should \
		 dwarf the content"
	);
	Ok(())
}

// endregion:    --- stat & read_dir

// region:    --- read_file & read_at

#[test]
fn test_read_file_of_an_empty_file_is_an_empty_vector() -> Result<()> {
	// -- Setup & Fixtures: a created-but-never-written file has a zero content
	// reference, which the read path has to treat as "no bytes" rather than as
	// an object at block 0.
	let mut store = store(BLOCKS)?;
	store.create_file("/empty", 0o644)?;

	// -- Exec
	let got = store.read_file("/empty")?;

	// -- Check
	assert_eq!(got, Vec::<u8>::new());
	assert_eq!(store.read_at("/empty", 0, 0)?, Vec::<u8>::new());
	assert_eq!(
		store.read_at("/empty", 0, 4096)?,
		Vec::<u8>::new(),
		"asking for more than there is"
	);
	assert_eq!(
		store.read_at("/empty", 100, 10)?,
		Vec::<u8>::new(),
		"past the end of nothing"
	);
	Ok(())
}

#[test]
fn test_read_at_returns_the_slice_of_read_file() -> Result<()> {
	// -- Setup & Fixtures: the property that makes `read_at` usable as a tail
	// follower: a range read must be byte-identical to the same slice of the
	// whole read, across block boundaries and across chunk objects.
	let mut store = store(BIG_BLOCKS)?;
	let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(7) % 256) as u8).collect();
	store.write_new_file("/big", &data, 0o644)?;

	// -- Check
	let whole = store.read_file("/big")?;
	assert_eq!(whole, data);

	for (offset, len) in [
		(0u64, 1usize),
		(0, 4096),
		(4095, 2),
		(4096, 4096),
		(100_000, 50_000),
		(262_143, 2),
		(262_144, 100),
		(299_999, 1),
		(299_999, 100_000),
		(300_000, 1),
		(1_000_000, 10),
	] {
		let got = store.read_at("/big", offset, len)?;
		let start = (offset as usize).min(data.len());
		let want_end = ((offset as usize).saturating_add(len)).min(data.len());
		let want: &[u8] = if start >= want_end { &[] } else { &data[start..want_end] };
		assert_eq!(got, want, "read_at({offset}, {len})");
	}
	Ok(())
}

#[test]
fn test_read_at_clamps_instead_of_erroring() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"0123456789", 0o644)?;

	// -- Check: every one of these is a normal answer to a tail follower, not a
	// failure: the difference between "nothing new" and "something is wrong".
	assert_eq!(store.read_at("/f", 0, 4)?, b"0123");
	assert_eq!(store.read_at("/f", 4, 4)?, b"4567");
	assert_eq!(store.read_at("/f", 10, 1)?, Vec::<u8>::new(), "exactly at EOF");
	assert_eq!(store.read_at("/f", 11, 1)?, Vec::<u8>::new(), "past EOF");
	assert_eq!(store.read_at("/f", u64::MAX, 1)?, Vec::<u8>::new(), "an absurd offset");
	assert_eq!(store.read_at("/f", 9, 1_000_000)?, b"9", "a len that runs off the end");
	assert_eq!(store.read_at("/f", 0, 0)?, Vec::<u8>::new(), "a zero-length read");
	assert_eq!(store.read_at("/f", 5, 0)?, Vec::<u8>::new());
	Ok(())
}

#[test]
fn test_read_at_of_a_missing_file_or_a_directory_is_refused() -> Result<()> {
	// -- Setup & Fixtures: the *only* two ways `read_at` can fail are the ones
	// the caller gets wrong before the offset even matters.
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec & Check
	assert!(is_no_such_file(
		&expect_err("read_at /missing", store.read_at("/missing", 0, 1)),
		"/missing"
	));
	let e = expect_err("read_at a directory", store.read_at("/d", 0, 1));
	assert!(matches!(e, Error::IsADirectory { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_a_following_tail_sees_every_appended_byte_exactly_once() -> Result<()> {
	// -- Setup & Fixtures: the actual use: a follower remembers a byte count
	// and asks for the rest, repeatedly, while an appender writes. Every byte
	// must arrive once and in order.
	let mut store = store(BLOCKS)?;
	let mut seen = Vec::new();
	let mut written = Vec::new();

	// -- Exec
	for round in 0..30u32 {
		let line = format!("event {round}\n");
		store.append("/log", line.as_bytes(), 0o644)?;
		written.extend_from_slice(line.as_bytes());

		// A tail with no idea how much is coming asks for a generous window.
		let fresh = store.read_at("/log", seen.len() as u64, 4096)?;
		assert!(!fresh.is_empty(), "round {round}: the tail must see the new bytes");
		seen.extend_from_slice(&fresh);
	}

	// -- Check
	assert_eq!(seen, written, "the tail is the file, byte for byte");
	Ok(())
}

#[test]
fn test_a_read_of_a_whole_large_file_matches_a_range_read_of_the_same_bytes() -> Result<()> {
	// -- Setup & Fixtures: 1 MiB, read three ways that must agree: whole-file,
	// one big range, and the same range in pieces.
	let mut store = store(BIG_BLOCKS)?;
	let data: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
	store.write_new_file("/meg", &data, 0o644)?;

	// -- Exec
	let whole = store.read_file("/meg")?;
	let one_range = store.read_at("/meg", 0, 1_048_576)?;

	let mut in_pieces = Vec::with_capacity(data.len());
	for offset in (0..1_048_576u64).step_by(8192) {
		in_pieces.extend_from_slice(&store.read_at("/meg", offset, 8192)?);
	}

	// -- Check
	assert_eq!(whole, data);
	assert_eq!(one_range, whole);
	assert_eq!(in_pieces, whole);
	Ok(())
}

#[test]
fn test_read_at_works_on_a_file_with_a_hole() -> Result<()> {
	// -- Setup & Fixtures: `write_at` past the end leaves the gap reading as
	// zeroes; a range read that lands entirely inside the hole must agree with
	// the whole read rather than erroring on a missing extent.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/hole", b"head", 0o644)?;
	store.write_at("/hole", 65_536, b"tail")?;

	// -- Check
	let whole = store.read_file("/hole")?;
	assert_eq!(whole.len(), 65_540);
	assert_eq!(&whole[..4], b"head");
	assert_eq!(&whole[65_536..], b"tail");
	assert_eq!(store.read_at("/hole", 65_000, 600)?, whole[65_000..65_540].to_vec());
	assert_eq!(store.read_at("/hole", 4, 65_532)?, whole[4..65_536].to_vec());
	assert_eq!(store.read_at("/hole", 0, 4)?, b"head");
	assert_eq!(store.read_at("/hole", 65_536, 4)?, b"tail");
	Ok(())
}

// endregion:    --- read_file & read_at

// region:    --- What a read costs on the device

/// A warm point read costs one device read per object on the path.
///
/// This is the read path's whole cost model, and it is a count rather than a
/// duration on purpose: a timing assertion would measure the machine, while a
/// count is a property of the design. `/d/f` is a two-level path, so the
/// objects are the root `Inode`, the root `DirNode`, `/d`'s `Inode`, `/d`'s
/// `DirNode`, `/d/f`'s `Inode`, its extent node and its chunk: seven, and one
/// read each.
///
/// The number that would be *eight* is the one this pins against. A lookup that
/// reads a candidate block to see whether it holds the wanted object, and then
/// reads it again to fetch the payload, costs two per object; that is what this
/// path used to do, and halving it is why the count is asserted exactly. A
/// change that adds a read here is not necessarily wrong, but it has to be a
/// deliberate change to this contract rather than a regression nobody noticed.
#[test]
fn test_a_warm_point_read_costs_one_device_read_per_object_on_the_path() -> Result<()> {
	// -- Setup & Fixtures: written and read through one handle, so the id→block
	// map is warm and no candidate has to be found by scanning.
	let device = CountingDevice::new(BLOCKS);
	let counts = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"payload", 0o644)?;

	// -- Exec
	counts.reset();
	let got = store.read_file("/d/f")?;

	// -- Check
	assert_eq!(got, b"payload");
	assert_eq!(
		counts.reads(),
		7,
		"one device read per object on the path (root inode, root dir, /d inode, \
		 /d dir, file inode, extent node, chunk)"
	);
	Ok(())
}

/// The same read on a reused handle is what the second one costs: nothing is
/// re-read and nothing is cached decoded, so the count holds operation after
/// operation.
#[test]
fn test_a_repeated_warm_read_does_not_get_cheaper_or_dearer() -> Result<()> {
	// -- Setup & Fixtures
	let device = CountingDevice::new(BLOCKS);
	let counts = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", b"payload", 0o644)?;
	let _ = store.read_file("/d/f")?;

	// -- Exec
	counts.reset();
	for _ in 0..4 {
		assert_eq!(store.read_file("/d/f")?, b"payload");
	}

	// -- Check
	assert_eq!(
		counts.reads(),
		4 * 7,
		"four reads of the same path cost four times one read of it"
	);
	Ok(())
}

// endregion:    --- What a read costs on the device

// region:    --- Names

#[test]
fn test_names_with_spaces_dots_and_unicode_round_trip() -> Result<()> {
	// -- Setup & Fixtures: a name is a byte string in a `DirNode`; there is no
	// POSIX layer to reject anything here, so the only question is whether the
	// bytes come back.
	let mut store = store(BLOCKS)?;
	let names_in = [
		"a name with spaces",
		"...",
		".hidden",
		"..double-dot-prefix",
		"trailing.dots..",
		"héllo wörld",
		"日本語のファイル",
		"emoji-🚀",
		"tab\tinside",
		"newline\ninside",
	];

	// -- Exec
	for (i, n) in names_in.iter().enumerate() {
		store.write_new_file(&format!("/{n}"), format!("body {i}").as_bytes(), 0o644)?;
	}

	// -- Check: every one is listed and reads back
	let mut got = names(&store.read_dir("/")?);
	got.sort();
	let mut want: Vec<String> = names_in.iter().map(|s| s.to_string()).collect();
	want.sort();
	assert_eq!(got, want);
	for (i, n) in names_in.iter().enumerate() {
		assert_eq!(store.read_file(&format!("/{n}"))?, format!("body {i}").as_bytes());
	}
	Ok(())
}

#[test]
fn test_a_name_at_the_length_limit_is_accepted() -> Result<()> {
	// -- Setup & Fixtures: `DirEntry.name` is a `heapless::String<256>`, so 256
	// bytes is the longest name that can be stored: the *capacity* of the
	// fixed-size string, not one less than it.
	let mut store = store(BLOCKS)?;
	let name = "x".repeat(256);

	// -- Exec
	store.write_new_file(&format!("/{name}"), b"at the limit", 0o644)?;

	// -- Check
	assert_eq!(names(&store.read_dir("/")?), vec![name.clone()]);
	assert_eq!(store.read_file(&format!("/{name}"))?, b"at the limit");
	assert_eq!(store.stat(&format!("/{name}"))?.size, 12);
	Ok(())
}

#[test]
fn test_a_name_over_the_length_limit_is_refused_not_truncated() -> Result<()> {
	// -- Setup & Fixtures: a truncated name would be a *different* file, so the
	// only safe answers are store-it or refuse-it. This pins "refuse".
	let mut store = store(BLOCKS)?;
	let name = "y".repeat(257);

	// -- Exec
	let r = store.write_new_file(&format!("/{name}"), b"too long", 0o644);

	// -- Check: an error, never a panic from `heapless`'s length check, and
	// never a shortened entry that a caller could not find again.
	assert!(r.is_err(), "a 257-byte name must be refused");
	match expect_err("over-long name", r) {
		Error::Format(e) => assert!(e.to_string().contains("too long"), "{e}"),
		other => panic!("expected Format naming the length, got {other:?}"),
	}

	// -- Check: the failed create left nothing behind, not even the truncated
	// name, which is what a partial insert would have left.
	assert!(store.read_dir("/")?.is_empty());
	Ok(())
}

#[test]
fn test_every_verb_refuses_an_over_long_name_the_same_way() -> Result<()> {
	// -- Setup & Fixtures: the length check lives in the format layer, so every
	// verb that writes a directory entry inherits it. What matters is that none
	// of them panics and none of them stores something shorter.
	let mut store = store(BLOCKS)?;
	let long = "z".repeat(300);

	// -- Exec & Check
	for (what, r) in [
		("mkdir", store.mkdir(&format!("/{long}"), 0o755)),
		("create_file", store.create_file(&format!("/{long}"), 0o644)),
		("write_new_file", store.write_new_file(&format!("/{long}"), b"x", 0o644)),
		("append", store.append(&format!("/{long}"), b"x", 0o644)),
	] {
		let e = expect_err(what, r);
		assert!(matches!(e, Error::Format(_)), "{what}: expected Format, got {e:?}");
	}

	// -- Check
	assert!(store.read_dir("/")?.is_empty());
	Ok(())
}

#[test]
fn test_a_multi_byte_name_is_measured_in_bytes_not_characters() -> Result<()> {
	// -- Setup & Fixtures: 128 * "日" is 384 UTF-8 bytes, over the limit, even
	// though it is only 128 characters, so a byte-length check is what has to
	// decide this.
	let mut store = store(BLOCKS)?;

	// -- Exec
	let long = "日".repeat(128);
	let r = store.write_new_file(&format!("/{long}"), b"x", 0o644);

	// -- Check
	assert!(r.is_err(), "384 bytes exceeds the 255-byte name limit");

	// -- Exec: the same in characters that fit
	let fits = "日".repeat(85); // 255 bytes
	store.write_new_file(&format!("/{fits}"), b"fits", 0o644)?;

	// -- Check
	assert_eq!(names(&store.read_dir("/")?), vec![fits.clone()]);
	assert_eq!(store.read_file(&format!("/{fits}"))?, b"fits");
	Ok(())
}

#[test]
fn test_a_name_with_an_embedded_nul_is_stored_verbatim() -> Result<()> {
	// -- Setup & Fixtures: there is no C string layer anywhere in this path;
	// names are Rust `String`s in a serialised `DirNode`, so a NUL is just a
	// byte. What matters is that it is neither dropped nor a truncation point.
	let mut store = store(BLOCKS)?;
	let name = "before\0after";

	// -- Exec
	store.write_new_file(&format!("/{name}"), b"nul", 0o644)?;

	// -- Check
	let listed = names(&store.read_dir("/")?);
	assert_eq!(listed, vec![name.to_string()]);
	assert_eq!(listed[0].len(), name.len(), "the NUL is still there");
	assert_eq!(store.read_file(&format!("/{name}"))?, b"nul");
	Ok(())
}

#[test]
fn test_a_file_and_a_directory_name_do_not_collide() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/target", 0o755)?;

	// -- Exec: the name is taken by a directory, so a file cannot have it too
	let e = expect_err(
		"file over a directory name",
		store.write_new_file("/target", b"x", 0o644),
	);

	// -- Check
	assert!(matches!(e, Error::AlreadyExists { .. }), "got {e:?}");
	assert_eq!(
		listing(store.fs(), "/")?,
		vec![("target".to_string(), InodeKind::Directory)]
	);
	Ok(())
}

// endregion:    --- Names

// region:    --- HostFs directly

#[test]
fn test_a_read_only_mount_reads_everything_a_writable_one_does() -> Result<()> {
	// -- Setup & Fixtures: the read path is shared, so a volume with no
	// allocator snapshot must be indistinguishable from a healthy one *for
	// reads*. This is what lets `lfs-read` inspect a volume a writer holds.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format(Box::new(device), BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/f", &vec![9u8; 10_000], 0o644)?;
	store.append("/log", b"line\n", 0o644)?;
	drop(store);

	let mut damaged = bytes(&watch);
	damaged[block_offset(5)..block_offset(6)].fill(0);

	// -- Exec
	let ro: HostFs = mount_bytes(damaged.clone())?;
	let rw = open_bytes(damaged)?;

	// -- Check
	assert_eq!(ro.read_file("/d/f")?, rw.read_file("/d/f")?);
	assert_eq!(ro.read_at("/d/f", 5000, 100)?, rw.read_at("/d/f", 5000, 100)?);
	assert_eq!(listing(&ro, "/")?, listing(rw.fs(), "/")?);
	assert_eq!(ro.stat("/log")?.size, 5);
	Ok(())
}

#[test]
fn test_hostfs_open_refuses_a_read_only_image_for_a_writable_store() -> Result<()> {
	// -- Setup & Fixtures: `Store::open_partition` opens read-write, so an
	// image file the process may not write must fail at open rather than at the
	// first commit, and it must say which file.
	let image = TempImage::new("ro-store-open");
	let (_, watch) = watched(BLOCKS)?;
	image.write(&bytes(&watch))?;
	let mut perms = std::fs::metadata(&image.path)?.permissions();
	perms.set_readonly(true);
	std::fs::set_permissions(&image.path, perms)?;

	// -- Exec
	let r = Store::open_partition(&image.path, ledgerfs_host::PartitionKind::Data);

	// -- Check: this image has no partition table at all, so the *first*
	// refusal is the geometry one, which is the right order: a caller is told
	// the image is not a partitioned image before being told anything about
	// permissions.
	let e = expect_err("open_partition a bare volume file", r);
	assert!(matches!(e, Error::NoPartitionTable { .. }), "got {e:?}");

	// -- Teardown: make the file writable again so `TempImage`'s Drop can unlink
	// it (on Linux unlinking needs write permission on the directory, not the
	// file, but a read-only file in a sticky /tmp is worth restoring anyway).
	// The restore is spelled per platform: `PermissionsExt` restores exactly
	// the owner write bit on Unix, and the read-only attribute is the spelling
	// Windows has. `set_readonly(false)` alone is not used on Unix because it
	// sets the group and other write bits too.
	let mut perms = std::fs::metadata(&image.path)?.permissions();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		perms.set_mode(perms.mode() | 0o200);
	}
	#[cfg(windows)]
	perms.set_readonly(false);
	std::fs::set_permissions(&image.path, perms)?;
	Ok(())
}

// endregion:    --- HostFs directly
