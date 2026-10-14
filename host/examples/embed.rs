//! An application embedding LedgerFS as its own store — no volume file, no
//! path, no mount, nothing on disk unless the application puts it there.
//!
//! ```text
//! cargo run --example embed
//! ```
//!
//! The point of this file is the shape of the code, not the output: an
//! application gets a filesystem by handing [`Store`] a device it owns, and
//! `MemBlockDevice` is that device. Everything past the first two lines is
//! ordinary filesystem work — the same calls `lfs-write` makes against a disk.
//!
//! The other half of the same idea is the last section: the volume is a plain
//! byte string, so an application can snapshot it, store it wherever it keeps
//! things, and restore it later. That is what "embeddable" buys — a real
//! filesystem with a commit protocol, in a buffer.

use crucible_ledgerfs_host::{Error, MemBlockDevice, Result, Store};

/// 4 MiB — enough for the reserved regions plus real object space.
const BLOCKS: u64 = 1024;

/// Something an application might actually keep in a store like this.
const CONFIG: &str = "\
[pipeline]\n\
workers = 8\n\
checkpoint_every = 128\n";

fn main() {
	if let Err(e) = run() {
		eprintln!("embed: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	// -- 1. A volume in this process's own heap.
	//
	// The clone is the way back to the bytes afterwards: `Store` takes
	// ownership of its device, and clones share one buffer.
	let device = MemBlockDevice::new(BLOCKS);
	let mirror = device.clone();

	// -- 2. Format it. This is the same `mkfs` the CLI runs against a drive —
	// same superblock copies, same genesis commit, same on-disk format.
	let mut store = Store::format_device(Box::new(device))?;
	println!(
		"formatted an in-memory volume: {} blocks, commit seq {}",
		store.fs().volume.total_blocks(),
		store.commit_seq()
	);

	// -- 3. Use it. From here on nothing is embedding-specific.
	store.mkdir("/data", 0o755)?;
	store.write_new_file("/data/config.toml", CONFIG.as_bytes(), 0o644)?;
	println!(
		"wrote /data/config.toml: {} bytes, commit seq {}",
		store.read_file("/data/config.toml")?.len(),
		store.commit_seq()
	);

	// -- 4. The volume is now a byte string the application owns. Where it goes
	// next is the application's business: a file, a blob store, a network
	// message. Nothing about it is tied to a path.
	let bytes = mirror.bytes_snapshot();
	println!("volume is {} bytes and relocatable", bytes.len());

	// -- 5. Drop the store and bring the volume back from those bytes, which is
	// what an application does on its next start.
	drop(store);
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;

	let restored = reopened.read_file("/data/config.toml")?;
	println!(
		"reopened from the bytes: {} bytes back, commit seq {}",
		restored.len(),
		reopened.commit_seq()
	);

	// An application that got different bytes back than it stored is not
	// tolerably broken, so the example asserts rather than prints.
	if restored != CONFIG.as_bytes() {
		return Err(Error::ContentMismatch {
			path: "/data/config.toml".to_string(),
			local: "the application's own copy".to_string(),
			detail: format!("read back {} bytes, wrote {}", restored.len(), CONFIG.len()),
		});
	}

	println!("\nthe restored volume is byte-for-byte what was written");
	Ok(())
}
