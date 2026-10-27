# Changelog

Notable changes to LedgerFS. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crates follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Two crates are published from this repository and they version independently:
`ledgerfs` carries the on-disk format and its copy-on-write
operations, `ledgerfs-host` carries the `std` volume layer. The on-disk
format has its own version field in the superblock, which is a third number
again; see [`FORMAT.md`](FORMAT.md).

## [Unreleased]

### Added

- **Encryption is available on the host path.** `ledgerfs-host` now drives the
  keyslots and the volume block cipher that the kernel crate has carried since
  the first release: `Store::format_encrypted` and `Store::open_encrypted`,
  `HostFs::mount_encrypted` for reading, `EncryptedDevice` (an XTS-AES-256
  `BlockDevice` inserted below the whole volume layer, so path resolution, the
  commit engine and staging run unchanged above it), and `VolumeKey` /
  `UnlockCredential`. A passphrase is stretched with the slot's recorded
  iterated-BLAKE3 parameters; a 32-byte key-encryption key, the kind an HSM
  supplies, is used directly. Both slot kinds may be populated at once, and the
  on-disk format is unchanged: a plaintext volume remains an ordinary volume.
- **The keyslot lifecycle.** `Store::add_slot`, `Store::revoke_slot` and
  `Store::change_passphrase` add, remove and rewrap credentials without
  re-encrypting a data block. Each rewrites all four superblock copies and
  restores the previous in-memory slot if that write fails, so a slot the disk
  did not receive cannot become durable later on an operation that reported an
  error. Unlocking searches every decoded copy, so a crash part-way through a
  change leaves a volume that opens with either the old or the new credential.
- `lfs-write` and `lfs-read` take `--passphrase` (one line from stdin, never
  argv) and `--key-file PATH` (exactly 32 bytes). `lfs-write mkfs --encrypt`
  formats an encrypted volume; `lfs-write compact` preserves the source's
  encryption, and `--encrypt` promotes a plaintext volume to an encrypted one.
  Both verify every path, size and byte before reporting success.
- **`SUPERBLOCK_VERSION` gates the mount (§43.1).** `Superblock::decode` refuses
  any other version with `Error::UnsupportedSuperblockVersion`: after the
  checksum, so a damaged block is still reported as damage, and before the
  feature flags, so a newer format is reported by the version it declared. A
  mixed set of copies falls back to the copies that decode. New volumes write
  the constant rather than a literal.
- **`ledgerfs::data_crypto::deactivate`** ends the ring-0 encryption registry's
  key lifetime for a partition at unmount. It is a no-op for a different
  partition, so unmounting one volume cannot disarm another.

### Changed

- **The intent log reports an in-flight commit; it never replays one.**
  `ReplayDecision::ReplayTo { commit_seq }` is now
  `ReplayDecision::InFlight { commit_seq }`, and `mount_select` keeps the root
  the selected superblock names. The format already said that no root is
  adopted from the log (§10.3, §18.5, §44.2); the old name promised otherwise
  and the code never did it. A transaction the ring also shows as committed or
  aborted is not in flight and is not reported.
- **`PrepareRootIntent` moved from `commit` to `intent_log`, with `encode` and
  `decode`.** `kernel::commit::PrepareRootIntent` is gone. One encoder now
  produces the payload for the commit engine, the example and every test, and
  readers decode the struct.
- **`block_is_encrypted` requires `total_blocks`.** The parameter was optional
  and `None` treated the trailing superblock copies as encrypted, which would
  decrypt plaintext into garbage. Every real caller has just read a superblock,
  which carries the number.
- **`SlotKind::Unknown(u8)` reads as occupied.** A slot kind written by a newer
  build is no longer decoded as `Empty`, because an overwrite on the next
  `add_slot` would destroy the only credential a newer build could use.
  `seal_slot` refuses anything that is not `Passphrase` or `Hsm`, so an unknown
  kind cannot be forged either.
