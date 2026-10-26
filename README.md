# LedgerFS

**Sing Security Research Laboratory**

A copy-on-write, content-addressed storage layer. An application links it and
uses a volume as its storage; an operating system adopts it as its filesystem.
The on-disk format is specified byte by byte, and the implementation is
published as two Rust crates.

---

## Abstract

LedgerFS stores a hierarchical file tree in a single volume: a flat array of
4 KiB blocks. Every object is immutable and named by the BLAKE3 hash of its
canonical encoding, and every modification produces new generations of the
objects on the affected path. Durability is obtained without a write-ahead log.
A commit writes the new objects and the allocator snapshot, issues a barrier,
writes the four superblock copies that name the new root, and issues a second
barrier. The superblock write is the commit point. A crash leaves the previous
root or the new one, never a blend, and mount has no log to replay.

The library is deployed in two forms. CrucibleOS uses it as the filesystem
behind its file-descriptor service. Applications link `ledgerfs-host` and use a
volume as their storage layer. The format crate performs no I/O; it reads and
writes through an `ObjectStore` trait, so the same format code runs in a kernel
and in a userspace process.

The design assumes a capture workload: records are written once, read back whole
or by byte range, and not rewritten. For that workload the system provides
atomic publication, content verification on read, and a volume that is one
portable artifact with no server beside it. It does not provide a schema,
indexes, or a query language, and it does not reclaim space. Section 7
quantifies the cost of the guarantee.

## Contributions

1. A normative on-disk format covering object framing, the metadata model, the
   allocator, the superblock, feature negotiation and compatibility rules
   (`FORMAT.md`).
2. A commit protocol whose crash argument rests on ordering rather than on
   logging, implemented once and shared by the host library and the kernel path.
3. A staging tier that separates accepted writes from durable writes and lets
   each read select the tier that answers it, with the seal defined as an
   ordinary commit.
4. An evaluation harness that measures the implementation against SQLite in WAL
   mode, redb and a raw `fsync` control on the same device, with each engine's
   durability setting stated (Section 7).

## 1. Introduction

Most storage systems make a write durable by writing it twice: once to a log,
once to the location it occupies, with a replay step to reconcile the two after
a crash. The log is a second source of truth, and its format, its recovery path
and the window in which the two disagree are part of the system's correctness
argument.

LedgerFS orders the write instead. Objects and the allocator snapshot reach the
medium and are flushed; the superblock copies that name the new root are then
written and flushed. Before the superblock write the old root is the volume;
after it, the new root is. Because nothing is modified in place, the objects
the old root names remain valid for the duration of the change, and the medium
holds either the previous state or the next one. There is no log format, no
replay path, and no interval in which two structures claim to be authoritative.

Three properties follow from the same design. Objects are named by the hash of
their content and verified against that name before they are returned, so a
read produces the recorded bytes or an error. A volume is a byte string, so a
snapshot is a copy and an archive is the volume itself. And the format code
performs no I/O, so a kernel and a userspace process can execute the same
implementation over different devices.

Section 2 defines the data model, Section 3 the write path, and Section 4 the
staging tier. Section 5 describes the implementation, Section 6 the deployment
in CrucibleOS, Section 7 the measurements, Section 8 the limitations and
Section 9 the workloads for which the design is and is not intended.

## 2. System model

A **volume** is a flat array of 4 KiB blocks with a superblock at a fixed
location, a root object, and a tree of objects reachable from that root. The
file or partition that holds a volume is not the volume; it is one way of
supplying the block array.

**Objects.** The format has five object kinds: `Chunk`, which holds file
content; `Inode`, which holds file metadata; `DirNode`, which holds one
directory's sorted entries; `ExtentNode`, which holds a file's extent list; and
`Root`, which names the tree. An object is addressed by the 32-byte BLAKE3 hash
of its canonical encoding. Two objects with equal content have the same id, and
a reader recomputes the id before accepting the bytes. A record additionally
carries a checksum, so framing damage is detected before decoding.

**Roots and generations.** Exactly one root is live at a time, and the
superblock names it together with a monotonic `commit_seq`. A modification
produces new generations of every object on the path from the root to the
changed node; the previous generations remain on the medium and remain valid
for readers that already adopted them.

**Allocation.** A block allocator tracks free, allocated and reserved blocks.
Its serialized snapshot is written as part of each commit at a fixed reserved
block index. It cannot be stored as an ordinary object, because placing it would
consume a block and change the state being recorded.

**Superblock redundancy.** Four superblock copies exist per volume, two at the
head of the volume and two at the end. Mount selects the highest verifying
`commit_seq` among them. A Root Locator Ring inside the superblock allows the
root to be recovered when the newest copy is damaged. The host library reports
disagreement among copies rather than repairing it silently.

