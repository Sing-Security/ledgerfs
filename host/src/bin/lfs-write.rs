//! `lfs-write` — format and modify a LedgerFS volume from the host.
//!
//! The mutating counterpart to `lfs-read`, and the harness for this crate's
//! writable side. Two kinds of target, which is the whole point of the
//! [`BlockDevice`] seam:
//!
//! - a **partitioned image**, where the volume is a partition (`--part`), and
//! - a **bare file**, where the file *is* the volume (`--bare`).
//!
//! Both go through the same `Store`, the same commit protocol and the same
//! on-disk format; only the device differs.
//!
//! ```text
//! lfs-write <TARGET> mkfs   [--bare] [--blocks N] [--part system|data]
//! lfs-write <TARGET> info   [--bare] [--part system|data]
//! lfs-write <TARGET> mkdir  <PATH>   [--bare] [--part system|data]
//! lfs-write <TARGET> put    <LOCAL> <PATH> [--bare] [--part system|data]
//! lfs-write <TARGET> append <LOCAL> <PATH> [--bare] [--part system|data]
//! lfs-write <TARGET> rm     <PATH>   [--bare] [--part system|data]
//! lfs-write <TARGET> mv     <FROM> <TO> [--bare] [--part system|data]
//! lfs-write <TARGET> verify <PATH> [LOCAL] [--bare] [--part system|data]
//! lfs-write <SRC>    compact <DEST>  [--bare]
//! ```
//!
//! [`BlockDevice`]: ledgerfs_host::BlockDevice

use std::path::{Path, PathBuf};

use ledgerfs_host::{
	BLOCK_SIZE, BlockDevice, Error, FileBlockDevice, InodeKind, PartitionKind, Result, Store, find_partition,
	rebuild_into,
};

// region:    --- Cli

struct Cli {
	target: PathBuf,
	command: String,
	positional: Vec<String>,
	part: PartitionKind,
	/// Treat the target as a whole volume rather than a partitioned image.
	bare: bool,
	/// Volume size for `mkfs`, in 4 KiB blocks.
	blocks: Option<u64>,
}

/// Which partition `mkfs`/`mkdir`/`put` touch when `--bare` is not given.
/// The data volume is what an application writes to; the system volume is the
/// sealed OS image, and not something a host tool should be mutating.
const DEFAULT_PART: PartitionKind = PartitionKind::Data;

const USAGE: &str = "\
usage: lfs-write <TARGET> <COMMAND> [ARGS] [--bare] [--part system|data] [--blocks N]

  mkfs [--blocks N]     format TARGET as a LedgerFS volume
  info                  how the volume was obtained, whether it takes writes, and the space left
  mkdir <PATH>          create a directory
  put <LOCAL> <PATH>    write a host file to PATH (creating it)
  append <LOCAL> <PATH> append a host file's bytes to PATH (creating it if absent)
  rm <PATH>             delete a file
  mv <FROM> <TO>        rename a file within one directory
  verify <PATH> [LOCAL] read PATH back; with LOCAL, compare the two byte for byte
  compact <DEST>        rebuild TARGET into a new volume file, verified (both --bare)

  --bare                TARGET is a whole volume file, not a partitioned image
  --part KIND           which partition to write, 'data' (default) or 'system'
  --blocks N            volume size for --bare mkfs, in 4096-byte blocks";

fn parse_args(argv: Vec<String>) -> Result<Cli> {
	let mut target: Option<PathBuf> = None;
	let mut command: Option<String> = None;
	let mut positional = Vec::new();
	let mut part = DEFAULT_PART;
	let mut bare = false;
	let mut blocks = None;

	let mut it = argv.into_iter();
	while let Some(arg) = it.next() {
		match arg.as_str() {
			"--help" | "-h" => return Err(Error::Usage(USAGE.to_string())),
			"--bare" => {
				bare = true;
				continue;
			}
			"--part" => {
				let value = it
					.next()
					.ok_or_else(|| Error::Usage(format!("--part needs a value\n\n{USAGE}")))?;
				part = PartitionKind::parse(&value)
					.ok_or_else(|| Error::Usage(format!("unknown partition '{value}' — expected system or data")))?;
				continue;
			}
			"--blocks" => {
				let value = it
					.next()
					.ok_or_else(|| Error::Usage(format!("--blocks needs a value\n\n{USAGE}")))?;
				blocks = Some(
					value
						.parse::<u64>()
						.map_err(|_| Error::Usage(format!("--blocks: '{value}' is not a block count")))?,
				);
				continue;
			}
			_ => {}
		}

		if target.is_none() {
			target = Some(PathBuf::from(arg));
		} else if command.is_none() {
			command = Some(arg);
		} else {
			positional.push(arg);
		}
	}

	let (target, command) = match (target, command) {
		(Some(target), Some(command)) => (target, command),
		_ => return Err(Error::Usage(USAGE.to_string())),
	};

	Ok(Cli {
		target,
		command,
		positional,
		part,
		bare,
		blocks,
	})
}

