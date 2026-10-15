//! `lfs-read` — inspect a LedgerFS partition from the host.
//!
//! The acceptance harness for the read path in this crate: point it at a real
//! built image and it either walks the volume or says exactly where it stopped.
//!
//! ```text
//! lfs-read <IMAGE> parts
//! lfs-read <IMAGE> info    [--part system|data]
//! lfs-read <IMAGE> ls      [PATH] [--part system|data]
//! lfs-read <IMAGE> cat     <PATH> [--part system|data]
//! lfs-read <IMAGE> stat    <PATH> [--part system|data]
//! ```

use std::io::Write;
use std::path::PathBuf;

use ledgerfs_host::{
	Error, FileBlockDevice, HostFs, InodeKind, ObjectId, PartitionKind, Result, Volume, find_partition, read_partitions,
};

// region:    --- Cli

struct Cli {
	image: PathBuf,
	command: String,
	positional: Vec<String>,
	part: PartitionKind,
}

/// Default to the system volume: it is the partition an image always has a
/// committed root on. The data volume gets one only after the image's first
/// boot, when a runtime bootstraps a genesis root — so it mounts on a booted
/// image but not on a freshly built one (see the crate docs).
const DEFAULT_PART: PartitionKind = PartitionKind::System;

const USAGE: &str = "\
usage: lfs-read <IMAGE> <COMMAND> [PATH] [--part system|data]

  parts            list the image's MBR partitions
  info             superblock, mount selection, and root summary
  ls [PATH]        list a directory (default: the volume root)
  cat <PATH>       write a file's contents to stdout
  stat <PATH>      show one inode's metadata

  --part KIND      which partition to open, 'system' (default) or 'data'";

fn parse_args(argv: Vec<String>) -> Result<Cli> {
	let mut image: Option<PathBuf> = None;
	let mut command: Option<String> = None;
	let mut positional = Vec::new();
	let mut part = DEFAULT_PART;

	let mut it = argv.into_iter();
	while let Some(arg) = it.next() {
		if arg == "--help" || arg == "-h" {
			return Err(Error::Usage(USAGE.to_string()));
		}
		if arg == "--part" {
			let value = it
				.next()
				.ok_or_else(|| Error::Usage(format!("--part needs a value\n\n{USAGE}")))?;
			part = PartitionKind::parse(&value)
				.ok_or_else(|| Error::Usage(format!("unknown partition '{value}' — expected system or data")))?;
			continue;
		}
		if image.is_none() {
			image = Some(PathBuf::from(arg));
		} else if command.is_none() {
			command = Some(arg);
		} else {
			positional.push(arg);
		}
	}

	let (image, command) = match (image, command) {
		(Some(image), Some(command)) => (image, command),
		_ => return Err(Error::Usage(USAGE.to_string())),
	};

	Ok(Cli {
		image,
		command,
		positional,
		part,
	})
}

impl Cli {
	fn one_path(&self) -> Result<String> {
		match self.positional.as_slice() {
			[] => Ok("/".to_string()),
			[path] => Ok(path.clone()),
			_ => Err(Error::Usage(format!(
				"'{}' takes at most one path\n\n{USAGE}",
				self.command
			))),
		}
	}

	fn required_path(&self) -> Result<String> {
		match self.positional.as_slice() {
			[path] => Ok(path.clone()),
			_ => Err(Error::Usage(format!(
				"'{}' needs exactly one path\n\n{USAGE}",
				self.command
			))),
		}
	}
}

// endregion: --- Cli

