//! The comparison harness: LedgerFS against SQLite, redb, and a raw fsync'd
//! file, on storage that can actually lose data.
//!
//! ```text
//! cargo run -p ledgerfs-benchmarks --release -- run [OPTIONS]
//! cargo run -p ledgerfs-benchmarks --release -- render [OPTIONS]
//! ```
//!
//! `run` measures, writes `results/<utc>-<host>.json`, and prints the tables.
//! `render` regenerates those tables inside `BENCHMARKS.md` from the newest
//! result file, so the document cannot drift from the harness.
//!
//! # The guard
//!
//! `run` refuses to start unless the target directory is on a filesystem whose
//! writes reach a device. This is not politeness: on tmpfs every `fsync`
//! returns immediately, so a durability benchmark there produces small,
//! plausible, meaningless numbers. See the check in `crate::config`.
//!
//! # Ordering
//!
//! One `(workload, engine)` pair at a time, each on a fresh location under
//! `--dir`, each deleted before the next. LedgerFS frees nothing, so a pair
//! that reused another pair's volume would run out of space rather than run
//! slowly — and a full sequential sweep would need several gigabytes resident
//! instead of one workload's worth.

mod config;
mod engine;
mod error;
mod medium;
mod report;
mod workload;

use std::io::Write;
use std::path::PathBuf;

use ledgerfs_host::BLOCK_SIZE;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::report::{Run, Sample};

const USAGE: &str = "\
ledgerfs-benchmarks — a comparison harness for durable writes

  run     [OPTIONS]    measure every engine and write a result file
  render  [OPTIONS]    regenerate the tables in BENCHMARKS.md from a result

run options
  --dir PATH           where the images and files live (default: benchmarks/data)
                       must be on a real filesystem; tmpfs is refused
  --results PATH       where the result JSON goes (default: benchmarks/results)
  --engine NAME        only this engine; repeatable (ledgerfs, sqlite-wal, redb, raw-file)
  --workload ID        only this workload; repeatable (see src/workload.rs)
  --insert-n N         inserts per repetition (default 200)
  --log-appends N      appends per repetition (default 64)
  --append-batches N   durable batches per repetition in the batched-append
                       workloads (default 512)
  --staged-seal-every N  batches per seal in staged_append_960b (default 10)
  --repeats N          repetitions per workload (default 3)
  --list               print the engines and workloads and exit

render options
  --results PATH       where the result JSONs are (default: benchmarks/results)
  --doc PATH           the document to update in place (default: BENCHMARKS.md)
  --stdout             print the tables instead of writing them";

fn main() {
	if let Err(e) = run() {
		eprintln!("ledgerfs-benchmarks: {e}");
		std::process::exit(1);
	}
}

fn run() -> Result<()> {
	let argv: Vec<String> = std::env::args().skip(1).collect();
	match argv.first().map(String::as_str) {
		None | Some("-h") | Some("--help") => {
			println!("{USAGE}");
			Ok(())
		}
		Some("run") => cmd_run(&argv[1..]),
		Some("render") => cmd_render(&argv[1..]),
		// `run --list` because that is where the option is documented, and a
		// bare `--list` because that is what a person types.
		Some("--list") | Some("-l") => {
			print_listing();
			Ok(())
		}
		Some(other) => Err(Error::usage(format!("unknown command '{other}'\n\n{USAGE}"))),
	}
}

/// Where the checked-in defaults live, so a bare `cargo run` does the obvious
/// thing from anywhere in the tree.
fn manifest_dir() -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// region:    --- run

struct RunArgs {
	dir: PathBuf,
	results: PathBuf,
	engines: Vec<String>,
	workloads: Vec<String>,
	cfg: Config,
}