## 3. Write path and crash consistency

A file operation builds its entire result in memory before any block is
allocated. A 256-byte write produces a chunk, an extent node, an inode, a
directory entry, and the ancestors whose identifiers changed. A write larger
than the 256 KiB object ceiling is split across several chunks and folded into
one transaction.

The commit then proceeds in a fixed order:

1. Place the new objects in free blocks and write them, together with the
   allocator snapshot.
2. Issue a barrier, so that all of the above is on stable media.
3. Write the four superblock copies naming the new root.
4. Issue a second barrier. The commit is complete when step 4 returns.

A crash before step 3 completes leaves the previous root live. The objects the
new root would have named are unreachable and occupy their blocks until the
volume is replaced; nothing frees space. A crash during step 3 leaves at least
one copy of either the old or the new superblock, and mount selects the highest
verifying sequence number. There is no state in which a root names objects that
were never written, because step 2 precedes step 3.

The first barrier is what permits mount to adopt a superblock without
consulting a log: by the time the new root is named, the objects it names are
already durable. The second barrier is the commit point. Removing the first
would move the cost of an interrupted commit into the mount path and change the
recovery story; `FORMAT.md` §15.1 states the ordering argument, and the
implementation does not take that trade.

The intent log defined by the format is not written on the live path. The
format reserves a bounded ring for commit-intent records so that a future
implementation can describe an in-flight commit, but neither the host library
nor CrucibleOS uses it, and no recovery decision depends on it (`FORMAT.md`
§11, §18.5).

## 4. Staging tier

A commit costs two barriers, which dominates the cost of a small durable write.
For a stream of records, paying that cost per record is unnecessary: the
application can decide when the point of truth moves.

`begin_staging` opens a session. `staged_append` extends an in-memory draft of
the target file: it hashes the appended bytes into a chunk, pushes an extent
descriptor onto the draft, and touches no device block, no allocator state and
no barrier. `seal_staged` builds the objects those appends imply, commits them
as one transaction of the ordinary shape, and returns. The commit sequence
advances by one, and the objects the intermediate generations produced are
pruned at the seal, so N staged appends carry N chunks plus one generation of
each metadata object to the medium.

`seal_staged_deferred` performs the same work except the superblock write, so
several seals can share one `sync`. The group is published whole or lost whole.

Two read tiers exist while a session is open. The handle that owns the session
resolves its own staged writes from memory, with no device read; every other
handle sees the committed generation. `read_file_committed` on the owning handle
also answers from the committed generation. The choice is per read, and there is
no ambient default.

A crash before the seal loses the staged work and nothing else. A crash after
the seal returns is the ordinary commit case. No format change is involved, and
a mount that never observed a session cannot distinguish a volume written with
one.

## 5. Implementation

Two crates are published.

