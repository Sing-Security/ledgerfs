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
  argument for batching and is reported for that reason. The staged and
  deferred rows apply the same rule to a durability point that is not the
  batch: their figure is 1/K of a commit, and the reason is printed under the
  table.
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
   column below: 36.3 KiB per 256-byte insert, and it does not shrink with the
   size of the write. This is the number to size a volume against, and it is
   why `KNOWN_ISSUES.md` §1 says a volume should be treated as append-only.
   *Correction:* every `volume KiB/op` recorded before 2026-10-23 was inflated
   by `repeats` — three times, for every run on file — because the free-space
   delta spanned all repetitions but was divided by one repetition's operation
   count. The 2026-10-23 run is the correction.
3. **These reads are warm, and this list exists to say so.** Every read and scan
   row is measured in a process that wrote the data first, so the id→block map
   the host crate fills as it places objects answers with a map hit. A volume
   read by a process that has not just written it and has no sidecar beside it
   pays a linear scan of the volume for the first lookup of each object instead
   (`KNOWN_ISSUES.md` §4, `host/src/hints.rs`). That case is real and it is not
   in these tables; the rows below are the steady state, not the cold one.
4. **The unit of durability is a whole-filesystem commit, and batching moves it
   rather than removing it.** The format cannot make two hundred writes
   durable for the price of one; it can let the application say when the one
   commit happens. `insert_batched_256b` is run through a staging session —
   two hundred inserts, one seal — and `staged_append_960b` seals every tenth
   batch. What LedgerFS can claim is different, not better: the commit is the
   durability point, there is no write-ahead log to replay, and there is no
   separate engine, log file, or tuning parameter to get wrong.

## The framing

LedgerFS is last on most rows here. Two of them are closer than they look, and
the reason is visible in the table itself.

**A durable 256-byte insert is 9.52 ms.** SQLite is 3.70 ms and redb 3.68 ms.
The row to compare it against is `raw-file` at 6.95 ms: no engine at all, a
file `fsync` and a directory `fsync` per put — the same *shape* of cost, two
synchronizations following new data, which is exactly what a LedgerFS commit
issues (`FORMAT.md` §15.1). LedgerFS pays 1.37× a raw two-sync put for a whole
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
28.0 µs against redb's 955 ns. A lookup behind it is five block reads, a count
pinned by a test (`host/tests/readpath.rs`); the rest of the gap is the
object's BLAKE3 hash, verified before the bytes come back, because a read that
cannot say whether it succeeded is not the guarantee this format sells. The
databases trust their pages; LedgerFS checks them. What that check is worth on
the workload it was built for is the next section.

What LedgerFS has instead is a narrower guarantee: storage whose commit is the
durability point, with no log to replay and no separate engine, in a library that
owns a byte string. Whether that is worth 2.6× is a question about your
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

**The batch is where the gap narrows.** LedgerFS is 16.40 ms a batch against
SQLite's 9.24 ms — 1.77×, against 2.57× on the single-insert row above — and
1.7× the raw control, which also stopped being cheap the moment the batch made
its two syncs unavoidable. Once the batch is the durability point, every engine
pays roughly one synchronization and the residual difference is bookkeeping;
LedgerFS's bookkeeping is a whole-filesystem commit, and this row prices it.

**The volume cost is the commit's, not the payload's.** A batch costs 35.9 KiB
of backing store; a 256-byte insert costs 36.3. The commit rewrites the
allocator snapshot and the path it touched, so its cost is fixed whether the
payload carries twenty records or one — which means sizing a capture volume
means sizing *commits*: roughly `batches × 36 KiB`, whatever the records hold.
This is the number behind `KNOWN_ISSUES.md` §1's advice to treat a volume as
append-only with a known lifetime, and it is why that entry describes the
intended shape rather than a workaround.

**Read back, the verification is already in the number.** Per batch read:
raw-file 232 ns — the page cache handing back bytes it already has — redb
1.6 µs, sqlite-wal 3.0 µs, ledgerfs 5.3 µs. The ledgerfs figure includes the
BLAKE3 verification of every object it returns; on the other engines that check
is work the caller has not done yet. Over the whole 480 KiB stream the verified
read-back lands within about 1.8× of SQLite's unverified one — the price of
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