- **`FORMAT.md` §36 documents what the implementation does.** The keyslot
  layout at byte 3772, the cipher's exact coverage (object region only;
  superblock, intent log and allocation snapshot stay plaintext), the
  key-management construction and domain separators, and the lifecycle, its
  crash behaviour and its one missing operation (master-key rekeying). Tier A
  in §44.1 now includes encryption.

### Fixed

- **The host test suite compiles on Windows, and its one Windows-only
  behaviour is documented.** `readpath.rs` imported
  `std::os::unix::fs::PermissionsExt` unconditionally, so the native Windows
  job stopped at compile time and had never run the suite. The restore is
  platform-split now. Compiling it surfaced that `File::try_lock` is mandatory
  on Windows and refuses a read through a second handle, which contradicts the
  advisory-lock promise the documents made. `KNOWN_ISSUES.md` §13 states the
  limitation, `EMBEDDING.md`, `README.md`, `host/README.md` and the device and
  error documentation qualify the promise, and the device test pins the refusal
  on Windows rather than failing on it. The Windows job compiles the suite
  instead of running tests that encode the Unix lock semantics.

## [0.7.0] / [0.1.0] - 2026-10-23

The first public release of both crates. `### Added` and `### Changed` list the
work in this release; `### Lineage` records the state the format and the crates
had reached before it, which is the greater part of what a consumer depends on.

### Added

- **Staging sessions** in `ledgerfs-host`: `Store::begin_staging`,
  `staged_append`, `staged_write_new_file`, `staged_remove`, `seal_staged`,
  `end_staging`, `staged_bytes`, `staging_is_open`, and the scoped
  `Store::staging(capacity)` wrapper. Staged writes thread a staged root in
  memory (no device block, no allocator state, no barrier) until
  `seal_staged` folds everything into **one** commit of the ordinary
  shape. The seal is pruned to what the final root reaches, so N staged appends
  carry N chunks plus one generation of each metadata object rather than one
  generation per append. No format change: a sealed session is indistinguishable
  from the equivalent sequence of commits.
- **Two read tiers.** While a session is open, the owning handle's `read_file`,
  `read_at`, `stat` and `read_dir` resolve through the staged root (the handle
  reads its own writes, with zero device reads for staged objects), and the new
  `Store::read_file_committed` returns the committed tier: what a crash at
  that instant would leave. Other handles see the committed tier only.
- `Error::StagingFull { wanted, capacity }`: a staged operation that would
  exceed the session's byte budget is refused, and the session is left exactly
  as it was.
- **`Store::seal_staged_deferred`, `Store::sync`, `Store::has_pending_sync`**:
  a seal variant that stops before the superblock write, so a program that
  seals in a loop pays **one barrier per group of seals** instead of two per
  seal. The objects and the allocator snapshot are still written and flushed
  by the seal itself, so no unbarriered root is ever published. A crash before
  `sync` leaves the previous root live: the group just sealed is lost whole,
  never blended, and the recovered volume may leak the group's space (blocks
  in the allocator snapshot that the live root does not name), never give a
  wrong answer. Any ordinary commit supersedes the pending root and clears it.
  This is a library-level durability tier: no format change, no feature bit,
  and the durable seal is still `seal_staged`.
- `examples/staging_capture.rs`: stage, read both tiers mid-session, seal, and
  reopen the volume from its bytes.

### Changed

- **A staged append costs the appended bytes, not the file's history.** Every
  staged append used to run the `write` builder: decode the file's `ExtentNode`,
  rebuild it, encode and hash all of it, and keep that generation until the
  seal: O(history) time per append and O(N²) session memory. A session now
  keeps a **draft** per appended file (the decoded inode and its extent list)
  and extends it in place; the objects are built when something needs their
  identity (the seal, a `stat`, a `read_dir`) and the seal builds each file's
  node once. Measured on a memory volume, 960-byte appends over 1,024 records
  went from 18 → 139 µs per append (growing with the file) to a flat ~6 µs, and
  the session held 20.7 MB for a ~1 MB payload where it now holds 1.05 MB.
  `staged_bytes` and the capacity budget count drafts too, and `stat`/`read_dir`
  materialize a draft before answering; contents reads do not. No format
  change and no change to what a seal writes: a test pins the sealed file's
  inode and extent-node ids to the ones the equivalent eager appends produce.
