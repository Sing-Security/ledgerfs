//! LUKS-style volume keyslots.
//!
//! A volume's data is encrypted with a **master key** that is generated
//! randomly once, at format time, and never derived from anything guessable.
//! That master key is then stored on disk *wrapped* — encrypted under a
//! key-encryption key (KEK) — once per keyslot. Unlocking the volume means
//! producing a KEK, unwrapping a slot, and checking that what came out is
//! really the master key.
//!
//! Two slot kinds, matching the two ways a volume may be unlocked:
//!
//! - [`SlotKind::Passphrase`] — KEK stretched from a user passphrase and a
//!   per-slot random salt. Resists theft of the whole machine, because the
//!   secret is not on it.
//! - [`SlotKind::Hsm`] — KEK held by the HSM co-processor. Allows unattended
//!   boot; security then rests on the HSM not being present with the disk.
//!
//! Both may be populated at once: either unlocks the volume, and revoking one
//! does not disturb the other. That is the entire reason for indirection
//! through a master key — changing a passphrase rewraps 64 bytes rather than
//! re-encrypting the volume.
//!
//! # What this deliberately is not
//!
//! The stretching below is iterated BLAKE3, not Argon2 or scrypt. It raises
//! the cost of guessing a passphrase linearly in [`KdfParams::iterations`],
//! but it is **not memory-hard**, so it is far weaker against GPU or ASIC
//! attack than a modern password KDF. This is stated rather than implied: a
//! passphrase slot is only as good as the passphrase behind it. Swapping in
//! a memory-hard KDF later is a slot-format change, which is why
//! [`KdfParams`] carries an explicit `algorithm` field.

extern crate alloc;

use crate::error::{Error, Result};

// region: --- Constants

/// Bytes one keyslot occupies on disk.
pub const SLOT_SIZE: usize = 128;

/// Number of keyslots a volume carries.
pub const SLOT_COUNT: usize = 2;

/// Byte offset of the keyslot array within the 4 KiB superblock block.
///
/// Sits in the slack region after `ACTIVE_ROOT_HINT_OFFSET`. Keyslots live in
/// the superblock rather than a dedicated block for two reasons: the
/// superblock is written in **four copies**, so key material inherits that
/// redundancy for free, and a volume that has never been encrypted reads
/// zeros here and decodes as "no slots", needing no format migration.
pub const KEYSLOTS_OFFSET: usize = 3772;

/// Length of the master key protected by a slot — 64 bytes, matching
/// `data_crypto::DATA_VOLUME_KEY_LEN` (XTS-AES-256 uses two 32-byte keys).
pub const MASTER_KEY_LEN: usize = 64;

/// Default passphrase stretching rounds.
///
/// Chosen to cost roughly a tenth of a second on the hardware this targets —
/// slow enough to matter across a guessing campaign, fast enough not to be
/// noticed once at mount.
pub const DEFAULT_KDF_ITERATIONS: u32 = 200_000;

// endregion: --- Constants

// region: --- Types

/// Which credential unlocks a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
	/// Nothing stored here.
	Empty,
	/// KEK stretched from a user passphrase (see [`KdfParams`]).
	Passphrase,
	/// KEK supplied by the HSM co-processor.
	Hsm,
}

impl SlotKind {
	fn to_u8(self) -> u8 {
		match self {
			SlotKind::Empty => 0,
			SlotKind::Passphrase => 1,
			SlotKind::Hsm => 2,
		}
	}

	fn from_u8(v: u8) -> Self {
		match v {
			1 => SlotKind::Passphrase,
			2 => SlotKind::Hsm,
			// Anything unrecognised reads as empty rather than erroring: an
			// unknown slot kind means this volume was written by a newer
			// build, and refusing to parse the superblock over it would make
			// the volume unmountable instead of merely that slot unusable.
			_ => SlotKind::Empty,
		}
	}
}

