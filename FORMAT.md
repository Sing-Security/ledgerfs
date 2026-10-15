# LedgerFS — On-Disk Format

This is the normative description of the bytes LedgerFS writes to a volume, and
of the rules a reader must follow to interpret them. It describes what the code
in `kernel/` and `host/` actually does. Where the code and this document
disagree, the code is right and this document is a bug.

## Normative status

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are
to be read as described in RFC 2119.

Every section number below is **stable**. Source comments throughout the crates
cite them — `record.rs` starts with `Per §7 and §9`, `keyslot.rs` explains why
`KdfParams` carries an `algorithm` field with reference to §36, and so on — so
renumbering would break the code's own documentation. Section numbers are held
even where the section is not implemented.

A section headed **Reserved** holds a number for a capability that this
implementation does not have. A Reserved section is normative only in the
negative sense: it tells you the bytes or the operation it describes are not
produced or accepted by this code, so you cannot rely on them. Nothing in a
Reserved section should be implemented from this document alone; it exists so
that a citation resolves and a reader is not left guessing.

## Reading this document

- Numbers in this document come from the code. Constants are named as they are
  named in `kernel/src/lib.rs` or in the module that defines them.
- Layouts are given with byte offsets within a 4096-byte block unless stated
  otherwise. All integer fields are **little-endian**.
- `§N` inside the code means a section of *this* document.

## Glossary

| Term | Meaning |
| --- | --- |
| **Object** | An immutable, content-addressed blob. Its identity is the BLAKE3 hash of its canonical encoding. |
| **ObjectId** | A 32-byte BLAKE3 hash (`BLAKE3_HASH_SIZE = 32`), the name of an object. |
| **ObjectRef** | An ObjectId plus a `StorageHint` — where the object is believed to live. A hint may be zero and may be wrong; it is never authoritative. |
| **Root** | The object that names everything else. There is exactly one live root per commit. |
| **Commit** | One transaction's worth of changes, made durable by the superblock write that names its root. |
| **RLR** | Root Locator Ring — a ring of recently committed roots inside the superblock. |
| **Intent log** | A ring buffer of records describing an in-flight commit, used to decide what to replay on mount. |
| **Deadlist / PendingFree** | Blocks that are no longer reachable from the current root and are candidates for freeing. |
| **Barrier** | A durability operation: everything written before it is on stable media before anything after it is. |

---

## 1. Naming convention

The format's on-disk and in-code names follow the original working name of the
project. The three magics are the correspondence table:

| Constant | Value | ASCII |
| --- | --- | --- |
| `SUPERBLOCK_MAGIC` | `0x5355504552424C4B` | `SUPERBLK` |
| `OBJECT_RECORD_MAGIC` | `0x004C454745524653` | `\0LEGERFS` |
| `INTENT_LOG_MAGIC` | `0x494E54454E544C47` | `INTENTLG` |

The leading NUL in the object-record magic is intentional: an all-zero block
cannot be mistaken for a valid record header regardless of how it is read.

## 2. Design philosophy

Three properties are load-bearing, in this order:

1. **The commit is the durability point.** A write is durable when the
   superblock naming its root is durable. There is no separate log to replay
   before data is considered committed, and no write-ahead log whose *absence*
   would lose a committed write.
2. **Nothing written is ever modified in place, except the superblock copies.**
   Every commit writes new objects and leaves the old ones where they were. That
   is what makes an older root still readable, and it is the reason free space
   falls monotonically until garbage collection runs (§22).
3. **A reader verifies what it is handed.** Content addressing means a block
   that does not hash to the id it was fetched for is not "the wrong version" —
   it is not the object at all. Hints, caches, and superblock accelerators are
   all allowed to be wrong because this check exists.

## 3. Threat model

### 3.1. In scope

- **Torn and partial writes.** A commit cut short at any point, including
  mid-superblock, MUST leave either the previous root or the new one readable,
  never a mixture. §10.3 and §15 define what "cut short" is allowed to mean.
- **Stale and wrong accelerators.** A persisted hint (§20) or a sidecar cache is
  treated as a claim, re-verified against the block it names, and abandoned on
  a mismatch. A wrong hint costs a read, never a wrong answer.
- **Theft of the medium at rest.** With keyslots provisioned (§36), the data
  volume's contents MUST NOT be recoverable from an image of the disk by an
  attacker who does not have the passphrase or the HSM.

### 3.2. Out of scope

- **An attacker with the passphrase and the disk.** Keyslot wrapping raises the
  cost of guessing a passphrase; it does not resist someone who already knows it.
- **A running system's process isolation.** The cipher is applied below the
  object model to protect data at rest, deliberately not as a live isolation
  boundary. That is a separate mechanism's job and is not in this format.
- **Bit rot without verification.** The format supports detection (§40), but
  nothing in this release reads the volume in the background.
- **Concurrent writers.** See §12.1.

## 4. Core invariants

A conforming implementation MUST maintain all of the following. Each is stated
because the code depends on it somewhere that is not obvious.

1. **Object identity is content.** For every stored object, `blake3(payload)`
   equals the ObjectId it is named by. Payload here means the *logical* bytes,
   before any compression (§28).
2. **Objects are immutable.** Once an ObjectId has been written, the bytes
   stored for it MUST NOT change. Only the four superblock copies are rewritten.
3. **The commit sequence is monotonic.** `active_commit_seq` in the selected
   superblock MUST be strictly greater than its predecessor's. A violation is a
   split-brain candidate, not a normal condition (§13.3).
4. **A root names a complete state.** A root whose objects were partially
   written is not selectable, because it is not reachable without the
   superblock that names it, and the superblock is written last (§15.1).
5. **A hint is never authoritative.** Every consumer of a `StorageHint` MUST
   verify the object it fetches.

## 5. Canonical encoding

Canonical encoding is the byte string that gets hashed to produce an ObjectId.
It MUST be deterministic: the same logical content MUST produce the same bytes
on every platform and every run, or deduplication (§21) silently stops working
and two roots that ought to be equal are not.

Objects above the framing layer (`Inode`, `DirNode`, `ExtentNode`, `AllocNode`,
`Root`) are encoded with `postcard`. Where a struct holds a self-referential
`object_id` — the hash of its own encoding — that field is excluded from the
encoding with `#[serde(skip)]`. A field cannot contribute the value it is being
hashed to produce; see `Inode::object_id` and `AllocNode`'s comments for the
failure this caused when it was not so marked.

### 5.1. Strings

Directory entry names and symbolic-link targets are UTF-8. A directory entry
name is bounded at **256 bytes** by `DirEntry::name`'s storage type
(`heapless::String<256>`); a name that does not fit MUST be rejected rather than
truncated, since truncating two names to the same thing would silently merge
directory entries.

