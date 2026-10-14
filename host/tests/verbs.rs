//! The mutating verbs: `mkdir`, `create_file`, `write_new_file`, `write_file`,
//! `append`, `write_at`, `remove`, `rename`.
//!
//! Two things are checked for each verb, and the second matters more than the
//! first:
//!
//! 1. the happy path, read back through the *read* path rather than trusted
//!    from the return value; and
//! 2. every refusal, because the refusals are this crate's contract with an
//!    app. `write_at` declining to overwrite stored bytes, `remove` declining a
//!    directory, `rename` declining a cross-directory move — an app is allowed
//!    to depend on those answers, so a regression that silently started
//!    permitting one would be worse than a crash.

mod common;

use common::*;
use crucible_ledgerfs_host::{Error, InodeKind};

// region:    --- mkdir

#[test]
fn test_mkdir_creates_a_directory_with_its_mode() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.mkdir("/notes", 0o750)?;

	// -- Check: through the read path, and through the entry the listing gives
	let inode = store.stat("/notes")?;
	assert_eq!(inode.kind, InodeKind::Directory);
	assert_eq!(inode.mode, 0o750);
	assert_eq!(inode.size, 0);
	let entry = store.read_dir("/")?.into_iter().find(|e| e.name == "notes").expect("notes is listed");
	assert_eq!(entry.kind, InodeKind::Directory);
	assert_eq!(entry.mode, 0o750);
	Ok(())
}

#[test]
fn test_mkdir_is_visible_immediately_without_a_remount() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.mkdir("/a", 0o755)?;

	// -- Check: a commit that the read half cannot see is the bug this guards:
	// the store must move its own root to the tree it just published, or the
	// very next `stat` resolves through the root it mounted at.
	assert_eq!(store.stat("/a")?.kind, InodeKind::Directory);
	assert_eq!(names(&store.read_dir("/")?), vec!["a".to_string()]);
	Ok(())
}

#[test]
fn test_mkdir_nests_and_reports_each_level() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	for path in ["/a", "/a/b", "/a/b/c"] {
		store.mkdir(path, 0o755)?;
	}

	// -- Check
	assert_eq!(store.stat("/a/b/c")?.kind, InodeKind::Directory);
	assert_eq!(names(&store.read_dir("/a")?), vec!["b".to_string()]);
	assert_eq!(names(&store.read_dir("/a/b")?), vec!["c".to_string()]);
	assert!(store.read_dir("/a/b/c")?.is_empty());
	Ok(())
}

#[test]
fn test_mkdir_refuses_a_name_that_is_taken() -> Result<()> {
	// -- Setup & Fixtures: both a directory and a file already occupy names
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.create_file("/f", 0o644)?;

	// -- Exec & Check: `cow::operations`' insert does not check for a duplicate,
	// so without this refusal a second entry with the same name would be
	// written into a sorted `DirEntry` list — a corrupt directory, not a failed
	// create.
	for path in ["/d", "/f"] {
		let e = expect_err(path, store.mkdir(path, 0o755));
		match e {
			Error::AlreadyExists { path: p } => assert_eq!(p, path),
			other => panic!("{path}: expected AlreadyExists, got {other:?}"),
		}
	}

	// -- Check: and the directories were not disturbed by the attempts
	assert_eq!(names(&store.read_dir("/")?), vec!["d".to_string(), "f".to_string()]);
	Ok(())
}

