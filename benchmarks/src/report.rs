//! One run, the file it is written to, and the tables rendered back out of it.
//!
//! # Why the document is generated
//!
//! `BENCHMARKS.md` is not written by hand. Its tables are rendered from the
//! JSON file the harness wrote, so a number in the document is a number some
//! run produced, and changing the harness without re-running it leaves the
//! document visibly stale rather than quietly wrong. The prose around the
//! tables is hand-written and stays where it is; the renderer only replaces
//! what sits between its markers.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::medium::{Machine, Medium};

/// Bumped when the shape below changes in a way an older file cannot answer.
pub const SCHEMA: u32 = 1;

pub const BEGIN: &str = "<!-- BEGIN GENERATED: benchmarks -->";
pub const END: &str = "<!-- END GENERATED: benchmarks -->";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
	pub schema: u32,
	/// UTC, ISO 8601, to the second.
	pub utc: String,
	pub machine: Machine,
	pub medium: Medium,
	pub config: Config,
	pub samples: Vec<Sample>,
}

/// One workload on one engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
	pub workload: String,
	pub engine: String,
	/// The engine's own statement of what it was told to guarantee.
	pub durability: String,
	pub repeats: usize,
	pub ops_per_repeat: u64,
	pub bytes_per_repeat: u64,
	/// Median of the per-repetition means, in nanoseconds per operation.
	/// NaN when the workload was not expressible — see `missing`.
	#[serde(with = "nan_as_null")]
	pub ns_per_op: f64,
	/// False when this row is not a like-for-like comparison with its
	/// neighbours. The reason is in `note`.
	pub comparable: bool,
	pub note: Option<String>,
	/// Why the engine could not run this at all. A gap, not a zero.
	pub missing: Option<String>,
	/// Volume bytes the measured operations consumed, when the engine can say.
	///
	/// `None` means the engine reports no free-space figure, not that the
	/// workload was free. Only writing workloads fill it in: for a read or a
	/// scan the population happens outside the timed section, so a delta across
	/// the timed region would measure nothing.
	#[serde(default)]
	pub space_per_op: Option<u64>,
}

/// JSON has no NaN, and a gap's `ns_per_op` is NaN.
///
/// The mapping is deliberately to `null` and not to `0`: a zero is a
/// measurement, and a result file where "not measured" and "measured as
/// instant" are the same bytes would be a trap for whatever reads it next.
/// Writing `null` and reading it back as NaN keeps the file a faithful
/// round-trip of the run.
mod nan_as_null {
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
		if v.is_finite() {
			v.serialize(s)
		} else {
			s.serialize_none()
		}
	}

	pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
		Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
	}
}

/// A workload an engine was asked for and could not deliver.
///
/// Used for both kinds of gap: a capability the engine does not have, and a run
/// that started and failed. Both are the absence of a number, and the reason
/// string is what distinguishes them.
pub fn gap(workload: &str, engine: &str, why: &str) -> Sample {
	Sample {
		workload: workload.to_string(),
		engine: engine.to_string(),
		durability: String::new(),
		repeats: 0,
		ops_per_repeat: 0,
		bytes_per_repeat: 0,
		ns_per_op: f64::NAN,
		comparable: false,
		note: None,
		missing: Some(why.to_string()),
		space_per_op: None,
	}
}

impl Sample {
	pub fn ns_per_op(&self) -> f64 {
		self.ns_per_op
	}

	pub fn ops_per_sec(&self) -> f64 {
		1e9 / self.ns_per_op
	}

	/// Payload bytes per second, or `None` for a workload that moves no payload
	/// (a reopen, say) where the figure would be meaningless rather than zero.
	pub fn bytes_per_sec(&self) -> Option<f64> {
		if self.bytes_per_repeat == 0 {
			return None;
		}
		let per_op_bytes = self.bytes_per_repeat as f64 / self.ops_per_repeat as f64;
		Some(per_op_bytes * self.ops_per_sec())
	}
}

// region:    --- Time

/// UTC now, as `(filename stamp, human stamp)`.
///
/// Hand-rolled rather than pulled in: the harness already reads `/proc` and
/// `/sys` and is Linux-only, so one more C library call would be no burden —
/// but a date *formatting* dependency for two strings is not worth a line in
/// the dependency list of a crate whose whole argument is that its numbers can
/// be reproduced.
pub fn utc_stamps() -> (String, String) {
	let secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);
	let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
	let rem = secs.rem_euclid(86_400);
	let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
	(
		format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"),
		format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z"),
	)
}

/// Days since 1970-01-01 to `(year, month, day)`, UTC.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole `i64` range
/// and needs no table.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
	let z = z + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
	let doe = (z - era * 146_097) as u64; // [0, 146096]
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
	let y = yoe as i64 + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
	let mp = (5 * doy + 2) / 153; // [0, 11]
	let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
	let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	(if m <= 2 { y + 1 } else { y }, m, d)
}

// endregion: --- Time

// region:    --- Files

