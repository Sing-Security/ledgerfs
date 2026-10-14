//! Where the time goes when the console renders *one campaign's detail*.
//!
//! `GET /campaigns/<name>` is the slowest thing an operator waits on when they
//! open a campaign, and it scales with how much a campaign has in it: a volume
//! with a few files answers in tens of milliseconds, one with hundreds answers
//! in hundreds. The endpoint itself is a fixed list of reads — this example
//! performs exactly that list and times each piece, so the cost can be
//! attributed to a step instead of to "campaigns with a lot of data".
//!
//! The number that matters most is **microseconds per directory entry**.
//! `HostFs::read_dir` reads every child's inode to fill in `kind`/`size`
//! (`src/path.rs`), so a listing is O(entries) object reads — and every one of
//! those is a `locate()` + `read_record()` against a volume whose on-disk hints
//! are all zero. If that per-entry figure is tens of microseconds the cost is
//! the I/O; if it is hundreds, each lookup is still falling back to a scan.
//!
//! ```text
//! cargo run --release --offline --example probe_detail -- campaigns/bbpos.ledgerfs
//! ```

use std::path::PathBuf;
use std::time::Instant;

use crucible_ledgerfs_host::{FileBlockDevice, Store};

type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

fn timed<T, F: FnOnce() -> T>(label: &str, f: F) -> T {
	let start = Instant::now();
	let out = f();
	println!("  {label:<40} {:>9} µs", start.elapsed().as_micros());
	out
}

/// List `dir` and report the time, the entry count, and the per-entry cost.
fn listing(store: &Store, dir: &str) -> usize {
	let start = Instant::now();
	let entries = store.read_dir(dir).unwrap_or_default();
	let us = start.elapsed().as_micros();
	let n = entries.len();
	let per = if n > 0 { us / n as u128 } else { 0 };
	let bytes: u64 = entries.iter().map(|e| e.size).sum();
	println!("  {dir:<28} {us:>8} µs  n={n:<4} {per:>4} µs/entry  {bytes:>8} B listed");
	n
}

fn main() -> Result<()> {
	let path: PathBuf = std::env::args()
		.nth(1)
		.unwrap_or_else(|| "campaigns/bbpos.ledgerfs".to_string())
		.into();
	let size = std::fs::metadata(&path)?.len();
	println!("{}\n", path.display());

	let device = FileBlockDevice::open(&path, 0, size)?;
	let store = Store::open(Box::new(device))?;

	// -- Warm the handle the way the console does before the first listing: one
	//    root read_dir, after which the hint map is loaded and the root's
	//    children are known. Timings below are therefore per-request, not
	//    process-cold.
	let _ = timed("warmup: read_dir(\"/\")", || store.read_dir("/"));

	println!("\nthe listing calls `campaign_detail` makes:");
	let mut total_entries = 0;
	for dir in ["/handovers", "/notes", "/evidence", "/kits", "/findings"] {
		total_entries += listing(&store, dir);
	}
	// `notes` is listed twice by the endpoint (once for the cross-compare
	// subset, once whole), so pay for it twice — that is what the handler does.
	total_entries += listing(&store, "/notes");
	println!("  {:<40} {total_entries:>9}", "TOTAL directory entries read");

	println!("\nthe document reads it makes:");
	for doc in ["/SCOPE.md", "/BRIEF.md", "/CLAUDE.md"] {
		let _ = timed(&format!("read_file(\"{doc}\")"), || store.read_file(doc));
	}

	// -- The findings are read *whole* and JSON-parsed by the handler.
	let findings = store.read_dir("/findings").unwrap_or_default();
	println!("\nfindings ({} files, read in full and parsed):", findings.len());
	let start = Instant::now();
	let mut bytes_read = 0u64;
	for e in &findings {
		let path = format!("/findings/{}", e.name);
		if let Ok(bytes) = store.read_file(&path) {
			bytes_read += bytes.len() as u64;
		}
	}
	println!(
		"  {:<40} {:>9} µs  ({} KiB)",
		"read every finding",
		start.elapsed().as_micros(),
		bytes_read / 1024
	);

	// -- And the single biggest remaining thing that is not in the volume at
	//    all: the handler builds "stats" by reading the console's whole bus log
	//    off the host filesystem and JSON-parsing every HTTP line in it.
	let bus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("../../../bugs/campaigns/events.jsonl");
	if bus.is_file() {
		let bytes = std::fs::metadata(&bus)?.len();
		let start = Instant::now();
		let text = std::fs::read_to_string(&bus)?;
		let read_us = start.elapsed().as_micros();
		let start = Instant::now();
		let mut hits = 0usize;
		for line in text.lines() {
			if !line.contains("\"http_request\"") && !line.contains("\"http_get\"") {
				continue;
			}
			hits += 1;
		}
		println!(
			"\nconsole bus log ({} KiB, {} lines):\n  {:<40} {:>9} µs\n  {:<40} {:>9} µs  ({hits} HTTP lines)",
			bytes / 1024,
			text.lines().count(),
			"read_to_string",
			read_us,
			"substring scan of every line",
			start.elapsed().as_micros(),
		);
	}
	Ok(())
}