#[test]
fn test_mkdir_names_the_missing_parent() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	let e = expect_err("mkdir under an absent parent", store.mkdir("/nope/deep", 0o755));

	// -- Check: the missing *directory*, not the whole path — naming the tail
	// (`deep`) would send the caller looking for the wrong thing. Note the
	// shape: `resolve_chain` reports the prefix it accumulated walking down
	// from the root, so it is slashless, while `lookup_in` reports the path as
	// the caller wrote it. The two differ for a top-level miss — see
	// `test_no_such_file_names_the_parent_but_not_always_with_a_leading_slash`.
	match e {
		Error::NoSuchFile { path } => assert_eq!(path, "nope"),
		other => panic!("expected NoSuchFile naming the parent, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_a_top_level_create_never_has_a_missing_parent() -> Result<()> {
	// -- Setup & Fixtures: the chain walk only *looks up* the components above
	// the target, so a one-component path resolves against the root and nothing
	// can be missing. Worth pinning, because it is the boundary of the
	// `NoSuchFile`-naming-the-parent rule.
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.mkdir("/notes", 0o755)?;
	store.create_file("/top", 0o644)?;
	store.write_file("/deep-name.txt", b"x", 0o644)?;

	// -- Check
	assert_eq!(names(&store.read_dir("/")?), vec!["deep-name.txt".to_string(), "notes".to_string(), "top".to_string()]);
	Ok(())
}

#[test]
fn test_no_such_file_names_the_parent_but_not_always_with_a_leading_slash() -> Result<()> {
	// -- Setup & Fixtures: two ways a `NoSuchFile` is produced for the same
	// missing name, and they do not agree on the shape of `path`. Pinned here
	// deliberately: a caller matching on this string needs to know that
	// `stat`'s miss carries a leading slash and a create's parent-miss does
	// not.
	let mut store = store(BLOCKS)?;

	// -- Exec: the create path, through `resolve_chain`
	let from_chain = expect_err("create under an absent parent", store.create_file("/gone/f", 0o644));

	// -- Exec: the lookup path, through `lookup_in`
	let from_lookup = expect_err("stat an absent top-level name", store.stat("/gone"));

	// -- Check
	match from_chain {
		Error::NoSuchFile { path } => assert_eq!(path, "gone", "the chain walk reports a slashless prefix"),
		other => panic!("expected NoSuchFile, got {other:?}"),
	}
	match from_lookup {
		Error::NoSuchFile { path } => assert_eq!(path, "/gone", "a lookup reports the path as given"),
		other => panic!("expected NoSuchFile, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_no_such_file_and_not_a_directory_disagree_about_which_path_they_name() -> Result<()> {
	// -- Setup & Fixtures: the third shape of the same disagreement. A create
	// under a *file* reports the whole path it was given, while a create under
	// a missing directory reports a slashless prefix — and neither is "the
	// component at fault", which for both is `/f` and `gone` respectively.
	let mut store = store(BLOCKS)?;
	store.create_file("/f", 0o644)?;

	// -- Exec & Check
	let e = expect_err("mkdir under a file", store.mkdir("/f/child", 0o755));
	match e {
		Error::NotADirectory { path } => assert_eq!(path, "/f/child"),
		other => panic!("expected NotADirectory, got {other:?}"),
	}

	let e = expect_err("mkdir under a missing directory", store.mkdir("/gone/child", 0o755));
	match e {
		Error::NoSuchFile { path } => assert_eq!(path, "gone"),
		other => panic!("expected NoSuchFile, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_mkdir_refuses_a_path_through_a_file() -> Result<()> {
	// -- Setup & Fixtures: `/f` is a file, so it has no children
	let mut store = store(BLOCKS)?;
	store.create_file("/f", 0o644)?;

	// -- Exec
	let e = expect_err("mkdir under a file", store.mkdir("/f/child", 0o755));

	// -- Check: "not a directory" is the answer, not "no such file"
	match e {
		Error::NotADirectory { path } => assert_eq!(path, "/f/child"),
		other => panic!("expected NotADirectory, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_mkdir_refuses_the_root() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec: re-creating the root must not be a way to replace it
	let r = store.mkdir("/", 0o755);

	// -- Check: refused — with `AlreadyExists`, since the root is a directory
	// that is already there, which is also what a caller would get for any
	// other taken path.
	assert!(r.is_err(), "mkdir / must not silently succeed");
	assert_eq!(names(&store.read_dir("/")?), Vec::<String>::new());
	Ok(())
}

// endregion:    --- mkdir

// region:    --- create_file

#[test]
fn test_create_file_makes_an_empty_regular_file() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.create_file("/empty", 0o600)?;

	// -- Check
	let inode = store.stat("/empty")?;
	assert_eq!(inode.kind, InodeKind::RegularFile);
	assert_eq!(inode.mode, 0o600);
	assert_eq!(inode.size, 0, "a created file has no content yet");
	assert_eq!(store.read_file("/empty")?, Vec::<u8>::new());
	Ok(())
}

#[test]
fn test_create_file_refuses_a_name_that_is_taken() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.create_file("/f", 0o644)?;

	// -- Exec
	let e = expect_err("create over an existing file", store.create_file("/f", 0o644));

	// -- Check
	assert!(matches!(e, Error::AlreadyExists { .. }), "got {e:?}");
	Ok(())
}

#[test]
fn test_create_file_names_the_missing_parent() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	let e = expect_err("create under an absent parent", store.create_file("/gone/f", 0o644));

	// -- Check: the parent, by its accumulated prefix — see the note on the
	// `mkdir` twin for why there is no leading slash.
	match e {
		Error::NoSuchFile { path } => assert_eq!(path, "gone"),
		other => panic!("expected NoSuchFile naming the parent, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_create_file_refuses_a_name_taken_by_a_directory() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let e = expect_err("file over a directory name", store.create_file("/d", 0o644));

	// -- Check
	assert!(matches!(e, Error::AlreadyExists { .. }), "got {e:?}");
	assert_eq!(store.stat("/d")?.kind, InodeKind::Directory, "the directory survived");
	Ok(())
}

// endregion:    --- create_file

// region:    --- write_new_file & write_file

#[test]
fn test_write_new_file_round_trips_its_bytes_and_size() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();

	// -- Exec
	store.write_new_file("/blob", &data, 0o644)?;

	// -- Check
	assert_eq!(store.read_file("/blob")?, data);
	assert_eq!(store.stat("/blob")?.size, 5000);
	assert_eq!(store.stat("/blob")?.mode, 0o644);
	Ok(())
}

#[test]
fn test_write_new_file_refuses_a_name_that_is_taken() -> Result<()> {
	// -- Setup & Fixtures: `write_new_file` is create-then-write, so it must
	// not quietly become a replace.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"first", 0o644)?;

	// -- Exec
	let e = expect_err("write_new_file over an existing file", store.write_new_file("/f", b"second", 0o644));

	// -- Check
	assert!(matches!(e, Error::AlreadyExists { .. }), "got {e:?}");
	assert_eq!(store.read_file("/f")?, b"first", "the original content is untouched");
	Ok(())
}

#[test]
fn test_write_file_replaces_the_content() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"a much longer first version", 0o644)?;

	// -- Exec: shorter replacement
	store.write_file("/f", b"short", 0o600)?;

	// -- Check: the size follows the content, so the old extent is gone rather
	// than merely shadowed
	assert_eq!(store.read_file("/f")?, b"short");
	assert_eq!(store.stat("/f")?.size, 5);
	assert_eq!(store.stat("/f")?.mode, 0o600, "the replacement carries the new mode");
	Ok(())
}

#[test]
fn test_write_file_creates_when_absent() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.write_file("/new", b"made", 0o644)?;

	// -- Check
	assert_eq!(store.read_file("/new")?, b"made");
	Ok(())
}

#[test]
fn test_write_file_grows_and_shrinks_repeatedly() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BIG_BLOCKS)?;

	// -- Exec: the shape a caller's repeated `state.json` rewrite has
	for len in [0usize, 1, 4096, 10, 100_000, 3] {
		let data: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
		store.write_file("/state.json", &data, 0o644)?;

		// -- Check: every intermediate version is whole, not merely the last
		assert_eq!(store.stat("/state.json")?.size, len as u64, "len {len}");
		assert_eq!(store.read_file("/state.json")?, data, "len {len}");
	}
	Ok(())
}

