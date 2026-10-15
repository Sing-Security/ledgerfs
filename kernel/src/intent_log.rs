//! Intent Log with O(N) ring parsing
//!
//! Per §16 and §17 of FORMAT.md:
//! - Intent log is a circular ring buffer
//! - Records transaction lifecycle events
//! - O(N) parsing with seq-window selection
//! - Duplicate resolution and seq collision detection

use crate::{
	BLAKE3_HASH_SIZE, Error, INTENT_LOG_MAGIC, MIN_RECORD_LEN, Result,
	encoding::{blake3_hash, read_u16, read_u32, read_u64, verify_hash, write_u16, write_u32, write_u64},
	nt_write::{nt_write_payload, sfence},
};
use alloc::{collections::BTreeMap, vec, vec::Vec};

/// Intent record types (§16.2)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum IntentRecordType {
	Begin = 0x01,
	AddObject = 0x02,
	PrepareRoot = 0x03,
	CommitRoot = 0x04,
	Abort = 0x05,
}

impl IntentRecordType {
	pub fn from_u16(val: u16) -> Result<Self> {
		match val {
			0x01 => Ok(Self::Begin),
			0x02 => Ok(Self::AddObject),
			0x03 => Ok(Self::PrepareRoot),
			0x04 => Ok(Self::CommitRoot),
			0x05 => Ok(Self::Abort),
			_ => Err(Error::InvalidIntentRecordType { type_id: val }),
		}
	}

	pub fn to_u16(self) -> u16 {
		self as u16
	}
}

/// Intent log record (§16.1)
#[derive(Debug, Clone)]
pub struct IntentLogRecord {
	pub magic: u64,
	pub seq: u64,
	pub tx_id: u64,
	pub record_type: IntentRecordType,
	pub record_len: u32,
	pub payload: Vec<u8>,
	pub record_hash: [u8; BLAKE3_HASH_SIZE],
}

impl IntentLogRecord {
	/// Minimum header size (excluding payload and hash)
	pub const HEADER_SIZE: usize = 8 + 8 + 8 + 2 + 4; // 30 bytes
	pub const MIN_SIZE: usize = Self::HEADER_SIZE + BLAKE3_HASH_SIZE; // 62 bytes (aligned up to 64)

	/// Create a new intent log record
	pub fn new(seq: u64, tx_id: u64, record_type: IntentRecordType, payload: Vec<u8>) -> Self {
		let record_len = (Self::HEADER_SIZE + payload.len() + BLAKE3_HASH_SIZE) as u32;
		let mut record = Self {
			magic: INTENT_LOG_MAGIC,
			seq,
			tx_id,
			record_type,
			record_len,
			payload,
			record_hash: [0u8; BLAKE3_HASH_SIZE],
		};
		record.compute_hash();
		record
	}

	/// Compute and update record hash
	pub fn compute_hash(&mut self) {
		let mut data = Vec::new();
		data.extend_from_slice(&self.magic.to_le_bytes());
		data.extend_from_slice(&self.seq.to_le_bytes());
		data.extend_from_slice(&self.tx_id.to_le_bytes());
		data.extend_from_slice(&self.record_type.to_u16().to_le_bytes());
		data.extend_from_slice(&self.record_len.to_le_bytes());
		data.extend_from_slice(&self.payload);

		self.record_hash = blake3_hash(&data);
	}

	/// Verify record hash
	pub fn verify(&self) -> Result<()> {
		let mut data = Vec::new();
		data.extend_from_slice(&self.magic.to_le_bytes());
		data.extend_from_slice(&self.seq.to_le_bytes());
		data.extend_from_slice(&self.tx_id.to_le_bytes());
		data.extend_from_slice(&self.record_type.to_u16().to_le_bytes());
		data.extend_from_slice(&self.record_len.to_le_bytes());
		data.extend_from_slice(&self.payload);

		verify_hash(&data, &self.record_hash)
	}

