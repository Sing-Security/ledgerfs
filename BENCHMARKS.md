# Measurements

**Sing Security Research Laboratory**

LedgerFS evaluated against three alternative methods of making a write durable,
on storage that can lose data. Every figure in this document was produced by the
harness in [`benchmarks/`](benchmarks/) and can be regenerated from the recorded
run.

## 1. Method

The harness is a standalone binary rather than a criterion suite, because the
question it answers is comparative rather than regressive. It is run twice:

```sh
cargo run --release -p ledgerfs-benchmarks -- run
cargo run --release -p ledgerfs-benchmarks -- render   # updates this file
```

Each run is recorded as a JSON file under `benchmarks/results/`, and the tables
between the markers at the end of this document are regenerated from the newest
recorded run. The tables therefore cannot diverge from a measurement that
exists, and the measurement is a file that can be inspected. Machine
identification in the record is redacted.

### 1.1 Engines

| engine | implementation |
|---|---|
| `ledgerfs` | `ledgerfs-host`'s `Store`, on a `FileBlockDevice` |
| `sqlite-wal` | SQLite through `rusqlite`, WAL journal |
| `redb` | a pure-Rust embedded key/value store |
| `raw-file` | one plain file per key, with `sync_all` on the file and on its directory |

`raw-file` is the control. It measures the cost of a durable write on this
device with no storage engine between the caller and the medium, and every row
is read against it.

### 1.2 Per-deployment builds

The comparison engines are cargo features named after the engine (`sqlite` and
`redb`, both enabled by default). A build with `--no-default-features --features
sqlite` contains no redb code: not compiled, not linked, not present in the
binary. A deployment is compared against the engines it actually links, and it
does not carry the others. The workloads never name an engine; they see the
harness's `Engine` trait, so a change of roster does not change a workload.
`ledgerfs` and `raw-file` are always present, as subject and control.

The published crate follows the same rule at the API boundary. An application
that embeds LedgerFS compiles `ledgerfs-host` and the four-method `BlockDevice`
seam; the block layer, the ring-0 path and the operating-system integrations
remain outside the application's dependency graph.

### 1.3 Comparability

- **One write per transaction, for every engine.** Where an engine exposes a
  batch API, one row measures it and is marked as not comparable, because its
  per-operation figure is 1/N of a commit. The distance between the two rows is
  the effect of batching and is reported for that reason. The staged and
  deferred rows apply the same rule to a durability point that is not the batch:
  their figure is 1/K of a commit, and the table states the interval.
- **Durability is stated per engine, not assumed.** `sqlite-wal` runs with
  `synchronous=FULL` and `journal_mode=WAL`; `redb` uses
  `Durability::Immediate`; `raw-file` synchronizes the file and its directory,
  because synchronizing the file alone does not make a new name durable.
- **No engine is tuned.** No engine receives a larger page, a larger cache, or
  a warmer cache.
- **The medium is verified, not assumed.** The harness refuses to start unless
  the target filesystem's writes reach a device. On tmpfs every barrier returns
  immediately, so the resulting figures would be small, plausible and
  meaningless. The guard is the reason the harness contains a `mod medium`.
- **Each repetition performs the same work and writes different keys.** Nothing
  is overwritten. LedgerFS frees nothing, so an overwrite would make repetition
  2 unequal to repetition 1 for a reason unrelated to the measurement.
- **A gap is recorded as a gap.** An engine that cannot express a workload is
  recorded as not measured, with the reason, rather than as a zero.

## 2. Interpretation of the LedgerFS rows

Four properties of the implementation affect these tables, and a reader who does
not know them will draw an incorrect conclusion from at least two rows.

1. **A durable insert is one commit, and a commit issues two barriers.** Every
   insert row stores 256 bytes, which fits in one object, so `write_new_file`
   places the chunk, its extent node, the file inode and the directory entry in
   one transaction. That transaction contains two synchronization points
   (`FORMAT.md` §15.1), and the write rows largely measure them: on this device
   a barrier that follows new data costs 3 to 5 ms, and the figure varies
   between runs by more than a factor of two. The control row exists for this
   reason; see Section 3.