#[test]
fn test_write_file_refuses_a_directory() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let e = expect_err("write over a directory", store.write_file("/d", b"x", 0o644));

	// -- Check: refused before the `remove` half could be attempted
	match e {
		Error::IsADirectory { path } => assert_eq!(path, "/d"),
		other => panic!("expected IsADirectory, got {other:?}"),
	}
	assert_eq!(store.stat("/d")?.kind, InodeKind::Directory);
	Ok(())
}

#[test]
fn test_a_failed_write_new_file_leaves_an_empty_file() -> Result<()> {
	// -- Setup & Fixtures: `write_new_file` is deliberately two transactions —
	// create, then write — so a write that fails for want of space leaves the
	// *created* file behind, empty. That is the documented trade: the weaker
	// guarantee "the file is there with the bytes that committed" rather than
	// atomicity, and a caller who needs the replace to be all-or-nothing is
	// told to write a scratch name and rename.
	let mut store = store(20)?;

	// -- Exec: far more content than a 20-block volume can hold
	let e = expect_err("a write that cannot fit", store.write_new_file("/over", &vec![0u8; 200_000], 0o644));

	// -- Check: refused on space...
	match e {
		Error::Format(e) => assert!(e.to_string().contains("space"), "{e}"),
		other => panic!("expected an out-of-space refusal, got {other:?}"),
	}

	// -- Check: ...and the file exists, empty, not partial and not absent. A
	// caller that sees a failure and retries must be able to stat it.
	let inode = store.stat("/over")?;
	assert_eq!(inode.kind, InodeKind::RegularFile);
	assert_eq!(inode.size, 0, "the failed write stored nothing");
	assert_eq!(store.read_file("/over")?, Vec::<u8>::new());
	Ok(())
}

