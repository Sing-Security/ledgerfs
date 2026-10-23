//! A capture loop where the application chooses how true the data it reads
//! back is: staged (memory, this session's own writes) or committed (the last
//! superblock, BLAKE3-verified, what a crash would leave).
//!
//! ```text
//! cargo run --example staging_capture
//! ```
//!
//! This is the shape a recorder that cannot afford a commit per batch wants.
//! Records are staged — no allocator, no device block, no barrier — and read
//! back at memory speed; an explicit seal turns everything staged into **one**
//! ordinary commit, after which the data has exactly the guarantee every write
//! in this store has. Nothing about the format changes: a sealed session is
//! indistinguishable from the equivalent sequence of commits, except that it
//! is one commit.
//!
//! The demo prints both tiers mid-session, seals, and then reopens the volume
//! from its bytes to show that the sealed data is what a plain mount sees.

use ledgerfs_host::{Error, MemBlockDevice, Result, Store};

/// 8 MiB — the reserved regions plus enough object space to keep the demo out
/// of the out-of-space path (which `Store::staged_bytes` and
/// `Error::StagingFull` exist for, and which this demo does not exercise).
const BLOCKS: u64 = 2048;

/// The staged byte budget. A real capture loop sets this to what its RAM
/// allows and seals when the seal's own latency stops being free; `0` would
/// mean unbounded.
const STAGING_BUDGET: u64 = 4 * 1024 * 1024;

fn main() {
	if let Err(e) = run() {
		eprintln!("staging_capture: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	let device = MemBlockDevice::new(BLOCKS);
	let mirror = device.clone();
	let mut store = Store::format_app(Box::new(device), "capture")?;

	// -- 1. A committed baseline: the batch that landed before the session.
	// `read_file_committed` will keep naming *this* content until the seal.
	store.mkdir("/capture", 0o755)?;
	store.write_new_file("/capture/log.jsonl", b"{\"batch\":0}\n", 0o644)?;
	let seq_before = store.commit_seq();
	println!("committed baseline at commit_seq {seq_before}");

	// -- 2. Open a session. Nothing below this line reaches the device until
	// the seal; `staged_bytes` is RAM, not volume.
	store.begin_staging(STAGING_BUDGET)?;
	let mut staged_records = 0usize;
	for batch in 1..=5 {
		let record = format!("{{\"batch\":{batch}}}\n");
		// Appends continue each other inside the session: the offset is the
		// *staged* size, so read-your-writes holds record by record.
		store.staged_append("/capture/log.jsonl", record.as_bytes(), 0o644)?;
		staged_records += 1;
	}
	// A new file too — a session is not a single-file concept.
	store.staged_write_new_file("/capture/summary.json", b"{\"sealed\":false}\n", 0o644)?;
	println!(
		"staged {staged_records} appends + 1 new file: {} bytes in RAM, commit_seq still {seq_before}",
		store.staged_bytes().unwrap_or(0)
	);

	// -- 3. The two tiers, side by side, before the seal. The store's own
	// `read_file` is the staged tier: this handle reads its own writes.
	let staged = store.read_file("/capture/log.jsonl")?;
	let committed = store.read_file_committed("/capture/log.jsonl")?;
	println!(
		"staged tier:    {} bytes, {:?}",
		staged.len(),
		String::from_utf8_lossy(&staged)
	);
	println!(
		"committed tier: {} bytes, {:?}",
		committed.len(),
		String::from_utf8_lossy(&committed)
	);
	println!(
		"staged-only file is visible to the staged tier and absent from the committed one: {}",
		match store.read_file_committed("/capture/summary.json") {
			Err(Error::NoSuchFile { .. }) => "yes",
			_ => "no",
		}
	);

	if staged.len() <= committed.len() {
		return Err(mismatch(
			"/capture/log.jsonl",
			"the staged tier must carry the records the committed one does not",
		));
	}

	// -- 4. The seal is the ultimate point of truth: one commit, and the
	// staged root becomes the volume's root.
	let result = store.seal_staged()?;
	println!(
		"sealed: commit_seq {seq_before} -> {}, {} objects, {} bytes written",
		store.commit_seq(),
		result.objects_written,
		result.bytes_written
	);
	// A sealed session is over; the store is an ordinary store again.
	if store.commit_seq() != seq_before + 1 {
		return Err(mismatch(
			"/",
			"a seal is exactly one commit, whatever the session staged",
		));
	}
	if store.staging_is_open() {
		return Err(mismatch("/", "a successful seal closes the session"));
	}

	// -- 5. Now every read is the committed tier and there is nothing left to
	// disagree with. The seal was one transaction, so a crash during it leaves
	// either the old root or the new one — never a half-applied batch.
	let sealed = store.read_file("/capture/log.jsonl")?;
	if sealed != store.read_file_committed("/capture/log.jsonl")? {
		return Err(mismatch("/capture/log.jsonl", "after a seal the two tiers must agree"));
	}
	println!("after the seal the tiers agree: {} bytes", sealed.len());

	// -- 6. The proof that this is durability, not caching: drop the store and
	// reopen the volume from its bytes, the way the next start would. No
	// recovery path runs — a plain mount is the whole story.
	let bytes = mirror.bytes_snapshot();
	drop(store);
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;

	let restored = reopened.read_file("/capture/log.jsonl")?;
	let summary = reopened.read_file("/capture/summary.json")?;
	println!(
		"reopened at commit_seq {}: {:?}",
		reopened.commit_seq(),
		String::from_utf8_lossy(&restored)
	);

	let expected: Vec<u8> = std::iter::once("{\"batch\":0}\n".to_string())
		.chain((1..=5).map(|b| format!("{{\"batch\":{b}}}\n")))
		.collect::<String>()
		.into_bytes();
	if restored != expected {
		return Err(mismatch(
			"/capture/log.jsonl",
			"a plain mount must recover exactly what the seal committed",
		));
	}
	if summary != b"{\"sealed\":false}\n" {
		return Err(mismatch(
			"/capture/summary.json",
			"the file created inside the session must survive the seal",
		));
	}
	if reopened.app()?.as_deref() != Some("capture") {
		return Err(mismatch("/.ledgerfs/APP", "the store's identity must survive"));
	}

	println!("\nstaged writes, one seal, and a plain mount that agrees with both tiers");
	Ok(())
}

/// The demo's one error shape: the volume answered something the walkthrough
/// says it cannot.
fn mismatch(path: &str, detail: &str) -> Error {
	Error::ContentMismatch {
		path: path.to_string(),
		local: "the staging walkthrough's expectation".to_string(),
		detail: detail.to_string(),
	}
}
