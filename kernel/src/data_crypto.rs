//! Transparent XTS-AES-256 encryption for the data volume's raw physical blocks.
//!
//! # Why this lives here, and not in `cow::operations` or the disk driver
//!
//! The threat model this defends is narrow and deliberate: protect the data
//! volume's contents when the disk is at rest and the host isn't running (a
//! stolen or removed drive). It is **not** trying to isolate processes from
//! each other
//! on a live, running system — that's the capability system's job. Given
//! that narrow model, the right place to apply the cipher is *below* the CoW
//! object model, not woven through it: every byte [`crate::commit::
//! AtaBlockDevice`] ever writes to or reads from the data volume
//! passes through exactly two functions, so encrypting there means
//! `cow::operations`, the extent tree, content-addressing, and everything
//! else above it never has to know encryption exists, is never handed
//! ciphertext, and never has its own invariants (`ObjectRecordReader::read`
//! verifying `blake3(stored_bytes) == object_id`, in particular) put in
//! tension with a second, incompatible notion of "the real stored bytes."
//!
//! # Cipher choice
//!
//! [XTS-AES-256](https://en.wikipedia.org/wiki/Disk_encryption_theory#XEX-based_tweaked-codebook_mode_with_ciphertext_stealing_(XTS)),
//! the standard construction for exactly this shape of problem (BitLocker,
//! dm-crypt's default, etc.): a block device where every unit is fixed-size,
//! gets rewritten in place, and has no spare room for a per-write nonce or
//! authentication tag. The tweak is derived deterministically from the LBA
//! (`get_tweak_default`, `xts_mode`'s standard little-endian sector-index
//! tweak) — no nonce bookkeeping, no new persisted metadata, no superblock
//! format change.
//!
//! XTS provides confidentiality only, not per-block authentication (see the
//! `xts-mode` crate's own security notes). That's an accepted, deliberate
//! trade-off here, not an oversight: the data volume's object layer already verifies
//! `blake3(decoded payload) == object_id` on every read
//! (`ObjectRecordReader::read`, `reader::read_object`'s scan-and-verify), so
//! a tampered or corrupted ciphertext block — XTS or not — already fails
//! loudly at that layer instead of being silently trusted. Adding a second,
//! independent authentication mechanism at the block layer (AES-GCM/
//! ChaCha20-Poly1305 per block) would mean persisting a nonce+tag table
//! somewhere (a real superblock/format change, much larger blast radius)
//! for a property this system already has.
//!
//! # Key material never derived from the disk image itself
//!
//! For "protects data when the host isn't around" to mean anything, the key
//! must not be recoverable from the disk alone — it is provisioned from a
//! source outside the disk image, never derived from the volume's own bytes.
//! This module only performs the actual block cipher operation. Where the
//! 64-byte key comes from is entirely the caller's concern — the two unlock
//! paths [`crate::keyslot`] describes are a passphrase stretched into a KEK and
//! an HSM-held KEK, and both end in a master key the caller has already
//! unwrapped. This module derives nothing, stores nothing, and has no opinion
//! about how a key should be protected.

//! # What is and isn't encrypted (the plaintext-header boundary)
//!
//! Only the **object region** — blocks at or after
//! [`crate::RESERVED_HEADER_BLOCKS`] — is encrypted. The reserved header
//! (superblock copies, intent log, genesis audit record) and the two
//! trailing backup superblock copies stay plaintext, for the same reason a
//! LUKS header does: the superblock is where the
//! [`FEATURE_INCOMPAT_DATA_ENCRYPTED`](crate::superblock::FEATURE_INCOMPAT_DATA_ENCRYPTED)
//! flag itself lives, so it has to be readable *before* anything knows
//! whether to decrypt, and before any key has been derived.
//!
//! **What this leaks, stated plainly:** an attacker with the raw disk can
//! read the superblock (volume size, `active_root_id` — a BLAKE3 hash of
//! the root inode, not its contents — and `active_commit_seq`) and the
//! intent log's transaction records (object IDs and the physical LBAs/block
//! counts they were written to). That is real metadata leakage: it reveals
//! roughly how many objects exist, how large they are, and how often the
//! volume is written, though not any object's actual contents. Closing it
//! would mean relocating the encryption flag out of the superblock into
//! some other plaintext anchor and encrypting the header too — a larger
//! format change than this feature warrants, and one that buys little while
//! file *contents* are what actually matter here.

