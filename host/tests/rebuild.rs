//! The rebuild path: `Store::walk` and `rebuild_into`.
//!
//! CoW frees nothing, so a volume that has been written for a while is mostly
//! unreachable garbage — the live tree may be a thousandth of the device. The
//! only way to get that space back is to write the live tree into an empty
//! volume and swap, which is what this pair of functions is for. That makes
//! three things load-bearing and worth pinning hard:
//!
//! 1. **`walk` is a faithful, deterministic description of the live tree.**
//!    `rebuild_into` is only as good as its input, and `bugs`'s `compact` uses
//!    a second `walk` as its verification — so a walk that quietly dropped an
//!    entry, or produced the same entry twice, or reordered between runs, would
//!    both lose data and make the verification a lie.
//! 2. **A rebuilt volume is a real volume.** Not "the bytes went somewhere" but
//!    a volume that mounts from a fresh byte buffer and walks to exactly the
//!    same tree. That is the property the swap depends on.
//! 3. **Rebuild reclaims space.** If it does not, the whole module is pointless.
//!
//! The functional half lives here. What a rebuild *refuses* — following a
//! symlink, and the depth of a hostile tree — is pinned at the end, and the
//! crafted-metadata half lives in `security_crafted.rs`.

mod common;

use common::*;
use crucible_ledgerfs_host::{Error, InodeKind, MemBlockDevice, Store, rebuild_into};

use crucible_ledgerfs_kernel::compression::CompressionAlgorithm;
use crucible_ledgerfs_kernel::object::{DirEntry, DirNode, Inode, ObjectId, ObjectRef, StorageHint};
use crucible_ledgerfs_kernel::record::{ObjectRecordWriter, ObjectType};
use crucible_ledgerfs_kernel::superblock::Superblock;
use crucible_ledgerfs_kernel::{BLOCK_SIZE, RESERVED_HEADER_BLOCKS};

const BS: usize = BLOCK_SIZE as usize;

// region:    --- Support

/// Deterministic non-repeating bytes, so a large file spans many distinct
/// chunks rather than one content-addressed chunk the store would dedup.
fn noisy(len: usize, seed: u64) -> Vec<u8> {
	let mut rng = Rng::new(seed);
	let mut out = Vec::with_capacity(len);
	while out.len() < len {
		out.extend_from_slice(&rng.next_u64().to_le_bytes());
	}
	out.truncate(len);
	out
}

/// The path of `entry`'s parent — `/` for a top-level entry.
fn parent_of(path: &str) -> &str {
	match path.rfind('/') {
		Some(0) => "/",
		Some(i) => &path[..i],
		None => "/",
	}
}

// endregion:    --- Support

// region:    --- Walking a volume

#[test]
fn test_walking_an_empty_volume_lists_nothing() -> Result<()> {
	// -- Setup & Fixtures: the root exists by definition but is not an entry,
	// so a formatted volume with nothing in it walks to an empty list.
	let store = store(BLOCKS)?;

	// -- Exec & Check
	assert!(store.walk()?.is_empty(), "a fresh volume has nothing to walk");
	Ok(())
}

#[test]
fn test_every_walked_path_is_absolute_and_normal() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = store(BLOCKS)?;
	store.mkdir("/a", 0o755)?;
	store.mkdir("/a/b c", 0o755)?;
	store.write_new_file("/a/x.txt", b"hi", 0o644)?;
	store.write_new_file("/a/b c/y.txt", b"yo", 0o644)?;

	// -- Exec
	let entries = store.walk()?;

	// -- Check: every path is volume-absolute, has no trailing slash, and no
	// doubled separator — a rebuild replays these into `mkdir`/`write_file`, and
	// a path that is not in that shape would create a different tree.
	assert!(!entries.is_empty());
	for e in &entries {
		assert!(e.path.starts_with('/'), "{} is not absolute", e.path);
		assert!(e.path.len() > 1, "the root must not be listed: {}", e.path);
		assert!(!e.path.ends_with('/'), "{} has a trailing slash", e.path);
		assert!(!e.path.contains("//"), "{} has a doubled separator", e.path);
	}
	Ok(())
}

