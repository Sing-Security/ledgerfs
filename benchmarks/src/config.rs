//! What one run was asked to do. Recorded in the result file, because a number
//! without its workload size is not a number anyone can compare against.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
	/// Inserts per repetition in the insert and read workloads.
	pub insert_n: usize,
	/// Appends per repetition in the log workload.
	pub log_appends: usize,
	/// Repetitions per workload. The reported figure is their median.
	pub repeats: usize,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			insert_n: 200,
			log_appends: 64,
			repeats: 3,
		}
	}
}

impl Config {
	pub fn describe(&self) -> String {
		format!(
			"insert_n={}, log_appends={}, repeats={}",
			self.insert_n, self.log_appends, self.repeats
		)
	}

	/// What one run needs free, not counting the LedgerFS images themselves —
	/// those are sized per workload and added by the caller.
	///
	/// Generous on purpose. Refusing a run that would have fit costs a retry;
	/// letting one fail halfway costs the run, and a table that stops partway
	/// through reads exactly like a table that finished.
	pub fn space_needed(&self) -> u64 {
		/// Rough bytes one stored 256-byte value costs an engine that keeps its
		/// own file — a row, a key, and the tree or page around them.
		const PER_KEY: u64 = 4096;

		let largest_workload = (1000 + self.insert_n as u64) * PER_KEY;
		let log = (self.log_appends * self.repeats) as u64 * 64 * 1024;
		// Four engines' own files, plus headroom for a table's growth during the
		// run and for whatever else the machine is doing.
		4 * (largest_workload + log) + 512 * 1024 * 1024
	}
}
