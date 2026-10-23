//! The four things being measured, behind one trait.
//!
//! # The model
//!
//! A key/value store where every write is durable before the call returns.
//! That is the narrowest thing all four engines can honestly be asked to do,
//! and it is the thing LedgerFS actually is: a place to put bytes such that
//! after a power cut they are still there.
//!
//! - **`ledgerfs`**: one file per key under `/kv`, written through
//!   [`Store`]. One [`put`](Engine::put) is `write_new_file`: a single
//!   transaction that creates the file and places its content together, so one
//!   commit, and a commit is two device barriers. Recorded rather than hidden,
//!   because it is part of the number.
//! - **`sqlite-wal`**: SQLite from its own bundled amalgamation, in WAL mode
//!   with `synchronous=FULL`. One `put` is one implicit transaction, so one WAL
//!   commit, so one fsync.
//! - **`redb`**: a pure-Rust embedded KV store, with each write transaction
//!   set to `Durability::Immediate`. One `put` is one fsynced commit.
//! - **`raw-file`**, the control: `File::create` + `write_all` + `sync_all`,
//!   plus a directory `sync_all` so the *name* is durable too. This is what
//!   "just use the filesystem" costs when you mean it.
//!
//! Every engine states its durability setting in
//! [`durability`](Engine::durability), and that string ends up in the output.
//!
//! # What is deliberately absent
//!
//! The raw-file control cannot express `Reopen`: a directory full of fsync'd
//! files has no journal to replay and no root to select, so there is no
//! recovery step to time. A gap is the honest entry; a fabricated one would be
//! the thing this harness exists to prevent. (A gap is kept rather than filled
//! by timing `open(2)`: that would measure the kernel, and the row would read
//! as a win for the control.)
//!
//! Batched *appends* are a different operation ([`Engine::append_batch`]) and
//! every engine can express it: one append is one object, so writing a batch of
//! rows costs one durability point per engine: a transaction for SQLite, one
//! write transaction for redb, one `sync_all` for the raw control, one commit
//! for LedgerFS.
//!
//! # The staged tier
//!
//! [`Engine::staged_append`] / [`Engine::seal_staged`] are the operation whose
//! durability point is *not* every call: rows accumulate with no barrier and
//! `seal_staged` makes everything since the last seal durable in one step.
//! Every engine has an honest idiom for it, which is why the row exists:
//!
//! - LedgerFS: a staging session (`begin_staging` … `seal_staged`): objects
//!   built in memory, one commit at the seal.
//! - SQLite: one deferred transaction left open: rows accumulate in the WAL's
//!   dirty pages, `COMMIT` at the seal.
//! - redb: one `WriteTransaction` held open with `Durability::Immediate`,
//!   committed at the seal.
//! - raw-file: a held append-mode file: `write_all` per row, one `sync_all`
//!   at the seal.
//!
//! What the row measures is therefore the cost of *one batch* when the
//! durability point is every Nth batch, and it is marked non-comparable for
//! exactly that reason. It is not a way to make LedgerFS look fast: the same
//! relaxation is granted to all four engines.
//!
//! [`Engine::seal_staged_deferred`] moves one more step: the seal writes and
//! flushes its data but does not publish the new state, and
//! [`Engine::sync_staged`] pays for that once per group of seals. The idioms
//! are, in the same order: LedgerFS `seal_staged_deferred` + `sync`; SQLite a
//! commit under `synchronous=NORMAL` with the group ended by a
//! `wal_checkpoint(FULL)`; redb a `Durability::None` commit with an empty
//! `Durability::Immediate` transaction as the sync; the raw control writes
//! without `sync_all` and syncs the held file at the group end. The promises
//! differ in how much work the seal itself does (LedgerFS flushes its
//! objects, SQLite leaves them in the WAL, redb leaves them mapped, raw leaves
//! them in the page cache), so the row is reported with that difference named
//! rather than summarised away.
//!
//! # Per deployment
//!
//! The workloads see only [`Engine`] (no workload names an engine), and each
//! third-party engine sits behind a cargo feature of the same name. A
//! deployment that compares against SQLite builds `--no-default-features
//! --features sqlite` and its binary contains no redb code at all, and the
//! other way round; nothing anywhere downcasts or probes for what was not
//! built. LedgerFS and the raw-file control are always present: one is the
//! subject, the other the baseline every number answers to.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use ledgerfs_host::{BLOCK_SIZE, FileBlockDevice, InodeKind, Store};
#[cfg(feature = "redb")]
use redb::{ReadableDatabase, ReadableTable};

use crate::error::{Error, Result};

/// A workload an engine may not be able to express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
	/// Several inserts in one transaction.
	Batch,
	/// Append to an existing file without rewriting it.
	Append,
	/// Accumulate rows with no durability point, then seal them in one.
	Staged,
	/// As `Staged`, with the seal's publish step moved to a group
	/// [`sync_staged`](Engine::sync_staged): one sync covers several seals.
	Deferred,
	/// Close and reopen, as a restart would.
	Reopen,
}

/// One storage engine, opened on a fresh location.
pub trait Engine {
	fn name(&self) -> &'static str;

	/// Exactly what the engine was told to guarantee, in its own terms.
	fn durability(&self) -> &'static str;

	fn supports(&self, op: Op) -> bool;