2. **A commit does not modify an existing object.** It writes a new allocator
   snapshot and four superblock copies, and the copy-on-write path replaces
   every object on the affected path. The consequence is the `volume KiB/op`
   column: 36.3 KiB per 256-byte insert, independent of the payload size. That
   figure governs capacity planning, and it is the basis of the guidance in
   `KNOWN_ISSUES.md` §1 that a volume be treated as append-only.
   *Correction:* every `volume KiB/op` recorded before 2026-10-23 was inflated
   by the repetition count (three times, for every recorded run) because the
   free-space delta spanned all repetitions but was divided by the operation
   count of one. The 2026-10-23 run is the corrected one.
3. **The read measurements are warm.** Every read and scan row is measured in
   the process that wrote the data, so the identifier-to-block map that the host
   crate populates while placing objects answers from memory. A process that
   opens a volume it did not write, with no sidecar beside it, performs a linear
   scan of the volume for the first lookup of each object
   (`KNOWN_ISSUES.md` §4, `host/src/hints.rs`). That case is not represented in
   these tables; the rows describe the steady state, not the cold state.
4. **The unit of durability is a whole-filesystem commit.** The format cannot
   make two hundred writes durable for the price of one; it allows the
   application to decide when the one commit occurs. `insert_batched_256b` runs
   through a staging session (two hundred inserts, one seal), and
   `staged_append_960b` seals every tenth batch. The guarantee LedgerFS offers
   is therefore different rather than better: the commit is the durability
   point, no write-ahead log requires replay, and no separate engine, log file
   or tuning parameter exists to be configured incorrectly.

## 3. Single-record operations

LedgerFS is last on most rows. Two of those rows are closer than the ordering
suggests, and the tables show why.

**A durable 256-byte insert costs 9.18 ms.** SQLite costs 3.79 ms and redb
4.13 ms. The relevant comparison is `raw-file` at 7.10 ms: no storage engine, a
file `fsync` and a directory `fsync` per put, which is the same shape of cost as
a LedgerFS commit, two synchronizations following new data (`FORMAT.md` §15.1).
LedgerFS costs 1.29 times a raw two-sync put for a complete filesystem commit:
an allocator snapshot, four superblock copies, and a copy-on-write replacement
of every object on the path.

**The remainder of the gap to SQLite is one synchronization.** SQLite in WAL
mode and redb synchronize once per commit, and LedgerFS synchronizes twice. The
first synchronization is what allows a mount to adopt a root without an intent
log to consult: the objects are on the medium before the superblock that names
them. The second is the commit point. Removing the first is not a local
optimization; it transfers the cost of an interrupted commit into the mount
path, which is a different recovery argument and a different format
specification (`FORMAT.md` §18). It is not part of this release.

**The read rows price the verification.** `read_point_256b` costs 25.5 µs
against redb's 949 ns. A lookup performs five block reads, a count fixed by a
test (`host/tests/readpath.rs`), and the remainder is the BLAKE3 computation
over the returned object, performed before the bytes are returned. The other
engines trust their pages without checking them. What the check is worth on the
intended workload is the subject of Section 4.

The narrower guarantee is therefore explicit: storage whose commit is the
durability point, with no log to replay and no separate engine, in a library
that owns a byte string. Whether that is worth 2.4 times the cost of SQLite is
a property of the application, not of these measurements. A system that
requires crash-atomic storage without a second component to operate obtains
something the other three rows do not provide. A system that requires maximum
throughput should select one of them.

Where a figure was not measured, the table states so. No row was inferred.

## 4. Batched records

The insert rows measure the smallest durability unit: one value, one commit.
Two workloads measure the unit that write-intensive applications commonly use,
and they are included because the result there differs in kind, not only in
degree.

### 4.1 Workload

`append_batched_960b` writes fixed-size records (48 bytes: one sequence number
and a payload) in batches of twenty, with **one durability point per batch**. It
is the write pattern of an application that accumulates records and flushes them
as they arrive. Every engine receives the same rows and the same batch
boundary: one transaction for SQLite, one write transaction for redb, one
`write_all` and one `sync_all` for the control, and one `append` for LedgerFS,
which is one commit because on this format the append is the transaction.
`read_back_960b` then reads a whole stream back and compares every byte against
what was written.

### 4.2 Results