### 5.2. Hash functions

Two BLAKE3 uses, with different purposes:

- **Identity**: the full 32-byte hash (`blake3_hash`), used as the ObjectId.
- **Integrity**: a truncated 64-bit checksum (`blake3_checksum`), used inside
  headers where a full hash would be wasted space. A checksum catches a torn
  write or a bit flip; it is not a collision-resistant name.

### 5.3. Encryption

Secrets are never derived from the disk image. A volume's data is encrypted
with a randomly generated master key; the master key is stored only wrapped,
one wrap per keyslot, and unlocking means producing a key-encryption key,
unwrapping a slot, and confirming the result is the master key.

The passphrase KDF is **iterated BLAKE3**, not Argon2 or scrypt. It is linear in
`KdfParams::iterations` and **not memory-hard**, so it is materially weaker
against GPU or ASIC attack than a modern password KDF. This is stated rather
than implied. `KdfParams` carries an explicit `algorithm` field so that
replacing it later is a slot-format change rather than a new format version.

The block cipher and the reason for it are in §36.

## 6. Global limits

Machine-readable limits, defined in `kernel/src/lib.rs`:

| Constant | Value | Meaning |
| --- | --- | --- |
| `BLOCK_SIZE` | 4096 | Bytes per block on the volume. Fixed. |
| `MAX_OBJECT_SIZE` | 256 KiB | Largest object payload. |
| `BLAKE3_HASH_SIZE` | 32 | ObjectId width. |
| `CHECKSUM_SIZE` | 8 | Truncated checksum width. |
| `MIN_RECORD_LEN` | 64 | Smallest byte run that can be an intent-log record (§17.2). |
| `RLR_SIZE` | 56 | Entries in the embedded root locator ring (§10). |
| `NUM_SUPERBLOCK_COPIES` | 4 | §10.4. |
| `RESERVED_HEADER_BLOCKS` | 6 | Blocks 0–5 are never object space (§11). |
| `RESERVED_TRAILER_BLOCKS` | 2 | The last two blocks are never object space (§11). |

Structural limits, defined on the types they bound:

| Type | `MAX_CANONICAL_SIZE` | `MAX_ENTRIES` |
| --- | --- | --- |
| `DirNode` | 128 KiB | 400 |
| `ExtentNode` | 128 KiB | 1500 |
| `AllocNode` | 128 KiB | per its own definition |

> **Not enforced.** The bounds compiled into these constants are not validated
> at mount time. A `total_blocks` smaller than the reserved header and trailer
> regions, or an object larger than `MAX_OBJECT_SIZE`, is a writer-side concern
> in this release. See `KNOWN_ISSUES.md`.

## 7. Object model

### 7.1. ObjectRecord structure

Every stored object is wrapped in an `ObjectRecord`. The header is 60 bytes:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 8 | `magic` — `OBJECT_RECORD_MAGIC` |
| 8 | 2 | `object_type` — §7.2 |
| 10 | 1 | `compression` — §28.1 |
| 11 | 1 | `encryption` |
| 12 | 4 | `stored_len` — bytes of payload as stored |
| 16 | 4 | `logical_len` — bytes of payload before compression |
| 20 | 8 | `checksum` — truncated BLAKE3 over the rest of the record |
| 28 | 32 | `object_id` — full BLAKE3 hash |

Total `ObjectRecordHeader::SIZE` is 60 bytes.

The distinction between `stored_len` and `logical_len` is what makes
compression work with content addressing: identity is computed over the
*logical* bytes, so compressing an object does not change its name. A reader
that finds `compression != None` MUST decompress to `logical_len` bytes and
verify `blake3(that) == object_id`.

### 7.2. Object types

`ObjectType` is a `u16` discriminator. Decoding an unrecognised value MUST fail
with `InvalidObjectType`; it MUST NOT be treated as opaque data.

| Value | Type | Purpose |
| --- | --- | --- |
| `0x01` | `Data` | File content chunk. |
| `0x02` | `Inode` | File, directory, or symlink metadata. |
| `0x03` | `BTreeNode` | Internal B-tree node. |
| `0x04` | `BTreeLeaf` | B-tree leaf. |
| `0x05` | `ExtentMap` | Logical-offset → chunk mapping (§19.2). |
| `0x06` | `DedupeEntry` | Deduplication index entry (§21.1). |
| `0x07` | `Root` | The commit root. |

> **Reserved values.** `0x08` and above are unassigned. A reader encountering
> one MUST fail the read; a writer MUST NOT emit one.

## 8. B-tree parameters

Metadata trees are stored as B-tree nodes with a fixed canonical-size ceiling
and a deterministic split policy (`ExtentNode::split` splits at the median, so
two writers producing the same content produce the same tree).

The ceiling is the point at which a node splits:

| Node type | Ceiling | Entry ceiling |
| --- | --- | --- |
| `DirNode` | 128 KiB canonical | 400 entries |
| `ExtentNode` | 128 KiB canonical | 1500 entries |

`DirNode::MAX_ENTRIES = 400` is derived from an estimate of roughly 300 bytes
per encoded entry. The estimate is conservative but the limit is real and
enforced: `DirNode::is_full()` is true past 400 entries *or* past 128 KiB, and a
write into a full directory fails.

> **No split.** Directory nodes do not split in this release. A directory that
> reaches 400 entries fails the write with
> `directory is full: B-tree splitting is not implemented`. An application must
> therefore shard its own namespace; the benchmark harness in `benchmarks/` does
> exactly that and says so where it does it. This is recorded in
> `KNOWN_ISSUES.md` and is the most likely limit a new user will hit.

## 9. ObjectRecord framing (Normative)

An object on the volume is a byte run inside one or more consecutive blocks.
The framing rules are:

- The run begins with a 60-byte `ObjectRecordHeader` (§7.1).
- `stored_len` gives the payload length in bytes immediately following the
  header.
- The checksum covers the record — header fields from `object_type` onward plus
  the payload — and does not cover `magic` or `checksum` itself, so a header can
  be validated before the fields it covers have been interpreted.
- **Records are 8-byte aligned.** The length field and the alignment together
  are what let a reader walk a region of blocks looking for recoverable objects
  without a separate index: it scans for the magic, decodes a header, verifies
  the checksum, and steps forward by the aligned record length. This is the
  mechanism behind the linear-scan fallback in §20.
- `MIN_RECORD_LEN = 64` is the smallest byte run that can be a *record* in this
  sense — 60 bytes of header rounded up to alignment. A run shorter than that
  cannot hold a header and MUST NOT be considered.

## 10. Superblock format (Normative)

### 10.1. Superblock structure