#[test]
fn test_a_parent_is_always_listed_before_its_children() -> Result<()> {
	// -- Setup & Fixtures: pre-order is the contract that makes the walk
	// replayable in one pass — each entry can be created as it arrives because
	// its parent already exists.
	let mut store = store(BLOCKS)?;
	store.mkdir("/d1", 0o755)?;
	store.mkdir("/d1/d2", 0o755)?;
	store.mkdir("/d1/d2/d3", 0o755)?;
	store.mkdir("/other", 0o755)?;
	store.write_new_file("/d1/d2/d3/deep.txt", b"x", 0o644)?;
	store.write_new_file("/other/top.txt", b"x", 0o644)?;

	// -- Exec
	let entries = store.walk()?;

	// -- Check: walk the list in order, tracking every path already seen; an
	// entry whose parent is `/` is fine, an entry whose parent has not been seen
	// is a violation.
	let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
	for e in &entries {
		let parent = parent_of(&e.path);
		if parent != "/" {
			assert!(seen.contains(parent), "{} came before its parent {parent}", e.path);
		}
		seen.insert(&e.path);
	}
	Ok(())
}

#[test]
fn test_the_walk_agrees_with_stat_for_every_entry() -> Result<()> {
	// -- Setup & Fixtures: `walk` reports kind/size/mode from one read of the
	// tree; `stat` reads one path. Two readers of the same metadata must not
	// disagree, or a rebuild built on the walk would copy something the volume
	// does not actually hold.
	let mut store = store(BLOCKS)?;
	store.mkdir("/code", 0o700)?;
	store.mkdir("/code/empty", 0o755)?;
	store.write_new_file("/code/main.rs", b"fn main() {}", 0o600)?;
	store.write_new_file("/readme", b"docs", 0o644)?;

	// -- Exec & Check
	for e in store.walk()? {
		let inode = store.stat(&e.path)?;
		assert_eq!(inode.kind, e.kind, "kind disagree at {}", e.path);
		assert_eq!(inode.size, e.size, "size disagree at {}", e.path);
		assert_eq!(inode.mode, e.mode, "mode disagree at {}", e.path);
	}
	Ok(())
}

#[test]
fn test_the_walk_is_sorted_and_repeats_itself() -> Result<()> {
	// -- Setup & Fixtures: a rebuild's verification is `walk(src) == walk(dst)`,
	// so a walk that reordered between runs would make a correct copy look
	// wrong. Two walks of one unchanged volume must be identical.
	let mut store = store(BLOCKS)?;
	for name in ["z", "a", "m", "b", "y"] {
		store.write_new_file(&format!("/{name}.txt"), name.as_bytes(), 0o644)?;
	}
	store.mkdir("/mid", 0o755)?;
	store.write_new_file("/mid/inner", b"i", 0o644)?;

	// -- Exec
	let first = store.walk()?;
	let second = store.walk()?;

	// -- Check: identical twice over
	assert_eq!(first, second, "two walks of one volume differ");

	// -- Check: and the order is the lexicographic order of the paths — DFS
	// pre-order with per-directory sorting, which is what makes it deterministic.
	let mut sorted = first.clone();
	sorted.sort_by(|a, b| a.path.cmp(&b.path));
	assert_eq!(paths(&first), paths(&sorted), "the walk is not in path order");
	Ok(())
}

#[test]
fn test_the_root_itself_is_not_listed() -> Result<()> {
	// -- Setup & Fixtures: `/` exists by definition and is not an entry; a
	// rebuild that saw it as one would try to `mkdir("/")` a volume that already
	// has one.
	let mut store = store(BLOCKS)?;
	store.write_new_file("/f", b"x", 0o644)?;

	// -- Exec & Check
	let entries = store.walk()?;
	assert!(
		entries.iter().all(|e| e.path != "/"),
		"the root must not appear as a walk entry"
	);
	Ok(())
}

