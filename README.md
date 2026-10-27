# LedgerFS

**Sing Security Research Laboratory**

[![CI](https://github.com/Sing-Security/ledgerfs/actions/workflows/ci.yml/badge.svg)](https://github.com/Sing-Security/ledgerfs/actions/workflows/ci.yml)

An embedded copy-on-write filesystem for records written once and read back
verified. A volume is a single byte string: a file, a partition, a disk image or
a buffer in memory. There is no server, no write-ahead log and no replay step at
mount.

LedgerFS is research-grade software at version 0.x. The API may change, and the
crates are not yet published to crates.io.

## Why LedgerFS

- **Crash consistency by ordering, not by a log.** A commit writes the new
  objects and the allocator snapshot, issues a barrier, writes the superblock
  copies that name the new root, and issues a second barrier. A crash leaves the
  previous root or the new one, never a blend. A mount has no log to replay.
- **Verified reads.** Every object is immutable and named by the BLAKE3 hash of
  its canonical encoding. A read checks the record framing, its checksum and the
  identifier before it answers.
- **Application-chosen durability.** Stage high-frequency writes in memory and
  seal them into one ordinary commit when the application decides. Reads choose
  the staged view or the committed view, per call.
- **A portable volume.** A snapshot is a copy and an archive is the volume
  itself. Copy the bytes, carry them, reopen them with no service.
- **A `no_std` core.** The format crate performs no I/O and runs in a kernel, a
  userspace process or a test harness over different stores.

## Quick start

Until the first crates.io release, depend on this repository:

```toml
[dependencies]
ledgerfs-host = { git = "https://github.com/Sing-Security/ledgerfs" }
```

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

The program writes one record, stages a second, reads both tiers, seals the
session into a single commit, and reopens the volume from its bytes. Run it with
`cargo run` in a new crate that depends on `ledgerfs-host`, or use
`host/examples/app_store.rs` in this repository.

`FileBlockDevice` replaces `MemBlockDevice` to drive a file or a partition with
the same calls. A writable file device takes an exclusive OS lock; a second
writer is refused.

## Command line

The host crate ships two tools. From a clone of this repository:

```sh
cargo run -p ledgerfs-host --bin lfs-write -- <TARGET> <COMMAND>
cargo run -p ledgerfs-host --bin lfs-read  -- <IMAGE> <COMMAND>
```

`lfs-write` creates and modifies volumes:

| command | effect |
|---|---|
| `mkfs [--blocks N]` | format `TARGET` as a LedgerFS volume |
| `info` | how the volume was obtained, whether it takes writes, space left |
| `mkdir <PATH>` | create a directory |
| `put <LOCAL> <PATH>` | write a host file to `PATH`, creating it |
| `append <LOCAL> <PATH>` | append a host file's bytes, creating the file if absent |
| `rm <PATH>` | delete a file |
| `mv <FROM> <TO>` | rename within one directory |
| `verify <PATH> [LOCAL]` | read back, and compare byte for byte when `LOCAL` is given |
| `compact <DEST>` | rebuild `TARGET` into a new volume file, verified; both are bare |

`lfs-read` inspects a volume without a running system:

| command | effect |
|---|---|
| `parts` | list the image's MBR partitions |
| `info` | superblock copies, mount selection and root summary |
| `ls [PATH]` | list a directory |
| `cat <PATH>` | write a file's contents to stdout |
| `stat <PATH>` | show one inode's metadata |

Both tools accept `--bare` when the target is a whole volume rather than a
partitioned image, `--part system|data` to select a partition, and
`--passphrase` or `--key-file PATH` to unlock an encrypted volume. A short
session:

```sh
cargo run -p ledgerfs-host --bin lfs-write -- ./capture.lfs mkfs --bare --blocks 8192
cargo run -p ledgerfs-host --bin lfs-write -- ./capture.lfs put ./events.jsonl /events.jsonl --bare
cargo run -p ledgerfs-host --bin lfs-write -- ./capture.lfs verify /events.jsonl ./events.jsonl --bare
cargo run -p ledgerfs-host --bin lfs-read  -- ./capture.lfs cat /events.jsonl --bare
```

Use `cargo install --path host` to install both binaries locally.

## Staging: choose the durability point

A commit per record is the honest guarantee, and for a high-frequency stream it
is also the dominating cost. A staging session lets the application accept
writes at memory speed and publish them at a boundary it chooses:

```rust
store.begin_staging(64 * 1024 * 1024)?;
for record in &records {
	store.staged_append("/capture.log", record, 0o644)?;
}
store.seal_staged()?; // one ordinary commit, durable when it returns
```

- Staged appends touch no device block, no allocator state and no barrier.
- The owning handle reads its own staged writes from memory, with no device
  read. Every other handle sees the committed generation.
- `read_file_committed` answers from the last published root, even on the owning
  handle. The read tier is chosen per call.
- A crash before the seal loses the staged work and nothing else. After the seal
  the data has the same guarantee as any other commit, and a plain mount
  recovers it with no special code.
- The capacity passed to `begin_staging` is a byte budget. Exceeding it returns
  `Error::StagingFull` and leaves the session intact; nothing is sealed
  implicitly.

`seal_staged_deferred` writes the objects and the allocator snapshot but not the
superblock, so several seals can share one `sync`:

```rust
store.seal_staged_deferred()?;
// ...
store.sync()?; // publishes every deferred seal since the last sync
```

A deferred group is published whole or lost whole. See
[`EMBEDDING.md`](EMBEDDING.md) for when to use each mode and
[`BENCHMARKS.md`](BENCHMARKS.md) §5 for the measured difference.

## Encryption

Encryption is a property of the volume. Format with a credential, open with the
same credential, and every call is unchanged:

```rust
use ledgerfs_host::{MemBlockDevice, Result, Store, UnlockCredential};

fn main() -> Result<()> {
	let passphrase = b"a passphrase for the demo";
	let device = MemBlockDevice::new(4096);
	let mirror = device.clone();

	let mut store = Store::format_encrypted(
		Box::new(device),
		4096,
		UnlockCredential::Passphrase(passphrase),
	)?;
	store.write_new_file("/secret.txt", b"classified\n", 0o600)?;

	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open_encrypted(
		Box::new(MemBlockDevice::from_bytes(bytes)?),
		UnlockCredential::Passphrase(passphrase),
	)?;
	assert_eq!(reopened.read_file("/secret.txt")?, b"classified\n");
	Ok(())
}
```

- XTS-AES-256 over the object region, below the object model. Content
  addressing, the extent tree and every verb run unchanged on top of it.
- Two 128-byte keyslots in the superblock. A passphrase slot derives a
  key-encryption key with iterated BLAKE3 (200,000 rounds by default); a second
  slot kind is sealed under a supplied 32-byte key-encryption key for an HSM or
  a machine-held secret.
- `Store::add_slot`, `Store::revoke_slot` and `Store::change_passphrase` manage
  the lifecycle. Changing a passphrase rewraps the 64-byte master key; the
  object region is not re-encrypted.
- The superblock, intent log and allocation snapshot stay plaintext. Contents
  are protected, metadata is not. Losing every credential loses the volume.
- There is no master-key rekeying and no per-file key hierarchy.

[`FORMAT.md`](FORMAT.md) §36 is the format description and [`DESIGN.md`](DESIGN.md)
§7 explains the layering.

## Feature flags

The `ledgerfs` crate exposes five feature sets:

| feature | effect |
|---|---|
| *(none)* | the format and the copy-on-write operations, `no_std` + `alloc` |
| `std` | pulls the LZ4 and Zstd decoders that `compression` gates, so a record written under either policy decodes |
| `reader` | compiles the ATA PIO path and its port-I/O instructions. Ring-0 code, x86-64 only, off by default |
| `data-encryption` | transparent XTS-AES-256 over a volume's raw blocks, applied below the object model and selected when the volume is formatted |
| `temper_sealing` | ML-DSA-65 and SLH-DSA sealing of audit records, through `temper-rs`. Off by default; no caller in this repository |

`ledgerfs-host` is `std` and enables `ledgerfs/std` and
`ledgerfs/data-encryption` for you. It deliberately does not enable `reader`,
which would compile ring-0 port I/O into a userspace binary.

## Performance

Median cost per operation on one machine, with a dm-crypt mapping over an NVMe
device so every engine pays the same encryption cost. Each engine uses the
durability setting its users would choose: SQLite WAL with `synchronous=FULL`,
redb with `Durability::Immediate`, and a raw control that syncs the file and its
directory.

| workload | LedgerFS | SQLite WAL | redb | raw `fsync` |
|---|---:|---:|---:|---:|
| Durable insert, 256 B | 9.18 ms | 3.79 ms | 4.13 ms | 7.10 ms |
| Verified point read, 256 B | 25.5 µs | 4.2 µs | 949 ns | 2.1 µs |
| Batched append, 960 B per batch | 16.87 ms | 9.25 ms | 9.87 ms | 9.09 ms |
| Staged append, seal every 10 batches | 2.05 ms | 1.25 ms | 981 µs | 869 µs |

The staged row reports per-batch cost with the seal included. The engines do not
perform identical work at the seal, so a row is not a general comparison of the
systems. [`BENCHMARKS.md`](BENCHMARKS.md) records the method, the full result
set, the comparability rules and what is not measured.

## Limits

The short list. [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) states each limit, its
cause and its workaround, and [`DESIGN.md`](DESIGN.md) §8 records the trade.

- **Space is not reclaimed.** No shipping path frees a block, so a volume fills
  monotonically. `rebuild_into` and `lfs-write compact` rebuild into a new
  volume; that is manual compaction, not a collector.
- **Appending to one growing file is quadratic in device bytes.** Staging
  removes the per-append rebuild, but each seal still rewrites the extent node.
  Bounded chunk files restore linear cost.
- **A directory holds at most 400 entries and does not split.** Object payloads
  are at most 256 KiB, names at most 256 bytes.
- **No in-place update.** `write_at` appends at or past the end; `write_file`
  replaces the content in one transaction. Stored bytes are never rewritten.
- **One writer per volume.** The file device takes an exclusive lock. On Unix it
  is advisory and a reader can coexist; on Windows it is mandatory and refuses a
  second handle's read.
- **No VFS integration.** Nothing registers with the host operating system, so
  no host path resolves into a volume. An OS adopts the library by writing the
  service layer above it.
- **Encryption protects contents, not metadata.** The passphrase KDF is iterated
  BLAKE3 and not memory-hard. A whole-volume rollback is not detected.
- **Tier B subsystems are not available.** Deduplication, snapshots, garbage
  collection, the scrubber and content-defined chunking live in the unpublished
  `pending/` crate with no caller. Replication, the audit chain and record
  sealing are present with no shipping caller. Four of those subsystems report
  success without performing the work; `KNOWN_ISSUES.md` §11 names them.
- **Cold reads scan.** Storage hints are not persisted, so a process that opens
  a volume it did not write scans for the first lookup of each object. The
  benchmark reads are warm.
- **Windows is a compile gate.** The host crate builds and its test targets
  compile on Windows; the behavioural suite runs on Linux because the file lock
  is mandatory there.

## Documentation

| document | contents |
|---|---|
| [`FORMAT.md`](FORMAT.md) | the normative byte layout, cited by section throughout the code |
| [`DESIGN.md`](DESIGN.md) | the design rationale, the commit and crash argument, the threat model, related work, and the boundary against the CrucibleOS research chapter |
| [`EMBEDDING.md`](EMBEDDING.md) | the contract an embedding application follows: one writer, the commit as the durability point, replacement rather than overwriting, and the identity convention |
| [`TERMINOLOGY.md`](TERMINOLOGY.md) | the vocabulary used across the code and documents |
| [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) | behavioural limits, known defects and their causes |
| [`BENCHMARKS.md`](BENCHMARKS.md) | the measurement method, results and comparability rules |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | build, test, lint and the feature matrix |
| [`RELEASING.md`](RELEASING.md) | the release gate, including the crates.io step that has not run |
| [`SECURITY.md`](SECURITY.md) | how to report a vulnerability |
| [`CHANGELOG.md`](CHANGELOG.md) | changes by release |
| [`kernel/README.md`](kernel/README.md) | the format crate |
| [`host/README.md`](host/README.md) | the host crate |

## Building from source

The released crates require Rust 1.89 and edition 2024. The benchmark harness
requires 1.90, because `redb` does.

```sh
git clone https://github.com/Sing-Security/ledgerfs
cd ledgerfs

cargo build --release
cargo test --workspace
cargo test -p ledgerfs --all-features
cargo fmt --all --check
cargo clippy --workspace --all-targets
```

Examples:

```sh
cargo run --example app_store -p ledgerfs-host
cargo run --example staging_capture -p ledgerfs-host
cargo run --example embed -p ledgerfs-host
cargo run --example disk_volume -p ledgerfs-host
cargo run --example custom_device -p ledgerfs-host
cargo run --example basic -p ledgerfs
```

CI builds every declared kernel feature, runs the host suite on Linux, compiles
the host test targets on Windows, runs the `pending/` suite and the 1.89 floor,
runs `rustfmt` and `clippy` with warnings denied, and fails if a declared
feature is missing from the matrix. The readme examples are pinned by
`host/tests/readme_examples.rs` and fail CI if they stop compiling.

## Contributing and security

Issues and pull requests are welcome. Read [`CONTRIBUTING.md`](CONTRIBUTING.md)
before a change to the format, and [`SECURITY.md`](SECURITY.md) before
disclosing a vulnerability.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

at your option. The [`NOTICE`](NOTICE) file carries the attribution. Unless you
state otherwise, any contribution you intentionally submit for inclusion is
dual-licensed as above, without additional terms.
