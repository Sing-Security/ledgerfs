//! At-rest encryption, end to end: format, write, close, reopen with a
//! credential, read back byte for byte.
//!
//! These tests exist because two things are only true if they are observed
//! from outside the code that implements them:
//!
//! - the volume's object region on the device really is ciphertext, not the
//!   file contents with a flag set; and
//! - a reopened volume really is the same volume, through the same verbs,
//!   with the same bytes, and refuses an open that has no credential rather
//!   than decoding noise and calling it damage.

mod common;

use common::{BLOCKS, Result, Rng, mem};
use ledgerfs_host::{
	BlockDevice, EncryptedDevice, Error, FileBlockDevice, HostFs, MemBlockDevice, SlotKind, Store, UnlockCredential,
};

/// A payload that compression cannot reduce, so "the marker is absent from the
/// device" is a statement about encryption and not about zstd.
fn incompressible(len: usize, seed: u64) -> Vec<u8> {
	let mut rng = Rng::new(seed);
	(0..len).map(|_| rng.next_u64() as u8).collect()
}

/// Read a stored byte range straight off the device, bypassing the volume.
fn raw_block(device: &MemBlockDevice, lba: u64) -> Vec<u8> {
	let mut buf = vec![0u8; 4096];
	device.read_block(lba, &mut buf).expect("raw device read");
	buf
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
	haystack.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn test_encrypted_volume_round_trips_and_leaves_only_ciphertext() -> Result<()> {
	const PASSPHRASE: &[u8] = b"correct horse battery staple";

	// -- Setup & Fixtures
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format_encrypted(Box::new(device), BLOCKS, UnlockCredential::Passphrase(PASSPHRASE))?;

	// -- Check: the volume says what it is, and its first slot is a passphrase.
	assert!(store.is_encrypted());
	assert_eq!(store.keyslot_kinds(), [SlotKind::Passphrase, SlotKind::Empty]);
	assert!(
		store.is_writable(),
		"a freshly formatted encrypted volume must be writable"
	);

	// -- Exec: something larger than one object block, so the read path has to
	// assemble a record that spans blocks.
	let payload = incompressible(9 * 1024 + 37, 0x5EED);
	store.mkdir("/vault", 0o700)?;
	store.write_new_file("/vault/secret.bin", &payload, 0o600)?;
	assert_eq!(
		store.read_file("/vault/secret.bin")?,
		payload,
		"read-your-writes inside the session"
	);

	// -- Check: on the raw medium, the payload's own bytes do not appear. The
	// needle is a run from the middle of the payload, which compression cannot
	// have removed and encryption must have.
	let needle = &payload[1024..1152];
	assert!(
		!contains(&watch.bytes_snapshot(), needle),
		"plaintext payload bytes reached the device"
	);

	// The public superblock is still legible, which is how a mount can learn the
	// volume is encrypted before it has a key.
	let superblock = raw_block(&watch, 0);
	assert_ne!(superblock[..8], [0u8; 8], "the superblock must be plaintext");

	// -- Exec: keep the bytes, drop the store, and try every way back in.
	let bytes = watch.bytes_snapshot();
	drop(store);

	// -- Check: an open without a credential refuses, and says why.
	let err = match Store::open(Box::new(MemBlockDevice::from_bytes(bytes.clone())?)) {
		Ok(_) => panic!("an encrypted volume must not open without a credential"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::EncryptedVolume { .. }), "got {err:?}");
	assert!(err.to_string().contains("encrypted"), "{err}");

	// -- Check: a bare read-only mount refuses for the same reason, so a tool
	// that has no credential path cannot read noise as data.
	let err = match HostFs::mount_device(Box::new(MemBlockDevice::from_bytes(bytes.clone())?)) {
		Ok(_) => panic!("a bare mount must not decode an encrypted volume"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::EncryptedVolume { .. }), "got {err:?}");

	// -- Check: the wrong passphrase is a credential failure, not damage.
	let err = match Store::open_encrypted(
		Box::new(MemBlockDevice::from_bytes(bytes.clone())?),
		UnlockCredential::Passphrase(b"not the passphrase"),
	) {
		Ok(_) => panic!("a wrong passphrase must not open the volume"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");

	// -- Exec & Check: the right credential reopens the volume, which reads
	// back byte for byte and stays writable.
	let mut reopened = Store::open_encrypted(
		Box::new(MemBlockDevice::from_bytes(bytes)?),
		UnlockCredential::Passphrase(PASSPHRASE),
	)?;
	assert!(reopened.is_encrypted());
	assert!(
		reopened.is_writable(),
		"an encrypted volume must reopen writable: {:?}",
		reopened.write_refusal()
	);
	assert_eq!(reopened.read_file("/vault/secret.bin")?, payload);
	assert_eq!(reopened.read_dir("/vault")?.len(), 1);

	reopened.append("/vault/secret.bin", b"more", 0o600)?;
	assert_eq!(reopened.read_file("/vault/secret.bin")?.len(), payload.len() + 4);

	Ok(())
}

#[test]
fn test_keyslot_lifecycle_add_revoke_and_change_passphrase() -> Result<()> {
	// -- Setup & Fixtures: two credentials on one master key.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format_encrypted(
		Box::new(device),
		BLOCKS,
		UnlockCredential::Passphrase(b"first passphrase"),
	)?;
	store.write_new_file("/f", b"sealed", 0o644)?;

	// -- Exec: add a second passphrase slot.
	let slot = store.add_slot(UnlockCredential::Passphrase(b"second passphrase"))?;
	assert_eq!(slot, 1);
	assert_eq!(store.keyslot_kinds(), [SlotKind::Passphrase, SlotKind::Passphrase]);

	// -- Check: both open the volume, independently.
	let bytes = watch.bytes_snapshot();
	assert_eq!(
		open_with(&bytes, UnlockCredential::Passphrase(b"first passphrase"))?.read_file("/f")?,
		b"sealed"
	);
	assert_eq!(
		open_with(&bytes, UnlockCredential::Passphrase(b"second passphrase"))?.read_file("/f")?,
		b"sealed"
	);

	// -- Check: no third slot exists, and the refusal does not disturb either.
	let err = match store.add_slot(UnlockCredential::Passphrase(b"third")) {
		Ok(_) => panic!("a third credential has nowhere to go"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::NoFreeKeySlot { .. }), "got {err:?}");

	// -- Exec: revoke the second credential.
	store.revoke_slot(1)?;
	assert_eq!(store.keyslot_kinds(), [SlotKind::Passphrase, SlotKind::Empty]);

	// -- Check: it no longer opens the volume, and the first one still does.
	let bytes = watch.bytes_snapshot();
	let err = match open_with(&bytes, UnlockCredential::Passphrase(b"second passphrase")) {
		Ok(_) => panic!("a revoked credential must not open the volume"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");

	// The handle opened from a *copy* of the bytes writes into its own buffer,
	// so it keeps its own watch. Snapshotting the original one after this would
	// assert against a volume that never saw the change.
	let (mut store, watch) = open_watched(&bytes, UnlockCredential::Passphrase(b"first passphrase"))?;

	// -- Check: the last credential cannot be revoked.
	let err = match store.revoke_slot(0) {
		Ok(_) => panic!("revoking the only credential must be refused"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::LastKeySlot { index: 0 }), "got {err:?}");
	let err = match store.revoke_slot(1) {
		Ok(_) => panic!("revoking an empty slot must be refused"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::KeySlotEmpty { index: 1 }), "got {err:?}");

	// -- Exec: change the passphrase. A wrong old one changes nothing.
	let err = match store.change_passphrase(b"not the old one", b"third") {
		Ok(_) => panic!("a wrong old passphrase must be refused"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");

	store.change_passphrase(b"first passphrase", b"third passphrase")?;

	// -- Check: only the new one opens the volume now.
	let bytes = watch.bytes_snapshot();
	let err = match open_with(&bytes, UnlockCredential::Passphrase(b"first passphrase")) {
		Ok(_) => panic!("the replaced passphrase must not open the volume"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");
	assert_eq!(
		open_with(&bytes, UnlockCredential::Passphrase(b"third passphrase"))?.read_file("/f")?,
		b"sealed"
	);

	Ok(())
}

#[test]
fn test_hsm_key_slot_and_passphrase_slot_coexist() -> Result<()> {
	// -- Setup & Fixtures: a machine-held key, which is the unattended-boot
	// shape, and the key material no disk image can reproduce.
	let hsm_key = [0xC3u8; 32];
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format_encrypted(Box::new(device), BLOCKS, UnlockCredential::KeyEncryptionKey(&hsm_key))?;
	assert_eq!(store.keyslot_kinds(), [SlotKind::Hsm, SlotKind::Empty]);
	store.write_new_file("/f", b"machine secret", 0o600)?;

	// -- Exec: add a human passphrase alongside it.
	store.add_slot(UnlockCredential::Passphrase(b"human passphrase"))?;
	assert_eq!(store.keyslot_kinds(), [SlotKind::Hsm, SlotKind::Passphrase]);

	// -- Check: either opens the volume, and a wrong key does not.
	let bytes = watch.bytes_snapshot();
	assert_eq!(
		open_with(&bytes, UnlockCredential::KeyEncryptionKey(&hsm_key))?.read_file("/f")?,
		b"machine secret"
	);
	assert_eq!(
		open_with(&bytes, UnlockCredential::Passphrase(b"human passphrase"))?.read_file("/f")?,
		b"machine secret"
	);

	let wrong_key = [0xC4u8; 32];
	let err = match open_with(&bytes, UnlockCredential::KeyEncryptionKey(&wrong_key)) {
		Ok(_) => panic!("a wrong key-encryption key must not open the volume"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");

	// A passphrase is not a key-encryption key: slot kinds are separate, so a
	// credential of the wrong kind finds no slot to try rather than something
	// that happens to hash to the right bytes.
	let err = match open_with(&bytes, UnlockCredential::Passphrase(&hsm_key)) {
		Ok(_) => panic!("a passphrase must not open an HSM slot"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");

	Ok(())
}

#[test]
fn test_file_backed_encrypted_volume_reopens_and_keeps_no_plaintext_sidecar() -> Result<()> {
	// -- Setup & Fixtures: a real file, so the device's hint-cache answer is the
	// one a FileBlockDevice gives, and a stale sidecar from a previous plaintext
	// volume at the same path.
	let image = common::TempImage::new("encrypted");
	let hints = std::path::PathBuf::from(format!("{}.hints", image.path.display()));
	std::fs::write(&hints, b"stale map of blocks that are about to be ciphertext")?;

	let payload = incompressible(64 * 1024, 0xA11CE);

	// -- Exec
	{
		let device = FileBlockDevice::create(&image.path, BLOCKS * 4096)?;
		let mut store = Store::format_encrypted(
			Box::new(device),
			BLOCKS,
			UnlockCredential::Passphrase(b"file passphrase"),
		)?;
		store.write_new_file("/secret.bin", &payload, 0o600)?;
	}

	// -- Check: the sidecar is gone. An encrypted volume reports no hint cache,
	// because the sidecar would map content hashes of plaintext to blocks.
	assert!(
		!hints.exists(),
		"a stale plaintext hint sidecar must be removed by the format"
	);

	// -- Exec & Check: reopen read-only, through a real file, and read every
	// byte back.
	{
		let device = FileBlockDevice::open(&image.path, 0, BLOCKS * 4096)?;
		let store = Store::open_encrypted(Box::new(device), UnlockCredential::Passphrase(b"file passphrase"))?;
		assert!(store.is_encrypted());
		assert_eq!(store.read_file("/secret.bin")?, payload);
	}

	// Nothing wrote a new sidecar during or after the encrypted session either.
	assert!(!hints.exists(), "an encrypted volume must not write a hint sidecar");

	Ok(())
}

#[test]
fn test_encrypted_device_refuses_partial_writes_but_reads_any_range() -> Result<()> {
	// -- Setup & Fixtures: a raw wrapper over a buffer, no store involved.
	let inner = mem(16);
	let watch = inner.clone();
	let key = ledgerfs_host::VolumeKey::from_bytes([0x7Bu8; 64]);
	let device = EncryptedDevice::new(Box::new(inner), 16, &key);

	let plaintext = incompressible(4 * 4096, 0xD00D);

	// -- Exec & Check: a whole number of blocks writes, encrypted.
	device.write_blocks(6, &plaintext)?;

	// -- Check: the medium does not hold the plaintext...
	let stored = raw_block(&watch, 6);
	assert!(
		!contains(&stored, &plaintext[..64]),
		"block 6 reached the device in the clear"
	);
	// ...and the wrapper reads it back.
	let mut back = vec![0u8; plaintext.len()];
	device.read_bytes_at(6 * 4096, &mut back)?;
	assert_eq!(back, plaintext);

	// -- Check: a byte range inside one block is served from a decrypted copy.
	let mut middle = vec![0u8; 100];
	device.read_bytes_at(6 * 4096 + 17, &mut middle)?;
	assert_eq!(middle, &plaintext[17..117]);

	// -- Check: a range that straddles a block boundary is assembled correctly.
	let mut straddle = vec![0u8; 4096];
	device.read_bytes_at(7 * 4096 - 2048, &mut straddle)?;
	assert_eq!(straddle, &plaintext[2048..4096 + 2048]);

	// -- Exec & Check: a partial write is refused rather than turned into a
	// read-modify-write the caller did not ask for.
	let err = device.write_bytes_at(6 * 4096, &[0u8; 100]).unwrap_err();
	assert!(
		matches!(err, Error::EncryptedWriteMisaligned { len: 100, .. }),
		"got {err:?}"
	);

	// -- Check: the header and trailer blocks stay plaintext, so the superblock
	// remains readable before a key exists.
	device.write_block(0, b"header")?;
	assert_eq!(&raw_block(&watch, 0)[..6], b"header");
	device.write_block(15, b"trailer")?;
	assert_eq!(&raw_block(&watch, 15)[..7], b"trailer");

	Ok(())
}

#[test]
fn test_operations_that_need_a_key_are_refused_on_a_plaintext_volume() -> Result<()> {
	// -- Setup & Fixtures: an ordinary volume, opened the ordinary way.
	let mut store = common::store(BLOCKS)?;
	assert!(!store.is_encrypted());
	assert_eq!(store.keyslot_kinds(), [SlotKind::Empty, SlotKind::Empty]);

	// -- Exec & Check: every lifecycle call names the missing key rather than
	// silently succeeding against empty slots.
	for (what, err) in [
		("add", store.add_slot(UnlockCredential::Passphrase(b"x")).unwrap_err()),
		("revoke", store.revoke_slot(0).unwrap_err()),
		("change", store.change_passphrase(b"a", b"b").unwrap_err()),
	] {
		assert!(matches!(err, Error::VolumeNotEncrypted { .. }), "{what}: got {err:?}");
	}

	// -- Check: and the plaintext volume has not been altered by the refusals.
	assert_eq!(store.read_dir("/")?.len(), 0);

	Ok(())
}

#[test]
fn test_a_failed_keyslot_write_leaves_no_credential_behind() -> Result<()> {
	// -- Setup & Fixtures: a volume on a device that can be told to stop
	// accepting writes, which is the only way to make the superblock write fail
	// while the store is healthy.
	let device = FailAfter::new(BLOCKS);
	let watch = device.watch();
	let mut store = Store::format_encrypted(
		Box::new(device.clone()),
		BLOCKS,
		UnlockCredential::Passphrase(b"real passphrase"),
	)?;
	store.write_new_file("/f", b"kept", 0o644)?;

	// -- Exec: the third superblock copy fails. The first two already carry the
	// new slot, so this is exactly the torn keyslot change a crash can leave.
	device.fail_after(2);
	let err = store.add_slot(UnlockCredential::Passphrase(b"phantom passphrase")).unwrap_err();
	assert!(matches!(err, Error::Usage(_)), "got {err:?}");

	// -- Check: the handle's own view was rolled back rather than left ahead of
	// the write that failed.
	assert_eq!(
		store.keyslot_kinds(),
		[SlotKind::Passphrase, SlotKind::Empty],
		"a failed add_slot must not leave the slot occupied in memory"
	);

	// -- Exec: let the device work again, then run an ordinary commit. Without
	// the rollback this is what would make the failed credential durable: every
	// commit rewrites the whole superblock from the in-memory copy.
	device.fail_after(usize::MAX);
	store.mkdir("/after", 0o755)?;

	// -- Check: the credential from the failed call opens nothing, and the real
	// one still opens the volume and its data.
	let bytes = watch.bytes_snapshot();
	let err = match open_with(&bytes, UnlockCredential::Passphrase(b"phantom passphrase")) {
		Ok(_) => panic!("a credential whose superblock write failed must not open the volume"),
		Err(e) => e,
	};
	assert!(matches!(err, Error::WrongCredential { .. }), "got {err:?}");
	let mut reopened = open_with(&bytes, UnlockCredential::Passphrase(b"real passphrase"))?;
	assert_eq!(reopened.read_file("/f")?, b"kept");
	assert_eq!(reopened.read_dir("/")?.len(), 2);
	reopened.append("/f", b"more", 0o644)?;

	Ok(())
}

#[test]
fn test_a_keyslot_change_publishes_a_deferred_seal_rather_than_losing_it() -> Result<()> {
	// -- Setup & Fixtures: a sealed but deliberately un-synced session.
	let device = mem(BLOCKS);
	let watch = device.clone();
	let mut store = Store::format_encrypted(Box::new(device), BLOCKS, UnlockCredential::Passphrase(b"passphrase"))?;
	store.begin_staging(1024 * 1024)?;
	store.staged_append("/capture.jsonl", b"row\n", 0o644)?;
	store.seal_staged_deferred()?;
	assert!(
		store.has_pending_sync(),
		"a deferred seal must leave the superblock write outstanding"
	);

	// -- Exec: a keyslot change rewrites the superblock, and a superblock write
	// is the only thing in this format that publishes a root.
	store.add_slot(UnlockCredential::Passphrase(b"second"))?;

	// -- Check: the pending seal is now published, and the handle says so rather
	// than leaving `sync` reporting work that has already been done.
	assert!(
		!store.has_pending_sync(),
		"the keyslot write published the root, so nothing is pending"
	);

	// -- Check: the staged row really is durable, with no `sync` call.
	let bytes = watch.bytes_snapshot();
	assert_eq!(
		open_with(&bytes, UnlockCredential::Passphrase(b"second"))?.read_file("/capture.jsonl")?,
		b"row\n"
	);

	Ok(())
}

/// A [`MemBlockDevice`] that stops accepting writes after a chosen count.
///
/// Every block helper funnels into `write_bytes_at`, so one counter is enough to
/// fail the *n*th block written, which is how the tests reach a superblock write
/// that fails after some copies have landed.
#[derive(Clone)]
struct FailAfter {
	inner: MemBlockDevice,
	remaining: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FailAfter {
	fn new(blocks: u64) -> Self {
		Self {
			inner: mem(blocks),
			remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
		}
	}

	/// A live handle on the same buffer, as [`MemBlockDevice::clone`] gives.
	fn watch(&self) -> MemBlockDevice {
		self.inner.clone()
	}

	/// Accept `writes` more writes, then fail every one after them.
	fn fail_after(&self, writes: usize) {
		self.remaining.store(writes, std::sync::atomic::Ordering::SeqCst);
	}
}

impl BlockDevice for FailAfter {
	fn size_bytes(&self) -> u64 {
		self.inner.size_bytes()
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> ledgerfs_host::Result<()> {
		self.inner.read_bytes_at(offset, buf)
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> ledgerfs_host::Result<()> {
		if self.remaining.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 0 {
			return Err(Error::Usage("injected device failure".to_string()));
		}
		self.inner.write_bytes_at(offset, data)
	}

	fn barrier(&self) -> ledgerfs_host::Result<()> {
		self.inner.barrier()
	}

	fn hint_cache(&self) -> Option<ledgerfs_host::HintCache> {
		None
	}
}

/// Reopen `bytes` with `credential`, the way a restart does.
///
/// Returns the crate's own error type rather than the boxed test one, so a
/// caller can assert on the variant instead of a message.
fn open_with(bytes: &[u8], credential: UnlockCredential<'_>) -> ledgerfs_host::Result<Store> {
	Ok(open_watched(bytes, credential)?.0)
}

/// [`open_with`], plus a live handle on the reopened volume's buffer.
fn open_watched(bytes: &[u8], credential: UnlockCredential<'_>) -> ledgerfs_host::Result<(Store, MemBlockDevice)> {
	let device = MemBlockDevice::from_bytes(bytes.to_vec())?;
	let watch = device.clone();
	Ok((Store::open_encrypted(Box::new(device), credential)?, watch))
}
