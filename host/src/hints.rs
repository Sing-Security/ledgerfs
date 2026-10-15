//! A persistent object-id → physical-block index, kept beside a volume.
//!
//! # The problem this solves
//!
//! LedgerFS is content-addressed and CoW: an object's identity is a BLAKE3 of
//! its payload, and every write leaves the previous version where it was. The
//! *reader* therefore has to turn an `ObjectId` into a physical block, and it
//! has exactly three ways to do that (see `objstore.rs`): the reference's own
//! storage hint, this in-memory map, or a linear scan of the volume.
//!
//! Hints are the intended fast path, but nothing in the writer populates them
//! yet — `cow::operations` writes `chunk_hint: StorageHint::zero()` and builds
//! its `DirEntry`/`Inode` references from ids alone — so **on any volume this
//! tree has written, every hint below the root is zero.** The superblock
//! persists only the root's placement, so the first lookup of anything else
//! falls through to the scan, which walks the whole volume reading every
//! block's header. That scan is the whole cost of a cold `read_dir("/")` on a
//! volume of any size, and it is paid again by every process that opens the
//! volume; the build-side numbers are in `BENCHMARKS.md`.
//!
//! # Why a sidecar rather than a format change
//!
//! The map is a *pure cache*: every entry is re-verified against the block it
//! names (`DiskObjectStore::read_object`) before use, so a stale or wrong
//! entry costs one block read and a fallback scan — never a wrong answer. That
//! is what makes it safe to keep out of the volume entirely. Writing it into
//! the volume would mean a format change with a compatibility story, and would
//! not help the volumes that already exist. A file next to the volume costs
//! nothing to skip and nothing to delete: an absent or unreadable sidecar just
//! means one scan, exactly as today.
//!
//! # Validity
//!
//! The header carries the volume's identity — its size, and on unix its
//! `(device, inode)` — and a load whose identity does not match is discarded.
//! That covers the two ways a sidecar can go out from under its volume: a
//! rebuild, which writes a whole new file and `rename`s it into place (a new
//! inode), and a re-format in place (handled at the source — `Store::format`
//! removes the sidecar). Entries are re-verified on use regardless, so the
//! identity check is an efficiency measure, not a correctness one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ledgerfs::object::ObjectId;

use crate::device::VolumeIdentity;

/// `"LFSHINTS"` — eight bytes so the file is recognisable at a glance.
const MAGIC: &[u8; 8] = b"LFSHINTS";
/// Bumped whenever the layout below changes; an unrecognised version is
/// discarded rather than misread.
const VERSION: u32 = 1;
/// magic + version + reserved + size + dev + ino + count + reserved
const HEADER_LEN: usize = 8 + 4 + 4 + 8 + 8 + 8 + 4 + 4;
/// One entry: a 32-byte object id and its little-endian block number.
const ENTRY_LEN: usize = 32 + 8;

/// Read the index beside `identity`, or `None` when there is nothing usable
/// there.
///
/// "Nothing usable" is deliberately indistinguishable from "nothing there":
/// every failure — no file, short file, bad magic, wrong version, mismatched
/// identity, a truncated final entry — means the same thing to the caller, one
/// scan. There is no state in which the right answer is to refuse.
pub(crate) fn load(path: &Path, identity: VolumeIdentity) -> Option<HashMap<ObjectId, u64>> {
	let bytes = std::fs::read(path).ok()?;
	if bytes.len() < HEADER_LEN || &bytes[0..8] != MAGIC {
		return None;
	}
	let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap_or([0; 4]));
	let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap_or([0; 8]));

	if u32_at(8) != VERSION {
		return None;
	}
	let stored = VolumeIdentity {
		size_bytes: u64_at(16),
		dev: u64_at(24),
		ino: u64_at(32),
	};
	if stored != identity {
		return None;
	}

	let count = u32_at(40) as usize;
	let need = HEADER_LEN.checked_add(count.checked_mul(ENTRY_LEN)?)?;
	if bytes.len() < need {
		return None;
	}

	let mut out = HashMap::with_capacity(count);
	let mut at = HEADER_LEN;
	for _ in 0..count {
		let mut id = [0u8; 32];
		id.copy_from_slice(&bytes[at..at + 32]);
		let lba = u64::from_le_bytes(bytes[at + 32..at + 40].try_into().unwrap_or([0; 8]));
		out.insert(ObjectId::from_bytes(&id), lba);
		at += ENTRY_LEN;
	}
	Some(out)
}

