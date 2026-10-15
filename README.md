# LedgerFS

A copy-on-write, content-addressed filesystem that an ordinary application can
embed. A **volume** is a flat array of 4 KiB blocks holding a self-contained tree
of files. Every object is addressed by `BLAKE3(payload)` and written
copy-on-write, nothing is ever modified in place, and a write becomes durable at
one point: the superblock write that ends a commit. Before that write the old
tree is the filesystem; after it, the new one. There is no interval a crash can
leave half-applied.

The intended use is not a system disk. It is a store a program owns: format a
bare file or a heap buffer as a volume, write to it through a small verb set, and
carry the whole filesystem around as a byte string.

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

	// A volume is a byte string. Ship it, archive it, or reopen it here.
	let bytes = mirror.bytes_snapshot();
	let reopened = Store::open(Box::new(MemBlockDevice::from_bytes(bytes)?))?;
	assert_eq!(reopened.read_file("/log.jsonl")?, b"{\"turn\":1}\n");
	Ok(())
}
```

Swap `MemBlockDevice` for `FileBlockDevice` and the same calls drive a file or a
partition. Runnable versions: `cargo run --example app_store` (the contract in
[`EMBEDDING.md`](EMBEDDING.md) as code) and `cargo run --example embed`
(in-memory, relocatable volumes).

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

- **Not a general-purpose system filesystem.** There is no VFS layer, no page
  cache, no `mmap`, no POSIX conformance claim. It is a library over a block
  device.
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
  durability here is a whole-filesystem commit, which is the wrong unit for a
  batched analytical load and would lose badly at one. Where a number was not
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

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