#[test]
fn test_a_directory_is_listed_by_its_inode_not_by_its_name() -> Result<()> {
	// -- Setup & Fixtures: names are data. A directory named like a text file
	// and a file named like a directory must each report the kind their *inode*
	// says, not the kind their name implies.
	let mut store = store(BLOCKS)?;
	store.mkdir("/readme.txt", 0o755)?;
	store.write_new_file("/dir", b"not a directory", 0o644)?;

	// -- Exec
	let entries = store.walk()?;
	let kind_of = |p: &str| entries.iter().find(|e| e.path == p).map(|e| e.kind);

	// -- Check
	assert_eq!(kind_of("/readme.txt"), Some(InodeKind::Directory));
	assert_eq!(kind_of("/dir"), Some(InodeKind::RegularFile));
	Ok(())
}

#[test]
fn test_an_empty_directory_is_an_entry_with_size_zero() -> Result<()> {
	// -- Setup & Fixtures: an empty directory has nothing under it, so a walk
	// that only reported leaves would drop it entirely and a rebuild would lose
	// the directory.
	let mut store = store(BLOCKS)?;
	store.mkdir("/kept", 0o755)?;

	// -- Exec
	let entries = store.walk()?;

	// -- Check: present, a directory, and reporting zero content bytes
	let kept = entries
		.iter()
		.find(|e| e.path == "/kept")
		.expect("an empty directory must still be an entry");
	assert_eq!(kept.kind, InodeKind::Directory);
	assert_eq!(kept.size, 0, "an empty directory has no content bytes");
	Ok(())
}

#[test]
fn test_modes_come_back_from_the_walk_unchanged() -> Result<()> {
	// -- Setup & Fixtures: modes are stored verbatim with no validation (pinned
	// in `security_paths.rs`), so the walk must report exactly what was written,
	// including a mode with the file-type bits set.
	let mut store = store(BLOCKS)?;
	store.mkdir("/d", 0o751)?;
	store.write_new_file("/d/a", b"a", 0o600)?;
	store.write_new_file("/d/b", b"b", 0o644)?;
	store.write_new_file("/d/c", b"c", 0o777)?;

	// -- Exec
	let entries = store.walk()?;
	let mode_of = |p: &str| entries.iter().find(|e| e.path == p).map(|e| e.mode);

	// -- Check
	assert_eq!(mode_of("/d"), Some(0o751));
	assert_eq!(mode_of("/d/a"), Some(0o600));
	assert_eq!(mode_of("/d/b"), Some(0o644));
	assert_eq!(mode_of("/d/c"), Some(0o777));
	Ok(())
}

#[test]
fn test_a_deeply_nested_tree_is_listed_parent_first() -> Result<()> {
	// -- Setup & Fixtures: one directory per level, so the walk's own recursion
	// carries the whole tree.
	let mut store = store(BIG_BLOCKS)?;
	let depth = 40;
	let mut path = String::new();
	for i in 0..depth {
		path.push_str(&format!("/l{i}"));
		store.mkdir(&path, 0o755)?;
	}
	store.write_new_file(&format!("{path}/leaf.txt"), b"deep", 0o644)?;

	// -- Exec
	let entries = store.walk()?;

	// -- Check: every level is present and in order
	assert_eq!(entries.len(), depth + 1);
	for (i, e) in entries.iter().take(depth).enumerate() {
		assert_eq!(e.path, format!("/{}", (0..=i).map(|j| format!("l{j}")).collect::<Vec<_>>().join("/")));
	}
	assert_eq!(entries.last().map(|e| e.path.as_str()), Some(format!("{path}/leaf.txt").as_str()));
	Ok(())
}

// endregion:    --- Walking a volume

// region:    --- Rebuilding into a fresh volume

