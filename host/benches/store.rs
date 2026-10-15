//! Criterion benchmarks for every [`Store`] action — the "measure every
//! action" harness.
//!
//! # What it answers
//!
//! Every verb an embedding application calls is one group here, at the sizes an
//! application actually uses: `mkfs`, mount, create, replace, append, read,
//! `read_dir`, `stat`, `remove`, `rename`, the space queries, `walk`, the
//! [`rebuild_into`] pass, and a campaign-shaped workload that strings them
//! together. Each is measured over both devices the crate offers — an in-heap
//! [`MemBlockDevice`], where the number is pure protocol cost, and a
//! [`FileBlockDevice`], where `fsync` dominates — because the two differ by
//! more than an order of magnitude and only the second is what a real
//! application pays.
//!
//! # How to run
//!
//! ```text
//! cargo bench --offline                  # everything; HTML under target/criterion
//! cargo bench --offline -- write         # only groups whose id contains "write"
//! cargo bench --offline -- --save-baseline before
//! cargo bench --offline -- --baseline before
//! ```
//!
//! # Why every mutation benchmark re-formats its volume
//!
//! CoW frees nothing (`EMBEDDING.md` §3): a volume grows on every commit and
//! never shrinks, so a benchmark cannot mutate one volume in a loop — it hits
//! out-of-space a few hundred commits in. Each mutating benchmark therefore
//! builds a fresh volume in `iter_batched` setup, which Criterion excludes from
//! the measured time. That is also why the volumes below are small: the setup
//! (a full-device erase, see [`Store::format`]) is real wall-clock even though
//! it is not measured, and 1 MiB makes it cheap.
//!
//! A second consequence, worth stating because it is the whole point of the
//! suite: a number here is the cost of *one* action on a *fresh* volume. It
//! does not include the fact that the volume the action wrote to can never give
//! the space back. That is [`bench_rebuild`], and it is the reason compaction
//! has to exist.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{
	BatchSize, BenchmarkGroup, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};

use ledgerfs_host::{BLOCK_SIZE, FileBlockDevice, MemBlockDevice, Store, rebuild_into};

// region:    --- Harness

/// Blocks in the in-memory volume a mutation benchmark runs against — 1 MiB.
/// Large enough for any single write below (a 64 KiB file is one object), small
/// enough that re-formatting it per iteration stays cheap.
const MEM_BLOCKS: u64 = 256;

/// Blocks in the file-backed volume — 4 MiB. The cost there is `fsync` and the
/// erase in `format`, neither of which scales with the volume at these sizes.
const FILE_BLOCKS: u64 = 1024;

const KB: usize = 1024;

/// Unique names for the file-backed images, so left-over files from an aborted
/// run cannot collide with a fresh one.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A group with settings chosen for a *suite* of ~30 benchmarks: enough samples
/// for a stable median, short enough that the whole run is minutes, not hours.
fn group<'a>(c: &'a mut Criterion, id: &str) -> BenchmarkGroup<'a, WallTime> {
	let mut g = c.benchmark_group(id);
	g.sample_size(20);
	g.warm_up_time(Duration::from_millis(300));
	g.measurement_time(Duration::from_millis(1500));
	g
}

/// A freshly formatted in-heap volume.
fn mem_store(blocks: u64) -> Store {
	Store::format(Box::new(MemBlockDevice::new(blocks)), blocks).expect("format a fresh in-memory volume")
}

/// A formatted in-heap volume, returned as the bytes a volume file would hold —
/// so the mount benchmark measures mounting, not formatting.
fn store_bytes(blocks: u64) -> Vec<u8> {
	let device = MemBlockDevice::new(blocks);
	// `format` takes ownership of the device; this clone still names the same
	// buffer, which is how the bytes come back out.
	let snapshot = device.clone();
	Store::format(Box::new(device), blocks).expect("format");
	snapshot.bytes_snapshot()
}

