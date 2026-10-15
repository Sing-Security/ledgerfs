# Benchmarks

LedgerFS against three other ways of making a write durable, on storage that can
lose data.

The harness is [`benchmarks/`](benchmarks/) — a plain binary, not a criterion
suite, because what it measures is a comparison and criterion measures a
regression. Run it yourself:

```sh
cargo run --release -p ledgerfs-benchmarks -- run
cargo run --release -p ledgerfs-benchmarks -- render   # updates this file
```

Everything between the two markers below is regenerated from the newest file in
`benchmarks/results/`, which is checked in. That is the point of the arrangement:
the tables in this document cannot drift from a run, and the run is a file you
can inspect.

## The engines

| engine | what it is |
|---|---|
| `ledgerfs` | `ledgerfs-host`'s `Store`, on a `FileBlockDevice` |
| `sqlite-wal` | SQLite via `rusqlite`, WAL journal |
| `redb` | a pure-Rust embedded key/value store |
| `raw-file` | a plain file per key, `sync_all` on the file and its directory |

`raw-file` is the control. It is what "durable" costs on this machine with no
engine at all, and every row should be read against it.

## Like for like

- **One write per transaction, for every engine.** Where an engine has a batch
  API, one row measures it and is marked as not comparable, because its
  per-operation figure is 1/N of a commit. The gap between the two rows is the
  argument for batching and is reported for that reason.
- **Durability is stated per engine, not assumed.** The generated section
  repeats each engine's own setting. `sqlite-wal` runs `synchronous=FULL` with
  `journal_mode=WAL`; `redb` sets `Durability::Immediate`; `raw-file` syncs the
  file *and* the directory, because syncing only the file does not make a new
  name durable.
- **Nothing is tuned.** No engine gets a larger page size, a bigger cache, or a
  warmer one.
- **The medium is checked, not trusted.** The harness refuses to start unless
  the target filesystem's writes reach a device. On tmpfs every barrier returns
  immediately, so a durability benchmark there produces small, plausible,
  meaningless numbers — and nothing in the output would say so. The guard is
  the whole reason the harness has a `mod medium`; see the module's docs.
- **Each repetition does the same amount of work**, and each writes different
  keys, so nothing is overwritten. LedgerFS frees nothing, so an overwrite would
  make repetition 2 unequal to repetition 1 for a reason that has nothing to do
  with what is being measured.
- **A gap is a gap.** An engine that cannot express a workload is recorded as
  not measured, with the reason, rather than as a zero.

## How to read the LedgerFS numbers

Four things about the implementation show up in these tables, and a reader who
does not know them will draw the wrong conclusion from at least two of the rows.

1. **A durable insert here is one commit, and a commit is two barriers.** Every
   row stores 256 bytes, which fits one object, so `write_new_file` places the
   chunk, its extent node, the file inode and the directory entry in a single
   transaction. That transaction costs two synchronization points on the medium
   (`FORMAT.md` §15.1), and it is those two that the write rows mostly measure:
   on this device a barrier that follows new data costs 3–5 ms, and the figure
   moves between runs by more than a factor of two. That is why the control row
   exists — see *The framing* below.
2. **LedgerFS's commit never touches an existing object.** It writes a new
   allocator snapshot and four superblock copies, and the CoW path re-places
   every object on the path it touched. The result is the `volume KiB/op`
   column below: 108.8 KiB per 256-byte insert, and it does not shrink with the
   size of the write. This is the number to size a volume against, and it is
   why `KNOWN_ISSUES.md` §1 says a volume should be treated as append-only.
3. **These reads are warm, and this list exists to say so.** Every read and scan
   row is measured in a process that wrote the data first, so the id→block map
   the host crate fills as it places objects answers with a map hit. A volume
   read by a process that has not just written it and has no sidecar beside it
   pays a linear scan of the volume for the first lookup of each object instead
   (`KNOWN_ISSUES.md` §4, `host/src/hints.rs`). That case is real and it is not
   in these tables; the rows below are the steady state, not the cold one.