## The staging tier

Every workload so far treats the commit as the write. For capture traffic that
is the wrong lever: records arrive continuously, and making each arrival pay
two barriers puts the medium in the application's hot path for data the
application has not yet decided to make durable. The staging tier separates
*accepted* from *durable* and makes the second an explicit act.

Every engine here has such a tier under a different name, and
`staged_append_960b` asks each for its own idiom: SQLite holds INSERTs inside
an open transaction and issues one `COMMIT` at the seal; redb holds one write
transaction open across batches and commits it at the seal; the raw control
writes buffered and calls `sync_all` at the seal; LedgerFS runs a staging
session — `begin_staging`, `staged_append` per batch, `seal_staged`.

**The tier changes the order of the number.** Per batch: raw-file 897 µs,
sqlite-wal 1.04 ms, redb 1.71 ms, ledgerfs 1.99 ms — against 16.40 ms for a
committed batch, an eight-fold difference for the same bytes. Every row is
marked † because only every tenth batch carries a durability point. The seal
cost sits inside the timed section, so the row is the true average *including*
seals, not the price of a write that was never made durable.
`--staged-seal-every` moves the trade-off; the model is a commit's cost divided
by the seal interval, plus the in-memory work per batch.

**What LedgerFS stages is not a log.** Each staged operation builds the same
objects a normal write would — chunk, extent node, inode, directory pair —
through the same builders, against an in-memory overlay over the committed
volume. Sealing is therefore not a replay: it is an ordinary commit of objects
the session has already built, with the same two barriers, allocator snapshot,
and four superblock copies as every other row here. There is no second recovery
path, no new on-disk structure, and no format change; a mount that never sees a
staging session cannot tell one was used.

**Two tiers on read, chosen per read.** While a session is open, the handle
that owns it resolves its own staged writes — `read_file`, `read_at`, `stat`
and `read_dir` see them with zero device reads — and `read_file_committed`
never does: it is the superblock's generation, exactly the read the previous
sections priced. An application that needs the current value reads staged; one
that needs the durable value reads committed; any other handle on the volume
sees only committed, by construction. After the seal the two agree, and the
staged copy is gone.

**The volume column is the same story as a rate.** A staged batch costs 7.3 KiB
of backing store over the run, which is 73 KiB per seal — about two ordinary
commits. The seal is the only thing that costs volume, and each one re-places
an extent node that has grown since the last, so a long stream pays for its
history at every seal. The interval therefore trades commit frequency against
per-seal growth, and the extent node's ceiling bounds how far one stream can go
whichever way the trade is set.

**The in-session cost is real too.** Each staged append rebuilds the current
extent node in memory, so the RAM work per batch grows with the stream's length
even though nothing reaches the device; that is the 1 ms a batch standing
between ledgerfs and raw-file on the row. Sealing does not shrink an extent
node — the file keeps growing — so the seal interval does not relieve it.
Splitting one growing stream across files is what bounds it, and a stream long
enough to reach its extent node's ceiling has to split that way regardless.

**The guarantee is unchanged at the seal.** Before it, staged records exist
only in the writing process's memory; a crash loses exactly those and nothing
else, and the volume still mounts to the last commit. During it, `FORMAT.md`
§15.2's old-or-new rule holds. After `seal_staged` returns, the records carry
the same guarantee as every other row in this document — atomic, BLAKE3-
verified, recovered by a plain mount with no new code. That is what *the commit
is the ultimate point of truth* means here: staging does not weaken the commit,
it hands the application the choice of when to spend one.

**One step further: the deferred seal.** The row above still pays two barriers
at every seal, because a seal is a commit. An application that seals regularly
but can tolerate losing the last group of seals may move the second barrier off
the seal: `seal_staged_deferred` does everything `seal_staged` does — builds
one transaction, writes its objects and the allocator snapshot, and flushes
them — and stops before the superblock write that publishes the new root.
`sync()` writes that superblock and flushes it once, covering every seal since
the last sync, so *n* seals plus one sync cost *n* + 1 barriers where *n*
ordinary seals cost 2*n*.

