# The LedgerFS application-store contract

This is the standard for an application that embeds LedgerFS as its own on-disk
store: what it calls, what those calls promise, and what it is expected to do in
return. The shape comes from the two applications that have used it — an OS that
keeps its writable data volume on it, and a log-structured application whose
transcripts were the first thing to lean on the contract. The code that
implements it is `Store` in `host/`; the same contract as running code is
`examples/app_store.rs`.

The rule of thumb throughout: **a volume is a filesystem, not a file.** If you
wanted a file, you wanted a file. You embed LedgerFS because you want a *tree*
of files with a crash-consistent commit protocol behind every write — and you
pay for that with the constraints below.

---

## 1. The shape

An application's entry points:

| intent | call |
|---|---|
| new store, stamped as mine | `Store::format_app(device, "myapp")` |
| new store, plain volume | `Store::format_device(device)` |
| open an existing volume | `Store::open(device)` |
| is this my volume? | `store.app()`, `store.contract_version()` |

A `BlockDevice` is a flat array of 4 KiB blocks. `FileBlockDevice` for a
partition, a disk image, or a bare file — it opens a region at a byte offset —
and `MemBlockDevice` for a volume in the process's own heap. The store takes
ownership of the device; it does not care which kind it is. That seam is the
entire story — nothing above it is host-specific, and nothing above it can tell
a volume provisioned onto a real disk from one embedded in a process.

## 2. The verbs

| verb | does |
|---|---|
| `read_file(path)` | whole file back |
| `read_at(path, offset, len)` | a byte range, clamped to the file (see §5) |
| `read_dir(path)` | one `Entry` per child: name, kind, size, mode |
| `stat(path)` | the inode: kind, size, mode, refs |
| `mkdir(path, mode)` | create a directory |
| `create_file(path, mode)` | create an empty regular file |
| `write_new_file(path, data, mode)` | create **and** write; refuses if the name is taken |
| `write_file(path, data, mode)` | create **or replace**; the idempotent write |
| `append(path, data, mode)` | create if absent, else write at the current end |
| `write_at(path, offset, data)` | extend at `offset` (must be ≥ the current size) |
| `remove(path)` | delete a file |
| `rename(from, to)` | same-directory rename (see §5) |
| `free_bytes()` / `reserved_bytes()` / `total_free_bytes()` | space left |
| `capacity_bytes()` / `total_blocks()` | space total |

Everything returns `crate::Result<T>`, one `Error` enum with a hand-written
`Display`. There is no `anyhow`, no `thiserror` — an app matches on the variant
it cares about (`Error::NoSuchFile`, `Error::ReadOnly`).

An error names the component that is actually absent, not the path the caller
asked for: creating `findings/FIND-0001.md` in a volume with no `findings/`
returns `NoSuchFile { path: "findings" }`. A create never makes its parents —
that is the app's job, and the store says which one is missing rather than
letting a caller conclude its *file* is the problem.

An app whose traffic is too fast for a commit per call has a second tier rather
than a second filesystem (§3, *Staging: many writes, one commit*):

| verb | does |
|---|---|
| `begin_staging(capacity_bytes)` | open a session; `0` = unbounded |
| `staged_append(path, data, mode)` | append into the session — no commit, no device write |
| `staged_write_new_file(path, data, mode)` | create into the session; refuses a taken name |
| `staged_remove(path)` | remove into the session |
| `seal_staged()` | everything staged, as **one** commit; the session ends |
| `seal_staged_deferred()` | the same seal, stopping before the superblock write (see below) |
| `sync()` | write the superblock a deferred seal withheld, and flush it; a no-op otherwise |
| `has_pending_sync()` | whether `sync()` has something to do |
| `end_staging()` | discard the session; the volume is untouched |
| `staged_bytes()` / `staging_is_open()` | what the session holds, and whether it is open |
| `read_file_committed(path)` | the committed tier, even while a session is open |

A mutating call outside a session and its `staged_` counterpart take the same
arguments and mean the same thing; the difference is *when* it becomes durable.

### The payload is bytes, whatever your type is

