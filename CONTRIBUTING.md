# Contributing

## Before you start

The kernel crate is `no_std` with `alloc` and carries a feature matrix; a change
that builds under `--all-features` has not been built. CI runs the same list:

```sh
cargo test -p ledgerfs
cargo test -p ledgerfs --no-default-features --features std
cargo test -p ledgerfs --no-default-features --features reader
cargo test -p ledgerfs --no-default-features --features data-encryption
cargo test -p ledgerfs --no-default-features --features temper_sealing
cargo test -p ledgerfs --all-features
cargo test -p ledgerfs-host
cargo test -p ledgerfs-pending
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets -- -D warnings
```

`reader` and `data-encryption` are the two that catch people. The first compiles
ring-0 ATA port I/O and only across `#[cfg(target_arch = "x86_64")]`; the second
swaps the block device under the whole CoW object model. A change that compiles
under neither is still broken.

## Toolchain

`rust-toolchain.toml` pins current stable, and says why it is not pinned to the
floor. The floors are the `rust-version` key in each manifest: 1.89 for the two
published crates and for the unpublished `pending/` crate, 1.90 for the
benchmark harness. CI builds the 1.89 crates on 1.89, so a change that needs
something newer than 1.89 in `kernel/`, `host/` or `pending/` fails there rather
than in a user's build.

## Formatting

The tree is indented with hard tabs and `max_width = 120`; the settings are in
`rustfmt.toml`, so a plain `cargo fmt --check` passes with no extra config.

## On-disk format changes

`FORMAT.md` is normative, and it is not a summary written after the fact — it
describes the same bytes the code writes, section by section, and the code cites
it by section number. If you change a field, an encoding, a limit, or an ordering
constraint:

- Update `FORMAT.md` in the same commit as the code, and keep the section
  numbers stable. A renumbering silently breaks every `§N` citation in the tree.
- Bump the format version and say so in `CHANGELOG.md`. A reader that accepts a
  volume it cannot correctly interpret is worse than one that refuses it.
- Say whether existing volumes still mount. "The record grew by four bytes" is
  a compatibility break even when it compiles.

## Ordering constraints

The commit protocol's write order is load-bearing: objects and the allocator
snapshot, one barrier, then the superblock copies, then a second barrier that
commits them. Swapping two stages still passes every test that does not inject a
fault, and loses data on the first power cut. If you touch that path, test it
with the fault injection in the durability suite, and prefer a test that fails
when you remove the barrier to one that passes when you keep it.

The barrier count is not the thing to optimise, and a barrier is not expensive
in itself: one that follows no new dirty data costs nothing on any real device.
What costs is a barrier that follows new data. Two that do are the floor for
this protocol's design — one to get the objects on media before the superblock
names them, one to commit — so a change that removes a barrier has to say which
of those two it removed and why the ordering it established no longer matters.

## Tests

- **A test that passes proves the happy path works, not that the check is
  load-bearing.** For a rejection path, break the check deliberately, confirm
  the test fails, then restore it.
- **Prefer a malformed input to a mocked one.** The decoder's bounds are the
  security surface; a hand-built three-byte record is a better test than a
  trait object that returns an error.
- Tests in this tree use the `// -- Setup & Fixtures` / `// -- Exec` /
  `// -- Check` region markers, and return a `Result` rather than unwrapping.
  Keep that shape.

## Benchmarks

`benchmarks/` is a plain binary, not a criterion suite, and it refuses to run on
a filesystem that cannot give it an honest answer — a tmpfs has no barrier to
pay for, so every durability number measured on one is fiction. If you add a
workload, keep the guard and keep the machine record: a number without the
device, filesystem, and durability setting it came from is not a result.

Checked-in results live in `benchmarks/results/`. Regenerate `BENCHMARKS.md`
from them rather than editing the tables by hand.

## Commits

Keep messages short and factual. Explain *why* in the body when the change is not
self-evident; the code comments are for what the code cannot say itself.
