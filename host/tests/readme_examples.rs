//! The code in `README.md` and in this crate's `README.md`, compiled and run.
//!
//! The repository README is the first thing a reader sees and the one thing no
//! doc test covers: a doctest lives in the crate root, and these live in
//! Markdown. A README example that stops compiling is a broken front door, so
//! every complete program in the two landing pages is pinned here, assertions
//! and all: `rustdoc` checks the crate docs, this file checks the READMEs.

use ledgerfs_host::{MemBlockDevice, Result, Store, UnlockCredential};

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

#[test]
fn root_readme_encryption_example() -> Result<()> {
	let passphrase = b"a passphrase for the demo";
	let device = MemBlockDevice::new(4096);
	let mirror = device.clone();

	let mut store = Store::format_encrypted(Box::new(device), 4096, UnlockCredential::Passphrase(passphrase))?;
	store.write_new_file("/secret.txt", b"classified\n", 0o600)?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open_encrypted(
		Box::new(MemBlockDevice::from_bytes(bytes)?),
		UnlockCredential::Passphrase(passphrase),
	)?;
	assert_eq!(reopened.read_file("/secret.txt")?, b"classified\n");
	Ok(())
}