	/// Make whatever structure the given key prefixes need, before anything is
	/// timed.
	///
	/// LedgerFS and the raw-file control need a directory per prefix, and on
	/// LedgerFS creating one is itself a commit. Doing it here keeps that cost
	/// out of the measurement rather than loading it onto the first operation
	/// of the first bucket. A key/value engine has nothing to do.
	fn prepare(&mut self, _prefixes: &[String]) -> Result<()> {
		Ok(())
	}

	/// Write `key` → `value`, durable before this returns.
	fn put(&mut self, key: &str, value: &[u8]) -> Result<()>;

	/// Write every pair in one transaction.
	fn put_batch(&mut self, items: &[(String, Vec<u8>)]) -> Result<()>;

	fn get(&self, key: &str) -> Result<Vec<u8>>;

	/// Every key present, as a directory listing would return them.
	fn keys(&self) -> Result<Vec<String>>;

	/// Append `data` to the log named `name`, durable before this returns.
	fn append(&mut self, name: &str, data: &[u8]) -> Result<()>;

	/// Append each row of `rows` to the log named `name`, and make the whole
	/// batch durable with one durability point before this returns.
	///
	/// This is the write shape of an application that accumulates fixed-size
	/// records and flushes them as they arrive: the batch, not the row, is what
	/// has to survive a power cut, so the number that matters is per batch.
	/// Each engine does it in its own idiom, and the idioms are the finding:
	/// SQLite wraps the rows in one transaction, redb puts them in one write
	/// transaction, the raw control gets one fsync by writing once, and on
	/// LedgerFS one append *is* one commit, so the batch costs what one append
	/// costs.
	fn append_batch(&mut self, name: &str, rows: &[&[u8]]) -> Result<()>;

	/// Append each row of `rows` to the log named `name` **without** a
	/// durability point, to be made durable by the next
	/// [`seal_staged`](Engine::seal_staged).
	///
	/// The bytes are in the engine's own machinery (dirty pages, an open
	/// transaction, a held file), but a power cut at this instant is allowed to
	/// lose them. That is the tier's whole point: the caller decides how often
	/// it pays for durability.
	fn staged_append(&mut self, name: &str, rows: &[&[u8]]) -> Result<()>;

	/// Make everything staged since the last seal durable, in one step.
	fn seal_staged(&mut self) -> Result<()>;

	/// Prepare for a run on the deferred tier, before the first staged append.
	///
	/// An engine whose durability mode is chosen per connection rather than per
	/// commit (SQLite) sets it here, because the mode cannot be changed inside
	/// the transaction a staged append opens. A no-op elsewhere, and never
	/// timed: it is what the run is configured with, not what it pays.
	fn begin_deferred(&mut self) -> Result<()> {
		Ok(())
	}

	/// Seal as [`seal_staged`](Engine::seal_staged) does, but stop short of the
	/// step that publishes the new state to the rest of the world, leaving it
	/// to the group's [`sync_staged`](Engine::sync_staged).
	///
	/// The tier is "the seal's data is written and pushed to the medium, the
	/// pointer that makes it the volume's current state waits". It is the
	/// durability relaxation an application makes when it can afford a seal
	/// often but not the full price of one; every engine has an honest idiom
	/// for it, documented on its implementation.
	fn seal_staged_deferred(&mut self) -> Result<()>;

	/// Complete the publish step of every [`seal_staged_deferred`] since the
	/// last sync, in one step.
	fn sync_staged(&mut self) -> Result<()>;

	/// The whole log named `name`, first byte written first.
	fn read_log(&self, name: &str) -> Result<Vec<u8>>;

	/// Drop everything in memory and reopen from disk.
	fn reopen(&mut self) -> Result<()>;

	/// Free bytes left on the backing store, if the engine can say.
	///
	/// Reported so a write workload can state what it *cost in space* and not
	/// only in time. On LedgerFS that is the more interesting of the two: every
	/// commit writes new objects and frees nothing (§22 of `FORMAT.md`), so the
	/// number is a lower bound on the volume a workload needs, and it is a
	/// property a reader cannot infer from the timing table at all.
	fn space_used(&self) -> Option<u64> {
		None
	}
}

/// The engine roster, in the order they are reported.
///
/// A `fn` rather than a `Box<dyn Fn>` because there is nothing to capture and
/// the table should be readable at a glance.
pub type Open = fn(&Path, u64) -> Result<Box<dyn Engine>>;

pub const ROSTER: &[(&str, Open)] = &[
	("ledgerfs", open_ledgerfs),
	#[cfg(feature = "sqlite")]
	("sqlite-wal", open_sqlite),
	#[cfg(feature = "redb")]
	("redb", open_redb),
	("raw-file", open_raw_file),
];

pub fn by_name(name: &str) -> Option<Open> {
	ROSTER.iter().find(|(n, _)| *n == name).map(|(_, open)| *open)
}

// region:    --- LedgerFS

/// LedgerFS is a filesystem, so the mapping is the obvious one: one file per
/// key, under a directory. `/kv` is created once at open, outside anything
/// timed; a key that contains a `/` gets its intermediate directories made on
/// first use, which only the scan workload's keys do.
///
/// The file name carries an `f` prefix so a key can never collide with a
/// directory that a longer key needs: `s7` is a directory, `s7/foo` is a file
/// inside it, and nothing is both.
const KV_DIR: &str = "/kv";

/// How many keys go in one directory.
///
/// LedgerFS's `DirNode` stops accepting entries at 400 and its split is not
/// implemented, so a directory holding a thousand files is not a slow case, it
/// is a refused one. Every workload therefore shards its keys at 100 on every
/// engine (what an embedding application has to do too), so a workload is
/// never a test of whether some engine happens to allow a flat directory. See
/// `KNOWN_ISSUES.md`.
pub const KEYS_PER_DIR: usize = 100;