fn parse_run_args(argv: &[String]) -> Result<RunArgs> {
	let mut args = RunArgs {
		dir: manifest_dir().join("data"),
		results: manifest_dir().join("results"),
		engines: Vec::new(),
		workloads: Vec::new(),
		cfg: Config::default(),
	};

	let mut it = argv.iter();
	while let Some(arg) = it.next() {
		let mut value = || -> Result<&String> { it.next().ok_or_else(|| Error::usage(format!("{arg} needs a value"))) };
		match arg.as_str() {
			"--dir" => args.dir = PathBuf::from(value()?),
			"--results" => args.results = PathBuf::from(value()?),
			"--engine" => args.engines.push(value()?.clone()),
			"--workload" => args.workloads.push(value()?.clone()),
			"--insert-n" => args.cfg.insert_n = parse_count(value()?, arg)?,
			"--log-appends" => args.cfg.log_appends = parse_count(value()?, arg)?,
			"--append-batches" => args.cfg.append_batches = parse_count(value()?, arg)?,
			"--staged-seal-every" => args.cfg.staged_seal_every = parse_count(value()?, arg)?,
			"--repeats" => args.cfg.repeats = parse_count(value()?, arg)?,
			"--list" => {
				print_listing();
				std::process::exit(0);
			}
			other => return Err(Error::usage(format!("unknown option '{other}'\n\n{USAGE}"))),
		}
	}

	if args.cfg.repeats == 0 {
		return Err(Error::usage("--repeats must be at least 1"));
	}
	if args.cfg.staged_seal_every == 0 {
		return Err(Error::usage("--staged-seal-every must be at least 1"));
	}
	// Everything an engine offers is named in the roster; a typo should stop the
	// run rather than silently measure three engines instead of four.
	for name in &args.engines {
		if engine::by_name(name).is_none() {
			return Err(Error::usage(format!("unknown engine '{name}' — try --list")));
		}
	}
	for id in &args.workloads {
		if workload::by_id(id).is_none() {
			return Err(Error::usage(format!("unknown workload '{id}' — try --list")));
		}
	}
	Ok(args)
}

fn parse_count(raw: &str, flag: &str) -> Result<usize> {
	raw.parse::<usize>()
		.map_err(|_| Error::usage(format!("{flag} wants a positive whole number, got '{raw}'")))
}

fn print_listing() {
	println!("engines:");
	for (name, _) in engine::ROSTER {
		println!("  {name}");
	}
	println!("workloads:");
	for w in workload::WORKLOADS {
		println!("  {}", w.id);
	}
}

fn cmd_run(argv: &[String]) -> Result<()> {
	let args = parse_run_args(argv)?;
	// Taken from the roster rather than from the arguments, so the names are
	// `'static` and a name that reached here is one that exists.
	let engine_names: Vec<&'static str> = engine::ROSTER
		.iter()
		.map(|(n, _)| *n)
		.filter(|n| args.engines.is_empty() || args.engines.iter().any(|e| e == n))
		.collect();
	let workloads: Vec<&workload::Workload> = if args.workloads.is_empty() {
		workload::WORKLOADS.iter().collect()
	} else {
		args.workloads.iter().filter_map(|id| workload::by_id(id)).collect()
	};

	// The guard comes first, before anything is created: a run that gets as far
	// as producing numbers on the wrong medium has already failed.
	let images = workloads.iter().map(|w| w.blocks).max().unwrap_or(0) * BLOCK_SIZE as u64 * 2;
	let medium = medium::require_durable(&args.dir, args.cfg.space_needed() + images)?;
	let mut medium = medium;
	// The record describes the tree, not the machine's home directory: the
	// path goes in relative to where the harness ran, when it can be — the
	// same anonymity rule the hostname and the device identifier follow.
	medium.path = recorded_path(std::path::Path::new(&medium.path));
	let machine = medium::machine();

	println!("{}", medium::describe(&machine, &medium));
	println!("workloads  {}", args.cfg.describe());
	println!("engines    {}\n", engine_names.join(", "));
	if !medium.barriers_are_real() {
		return Err(Error::usage("the guard let through a volatile medium"));
	}

	let (utc, _) = report::utc_stamps();
	let mut samples: Vec<Sample> = Vec::new();
	let total = engine_names.len() * workloads.len();
	let mut done = 0usize;

	for w in &workloads {
		for name in &engine_names {
			done += 1;
			let open = engine::by_name(name).expect("checked at parse time");
			// A directory per pair, wiped first: nothing an earlier pair wrote
			// can be mistaken for this pair's data or its free space.
			let home = args.dir.join(format!("{name}--{}", w.id));
			let _ = std::fs::remove_dir_all(&home);
			std::fs::create_dir_all(&home)?;

			print!("[{done:>2}/{total}] {name:<11} {:<20} ", w.id);
			std::io::stdout().flush()?;

			// A pair that fails becomes a recorded gap rather than the end of
			// the run: a harness that stops at the first refusal cannot say
			// that *this* engine cannot do *that* thing, which is one of the
			// more useful results it has to offer.
			let pair = run_pair(open, &home, w.blocks, w, &args.cfg);
			// Drop the engine before the directory goes, so an open handle
			// cannot keep a deleted image's blocks alive past the next pair.
			let _ = std::fs::remove_dir_all(&home);

			let sample = pair.finish(w.id, name);
			println!("{}", summarise(&sample));
			samples.push(sample);
		}
	}

	let run = Run {
		schema: report::SCHEMA,
		utc,
		machine,
		medium,
		config: args.cfg.clone(),
		samples,
	};
	let path = report::write_run(&args.results, &run)?;
	println!("\nwrote {}", path.display());
	print!("\n{}", report::render(&run));
	Ok(())
}