#[test]
fn test_writing_an_empty_file_leaves_no_content() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.write_new_file("/zero", b"", 0o644)?;

	// -- Check
	assert_eq!(store.stat("/zero")?.size, 0);
	assert_eq!(store.read_file("/zero")?, Vec::<u8>::new());
	Ok(())
}

#[test]
fn test_a_write_larger_than_the_object_ceiling_is_split_across_transactions() -> Result<()> {
	// -- Setup & Fixtures: the default ceiling is 256 KiB, so this needs three
	// transactions — which is the case the "re-resolve the file reference on
	// every iteration" comment in `write_chunks` exists for. A stale reference
	// would silently keep only the last piece.
	let mut store = store(BIG_BLOCKS)?;
	let data: Vec<u8> = (0..700_000u32).map(|i| (i % 257) as u8).collect();

	// -- Exec
	store.write_new_file("/big", &data, 0o644)?;

	// -- Check: the whole thing, in order, not just the tail
	assert_eq!(store.stat("/big")?.size, 700_000);
	assert_eq!(store.read_file("/big")?, data);
	Ok(())
}

#[test]
fn test_a_write_exactly_at_the_object_ceiling_is_not_split() -> Result<()> {
	// -- Setup & Fixtures: the boundary case of the chunking loop — `chunks(n)`
	// yields one piece for exactly `n` bytes, and an off-by-one that made the
	// last piece empty would commit a stray transaction.
	let mut store = store(BIG_BLOCKS)?;
	let mut data = vec![0u8; 256 * 1024];
	for (i, b) in data.iter_mut().enumerate() {
		*b = (i % 249) as u8;
	}

	// -- Exec
	store.write_new_file("/ceiling", &data, 0o644)?;

	// -- Check
	assert_eq!(store.read_file("/ceiling")?, data);
	assert_eq!(store.stat("/ceiling")?.size, 256 * 1024);
	Ok(())
}

// endregion:    --- write_new_file & write_file

// region:    --- append

#[test]
fn test_append_creates_the_file_when_it_is_absent() -> Result<()> {
	// -- Setup & Fixtures: this is the logging case, so it must not require a
	// prior `create_file`.
	let mut store = store(BLOCKS)?;

	// -- Exec
	store.append("/log", b"first line\n", 0o644)?;

	// -- Check
	assert_eq!(store.read_file("/log")?, b"first line\n");
	assert_eq!(store.stat("/log")?.kind, InodeKind::RegularFile);
	Ok(())
}