/// The relative path key `i` lives at, under buckets named from `tag`:
/// `s000/k0000001`.
pub fn shard_key(tag: &str, i: usize) -> String {
	format!("{tag}{:03}/k{i:07}", i / KEYS_PER_DIR)
}

/// Every directory the keys `tag`0..`n` need, for [`Engine::prepare`].
pub fn shard_dirs(tag: &str, n: usize) -> Vec<String> {
	(0..n).map(|i| format!("{tag}{:03}", i / KEYS_PER_DIR)).collect()
}

struct LedgerFs {
	path: PathBuf,
	blocks: u64,
	store: Option<Store>,
}

fn open_ledgerfs(dir: &Path, blocks: u64) -> Result<Box<dyn Engine>> {
	let path = dir.join("ledgerfs.img");
	let bytes = blocks * BLOCK_SIZE as u64;
	let device = FileBlockDevice::create(&path, bytes).map_err(|e| Error::engine("ledgerfs", e.to_string()))?;
	let mut store = Store::format(Box::new(device), blocks).map_err(|e| Error::engine("ledgerfs", e.to_string()))?;
	// `/kv` and `/log` exist before anything is timed: a file's parent must be
	// a directory, and creating one is a commit of its own.
	store
		.mkdir(KV_DIR, 0o755)
		.map_err(|e| Error::engine("ledgerfs", e.to_string()))?;
	store
		.mkdir("/log", 0o755)
		.map_err(|e| Error::engine("ledgerfs", e.to_string()))?;
	Ok(Box::new(LedgerFs {
		path,
		blocks,
		store: Some(store),
	}))
}

impl LedgerFs {
	fn store(&mut self) -> Result<&mut Store> {
		self.store.as_mut().ok_or_else(|| Error::engine("ledgerfs", "volume is closed"))
	}

	/// Where a key lives on the volume, with only the last component given the
	/// `f` prefix: the intermediate components are directories, and those
	/// carry the key's own names.
	fn key_path(key: &str) -> String {
		match key.rsplit_once('/') {
			Some((dir, leaf)) => format!("{KV_DIR}/{dir}/f{leaf}"),
			None => format!("{KV_DIR}/f{key}"),
		}
	}

	fn err(e: impl std::fmt::Display) -> Error {
		Error::engine("ledgerfs", e.to_string())
	}
}