/// `fn main() -> Result<()>` would print the error's `Debug`, not its
/// `Display` — every hand-written message in `error.rs` bypassed by the one
/// thing that shows errors to a human. So the work goes in `run` and `main`
/// prints it properly.
fn main() {
	if let Err(e) = run() {
		eprintln!("lfs-read: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	let argv: Vec<String> = std::env::args().skip(1).collect();

	let cli = match parse_args(argv) {
		Ok(cli) => cli,
		Err(e) => usage_exit(&e.to_string()),
	};

	match cli.command.as_str() {
		"parts" => cmd_parts(&cli),
		"info" => cmd_info(&cli),
		"ls" => cmd_ls(&cli),
		"cat" => cmd_cat(&cli),
		"stat" => cmd_stat(&cli),
		other => usage_exit(&format!("unknown command '{other}'\n\n{USAGE}")),
	}
}

/// A usage error is not a failure of the tool, so it goes to stdout and exits
/// `2` — distinct from the `1` `main` gives a volume that said no. A script
/// can then tell "you called me wrong" from "the filesystem refused".
fn usage_exit(message: &str) -> ! {
	println!("{message}");
	std::process::exit(2);
}

// region:    --- Commands

fn cmd_parts(cli: &Cli) -> Result<()> {
	let partitions = read_partitions(&cli.image)?;
	if partitions.is_empty() {
		println!("no partitions");
		return Ok(());
	}
	println!("{:<10} {:>6} {:>12} {:>12}", "kind", "type", "start_lba", "sectors");
	for p in partitions {
		println!(
			"{:<10}   0x{:02X} {:>12} {:>12}",
			p.kind.label(),
			p.part_type,
			p.start_lba,
			p.sectors
		);
	}
	Ok(())
}

fn cmd_info(cli: &Cli) -> Result<()> {
	let partition = find_partition(&cli.image, cli.part)?;

	// Mount the volume on its own first: this reports superblock-level facts
	// even when the root turns out to be unreadable, which is the more useful
	// failure to see.
	let device = FileBlockDevice::open(&cli.image, partition.start_bytes(), partition.size_bytes())?;
	let volume = Volume::mount(&device)?;
	let sb = &volume.superblock;

	println!("image          {}", cli.image.display());
	println!(
		"partition      {} (type 0x{:02X})",
		partition.kind.label(),
		partition.part_type
	);
	println!(
		"  start        {} (byte {})",
		partition.start_lba,
		partition.start_bytes()
	);
	println!(
		"  size         {} bytes ({} blocks)",
		partition.size_bytes(),
		sb.total_blocks
	);
	println!();
	println!("superblock");
	println!("  version      {}", sb.version);
	println!("  block_size   {}", sb.block_size);
	println!("  commit_seq   {}", sb.active_commit_seq);
	println!("  root_id      {}", ObjectId::from_bytes(&sb.active_root_id).to_hex());
	println!(
		"  root_hint    {}{}",
		sb.active_root_hint,
		if sb.active_root_hint == 0 {
			"  (0 = scan for it)"
		} else {
			""
		}
	);
	println!("  rlr          head={} tail={}", sb.rlr_head, sb.rlr_tail);
	println!(
		"  features     compat=0x{:X} ro=0x{:X} incompat=0x{:X}",
		sb.features_compat, sb.features_ro_compat, sb.features_incompat
	);
	println!("  encrypted    {}", sb.is_data_encrypted());
	println!(
		"  limits       object={}B range={}B scan={} blocks",
		sb.max_object_bytes(),
		sb.max_range_read_bytes(),
		sb.max_scan_blocks()
	);
	println!();
	println!("mount");
	println!("  mode         {:?}", volume.selection.mode);
	println!("  replay       {:?}", volume.selection.replay_decision);
	println!(
		"  selected     commit_seq={} from {:?}",
		volume.selection.selected_root.commit_seq, volume.selection.selected_root.source
	);

	// The four copies exist so one damaged copy is survivable. A copy that
	// names a different root than the selected one cannot answer for it.
	let status = volume.copy_status();
	println!(
		"  copies       {}/{} decoded{}",
		status.decoded_count(),
		status.lbas.len(),
		if status.is_healthy() {
			", all agree"
		} else {
			" — see below"
		}
	);
	if !status.is_healthy() {
		for (i, lba) in status.lbas.iter().enumerate() {
			let state = if !status.decoded[i] {
				"unreadable".to_string()
			} else if status.agree[i] {
				"agrees".to_string()
			} else {
				format!("DIFFERS (commit_seq {})", status.commit_seqs[i].unwrap_or(0))
			};
			let marker = if i == status.selected { " <- selected" } else { "" };
			println!("    copy {i} @ block {lba:<6} {state}{marker}");
		}
		println!("  {}", COPY_WARNING);
	}

	// Then the root, via a full open — a volume with no valid root is a real
	// and expected state (the data volume before its first boot), not a tool
	// error.
	match HostFs::open_partition(&cli.image, &partition) {
		Ok(fs) => {
			let inode = fs.store.inode(fs.root_inode_ref)?;
			println!();
			println!("root inode");
			println!("  id           {}", fs.root_inode_ref.object_id.to_hex());
			println!("  hint         {}", fs.root_inode_ref.hint.0);
			println!("  kind         {:?}", inode.kind);
			println!("  mode         {:o}", inode.mode);
			println!("  size         {}", inode.size);
			println!("  content_ref  {}", inode.content_ref.object_id.to_hex());
		}
		Err(e) => {
			println!();
			println!("root           UNREADABLE — {e}");
		}
	}

	Ok(())
}

fn cmd_ls(cli: &Cli) -> Result<()> {
	let path = cli.one_path()?;
	let fs = HostFs::open(&cli.image, cli.part)?;
	let entries = fs.read_dir(&path)?;

	if entries.is_empty() {
		println!("{path}: empty");
		return Ok(());
	}

	for e in entries {
		println!("{} {:>10} {:o} {}", kind_char(e.kind), e.size, e.mode, e.name);
	}
	Ok(())
}

fn cmd_cat(cli: &Cli) -> Result<()> {
	let path = cli.required_path()?;
	let fs = HostFs::open(&cli.image, cli.part)?;
	let data = fs.read_file(&path)?;

	// Raw bytes, not a lossy string: a file on this volume may be anything.
	let stdout = std::io::stdout();
	let mut lock = stdout.lock();
	lock.write_all(&data).map_err(Error::Io)?;
	lock.flush().map_err(Error::Io)?;
	Ok(())
}

fn cmd_stat(cli: &Cli) -> Result<()> {
	let path = cli.required_path()?;
	let fs = HostFs::open(&cli.image, cli.part)?;
	let inode = fs.stat(&path)?;

	println!("path         {path}");
	println!("object_id    {}", inode.object_id.to_hex());
	println!("kind         {:?}", inode.kind);
	println!("mode         {:o}", inode.mode);
	println!("uid/gid      {}/{}", inode.uid, inode.gid);
	println!("size         {}", inode.size);
	println!("content_ref  {}", inode.content_ref.object_id.to_hex());
	println!("content_hint {}", inode.content_ref.hint.0);
	Ok(())
}

// endregion: --- Commands

// region:    --- Support

/// Printed when the superblock copies disagree on the root.
///
/// This is the state a volume is in after a writer that updates the primary
/// copy only has committed to it: the other three are left behind. Worth
/// saying out loud, because the mount still succeeds — nothing else in the
/// stack surfaces it.
const COPY_WARNING: &str = "\
note: the copies disagree, so the redundancy this format is built around is not
  currently being maintained — a writer that updates the primary copy only
  leaves the others behind. Damage to the selected copy rolls this volume
  back to whatever the surviving copies name, not forward to the committed
  state.";

fn kind_char(kind: InodeKind) -> char {
	match kind {
		InodeKind::Directory => 'd',
		InodeKind::RegularFile => '-',
		InodeKind::Symlink => 'l',
	}
}

// endregion: --- Support
