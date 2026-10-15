# ledgerfs

The `no_std` half of LedgerFS: the on-disk format and the copy-on-write
operations that produce it. This crate performs no I/O. It reads and writes
*through* an `ObjectStore` (kernel crate) or a `BlockDevice` (host crate), so the
same format code runs in a kernel, in a userspace process, and in a test harness
holding a `Vec<u8>`.

The normative description of the bytes is [`FORMAT.md`](../FORMAT.md), which this
crate cites by section number. If you are looking for how to *use* a volume
rather than what one looks like, start at the [`ledgerfs-host`
crate](https://github.com/Sing-Security/ledgerfs) instead.

## Modules

| | |
|---|---|
| `encoding` | canonical little-endian encoding, BLAKE3 hashing, strict decode rejection |
| `record` | `ObjectRecord` framing — magic, length, checksum, verification |
| `object` | the object model: `ObjectId`, `Chunk`, `Inode`, `DirNode`, `ExtentNode`, `Root` |
| `superblock` | the 4 KiB superblock, its four copies, the Root Locator Ring, `mount_select` |
| `intent_log` | the ring buffer of transaction events, and `determine_replay` |
| `alloc_node`, `alloc_state`, `allocator` | the allocation tree, block states, and the allocator with its emergency reserves |
| `placement` | turns a `TxnPlan` into placed `ObjectRecord`s |
| `commit` | `CommitEngine`: the ordered write of objects → allocator snapshot → superblocks |
| `cow` | the operations — `read`, `readdir`, `mkdir`, `create_file`, `write`, `unlink`, `rename` — and the `ObjectStore` trait they are generic over |
| `dedupe` | the in-memory `ObjectId → physical location` index, refcounts and pins |
| `gc` | deadlist processing and extent reconciliation |
| `snapshot` | snapshot registry, pinning, rollback |
| `cdc`, `compression`, `lz4_block` | content-defined chunking and the per-chunk compression policies |
| `replication` | send/receive: walking a reachable set, diffing two roots, verifying what arrives |
| `scrubber`, `selftest` | integrity verification, in the background and at boot |
| `audit`, `seal_record` | the tamper-evident audit chain and its seals |
| `keyslot`, `data_crypto` | passphrase-derived key slots and transparent block encryption |
| `mode` | `FsMode` — `ReadWrite` versus the write-embargoed `ReadOnlySafe` |
| `reader`, `writer`, `ata_io`, `arch` | the ATA PIO path used when this crate is the only route to its own partition |

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
[`CONTRIBUTING.md`](../CONTRIBUTING.md).

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
[`FORMAT.md`](../FORMAT.md).

## License

Apache-2.0. See [`LICENSE`](../LICENSE) and [`NOTICE`](../NOTICE).
