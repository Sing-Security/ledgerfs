//! Security tests for *crafted metadata*: a volume assembled byte by byte
//! through the format's own public API, so that a directory names structures a
//! well-formed volume could never contain.
//!
//! A volume is untrusted input. It is a file a user downloaded, or a disk
//! pulled out of a machine that was compromised — so its object graph, its
//! directory nodes and its extent chains are all attacker-chosen. Everything
//! above the block device resolves those structures by *recursing* into them:
//! `HostFs::resolve` descends a `DirNode`'s children, `read_dir` gathers a
//! directory through `cow::operations`' recursive `collect_dir_entries`, and
//! `Store::walk` recurses once per directory level. None of those walks bounds
//! its own depth, so a volume whose metadata is a long chain turns a harmless
//! listing into unbounded recursion — and unbounded recursion is a stack
//! overflow, which aborts the process that mounted the volume.
//!
//! Two rules run through this file, the same two as `security_media.rs`:
//!
//! 1. **The bytes are built through the format's own API.** `Superblock`,
//!    `DirNode` and `ObjectRecordWriter` are the same types the writer uses, so
//!    every record verifies (checksum, `object_id == blake3(payload)`) exactly
//!    as a real one does. The test proves "a volume that passes every integrity
//!    check can still take the reader down", not "a corrupt volume is
//!    rejected".
//! 2. **The answer is an error, never a panic and never a hang.** A walk that
//!    cannot bound its depth has to *refuse* rather than follow the chain into
//!    a stack overflow.
//!
//! # Why these tests run on a small stack
//!
//! The chains built here are finite, so on an ordinary thread a deep walk
//! eventually runs out of chain and returns `Ok`. What it runs out of *first*
//! depends on the stack, and the defect is precisely that the walk has no bound
//! of its own — so the depth at which it dies is the thread's stack, not any
//! property of the volume. Each test therefore runs the read on a thread with a
//! deliberately small stack ([`SMALL_STACK`]), which turns "unbounded" into a
//! deterministic abort: before the guard, the walk overflows the stack and the
//! test binary dies; after it, the walk refuses the chain long before the
//! stack matters. A stack overflow is not catchable, which is the point — it is
//! why this is a denial of service and not a returned error.

mod common;

use common::*;
use ledgerfs_host::{Error, HostFs, MemBlockDevice, Store};

use ledgerfs::compression::CompressionAlgorithm;
use ledgerfs::object::{DirEntry, DirNode, Inode, ObjectId, ObjectRef, StorageHint};
use ledgerfs::record::{ObjectRecordWriter, ObjectType};
use ledgerfs::superblock::Superblock;
use ledgerfs::{BLOCK_SIZE, RESERVED_HEADER_BLOCKS};

use std::thread;

const BS: usize = BLOCK_SIZE as usize;

/// Stack for the threads that run the reads under test — small enough that an
/// unbounded recursion overflows it well before it would overflow the 8 MiB a
/// normal thread gets, so the abort is deterministic rather than a race with
/// the depth of the chain.
const SMALL_STACK: usize = 256 * 1024;

/// How deep the crafted chains are. Comfortably past any stack a test thread
/// has (so an unbounded walk aborts) while staying far under the guard the fix
/// adds (so a bounded walk returns a refusal instead).
const DEEP: usize = 4_000;

/// Blocks in the crafted volumes: 64 MiB, enough to hold a [`DEEP`] chain and a
/// [`DEEP`]-level tree (two blocks per level) with room to spare, since every
/// object here occupies its own block.
const BLOCK_COUNT: u64 = 16_384;

// region:    --- Crafting a volume

/// A volume being assembled by hand, one block-aligned `ObjectRecord` at a
/// time.
///
/// Starts at the first block past the reserved header (superblock copies 0–1, the
/// journal, the genesis audit and the allocator snapshot all live below
/// [`RESERVED_HEADER_BLOCKS`]), so every placed object lands in real object
/// space and the superblock copies are written last, over the top, by
/// [`finish`](Self::finish).
struct Crafted {
	blocks: u64,
	bytes: Vec<u8>,
	next_lba: u64,
}

impl Crafted {
	fn new(blocks: u64) -> Self {
		Self {
			blocks,
			bytes: vec![0u8; blocks as usize * BS],
			next_lba: RESERVED_HEADER_BLOCKS,
		}
	}

	/// Encode `payload` as an `ObjectRecord` at the next free LBA and return
	/// `(object_id, lba)`.
	///
	/// Through [`ObjectRecordWriter`], not by hand: it computes the BLAKE3
	/// `object_id` and the record checksum the same way a real commit does, so
	/// every object placed here passes `ObjectRecordReader::read` end to end.
	/// An object the reader would reject proves nothing about the reader and
	/// everything about the corruption being detectable.
	fn place(&mut self, payload: &[u8], object_type: ObjectType) -> (ObjectId, u64) {
		let lba = self.next_lba;
		self.next_lba += 1;

		let mut block = vec![0u8; BS];
		let written = ObjectRecordWriter::write(&mut block, object_type, CompressionAlgorithm::None, 0, payload)
			.expect("encode a crafted ObjectRecord");
		assert!(written <= BS, "a crafted record must fit one block: {written} bytes");

		let start = lba as usize * BS;
		self.bytes[start..start + BS].copy_from_slice(&block);

		(ObjectId::from_content(payload), lba)
	}

