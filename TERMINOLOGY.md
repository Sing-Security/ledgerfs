# Terminology

The words this project uses, in the senses it uses them. This document is
**normative for prose and naming**: when `README.md`, `EMBEDDING.md`,
`BENCHMARKS.md` or the source says *seal*, *draft* or *commit*, it means the
thing defined here, and a public API name follows the same word.

The point is not pedantry. LedgerFS is a filesystem that is also used as the
storage layer under an application and under an operating system, and most of
the confusion such a system attracts is vocabulary borrowed from databases, from
POSIX filesystems, or from write-ahead-log storage, each of which brings a
different promise with it. Where a word is taken from another field, this
document says which of its meanings applies and which does not.

The on-disk specifics live in [`FORMAT.md`](FORMAT.md) and the application
contract in [`EMBEDDING.md`](EMBEDDING.md); this file is the vocabulary those
two share.

## 1. What the product is

| Term | Meaning |
| --- | --- |
| **LedgerFS** | The project: two crates, one on-disk format. Written with a capital L and F, no space or hyphen. |
| **Embedded filesystem** | A filesystem that is a **library** rather than a kernel service. An application links it, gives it a block device, and gets file verbs in-process. Nothing registers with a VFS, so no path on the host resolves into a volume. An operating system can still adopt it as its own filesystem by writing the service layer above it; CrucibleOS does exactly that. |
| **Application storage layer** | What a volume is when an application uses it as its database: the place its records live. The description that matches the use, and the one that carries what a filesystem gives that a database engine does not. |
| **Database** | A role a volume can occupy. It holds an application's records and returns them, and for a large class of applications that is the complete requirement. The word names what LedgerFS can replace; it does not denote a database engine. |
| **Database engine** | What LedgerFS is not, yet: a catalog, secondary indexes, a query planner and an executor. Those are a layer above this one and can be built on it; none of them is part of this release. Reach for this phrase when a reader needs to know what is missing. |
| **Capture tier** | The data class every decision here was made for: records that arrive once, are written once, and are read back whole or by byte range. A capture workload never rewrites a record and never queries across records. |
| **Volume** | One formatted LedgerFS filesystem: a flat array of 4 KiB blocks with a superblock, a root, and a tree under it. The unit that is formatted, opened, committed to, snapshotted as bytes, and carried around. The file or partition holding a volume is *not* the volume. |
| **Embedding** | Using a volume from inside an application process, as opposed to provisioning it onto a disk. Both are supported deployments; neither is a special case of the other. |
| **Provisioning** | Formatting a volume onto a real partition that the owner of the machine will keep, as opposed to one that lives in a file or in memory. |

## 2. The data model

| Term | Meaning |
| --- | --- |
| **Object** | The only kind of stored thing. An immutable, content-addressed blob whose name is the BLAKE3 hash of its canonical encoding. Everything on a volume is objects. |
| **ObjectId** | A 32-byte BLAKE3 hash, the name of an object. Two equal ids mean two identical objects, which is why the format can deduplicate and verify by name alone. |
| **ObjectRef** | An `ObjectId` plus a **storage hint**. The id is authoritative; the hint is advice. |
| **Storage hint** | A claim about which block an object is believed to live at. It may be zero, stale, or wrong, and a reader re-verifies it before use. Never say "pointer" for a hint: a pointer that can be wrong is not a pointer. |
| **Chunk** | The object that holds file content bytes, at most `max_object_bytes` (256 KiB by default). A file is an extent list over chunks. |
| **Inode** | The object that describes one file: kind, mode, size, timestamps, and the reference to its extent tree. |
| **DirNode** | The object holding one directory's entries: sorted names, each naming an inode. A directory is a tree of DirNodes. |
| **ExtentNode** | The object holding a file's extents, or children from which they can be reached. It is the one metadata object whose encoding grows with the file's history, which is why the append path cares about it. |
| **Extent descriptor** | One entry in an extent tree: a logical offset, a length, a chunk id, and an offset within that chunk. |
| **Root** | The object from which everything else on a volume is reachable, named by the superblock. Exactly one root is live at a time. |
| **Generation** | One version of one object, produced by one write. A copy-on-write operation creates a new generation and names it; it never edits the old one. |
| **Superblock** | The 4 KiB block that names the live root, its commit sequence, the volume's limits and its key material. Four copies exist per volume. |
| **Root Locator Ring (RLR)** | The ring of recently committed root identities inside the superblock, which is what lets a mount find a root when the newest superblock copy is damaged. |

## 3. The write path