/// An in-heap volume already holding `files`, under a `/notes` directory.
fn filled_mem_store(blocks: u64, files: &[(&str, &[u8])]) -> Store {
	let mut store = mem_store(blocks);
	store.mkdir("/notes", 0o755).expect("mkdir /notes");
	for (name, data) in files {
		store.write_new_file(name, data, 0o644).expect("write fixture");
	}
	store
}

fn temp_image(tag: &str) -> PathBuf {
	let n = SEQ.fetch_add(1, Ordering::Relaxed);
	std::env::temp_dir().join(format!("lfs-bench-{}-{tag}-{n}.img", std::process::id()))
}

/// A freshly formatted file-backed volume, and the path to remove afterwards.
fn file_store(tag: &str) -> (Store, PathBuf) {
	let path = temp_image(tag);
	let device = FileBlockDevice::create(&path, FILE_BLOCKS * BLOCK_SIZE as u64).expect("create the benchmark image");
	let store = Store::format(Box::new(device), FILE_BLOCKS).expect("format the benchmark image");
	(store, path)
}

/// A freshly formatted file-backed volume of an explicit size, for the
/// benchmarks where the volume's *geometry* is the thing under test.
fn big_file_store(tag: &str, blocks: u64) -> (Store, PathBuf) {
	let path = temp_image(tag);
	let device = FileBlockDevice::create(&path, blocks * BLOCK_SIZE as u64).expect("create the benchmark image");
	let store = Store::format(Box::new(device), blocks).expect("format the benchmark image");
	(store, path)
}

/// Drop the store and delete its image, after the write being measured.
fn retire(store: Store, path: &Path) {
	drop(store);
	let _ = std::fs::remove_file(path);
}

// endregion: --- Harness

// region:    --- Format / mount

/// `mkfs` — and the one benchmark whose cost scales with the volume, because
/// `Store::format` erases every block past the superblocks so the medium holds
/// this filesystem and nothing else.
fn bench_mkfs(c: &mut Criterion) {
	let mut g = group(c, "mkfs");
	for blocks in [64u64, 256, 1024, 4096] {
		g.throughput(Throughput::Bytes(blocks * BLOCK_SIZE as u64));
		g.bench_with_input(BenchmarkId::from_parameter(blocks), &blocks, |b, &blocks| {
			b.iter(|| black_box(mem_store(blocks)));
		});
	}
	g.finish();
}

/// Mounting an existing volume, which is the cost an app pays on every launch
/// and every CLI read. Volume size is the parameter because that is what the
/// object store's scan walks.
fn bench_mount(c: &mut Criterion) {
	let mut g = group(c, "mount_mem");
	for blocks in [64u64, 256, 1024] {
		let snapshot = store_bytes(blocks);
		g.bench_with_input(BenchmarkId::from_parameter(blocks), &snapshot, |b, snapshot| {
			b.iter_batched(
				|| MemBlockDevice::from_bytes(snapshot.clone()).expect("adopt snapshot"),
				|device| black_box(Store::open(Box::new(device)).expect("mount")),
				BatchSize::PerIteration,
			);
		});
	}
	g.finish();
}

/// Opening a file-backed volume read-only — what `lfs-read` and the CLI do
/// against a volume an app is already holding.
fn bench_mount_file(c: &mut Criterion) {
	let mut g = group(c, "mount_file");
	let (mut store, path) = file_store("mount");
	store.write_new_file("/a.md", b"fixture\n", 0o644).expect("fixture");
	// The writer must be gone before a *second* writable open could succeed; a
	// read-only open takes no lock, which is the case being measured.
	drop(store);

	let size = FILE_BLOCKS * BLOCK_SIZE as u64;
	g.bench_function("open_read_only", |b| {
		b.iter(|| {
			let device = FileBlockDevice::open(&path, 0, size).expect("open image");
			black_box(Store::open(Box::new(device)).expect("mount"))
		});
	});
	let _ = std::fs::remove_file(&path);
	g.finish();
}

// endregion: --- Format / mount

// region:    --- Creating

