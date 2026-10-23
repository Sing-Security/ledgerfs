# ledgerfs

The `no_std` half of LedgerFS: the on-disk format and the copy-on-write
operations that produce it. This crate performs no I/O of its own. It reads and
writes *through* an `ObjectStore` trait, which
[`ledgerfs-host`](https://crates.io/crates/ledgerfs-host) implements over a
block device and tests implement over a `Vec<u8>`, so the same format code runs
in a kernel, in a userspace process, and in a test harness.

**You probably want [`ledgerfs-host`](https://crates.io/crates/ledgerfs-host)**
— that is the crate an application links to open a volume and write files to.
This one is for a consumer that already has a byte-addressed store of its own,
or that is implementing the filesystem from the bottom: a boot loader, an OS, a
tool that inspects a volume.

The normative description of the bytes is
[`FORMAT.md`](https://github.com/Sing-Security/ledgerfs/blob/main/FORMAT.md),
which this crate cites by section number. The project's vocabulary is
[`TERMINOLOGY.md`](https://github.com/Sing-Security/ledgerfs/blob/main/TERMINOLOGY.md).
If the two disagree with the code, the code is right and the document is a bug.

## How the format works

- **Everything is an object.** An immutable blob named by the BLAKE3 hash of its
  canonical encoding: `Chunk` for content, `Inode` for file metadata, `DirNode`
  for a directory, `ExtentNode` for a file's extent list, `Root` for the tree.
  A reader verifies an object against its name before returning it.
- **Nothing is modified in place.** An operation writes new generations of
  every object on the path and returns one `TxnPlan` naming them plus the new
  root.
- **One ordered write makes a transaction durable.** `CommitEngine` places the
  objects and the allocator snapshot, issues a barrier, writes the four
  superblock copies naming the new root, and issues a second barrier. That
  order is the crash-consistency argument (`FORMAT.md` §15): a crash leaves the
  old root live or the new one, never a blend.
- **The superblock names the root.** `mount_select` picks the highest verifying
  `commit_seq` among the four copies and uses the Root Locator Ring when the
  newest copy is damaged.

## Modules

| | |
|---|---|
| `encoding` | canonical little-endian encoding, BLAKE3 hashing, strict decode rejection |
| `record` | `ObjectRecord` framing — magic, length, checksum, verification |
| `object` | the object model: `ObjectId`, `Chunk`, `Inode`, `DirNode`, `ExtentNode`, `Root` |
| `superblock` | the 4 KiB superblock, its four copies, the Root Locator Ring, `mount_select` |
| `intent_log` | the ring buffer of commit-intent records, and `determine_replay` |
| `alloc_node`, `alloc_state`, `allocator` | the allocation tree, block states, and the allocator with its emergency reserves |
| `placement` | turns a `TxnPlan` into placed `ObjectRecord`s |
| `commit` | `CommitEngine`: the ordered write of objects → allocator snapshot → superblocks |
| `cow` | the operations — `read`, `readdir`, `mkdir`, `create_file`, `write`, `unlink`, `rename` — and the `ObjectStore` trait they are generic over |
| `compression`, `lz4_block` | the per-chunk compression policies |
| `replication` | send/receive: walking a reachable set, diffing two roots, verifying what arrives |
| `selftest` | integrity verification at boot |
| `audit`, `seal_record` | the tamper-evident audit chain and its seals |
| `keyslot`, `data_crypto` | passphrase-derived key slots and transparent block encryption |
| `mode` | `FsMode` — `ReadWrite` versus the write-embargoed `ReadOnlySafe` |
| `reader`, `writer`, `ata_io`, `arch` | the ATA PIO path used when this crate is the only route to its own partition |

`dedupe`, `gc`, `snapshot`, `cdc` and `scrubber` are deliberately absent.
`FORMAT.md` §44 calls them Tier B — implemented and tested, but driven by nothing
in the shipping path — so they build and test as the separate, unpublished
`ledgerfs-pending` crate instead of shipping in this one.

## Features

| | |
|---|---|
| *(none)* | the format and the CoW operations, `no_std` + `alloc` |
| `std` | pulls the `lz4` and `zstd` decoders that `compression` gates, so a record written under either policy decodes |
| `reader` | compiles the ATA PIO path and its four port-I/O instructions. Ring-0 code, x86-64 only. Off by default. |
| `data-encryption` | transparent XTS-AES-256 over a volume's raw blocks, applied below the object model, chosen at format time |
| `temper_sealing` | ML-DSA-65 and SLH-DSA sealing of audit records, via the [`temper-rs`](https://crates.io/crates/temper-rs) crate |

```sh
cargo test -p ledgerfs --all-features
```

The full matrix, including the combinations that catch people, is in
[`CONTRIBUTING.md`](https://github.com/Sing-Security/ledgerfs/blob/main/CONTRIBUTING.md).

## Example

The kernel crate has no device of its own, so an example has to supply one. The
shortest one that runs is `examples/basic.rs`, over the `commit` and `cow`
modules directly. For a volume you can actually write files to, use the host
crate:

```sh
cargo run --example app_store -p ledgerfs-host
```

## Notes on the format

Two constraints are worth knowing before reading the code, because they look
arbitrary without the reasoning:

- **`RESERVED_HEADER_BLOCKS` and `RESERVED_TRAILER_BLOCKS` are load-bearing.** No
  object may be placed in those blocks. The allocator's initialization and the
  boot-time object scan both have to agree on this, or a fresh volume's first
  commit can place its root inode on top of the superblock the next write
  overwrites, orphaning it permanently.
- **The allocator snapshot lives at a fixed block (`ALLOC_SNAPSHOT_BLOCK`), not
  in an ordinary object.** The allocator's state cannot be stored through the
  allocator: doing so consumes a block, which changes the state being written.

Both are described where they are defined, in `src/lib.rs`, and in
[`FORMAT.md`](https://github.com/Sing-Security/ledgerfs/blob/main/FORMAT.md).

## License

Dual-licensed: Apache-2.0 or MIT, at your option. See
[`LICENSE-APACHE`](LICENSE-APACHE), [`LICENSE-MIT`](LICENSE-MIT), and
[`NOTICE`](NOTICE) in this directory.
