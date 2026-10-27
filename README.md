# LedgerFS

**Sing Security Research Laboratory**

A copy-on-write, content-addressed storage layer that an application links and
an operating system adopts. A volume is a single byte string: a flat array of
4 KiB blocks that holds a file tree, its allocator state and its recovery
metadata, with no server and no write-ahead log beside it. The on-disk format is
specified byte by byte in [`FORMAT.md`](FORMAT.md).

Research-grade software. Two Rust crates, `ledgerfs` and `ledgerfs-host`. They
are not yet published to crates.io; Section 11 gives the dependency form that
works today.

---

## Contents

- [Abstract](#abstract)
- [1. Research question](#1-research-question)
- [2. Claims and evidence](#2-claims-and-evidence)
- [3. Design](#3-design)
- [4. Implementation](#4-implementation)
- [5. Deployment](#5-deployment)
- [6. Evaluation](#6-evaluation)
- [7. Threat model](#7-threat-model)
- [8. Limitations](#8-limitations)
- [9. Related work](#9-related-work)
- [10. Applicability](#10-applicability)
- [11. Availability and reproducibility](#11-availability-and-reproducibility)
- [12. Guarantees](#12-guarantees)
- [13. License](#13-license)

---

## Abstract

LedgerFS stores a hierarchical file tree in a single volume: a flat array of
4 KiB blocks. Every object is immutable and named by the BLAKE3 hash of its
canonical encoding, and every modification produces new generations of the
objects on the affected path. Durability is obtained without a write-ahead log.
A commit writes the new objects and the allocator snapshot, issues a barrier,
writes the four superblock copies that name the new root, and issues a second
barrier. The superblock write is the commit point. A crash leaves the previous
root or the new one, never a blend, and a mount performed by this implementation
has no log to replay.

The implementation is two Rust crates. `ledgerfs` is `no_std` and owns the
format; it performs no I/O and reads and writes through an `ObjectStore` trait.
`ledgerfs-host` is `std` and supplies an `ObjectStore` over any `BlockDevice`, a
`Store` with the mutation verbs, a staging tier that separates accepted writes
from durable writes, and per-volume encryption. A volume is a byte string, so a
snapshot is a copy and an archive is the volume itself.

The design assumes a capture workload: records are written once, read back whole
or by byte range, and not rewritten. For that workload the system provides
atomic publication, content verification on read, and a volume with no server
beside it. It does not provide a schema, indexes, a query language, space
reclamation, or in-place update. Section 6 reports the measured cost of the
guarantee on one device against SQLite in WAL mode, redb and a raw two-sync
control. Section 2.2 states which claims from the accompanying research chapter
this release supports, and which belong to the CrucibleOS deployment and are not
in these crates.

---

## 1. Research question

The LedgerFS chapter of the CrucibleOS whitepaper asks whether a copy-on-write
filesystem can serve as the cryptographically verifiable trust anchor of a
microkernel: binding content integrity, boot attestation and encryption into one
on-disk format without giving up crash consistency. The question has two halves.
One is a storage layer whose crash, verification and durability properties hold
on their own. The other is a boot chain that can verify the volume before the
full storage stack exists, with a root seal and a key hierarchy above it.

This repository is the first half, released as a standalone library. It
implements the format, the object model, the ordered commit, the staging tier
and volume encryption. The boot chain, the root seal, the vault key hierarchy
and the boot-time reader belong to CrucibleOS and are not in these crates.
Section 2.2 records that boundary claim by claim.

Within the storage layer, the release answers four questions:

- **Q1. Can crash consistency be obtained by ordering alone**, without a
  write-ahead log and without replay at mount?
- **Q2. Can every read verify the content it returns**, so that a corrupted
  block or a substituted object is an error rather than a silent answer?
- **Q3. Can an application choose when its writes become durable**, without a
  second recovery path and without a new on-disk state?
- **Q4. Can one format implementation run in a kernel and in a userspace
  process**, over devices as different as a disk and a heap buffer?

Section 3 answers the first three through the design. Section 4 addresses the
fourth through the crate split and the device seam. Section 6 reports what the
resulting guarantees cost. The boot-chain questions the chapter also asks are
out of scope here; Section 2.2 does not restate them as properties of this
release.

## 2. Claims and evidence

### 2.1 Supported claims

Five claims are made for the release. Each names its evidence, and a property
without evidence is not claimed.

| claim | evidence |
|---|---|
| **L1. A commit is the durability point, and a crash leaves the previous root or the new one, never a blend, with no log to replay.** | The order is normative in `FORMAT.md` §15.1. `host/tests/durability.rs` sweeps every prefix of a mutation session and mounts each one to a whole tree; it also pins the crash window between the allocator snapshot and the superblock. `kernel/src/commit.rs` performs the write, the barrier, the superblocks and the second barrier. |
| **L2. Every object read is verified against its content address and its record checksum before it is returned.** | `kernel/src/record.rs` frames each object with a magic, a length and a checksum; `host/src/objstore.rs` checks `blake3(decoded payload) == object_id` before answering. `read_back_960b` in the benchmark harness compares every byte of a stream after it is read back. |
| **L3. Staging separates accepted writes from durable writes; a seal is an ordinary commit, and the application selects the read tier.** | `host/src/staging.rs` and `host/tests/staging.rs`: staged appends touch no device block, no allocator state and no barrier; `seal_staged` produces one transaction of the existing shape; `read_file_committed` answers from the superblock while `read_file` answers from the staged view. `BENCHMARKS.md` §5 measures the tier. |
| **L4. The format performs no I/O, and one implementation serves a kernel, a userspace process and a test harness through different stores.** | `kernel/src/cow/object_store.rs` defines the `ObjectStore` seam. `ledgerfs-host` supplies `DiskObjectStore` over a four-method `BlockDevice`; `MemBlockDevice` supplies the same over a `Vec<u8>`; a kernel can supply its own. CI builds every declared kernel feature, including `reader` for the ring-0 path. `host/examples/custom_device.rs` implements the device contract directly. |
| **L5. A volume is a byte string: it can be copied, carried and reopened without a server or a host filesystem.** | `host/examples/embed.rs` formats an in-memory volume, snapshots its bytes and reopens them. `host/examples/disk_volume.rs` does the same over a file region. The partition reader in `host/src/image.rs` locates a volume inside a whole-disk image. |

The release does **not** claim the following, and a reader should not infer them
from the format:

- A measured comparison with ZFS, Btrfs or WAFL, including the chapter's claim
  of fewer I/O operations than a ZIL or a log tree. The comparison in Section 6
  is against SQLite, redb and a raw two-sync control.
- Power-fail injection or a formal model of the commit protocol. Crash behavior
  is tested by mounting recorded byte prefixes; it is not tested against a
  controller that reorders writes.
- Space reclamation, snapshots, write-time deduplication, a background scrubber
  or content-defined chunking on a live volume. Those implementations exist in
  the unpublished `pending/` crate and have no caller in the shipping path.
- A root seal, a vault key hierarchy, per-file encryption or a boot-time reader.
  Volume encryption in this release is XTS-AES-256 over the object region, with
  two keyslots in the superblock.
- A VFS registration, a mount point, a schema, secondary indexes, a query
  language or in-place update.

### 2.2 Relation to the research chapter

The research chapter and this release describe different scopes. The chapter is
a statement of the wider CrucibleOS program; this repository is the storage
layer released on its own. The table records the boundary so that a claim is not
carried across it by accident.

| chapter claim | status in this repository |
|---|---|
| **C1. A dual-PQC root seal in a dedicated superblock block binds the content tree to the disk geometry and is verifiable by a minimal boot reader.** | Not in these crates. This format has no root seal. Block 5 is the allocator snapshot, not a seal record. The kernel crate contains `seal_record`, the ML-DSA-65 and SLH-DSA record-sealing module, behind the off-by-default `temper_sealing` feature, and no shipping path in this repository calls it. |
| **C2. A TxnPlan five-phase commit with deterministic pre-allocation uses fewer I/O operations than a ZFS ZIL or a Btrfs log tree.** | The in-memory `TxnPlan` and `CommitEngine` exist and are exercised. The live commit is the two-barrier ordered write in `FORMAT.md` §15.1: objects and allocator snapshot, barrier, four superblock copies, barrier. It writes no checkpoint and no intent-log records. The I/O-count comparison with ZFS or Btrfs is not evaluated here, and this repository has no such measurement. |
| **C3. A vault Merkle tree over encrypted DataKey entries enables partial key-hierarchy verification with logarithmic proofs.** | Not in these crates. There is no vault and no per-file key hierarchy. Encryption is volume-level XTS-AES-256 with two flat keyslots; a slot is passphrase-derived or sealed under a supplied 32-byte key-encryption key. |
| **C4. A dual-layer mount resolves a kernel image from a cold disk in under 100 ms.** | Not in these crates, and no boot timing is measured. The `reader` feature compiles a ring-0 ATA PIO path for a consumer that is the only route to its own partition. It is a read path, not a measured boot mount with a latency budget. |
| **C5. Content addressing provides measurable deduplication savings without a separate index.** | Not claimed and not measured. Content addressing gives identical content an identical object id, but the shipping path has no deduplication index and no merge pass, and no savings figure exists. Deduplication is implemented in `pending/` with no caller. |

The chapter is a research statement, not the specification of this release.
Where it describes a layout or a mechanism that differs from these crates,
[`FORMAT.md`](FORMAT.md) and the code in this repository are authoritative for
the released implementation.

## 3. Design

Three choices determine the design: objects are immutable and content-addressed,
a commit is defined by the order of writes rather than by a log, and the
durability point is an operation the application can move. Each choice buys a
property and costs something. Section 3.8 states both sides.

### 3.1 Volume and on-disk layout

A volume is a flat array of 4 KiB blocks. The first six and the last two are
reserved; the object region lies between them. Four superblock copies are split
between the two ends so that damage to one end leaves the other pair intact.

| blocks | contents | on an encrypted volume |
|---|---|---|
| 0, 1 | superblock copies 0 and 1 | plaintext |
| 2, 3 | intent-log ring, 8 KiB | plaintext |
| 4 | genesis audit record | plaintext |
| 5 | allocation-tree snapshot | plaintext |
| 6 to `total_blocks - 3` | object space | ciphertext |
| `total_blocks - 2`, `total_blocks - 1` | superblock copies 2 and 3 | plaintext |

`FORMAT.md` §11 is the normative table. The split between head and tail copies
is the redundancy argument, and it is why the last two blocks are never object
space. The intent log is two blocks, not a multi-megabyte region: it is a
tie-breaker for mount selection, not a durable write-ahead log, and a ring that
small cannot grow into one by accident.

### 3.2 Objects and addressing

The format has five object kinds: `Chunk` for file content, `Inode` for file
metadata, `DirNode` for one directory's sorted entries, `ExtentNode` for a
file's extent list, and `Root`, which names the tree. An object is addressed by
the 32-byte BLAKE3 hash of its canonical encoding. Two objects with equal
content have the same id, and a reader recomputes the id before accepting the
bytes.

`Root` is a descriptor rather than a stored record. `Root::file_tree_root` holds
the reference to the root directory's `Inode`, and the superblock names that
reference directly, so a read of the volume's root is a typed `Inode` read. The
four kinds that occupy object space are `Chunk`, `Inode`, `DirNode` and
`ExtentNode`.

`ObjectRecord` framing adds a magic, a length and a truncated BLAKE3 checksum,
so framing damage is detected before decoding. The full hash is the identity;
the truncated checksum catches a torn write or a bit flip and is not a
collision-resistant name. Chunks above 4 KiB are compressed with LZ4 when it
saves at least ten percent; the identifier is over the logical payload, and
verification is performed after decoding.

Exactly one root is live at a time. The superblock names it together with a
monotonic `commit_seq`. A modification produces new generations of every object
on the path from the root to the changed node. The previous generations remain
on the medium and remain valid for a reader that already adopted them.

### 3.3 Allocation

A block allocator tracks free, allocated and reserved blocks. Its serialized
snapshot is written at block 5 as part of each commit. It cannot be stored as an
ordinary object, because placing it would consume a block and change the state
being recorded.

The shipping path allocates and never frees. A block that stops being reachable
from the current root is left where it is. Consistency does not depend on
reclaiming it: the published root is the only authority on what is reachable,
and a snapshot that runs ahead of that root only marks unreachable blocks
allocated. It cannot free a block the published root still names.

### 3.4 Commit protocol

A file operation builds its entire result in memory before any block is
allocated. A 256-byte write produces a chunk, an extent node, an inode, a
directory entry and the ancestors whose identifiers changed. A write larger than
the 256 KiB object ceiling is split across several chunks and folded into one
transaction.

The commit then proceeds in a fixed order:

1. Place the new objects in free blocks and write them, together with the
   allocator snapshot.
2. Issue a barrier, so that all of the above is on stable media.
3. Write the four superblock copies naming the new root.
4. Issue a second barrier. The commit is complete when step 4 returns.

| crash point | what a subsequent mount sees |
|---|---|
| before the superblock write completes | the previous root; the new objects are unreachable |
| during the superblock write, with some copies valid | the previous root, or the new root if a written copy verifies |
| after the second barrier | the new root |

There is no state in which a root names objects that were never written,
because step 2 precedes step 3. There is also no window in which the previous
root's objects have been modified, because no object is overwritten in place.

The first barrier is what permits a mount to adopt a superblock without
consulting a log: by the time the new root is named, the objects it names are
already durable. The second barrier is the commit point. Removing the first
would move the cost of an interrupted commit into the mount path and change the
recovery argument; the implementation does not take that trade.

One exposure remains, and it is a space cost rather than a correctness one. A
crash between the allocator snapshot write and the barrier that covers it can
leave the previous root published with a snapshot written for the commit that
did not land. That snapshot is ahead of the published root: it can mark blocks
allocated that no root names, which leaks space until a later commit rewrites
the tree. It cannot mark a block free that the published root references,
because the live path allocates and never frees, so the snapshot is always the
previous state plus more allocations.

The intent log defined by the format is not written on the live path. The host
mount calls `mount_select` without a ring, so it has no replay path at all. The
format reserves the bounded ring so that a future writer can describe an
in-flight commit, and `FORMAT.md` §16 to §18 specify how such a writer orders
four barriers and how a mount distinguishes an in-flight commit from a
committed one without ever adopting a root from the log. No volume this release
produces exercises that machinery.

### 3.5 Mount and root selection

Mount decodes and checksums all four superblock copies and discards the invalid
ones. It collects candidates from each copy's active root and from the Root
Locator Ring, then selects the highest verifying `commit_seq`. The ring holds 56
entries of 60 bytes and allows the root to be recovered when the newest copy is
damaged.

Two valid copies with the same sequence and different roots are a split-brain
candidate. The format requires that the conflict be reported and that no root be
chosen by a guess; the kernel's mount selection records the condition as a
read-only safe mode. The host does not reconcile or repair copies. `lfs-read
info` reports which copies decoded, their sequence numbers and whether they
agree with the selected copy.

### 3.6 Staging tier

A commit costs two barriers, which dominates the cost of a small durable write.
For a stream of records, paying that cost per record is unnecessary: the
application can decide when the point of truth moves.

`begin_staging` opens a session. `staged_append` extends an in-memory draft of
the target file: it hashes the appended bytes into a chunk, appends one extent
descriptor to the draft, and touches no device block, no allocator state and no
barrier. The draft retains the decoded extent list rather than rebuilding the
node on every append, so the per-append work follows the appended bytes and not
the file's history.

`seal_staged` builds the objects those appends imply, commits them as one
transaction of the ordinary shape, and returns. The commit sequence advances by
one. Intermediate generations are pruned at the seal, so N staged appends carry
N chunks plus one generation of each metadata object to the medium.

`seal_staged_deferred` performs the same work except the superblock write, so
several seals can share one `sync`. The group is published whole or lost whole.
A deferred group is otherwise the same commit: its objects and allocator
snapshot are on the medium when the seal returns, and the superblock is what
`sync` adds.

Two read tiers exist while a session is open. The handle that owns the session
resolves its own staged writes from memory, with no device read; every other
handle sees the committed generation. `read_file_committed` on the owning handle
also answers from the committed generation. The choice is per read, and there is
no ambient default.

A crash before the seal loses the staged work and nothing else. During the seal
the old-or-new rule applies. After the seal returns, the data is an ordinary
committed generation. No format change is involved, and a mount that never
observed a session cannot distinguish a volume written with one. Staged data has
no on-disk representation before the seal; it exists only in the writing
process.

### 3.7 Encryption

Encryption has two deliberately separate layers. Keyslots wrap a randomly
generated 64-byte master key once per slot. The block cipher applies XTS-AES-256
below the object model, so exactly the disk path sees plaintext and everything
above it, including content addressing and the extent tree, never learns that
encryption exists.

A keyslot is 128 bytes, and two slots occupy bytes 3772 to 4028 of the
superblock. A passphrase slot derives a key-encryption key with iterated BLAKE3
over a per-slot salt, 200,000 rounds by default, and wraps the master key with
AES-256-CBC. A second slot kind is sealed under a supplied 32-byte
key-encryption key, which is the shape an HSM or a machine-held secret
provides. Unwrapping checks the recovered key against a slot checksum, which
separates a wrong credential from a corrupt slot. Changing a passphrase rewraps
64 bytes rather than re-encrypting the volume.

XTS covers blocks 6 to `total_blocks - 3`, with the tweak derived from the
volume-relative LBA. It provides confidentiality and no per-block
authentication. The object layer already recomputes `blake3(decoded payload)`
against the object id on every read, so a tampered ciphertext block fails there.
Adding a block-layer tag would require a nonce-and-tag table and a larger format
change for a property the system already has.

The superblock, the intent log, the genesis record and the allocation snapshot
stay plaintext because the feature flag that says a volume is encrypted lives in
the superblock and must be readable before a key exists. Contents are protected;
metadata is not. The host drives the lifecycle: provision at format time, open
with a credential, add a slot, revoke a slot, and change a passphrase. There is
no master-key rekeying, which would generate a new master key and re-encrypt the
object region.

### 3.8 Decisions and consequences

| decision | what it buys | what it costs |
|---|---|---|
| No write-ahead log; the commit is a write order | One recovery path, no replay, and a superblock that is the only commit point | Two barriers per commit; an interrupted commit leaves unreachable allocated blocks |
| Immutable, content-addressed objects | Verification on read; older roots stay readable; a volume is portable bytes | Every write rewrites the path; appending to one growing file rewrites its extent node, which is quadratic in device bytes; no in-place update |
| Allocator snapshot in a fixed block | Allocation state is recoverable without walking the object tree | The snapshot is rewritten every commit and is plaintext on an encrypted volume |
| Application-selected durability | Per-record cost falls toward memory speed; the only durability mechanism remains an ordinary commit | Staged data is volatile before the seal; a seal pays for the file's history |
| Metadata stays plaintext on encrypted volumes | The volume can be identified and unlocked before a key exists | Allocation state and file-system structure are visible at rest |
| XTS with no per-block tag | No nonce-and-tag table; integrity is already provided by object hashes | Confidentiality only at the block layer; integrity depends on the object layer; metadata is unprotected |
| One writer, no VFS | A simple concurrency and crash model, and a library that embeds anywhere | No multi-writer volume; no host path resolves into a volume without a service layer |
| Fixed structural limits | Predictable encoding and bounded memory | A 400-entry directory does not split; object and name ceilings are errors, not slowdowns |
| No space reclamation in the shipping path | The allocator and recovery stay simple, with no collector to interact with a commit | A volume fills monotonically; it is an append-only store with a planned lifetime |

## 4. Implementation

### 4.1 Crates

| crate | runtime | contents |
|---|---|---|
| [`ledgerfs`](kernel/) | `no_std` + `alloc` | the format: record framing, canonical encoding, the object model, the allocator, placement, the commit engine and the copy-on-write operations. No I/O. |
| [`ledgerfs-host`](host/) | `std` | an `ObjectStore` over a `BlockDevice`, mount and path handling, `Store` (format, open, mutation verbs, staging, encryption), and the `lfs-read` and `lfs-write` tools. |

The workspace also contains `pending/`, a separate unpublished crate holding
the Tier B subsystems, and `benchmarks/`, the measurement harness. Neither is
part of the released API. Two crates make up the release; publication to
crates.io is a separate step governed by [`RELEASING.md`](RELEASING.md) and has
not happened.

### 4.2 One format, two runtimes

The format crate owns every byte and the host re-derives no offset or checksum.
The seam is `ObjectStore`: the format asks for an inode, a directory node, an
extent node or a chunk, and the implementation decides where those bytes live.
The host implements the trait in `host/src/objstore.rs` over a four-method
`BlockDevice` (size, positioned read, positioned write, barrier).
`MemBlockDevice` is that device over a `Vec<u8>`; a kernel implements the
`ObjectStore` trait over the disk it owns. The kernel's `CommitEngine` has its
own two-method block seam (`write_blocks` and `storage_barrier`) for a writer
that owns placement.

| kernel feature | effect |
|---|---|
| *(none)* | the format and the copy-on-write operations, `no_std` + `alloc` |
| `std` | pulls the LZ4 and Zstd decoders that `compression` gates, so a record written under either policy decodes |
| `reader` | compiles the ATA PIO path and its port-I/O instructions. Ring-0 code, x86-64 only, off by default |
| `data-encryption` | transparent XTS-AES-256 over a volume's raw blocks, applied below the object model and selected when the volume is formatted |
| `temper_sealing` | ML-DSA-65 and SLH-DSA sealing of audit records, through `temper-rs`. Off by default; no caller in this repository |

Both crates declare Rust 1.89 and edition 2024. The CI matrix builds every
declared kernel feature, the host suite on Linux, the host targets on Windows as
a compile gate, the `pending/` suite, the 1.89 floor, and the benchmark harness.
It also fails if a feature is declared and left out of the matrix. The benchmark
harness requires 1.90 because `redb` does.

### 4.3 Verification

The test suite is organized by property rather than by module. `durability.rs`
sweeps crash prefixes and pins the commit count of each verb. `staging.rs` pins
the two read tiers, the single-commit seal and the pruning of intermediate
generations. `readpath.rs` pins the exact device reads of a lookup. `encryption.rs`
covers slot provisioning, unwrap, revocation and passphrase change.
`security_paths.rs` and `security_crafted.rs` drive malformed and adversarial
paths. `kernel/src/superblock.rs` carries the mount-selection tests, including
split brain and the in-flight report. The Windows CI job compiles every test
target but does not run the behavior suite, because its lock semantics differ;
`KNOWN_ISSUES.md` §13 states why.

## 5. Deployment

CrucibleOS is the reference system for which the format is designed. Its
filesystem service, boot path and board support are CrucibleOS code and are not
part of this repository; the OS-side integration is a separate consumer and is
not covered by this release's tests. The standalone `ledgerfs-host` crate is the
application path: it supplies the format over a file, a partition or a memory
buffer.

That division is the intended shape of adoption. LedgerFS supplies the format
and the commit protocol; the adopter supplies the policy above them. An
operating system supplies a file-descriptor service and access checks. A
database engine would sit one level higher and supply a schema, indexes and a
query executor. Neither layer is part of this release, and the format does not
obstruct either.

## 6. Evaluation

### 6.1 Method

The figures below were produced by the harness in [`benchmarks/`](benchmarks/)
and recorded on 2026-10-23 on one machine, with a dm-crypt mapping over an NVMe
device, so every engine pays the same encryption cost. The machine identification
in the record is redacted. The harness refuses to run on a filesystem whose
writes do not reach a device, because a barrier on tmpfs measures nothing.

The comparison engines are SQLite 3 in WAL mode with `synchronous=FULL`, `redb`
with `Durability::Immediate`, and a control that writes one file per key and
synchronizes both the file and its directory. The control is the relevant
baseline for durability cost: it issues the same kind of operation a commit
issues, with no filesystem on top. Each repetition performs the same work and
writes different keys. Nothing is overwritten, because LedgerFS frees nothing.
The figure is the median of per-repetition means.

### 6.2 Results

| workload | LedgerFS | SQLite WAL | redb | raw `fsync` |
|---|---:|---:|---:|---:|
| Durable insert, 256 B | 9.18 ms | 3.79 ms | 4.13 ms | 7.10 ms |
| Verified point read, 256 B | 25.5 µs | 4.2 µs | 949 ns | 2.1 µs |
| Batched append, 960 B per batch | 16.87 ms | 9.25 ms | 9.87 ms | 9.09 ms |
| Staged append, seal every 10 batches | 2.05 ms | 1.25 ms | 981 µs | 869 µs |
| Deferred seal, one group sync | 1.07 ms | 397 µs | 161 µs | 126 µs |

The staged and deferred rows report per-batch cost with the seal included,
since their durability point is the seal rather than the batch. Every such row
is marked in `BENCHMARKS.md`. The engines do not perform identical work at the
seal: LedgerFS flushes its objects at the seal, SQLite leaves them in the WAL,
redb leaves them mapped and the control leaves them in the page cache. No row
should be compared across engines without that statement.

### 6.3 Interpretation

**Barriers dominate small durable writes.** A barrier that follows new data
costs 3 to 5 ms on this device, and the single-insert row moves between runs by
more than a factor of two. Against the control, which pays the same two
synchronizations, LedgerFS costs 1.29 times as much. The remainder is the commit
itself: a fresh allocator snapshot, four superblock copies and a copy-on-write
replacement of every object on the path. SQLite and redb synchronize once per
commit; LedgerFS synchronizes twice because the first synchronization is what
removes the log.

**Reads verify their own result.** The 25.5 µs point read includes a BLAKE3
computation over the returned object and one block read per object on the path:
seven for the two-level path fixed by a test, and two more for the three-level
`/kv/<dir>/f<key>` path this workload reads. SQLite and redb trust their pages.
Per batch read on the 960-byte stream, LedgerFS costs 3.2 µs against SQLite's
2.7 µs, redb's 1.4 µs and the control's 150 ns. Over the full 480 KiB stream the
verified read-back is within about 1.2 times SQLite's unverified read, which is
the price of establishing that the recording is intact rather than presuming
it.

**Volume consumption follows commits, and a commit pays for what it allocates.**
A 256-byte insert consumes 36.3 KiB of volume and a batch of twenty 48-byte
records consumes 35.9 KiB, most of it the allocator snapshot, the four
superblock copies and the rewritten path rather than the payload. A 64 KiB
append consumes 93.9 KiB. A commit that creates two hundred files consumes
12.2 KiB per insert because the fixed overhead is shared and each file
contributes a chunk, an inode and an extent node. Capacity planning follows the
number of commits and the number of objects they allocate.

**Staging changes the order of magnitude of the per-record cost.** At a seal
every ten batches a staged append costs 2.05 ms per batch against 16.87 ms for a
committed batch of the same bytes, a difference of about eight times. A staged
batch consumes 7.3 KiB of volume over the run, 73 KiB per seal, because only
the seal consumes volume and each seal rewrites the extent node that grew since
the previous one. At a seal every 512 batches the harness reports 37.9 µs per
batch, which measures the in-memory append with the seal's share set aside;
that interval is not the one recorded in Appendix A of `BENCHMARKS.md`. The
deferred-seal row removes one of the two barriers per group, at the cost of a
weaker crash promise: the group since the last sync is lost whole, and blocks
may be marked allocated without a live root naming them, which leaks space on a
recovery and never produces a wrong answer.

### 6.4 What is not measured

- **ZFS, Btrfs and WAFL.** No head-to-head comparison exists, so the chapter's
  I/O-count claim is not evaluated here.
- **Power-fail injection.** Crash consistency is tested by mounting byte
  prefixes; it is not tested against a controller that reorders or discards
  writes.
- **Concurrency.** One writer and one reader, without contention. The
  single-writer rule is a design constraint, not a benchmark condition.
- **Cold reads.** Every read row is warm, measured in the process that wrote the
  data. A process that opens a volume it did not write pays a linear scan for
  the first lookup of each object, because storage hints are not persisted
  (`KNOWN_ISSUES.md` §4).
- **Volumes larger than memory.** Every measurement fits in the page cache once
  warmed. A larger volume is dominated by the scan fallback, by a wider margin
  than these figures show.
- **Other hardware.** The results come from one machine. The harness should be
  run on the target hardware before a figure is quoted for it.

## 7. Threat model

### 7.1 In scope

- **Torn and partial writes.** A commit cut short at any point, including
  mid-superblock, leaves the previous root or the new one readable, never a
  mixture.
- **Stale and wrong accelerators.** A persisted hint or a sidecar cache is
  treated as a claim, re-verified against the block it names and abandoned on a
  mismatch. A wrong hint costs a read, never a wrong answer.
- **Content corruption.** Every object is checked against its identifier and
  its record checksum before it is returned. A bad block is an error, not a
  different version of the object.
- **Theft of the medium at rest.** With keyslots provisioned, the object
  region's contents are not recoverable from an image of the disk without the
  passphrase or the supplied key-encryption key.

### 7.2 Out of scope

- **An attacker with the credential and the disk.** Keyslot wrapping raises the
  cost of guessing a passphrase; it does not resist someone who already knows
  it.
- **A whole-volume rollback.** A complete older volume that is internally
  consistent verifies and mounts. There is no external monotonic anchor and no
  root seal in these crates, so rollback is not detected. The commit sequence is
  internal to the volume and does not defend against an attacker who can replace
  the whole volume.
- **A malicious drive or controller.** The format assumes barriers mean what
  they are specified to mean. It does not use T10 protection information or
  NVMe end-to-end data protection, and it cannot detect a device that returns
  stale but internally consistent data.
- **Metadata confidentiality.** On an encrypted volume the superblock, the
  intent log and the allocation snapshot stay plaintext, so file-system
  structure and aggregate allocation state are visible.
- **Process isolation and side channels.** The cipher protects data at rest. It
  is not a live isolation boundary, and no side-channel analysis is claimed.
- **Multi-writer safety.** At most one writer per volume is supported. The
  file device takes an exclusive OS lock; on Unix it is advisory, and on Windows
  it is mandatory and refuses a second handle's read (`KNOWN_ISSUES.md` §13).
- **Availability.** No quota, rate limit or denial-of-service resistance is
  implemented.

## 8. Limitations

- **Space is not reclaimed.** No shipping path frees a block. A volume fills
  monotonically and should be treated as append-only with a planned lifetime.
  Garbage collection, snapshots, deduplication, the scrubber and content-defined
  chunking are implemented and tested in the unpublished `pending/` crate, and
  none is driven by the shipping path. `Store::rebuild_into` and `lfs-write
  compact` copy the live tree into a new volume, dropping dead objects; that is
  a rebuild, not an in-place collector.
- **Appending to one growing file is quadratic in device bytes.** Every commit
  rewrites the file's extent node, whose encoding grows with the file. A staging
  session removes the per-append rebuild, but each seal still rewrites the node
  that grew since the previous seal. Bounded chunk files restore linear cost.
  Extent nodes split at 1500 entries; a file larger than the 256 KiB object
  ceiling is stored as multiple chunks.
- **A directory holds at most 400 entries and does not split.** The limit is
  `DirNode::MAX_ENTRIES`; the 401st name is an error, not a slowdown. An
  application must shard its own namespace. Object payloads are at most 256 KiB,
  directory entry names at most 256 bytes, and the root locator ring holds 56
  entries.
- **No in-place update.** `write_at` may append at or past the end of a file but
  cannot rewrite stored bytes. A file is replaced whole or extended.
  `write_file` builds the new content and repoints the entry in one transaction.
- **One writer per volume.** A writable file device takes an exclusive lock.
  Readers take none and read the root they opened. On Unix the lock is advisory
  and a reader coexists with a live writer; on Windows it is mandatory and a
  second handle's read is refused.
- **No VFS integration.** Nothing registers with the host operating system, so
  no host path resolves into a volume. An OS adopts the library by writing the
  service layer above it.
- **Encryption protects contents, not metadata.** The object region is
  XTS-AES-256; the superblock, intent log and allocation snapshot stay plaintext.
  The passphrase KDF is iterated BLAKE3 and not memory-hard. Losing every
  credential loses the volume: there is no recovery path and no back door.
  Master-key rekeying is not implemented.
- **Rollback is not detected.** A whole older volume that is internally
  consistent mounts normally, because there is no external counter or root seal
  in these crates.
- **Superblock copies are not repaired.** The format requires a conflicting
  pair to be reported rather than guessed. `lfs-read info` reports copy status;
  the host does not reconcile copies.
- **The intent log is not written on the live path.** The ring and its parser
  exist, but a volume produced by this release has no intent records and no
  replay step.
- **Tier B subsystems are not available.** In addition to the `pending/` set,
  replication, the audit chain and record sealing are present in the kernel
  crate with no shipping caller. Four subsystems return success without
  performing the work; `KNOWN_ISSUES.md` §11 names them. Quotas, ACLs, disaster
  recovery, capability enforcement, reflink clones, RAID, IPC and in-place
  format upgrades have no implementation at all.
- **No root seal, vault, per-file encryption or boot reader.** Those belong to
  the CrucibleOS deployment and are not in these crates. Section 2.2 records the
  boundary.
- **No formal model and no power-fail testing.** The crash argument is stated
  and tested by prefix mounting, not model-checked. There is no TLA+ model.
- **Cold reads scan.** Storage hints are not persisted, so a process that opens
  a volume it did not write performs a linear scan for the first lookup of each
  object. The read benchmarks are warm.
- **Windows is a compile gate.** The host crate builds and its test targets
  compile on Windows, but the behavioral suite runs on Linux because the file
  lock is mandatory on Windows.

## 9. Related work

**Copy-on-write filesystems.** ZFS [1], Btrfs [2] and WAFL [3] established the
immutable-tree model that LedgerFS shares: a new root names new objects, and
older roots remain readable. LedgerFS differs in scope. It has no storage pool,
no RAID, no send/receive and no replication on the shipping path. It obtains
crash consistency by ordering writes around a superblock commit rather than by
a ZIL or a log tree, which removes replay from its mount path but does not by
itself make it faster; Section 6 does not measure those systems.

**Embedded storage engines.** SQLite and redb [4] are the measured alternatives
in Section 6. SQLite provides a query surface, and both engines support in-place
update; LedgerFS provides neither. The comparison is about the cost of a
durability point and of a verified read, not a claim of general superiority. For
a capture workload the two can be composed: records land in LedgerFS with a
verified read-back, and are exported to a query engine when the workload
becomes a query.

**Block-device encryption.** LUKS [5] provides volume-level encryption with a
flat key-slot structure. LedgerFS uses the same broad shape with two slots and
XTS-AES-256, and adds no per-file key hierarchy. The research chapter's vault
and per-file encryption are not in this release.

**Content-addressed storage.** IPFS [6] shares content addressing and Merkle
DAGs but is a networked storage protocol, not an embedded local volume. LedgerFS
makes no network claim. eCryptfs [7] and fscrypt [8] provide per-file encryption
above an existing filesystem; this release does not implement that layer.

**Trust anchors.** TPM 2.0 [9] provides a hardware-bound key hierarchy. It is
not used here, and the boot-time root seal that the research chapter describes
is not part of these crates.

**Primitives.** Object identity uses BLAKE3 [10]. Volume encryption uses
XTS-AES [11].

### References

1. Bonwick, J., et al. *ZFS: The Last Word in File Systems.* Sun Microsystems,
   2005.
2. Rodeh, O., et al. *BTRFS: The Linux B-tree Filesystem.* ACM Transactions on
   Storage, 2013. DOI: 10.1145/2501620.2501623.
3. Hitz, D., et al. *File System Design for an NFS File Server Appliance.*
   USENIX Winter 1994.
4. SQLite and redb are the embedded engines measured in
   [`BENCHMARKS.md`](BENCHMARKS.md); their versions are pinned by
   `Cargo.lock`.
5. Fruhwirth, C. *LUKS: Linux Unified Key Setup.* 2004.
6. Benet, J. *IPFS: Content Addressed, Versioned, P2P File System.*
   arXiv:1407.3561, 2014.
7. Halcrow, M. A. *eCryptfs: An Enterprise-Class Encrypted Filesystem for
   Linux.* Linux Symposium, 2005.
8. Google LLC. *fscrypt: Filesystem-Level Encryption for Linux.* 2018.
9. Trusted Computing Group. *TPM 2.0 Library Specification.* ISO/IEC 11889,
   2019.
10. Aumasson, J.-P., et al. *BLAKE3: One Function, Fast Everywhere.* 2020.
11. IEEE. *IEEE Standard for Wide-Block Encryption for Shared Storage Media.*
    IEEE Std 1619-2019 (XTS-AES).

## 10. Applicability

The design is intended for records that are written once, retained for a bounded
period, and read back whole or by byte range: logs, capture streams, archives,
appliance state, and the file tree of a system that owns its disk.

Within that class, a volume can occupy the position a database would: it stores
an application's records and returns them, and for a capture workload that is
the complete requirement. What it does not provide is a database engine, meaning
a schema, secondary indexes, a query planner and an executor.

It is not intended for data edited in place, for workloads that require
cross-record queries, or for volumes that must run indefinitely without
attention.

| suitable | unsuitable |
|---|---|
| append-only capture and logging | a query engine: no schema, no indexes, no joins |
| portable archives and snapshot-by-copy | data rewritten in place |
| an operating system's own file tree | a shared volume with many writers |
| an application store with atomic publication | a volume that runs forever without attention |

The absence of a query layer is a scope boundary, not a claim about the floor of
the design. A catalog, secondary indexes, a query planner and an executor are a
layer above this one, and the format does not obstruct them. That layer is not
part of this release.

## 11. Availability and reproducibility

### 11.1 Using the crates

The crates are not yet on crates.io. Until the first release, depend on the
repository:

```toml
[dependencies]
ledgerfs-host = { git = "https://github.com/Sing-Security/ledgerfs" }
```

A consumer that owns its own block storage and wants the format without the host
layer can depend on `ledgerfs = { git = "https://github.com/Sing-Security/ledgerfs" }`
instead. The manifests declare `ledgerfs` 0.7.0 and `ledgerfs-host` 0.1.0.

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
	assert_eq!(store.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	let _seal = store.seal_staged()?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	Ok(())
}
```

`FileBlockDevice` in place of `MemBlockDevice` drives a file or a partition with
the same calls. The full code is pinned by `host/tests/readme_examples.rs`.

### 11.2 Build and test

Both released crates build on stable Rust 1.89, edition 2024. The benchmark
harness requires 1.90, because `redb` does.

```sh
cargo test -p ledgerfs --all-features
cargo test -p ledgerfs-host
cargo test -p ledgerfs-pending
cargo run --release -p ledgerfs-benchmarks -- run
cargo run --release -p ledgerfs-benchmarks -- render
```

The harness records each run as a JSON file under `benchmarks/results/`. The
tables in `BENCHMARKS.md` are regenerated from the newest file and therefore
cannot drift from a recorded run. The machine identification in those records is
redacted.

### 11.3 Documents

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

### 11.4 Examples

| example | subject |
|---|---|
| [`embed`](host/examples/embed.rs) | in-memory volumes, snapshotted and reopened |
| [`disk_volume`](host/examples/disk_volume.rs) | a file-backed volume, the writer lock, a concurrent reader |
| [`app_store`](host/examples/app_store.rs) | the application-store contract from `EMBEDDING.md` |
| [`staging_capture`](host/examples/staging_capture.rs) | both read tiers during a session, then a seal |
| [`custom_device`](host/examples/custom_device.rs) | the four-method `BlockDevice` contract |

## 12. Guarantees

- **Atomic publication.** One transaction per verb, one commit as the durability
  point. A crash leaves the previous root or the new one, never a blend. A
  staged session is published by one commit of the same shape.
- **Explicit durability.** A commit is durable when it returns. A seal is
  durable when it returns. A deferred seal is durable at `sync`. Before a seal,
  the staged data exists only in the writing process and a crash loses exactly
  that.
- **Content verification.** Every stored object is named by the hash of its
  canonical encoding and every record is checksummed. A read checks the framing,
  the checksum and the identifier before it answers, and does so on every typed
  accessor: `Inode`, `DirNode`, `ExtentNode` and `Chunk`.
- **Reader isolation.** A reader that opened an older root continues to read
  consistent data while a writer commits newer ones, because no object is
  modified in place.
- **Single writer.** Enforced by an exclusive OS lock on a writable file
  device. The lock is advisory on Unix and mandatory on Windows.
- **Portability.** A volume is a byte string. A copy is a snapshot, an archive
  is the volume, and reopening it requires no service.

## 13. License

Dual-licensed: Apache-2.0 or MIT, at your option. See
[`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT) and
[`NOTICE`](NOTICE).
