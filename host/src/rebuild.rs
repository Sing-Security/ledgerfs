//! Rebuilding a volume into a fresh one — the answer to "space only falls".
//!
//! CoW frees nothing (see `EMBEDDING.md` §3), so a volume that has been written
//! for a while is mostly unreachable garbage: every commit left the previous
//! version of every object it touched behind. The *live* tree may be a
//! thousandth of the device. Nothing in the format reclaims that in place —
//! the kernel crate's GC is not wired into this path — so the only way to get
//! the space back is to write the live tree into an empty volume and swap.
//!
//! That is what this module is for, and it is deliberately **not** an
//! in-place operation: `rebuild_into` reads `src` and writes `dst`, two
//! different devices, because a volume has one writer and the source must stay
//! untouched until the caller has verified the copy. The swap is the caller's
//! business (a `rename` of the new image over the old one, after verification).
//!
//! Measured on a real volume: 554 KiB of transcript content had grown the
//! volume to 124 MiB. Rebuilding it returns the difference.

use crate::{Error, InodeKind, Result, Store};

/// One entry in a volume's tree, as a volume-absolute path (`/notes/x.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
	/// Volume-absolute path, `/`-separated, no trailing slash. The root itself
	/// is not an entry.
	pub path: String,
	pub kind: InodeKind,
	pub size: u64,
	pub mode: u32,
}

/// What a rebuild moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebuildStats {
	pub dirs: u64,
	pub files: u64,
	pub bytes: u64,
}

impl Store {
	/// Every directory and file in the volume, depth-first, parents before
	/// children.
	///
	/// Pre-order matters: a caller replaying this list into an empty volume can
	/// create each entry as it arrives, because its parent is always already
	/// there. The root (`/`) is not listed — it exists by definition.
	///
	/// A directory larger than `DirNode`'s ~400-entry bound is not a problem
	/// here (this is a read), but a *destination* of such a tree is; see
	/// `EMBEDDING.md` §5.
	pub fn walk(&self) -> Result<Vec<WalkEntry>> {
		let mut out = Vec::new();
		self.walk_into("/", &mut out, 0)?;
		Ok(out)
	}

	fn walk_into(&self, dir: &str, out: &mut Vec<WalkEntry>, depth: usize) -> Result<()> {
		// A directory tree can be nested arbitrarily deeply on a crafted volume —
		// each level is one inode and one `DirNode` — and this descends one frame
		// per level, so an unbounded tree is the same stack overflow as a deep
		// `DirNode` chain. See `path::MAX_DIR_DEPTH`.
		if depth >= crate::path::MAX_DIR_DEPTH {
			return Err(crate::path::too_deep("a directory tree", depth));
		}

		// The listing order a directory hands back is the on-disk `DirNode`
		// order, which is not sorted. Sort here so a rebuild is deterministic:
		// two runs over the same source produce byte-identical destinations
		// (given the same CoW allocator), which is what makes the verification
		// in `rebuild_into` a meaningful check rather than a coincidence.
		let mut entries = self.read_dir(dir)?;
		entries.sort_by(|a, b| a.name.cmp(&b.name));
		for e in entries {
			let path = if dir == "/" {
				format!("/{}", e.name)
			} else {
				format!("{dir}/{}", e.name)
			};
			out.push(WalkEntry {
				path: path.clone(),
				kind: e.kind,
				size: e.size,
				mode: e.mode,
			});
			if e.kind == InodeKind::Directory {
				self.walk_into(&path, out, depth + 1)?;
			}
		}
		Ok(())
	}
}