impl Engine for LedgerFs {
	fn name(&self) -> &'static str {
		"ledgerfs"
	}

	fn durability(&self) -> &'static str {
		"one superblock commit per put (the value fits one object); objects + allocator snapshot, barrier, superblock, barrier"
	}

	fn supports(&self, op: Op) -> bool {
		// Every operation, now that `put_batch` is a staging session: there is
		// no recovery step separable from a mount (`reopen` below re-mounts,
		// which is the thing a restart pays for).
		matches!(op, Op::Batch | Op::Append | Op::Staged | Op::Deferred | Op::Reopen)
	}

	fn prepare(&mut self, prefixes: &[String]) -> Result<()> {
		for prefix in prefixes {
			let dir = format!("{KV_DIR}/{prefix}");
			let store = self.store()?;
			if store.stat(&dir).is_err() {
				store.mkdir(&dir, 0o755).map_err(Self::err)?;
			}
		}
		Ok(())
	}

	fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
		let path = Self::key_path(key);
		self.store()?.write_new_file(&path, value, 0o644).map_err(Self::err)
	}

	/// One staging session, one seal: the caller-visible batch transaction the
	/// volume did not have before it had a staging tier. Each key is still its
	/// own file; what is shared is the commit.
	fn put_batch(&mut self, items: &[(String, Vec<u8>)]) -> Result<()> {
		const WORKLOAD: &str = "ledgerfs";
		let store = self.store()?;
		store.begin_staging(0).map_err(Self::err)?;
		for (key, value) in items {
			if let Err(e) = store.staged_write_new_file(&Self::key_path(key), value, 0o644) {
				store.end_staging();
				return Err(Error::engine(WORKLOAD, e.to_string()));
			}
		}
		match store.seal_staged() {
			Ok(_) => Ok(()),
			Err(e) => {
				store.end_staging();
				Err(Error::engine(WORKLOAD, e.to_string()))
			}
		}
	}

	fn get(&self, key: &str) -> Result<Vec<u8>> {
		let store = self
			.store
			.as_ref()
			.ok_or_else(|| Error::engine("ledgerfs", "volume is closed"))?;
		store
			.read_file(&Self::key_path(key))
			.map_err(|e| Error::engine("ledgerfs", e.to_string()))
	}

	fn keys(&self) -> Result<Vec<String>> {
		let store = self
			.store
			.as_ref()
			.ok_or_else(|| Error::engine("ledgerfs", "volume is closed"))?;
		let mut keys = Vec::new();
		let mut stack = vec![(KV_DIR.to_string(), String::new())];
		while let Some((dir, prefix)) = stack.pop() {
			for entry in store.read_dir(&dir).map_err(Self::err)? {
				let name = entry.name.trim_start_matches('f');
				let key = if prefix.is_empty() {
					name.to_string()
				} else {
					format!("{prefix}/{name}")
				};
				if entry.kind == InodeKind::Directory {
					stack.push((format!("{dir}/{}", entry.name), key));
				} else {
					keys.push(key);
				}
			}
		}
		keys.sort();
		Ok(keys)
	}

	fn append(&mut self, name: &str, data: &[u8]) -> Result<()> {
		let path = format!("/log/{name}");
		self.store()?
			.append(&path, data, 0o644)
			.map_err(|e| Error::engine("ledgerfs", e.to_string()))
	}

	/// The rows land as consecutive bytes of one object, so a batch is one
	/// `append` and one commit: there is nothing smaller on this volume to
	/// make it cheaper than.
	fn append_batch(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let mut data = Vec::with_capacity(rows.iter().map(|r| r.len()).sum());
		for row in rows {
			data.extend_from_slice(row);
		}
		self.append(name, &data)
	}

	/// The volume's own staged tier: bytes go into objects built in memory, and
	/// the session is opened on `open_ledgerfs`'s store if it is not already
	/// open. Nothing here reaches the device.
	fn staged_append(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let mut data = Vec::with_capacity(rows.iter().map(|r| r.len()).sum());
		for row in rows {
			data.extend_from_slice(row);
		}
		let path = format!("/log/{name}");
		let store = self.store()?;
		if !store.staging_is_open() {
			store.begin_staging(0).map_err(Self::err)?;
		}
		store.staged_append(&path, &data, 0o644).map_err(Self::err)
	}

	fn seal_staged(&mut self) -> Result<()> {
		let store = self.store()?;
		if store.staging_is_open() {
			store.seal_staged().map_err(Self::err)?;
		}
		Ok(())
	}

	/// The volume's deferred tier: the seal commits, writes and flushes its
	/// objects, and withholds the superblock that would publish the new root.
	fn seal_staged_deferred(&mut self) -> Result<()> {
		let store = self.store()?;
		if store.staging_is_open() {
			store.seal_staged_deferred().map_err(Self::err)?;
		}
		Ok(())
	}

	/// One superblock write and one barrier, covering every seal deferred since
	/// the last sync.
	fn sync_staged(&mut self) -> Result<()> {
		self.store()?.sync().map_err(Self::err)
	}

	fn read_log(&self, name: &str) -> Result<Vec<u8>> {
		let store = self
			.store
			.as_ref()
			.ok_or_else(|| Error::engine("ledgerfs", "volume is closed"))?;
		store
			.read_file(&format!("/log/{name}"))
			.map_err(|e| Error::engine("ledgerfs", e.to_string()))
	}

	/// Drop the mount and mount again from the same image.
	///
	/// This is the whole of LedgerFS recovery: a mount reads the superblock
	/// copies, picks the intact copy with the highest commit sequence, and
	/// adopts the root it names, without fetching that root, so a mount is as
	/// cheap as reading four blocks. There is no intent log to replay: the live
	/// commit path writes none (`FORMAT.md` §15.1). Dropping the `Store`
	/// releases the image's advisory lock, which is what the writer needed it
	/// for.
	fn reopen(&mut self) -> Result<()> {
		self.store = None;
		let bytes = self.blocks * BLOCK_SIZE as u64;
		let device = FileBlockDevice::open_rw(&self.path, 0, bytes)
			.map_err(|e| Error::engine("ledgerfs", format!("reopen: {e}")))?;
		let store = Store::open(Box::new(device)).map_err(|e| Error::engine("ledgerfs", format!("reopen: {e}")))?;
		self.store = Some(store);
		Ok(())
	}

	fn space_used(&self) -> Option<u64> {
		// `free_bytes` is `None` on a mount whose superblock carries no
		// allocation accounting, which is a real answer (not measured), and is
		// reported as such rather than as zero.
		self.store.as_ref().and_then(|s| s.free_bytes())
	}
}

// endregion: --- LedgerFS

// region:    --- SQLite

#[cfg(feature = "sqlite")]
const SQLITE_SCHEMA: &str = "
	CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v BLOB NOT NULL);
	CREATE TABLE IF NOT EXISTS log (
		stream TEXT NOT NULL,
		seq INTEGER NOT NULL,
		data BLOB NOT NULL,
		PRIMARY KEY (stream, seq)
	);
";

#[cfg(feature = "sqlite")]
struct Sqlite {
	path: PathBuf,
	conn: rusqlite::Connection,
}

#[cfg(feature = "sqlite")]
fn open_sqlite(dir: &Path, _blocks: u64) -> Result<Box<dyn Engine>> {
	let path = dir.join("sqlite.db");
	let conn = rusqlite::Connection::open(&path).map_err(sq)?;
	configure(&conn)?;
	conn.execute_batch(SQLITE_SCHEMA).map_err(sq)?;
	Ok(Box::new(Sqlite { path, conn }))
}

/// WAL plus `synchronous=FULL`.
///
/// `FULL` rather than `NORMAL` is the load-bearing part: in WAL mode `NORMAL`
/// does not fsync on commit, so a powered-off machine can lose committed
/// transactions. `FULL` is the setting that makes a SQLite commit comparable to
/// a LedgerFS commit.
#[cfg(feature = "sqlite")]
fn configure(conn: &rusqlite::Connection) -> Result<()> {
	let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).map_err(sq)?;
	if !mode.eq_ignore_ascii_case("wal") {
		return Err(Error::engine(
			"sqlite-wal",
			format!("asked for WAL, got '{mode}'; the medium cannot do it"),
		));
	}
	conn.execute_batch("PRAGMA synchronous=FULL;").map_err(sq)?;
	Ok(())
}