`deferred_append_960b` asks each engine for the same relaxation in its own
idiom: LedgerFS `seal_staged_deferred` + `sync`; SQLite a commit under
`synchronous=NORMAL` with the group ended by `wal_checkpoint(FULL)`; redb a
`Durability::None` commit with an empty `Durability::Immediate` transaction as
the sync; the raw control writes into its held file and calls `sync_all` once
per group. The promises are not identical in what the seal itself does —
LedgerFS flushes its objects at the seal, SQLite leaves them in the WAL, redb
leaves them mapped, the raw control leaves them in the page cache — so the row
is read as "the cost of a batch when the group is the durability point",
exactly as the staged row is read.

**Measured, at the defaults.** Per batch: raw-file 123 µs, redb 130 µs,
sqlite-wal 473 µs, ledgerfs 1.12 ms. The raw control and redb are near their
in-session floor because their seals do almost nothing until the group sync;
SQLite pays for writing WAL frames at each deferred commit; LedgerFS pays for
flushing its objects at each seal, which is the price of the crash behaviour
below — its group is already on the medium when the seal returns. Against the
row above it, ledgerfs 1.99 → 1.12 ms a batch, and that difference is barrier
count, not in-session work: the extent-node rebuild per staged append is
unchanged by the tier.

What a crash costs in the deferred tier is bounded and stated: the previous
root stays live, so the group since the last `sync` is lost whole, never a
blend, and the allocator snapshot published ahead of it may mark that group's
blocks as used while no live root names them — leaked space on a crash-
recovered volume, never a wrong answer. That is a weaker promise than
`seal_staged` makes, which is why it is a separate call and a separate row, not
a default.

The tier is additive. `write_new_file`, `append` and `write_file` still each
produce one commit; `insert_batched_256b`'s LedgerFS row is the same machinery
at its extreme — two hundred inserts, one seal, 111 µs per insert. There is no
read-back row for the staged tier: the read would race the seal boundary, and a
row whose meaning moves with the seal interval would not be a comparison. That
behaviour is pinned by `host/tests/staging.rs` and demonstrated by
`cargo run --example staging_capture` instead.

<!-- BEGIN GENERATED: benchmarks -->
<!-- This section is generated by `ledgerfs-benchmarks render`.
     Edit the prose around it, not the tables: they are overwritten. -->

## The machine this was measured on

- **UTC** — 2026-10-23T01:13:10Z
- **CPU** — Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz (12 logical)
- **Memory** — 15.5 GiB
- **Kernel** — 7.0.11-1-cachyos
- **Filesystem** — btrfs
- **Device** — /dev/mapper/luks-redacted (SAMSUNG MZVLB512HAJQ-00000) — **dm-crypt, so an encryption cost is included**
- **Free space** — 14.6 GiB of 474.9 GiB
- **Workload sizes** — insert_n=200, log_appends=64, append_batches=512, staged_seal_every=10, staged_sync_every=100, repeats=3

Each workload is run for every engine `repeats` times; the figure below is the median of the per-repetition means, and each repetition does the same amount of work. Nothing is tuned. Every engine is asked for one write, durable before the call returns, except where a row is marked †: those state their own durability point instead.

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
| `redb` | 3.68 ms | 272 | 0.07 | — |
| `sqlite-wal` | 3.70 ms | 270 | 0.07 | — |
| `raw-file` | 6.95 ms | 144 | 0.04 | — |
| `ledgerfs` | 9.52 ms | 105 | 0.03 | 36.3 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `insert_batched_256b`

200 inserts in one transaction; the per-insert figure is 1/200 of a commit.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `sqlite-wal` † | 19.6 µs | 51,051 | 12.46 | — |
| `redb` † | 20.9 µs | 47,890 | 11.69 | — |
| `ledgerfs` † | 111.0 µs | 9,012 | 2.20 | 12.2 |
| `raw-file` † | 3.55 ms | 282 | 0.07 | — |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

† not a like-for-like row:
- `sqlite-wal`, `redb`, `ledgerfs`, `raw-file` — 200 inserts in one transaction; the per-insert figure is 1/200 of a commit.

### `read_point_256b`

