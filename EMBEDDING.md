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
that path, not just the one being changed. A two-level path (root, directory,
file inode, extent, chunk) is therefore five or six fresh 4 KiB blocks per
commit. `BENCHMARKS.md` reports the measured figure as the `volume KiB/op`
column, produced by `benchmarks/` on the reader's own machine; the harness
exists so this number can be re-derived rather than believed.

The corollaries are the ones an app plans around: fewer, larger commits beat
many small ones; a deep path costs more per write than a shallow one; and
`write_new_file` is **one** commit — the create and the write are a single
transaction, and a payload spanning several objects is folded into that same
transaction rather than committed one piece at a time. The one verb that costs
two is `write_file` replacing an existing path, which is a remove followed by a
create; `create_file` plus `append` is the same cost, and `rename` over the
target is one commit if you need an atomic replace.

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

- **replace** it — `write_file(path, data, mode)` — for a plain rewrite; or
- **atomically replace** it — write a scratch name, then `rename` it over the
  target — when a crash must never expose a half-written file.

The scratch-then-rename form is the one to reach for when the file is something
another process (or another turn) reads. `rename` is a single transaction and a
same-directory rename is a single CoW chain, so it is the one atomic-replace
primitive the store offers.

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
- **No batch commit in the host API.** Each verb is its own transaction; a
  multi-op transaction needs an in-memory overlay the host `Store` does not
  build, though the kernel side is closer to it than the API suggests
  (`cow::apply_ops_to_root` already takes a *list* of operations, and refuses
  when one op's input feeds another's path). A single verb that touches a deep
  path *is* still one commit — the ancestor cascade merges several objects into
  one.
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
