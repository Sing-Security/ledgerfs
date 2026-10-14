fn main() {
	// Placeholder — the harness proper lands in the comparison-harness stage.
	// Kept minimal so the workspace resolves while the crate below is built out.
	let _ = redb::Database::create("/tmp/ledgerfs-bench-probe.redb");
	let _ = rusqlite::Connection::open_in_memory();
	println!("ledgerfs-benchmarks: placeholder");
}