/// A tree with a known shape, for the rebuild tests.
///
/// Returns `(store, dirs, files, content_bytes)` so a stats assertion has
/// something to check against that is not the same call it is testing.
fn populated(blocks: u64) -> Result<(Store, u64, u64, u64)> {
	let mut store = store(blocks)?;
	store.mkdir("/d1", 0o755)?; // 1
	store.mkdir("/d1/sub", 0o755)?; // 2
	store.mkdir("/empty", 0o700)?; // 3
	store.write_new_file("/d1/a.txt", b"hello", 0o644)?; // 5 bytes
	store.write_new_file("/d1/sub/b.txt", b"abc", 0o600)?; // 3 bytes
	store.write_new_file("/top.bin", &noisy(100, 1), 0o644)?; // 100 bytes
	Ok((store, 3, 3, 108))
}

#[test]
fn test_a_rebuild_reproduces_the_source_tree_exactly() -> Result<()> {
	// -- Setup & Fixtures
	let (src, _, _, _) = populated(BLOCKS)?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst)?;

	// -- Check: the walk of the destination is the walk of the source, entry for
	// entry — the property `bugs compact` verifies with
	assert_eq!(dst.walk()?, src.walk()?, "the rebuilt tree is not the source tree");

	// -- Check: and the bytes match on a spot check
	assert_eq!(dst.read_file("/d1/a.txt")?, b"hello");
	assert_eq!(dst.read_file("/top.bin")?, src.read_file("/top.bin")?);
	Ok(())
}

#[test]
fn test_a_rebuild_reports_what_it_moved() -> Result<()> {
	// -- Setup & Fixtures
	let (src, dirs, files, content) = populated(BLOCKS)?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	let stats = rebuild_into(&src, &mut dst)?;

	// -- Check: the counts are the tree's, not the call's own idea of it
	assert_eq!(stats.dirs, dirs, "directory count");
	assert_eq!(stats.files, files, "file count");
	assert_eq!(stats.bytes, content, "content byte count");
	Ok(())
}

#[test]
fn test_a_rebuild_preserves_modes_and_empty_directories() -> Result<()> {
	// -- Setup & Fixtures: the two things a naive copy loses. Modes because it
	// writes new files with a default, and empty directories because it only
	// creates directories it is about to put something in.
	let (src, _, _, _) = populated(BLOCKS)?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst)?;

	// -- Check: the empty directory survived...
	assert!(dst.stat("/empty").is_ok(), "the empty directory was dropped");
	// -- Check: ...as a directory, with its mode
	assert_eq!(dst.stat("/empty")?.kind, InodeKind::Directory);
	assert_eq!(dst.stat("/empty")?.mode, 0o700);
	// -- Check: and a nested mode survived too
	assert_eq!(dst.stat("/d1/sub/b.txt")?.mode, 0o600);
	assert_eq!(dst.stat("/d1")?.mode, 0o755);
	Ok(())
}

#[test]
fn test_a_rebuilt_volume_is_a_valid_volume_when_mounted_from_bytes() -> Result<()> {
	// -- Setup & Fixtures: the swap replaces the old image with the new one, so
	// the destination must be a real volume file — not just a tree the writing
	// process can still see because it holds the allocator in memory.
	let (src, _, _, _) = populated(BLOCKS)?;
	let (mut dst, watch) = watched(BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst)?;
	let reopened = open_bytes(bytes(&watch))?;

	// -- Check: mounted cold from its bytes, the volume holds the same tree
	assert_eq!(reopened.walk()?, src.walk()?);
	assert_eq!(reopened.read_file("/d1/a.txt")?, b"hello");
	Ok(())
}

#[test]
fn test_two_rebuilds_of_one_source_are_byte_identical() -> Result<()> {
	// -- Setup & Fixtures: the same source into two freshly formatted volumes of
	// the same size. Nothing about a rebuild should depend on wall-clock time,
	// on iteration order of a hash map, or on the target's arbitrary state — a
	// rebuilt image should be reproducible, so two hosts compacting the same
	// volume produce the same file.
	let (src, _, _, _) = populated(BLOCKS)?;
	let (mut dst1, watch1) = watched(BLOCKS)?;
	let (mut dst2, watch2) = watched(BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst1)?;
	rebuild_into(&src, &mut dst2)?;

	// -- Check
	assert_eq!(bytes(&watch1), bytes(&watch2), "two rebuilds of one source differ");
	Ok(())
}