/// Write `run` to `<dir>/<utc>-<host>.json` and return the path.
pub fn write_run(dir: &Path, run: &Run) -> Result<std::path::PathBuf> {
	std::fs::create_dir_all(dir)?;
	let (_, stamp) = utc_stamps();
	let host = run.machine.hostname.clone().unwrap_or_else(|| "unknown".into());
	let path = dir.join(format!("{stamp}-{host}.json"));
	let text = serde_json::to_string_pretty(run).map_err(|e| Error::usage(format!("cannot encode results: {e}")))?;
	std::fs::write(&path, text)?;
	Ok(path)
}

/// The newest result file in `dir`.
///
/// Ordered by name rather than by mtime: the name starts with a UTC timestamp
/// in a fixed-width, lexically ordered format, so this is the same answer and
/// does not depend on a clock that can be set backwards.
pub fn latest_run(dir: &Path) -> Result<(std::path::PathBuf, Run)> {
	let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
		.filter_map(|e| e.ok().map(|e| e.path()))
		.filter(|p| p.extension().is_some_and(|x| x == "json"))
		.collect();
	names.sort();
	let path = names
		.pop()
		.ok_or_else(|| Error::usage(format!("no results in {} — run the harness first", dir.display())))?;

	let text = std::fs::read_to_string(&path)?;
	let run: Run = serde_json::from_str(&text)
		.map_err(|e| Error::usage(format!("{} is not a result file: {e}", path.display())))?;
	if run.schema != SCHEMA {
		return Err(Error::usage(format!(
			"{} is schema {} and this build reads {SCHEMA}",
			path.display(),
			run.schema
		)));
	}
	Ok((path, run))
}

// endregion: --- Files

// region:    --- Rendering

/// The tables, as markdown, with the run's provenance stated first.
pub fn render(run: &Run) -> String {
	let mut out = String::new();

	out.push_str("<!-- This section is generated by `ledgerfs-benchmarks render`.\n");
	out.push_str("     Edit the prose around it, not the tables: they are overwritten. -->\n\n");
	out.push_str("## The machine this was measured on\n\n");
	out.push_str(&format!("- **UTC** — {}\n", run.utc));
	if let Some(cpu) = &run.machine.cpu {
		let cores = run.machine.cores.as_deref().unwrap_or("?");
		out.push_str(&format!("- **CPU** — {cpu} ({cores} logical)\n"));
	}
	if let Some(mem) = run.machine.memory_bytes {
		out.push_str(&format!(
			"- **Memory** — {:.1} GiB\n",
			mem as f64 / (1024.0 * 1024.0 * 1024.0)
		));
	}
	if let Some(k) = &run.machine.kernel {
		out.push_str(&format!("- **Kernel** — {k}\n"));
	}
	out.push_str(&format!("- **Filesystem** — {}\n", run.medium.fs_name));
	if let Some(src) = &run.medium.source {
		out.push_str(&format!("- **Device** — {src}"));
		if let Some(model) = &run.medium.device_model {
			out.push_str(&format!(" ({model})"));
		}
		if run.medium.encrypted {
			out.push_str(" — **dm-crypt, so an encryption cost is included**");
		}
		out.push('\n');
	}
	out.push_str(&format!(
		"- **Free space** — {:.1} GiB of {:.1} GiB\n",
		run.medium.free_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
		run.medium.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
	));
	out.push_str(&format!("- **Workload sizes** — {}\n\n", run.config.describe()));
	out.push_str(
		"Each workload is run for every engine `repeats` times; the figure below is the median of \
		 the per-repetition means, and each repetition does the same amount of work. Nothing is \
		 tuned. Every engine is asked for one write, durable before the call returns, except where \
		 a row is marked \u{2020}: those state their own durability point instead.\n\n",
	);

	// The durability statements belong before the numbers, not after them: a
	// fast number from an engine that was allowed to skip its fsync is not a
	// number, it is a different experiment.
	out.push_str("### What each engine was told\n\n");
	out.push_str("| engine | durability setting |\n|---|---|\n");
	let mut engines: Vec<String> = run.samples.iter().map(|s| s.engine.clone()).collect();
	engines.sort();
	engines.dedup();
	for engine in &engines {
		let durability = run
			.samples
			.iter()
			.find(|s| &s.engine == engine)
			.map(|s| s.durability.as_str())
			.unwrap_or("");
		out.push_str(&format!("| `{engine}` | {durability} |\n"));
	}
	out.push('\n');

	let workloads = workload_order(run);
	for workload in &workloads {
		render_workload(&mut out, run, workload);
	}
	out
}

/// Workloads in the order a reader should meet them, not the order the engines
/// happened to finish in.
fn workload_order(run: &Run) -> Vec<String> {
	let mut present: Vec<String> = run.samples.iter().map(|s| s.workload.clone()).collect();
	present.sort();
	present.dedup();
	// The harness's own order is the intended reading order; anything the
	// roster does not know about goes at the end alphabetically.
	let known = [
		"insert_durable_256b",
		"insert_batched_256b",
		"read_point_256b",
		"append_log_64k",
		"append_batched_960b",
		"staged_append_960b",
		"deferred_append_960b",
		"read_back_960b",
		"scan_10",
		"scan_100",
		"scan_1000",
		"reopen_and_read",
	];
	let mut ordered: Vec<String> = known
		.iter()
		.filter(|k| present.iter().any(|p| p == *k))
		.map(|k| (*k).to_string())
		.collect();
	for p in present {
		if !ordered.contains(&p) {
			ordered.push(p);
		}
	}
	ordered
}

