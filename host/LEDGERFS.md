# LedgerFS — an application-controlled, embeddable filesystem

LedgerFS is a **copy-on-write, content-addressed, crash-consistent
filesystem** whose defining move is that a **volume is a filesystem, not a
file**: it is a self-contained tree of files with a commit protocol behind
every write, and an ordinary application can own one, embed it in its own
process, and carry it around as a single byte string. This document is the
high-level overview — what it is, how an application adopts it, what it
guarantees, and what "the game change" actually means.

It complements two narrower documents that already live here:

- `EMBEDDING.md` — the **application-store contract**: the rules an app
  follows (one writer per volume, commit = durability point, space only falls,
  replace-not-overwrite, the `/.ledgerfs` identity convention).
- the crate-level `lib.rs` docs and `crucible-ledgerfs-kernel` — the on-disk
  format and the CoW operations.

---

## 1. The short version

- A **volume** is a flat array of 4 KiB blocks. Everything the filesystem
  knows — every file, every directory, every version — is an object
  **addressed by its content** (a BLAKE3 hash) and written **copy-on-write**.
- A **commit** places the new objects, writes an allocator snapshot, then a
  superblock, and issues barriers between them. The moment the superblock
  lands, the new tree is the filesystem — atomically. Before that, the old one
  is. There is no in-between state a crash can leave behind.
- A **`Store`** (host) is `mkfs` + mount + the mutation verbs, all in one
  type, over any `BlockDevice`. The device can be a disk partition, a bare
  file, or a `Vec<u8>` in the caller's heap.
- An app **names its volume** (`format_app(device, "myapp")`) and can ask a
  volume `is this mine, and against which contract?` before trusting it.

The result: an application that used to `mkdir` and `File::write` on the host
can instead run a real filesystem inside itself — isolated per volume,
crash-consistent per commit, and relocatable as bytes.

---

## 2. Two layers: a format kernel and an embeddable host

LedgerFS is split so the format can run where a host process cannot.

| layer | crate | what it owns |
|---|---|---|
| **kernel** | `crucible-ledgerfs-kernel` (`no_std`) | the on-disk format (superblock, `ObjectRecord` framing, object model) and the CoW operations (`read`, `readdir`, `mkdir`, `write`, `unlink`, `rename`) — generic over an `ObjectStore` trait. No I/O of its own. |
| **host** | `crucible-ledgerfs-host` (this crate, `std`) | one `ObjectStore` implementation over a `BlockDevice`, the mount/path plumbing, and the `Store` that commits through the kernel's real `CommitEngine`. This is what an application links. |

The kernel crate is the single source of truth for the format; the host crate
re-derives no offset and no checksum. Inside CrucibleOS the same commit path
drives `/data`; on the host the same path drives a bare file or a heap buffer.

---

## 3. The model: a volume is a filesystem, not a file

This is the sentence to internalise, because it is what changed.

- **Content addressing.** Every object's identity is
  `BLAKE3(payload)`. Two identical payloads are the same object; a rewrite
  makes a *new* object and leaves the old one in place. Nothing is modified in
  place, ever.
- **Copy-on-write.** Touching a file rewrites the file inode, its extent, its
  chunk, and then CoW-copies the immediate parent directory up the ancestor
  chain to the volume root. The root inode is therefore rewritten on **every**
  commit.
- **The root is a value.** The superblock names the current root object by id
  (plus a placement hint). "The filesystem" is just whichever root the
  superblock points at — which is why a commit is atomic and a rollback is
  free.
- **Four superblock copies.** Copy 0 is primary; the other three exist so one
  damaged copy is survivable. `mount_select` picks a consistent, committed
  root across them, and reports disagreement rather than averaging over it.

A volume is ~`total_blocks × 4 KiB` of flat blocks: two reserved regions
(header, trailer), four superblock copies, an allocator-snapshot block, and
the rest is object space. That is the whole layout an application has to know.

---

## 4. Embedding: how an application adopts it

### 4.1 A `BlockDevice` is the seam

Nothing above the device cares where the bytes live:

- `FileBlockDevice` — a disk image or a **bare file** (`base_offset = 0`), or a
  window into a partitioned image.
- `MemBlockDevice` — a volume in the process's own heap. Hand back
  `bytes_snapshot()` and you have a valid volume file; `from_bytes()` mounts it
  again. This is what makes a store **relocatable and testable in memory**.

### 4.2 The `Store` lifecycle

```
Store::format_app(device, "myapp")   // mkfs + stamp identity, for a NEW store
Store::format_device(device)          // mkfs, plain volume
Store::open(device)                   // mount an existing volume
```

`format_app` formats a volume and writes three ordinary files under one
reserved top-level name:

```
/.ledgerfs/APP       the app that owns the volume
/.ledgerfs/VERSION   the contract version (STORE_CONTRACT_VERSION)
```

They are ordinary files, not a format change — a plain reader lists them — but
they let a caller ask `store.app()` / `store.contract_version()` and refuse a
volume that is not its own before trusting anything on it.

### 4.3 The verbs

Everything returns one `Error` enum (hand-written `Display`, no
`anyhow`/`thiserror`). The surface an app programs against:

| verb | does |
|---|---|
| `read_file(path)` / `read_dir(path)` / `stat(path)` | read |
| `mkdir(path, mode)` / `create_file(path, mode)` | create a container |
| `write_new_file(path, data, mode)` | create **and** write; refuses if taken |
| `write_file(path, data, mode)` | create **or replace** — the idempotent write |
| `append(path, data, mode)` | create-if-absent, else write at the current end |
| `write_at(path, offset, data)` | extend at `offset` (≥ current size; overwriting stored bytes is refused) |
| `remove(path)` | delete a file |
| `rename(from, to)` | **same-directory** rename — the atomic-replace primitive |
| `free_bytes()` / `reserved_bytes()` / `total_free_bytes()` | the space budget |
| `capacity_bytes()` / `total_blocks()` | the geometry |