impl Cli {
	fn required_path(&self) -> Result<String> {
		match self.positional.as_slice() {
			[path] => Ok(normalise(path)),
			_ => Err(Error::Usage(format!(
				"'{}' needs exactly one path\n\n{USAGE}",
				self.command
			))),
		}
	}

	fn local_and_path(&self) -> Result<(PathBuf, String)> {
		match self.positional.as_slice() {
			[local, path] => Ok((PathBuf::from(local), normalise(path))),
			_ => Err(Error::Usage(format!(
				"put needs a host file and a target path\n\n{USAGE}"
			))),
		}
	}

	/// The volume path, and the host file to compare it against if one was given.
	fn path_and_optional_local(&self) -> Result<(String, Option<PathBuf>)> {
		match self.positional.as_slice() {
			[path] => Ok((normalise(path), None)),
			[path, local] => Ok((normalise(path), Some(PathBuf::from(local)))),
			_ => Err(Error::Usage(format!(
				"verify takes a volume path, and optionally the host file to compare it against\n\n{USAGE}"
			))),
		}
	}
}

// endregion: --- Cli

/// `fn main() -> Result<()>` would print the error's `Debug`, not its
/// `Display`, bypassing every hand-written message in `error.rs`. So the work is
/// in `run` and `main` reports it properly.
fn main() {
	if let Err(e) = run() {
		eprintln!("lfs-write: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	let argv: Vec<String> = std::env::args().skip(1).collect();

	let cli = match parse_args(argv) {
		Ok(cli) => cli,
		Err(e) => usage_exit(&e.to_string()),
	};

	// A partitioned target without an image behind it fails deep inside the MBR
	// parser with a message about a signature byte. Checked here instead, where
	// the message can name the flag that was probably meant.
	if !cli.bare {
		let is_file = cli.target.is_file();
		if !is_file {
			usage_exit(&format!(
				"{}: no such file — pass --bare if this is a whole volume rather than a partitioned image\n\n{USAGE}",
				cli.target.display()
			));
		}
	}

	match cli.command.as_str() {
		"mkfs" => cmd_mkfs(&cli),
		"info" => cmd_info(&cli),
		"mkdir" => cmd_mkdir(&cli),
		"put" => cmd_put(&cli),
		"append" => cmd_append(&cli),
		"rm" => cmd_remove(&cli),
		"mv" => cmd_rename(&cli),
		"verify" => cmd_verify(&cli),
		"compact" => cmd_compact(&cli),
		other => usage_exit(&format!("unknown command '{other}'\n\n{USAGE}")),
	}
}

/// A usage error is not a failure of the tool, so it goes to stdout and exits
/// `2` — distinct from the `1` a volume that said no gives. A script can then
/// tell "you called me wrong" from "the filesystem refused".
fn usage_exit(message: &str) -> ! {
	println!("{message}");
	std::process::exit(2);
}

// region:    --- Commands

fn cmd_mkfs(cli: &Cli) -> Result<()> {
	let (device, total_blocks) = open_target(cli, Access::Create)?;
	let store = Store::format(device, total_blocks)?;

	println!("formatted   {}", cli.target.display());
	println!(
		"  target    {}",
		if cli.bare {
			"bare volume file".to_string()
		} else {
			format!("{} partition", cli.part.label())
		}
	);
	println!(
		"  blocks    {} ({} bytes)",
		total_blocks,
		total_blocks * BLOCK_SIZE as u64
	);
	println!("  commit    seq {}", store.commit_seq());
	println!("  root      {}", store.persisted_root_ref().object_id.to_hex());

	Ok(())
}

fn cmd_info(cli: &Cli) -> Result<()> {
	// Read-write where the file allows it, and only read-only as a fallback.
	// `info` writes nothing, but `Store::is_writable` answers for the *handle*
	// as well as the volume, so a read-only handle would report every healthy
	// volume as unwritable — the wrong answer to the question this command is
	// being asked.
	let (device, _) = match open_target(cli, Access::ReadWrite) {
		Ok(opened) => opened,
		Err(_) => open_target(cli, Access::ReadOnly)?,
	};
	let store = Store::open(device)?;

	let volume = &store.fs().volume;
	println!("target      {}", cli.target.display());
	println!("  blocks    {}", volume.total_blocks());
	println!("  commit    seq {}", store.commit_seq());
	println!("  root      {}", store.persisted_root_ref().object_id.to_hex());
	println!(
		"  root_hint {}{}",
		store.persisted_root_ref().hint.0,
		if store.persisted_root_ref().hint.0 == 0 {
			"  (0 = scan for it)"
		} else {
			""
		}
	);

	let status = volume.copy_status();
	println!(
		"  copies    {}/{} decoded{}",
		status.decoded_count(),
		status.lbas.len(),
		if status.is_healthy() {
			", all agree"
		} else {
			" — they DISAGREE"
		}
	);

	println!();
	println!("origin      {:?}", store.origin());
	match store.write_refusal() {
		None => println!("writable    yes"),
		Some(refusal) => println!("writable    no — {refusal}"),
	}

	// Free space only ever falls, so this is the number an always-on store has
	// to watch — see `EMBEDDING.md`.
	match store.free_bytes() {
		Some(free) => println!(
			"free        {free} of {} bytes ({:.0}% left)",
			store.capacity_bytes(),
			100.0 * free as f64 / store.capacity_bytes().max(1) as f64
		),
		None => println!("free        unknown — no allocator snapshot"),
	}
	match store.app()? {
		Some(app) => println!(
			"app         {app} (contract {})",
			store.contract_version()?.unwrap_or_else(|| "?".to_string())
		),
		None => println!("app         none — not an application store"),
	}

	Ok(())
}

fn cmd_mkdir(cli: &Cli) -> Result<()> {
	let path = cli.required_path()?;
	let (device, _) = open_target(cli, Access::ReadWrite)?;
	let mut store = Store::open(device)?;

	store.mkdir(&path, 0o755)?;

	println!("created     {path} (commit seq {})", store.commit_seq());
	Ok(())
}

fn cmd_put(cli: &Cli) -> Result<()> {
	let (local, path) = cli.local_and_path()?;
	let data = std::fs::read(&local).map_err(Error::Io)?;

	let (device, _) = open_target(cli, Access::ReadWrite)?;
	let mut store = Store::open(device)?;

	store.write_new_file(&path, &data, 0o644)?;

	println!(
		"wrote       {path} ({} bytes from {}, commit seq {})",
		data.len(),
		local.display(),
		store.commit_seq()
	);
	Ok(())
}

/// Append a host file's bytes to `PATH`, creating the file if it is absent —
/// the append-only log case the store is built around.
fn cmd_append(cli: &Cli) -> Result<()> {
	let (local, path) = cli.local_and_path()?;
	let data = std::fs::read(&local).map_err(Error::Io)?;

	let (device, _) = open_target(cli, Access::ReadWrite)?;
	let mut store = Store::open(device)?;

	store.append(&path, &data, 0o644)?;
	let size = store.stat(&path)?.size;

	println!(
		"appended    {} bytes to {path} (now {size} bytes, commit seq {})",
		data.len(),
		store.commit_seq()
	);
	Ok(())
}

/// Delete a file. A directory is refused rather than emptied — there is no
/// recursive delete.
fn cmd_remove(cli: &Cli) -> Result<()> {
	let path = cli.required_path()?;

	let (device, _) = open_target(cli, Access::ReadWrite)?;
	let mut store = Store::open(device)?;

	store.remove(&path)?;

	println!("removed     {path} (commit seq {})", store.commit_seq());
	Ok(())
}

/// Rename a file within one directory. A cross-directory move is refused by the
/// store, and the refusal explains why.
fn cmd_rename(cli: &Cli) -> Result<()> {
	let (from, to) = match cli.positional.as_slice() {
		[a, b] => (normalise(a), normalise(b)),
		_ => return Err(Error::Usage(format!("mv needs a source and a destination\n\n{USAGE}"))),
	};

	let (device, _) = open_target(cli, Access::ReadWrite)?;
	let mut store = Store::open(device)?;

	store.rename(&from, &to)?;

	println!("renamed     {from} -> {to} (commit seq {})", store.commit_seq());
	Ok(())
}

/// Read a file back out of the volume, after the fact and through a fresh
/// mount — the only check that proves what landed is what a later reader sees.
///
/// With a host file as the second argument the comparison is **byte for byte**,
/// which is the whole point: a size-only check passes for a file whose every
/// chunk was written through a stale reference and whose tail happens to be the
/// right length, and that is exactly the bug this path was written to catch.
fn cmd_verify(cli: &Cli) -> Result<()> {
	let (path, against) = cli.path_and_optional_local()?;
	let (device, _) = open_target(cli, Access::ReadOnly)?;
	let store = Store::open(device)?;

	let data = store.read_file(&path)?;
	let inode = store.stat(&path)?;

	println!("path        {path}");
	println!("  size      {}", inode.size);
	println!("  object    {}", inode.object_id.to_hex());
	println!("  content   {}", inode.content_ref.object_id.to_hex());
	println!("  bytes     {} read back", data.len());

	let Some(local) = against else {
		return Ok(());
	};

	let expected = std::fs::read(&local).map_err(Error::Io)?;
	if data != expected {
		// The offset of the first difference is the useful part of a mismatch:
		// it says whether the damage is at a chunk seam or throughout.
		let at = data
			.iter()
			.zip(expected.iter())
			.position(|(a, b)| a != b)
			.unwrap_or(data.len().min(expected.len()));
		return Err(Error::ContentMismatch {
			path,
			local: local.display().to_string(),
			detail: format!(
				"{} bytes read back, {} in {}; first difference at byte {at}",
				data.len(),
				expected.len(),
				local.display()
			),
		});
	}

	println!("  match     {} bytes identical to {}", expected.len(), local.display());
	Ok(())
}

/// Rebuild a volume into a fresh one — the only way to reclaim what CoW leaves
/// behind, since nothing in the format frees anything in place.
///
/// `lfs-write <SRC> compact <DEST> --bare`. Both are whole volume files: an
/// image partition cannot be *created* by this command, so a partitioned source
/// has nowhere to go. The source is opened read-only and is not modified — the
/// new volume is a separate file, which the caller renames over the old one
/// when it is satisfied.
fn cmd_compact(cli: &Cli) -> Result<()> {
	if !cli.bare {
		return Err(Error::Usage(format!(
			"compact needs --bare: it creates a new whole volume file, which a partition of an \
			 image cannot be\n\n{USAGE}"
		)));
	}
	let dest = match cli.positional.as_slice() {
		[d] => PathBuf::from(d),
		_ => {
			return Err(Error::Usage(format!(
				"compact takes exactly one destination path\n\n{USAGE}"
			)));
		}
	};
	if dest.exists() {
		return Err(Error::Usage(format!(
			"{} already exists — compact writes a *new* volume; pass a path that does not exist, \
			 then rename it over the old one",
			dest.display()
		)));
	}

	let src_len = std::fs::metadata(&cli.target).map_err(Error::Io)?.len();
	if !src_len.is_multiple_of(BLOCK_SIZE as u64) {
		return Err(Error::Usage(format!(
			"{}: {} bytes is not a whole number of {} byte blocks",
			cli.target.display(),
			src_len,
			BLOCK_SIZE
		)));
	}

	// Read-only source: no write lock, so this works on a volume another
	// process holds — and on one with no free space left to write into.
	let src = Store::open(Box::new(FileBlockDevice::open(&cli.target, 0, src_len)?))?;
	let expected = src.walk()?;
	let free_before = src.free_bytes();

	let mut dst = Store::format(
		Box::new(FileBlockDevice::create(&dest, src_len)?),
		src_len / BLOCK_SIZE as u64,
	)?;
	let stats = rebuild_into(&src, &mut dst)?;
	let free_after = dst.free_bytes();
	drop(dst);

	// Verify through a fresh mount before reporting success. The caller is
	// about to delete the original on the strength of this answer.
	let check = Store::open(Box::new(FileBlockDevice::open(&dest, 0, src_len)?))?;
	let got = check.walk()?;
	for (want, have) in expected.iter().zip(got.iter()) {
		if want.path != have.path || want.kind != have.kind || want.size != have.size {
			return Err(Error::ContentMismatch {
				path: want.path.clone(),
				local: dest.display().to_string(),
				detail: format!(
					"{:?}/{} bytes became {:?}/{} bytes",
					want.kind, want.size, have.kind, have.size
				),
			});
		}
	}
	if expected.len() != got.len() {
		return Err(Error::ContentMismatch {
			path: "/".to_string(),
			local: dest.display().to_string(),
			detail: format!("{} entries in the source, {} in the copy", expected.len(), got.len()),
		});
	}
	// Sizes agree does not mean bytes agree: a file whose every chunk was
	// written through a stale reference can come out the right length. The
	// caller deletes the original on the strength of this check.
	for entry in &expected {
		if entry.kind != InodeKind::RegularFile {
			continue;
		}
		let (a, b) = (src.read_file(&entry.path)?, check.read_file(&entry.path)?);
		if a != b {
			return Err(Error::ContentMismatch {
				path: entry.path.clone(),
				local: dest.display().to_string(),
				detail: format!("{} bytes in the source, {} in the copy", a.len(), b.len()),
			});
		}
	}

	println!("compacted   {} -> {}", cli.target.display(), dest.display());
	println!(
		"  entries   {} ({} dirs, {} files, {} bytes)",
		expected.len(),
		stats.dirs,
		stats.files,
		stats.bytes
	);
	println!(
		"  free      {} -> {} bytes",
		free_before.unwrap_or(0),
		free_after.unwrap_or(0)
	);
	println!("  verified  every path, size and byte matches the source");
	println!("  next      rename {} over {}", dest.display(), cli.target.display());
	Ok(())
}

// endregion: --- Commands

// region:    --- Target resolution

/// What a command needs from the device, so the file is opened with enough
/// permission — a read-only handle turns every commit into `EBADF` at the first
/// `write_at`, several layers below anything that could explain it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
	/// Make a volume of the requested size here, discarding what was there.
	Create,
	/// Open an existing volume for reading and writing.
	ReadWrite,
	/// Open an existing volume for reading only.
	ReadOnly,
}

/// Open `cli.target` as a device, and report how many blocks the volume has.
///
/// The two modes this crate exists to serve — a partitioned image and a bare
/// volume file — differ only here.
fn open_target(cli: &Cli, access: Access) -> Result<(BoxedDevice, u64)> {
	if cli.bare {
		let device = match access {
			Access::Create => FileBlockDevice::create(&cli.target, bare_size(cli)?)?,
			Access::ReadWrite => FileBlockDevice::open_rw(&cli.target, 0, existing_len(&cli.target)?)?,
			Access::ReadOnly => FileBlockDevice::open(&cli.target, 0, existing_len(&cli.target)?)?,
		};

		let total_blocks = device.size_bytes() / BLOCK_SIZE as u64;
		Ok((Box::new(device), total_blocks))
	} else {
		let partition = find_partition(&cli.target, cli.part)?;
		let (start, size) = (partition.start_bytes(), partition.size_bytes());

		let device: BoxedDevice = match access {
			Access::ReadOnly => Box::new(FileBlockDevice::open(&cli.target, start, size)?),
			// `mkfs` on a partition cannot choose its size — the partition table
			// already fixed it — so creating and re-opening are the same thing.
			Access::Create | Access::ReadWrite => Box::new(FileBlockDevice::open_rw(&cli.target, start, size)?),
		};

		Ok((device, size / BLOCK_SIZE as u64))
	}
}

/// The size `mkfs --bare` should give the file.
fn bare_size(cli: &Cli) -> Result<u64> {
	if let Some(blocks) = cli.blocks {
		return Ok(blocks * BLOCK_SIZE as u64);
	}

	// No `--blocks`, so the only size available is the one the file already
	// has. A file that does not exist yet has none, and `mkfs` on a file of
	// size zero is not a volume.
	match existing_len(&cli.target) {
		Ok(len) if len > 0 => Ok(len),
		_ => Err(Error::Usage(format!(
			"mkfs --bare {}: the file does not exist, so its size has to be chosen — pass --blocks N\n\n{USAGE}",
			cli.target.display()
		))),
	}
}

fn existing_len(path: &Path) -> Result<u64> {
	std::fs::metadata(path).map(|m| m.len()).map_err(Error::Io)
}

// endregion: --- Target resolution

// region:    --- Support

/// `BlockDevice` is a trait but not a type, so this keeps `open_target`'s
/// signature readable.
type BoxedDevice = Box<dyn BlockDevice>;

/// `/data` and `/data/` are the same path, and so are `data` and `/data`.
fn normalise(path: &str) -> String {
	let trimmed = path.trim_end_matches('/');
	if trimmed.starts_with('/') {
		return if trimmed.is_empty() {
			"/".to_string()
		} else {
			trimmed.to_string()
		};
	}
	format!("/{trimmed}")
}

// endregion: --- Support