/// Key-derivation parameters recorded per slot, so a volume written by one
/// build can still be unlocked by another that has changed the defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
	/// `1` = iterated BLAKE3 (the only algorithm implemented). Stored
	/// explicitly so a future memory-hard KDF can be added without guessing
	/// which one an existing slot used.
	pub algorithm: u8,
	/// Stretching rounds. Ignored for [`SlotKind::Hsm`].
	pub iterations: u32,
	/// Per-slot random salt. Two volumes with the same passphrase must not
	/// produce the same KEK, and a precomputed table must not apply to both.
	pub salt: [u8; 16],
}

/// One keyslot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySlot {
	pub kind: SlotKind,
	pub kdf: KdfParams,
	/// Master key encrypted under this slot's KEK (AES-256-CBC).
	pub wrapped_key: [u8; MASTER_KEY_LEN],
	/// CBC initialisation vector for `wrapped_key`.
	pub iv: [u8; 16],
	/// BLAKE3-64 of the *plaintext* master key.
	///
	/// This is what distinguishes "wrong passphrase" from "corrupt slot":
	/// unwrapping with the wrong KEK yields 64 bytes of garbage that would
	/// otherwise be installed as the volume key, and every subsequent read
	/// would return noise with no indication why.
	pub key_checksum: u64,
}

impl KeySlot {
	/// An unoccupied slot.
	pub const fn empty() -> Self {
		Self {
			kind: SlotKind::Empty,
			kdf: KdfParams {
				algorithm: 1,
				iterations: 0,
				salt: [0u8; 16],
			},
			wrapped_key: [0u8; MASTER_KEY_LEN],
			iv: [0u8; 16],
			key_checksum: 0,
		}
	}

	pub fn is_empty(&self) -> bool {
		self.kind == SlotKind::Empty
	}
}

// endregion: --- Types

// region: --- Key derivation

/// Stretch `passphrase` with `params` into a 32-byte KEK.
///
/// Iterated BLAKE3: the salt and a domain string are bound into the first
/// block, then the digest is re-hashed `iterations` times. Each round feeds
/// the previous digest *and* the salt, so the chain cannot be shortened by
/// precomputing a prefix independent of the salt.
///
/// See this module's doc comment: this is not memory-hard.
pub fn derive_kek(passphrase: &[u8], params: &KdfParams) -> Result<[u8; 32]> {
	if params.algorithm != 1 {
		return Err(Error::custom("keyslot: unknown KDF algorithm"));
	}

	let mut hasher = blake3::Hasher::new();
	hasher.update(b"LedgerFS.VolumeKeyslot.KEK.v1");
	hasher.update(&params.salt);
	hasher.update(passphrase);
	let mut digest = *hasher.finalize().as_bytes();

	for _ in 0..params.iterations {
		let mut h = blake3::Hasher::new();
		h.update(&digest);
		h.update(&params.salt);
		digest = *h.finalize().as_bytes();
	}

	Ok(digest)
}

// endregion: --- Key derivation

// region: --- Wrap / unwrap

#[cfg(feature = "data-encryption")]
mod wrap {
	use aes::Aes256;
	use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

	const BLOCK: usize = 16;

	/// AES-256-CBC over a buffer whose length is a multiple of 16.
	/// `MASTER_KEY_LEN` is 64, so no padding scheme is needed or chosen.
	pub(super) fn cbc_encrypt(kek: &[u8; 32], iv: &[u8; 16], data: &mut [u8]) {
		let c = Aes256::new(&Array::from(*kek));
		let mut prev = *iv;
		for block in data.chunks_mut(BLOCK) {
			for (b, p) in block.iter_mut().zip(prev.iter()) {
				*b ^= *p;
			}
			let mut arr = Array::from(<[u8; BLOCK]>::try_from(&block[..]).unwrap());
			c.encrypt_block(&mut arr);
			block.copy_from_slice(&arr);
			prev.copy_from_slice(block);
		}
	}

