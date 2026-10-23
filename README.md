# LedgerFS

A copy-on-write, content-addressed filesystem. A **volume** is a flat array of
4 KiB blocks holding a self-contained tree of files — a partition on a real disk,
a file, or a buffer in memory. Every object is addressed by `BLAKE3(payload)` and
written copy-on-write, nothing is ever modified in place, and a write becomes
durable at one point: the superblock write that ends a commit. Before that write
the old tree is the filesystem; after it, the new one. There is no interval a
crash can leave half-applied.

A caller with more mutations than commits can afford does not have to loosen
that: a **staging session** gathers writes in memory and seals them as one
ordinary commit, and the caller chooses, per read, whether it wants the
committed tier or its own staged one. The seal is still the point of truth —
before it, the staged work is RAM only; after it, it has exactly the guarantee
every other write has.

It is built for two deployments, and neither is a special case of the other. A
volume can be **provisioned onto a real disk** — its own partition, formatted
once and opened directly by whatever owns it — or **embedded in a program**,
where it lives in a file or in that process's heap and can be carried around as a
byte string. The block device underneath is the only difference:
`FileBlockDevice` binds a bare file and a partition with the same type, and
everything above it is identical.

## Two crates

| crate | | owns |
|---|---|---|
| `ledgerfs` | `no_std` + `alloc` | the on-disk format — superblock, `ObjectRecord` framing, object model — and the copy-on-write operations, generic over an `ObjectStore`. No I/O of its own. |
| `ledgerfs-host` | `std` | one `ObjectStore` over a `BlockDevice`, the path and mount plumbing, and `Store`: `mkfs`, mount, and the mutation verbs. This is what an application links. |

The kernel crate is the only place any on-disk offset or checksum is defined.
The host crate re-derives neither. Where the kernel is compiled into a kernel,
it also carries an ATA PIO reader and writer (`reader` feature); a userspace
consumer takes the crate without it.

Two more directories are in the workspace and are not published. `benchmarks/`
is the comparison harness behind `BENCHMARKS.md`. `pending/` holds the
subsystems that are implemented and tested but that nothing in the shipping path
calls — the dedupe index, snapshots, the scrubber, GC safety rules and
content-defined chunking. `FORMAT.md` §44 calls those Tier B and tells a reader
to treat them as "not available", so they build and test here and are shipped to
nobody.

## Example

```rust
use ledgerfs_host::{MemBlockDevice, Result, Store};

fn main() -> Result<()> {
	// 8 MiB of volume living in this process's heap. The clone is how we get
	// the bytes back after `Store` takes ownership of the device.
	let device = MemBlockDevice::new(2048);
	let mirror = device.clone();

	// mkfs, and stamp the volume with the name of the app that owns it.
	let mut store = Store::format_app(Box::new(device), "example")?;

	// Create-and-write. Refuses if the path is taken.
	store.write_new_file("/log.jsonl", b"{\"turn\":1}\n", 0o644)?;

	// Atomic replace: build the new content under a scratch name, then rename
	// it over the target. One transaction, so no reader sees a half-written
	// state file.
	store.write_new_file("/state.new", b"{\"phase\":\"stopped\"}\n", 0o644)?;
	store.rename("/state.new", "/state")?;

	// High-frequency traffic: many writes, one commit — and a per-read choice
	// of how true the answer has to be.
	store.begin_staging(4 * 1024 * 1024)?;
	store.staged_append("/log.jsonl", b"{\"turn\":2}\n", 0o644)?;
	assert_eq!(store.read_file_committed("/log.jsonl")?, b"{\"turn\":1}\n");
	assert_eq!(store.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	let _seal = store.seal_staged()?; // one transaction, one durability point

	// A volume is a byte string. Ship it, archive it, or reopen it here.
	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n{\"turn\":2}\n");
	Ok(())
}
```

Swap `MemBlockDevice` for `FileBlockDevice` and the same calls drive a file or a
partition. Runnable versions, one per way to hold a volume:

- `cargo run --example embed` — in-memory, relocatable volumes (`MemBlockDevice`)
- `cargo run --example disk_volume` — a hosted volume: a real file, the
  one-writer lock, a read-only open beside a live writer, the hint sidecar