#[cfg(feature = "sqlite")]
fn sq(e: rusqlite::Error) -> Error {
	Error::engine("sqlite-wal", e.to_string())
}

#[cfg(feature = "sqlite")]
impl Engine for Sqlite {
	fn name(&self) -> &'static str {
		"sqlite-wal"
	}

	fn durability(&self) -> &'static str {
		"journal_mode=WAL, synchronous=FULL; one transaction per put"
	}

	fn supports(&self, op: Op) -> bool {
		matches!(op, Op::Batch | Op::Append | Op::Staged | Op::Deferred | Op::Reopen)
	}

	fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
		self.conn
			.execute("INSERT INTO kv (k, v) VALUES (?1, ?2)", rusqlite::params![key, value])
			.map_err(sq)?;
		Ok(())
	}

	fn put_batch(&mut self, items: &[(String, Vec<u8>)]) -> Result<()> {
		let tx = self.conn.transaction().map_err(sq)?;
		{
			let mut stmt = tx.prepare("INSERT INTO kv (k, v) VALUES (?1, ?2)").map_err(sq)?;
			for (key, value) in items {
				stmt.execute(rusqlite::params![key, value]).map_err(sq)?;
			}
		}
		tx.commit().map_err(sq)?;
		Ok(())
	}

	fn get(&self, key: &str) -> Result<Vec<u8>> {
		self.conn
			.query_row("SELECT v FROM kv WHERE k = ?1", [key], |r| r.get(0))
			.map_err(sq)
	}

	fn keys(&self) -> Result<Vec<String>> {
		let mut stmt = self.conn.prepare("SELECT k FROM kv").map_err(sq)?;
		let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(sq)?;
		let mut keys = Vec::new();
		for row in rows {
			keys.push(row.map_err(sq)?);
		}
		Ok(keys)
	}

	fn append(&mut self, name: &str, data: &[u8]) -> Result<()> {
		// One row per append, which is the shape LedgerFS's chunked log has:
		// nothing already stored is rewritten. The sequence is derived inside
		// the statement (`MAX(seq) + 1` over this stream's rows, 0 for the
		// first), so the counter survives a reopen without bookkeeping.
		self.conn
			.execute(
				"INSERT INTO log (stream, seq, data)
				 SELECT ?1, COALESCE(MAX(seq) + 1, 0), ?2 FROM log WHERE stream = ?1",
				rusqlite::params![name, data],
			)
			.map_err(sq)?;
		Ok(())
	}

	/// The batch in SQLite's own idiom (BEGIN, one INSERT per row, COMMIT),
	/// the diagram-shaped answer to "many rows, one fsync".
	fn append_batch(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let tx = self.conn.transaction().map_err(sq)?;
		{
			let mut stmt = tx
				.prepare(
					"INSERT INTO log (stream, seq, data)
					 SELECT ?1, COALESCE(MAX(seq) + 1, 0), ?2 FROM log WHERE stream = ?1",
				)
				.map_err(sq)?;
			for row in rows {
				stmt.execute(rusqlite::params![name, row]).map_err(sq)?;
			}
		}
		tx.commit().map_err(sq)
	}

	/// One deferred transaction left open: rows land in the WAL's dirty pages
	/// and nothing is fsync'd until [`seal_staged`](Self::seal_staged) commits.
	fn staged_append(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		if self.conn.is_autocommit() {
			self.conn.execute_batch("BEGIN").map_err(sq)?;
		}
		let mut stmt = self
			.conn
			.prepare(
				"INSERT INTO log (stream, seq, data)
				 SELECT ?1, COALESCE(MAX(seq) + 1, 0), ?2 FROM log WHERE stream = ?1",
			)
			.map_err(sq)?;
		for row in rows {
			stmt.execute(rusqlite::params![name, row]).map_err(sq)?;
		}
		Ok(())
	}

	fn seal_staged(&mut self) -> Result<()> {
		if !self.conn.is_autocommit() {
			self.conn.execute_batch("COMMIT").map_err(sq)?;
		}
		Ok(())
	}

	/// WAL mode's own group-durability setting: commits append to the WAL and do
	/// not fsync it, and the WAL is synced when the group is checkpointed. It
	/// has to be set between transactions, which is why it is here and not in
	/// [`seal_staged_deferred`](Self::seal_staged_deferred): a staged append
	/// has already opened one.
	fn begin_deferred(&mut self) -> Result<()> {
		self.conn.execute_batch("PRAGMA synchronous=NORMAL;").map_err(sq)
	}

	/// A commit that writes the WAL and leaves the fsync to the group's
	/// checkpoint.
	fn seal_staged_deferred(&mut self) -> Result<()> {
		if !self.conn.is_autocommit() {
			self.conn.execute_batch("COMMIT").map_err(sq)?;
		}
		Ok(())
	}

	/// A FULL checkpoint copies the WAL back into the database and syncs both,
	/// which is the point at which every commit since the last sync becomes
	/// durable.
	fn sync_staged(&mut self) -> Result<()> {
		self.conn.execute_batch("PRAGMA wal_checkpoint(FULL);").map_err(sq)?;
		Ok(())
	}

	fn read_log(&self, name: &str) -> Result<Vec<u8>> {
		let mut stmt = self
			.conn
			.prepare("SELECT data FROM log WHERE stream = ?1 ORDER BY seq")
			.map_err(sq)?;
		let rows = stmt.query_map([name], |r| r.get::<_, Vec<u8>>(0)).map_err(sq)?;
		let mut out = Vec::new();
		for row in rows {
			out.extend_from_slice(&row.map_err(sq)?);
		}
		Ok(out)
	}

	fn reopen(&mut self) -> Result<()> {
		// A reopen with a staged transaction open would roll it back by closing
		// the connection; refuse instead, so a workload bug cannot read as a
		// durability result.
		if !self.conn.is_autocommit() {
			return Err(Error::engine(
				"sqlite-wal",
				"reopen: a staged transaction is still open; seal it first",
			));
		}
		self.conn = rusqlite::Connection::open(&self.path).map_err(sq)?;
		configure(&self.conn)
	}
}

