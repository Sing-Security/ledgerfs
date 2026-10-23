# ledgerfs-host

**Sing Security Research Laboratory**

The `std` host of LedgerFS: a volume formatted, opened and modified inside the
calling process.

## Abstract

This crate supplies the parts of the system that the format crate leaves
abstract. It implements `ObjectStore` over a `BlockDevice`, provides device
implementations for a file region and for memory, and exposes `Store`, whose
methods are `mkfs`, open, the file operations and the staging tier. It works
over any byte-addressed device: a partition, a disk image, a bare file, or a
buffer in memory.

A volume can serve as an application's storage layer. It holds records and
returns them. It is not a database engine: no schema, no query language, no
secondary index and no in-place update. A catalog, indexes and a query executor
are a layer that can be built on this one, and that work is not part of this
release. What this release provides is the storage layer such a layer would sit
on, with a format specified byte by byte.

```rust
use ledgerfs_host::{MemBlockDevice, Result, Store};

fn main() -> Result<()> {
	let device = MemBlockDevice::new(2048);
	let mirror = device.clone();
	let mut store = Store::format_app(Box::new(device), "example")?;
	store.write_new_file("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;

	// Many appends, one durability point.
	store.begin_staging(1024 * 1024)?;
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
the same calls. A `FileBlockDevice` opened for writing takes an advisory
exclusive lock and refuses a second writer. A `MemBlockDevice` is a `Vec<u8>`
behind an `Arc<Mutex<...>>` that can be snapshotted to bytes and rebuilt, which
is how the durability tests simulate a power loss.

The partition case is a supported deployment. `FileBlockDevice` opens an
existing region at a byte offset, so the same `Store` drives a partition inside
a disk image or a partition device node. `lfs-write <TARGET> mkfs --part data`
formats one in place, and `lfs-read <IMAGE> parts` reads the partition table
back to locate it. Writing the partition table is the installer's task; this
crate reads a table that is already present.

## 1. Write path

Each method is one transaction. The objects and the allocator snapshot are
written, a barrier flushes them, the four superblock copies naming the new root
are written, and a second barrier flushes those. Before the superblock write the
previous root is the volume; after it, the next root is. A crash produces one or
the other, never a blend.

For a stream, `begin_staging` opens a session. `staged_append` extends an
in-memory draft of the target file and touches no device block, no allocator
state and no barrier. `seal_staged` builds the objects those appends imply and
commits them once. Reads select their tier while a session is open:
`read_file`, `read_at`, `stat` and `read_dir` answer from the staged view, and
`read_file_committed` answers from the generation the superblock names. A crash
before the seal loses the staged work and nothing else.

## 2. Contents

| module | contents |
|---|---|
| `device` | `BlockDevice`, `FileBlockDevice`, `MemBlockDevice`, the hint cache, volume identity |
| `store` | `Store`: format, open, the file operations (`read_file`, `read_dir`, `stat`, `mkdir`, `create_file`, `write_new_file`, `write_file`, `append`, `write_at`, `remove`, `rename`), staging, and the free-space and geometry accessors |
| `staging` | `Staging`, the scoped handle over a staging session |
| `volume` | `Volume`, the lower-level mount for driving the kernel's `CommitEngine` directly |
| `path`, `mutate`, `objstore` | path resolution, mutation plumbing, and the `DiskObjectStore` that reads and writes records |
| `image` | partition table parsing, for locating a volume inside a whole-disk image |
| `rebuild` | `rebuild_into`, which copies this volume's live tree into another and drops dead objects |
| `bin` | `lfs-read` and `lfs-write`, diagnostic tools for a volume |
| `error` | one hand-written `Error` enum; no `anyhow`, no `thiserror` |

## 3. Boundaries

There is no VFS registration and no mount point: no path on the host resolves
into a LedgerFS volume. A program opens a volume with `Store` and calls its
methods. An operating system adopts the library as its filesystem by writing the
service layer above it; CrucibleOS does this, and that service is what serves
file descriptors to other processes.

The `data-encryption` and `reader` features are not enabled here. The first
would place a key-management surface in a library that has no answer for where
the key resides; the second compiles ring-0 ATA port I/O into a userspace
binary. A volume formatted with encryption enabled fails to mount with a
diagnostic rather than being decoded as ciphertext.

The behavioural limits of a volume, and their causes, are listed in
`KNOWN_ISSUES.md`. No shipping path frees space, so a volume fills
monotonically. `write_at` cannot rewrite stored bytes. A directory holds at most
400 entries. An append to a growing file is quadratic in device bytes; bounded
chunk files restore linear cost.

The full contract an embedding application follows (one writer per volume, the
commit as the durability point, replacement rather than overwriting, and the
`/.ledgerfs` identity convention) is in
[`EMBEDDING.md`](https://github.com/Sing-Security/ledgerfs/blob/main/EMBEDDING.md).
The vocabulary is in
[`TERMINOLOGY.md`](https://github.com/Sing-Security/ledgerfs/blob/main/TERMINOLOGY.md).

```sh
cargo test -p ledgerfs-host
cargo run --example app_store -p ledgerfs-host
```

## License

Dual-licensed: Apache-2.0 or MIT, at your option. The license texts ship inside
the published crate as `LICENSE-APACHE`, `LICENSE-MIT` and `NOTICE`, and are
also at
[LICENSE-APACHE](https://github.com/Sing-Security/ledgerfs/blob/main/host/LICENSE-APACHE),
[LICENSE-MIT](https://github.com/Sing-Security/ledgerfs/blob/main/host/LICENSE-MIT)
and
[NOTICE](https://github.com/Sing-Security/ledgerfs/blob/main/host/NOTICE).
