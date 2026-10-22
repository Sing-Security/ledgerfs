//! The portability contract, as code: [`BlockDevice`] is the whole seam.
//!
//! ```text
//! cargo run --example custom_device
//! ```
//!
//! An embedder with storage of its own — flash pages, a network volume, an
//! NVMe queue, battery-backed RAM — does not wait for this crate to support
//! it. Four methods implemented, the object handed to [`Store`], and
//! everything above the seam (the commit protocol, CoW, content addressing,
//! the on-disk format) works unchanged, because nothing above the seam ever
//! learns what the medium is.
//!
//! The device here is a fixed byte buffer with a barrier counter — the
//! smallest honest stand-in for a medium whose durability point is explicit.
//! `MemBlockDevice` is the same shape without the counter; the counter is
//! what makes this example say something.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ledgerfs_host::{BLOCK_SIZE, BlockDevice, Error, Result, Store};

/// A volume over storage the embedder owns. The real thing would be a flash
/// driver, an RPC client to a remote volume, a mapped partition — anything
/// that can move blocks and name a durability point.
#[derive(Clone)]
pub struct OwnStorage {
	/// The medium. `Arc<Mutex<_>>`, not `RefCell`: `BlockDevice: Send + Sync`
	/// (a `File` earns the same `&self` interface from positioned I/O), and
	/// the `Arc` lets the embedder keep its own handle on storage the store
	/// now owns.
	medium: Arc<Mutex<Vec<u8>>>,
	/// Every `barrier` call is a durability point. Shared for the same
	/// reason: the embedder can watch its medium while the store holds it.
	barriers: Arc<AtomicU64>,
}

impl OwnStorage {
	/// A zeroed medium of `block_count` blocks.
	pub fn new(block_count: u64) -> Self {
		Self {
			medium: Arc::new(Mutex::new(vec![0u8; block_count as usize * BLOCK_SIZE as usize])),
			barriers: Arc::new(AtomicU64::new(0)),
		}
	}

	/// Adopt an existing buffer as the medium. The buffer's length defines
	/// the volume size, so a truncated one is refused here rather than
	/// surfacing as a short read much later.
	pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
		if !bytes.len().is_multiple_of(BLOCK_SIZE as usize) {
			return Err(Error::Misaligned {
				context: "OwnStorage::from_bytes",
				len: bytes.len(),
				block_size: BLOCK_SIZE,
			});
		}
		Ok(Self {
			medium: Arc::new(Mutex::new(bytes)),
			barriers: Arc::new(AtomicU64::new(0)),
		})
	}

	/// How many durability points the medium has crossed. Reads the live
	/// counter, which keeps ticking after the device is handed to a `Store`.
	pub fn barriers(&self) -> u64 {
		self.barriers.load(Ordering::Relaxed)
	}

	/// A copy of the medium's bytes — a valid volume file, reopenable with
	/// [`from_bytes`](Self::from_bytes). A real medium would not need this
	/// (its bytes are already somewhere durable); a buffer does, so the demo
	/// can prove the volume survives the handle.
	pub fn snapshot(&self) -> Vec<u8> {
		self.medium.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
	}
}

impl BlockDevice for OwnStorage {
	fn size_bytes(&self) -> u64 {
		self.medium.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len() as u64
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
		self.check_bounds(offset, buf.len())?;
		let medium = self.medium.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		buf.copy_from_slice(&medium[offset as usize..][..buf.len()]);
		Ok(())
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> Result<()> {
		self.check_bounds(offset, data.len())?;
		let mut medium = self.medium.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
		medium[offset as usize..][..data.len()].copy_from_slice(data);
		Ok(())
	}

	fn barrier(&self) -> Result<()> {
		// The one method that is not a byte move: "everything written before
		// this point survives the power going away." A commit calls this
		// between its writes and its success; a flash driver flushes here, a
		// network volume waits for the flush RPC's reply. A medium that lies
		// in this method is the one way to turn the commit protocol into
		// theatre — see EMBEDDING.md §2.
		self.barriers.fetch_add(1, Ordering::Relaxed);
		Ok(())
	}
}

fn main() {
	if let Err(e) = run() {
		eprintln!("custom_device: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	// -- 1. The medium, the seam, the store. Two lines do the embedding —
	// the same two lines `MemBlockDevice` would cost, which is the point.
	// The mirror keeps the embedder's own handle on its storage.
	let medium = OwnStorage::new(1024);
	let mirror = medium.clone();
	let mut store = Store::format_device(Box::new(medium))?;
	println!(
		"formatted over own storage: {} blocks, commit seq {}",
		store.fs().volume.total_blocks(),
		store.commit_seq()
	);

	// -- 2. Ordinary work. Nothing here can tell this device from a disk,
	// because it never asks.
	store.mkdir("/records", 0o755)?;
	store.write_new_file("/records/first.jsonl", b"{\"event\":\"boot\"}\n", 0o644)?;
	store.append("/records/first.jsonl", b"{\"event\":\"ready\"}\n", 0o644)?;
	let log = store.read_file("/records/first.jsonl")?;
	println!("read back {} bytes from /records/first.jsonl", log.len());

	// -- 3. The numbers an embedder plans around: how much medium a workload
	// costs (commits are not free — see KNOWN_ISSUES.md entry 3), and how
	// many durability points it crossed.
	if let Some(free) = store.free_bytes() {
		println!("free space: {} bytes", free);
	}
	println!(
		"commit seq {} after three writes; the medium crossed {} barriers",
		store.commit_seq(),
		mirror.barriers()
	);

	// -- 4. Drop the store and reopen over the same storage, as an embedder
	// does on its next start: no special reopen path, because the format
	// never knew the medium was ours. For a buffer that means handing the
	// bytes back; for flash or a remote volume it means opening it again.
	let bytes = mirror.snapshot();
	drop(store);
	let reopened = Store::open(Box::new(OwnStorage::from_bytes(bytes)?))?;
	let log = reopened.read_file("/records/first.jsonl")?;
	println!(
		"reopened over own storage: {} bytes back, commit seq {}",
		log.len(),
		reopened.commit_seq()
	);

	// An embedder that got different bytes back than it stored is not
	// tolerably broken, so this asserts rather than prints.
	if log != b"{\"event\":\"boot\"}\n{\"event\":\"ready\"}\n" {
		return Err(Error::ContentMismatch {
			path: "/records/first.jsonl".to_string(),
			local: "the application's own copy".to_string(),
			detail: format!("read back {} bytes", log.len()),
		});
	}

	println!("\nthe volume over custom storage round-tripped byte-for-byte");
	Ok(())
}