**The batch is where the gap narrows.** LedgerFS costs 16.87 ms per batch
against SQLite's 9.25 ms, a factor of 1.82, compared with 2.42 on the
single-insert row. It costs 1.86 times the control, which also ceased to be
cheap once the batch made its two synchronizations unavoidable. When the batch
is the durability point, every engine pays approximately one synchronization
and the remaining difference is bookkeeping; the bookkeeping of LedgerFS is a
complete filesystem commit, and this row prices it.

**Volume consumption is a property of the commit, not of the payload.** A batch
consumes 35.9 KiB of backing store and a 256-byte insert consumes 36.3 KiB. The
commit rewrites the allocator snapshot and the path it touched, so its cost is
fixed whether the payload contains twenty records or one. Capacity planning for
a capture volume therefore follows commits: approximately `batches x 36 KiB`,
independent of record size. This figure underlies the guidance in
`KNOWN_ISSUES.md` §1 that a volume be treated as append-only with a known
lifetime.

### 4.3 Read-back verification

Per batch read, the control costs 232 ns (the page cache returning resident
bytes), redb 1.6 µs, sqlite-wal 3.0 µs and ledgerfs 5.3 µs. The LedgerFS figure
includes BLAKE3 verification of every object returned; on the other engines that
check remains outstanding work for the caller. Over the full 480 KiB stream the
verified read-back is within approximately 1.8 times SQLite's unverified read,
which is the cost of establishing that the recording is intact rather than
presuming it.

### 4.4 Resulting characterisation

Taken together, the rows describe a specific class of store. It is not a row
store: it has no index, no in-place update and no query facility, and the volume
cost per commit means such a facility could not be added without producing a
different format. It is a store for records that arrive once, are not rewritten,
live for a lifetime the application determines, and are returned verified. For
that class of data it can occupy the position a database would otherwise hold.

The division of labour follows from the same facts. Records are captured into
this filesystem when they must survive a power loss and must be shown to have
survived it; that property is what the read-back row prices and what the commit
provides. They are exported to SQLite, or to another query engine, when the
workload becomes a query. An export is a sequential read, and the
byte-for-byte comparison in `read_back_960b` is the operation an exporter
performs. The comparison engines are per-deployment features for this reason
(Section 1.2): a deployment that captures records and exports them to SQLite
links SQLite and LedgerFS and nothing else.

## 5. Staging tier

Every workload above treats the commit as the write. For capture traffic that
choice places two barriers in the path of each record, before the application
has decided that the record should be durable. The staging tier separates
accepted writes from durable writes and makes the second an explicit operation.

Every engine in this comparison has an equivalent tier under a different name,
and `staged_append_960b` requests each in its own idiom: SQLite holds inserts
inside an open transaction and issues one `COMMIT` at the seal; redb holds one
write transaction open across batches and commits it at the seal; the control
writes to a buffered file and calls `sync_all` at the seal; LedgerFS runs a
staging session (`begin_staging`, `staged_append` per batch, `seal_staged`).

### 5.1 Results

Per batch: raw-file 869 µs, redb 981 µs, sqlite-wal 1.25 ms and ledgerfs
2.05 ms, against 16.87 ms for a committed batch, a difference of approximately
eight times for the same bytes. Every row is marked with a dagger because only
every tenth batch carries a durability point. The seal is inside the timed
section, so the row is the average including seals, not the cost of a write that
was never made durable. `--staged-seal-every` changes the interval; the cost
model is the cost of a commit divided by the seal interval, plus the in-memory
work per batch.

### 5.2 What the tier stages

A staged batch hashes its bytes into a chunk and appends one extent descriptor
to a per-file draft. The objects a commit would name, the extent node and the
inode, are built once at the seal, through the same builders the ordinary
methods use, against an in-memory overlay over the committed volume. A seal is
therefore not a replay: it is an ordinary commit of objects the session has
already built, with the same two barriers, allocator snapshot and four
superblock copies as every other row in this document. There is no second
recovery path, no additional on-disk structure and no format change. A mount
that never observed a session cannot determine that one was used.

### 5.3 Read tiers

While a session is open, the handle that owns it resolves its own staged writes:
`read_file`, `read_at`, `stat` and `read_dir` observe them without a device
read. `read_file_committed` does not: it returns the generation named by the
superblock, which is the read that Sections 3 and 4 measure. An application that
requires the current value reads staged; one that requires the durable value
reads committed. Any other handle on the volume observes the committed
generation only. After the seal the two tiers agree, and the staged copy is
discarded.