| crate | runtime | contents |
|---|---|---|
| [`ledgerfs`](https://crates.io/crates/ledgerfs) | `no_std` + `alloc` | the format: record framing, encoding, the object model, the allocator, placement, the commit engine, and the copy-on-write operations. No I/O. |
| [`ledgerfs-host`](https://crates.io/crates/ledgerfs-host) | `std` | an `ObjectStore` over a `BlockDevice`, mount and path handling, `Store` (format, open, mutation verbs, staging), per-volume encryption, and the `lfs-read` and `lfs-write` tools. |

The kernel crate defines [`ObjectStore`](kernel/src/cow/object_store.rs) as the
seam between the format and the medium. The host crate supplies a disk-backed
implementation; the kernel path inside CrucibleOS supplies one over an ATA disk;
tests supply one over a `Vec<u8>`. The same format code executes in all three.

Public API documentation is built with `all-features`, and both crates deny
`missing_docs`. Every public item is documented. The feature sets, including
the ring-0 `reader` path and the optional `data-encryption` and `temper_sealing`
features, are described in [`kernel/README.md`](kernel/README.md).

## 6. Deployment

CrucibleOS uses LedgerFS as the filesystem beneath its file-descriptor service.
The kernel crate is compiled into a Tier 2 IPC service that maintains a handle
table, answers each operation against the root current at the time of the call,
and performs access checks; the volume is a disk the kernel owns rather than a
file it opens. That service is the OS-side layer the format leaves to its
adopter: LedgerFS supplies the format and the commit protocol, and the operating
system supplies the policy above them. A database engine would occupy the same
position one level higher.

## 7. Evaluation

All figures below were produced by the harness in [`benchmarks/`](benchmarks/),
on one device, with each engine's durability setting stated in
[`BENCHMARKS.md`](BENCHMARKS.md). The comparison engines are SQLite 3 in WAL
mode with `synchronous=FULL`, `redb` with `Durability::Immediate`, and a control
that writes one file per key and synchronizes both the file and its directory.
The control is the relevant baseline for durability cost: it issues the same
kind of operation a commit issues, with no filesystem on top.

| workload | LedgerFS | SQLite WAL | redb | raw `fsync` |
|---|---:|---:|---:|---:|
| Durable insert, 256 B | 9.18 ms | 3.79 ms | 4.13 ms | 7.10 ms |
| Verified point read, 256 B | 25.5 µs | 4.2 µs | 949 ns | 2.1 µs |
| Batched append, 960 B per batch | 16.87 ms | 9.25 ms | 9.87 ms | 9.09 ms |
| Staged append, seal every 10 batches | 2.05 ms | 1.25 ms | 981 µs | 869 µs |
| Deferred seal, one group sync | 1.07 ms | 397 µs | 161 µs | 126 µs |

The staged and deferred rows report per-batch cost with the seal included, since
their durability point is the seal rather than the batch. The engines do not
perform identical work at the seal; `BENCHMARKS.md` states the difference for
each of them, and no row should be compared across engines without that
statement.

Three observations account for the differences.

**Barriers dominate small durable writes.** A barrier that follows new data
costs 3 to 5 ms on this device, and the single-insert row moves between runs by
more than a factor of two. Against the control, which pays the same two
synchronizations, LedgerFS costs 1.29 times as much; the remainder is the commit
itself: a fresh allocator snapshot, four superblock copies, and a copy-on-write
replacement of every object on the path.

**Reads verify their own result.** The 25.5 µs point read includes a BLAKE3
computation over the returned object. SQLite and redb trust their pages. On the
960-byte read-back workload the verified figure is within about 1.2 times
SQLite's unverified read, which is the price of the check. The read row is also
warm: it is measured in the process that wrote the data, so the block map is
resident. A process that opens a volume it did not write pays a linear scan for
the first lookup of each object, because storage hints are not persisted
(`KNOWN_ISSUES.md` §4).

**Volume consumption follows commits, and a commit pays for what it
allocates.** A 256-byte insert consumes 36.3 KiB of volume and a batch of twenty
48-byte records consumes 35.9 KiB, most of it the allocator snapshot, the four
superblock copies and the rewritten path rather than the payload. A commit that
carries more pays for more: a 64 KiB append consumes 93.9 KiB, and a commit that
creates two hundred files consumes 12.2 KiB per insert because each file
allocates a chunk, an inode and an extent node. Capacity planning therefore
follows the number of commits and the number of objects they allocate.

Staging changes the order of magnitude of the per-record cost. At a seal every
ten batches a staged append costs 2.05 ms per batch against 16.87 ms for a
committed batch of the same bytes. At a seal every 512 batches the harness
reports 37.9 µs per batch, which measures the in-memory append with the seal's
share set aside; that interval is not the one recorded in Appendix A. The
deferred-seal row removes one of the two barriers per group, at the cost of a
weaker crash promise: the group since the last `sync`
is lost whole, and blocks may be marked allocated without a live root naming
them, which leaks space on a recovery and never produces a wrong answer.

## 8. Limitations

- **Space is not reclaimed.** No shipping path frees a block. A volume fills
  monotonically and should be treated as append-only with a planned lifetime.
  Snapshots, a garbage collector and a scrubber are implemented and tested in
  the unpublished `pending/` crate; none is driven by the shipping path.
- **Appending to one growing file is quadratic in device bytes.** Every commit
  rewrites the file's extent node, whose encoding grows with the file. Bounded
  chunk files, or a staging session with periodic seals, restore linear cost.
- **Fixed ceilings.** Objects are at most 256 KiB, directories 400 entries,
  extent nodes 1500 entries and names 256 bytes. Exceeding a ceiling is an
  error, not a slowdown.
- **No in-place update.** `write_at` may append past the end of a file but
  cannot rewrite stored bytes. A file is replaced whole or extended.
- **One writer per volume.** A writable file device takes an advisory exclusive
  lock. Readers take none and read the root they opened.
- **No VFS integration.** Nothing registers with the host operating system, so
  no path on the host resolves into a volume. An OS adopts the library by
  writing the service layer above it, as CrucibleOS does.
- **Tier B subsystems are not available.** Garbage collection, snapshots,
  deduplication at write time, the scrubber and content-defined chunking are
  implemented and tested in the unpublished `pending/` crate; replication and
  the audit chain are present in the format crate with no caller. Four
  subsystems return success without performing the work; `KNOWN_ISSUES.md` §11
  lists which. Quotas, ACLs, disaster recovery, capability enforcement and
  master-key rekeying have no implementation at all. Per-volume encryption is
  not in that list: keyslots, the block cipher and the slot lifecycle are driven
  by `ledgerfs-host`, and only replacing the master key is missing.
- **Encryption protects contents, not metadata.** On an encrypted volume the
  object region is XTS-AES-256; the superblock, the intent log and the
  allocation snapshot stay plaintext so the volume can be identified before it
  is unlocked. Allocation state and file-system structure are therefore visible
  to an attacker holding the disk. The passphrase KDF is iterated BLAKE3 and not
  memory-hard, and losing every credential loses the volume: there is no
  recovery path and no back door.

## 9. Applicability

The design is intended for records that are written once, retained for a
bounded period, and read back whole or by byte range: logs, capture streams,
archives, appliance state, and the file tree of a system that owns its disk.

Within that class, a volume can occupy the position a database would: it
stores an application's records and returns them, and for a capture workload
that is the complete requirement. What it does not provide is a database
engine, meaning a schema, secondary indexes, a query planner and an executor.

It is not intended for data edited in place, for workloads that require
cross-record queries, or for volumes that must run indefinitely without
attention.

| suitable | unsuitable |
|---|---|
| append-only capture and logging | a query engine: no schema, no indexes, no joins |
| portable archives and snapshots | data rewritten in place |
| an operating system's own file tree | a shared volume with many writers |
| an application store with atomic publication | a volume that runs forever without attention |

The absence of a query layer is a scope boundary, not a claim about the floor
of the design. A catalog, secondary indexes, a query planner and an executor
are a layer above this one, and the format does not obstruct them. That layer is
not part of this release.

## 10. Availability and reproducibility

```sh
cargo add ledgerfs-host
```

```rust
use ledgerfs_host::{MemBlockDevice, Result, Store};

fn main() -> Result<()> {
	let device = MemBlockDevice::new(2048);
	let mirror = device.clone();

	let mut store = Store::format_app(Box::new(device), "example")?;
	store.write_new_file("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;

	store.begin_staging(4 * 1024 * 1024)?;
	store.staged_append("/log.jsonl", b"{\"turn\":2}\n", 0o644)?;
	assert_eq!(store.read_file_committed("/log.jsonl")?, b"{\"turn\":1}\n");
	let _seal = store.seal_staged()?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	Ok(())
}
```

`FileBlockDevice` in place of `MemBlockDevice` drives a file or a partition with
the same calls.

Both published crates build on stable Rust 1.89, edition 2024. The benchmark
harness is not published and requires 1.90, because `redb` does.

```sh
cargo test -p ledgerfs --all-features
cargo test -p ledgerfs-host
cargo run --release -p ledgerfs-benchmarks -- run
cargo run --release -p ledgerfs-benchmarks -- render
```

The harness records each run as a JSON file under `benchmarks/results/`; the
tables in `BENCHMARKS.md` are regenerated from the newest file and therefore
cannot drift from a recorded run. The machine identification in those records is
redacted.

### Documents

| document | role |
|---|---|
| [`FORMAT.md`](FORMAT.md) | the on-disk format, normative, cited by section from the source |
| [`TERMINOLOGY.md`](TERMINOLOGY.md) | the project vocabulary, normative for prose and naming |
| [`EMBEDDING.md`](EMBEDDING.md) | the contract for an application embedding a volume |
| [`BENCHMARKS.md`](BENCHMARKS.md) | measurement method, provenance and results |
| [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) | limitations, with their causes |
| [`SECURITY.md`](SECURITY.md) | vulnerability reporting |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | feature matrix and format-change rules |
| [`RELEASING.md`](RELEASING.md) | publish order, gates and version semantics |
| [`CHANGELOG.md`](CHANGELOG.md) | release history |

### Examples

| example | subject |
|---|---|
| [`embed`](host/examples/embed.rs) | in-memory volumes, snapshotted and reopened |
| [`disk_volume`](host/examples/disk_volume.rs) | a file-backed volume, the writer lock, a concurrent reader |
| [`app_store`](host/examples/app_store.rs) | the application-store contract from `EMBEDDING.md` |
| [`staging_capture`](host/examples/staging_capture.rs) | both read tiers during a session, then a seal |
| [`custom_device`](host/examples/custom_device.rs) | the four-method `BlockDevice` contract |

## 11. Guarantees

- **Atomic publication.** One transaction per verb, one commit as the durability
  point. A crash leaves the previous root or the new one, never a blend.
- **Explicit durability.** A commit is durable when it returns. A seal is
  durable when it returns. A deferred seal is durable at `sync()`. Before either,
  the data exists only in the writing process and a crash loses exactly that.
- **Content verification.** Every object is named by its hash and every record
  is checksummed. A read verifies before it answers.
- **Single writer.** Enforced by an advisory exclusive lock on a writable file
  device.

## 12. License

Dual-licensed: Apache-2.0 or MIT, at your option. See
[`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT) and
[`NOTICE`](NOTICE).