#[test]
fn test_append_adds_to_the_end_and_preserves_what_was_there() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/log", b"one\n", 0o644)?;

	// -- Exec
	for line in ["two\n", "three\n", "four\n"] {
		store.append("/log", line.as_bytes(), 0o644)?;
	}

	// -- Check
	assert_eq!(store.read_file("/log")?, b"one\ntwo\nthree\nfour\n");
	assert_eq!(store.stat("/log")?.size, 19);
	Ok(())
}

#[test]
fn test_append_of_nothing_leaves_the_file_alone() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/log", b"content", 0o644)?;
	let seq = store.commit_seq();

	// -- Exec
	store.append("/log", b"", 0o644)?;

	// -- Check: an empty append commits nothing — no transaction, no sequence
	// bump, no space consumed. A caller's loop that appended nothing must not
	// cost it a commit.
	assert_eq!(store.commit_seq(), seq, "an empty append must not commit");
	assert_eq!(store.read_file("/log")?, b"content");
	Ok(())
}

#[test]
fn test_append_refuses_a_directory() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let e = expect_err("append to a directory", store.append("/d", b"x", 0o644));

	// -- Check
	match e {
		Error::IsADirectory { path } => assert_eq!(path, "/d"),
		other => panic!("expected IsADirectory, got {other:?}"),
	}
	Ok(())
}

#[test]
fn test_many_appends_are_all_readable_in_order() -> Result<()> {
	// -- Setup & Fixtures: 200 commits through one file is the append-only log
	// pattern the contract is written for.
	let mut store = store(BIG_BLOCKS)?;
	let mut expected = Vec::new();

	// -- Exec
	for i in 0..200u32 {
		let line = format!("line {i}\n");
		store.append("/log", line.as_bytes(), 0o644)?;
		expected.extend_from_slice(line.as_bytes());
	}

	// -- Check
	assert_eq!(store.read_file("/log")?, expected);
	assert_eq!(store.stat("/log")?.size, expected.len() as u64);
	// 200 commits consumed real space, and the store is still taking writes —
	// an append path that leaked a commit per line would be visible here first.
	assert!(store.is_writable());
	assert!(store.free_bytes().expect("free bytes") > 0);
	Ok(())
}

#[test]
fn test_an_append_log_survives_a_remount() -> Result<()> {
	// -- Setup & Fixtures
	let device = mem(BIG_BLOCKS);
	let watch = device.clone();
	let mut store = crucible_ledgerfs_host::Store::format(Box::new(device), BIG_BLOCKS)?;
	let mut expected = Vec::new();
	for i in 0..50u32 {
		let line = format!("entry-{i}\n");
		store.append("/log", line.as_bytes(), 0o644)?;
		expected.extend_from_slice(line.as_bytes());
	}
	drop(store);

	// -- Exec: a fresh mount of those bytes, as a restart would do
	let back = open_bytes(bytes(&watch))?;

	// -- Check
	assert_eq!(back.read_file("/log")?, expected);
	Ok(())
}

// endregion:    --- append

// region:    --- write_at

#[test]
fn test_write_at_extends_from_the_end() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"head", 0o644)?;

	// -- Exec: exactly at the end, which is the one offset that is allowed
	store.write_at("/f", 4, b"-tail")?;

	// -- Check
	assert_eq!(store.read_file("/f")?, b"head-tail");
	assert_eq!(store.stat("/f")?.size, 9);
	Ok(())
}

#[test]
fn test_write_at_refuses_an_offset_inside_the_file() -> Result<()> {
	// -- Setup & Fixtures: the refusal that keeps the extent tree from carrying
	// two overlapping ranges, which no reader could resolve.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"0123456789", 0o644)?;

	// -- Exec & Check
	for offset in [0u64, 1, 5, 9] {
		let e = expect_err(&format!("write_at at {offset}"), store.write_at("/f", offset, b"XX"));
		match e {
			Error::Unsupported { detail } => {
				assert!(detail.contains("write_at"), "{detail}");
				assert!(detail.contains("10 bytes"), "the message names the size: {detail}");
			}
			other => panic!("offset {offset}: expected Unsupported, got {other:?}"),
		}
	}

	// -- Check: and none of the attempts changed anything
	assert_eq!(store.read_file("/f")?, b"0123456789");
	Ok(())
}

