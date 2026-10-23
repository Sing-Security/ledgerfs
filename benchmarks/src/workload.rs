//! The workloads, and the discipline that makes them comparable.
//!
//! # Like for like
//!
//! Every engine is asked for the same thing and told the same thing: one write,
//! durable before the call returns. Nothing is tuned differently between them
//! and no engine gets a warmer cache than another — each workload gets a fresh
//! engine on a fresh location, populates it outside the timed section, and is
//! then timed over the same number of operations.
//!
//! # Repetition
//!
//! A single timing is a rumour. Each workload runs `repeats` times and the
//! reported number is the **median** of the per-repetition means, so one
//! scheduler hiccup does not become the result. Each repetition does the same
//! amount of work; where a workload writes, the keys differ between repetitions
//! so nothing is overwritten (LedgerFS frees nothing, and an overwrite would
//! make the repetitions unequal for a different reason).
//!
//! # The non-comparable rows
//!
//! Two workloads are deliberately not like-for-like and are marked so
//! (`comparable = false`, a dagger in the tables, the reason in `note`):
//! `insert_batched_256b`, whose per-insert figure is 1/N of one commit, and
//! `staged_append_960b`, whose durability point is every Nth batch. Both apply
//! the same relaxation to every engine, so the engines stay comparable to each
//! other; what does not hold is the comparison to the one-durability-point-per-
//! operation rows.
//!
//! # Gaps
//!
//! An engine that cannot express a workload is recorded with `missing` set, not
//! with a zero. One row is expected to be a gap somewhere — reopen for the
//! raw-file control, because a directory tree has no recovery step — as is
//! anything on a platform where an engine will not open.

use std::time::Instant;

use crate::config::Config;
use crate::engine::{Engine, KEYS_PER_DIR, Op, shard_dirs, shard_key};
use crate::error::{Error, Result};
use crate::report::Sample;

/// Bytes in a value for the insert workloads. Small enough to be dominated by
/// the commit rather than by the data: 256 bytes is one block on the volume and
/// one page on the filesystem.
pub const VALUE_BYTES: usize = 256;

/// Bytes per append in the log workload — large enough that the per-append cost
/// has a data component as well as a commit component, which is the honest
/// shape of an append-only log.
pub const LOG_CHUNK: usize = 64 * 1024;

/// Bytes in one record for the batched-append workloads. Small enough that the
/// commit, not the payload, is what the number measures.
pub const RECORD_BYTES: usize = 48;

/// Records per durable batch in the batched-append workloads — the unit an
/// application that accumulates records and flushes them actually pays for.
pub const ROWS_PER_BATCH: usize = 20;

/// Payload bytes of one batch.
pub const BATCH_BYTES: usize = RECORD_BYTES * ROWS_PER_BATCH;

/// What one workload is, and the space it needs on a volume it will grow.
pub struct Workload {
	pub id: &'static str,
	/// Volume blocks to format for engines that need to be told (LedgerFS).
	///
	/// Sized for what the workload *writes*, which is much larger than its
	/// payload: LedgerFS frees nothing (`FORMAT.md` §22) and every commit
	/// rewrites the directory node that contains its entry. A volume sized to
	/// the payload alone fails partway with an out-of-space error and no number,
	/// which is how these sizes were arrived at. They were derived when a `put`
	/// was two commits and are left alone: a `put` is now one, so a volume here
	/// is generous rather than tight, and shrinking it would change what the
	/// checked-in run measured.
	pub blocks: u64,
	pub run: fn(&mut dyn Engine, &Config) -> Result<Sample>,
}

/// The roster, in report order.
pub const WORKLOADS: &[Workload] = &[
	Workload {
		id: "insert_durable_256b",
		blocks: 262144,
		run: insert_durable,
	},
	Workload {
		id: "insert_batched_256b",
		blocks: 262144,
		run: insert_batched,
	},
	Workload {
		id: "read_point_256b",
		blocks: 131072,
		run: read_point,
	},
	Workload {
		id: "append_log_64k",
		blocks: 65536,
		run: append_log,
	},
	Workload {
		id: "append_batched_960b",
		blocks: 131072,
		run: append_batched,
	},
	Workload {
		id: "staged_append_960b",
		blocks: 131072,
		run: append_staged,
	},
	Workload {
		id: "deferred_append_960b",
		blocks: 131072,
		run: append_deferred,
	},
	Workload {
		id: "read_back_960b",
		blocks: 65536,
		run: read_back,
	},
	Workload {
		id: "scan_10",
		blocks: 8192,
		run: |engine, cfg| scan(engine, cfg, 10),
	},
	Workload {
		id: "scan_100",
		blocks: 16384,
		run: |engine, cfg| scan(engine, cfg, 100),
	},
	Workload {
		id: "scan_1000",
		blocks: 131072,
		run: |engine, cfg| scan(engine, cfg, 1000),
	},
	Workload {
		id: "reopen_and_read",
		blocks: 131072,
		run: reopen_and_read,
	},
];