### 5.4 Volume consumption

A staged batch consumes 7.3 KiB of backing store over the run, which is 73 KiB
per seal, approximately two ordinary commits. The seal is the only operation
that consumes volume, and each seal rewrites an extent node that has grown since
the previous seal, so a long stream pays for its history at every seal.

**A staged append costs the appended bytes, not the file's history.** Before
the draft representation, a staged append rebuilt the file's extent node in
memory, the one object whose encoding contains the file's entire history, so the
work per batch grew with the stream although nothing reached the device. A
session now retains the decoded extent list, appends to it, and constructs the
node once at the seal. The effect is concealed at the default interval, where
the seal dominates the row. At `--staged-seal-every 512`, with all 512 batches
under one seal, the cost is 37.9 µs per batch against 78.7 µs before the draft.
What remains of the 37.9 µs after the seal's share is the append, and it no
longer grows with the file. The session's memory footprint changed in the same
way, from every generation of the node to the payload plus one descriptor per
piece (`EMBEDDING.md` §3).

A long stream still pays for the seal, which rewrites an extent node that has
grown since the previous seal. Sealing does not shrink that node, because the
file continues to grow, so the interval trades commit frequency against per-seal
growth. Splitting one growing stream across files bounds both. A stream long
enough to reach the extent node's ceiling must be split in that way regardless.

### 5.5 Durability at the seal

Before the seal, staged records exist only in the memory of the writing process.
A crash loses exactly those records and nothing else, and the volume still
mounts to the last commit. During the seal, the old-or-new rule of `FORMAT.md`
§15.2 applies. After `seal_staged` returns, the records carry the same guarantee
as every other row in this document: atomic, BLAKE3-verified, and recovered by a
plain mount with no additional code. The commit remains the ultimate point of
truth; staging determines when the application pays for one.

### 5.6 Deferred seal

The staged row still pays two barriers at every seal, because a seal is a
commit. An application that seals regularly but can lose the most recent group
of seals may move the second barrier off the seal. `seal_staged_deferred`
performs the work of `seal_staged` except the superblock write: it builds one
transaction, writes its objects and the allocator snapshot, and flushes them.
`sync()` writes that superblock and flushes it once, covering every seal since
the previous sync. Consequently n seals plus one sync cost n + 1 barriers, where
n ordinary seals cost 2n.

`deferred_append_960b` requests the same relaxation of each engine in its own
idiom: LedgerFS uses `seal_staged_deferred` and `sync`; SQLite commits under
`synchronous=NORMAL` and ends the group with `wal_checkpoint(FULL)`; redb
commits with `Durability::None` and performs an empty `Durability::Immediate`
transaction as the sync; the control writes to its held file and calls
`sync_all` once per group. The engines do not perform identical work at the
seal, since LedgerFS flushes its objects at the seal, SQLite leaves them in the
WAL, redb leaves them mapped and the control leaves them in the page cache. The
row is therefore read as the cost of a batch when the group is the durability
point, on the same basis as the staged row.

**Measured at the defaults.** Per batch: raw-file 126 µs, redb 161 µs,
sqlite-wal 397 µs and ledgerfs 1.07 ms. The control and redb are near their
in-session floor because their seals perform almost no work until the group
sync. SQLite pays for writing WAL frames at each deferred commit. LedgerFS pays
for flushing its objects at each seal, which is the cost of the crash behaviour
below: its group is already on the medium when the seal returns. Against the
staged row, ledgerfs falls from 2.05 ms to 1.07 ms per batch, and the difference
is the barrier count rather than in-session work, since a staged append performs
the same operation under either seal.

The cost of a crash in the deferred tier is bounded and stated. The previous
root remains live, so the group since the previous `sync` is lost whole, never
partially. The allocator snapshot published ahead of it may mark that group's
blocks as allocated while no live root names them, which leaks space on a
recovered volume and never produces an incorrect value. That promise is weaker
than the one `seal_staged` provides, which is why the deferred seal is a
separate call and a separate row rather than a default.