#[test]
fn test_rebuilding_over_the_same_target_twice_is_well_defined() -> Result<()> {
	// -- Setup & Fixtures: `bugs compact` may resume a rebuild that was
	// interrupted, so replaying a tree into a volume that already carries it
	// must be a no-op rather than an `AlreadyExists` error. `rebuild_into` uses
	// `write_file` (create-or-replace) and creates a directory only if absent —
	// this pins that.
	let (src, dirs, files, content) = populated(BLOCKS)?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst)?;
	let once = dst.walk()?;
	let stats = rebuild_into(&src, &mut dst)?;

	// -- Check: the second pass neither errored nor changed the tree, and the
	// stats still describe the source (they count what it walked, not what it
	// newly created).
	assert_eq!(dst.walk()?, once, "the second rebuild changed the tree");
	assert_eq!((stats.dirs, stats.files, stats.bytes), (dirs, files, content));
	Ok(())
}

#[test]
fn test_rebuilding_an_empty_volume_moves_nothing() -> Result<()> {
	// -- Setup & Fixtures
	let src = store(BLOCKS)?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	let stats = rebuild_into(&src, &mut dst)?;

	// -- Check
	assert_eq!(stats.dirs, 0);
	assert_eq!(stats.files, 0);
	assert_eq!(stats.bytes, 0);
	assert!(dst.walk()?.is_empty());
	Ok(())
}

#[test]
fn test_rebuilding_an_app_volume_keeps_the_app_identity() -> Result<()> {
	// -- Setup & Fixtures: an application store carries its identity in
	// `/.ledgerfs/*`, and that identity is what `Store::app` reads back. A
	// rebuild into such a volume (the app compacting itself) must not clobber
	// it — `rebuild_into` only touches paths the source has.
	let (src, _, _, _) = populated(BLOCKS)?;
	let mut dst = app_store(BLOCKS, "demo")?;
	let identity_before: Vec<String> = dst
		.walk()?
		.into_iter()
		.filter(|e| e.path.starts_with("/.ledgerfs"))
		.map(|e| e.path)
		.collect();
	assert!(!identity_before.is_empty(), "the app volume must carry identity files");

	// -- Exec
	rebuild_into(&src, &mut dst)?;

	// -- Check: the app still owns the volume...
	assert_eq!(dst.app()?, Some("demo".to_string()), "the rebuild clobbered the app identity");
	// -- Check: ...its identity files are still there...
	let identity_after: Vec<String> = dst
		.walk()?
		.into_iter()
		.filter(|e| e.path.starts_with("/.ledgerfs"))
		.map(|e| e.path)
		.collect();
	assert_eq!(identity_after, identity_before, "the identity files changed");
	// -- Check: ...and the source tree arrived alongside.
	assert!(dst.stat("/d1/a.txt").is_ok());
	Ok(())
}

#[test]
fn test_a_rebuild_of_a_large_file_round_trips_every_byte() -> Result<()> {
	// -- Setup & Fixtures: a file large enough to span dozens of blocks, with
	// content chosen so no two chunks are equal (a run of one byte would dedup
	// to a single chunk and never exercise the extent chain).
	let data = noisy(200 * 1024, 7);
	let mut src = store(BLOCKS)?;
	src.write_new_file("/big.bin", &data, 0o644)?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	let stats = rebuild_into(&src, &mut dst)?;

	// -- Check: every byte, and the size accounting
	assert_eq!(stats.bytes, data.len() as u64);
	let back = dst.read_file("/big.bin")?;
	assert_eq!(back.len(), data.len(), "the rebuilt file has the wrong length");
	assert!(back == data, "the rebuilt file's contents differ");
	Ok(())
}

