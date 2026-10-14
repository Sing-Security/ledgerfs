//! Simple example demonstrating LedgerFS APIs
//!
//! This example shows basic usage of:
//! - ObjectRecord encoding/decoding
//! - Superblock creation and encoding
//! - Intent log record creation
//! - Mount selection

use crucible_ledgerfs_kernel::{
	FsMode,
	intent_log::{IntentLog, IntentLogRecord, IntentRecordType, parse_ring},
	compression::CompressionAlgorithm,
	record::{ObjectRecordReader, ObjectRecordWriter, ObjectType},
	superblock::{RlrEntry, Superblock, mount_select},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	println!("LedgerFS example\n");

	// Example 1: ObjectRecord
	println!("=== Example 1: ObjectRecord ===");
	let data = b"Hello, LedgerFS!";
	let mut buf = vec![0u8; 1024];

	let written = ObjectRecordWriter::write(
		&mut buf,
		ObjectType::Data,
		CompressionAlgorithm::None,
		0, // no encryption
		data,
	)?;
	println!("Written {} bytes", written);

	let (header, payload) = ObjectRecordReader::read(&buf[..written])?;
	println!("Object type: {:?}", header.object_type);
	println!("Payload: {}", String::from_utf8_lossy(&payload));
	println!("Object ID: {:02x?}", &header.object_id[..8]);

	// Example 2: Superblock
	println!("\n=== Example 2: Superblock ===");
	let mut sb = Superblock::new(1_000_000);

	// Add an RLR entry
	let root_id = [42u8; 32];
	let entry = RlrEntry::new(root_id, 1, 1234567890, 0);
	sb.add_rlr_entry(entry);

	// Update active root
	sb.active_root_id = root_id;
	sb.active_commit_seq = 1;

	// Encode to 4KB block
	let mut sb_buf = vec![0u8; 4096];
	sb.encode(&mut sb_buf)?;
	println!("Superblock encoded to 4096 bytes");

	// Decode and verify
	let decoded_sb = Superblock::decode(&sb_buf)?;
	println!("Active commit_seq: {}", decoded_sb.active_commit_seq);
	println!("RLR head: {}", decoded_sb.rlr_head);

	// Example 3: Mount Selection
	println!("\n=== Example 3: Mount Selection ===");
	let mut sb1 = Superblock::new(1_000_000);
	sb1.active_commit_seq = 100;
	sb1.active_root_id = [1u8; 32];

	let mut sb2 = Superblock::new(1_000_000);
	sb2.active_commit_seq = 100;
	sb2.active_root_id = [2u8; 32]; // Different root - split brain!

	let superblocks = [Some(sb1), Some(sb2), None, None];
	let selection = mount_select(&superblocks)?;

	println!("Selected commit_seq: {}", selection.selected_root.commit_seq);
	println!("Mode: {:?}", selection.mode);

	match selection.mode {
		FsMode::ReadWrite => println!("Mounted read-write"),
		FsMode::ReadOnlySafe => println!("Mounted read-only (split-brain detected!)"),
	}

	// Example 4: Intent Log
	println!("\n=== Example 4: Intent Log ===");
	let mut ring = vec![0u8; 4096];
	let log = IntentLog::new(4096);

	// Create and write a PREPARE_ROOT record
	let mut payload = vec![0u8; 8];
	payload[0..8].copy_from_slice(&101u64.to_le_bytes()); // commit_seq

	let record = IntentLogRecord::new(
		1,  // seq
		42, // tx_id
		IntentRecordType::PrepareRoot,
		payload,
	);

	log.write_record(&mut ring, 0, &record)?;
	println!("Intent log record written");

	// Parse ring
	let records = parse_ring(&ring, 4096)?;
	println!("Found {} records in intent log", records.len());

	if let Some(r) = records.first() {
		println!("Record seq: {}, tx_id: {}, type: {:?}", r.seq, r.tx_id, r.record_type);
	}

	println!("\n✓ All examples completed successfully!");
	Ok(())
}