A superblock occupies exactly one 4096-byte block. Fixed-layout fields, with
extensions at absolute offsets so that the earlier fields never move:

| Offset | Field | Notes |
| --- | --- | --- |
| 0 | `magic` = `SUPERBLOCK_MAGIC` | |
| 8 | `version` | |
| 12 | `block_size` | Must equal `BLOCK_SIZE`. |
| 16 | `total_blocks` | Volume size in blocks. |
| 24 | `active_root_id` | 32 bytes. The root this commit names. |
| 56 | `active_commit_seq` | Monotonic (§13.1). |
| 64 | `active_checksum` | Integrity of the active root reference. |
| … | `rlr_head`, `rlr_tail`, embedded RLR | The 56-entry ring (§10.4). |
| … | `features_compat`, `features_ro_compat`, `features_incompat` | Feature gates (§43.2). |
| 3464 | `ext_rlr_lba` | 0 = absent; embedded ring is authoritative. |
| 3472 | `ext_rlr_block_count` | 0 = absent. |
| 3476 | volume label | 4-byte length prefix + up to 256 bytes of text, ending at 3736. Not a field on the struct: written out-of-band, and **cleared by any encode** — see below. |
| 3736 | `limits` (`LedgerFsLimits`) | 0 = absent, use compiled-in defaults. |
| 3764 | `active_root_hint` | 0 = absent; see §20. |
| … | `keyslots` | One 128-byte slot per key, per §36. |
| … | `superblock_checksum` | Over the rest of the block. |

`LedgerFsLimits` carries only limits whose value *changes a comparison the
reader makes* — those are the only ones where a runtime knob is meaningful
rather than an invitation to disagree about a constant. It is documented at its
definition for that reason.

Two accidental properties are load-bearing and MUST be preserved by any
rewrite of `Superblock::encode`:

- `encode` begins with `buf.fill(0)`. **Anything not re-emitted is erased.**
  That is why keyslots are fields on this struct rather than left as slack: a
  commit rewrites the superblock, so a slot left out of the struct would be
  wiped by the first write after provisioning, permanently unopenable, while
  reporting success. The volume label above is the counter-example — it is out
  of band and is not re-emitted, so a commit erases it; see `KNOWN_ISSUES.md`
  §9.
- Both extension offsets and the RLR's position are absolute constants. New
  fields go in slack above the current high-water mark; existing offsets do not
  move.

### 10.2. Partial-write reality (Normative)

A 4096-byte write is not guaranteed to be atomic by the medium, so a
superblock can be torn. The format handles this with a checksum rather than by
assuming atomicity: a copy whose `superblock_checksum` does not verify is not a
copy, and is discarded before selection. A torn write therefore costs one copy,
not the volume — provided the copies are not all written in the same window,
which is what §10.4 is for.

### 10.3. Minimum durable commit (Normative)

A commit is durable when **at least one** superblock copy naming the new root
has been written and the barrier after it has completed. Conversely, a commit
whose superblock write never landed is not durable and MUST NOT be replayed as
though it were — the objects it wrote are unreachable garbage, which is a
space cost and not a correctness one.

This is why the commit ordering in §15.1 is not interchangeable.

### 10.4. Superblock redundancy

Four copies. Their LBAs depend on the volume size and are computed by
`superblock_copy_lbas` in the host crate:

| Copy | LBA |
| --- | --- |
| 0 | `0` |
| 1 | `1` |
| 2 | `total_blocks - 2` |
| 3 | `total_blocks - 1` |

The split between the head pair and the tail pair is the redundancy argument:
a failure that destroys one end of the volume leaves the other pair intact. It
is also the reason the trailing two blocks are reserved (§11) — the last two
blocks of a partition are never object space.

## 11. On-disk layout

| Blocks | Contents |
| --- | --- |
| 0–1 | Superblock copies 0 and 1. |
| 2–3 | Intent log ring. |
| 4 | Genesis audit record. |
| 5 | Allocation-tree snapshot (`ALLOC_SNAPSHOT_BLOCK`, `ALLOC_SNAPSHOT_MAGIC`). |
| 6 … N-3 | Object space. |
| N-2, N-1 | Superblock copies 2 and 3. |

`RESERVED_HEADER_BLOCKS = 6` and `RESERVED_TRAILER_BLOCKS = 2` are the totals
that follow from that table, and both are excluded from allocation.

Block 5's allocation snapshot and block 4's genesis record are extensions the
original numbered-spec revision did not describe: they exist because the
allocator's state must be recoverable without walking every object, and because
a freshly formatted volume benefits from an explicit witness of its own
creation. Both are part of the format.

The intent log is **two blocks** — one 8 KiB ring, not a multi-megabyte region.
Its small size is a deliberate trade: it is a *tie-breaker* for mount selection
(§18.5), not a durable write-ahead log, and a ring that small cannot grow into
one by accident.

## 12. Concurrency model

### 12.1. Single-writer principle

At most one writer per volume. The host crate's `FileBlockDevice` enforces this
with an advisory exclusive lock on the image file, taken by writers and not by
readers. The lock is advisory because the format cannot stop a program that
ignores it; what it does guarantee is that two cooperating processes cannot both
commit, which is the failure that would corrupt a root without any torn write
being involved.

### 12.2. Reader isolation

Readers take no lock and are never blocked by a writer. A reader MAY see any
committed root from the moment that root's superblock is durable. Because
objects are immutable and the old root's objects are never overwritten, a reader
holding an older root continues to read consistent data after later commits.

### 12.3. Transaction lifecycle

A transaction is: open, mutate (which stages new objects), commit. Commit is the
only operation that makes anything visible, and the only one that requires the
volume's write lock for its duration. The host crate's `Store` exposes exactly
this shape; see §15.1 for the steps a commit performs.

## 13. Commit sequence semantics (Normative)

### 13.1. Monotonicity requirement

`active_commit_seq` MUST strictly increase across commits. It is the ordering
the mount algorithm trusts when two valid superblock copies disagree.

### 13.2. Sequence verification

A copy whose `active_checksum` verifies is a *valid* copy. Validity says the
bytes are intact; it says nothing about which copy is newest. Recency comes from
`active_commit_seq` and from the RLR.

### 13.3. Handling violations

If two valid copies carry equal-but-different roots, or a sequence that
contradicts the RLR's own history, that is a split-brain candidate. Mount
selection MUST NOT silently pick one; it reports the condition and the
candidates (§18, §26) rather than choosing. A system that guesses here converts
a detectable problem into silent data loss.

## 14. Immutable allocation tree

Allocation state lives in a tree that is itself copy-on-write, so it is
snapshotted by the same mechanism as everything else and needs no separate
journal.

