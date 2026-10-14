# LedgerFS v7 - Core On-Disk Substrate

**Phase 1**: On-disk format + mount/replay core (no directory/file ops yet)

## Overview

LedgerFS is a copy-on-write filesystem with:
- Content-addressed objects using BLAKE3
- Four-copy superblock with Root Locator Ring (RLR)
- Intent log for crash-consistent commit protocol
- Deterministic mount selection and replay

This crate implements the foundational on-disk data structures and algorithms
per `ledgerfs-spec-v7.md`.

## Architecture

### Key Components

1. **Canonical Encoding** (`encoding`)
   - Little-endian integer encoding
   - BLAKE3 hashing (full 32-byte and truncated 8-byte checksum)
   - Strict decode rejection rules

2. **ObjectRecord Framing** (`record`)
   - Content-addressed using BLAKE3
   - Header + payload structure
   - Checksum verification
   - 8-byte aligned records

3. **Superblock** (`superblock`)
   - 4096-byte block format
   - Four redundant copies (blocks 0, 1, N-2, N-1)
   - Root Locator Ring (RLR) with 56 entries
   - Deterministic mount selection algorithm
   - Split-brain detection

4. **Intent Log** (`intent_log`)
   - Circular ring buffer for transaction events
   - O(N) parsing with seq-window selection
   - Duplicate resolution (last-wins per tx_id)
   - Sequence collision detection
   - Record types: BEGIN, ADD_OBJECT, PREPARE_ROOT, COMMIT_ROOT, ABORT

5. **Filesystem Mode** (`mode`)
   - `ReadWrite`: Normal operation
   - `ReadOnlySafe`: Entered on split-brain or continuity violation
   - Write embargo enforcement

6. **Dedupe Index** (`dedupe`)
   - In-memory B-tree index mapping ObjectId → physical location
   - Hint-miss fallback for object reads
   - Refcount tracking for GC safety
   - Pin/unpin functionality for snapshot protection

7. **Content-Defined Chunking** (`cdc`) - W4-B
   - Gear hash algorithm for deterministic chunking
   - Min/avg/max boundaries: 16 KiB / 64 KiB / 256 KiB
   - Streaming support for incremental processing
   - Shift problem improvement (local re-chunking only)
   - Cross-platform deterministic boundaries

8. **Compression** (`compression`) - W4-A
   - Per-chunk compression with zstd/lz4
   - Automatic compression threshold (4 KiB size, 1.1 ratio)
   - ObjectId from canonical bytes (deterministic)
   - Checksum over stored (compressed) bytes

## Public API

### Record API

```rust
use crucible_ledgerfs::record::{ObjectRecordReader, ObjectRecordWriter, ObjectType};

// Write a record
let mut buf = vec![0u8; 1024];
let len = ObjectRecordWriter::write(
    &mut buf,
    ObjectType::Data,
    0, // no compression
    0, // no encryption
    data,
    data,
)?;

// Read and verify a record
let (header, payload) = ObjectRecordReader::read(&buf[..len])?;
```

### Superblock API

```rust
use crucible_ledgerfs::superblock::{Superblock, RlrEntry, mount_select};

// Create new superblock
let mut sb = Superblock::new(total_blocks);

// Add RLR entry
let entry = RlrEntry::new(root_id, commit_seq, timestamp, 0);
sb.add_rlr_entry(entry);

// Encode to 4KB block
let mut buf = vec![0u8; 4096];
sb.encode(&mut buf)?;

// Mount selection from 4 superblock copies
let superblocks = [Some(sb1), Some(sb2), None, None];
let selection = mount_select(&superblocks)?;
```

### Intent Log API

```rust
use crucible_ledgerfs::intent_log::{IntentLog, IntentLogRecord, IntentRecordType, parse_ring, build_tx_views, determine_replay};

// Create record
let record = IntentLogRecord::new(
    seq,
    tx_id,
    IntentRecordType::PrepareRoot,
    payload,
);

// Write to ring
let log = IntentLog::new(ring_size);
log.write_record(&mut ring, offset, &record)?;

// Parse ring (O(N))
let records = parse_ring(&ring, ring_size)?;

// Build transaction views
let tx_views = build_tx_views(&records);

// Determine replay decision
let decision = determine_replay(mounted_seq, &tx_views)?;
```

### Mode API

```rust
use crucible_ledgerfs::FsMode;

let mode = FsMode::ReadWrite;
mode.check_write_allowed()?; // Ok

let mode = FsMode::ReadOnlySafe;
mode.check_write_allowed()?; // Error: write embargo
```

### Dedupe Index API

