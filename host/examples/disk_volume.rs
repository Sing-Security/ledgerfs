//! A volume in a real file on a real disk — the hosted case.
//!
//! ```text
//! cargo run --example disk_volume
//! ```
//!
//! `embed` showed an application owning its volume as a byte string. This is
//! the other consumer: a host that keeps the volume at a path, where the OS
//! semantics come with the deal — one writer, any number of readers, commits
//! end in `fsync`, and a hint sidecar appears beside the file when the writer
//! closes, so the next process to open the volume skips the cold scan.
//!
//! Everything after the format is ordinary filesystem work; the point is the
//! frame around it. The volume is left in place, with the command that
//! inspects it printed at the end.

use ledgerfs_host::{Error, FileBlockDevice, Result, Store};

/// 8 MiB — plenty for a demo volume, and `create` sizes the file to exactly
/// this, block-aligned.
const VOLUME_BYTES: u64 = 8 * 1024 * 1024;

fn main() {
	if let Err(e) = run() {
		eprintln!("disk_volume: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	// -- 1. Geometry. The volume is the whole file, so `create` is mkfs's
	// stage: it sizes the file and opens it for writing. It does not erase —
	// erasing belongs to the format, so the guarantee holds for a partition
	// inside an image too, which never had a `create` call at all.
	let dir = std::env::temp_dir().join("ledgerfs-disk-example");
	std::fs::create_dir_all(&dir).map_err(Error::Io)?;
	let volume = dir.join("app.vol");
	let _ = std::fs::remove_file(&volume);
	let _ = std::fs::remove_file(dir.join("app.vol.hints"));

	let device = FileBlockDevice::create(&volume, VOLUME_BYTES)?;
	println!("sized a volume file at {}", volume.display());

	// -- 2. Format: the same mkfs the CLI runs against a drive — four
	// superblock copies, a genesis commit, every prior block erased.
	let mut store = Store::format_app(Box::new(device), "disk-example")?;
	println!("formatted: app 'disk-example', commit seq {}", store.commit_seq());

	// -- 3. Use it. From here on nothing is file-specific — these are the
	// same calls `embed` made against a buffer.
	store.mkdir("/notes", 0o755)?;
	store.write_new_file("/notes/hello.txt", b"a volume in a plain file\n", 0o644)?;
	println!("wrote /notes/hello.txt, commit seq {}", store.commit_seq());

	// -- 4. One writer per volume. The writable handle holds an advisory
	// lock on the file, so a second writer — another process, or a second
	// open in this one — is refused with an error that names the file,
	// instead of two allocators placing objects on the same free blocks.
	match FileBlockDevice::open_rw(&volume, 0, VOLUME_BYTES) {
		Err(Error::Locked { path }) => {
			println!("a second writable open was refused: {} is locked", path.display());
		}
		Err(e) => return Err(e),
		Ok(_) => unreachable!("the advisory lock did not hold"),
	}

	// -- 5. Readers take no lock, so an inspector coexists with the live
	// writer. A second process doing this right now would see the same bytes.
	let peek = Store::open(Box::new(FileBlockDevice::open(&volume, 0, VOLUME_BYTES)?))?;
	let hello = peek.read_file("/notes/hello.txt")?;
	println!(
		"a read-only open sees /notes/hello.txt: {:?}",
		String::from_utf8_lossy(&hello)
	);
	drop(peek);

	// -- 6. Close the writer. Its drop flushes the hint sidecar beside the
	// volume — the object-id→block map that turns the *next* process's first
	// lookup into a file read instead of a whole-volume scan.
	drop(store);
	let sidecar = dir.join("app.vol.hints");
	let sidecar_len = std::fs::metadata(&sidecar).map(|m| m.len()).unwrap_or(0);
	println!(
		"writer closed; hint sidecar {} ({} bytes)",
		if sidecar_len > 0 { "written:" } else { "absent:" },
		sidecar_len
	);

	// -- 7. Reopen. A fresh process starts warm: the sidecar is loaded,
	// every entry re-verified against the block it names, and the scan the
	// first lookup would otherwise pay is gone.
	let reopened = Store::open(Box::new(FileBlockDevice::open(&volume, 0, VOLUME_BYTES)?))?;
	let hello = reopened.read_file("/notes/hello.txt")?;
	println!(
		"reopened warm: {:?} back, commit seq {}",
		String::from_utf8_lossy(&hello),
		reopened.commit_seq()
	);

	println!(
		"\nvolume left at {} — inspect it:\n  cargo run --bin lfs-read -- {} ls / --bare",
		volume.display(),
		volume.display()
	);
	Ok(())
}
