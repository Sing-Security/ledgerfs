# ledgerfs

**Sing Security Research Laboratory**

The `no_std` implementation of the LedgerFS on-disk format and of the
copy-on-write operations that produce it.

## Abstract

This crate defines every byte a LedgerFS volume contains: the superblock and its
four copies, the Root Locator Ring, object framing, the object model, the block
allocator, the commit engine and the copy-on-write operations. It performs no
I/O. Reads and writes are expressed through an `ObjectStore` trait, which the
companion crate [`ledgerfs-host`](https://github.com/Sing-Security/ledgerfs/tree/main/host)
implements over a block device and which a kernel can implement over the disk it
owns.

The normative description of the byte layout is
[`FORMAT.md`](https://github.com/Sing-Security/ledgerfs/blob/main/FORMAT.md),
which this crate cites by section number. The project vocabulary is
[`TERMINOLOGY.md`](https://github.com/Sing-Security/ledgerfs/blob/main/TERMINOLOGY.md).
Where this document and the format document disagree, the implementation is
correct and the document contains a defect.

An application that opens a volume and writes files to it should depend on
`ledgerfs-host` instead. This crate is addressed to a consumer that already owns
a byte-addressed store, or that is constructing the filesystem from the bottom:
a boot loader, an operating system, or an inspection tool.

## 1. The format

- **Objects.** An object is an immutable blob addressed by the BLAKE3 hash of
  its canonical encoding. The kinds are `Chunk` for content, `Inode` for file
  metadata, `DirNode` for a directory, `ExtentNode` for a file's extent list,
  and `Root` for the tree. A reader recomputes the identifier and verifies the
  record checksum before accepting an object.
- **No modification in place.** An operation writes new generations of every
  object on the path from the root and returns one `TxnPlan` naming those
  objects and the new root.
- **Ordered commit.** `CommitEngine` places the objects and the allocator
  snapshot, issues a barrier, writes the four superblock copies naming the new
  root, and issues a second barrier. That order is the crash-consistency
  argument (`FORMAT.md` §15): a crash leaves the previous root live or the next
  one, never an intermediate state.
- **Root selection.** `mount_select` selects the highest verifying `commit_seq`
  among the four superblock copies and consults the Root Locator Ring when the
  newest copy is damaged.

## 2. Modules

| module | contents |
|---|---|
| `encoding` | canonical little-endian encoding, BLAKE3 hashing, strict decode rejection |
| `record` | `ObjectRecord` framing: magic, length, checksum, verification |
| `object` | the object model: `ObjectId`, `Chunk`, `Inode`, `DirNode`, `ExtentNode`, `Root` |
| `superblock` | the 4 KiB superblock, its four copies, the Root Locator Ring, `mount_select` |
| `intent_log` | the ring buffer of commit-intent records and `determine_replay` |
| `alloc_node`, `alloc_state`, `allocator` | the allocation tree, block states, and the allocator with its emergency reserves |
| `placement` | turns a `TxnPlan` into placed `ObjectRecord`s |
| `commit` | `CommitEngine`: the ordered write of objects, allocator snapshot and superblocks |
| `cow` | the operations (`read`, `readdir`, `mkdir`, `create_file`, `write`, `unlink`, `rename`) and the `ObjectStore` trait they are generic over |
| `compression`, `lz4_block` | the per-chunk compression policies |
| `keyslot`, `data_crypto` | passphrase-derived key slots and transparent block encryption |
| `mode` | `FsMode`: `ReadWrite` against the write-embargoed `ReadOnlySafe` |
| `selftest`, `reader`, `writer`, `ata_io`, `arch` | the ATA PIO path and its boot-time round-trip test, used when this crate is the only route to its own partition |

## 3. Tier B subsystems

`FORMAT.md` §44 designates a set of subsystems Tier B and instructs a reader to
treat them as not available. Deduplication at write time, snapshots, the
scrubber, garbage-collection execution and content-defined chunking are
implemented and tested in the unpublished `ledgerfs-pending` crate in the same
repository. `replication`, `audit` and `seal_record` are present in this crate
and are not called by any shipping path. `KNOWN_ISSUES.md` §11 lists the
subsystems and identifies the four that report success without performing the
work.

## 4. Feature sets

| feature | effect |
|---|---|
| *(none)* | the format and the copy-on-write operations, `no_std` + `alloc` |
| `std` | pulls the `lz4` and `zstd` decoders that `compression` gates, so a record written under either policy decodes |
| `reader` | compiles the ATA PIO path and its four port-I/O instructions. Ring-0 code, x86-64 only, off by default |
| `data-encryption` | transparent XTS-AES-256 over a volume's raw blocks, applied below the object model and selected when the volume is formatted |
| `temper_sealing` | ML-DSA-65 and SLH-DSA sealing of audit records, through the [`temper-rs`](https://crates.io/crates/temper-rs) crate |

```sh
cargo test -p ledgerfs --all-features
```

The full feature matrix, including the combinations that are easy to get wrong,
is in
[`CONTRIBUTING.md`](https://github.com/Sing-Security/ledgerfs/blob/main/CONTRIBUTING.md).

## 5. Example

`examples/basic.rs` exercises the parts that need no device: record framing,
superblock encoding and mount selection. An example that commits objects has to
supply an object store of its own. For a volume that accepts files, use the
host crate:

```sh
cargo run --example app_store -p ledgerfs-host
```

## 6. Constraints that are not evident from the code

Two constraints appear arbitrary without their reasons:

- **`RESERVED_HEADER_BLOCKS` and `RESERVED_TRAILER_BLOCKS` are load-bearing.**
  No object may be placed in those blocks. The allocator initialisation and the
  boot-time object scan must agree on this, or the first commit of a fresh
  volume can place an object on top of a superblock that a later write
  overwrites, orphaning the object permanently.
- **The allocator snapshot occupies a fixed block (`ALLOC_SNAPSHOT_BLOCK`),
  not an ordinary object.** The allocator's state cannot be stored through the
  allocator, since doing so consumes a block and changes the state being
  written.

Both are stated where they are defined, in `src/lib.rs`, and in
[`FORMAT.md`](https://github.com/Sing-Security/ledgerfs/blob/main/FORMAT.md).

## License

Dual-licensed: Apache-2.0 or MIT, at your option. The license texts ship inside
the crate package as `LICENSE-APACHE`, `LICENSE-MIT` and `NOTICE`, and are
also at
[LICENSE-APACHE](https://github.com/Sing-Security/ledgerfs/blob/main/kernel/LICENSE-APACHE),
[LICENSE-MIT](https://github.com/Sing-Security/ledgerfs/blob/main/kernel/LICENSE-MIT)
and
[NOTICE](https://github.com/Sing-Security/ledgerfs/blob/main/kernel/NOTICE).