	pub(super) fn cbc_decrypt(kek: &[u8; 32], iv: &[u8; 16], data: &mut [u8]) {
		let c = Aes256::new(&Array::from(*kek));
		let mut prev = *iv;
		for block in data.chunks_mut(BLOCK) {
			let this_ct = <[u8; BLOCK]>::try_from(&block[..]).unwrap();
			let mut arr = Array::from(this_ct);
			c.decrypt_block(&mut arr);
			block.copy_from_slice(&arr);
			for (b, p) in block.iter_mut().zip(prev.iter()) {
				*b ^= *p;
			}
			prev = this_ct;
		}
	}
}

/// Checksum of a plaintext master key, used to recognise a correct unwrap.
///
/// Only the sealing path below calls this, so it is gated with it: a build
/// without `data-encryption` parses key slots but never wraps or unwraps a key.
#[cfg(feature = "data-encryption")]
fn master_key_checksum(key: &[u8; MASTER_KEY_LEN]) -> u64 {
	let mut h = blake3::Hasher::new();
	h.update(b"LedgerFS.VolumeKeyslot.KeyCheck.v1");
	h.update(key);
	let d = h.finalize();
	u64::from_le_bytes(d.as_bytes()[..8].try_into().unwrap())
}

/// Wrap `master_key` into a slot unlocked by `kek`.
#[cfg(feature = "data-encryption")]
pub fn seal_slot(
	kind: SlotKind,
	kdf: KdfParams,
	kek: &[u8; 32],
	iv: [u8; 16],
	master_key: &[u8; MASTER_KEY_LEN],
) -> Result<KeySlot> {
	if kind == SlotKind::Empty {
		return Err(Error::custom("keyslot: cannot seal an Empty slot"));
	}
	let mut wrapped = *master_key;
	wrap::cbc_encrypt(kek, &iv, &mut wrapped);
	Ok(KeySlot {
		kind,
		kdf,
		wrapped_key: wrapped,
		iv,
		key_checksum: master_key_checksum(master_key),
	})
}

/// Recover the master key from `slot` using `kek`.
///
/// # Errors
///
/// The slot is empty, or the recovered key fails its checksum — which means
/// the KEK was wrong (wrong passphrase, wrong HSM) or the slot was tampered
/// with. Both are refusals, never a returned key: installing an unverified
/// 64 bytes as the volume key would turn every subsequent read into silent
/// noise.
#[cfg(feature = "data-encryption")]
pub fn open_slot(slot: &KeySlot, kek: &[u8; 32]) -> Result<[u8; MASTER_KEY_LEN]> {
	if slot.is_empty() {
		return Err(Error::custom("keyslot: slot is empty"));
	}
	let mut key = slot.wrapped_key;
	wrap::cbc_decrypt(kek, &slot.iv, &mut key);
	if master_key_checksum(&key) != slot.key_checksum {
		// Zeroise before returning: the caller must not see it, and it should
		// not linger on the stack either.
		key.fill(0);
		return Err(Error::custom("keyslot: wrong key or corrupt slot"));
	}
	Ok(key)
}

// endregion: --- Wrap / unwrap

// region: --- On-disk encoding

impl KeySlot {
	/// Serialize into exactly [`SLOT_SIZE`] bytes.
	///
	/// Layout: kind(1) ‖ algorithm(1) ‖ pad(2) ‖ iterations(4) ‖ salt(16) ‖
	/// iv(16) ‖ wrapped_key(64) ‖ key_checksum(8) ‖ reserved(16).
	pub fn encode(&self, out: &mut [u8]) -> Result<()> {
		if out.len() < SLOT_SIZE {
			return Err(Error::custom("keyslot: output buffer too small"));
		}
		out[..SLOT_SIZE].fill(0);
		out[0] = self.kind.to_u8();
		out[1] = self.kdf.algorithm;
		out[4..8].copy_from_slice(&self.kdf.iterations.to_le_bytes());
		out[8..24].copy_from_slice(&self.kdf.salt);
		out[24..40].copy_from_slice(&self.iv);
		out[40..104].copy_from_slice(&self.wrapped_key);
		out[104..112].copy_from_slice(&self.key_checksum.to_le_bytes());
		Ok(())
	}