### 14.1. Allocation states

`AllocationState`, a `u8`:

| Value | Name | Meaning |
| --- | --- | --- |
| `0x00` | `Free` | Available. |
| `0x01` | `Allocated` | In use by the current root. |
| `0x02` | `PendingFree` | In the deadlist, awaiting GC. |
| `0x03` | `Reserved` | Held in the emergency reserve (§25). |
| `0x04` | `Pinned` | Held by a snapshot; MUST NOT be freed while any snapshot needs it. |

An unknown value MUST be treated as "not allocatable".

> `PendingFree` and `Reserved` are additions relative to the earlier numbered
> revision, which had three states and no reserve. The five-state machine is the
> one this format implements.

### 14.2. State transitions

```
Free ──allocate──▶ Allocated ──superseded──▶ PendingFree ──GC──▶ Free
                      │                          ▲
                      └──snapshot──▶ Pinned ─────┘
                                     Pinned ──(last snapshot gone)──▶ PendingFree
```

Two rules the diagram compresses:

- `Allocated → PendingFree` happens when an object stops being reachable from
  the current root. It is a *marking*, not a free.
- `Pinned → Free` directly is permitted only on the snapshot-deletion fast path
  where no deadlist entry exists to wait on; otherwise pinned blocks pass
  through `PendingFree` like any other.

## 15. Commit protocol (Crash-Consistent)

### 15.1. Commit steps (Normative)

The order is the protocol:

1. Stage new objects in memory.
2. Write the objects, in the order the allocator assigned.
3. **Barrier.**
4. Write the allocation-tree snapshot (fixed at `ALLOC_SNAPSHOT_BLOCK`).
5. **Barrier.**
6. Write the intent-log records describing the transaction.
7. **Barrier.**
8. Write the superblock copies naming the new root.
9. **Barrier.**
10. Append the commit records to the intent log.

Steps 3, 5, 7, and 9 exist so that each stage is on media before the next is
begun. Removing any one of them does not make the commit faster; it makes the
recovery story false.

Steps 6 and 10 bracket the superblock write so that a mount can distinguish
"this commit was in flight and did not finish" from "this commit finished",
which is the whole of §17 and §18.

> **What the live path actually performs.** The host `Store`'s commit writes
> the objects, then the allocator snapshot, then **one** barrier, then the
> superblock copies, then a **second** barrier. It writes **no intent-log
> records**: the intent-log machinery (§16–§18) is not wired into it. Mount
> selection therefore always takes the no-replay branch on such a volume, which
> is correct — with no records to misread, there is nothing to replay from.
> The four-barrier sequence above is the protocol the format is built around and
> what a writer that logs intent must do; it is not what this release's writer
> does.
>
> Two barriers instead of four is not two steps skipped. Steps 6 and 10 are the
> intent-log writes, and steps 5 and 7 exist only to order those writes against
> the superblock — with no records written, those two barriers follow no new
> dirty data at all, and a barrier with nothing dirty behind it costs nothing
> while claiming an ordering it does not establish. Issuing it is not slower
> than not issuing it; it is only less honest. Steps 3 and 9 remain and are the
> two that do work: step 3 puts the objects and the snapshot on media before the
> superblock names them, and step 9 is the commit point.
>
> **The one reordering** is that the live path writes the snapshot immediately
> after step 2, under step 3's barrier, rather than as step 4 under step 5's.
> The invariant that matters is unchanged: the snapshot is read only after a
> superblock is durable, and in both orders the superblock is written after a
> barrier that follows the snapshot, so the snapshot is on media before the
> superblock that depends on it.
>
> What the reorder leaves is the exposure either order already had, in the same
> window: a crash between a snapshot write and the barrier that covers it can
> leave the *previous* root published with a snapshot written for the commit
> that did not land. That snapshot is **ahead** of the published
> root — it describes blocks as allocated that the root on media does not
> reference. Under §14.2 those blocks are `Allocated` and unreachable, so they
> leak until a later commit rewrites the tree. A block the published root *does*
> reference cannot be marked `Free` by that snapshot, because `Free` is
> reachable only through `PendingFree`, and because the live commit path
> allocates and never frees: the snapshot written for a commit is the previous
> one plus more `Allocated` marks, so the only way it can differ from the
> published root's is by having allocated more.
>
> Over-allocating is safe and under-allocating would not be, and neither order
> can produce the second: the root a commit publishes is always the one built
> from the snapshot written in that same commit, so the snapshot can run ahead
> of the root and never behind it.

### 15.2. Crash recovery semantics

A crash at any point leaves the following, in terms of what a subsequent mount
sees:

| Crash before | Volume mounts as |
| --- | --- |
| step 6 | the previous root; new objects are garbage |
| step 8 completes on no copy | the previous root; new objects are garbage |
| step 8 completes on some copies | the previous root, or the new one if any written copy verifies |
| after step 9 | the new root |

There is no window in which the previous root's objects have been modified.

The allocator snapshot is a special case of the same rule rather than an
exception to it. Because a commit writes it before the barrier that precedes its
superblock (§15.1), a crash can leave a mount running the previous root with a
snapshot written for the commit that did not land. That snapshot describes
blocks the published root does not reference, which under §14.2 makes them
`Allocated` and unreachable — leaked until a later commit rewrites the tree. It
cannot describe a block the published root holds as free; see §15.1.

### 15.3. Barriers and store ordering

A barrier is stronger than a flush: on hardware where stores may be reordered or
held in write-combining buffers, issuing the flush is not sufficient unless the
buffers themselves are drained first. Code paths that use non-temporal stores
MUST issue `sfence()` **before** the barrier, and the barrier MUST NOT be
assumed to imply it. The host crate's `FileBlockDevice` implements the barrier
as `fsync`; `MemBlockDevice` is a no-op, which is precisely why a benchmark run
on a memory device measures the protocol and not the medium (§ and
`BENCHMARKS.md`).

## 16. Intent log (Normative)

### 16.1. Intent log structure

`IntentLogRecord`:

| Field | Size | Notes |
| --- | --- | --- |
| `magic` | 8 | `INTENT_LOG_MAGIC` |
| `seq` | 8 | Record sequence within the ring. |
| `tx_id` | 8 | Transaction this record belongs to. |
| `record_type` | 2 | §16.2 |
| `record_len` | 4 | Payload length. |
| `payload` | `record_len` | Type-specific; see below. |
| `record_hash` | 32 | BLAKE3 over the record. |

`HEADER_SIZE = 30`, so `MIN_SIZE = 62`, aligned up to 64 — which is
`MIN_RECORD_LEN` from §9. The alignment is not cosmetic: the ring parser scans
at 8-byte strides and a record that were not aligned would be invisible to it.