A call larger than the volume's object ceiling (`max_object_bytes`, 256 KiB by
default) becomes several transactions internally; an app writes a turn's worth
of files, not bytes.

---

## 5. The guarantees

- **Atomic commit.** A mutation is one transaction through the real
  `CommitEngine`: objects placed, data written, ≥2 device barriers (allocator
  snapshot, then superblock — that order is not interchangeable). On a file
  device a barrier is `fsync`; on a memory device it is a no-op (fast, not
  durable, by construction).
- **Crash consistency.** Because the root is a value and the superblock is the
  last thing written, a crash leaves either the old tree or the new one —
  never a torn one. The snapshot-before-superblock order specifically avoids
  bricking a volume (a published root with no snapshot behind it mounts
  read-only and cannot write the snapshot that would fix it).
- **Integrity.** Every `ObjectRecord` carries a magic, a length, a BLAKE3
  checksum over the stored bytes, and an `object_id` that is `BLAKE3(payload)`.
  A payload read back is verified end to end, with optional LZ4/zstd
  compression when the record says so.
- **Isolation.** One volume per writer. There is no cross-volume coupling; a
  volume is a closed tree.

---

## 6. What it deliberately does not do yet

Stated plainly so an app plans around them (full list in `EMBEDDING.md` §5):

- **No GC.** CoW frees nothing on the commit path yet, so `free_bytes()` is
  monotonically non-increasing. An always-on store must watch it and
  rotate/archive. This is why free-space reporting is on the standard surface,
  not an optional extra.
- **Overwrite is not a thing; replace is.** `write_at`/`append` extend only;
  changing a file means `write_file` (replace) or scratch-name + `rename`
  (atomic replace).
- **Cross-directory `rename`** is refused (the two CoW chains can share an
  ancestor the host cascade does not yet reconcile). **No `rmdir`/recursive
  delete. No in-place truncate.**
- **No host-path encryption** (an encrypted volume fails the mount with a
  clear message rather than decoding ciphertext). **No batch commit** (each
  verb is its own transaction).
- A **directory holds a bounded number of entries** (the `DirNode` B-tree does
  not split yet), so a volume with thousands of files spreads them across
  subdirectories.

These are format- and path-level facts, not doc caveats; an app that respects
them gets the guarantees above.

---

## 7. Where it runs today

- **CrucibleOS `/data`** — the same commit path, driven by `fs-cbl`.
- **`bugs` (the rebounty relay)** — each bug-bounty campaign is a **single
  LedgerFS volume** (`campaigns/<name>.ledgerfs`). The whole campaign —
  `SCOPE.md`, `BRIEF.md`, `state.json`, handovers, findings, notes, evidence,
  kits, and the append-only relay transcript at `/relay/transcript.jsonl` —
  lives inside one file. The relay agent's own tools read and write that
  volume; a legacy campaign directory migrates into a volume byte-for-byte;
  and any campaign can be exported back to a host directory.

That last one is the clean demonstration of the model: a bug-bounty campaign
that was a loose folder of files is now a **single application-owned
filesystem** — isolated, crash-consistent, portable as bytes, and exportable
on demand.

---

## 8. Minimal example

```rust
use crucible_ledgerfs_host::{MemBlockDevice, Store};

let device = MemBlockDevice::new(2048);          // 8 MiB in the heap
let mirror = device.clone();                     // keep a way back to the bytes
let mut store = Store::format_app(Box::new(device), "myapp")?;

// An append-only log: created by its first line, durable on each append.
store.append("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;
store.append("/log.jsonl", b"{\"turn\":2}\n", 0o644)?;

// An atomic replace: build under a scratch name, then rename over the target.
store.write_new_file("/state.new", b"{\"phase\":\"stopped\"}\n", 0o644)?;
store.rename("/state.new", "/state")?;

// The volume is a byte string. Relocate it, hand it to another process, store it.
let bytes = mirror.bytes_snapshot();

let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
assert_eq!(reopened.app()?.as_deref(), Some("myapp"));
assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
```

Run it: `cargo run --example app_store`. The in-memory, relocatable half is
`examples/embed.rs`.

---

## 9. Why this is "the game change"

A conventional application stores its state as **files in a shared, host-owned
directory**. That directory is not a thing the app controls: its consistency is
whatever the OS and a crash happen to leave, its isolation is a naming
convention, and its portability is a tarball.

LedgerFS inverts that:

- **Application-controlled.** The app owns a *filesystem*, not a folder. The
  volume names its owner and contract version; the app decides what is valid,
  and can atomically publish a new state per commit.
- **Embeddable.** The store is a library over a `BlockDevice`. It runs
  in-process, in a heap `Vec`, in a bare file, or on a CrucibleOS partition —
  the same code. No root, no mount point, no OS cooperation required.
- **Crash-consistent and verifiable.** Every write is a transaction that ends
  in a superblock; every object is checksummed and content-addressed. The
  recovery story is "the root is whatever the superblock names", not
  "fsck".
- **Self-contained and portable.** A volume is a byte string. Copy it, ship
  it, snapshot it, archive it, restore it — it is the whole filesystem.

The practical upshot for this repo: `bugs` went from campaign directories on
disk to **one LedgerFS volume per campaign**, and the standard that made that
safe is written down in `EMBEDDING.md`.

---

## See also

- `EMBEDDING.md` — the application-store contract (the rules).
- `examples/app_store.rs` — the contract as running code.
- `examples/embed.rs` — in-memory, relocatable volumes.
- `crucible-ledgerfs-kernel` — the on-disk format and CoW operations.