	/// Parse a slot. A zeroed region decodes as [`SlotKind::Empty`].
	pub fn decode(buf: &[u8]) -> Result<Self> {
		if buf.len() < SLOT_SIZE {
			return Err(Error::custom("keyslot: input buffer too small"));
		}
		let kind = SlotKind::from_u8(buf[0]);
		if kind == SlotKind::Empty {
			return Ok(Self::empty());
		}
		let mut salt = [0u8; 16];
		salt.copy_from_slice(&buf[8..24]);
		let mut iv = [0u8; 16];
		iv.copy_from_slice(&buf[24..40]);
		let mut wrapped_key = [0u8; MASTER_KEY_LEN];
		wrapped_key.copy_from_slice(&buf[40..104]);
		Ok(Self {
			kind,
			kdf: KdfParams {
				algorithm: buf[1],
				iterations: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
				salt,
			},
			iv,
			wrapped_key,
			key_checksum: u64::from_le_bytes(buf[104..112].try_into().unwrap()),
		})
	}
}

/// Encode all slots into the superblock's keyslot region.
pub fn encode_slots(slots: &[KeySlot; SLOT_COUNT], block: &mut [u8]) -> Result<()> {
	if block.len() < KEYSLOTS_OFFSET + SLOT_COUNT * SLOT_SIZE {
		return Err(Error::custom("keyslot: superblock buffer too small"));
	}
	for (i, slot) in slots.iter().enumerate() {
		let off = KEYSLOTS_OFFSET + i * SLOT_SIZE;
		slot.encode(&mut block[off..off + SLOT_SIZE])?;
	}
	Ok(())
}

/// Decode all slots from the superblock's keyslot region.
pub fn decode_slots(block: &[u8]) -> Result<[KeySlot; SLOT_COUNT]> {
	if block.len() < KEYSLOTS_OFFSET + SLOT_COUNT * SLOT_SIZE {
		return Err(Error::custom("keyslot: superblock buffer too small"));
	}
	let mut slots = [KeySlot::empty(); SLOT_COUNT];
	for (i, slot) in slots.iter_mut().enumerate() {
		let off = KEYSLOTS_OFFSET + i * SLOT_SIZE;
		*slot = KeySlot::decode(&block[off..off + SLOT_SIZE])?;
	}
	Ok(slots)
}

/// Index of the first slot of `kind`, if any.
pub fn find_slot(slots: &[KeySlot; SLOT_COUNT], kind: SlotKind) -> Option<usize> {
	slots.iter().position(|s| s.kind == kind)
}

// endregion: --- On-disk encoding

#[cfg(test)]
mod tests {
	use super::*;

	extern crate alloc;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	fn params(iters: u32) -> KdfParams {
		KdfParams {
			algorithm: 1,
			iterations: iters,
			salt: [0x5A; 16],
		}
	}

	/// Sealing and opening a slot is the only thing that consumes a master
	/// key, and it is behind `data-encryption` — so this helper is too.
	#[cfg(feature = "data-encryption")]
	fn master() -> [u8; MASTER_KEY_LEN] {
		let mut k = [0u8; MASTER_KEY_LEN];
		for (i, b) in k.iter_mut().enumerate() {
			*b = (i as u8).wrapping_mul(7).wrapping_add(3);
		}
		k
	}

	#[test]
	fn test_kek_depends_on_passphrase_salt_and_iterations() -> TestResult<()> {
		let base = derive_kek(b"correct horse", &params(64))?;

		// Different passphrase.
		assert_ne!(base, derive_kek(b"correct horsf", &params(64))?);

		// Different salt — this is what stops one precomputed table from
		// applying to every volume that shares a passphrase.
		let mut other = params(64);
		other.salt = [0xA5; 16];
		assert_ne!(base, derive_kek(b"correct horse", &other)?);

		// Different iteration count must not collide either, or a slot's
		// recorded cost could be lowered without invalidating it.
		assert_ne!(base, derive_kek(b"correct horse", &params(65))?);

		// Deterministic for identical inputs, or nothing could ever unlock.
		assert_eq!(base, derive_kek(b"correct horse", &params(64))?);

		Ok(())
	}