- `cargo run --example app_store` — the application-store contract from
  [`EMBEDDING.md`](EMBEDDING.md) as code
- `cargo run --example staging_capture` — the staging tier: stage, read both
  tiers mid-session, seal, and reopen from the volume's bytes
- `cargo run --example custom_device` — your own storage: implement the four
  required [`BlockDevice`](host/src/device.rs) methods and the whole filesystem
  works on top of it unchanged

The crate is portable: the file device's positioned I/O and writer lock are
gated per family and the crate is tested on Linux and Windows in CI (macOS
builds the same Unix path); the kernel crate is `no_std` and touches no OS at
all.

## What it guarantees

- **Atomic commit.** A mutation is one transaction: place the objects and write
  the allocator snapshot, one device barrier, then the superblock copies, then
  the barrier that commits them. The order is load-bearing and not
  interchangeable.
- **Crash consistency.** The superblock is the last write, and it names the root
  by content. A crash yields the old tree or the new one, never a blend. The
  snapshot-before-superblock order exists specifically so a published root always
  has the allocator state to go with it — a root without it mounts read-only and
  cannot write the snapshot that would fix it.
- **Integrity.** Every record carries a magic, a length, a truncated-BLAKE3
  checksum over the stored bytes, and an `object_id` that is `BLAKE3(payload)`.
  What comes back out of the device is verified against all of them.
- **Isolation.** One writer per volume, enforced by an advisory lock on a file
  device and by ownership on any other. A volume is a closed tree with no
  cross-volume state.

## What this is not

- **Not an operating system's root filesystem.** There is no VFS layer, no page
  cache, no `mmap`, and no POSIX conformance claim; nothing mounts a volume on
  `/`. It is a library over a block device — and the block device is expected to
  be a real one. A dedicated partition is the intended place for a volume to
  live, which is why the crate carries an ATA PIO reader and writer for the case
  where the crate is the only route to its own partition.
- **Not a database.** No query engine, no transactions across volumes, no
  concurrent writers.
- **Not battle-tested at scale.** The format is pre-1.0 and not frozen.
- **Not free of caveats.** [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) is the honest
  list, including the deliberate limitations: free space falls monotonically
  because nothing collects yet, `write_at` cannot rewrite inside an existing
  file, and a directory holds a bounded number of entries. Read it before
  adopting this.
- **Not yet claimed to be fast.** [`BENCHMARKS.md`](BENCHMARKS.md) reports
  LedgerFS against SQLite with `synchronous=FULL`, `redb`, and a raw `fsync`'d
  file, with the durability setting stated for every engine. The unit of
  durability here is a whole-filesystem commit, and a commit that is paid per
  small record is the wrong unit for a capture-shaped load. That is what the
  staging tier is for: the same traffic pays one commit per seal instead of one
  per record, and the benchmark has a row for it. Where a number was not
  measured, the table says so rather than leaving a blank.

## Documents

| | |
|---|---|
| [`FORMAT.md`](FORMAT.md) | the on-disk format, normative, cited by section from the source |
| [`EMBEDDING.md`](EMBEDDING.md) | the contract for an application embedding a volume |
| [`BENCHMARKS.md`](BENCHMARKS.md) | what was measured, on what, and against what |
| [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) | limitations, deliberate and otherwise |
| [`SECURITY.md`](SECURITY.md) | what a useful vulnerability report looks like |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | the feature matrix and the format-change rules |
| [`CHANGELOG.md`](CHANGELOG.md) | release history |

## Building

```sh
cargo test -p ledgerfs --all-features
cargo test -p ledgerfs-host
```

The kernel is `no_std` and has a feature matrix; see
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the full list. Both published crates
build on stable Rust 1.89. The benchmark harness is not published and needs 1.90,
because `redb` does.

[`rust-toolchain.toml`](rust-toolchain.toml) pins development to current stable
with rustfmt and clippy, and says why it is not pinned to the floor.

## License

Dual-licensed: Apache-2.0 or MIT, at your option. See
[`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT), and
[`NOTICE`](NOTICE).
