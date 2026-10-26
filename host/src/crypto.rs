//! Transparent at-rest encryption for a whole volume.
//!
//! This is the host half of FORMAT.md §36. The format crate owns the two
//! primitives (the XTS-AES-256 block cipher and the keyslot encoding); this
//! module supplies what turns them into a volume a caller can actually use:
//!
//! - [`VolumeKey`], 64 random bytes generated once at format time, wiped when
//!   the last handle to it is dropped;
//! - [`UnlockCredential`], the two ways a volume may be unlocked (a passphrase
//!   stretched into a key-encryption key, or a key-encryption key supplied
//!   directly, which is the slot kind an HSM fills);
//! - [`EncryptedDevice`], a [`BlockDevice`] that encrypts and decrypts each
//!   4 KiB block in place, below everything else in the crate. Path
//!   resolution, the commit engine, the allocator and the staging tier all run
//!   on top of it unchanged, which is the point: none of them knows encryption
//!   exists.
//!
//! [`Store::format_encrypted`](crate::Store::format_encrypted) and
//! [`Store::open_encrypted`](crate::Store::open_encrypted) are the entry
//! points. The keyslot lifecycle (`add_slot`, `revoke_slot`,
//! `change_passphrase`) lives on [`Store`](crate::Store) because it must
//! update the in-memory superblock together with the four on-disk copies.
//!
//! # What is encrypted
//!
//! Only the object region, blocks `[RESERVED_HEADER_BLOCKS, total - 2)`. The
//! reserved header (superblocks, intent log, genesis audit record, allocation
//! snapshot) and the two trailing superblock copies stay plaintext, because the
//! superblock carries the feature flag that says the volume is encrypted and
//! has to be readable before any key exists. The privacy consequence is stated
//! in `ledgerfs::data_crypto` and in FORMAT.md §36.2: contents are protected,
//! metadata is not.
//!
//! # Key lifetime and the hint cache
//!
//! The cipher inside [`EncryptedDevice`] is built from the same [`VolumeKey`]
//! the [`Store`](crate::Store) keeps, and both are dropped with the store, so
//! the key bytes are zeroized when the last handle goes away. The expanded AES
//! key schedules inside the cipher cannot be zeroed through the `aes` API; that
//! limitation is documented on `ledgerfs::data_crypto` and is not papered over
//! here.
//!
//! An encrypted volume deliberately reports **no hint cache**
//! ([`BlockDevice::hint_cache`] returns `None`). The sidecar maps every object
//! id to the block it lives on, and an object id is the BLAKE3 hash of the
//! plaintext, which would put a content-derived identifier for every object on
//! disk in the clear. The cost is that an encrypted volume pays the linear
//! scan on each open instead of reading its hint sidecar.

use std::fmt;
use std::sync::Arc;

use ledgerfs::BLOCK_SIZE;
use ledgerfs::data_crypto::{DATA_VOLUME_KEY_LEN, DataVolumeCipher, block_is_encrypted};
use ledgerfs::keyslot::{
	DEFAULT_KDF_ITERATIONS, KdfParams, KeySlot, SLOT_COUNT, SlotKind, derive_kek, open_slot, seal_slot,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::device::{BlockDevice, HintCache};
use crate::error::{Error, Result};
use crate::volume::Volume;

// region:    --- Key material

/// The 64 bytes a volume is encrypted with: two independent AES-256 keys for
/// XTS (the second derives the tweak).
///
/// Generated once by [`Store::format_encrypted`](crate::Store::format_encrypted)
/// from the operating system's random source, never derived from anything on
/// the disk, and never written to it except wrapped inside a keyslot. The bytes
/// are wiped when the last reference is dropped.
///
/// The `Debug` implementation redacts them.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct VolumeKey([u8; DATA_VOLUME_KEY_LEN]);

impl VolumeKey {
	/// Generate a new volume key from the operating system's entropy source.
	pub fn generate() -> Result<Self> {
		let mut key = [0u8; DATA_VOLUME_KEY_LEN];
		fill_random(&mut key)?;
		Ok(Self(key))
	}

	/// Adopt key material from somewhere else, for example a key-management
	/// system that stores the master key and rewraps it for this volume.
	///
	/// The bytes are not validated: XTS accepts any 64 bytes, including all
	/// zeros. A caller using this constructor is responsible for the key's
	/// quality, because a weak key here is a weak volume and the format has no
	/// way to tell the difference.
	pub fn from_bytes(bytes: [u8; DATA_VOLUME_KEY_LEN]) -> Self {
		Self(bytes)
	}

	pub(crate) fn bytes(&self) -> &[u8; DATA_VOLUME_KEY_LEN] {
		&self.0
	}
}

impl fmt::Debug for VolumeKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("VolumeKey(<redacted>)")
	}
}

