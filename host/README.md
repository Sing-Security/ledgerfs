# ledgerfs-host

The `std` half of LedgerFS. It supplies the one `ObjectStore` implementation the
kernel crate's copy-on-write operations run against, the path and mount
plumbing, and `Store` — `mkfs`, mount, and the mutation verbs in a single type,
over any `BlockDevice`.

```rust
use ledgerfs_host::{MemBlockDevice, Result, Store};

fn main() -> Result<()> {
	let device = MemBlockDevice::new(2048);
	let mirror = device.clone();
	let mut store = Store::format_app(Box::new(device), "example")?;
	store.write_new_file("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n");
	Ok(())
}
```

Swap `MemBlockDevice` for `FileBlockDevice` and the same calls drive a bare file
or a partition. A `FileBlockDevice` opened for writing takes an advisory
exclusive lock and refuses a second writer; a `MemBlockDevice` is a `Vec<u8>`
behind an `Arc<Mutex<…>>` that can be snapshotted to bytes and rebuilt, which is
how the durability tests simulate a power cut.

## What is in the crate

| | |
|---|---|
| `device` | `BlockDevice`, `FileBlockDevice`, `MemBlockDevice`, the hint cache, volume identity |
| `store` | `Store` — format, open, and the verbs (`read_file`, `read_dir`, `stat`, `mkdir`, `create_file`, `write_new_file`, `write_file`, `append`, `write_at`, `remove`, `rename`, and the free-space and geometry accessors) |
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

For the full contract an application follows — one writer per volume, commit as
the durability point, replace rather than overwrite, and the `/.ledgerfs`
identity convention — see [`EMBEDDING.md`](../EMBEDDING.md). For what the format
guarantees and what it does not, see [`KNOWN_ISSUES.md`](../KNOWN_ISSUES.md).

```sh
cargo test -p ledgerfs-host
cargo run --example app_store -p ledgerfs-host
```

## License

Apache-2.0. See [`LICENSE`](../LICENSE) and [`NOTICE`](../NOTICE).