Every verb that moves data takes `&[u8]`; every read returns `Vec<u8>`. The
store has no schema, no column types and no parser on either path — an `f32`
sample is four bytes the application chose an encoding for, exactly as a JSON
line is bytes. Encode on the way in and decode on the way out; a fixed-width
numeric stream, the workload a database would be asked to hold as `REAL`s, is
the easiest case:

```rust
// f32 samples -> bytes. The endianness is the app's choice and part of its
// format; the store never interprets it.
let mut batch = Vec::with_capacity(samples.len() * 4);
for sample in &samples {
    batch.extend_from_slice(&sample.to_le_bytes());
}
store.staged_append("/telemetry/samples.f32", &batch, 0o644)?;

// ... and back, all of it or a byte window of it:
let all: Vec<f32> = store
    .read_file("/telemetry/samples.f32")?
    .chunks_exact(4)
    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
    .collect();
let one = store.read_at("/telemetry/samples.f32", 40, 4)?; // sample 10
```

Nothing between the caller and the medium reinterprets the payload: the store
verifies the bytes it returns, it does not know what they mean. The consequence
to rely on is bit preservation — `-0.0`, infinities, subnormals and NaN
payloads included — through every tier, pinned by
`host/tests/typed_payloads.rs`. If you would rather cast than copy, `bytemuck`
or `zerocopy` over your sample array is a caller-side choice; the store never
needs it.

## 3. Rules

### One writer per volume

`Store` takes `&mut self` for every mutation, and owns the allocator. A volume
is written by **one** store at a time; two writers on one device is a
corruption, not a contention problem. An app's shared copy lives behind one
`Mutex<Store>`.

**This is enforced, not merely documented.** Every writable `FileBlockDevice`
takes an advisory exclusive OS file lock on the image at open time, so a second
writable handle — another process, or a second open in the same one — fails
fast with `Error::Locked` instead of placing objects on blocks the first handle
still counts as free. The lock is per *file*, which is coarser than per volume:
a device cannot see where one volume's allocator ends and the next partition
begins, so two writable windows on one image are refused too. Serialise them.

Read-only opens (`FileBlockDevice::open`, the default for every reader in this
crate) take **no** lock and coexist freely — that is what lets a CLI read a
volume a running app still holds. So an app owes its operators two entry
points: a writable one for the app, and a read-only one for everything else.

A *second* handle opened on the same device reads a **snapshot**, not a peer: it
resolves paths through the root it mounted at and will never see the first
handle's commits. Today that reads as merely stale, because nothing is ever
freed — the moment GC is wired up, a stale reader's objects start being reclaimed
underneath it. Treat a second handle as read-only and short-lived.

### Never append to a growing file — chunk it

`append` exists, and using it on a file that grows without bound is the one way
to make this filesystem quadratic. Every commit CoWs the whole file object, so
`append` on an *n*-byte file costs *n* bytes of object space, and a log built by
appending N times writes O(N²) bytes — a 4 MiB transcript can cost gigabytes. An
app whose data is a log, a transcript or a stream should write **bounded chunk
files** (`log-000001.jsonl`, …) and never touch a chunk after committing it: the
cost of a commit is then bounded by the chunk, and total write amplification is
linear in the data. This is not a tuning detail — it is the difference between a
volume that lasts a campaign and one that fills in an afternoon.

That rule is about *committed* appends. A staging session (§3, *Staging: many
writes, one commit*) makes the same traffic linear in **device** bytes, at the
cost of holding the intermediate generations in RAM until the seal — so a
long-running recorder either chunks its files or seals its sessions, and which
one is cheaper depends on how often it can afford the seal.

### A commit is the durability point

Every mutating call is one transaction through the real `CommitEngine`: the
objects placed and the allocator snapshot written, one device barrier, then the
superblock copies, then the barrier that commits them — in that order, which is
not interchangeable. On a `FileBlockDevice` a barrier is `fsync`; on a
`MemBlockDevice` it is a no-op, so an in-memory volume is fast and not durable,
by construction.

