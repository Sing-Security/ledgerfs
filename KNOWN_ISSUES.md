# Known issues

The limitations of LedgerFS in its current state, ordered by the likelihood that
an evaluator encounters them. Each entry states its consequence and its cause.
Every item is deliberate: either a trade that was made or a component that was
not built. A list of limitations is useful only when it distinguishes the
load-bearing constraints from the incidental ones, and each entry here does.

---

## 1. Nothing frees space. A volume fills and stays full.

The allocation tree, the deadlist, the `PendingFree` and `Pinned` states, and
the safety rules that govern freeing a block all exist in the on-disk format
(`FORMAT.md` §14, §22). Nothing executes garbage collection. A block that stops
being reachable from the current root is marked and left where it is.

**Consequence.** Free space falls monotonically with every commit, forever. A
volume that is written to continuously will fill, and the failure is an
out-of-space error on a write that had plenty of room when it started.

**How much.** The benchmark harness in `benchmarks/` measures this and reports
it as the `volume KiB/op` column; see `BENCHMARKS.md`. The number to carry
around is that it is on the order of 100 KiB of volume per small file, not per
*byte written*: see entries 2 and 3 for why.

**Workaround.** Treat a LedgerFS volume as append-only with a known lifetime,
and reformat rather than reuse. Do not make it the primary store for anything
that must run indefinitely without intervention.

---

## 2. A directory is capped at 400 entries, and directories do not split.

`DirNode::MAX_ENTRIES` is 400 and `DirNode::MAX_CANONICAL_SIZE` is 128 KiB. Past
either, `DirNode::is_full()` is true and a write into the directory fails with:

```
directory is full: B-tree splitting is not implemented
```

This is not a tunable. The B-tree split exists on the extent tree
(`ExtentNode::split`) and not on the directory tree, so a directory that reaches
the ceiling has nowhere to put its 401st name.

**Consequence.** An embedding application must shard its own namespace:
`benchmarks/` keeps 100 entries to a directory and states the reason where it
does so. An application that does not shard encounters the failure as a write
error partway through a population loop, not at design time.

**What is not affected.** Lookups and listings inside a directory that is under
the ceiling; those are a binary search over a sorted entry vector and are fast
(`BENCHMARKS.md`, the `scan_*` rows).

---

## 3. An object is at most 256 KiB, so a large write is several objects.

A write of any length is **one commit**: the chunks, the extent tree over them,
the file inode and the directory entry are folded into a single transaction and
placed together. The payload is still split into one object per
`max_object_bytes` (256 KiB by default), and that ceiling is not this layer's
to move: `MAX_OBJECT_SIZE` is a format limit (`FORMAT.md` §6), and the reader's
scan skips any record above it, so an object over the limit would commit
successfully and then read back as absent. `CommitEngine` refuses to place one
rather than let that happen.

**Consequence.** A large write costs one barrier pair, not one per object. What
it still costs is *volume*: every chunk is a fresh 4 KiB-aligned object, and
CoW re-places the metadata around it. The pieces are folded in memory before
anything is written, so a payload that spans many objects also spends
proportional transient RAM during the write.

**The ceiling that does bind.** `Store::append` adds one extent to the file's
`ExtentNode` and CoW-copies the whole node on every commit, and extent-tree
splitting is not wired into the write path. So a file that grows by appending
*forever* eventually has an `ExtentNode` larger than the object ceiling, and its
next append is refused; before that it is already paying O(N²) bytes. Write
bounded chunk files instead of one endless file (`EMBEDDING.md` makes the case
at length), and see entry 1 for why that is not a temporary limitation.

**Also worth knowing.** `Store::write_file` (the verb that replaces an existing
path) is one commit too, and is atomic: it builds the new content under no
directory entry and re-points the existing entry at it, so a reader never sees
the path absent or the file empty. `remove` followed by `create_file` (which is
what a caller writing the sequence itself does) still has that window.

**The staging tier is the answer for a stream that keeps appending.** Inside a
staging session an append extends a per-file **draft** (the decoded inode and
its extent list) instead of building an object, so neither the device nor the
session pays for the file's history per append: N staged appends hold their
payload plus one descriptor each, and the seal writes one generation of the
file's metadata pruned to the final root. The `ExtentNode` ceiling still binds
*at the seal*, so a stream longer than one node can hold has to seal in batches
rather than stage one enormous file. See `EMBEDDING.md` §3.

---

## 4. Reads fall back to scanning the volume, because hints are never written.

An `ObjectRef` carries a `StorageHint`: a claim about which block an object is
in. The reader uses it, then an in-memory map, then a linear scan that walks the
volume decoding object headers until it finds the id it wants
(`FORMAT.md` §20).