- **A write spanning several objects is one commit.** `write_new_file`,
  `append`, `write_at` and `write_file` over the volume's `max_object_bytes`
  split the payload into several objects but fold them into a single
  transaction; they used to commit once per object, so a failed larger write
  could leave a prefix behind. `KNOWN_ISSUES.md` §3 is retired.
- **`write_file` over an existing file is one atomic commit.** The new content
  is built whole under no directory entry, then the entry the path names is
  re-pointed at it in the same commit; there is no instant at which the path is
  absent or empty. The `remove` + `create` sequence still has that window, and
  `durability.rs` still pins it as the thing `write_file` avoids.
- `EMBEDDING.md` states the payload rule (every verb takes and returns bytes
  and the store never interprets them) with the `f32` sample recipe a database
  would be asked to hold as `REAL`s. `host/tests/typed_payloads.rs` pins that
  the encoding survives bit for bit (including `-0.0`, infinities, subnormals
  and NaN payloads) through a plain write, a staged/deferred capture session,
  and a remount.
- `STORE_CONTRACT_VERSION` is `"2"`. The bump names a real behavioural change
  for an embedding app (atomic replace, and one commit per verb however many
  objects it spans), not a format change.
- `FORMAT.md` §12.3 states that a staged session is library behaviour: no
  on-disk state, no feature bit, no recovery path. `EMBEDDING.md` gains the
  staging rule and the tier choice; `README.md` says what the tier is for.
- `kernel`: `cow::apply_ops_to_root` documents that it threads only the root
  descriptor, so an op cannot see an earlier op's objects, and that it does not
  perform the ancestor cascade. Documentation only.
- **`benchmarks/` measures the staging tier on all four engines.** The new
  `staged_append_960b` workload streams batches through each engine's own
  staging idiom and seals every `--staged-seal-every` batches (default 10);
  `insert_batched_256b` now runs on LedgerFS through a staging session, so that
  long-standing gap is gone. Separately, the `volume KiB/op` column is
  **corrected**: every figure recorded before 2026-10-23 was inflated by
  `Config::repeats`, because the free-space delta spans the whole run while the
  divisor was one repetition's operation count. `BENCHMARKS.md` gains *The
  staging tier* and re-rendered tables.
- **`benchmarks/` measures the deferred tier.** `deferred_append_960b` seals
  every `--staged-seal-every` batches and publishes the group every
  `--staged-sync-every` (default 100), each engine in its own idiom: LedgerFS
  `seal_staged_deferred` + `sync`, SQLite a commit under `synchronous=NORMAL`
  ended by `wal_checkpoint(FULL)`, redb a `Durability::None` commit ended by an
  empty `Durability::Immediate` one, the raw control a held file written
  without `sync_all`. The row is marked non-comparable, as the staged row is,
  because only the group carries the durability point; the note says what each
  engine's seal still does. `BENCHMARKS.md` gains *One step further: the
  deferred seal*.
- **An append encodes its extent node once, not three times.** `write` and
  `create_file_with_data` let `insert_extent` rehash the node, called
  `update_object_id` on it again, then encoded it a third time for the
  transaction. The extent node is the one object whose canonical encoding grows
  with a file's history, so that redundancy was O(history) per append.
  `ExtentNode::insert_extent_deferred` plus `seal_canonical` make it one pass;
  `Inode` and `DirNode` gain the same one-pass `seal_canonical` for the objects
  every builder writes beside it. Object ids are unchanged; a test pins the
  eager and deferred routes to identical bytes and hashes. On the staged path,
  which rebuilds the node on every operation, an isolated measurement put a
  staged append at p50 76 → 50 µs and the 512-batch tail at 132 → 80 µs.

### Lineage: what existed before this release

