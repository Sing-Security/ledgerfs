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

### Per deployment

The comparison engines are cargo features named after the engine — `sqlite`,
`redb`, both on by default. A build with `--no-default-features --features
sqlite` contains no redb code at all: not compiled, not linked, not in the
binary. A deployment compares itself against what it actually deploys, and it
does not carry the rest. The workloads never name an engine — they see the
harness's `Engine` trait and nothing else — so a feature change is a roster
change and nothing downstream of it moves. `ledgerfs` and `raw-file` are always
present: one is the subject, the other the control.

The published crate follows the same rule one level down. An application that
embeds LedgerFS compiles `ledgerfs-host` and its four-method `BlockDevice`
seam; the block layer, the ring-0 path and everything OS-shaped stay in the OS
tree, not in the application's dependency graph.

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

LedgerFS is last on most rows here. Two of them are closer than they look, and
the reason is visible in the table itself.

**A durable 256-byte insert is 7.90 ms.** SQLite is 3.72 ms and redb 3.93 ms.
The row to compare it against is `raw-file` at 6.92 ms: no engine at all, a
file `fsync` and a directory `fsync` per put — the same *shape* of cost, two
synchronizations following new data, which is exactly what a LedgerFS commit
issues (`FORMAT.md` §15.1). LedgerFS pays 1.14× a raw two-sync put for a whole
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

**The read rows are the guarantee showing its price.** `read_point_256b` is
26.2 µs against redb's 917 ns. A lookup behind it is five block reads, a count
pinned by a test (`host/tests/readpath.rs`); the rest of the gap is the
object's BLAKE3 hash, verified before the bytes come back, because a read that
cannot say whether it succeeded is not the guarantee this format sells. The
databases trust their pages; LedgerFS checks them. What that check is worth on
the workload it was built for is the next section.

What LedgerFS has instead is a narrower guarantee: storage whose commit is the
durability point, with no log to replay and no separate engine, in a library that
owns a byte string. Whether that is worth 2.1× is a question about your
application, not about these numbers — a system that needs crash-atomic storage
without a second component to operate is buying something the other three rows
are not selling. A system that needs throughput should use one of them.

Where a number was not measured, the table says so. There is no row here that
was inferred.

## Batched records, read back whole

The insert rows measure the smallest possible durability unit: one value, one
commit. Two workloads measure the unit most write-heavy applications actually
use, and they exist because the answer there is different in kind, not only in
degree.

`append_batched_960b` writes fixed-size records — 48 bytes, one sequence
number plus payload — in batches of twenty, with **one durability point per
batch**. It is the write pattern of an application that accumulates records and
flushes them as they arrive. Every engine gets the same rows and the same batch
boundary and does the batch in its own idiom: one transaction for SQLite, one
write transaction for redb, one `write_all` and one `sync_all` for the raw
control, and for LedgerFS one `append` — which is one commit, because on this
format the append *is* the transaction. `read_back_960b` then reads a whole
stream back and compares every byte against what was written.

**The batch is where the gap narrows.** LedgerFS is 16.20 ms a batch against
SQLite's 9.25 ms — 1.75×, against 2.12× on the single-insert row above — and
1.7× the raw control, which also stopped being cheap the moment the batch made
its two syncs unavoidable. Once the batch is the durability point, every engine
pays roughly one synchronization and the residual difference is bookkeeping;
LedgerFS's bookkeeping is a whole-filesystem commit, and this row prices it.

**The volume cost is the commit's, not the payload's.** A batch costs 107.6 KiB
of backing store; a 256-byte insert costs 108.8. The commit rewrites the
allocator snapshot and the path it touched, so its cost is fixed whether the
payload carries twenty records or one — which means sizing a capture volume
means sizing *commits*: roughly `batches × 108 KiB`, whatever the records hold.
This is the number behind `KNOWN_ISSUES.md` §1's advice to treat a volume as
append-only with a known lifetime, and it is why that entry describes the
intended shape rather than a workaround.

**Read back, the verification is already in the number.** Per batch read:
raw-file 113 ns — the page cache handing back bytes it already has — redb
1.6 µs, sqlite-wal 2.7 µs, ledgerfs 3.6 µs. The ledgerfs figure includes the
BLAKE3 verification of every object it returns; on the other engines that check
is work the caller has not done yet. Over the whole 480 KiB stream the verified
read-back lands within about 1.3× of SQLite's unverified one — the price of
*knowing* the recording is intact rather than hoping.

### What kind of store this is

Put the rows together and the shape is specific. This is not a row store: there
is no index, no in-place update, no query, and the volume cost per commit means
there cannot be one without becoming a different format. What it is: records
that land once, are never rewritten, live for a lifetime the application
chooses, and come back **verified** — the capture tier that sits on the near
side of a database, not a replacement for one.