	/// Decode from buffer (at specific offset in ring)
	pub fn decode(buf: &[u8]) -> Result<Self> {
		if buf.len() < Self::MIN_SIZE {
			return Err(Error::BufferTooSmall {
				required: Self::MIN_SIZE,
				available: buf.len(),
			});
		}

		// Read header
		let magic = read_u64(&buf[0..])?;
		if magic != INTENT_LOG_MAGIC {
			return Err(Error::InvalidMagic {
				expected: INTENT_LOG_MAGIC,
				found: magic,
			});
		}

		let seq = read_u64(&buf[8..])?;
		let tx_id = read_u64(&buf[16..])?;
		let record_type_raw = read_u16(&buf[24..])?;
		let record_type = IntentRecordType::from_u16(record_type_raw)?;
		let record_len = read_u32(&buf[26..])?;

		// Validate record_len
		if record_len < MIN_RECORD_LEN || record_len > buf.len() as u32 {
			return Err(Error::InvalidRecordLength { len: record_len });
		}

		// Read payload
		let payload_len = record_len as usize - Self::HEADER_SIZE - BLAKE3_HASH_SIZE;
		let payload = buf[Self::HEADER_SIZE..Self::HEADER_SIZE + payload_len].to_vec();

		// Read hash
		let hash_offset = Self::HEADER_SIZE + payload_len;
		let mut record_hash = [0u8; BLAKE3_HASH_SIZE];
		record_hash.copy_from_slice(&buf[hash_offset..hash_offset + BLAKE3_HASH_SIZE]);

		let record = Self {
			magic,
			seq,
			tx_id,
			record_type,
			record_len,
			payload,
			record_hash,
		};

		record.verify()?;
		Ok(record)
	}

	/// Encode to buffer
	pub fn encode(&self, buf: &mut [u8]) -> Result<usize> {
		let total_len = self.record_len as usize;

		if buf.len() < total_len {
			return Err(Error::BufferTooSmall {
				required: total_len,
				available: buf.len(),
			});
		}

		// Write header
		write_u64(&mut buf[0..], self.magic)?;
		write_u64(&mut buf[8..], self.seq)?;
		write_u64(&mut buf[16..], self.tx_id)?;
		write_u16(&mut buf[24..], self.record_type.to_u16())?;
		write_u32(&mut buf[26..], self.record_len)?;

		// Write payload
		buf[Self::HEADER_SIZE..Self::HEADER_SIZE + self.payload.len()].copy_from_slice(&self.payload);

		// Write hash
		let hash_offset = Self::HEADER_SIZE + self.payload.len();
		buf[hash_offset..hash_offset + BLAKE3_HASH_SIZE].copy_from_slice(&self.record_hash);

		Ok(total_len)
	}
}

/// Transaction view (state from intent log)
#[derive(Debug, Clone)]
pub struct TxView {
	pub tx_id: u64,
	pub begin_seq: Option<u64>,
	pub prepare_root_seq: Option<u64>,
	pub prepare_root_commit_seq: Option<u64>,
	pub commit_root_seq: Option<u64>,
	pub abort_seq: Option<u64>,
}

impl TxView {
	pub fn new(tx_id: u64) -> Self {
		Self {
			tx_id,
			begin_seq: None,
			prepare_root_seq: None,
			prepare_root_commit_seq: None,
			commit_root_seq: None,
			abort_seq: None,
		}
	}

	/// Check if transaction is committed
	pub fn is_committed(&self) -> bool {
		self.commit_root_seq.is_some()
	}

	/// Check if transaction is aborted
	pub fn is_aborted(&self) -> bool {
		self.abort_seq.is_some()
	}
}

/// Replay decision for mount
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayDecision {
	NoReplay,
	ReplayTo { commit_seq: u64 },
}

/// Intent log structure
pub struct IntentLog {
	ring_size: usize,
}

impl IntentLog {
	/// Create new intent log
	pub fn new(ring_size: usize) -> Self {
		Self { ring_size }
	}

	/// Write record to ring buffer (with wrapping)
	pub fn write_record(&self, ring: &mut [u8], offset: usize, record: &IntentLogRecord) -> Result<usize> {
		let record_len = record.record_len as usize;

		// Align to 8 bytes
		let aligned_len = (record_len + 7) & !7;

		if aligned_len > self.ring_size {
			return Err(Error::InvalidRecordLength {
				len: aligned_len as u32,
			});
		}

		// Handle wrapping
		if offset + aligned_len <= self.ring_size {
			// No wrap needed — encode to temp first, then NT-write to ring.
			let mut temp = vec![0u8; aligned_len];
			record.encode(&mut temp[..record_len])?;
			// SAFETY: ring[offset..] is valid for aligned_len bytes (checked above).
			// temp.as_ptr() is valid for aligned_len bytes. Non-overlapping distinct
			// allocations. sfence follows to order NT stores before storage barrier (§15.3).
			unsafe {
				nt_write_payload(ring[offset..].as_mut_ptr(), temp.as_ptr(), aligned_len);
			}
			sfence(); // MUST precede any subsequent storage barrier (§15.3)
		} else {
			// Need to wrap — encode to temp, then NT-write in two parts.
			let first_part = self.ring_size - offset;
			let second_part = aligned_len - first_part;

			let mut temp = vec![0u8; aligned_len];
			record.encode(&mut temp)?;

			// SAFETY: ring[offset..] valid for first_part bytes; ring[..second_part]
			// valid for second_part bytes. temp.as_ptr() covers both ranges. No aliasing.
			// sfence follows all NT writes (§15.3).
			unsafe {
				nt_write_payload(ring[offset..].as_mut_ptr(), temp.as_ptr(), first_part);
				nt_write_payload(ring.as_mut_ptr(), temp[first_part..].as_ptr(), second_part);
			}
			sfence(); // MUST precede any subsequent storage barrier (§15.3)
		}

		Ok(aligned_len)
	}
}

