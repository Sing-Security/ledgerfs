//! One error type for the harness.
//!
//! Three things go wrong here and they want different treatment: an engine
//! refuses a workload it cannot express, the environment fails, or the caller
//! asked for something that does not exist. Only the middle one is a failed
//! run; the first is a *result*, recorded as a gap rather than as a zero.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
	/// The environment failed: an image could not be created, a directory could
	/// not be read. A run that hits this has no numbers.
	Io(std::io::Error),

	/// An engine failed on a workload. Carries the engine's own message, which
	/// is usually the only thing that says what happened.
	Engine { engine: &'static str, detail: String },

	/// The caller asked for something the harness does not do.
	Usage(String),
}

impl Error {
	pub fn engine(engine: &'static str, detail: impl Into<String>) -> Self {
		Error::Engine {
			engine,
			detail: detail.into(),
		}
	}

	pub fn usage(detail: impl Into<String>) -> Self {
		Error::Usage(detail.into())
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Error::Io(e) => write!(f, "{e}"),
			Error::Engine { engine, detail } => write!(f, "{engine}: {detail}"),
			Error::Usage(m) => write!(f, "{m}"),
		}
	}
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		Error::Io(e)
	}
}

impl From<Error> for std::io::Error {
	fn from(e: Error) -> Self {
		match e {
			Error::Io(io) => io,
			other => std::io::Error::other(other.to_string()),
		}
	}
}