// endregion: --- SQLite

// region:    --- redb

#[cfg(feature = "redb")]
const REDB_KV: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("kv");
#[cfg(feature = "redb")]
const REDB_LOG: redb::TableDefinition<u128, &[u8]> = redb::TableDefinition::new("log");

#[cfg(feature = "redb")]
struct Redb {
	path: PathBuf,
	db: Option<redb::Database>,
	/// Per-stream next sequence, keyed by the same 64-bit hash the row keys
	/// carry, so [`Redb::reopen`] can rebuild every counter in one pass over
	/// the table, exactly the way the single counter used to.
	log_seq: std::collections::HashMap<u64, u64>,
	/// The write transaction a staging session holds open, if any.
	///
	/// redb's `WriteTransaction` owns `Arc`s rather than borrowing the database,
	/// so holding one across calls is just a field: no leak, no ouroboros.
	staged: Option<redb::WriteTransaction>,
}

/// A log row's key: the stream name's 64-bit FNV-1a hash in the high half, the
/// row's sequence in the low half: one stream's rows sort together and apart
/// from every other stream's, and the table stays one `u128`-keyed table.
#[cfg(feature = "redb")]
fn stream_key(name: &str, seq: u64) -> u128 {
	((stream_hash(name) as u128) << 64) | seq as u128
}

#[cfg(feature = "redb")]
fn stream_hash(name: &str) -> u64 {
	let mut h = 0xcbf2_9ce4_8422_2325u64;
	for byte in name.bytes() {
		h ^= byte as u64;
		h = h.wrapping_mul(0x0000_0100_0000_01B3);
	}
	h
}

#[cfg(feature = "redb")]
fn open_redb(dir: &Path, _blocks: u64) -> Result<Box<dyn Engine>> {
	let path = dir.join("redb.db");
	let db = redb::Database::create(&path).map_err(rd)?;
	// Both tables have to be created before the first read transaction, or a
	// read of an absent table is an error rather than an empty result.
	let write = db.begin_write().map_err(rd)?;
	write.open_table(REDB_KV).map_err(rd)?;
	write.open_table(REDB_LOG).map_err(rd)?;
	write.commit().map_err(rd)?;
	Ok(Box::new(Redb {
		path,
		db: Some(db),
		log_seq: std::collections::HashMap::new(),
		staged: None,
	}))
}

#[cfg(feature = "redb")]
fn rd<E: std::fmt::Display>(e: E) -> Error {
	Error::engine("redb", e.to_string())
}

#[cfg(feature = "redb")]
impl Redb {
	fn db(&self) -> Result<&redb::Database> {
		self.db.as_ref().ok_or_else(|| Error::engine("redb", "database is closed"))
	}
}