/// Creating things: a directory, an empty file, and a file with content — from
/// a 12-byte note to a 64 KiB evidence blob.
fn bench_create(c: &mut Criterion) {
	let mut g = group(c, "create");

	g.bench_function("mkdir", |b| {
		b.iter_batched(
			|| mem_store(MEM_BLOCKS),
			|mut store| store.mkdir("/notes", 0o755).expect("mkdir"),
			BatchSize::PerIteration,
		);
	});

	g.bench_function("create_file", |b| {
		b.iter_batched(
			|| mem_store(MEM_BLOCKS),
			|mut store| store.create_file("/empty", 0o644).expect("create_file"),
			BatchSize::PerIteration,
		);
	});

	// The nested case is what a campaign actually does: the parent has to exist,
	// so this is the whole path walk plus the commit.
	g.bench_function("write_new_file_nested", |b| {
		b.iter_batched(
			|| {
				let mut store = mem_store(MEM_BLOCKS);
				store.mkdir("/notes", 0o755).expect("mkdir");
				store.mkdir("/notes/sessions", 0o755).expect("mkdir");
				store
			},
			|mut store| {
				store
					.write_new_file("/notes/sessions/s1.md", b"handover\n", 0o644)
					.expect("write")
			},
			BatchSize::PerIteration,
		);
	});

	for size in [12usize, 4 * KB, 64 * KB] {
		let data = vec![b'x'; size];
		g.throughput(Throughput::Bytes(size as u64));
		g.bench_with_input(BenchmarkId::new("write_new_file", size), &data, |b, data| {
			b.iter_batched(
				|| {
					let mut store = mem_store(MEM_BLOCKS);
					store.mkdir("/notes", 0o755).expect("mkdir /notes");
					store
				},
				|mut store| store.write_new_file("/notes/a.md", data, 0o644).expect("write"),
				BatchSize::PerIteration,
			);
		});
	}

	g.finish();
}

/// Replacing an existing file — the idempotent write, and the one a relay does
/// on every session boundary.
fn bench_replace(c: &mut Criterion) {
	let mut g = group(c, "replace");
	for size in [12usize, 4 * KB] {
		let data = vec![b'y'; size];
		g.throughput(Throughput::Bytes(size as u64));
		g.bench_with_input(BenchmarkId::new("write_file", size), &data, |b, data| {
			b.iter_batched(
				|| {
					let mut store = mem_store(MEM_BLOCKS);
					store.write_new_file("/state.json", b"{}", 0o644).expect("fixture");
					store
				},
				|mut store| store.write_file("/state.json", data, 0o644).expect("replace"),
				BatchSize::PerIteration,
			);
		});
	}
	g.finish();
}

/// `append` — the verb that is quadratic if you loop it on a growing file. The
/// two cases are deliberately split: appending to a *fresh* file measures the
/// create path, appending to an *existing* file measures the extension path,
/// and the gap between them is why chunking beats a log.
fn bench_append(c: &mut Criterion) {
	let mut g = group(c, "append");
	let line = b"{\"kind\":\"turn\",\"n\":1}\n";

	g.bench_function("new_file", |b| {
		b.iter_batched(
			|| mem_store(MEM_BLOCKS),
			|mut store| store.append("/log.jsonl", line, 0o644).expect("append"),
			BatchSize::PerIteration,
		);
	});

	for existing in [4 * KB, 64 * KB] {
		g.bench_with_input(
			BenchmarkId::new("existing_file", existing),
			&existing,
			|b, &existing| {
				b.iter_batched(
					|| {
						let mut store = mem_store(MEM_BLOCKS);
						store
							.write_new_file("/log.jsonl", &vec![b'z'; existing], 0o644)
							.expect("fixture");
						store
					},
					|mut store| store.append("/log.jsonl", line, 0o644).expect("append"),
					BatchSize::PerIteration,
				);
			},
		);
	}
	g.finish();
}

// endregion: --- Creating

// region:    --- Reading