Everything in this section existed in some internal state before the crates were
published; it is recorded because each entry is a fact about the format or the
crates rather than because it was a change you could have observed.

#### Added

- **The on-disk format**, specified in [`FORMAT.md`](FORMAT.md): content-addressed
  objects with BLAKE3 identity, four superblock copies with a Root Locator Ring,
  a ring-buffer intent log, a persistent allocator snapshot, and a commit
  protocol whose write order is the crash-consistency argument.
- **`ledgerfs`** (the `no_std` half): canonical encoding with
  strict decode rejection, `ObjectRecord` framing and verification, the object
  model, the block allocator with emergency reserves, copy-on-write transactions,
  and the ATA reader and writer used when the crate is the only path to its own
  partition.
- **`ledgerfs-host`** (the `std` half): `Store`, which formats, mounts
  and mutates a volume over any `BlockDevice`. `FileBlockDevice` puts a volume in
  a file or a partition and takes an advisory exclusive lock that refuses a
  second writer; `MemBlockDevice` keeps one in memory and can be snapshotted to
  bytes and rebuilt from them, which is how the durability tests simulate a
  power cut.
- **`benchmarks/`**: the comparison harness against SQLite in WAL mode, `redb`,
  and raw `File::write` + `sync_all`, with a guard that refuses to run on a
  filesystem whose barrier is not real.
- **`pending/`** (the subsystems `FORMAT.md` §44 calls Tier B): the dedupe index,
  snapshots, the scrubber, GC safety rules and content-defined chunking. They are
  implemented and tested, but nothing in the shipping path calls them, so they
  are built in this workspace and published to nobody.

#### Features

- `reader`: compiles the ATA PIO read path and its port-I/O primitives. Off by
  default; it is ring-0 code and does not belong in a userspace binary.
- `std`: pulls the `lz4` and `zstd` decoders that `compression` gates, so a
  record written under either policy decodes.
- `data-encryption`: transparent XTS-AES-256 over a volume's raw blocks,
  applied at the block device rather than per chunk, and chosen at format time.
  Off by default, and opt-in per volume: a lost key makes the volume
  permanently unreadable, which is a user-facing tradeoff rather than a default.
- `temper_sealing`: ML-DSA-65 and SLH-DSA sealing of audit records, via the
  `temper-rs` crate on crates.io. Off by default; a plain filesystem build does
  not need a post-quantum signature stack.

#### Changed - Breaking

- **The `data-encryption` key-derivation domain strings were renamed.** The two
  BLAKE3 domain separators (one stretching a passphrase into a key-encryption
  key, one checksumming the master key) now begin `LedgerFS.VolumeKeyslot.`
  rather than naming the project this crate was developed inside. A domain
  separator is a cryptographic separator, not a label: the same passphrase
  derives a different KEK under the new strings, so a volume encrypted by an
  earlier build does not unlock under this one. No volume has been released, and
  the two literals live only in this crate; nothing in a published volume or on
  a wire format carries them. A development volume from before this change is
  unreadable; reformat it.

#### Changed

- **The x86 port-I/O primitives are vendored.** The four instructions the ATA
  path issues (`inb`, `outb`, `rep insw`, `rep outsw`) now live in
  `kernel/src/arch/x86_port.rs` instead of arriving as a dependency. A filesystem
  that owns an on-disk format should not take a dependency on its host OS's
  assembly crate to reach four instructions.
- **The crates build on stable.** Neither uses a nightly feature, so neither
  needs a nightly toolchain; the declared minimum is Rust 1.89 and edition 2024.
  The benchmark harness is a workspace binary rather than a published crate and
  needs 1.90; see [`BENCHMARKS.md`](BENCHMARKS.md).

#### Removed

- Internal work-log documents: per-phase implementation summaries, standalone
  test-status reports, and the shell scripts that re-ran one phase's checks.
  They described a project phase rather than a system, and several stated test
  counts that were true only on one feature set.

#### Fixed before this release

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
  persisting the allocator snapshot to a pre-reserved block: it cannot be stored
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
