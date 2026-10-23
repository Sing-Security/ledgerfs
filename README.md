# LedgerFS

**An embedded filesystem you can use as your application's storage layer.**

LedgerFS is a library that owns a volume — a flat array of 4 KiB blocks — and
gives your process a small set of file verbs over it. Every write is
copy-on-write and content-addressed, and a write becomes durable at exactly one
point: the superblock write that ends a **commit**. Before it, the old tree is
the filesystem; after it, the new one. There is no interval a crash can leave
half-applied, and nothing to replay on mount.

You can use it as your application's database. It holds your records and gives
them back, and for a great many applications that is the whole job — logs,
samples, telemetry batches, application state, snapshots. What it is not is a
database **engine**: no schema, no query language, no index, no in-place update.
It is a different solution to the same problem, and the difference is what you
get for it — see [what a filesystem buys](#what-a-filesystem-buys-you) and
[what it costs](#what-it-costs).

```sh
cargo add ledgerfs-host
```

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

Swap `MemBlockDevice` for `FileBlockDevice` and the same calls drive a bare file
or a disk partition. Runnable versions, one per way to hold a volume, are listed
under [Examples](#examples).

## What this is

- **An embedded filesystem.** The crate is the filesystem. An application links
  it, hands it a `BlockDevice`, and calls verbs. Nothing mounts into a path
  namespace, there is no VFS, no page cache, no daemon and no root privilege —
  it runs in your process, on your buffer, on your partition.
- **A commit is the point of truth.** A mutation is one transaction: place the
  objects and write the allocator snapshot, one device barrier, then the
  superblock copies, then the barrier that commits them. The order is
  load-bearing and the format's crash argument is made of it (`FORMAT.md` §15).
- **Content-addressed and verified.** Every object is named `BLAKE3(payload)`,
  and every record carries a magic, a length and a truncated-BLAKE3 checksum.
  What comes back out of the device is verified against all of them, so a read
  either returns exactly the bytes that were written or an error. Nothing is
  silently corrupt.
- **A volume is bytes.** One file, one partition, or one `Vec<u8>` in your heap
  — the block device is the only difference. A volume can be snapshotted to
  bytes and reopened anywhere.
- **Built for a stream.** A staging session gathers appends in memory and seals
  them as one ordinary commit, so a capture loop pays one durability point per
  seal, not per record. The seal is still the point of truth; you choose how
  often it moves.

## What a filesystem buys you

LedgerFS is a different solution to the problem a database solves, and the
difference is the shape of the thing you end up operating. Using a whole
filesystem as your storage layer buys:

| | |
|---|---|
| **Your whole store is one file.** | A volume is a flat array of blocks, so it copies, archives, ships and diffs as one artifact. There is no dump and restore, no WAL directory, no sidecar config — the storage *is* the file, and `Store::open` is the import. |
| **Nothing to run.** | No server, no daemon, no port, no socket, no service to monitor or upgrade. The storage layer is linked into your binary and dies with your process. |
| **Paths, not tables.** | You name your data and the filesystem does the rest. Any tool that reads a volume can read it: `lfs-read <IMAGE> ls /` needs no library and no schema. |
| **The format outlives the library.** | The bytes are specified in [`FORMAT.md`](FORMAT.md), normatively and by section. If this crate stopped being maintained, the format would not stop being readable. |
| **Integrity is the format's job, not yours.** | Every object is named by the hash of its content and every record is checksummed, so a read returns the bytes that were written or an error. You do not write a verifier. |
| **A write is atomic by construction.** | One transaction per verb, one commit as the durability point, the old root or the new one after a crash. You do not write a recovery path. |
| **Your data is bytes.** | No schema, no type coercion, no migration. What you wrote is what you read back — `f32` payloads bit for bit, including the ones a database would canonicalize. |
| **The same code everywhere.** | One format, one crate: a process, a boot loader, a device with no OS. A volume written under one is read under the other. |

## What it costs

Speed, on some paths, and it is worth saying which:

- **Small durable writes.** A 256-byte insert costs 9.18 ms against SQLite's
  3.79 ms — 2.42×, and 1.29× a raw file doing its own two `fsync`s. Most of the
  gap is the second barrier, and the two-barrier order is the format's
  crash-consistency argument ([`FORMAT.md`](FORMAT.md) §15), not a tuning miss.
- **Reads.** A verified point read of 256 bytes is 25.5 µs against `redb`'s
  949 ns. The gap is the BLAKE3 check. A database trusts its pages; this proves
  them.
- **What pays it back.** A **staging session** moves the commit off the
  per-record path: appends land in memory and one seal commits them all, so a
  capture loop pays one durability point per seal rather than per record, and
  the commit is amortized across the whole stream. Those rows are marked
  non-comparable in [`BENCHMARKS.md`](BENCHMARKS.md) for the honest reason —
  only the seal carries the durability point — and the read back is still
  verified.

If raw throughput is the only thing you are choosing on, a database engine wins
those rows and you should use one. If you would rather have a storage layer that
is a single file, that nothing has to run, and that proves what it returns, this
is what that costs. [`BENCHMARKS.md`](BENCHMARKS.md) states the comparison in
full, including the durability setting and the non-comparable rows for every
engine.

## Where it does not fit

| | |
|---|---|
| **Not a query engine.** | No schema, no indexes, no joins, no SQL, no filter or aggregation across records. Reading a record means knowing its path. |
| **Not a POSIX filesystem.** | No mount point, no `mmap`, no POSIX conformance claim, no root filesystem. It is a library over a block device, not a filesystem your OS mounts. |
| **Not free of caveats.** | [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) is the honest list: free space falls monotonically (nothing collects yet), `write_at` cannot rewrite stored bytes, a directory is capped at 400 entries. Read it before adopting this. |

None of the three is on the path of the data this was built for. Where they do
bite, a database engine is the better tool:

| Choose LedgerFS when | Choose a database engine when |
|---|---|
| Records land once and are read back whole or by byte range | Records are queried, filtered, joined or updated in place |
| One writer owns the volume | Several processes write concurrently |
| You want the storage to be a byte string you can ship, snapshot or embed | You want SQL, indexes and a server to operate |
| A crash must leave the old state or the new one, never a blend | You need multi-statement transactions across many records and tables |
| You want reads that prove they are correct | Approximate or best-effort reads are acceptable |
| You are on a device that must work offline, with no daemon | You have an operations team and a database already |

## How it works

1. **Objects.** Everything on a volume is an immutable blob named by its BLAKE3
   hash: `Chunk` for content, `Inode` for a file's metadata, `DirNode` for a
   directory, `ExtentNode` for a file's extent list, `Root` for the tree.
2. **Copy-on-write.** A mutation never edits an object. It writes new
   generations of everything on the path and re-points the parents at them.
3. **One transaction per verb.** A write of any length — split into as many
   256 KiB objects as it needs — is folded into a single `TxnPlan`: chunks,
   extent node, inode, directory entry, ancestors.
4. **One commit per transaction.** Placement assigns blocks, the objects and
   the allocator snapshot are written and flushed, the superblock copies that
   name the new root are written and flushed. Crash yields the old root or the
   new one.
5. **Staging, when one commit per write is too many.** `begin_staging` opens a
   session; `staged_append` extends in-memory drafts and touches no device;
   `seal_staged` folds everything into one commit. `seal_staged_deferred` +
   `sync` moves the second barrier off the seal when a group can share it.
6. **Two read tiers, chosen per read.** Inside a session, `read_file`,
   `read_at`, `stat` and `read_dir` answer from the staged view at memory
   speed; `read_file_committed` answers from the superblock's generation. Any
   other handle sees only committed state, by construction.

The normative bytes are in [`FORMAT.md`](FORMAT.md); the application contract —
one writer, replace rather than overwrite, the `/.ledgerfs` identity convention,
which verb means what — is [`EMBEDDING.md`](EMBEDDING.md). The words are fixed
in [`TERMINOLOGY.md`](TERMINOLOGY.md), so *seal*, *commit* and *draft* mean the
same thing in all three.

## Crates

| crate | | owns |
|---|---|---|
| [`ledgerfs`](https://crates.io/crates/ledgerfs) | `no_std` + `alloc` | the on-disk format — superblock, `ObjectRecord` framing, object model, allocator, commit engine — and the copy-on-write operations, generic over an `ObjectStore`. No I/O of its own. |
| [`ledgerfs-host`](https://crates.io/crates/ledgerfs-host) | `std` | one `ObjectStore` over a `BlockDevice`, mount and path plumbing, `Store` (`mkfs`, open, and the verbs), and `lfs-read`/`lfs-write`. This is what an application links. |

The kernel crate is the only place an on-disk offset or checksum is defined;
the host crate re-derives neither. The kernel also carries an ATA PIO reader and
writer behind the `reader` feature for the ring-0 case where the crate is the
only route to its own partition; a userspace consumer takes the crate without
it.

Two workspace directories are **not** published. `benchmarks/` is the
comparison harness behind `BENCHMARKS.md`. `pending/` holds subsystems that are
implemented and tested but that nothing in the shipping path calls — the dedupe
index, snapshots, the scrubber, GC safety rules and content-defined chunking.
`FORMAT.md` §44 calls those Tier B and tells a reader to treat them as "not
available", so they build and test here and are shipped to nobody.

## Examples

- [`examples/embed`](host/examples/embed.rs) — in-memory, relocatable volumes
  (`MemBlockDevice`)
- [`examples/disk_volume`](host/examples/disk_volume.rs) — a hosted volume: a
  real file, the one-writer lock, a read-only open beside a live writer
- [`examples/app_store`](host/examples/app_store.rs) — the application-store
  contract from [`EMBEDDING.md`](EMBEDDING.md) as code
- [`examples/staging_capture`](host/examples/staging_capture.rs) — the staging
  tier: stage, read both tiers mid-session, seal, reopen from the volume's bytes
- [`examples/custom_device`](host/examples/custom_device.rs) — your own storage:
  implement the four required `BlockDevice` methods and the whole filesystem
  works on top of it unchanged

```sh
cargo run --example app_store -p ledgerfs-host
```

## Guarantees

- **Atomic commit.** A mutation is one transaction, and the commit sequence is
  the format's crash-consistency argument: old tree or new tree, never a blend.
- **Durability is explicit.** Nothing is "eventually" durable. A commit is
  durable when it returns; a seal is durable when it returns; a deferred seal
  is durable at `sync()`. Before any of those, the data is in the process's
  memory and a crash loses exactly that.
- **Integrity on read.** Every object's id is its content hash and every record
  is checksummed; a read verifies before it answers.
- **Isolation.** One writer per volume, enforced by an advisory exclusive lock
  on a file device and by ownership on any other. A volume is a closed tree
  with no cross-volume state.

## Documents

| | |
|---|---|
| [`TERMINOLOGY.md`](TERMINOLOGY.md) | the vocabulary, normative — read first if a word seems to mean two things |
| [`FORMAT.md`](FORMAT.md) | the on-disk format, normative, cited by section from the source |
| [`EMBEDDING.md`](EMBEDDING.md) | the contract for an application embedding a volume |
| [`BENCHMARKS.md`](BENCHMARKS.md) | what was measured, on what, and against what |
| [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) | limitations, deliberate and otherwise |
| [`SECURITY.md`](SECURITY.md) | what a useful vulnerability report looks like |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | the feature matrix and the format-change rules |
| [`RELEASING.md`](RELEASING.md) | publish order, the gates, and what a version number means |
| [`CHANGELOG.md`](CHANGELOG.md) | release history |

## Building

```sh
cargo test -p ledgerfs --all-features
cargo test -p ledgerfs-host
```

The kernel is `no_std` and has a feature matrix; see
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the full list. Both published crates
build on stable Rust 1.89, edition 2024. The benchmark harness is not published
and needs 1.90, because `redb` does.

[`rust-toolchain.toml`](rust-toolchain.toml) pins development to current stable
with rustfmt and clippy, and says why it is not pinned to the floor.

## License

Dual-licensed: Apache-2.0 or MIT, at your option. See
[`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT), and
[`NOTICE`](NOTICE).