	#[test]
	fn test_unknown_kdf_algorithm_is_refused() {
		let mut p = params(4);
		p.algorithm = 99;
		assert!(
			derive_kek(b"x", &p).is_err(),
			"an unimplemented KDF must not silently fall back to a known one"
		);
	}

	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_seal_then_open_recovers_the_master_key() -> TestResult<()> {
		let kek = derive_kek(b"passphrase", &params(128))?;
		let mk = master();

		let slot = seal_slot(SlotKind::Passphrase, params(128), &kek, [0x11; 16], &mk)?;

		// The stored bytes must not be the master key.
		assert_ne!(
			slot.wrapped_key, mk,
			"wrapped key must not equal the plaintext master key"
		);

		assert_eq!(open_slot(&slot, &kek)?, mk);
		Ok(())
	}

	/// The check that separates "wrong passphrase" from "corrupt slot" — and,
	/// more importantly, stops 64 bytes of garbage from being installed as
	/// the volume key, which would make every read return silent noise.
	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_wrong_passphrase_is_refused_rather_than_returning_garbage() -> TestResult<()> {
		let good = derive_kek(b"right", &params(128))?;
		let bad = derive_kek(b"wrong", &params(128))?;
		let slot = seal_slot(SlotKind::Passphrase, params(128), &good, [0x22; 16], &master())?;

		assert!(
			open_slot(&slot, &bad).is_err(),
			"a wrong KEK must be refused, never returned as a key"
		);
		Ok(())
	}

	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_tampered_slot_is_detected() -> TestResult<()> {
		let kek = derive_kek(b"pw", &params(64))?;
		let good = seal_slot(SlotKind::Passphrase, params(64), &kek, [0x33; 16], &master())?;

		// Flip a ciphertext bit.
		let mut t = good;
		t.wrapped_key[0] ^= 0x01;
		assert!(
			open_slot(&t, &kek).is_err(),
			"modified ciphertext must fail the checksum"
		);

		// Flip an IV bit — corrupts only the first block, still detectable.
		let mut t2 = good;
		t2.iv[0] ^= 0x01;
		assert!(open_slot(&t2, &kek).is_err(), "modified IV must fail the checksum");

		Ok(())
	}

	/// Two slots, two credentials, one volume: either opens it, and they are
	/// independent. This is the whole point of wrapping a master key instead
	/// of deriving the volume key from the passphrase directly.
	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_passphrase_and_hsm_slots_open_the_same_master_key_independently() -> TestResult<()> {
		let mk = master();
		let pass_kek = derive_kek(b"user passphrase", &params(128))?;
		let hsm_kek = [0xC3u8; 32]; // supplied by the co-processor, not stretched

		let mut slots = [KeySlot::empty(); SLOT_COUNT];
		slots[0] = seal_slot(SlotKind::Passphrase, params(128), &pass_kek, [0x44; 16], &mk)?;
		slots[1] = seal_slot(SlotKind::Hsm, params(0), &hsm_kek, [0x55; 16], &mk)?;

		assert_eq!(open_slot(&slots[0], &pass_kek)?, mk);
		assert_eq!(open_slot(&slots[1], &hsm_kek)?, mk);

		// A credential must not open the *other* slot.
		assert!(open_slot(&slots[1], &pass_kek).is_err());
		assert!(open_slot(&slots[0], &hsm_kek).is_err());

		// Revoking the passphrase slot must leave the HSM slot working —
		// otherwise changing a credential would strand the volume.
		slots[0] = KeySlot::empty();
		assert!(open_slot(&slots[0], &pass_kek).is_err());
		assert_eq!(open_slot(&slots[1], &hsm_kek)?, mk);

		Ok(())
	}

	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_slots_round_trip_through_the_superblock_region() -> TestResult<()> {
		let mk = master();
		let kek = derive_kek(b"pw", &params(64))?;
		let mut slots = [KeySlot::empty(); SLOT_COUNT];
		slots[0] = seal_slot(SlotKind::Passphrase, params(64), &kek, [0x66; 16], &mk)?;

		let mut block = alloc::vec![0u8; crate::BLOCK_SIZE as usize];
		encode_slots(&slots, &mut block)?;

		// Must not run past the slack region into the superblock checksum.
		assert!(
			KEYSLOTS_OFFSET + SLOT_COUNT * SLOT_SIZE <= crate::BLOCK_SIZE as usize - 8,
			"keyslots must fit before the superblock checksum"
		);

		let back = decode_slots(&block)?;
		assert_eq!(back[0], slots[0]);
		assert!(back[1].is_empty());
		assert_eq!(open_slot(&back[0], &kek)?, mk);

		Ok(())
	}

