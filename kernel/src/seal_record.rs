//! Shared `BootSealRecord` type — the single canonical definition.
//!
//! This struct was previously identically duplicated in 5 places
//! (forge-boot, forge-builder, crucible-keygen-seal ×2, crucible-installer).
//! All consumers MUST import from this crate to prevent postcard decode
//! breakage when fields are added or reordered.
//!
//! # Wire Format
//!
//! Stored as a `postcard`-serialized blob at LedgerFS block 5, prefixed by a
//! 4-byte little-endian length field (see `write_padded_block` in the
//! installer).  Also used for `.elf.sig` sidecar files (TCB verification).
//!
//! # Schema Version
//!
//! Increment [`ROOT_SEAL_SCHEMA_V1`] if the wire format ever changes (and
//! update every consumer's deserialization to handle both old and new).

use alloc::vec::Vec;

/// Current root-seal schema version (`1`).
pub const ROOT_SEAL_SCHEMA_V1: u16 = 1;

/// Schema version for records carrying the SLH-DSA backup signature (`2`).
///
/// Used by TCB `.sig` sidecars, which are ordinary files and so have no block
/// budget. The **root seal at block 5 stays v1**: an SLH-DSA-SHA2-128f
/// signature is 17 088 bytes against 4 092 usable bytes per block, so carrying
/// one there would need roughly six blocks and a change to the reserved header
/// layout (`RESERVED_HEADER_BLOCKS` is 6, and block 5 is already the last).
///
/// That split is deliberate: it buys dual-scheme verification for every TCB
/// binary — the things that actually execute — without an on-disk migration.
pub const ROOT_SEAL_SCHEMA_V2: u16 = 2;

/// Compact root seal record stored at LedgerFS block 5 for boot verification.
///
/// The installer produces a full dual-sig [`temper::Seal`] (ML-DSA-65
/// + SLH-DSA) via [`temper::create_seal`].  Only the primary
/// (ML-DSA-65) data and the signing binding are stored here so the record fits
/// in a single 4 KiB block.  The complete `Seal` (including the SLH-DSA backup)
/// is preserved for post-boot TCB-level verification, which is not yet
/// implemented.
///
/// # Approximate Sizes
///
/// | Field             | Bytes  |
/// |-------------------|--------|
/// | schema_version    | 2      |
/// | content_hash_raw  | 32     |
/// | binding (varint+data) | ~200 |
/// | mldsa_sig (varint+data) | ~3312 |
/// | mldsa_key_id_raw  | 32     |
/// | **Total**         | **~3580** |
///
/// Fits comfortably within the 4 092 usable bytes per block.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct BootSealRecord {
	/// Schema version — currently [`ROOT_SEAL_SCHEMA_V1`] (`1`).
	pub schema_version: u16,
	/// BLAKE3 hash of superblock block 0 (raw 32 bytes).
	pub content_hash_raw: [u8; 32],
	/// Signing binding as constructed by `create_seal`:
	/// `hex(content_hash).as_bytes() || postcard(SealContext)`.
	pub binding: Vec<u8>,
	/// Raw ML-DSA-65 signature bytes (typically 3 309 bytes).
	pub mldsa_sig: Vec<u8>,
	/// BLAKE3(mldsa_public_key) — raw 32 bytes.
	pub mldsa_key_id_raw: [u8; 32],
	/// Optional hardware co-signature: RSA-2048 signature of
	/// `content_hash_raw` from the ESP32 HSM's key store.
	/// `None` for seals that weren't hardware co-signed.
	/// `#[serde(default)]` ensures old sidecar files (pre-HSM) deserialise
	/// without this field — they simply get `None`.
	#[serde(default)]
	pub hsm_rsa_sig: Option<Vec<u8>>,
	/// Which HSM key-store slot signed `hsm_rsa_sig`, if present.
	/// See `hsm_rsa_sig` for the backward-compat rationale.
	#[serde(default)]
	pub hsm_key_id: Option<u8>,
	/// Raw SLH-DSA-SHA2-128f signature over the same `binding` (17 088 bytes).
	///
	/// Present in schema v2 sidecars, absent from v1 and from the root seal
	/// (see [`ROOT_SEAL_SCHEMA_V2`] for why the root seal cannot carry one).
	///
	/// **Whether this is required is decided by the system's key material, not
	/// by the record.** A verifier that holds an SLH-DSA public key demands a
	/// signature here; otherwise a v1 sidecar could be substituted for a v2 one
	/// and silently downgrade the check to a single scheme, which would defeat
	/// the entire point of carrying two.
	#[serde(default)]
	pub slhdsa_sig: Option<Vec<u8>>,
	/// BLAKE3(slhdsa_public_key) — binds the backup signature to a key, exactly
	/// as `mldsa_key_id_raw` does for the primary.
	#[serde(default)]
	pub slhdsa_key_id_raw: Option<[u8; 32]>,
}