#[test]
fn test_a_rebuild_of_a_deep_tree_round_trips() -> Result<()> {
	// -- Setup & Fixtures: deep nesting, which is where a rebuild's own
	// recursion (create each parent before its child) is exercised.
	let mut src = store(BIG_BLOCKS)?;
	let depth = 40;
	let mut path = String::new();
	for i in 0..depth {
		path.push_str(&format!("/n{i}"));
		src.mkdir(&path, 0o755)?;
	}
	src.write_new_file(&format!("{path}/leaf.bin"), &noisy(4096, 3), 0o644)?;
	let mut dst = store(BIG_BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst)?;

	// -- Check
	assert_eq!(dst.walk()?, src.walk()?);
	assert_eq!(dst.read_file(&format!("{path}/leaf.bin"))?.len(), 4096);
	Ok(())
}

#[test]
fn test_a_rebuild_into_a_file_backed_volume() -> Result<()> {
	// -- Setup & Fixtures: the real `compact` target is a file, not a heap
	// buffer, so the destination path must work over a `FileBlockDevice` too.
	let (src, _, _, _) = populated(BLOCKS)?;
	let image = TempImage::new("rebuild-dst");
	let mut dst = image.store(BLOCKS)?;

	// -- Exec
	rebuild_into(&src, &mut dst)?;

	// -- Check
	assert_eq!(dst.walk()?, src.walk()?);
	Ok(())
}

#[test]
fn test_a_rebuild_reclaims_space_a_churned_volume_has_leaked() -> Result<()> {
	// -- Setup & Fixtures: the whole reason this module exists. Rewrite one file
	// many times with different content; every rewrite is a fresh `remove` +
	// `create` + `write`, so the source volume fills with versions no reader can
	// reach. The destination holds only the live tree and should be far emptier.
	let (mut src, _watch) = watched(BLOCKS)?;
	let rounds = 40;
	for i in 0..rounds {
		let payload = noisy(4096, 100 + i);
		src.write_file("/log", &payload, 0o644)?;
	}
	let src_free = src.free_bytes().expect("the source has an allocator snapshot");
	let live = src.read_file("/log")?;

	// -- Exec
	let mut dst = store(BLOCKS)?;
	rebuild_into(&src, &mut dst)?;
	let dst_free = dst.free_bytes().expect("the destination has an allocator snapshot");

	// -- Check: the destination is emptier than the source — the leaked versions
	// are gone.
	assert!(
		dst_free > src_free,
		"the rebuild did not reclaim space: src free {src_free}, dst free {dst_free}"
	);

	// -- Check: and it is emptier by roughly the churn, not by losing the file —
	// the content is intact.
	assert_eq!(dst.read_file("/log")?, live);
	assert_eq!(dst.walk()?, src.walk()?);
	Ok(())
}

// endregion:    --- Rebuilding into a fresh volume

// region:    --- What a rebuild will not do