extern crate alloc;

use aes::Aes256;
use aes::cipher::{Array, KeyInit, consts::U32};
use xts_mode::{Xts128, get_tweak_default};

/// Whether a given partition-relative block index falls in the encrypted
/// (object) region rather than the plaintext reserved header/trailer.
///
/// `total_blocks` is the volume's size in LedgerFS blocks, needed to
/// identify the trailing backup superblock copies
/// ([`crate::RESERVED_TRAILER_BLOCKS`]). Pass `None` when the caller
/// genuinely doesn't know the volume size — the trailing copies are then
/// treated as encrypted, which is wrong; every real caller (which has just
/// read the superblock to learn the volume is encrypted at all) knows it.
pub fn block_is_encrypted(block_lba: u64, total_blocks: Option<u64>) -> bool {
	if block_lba < crate::RESERVED_HEADER_BLOCKS {
		return false;
	}
	if let Some(total) = total_blocks
		&& block_lba >= total.saturating_sub(crate::RESERVED_TRAILER_BLOCKS)
	{
		return false;
	}
	true
}

/// Raw key material length: two independent AES-256 keys (XTS's "key1",
/// used for the actual block cipher, and "key2", used to derive the tweak),
/// 32 bytes each — see `xts_mode`'s own AES-256 example.
pub const DATA_VOLUME_KEY_LEN: usize = 64;

/// A ready-to-use XTS-AES-256 cipher for one data volume's blocks.
///
/// Constructed once, at mount time, from the volume's derived key. Never
/// exposes the key material back out — only `encrypt_block`/`decrypt_block`.
pub struct DataVolumeCipher {
	xts: Xts128<Aes256>,
}

impl DataVolumeCipher {
	/// Build a cipher from 64 bytes of raw key material (two concatenated
	/// 32-byte AES-256 keys). The caller's copy of `key` is not zeroized by
	/// this call — callers handling raw key bytes are responsible for their
	/// own zeroization once this returns (see `zeroize`'s `Zeroize` trait);
	/// this function only consumes the bytes needed to expand both AES-256
	/// key schedules.
	pub fn new(key: &[u8; DATA_VOLUME_KEY_LEN]) -> Self {
		let key_1: Array<u8, U32> = Array(key[..32].try_into().expect("32-byte slice"));
		let key_2: Array<u8, U32> = Array(key[32..].try_into().expect("32-byte slice"));
		let cipher_1 = Aes256::new(&key_1);
		let cipher_2 = Aes256::new(&key_2);
		Self {
			xts: Xts128::new(cipher_1, cipher_2),
		}
	}

	/// Encrypt exactly one physical block in place.
	///
	/// `buf.len()` must equal [`crate::BLOCK_SIZE`] (the XTS "sector size"
	/// for this volume — one LedgerFS block, not one 512-byte ATA sector).
	/// `lba` is the same partition-relative, 4 KiB-unit block index used
	/// everywhere else in this crate (`AtaBlockDevice::write_blocks`'s `lba`
	/// parameter, `PlacedObject::physical_lba`, ...) — it becomes the XTS
	/// tweak, so encrypting the same plaintext at two different LBAs always
	/// produces different ciphertext.
	///
	/// # Panics
	///
	/// If `buf.len() != crate::BLOCK_SIZE as usize`.
	pub fn encrypt_block(&self, buf: &mut [u8], lba: u64) {
		assert_eq!(
			buf.len(),
			crate::BLOCK_SIZE as usize,
			"encrypt_block: buffer must be exactly one BLOCK_SIZE block"
		);
		let tweak = get_tweak_default(lba as u128);
		self.xts.encrypt_sector(buf, tweak);
	}