200 reads over a store populated outside the timed section.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 955 ns | 1,046,616 | 255.52 |
| `raw-file` | 2.5 µs | 407,818 | 99.56 |
| `sqlite-wal` | 4.2 µs | 236,133 | 57.65 |
| `ledgerfs` | 28.0 µs | 35,661 | 8.71 |

### `append_log_64k`

64 appends of 64 KiB, one fsync each.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` | 3.91 ms | 256 | 15.97 | — |
| `sqlite-wal` | 4.15 ms | 241 | 15.07 | — |
| `redb` | 4.56 ms | 219 | 13.70 | — |
| `ledgerfs` | 8.06 ms | 124 | 7.75 | 93.9 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `append_batched_960b`

512 durable batches of 20 × 48 B records; the batch is the durability point.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `sqlite-wal` | 9.24 ms | 108 | 0.10 | — |
| `raw-file` | 9.59 ms | 104 | 0.10 | — |
| `redb` | 10.22 ms | 98 | 0.09 | — |
| `ledgerfs` | 16.40 ms | 61 | 0.06 | 35.9 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

### `staged_append_960b`

512 batches of 20 × 48 B records, sealed every 10; the durability point is the seal, so the per-batch figure is 1/10 of a commit.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` † | 897.0 µs | 1,115 | 1.02 | — |
| `sqlite-wal` † | 1.04 ms | 962 | 0.88 | — |
| `redb` † | 1.71 ms | 586 | 0.54 | — |
| `ledgerfs` † | 1.99 ms | 504 | 0.46 | 7.3 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

† not a like-for-like row:
- `raw-file`, `sqlite-wal`, `redb`, `ledgerfs` — 512 batches of 20 × 48 B records, sealed every 10; the durability point is the seal, so the per-batch figure is 1/10 of a commit.

### `deferred_append_960b`

512 batches of 20 × 48 B records, sealed every 10 and synced every 100; each seal writes its group, one sync publishes every group since the last one.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` † | 123.3 µs | 8,113 | 7.43 | — |
| `redb` † | 130.0 µs | 7,695 | 7.05 | — |
| `sqlite-wal` † | 473.1 µs | 2,114 | 1.94 | — |
| `ledgerfs` † | 1.12 ms | 894 | 0.82 | 7.3 |

`volume KiB/op` is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `—`; a `—` is not a zero.

† not a like-for-like row:
- `raw-file`, `redb`, `sqlite-wal`, `ledgerfs` — 512 batches of 20 × 48 B records, sealed every 10 and synced every 100; each seal writes its group, one sync publishes every group since the last one.

### `read_back_960b`

one whole-stream read per repetition (512 batches, every byte compared).

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `raw-file` | 232 ns | 4,319,363 | 3954.49 |
| `redb` | 1.6 µs | 631,490 | 578.15 |
| `sqlite-wal` | 3.0 µs | 331,209 | 303.23 |
| `ledgerfs` | 5.3 µs | 189,193 | 173.21 |

### `scan_10`

10 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 228 ns | 4,382,121 | 1069.85 |
| `sqlite-wal` | 619 ns | 1,616,031 | 394.54 |
| `raw-file` | 965 ns | 1,036,699 | 253.10 |
| `ledgerfs` | 3.3 µs | 304,915 | 74.44 |

### `scan_100`

100 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 107 ns | 9,315,324 | 2274.25 |
| `sqlite-wal` | 157 ns | 6,350,416 | 1550.39 |
| `raw-file` | 280 ns | 3,569,007 | 871.34 |
| `ledgerfs` | 1.8 µs | 559,328 | 136.55 |

### `scan_1000`

1000 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 99 ns | 10,149,193 | 2477.83 |
| `sqlite-wal` | 112 ns | 8,927,774 | 2179.63 |
| `raw-file` | 299 ns | 3,349,074 | 817.65 |
| `ledgerfs` | 2.0 µs | 493,413 | 120.46 |

### `reopen_and_read`

reopen plus one read, so lazy recovery is inside the timed section.

| engine | median per operation | operations/s |
|---|---:|---:|
| `sqlite-wal` | 213.9 µs | 4,674 |
| `ledgerfs` | 432.3 µs | 2,313 |
| `redb` | 34.67 ms | 29 |

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
