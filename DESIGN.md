# LedgerFS design

This document explains why LedgerFS is built the way it is: the object model,
the commit protocol and its crash argument, the allocator, the staging tier, the
encryption layers, the threat model and the relation to the wider CrucibleOS
research programme. [`FORMAT.md`](FORMAT.md) is the normative byte layout and
takes precedence over this document wherever the two disagree. The contract an
embedding application follows is in [`EMBEDDING.md`](EMBEDDING.md), the known
behavioural limits are in [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md), and the measured
costs are in [`BENCHMARKS.md`](BENCHMARKS.md).

## 1. The research question

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
and the boot-time reader belong to CrucibleOS, are not in these crates and are
not covered by this repository's tests. Section 10 records that boundary claim by
claim.

Within the storage layer, the release answers four questions:

- **Q1. Can crash consistency be obtained by ordering alone**, without a
  write-ahead log and without replay at mount?
- **Q2. Can every read verify the content it returns**, so that a corrupted
  block or a substituted object is an error rather than a silent answer?
- **Q3. Can an application choose when its writes become durable**, without a
  second recovery path and without a new on-disk state?
- **Q4. Can one format implementation run in a kernel and in a userspace
  process**, over devices as different as a disk and a heap buffer?

The answer to each is yes within the stated scope, and the measured cost of the
guarantees is in [`BENCHMARKS.md`](BENCHMARKS.md).

## 2. Object model

A volume is a flat array of 4 KiB blocks. Above the block layer everything is an
object, and every object is immutable and named by the 32-byte BLAKE3 hash of
its canonical encoding. Two objects with equal content have the same identifier,
and a reader recomputes the identifier before accepting the bytes.

The format has five object kinds: `Chunk` for file content, `Inode` for file
metadata, `DirNode` for one directory's sorted entries, `ExtentNode` for a
file's extent list, and `Root`, which names the tree. `Root` is a descriptor
rather than a stored record: `Root::file_tree_root` holds the reference to the
root directory's `Inode`, and the superblock names that reference directly. The
four kinds that occupy object space are `Chunk`, `Inode`, `DirNode` and
`ExtentNode`.

`ObjectRecord` framing adds a magic, a length and a truncated BLAKE3 checksum,
so framing damage is detected before decoding. The full hash is the identity;
the truncated checksum catches a torn write or a bit flip and is not a
collision-resistant name. Chunks above 4 KiB are compressed with LZ4 when it
saves at least ten percent; the identifier is over the logical payload, and
verification is performed after decoding.

Immutable objects buy three properties. A read can verify what it returns
against a name the caller already trusts. An older root remains readable while a
writer publishes newer roots, because no object is modified in place. And a
volume is portable bytes, so a snapshot is a copy and an archive is the volume.
The cost is that every write rewrites the path from the root to the changed
node, and that in-place update is impossible by design.

Exactly one root is live at a time. The superblock names it together with a
monotonic `commit_seq`. A modification produces new generations of every object
on the path from the root to the changed node. The previous generations remain
on the medium and remain valid for a reader that already adopted them.

## 3. Commit protocol and crash argument

A commit writes the new objects and the allocator snapshot, issues a barrier,
writes the four superblock copies that name the new root, and issues a second
barrier. The superblock write is the commit point: before it the previous root
is the volume, after it the next root is. A crash therefore leaves the previous
root or the new one, never a blend, and a mount performed by this implementation
has no log to replay. [`FORMAT.md`](FORMAT.md) §15 states the order normatively.

The two barriers are load-bearing and are not an optimization target. The first
guarantees that every object the new root can reach is on the medium before any
superblock names it. The second guarantees that the superblock write itself
reaches the medium before the commit returns. Removing either reopens the crash
window the order exists to close. Speed comes from amortizing barriers across
many writes, which is what the staging tier does, not from dropping one.

The commit leaves one deliberate exposure. The allocator snapshot is written
before the superblock, so a crash between them can leave blocks marked allocated
that no live root names. The result leaks space and never frees a block a live
root can reach, so it cannot produce a wrong answer. The alternative, writing the
snapshot after the superblock, would let a live root reference blocks the
allocator believes are free.

