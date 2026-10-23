//! Values that are not text: the store is a byte container, and a capture
//! workload's first question is whether its own type survives it.
//!
//! A volume has no schema, no column types and no parser on the write or read
//! path — an `f32` is four bytes the caller chose an encoding for, exactly like
//! a JSON line is bytes. These tests pin the consequence an application needs
//! to be able to rely on: every bit comes back, including the bit patterns no
//! numeric library would round-trip through text (`-0.0`, a NaN with a payload,
//! infinities, the smallest subnormal), and it survives the staged and deferred
//! tiers the capture path uses.

mod common;

use common::{BLOCKS, Result, bytes, expect_err, is_no_such_file, open_bytes, watched};
use ledgerfs_host::MemBlockDevice;

// region:    --- Fixtures

/// The values a text round-trip is most likely to lose, plus a few ordinary
/// ones so the fixture is not only edge cases.
fn edge_samples() -> Vec<f32> {
	vec![
		0.0,
		-0.0,
		1.0,
		-1.0,
		core::f32::consts::PI,
		-core::f32::consts::E,
		f32::MIN,
		f32::MAX,
		f32::MIN_POSITIVE,
		f32::from_bits(1),           // smallest positive subnormal
		f32::from_bits(0x8000_0001), // smallest negative subnormal
		f32::INFINITY,
		f32::NEG_INFINITY,
		f32::from_bits(0x7fc0_1234), // a NaN with a payload, not the canonical one
	]
}

/// The on-volume form the application chose: little-endian, four bytes each.
///
/// The store never sees this as numbers — this is what makes the encoding a
/// caller decision rather than a format one.
fn encode_le(samples: &[f32]) -> Vec<u8> {
	let mut out = Vec::with_capacity(samples.len() * 4);
	for sample in samples {
		out.extend_from_slice(&sample.to_le_bytes());
	}
	out
}

/// Decode bytes the store returned, asserting the length is a whole number of
/// samples before converting.
fn decode_le(bytes: &[u8]) -> Result<Vec<f32>> {
	let (chunks, remainder) = bytes.as_chunks::<4>();
	if !remainder.is_empty() {
		return Err(format!("{} bytes is not a whole number of f32 values", bytes.len()).into());
	}
	Ok(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Compare by bit pattern, not by value: `NaN != NaN` would make a value
/// comparison of this fixture fail whether or not the store was correct, and
/// `-0.0 == 0.0` would let a lost sign pass.
fn bits(samples: &[f32]) -> Vec<u32> {
	samples.iter().map(|s| s.to_bits()).collect()
}

// endregion: --- Fixtures

// region:    --- One write, one read

#[test]
fn test_f32_values_round_trip_bit_exactly() -> Result<()> {
	// -- Setup & Fixtures
	let mut store = common::store(BLOCKS)?;
	let samples = edge_samples();
	let encoded = encode_le(&samples);

	// -- Exec
	store.write_new_file("/samples.f32", &encoded, 0o644)?;
	let read_back = store.read_file("/samples.f32")?;

	// -- Check: the bytes are the bytes, and every bit survived
	assert_eq!(read_back, encoded, "an f32 array must come back byte for byte");
	assert_eq!(bits(&decode_le(&read_back)?), bits(&samples));

	// -- Exec: a byte range, which is how a graph or a window reads one sample
	let mut window = Vec::new();
	for sample in &samples[3..7] {
		window.extend_from_slice(&sample.to_le_bytes());
	}
	let read_window = store.read_at("/samples.f32", 3 * 4, 4 * 4)?;

	// -- Check
	assert_eq!(read_window, window, "read_at must be a byte window, not a text line");
	assert_eq!(bits(&decode_le(&read_window)?), bits(&samples[3..7]));

	Ok(())
}

// endregion: --- One write, one read

// region:    --- The capture path

#[test]
fn test_f32_capture_survives_its_deferred_seal() -> Result<()> {
	// -- Setup & Fixtures: three batches of 240 samples (960 B), the shape the
	// benchmarks measure
	let (mut store, watch) = watched(BLOCKS)?;
	store.mkdir("/telemetry", 0o755)?;
	let start = store.commit_seq();

	let edge = edge_samples();
	let batches: Vec<Vec<f32>> = (0..3)
		.map(|b| {
			(0..240 - edge.len())
				.map(|i| (b as f32) + (i as f32) * 0.5)
				.chain(edge.iter().copied())
				.collect()
		})
		.collect();
	let encoded_batches: Vec<Vec<u8>> = batches.iter().map(|b| encode_le(b)).collect();
	let whole: Vec<u8> = encoded_batches.iter().flatten().copied().collect();

	// -- Exec: stage every batch, read the staged tier, then seal deferred
	store.begin_staging(0)?;
	for batch in &encoded_batches {
		store.staged_append("/telemetry/samples.f32", batch, 0o644)?;
	}

	// -- Check: the owning handle reads its own writes; the committed tier does
	// not exist yet
	assert_eq!(store.read_file("/telemetry/samples.f32")?, whole);
	assert!(is_no_such_file(
		&expect_err(
			"a staged file is not committed",
			store.read_file_committed("/telemetry/samples.f32")
		),
		"/telemetry/samples.f32"
	));

	// -- Exec: a deferred seal writes the data and withholds the superblock
	store.seal_staged_deferred()?;
	assert!(store.has_pending_sync());
	assert_eq!(store.commit_seq(), start + 1, "the live root advanced");

	// -- Check: a crash at this instant still mounts the old tree
	let crashed = open_bytes(bytes(&watch))?;
	assert_eq!(crashed.commit_seq(), start);
	assert!(
		is_no_such_file(
			&expect_err(
				"the deferred seal must not be published yet",
				crashed.read_file("/telemetry/samples.f32")
			),
			"/telemetry/samples.f32"
		),
		"a deferred seal must not appear in a mount of the volume's bytes"
	);

	// -- Exec: the durable point
	store.sync()?;
	let reopened = open_bytes(bytes(&watch))?;

	// -- Check: every f32 value, bit for bit, after a remount
	assert_eq!(reopened.commit_seq(), start + 1);
	let stored = reopened.read_file("/telemetry/samples.f32")?;
	assert_eq!(stored, whole);
	let expected: Vec<f32> = batches.iter().flatten().copied().collect();
	assert_eq!(bits(&decode_le(&stored)?), bits(&expected));

	Ok(())
}

// endregion: --- The capture path

// region:    --- The device is not asked to interpret anything

/// A byte-for-byte comparison after a raw device round-trip, to make the point
/// that no layer between the caller and the medium touched the payload: the
/// same bytes the caller handed `write_new_file` are in the volume's image.
#[test]
fn test_f32_payload_is_not_rewritten_on_the_way_to_the_medium() -> Result<()> {
	// -- Setup & Fixtures
	let (mut store, watch) = watched(BLOCKS)?;
	let samples = edge_samples();
	let encoded = encode_le(&samples);

	// -- Exec
	store.write_new_file("/raw.f32", &encoded, 0o644)?;
	let image = bytes(&watch);

	// -- Check: the payload appears verbatim in the volume's bytes, in order
	assert!(
		image.windows(encoded.len()).any(|w| w == encoded),
		"the exact payload must be present in the volume image"
	);

	// -- Check: and it is still exact after a mount of those bytes
	let mounted = MemBlockDevice::from_bytes(image)?;
	let reopened = ledgerfs_host::Store::open(Box::new(mounted))?;
	assert_eq!(reopened.read_file("/raw.f32")?, encoded);

	Ok(())
}

// endregion: --- The device is not asked to interpret anything