/// The two ways a volume can be unlocked.
///
/// Both end at the same place: a key-encryption key that unwraps one of the
/// volume's slots into the master [`VolumeKey`]. Which one is used is recorded
/// in the slot, so a volume provisioned with both accepts either and revoking
/// one does not disturb the other.
#[derive(Clone, Copy)]
pub enum UnlockCredential<'a> {
	/// A user passphrase. It is stretched with the slot's recorded KDF
	/// parameters into a 32-byte key-encryption key.
	Passphrase(&'a [u8]),

	/// A 32-byte key-encryption key supplied directly, which is what an HSM or
	/// a machine-held secret provides. It is not stretched; the slot is sealed
	/// under it as given.
	KeyEncryptionKey(&'a [u8; 32]),
}

impl fmt::Debug for UnlockCredential<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Passphrase(passphrase) => write!(f, "Passphrase(<{} bytes>)", passphrase.len()),
			Self::KeyEncryptionKey(_) => f.write_str("KeyEncryptionKey(<redacted>)"),
		}
	}
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
	getrandom::fill(buf).map_err(|e| Error::Entropy { detail: e.to_string() })
}

// endregion: --- Key material

// region:    --- Slot provisioning and unlocking

/// Seal `master` into a fresh slot under `credential`.
///
/// A passphrase slot gets a new random salt and the default stretching cost; an
/// HSM slot records no stretching, because there is nothing to stretch. Both
/// get a new random IV.
pub(crate) fn seal_credential(credential: UnlockCredential<'_>, master: &VolumeKey) -> Result<KeySlot> {
	match credential {
		UnlockCredential::Passphrase(passphrase) => {
			if passphrase.is_empty() {
				return Err(Error::Usage(
					"an empty passphrase is not a credential: it would protect the volume with nothing".to_string(),
				));
			}
			let mut salt = [0u8; 16];
			fill_random(&mut salt)?;
			let params = KdfParams {
				algorithm: 1,
				iterations: DEFAULT_KDF_ITERATIONS,
				salt,
			};
			let kek = Zeroizing::new(derive_kek(passphrase, &params)?);
			let mut iv = [0u8; 16];
			fill_random(&mut iv)?;
			Ok(seal_slot(SlotKind::Passphrase, params, &kek, iv, master.bytes())?)
		}
		UnlockCredential::KeyEncryptionKey(kek) => {
			let mut iv = [0u8; 16];
			fill_random(&mut iv)?;
			let params = KdfParams {
				algorithm: 1,
				iterations: 0,
				salt: [0u8; 16],
			};
			Ok(seal_slot(SlotKind::Hsm, params, kek, iv, master.bytes())?)
		}
	}
}

/// Recover the master key from any slot that `credential` opens.
///
/// Every decoded superblock copy is tried, not just the one `mount_select`
/// chose. That matters after a crash during a keyslot change: the copies are
/// written one at a time, so a crash can leave some holding the old credential
/// and some the new one, and a volume must open with either. A copy that was
/// torn mid-write fails its superblock checksum and was never decoded at all.
///
/// Passphrase derivations are cached by their recorded parameters, so the four
/// identical copies of one slot cost one stretching operation rather than four.
pub(crate) fn unlock(volume: &Volume, credential: UnlockCredential<'_>) -> Result<VolumeKey> {
	let mut derivations = derivation_cache::Cache::new();

	for copy in volume.copies.iter().flatten() {
		for slot in &copy.keyslots {
			if slot.is_empty() {
				continue;
			}

			let kek: Zeroizing<[u8; 32]> = match (credential, slot.kind) {
				(UnlockCredential::Passphrase(passphrase), SlotKind::Passphrase) => {
					if let Some(idx) = derivations.position(&slot.kdf) {
						derivations.get(idx)
					} else {
						// An unknown KDF algorithm makes this slot unopenable, not
						// the volume: another copy or slot may carry one this
						// build understands.
						let Ok(kek) = derive_kek(passphrase, &slot.kdf) else {
							continue;
						};
						derivations.insert(slot.kdf, kek)
					}
				}
				(UnlockCredential::KeyEncryptionKey(kek), SlotKind::Hsm) => Zeroizing::new(*kek),
				_ => continue,
			};

			if let Ok(mut master) = open_slot(slot, &kek) {
				let key = VolumeKey::from_bytes(master);
				// The key now lives in `key`, which wipes itself when the store
				// drops it. This frame's copy is wiped here so a successful
				// unlock does not leave the master key on the stack.
				master.fill(0);
				return Ok(key);
			}
		}
	}

	let tried = match credential {
		UnlockCredential::Passphrase(_) => "no passphrase slot opened",
		UnlockCredential::KeyEncryptionKey(_) => "no key-encryption-key slot opened",
	};
	Err(Error::WrongCredential {
		detail: format!("{tried}: the credential is wrong or this volume has no such slot"),
	})
}

