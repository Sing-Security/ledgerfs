# Releasing

Two crates publish from this repository, and they publish in a fixed order
because one depends on the other by version:

| order | crate | version in the manifest | why the order |
| --- | --- | --- | --- |
| 1 | [`ledgerfs`](kernel/Cargo.toml) | 0.7.0 | nothing depends on it at publish time |
| 2 | [`ledgerfs-host`](host/Cargo.toml) | 0.1.0 | depends on `ledgerfs = { path = "../kernel", version = "0.7" }`; Cargo strips the path and resolves the version from the registry |

`benchmarks/` and `pending/` are not published and must not be. If either ever
grows a `publish` key, the release is wrong, not the manifest.

## The gate that cannot be skipped

**A `cargo publish` requires an independent verification pass first.** Not a
re-read of the diff by the author, and not "the tests are green". A separate
review whose job is to ask what the package contains and what a fresh consumer
gets. crates.io cannot delete a version; it can only yank it, and the name stays
taken forever. Everything below is the mechanical part of that pass; the
judgement part is a second pair of eyes.

The things that pass must answer, because they have been wrong before:

- Does every published crate carry its own `LICENSE-MIT`, `LICENSE-APACHE` and
  `NOTICE`? `license = "MIT OR Apache-2.0"` states the grant; Apache-2.0 §4
  requires the text and the NOTICE to travel with the distribution, and a file
  in the repository root is **not** in the `.crate`.
- Does every link in a crate's `README.md` resolve **on crates.io**? A relative
  link such as `../FORMAT.md` points outside the package and renders broken.
  Links to repository documents must be absolute
  (`https://github.com/Sing-Security/ledgerfs/blob/main/...`).
- Does `cargo package` reproduce the crate you think you are shipping? Read
  `cargo package --list` line by line; it is the actual file list.
- Does the dependency graph resolve from crates.io alone? A path dependency
  that is not also a version requirement is a publish-time error, and a
  pre-release dependency is a consumer-time one.
- Is the benchmark provenance still redacted? `BENCHMARKS.md` and
  `benchmarks/results/` must not name a hostname, a device path, or the LUKS
  UUID of the volume they were measured on.

## Checklist

1. **Decide the version.** `ledgerfs` and `ledgerfs-host` version independently;
   bump the one whose public surface changed. Note that the on-disk format has
   its own version field, which is a third number again, so a format change is not
   automatically a crate major.
2. **Fold [`CHANGELOG.md`](CHANGELOG.md).** `## [Unreleased]` becomes the new
   version's section with today's date; the entries go under `Added`, `Changed`,
   `Deprecated`, `Removed`, `Fixed` or `Security`.
3. **Green gates, on the exact commit that will publish.**

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace
   RUSTDOCFLAGS="-D warnings" cargo doc -p ledgerfs -p ledgerfs-host --no-deps --all-features
   ```

   The doc command is not decoration: both crates deny `missing_docs`, and
   docs.rs builds with `all-features`, so a link or an item that only resolves
   under one feature set fails the release rather than the build.

4. **Package and inspect.**

   ```sh
   cargo package --list -p ledgerfs
   cargo package --list -p ledgerfs-host
   cargo package -p ledgerfs          # builds the .crate from scratch
   ```

   For `ledgerfs-host`, `cargo package` (with verification on) fails until
   `ledgerfs` is on the registry; that is the publish order, not a defect. Use
   `--no-verify` for the file-list check, and re-run it verified after step 5.

5. **Publish in order, without `--no-verify`.**

   ```sh
   cargo publish -p ledgerfs
   # wait until the index serves it: https://crates.io/api/v1/crates/ledgerfs
   cargo publish -p ledgerfs-host
   ```

6. **Verify what a stranger gets.** In an empty directory outside the
   repository:

   ```sh
   cargo new /tmp/lfs-smoke && cd /tmp/lfs-smoke
   cargo add ledgerfs-host
   cargo test
   ```

   Then check the two crates.io pages render: README, license, keywords,
   categories, and the docs.rs build for both (docs.rs builds the kernel with
   `all-features`, which is why that build is the one that must be green).

7. **Tag it.** One annotated tag per crate version published
   (`ledgerfs-v0.7.0`, `ledgerfs-host-v0.1.0`), on the commit that was
   published. The repository is private; pushing is a separate decision from
   publishing.

## What a version number means here

- `ledgerfs` follows the format. A change that alters the bytes of an object, a
  superblock or an allocator snapshot changes the format version field in the
  superblock, and is documented in [`FORMAT.md`](FORMAT.md), including the
  mount-compatibility story: a volume written by the new format either mounts in
  the old reader or is refused by it as an incompatible feature, never decoded
  as something else.
- `ledgerfs-host` follows the embedding API, not the format. Its
  `STORE_CONTRACT_VERSION` constant names the behavioural contract an embedding
  application relies on (atomic replace, one commit per verb), which is a
  different promise from the version number and can change with it.
- Neither crate publishes a breaking change without a `### Changed - Breaking`
  entry that says what a consumer must do.