/// Copy every entry of `src`'s live tree into `dst`.
///
/// `dst` must be a volume this call may fill — an empty one it has just
/// formatted, in practice. Every write is `write_file` (create-or-replace) and
/// every directory is created only if absent, so replaying a tree into a volume
/// that already carries the store's identity files (`/.ledgerfs/*`) is
/// well-defined rather than an `AlreadyExists` error.
///
/// Symlinks are refused rather than dereferenced: this host path has no
/// symlink writer, so silently following one would copy the target's bytes to
/// the link's name and quietly change what the tree means.
///
/// The caller is expected to verify before trusting the result; `walk()` on
/// both volumes is the check a compaction pass uses.
pub fn rebuild_into(src: &Store, dst: &mut Store) -> Result<RebuildStats> {
	let mut stats = RebuildStats::default();
	for entry in src.walk()? {
		match entry.kind {
			InodeKind::Directory => {
				if dst.stat(&entry.path).is_err() {
					dst.mkdir(&entry.path, entry.mode)?;
				}
				stats.dirs += 1;
			}
			InodeKind::RegularFile => {
				let data = src.read_file(&entry.path)?;
				dst.write_file(&entry.path, &data, entry.mode)?;
				stats.files += 1;
				stats.bytes += data.len() as u64;
			}
			InodeKind::Symlink => {
				return Err(Error::Unsupported {
					detail: format!(
						"{} is a symlink; rebuild_into does not follow or recreate symlinks",
						entry.path
					),
				});
			}
		}
	}
	Ok(stats)
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use crate::MemBlockDevice;

	const BLOCKS: u64 = 4096;

	fn volume() -> Result<Store> {
		Store::format_app(Box::new(MemBlockDevice::new(BLOCKS)), "testapp")
	}

	#[test]
	fn test_walk_is_depth_first_parents_before_children() -> Result<()> {
		let mut s = volume()?;
		s.mkdir("a", 0o755)?;
		s.mkdir("a/b", 0o755)?;
		s.write_new_file("a/b/deep.txt", b"deep", 0o644)?;
		s.write_new_file("top.txt", b"top", 0o644)?;

		let paths: Vec<String> = s.walk()?.into_iter().map(|e| e.path).collect();
		let pos = |p: &str| {
			paths
				.iter()
				.position(|x| x == p)
				.unwrap_or_else(|| panic!("{p} missing from {paths:?}"))
		};
		assert!(
			pos("/a") < pos("/a/b"),
			"a directory must precede its children: {paths:?}"
		);
		assert!(pos("/a/b") < pos("/a/b/deep.txt"), "{paths:?}");
		// Sorted siblings: the identity file sorts before `a`.
		assert_eq!(paths[0], "/.ledgerfs", "{paths:?}");
		assert!(paths.contains(&"/top.txt".to_string()));
		Ok(())
	}

	#[test]
	fn test_rebuild_copies_the_live_tree_and_reclaims_space() -> Result<()> {
		let mut src = volume()?;
		src.mkdir("notes", 0o755)?;
		src.write_new_file("notes/a.md", b"alpha", 0o644)?;
		// Replacing a file leaves the old object behind — the CoW garbage a
		// rebuild is meant to drop.
		for _ in 0..64 {
			src.write_file("notes/a.md", b"alpha", 0o644)?;
		}
		let src_garbage = src.free_bytes().expect("free bytes");

		let mut dst = volume()?;
		let stats = rebuild_into(&src, &mut dst)?;
		assert_eq!(stats.dirs, 2, "notes plus .ledgerfs");
		assert_eq!(stats.files, 3, "a.md plus the two identity files");

		// The tree is identical, entry for entry.
		assert_eq!(src.walk()?, dst.walk()?);

		// ...and the copy is smaller, because the source's dead objects were
		// not carried over.
		let dst_free = dst.free_bytes().expect("free bytes");
		assert!(
			dst_free > src_garbage,
			"rebuild must reclaim the replaced-file garbage: src {src_garbage} vs dst {dst_free}"
		);
		Ok(())
	}

	#[test]
	fn test_rebuild_preserves_identity_and_content() -> Result<()> {
		let mut src = volume()?;
		src.write_new_file("big.bin", &vec![7u8; 40_000], 0o600)?;

		let mut dst = volume()?;
		rebuild_into(&src, &mut dst)?;

		assert_eq!(
			dst.app()?,
			Some("testapp".to_string()),
			"identity files survive the copy"
		);
		assert_eq!(dst.read_file("big.bin")?, vec![7u8; 40_000]);
		assert_eq!(dst.stat("big.bin")?.mode, 0o600, "mode is replayed, not defaulted");
		Ok(())
	}
}

// endregion: --- Tests