/// Read the plaintext superblocks, unlock the volume, and hand back the device
/// together with the key that opened it.
///
/// Shared by [`Store::open_encrypted`](crate::Store::open_encrypted) and
/// [`HostFs::mount_encrypted`](crate::HostFs::mount_encrypted) so the two entry
/// points cannot disagree about what a credential opens, or about which error a
/// volume that was never encrypted gets.
pub(crate) fn unlock_device(
	device: Box<dyn BlockDevice>,
	credential: UnlockCredential<'_>,
) -> Result<(Box<dyn BlockDevice>, Volume, Arc<VolumeKey>)> {
	let volume = Volume::mount(device.as_ref())?;
	if !volume.superblock.is_data_encrypted() {
		return Err(Error::VolumeNotEncrypted {
			detail: "it was not formatted encrypted, so it takes no credential; open it without one".to_string(),
		});
	}

	let key = Arc::new(unlock(&volume, credential)?);
	Ok((device, volume, key))
}

/// A tiny cache of passphrase derivations, kept private because the only
/// interesting thing about it is that it exists.
mod derivation_cache {
	use super::KdfParams;
	use zeroize::Zeroizing;

	pub(super) struct Cache {
		entries: Vec<(KdfParams, Zeroizing<[u8; 32]>)>,
	}

	impl Cache {
		pub(super) fn new() -> Self {
			Self { entries: Vec::new() }
		}

		pub(super) fn position(&self, params: &KdfParams) -> Option<usize> {
			self.entries.iter().position(|(p, _)| p == params)
		}

		pub(super) fn get(&self, idx: usize) -> Zeroizing<[u8; 32]> {
			self.entries[idx].1.clone()
		}

		pub(super) fn insert(&mut self, params: KdfParams, kek: [u8; 32]) -> Zeroizing<[u8; 32]> {
			let kek = Zeroizing::new(kek);
			self.entries.push((params, kek.clone()));
			kek
		}
	}
}

// endregion: --- Slot provisioning and unlocking

// region:    --- Encrypted device

/// A [`BlockDevice`] that encrypts and decrypts every block of the object
/// region.
///
/// The wrapper sits below the whole crate. A [`Store`](crate::Store) built on
/// one behaves exactly like a store on a plaintext volume; the difference is
/// what reaches the medium.
///
/// # Contract
///
/// **Reads** may name any byte range. A range that is not block aligned is
/// served from a scratch buffer covering the blocks it touches, because reads
/// do not modify anything and one `ObjectRecord` read legitimately asks for an
/// exact byte length rather than a whole number of blocks.
///
/// **Writes** must be a whole number of [`BLOCK_SIZE`] blocks starting on a
/// block boundary. That is what every block-addressed method on [`BlockDevice`]
/// already issues, and a partial write is refused rather than emulated: it
/// would have to decrypt, splice and re-encrypt, turning one write into a
/// read-modify-write the caller did not ask for, with a cost it cannot see.
pub struct EncryptedDevice {
	inner: Box<dyn BlockDevice>,
	cipher: DataVolumeCipher,
	total_blocks: u64,
}

impl EncryptedDevice {
	/// Encrypt `inner`'s object region under `key`.
	///
	/// `total_blocks` is the *volume's* size, which is what locates the
	/// plaintext trailer; it may be smaller than the device, exactly as a
	/// volume inside a partition is.
	pub fn new(inner: Box<dyn BlockDevice>, total_blocks: u64, key: &VolumeKey) -> Self {
		Self {
			inner,
			cipher: DataVolumeCipher::new(key.bytes()),
			total_blocks,
		}
	}

	/// The volume size this wrapper was built for.
	pub fn total_blocks(&self) -> u64 {
		self.total_blocks
	}

	fn for_each_encrypted_block<F>(&self, buf: &mut [u8], offset: u64, mut f: F)
	where
		F: FnMut(&DataVolumeCipher, &mut [u8], u64),
	{
		let first_lba = offset / BLOCK_SIZE as u64;
		for (i, chunk) in buf.chunks_mut(BLOCK_SIZE as usize).enumerate() {
			let lba = first_lba + i as u64;
			if block_is_encrypted(lba, self.total_blocks) {
				f(&self.cipher, chunk, lba);
			}
		}
	}
}

