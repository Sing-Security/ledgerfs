//! Inode object type - file metadata with xattrs and security flags

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use super::object_id::{ObjectId, ObjectRef};
use crate::error::{Error, Result};

/// Inode kind (file type)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InodeKind {
	RegularFile,
	Directory,
	Symlink,
	// Reserved for future: BlockDevice, CharDevice, Fifo, Socket
}

/// Security flags for inodes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityFlags {
	/// Disable deduplication for this inode
	pub no_dedupe: bool,

	/// Immutable flag (cannot modify)
	pub immutable: bool,

	/// Append-only flag
	pub append_only: bool,
}

impl Default for SecurityFlags {
	fn default() -> Self {
		Self {
			no_dedupe: false,
			immutable: false,
			append_only: false,
		}
	}
}

/// Inode metadata structure
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inode {
	/// Content-addressed ID of this inode — `blake3` of this struct's own
	/// canonical encoding, deliberately **excluded** from that encoding
	/// (`#[serde(skip)]`, matching `AllocNode`'s convention): a field can't
	/// serialize its own final value into the bytes being hashed to
	/// produce that value without becoming self-referentially inconsistent
	/// (found live, the hard way — see `update_object_id`/`decode_canonical`).
	#[serde(skip)]
	pub object_id: ObjectId,

	/// Inode kind (file, directory, symlink)
	pub kind: InodeKind,

	/// Unix permissions (mode bits)
	pub mode: u32,

	/// Owner user ID
	pub uid: u32,

	/// Owner group ID
	pub gid: u32,

	/// File size in bytes
	pub size: u64,

	/// Creation timestamp (UNIX nanoseconds)
	pub ctime_ns: u64,

	/// Modification timestamp (UNIX nanoseconds)
	pub mtime_ns: u64,

	/// Access timestamp (UNIX nanoseconds)
	pub atime_ns: u64,

	/// Extended attributes (BTreeMap for determinism)
	pub xattrs: BTreeMap<heapless::String<256>, Vec<u8>>,

	/// Security flags
	pub security_flags: SecurityFlags,

	/// Content reference (for files: extent tree root, for dirs: dir tree root, for symlinks: target)
	pub content_ref: ObjectRef,
}

impl Inode {
	/// Create new regular file inode
	pub fn new_file(uid: u32, gid: u32, mode: u32) -> Self {
		let mut inode = Self {
			object_id: ObjectId::zero(), // Will be set after encoding
			kind: InodeKind::RegularFile,
			mode,
			uid,
			gid,
			size: 0,
			ctime_ns: 0,
			mtime_ns: 0,
			atime_ns: 0,
			xattrs: BTreeMap::new(),
			security_flags: SecurityFlags::default(),
			content_ref: ObjectRef::zero(),
		};
		inode.update_object_id();
		inode
	}

	/// Create new directory inode
	pub fn new_directory(uid: u32, gid: u32, mode: u32) -> Self {
		let mut inode = Self {
			object_id: ObjectId::zero(),
			kind: InodeKind::Directory,
			mode,
			uid,
			gid,
			size: 0,
			ctime_ns: 0,
			mtime_ns: 0,
			atime_ns: 0,
			xattrs: BTreeMap::new(),
			security_flags: SecurityFlags::default(),
			content_ref: ObjectRef::zero(),
		};
		inode.update_object_id();
		inode
	}

	/// Create new symlink inode
	pub fn new_symlink(uid: u32, gid: u32, target_ref: ObjectRef) -> Self {
		let mut inode = Self {
			object_id: ObjectId::zero(),
			kind: InodeKind::Symlink,
			mode: 0o777,
			uid,
			gid,
			size: 0,
			ctime_ns: 0,
			mtime_ns: 0,
			atime_ns: 0,
			xattrs: BTreeMap::new(),
			security_flags: SecurityFlags::default(),
			content_ref: target_ref,
		};
		inode.update_object_id();
		inode
	}

	/// Update object ID based on current content.
	///
	/// Safe to call at any point, in any order relative to setting other
	/// fields, precisely because `object_id` is `#[serde(skip)]` — the
	/// encoding this hashes never includes `object_id` itself, so there's
	/// no "encode with a placeholder, hash, then re-encode with the real
	/// value and get different bytes" trap.
	pub fn update_object_id(&mut self) {
		let canonical = self.encode_canonical().unwrap();
		self.object_id = ObjectId::from_content(&canonical);
	}

	/// Encode inode to canonical bytes. `object_id` itself is never part
	/// of the output (`#[serde(skip)]`) — see `update_object_id`.
	pub fn encode_canonical(&self) -> Result<Vec<u8>> {
		postcard::to_allocvec(self).map_err(Error::from)
	}