The cost is per *call*, not per byte. An app that writes a turn's worth of
files should think of each call as a sync point and not scatter single-byte
writes. A call larger than the volume's object ceiling (`max_object_bytes`,
256 KiB by default) is split into several *objects* but stays **one**
transaction: the pieces, the extent tree over them and the directory entry are
folded and placed together, then committed once.

Worth budgeting in real terms, because it is not small and it does not fall
with the size of the write: **a commit costs about five blocks of volume
metadata** (the allocator snapshot, four superblock copies) **plus one block
for every object in the path it touched** — and CoW re-places every object on
that path, not just the one being changed. A two-level path (directory, file
inode, extent, chunk) is therefore about nine fresh 4 KiB blocks per commit,
which is the 36 KiB the harness measures for a single 256-byte insert.
`BENCHMARKS.md` reports the measured figure as the `volume KiB/op`
column, produced by `benchmarks/` on the reader's own machine; the harness
exists so this number can be re-derived rather than believed.

The corollaries are the ones an app plans around: fewer, larger commits beat
many small ones; a deep path costs more per write than a shallow one; and
**every** mutating verb is one commit, whatever it spans. `write_new_file`'s
create and write are a single transaction, a payload spanning several objects
is folded into that same transaction rather than committed one piece at a time,
and `write_file` over an existing path builds the new content whole and
re-points the entry at it in one transaction — there is no instant at which the
path is absent or empty. `create_file` followed by `append` is the one shape
that still costs two, because it is two calls.

### Staging: many writes, one commit

A capture loop — records arriving faster than a commit can be paid for — uses a
staging session instead of a per-record verb. `begin_staging` opens one;
`staged_append`, `staged_write_new_file` and `staged_remove` run against the
session; `seal_staged` folds everything staged into **one** transaction of the
ordinary shape and commits it. Between the two, no device block is allocated, no
allocator state is touched, and no barrier is paid.

While the session is open, the handle that owns it resolves reads through the
staged root: `read_file`, `read_at`, `stat` and `read_dir` show the caller its
own writes at memory speed, and a staged object costs zero device reads.
`read_file_committed` is the other tier — the last root a superblock published,
which is exactly what a crash at this instant would leave. Any other handle on
the volume sees the committed tier only. That choice is the point: the
application decides how true the data it reads back is, per read.

The seal is the ultimate point of truth. Once it returns, the staged work has
the guarantee every write in this store has — one atomic commit,
BLAKE3-verified on every read, recovered by a plain mount with no new recovery
code. The format does not change: a sealed session is indistinguishable from the
equivalent sequence of commits, except that it is one. Before the seal the
staged tier is RAM only, so a crash loses it exactly — not partially, and never
half-applied.

A capture loop that seals faster than it can afford a full durability point
calls `seal_staged_deferred` instead. It does everything `seal_staged` does —
one transaction, objects written, allocator snapshot written, one device
barrier — and then stops before the superblock write. The new root is live for
this handle and invisible to every other handle and to a crash, which is
exactly the two-tier read story one level down. `sync()` writes the withheld
superblock and flushes it, one barrier, and after that the whole group of seals
since the last `sync()` is as durable as any commit. Sealing *n* times between
syncs therefore costs *n* barriers plus one, where *n* ordinary seals cost 2*n*.

What a crash in that window costs is bounded and honest: the previous root
stays live, so the group since the last `sync()` is lost whole — never a
blend, because the superblock naming it was never written. The allocator
snapshot did ride with the objects, so the recovered volume may have that
group's blocks marked in use while no live root names them: leaked space, never
a wrong answer. Any ordinary commit writes a newer root and clears the pending
one, so a write-heavy program that mixes tiers does not have to track it. The
cost is a real one — an app that never calls `sync()` has a fast, consistent,
*ephemeral* volume — and that is why the durable seal is `seal_staged` and the
deferred one says so in its name.

What that buys, and what it costs:

- The seal is pruned to what the final root reaches, so a session of N appends
  carries N chunks plus **one** generation of each metadata object, not one per
  append: O(N) device bytes instead of the O(N²) that N committed appends cost.