### 16.2. Record types

`IntentRecordType`, a `u16`:

| Value | Name | Payload |
| --- | --- | --- |
| `0x01` | `Begin` | Transaction start. |
| `0x02` | `AddObject` | object id, physical LBA, size in blocks. |
| `0x03` | `PrepareRoot` | new root id, commit seq, previous commit seq. |
| `0x04` | `CommitRoot` | The transaction's root is durable. |
| `0x05` | `Abort` | The transaction did not complete. |

`PrepareRoot`'s previous-sequence field is what the continuity check in §18.4
compares against.

> These discriminants differ from the earlier numbered revision, which started
> at `0x00` and had no `AddObject`. The values above are the format.

### 16.3. Ring buffer properties

The ring wraps and overwrites. It holds recent history, not all history, and a
record that has been overwritten is simply gone — the parser (§17) is built to
tolerate exactly that rather than to require a complete history.

## 17. Intent-log ring parsing (Normative)

Parsing is O(N) in the ring size with bounded memory. The ring is 8 KiB, so N is
small, but the algorithm is specified independently of the size because its
*determinism* is what mount selection relies on.

### 17.1. Alignment and candidate offsets (Normative)

Candidate record starts are the 8-byte-aligned offsets. An unaligned run MUST
NOT be considered.

### 17.2. MIN_RECORD_LEN (Normative)

A candidate is rejected if fewer than `MIN_RECORD_LEN` (64) bytes remain before
the end of the ring. There is no wraparound record: a record that does not fit
is not written.

### 17.3. Valid record definition (Normative)

A record is valid when all of the following hold:

1. `magic == INTENT_LOG_MAGIC`
2. `record_len` is within the ring's remaining bytes
3. `record_hash` verifies over the record as stored
4. `record_type` decodes to a known value

Failing any of the four makes the offset not-a-record, and parsing continues at
the next aligned offset.

### 17.4. Building the valid set

`parse_ring` collects every valid record it finds, in ring order. It makes no
judgement about which are current; that is §17.5's job.

### 17.5. Selecting the authoritative history window

The window is selected by sequence order, deterministically: given the same ring
bytes, any implementation MUST select the same window. Determinism matters
because two implementations disagreeing about which records are current would
disagree about whether a commit was in flight, and that disagreement is a
data-loss bug, not a performance one.

### 17.6. Duplicate resolution

`build_tx_views` folds the record set into one view per transaction. A record
type seen more than once within a transaction resolves to the **last** one in
sequence order.

### 17.7. Wrap/overwrite detection

Because the ring overwrites, the oldest records in a window may be missing. A
sequence gap MUST be interpreted as "overwritten", never as "the transaction
never began". The practical consequence is the conservative direction: an
unreadable history means "do not replay", not "replay optimistically".

## 18. Mount selection + root discovery + replay (Normative)

The algorithm lives in `superblock::mount_select` (the kernel) and is used
unchanged by the host crate's `Volume`, which supplies only the four copies it
fetched off the device.

### 18.1. Superblock selection

1. Decode and checksum-verify each of the four copies; discard the invalid ones.
   No valid copy means no mount (`NoValidSuperblock`), not a format attempt.
2. Collect root candidates: every copy's `active_root_id` where
   `active_commit_seq > 0`, and every non-empty RLR entry, tagged with the copy
   and slot they came from.
3. Select by the rules below.

### 18.2. RLR validation

An RLR entry is a candidate only if its own `entry_checksum` verifies and its
`root_id` is non-zero. The ring's `rlr_head`/`rlr_tail` index the *ring*, not the
selection — which entries matter is determined by their content, not their
position.

### 18.3. RLR monotonicity invariant

Within one superblock copy, RLR entries MUST be monotonic in `commit_seq` going
back from the head. An entry that breaks monotonicity is not trustworthy and is
excluded. `RLR_FLAG_NON_MONOTONIC` exists to mark the condition.

> **Never set in this release.** The flag is defined, decoded, and never
> written. A reader that finds it set is reading a volume this code did not
> write.

### 18.4. Deterministic previous-known-seq

For the continuity check (§18.7), "the previous commit sequence" MUST be
computed deterministically from the decoded state — specifically, from
`PrepareRoot`'s recorded previous sequence in the intent log when the log is
available, and from the highest `commit_seq` below the candidate in the same
copy's RLR otherwise. Two mounts of the same bytes MUST agree.

### 18.5. Intent-log replay decision

`determine_replay` takes the mounted `commit_seq` and the parsed ring and
returns a decision. The rule: if the ring holds a `CommitRoot` for a transaction
whose sequence is at or below the mounted root, the mount is current and nothing
is replayed. If the ring holds a `PrepareRoot` with no matching `CommitRoot` for
a sequence *above* the mounted root, the commit was in flight and is **not**
replayed — the objects it staged are unreachable and are left for GC (§22).

The log never causes a root to be adopted. A root becomes current by having its
superblock written, and by nothing else (§10.3).

### 18.6. Replay correctness gates

Replay is gated on the checks in §17.3 and §18.3 — nothing is replayed from a
record that failed integrity or monotonicity. §44.2 records the gate as a
release condition.

### 18.7. Continuity enforcement

`COMMIT_SEQ_CONTINUITY_UNKNOWN` exists to record that continuity could not be
established — for instance, when the intent log was overwritten and the RLR
gives no comparable predecessor. It is a *report*, and the mount proceeds under
the conservative reading of §17.7.

> **Never set in this release.** As with `RLR_FLAG_NON_MONOTONIC`, the condition
> is defined and not raised by this code.

## 19. Metadata trees

### 19.1. Directory tree

A directory inode's `content_ref` names a `DirNode`. Entries are
`DirEntry { name, inode_id, inode_hint }`, kept sorted by name so that a lookup
is a binary search and two writers enumerating the same names produce the same
encoding — which is what makes a directory's ObjectId stable.

Node ceilings and the missing split are in §8.

### 19.2. Extent tree

A file inode's `content_ref` names an `ExtentNode` tree keyed by logical file
offset. `ExtentDescriptor`:

| Field | Meaning |
| --- | --- |
| `logical_offset` | Start offset within the file. |
| `length` | Bytes covered. |
| `chunk_id` | ObjectId of the chunk. |
| `chunk_hint` | Storage hint for the chunk (§20). |
| `extent_offset` | Offset within the chunk where this extent's data begins. |

`insert_extent` replaces any existing coverage of the same range, trimming or
dropping overlapping extents first. This is not a nicety. Without it, a second
write to an already-covered range appended a second, overlapping extent
alongside the first, and the read path — which has no arbitration rule for
overlaps — resolved the conflict by position in the vector. Because insertion at
an exact-offset match shifts the new entry *before* the existing one, the older
write won the read. Overwriting a config file and reading back the old contents
is a severe enough symptom to be worth stating in the format document: an
implementation MUST replace coverage rather than accumulate it.