/// The path as the result file should carry it: relative to the current
/// directory when it sits under one, otherwise as given.
fn recorded_path(path: &std::path::Path) -> String {
	if let Ok(cwd) = std::env::current_dir()
		&& let Ok(rel) = path.strip_prefix(&cwd)
	{
		return rel.display().to_string();
	}
	path.display().to_string()
}

/// One pair's outcome, carrying the durability string even when it failed —
/// a gap row should still say what the engine was being asked for.
struct Pair {
	durability: String,
	sample: Result<Sample>,
}

impl Pair {
	fn finish(self, workload: &str, engine: &str) -> Sample {
		match self.sample {
			Ok(mut s) => {
				s.durability = self.durability;
				s
			}
			Err(e) => {
				let mut gap = report::gap(workload, engine, &e.to_string());
				gap.durability = self.durability;
				gap
			}
		}
	}
}

/// Open one engine on a fresh location and run one workload through it.
fn run_pair(open: engine::Open, home: &std::path::Path, blocks: u64, w: &workload::Workload, cfg: &Config) -> Pair {
	let mut eng = match open(home, blocks) {
		Ok(eng) => eng,
		Err(e) => {
			return Pair {
				durability: String::new(),
				sample: Err(e),
			};
		}
	};
	let durability = eng.durability().to_string();
	let sample = (w.run)(&mut *eng, cfg);
	// Dropped before the caller removes the directory, so no open handle keeps
	// a deleted image's blocks alive into the next pair.
	drop(eng);
	Pair { durability, sample }
}

/// The one-line progress entry for a finished sample.
fn summarise(sample: &Sample) -> String {
	if let Some(why) = &sample.missing {
		return format!("not measured ({why})");
	}
	let base = format!(
		"{} /op, {} ops/s",
		human_duration(sample.ns_per_op()),
		human_count(sample.ops_per_sec())
	);
	match &sample.note {
		Some(note) if !sample.comparable => format!("{base}  [{note}]"),
		_ => base,
	}
}

fn human_duration(ns: f64) -> String {
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

fn human_count(v: f64) -> String {
	if !v.is_finite() {
		return "—".to_string();
	}
	format!("{:.0}", v)
}

// endregion: --- run

// region:    --- render

fn cmd_render(argv: &[String]) -> Result<()> {
	let mut results = manifest_dir().join("results");
	let mut doc = manifest_dir().parent().expect("crate is in the tree").join("BENCHMARKS.md");
	let mut to_stdout = false;

	let mut it = argv.iter();
	while let Some(arg) = it.next() {
		match arg.as_str() {
			"--results" => results = PathBuf::from(it.next().ok_or_else(|| Error::usage("--results needs a path"))?),
			"--doc" => doc = PathBuf::from(it.next().ok_or_else(|| Error::usage("--doc needs a path"))?),
			"--stdout" => to_stdout = true,
			other => return Err(Error::usage(format!("unknown option '{other}'\n\n{USAGE}"))),
		}
	}

	let (path, run) = report::latest_run(&results)?;
	let rendered = report::render(&run);
	if to_stdout {
		print!("{rendered}");
		return Ok(());
	}

	let existing =
		std::fs::read_to_string(&doc).map_err(|e| Error::usage(format!("cannot read {}: {e}", doc.display())))?;
	let spliced = splice(&existing, &rendered).map_err(|e| Error::usage(format!("{}: {e}", doc.display())))?;
	std::fs::write(&doc, spliced)?;
	println!(
		"{}: tables regenerated from {}",
		doc.display(),
		path.file_name().unwrap_or_default().to_string_lossy()
	);
	Ok(())
}

/// Replace the region between the markers, keeping everything around it.
///
/// Deliberately strict about the markers: a document that has lost one is a
/// document whose tables would silently stop being regenerated, and that is a
/// worse failure than this one.
fn splice(doc: &str, rendered: &str) -> std::result::Result<String, String> {
	let begin = doc.find(report::BEGIN).ok_or_else(|| format!("missing '{}'", report::BEGIN))?;
	let after_begin = begin + report::BEGIN.len();
	let end = doc[after_begin..]
		.find(report::END)
		.ok_or_else(|| format!("missing '{}'", report::END))?
		+ after_begin;

	let mut out = String::with_capacity(doc.len() + rendered.len());
	out.push_str(&doc[..after_begin]);
	out.push('\n');
	out.push_str(rendered);
	out.push_str(&doc[end..]);
	Ok(out)
}

// endregion: --- render