/// The read half. None of these grow the volume, so one store serves every
/// iteration and the measurement is clean.
fn bench_read(c: &mut Criterion) {
	let mut g = group(c, "read");
	let store = filled_mem_store(
		MEM_BLOCKS,
		&[
			("/notes/small.md", &[b'a'; 12]),
			("/notes/medium.bin", &[b'b'; 4 * KB]),
			("/notes/large.bin", &[b'c'; 64 * KB]),
		],
	);

	g.bench_function("read_file_12b", |b| {
		b.iter(|| black_box(store.read_file("/notes/small.md").expect("read")));
	});
	g.bench_function("read_at_64b", |b| {
		b.iter(|| black_box(store.read_at("/notes/large.bin", 0, 64).expect("read_at")));
	});
	g.bench_function("read_dir", |b| {
		b.iter(|| black_box(store.read_dir("/notes").expect("read_dir")));
	});
	g.bench_function("stat", |b| {
		b.iter(|| black_box(store.stat("/notes/large.bin").expect("stat")));
	});

	for (name, path, size) in [
		("read_file_4k", "/notes/medium.bin", 4 * KB),
		("read_file_64k", "/notes/large.bin", 64 * KB),
	] {
		g.throughput(Throughput::Bytes(size as u64));
		g.bench_function(name, |b| {
			b.iter(|| black_box(store.read_file(path).expect("read")));
		});
	}

	g.finish();
}

/// The space queries, which a relay calls at every session boundary.
fn bench_space(c: &mut Criterion) {
	let mut g = group(c, "space");
	let store = filled_mem_store(MEM_BLOCKS, &[("/notes/a.md", b"x")]);
	g.bench_function("free_bytes", |b| {
		b.iter(|| black_box(store.free_bytes()));
	});
	g.bench_function("total_free_bytes", |b| {
		b.iter(|| black_box(store.total_free_bytes()));
	});
	g.finish();
}

// endregion: --- Reading

// region:    --- Mutating

/// `remove` and `rename` — the two mutations that are not a write.
fn bench_mutate(c: &mut Criterion) {
	let mut g = group(c, "mutate");

	g.bench_function("remove", |b| {
		b.iter_batched(
			|| {
				let mut store = mem_store(MEM_BLOCKS);
				store.write_new_file("/gone.md", b"x", 0o644).expect("fixture");
				store
			},
			|mut store| store.remove("/gone.md").expect("remove"),
			BatchSize::PerIteration,
		);
	});

	g.bench_function("rename", |b| {
		b.iter_batched(
			|| {
				let mut store = mem_store(MEM_BLOCKS);
				store.write_new_file("/a.md", b"x", 0o644).expect("fixture");
				store
			},
			|mut store| store.rename("/a.md", "/b.md").expect("rename"),
			BatchSize::PerIteration,
		);
	});

	g.finish();
}

// endregion: --- Mutating

// region:    --- Walking / rebuilding

/// `walk` — the whole tree, which is the first half of a compaction.
fn bench_walk(c: &mut Criterion) {
	let mut g = group(c, "walk");
	for n in [10usize, 100] {
		let mut store = mem_store(4096);
		store.mkdir("/notes", 0o755).expect("mkdir");
		for i in 0..n {
			store
				.write_new_file(&format!("/notes/f{i:04}.md"), b"x", 0o644)
				.expect("fixture");
		}
		g.bench_with_input(BenchmarkId::from_parameter(n), &store, |b, store| {
			b.iter(|| black_box(store.walk().expect("walk")));
		});
	}
	g.finish();
}

/// [`rebuild_into`] — the compaction pass. This answers "what does it cost to
/// get the space back", and is the number to watch when a volume has grown
/// mostly dead.
fn bench_rebuild(c: &mut Criterion) {
	let mut g = group(c, "rebuild");
	for n in [10usize, 100] {
		let mut src = mem_store(4096);
		src.mkdir("/notes", 0o755).expect("mkdir");
		for i in 0..n {
			src.write_new_file(&format!("/notes/f{i:04}.md"), b"fixture\n", 0o644)
				.expect("fixture");
		}
		let src = black_box(src);
		g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
			b.iter_batched(
				|| mem_store(4096),
				|mut dst| black_box(rebuild_into(&src, &mut dst).expect("rebuild")),
				BatchSize::PerIteration,
			);
		});
	}
	g.finish();
}