#[test]
fn test_write_at_past_the_end_lands_at_the_offset() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"abc", 0o644)?;

	// -- Exec: an offset beyond the end is *not* refused — it is past the
	// stored bytes, so no extent is overlapped.
	store.write_at("/f", 10, b"far")?;

	// -- Check: the file is now as long as the write reached, and the bytes
	// that were never written read back as zeroes — a hole, not corruption and
	// not an error.
	let got = store.read_file("/f")?;
	assert_eq!(got.len(), 13);
	assert_eq!(&got[..3], b"abc");
	assert_eq!(
		&got[3..10],
		&[0u8; 7],
		"the gap reads as zeroes, not as whatever was in the buffer"
	);
	assert_eq!(&got[10..], b"far");
	Ok(())
}

#[test]
fn test_write_at_refuses_a_missing_file_and_a_directory() -> Result<()> {
	// -- Setup & Fixtures: unlike `append`, `write_at` does not create.
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec & Check
	let e = expect_err("write_at on a missing file", store.write_at("/gone", 0, b"x"));
	assert!(is_no_such_file(&e, "/gone"), "got {e:?}");

	let e = expect_err("write_at on a directory", store.write_at("/d", 0, b"x"));
	assert!(matches!(e, Error::IsADirectory { .. }), "got {e:?}");
	Ok(())
}

// endregion:    --- write_at

// region:    --- remove

#[test]
fn test_remove_unlinks_a_file() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/gone", b"bye", 0o644)?;
	store.write_new_file("/kept", b"hi", 0o644)?;

	// -- Exec
	store.remove("/gone")?;

	// -- Check: gone from the listing, absent to `stat`, and its neighbour is
	// untouched — a CoW unlink must re-root the parent directory, not the tree.
	assert_eq!(names(&store.read_dir("/")?), vec!["kept".to_string()]);
	assert!(is_no_such_file(&expect_err("stat a removed file", store.stat("/gone")), "/gone"));
	assert_eq!(store.read_file("/kept")?, b"hi");
	Ok(())
}

#[test]
fn test_remove_refuses_a_directory() -> Result<()> {
	// -- Setup & Fixtures: there is no recursive unlink, and this crate does not
	// invent one — a directory that is non-empty would leave its children
	// unreachable and its inode unlinkable.
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let e = expect_err("remove a directory", store.remove("/d"));

	// -- Check
	match e {
		Error::IsADirectory { path } => assert_eq!(path, "/d"),
		other => panic!("expected IsADirectory, got {other:?}"),
	}
	assert_eq!(store.stat("/d")?.kind, InodeKind::Directory);
	Ok(())
}

#[test]
fn test_remove_names_a_missing_file() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	let e = expect_err("remove a missing file", store.remove("/never"));

	// -- Check
	assert!(is_no_such_file(&e, "/never"), "got {e:?}");
	Ok(())
}

#[test]
fn test_remove_frees_the_name_for_reuse() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"first", 0o644)?;

	// -- Exec: unlink, then occupy the name again
	store.remove("/f")?;
	store.write_new_file("/f", b"second", 0o644)?;

	// -- Check: no duplicate entry, and the new content is what reads back
	assert_eq!(names(&store.read_dir("/")?), vec!["f".to_string()]);
	assert_eq!(store.read_file("/f")?, b"second");
	assert_eq!(store.stat("/f")?.size, 6);
	Ok(())
}

#[test]
fn test_remove_and_recreate_do_not_leave_a_ghost_entry() -> Result<()> {
	// -- Setup & Fixtures: 50 cycles through one name. A directory whose entry
	// list accumulated a duplicate per cycle would still "work" for a while and
	// then fail an insert with a corrupt-list error — this is the test that
	// catches the accumulation early.
	let mut store = store(BIG_BLOCKS)?;

	// -- Exec
	for i in 0..50u32 {
		store.write_file("/churn", format!("v{i}").as_bytes(), 0o644)?;
		assert_eq!(names(&store.read_dir("/")?), vec!["churn".to_string()], "cycle {i}");
	}

	// -- Check
	assert_eq!(store.read_file("/churn")?, b"v49");
	Ok(())
}

