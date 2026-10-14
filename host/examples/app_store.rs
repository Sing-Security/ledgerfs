//! An application using LedgerFS as its own store, the way the contract in
//! `EMBEDDING.md` describes it.
//!
//! ```text
//! cargo run --example app_store
//! ```
//!
//! This is the shape a real consumer — a relay writing a turn's files, say —
//! programs against: stamp the volume as mine, keep a log *in chunks*, replace
//! a state file, do an atomic replace with a scratch name and `rename`, watch
//! free space, rebuild the volume when a long life has filled it with dead
//! objects, and be able to say on the next start "this is my volume and here
//! is what I left on it". Nothing here is in-memory-only trivia: swap
//! `MemBlockDevice` for `FileBlockDevice` and the same calls drive a disk.

use crucible_ledgerfs_host::{Error, MemBlockDevice, Result, Store, rebuild_into};

/// 8 MiB — room for the reserved regions plus real object space to watch fall.
const BLOCKS: u64 = 2048;

fn main() {
	if let Err(e) = run() {
		eprintln!("app_store: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	// -- 1. A volume in this process's heap, stamped as this app's store. The
	// clone is the way back to the bytes after `Store` takes ownership.
	let device = MemBlockDevice::new(BLOCKS);
	let mirror = device.clone();
	let mut store = Store::format_app(Box::new(device), "bugs")?;
	println!(
		"formatted a store for {:?} (contract {}), {} blocks",
		store.app()?.unwrap_or_default(),
		store.contract_version()?.unwrap_or_default(),
		store.total_blocks()
	);

	// -- 2. A log is written in **chunks**, never by appending to one growing
	// file. `append` exists, but every commit CoWs the whole file object, so
	// appending N times to one file writes O(N²) bytes — the way a 4 MiB
	// transcript costs gigabytes. A chunk that is written once and never touched
	// again costs its own size and nothing more.
	store.mkdir("/relay", 0o755)?;
	store.write_new_file("/relay/transcript-000001.jsonl", b"{\"turn\":1,\"action\":\"recon\"}\n", 0o644)?;
	store.write_new_file("/relay/transcript-000002.jsonl", b"{\"turn\":2,\"action\":\"probe\"}\n", 0o644)?;
	println!(
		"wrote 2 transcript chunks: {} bytes free",
		store.free_bytes().unwrap_or(0)
	);

	// -- 3. Replace a state file idempotently. `write_file` overwrites; a plain
	// extend would leave the old tail behind.
	store.write_file("/relay/state.json", b"{\"phase\":\"running\"}\n", 0o644)?;

	// -- 4. An atomic replace: build the new content under a scratch name, then
	// `rename` it into place — one transaction, so a reader never sees a
	// half-written state file.
	store.write_new_file("/relay/state.json.new", b"{\"phase\":\"stopped\"}\n", 0o644)?;
	store.rename("/relay/state.json.new", "/relay/state.json")?;
	println!("replaced /relay/state.json atomically: {:?}",
		String::from_utf8_lossy(&store.read_file("/relay/state.json")?).trim());

	// -- 5. Free space only falls — CoW frees nothing yet, so replacing a file
	// leaves every previous version behind. Sixty state writes: the file is a
	// few dozen bytes, and watch what it costs.
	let before = store.free_bytes().unwrap_or(0);
	for i in 0..60 {
		store.write_file("/relay/state.json", format!("{{\"phase\":\"step-{i}\"}}\n").as_bytes(), 0o644)?;
	}
	let after = store.free_bytes().unwrap_or(0);
	println!(
		"60 writes of a 20-byte file cost {} bytes; {} of {} bytes free",
		before.saturating_sub(after),
		after,
		store.capacity_bytes()
	);

	// -- 6. The way back is a rebuild: copy the live tree into a fresh volume
	// and swap it in. Not in place — a volume has one writer, and the source has
	// to survive until the copy has been checked. On disk this is `bugs compact`.
	let mut compacted = Store::format_app(Box::new(MemBlockDevice::new(BLOCKS)), "bugs")?;
	let stats = rebuild_into(&store, &mut compacted)?;
	println!(
		"rebuilt into a fresh volume: {} dirs, {} files, {} bytes of content; {} bytes free",
		stats.dirs,
		stats.files,
		stats.bytes,
		compacted.free_bytes().unwrap_or(0)
	);

	// Verify before the swap — compare the whole tree, or "it looked fine" is
	// the only thing standing between a rebuild and a destroyed campaign.
	if store.walk()? != compacted.walk()? {
		return Err(Error::ContentMismatch {
			path: "/".to_string(),
			local: "the source tree".to_string(),
			detail: "the rebuilt volume's tree differs; swapping it over the source would lose data"
				.to_string(),
		});
	}
	println!("the rebuild is entry-for-entry identical to the source");

	// -- 7. Delete a file. The name is released and can be reused.
	store.write_new_file("/relay/raw.tmp", b"scratch", 0o644)?;
	store.remove("/relay/raw.tmp")?;

	// -- 8. The volume is a byte string. Drop the store and bring it back, the
	// way an app starts up again.
	let bytes = mirror.bytes_snapshot();
	drop(store);
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;

	// -- 9. First question on any volume handed to us: is it mine?
	match reopened.app()? {
		Some(app) if app == "bugs" => {
			println!("reopened a {app} store (contract {:?})", reopened.contract_version()?);
		}
		Some(other) => {
			return Err(Error::MarkerUnreadable {
				path: "/.ledgerfs/APP".to_string(),
				detail: format!("expected \"bugs\", found {other:?}"),
			});
		}
		None => {
			return Err(Error::MarkerUnreadable {
				path: "/.ledgerfs/APP".to_string(),
				detail: "the marker survived the round trip as absent".to_string(),
			});
		}
	}

	// -- 10. Everything the app left is still there, in order.
	let chunk1 = reopened.read_file("/relay/transcript-000001.jsonl")?;
	let chunk2 = reopened.read_file("/relay/transcript-000002.jsonl")?;
	let state = reopened.read_file("/relay/state.json")?;
	println!("restored log: {} bytes; state: {:?}", chunk1.len() + chunk2.len(),
		String::from_utf8_lossy(&state).trim());

	if chunk1 != b"{\"turn\":1,\"action\":\"recon\"}\n"
		|| chunk2 != b"{\"turn\":2,\"action\":\"probe\"}\n"
	{
		return Err(Error::ContentMismatch {
			path: "/relay/transcript-000001.jsonl".to_string(),
			local: "the chunks the app wrote".to_string(),
			detail: format!("read back {} + {} bytes", chunk1.len(), chunk2.len()),
		});
	}
	if state != b"{\"phase\":\"step-59\"}\n" {
		return Err(Error::ContentMismatch {
			path: "/relay/state.json".to_string(),
			local: "the last state written".to_string(),
			detail: format!("read back {:?}", String::from_utf8_lossy(&state)),
		});
	}

	println!("\nthe store round-tripped: identity, chunked log, atomic replace and rebuild all survived");
	Ok(())
}