impl BlockDevice for EncryptedDevice {
	fn size_bytes(&self) -> u64 {
		self.inner.size_bytes()
	}

	fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
		if buf.is_empty() {
			return self.inner.read_bytes_at(offset, buf);
		}
		let block = BLOCK_SIZE as u64;

		if offset.is_multiple_of(block) && buf.len().is_multiple_of(block as usize) {
			self.inner.read_bytes_at(offset, buf)?;
			self.for_each_encrypted_block(buf, offset, |cipher, chunk, lba| cipher.decrypt_block(chunk, lba));
			return Ok(());
		}

		// An unaligned range. One `ObjectRecord` read asks for exactly
		// `header + stored_len` bytes, which is rarely a whole number of
		// blocks, so the read is served from the blocks that cover it and the
		// requested slice is copied out.
		let end = offset.checked_add(buf.len() as u64).ok_or(Error::OutOfBounds {
			offset,
			len: buf.len(),
			size_bytes: self.size_bytes(),
		})?;
		let first_lba = offset / block;
		let last_lba = (end - 1) / block;
		let span = (last_lba - first_lba + 1) * block;
		let span = usize::try_from(span).map_err(|_| Error::OutOfBounds {
			offset,
			len: buf.len(),
			size_bytes: self.size_bytes(),
		})?;

		let mut scratch = vec![0u8; span];
		self.inner.read_bytes_at(first_lba * block, &mut scratch)?;
		self.for_each_encrypted_block(&mut scratch, first_lba * block, |cipher, chunk, lba| {
			cipher.decrypt_block(chunk, lba)
		});

		let start = (offset - first_lba * block) as usize;
		buf.copy_from_slice(&scratch[start..start + buf.len()]);
		Ok(())
	}

	fn write_bytes_at(&self, offset: u64, data: &[u8]) -> Result<()> {
		if data.is_empty() {
			return self.inner.write_bytes_at(offset, data);
		}
		require_block_aligned_write(offset, data.len())?;

		let first_lba = offset / BLOCK_SIZE as u64;
		let any_encrypted = data
			.chunks(BLOCK_SIZE as usize)
			.enumerate()
			.any(|(i, _)| block_is_encrypted(first_lba + i as u64, self.total_blocks));
		if !any_encrypted {
			return self.inner.write_bytes_at(offset, data);
		}

		// Never encrypt in place over the caller's buffer: `write_bytes_at`
		// takes `&[u8]` and the caller may still be holding it.
		let mut buf = data.to_vec();
		self.for_each_encrypted_block(&mut buf, offset, |cipher, chunk, lba| cipher.encrypt_block(chunk, lba));
		self.inner.write_bytes_at(offset, &buf)
	}

	fn barrier(&self) -> Result<()> {
		self.inner.barrier()
	}

	fn is_writable(&self) -> bool {
		self.inner.is_writable()
	}

	/// Deliberately `None`: the hint sidecar maps object ids to blocks, and an
	/// object id is a BLAKE3 hash of the plaintext. See this module's docs.
	fn hint_cache(&self) -> Option<HintCache> {
		None
	}
}

fn require_block_aligned_write(offset: u64, len: usize) -> Result<()> {
	let block = BLOCK_SIZE as usize;
	if !offset.is_multiple_of(BLOCK_SIZE as u64) || !len.is_multiple_of(block) {
		return Err(Error::EncryptedWriteMisaligned {
			offset,
			len,
			block_size: BLOCK_SIZE,
		});
	}
	Ok(())
}

// endregion: --- Encrypted device

// region:    --- Store helpers

/// The keyslot array a freshly formatted volume starts with: slot 0 sealed
/// under `credential`, slot 1 free.
///
/// Kept here so the store module does not have to know how a slot is sealed.
pub(crate) fn initial_slots(key: &Arc<VolumeKey>, credential: UnlockCredential<'_>) -> Result<[KeySlot; SLOT_COUNT]> {
	let slot = seal_credential(credential, key)?;
	let mut slots = [KeySlot::empty(); SLOT_COUNT];
	slots[0] = slot;
	Ok(slots)
}

/// Refuse a role that only makes sense on an encrypted store.
pub(crate) fn require_encrypted(key: &Option<Arc<VolumeKey>>) -> Result<Arc<VolumeKey>> {
	key.as_ref().map(Arc::clone).ok_or_else(|| Error::VolumeNotEncrypted {
		detail: "the store was opened without a volume key, so it has no keyslots to manage".to_string(),
	})
}

// endregion: --- Store helpers
