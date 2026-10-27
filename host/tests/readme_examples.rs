//! The code in `README.md` and in this crate's `README.md`, compiled and run.
//!
//! The repository README is the first thing a reader sees and the one thing no
//! doc test covers: a doctest lives in the crate root, and these live in
//! Markdown. A README example that stops compiling is a broken front door, so
//! both are pinned here, assertions and all: `rustdoc` checks the crate docs,
//! this file checks the two landing pages.

use ledgerfs_host::{MemBlockDevice, Result, Store};

#[test]
fn root_readme_example() -> Result<()> {
	let device = MemBlockDevice::new(2048);
	let mirror = device.clone();

	let mut store = Store::format_app(Box::new(device), "example")?;

	store.write_new_file("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;

	store.begin_staging(4 * 1024 * 1024)?;
	store.staged_append("/log.jsonl", b"{\"turn\":2}\n", 0o644)?;
	assert_eq!(store.read_file_committed("/log.jsonl")?, b"{\"turn\":1}\n");
	assert_eq!(store.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	let _seal = store.seal_staged()?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	Ok(())
}

#[test]
fn host_readme_example() -> Result<()> {
	let device = MemBlockDevice::new(2048);
	let mirror = device.clone();
	let mut store = Store::format_app(Box::new(device), "example")?;
	store.write_new_file("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;

	store.begin_staging(1024 * 1024)?;
	store.staged_append("/log.jsonl", b"{\"turn\":2}\n", 0o644)?;
	assert_eq!(store.read_file_committed("/log.jsonl")?, b"{\"turn\":1}\n");
	assert_eq!(store.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	let _seal = store.seal_staged()?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	Ok(())
}