pub fn by_id(id: &str) -> Option<&'static Workload> {
	WORKLOADS.iter().find(|w| w.id == id)
}

// region:    --- Timing

/// Run `body` `repeats` times and report the median of the per-repetition
/// per-operation times.
///
/// `ops` and `bytes` are what one repetition performs and must be the same for
/// every repetition — the caller guarantees that by construction, which is why
/// a writing workload varies its keys per repetition rather than overwriting.
/// The repetition index is handed to `body` for exactly that.
fn measure(
	engine_name: &'static str,
	workload: &'static str,
	ops: u64,
	bytes: u64,
	repeats: usize,
	mut body: impl FnMut(usize) -> Result<()>,
) -> Result<Sample> {
	let mut per_op = Vec::with_capacity(repeats);
	for r in 0..repeats {
		let started = Instant::now();
		body(r)?;
		let elapsed = started.elapsed().as_secs_f64();
		per_op.push(elapsed / ops as f64);
	}
	per_op.sort_by(|a, b| a.partial_cmp(b).expect("durations are never NaN"));

	let median = per_op[per_op.len() / 2];
	Ok(Sample {
		workload: workload.to_string(),
		engine: engine_name.to_string(),
		durability: String::new(), // filled in by the runner from the engine
		repeats,
		ops_per_repeat: ops,
		bytes_per_repeat: bytes,
		ns_per_op: median * 1e9,
		comparable: true,
		note: None,
		missing: None,
		space_per_op: None,
	})
}