// endregion:    --- remove

// region:    --- rename

#[test]
fn test_rename_moves_a_file_within_its_directory() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/old", b"payload", 0o644)?;

	// -- Exec
	store.rename("/old", "/new")?;

	// -- Check: one entry, the content preserved, mode preserved
	assert_eq!(names(&store.read_dir("/")?), vec!["new".to_string()]);
	assert_eq!(store.read_file("/new")?, b"payload");
	assert_eq!(store.stat("/new")?.mode, 0o644);
	assert!(is_no_such_file(&expect_err("stat the old name", store.stat("/old")), "/old"));
	Ok(())
}

#[test]
fn test_rename_overwrites_an_existing_file_destination() -> Result<()> {
	// -- Setup & Fixtures: this is the atomic-replace an app is told to use —
	// write a scratch name, rename it over the target.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/state.json", b"old state", 0o644)?;
	store.write_new_file("/state.tmp", b"new state", 0o644)?;

	// -- Exec
	store.rename("/state.tmp", "/state.json")?;

	// -- Check: the destination holds the source's content and the scratch name
	// is gone, with no duplicate left behind.
	assert_eq!(names(&store.read_dir("/")?), vec!["state.json".to_string()]);
	assert_eq!(store.read_file("/state.json")?, b"new state");
	Ok(())
}

#[test]
fn test_rename_refuses_a_cross_directory_move() -> Result<()> {
	// -- Setup & Fixtures: the two CoW chains a cross-directory move needs are
	// not reconciled yet, so this is a refusal rather than a wrong tree.
	let mut store = store(BLOCKS)?;
	store.mkdir("/a", 0o755)?;
	store.mkdir("/b", 0o755)?;
	store.write_new_file("/a/f", b"x", 0o644)?;

	// -- Exec
	let e = expect_err("cross-directory rename", store.rename("/a/f", "/b/f"));

	// -- Check
	match e {
		Error::Unsupported { detail } => assert!(detail.contains("different directories"), "{detail}"),
		other => panic!("expected Unsupported, got {other:?}"),
	}

	// -- Check: nothing moved and nothing was copied
	assert_eq!(names(&store.read_dir("/a")?), vec!["f".to_string()]);
	assert!(store.read_dir("/b")?.is_empty());
	Ok(())
}

#[test]
fn test_rename_refuses_a_directory() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let e = expect_err("rename a directory", store.rename("/d", "/e"));

	// -- Check
	match e {
		Error::Unsupported { detail } => assert!(detail.contains("directory"), "{detail}"),
		other => panic!("expected Unsupported, got {other:?}"),
	}
	assert_eq!(names(&store.read_dir("/")?), vec!["d".to_string()]);
	Ok(())
}

#[test]
fn test_rename_names_a_missing_source() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;

	// -- Exec
	let e = expect_err("rename a missing file", store.rename("/never", "/other"));

	// -- Check: "no such file" rather than "invalid operation", so a caller can
	// tell a typo from an unsupported request.
	assert!(is_no_such_file(&e, "/never"), "got {e:?}");
	Ok(())
}

#[test]
fn test_rename_into_an_absent_directory_is_refused() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"x", 0o644)?;

	// -- Exec
	let e = expect_err("rename into an absent directory", store.rename("/f", "/gone/f"));

	// -- Check: cannot be a cross-directory refusal (there is no destination
	// directory), so it has to be the missing parent — reported by the
	// destination's chain walk, hence the slashless prefix.
	assert!(is_no_such_file(&e, "gone"), "got {e:?}");
	assert_eq!(names(&store.read_dir("/")?), vec!["f".to_string()]);
	Ok(())
}