The tier is additive. `write_new_file`, `append` and `write_file` each still
produce one commit. The LedgerFS row of `insert_batched_256b` is the same
machinery at its extreme: two hundred inserts under one seal, at 104 µs per
insert. No read-back row is reported for the staged tier, because the read would
race the seal boundary, and a row whose meaning varies with the seal interval
would not be a comparison. That behaviour is fixed by `host/tests/staging.rs`
and demonstrated by `cargo run --example staging_capture`.

## 6. What is not measured

- **Concurrency.** One writer and one reader, without contention. A concurrent
  workload is not exercised, and the single-writer rule (`FORMAT.md` §12.1) is a
  design constraint rather than a benchmark condition.
- **Volumes larger than memory.** Every measurement fits in this machine's page
  cache once warmed. A volume larger than RAM would be dominated by the scan
  fallback described in Section 2, point 3, and by a wider margin than these
  figures show.
- **Crash injection.** The crash-consistency claim is tested in `host/tests/`
  and `kernel/tests/`; it is not benchmarked, because a benchmark cannot produce
  a power loss.
- **Other hardware.** These results come from one machine, whose processor,
  device, filesystem and encryption state are recorded in Appendix A. The
  harness should be run on the target hardware before any of these figures is
  quoted for it.

<!-- BEGIN GENERATED: benchmarks -->
<!-- This section is generated by `ledgerfs-benchmarks render`.
     Edit the prose around it, not the tables: they are overwritten. -->

## Appendix A. Recorded measurements

- **UTC**: 2026-10-23T02:35:01Z
- **CPU**: Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz (12 logical)
- **Memory**: 15.5 GiB
- **Kernel**: 7.0.11-1-cachyos
- **Filesystem**: btrfs
- **Device**: /dev/mapper/luks-redacted (SAMSUNG MZVLB512HAJQ-00000), **dm-crypt, so an encryption cost is included**
- **Free space**: 14.3 GiB of 474.9 GiB
- **Workload sizes**: insert_n=200, log_appends=64, append_batches=512, staged_seal_every=10, staged_sync_every=100, repeats=3

Each workload is run for every engine `repeats` times; the figure below is the median of the per-repetition means, and each repetition does the same amount of work. Nothing is tuned. Every engine is asked for one write, durable before the call returns, except where a row is marked †: those state their own durability point instead.

Where a table has a `volume KiB/op` column, that column is the backing store's own free-space delta divided by the operations the timed section performed. An engine that reports no free-space figure shows `n/a`; `n/a` is not a zero.

### A.1 Engine durability settings

| engine | durability setting |
|---|---|
| `ledgerfs` | one superblock commit per put (the value fits one object); objects + allocator snapshot, barrier, superblock, barrier |
| `raw-file` | file sync_all + parent-directory sync_all per put |
| `redb` | Durability::Immediate on every write transaction |
| `sqlite-wal` | journal_mode=WAL, synchronous=FULL; one transaction per put |

### `insert_durable_256b`

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `sqlite-wal` | 3.79 ms | 264 | 0.06 | n/a |
| `redb` | 4.13 ms | 242 | 0.06 | n/a |
| `raw-file` | 7.10 ms | 141 | 0.03 | n/a |
| `ledgerfs` | 9.18 ms | 109 | 0.03 | 36.3 |

### `insert_batched_256b`

200 inserts in one transaction; the per-insert figure is 1/200 of a commit.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `sqlite-wal` † | 19.4 µs | 51,506 | 12.57 | n/a |
| `redb` † | 19.8 µs | 50,466 | 12.32 | n/a |
| `ledgerfs` † | 104.3 µs | 9,586 | 2.34 | 12.2 |
| `raw-file` † | 3.45 ms | 290 | 0.07 | n/a |

† not a like-for-like row:
- `sqlite-wal`, `redb`, `ledgerfs`, `raw-file`: 200 inserts in one transaction; the per-insert figure is 1/200 of a commit.

### `read_point_256b`

200 reads over a store populated outside the timed section.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 949 ns | 1,053,724 | 257.26 |
| `raw-file` | 2.1 µs | 481,733 | 117.61 |
| `sqlite-wal` | 4.2 µs | 240,003 | 58.59 |
| `ledgerfs` | 25.5 µs | 39,283 | 9.59 |

### `append_log_64k`