The format also defines an intent-log ring and a parser for it. Neither is on the
live path: the host mount calls `mount_select` without a ring, so it has no
replay step at all. The ring is two blocks, not a multi-megabyte region, so it
cannot grow into a write-ahead log by accident. It exists so that a future writer
can describe an in-flight commit, and no volume this release produces records
intent entries.

## 4. Allocation

A block allocator tracks free, allocated and reserved blocks. Its serialized
snapshot occupies a fixed block, block 5, rather than an ordinary object,
because storing allocator state through the allocator would consume a block and
change the state being written. The same fixed-location trick is what allows the
superblock to describe itself.

On a fresh volume the allocator initialisation and the boot-time object scan must
agree about the reserved regions: the first six blocks and the last two are never
object space. If they disagree, the first commit can place an object on top of a
superblock that a later write overwrites, orphaning it permanently. Both
constants and the reason they are load-bearing are stated in
[`kernel/src/lib.rs`](kernel/src/lib.rs) and in [`FORMAT.md`](FORMAT.md).

The shipping path never frees a block. Garbage collection, snapshots,
deduplication and the scrubber are implemented and tested in the unpublished
`pending/` crate, and nothing in the shipping path calls them. A volume fills
monotonically, and capacity planning follows the number of commits and the
number of objects they allocate. `Store::rebuild_into` and `lfs-write compact`
copy the live tree into a new volume and drop dead objects; that is a manual
rebuild, not an in-place collector.

## 5. Mount and root selection

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

## 6. The staging tier

Staging separates accepted writes from durable writes. `begin_staging` opens a
session, `staged_append` extends an in-memory draft of the target file, and
`seal_staged` builds the objects those appends imply and commits them once. A
staged append touches no device block, no allocator state and no barrier.

The read tier is explicit. The handle that owns the session resolves its own
staged writes from memory with no device read, and every other handle sees the
committed generation. `read_file_committed` on the owning handle also answers
from the committed generation. The choice is per read, and there is no ambient
default.

Intermediate generations are pruned at the seal, so N staged appends carry N
chunks plus one generation of each metadata object to the medium. A commit per
append would carry a generation per append instead. The seal itself is an
ordinary commit of the existing shape, at a sequence number one greater than the
committed one.

`seal_staged_deferred` performs the same work except the superblock write, so
several seals can share one `sync`. The group since the last sync is published
whole or lost whole. A deferred group is otherwise the same commit: its objects
and allocator snapshot are on the medium when the seal returns, and the
superblock is what `sync` adds.

A crash before the seal loses the staged work and nothing else. During the seal
the old-or-new rule applies. After the seal returns, the data is an ordinary
committed generation. No format change is involved, and a mount that never
observed a session cannot distinguish a volume written with one. Staged data has
no on-disk representation before the seal; it exists only in the writing
process.

Two costs remain inside a session. In-memory CPU is quadratic in the number of
appends to one file, because each staged operation rebuilds the current extent
node, though device bytes are linear. And the per-object ceilings bind at the
seal: a file that would exceed the 256 KiB object ceiling or the extent-node
entry limit must be sealed earlier.

## 7. Encryption

Encryption has two deliberately separate layers. Keyslots wrap a randomly
generated 64-byte master key once per slot. The block cipher applies
XTS-AES-256 below the object model, so exactly the disk path sees plaintext and
everything above it, including content addressing and the extent tree, never
learns that encryption exists.

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

## 8. Decisions and consequences

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

## 9. Threat model

### 9.1 In scope

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