/// Hand-assemble a volume whose root directory holds exactly one entry: a
/// symlink named `link`.
///
/// There is no symlink *writer* in this crate — the host path has no verb that
/// creates one — so the only way a symlink can appear in a volume this reader
/// is handed is a volume assembled elsewhere. That is exactly the case the
/// refusal exists for, and it is built here through the format's own public
/// API so every record on the volume is valid: the test proves the reader
/// refuses a *well-formed* symlink, not that it rejects garbage.
fn symlink_volume() -> Result<Vec<u8>> {
	let blocks = BLOCKS;
	let mut bytes = vec![0u8; blocks as usize * BS];
	let mut next_lba = RESERVED_HEADER_BLOCKS;

	// Place one encoded record at the next free block, returning its
	// `(object_id, lba)`.
	let mut place = |payload: &[u8], kind: ObjectType| -> Result<(ObjectId, u64)> {
		let lba = next_lba;
		next_lba += 1;
		let mut block = vec![0u8; BS];
		let written = ObjectRecordWriter::write(
			&mut block,
			kind,
			CompressionAlgorithm::None,
			0,
			payload,
		)
		.map_err(|e| Error::Format(e))?;
		assert!(written <= BS, "a crafted record must fit one block");
		let start = lba as usize * BS;
		bytes[start..start + BS].copy_from_slice(&block);
		Ok((ObjectId::from_content(payload), lba))
	};

	// -- The symlink inode. Its `content_ref` is its target; pointing it at a
	// ref that need not exist is fine — the refusal happens before it is
	// followed.
	let mut link = Inode::new_symlink(0, 0, ObjectRef::new(ObjectId::from_content(b"/target"), StorageHint(9)));
	link.update_object_id();
	let link_payload = link.encode_canonical().map_err(Error::Format)?;
	let link_ref = place(&link_payload, ObjectType::Inode)?;

	// -- The root directory node, naming that inode `link`.
	let mut node = DirNode::new_leaf();
	node.insert_entry(DirEntry::new("link", ObjectRef::new(link_ref.0, StorageHint(link_ref.1))).map_err(Error::Format)?)
		.map_err(Error::Format)?;
	let node_payload = node.encode_canonical().map_err(Error::Format)?;
	let node_ref = place(&node_payload, ObjectType::BTreeLeaf)?;

	// -- The root directory inode, and the four superblock copies.
	let mut root = Inode::new_directory(0, 0, 0o755);
	root.content_ref = ObjectRef::new(node_ref.0, StorageHint(node_ref.1));
	root.update_object_id();
	let root_payload = root.encode_canonical().map_err(Error::Format)?;
	let root_ref = place(&root_payload, ObjectType::Inode)?;

	let mut sb = Superblock::new(blocks);
	sb.active_root_id = *root_ref.0.as_bytes();
	sb.active_commit_seq = 1;
	sb.active_root_hint = root_ref.1;
	let mut encoded = vec![0u8; BS];
	sb.encode(&mut encoded).map_err(Error::Format)?;
	for lba in copy_lbas(blocks) {
		let start = lba as usize * BS;
		bytes[start..start + BS].copy_from_slice(&encoded);
	}
	Ok(bytes)
}

#[test]
fn test_a_walk_reports_a_symlink_as_a_symlink() -> Result<()> {
	// -- Setup & Fixtures: before pinning that a rebuild refuses it, pin that
	// the walk *sees* it correctly — a symlink reported as a regular file would
	// be copied as one, which is the silent corruption the refusal prevents.
	let bytes = symlink_volume()?;

	// -- Exec
	let store = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	let entries = store.walk()?;

	// -- Check
	let link = entries
		.iter()
		.find(|e| e.path == "/link")
		.expect("the symlink must be an entry");
	assert_eq!(link.kind, InodeKind::Symlink, "a symlink must be reported as one");
	Ok(())
}

#[test]
fn test_rebuild_refuses_to_follow_a_symlink() -> Result<()> {
	// -- Setup & Fixtures: rebuild copies files by reading their bytes and
	// writing them to the same name in the destination. Following a symlink
	// would copy its *target's* bytes under the link's name and turn a link into
	// a file — a quiet change to what the tree means. There is no symlink writer
	// here, so the volume is crafted; the refusal must be by name.
	let bytes = symlink_volume()?;
	let src = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	let mut dst = store(BLOCKS)?;

	// -- Exec
	let result = rebuild_into(&src, &mut dst);

	// -- Check: refused by name, and the name that was refused is the link's
	let e = expect_err("rebuild_into over a volume holding a symlink", result);
	match e {
		Error::Unsupported { detail } => {
			assert!(detail.contains("/link"), "the refusal must name the link: {detail}")
		}
		other => panic!("a symlink must be refused as Unsupported, not {other:?}"),
	}

	// -- Check: and nothing was copied — the destination is still empty, so a
	// caller who ignores the error has not been handed a half-tree that looks
	// complete. (The source holds only the symlink, so a refusal on it happens
	// before any write; this asserts the ordering holds.)
	assert!(dst.walk()?.is_empty(), "a refused rebuild must not leave a partial tree");
	Ok(())
}

// endregion:    --- What a rebuild will not do
