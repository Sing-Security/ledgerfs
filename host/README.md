# ledgerfs-host

An embedded filesystem: a volume you format, write files to and read back,
inside your own process.

This is the crate an application depends on. It supplies the one `ObjectStore`
implementation the kernel crate's copy-on-write operations run against, the path
and mount plumbing, and `Store` — `mkfs`, mount, and the verbs in a single type,
over any `BlockDevice`.

You can use it as your application's database: it holds your records and gives
them back. What it is not is a database **engine** — no schema, no query
language, no indexes, no in-place update. It is a different solution to the same
problem, and the difference is what the storage being a filesystem buys: the
whole store is one file that copies and ships as one artifact, nothing has to
run beside it, every object is content-addressed and verified on the way out,
and every write is atomic. What it costs is some speed on the smallest durable
writes — a commit is two barriers, and that order is the crash argument — which
a staging session amortizes over a stream.

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

Swap `MemBlockDevice` for `FileBlockDevice` and the same calls drive a bare file
or a partition. A `FileBlockDevice` opened for writing takes an advisory
exclusive lock and refuses a second writer; a `MemBlockDevice` is a `Vec<u8>`
behind an `Arc<Mutex<…>>` that can be snapshotted to bytes and rebuilt, which is
how the durability tests simulate a power cut.

The partition case is a real deployment, not a test shape. `FileBlockDevice`
opens an existing region at a byte offset, so the same `Store` drives a partition
inside a disk image or a partition device node; `lfs-write <TARGET> mkfs --part
data` formats one in place, and `lfs-read <IMAGE> parts` reads the table back to
locate it. Writing the partition table is the installer's job, not this crate's —
`image` reads a table that is already there.

## The write path

Each verb is one transaction: the objects and the allocator snapshot are
written, a barrier flushes them, the four superblock copies naming the new root
are written, and a second barrier flushes those. Before the superblock write the
old root is the filesystem; after it, the new one. A crash never leaves a blend.

For a capture loop, `begin_staging` opens a session in which `staged_append`
extends in-memory drafts and touches no device block, no allocator state and no
barrier; `seal_staged` folds the session into one ordinary commit. The seal is
the durability point, and reads choose their side of it: `read_file`,
`read_at`, `stat` and `read_dir` answer from the staged view while a session is
open, and `read_file_committed` answers only from the superblock's generation.
A crash before the seal loses the staged work and nothing else.

## What is in the crate

| | |
|---|---|
| `device` | `BlockDevice`, `FileBlockDevice`, `MemBlockDevice`, the hint cache, volume identity |
| `store` | `Store` — format, open, and the verbs (`read_file`, `read_dir`, `stat`, `mkdir`, `create_file`, `write_new_file`, `write_file`, `append`, `write_at`, `remove`, `rename`, and the free-space and geometry accessors) |
| `staging` | `Staging` — the scoped handle over a staging session |
| `volume` | `Volume` — the lower-level mount, for driving the kernel's `CommitEngine` directly |
| `path`, `mutate`, `objstore` | path resolution, mutation plumbing, and the `DiskObjectStore` that reads and writes records |
| `image` | partition table parsing, for finding a volume's window inside a whole-disk image |
| `rebuild` | `rebuild_into` — copy this volume's live tree into another, dropping dead objects |
| `bin` | `lfs-read`, `lfs-write` — small tools for looking at a volume from a shell |
| `error` | one hand-written `Error` enum; no `anyhow`, no `thiserror` |

## What it does not do

`data-encryption` and `reader` are deliberately not enabled: the first would put
a key-management surface in a library that has no answer for where the key lives,
and the second compiles ring-0 ATA port I/O into a userspace binary. A volume
formatted with encryption enabled fails to mount here with a clear message rather
than decoding ciphertext.

What a volume does not do, and why: `KNOWN_ISSUES.md` is the honest list. Free
space falls monotonically (nothing collects yet), `write_at` cannot rewrite
stored bytes, and a directory is capped at 400 entries.

For the full contract an application follows — one writer per volume, commit as
the durability point, replace rather than overwrite, and the `/.ledgerfs`
identity convention — see
[`EMBEDDING.md`](https://github.com/Sing-Security/ledgerfs/blob/main/EMBEDDING.md).
For the vocabulary, see
[`TERMINOLOGY.md`](https://github.com/Sing-Security/ledgerfs/blob/main/TERMINOLOGY.md).

```sh
cargo test -p ledgerfs-host
cargo run --example app_store -p ledgerfs-host
```

## License

Dual-licensed: Apache-2.0 or MIT, at your option. See
[`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT), and
[`NOTICE`](NOTICE) in this directory.