	/// Publish `root_inode` as the volume's active root, write the four
	/// superblock copies, and hand back the finished volume.
	///
	/// `active_commit_seq = 1` because `mount_select` builds its candidate list
	/// only from copies with a committed sequence — a zero there is the
	/// "initialised but never formatted" state, not a hostile one. The hint
	/// makes the root a single read rather than a scan.
	fn finish(mut self, root_inode: (ObjectId, u64)) -> Vec<u8> {
		let (id, lba) = root_inode;

		let mut sb = Superblock::new(self.blocks);
		sb.active_root_id = *id.as_bytes();
		sb.active_commit_seq = 1;
		sb.active_root_hint = lba;

		let mut encoded = vec![0u8; BS];
		sb.encode(&mut encoded).expect("encode a crafted superblock");

		for lba in copy_lbas(self.blocks) {
			let start = lba as usize * BS;
			self.bytes[start..start + BS].copy_from_slice(&encoded);
		}
		self.bytes
	}
}

/// A `DirNode` chain `depth` deep, all nodes empty (so a name lookup never
/// matches and always descends), ending in a leaf.
///
/// Each internal node's single child reference is the *verified* id of the node
/// below it — the format keeps `blake3(payload) == object_id` even for a
/// hostile volume, so the chain is a genuine not-yet-cyclic one. That the
/// descent has no self-reference is what makes this a depth problem rather than
/// an infinite-loop one, and both are the same denial of service.
fn dir_chain(volume: &mut Crafted, depth: usize) -> (ObjectId, u64) {
	let mut child: Option<(ObjectId, u64)> = None;
	let mut root = (ObjectId::zero(), 0u64);
	for _ in 0..depth {
		let node = match child {
			None => DirNode::new_leaf(),
			Some((id, lba)) => DirNode::new_internal(Vec::new(), vec![ObjectRef::new(id, StorageHint(lba))]),
		};
		let payload = node.encode_canonical().expect("encode a DirNode");
		root = volume.place(&payload, ObjectType::BTreeNode);
		child = Some(root);
	}
	root
}

/// A directory tree `depth` levels deep: each level holds one subdirectory `d`
/// and nothing else, the deepest being an empty leaf.
///
/// Returns the root directory's inode reference. This is a different axis from
/// [`dir_chain`]: the chain stresses one directory's degree, this stresses the
/// *nesting* of directories, which is what `Store::walk` recurses over.
fn dir_tree(volume: &mut Crafted, depth: usize) -> (ObjectId, u64) {
	let mut child_inode: Option<(ObjectId, u64)> = None;
	let mut root = (ObjectId::zero(), 0u64);
	for _ in 0..depth {
		let dir_node = match child_inode {
			None => DirNode::new_leaf(),
			Some((id, lba)) => {
				let mut node = DirNode::new_leaf();
				node.insert_entry(DirEntry::new("d", ObjectRef::new(id, StorageHint(lba))).expect("entry"))
					.expect("insert");
				node
			}
		};
		let node_payload = dir_node.encode_canonical().expect("encode a DirNode");
		let (node_id, node_lba) = volume.place(&node_payload, ObjectType::BTreeLeaf);

		let mut inode = Inode::new_directory(0, 0, 0o755);
		inode.content_ref = ObjectRef::new(node_id, StorageHint(node_lba));
		inode.update_object_id();
		let inode_payload = inode.encode_canonical().expect("encode an Inode");
		root = volume.place(&inode_payload, ObjectType::Inode);
		child_inode = Some(root);
	}
	root
}

/// A directory inode whose `content_ref` is `content`.
fn directory_inode(volume: &mut Crafted, content: (ObjectId, u64)) -> (ObjectId, u64) {
	let mut inode = Inode::new_directory(0, 0, 0o755);
	inode.content_ref = ObjectRef::new(content.0, StorageHint(content.1));
	inode.update_object_id();
	let payload = inode.encode_canonical().expect("encode an Inode");
	volume.place(&payload, ObjectType::Inode)
}

// endregion:    --- Crafting a volume

// region:    --- Support

/// Run `read` on a thread with [`SMALL_STACK`], so an unbounded recursion in it
/// overflows deterministically. A stack overflow aborts the whole process — it
/// cannot be caught — which is exactly the failure this file is about.
fn on_small_stack<T, F>(read: F) -> T
where
	T: Send + 'static,
	F: FnOnce() -> T + Send + 'static,
{
	thread::Builder::new()
		.name("security-crafted-reader".to_string())
		.stack_size(SMALL_STACK)
		.spawn(read)
		.expect("spawn the small-stack reader")
		.join()
		.expect("the small-stack reader panicked")
}