4. **The unit of durability is a whole-filesystem commit.** It is the wrong unit
   for a batched analytical load, and the batched row shows by how much —
   LedgerFS cannot express it at all. What it can claim is different, not
   better: the commit is the durability point, there is no write-ahead log to
   replay, and there is no separate engine, log file, or tuning parameter.

## The framing

LedgerFS is last on every row here. Two of those rows are now close, and the
reason is visible in the table itself.

**A durable 256-byte insert is 10.1 ms.** SQLite is 4.62 ms and redb 4.05 ms, so
LedgerFS is about 2.2× where an earlier revision of this document recorded about
8×. The row to compare it against is `raw-file` at 6.99 ms: no engine at all, a
file `fsync` and a directory `fsync` per put — the same *shape* of cost, two
synchronizations following new data, which is exactly what a LedgerFS commit
issues (`FORMAT.md` §15.1). LedgerFS pays 1.44× a raw two-sync put for a whole
filesystem commit: an allocator snapshot, four superblock copies, and a CoW
rewrite of every object on the path.

**The rest of the gap to SQLite is one synchronization.** SQLite in WAL mode and
redb each synchronize once per commit — their `durability` strings above say so
— and LedgerFS synchronizes twice. The first of the two is what lets a mount
adopt a root without an intent log to consult: the objects are on media before
the superblock that names them. The second is the commit point. Dropping the
first is not a local optimisation; it moves the cost of an interrupted commit
into the mount path, which is a different recovery story and a different format
argument (`FORMAT.md` §18). It is not in this release.

**The read rows moved less, and that is reported rather than explained away.**
`read_point_256b` is 34 µs against 30 µs in the previous run, on a machine that
was slower for every engine in the same window — SQLite went 4.2 → 6.1 µs on
that row. The read path did get cheaper on the device: a lookup used to read
each candidate block twice, once to check it held the wanted object and once to
fetch the payload, and that is gone — 11 block reads behind a point read in a
root directory, now 5, with the count pinned by a test
(`host/tests/readpath.rs`). It does not show up here because a warm read of 256
bytes is not spending its time on device reads. The scan rows are unchanged for
the same reason.

What LedgerFS has instead is a narrower guarantee: storage whose commit is the
durability point, with no log to replay and no separate engine, in a library that
owns a byte string. Whether that is worth 2.2× is a question about your
application, not about these numbers — a system that needs crash-atomic storage
without a second component to operate is buying something the other three rows
are not selling. A system that needs throughput should use one of them.

Where a number was not measured, the table says so. There is no row here that
was inferred.

<!-- BEGIN GENERATED: benchmarks -->
<!-- This section is generated by `ledgerfs-benchmarks render`.
     Edit the prose around it, not the tables: they are overwritten. -->

## The machine this was measured on

- **UTC** — 2026-10-14T19:52:56Z
- **CPU** — Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz (12 logical)
- **Memory** — 15.5 GiB
- **Kernel** — 7.0.11-1-cachyos
- **Filesystem** — btrfs
- **Device** — /dev/mapper/luks-REDACTED (SAMSUNG MZVLB512HAJQ-00000) — **dm-crypt, so an encryption cost is included**
- **Free space** — 108.4 GiB of 474.9 GiB
- **Workload sizes** — insert_n=200, log_appends=64, repeats=3

Each workload is run for every engine `repeats` times; the figure below is the median of the per-repetition means, and each repetition does the same amount of work. Nothing is tuned: every engine is asked for one write, durable before the call returns.

### What each engine was told

| engine | durability setting |
|---|---|
| `ledgerfs` | one superblock commit per put (the value fits one object); objects + allocator snapshot, barrier, superblock, barrier |
| `raw-file` | file sync_all + parent-directory sync_all per put |
| `redb` | Durability::Immediate on every write transaction |
| `sqlite-wal` | journal_mode=WAL, synchronous=FULL; one transaction per put |