/// O(N) ring parser (§17)
pub fn parse_ring(ring: &[u8], ring_size: usize) -> Result<Vec<IntentLogRecord>> {
	let mut valid_records = Vec::new();

	// Step 1: Scan ring in 8-byte increments (§17.1)
	let mut offset = 0;
	while offset + 8 <= ring_size {
		// Check for magic at this offset
		if let Ok(magic) = read_u64(&ring[offset..])
			&& magic == INTENT_LOG_MAGIC
		{
			// Potential record - need to read record_len
			// Record header minimum is 30 bytes (before hash)
			const HEADER_BEFORE_LEN: usize = 26; // magic(8) + seq(8) + tx_id(8) + type(2)

			// Check if we can read record_len (need at least 30 bytes for header)
			let can_read_len = if offset + HEADER_BEFORE_LEN + 4 <= ring_size {
				// Can read directly
				true
			} else if offset + HEADER_BEFORE_LEN < ring_size {
				// Header wraps, need to reassemble to read record_len
				true
			} else {
				// Not enough bytes even with wrapping
				false
			};

			if can_read_len {
				// Read record_len (handles wrap-around if needed)
				let record_len = if offset + HEADER_BEFORE_LEN + 4 <= ring_size {
					// Can read directly
					read_u32(&ring[offset + HEADER_BEFORE_LEN..])?
				} else {
					// Need to wrap to read record_len
					let remaining = ring_size - (offset + HEADER_BEFORE_LEN);
					let mut len_bytes = [0u8; 4];
					len_bytes[..remaining].copy_from_slice(&ring[offset + HEADER_BEFORE_LEN..]);
					len_bytes[remaining..].copy_from_slice(&ring[..4 - remaining]);
					read_u32(&len_bytes)?
				};

				if record_len >= MIN_RECORD_LEN && record_len <= ring_size as u32 {
					let record_end = offset + record_len as usize;

					// Check if record wraps around ring boundary (§17.3)
					if record_end > ring_size {
						// Need to reassemble wrapped record
						let first_part_len = ring_size - offset;
						let second_part_len = record_end - ring_size;

						// Create contiguous buffer
						let mut temp_buf = vec![0u8; record_len as usize];
						temp_buf[..first_part_len].copy_from_slice(&ring[offset..ring_size]);
						temp_buf[first_part_len..].copy_from_slice(&ring[..second_part_len]);

						// Try to decode from reassembled buffer
						match IntentLogRecord::decode(&temp_buf) {
							Ok(record) => {
								valid_records.push(record);
							}
							Err(_) => {
								// Invalid record, continue scanning
							}
						}
					} else {
						// No wrap-around, decode directly
						match IntentLogRecord::decode(&ring[offset..]) {
							Ok(record) => {
								valid_records.push(record);
							}
							Err(_) => {
								// Invalid record, continue scanning
							}
						}
					}
				}
			}
		}

		offset += 8; // 8-byte alignment scan
	}

	// Step 2: Select authoritative window (§17.5)
	if valid_records.is_empty() {
		return Ok(Vec::new());
	}

	let max_seq = valid_records.iter().map(|r| r.seq).max().unwrap();
	let max_records = ring_size / MIN_RECORD_LEN as usize;
	let window_start = max_seq.saturating_sub((max_records - 1) as u64);

	// Filter to window
	let mut window_records: Vec<_> = valid_records
		.into_iter()
		.filter(|r| r.seq >= window_start && r.seq <= max_seq)
		.collect();

	// If more than max_records, keep highest seqs
	if window_records.len() > max_records {
		window_records.sort_by_key(|r| core::cmp::Reverse(r.seq));
		window_records.truncate(max_records);
	}

	// Step 3: Check for seq collision (§17.7)
	let mut seq_map: BTreeMap<u64, Vec<[u8; BLAKE3_HASH_SIZE]>> = BTreeMap::new();
	for record in &window_records {
		seq_map.entry(record.seq).or_default().push(record.record_hash);
	}

	for (seq, hashes) in seq_map {
		if hashes.len() > 1 {
			// Check if all hashes are the same
			let first = &hashes[0];
			if !hashes.iter().all(|h| h == first) {
				return Err(Error::IntentLogSeqCollision { seq });
			}
		}
	}

	Ok(window_records)
}