The CoW layer writes `StorageHint::zero()` for every chunk and builds its
references from ids alone. So on a volume this code wrote, every hint below the
root is zero, and **the first lookup of anything is a scan**.

**Consequence.** A read by a process that has not just written the volume, and
has no sidecar beside it, pays that scan, which is why it costs tens of
microseconds where a key/value store costs one. The `BENCHMARKS.md` read rows
are *not* this case, and they say so: they are measured in a process that wrote
the data first, so the map is warm and they are map hits. The host crate keeps an
optional sidecar cache next to a volume (`host/src/hints.rs`) that gives a later
process the same map hit; it is a pure cache, re-verified against the block it
names before use, so it cannot return a wrong answer, but it does not exist
until something builds it.

**Why this is safe.** Verifying the hint is not optional and is not skipped: a
hint that names the wrong block costs one read and falls through to the scan.
That is what allows the writer to leave them zero and remain correct.

---

## 5. `write_at` cannot overwrite bytes already stored.

`Store::write_at(path, offset, data)` requires `offset` to be at or past the
file's current end. Writing inside the file is refused with
`Error::Unsupported` rather than performed.

**Why.** The write path builds a fresh extent for the bytes it is given and does
not split an existing extent. Overwriting stored bytes would leave the extent
tree with two overlapping ranges over the same file region. Use `write_file` to
replace a file's contents, or `append` to extend it.

---

## 6. Fixed ceilings

These are format limits, not configuration. Exceeding one is an error, not a
degradation.

| Ceiling | Value | What happens past it |
| --- | --- | --- |
| Object payload | 256 KiB (`MAX_OBJECT_SIZE`) | Rejected. Also the maximum chunk size (§29.1). |
| Directory entries | 400 | Write fails; see entry 2. |
| Extent-tree entries per node | 1500 | Split; this one is implemented. |
| Names in embedded root locator ring | 56 (`RLR_SIZE`) | Older roots roll out of the ring; §18 still finds them from the active slot. |
| Directory entry name | 256 bytes | Rejected, not truncated. |
| Superblock copies | 4 | N/A |

A file larger than 256 KiB is stored as multiple chunks, each an object, so the
object ceiling is not a file-size ceiling.

---

## 7. A record with a non-zero encryption byte reads as absent, not as an error.

The reader skips an object record whose header's encryption byte is set, and
reports it as `NotFound` (`kernel/src/reader.rs`, both in `try_read_block` and
in the scan path). A missing object and an object it declined to read are the
same answer.

**This is unreachable from this code.** Nothing here writes a non-zero
encryption byte: encryption is applied *below* the object model
(`FORMAT.md` §36.2), so the byte stays zero and the cipher is invisible to
`cow::operations`, the extent tree, and content addressing. The superblock's
`FEATURE_INCOMPAT_DATA_ENCRYPTED` flag is how an encrypted volume announces
itself, and a reader without the cipher compiled in refuses the mount outright
rather than decoding ciphertext as garbage.

**Consequence.** A volume written by something that *does* set the byte would
have objects that read as missing rather than as unreadable. That is the wrong
answer to give an operator, and it is recorded here rather than fixed because
there is no writer in this tree to test the fix against.

Compression is the opposite case and was fixed: a compressed record is
decompressed before hashing, and the `compression_algo` byte is carried down to
where the payload is assembled.

---

## 8. Superblock copies are written but not reconciled.

`Store::format` and every `Store` commit write all four copies
(`host/src/store.rs`, `write_all_superblock_copies`). Mount selection takes the
highest verifying `commit_seq` and reports a disagreement rather than guessing
(`FORMAT.md` §18, §13.3).

But nothing reconciles copies that have drifted. A volume whose copies disagree
mounts *correctly* (the newest one wins) while the redundancy the four copies
exist for has quietly degraded. If only one copy carries a root, then damage to
one block loses the root, and the volume reads back empty rather than falling
through to a surviving copy.

`Volume::copy_status` reports the per-copy state and the `lfs-read` tool prints
it, so the condition is inspectable. It is not repaired.

---

## 9. Any commit erases the volume label.

The superblock block sets aside bytes 3476-3735 for a human-readable label: a
4-byte length prefix and up to 256 bytes of text (`FORMAT.md` §10.1). It is
written out of band, by whichever tool prepares the volume, and it is not a
field on the `Superblock` struct.

`Superblock::encode` opens with `buf.fill(0)` and then re-emits the struct's
fields, the extended RLR pair, the limits block, the root placement hint, and
the keyslots. It does not re-emit the label, and there is nothing to re-emit it
from. So every superblock write (that is, every commit) also zeroes the label
region.