	/// Decrypt exactly one physical block in place. See `encrypt_block` for
	/// the `buf`/`lba` contract — decrypting with the wrong `lba` silently
	/// produces garbage plaintext (XTS has no way to detect this itself;
	/// see this module's doc comment on why that's an accepted trade-off).
	///
	/// # Panics
	///
	/// If `buf.len() != crate::BLOCK_SIZE as usize`.
	pub fn decrypt_block(&self, buf: &mut [u8], lba: u64) {
		assert_eq!(
			buf.len(),
			crate::BLOCK_SIZE as usize,
			"decrypt_block: buffer must be exactly one BLOCK_SIZE block"
		);
		let tweak = get_tweak_default(lba as u128);
		self.xts.decrypt_sector(buf, tweak);
	}
}

// region:    --- Active-volume registry

/// The one encrypted volume's cipher + the partition it belongs to, set
/// once at mount time.
///
/// A registry (rather than a field on `AtaBlockDevice`) because
/// `AtaBlockDevice::new(partition_start_lba)` is constructed fresh at every
/// call site in the kernel bridge — there is no long-lived device object
/// to hang a key off. Keying on `partition_start_lba` is what makes this
/// safe: a block device for any *other* partition (notably a read-only,
/// signature-verified system partition, which must never be encrypted)
/// finds no entry and passes bytes through untouched.
///
/// Deliberately holds at most **one** volume: the data volume is the only
/// writable, non-system mount this system has today. A second encrypted
/// volume would need this to become a small map, but pretending to support
/// that now would be untested speculation.
static ACTIVE: spin::Mutex<Option<ActiveVolume>> = spin::Mutex::new(None);

struct ActiveVolume {
	partition_start_lba: u32,
	total_blocks: u64,
	cipher: DataVolumeCipher,
}

/// Install the volume key for `partition_start_lba`, enabling transparent
/// encryption for that partition's object region from this point on.
///
/// Called once, by whatever unlocks the volume, at the point the master key
/// has been unwrapped — see [`crate::keyslot`]. The key bytes are consumed into
/// two AES-256 key schedules here, and the caller's buffer should be zeroized
/// by the caller afterward: this function copies the key, it does not own it.
///
/// Replacing an already-active volume is allowed (re-mount), and drops the
/// previous cipher.
pub fn activate(partition_start_lba: u32, total_blocks: u64, key: &[u8; DATA_VOLUME_KEY_LEN]) {
	let mut slot = ACTIVE.lock();
	*slot = Some(ActiveVolume {
		partition_start_lba,
		total_blocks,
		cipher: DataVolumeCipher::new(key),
	});
}

/// Whether transparent encryption is currently active for this partition.
pub fn is_active_for(partition_start_lba: u32) -> bool {
	ACTIVE
		.lock()
		.as_ref()
		.is_some_and(|v| v.partition_start_lba == partition_start_lba)
}

/// Encrypt one block in place if `partition_start_lba` has an active
/// volume key *and* `block_lba` falls in the encrypted object region.
/// A no-op otherwise — which is exactly what makes every non-data-volume
/// caller, and every plaintext-header block, pass through untouched.
pub fn maybe_encrypt_block(partition_start_lba: u32, block_lba: u64, buf: &mut [u8]) {
	let slot = ACTIVE.lock();
	let Some(v) = slot.as_ref() else { return };
	if v.partition_start_lba != partition_start_lba || !block_is_encrypted(block_lba, Some(v.total_blocks)) {
		return;
	}
	v.cipher.encrypt_block(buf, block_lba);
}

/// Decrypt one block in place under the same conditions as
/// [`maybe_encrypt_block`] — the exact inverse, so a block that was passed
/// through untouched on write is also passed through untouched on read.
pub fn maybe_decrypt_block(partition_start_lba: u32, block_lba: u64, buf: &mut [u8]) {
	let slot = ACTIVE.lock();
	let Some(v) = slot.as_ref() else { return };
	if v.partition_start_lba != partition_start_lba || !block_is_encrypted(block_lba, Some(v.total_blocks)) {
		return;
	}
	v.cipher.decrypt_block(buf, block_lba);
}

