# Changelog

Notable changes to LedgerFS. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Two crates are published from this repository and they version independently:
`ledgerfs` carries the on-disk format and its copy-on-write
operations, `ledgerfs-host` carries the `std` volume layer. The on-disk
format has its own version field in the superblock, which is a third number
again — see [`FORMAT.md`](FORMAT.md).

## [0.7.0] / [0.1.0] - 2026-10-14

First public release of both crates. Everything below the two headings existed
before this in some internal state; treat it as lineage rather than as release
notes for something you could have depended on.

### Added

- **The on-disk format**, specified in [`FORMAT.md`](FORMAT.md): content-addressed
  objects with BLAKE3 identity, four superblock copies with a Root Locator Ring,
  a ring-buffer intent log, a persistent allocator snapshot, and a commit
  protocol whose write order is the crash-consistency argument.
- **`ledgerfs`** — the `no_std` half: canonical encoding with
  strict decode rejection, `ObjectRecord` framing and verification, the object
  model, the block allocator with emergency reserves, copy-on-write transactions,
  and the ATA reader and writer used when the crate is the only path to its own
  partition.
- **`ledgerfs-host`** — the `std` half: `Store`, which formats, mounts
  and mutates a volume over any `BlockDevice`. `FileBlockDevice` puts a volume in
  a file or a partition and takes an advisory exclusive lock that refuses a
  second writer; `MemBlockDevice` keeps one in memory and can be snapshotted to
  bytes and rebuilt from them, which is how the durability tests simulate a
  power cut.
- **`benchmarks/`** — the comparison harness against SQLite in WAL mode, `redb`,
  and raw `File::write` + `sync_all`, with a guard that refuses to run on a
  filesystem whose barrier is not real.
- **`pending/`** — the subsystems `FORMAT.md` §44 calls Tier B: the dedupe index,
  snapshots, the scrubber, GC safety rules and content-defined chunking. They are
  implemented and tested, but nothing in the shipping path calls them, so they
  are built in this workspace and published to nobody.

### Features

- `reader` — compiles the ATA PIO read path and its port-I/O primitives. Off by
  default; it is ring-0 code and does not belong in a userspace binary.
- `std` — pulls the `lz4` and `zstd` decoders that `compression` gates, so a
  record written under either policy decodes.
- `data-encryption` — transparent XTS-AES-256 over a volume's raw blocks,
  applied at the block device rather than per chunk, and chosen at format time.
  Off by default, and opt-in per volume: a lost key makes the volume
  permanently unreadable, which is a user-facing tradeoff rather than a default.
- `temper_sealing` — ML-DSA-65 and SLH-DSA sealing of audit records, via the
  `temper-rs` crate on crates.io. Off by default; a plain filesystem build does
  not need a post-quantum signature stack.

### Changed - Breaking

- **The `data-encryption` key-derivation domain strings were renamed.** The two
  BLAKE3 domain separators — one stretching a passphrase into a key-encryption
  key, one checksumming the master key — now begin `LedgerFS.VolumeKeyslot.`
  rather than naming the project this crate was developed inside. A domain
  separator is a cryptographic separator, not a label: the same passphrase
  derives a different KEK under the new strings, so a volume encrypted by an
  earlier build does not unlock under this one. No volume has been released, and
  the two literals live only in this crate — nothing in a published volume or on
  a wire format carries them. A development volume from before this change is
  unreadable; reformat it.

### Changed

- **The x86 port-I/O primitives are vendored.** The four instructions the ATA
  path issues (`inb`, `outb`, `rep insw`, `rep outsw`) now live in
  `kernel/src/arch/x86_port.rs` instead of arriving as a dependency. A filesystem
  that owns an on-disk format should not take a dependency on its host OS's
  assembly crate to reach four instructions.
- **The crates build on stable.** Neither uses a nightly feature, so neither
  needs a nightly toolchain; the declared minimum is Rust 1.89 and edition 2024.
  The benchmark harness is a workspace binary rather than a published crate and
  needs 1.90 — see [`BENCHMARKS.md`](BENCHMARKS.md).

### Removed

- Internal work-log documents: per-phase implementation summaries, standalone
  test-status reports, and the shell scripts that re-ran one phase's checks.
  They described a project phase rather than a system, and several stated test
  counts that were true only on one feature set.

### Fixed before this release

Listed because each one is the class of defect the format's constraints exist to
prevent, and because the constraint in the code reads as arbitrary without the
failure that produced it:

- **The genesis commit placed an object on top of the superblock.** A fresh
  volume's first commit could allocate its root directory's `Inode` at block 0,
  which the subsequent superblock write silently overwrote, orphaning the object
  permanently. Fixed by reserving the header and trailer blocks and making both
  the object scan and the allocator's initialization respect the reservation.
- **A volume was writable exactly once per image.** Nothing on disk recorded
  which blocks were in use, and reconstructing that required walking the whole
  partition, so a volume with content could only be resumed read-only. Fixed by
  persisting the allocator snapshot to a pre-reserved block — it cannot be stored
  through the allocator, since writing it consumes a block and so changes the
  state being written.
- **The ATA path could deadlock the whole disk permanently.** `ATA_LOCK` is a
  bare spin mutex; a timer interrupt landing mid-command-sequence on a
  single-core system could hand the CPU to a task that then spun forever on a
  lock whose holder could not be rescheduled. Fixed by acquiring the bus with
  interrupts disabled on the current core and restoring the previous interrupt
  state on drop.
- **Volumes larger than 128 GiB had most of themselves unreachable.** The
  reader issued 28-bit LBA commands, which top out there. Fixed by adding the
  48-bit form and keeping the 28-bit one for requests that fit, so existing
  volumes and pre-48-bit drives are unaffected.
- **A write could succeed and then read back as not found.** The reader skips any
  record above `MAX_OBJECT_SIZE`; the write path did not know about that ceiling,
  so an object committed above it vanished. The commit engine now refuses to
  place one, and a volume may lower the limit in its superblock.
- **The sealed-audit code did not compile.** The `temper_sealing` path named a
  crate that no longer resolved, because no build in its development tree ever
  enabled the feature. It now uses the published `temper` crate and builds under
  CI.