### `insert_durable_256b`

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `redb` | 4.05 ms | 247 | 0.06 | — |
| `sqlite-wal` | 4.62 ms | 216 | 0.05 | — |
| `raw-file` | 6.99 ms | 143 | 0.03 | — |
| `ledgerfs` | 10.09 ms | 99 | 0.02 | 108.8 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `insert_batched_256b`

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `sqlite-wal` † | 20.8 µs | 48,167 | 11.76 |
| `redb` † | 24.8 µs | 40,340 | 9.85 |
| `raw-file` † | 3.82 ms | 262 | 0.06 |

† not a like-for-like row:
- `sqlite-wal` — 200 inserts in one transaction; the per-insert figure is 1/200 of a commit.
- `redb` — 200 inserts in one transaction; the per-insert figure is 1/200 of a commit.
- `raw-file` — 200 inserts in one transaction; the per-insert figure is 1/200 of a commit.

Not measured:
- `ledgerfs` — no caller-visible batch transaction; every put is its own commit

### `read_point_256b`

200 reads over a store populated outside the timed section.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 870 ns | 1,150,000 | 280.76 |
| `raw-file` | 2.6 µs | 388,221 | 94.78 |
| `sqlite-wal` | 6.1 µs | 163,570 | 39.93 |
| `ledgerfs` | 34.4 µs | 29,037 | 7.09 |

### `append_log_64k`

64 appends of 64 KiB, one fsync each.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` | 3.74 ms | 267 | 16.70 | — |
| `redb` | 4.42 ms | 226 | 14.15 | — |
| `sqlite-wal` | 5.18 ms | 193 | 12.06 | — |
| `ledgerfs` | 7.85 ms | 127 | 7.97 | 281.8 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `scan_10`

10 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 321 ns | 3,117,207 | 761.04 |
| `sqlite-wal` | 841 ns | 1,188,637 | 290.19 |
| `raw-file` | 1.3 µs | 757,576 | 184.96 |
| `ledgerfs` | 5.5 µs | 181,334 | 44.27 |

### `scan_100`

100 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `sqlite-wal` | 177 ns | 5,654,829 | 1380.57 |
| `redb` | 255 ns | 3,923,107 | 957.79 |
| `raw-file` | 300 ns | 3,328,562 | 812.64 |
| `ledgerfs` | 2.7 µs | 364,781 | 89.06 |

### `scan_1000`

1000 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 117 ns | 8,573,462 | 2093.13 |
| `sqlite-wal` | 125 ns | 7,977,344 | 1947.59 |
| `raw-file` | 268 ns | 3,726,477 | 909.78 |
| `ledgerfs` | 2.1 µs | 483,025 | 117.93 |

### `reopen_and_read`

reopen plus one read, so lazy recovery is inside the timed section.

| engine | median per operation | operations/s |
|---|---:|---:|
| `sqlite-wal` | 183.9 µs | 5,439 |
| `ledgerfs` | 438.9 µs | 2,279 |
| `redb` | 32.12 ms | 31 |

Not measured:
- `raw-file` — no recovery step: there is no journal to replay and no root to select

<!-- END GENERATED: benchmarks -->

## What is not measured

- **Concurrency.** One writer, one reader, no contention. A concurrent workload
  is not exercised, and the single-writer rule (`FORMAT.md` §12.1) is a design
  constraint rather than a benchmark condition.
- **Larger-than-memory volumes.** Everything here fits in this machine's page
  cache once warmed. A volume larger than RAM would be dominated by the scan
  fallback in point 3, and by a wider margin than these numbers show.
- **Crash injection.** The crash-consistency claim is tested in `host/tests/`
  and `kernel/tests/`, not benchmarked. There is no power-loss row here because
  a benchmark cannot produce one.
- **Other hardware.** The numbers below are from one machine, whose CPU, device,
  filesystem and encryption state are recorded in the generated header. Run the
  harness on yours before quoting any of them.