	/// A volume that predates keyslots reads zeros here and must decode as
	/// "no slots" rather than failing — otherwise adding this feature would
	/// make every existing volume unmountable.
	#[test]
	fn test_zeroed_region_decodes_as_no_slots() -> TestResult<()> {
		let block = alloc::vec![0u8; crate::BLOCK_SIZE as usize];
		let slots = decode_slots(&block)?;
		assert!(slots.iter().all(|s| s.is_empty()));
		assert_eq!(find_slot(&slots, SlotKind::Passphrase), None);
		assert_eq!(find_slot(&slots, SlotKind::Hsm), None);
		Ok(())
	}

	/// A superblock rewrite must not destroy key material.
	///
	/// `Superblock::encode` starts with `buf.fill(0)`, so anything it does not
	/// re-emit is erased. Because a commit rewrites the superblock, keyslots
	/// that were not a real struct field would be wiped by the first write
	/// after provisioning — leaving an encrypted volume permanently
	/// unopenable while every operation reported success.
	#[cfg(feature = "data-encryption")]
	#[test]
	fn test_keyslots_survive_a_superblock_encode_decode_cycle() -> TestResult<()> {
		use crate::superblock::Superblock;

		let mk = master();
		let kek = derive_kek(b"pw", &params(64))?;

		let mut sb = Superblock::new(4096);
		sb.keyslots[0] = seal_slot(SlotKind::Passphrase, params(64), &kek, [0x77; 16], &mk)?;

		let mut block = alloc::vec![0u8; crate::BLOCK_SIZE as usize];
		sb.encode(&mut block)?;
		let back = Superblock::decode(&block)?;

		assert_eq!(
			back.keyslots[0], sb.keyslots[0],
			"keyslot must survive a superblock round trip"
		);
		assert_eq!(
			open_slot(&back.keyslots[0], &kek)?,
			mk,
			"master key must still unwrap after the round trip"
		);

		// And a *second* rewrite (what a commit does repeatedly) must not
		// erode them either.
		let mut block2 = alloc::vec![0u8; crate::BLOCK_SIZE as usize];
		let mut sb2 = back;
		sb2.encode(&mut block2)?;
		let back2 = Superblock::decode(&block2)?;
		assert_eq!(
			open_slot(&back2.keyslots[0], &kek)?,
			mk,
			"repeated superblock writes must preserve key material"
		);

		Ok(())
	}

	#[test]
	fn test_find_slot_locates_each_kind() -> TestResult<()> {
		let mut slots = [KeySlot::empty(); SLOT_COUNT];
		slots[1].kind = SlotKind::Hsm;
		assert_eq!(find_slot(&slots, SlotKind::Hsm), Some(1));
		assert_eq!(find_slot(&slots, SlotKind::Passphrase), None);
		Ok(())
	}
}