	/// Decode inode from canonical bytes.
	///
	/// `object_id` is `#[serde(skip)]`, so `postcard::from_bytes` leaves it
	/// as `ObjectId::default()` (zero) — recompute it directly from
	/// `bytes` (which *is* the canonical encoding, by definition) rather
	/// than re-encoding the freshly-decoded struct, which is both cheaper
	/// and exactly what a caller verifying `blake3(bytes) ==
	/// header.object_id` already independently checks.
	pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
		let mut inode: Self = postcard::from_bytes(bytes).map_err(Error::from)?;
		inode.object_id = ObjectId::from_content(bytes);
		Ok(inode)
	}

	/// Set extended attribute
	pub fn set_xattr(&mut self, key: &str, value: Vec<u8>) -> Result<()> {
		let key_str: heapless::String<256> =
			key.try_into().map_err(|_| Error::invalid_encoding("xattr key too long"))?;
		self.xattrs.insert(key_str, value);
		self.update_object_id();
		Ok(())
	}

	/// Get extended attribute
	pub fn get_xattr(&self, key: &str) -> Option<&Vec<u8>> {
		let key_str: heapless::String<256> = key.try_into().ok()?;
		self.xattrs.get(&key_str)
	}

	/// Update modification time
	pub fn touch(&mut self, timestamp_ns: u64) {
		self.mtime_ns = timestamp_ns;
		self.update_object_id();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, alloc::boxed::Box<dyn core::error::Error>>;

	#[test]
	fn test_inode_new_file() -> TestResult<()> {
		// -- Setup & Fixtures
		let uid = 1000;
		let gid = 1000;
		let mode = 0o644;

		// -- Exec
		let inode = Inode::new_file(uid, gid, mode);

		// -- Check
		assert_eq!(inode.kind, InodeKind::RegularFile);
		assert_eq!(inode.uid, uid);
		assert_eq!(inode.gid, gid);
		assert_eq!(inode.mode, mode);
		assert!(!inode.object_id.is_zero());

		Ok(())
	}

	#[test]
	fn test_inode_object_id_matches_blake3_of_its_own_stored_bytes() -> TestResult<()> {
		// -- Setup & Fixtures: regression test for a real, previously-
		// undiscovered bug — `object_id` used to be a normal serialized
		// field, so `update_object_id()`'s "encode with id=0, hash, set
		// id=hash" trick produced an id that did NOT match
		// `blake3(encode_canonical())` afterward (the second encode call
		// embeds the *real* id, producing different bytes than what was
		// hashed). This is exactly the invariant `ObjectRecordReader::read`
		// checks on real disk (`blake3(payload) == header.object_id`) —
		// existing round-trip tests never caught it because
		// `decode(encode(x)) == x` holds trivially either way (decode just
		// deserializes whatever bytes were given verbatim). Found live: a
		// genesis `Inode` committed to real disk was permanently
		// unreadable afterward
		let inode = Inode::new_file(1000, 1000, 0o644);

		// -- Exec
		let canonical = inode.encode_canonical()?;

		// -- Check
		assert_eq!(inode.object_id, ObjectId::from_content(&canonical), "object_id must equal blake3 of its own canonical encoding");

		// -- Check: decode_canonical must recompute the same id from the
		// bytes, not just deserialize whatever's embedded (there's nothing
		// embedded — object_id is #[serde(skip)]).
		let decoded = Inode::decode_canonical(&canonical)?;
		assert_eq!(decoded.object_id, inode.object_id);
		assert_eq!(decoded, inode);

		Ok(())
	}

	#[test]
	fn test_inode_encode_decode_deterministic() -> TestResult<()> {
		// -- Setup & Fixtures
		let inode1 = Inode::new_file(1000, 1000, 0o644);
		let inode2 = Inode::new_file(1000, 1000, 0o644);

		// -- Exec
		let encoded1 = inode1.encode_canonical()?;
		let encoded2 = inode2.encode_canonical()?;

		// -- Check
		assert_eq!(encoded1, encoded2); // Deterministic encoding

		let decoded1 = Inode::decode_canonical(&encoded1)?;
		let decoded2 = Inode::decode_canonical(&encoded2)?;

		assert_eq!(decoded1, decoded2);
		assert_eq!(decoded1.object_id, inode1.object_id);

		Ok(())
	}

	#[test]
	fn test_inode_xattrs_btreemap_sorted() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut inode = Inode::new_file(1000, 1000, 0o644);

		// -- Exec - Add xattrs in reverse alphabetical order
		inode.set_xattr("user.zebra", vec![1, 2, 3])?;
		inode.set_xattr("user.alpha", vec![4, 5, 6])?;
		inode.set_xattr("user.middle", vec![7, 8, 9])?;

		let encoded = inode.encode_canonical()?;

		// -- Check - BTreeMap ensures sorted keys in encoding
		// Decode and verify order is preserved
		let decoded = Inode::decode_canonical(&encoded)?;
		let keys: Vec<_> = decoded.xattrs.keys().collect();

		assert_eq!(keys.len(), 3);
		// BTreeMap guarantees sorted order
		assert!(keys[0] < keys[1]);
		assert!(keys[1] < keys[2]);

		Ok(())
	}

	#[test]
	fn test_inode_security_flags() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut inode = Inode::new_file(1000, 1000, 0o644);

		// -- Exec
		inode.security_flags.no_dedupe = true;
		inode.update_object_id();

		let encoded = inode.encode_canonical()?;
		let decoded = Inode::decode_canonical(&encoded)?;

		// -- Check
		assert!(decoded.security_flags.no_dedupe);
		assert!(!decoded.security_flags.immutable);

		Ok(())
	}
}