/// Write `entries` beside the volume, atomically.
///
/// Best-effort by design: a sidecar that cannot be written is a cache miss on
/// the next open, not a failed operation. The write goes to a temporary in the
/// same directory and is `rename`d into place, so an interrupted save leaves
/// the previous cache (or nothing) rather than a half-file that would parse as
/// a plausible wrong answer.
pub(crate) fn save(path: &Path, identity: VolumeIdentity, entries: &HashMap<ObjectId, u64>) -> std::io::Result<()> {
	let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
	let mut bytes = Vec::with_capacity(HEADER_LEN + entries.len() * ENTRY_LEN);
	bytes.extend_from_slice(MAGIC);
	bytes.extend_from_slice(&VERSION.to_le_bytes());
	bytes.extend_from_slice(&0u32.to_le_bytes());
	bytes.extend_from_slice(&identity.size_bytes.to_le_bytes());
	bytes.extend_from_slice(&identity.dev.to_le_bytes());
	bytes.extend_from_slice(&identity.ino.to_le_bytes());
	bytes.extend_from_slice(&count.to_le_bytes());
	bytes.extend_from_slice(&0u32.to_le_bytes());
	for (id, lba) in entries.iter().take(count as usize) {
		bytes.extend_from_slice(id.as_bytes());
		bytes.extend_from_slice(&lba.to_le_bytes());
	}

	let tmp = tmp_path(path);
	std::fs::write(&tmp, &bytes)?;
	std::fs::rename(&tmp, path)?;
	Ok(())
}

/// Remove a stale sidecar — used by `Store::format`, where the volume is being
/// laid down anew in the same file and the old index would name blocks that no
/// longer hold what it says.
pub(crate) fn remove(path: &Path) {
	let _ = std::fs::remove_file(path);
}

/// The temporary `save` writes through. Same directory as `path`, so the
/// `rename` is within one filesystem and therefore atomic.
fn tmp_path(path: &Path) -> PathBuf {
	let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
	name.push(".tmp");
	path.with_file_name(name)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::device::VolumeIdentity;

	type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

	fn identity() -> VolumeIdentity {
		VolumeIdentity {
			size_bytes: 4096 * 64,
			dev: 7,
			ino: 99,
		}
	}

	fn temp(tag: &str) -> PathBuf {
		let mut p = std::env::temp_dir();
		p.push(format!("lfs-hints-{tag}-{}.hints", std::process::id()));
		p
	}

	fn map(pairs: &[(u8, u64)]) -> HashMap<ObjectId, u64> {
		pairs
			.iter()
			.map(|(b, lba)| {
				let mut id = [0u8; 32];
				id[0] = *b;
				(ObjectId::from_bytes(&id), *lba)
			})
			.collect()
	}

	#[test]
	fn a_saved_index_round_trips() -> Result<()> {
		let path = temp("round-trip");
		let want = map(&[(1, 6), (2, 7), (3, 900)]);
		save(&path, identity(), &want)?;
		let got = load(&path, identity()).expect("a fresh save must load");
		assert_eq!(got, want);
		let _ = std::fs::remove_file(&path);
		Ok(())
	}

	#[test]
	fn a_missing_file_is_a_miss_not_an_error() {
		assert!(load(&temp("absent"), identity()).is_none());
	}

	#[test]
	fn a_different_volume_identity_discards_the_index() {
		// -- Setup & Fixtures: same path, a volume that is not the one the
		// index was written against — the `compact` case (new inode).
		let path = temp("identity");
		let mut other = identity();
		other.ino += 1;
		save(&path, identity(), &map(&[(1, 6)])).expect("save");
		// -- Check
		assert!(
			load(&path, other).is_none(),
			"an index for another volume must not load"
		);
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn a_truncated_file_is_discarded() {
		// -- Setup & Fixtures: a valid header claiming more entries than follow.
		let path = temp("truncated");
		save(&path, identity(), &map(&[(1, 6), (2, 7)])).expect("save");
		let mut bytes = std::fs::read(&path).expect("read");
		bytes.truncate(HEADER_LEN + ENTRY_LEN); // one entry short of the count
		std::fs::write(&path, &bytes).expect("write");
		// -- Check
		assert!(load(&path, identity()).is_none());
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn garbage_is_discarded() {
		let path = temp("garbage");
		std::fs::write(&path, b"not a hint index at all, not even close").expect("write");
		assert!(load(&path, identity()).is_none());
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn remove_deletes_the_sidecar() {
		let path = temp("remove");
		save(&path, identity(), &map(&[(1, 6)])).expect("save");
		remove(&path);
		assert!(load(&path, identity()).is_none());
	}
}