| Term | Meaning |
| --- | --- |
| **Transaction** | The set of objects one operation produces, plus the root that names them. Built entirely in memory before anything is placed. Say *transaction* for this bundle; say **commit** for making it durable. |
| **Commit** | One transaction made durable. A commit is **the durability point**: objects and the allocator snapshot are written, a barrier flushes them, the superblock copies are written, and a second barrier flushes those. Before the superblock write the old root is the filesystem; after it, the new one. There is no interval in which a crash leaves the volume half-changed. |
| **Barrier** | A durability operation that guarantees everything written before it is on stable media before anything after it is. `fsync` on a file, a cache flush on a disk. On a device whose barrier is not real, a benchmark refuses to run. |
| **Allocator snapshot** | The block allocator's serialized state, written as part of a commit so a mounted volume can resume writing. It lives at a fixed block, not in an ordinary object, because writing it through the allocator would change the state being written. |
| **Commit sequence** | The monotonic counter a commit advances. `commit_seq` is the volume's, and a seal is one commit no matter how many operations it folds. Never call it a "version" or a "timestamp". |
| **Copy-on-write (CoW)** | The rule that nothing is ever modified in place: an operation writes a new generation and re-points the parent at it. Say *copy-on-write*, or *CoW*, but not *journal*: there is no journal in the write path. |
| **Intent log** | A ring of records describing an in-flight commit, defined by the format for crash recovery. It is **not** a write-ahead log and the live path does not write one; `FORMAT.md` §11 and §18.5 say why (a log the format would have to trust is a second source of truth). Use "intent log" only for this structure, never as a synonym for a WAL. |
| **Staging session** | A library-level mode in which writes are gathered in memory and made durable together, at an explicit seal. Opened with `begin_staging`, closed by `seal_staged`, `seal_staged_deferred` + `sync`, or discarded by `end_staging`. It is not a transaction in the database sense (no isolation from the writer, no rollback of committed work), and it is not a log. |
| **Staged write** | An operation performed inside a staging session: it builds or extends in-memory state and touches no device block, no allocator state and no barrier. |
| **Draft** | The in-memory state of one file a staging session has appended to: the inode the tree names, its extent list, the bytes appended since, and the size those bytes imply. An append extends a draft; the objects it implies are built when their identity is needed. The draft is an implementation of the staging tier, not a format concept: nothing on a volume is a draft. |
| **Materialize** | Turn a draft into the objects the eager path would have produced (extent node, inode, re-pointed directory entry, cascaded ancestors) without publishing them. The seal materializes every remaining draft; `stat` and `read_dir` materialize the draft they are asked about, because a size or an inode reference is an identity a draft does not have until then. Contents reads do not materialize. |
| **Seal** | The act that makes a staging session's work durable: one transaction of the ordinary shape, one commit, one commit-sequence step. The word is specific to staging and is never a synonym for "flush", "sync" or "save". |
| **Deferred seal** | A seal that stops before the superblock write (`seal_staged_deferred`), so several seals can share one `sync`. The group is whole or lost whole; a crash never sees a blend. |
| **Prune** | At the seal, dropping every staged object the final root does not reach, typically the intermediate generations a staging session built. Pruning is why N staged appends carry N chunks plus one generation of each metadata object on the medium. |
| **Truth tier** | Which answer a read returns while a session is open: the **staged tier** (the owning handle's own writes, answered from memory, may be lost on a crash) or the **committed tier** (the superblock's generation, durable, exactly what any other handle sees). The caller chooses per read; there is no ambient default. |

## 4. Say this, not that

| Do not say | Say | Because |
| --- | --- | --- |
| "not a database" (as a flat denial) | "not a database engine"; "an embedded filesystem used as an application storage layer" | It holds an application's data and gives it back. What it lacks is a schema, a query language and indexes, which is a narrower denial than the flat one and leads to a fair comparison. |
| "database" (of LedgerFS, as a claim about the storage) | database, application storage layer | Correct if the next sentence states what is absent (see **database engine**). The system is a different structure for the same role, not a refusal to fill it. |
| "flush", "save" | seal, commit | Both commit and seal have exact meanings here. "Flush" also names a specific device operation. |
| "journal", "WAL" | commit protocol, intent log | There is no write-ahead log in the write path; the protocol's order is the crash-consistency argument. |
| "sync" (as a noun for a commit) | barrier, commit, `sync()` | A barrier is one step; a commit is the whole thing; `sync()` is the deferred seal's publishing call. |
| "mount point", "mount a partition" | open a volume | Nothing mounts into a path namespace. The host API's verb is `open`. |
| "file system" | filesystem | One word, as in the crate names and the rest of the docs. |
| "extent tree entry" | extent descriptor | The descriptor is the entry; the tree is the structure. |
| "pointer" (for a reference) | reference; storage hint | References are content hashes plus advice. Nothing is a raw address. |
| "overwrite" (for anything but `write_at`'s byte range) | replace | Files are replaced as a whole; `write_at` may append past the end but cannot rewrite stored bytes. |
| "transaction" (for a staging session) | staging session; seal | A session is not atomic and has no isolation; its seal is the atomic, durable step. |
| "version" (of a volume) | commit sequence, generation | "Version" is reserved for crate and format version numbers. |
| "crash-safe" (unqualified) | crash-consistent at the commit point | The guarantee is specific: old root or new root, never a blend. |

## 5. Typography and identifiers

- Numbers and units: `4 KiB` blocks, `256 KiB` objects, `MiB/s` rates, binary
  units throughout, as the code uses.
- `BLAKE3` in prose and code; the crate is `blake3`.
- `no_std` and `std` in backticks; "stable Rust" with a minimum version from
  the manifests.
- Public API names are in backticks: `Store::seal_staged`, `begin_staging`,
  `read_file_committed`.
- A commit is written `commit_seq`; the schema field of a benchmark sample is
  `ns_per_op`. Use code formatting for anything a reader could grep.
- "LedgerFS" is never abbreviated. The crates are `ledgerfs` and
  `ledgerfs-host`; the CLI tools are `lfs-read` and `lfs-write`.
- Prose uses no em dashes. Use a colon, a semicolon, a comma, parentheses or a
  second sentence. This applies to every document and every doc comment, not
  only to this file.
