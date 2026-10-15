//! Filesystem mode (ReadWrite vs ReadOnlySafe)
//!
//! Provides read-only safe mode enforcement when split-brain or
//! continuity violations are detected during mount.

/// Filesystem operational mode
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FsMode {
	/// Normal read-write mode
	#[default]
	ReadWrite,

	/// Read-only safe mode (entered when split-brain or continuity violation detected)
	/// In this mode, write operations are embargoed to prevent data corruption
	ReadOnlySafe,
}

impl FsMode {
	/// Check if filesystem is in read-write mode
	pub fn is_read_write(&self) -> bool {
		matches!(self, FsMode::ReadWrite)
	}

	/// Check if filesystem is in read-only safe mode
	pub fn is_read_only(&self) -> bool {
		matches!(self, FsMode::ReadOnlySafe)
	}

	/// Enforce write embargo in read-only safe mode
	pub fn check_write_allowed(&self) -> crate::Result<()> {
		match self {
			FsMode::ReadWrite => Ok(()),
			FsMode::ReadOnlySafe => Err(crate::Error::Custom(
				"Write operations embargoed in read-only safe mode",
			)),
		}
	}
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_mode_read_write() -> TestResult<()> {
		// -- Setup & Fixtures
		let mode = FsMode::ReadWrite;

		// -- Check
		assert!(mode.is_read_write());
		assert!(!mode.is_read_only());
		assert!(mode.check_write_allowed().is_ok());

		Ok(())
	}

	#[test]
	fn test_mode_read_only_safe() -> TestResult<()> {
		// -- Setup & Fixtures
		let mode = FsMode::ReadOnlySafe;

		// -- Check
		assert!(!mode.is_read_write());
		assert!(mode.is_read_only());
		assert!(mode.check_write_allowed().is_err());

		Ok(())
	}

	#[test]
	fn test_mode_default() -> TestResult<()> {
		// -- Setup & Fixtures
		let mode = FsMode::default();

		// -- Check
		assert_eq!(mode, FsMode::ReadWrite);

		Ok(())
	}
}

// endregion: --- Tests
