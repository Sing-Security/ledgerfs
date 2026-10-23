//! What one run was asked to do. Recorded in the result file, because a number
//! without its workload size is not a number anyone can compare against.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
	/// Inserts per repetition in the insert and read workloads.
	pub insert_n: usize,
	/// Appends per repetition in the log workload.
	pub log_appends: usize,
	/// Durable batches per repetition in the batched-append workloads.
	pub append_batches: usize,
	/// Batches per seal in the staged-append workload: the durability point is
	/// every `staged_seal_every`th batch, not every batch.
	pub staged_seal_every: usize,
	/// Repetitions per workload. The reported figure is their median.
	pub repeats: usize,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			insert_n: 200,
			log_appends: 64,
			append_batches: 512,
			staged_seal_every: 10,
			repeats: 3,
		}
	}
}

impl Config {
	pub fn describe(&self) -> String {
		format!(
			"insert_n={}, log_appends={}, append_batches={}, staged_seal_every={}, repeats={}",
			self.insert_n, self.log_appends, self.append_batches, self.staged_seal_every, self.repeats
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

		/// Rough bytes one durable batch costs LedgerFS's free space — the commit
		/// overhead, not the 960-byte payload. Measured, not modelled: it is the
		/// same order as the `volume KiB/op` the harness reports, with headroom
		/// for a volume whose allocator snapshot is larger than the last run's.
		const PER_BATCH: u64 = 320 * 1024;

		let largest_workload = (1000 + self.insert_n as u64) * PER_KEY;
		let log = (self.log_appends * self.repeats) as u64 * 64 * 1024;
		let batched = (self.append_batches * self.repeats) as u64 * PER_BATCH;
		// Four engines' own files, plus headroom for a table's growth during the
		// run and for whatever else the machine is doing.
		4 * (largest_workload + log) + 2 * batched + 512 * 1024 * 1024
	}
}