```rust
use crucible_ledgerfs_kernel::dedupe::{DedupeIndex, PhysicalLocator};
use crucible_ledgerfs_kernel::object::ObjectId;

// Create dedupe index
let mut index = DedupeIndex::new();

// Insert object location (increments refcount if already exists)
let object_id = ObjectId::from_content(b"data");
let locator = PhysicalLocator::new(lba, record_stored_len);
index.insert(object_id, locator)?;

// Lookup for hint-miss fallback
match index.lookup(&object_id) {
    Some(locator) => {
        // Hint hit - read from locator.lba
    }
    None => {
        // Hint miss - fall back to full scan/index
    }
}

// Refcount management
index.increment_refcount(&object_id)?;
index.decrement_refcount(&object_id)?;

// GC safety
index.pin(&object_id)?;  // Protect from GC
let candidates: Vec<_> = index.gc_candidates().collect();
```

### CDC API (W4-B)

```rust
use crucible_ledgerfs_kernel::cdc::{Chunker, StreamingChunker};

// Batch chunking
let mut chunker = Chunker::new();
let boundaries = chunker.chunk_data(&data);

// boundaries = [65536, 131072, ...] (positions where chunks end)

// Streaming chunking (for direct-channel writes)
let mut streaming = StreamingChunker::new();

// Process data incrementally
for chunk in data_stream {
    let boundaries = streaming.process(&chunk);
    // Handle complete chunks
}

// Finalize to get last chunk
if let Some(final_pos) = streaming.finalize() {
    // Handle final chunk
}
```

### Compression API (W4-A)

```rust
use crucible_ledgerfs_kernel::compression::{CompressionAlgorithm, compress, decompress};

// Compress data
let compressed = compress(&data, CompressionAlgorithm::Zstd)?;

// Decompress data
let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd, expected_size)?;
```

## Constraints

- `#![no_std]` + `alloc` (no standard library)
- All locators use `record_stored_len` semantics
- Ring parsing is O(N) with bounded memory
- No `HashMap` in serialized structures (use `BTreeMap`/`BTreeSet`)
- Deterministic output (same input → same output)

## Phase 1 Implementation Notes

### RLR Size Adjustment

The spec calls for 256 RLR entries, but this would require >20KB (256 * 80 bytes).
Since the superblock must fit in a 4096-byte block, **Phase 1 uses 56 RLR entries**
(60 bytes each, matching `RLR_SIZE` and `RlrEntry` in `src/superblock.rs`).

This provides sufficient history for typical mount scenarios while fitting in the
4KB block constraint. Future phases may extend RLR to a separate region.

### Simplified Root Verification

Phase 1 implements the mount selection algorithm but defers full root verification
to later phases. The current implementation:
- Selects highest `commit_seq` candidate
- Detects split-brain (same seq, different root_id)
- Prefers RLR candidates over active slot
- Enters read-only safe mode on split-brain

Full root verification (traversing B-tree, checking refcounts) will be added in
later phases when directory/file operations are implemented.

## Testing

Unit tests (89+ tests including 17 dedupe tests + 11 CDC tests):

```bash
cd crucible/kernel/ledgerfs
cargo test --lib
```

Integration tests:

```bash
# Dedupe integration tests (11 tests)
cargo test --features std --test dedupe_integration

# CDC standalone tests (11 tests) - blocked by pre-existing build errors
cargo test --features std --test cdc_standalone
```

Test coverage:
- Encoding: 8 tests (checksums, roundtrips, validation)
- Record: 7 tests (framing, verification, determinism)
- Superblock: 6 tests (encoding, RLR, mount selection, split-brain)
- Intent Log: 8 tests (parsing, seq collision, tx views, replay)
- Mode: 3 tests (write embargo enforcement)
- Dedupe: 28 tests (17 unit + 11 integration)
- CDC: 22 tests (11 module + 11 standalone) - blocked by build errors
- Compression: 22 tests - blocked by build errors

**Note**: W4-A compression and W4-B CDC tests are implemented and compile-checked
but cannot run due to pre-existing build errors in placement.rs, commit.rs, and
alloc_node.rs. See `W4A_IMPLEMENTATION_SUMMARY.md` and `W4B_IMPLEMENTATION_SUMMARY.md`
for details.

Verification scripts:
```bash
# Verify W4-B CDC (after fixing build errors)
./verify_w4b.sh

# Verify W4-A compression (after fixing build errors)
./verify_w4a.sh
```

## Next Steps (Phase 2+)

Phase 1 provides the foundational on-disk substrate. Future phases will add:

1. **Directory Operations**: B-tree implementation, path resolution
2. **File I/O**: Extent mapping, data chunking, CoW mechanics
3. **Full Replay**: Intent log replay with root verification
4. **Dedupe**: Content-defined chunking, dedupe index
5. **GC**: Deadlist processing, allocation extent reconciliation
6. **Snapshots**: Subvolume cloning, reflink
7. **Encryption**: Per-subvolume encryption via Temper

## References

- **Specification**: `ledgerfs-spec-v7.md` (canonical normative spec)
- **Architecture**: `ledgerfs-arch-v7` (archival design rationale)
- **Problem Statement**: Issue describing Phase 1 requirements