/// The record for a workload an engine cannot express.
///
/// `ns_per_op` is NaN rather than zero: a zero is a measurement, and this is
/// the absence of one.
fn unsupported(engine: &'static str, workload: &'static str, why: &str) -> Sample {
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

/// A deterministic 256-byte value. Content matters: an all-zero buffer would be
/// compressed, deduplicated, or written sparsely by some engine and not others.
fn value(tag: &str, i: usize) -> Vec<u8> {
	let mut out = Vec::with_capacity(VALUE_BYTES);
	// FNV-1a's 64-bit prime, as the odd multiplier that keeps distinct `i`
	// apart before the mixing rounds below.
	let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (i as u64).wrapping_mul(0x0000_0100_0000_01B3);
	for byte in tag.bytes() {
		x = splitmix(x ^ byte as u64);
	}
	while out.len() < VALUE_BYTES {
		x = splitmix(x);
		out.extend_from_slice(&x.to_le_bytes());
	}
	out.truncate(VALUE_BYTES);
	out
}

fn splitmix(mut x: u64) -> u64 {
	x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
	x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
	x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
	x ^ (x >> 31)
}

/// One fixed-size record: an 8-byte little-endian sequence number, then payload
/// bytes. Deterministic in `(stream, row)` for the same reason [`value`] is —
/// an all-zero or repeating buffer invites compression, deduplication, or
/// sparsity tricks that some engine does and others don't.
fn record(stream: &str, row: usize) -> [u8; RECORD_BYTES] {
	let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (row as u64).wrapping_mul(0x0000_0100_0000_01B3);
	for byte in stream.bytes() {
		x = splitmix(x ^ byte as u64);
	}
	let mut out = [0u8; RECORD_BYTES];
	out[0..8].copy_from_slice(&(row as u64).to_le_bytes());
	for chunk in out.as_chunks_mut::<8>().0 {
		x = splitmix(x);
		*chunk = x.to_le_bytes();
	}
	out
}

/// Record what the measured operations cost in backing-store space.
///
/// Called *after* the timed section, from the free-space figure the engine
/// reports before and after it. A free-space delta rather than a file size
/// because the engines do not share a file layout: SQLite's database and
/// LedgerFS's image are both one file, but what grew inside it is not
/// comparable, whereas "bytes the medium no longer has" is.
///
/// The reading is taken once before and once after the **whole** repeat run, so
/// the delta spans every repetition; it is divided by `ops × repeats`, the work
/// the run actually did. Dividing by the per-repetition `ops` alone multiplies
/// every figure by the repeat count — which is what this harness did until
/// 2026-10-23, so every `volume KiB/op` recorded before that date is inflated
/// by `Config::repeats` (3). The 2026-10-23 run is the correction.
///
/// Left `None` when the engine reports no figure, or when the delta is not
/// positive — a workload that freed space, or one whose engine cannot say, is
/// reported as not measured rather than as zero.
fn record_space(sample: &mut Sample, engine: &dyn Engine, before: Option<u64>, ops: u64) {
	let (Some(before), Some(after)) = (before, engine.space_used()) else {
		return;
	};
	let run_ops = ops.saturating_mul(sample.repeats as u64);
	if before > after && run_ops > 0 {
		sample.space_per_op = Some((before - after) / run_ops);
	}
}

/// One bucket-name prefix per repetition, so a repetition's keys are its own
/// and nothing is overwritten (an overwrite would make the repetitions unequal).
fn tags_for(base: &str, repeats: usize) -> Vec<String> {
	(0..repeats).map(|r| format!("{base}{r}")).collect()
}

/// Hand the engine every directory the workload's keys will land in, before
/// anything is timed.
fn prepare_all(engine: &mut dyn Engine, tags: &[String], n: usize) -> Result<()> {
	for tag in tags {
		engine.prepare(&shard_dirs(tag, n))?;
	}
	Ok(())
}

// endregion: --- Timing

// region:    --- The workloads

/// One durable insert per operation — the headline number.
fn insert_durable(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	let n = cfg.insert_n;
	let tags = tags_for("i", cfg.repeats);
	prepare_all(engine, &tags, n)?;
	let before = engine.space_used();
	let mut sample = measure(
		name,
		"insert_durable_256b",
		n as u64,
		(n * VALUE_BYTES) as u64,
		cfg.repeats,
		|r| {
			for i in 0..n {
				engine.put(&shard_key(&tags[r], i), &value("kv", i))?;
			}
			Ok(())
		},
	)?;
	record_space(&mut sample, engine, before, n as u64);
	Ok(sample)
}

/// The same inserts, all in one transaction.
///
/// Not comparable with the row above and marked so: the per-insert number this
/// produces is the cost of *1/N* of a commit. It is here because the gap between
/// the two rows is the whole argument for batching — and because LedgerFS now
/// produces it too, through a staging session sealed once, which is the same
/// transaction SQLite's `BEGIN … COMMIT` is.
fn insert_batched(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	if !engine.supports(Op::Batch) {
		return Ok(unsupported(
			name,
			"insert_batched_256b",
			"no caller-visible batch transaction; every put is its own commit",
		));
	}
	let n = cfg.insert_n;
	let tags = tags_for("b", cfg.repeats);
	prepare_all(engine, &tags, n)?;
	let before = engine.space_used();
	let mut sample = measure(
		name,
		"insert_batched_256b",
		n as u64,
		(n * VALUE_BYTES) as u64,
		cfg.repeats,
		|r| {
			let items: Vec<(String, Vec<u8>)> = (0..n).map(|i| (shard_key(&tags[r], i), value("kb", i))).collect();
			engine.put_batch(&items)
		},
	)?;
	sample.comparable = false;
	sample.note = Some(format!(
		"{n} inserts in one transaction; the per-insert figure is 1/{n} of a commit"
	));
	record_space(&mut sample, engine, before, n as u64);
	Ok(sample)
}

/// Point reads over a populated store.
///
/// The population is outside the timed section on purpose: this measures the
/// read path, and mixing in the write cost of building the data would make the
/// two engines' different write costs show up here as well.
fn read_point(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	let n = cfg.insert_n;
	let tag = "r";
	prepare_all(engine, std::slice::from_ref(&tag.to_string()), n)?;
	for i in 0..n {
		engine.put(&shard_key(tag, i), &value("kr", i))?;
	}
	let mut sample = measure(
		name,
		"read_point_256b",
		n as u64,
		(n * VALUE_BYTES) as u64,
		cfg.repeats,
		|_| {
			for i in 0..n {
				let got = engine.get(&shard_key(tag, i))?;
				if got.len() != VALUE_BYTES {
					return Err(Error::engine(
						name,
						format!("read {} bytes, wrote {VALUE_BYTES}", got.len()),
					));
				}
			}
			Ok(())
		},
	)?;
	sample.note = Some(format!("{n} reads over a store populated outside the timed section"));
	Ok(sample)
}

/// Append-only log: one durable append of `LOG_CHUNK` per operation.
fn append_log(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	if !engine.supports(Op::Append) {
		return Ok(unsupported(name, "append_log_64k", "no append path"));
	}
	let m = cfg.log_appends;
	let chunk: Vec<u8> = value("log", 7).repeat(LOG_CHUNK / VALUE_BYTES);
	let before = engine.space_used();
	let mut sample = measure(
		name,
		"append_log_64k",
		m as u64,
		(m * LOG_CHUNK) as u64,
		cfg.repeats,
		|_| {
			for _ in 0..m {
				engine.append("default", &chunk)?;
			}
			Ok(())
		},
	)?;
	sample.note = Some(format!("{m} appends of {} KiB, one fsync each", LOG_CHUNK / 1024));
	record_space(&mut sample, engine, before, m as u64);
	Ok(sample)
}

/// Batched appends: one durable batch of fixed-size records per operation.
///
/// `ops` counts batches, not records — the batch is the durability point, so
/// per-batch is the number an application that flushes this way actually pays.
/// Record generation happens outside the timed section: this measures the
/// storage call, not the producer. Each repetition appends to its own stream,
/// the same fresh-keys rule the insert workloads follow.
fn append_batched(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	let batches = cfg.append_batches;
	let streams: Vec<String> = (0..cfg.repeats).map(|r| format!("s{r}")).collect();
	let mut per_rep: Vec<Vec<Vec<u8>>> = Vec::with_capacity(cfg.repeats);
	for stream in &streams {
		let rep: Vec<Vec<u8>> = (0..batches)
			.map(|b| {
				(0..ROWS_PER_BATCH)
					.flat_map(|j| record(stream, b * ROWS_PER_BATCH + j))
					.collect()
			})
			.collect();
		per_rep.push(rep);
	}
	let before = engine.space_used();
	let mut sample = measure(
		name,
		"append_batched_960b",
		batches as u64,
		(batches * BATCH_BYTES) as u64,
		cfg.repeats,
		|r| {
			for batch in &per_rep[r] {
				let rows: Vec<&[u8]> = batch.chunks(RECORD_BYTES).collect();
				engine.append_batch(&streams[r], &rows)?;
			}
			Ok(())
		},
	)?;
	sample.note = Some(format!(
		"{batches} durable batches of {ROWS_PER_BATCH} × {RECORD_BYTES} B records; the batch is the durability point"
	));
	record_space(&mut sample, engine, before, batches as u64);
	Ok(sample)
}

/// The same records as [`append_batched`], with the durability point moved off
/// the batch: rows accumulate in each engine's staged tier and one seal makes
/// `staged_seal_every` batches durable at once.
///
/// This is the row for an application that has decided a commit per batch is
/// more durability than it needs — the promise is "a seal's worth survives, an
/// unsealed tail need not". It is marked non-comparable because it is not: the
/// same work is being made durable an Nth as often. Every engine gets the same
/// relaxation, in its own idiom, so the comparison between them stays honest
/// even though the comparison to the rows above does not.
///
/// `ops` counts batches, as in the row above, so the two figures are per-batch
/// costs of the same payload; what differs is how many of those batches share a
/// durability point.
fn append_staged(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	if !engine.supports(Op::Staged) {
		return Ok(unsupported(name, "staged_append_960b", "no staging tier"));
	}
	let batches = cfg.append_batches;
	let seal_every = cfg.staged_seal_every.max(1);
	let streams: Vec<String> = (0..cfg.repeats).map(|r| format!("t{r}")).collect();
	let per_rep = staged_fixtures(cfg, &streams, batches);
	let before = engine.space_used();
	let mut sample = measure(
		name,
		"staged_append_960b",
		batches as u64,
		(batches * BATCH_BYTES) as u64,
		cfg.repeats,
		|r| {
			let mut since_seal = 0;
			for batch in &per_rep[r] {
				let rows: Vec<&[u8]> = batch.chunks(RECORD_BYTES).collect();
				engine.staged_append(&streams[r], &rows)?;
				since_seal += 1;
				if since_seal == seal_every {
					engine.seal_staged()?;
					since_seal = 0;
				}
			}
			// A tail shorter than one seal's worth is sealed too: the workload
			// is finished, so leaving it staged would be data the engine
			// promises to nobody.
			if since_seal > 0 {
				engine.seal_staged()?;
			}
			Ok(())
		},
	)?;
	sample.comparable = false;
	sample.note = Some(format!(
		"{batches} batches of {ROWS_PER_BATCH} × {RECORD_BYTES} B records, sealed every {seal_every}; \
		 the durability point is the seal, so the per-batch figure is 1/{seal_every} of a commit"
	));
	record_space(&mut sample, engine, before, batches as u64);
	Ok(sample)
}

/// The payload of every repetition, generated before anything is timed.
///
/// One repetition's stream is its own log (`t0`, `t1`, …), so a repetition
/// starts on an empty log without the previous one's bytes in it, and the
/// per-repetition operation counts are identical by construction.
fn staged_fixtures(cfg: &Config, streams: &[String], batches: usize) -> Vec<Vec<Vec<u8>>> {
	let mut per_rep: Vec<Vec<Vec<u8>>> = Vec::with_capacity(cfg.repeats);
	for stream in streams {
		let rep: Vec<Vec<u8>> = (0..batches)
			.map(|b| {
				(0..ROWS_PER_BATCH)
					.flat_map(|j| record(stream, b * ROWS_PER_BATCH + j))
					.collect()
			})
			.collect();
		per_rep.push(rep);
	}
	per_rep
}

/// The same records as [`append_staged`], with the seal's publish step moved to
/// a group sync: `staged_sync_every` batches are made durable by one `sync`,
/// and the seals in between are [`Engine::seal_staged_deferred`].
///
/// This is the row for the tier the LedgerFS host calls deferred durability.
/// The promise is the one the row above gives — "a sealed group survives, an
/// unsealed tail need not" — except that "durable" attaches to the sync rather
/// than to every seal. It is marked non-comparable for the same reason the row
/// above is, and it is not a claim that the seals are free: each engine's seal
/// still does the work its architecture requires (LedgerFS flushes its objects,
/// SQLite writes the WAL, redb commits its pages, the raw control writes the
/// file), and only the publish step — the one that makes the group the
/// current state — is shared.
fn append_deferred(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	if !engine.supports(Op::Deferred) {
		return Ok(unsupported(name, "deferred_append_960b", "no deferred seal tier"));
	}
	let batches = cfg.append_batches;
	let seal_every = cfg.staged_seal_every.max(1);
	let sync_every = cfg.staged_sync_every.max(1);
	let streams: Vec<String> = (0..cfg.repeats).map(|r| format!("t{r}")).collect();
	// Outside the timed section: this is how the run is configured, not what it
	// pays. Engines whose mode is per-commit ignore it.
	engine.begin_deferred()?;
	let per_rep = staged_fixtures(cfg, &streams, batches);
	let before = engine.space_used();
	let mut sample = measure(
		name,
		"deferred_append_960b",
		batches as u64,
		(batches * BATCH_BYTES) as u64,
		cfg.repeats,
		|r| {
			let mut since_seal = 0;
			let mut since_sync = 0;
			for batch in &per_rep[r] {
				let rows: Vec<&[u8]> = batch.chunks(RECORD_BYTES).collect();
				engine.staged_append(&streams[r], &rows)?;
				since_seal += 1;
				since_sync += 1;
				if since_seal == seal_every {
					engine.seal_staged_deferred()?;
					since_seal = 0;
				}
				if since_sync == sync_every {
					engine.sync_staged()?;
					since_sync = 0;
				}
			}
			// Whatever the tail had not sealed or synced is finished now, so it
			// is sealed and published: leaving it would be data the engine
			// promises to nobody.
			if since_seal > 0 {
				engine.seal_staged_deferred()?;
			}
			if since_sync > 0 {
				engine.sync_staged()?;
			}
			Ok(())
		},
	)?;
	sample.comparable = false;
	sample.note = Some(format!(
		"{batches} batches of {ROWS_PER_BATCH} × {RECORD_BYTES} B records, sealed every {seal_every} \
		 and synced every {sync_every}; each seal writes its group, one sync publishes \
		 every group since the last one"
	));
	record_space(&mut sample, engine, before, batches as u64);
	Ok(sample)
}

/// Read a whole stream back and check it, byte for byte.
///
/// The population happens outside the timed section, so the figure is the read
/// path alone; the comparison is deliberately inside it, because a read-back
/// that does not check what it read is not the operation this measures. On
/// LedgerFS the check is not added work — every object read verifies its own
/// BLAKE3 hash on the way out, so verification is part of the number by
/// construction. On the other engines it is a comparison the caller does.
fn read_back(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	let batches = cfg.append_batches;
	let stream = "main";
	for b in 0..batches {
		let rows: Vec<Vec<u8>> = (0..ROWS_PER_BATCH)
			.map(|j| record(stream, b * ROWS_PER_BATCH + j).to_vec())
			.collect();
		let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
		engine.append_batch(stream, &refs)?;
	}
	let mut expected = Vec::with_capacity(batches * BATCH_BYTES);
	for row in 0..batches * ROWS_PER_BATCH {
		expected.extend_from_slice(&record(stream, row));
	}
	let expected_len = expected.len();
	let mut sample = measure(
		name,
		"read_back_960b",
		batches as u64,
		expected_len as u64,
		cfg.repeats,
		|_| {
			let got = engine.read_log(stream)?;
			if got != expected {
				return Err(Error::engine(
					name,
					format!("read-back differs: {} bytes, expected {expected_len}", got.len()),
				));
			}
			Ok(())
		},
	)?;
	sample.note = Some(format!(
		"one whole-stream read per repetition ({batches} batches, every byte compared)"
	));
	Ok(sample)
}

/// Enumerate every key in the store — a directory listing for the filesystems,
/// a full scan for the databases.
///
/// Laid out `KEYS_PER_DIR` to a directory on every engine, because that is the
/// layout LedgerFS forces on an embedding application: see [`shard_key`] and
/// `KNOWN_ISSUES.md`. The 10- and
/// 100-entry workloads are one directory deep and the 1000-entry one is ten, so
/// the three sizes differ by how much data lives in the leaves rather than by
/// how deep the walk goes.
fn scan(engine: &mut dyn Engine, cfg: &Config, entries: usize) -> Result<Sample> {
	let name = engine.name();
	let workload = match entries {
		10 => "scan_10",
		100 => "scan_100",
		_ => "scan_1000",
	};
	let tag = "s";
	prepare_all(engine, std::slice::from_ref(&tag.to_string()), entries)?;
	for i in 0..entries {
		engine.put(&shard_key(tag, i), &value("ks", i))?;
	}
	let mut sample = measure(
		name,
		workload,
		entries as u64,
		(entries * VALUE_BYTES) as u64,
		cfg.repeats,
		|_| {
			let keys = engine.keys()?;
			if keys.len() != entries {
				return Err(Error::engine(
					name,
					format!("enumerated {} keys, expected {entries}", keys.len()),
				));
			}
			Ok(())
		},
	)?;
	sample.note = Some(format!(
		"{entries} entries, populated outside the timed section; {KEYS_PER_DIR} per directory"
	));
	Ok(sample)
}

/// What a restart costs: reopen, then touch the data once.
///
/// The read is inside the timed section deliberately. SQLite recovers a WAL
/// lazily — `open` returns before the recovery happens — so timing the open
/// alone would report near zero for the engine with the most recovery to do.
fn reopen_and_read(engine: &mut dyn Engine, cfg: &Config) -> Result<Sample> {
	let name = engine.name();
	if !engine.supports(Op::Reopen) {
		return Ok(unsupported(
			name,
			"reopen_and_read",
			"no recovery step: there is no journal to replay and no root to select",
		));
	}
	let n = cfg.insert_n;
	let tag = "o";
	prepare_all(engine, std::slice::from_ref(&tag.to_string()), n)?;
	for i in 0..n {
		engine.put(&shard_key(tag, i), &value("ko", i))?;
	}
	let mut sample = measure(name, "reopen_and_read", 1, 0, cfg.repeats, |_| {
		engine.reopen()?;
		engine.get(&shard_key(tag, 0))?;
		Ok(())
	})?;
	sample.note = Some("reopen plus one read, so lazy recovery is inside the timed section".to_string());
	Ok(sample)
}

// endregion: --- The workloads