### 9.2 Out of scope

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
  it is mandatory and refuses a second handle's read
  ([`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) §13).
- **Availability.** No quota, rate limit or denial-of-service resistance is
  implemented.

## 10. Relation to the CrucibleOS research chapter

The research chapter and this release describe different scopes. The chapter is
a statement of the wider CrucibleOS programme; this repository is the storage
layer released on its own. The table records the boundary so that a claim is not
carried across it by accident.

| chapter claim | status in this repository |
|---|---|
| **C1. A dual-PQC root seal in a dedicated superblock block binds the content tree to the disk geometry and is verifiable by a minimal boot reader.** | Not in these crates. This format has no root seal. Block 5 is the allocator snapshot, not a seal record. The kernel crate contains `seal_record`, the ML-DSA-65 and SLH-DSA record-sealing module, behind the off-by-default `temper_sealing` feature, and no shipping path in this repository calls it. |
| **C2. A TxnPlan five-phase commit with deterministic pre-allocation uses fewer I/O operations than a ZFS ZIL or a Btrfs log tree.** | The in-memory `TxnPlan` and `CommitEngine` exist and are exercised. The live commit is the two-barrier ordered write in [`FORMAT.md`](FORMAT.md) §15.1: objects and allocator snapshot, barrier, four superblock copies, barrier. It writes no checkpoint and no intent-log records. The I/O-count comparison with ZFS or Btrfs is not evaluated here, and this repository has no such measurement. |
| **C3. A vault Merkle tree over encrypted DataKey entries enables partial key-hierarchy verification with logarithmic proofs.** | Not in these crates. There is no vault and no per-file key hierarchy. Encryption is volume-level XTS-AES-256 with two flat keyslots; a slot is passphrase-derived or sealed under a supplied 32-byte key-encryption key. |
| **C4. A dual-layer mount resolves a kernel image from a cold disk in under 100 ms.** | Not in these crates, and no boot timing is measured. The `reader` feature compiles a ring-0 ATA PIO path for a consumer that is the only route to its own partition. It is a read path, not a measured boot mount with a latency budget. |
| **C5. Content addressing provides measurable deduplication savings without a separate index.** | Not claimed and not measured. Content addressing gives identical content an identical object id, but the shipping path has no deduplication index and no merge pass, and no savings figure exists. Deduplication is implemented in `pending/` with no caller. |

The chapter is a research statement, not the specification of this release. The
byte layout and the behaviour of the released crates are specified by
[`FORMAT.md`](FORMAT.md) and by the code in this repository.

## 11. Verification

The test suite is organized by property rather than by module. `durability.rs`
sweeps crash prefixes and pins the commit count of each verb. `staging.rs` pins
the two read tiers, the single-commit seal and the pruning of intermediate
generations. `readpath.rs` pins the exact device reads of a lookup.
`encryption.rs` covers slot provisioning, unwrap, revocation and passphrase
change. `security_paths.rs` and `security_crafted.rs` drive malformed and
adversarial paths. `kernel/src/superblock.rs` carries the mount-selection tests,
including split brain and the in-flight report. The readme examples are pinned by
`host/tests/readme_examples.rs`, so a landing-page snippet that stops compiling
fails CI.

The Windows CI job compiles every host test target but does not run the
behavioural suite, because the file lock is mandatory there and several tests
encode Unix advisory-lock semantics. [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) §13
states the limitation.

## 12. Related work

**Copy-on-write filesystems.** ZFS [1], Btrfs [2] and WAFL [3] established the
immutable-tree model that LedgerFS shares: a new root names new objects, and
older roots remain readable. LedgerFS differs in scope. It has no storage pool,
no RAID, no send/receive and no replication on the shipping path. It obtains
crash consistency by ordering writes around a superblock commit rather than by
a ZIL or a log tree, which removes replay from its mount path but does not by
itself make it faster; [`BENCHMARKS.md`](BENCHMARKS.md) does not measure those
systems.

**Embedded storage engines.** SQLite and redb [4] are the measured alternatives
in [`BENCHMARKS.md`](BENCHMARKS.md). SQLite provides a query surface, and both
engines support in-place update; LedgerFS provides neither. The comparison is
about the cost of a durability point and of a verified read, not a claim of
general superiority. For a capture workload the two can be composed: records
land in LedgerFS with a verified read-back, and are exported to a query engine
when the workload becomes a query.

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

## 13. Applicability

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