The `extent_offset` field is what allows a range inside a shared chunk to be
mapped without copying the chunk.

## 20. Physical hints (virtual LBA hints)

A `StorageHint` is a claim about where an object lives, carried in an
`ObjectRef`. A reader resolves an ObjectId by three routes, in order:

1. The hint on the reference it was handed.
2. An in-memory object-id → block map.
3. A linear scan of the volume, decoding object headers until the id is found.

**The hint is never authoritative.** Every hinted block is read and its stored
`ObjectId` compared before use; a mismatch costs the read and falls through to
the scan. That property is what allows hints to be wrong, stale, or zero without
a correctness consequence, and it is the reason the current writer can leave
them zero (see below) and remain correct.

`active_root_hint` in the superblock (§10.1) is the same idea for the root
itself, and exists for a reason worth recording: without it, every mount pays a
full scan just to find its own root — with a 32 768-block scan limit over ATA
PIO that is tens of seconds, which presents as a mount that hangs.

> **Hints are not populated by the writer.** The CoW layer writes
> `StorageHint::zero()` for chunks and builds references from ids alone, so
> below the root every hint on a volume this code wrote is zero, and lookups
> fall through to the scan. The host crate therefore keeps an optional
> sidecar cache beside a volume — a pure cache, re-verified on use, safe to
> delete, and never written into the volume. See `KNOWN_ISSUES.md`.

## 21. Dedupe index + refcount rules

### 21.1. Deduplication index

A `DedupeEntry` maps a chunk's content hash to a refcount and the set of objects
referencing it. It is an ordinary object with type `0x06` (§7.2), rooted from
the commit root's `dedupe_index_root`.

The index is *not* consulted to write data in this release — the writer stores
each chunk it is given and does not look for an existing copy first. The
structure, its type discriminator, and its root pointer are part of the format;
the decision procedure is not implemented. See §29.2 and `KNOWN_ISSUES.md`.

### 21.2. Refcount safety (Normative)

Where refcounts exist they MUST be validated before a block is freed. `gc.rs`
states the check as four conditions — in the deadlist, no snapshot references
it, refcount is zero, allocation state permits freeing — and requires all four.
On any uncertainty the conservative action is to keep the block, never to free
it.

### 21.3. Deduplication decision

> **Reserved.** No content-based sharing decision is made at write time.

## 22. Garbage collection

### 22.1. Deadlist construction

An object that the current root no longer reaches becomes a `PendingFree` block
and is recorded in the deadlist, rooted from the commit root's `deadlist_root`.
Detection is by comparison against the current root, not by scanning for
references to a specific block.

### 22.2. Safe freeing procedure (Normative)

GC MUST verify, before freeing any block: (1) it is in the deadlist; (2) no
active snapshot references it; (3) for deduplicated chunks, the refcount is
zero; (4) its allocation state is neither `Pinned` nor `Reserved`. Failing any
check, the block is retained. Freeing a block that a snapshot still needs is the
one GC bug that silently corrupts an older root.

Three tiers are defined as an enum (`GcMode`): A, stop-the-world mark-sweep;
B, incremental deadlist processing with periodic reconciliation; C,
generational, unimplemented.

> **No GC runs in this release.** The states, the deadlist root, and the safety
> rules exist in the format. Nothing sweeps. Free space therefore falls
> monotonically as a volume is written, and a volume eventually fills. This is
> the single most important thing to know before depending on this
> implementation; it is recorded in `KNOWN_ISSUES.md`.

### 22.3. Snapshot protection

`Pinned` blocks MUST NOT be freed while any snapshot that pinned them exists.
Unpinning is a transition to `PendingFree`, not to `Free`, unless no deadlist
entry is pending — see the fast path in §14.2.

## 23. Allocation extent state machine (Normative)

The five states and their legal transitions are §14.1 and §14.2. This section is
the normative restatement: a transition not drawn in §14.2 MUST NOT occur, and a
decoder MUST treat an unknown state as non-allocatable.

## 24. GC pinning atomicity (Normative)

### 24.1. Snapshot creation atomicity

Snapshot creation is three phases, and all three MUST succeed or the whole
operation MUST be rolled back:

1. **Pin** — transition the blocks the snapshot will need from `Allocated` to
   `Pinned`.
2. **Commit** — write a root whose allocation tree reflects those pins.
3. **Register** — add the snapshot to the active list.

Committing the pins before registering the snapshot is what closes the race: a
crash between 2 and 3 leaves pinned blocks with no snapshot referencing them,
which wastes space but cannot lose data. The reverse order would leave a
registered snapshot whose blocks GC is free to reuse.

> **Reserved.** No snapshot is registered in this release; the `Root` type is
> frozen at the field set in §30.1.

### 24.2. Pin cost

Marking every allocated block `Pinned` on snapshot creation is O(allocated
blocks) in the naive form. This is noted as a cost, not a design; no incremental
pin structure is implemented.

## 25. Emergency space reserves

### 25.1. Reserve sizing and 25.2. Accounting

`AllocationState::Reserved` exists so a volume can keep enough blocks to perform
a deletion or a commit when it is otherwise full — a filesystem that cannot free
space when full is a filesystem whose operator has no recovery move.

> **Reserved.** No reserve is sized, accounted, or consumed in this release.

### 25.3. Reserve exhaustion

> **Reserved.** No policy is implemented for a volume whose reserve is spent.

## 26. Disaster recovery mode

### 26.1. Entry conditions and 26.2. Candidate root ranking

When mount selection finds multiple plausible roots it MUST report them rather
than choose. The `MountSelection` result carries the candidates and their
sources (§18.1) for exactly this purpose.

> **Reserved.** There is no operator-facing recovery mode, no interactive
> confirmation, and no ranked presentation. The information a recovery tool
> would need is produced by mount selection; the tool is not implemented.

### 26.3. Operator confirmation

> **Reserved.**

## 27. Copy-on-write mechanics

The modification path: mutating an object writes a **new** object and updates
its parent to reference the new id; the parent is itself rewritten as a new
object, and so on to the root. No ancestor is modified in place. `cow/operations`
implements this over the `ObjectStore` trait.

The consequence for readers is §12.2: an older root remains fully readable after
later commits, because none of its objects were touched.

## 28. Compression

### 28.1. Supported algorithms

`CompressionAlgorithm`, a `u8`, stored in the record header's `compression`
field (§7.1):