#[cfg(feature = "redb")]
impl Engine for Redb {
	fn name(&self) -> &'static str {
		"redb"
	}

	fn durability(&self) -> &'static str {
		"Durability::Immediate on every write transaction"
	}

	fn supports(&self, op: Op) -> bool {
		matches!(op, Op::Batch | Op::Append | Op::Staged | Op::Deferred | Op::Reopen)
	}

	fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
		let mut write = self.db()?.begin_write().map_err(rd)?;
		write.set_durability(redb::Durability::Immediate).map_err(rd)?;
		{
			let mut table = write.open_table(REDB_KV).map_err(rd)?;
			table.insert(key, value).map_err(rd)?;
		}
		write.commit().map_err(rd)?;
		Ok(())
	}

	fn put_batch(&mut self, items: &[(String, Vec<u8>)]) -> Result<()> {
		let mut write = self.db()?.begin_write().map_err(rd)?;
		write.set_durability(redb::Durability::Immediate).map_err(rd)?;
		{
			let mut table = write.open_table(REDB_KV).map_err(rd)?;
			for (key, value) in items {
				table.insert(key.as_str(), value.as_slice()).map_err(rd)?;
			}
		}
		write.commit().map_err(rd)?;
		Ok(())
	}

	fn get(&self, key: &str) -> Result<Vec<u8>> {
		let read = self.db()?.begin_read().map_err(rd)?;
		let table = read.open_table(REDB_KV).map_err(rd)?;
		table
			.get(key)
			.map_err(rd)?
			.map(|v| v.value().to_vec())
			.ok_or_else(|| Error::engine("redb", format!("{key} is absent")))
	}

	fn keys(&self) -> Result<Vec<String>> {
		let read = self.db()?.begin_read().map_err(rd)?;
		let table = read.open_table(REDB_KV).map_err(rd)?;
		let mut keys = Vec::new();
		for entry in table.iter().map_err(rd)? {
			let (k, _) = entry.map_err(rd)?;
			keys.push(k.value().to_string());
		}
		Ok(keys)
	}

	fn append(&mut self, name: &str, data: &[u8]) -> Result<()> {
		let seq = *self.log_seq.entry(stream_hash(name)).or_insert(0);
		let mut write = self.db()?.begin_write().map_err(rd)?;
		write.set_durability(redb::Durability::Immediate).map_err(rd)?;
		{
			let mut table = write.open_table(REDB_LOG).map_err(rd)?;
			table.insert(stream_key(name, seq), data).map_err(rd)?;
		}
		write.commit().map_err(rd)?;
		self.log_seq.insert(stream_hash(name), seq + 1);
		Ok(())
	}

	/// One write transaction for the whole batch, `Durability::Immediate` at
	/// its commit: the same single durability point the other engines get.
	fn append_batch(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let hash = stream_hash(name);
		let first = *self.log_seq.entry(hash).or_insert(0);
		let mut write = self.db()?.begin_write().map_err(rd)?;
		write.set_durability(redb::Durability::Immediate).map_err(rd)?;
		{
			let mut table = write.open_table(REDB_LOG).map_err(rd)?;
			for (i, row) in rows.iter().enumerate() {
				table.insert(stream_key(name, first + i as u64), *row).map_err(rd)?;
			}
		}
		write.commit().map_err(rd)?;
		self.log_seq.insert(hash, first + rows.len() as u64);
		Ok(())
	}

	/// One write transaction held open across calls: rows are inserted into it
	/// and nothing is fsync'd until [`seal_staged`](Self::seal_staged) commits,
	/// which is redb's own answer to "many rows, one durability point".
	fn staged_append(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let hash = stream_hash(name);
		let first = *self.log_seq.entry(hash).or_insert(0);
		if self.staged.is_none() {
			let mut write = self.db()?.begin_write().map_err(rd)?;
			write.set_durability(redb::Durability::Immediate).map_err(rd)?;
			self.staged = Some(write);
		}
		{
			let write = self.staged.as_ref().expect("just opened");
			let mut table = write.open_table(REDB_LOG).map_err(rd)?;
			for (i, row) in rows.iter().enumerate() {
				table.insert(stream_key(name, first + i as u64), *row).map_err(rd)?;
			}
		}
		self.log_seq.insert(hash, first + rows.len() as u64);
		Ok(())
	}

	fn seal_staged(&mut self) -> Result<()> {
		if let Some(write) = self.staged.take() {
			write.commit().map_err(rd)?;
		}
		Ok(())
	}

	/// The transaction still commits, so its pages are written and the root it
	/// publishes is visible to later transactions; only the fsync is skipped
	/// (`Durability::None`), which is redb's own separation of the two.
	fn seal_staged_deferred(&mut self) -> Result<()> {
		if let Some(mut write) = self.staged.take() {
			write.set_durability(redb::Durability::None).map_err(rd)?;
			write.commit().map_err(rd)?;
		}
		Ok(())
	}

	/// An empty `Immediate` transaction is redb's way to say "sync what is
	/// already written": its commit fsyncs the database file, which is where
	/// the `None` commits put their pages.
	fn sync_staged(&mut self) -> Result<()> {
		let mut write = self.db()?.begin_write().map_err(rd)?;
		write.set_durability(redb::Durability::Immediate).map_err(rd)?;
		write.commit().map_err(rd)?;
		Ok(())
	}

	fn read_log(&self, name: &str) -> Result<Vec<u8>> {
		let read = self.db()?.begin_read().map_err(rd)?;
		let table = read.open_table(REDB_LOG).map_err(rd)?;
		let hash = stream_hash(name) as u128;
		let range = table.range(hash << 64..(hash + 1) << 64).map_err(rd)?;
		let mut out = Vec::new();
		for entry in range {
			let (_, v) = entry.map_err(rd)?;
			out.extend_from_slice(v.value());
		}
		Ok(out)
	}

	fn reopen(&mut self) -> Result<()> {
		// A staged write transaction is uncommitted by definition; aborting it
		// here is what a crash would do, and it must happen before the handle
		// to the database goes away.
		if let Some(write) = self.staged.take() {
			write.abort().map_err(rd)?;
		}
		self.db = None;
		let db = redb::Database::open(&self.path).map_err(rd)?;
		// Pick each stream's sequence counter back up from what survived: the
		// stream hash is the high half of every key, so one pass rebuilds every
		// counter without knowing the names.
		let mut log_seq: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
		{
			let read = db.begin_read().map_err(rd)?;
			let table = read.open_table(REDB_LOG).map_err(rd)?;
			for entry in table.iter().map_err(rd)? {
				let (k, _) = entry.map_err(rd)?;
				let key = k.value();
				let counter = log_seq.entry((key >> 64) as u64).or_insert(0);
				*counter = (*counter).max((key & (u64::MAX as u128)) as u64 + 1);
			}
		}
		self.db = Some(db);
		self.log_seq = log_seq;
		Ok(())
	}
}

// endregion: --- redb

// region:    --- Raw file

/// The control: no engine, just the filesystem's own primitives.
///
/// Worth having because it is the number every other number has to beat to
/// justify existing. It is also not quite as cheap as it looks: a created file
/// is not durable until its parent directory is synced, so a `put` here is a
/// file fsync *and* a directory fsync, which is a cost LedgerFS folds into one
/// superblock commit.
struct RawFile {
	dir: PathBuf,
	next_seq: u64,
	/// The append handle a staging session holds open, and the stream it names.
	/// The name is kept so a session that switches streams reopens the right
	/// file rather than appending to the wrong one.
	staged: Option<(String, File)>,
}