// endregion: --- Walking / rebuilding

// region:    --- Scan cost

/// How many times `State.json` is replaced before the churned mount is
/// measured. Each replacement leaves its predecessors on the device — CoW frees
/// nothing — so this is the dead-object population a long campaign builds.
const CHURN: u32 = 1000;

/// Mounting a volume that has been written for a long time — the shape a real
/// campaign image is in.
///
/// Paired with a *fresh* volume of exactly the same size, so the difference
/// between the two numbers is the cost of finding the live root among the dead
/// blocks, and not the volume's geometry. `DiskObjectStore::scan` answers a
/// miss in its bounded window by building an id→block index from *every block
/// in the volume*; if these two numbers track the volume size rather than the
/// live content, that is the amplification to fix.
fn bench_mount_churned(c: &mut Criterion) {
	let mut g = group(c, "mount_churned");
	const BLOCKS: u64 = 32768; // 128 MiB, the size a long campaign image reaches
	let size = BLOCKS * BLOCK_SIZE as u64;

	let (store, fresh_path) = big_file_store("fresh", BLOCKS);
	drop(store);
	g.bench_function("fresh_128mib", |b| {
		b.iter(|| {
			let device = FileBlockDevice::open(&fresh_path, 0, size).expect("open");
			black_box(Store::open(Box::new(device)).expect("mount"))
		});
	});
	let _ = std::fs::remove_file(&fresh_path);

	let (mut store, churned_path) = big_file_store("churned", BLOCKS);
	store.mkdir("/notes", 0o755).expect("mkdir");
	for i in 0..CHURN {
		store
			.write_file("/state.json", format!("{{\"n\":{i}}}").as_bytes(), 0o644)
			.expect("churn");
	}
	drop(store);
	g.bench_function("churned_128mib", |b| {
		b.iter(|| {
			let device = FileBlockDevice::open(&churned_path, 0, size).expect("open");
			black_box(Store::open(Box::new(device)).expect("mount"))
		});
	});

	// Mounting only reads the superblock, the alloc snapshot and the root
	// inode — all of them hint-addressed — so it says nothing about the scan.
	// The scan is paid on the *first lookup whose hint is missing*, which is
	// what a fresh handle has for every object but the root. These three are
	// the same tree a compaction walks, so they are where the read
	// amplification shows up if it is real.
	let reopen = || {
		let device = FileBlockDevice::open(&churned_path, 0, size).expect("open");
		Store::open(Box::new(device)).expect("mount")
	};

	g.bench_function("churned_read_dir_root", |b| {
		b.iter_batched(
			reopen,
			|store| black_box(store.read_dir("/").expect("read_dir")),
			BatchSize::PerIteration,
		);
	});
	g.bench_function("churned_read_file", |b| {
		b.iter_batched(
			reopen,
			|store| black_box(store.read_file("/state.json").expect("read")),
			BatchSize::PerIteration,
		);
	});
	g.bench_function("churned_walk", |b| {
		b.iter_batched(
			reopen,
			|store| black_box(store.walk().expect("walk")),
			BatchSize::PerIteration,
		);
	});

	let _ = std::fs::remove_file(&churned_path);

	g.finish();
}

/// `read_at` on a file much larger than the range asked for. The contract calls
/// it a byte range; if the cost tracks the *file* rather than the *range*, a
/// tail-follower pays for the whole transcript on every poll — and that is the
/// difference between a bounded read and a quadratic one.
fn bench_read_at_scaling(c: &mut Criterion) {
	let mut g = group(c, "read_at");
	for size in [4 * KB, 64 * KB, 1024 * KB] {
		let mut store = mem_store(4096);
		store.write_new_file("/big.bin", &vec![b'c'; size], 0o644).expect("fixture");
		g.bench_with_input(
			BenchmarkId::new("tail_64b_of", size),
			&(store, size),
			|b, (store, size)| {
				b.iter(|| black_box(store.read_at("/big.bin", (*size - 64) as u64, 64).expect("read_at")));
			},
		);
	}
	g.finish();
}

