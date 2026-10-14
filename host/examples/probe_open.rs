//! Where the time goes when a campaign volume is opened.
//!
//! A cold `Store::open` on a real `bugs` campaign costs roughly a millisecond
//! per megabyte — a 1.1 GiB volume takes about a second — and the console
//! re-opens every volume on every poll of `/api/overview`, so this is the
//! single hottest path in the operator UI. Before changing anything it is worth
//! knowing *which* step scales with volume size, because the fix differs:
//!
//! - If the mount itself (superblock copies, allocator snapshot) is the cost,
//!   the fix is to stop re-opening — cache the handle.
//! - If the cost is an object lookup falling back to a linear scan, the fix is
//!   a hint that survives a reopen, so the scan never happens.
//!
//! Run it against a real volume:
//!
//! ```text
//! cargo run --release --offline --example probe_open -- campaigns/anduril.ledgerfs
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crucible_ledgerfs_host::{FileBlockDevice, HostFs, Store};

type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

/// Run `f` and report how long it took under `label`.
fn timed<T, F: FnOnce() -> T>(label: &str, f: F) -> T {
	let start = Instant::now();
	let out = f();
	println!("  {label:<44} {:>10} µs", start.elapsed().as_micros());
	out
}

fn main() -> Result<()> {
	let path: PathBuf = std::env::args()
		.nth(1)
		.unwrap_or_else(|| "campaigns/anduril.ledgerfs".to_string())
		.into();
	let size = std::fs::metadata(&path)?.len();
	println!("{}  ({:.1} MiB)\n", path.display(), size as f64 / (1024.0 * 1024.0));

	// -- The mount on its own: superblock copies + allocator snapshot.
	let mount = {
		let p = path.clone();
		timed("HostFs::mount_device (superblock + root)", move || -> Result<_> {
			let device = FileBlockDevice::open(&p, 0, std::fs::metadata(&p)?.len())?;
			Ok(HostFs::mount_device(Box::new(device))?)
		})
	}?;
	let superblock = mount.volume.superblock.clone();
	println!(
		"      total_blocks={} max_object_bytes={} active_commit_seq={}",
		mount.volume.total_blocks(),
		superblock.max_object_bytes(),
		superblock.active_commit_seq,
	);

	// -- The root listing: resolution of `/` plus one read_dir.
	let _ = timed("HostFs::read_dir(\"/\")", || mount.read_dir("/"));

	// -- A second listing of the same directory, now that any cache is warm.
	let _ = timed("HostFs::read_dir(\"/\") again", || mount.read_dir("/"));

	// -- A full `Store::open`, which is what every console endpoint pays.
	let full = timed("Store::open (mount + allocator)", || -> Result<Duration> {
		let start = Instant::now();
		let device = FileBlockDevice::open(&path, 0, size)?;
		let _store = Store::open(Box::new(device))?;
		Ok(start.elapsed())
	})?;
	println!("      ({:.1} ms end to end)", full.as_secs_f64() * 1000.0);

	// -- And what the console actually asks of it: the app marker, one small
	//    file read, and the findings listing.
	let store = {
		let device = FileBlockDevice::open(&path, 0, size)?;
		Store::open(Box::new(device))?
	};
	let app = timed("Store::app()", || store.app());
	println!("      app={app:?}");
	let _ = timed("Store::read_file(\"/.ledgerfs/state.json\")", || {
		store.read_file("/.ledgerfs/state.json")
	});
	let _ = timed("Store::read_dir(\"/findings\")", || store.read_dir("/findings"));

	// -- Where the on-disk hints actually are. If every entry's hint is zero,
	//    a cold handle has nothing to go on and must scan; if the hints are
	//    there, the scan means the reader is throwing them away.
	println!("\non-disk hints:");
	let root_inode = mount.store.inode(mount.root_inode_ref)?;
	println!(
		"  root inode content_ref hint = {}",
		root_inode.content_ref.hint.0
	);
	if !root_inode.content_ref.is_zero() {
		let node = mount.store.dir_node(root_inode.content_ref)?;
		println!(
			"  root DirNode: is_leaf={} entries={} children={}",
			node.is_leaf,
			node.entries.len(),
			node.children.len()
		);
		for e in node.entries.iter().take(12) {
			println!("    {:<28} inode_hint={}", e.name.as_str(), e.inode_hint.0);
		}
	}

	// -- The extent tree of a real transcript chunk: does a chunk's ref carry
	//    its placement, or is it zero (which would make every block of a large
	//    file cost a scan)?
	let listing = store.read_dir("/notes").unwrap_or_default();
	if let Some(entry) = listing.iter().max_by_key(|e| e.size) {
		println!("\nlargest /notes file: {} ({} bytes)", entry.name, entry.size);
		let inode = store.stat(&format!("/notes/{}", entry.name))?;
		println!("  inode content_ref hint = {}", inode.content_ref.hint.0);
		if !inode.content_ref.is_zero() {
			let ext = store.fs().store.extent_node(inode.content_ref)?;
			println!(
				"  extent node: is_leaf={} extents={} children={}",
				ext.is_leaf,
				ext.extents.len(),
				ext.children.len()
			);
			for c in ext.extents.iter().take(6) {
				println!(
					"    extent off={} len={} chunk_hint={}",
					c.logical_offset, c.length, c.chunk_hint.0
				);
			}
		}
	}
	Ok(())
}