fn open_raw_file(dir: &Path, _blocks: u64) -> Result<Box<dyn Engine>> {
	let kv = dir.join("raw/kv");
	let log = dir.join("raw/log");
	fs::create_dir_all(&kv)?;
	fs::create_dir_all(&log)?;
	// One file, pre-created, so `append` measures appending rather than
	// creating.
	File::create(log.join("default"))?.sync_all()?;
	sync_dir(&log)?;
	Ok(Box::new(RawFile {
		dir: dir.join("raw"),
		next_seq: 0,
		staged: None,
	}))
}

fn sync_dir(dir: &Path) -> Result<()> {
	File::open(dir)?.sync_all()?;
	Ok(())
}

impl Engine for RawFile {
	fn name(&self) -> &'static str {
		"raw-file"
	}

	fn durability(&self) -> &'static str {
		"file sync_all + parent-directory sync_all per put"
	}

	fn supports(&self, op: Op) -> bool {
		// There is nothing to recover: a directory full of fsync'd files has no
		// journal to replay and no root to select.
		matches!(op, Op::Batch | Op::Append | Op::Staged | Op::Deferred)
	}

	fn prepare(&mut self, prefixes: &[String]) -> Result<()> {
		for prefix in prefixes {
			fs::create_dir_all(self.dir.join("kv").join(prefix))?;
		}
		Ok(())
	}

	fn put(&mut self, key: &str, value: &[u8]) -> Result<()> {
		let path = self.dir.join("kv").join(key);
		let mut file = File::create(&path)?;
		file.write_all(value)?;
		file.sync_all()?;
		sync_dir(path.parent().expect("a key always has a parent"))
	}

	fn put_batch(&mut self, items: &[(String, Vec<u8>)]) -> Result<()> {
		// "Batched" here can only mean "sync the directory once at the end",
		// because a file's own fsync is not deferrable. That is a real
		// difference and it is why the row is labelled as batched.
		let root = self.dir.join("kv");
		for (key, value) in items {
			let mut file = File::create(root.join(key))?;
			file.write_all(value)?;
			file.sync_all()?;
		}
		sync_dir(&root)
	}

	fn get(&self, key: &str) -> Result<Vec<u8>> {
		Ok(fs::read(self.dir.join("kv").join(key))?)
	}

	fn keys(&self) -> Result<Vec<String>> {
		let mut keys = Vec::new();
		let mut stack = vec![(self.dir.join("kv"), String::new())];
		while let Some((dir, prefix)) = stack.pop() {
			for entry in fs::read_dir(&dir)? {
				let entry = entry?;
				let name = entry.file_name();
				let Some(name) = name.to_str() else { continue };
				let key = if prefix.is_empty() {
					name.to_string()
				} else {
					format!("{prefix}/{name}")
				};
				if entry.file_type()?.is_dir() {
					stack.push((entry.path(), key));
				} else {
					keys.push(key);
				}
			}
		}
		keys.sort();
		Ok(keys)
	}

	fn append(&mut self, name: &str, data: &[u8]) -> Result<()> {
		let path = self.dir.join("log").join(name);
		let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
		file.write_all(data)?;
		file.sync_all()?;
		// The sequence counter is bookkeeping, not storage: nothing reads it.
		self.next_seq += 1;
		Ok(())
	}

	/// The control's way to make a batch durable is to make it one append: one
	/// `write_all` per row into the open handle, then a single `sync_all`.
	fn append_batch(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let path = self.dir.join("log").join(name);
		let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
		for row in rows {
			file.write_all(row)?;
		}
		file.sync_all()?;
		Ok(())
	}

	/// A held append handle: `write_all` per row into the kernel's page cache,
	/// and a power cut at this instant may lose any of it. The `sync_all` that
	/// makes it durable is [`seal_staged`](Self::seal_staged).
	fn staged_append(&mut self, name: &str, rows: &[&[u8]]) -> Result<()> {
		let wrong_stream = matches!(&self.staged, Some((open, _)) if open != name);
		if self.staged.is_none() || wrong_stream {
			let path = self.dir.join("log").join(name);
			let file = OpenOptions::new().create(true).append(true).open(&path)?;
			self.staged = Some((name.to_string(), file));
		}
		let (_, file) = self.staged.as_mut().expect("just opened");
		for row in rows {
			file.write_all(row)?;
		}
		Ok(())
	}

	fn seal_staged(&mut self) -> Result<()> {
		if let Some((_, file)) = self.staged.take() {
			file.sync_all()?;
		}
		Ok(())
	}

	/// The file *is* the record here: its bytes are the data, so a seal has
	/// nothing to publish beyond writing them, and they are already in the
	/// page cache. Nothing is done, and the held handle stays open so the next
	/// batch continues the same stream.
	fn seal_staged_deferred(&mut self) -> Result<()> {
		Ok(())
	}

	/// The `sync_all` a whole group of deferred seals was waiting for.
	fn sync_staged(&mut self) -> Result<()> {
		if let Some((_, file)) = &self.staged {
			file.sync_all()?;
		}
		Ok(())
	}

	fn read_log(&self, name: &str) -> Result<Vec<u8>> {
		Ok(fs::read(self.dir.join("log").join(name))?)
	}

	fn reopen(&mut self) -> Result<()> {
		Err(Error::engine("raw-file", "a directory tree has no recovery step"))
	}
}

// endregion: --- Raw file