**Consequence.** A label does not survive the first write to the volume. The
loss is silent in both directions: writing the label succeeds, the write that
erases it also succeeds, and nothing verifies the region on mount. It surfaces
only when something reads the label back after data has been written.

**Why the keyslots are not affected and the label is.** The keyslot area is
zeroed by the same `buf.fill(0)`. It survives because `keyslots` was made a
field on the struct and re-emitted at the end of `encode`, with a comment
saying in as many words that this is what keeps every superblock write from
destroying the volume's key material. The label was left out of band, so it
gets the behaviour the keyslots used to have.

**Consequence for a label writer.** Anything that writes a label has to keep
the region intact itself: read it before a commit and re-emit it after, which
means either avoiding commits entirely or carrying the bytes across them by
hand. That is a constraint on the wrong side of the interface: the format
documents a label field, and the crate that owns the format silently deletes
it.

**Fix, not done here.** Either model the label as a struct field (the keyslot
fix) or read-modify-write the region in `encode`. Both are changes to the
on-disk encode path, and this release is a documents-and-harness release.

---

## 10. One disk, one writer, and PIO.

- **One physical disk.** No RAID, no mirroring, no erasure coding
  (`FORMAT.md` §42). The redundancy story is the four superblock copies and
  nothing below them.
- **One writer.** Writers take an advisory exclusive lock on the image;
  readers take none. The lock is advisory because a library cannot enforce it;
  see `FORMAT.md` §12.1.
- **ATA PIO, in the kernel crate.** The in-tree block device talks to the
  primary bus with programmed I/O, one sector at a time, which is why the
  per-commit fixed cost is what it is. The host crate's `FileBlockDevice` does
  not use it; it reads and writes a file or partition window and implements the
  barrier as `fsync`. A benchmark run on that device measures the protocol and
  the medium, not the ATA path.

---

## 11. Implemented in the format, not wired into the library

These have types, encodings, and tests in `kernel/`, and nothing calls them from
the host `Store`. `FORMAT.md` §44.1 calls this Tier B, and a reader deciding
whether to depend on one of them should read Tier B as "not available":

garbage-collection *execution* (§22), snapshots (§30), deduplication at write
time (§21), replication (§31), the background scrubber (§40), the audit chain
(§37), disaster recovery (§26), quotas (§33), ACLs (§34.1), capability-flag
*enforcement* (§34.3), and keyslot lifecycle: adding, revoking, rekeying
(§36.4).

And these have no implementation at all: reflink clones (§32), RAID (§42), IPC
(§38: LedgerFS is a library, not a daemon), and in-place format upgrades
(§43.3).

**Four of them return success without doing the work**, which is the part worth
knowing before calling one. Each is annotated where it lives:

- `Scrubber::scrub_batch` advances a cursor and counts entries without issuing a
  read, so its returned count is entries walked, not objects verified; and
  `Scrubber::attempt_repair` relocates to the alternate location and records a
  successful repair without reading it, so `repairs_successful` counts attempts.
- `replication::create_full_send_stream` and `create_incremental_send_stream`
  build a stream whose entries are self-consistent but synthetic: the payload is
  an object id's bytes and the entry's id is that payload's hash. Neither
  carries an object's contents, and the reachable set they enumerate is one
  level deep.
- `audit::seal_commit_record` returns the record's canonical bytes rather than a
  seal, and `audit::seal_root` returns nothing. Verification rejects both.
- `GcEngine::reconcile` reports completed with all three counters zero, because
  the streamed mark it would need does not exist.

None of these can corrupt data as written: the failure mode is a number that
says work happened when it did not.

---

## 12. It is a library, not a mountable filesystem.

Nothing here registers with a VFS, and there is no FUSE layer and no mount
point: no path on the host resolves into a LedgerFS volume. A program opens
one with `Store` and calls its methods; see `EMBEDDING.md`.

That is not the same as saying it only runs in userspace. The crate carries its
own ATA PIO read and write path (`reader`, `writer`, behind the `reader`
feature) for the ring-0 case: compiled into a kernel, opening a real disk with
no OS block layer underneath it, because the crate is the only route to its own
partition. It is still not a VFS driver: it owns the whole device and serves
nobody else.

The nearest thing to a tool is `lfs-read` and `lfs-write` in `host/src/bin/`,
which look at and mutate a volume's bytes and directory tree from a shell.

---

## Reporting

A defect not on this list is a bug rather than a limitation. See
`CONTRIBUTING.md`.