/// Build transaction views from records (§17.6 duplicate resolution)
pub fn build_tx_views(records: &[IntentLogRecord]) -> BTreeMap<u64, TxView> {
	let mut tx_views: BTreeMap<u64, TxView> = BTreeMap::new();

	// Sort by seq (increasing order)
	let mut sorted_records = records.to_vec();
	sorted_records.sort_by_key(|r| r.seq);

	// Process in seq order - last record wins for each type
	for record in sorted_records {
		let view = tx_views.entry(record.tx_id).or_insert_with(|| TxView::new(record.tx_id));

		match record.record_type {
			IntentRecordType::Begin => {
				view.begin_seq = Some(record.seq);
			}
			IntentRecordType::PrepareRoot => {
				view.prepare_root_seq = Some(record.seq);
				// Extract commit_seq from payload if available
				if record.payload.len() >= 8
					&& let Ok(commit_seq) = read_u64(&record.payload)
				{
					view.prepare_root_commit_seq = Some(commit_seq);
				}
			}
			IntentRecordType::CommitRoot => {
				view.commit_root_seq = Some(record.seq);
			}
			IntentRecordType::Abort => {
				view.abort_seq = Some(record.seq);
			}
			_ => {}
		}
	}

	tx_views
}

/// Determine replay decision based on mounted commit_seq and intent log (§18.5)
pub fn determine_replay(mounted_seq: u64, tx_views: &BTreeMap<u64, TxView>) -> Result<ReplayDecision> {
	// Look for a PREPARE_ROOT with commit_seq = mounted_seq + 1
	for view in tx_views.values() {
		if let Some(prepare_commit_seq) = view.prepare_root_commit_seq
			&& prepare_commit_seq == mounted_seq + 1
		{
			// Check base binding matches
			// (Simplified: full base-binding verification is deferred)

			// Check that transaction is not aborted
			if view.is_aborted() {
				continue;
			}

			// Valid replay candidate
			return Ok(ReplayDecision::ReplayTo {
				commit_seq: mounted_seq + 1,
			});
		}
	}

	// No valid replay found
	Ok(ReplayDecision::NoReplay)
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::boxed::Box;

	type TestResult<T> = core::result::Result<T, Box<dyn core::error::Error>>;

	#[test]
	fn test_intent_log_record_type_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let types = [
			IntentRecordType::Begin,
			IntentRecordType::AddObject,
			IntentRecordType::PrepareRoot,
			IntentRecordType::CommitRoot,
			IntentRecordType::Abort,
		];

		// -- Exec & Check
		for record_type in types {
			let val = record_type.to_u16();
			let decoded = IntentRecordType::from_u16(val)?;
			assert_eq!(record_type, decoded);
		}

		Ok(())
	}

	#[test]
	fn test_intent_log_record_roundtrip() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut buf = vec![0u8; 1024];
		let payload = vec![1, 2, 3, 4, 5];
		let record = IntentLogRecord::new(100, 42, IntentRecordType::Begin, payload.clone());

		// -- Exec
		let len = record.encode(&mut buf)?;
		let decoded = IntentLogRecord::decode(&buf[..len])?;

		// -- Check
		assert_eq!(decoded.seq, record.seq);
		assert_eq!(decoded.tx_id, record.tx_id);
		assert_eq!(decoded.record_type, record.record_type);
		assert_eq!(decoded.payload, payload);

		Ok(())
	}

	#[test]
	fn test_intent_log_parse_ring_empty() -> TestResult<()> {
		// -- Setup & Fixtures
		let ring = vec![0u8; 4096];

		// -- Exec
		let records = parse_ring(&ring, 4096)?;

		// -- Check
		assert_eq!(records.len(), 0);

		Ok(())
	}

	#[test]
	fn test_intent_log_parse_ring_single_record() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut ring = vec![0u8; 4096];
		let record = IntentLogRecord::new(1, 42, IntentRecordType::Begin, vec![1, 2, 3]);
		let log = IntentLog::new(4096);

		// -- Exec
		log.write_record(&mut ring, 0, &record)?;
		let parsed = parse_ring(&ring, 4096)?;

		// -- Check
		assert_eq!(parsed.len(), 1);
		assert_eq!(parsed[0].seq, 1);
		assert_eq!(parsed[0].tx_id, 42);

		Ok(())
	}

	#[test]
	fn test_intent_log_seq_collision_detection() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut ring = vec![0u8; 4096];
		let log = IntentLog::new(4096);

		// Write two records with same seq but different data
		let record1 = IntentLogRecord::new(100, 1, IntentRecordType::Begin, vec![1, 2, 3]);
		let record2 = IntentLogRecord::new(100, 2, IntentRecordType::Begin, vec![4, 5, 6]);

		log.write_record(&mut ring, 0, &record1)?;
		log.write_record(&mut ring, 128, &record2)?;

		// -- Exec & Check
		let result = parse_ring(&ring, 4096);
		assert!(matches!(result, Err(Error::IntentLogSeqCollision { seq: 100 })));

		Ok(())
	}

	#[test]
	fn test_intent_log_parse_ring_wraparound() -> TestResult<()> {
		// -- Setup & Fixtures
		let ring_size = 512; // Small ring for easier testing
		let mut ring = vec![0u8; ring_size];
		let log = IntentLog::new(ring_size);

		// Create a record that will wrap around
		// Place it near the end of the ring so it wraps
		let payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]; // 10-byte payload
		let record = IntentLogRecord::new(100, 42, IntentRecordType::PrepareRoot, payload);

		// Calculate where to place the record so it wraps
		// Record length is: 30 (header) + 10 (payload) + 32 (hash) = 72 bytes
		// Place it at offset 480 so it wraps (480 + 72 = 552, which wraps to 40)
		let offset = ring_size - 40;

		// -- Exec
		log.write_record(&mut ring, offset, &record)?;
		let parsed = parse_ring(&ring, ring_size)?;

		// -- Check
		assert_eq!(parsed.len(), 1, "Should parse exactly 1 wrapped record");
		assert_eq!(parsed[0].seq, 100);
		assert_eq!(parsed[0].tx_id, 42);
		assert_eq!(parsed[0].payload.len(), 10);
		assert_eq!(parsed[0].payload, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

		Ok(())
	}

	#[test]
	fn test_intent_log_build_tx_views() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut payload = vec![0u8; 8];
		write_u64(&mut payload, 101)?; // commit_seq for PREPARE_ROOT

		let records = vec![
			IntentLogRecord::new(1, 42, IntentRecordType::Begin, vec![]),
			IntentLogRecord::new(2, 42, IntentRecordType::PrepareRoot, payload),
			IntentLogRecord::new(3, 42, IntentRecordType::CommitRoot, vec![]),
		];

		// -- Exec
		let tx_views = build_tx_views(&records);

		// -- Check
		assert_eq!(tx_views.len(), 1);
		let view = &tx_views[&42];
		assert_eq!(view.begin_seq, Some(1));
		assert_eq!(view.prepare_root_seq, Some(2));
		assert_eq!(view.prepare_root_commit_seq, Some(101));
		assert_eq!(view.commit_root_seq, Some(3));
		assert!(view.is_committed());

		Ok(())
	}

	#[test]
	fn test_intent_log_determine_replay() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut tx_views = BTreeMap::new();
		let mut view = TxView::new(42);
		view.prepare_root_commit_seq = Some(101); // N+1 where N=100
		tx_views.insert(42, view);

		// -- Exec
		let decision = determine_replay(100, &tx_views)?;

		// -- Check
		assert_eq!(decision, ReplayDecision::ReplayTo { commit_seq: 101 });

		Ok(())
	}

	#[test]
	fn test_intent_log_no_replay_when_aborted() -> TestResult<()> {
		// -- Setup & Fixtures
		let mut tx_views = BTreeMap::new();
		let mut view = TxView::new(42);
		view.prepare_root_commit_seq = Some(101);
		view.abort_seq = Some(5); // Aborted
		tx_views.insert(42, view);

		// -- Exec
		let decision = determine_replay(100, &tx_views)?;

		// -- Check
		assert_eq!(decision, ReplayDecision::NoReplay);

		Ok(())
	}
}

// endregion: --- Tests