// endregion: --- Active-volume registry

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;
	use alloc::vec;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	fn test_key() -> [u8; DATA_VOLUME_KEY_LEN] {
		let mut key = [0u8; DATA_VOLUME_KEY_LEN];
		for (i, b) in key.iter_mut().enumerate() {
			*b = (i as u8).wrapping_mul(31).wrapping_add(7);
		}
		key
	}

	#[test]
	fn test_encrypt_then_decrypt_round_trips() -> TestResult<()> {
		// -- Setup & Fixtures
		let cipher = DataVolumeCipher::new(&test_key());
		let plaintext = vec![0xABu8; crate::BLOCK_SIZE as usize];
		let mut buf = plaintext.clone();

		// -- Exec
		cipher.encrypt_block(&mut buf, 42);
		assert_ne!(buf, plaintext, "encryption must actually change the bytes");
		cipher.decrypt_block(&mut buf, 42);

		// -- Check
		assert_eq!(buf, plaintext, "decrypt(encrypt(x)) must equal x");

		Ok(())
	}

	#[test]
	fn test_same_plaintext_different_lba_produces_different_ciphertext() -> TestResult<()> {
		// -- Setup & Fixtures
		let cipher = DataVolumeCipher::new(&test_key());
		let plaintext = vec![0x42u8; crate::BLOCK_SIZE as usize];

		// -- Exec
		let mut buf_a = plaintext.clone();
		cipher.encrypt_block(&mut buf_a, 100);
		let mut buf_b = plaintext.clone();
		cipher.encrypt_block(&mut buf_b, 200);

		// -- Check: identical plaintext at two different LBAs must not
		// produce identical ciphertext — this is the whole point of the
		// per-LBA tweak (without it, an observer with raw disk access could
		// trivially spot repeated block content across the volume).
		assert_ne!(
			buf_a, buf_b,
			"ciphertext must differ across LBAs for identical plaintext"
		);

		Ok(())
	}

	#[test]
	fn test_decrypting_at_the_wrong_lba_does_not_recover_the_plaintext() -> TestResult<()> {
		// -- Setup & Fixtures
		let cipher = DataVolumeCipher::new(&test_key());
		let plaintext = vec![0x99u8; crate::BLOCK_SIZE as usize];
		let mut buf = plaintext.clone();

		// -- Exec
		cipher.encrypt_block(&mut buf, 7);
		cipher.decrypt_block(&mut buf, 8); // wrong LBA/tweak

		// -- Check: decrypting with the wrong tweak must not silently
		// recover the original plaintext (would indicate the LBA isn't
		// actually being mixed into the tweak at all).
		assert_ne!(
			buf, plaintext,
			"decrypting at the wrong LBA must not recover the real plaintext"
		);

		Ok(())
	}

	#[test]
	fn test_different_keys_produce_different_ciphertext_for_the_same_block() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut key_b = test_key();
		key_b[0] ^= 0xFF; // one-bit-ish difference in the first key half

		let cipher_a = DataVolumeCipher::new(&test_key());
		let cipher_b = DataVolumeCipher::new(&key_b);
		let plaintext = vec![0x55u8; crate::BLOCK_SIZE as usize];

		// -- Exec
		let mut buf_a = plaintext.clone();
		cipher_a.encrypt_block(&mut buf_a, 0);
		let mut buf_b = plaintext.clone();
		cipher_b.encrypt_block(&mut buf_b, 0);

		// -- Check
		assert_ne!(
			buf_a, buf_b,
			"different keys must produce different ciphertext for the same LBA/plaintext"
		);

		Ok(())
	}

	#[test]
	fn test_reserved_header_and_trailer_blocks_are_never_encrypted() -> TestResult<()> {
		// -- Setup & Fixtures: a 1000-block volume.
		let total = Some(1000u64);

		// -- Check: the plaintext header (superblock copies, intent log,
		// genesis audit) must never be encrypted — the superblock is where
		// the "this volume is encrypted" flag itself lives, so encrypting
		// it would be unreadable-by-construction.
		for lba in 0..crate::RESERVED_HEADER_BLOCKS {
			assert!(
				!block_is_encrypted(lba, total),
				"reserved header block {lba} must stay plaintext"
			);
		}
		// The trailing backup superblock copies, likewise.
		for lba in (1000 - crate::RESERVED_TRAILER_BLOCKS)..1000 {
			assert!(
				!block_is_encrypted(lba, total),
				"reserved trailer block {lba} must stay plaintext"
			);
		}
		// Everything in between (the object region) is encrypted.
		assert!(block_is_encrypted(crate::RESERVED_HEADER_BLOCKS, total));
		assert!(block_is_encrypted(500, total));
		assert!(block_is_encrypted(1000 - crate::RESERVED_TRAILER_BLOCKS - 1, total));

		Ok(())
	}

	#[test]
	fn test_registry_only_applies_to_the_activated_partition() -> TestResult<()> {
		// -- Setup & Fixtures: activate the data volume's partition (590848) only.
		const DATA_PART: u32 = 590_848;
		const SYSTEM_PART: u32 = 133_120;
		activate(DATA_PART, 10_000, &test_key());

		let plaintext = vec![0x77u8; crate::BLOCK_SIZE as usize];

		// -- Exec & Check: a block on the activated partition IS encrypted...
		let mut on_data = plaintext.clone();
		maybe_encrypt_block(DATA_PART, 100, &mut on_data);
		assert_ne!(
			on_data, plaintext,
			"blocks on the activated partition must be encrypted"
		);

		// ...and round-trips back.
		maybe_decrypt_block(DATA_PART, 100, &mut on_data);
		assert_eq!(
			on_data, plaintext,
			"the same partition/LBA must decrypt back to plaintext"
		);

		// -- Check: a block on a DIFFERENT partition (e.g. the system volume,
		// signed and must never be encrypted) passes through untouched.
		let mut on_system = plaintext.clone();
		maybe_encrypt_block(SYSTEM_PART, 100, &mut on_system);
		assert_eq!(
			on_system, plaintext,
			"a non-activated partition must never be encrypted"
		);

		// -- Check: a reserved-header block on the activated partition also
		// passes through untouched.
		let mut header = plaintext.clone();
		maybe_encrypt_block(DATA_PART, 0, &mut header);
		assert_eq!(header, plaintext, "the superblock block must never be encrypted");

		assert!(is_active_for(DATA_PART));
		assert!(!is_active_for(SYSTEM_PART));

		Ok(())
	}

	#[test]
	fn test_multi_block_volume_round_trips_and_blocks_stay_independently_addressable() -> TestResult<()> {
		// -- Setup & Fixtures: simulate several consecutive LBAs the way
		// commit()/AtaBlockDevice actually calls this — one encrypt_block
		// per physical block, not a single multi-block operation.
		let cipher = DataVolumeCipher::new(&test_key());
		let block = crate::BLOCK_SIZE as usize;
		let mut disk: alloc::vec::Vec<alloc::vec::Vec<u8>> = (0..8).map(|i| vec![i as u8; block]).collect();

		// -- Exec: encrypt every block at its own LBA, then decrypt in a
		// *different* order — proves each block's ciphertext only depends
		// on its own (key, lba, plaintext), not on encrypt/decrypt order.
		for (lba, buf) in disk.iter_mut().enumerate() {
			cipher.encrypt_block(buf, lba as u64);
		}
		for lba in (0..8).rev() {
			cipher.decrypt_block(&mut disk[lba], lba as u64);
		}

		// -- Check
		for (i, buf) in disk.iter().enumerate() {
			assert!(
				buf.iter().all(|&b| b == i as u8),
				"block {i} did not round-trip correctly"
			);
		}

		Ok(())
	}
}