fn is_refusal(e: &Error) -> bool {
	matches!(e, Error::Unsupported { .. })
}

// endregion:    --- Support

// region:    --- Unbounded recursion on crafted metadata

#[test]
fn test_a_deep_dir_node_chain_is_refused_by_resolve_and_read_dir() -> Result<()> {
	// -- Setup & Fixtures: a directory whose `DirNode` head references a chain
	// of further nodes, four thousand deep, every record byte-for-byte valid on
	// disk. `resolve` walks it through `path::lookup`; `read_dir` walks it
	// through `cow::operations`' `collect_dir_entries`. Both recurse with no
	// bound, so this is the same defect on two read paths.
	let mut volume = Crafted::new(BLOCK_COUNT);
	let chain = dir_chain(&mut volume, DEEP);
	let root = directory_inode(&mut volume, chain);
	let bytes = volume.finish(root);

	// -- Exec & Check: resolving an absent name must descend the whole chain
	// before it can answer "no such file" — so an unbounded walk overflows and
	// the test binary dies here, while a bounded one returns a refusal.
	let resolve = on_small_stack(move || {
		let fs = HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
		fs.resolve("anything")
	});
	let e = expect_err("resolve over a deep DirNode chain", resolve);
	assert!(
		is_refusal(&e),
		"the deep chain must be refused by name, not followed: {e:?}"
	);

	// -- Exec & Check: the listing side of the same structure, on its own
	// mount so the two paths are exercised independently.
	let mut volume = Crafted::new(BLOCK_COUNT);
	let chain = dir_chain(&mut volume, DEEP);
	let root = directory_inode(&mut volume, chain);
	let bytes = volume.finish(root);
	let listing = on_small_stack(move || {
		let fs = HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
		fs.read_dir("/")
	});
	let e = expect_err("read_dir over a deep DirNode chain", listing);
	assert!(is_refusal(&e), "the listing walk must refuse the chain too: {e:?}");
	Ok(())
}

#[test]
fn test_a_deeply_nested_directory_tree_is_refused_by_walk() -> Result<()> {
	// -- Setup & Fixtures: not a wide directory but a deep one — four thousand
	// directories, each holding one subdirectory. No DirNode holds a chain, so
	// the guard on the *degree* of a node does not cover this; the recursion is
	// in `Store::walk`'s own descent, one level per directory.
	let mut volume = Crafted::new(BLOCK_COUNT);
	let root = dir_tree(&mut volume, DEEP);
	let bytes = volume.finish(root);

	// -- Exec: `walk` recurses once per directory level. Unbounded, it reaches
	// four thousand frames and overflows the small stack; bounded, it refuses
	// long before that.
	let result = on_small_stack(move || {
		let store = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
		store.walk()
	});

	// -- Check
	let e = expect_err("walk over a deep directory tree", result);
	assert!(is_refusal(&e), "the deep tree must be refused by name: {e:?}");
	Ok(())
}

// endregion:    --- Unbounded recursion on crafted metadata

// region:    --- References out of range

#[test]
fn test_a_dir_node_child_pointing_at_block_u64_max_is_refused_not_followed() -> Result<()> {
	// -- Setup & Fixtures: an internal `DirNode` whose child has a plausible
	// object id but a storage hint of `u64::MAX`. The hint is a physical block
	// number straight off the volume, so on a hostile one it is attacker-chosen;
	// `u64::MAX * 4096` overflows a `u64`, and the classic bug is to check the
	// product *after* computing it — which a mount of this volume would turn
	// into a panic on the mounting process.
	let mut volume = Crafted::new(BLOCKS);
	let bogus = ObjectId::from_content(b"no record on this volume carries this id");
	let node = DirNode::new_internal(Vec::new(), vec![ObjectRef::new(bogus, StorageHint(u64::MAX))]);
	let payload = node.encode_canonical().expect("encode a DirNode");
	let chain = volume.place(&payload, ObjectType::BTreeNode);
	let root = directory_inode(&mut volume, chain);
	let bytes = volume.finish(root);

	// -- Exec: no small stack needed — the point is that this *returns* at all.
	let fs = mount_bytes(bytes)?;
	let result = fs.resolve("anything");

	// -- Check: the hinted block is out of range, the id is not on the volume,
	// and the answer is a named "not present" error rather than a panic or a
	// wrapped read of some unrelated block.
	let e = expect_err("a child hint at u64::MAX", result);
	assert!(
		matches!(e, Error::ObjectNotFound { .. }),
		"an out-of-range hint must resolve to a missing object: {e:?}"
	);
	Ok(())
}

// endregion:    --- References out of range