- The transient cost inside the session is still O(N²) — each staged append
  rebuilds the current extent node in RAM — bounded by the capacity passed to
  `begin_staging`. Be generous with RAM; be stingy about how long one file grows
  before you seal.
- Per-object ceilings bind at the seal exactly as at a commit: an extent node's
  ~1500-extent ceiling, and the volume's `max_object_bytes` split, are enforced
  there. A stream longer than those should seal in batches rather than stage one
  enormous file.
- Capacity is enforced in canonical staged bytes, intermediate generations
  included. Exceeding it refuses that call with `Error::StagingFull`, leaving
  the session as it was. A session never seals itself: an implicit seal would be
  an implicit durability point.
- While a session is open, the ordinary mutating verbs and `rebuild_into` are
  refused (`Error::Usage`, naming the session). One staged root, one writer.

**Ordering rule, do not invert:** within `commit`, the allocator snapshot is
persisted before the superblock. The other order can brick a volume — a
published root with no snapshot behind it mounts read-only and a read-only
volume cannot write the snapshot that would fix it.

### Space only falls

CoW rewrites a new copy of every object a transaction touches and **frees
nothing** — GC exists in the kernel crate but is not wired into this path. So
`free_bytes()` is monotonically non-increasing. An always-on store fills up and
stays full. An app that runs indefinitely must watch `free_bytes()` and rotate or
archive, or it will eventually fail a write with out-of-space. This is the one
constraint most likely to bite a long-running agent, which is exactly why
free-space reporting is on the standard surface rather than an optional extra.

An always-on store can also grow far past the size of its live data, and by a
factor rather than a percentage: the volume holds dead versions of live objects,
and nothing in this path can reach them. `BENCHMARKS.md` measures what a volume
costs per small file; multiply by the write rate and the lifetime to get the
number an app has to plan against.

The way back is [`rebuild_into`](crate::rebuild_into) — write the live tree into
a fresh volume and swap it in. It is deliberately not an in-place operation: a
volume has one writer, and the source has to survive until the copy is checked.
The shape of a correct compaction:

1. open the source **read-only** (it needs no free space to be read, so this
   works on a volume with 0 bytes free);
2. `Store::walk()` it — the whole tree, parents before children;
3. `rebuild_into(&src, &mut dst)` into a freshly formatted volume beside it;
4. **verify**: `walk()` both and compare paths, kinds, sizes and bytes;
5. only then `rename` the new image over the old one — one atomic step, so a
   failure anywhere before it leaves the original untouched.

`walk()` and `rebuild_into` are on the public surface so an embedding app can do
this itself. Budget for it: the scratch image is a full-size copy, so compaction
needs as much free *disk* as the volume is large, even though it returns most of
it.

### Overwrite is not a thing; replace is

LedgerFS growth is **extend-only**: `append` and `write_at` add new bytes at or
past the end. Writing *inside* a file's existing bytes is refused
(`Error::Unsupported`) rather than performed, because the extent tree would end
up with two overlapping ranges — a corrupt file, not an updated one. To change a
file's content:

- **replace** it — `write_file(path, data, mode)`.

`write_file` is atomic: the new content is built as objects under no directory
entry, then the entry the path names is re-pointed at it, and both halves are
placed in **one** commit. A reader — or a crash recovery — sees the whole old
version or the whole new one, never an absent path and never an empty file.
(Sequencing `remove` then `create_file` yourself reintroduces exactly that
window; see `durability.rs` for the three states a reader can catch.)

`rename` over the target is also a single transaction, and remains the way to
make *two* files' replacement atomic together — but for rewriting one file's
content, `write_file` alone is enough.

## 4. Identity — recognising your own volume

`format_app(device, "myapp")` writes an ordinary volume plus two files under one
reserved top-level name:

```
/.ledgerfs/APP       the app that owns the volume
/.ledgerfs/VERSION   the contract version (STORE_CONTRACT_VERSION)
```

