//! Credential handling shared by `lfs-read` and `lfs-write`.
//!
//! Both tools take a credential the same way and pass it to the same two entry
//! points (`HostFs::mount_encrypted`, `Store::open_encrypted`), so the parsing
//! and the reading of the secret live in one place rather than in two copies
//! that can drift apart. This is CLI support, not part of the crate's API: it
//! reads stdin and the filesystem, which a library type should not do.
//!
//! The passphrase is deliberately a flag with no argument. An argument would be
//! visible in `ps` for as long as the process runs and would land in the shell's
//! history file. Reading one line from stdin also composes with a secret manager
//! (`secret-tool lookup ... | lfs-read ...`), which an argv value cannot do
//! without exposing the secret first.

use std::io::BufRead;
use std::path::PathBuf;

use ledgerfs_host::{Error, Result, UnlockCredential};
use zeroize::{Zeroize, Zeroizing};

/// Where the credential for an encrypted volume comes from.
pub(crate) enum UnlockSource {
	None,
	Passphrase,
	KeyFile(PathBuf),
}

impl UnlockSource {
	pub(crate) fn is_none(&self) -> bool {
		matches!(self, Self::None)
	}

	/// Read the credential, if one was named.
	///
	/// Called only when a volume is really being opened, so `parts` on a locked
	/// image never blocks waiting for input it would not use.
	pub(crate) fn read(&self) -> Result<Option<Unlock>> {
		match self {
			Self::None => Ok(None),
			Self::Passphrase => {
				let mut line = String::new();
				std::io::stdin().lock().read_line(&mut line).map_err(Error::Io)?;
				let passphrase = line.trim_end_matches(['\n', '\r']).as_bytes().to_vec();
				if passphrase.is_empty() {
					return Err(Error::Usage(
						"--passphrase read an empty line from stdin; there is nothing to unlock with".to_string(),
					));
				}
				Ok(Some(Unlock::Passphrase(Zeroizing::new(passphrase))))
			}
			Self::KeyFile(path) => {
				let mut bytes = std::fs::read(path).map_err(Error::Io)?;
				let key: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
					Error::Usage(format!(
						"--key-file {} holds {} bytes; a key-encryption key is exactly 32",
						path.display(),
						bytes.len()
					))
				})?;
				bytes.zeroize();
				Ok(Some(Unlock::KeyEncryptionKey(Zeroizing::new(key))))
			}
		}
	}
}

/// A credential that has been read, and wipes itself when it goes out of scope.
pub(crate) enum Unlock {
	Passphrase(Zeroizing<Vec<u8>>),
	KeyEncryptionKey(Zeroizing<[u8; 32]>),
}

impl Unlock {
	pub(crate) fn credential(&self) -> UnlockCredential<'_> {
		match self {
			Self::Passphrase(passphrase) => UnlockCredential::Passphrase(passphrase.as_slice()),
			Self::KeyEncryptionKey(key) => UnlockCredential::KeyEncryptionKey(key),
		}
	}
}