The division of labour follows. Capture into this filesystem when the record
has to survive a power cut *and survive provably* — that is what the read-back
row prices and what the commit buys. Export to SQLite, or to whatever else,
when the workload becomes a query: an export is a plain sequential read, and
the byte-for-byte comparison in `read_back_960b` is the same operation the
exporter performs. The comparison engines are per-deployment cargo features
([above](#per-deployment)) precisely because of this split — the deployment
that captures and exports to SQLite builds SQLite and LedgerFS and nothing
else.

<!-- BEGIN GENERATED: benchmarks -->
<!-- This section is generated by `ledgerfs-benchmarks render`.
     Edit the prose around it, not the tables: they are overwritten. -->

## The machine this was measured on

- **UTC** — 2026-10-22T22:40:04Z
- **CPU** — Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz (12 logical)
- **Memory** — 15.5 GiB
- **Kernel** — 7.0.11-1-cachyos
- **Filesystem** — btrfs
- **Device** — /dev/mapper/luks-redacted (SAMSUNG MZVLB512HAJQ-00000) — **dm-crypt, so an encryption cost is included**
- **Free space** — 15.3 GiB of 474.9 GiB
- **Workload sizes** — insert_n=200, log_appends=64, append_batches=512, repeats=3

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
| `sqlite-wal` | 3.72 ms | 269 | 0.07 | — |
| `redb` | 3.93 ms | 254 | 0.06 | — |
| `raw-file` | 6.92 ms | 144 | 0.04 | — |
| `ledgerfs` | 7.90 ms | 127 | 0.03 | 108.8 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `insert_batched_256b`

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `sqlite-wal` † | 20.6 µs | 48,618 | 11.87 |
| `redb` † | 20.9 µs | 47,821 | 11.68 |
| `raw-file` † | 3.47 ms | 288 | 0.07 |

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
| `redb` | 917 ns | 1,090,102 | 266.14 |
| `raw-file` | 2.1 µs | 474,134 | 115.76 |
| `sqlite-wal` | 4.3 µs | 234,507 | 57.25 |
| `ledgerfs` | 26.2 µs | 38,196 | 9.33 |

### `append_log_64k`

64 appends of 64 KiB, one fsync each.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` | 3.73 ms | 268 | 16.77 | — |
| `sqlite-wal` | 4.19 ms | 239 | 14.92 | — |
| `redb` | 4.27 ms | 234 | 14.64 | — |
| `ledgerfs` | 7.78 ms | 129 | 8.04 | 281.8 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `append_batched_960b`

512 durable batches of 20 × 48 B records; the batch is the durability point.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `sqlite-wal` | 9.25 ms | 108 | 0.10 | — |
| `raw-file` | 9.45 ms | 106 | 0.10 | — |
| `redb` | 10.11 ms | 99 | 0.09 | — |
| `ledgerfs` | 16.20 ms | 62 | 0.06 | 107.6 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `read_back_960b`

one whole-stream read per repetition (512 batches, every byte compared).

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `raw-file` | 111 ns | 9,011,705 | 8250.46 |
| `redb` | 1.4 µs | 735,382 | 673.26 |
| `sqlite-wal` | 2.7 µs | 374,276 | 342.66 |
| `ledgerfs` | 4.1 µs | 241,019 | 220.66 |

### `scan_10`

10 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 250 ns | 4,004,806 | 977.74 |
| `sqlite-wal` | 697 ns | 1,435,338 | 350.42 |
| `raw-file` | 952 ns | 1,050,089 | 256.37 |
| `ledgerfs` | 3.2 µs | 317,390 | 77.49 |

### `scan_100`

100 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 118 ns | 8,445,233 | 2061.82 |
| `sqlite-wal` | 169 ns | 5,921,014 | 1445.56 |
| `raw-file` | 276 ns | 3,627,657 | 885.66 |
| `ledgerfs` | 1.7 µs | 579,227 | 141.41 |

### `scan_1000`

1000 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 97 ns | 10,339,978 | 2524.41 |
| `sqlite-wal` | 113 ns | 8,841,264 | 2158.51 |
| `raw-file` | 276 ns | 3,617,212 | 883.11 |
| `ledgerfs` | 1.9 µs | 527,807 | 128.86 |

### `reopen_and_read`

reopen plus one read, so lazy recovery is inside the timed section.

| engine | median per operation | operations/s |
|---|---:|---:|
| `sqlite-wal` | 175.5 µs | 5,697 |
| `ledgerfs` | 454.4 µs | 2,201 |
| `redb` | 35.00 ms | 29 |

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