64 appends of 64 KiB, one fsync each.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` | 3.91 ms | 256 | 15.99 | n/a |
| `sqlite-wal` | 4.01 ms | 250 | 15.60 | n/a |
| `redb` | 4.09 ms | 245 | 15.29 | n/a |
| `ledgerfs` | 7.77 ms | 129 | 8.05 | 93.9 |

### `append_batched_960b`

512 durable batches of 20 × 48 B records; the batch is the durability point.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` | 9.09 ms | 110 | 0.10 | n/a |
| `sqlite-wal` | 9.25 ms | 108 | 0.10 | n/a |
| `redb` | 9.87 ms | 101 | 0.09 | n/a |
| `ledgerfs` | 16.87 ms | 59 | 0.05 | 35.9 |

### `staged_append_960b`

512 batches of 20 × 48 B records, sealed every 10; the durability point is the seal, so the per-batch figure is 1/10 of a commit.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` † | 868.9 µs | 1,151 | 1.05 | n/a |
| `redb` † | 981.2 µs | 1,019 | 0.93 | n/a |
| `sqlite-wal` † | 1.25 ms | 800 | 0.73 | n/a |
| `ledgerfs` † | 2.05 ms | 489 | 0.45 | 7.3 |

† not a like-for-like row:
- `raw-file`, `redb`, `sqlite-wal`, `ledgerfs`: 512 batches of 20 × 48 B records, sealed every 10; the durability point is the seal, so the per-batch figure is 1/10 of a commit.

### `deferred_append_960b`

512 batches of 20 × 48 B records, sealed every 10 and synced every 100; each seal writes its group, one sync publishes every group since the last one.

| engine | median per operation | operations/s | payload MiB/s | volume KiB/op |
|---|---:|---:|---:|---:|
| `raw-file` † | 126.5 µs | 7,906 | 7.24 | n/a |
| `redb` † | 160.7 µs | 6,221 | 5.70 | n/a |
| `sqlite-wal` † | 397.4 µs | 2,517 | 2.30 | n/a |
| `ledgerfs` † | 1.07 ms | 938 | 0.86 | 7.3 |

† not a like-for-like row:
- `raw-file`, `redb`, `sqlite-wal`, `ledgerfs`: 512 batches of 20 × 48 B records, sealed every 10 and synced every 100; each seal writes its group, one sync publishes every group since the last one.

### `read_back_960b`

one whole-stream read per repetition (512 batches, every byte compared).

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `raw-file` | 150 ns | 6,688,788 | 6123.77 |
| `redb` | 1.4 µs | 708,202 | 648.38 |
| `sqlite-wal` | 2.7 µs | 373,000 | 341.49 |
| `ledgerfs` | 3.2 µs | 307,763 | 281.77 |

### `scan_10`

10 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 195 ns | 5,117,707 | 1249.44 |
| `sqlite-wal` | 570 ns | 1,754,694 | 428.39 |
| `raw-file` | 893 ns | 1,120,072 | 273.46 |
| `ledgerfs` | 3.0 µs | 336,995 | 82.27 |

### `scan_100`

100 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 128 ns | 7,809,449 | 1906.60 |
| `sqlite-wal` | 178 ns | 5,633,485 | 1375.36 |
| `raw-file` | 281 ns | 3,560,873 | 869.35 |
| `ledgerfs` | 1.6 µs | 611,404 | 149.27 |

### `scan_1000`

1000 entries, populated outside the timed section; 100 per directory.

| engine | median per operation | operations/s | payload MiB/s |
|---|---:|---:|---:|
| `redb` | 97 ns | 10,293,361 | 2513.03 |
| `sqlite-wal` | 130 ns | 7,692,544 | 1878.06 |
| `raw-file` | 246 ns | 4,063,059 | 991.96 |
| `ledgerfs` | 1.9 µs | 535,145 | 130.65 |

### `reopen_and_read`

reopen plus one read, so lazy recovery is inside the timed section.

| engine | median per operation | operations/s |
|---|---:|---:|
| `sqlite-wal` | 214.6 µs | 4,660 |
| `ledgerfs` | 464.5 µs | 2,153 |
| `redb` | 34.68 ms | 29 |

Not measured:
- `raw-file`: no recovery step: there is no journal to replay and no root to select

<!-- END GENERATED: benchmarks -->