These are ordinary files, not a format change: `lfs-read` lists them and a plain
`Store::open` reads them. What they buy is `store.app()` — a caller that has just
been handed a device can ask *is this mine, and against which contract?* before
trusting anything it finds. `Ok(None)` is a plain volume; a marker that exists
but cannot be read as text is surfaced as `Error::MarkerUnreadable` rather than
silently treated as unmarked.

An app reserves exactly one top-level name. Keep your data anywhere else.

## 5. What the contract does not cover yet

Stated plainly so an app plans around them rather than discovering them:

- **Cross-directory `rename`** is refused (`Error::Unsupported`). The two CoW
  chains it needs can share an ancestor, and the host cascade does not yet
  reconcile them. Move content with `write_file` + `remove` if you need it.
- **No `rmdir` / recursive delete.** `remove` deletes files; directories are
  refused.
- **No in-place truncate.** Replace the file instead.
- **No GC, no encryption** on the host write path (an encrypted volume fails the
  mount with a clear message rather than decoding garbage).
- **A staging session is RAM only until its seal.** Closing the process, or
  losing power, before `seal_staged` loses everything the session staged — the
  session is a durability *choice*, not a durability guarantee, and the
  committed tier is always the one a crash leaves. On-disk staged durability
  (a format-level fast path) is not implemented, and nothing in the format is
  reserved for one yet. A session's staged writes are limited to files;
  directories cannot be created or removed inside one.
- **A deferred seal is durable only at `sync()`.** `seal_staged_deferred`
  writes and flushes the objects and the allocator snapshot but withholds the
  superblock, so the root it sealed is live for this handle and lost to a crash
  until `sync()` returns. Do not treat its return as a durability point, and do
  not treat a volume whose app never syncs as durable at all; see §3,
  *Staging: many writes, one commit*.
- **A session is not a batch across handles.** Only the handle that opened it
  sees the staged tier, and every other handle sees the committed tier. There is
  no cohort commit: one `seal_staged`, one root.
- **A directory is flat and bounded at 400 entries.** `DirNode` entries live in
  one object and there is no split, so the 401st child fails the write. Partition
  deep fan-out across subdirectories; do not plan on a `node_modules`-shaped
  directory. See `KNOWN_ISSUES.md` §2.
- **No partial or streaming read.** `read_file` returns the whole file;
  `read_at(path, offset, len)` gives a byte range, clamped to the file (past the
  end is an empty vector, not an error) — enough to tail an append-only file,
  not enough to stream one you have not sized.
- **`..` is refused.** The store resolves downward from a root object and keeps
  no parent pointers, so `..` cannot be honoured without re-walking the parent.
  `Error::Usage` is the answer rather than silently dropping the component —
  `../etc/passwd` must not quietly become `etc/passwd` inside the volume. Build
  every path from the store root.

## 6. Reading a volume without the app

A store is not `cat`-able, `grep`-able, or editable. That is the price of the
format, and it means an app that embeds LedgerFS owes its operators an **export
path**: `lfs-read` is the read-only CLI that lists and extracts a volume
(`lfs-read ls`, `lfs-read cat`) for humans and for tools that expect files. An
app whose data is looked at by anything other than itself should ship or document
one — the format's opacity is a feature for integrity and a liability for
observability, and the export path is how the two are reconciled.

## 7. Conformance

- The contract as running code: `examples/app_store.rs`
  (`cargo run --example app_store`).
- The staging tier, both reads and the seal: `examples/staging_capture.rs`
  (`cargo run --example staging_capture`) — stage, read both tiers mid-session,
  seal, reopen from the volume's bytes.
- The verb-by-verb behaviour, including every refusal above, is covered by the
  `store::tests` suite in `src/store.rs`.
- The in-memory, relocatable half of the story — snapshot the bytes, restore
  them, get your data back — is `examples/embed.rs`.
- The hosted half — a volume in a real file, the one-writer lock, a read-only
  open beside a live writer, the hint sidecar written on close — is
  `examples/disk_volume.rs`.
- The seam itself, implemented by an embedder over storage of its own, is
  `examples/custom_device.rs`: four methods, and the filesystem above works
  unchanged.