// endregion: --- Scan cost

// region:    --- Workload

/// A campaign-shaped sequence, in one measured routine: make the directories a
/// template makes, write a handover, write a finding, and write one 512 KiB
/// transcript chunk. This is the "every action in the application, in order"
/// number — the one to compare against a real relay's wall-clock.
fn bench_workload(c: &mut Criterion) {
	let mut g = group(c, "workload");
	let chunk = vec![b'q'; 512 * KB];

	g.bench_function("campaign_session", |b| {
		b.iter_batched(
			|| mem_store(4096),
			|mut store| {
				store.mkdir("/notes", 0o755).expect("mkdir notes");
				store.mkdir("/findings", 0o755).expect("mkdir findings");
				store.mkdir("/evidence", 0o755).expect("mkdir evidence");
				store
					.write_new_file(
						"/notes/HANDOVER-0001.md",
						b"# Handover\n\nWhat was tried, what survived.\n",
						0o644,
					)
					.expect("handover");
				store
					.write_new_file("/findings/FIND-0001.md", b"# Finding\n\nEvidence.\n", 0o644)
					.expect("finding");
				store
					.write_new_file("/evidence/transcript-000000.jsonl", &chunk, 0o644)
					.expect("chunk");
				black_box(())
			},
			BatchSize::PerIteration,
		);
	});

	g.finish();
}

// endregion: --- Workload

// region:    --- File-backed

/// The same actions over a real file, where every commit costs two `fsync`s.
/// These are the numbers an application actually pays at run time; the
/// in-memory groups above are the protocol cost with the durability removed.
fn bench_file_backed(c: &mut Criterion) {
	let mut g = group(c, "file");

	g.bench_function("format", |b| {
		b.iter_batched(
			|| temp_image("format"),
			|path| {
				let device = FileBlockDevice::create(&path, FILE_BLOCKS * BLOCK_SIZE as u64).expect("create");
				let store = Store::format(Box::new(device), FILE_BLOCKS).expect("format");
				retire(store, &path);
			},
			BatchSize::PerIteration,
		);
	});

	g.bench_function("write_new_file_12b", |b| {
		b.iter_batched(
			|| file_store("write"),
			|(mut store, path)| {
				store.write_new_file("/a.md", b"handover\n", 0o644).expect("write");
				retire(store, &path);
			},
			BatchSize::PerIteration,
		);
	});

	g.throughput(Throughput::Bytes((4 * KB) as u64));
	g.bench_function("read_file_4k", |b| {
		let size = FILE_BLOCKS * BLOCK_SIZE as u64;
		b.iter_batched(
			|| {
				let (mut store, path) = file_store("read");
				store.write_new_file("/a.bin", &vec![b'r'; 4 * KB], 0o644).expect("fixture");
				drop(store);
				(path, size)
			},
			|(path, size)| {
				let device = FileBlockDevice::open(&path, 0, size).expect("open");
				let store = Store::open(Box::new(device)).expect("mount");
				let out = black_box(store.read_file("/a.bin").expect("read"));
				drop(store);
				let _ = std::fs::remove_file(&path);
				out
			},
			BatchSize::PerIteration,
		);
	});

	g.finish();
}

// endregion: --- File-backed

criterion_group!(
	benches,
	bench_mkfs,
	bench_mount,
	bench_mount_file,
	bench_create,
	bench_replace,
	bench_append,
	bench_read,
	bench_space,
	bench_mutate,
	bench_walk,
	bench_rebuild,
	bench_mount_churned,
	bench_read_at_scaling,
	bench_workload,
	bench_file_backed,
);
criterion_main!(benches);