| Value | Name |
| --- | --- |
| `0x00` | `None` |
| `0x01` | `LZ4` |
| `0x02` | `ZSTD` (level 3) |
| `0x03` | `ZSTD_HIGH` (level 19) |

### 28.2. Compression decision

Compression is attempted when the payload exceeds `COMPRESSION_THRESHOLD_SIZE`
(4096 bytes) and accepted only when the achieved ratio exceeds
`COMPRESSION_MIN_RATIO` (1.1). Below either threshold the object is stored
uncompressed with `compression = None`.

Identity is unaffected: `object_id` is the hash of the logical bytes, and
`stored_len`/`logical_len` in the header record both. Two volumes holding the
same file at different compression settings MUST agree on the file's chunk ids.

## 29. Content-defined chunking

### 29.1. Chunking algorithm

Chunk boundaries are content-defined, using a **Gear hash** rolling hash:

| Constant | Value |
| --- | --- |
| `MIN_CHUNK_SIZE` | 16 KiB |
| `AVG_CHUNK_SIZE` | 64 KiB (target, via `GEAR_MASK = 0xFFFF`) |
| `MAX_CHUNK_SIZE` | 256 KiB (hard cut) |

The hash table is generated from a fixed seed
(`GEAR_SEED = 0x4C45444745524653`, `"LEDGERFS"`) and MUST NOT be randomised.
Determinism across platforms and runs is the property that makes chunk
boundaries stable, and stable boundaries are the precondition for content
addressing to do anything useful: a change in the seed re-chunks all existing
data.

`MAX_CHUNK_SIZE` equals `MAX_OBJECT_SIZE` (§6) because a chunk is stored as one
object.

> The earlier numbered revision described Rabin fingerprinting here. The format
> uses Gear. Boundaries from the two algorithms differ, so data chunked under
> the other description would not deduplicate against data chunked under this
> one.

### 29.2. Deduplication benefit

The reason to chunk this way rather than at fixed offsets: inserting a byte
near the start of a file re-chunks only the chunk containing the insertion,
because boundaries are chosen by content and resynchronise, rather than shifting
every subsequent boundary and invalidating the whole remainder of the file.

> **Reserved.** The chunker exists and is deterministic; nothing uses the
> stability for storage sharing, because deduplication is not performed at write
> time (§21.1).

## 30. Snapshots

A snapshot is a root that is kept after it stops being current. Because objects
are immutable (§4.2) and blocks referenced by a snapshot are `Pinned` (§14.1), a
snapshot needs no copy of anything.

### 30.1. Snapshot semantics

- **Read-only.** A snapshot MUST NOT be modified.
- **Space-efficient.** Objects shared with the current root consume no
  additional space.
- **Independent.** Deleting a file from the current root MUST NOT affect any
  snapshot.

> **Not representable in this release.** The `Root` struct has no snapshot
> field, so a volume written by this code cannot record that it has snapshots.
> `snapshot.rs` implements the pin/unpin/rollback mechanics, and `SnapshotId` is
> a `u32`; nothing persists one. See `KNOWN_ISSUES.md`.

### 30.2. Snapshot lifecycle

> **Reserved.** No lifecycle is implemented; see §30.1.

## 31. Send and receive — replication

> **Reserved.** `replication.rs` defines a stream format and validation rules.
> No send or receive path is wired into the host `Store`, and no volume is
> replicated by this release.

### 31.1. Send stream format and 31.2. Receive validation

> **Reserved.**

## 32. Reflink clones

> **Reserved.** No clone operation exists.

### 32.1. Clone operation and 32.2. Clone divergence

> **Reserved.**

## 33. Quotas

> **Reserved.** No quota is tracked or enforced.

### 33.1. Quota types and 33.2. Quota enforcement

> **Reserved.**

## 34. ACLs, xattrs, and capability gates

### 34.1. Access control lists

> **Reserved.** No ACL is stored.

### 34.2. Extended attributes (xattrs)

An `Inode` carries xattrs as a `BTreeMap<String, Vec<u8>>` — ordered, so the
inode's canonical encoding is deterministic. xattrs are **stored and returned**;
no access-control decision is made from them.

### 34.3. Capability gates

`SecurityFlags` on an inode carry `Immutable`, `AppendOnly`, and `NoDedupe`.
They are stored and read back as part of the inode. > **Not enforced.** The
flags round-trip; violation of them is not prevented by this release.

## 35. Timestamps (Normative)

### 35.1. Required timestamps

An `Inode` carries three UNIX-nanosecond timestamps: `created_ns`,
`modified_ns`, and `accessed_ns`.

### 35.2. Resolution

Nanoseconds. A caller with coarser resolution MUST pass the coarsest value it
has rather than zero, since zero is a valid timestamp (the epoch) and is
indistinguishable from "not set".

### 35.3. Update rules

`modified_ns` is updated on a content write; `accessed_ns` is updated on a read
that goes through the host `Store`. There is no separate change-time (`ctime`)
field: the earlier numbered revision's `ctime` was documented in the code as
*creation* time, which is the same thing as `created_ns` here under a
misleading name. The three fields above are the format.

## 36. Encryption (per-volume) via keyslots (Normative)

### 36.1. Architecture

Two layers, deliberately separate:

- **Keyslots** (`keyslot.rs`) — a randomly generated master key, wrapped once
  per slot under a key-encryption key. A passphrase slot derives its KEK from
  the passphrase and a per-slot salt; an HSM slot takes its KEK from the HSM.
  Both may be populated, either unlocks the volume, and revoking one does not
  disturb the other. This indirection is why changing a passphrase rewraps 64
  bytes rather than re-encrypting the volume.
- **Block cipher** (`data_crypto.rs`) — applied *below* the object model, so
  that exactly two functions in the disk path see plaintext and everything
  above (`cow::operations`, the extent tree, content addressing) never learns
  encryption exists.

A 128-byte slot (`SLOT_SIZE = 128`), repeated `SLOT_COUNT` times, is a field of
the superblock (§10.1) precisely so it survives the superblock rewrite.

### 36.2. Encryption envelope

**XTS-AES-256**, with the tweak derived deterministically from the LBA. There
is no per-write nonce and no per-block authentication tag.

XTS provides confidentiality only. That is a deliberate trade rather than an
oversight, and the reasoning belongs in the format document because it is the
question a reader will ask: the object layer already verifies
`blake3(decoded payload) == object_id` on every read, so a tampered or corrupt
ciphertext block fails loudly there. Adding a second, independent
authentication mechanism at the block layer would require persisting a
nonce-and-tag table — a much larger format change — for a property the system
already has.

> The earlier numbered revision described AES-256-GCM and ChaCha20-Poly1305 with
> an `EncryptedObject` envelope carrying an authentication tag. **No such
> envelope exists in this format.** An implementation reading under that
> description would be reading a different format.