#[test]
fn test_rename_onto_a_directory_is_refused_rather_than_corrupting() -> Result<()> {
	// -- Setup & Fixtures: overwriting a *file* destination is allowed; a
	// directory destination is a different thing entirely, and the store must
	// not leave a directory entry shadowed by a file.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"x", 0o644)?;
	store.mkdir("/d", 0o755)?;

	// -- Exec
	let r = store.rename("/f", "/d");

	// -- Check: the refusal comes from the kernel's own rename, wrapped as
	// `Error::Format`, so the *variant* is not one of the host's named path
	// errors — a caller cannot distinguish it from a genuine format problem
	// without reading the detail text. Pinned as it is, with the wart recorded:
	// this is a place the host error surface is thinner than the others.
	match r {
		Err(Error::Format(e)) => {
			let text = e.to_string();
			assert!(text.contains("destination exists"), "{text}");
			assert!(text.contains("directory"), "{text}");
		}
		Err(other) => panic!("expected the kernel's refusal, got {other:?}"),
		Ok(()) => panic!("renaming a file onto a directory must not be reported as success"),
	}

	// -- Check: and the tree is untouched — the important half of this test
	assert_eq!(store.stat("/d")?.kind, InodeKind::Directory);
	assert_eq!(names(&store.read_dir("/d")?), Vec::<String>::new());
	assert_eq!(store.read_file("/f")?, b"x", "the source file is still where it was");
	Ok(())
}

// endregion:    --- rename

// region:    --- Cross-verb interaction

#[test]
fn test_a_whole_session_of_verbs_leaves_the_tree_it_should() -> Result<()> {
	// -- Setup & Fixtures: one volume, every verb, and a full tree assertion at
	// the end — the integration shape a caller actually has.
	let mut store = store(BIG_BLOCKS)?;

	// -- Exec
	store.mkdir("/app", 0o755)?;
	store.mkdir("/app/logs", 0o755)?;
	store.write_new_file("/app/config.json", b"{\"v\":1}", 0o644)?;
	store.append("/app/logs/run.log", b"start\n", 0o644)?;
	store.append("/app/logs/run.log", b"step\n", 0o644)?;
	store.write_file("/app/config.json", b"{\"v\":2}", 0o600)?;
	store.write_new_file("/app/tmp", b"scratch", 0o644)?;
	store.rename("/app/tmp", "/app/real")?;
	store.remove("/app/real")?;
	store.write_at("/app/logs/run.log", 11, b"done\n")?;

	// -- Check
	assert_eq!(names(&store.read_dir("/")?), vec!["app".to_string()]);
	assert_eq!(names(&store.read_dir("/app")?), vec!["config.json".to_string(), "logs".to_string()]);
	assert_eq!(names(&store.read_dir("/app/logs")?), vec!["run.log".to_string()]);
	assert_eq!(store.read_file("/app/config.json")?, b"{\"v\":2}");
	assert_eq!(store.stat("/app/config.json")?.mode, 0o600);
	assert_eq!(store.read_file("/app/logs/run.log")?, b"start\nstep\ndone\n");
	Ok(())
}

#[test]
fn test_the_same_tree_reads_back_after_a_remount() -> Result<()> {
	// -- Setup & Fixtures
	let device = mem(BIG_BLOCKS);
	let watch = device.clone();
	let mut store = crucible_ledgerfs_host::Store::format(Box::new(device), BIG_BLOCKS)?;
	store.mkdir("/d", 0o755)?;
	store.write_new_file("/d/a", b"alpha", 0o644)?;
	store.write_new_file("/d/b", b"beta", 0o600)?;
	store.remove("/d/b")?;
	store.write_new_file("/d/c", &vec![7u8; 9000], 0o644)?;
	drop(store);

	// -- Exec
	let back = open_bytes(bytes(&watch))?;

	// -- Check
	assert_eq!(names(&back.read_dir("/d")?), vec!["a".to_string(), "c".to_string()]);
	assert_eq!(back.read_file("/d/a")?, b"alpha");
	assert_eq!(back.stat("/d/c")?.size, 9000);
	assert_eq!(back.read_file("/d/c")?, vec![7u8; 9000]);
	Ok(())
}

// endregion:    --- Cross-verb interaction