fn render_workload(out: &mut String, run: &Run, workload: &str) {
	let rows: Vec<&Sample> = run.samples.iter().filter(|s| s.workload == workload).collect();
	let Some(first) = rows.first() else {
		return;
	};

	out.push_str(&format!("### `{workload}`\n\n"));
	if let Some(note) = &first.note {
		out.push_str(&format!("{note}.\n\n"));
	}

	let mut measured: Vec<&Sample> = rows
		.iter()
		.copied()
		.filter(|s| s.missing.is_none() && s.ns_per_op.is_finite())
		.collect();
	measured.sort_by(|a, b| a.ns_per_op.partial_cmp(&b.ns_per_op).expect("finite"));

	if measured.is_empty() {
		out.push_str("*No engine could run this workload.*\n\n");
	} else {
		let show_bytes = measured.iter().any(|s| s.bytes_per_sec().is_some());
		let show_space = measured.iter().any(|s| s.space_per_op.is_some());
		out.push_str("| engine | median per operation | operations/s |");
		if show_bytes {
			out.push_str(" payload MiB/s |");
		}
		if show_space {
			out.push_str(" volume KiB/op |");
		}
		out.push_str("\n|---|---:|---:|");
		if show_bytes {
			out.push_str("---:|");
		}
		if show_space {
			out.push_str("---:|");
		}
		out.push('\n');
		for s in &measured {
			let label = if s.comparable {
				format!("`{}`", s.engine)
			} else {
				format!("`{}` \u{2020}", s.engine)
			};
			out.push_str(&format!(
				"| {label} | {} | {} |",
				duration(s.ns_per_op),
				count(s.ops_per_sec())
			));
			if show_bytes {
				match s.bytes_per_sec() {
					Some(b) => out.push_str(&format!(" {} |", mib(b))),
					None => out.push_str(" — |"),
				}
			}
			if show_space {
				match s.space_per_op {
					Some(b) => out.push_str(&format!(" {:.1} |", b as f64 / 1024.0)),
					None => out.push_str(" — |"),
				}
			}
			out.push('\n');
		}
		out.push('\n');
		if show_space {
			out.push_str(
				"`volume KiB/op` is the backing store's own free-space delta divided by the \
				 operations the timed section performed. An engine that reports no free-space \
				 figure shows `—`; a `—` is not a zero.\n\n",
			);
		}

		// Grouped by reason, not listed per engine: every engine in a tier row
		// carries the same sentence, and four copies of it push the one fact a
		// reader needs out of view.
		let slanted: Vec<&&Sample> = measured.iter().filter(|s| !s.comparable).collect();
		if !slanted.is_empty() {
			out.push_str("\u{2020} not a like-for-like row:\n");
			let mut shown: Vec<&str> = Vec::new();
			for note in slanted.iter().filter_map(|s| s.note.as_deref()) {
				if shown.contains(&note) {
					continue;
				}
				shown.push(note);
				let names: Vec<String> = slanted
					.iter()
					.filter(|s| s.note.as_deref() == Some(note))
					.map(|s| format!("`{}`", s.engine))
					.collect();
				out.push_str(&format!("- {} — {note}.\n", names.join(", ")));
			}
			out.push('\n');
		}
	}

	let gaps: Vec<&&Sample> = rows.iter().filter(|s| s.missing.is_some()).collect();
	if !gaps.is_empty() {
		out.push_str("Not measured:\n");
		for s in gaps {
			out.push_str(&format!(
				"- `{}` — {}\n",
				s.engine,
				s.missing.as_deref().unwrap_or("no reason recorded")
			));
		}
		out.push('\n');
	}
}

fn duration(ns: f64) -> String {
	if ns < 1_000.0 {
		format!("{ns:.0} ns")
	} else if ns < 1_000_000.0 {
		format!("{:.1} µs", ns / 1_000.0)
	} else if ns < 1_000_000_000.0 {
		format!("{:.2} ms", ns / 1_000_000.0)
	} else {
		format!("{:.2} s", ns / 1_000_000_000.0)
	}
}

/// Thousands-separated, because a column of eight-digit numbers is unreadable.
fn count(v: f64) -> String {
	if !v.is_finite() {
		return "—".to_string();
	}
	let n = v.round() as u64;
	let digits = n.to_string();
	let mut out = String::with_capacity(digits.len() + digits.len() / 3);
	for (i, c) in digits.chars().enumerate() {
		if i > 0 && (digits.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(c);
	}
	out
}

fn mib(bytes_per_sec: f64) -> String {
	format!("{:.2}", bytes_per_sec / (1024.0 * 1024.0))
}

// endregion: --- Rendering