### 36.3. Key management

The master key is never derived from the disk image. The passphrase KDF is
iterated BLAKE3 (§5.3).

### 36.4. Encryption lifecycle

> **Reserved.** No rekeying, adding, or revoking of a slot is implemented at the
> format level; slots are provisioned at format time.

## 37. Audit integration + audit chain

### 37.1. Audit events

`AuditChainHead` is an `Inode`... — no. `AuditChainHead` is a field of the
`Root` (§30.1), and block 4 holds a genesis audit record (§11). An audit record
type exists with a checksum.

### 37.2. Audit chain

The chain head is stored in the root and is part of the root's canonical
encoding, so the chain advances with the commit sequence.

> **Not maintained.** The head field round-trips; nothing appends to the chain,
> and no verification is performed. See `KNOWN_ISSUES.md`.

### 37.3. Audit verification

> **Reserved.**

## 38. IPC protocol

> **Reserved.** LedgerFS is a library. It exposes no IPC surface, no message
> format, and no daemon. An embedding application calls the `Store` API
> directly; see `EMBEDDING.md`.

### 38.1. Interface, 38.2. Message format, 38.3. Error handling

> **Reserved.**

## 39. Error semantics, partial failure, and data loss model

### 39.1. Partial write handling

A partial write is bounded by the commit protocol (§15.2): the outcome is the
old root or the new one, and never a mixture. A short write inside a record is
caught by the record checksum (§9).

### 39.2. Media error handling

A read error on an object block is a failed object read. A read error on a
superblock copy removes that copy from selection and mount proceeds on the
remaining ones (§18.1). A read error on every copy is a failed mount.

### 39.3. Data loss scenarios

The format's data-loss surface is deliberately small, and worth enumerating
because each has a different answer:

| Scenario | Outcome |
| --- | --- |
| Power loss during a commit | Previous root. New objects are garbage. |
| Power loss after the final barrier | New root. |
| One superblock copy torn | That copy is discarded; three remain. |
| All four copies destroyed | No mount. The objects remain on the volume but nothing names a root. |
| Object block corrupted in place | Fails its hash check. `KNOWN_ISSUES.md` records the current behaviour on encountering it. |

## 40. Background scrubber (scheduling rules)

### 40.1. Scrubber operation

A scrub reads stored objects and verifies `blake3(payload) == object_id`,
reporting mismatches. The verification is exactly the reader's check (§4.1),
performed proactively.

### 40.2. Scrub scheduling

> **Reserved.** Nothing reads a volume in the background in this release;
> `scrubber.rs` is not driven by the host `Store` or by any daemon.

### 40.3. Error reporting

> **Reserved.**

## 41. Superblock and critical metadata redundancy

### 41.1. Redundancy strategy

Four copies, split across the head and tail of the volume (§10.4), each
independently checksummed (§10.2). Selection prefers the highest verifying
`commit_seq` and refuses to guess when the copies disagree (§13.3).

### 41.2. Recovery priority

The head pair is read first, because copies 0 and 1 sit at fixed LBAs while the
tail pair's LBAs depend on `total_blocks` — a value that comes *from* a decoded
superblock. A volume whose head pair is destroyed is still recoverable, but only
by a reader willing to search for the tail pair rather than compute its address.

## 42. Optional RAID support

> **Reserved.** No RAID integration exists in this format or this code.

### 42.1. Integration and 42.2. Considerations

> **Reserved.**

## 43. Compatibility and versioning

### 43.1. Version numbering

The superblock carries a `version` field and the `Root` carries a schema
version and a format version. A reader MUST refuse to mount a version it does
not know rather than mount it optimistically — an unknown version is an unknown
*format*, and the fields it does recognise may not mean what it assumes.

### 43.2. Feature flags

`features_compat`, `features_ro_compat`, and `features_incompat`, following the
usual convention: a `compat` feature a reader does not know may be ignored;
`ro_compat` forces read-only; `incompat` forces refusal. One incompat bit is
defined:

| Bit | Name |
| --- | --- |
| `0x0…1` | `FEATURE_INCOMPAT_DATA_ENCRYPTED` |

### 43.3. Upgrade path

> **Reserved.** No in-place upgrade is implemented. A format change is currently
> a new volume plus `replication` (§31), which is itself reserved — so in
> practice, a rewrite of the data.

## 44. Production readiness tiers (Normative)

### 44.1. Tier definitions

The tiers describe how much of the format is exercised, not how well tested a
component is.

| Tier | Meaning in this release |
| --- | --- |
| **A** | Implemented and exercised end to end by the host `Store` and its test suite: object framing, superblock copies and selection, mount and replay (which is what parses the intent log and decides whether a commit was in flight), the commit protocol, CoW reads and writes, directories, extents, compression. |
| **B** | Implemented in the kernel crate with tests, but not driven by the host `Store` on a live volume: dedupe index, snapshot pinning, chunking, scrubber, GC safety rules, replication, audit chain, keyslots and the block cipher. |
| **C** | Not implemented: GC *execution*, snapshots, disaster recovery, RAID, quotas, ACLs, IPC, rekeying. |

Tier B is the honest and unusual category: the code exists and is tested, but
nothing calls it in the shipping path. A reader deciding whether to depend on
LedgerFS should read Tier B as "not available". The five Tier B subsystems that
form a self-contained set — dedupe, snapshots, chunking, the scrubber and GC
safety — are therefore built as the separate, unpublished `ledgerfs-pending`
crate rather than shipped. The rest stay in this crate, because the on-disk
structures they describe still have to be parsed to read a volume even when
nothing here drives them.

### 44.2. Replay correctness gate

**MUST.** A mount MUST NOT adopt a root on the strength of an intent-log
record. Replay decides whether a commit *was in flight*; the superblock decides
what is current (§18.5). The gate is that the two are never conflated.

### 44.3. Split-brain persistence gate

**MUST.** When two valid superblock copies carry conflicting roots, mount
selection MUST report the conflict and MUST NOT pick one silently (§13.3).

### 44.4. Gate verification

The gates above are covered by the kernel crate's test suite. See
`CONTRIBUTING.md` for how to run it.

---

## Appendix A. Test vectors

A.1 superblock parsing, A.2 intent-log parsing, A.3 GC safety, A.4 crash
consistency, A.5 encryption, and A.6 split-brain detection are covered by the
test suites in `kernel/tests/` and `host/tests/`, which carry their own
fixtures. This document does not duplicate them, because a duplicated vector is
a second thing to keep correct; read the tests for the exact bytes.

## Appendix B. Change log

See `CHANGELOG.md`.
