//! Mounting a LedgerFS partition: superblock copies, root selection.
//!
//! The whole of §18's mount algorithm already lives in
//! `superblock::mount_select`: this module only does the part it cannot:
//! fetch the four 4 KiB copies off the device and hand them over as decoded
//! `Superblock`s. Deliberately no reimplementation of the selection rules
//! (highest `commit_seq`, split-brain detection, replay decision); a second
//! copy of that logic would be a second thing to get wrong.
//!
//! # Why the copy LBAs are what they are
//!
//! The format places them at blocks `0`, `1`, `N-2`, `N-1`. The trailing pair
//! means the last two blocks of a partition are never object space, which is
//! why `RESERVED_TRAILER_BLOCKS` exists and why the object scanner stops short
//! of the end.

use ledgerfs::object::{ObjectId, ObjectRef, Root, StorageHint};
use ledgerfs::superblock::{CandidateSource, MountSelection, Superblock};
use ledgerfs::{BLOCK_SIZE, NUM_SUPERBLOCK_COPIES};

use crate::device::BlockDevice;
use crate::error::{Error, Result};

// `superblock_copy_lbas` is not exported by the kernel crate, so the formula
// is restated here, and pinned by
// `test_volume_copy_lbas_are_the_leading_and_trailing_pair` against the
// literal array.
pub const fn superblock_copy_lbas(total_blocks: u64) -> [u64; NUM_SUPERBLOCK_COPIES] {
	[0, 1, total_blocks.saturating_sub(2), total_blocks.saturating_sub(1)]
}

/// A mounted volume: the superblock the mount reads from, every copy that
/// decoded, and the selection that chose between them.
pub struct Volume {
	/// The superblock copy `mount_select` chose.
	pub superblock: Superblock,

	/// All copies that decoded, in `superblock_copy_lbas` order.
	pub copies: [Option<Superblock>; NUM_SUPERBLOCK_COPIES],

	/// The selection decision, including which copy won and why.
	pub selection: MountSelection,
}

/// The state of the four superblock copies, as an auditor sees it.
///
/// Every copy is written together by the builders, so a volume whose copies
/// disagree has been through a writer that does not maintain them. That is a
/// durability question rather than a cosmetic one: the copies exist so that a
/// damaged copy is survivable, and a copy that was never updated cannot answer
/// for the one that was.
///
/// See [`Volume::copy_status`], and `lfs-read info`, which prints this.
#[derive(Debug, Clone)]
pub struct CopyStatus {
	/// The copy `mount_select` chose: the one the mount is reading from.
	pub selected: usize,

	/// Physical block of each copy, in `superblock_copy_lbas` order.
	pub lbas: [u64; NUM_SUPERBLOCK_COPIES],

	/// Which copies were readable *and* decoded.
	pub decoded: [bool; NUM_SUPERBLOCK_COPIES],

	/// Which decoded copies name the same root as the selected copy.
	pub agree: [bool; NUM_SUPERBLOCK_COPIES],

	/// Each decoded copy's `active_commit_seq`; `None` where it did not decode.
	pub commit_seqs: [Option<u64>; NUM_SUPERBLOCK_COPIES],
}

impl CopyStatus {
	/// Every copy decoded and names the same root, the state a volume is in
	/// after a writer that maintains all four.
	pub fn is_healthy(&self) -> bool {
		self.decoded.iter().all(|d| *d) && self.agree.iter().all(|a| *a)
	}

	/// How many copies decoded.
	pub fn decoded_count(&self) -> usize {
		self.decoded.iter().filter(|d| **d).count()
	}
}

// region:    --- Mount

impl Volume {
	/// Read and decode the superblocks, then run mount selection.
	pub fn mount(device: &dyn BlockDevice) -> Result<Self> {
		let copies = read_superblock_copies(device)?;

		// `mount_select` takes `&[Option<_>; 4]` and returns the winner among
		// them, or `NoValidSuperblock` when every copy is empty/dead. That one
		// error covers two very different volumes, so tell them apart before
		// reporting it: `mount_select` builds its candidate list only from
		// copies with `active_commit_seq > 0`, so a volume that is entirely
		// intact and simply has nothing committed to it (what the image
		// initialiser leaves behind) lands on the same branch as a destroyed
		// one.
		let selection = match ledgerfs::superblock::mount_select(&copies) {
			Ok(selection) => selection,
			Err(e) => {
				return Err(match uncommitted_total_blocks(&copies) {
					Some(total_blocks) => Error::VolumeUncommitted { total_blocks },
					None => Error::Mount { detail: e.to_string() },
				});
			}
		};

		let selected = selected_copy_index(&selection).ok_or_else(|| Error::Mount {
			detail: "mount selection named a superblock copy that did not decode".into(),
		})?;

		let superblock = copies[selected].clone().ok_or_else(|| Error::Mount {
			detail: "mount selection named a superblock copy that did not decode".into(),
		})?;

		if superblock.total_blocks * BLOCK_SIZE as u64 > device.size_bytes() {
			return Err(Error::Mount {
				detail: format!(
					"superblock claims {} blocks ({} bytes) but the partition is {} bytes",
					superblock.total_blocks,
					superblock.total_blocks * BLOCK_SIZE as u64,
					device.size_bytes()
				),
			});
		}

		Ok(Self {
			superblock,
			copies,
			selection,
		})
	}

	/// Number of 4 KiB blocks in the volume, from the superblock.
	pub fn total_blocks(&self) -> u64 {
		self.superblock.total_blocks
	}

	/// The object reference for the volume's active root.
	///
	/// Read from the live superblock rather than from `selection`: at mount time
	/// they are the same copy, but a writer that has published a newer root
	/// ([`publish_root`](Self::publish_root)) would otherwise leave this
	/// answering with the root the volume had when it was mounted.
	pub fn root_ref(&self) -> ObjectRef {
		let root_id = ObjectId::from_bytes(&self.superblock.active_root_id);
		ObjectRef::new(root_id, StorageHint(self.active_root_hint()))
	}

	/// Physical block of the active root, or `0` when the volume never
	/// recorded one (`ACTIVE_ROOT_HINT_OFFSET` = 0 means "scan for it").
	///
	/// A writer that sets `active_root_id` and `active_commit_seq` but not this
	/// leaves `0` here, and a volume built that way scans for its root on
	/// every mount. It is an accelerator only, and `DiskObjectStore::locate`
	/// verifies any hint before trusting it.
	pub fn active_root_hint(&self) -> u64 {
		self.superblock.active_root_hint
	}

	/// Adopt `root` as the volume's published root, in memory.
	///
	/// The in-memory counterpart of writing the superblock: it sets the active
	/// slot and refreshes the copies, because that is what the on-disk write
	/// does and the two must not be allowed to disagree.
	/// [`copy_status`](Self::copy_status) reads `copies`, so without this a live
	/// store would keep reporting whichever state it mounted at.
	///
	/// This touches no device. The caller must still write
	/// [`superblock`](Self::superblock) back, to **all four** copies; see the
	/// note in this crate's `lib.rs` on what a single-copy writer leaves behind.
	///
	/// `selection` is deliberately left alone: it records how the *mount*
	/// chose, which a later commit does not change.
	pub fn publish_root(&mut self, root: &Root) {
		publish_root_into(&mut self.superblock, root);
		self.refresh_copies();
	}

	/// Make `copies` match `superblock` after a superblock write that did not
	/// move the root, such as a keyslot change.
	///
	/// Separate from [`publish_root`](Self::publish_root) because that one also
	/// rewrites the active root, which a keyslot change must not do: it would
	/// report a root as published that this write did not carry.
	///
	/// Every copy, not just the selected one: a successful write puts this
	/// superblock in all four, so that is what the in-memory view should say.
	pub fn refresh_copies(&mut self) {
		let published = self.superblock.clone();
		self.copies = core::array::from_fn(|_| Some(published.clone()));
	}

	/// Report which superblock copies decoded and whether they agree.
	///
	/// A volume whose initialiser wrote all four copies, and which nothing has
	/// committed to since, is healthy here. A writer that updates the primary
	/// copy only leaves copies 1..3 naming the root the volume had *before*
	/// that commit.
	pub fn copy_status(&self) -> CopyStatus {
		let selected = selected_copy_index(&self.selection).unwrap_or(0);
		let lbas = superblock_copy_lbas(self.superblock.total_blocks);

		// Compare on the two fields that identify what a copy points at. A
		// copy that names the same root at the same sequence is interchangeable
		// for mount purposes; anything else is a copy that has fallen behind.
		let reference = self.copies[selected]
			.as_ref()
			.map(|sb| (sb.active_root_id, sb.active_commit_seq));

		let mut decoded = [false; NUM_SUPERBLOCK_COPIES];
		let mut agree = [false; NUM_SUPERBLOCK_COPIES];
		let mut commit_seqs = [None; NUM_SUPERBLOCK_COPIES];

		for (i, copy) in self.copies.iter().enumerate() {
			let Some(sb) = copy else { continue };
			decoded[i] = true;
			commit_seqs[i] = Some(sb.active_commit_seq);
			agree[i] = reference == Some((sb.active_root_id, sb.active_commit_seq));
		}

		CopyStatus {
			selected,
			lbas,
			decoded,
			agree,
			commit_seqs,
		}
	}
}

// endregion: --- Mount

// region:    --- Support

/// Fetch all four superblock copies. A copy that will not read or decode is
/// `None` rather than a hard error: a dead trailing copy is precisely what
/// the other three exist to survive.
fn read_superblock_copies(device: &dyn BlockDevice) -> Result<[Option<Superblock>; NUM_SUPERBLOCK_COPIES]> {
	// The trailing pair's LBAs need `total_blocks`, which is only known from a
	// decoded superblock. The first two sit at fixed LBAs, so read those first
	// for the geometry, then finish the set.
	let mut first = [0u8; BLOCK_SIZE as usize];
	device.read_block(0, &mut first)?;
	let head = match Superblock::decode(&first) {
		Ok(sb) => sb,
		Err(e0) => {
			device.read_block(1, &mut first)?;
			Superblock::decode(&first).map_err(|e1| Error::SuperblockUnreadable {
				copy_lba: 0,
				detail: format!("copy 0: {e0}; copy 1: {e1}"),
			})?
		}
	};

	let lbas = superblock_copy_lbas(head.total_blocks);

	let mut copies: [Option<Superblock>; NUM_SUPERBLOCK_COPIES] = [None, None, None, None];
	for (i, lba) in lbas.iter().enumerate() {
		let mut buf = [0u8; BLOCK_SIZE as usize];
		if device.read_block(*lba, &mut buf).is_err() {
			continue;
		}
		if let Ok(sb) = Superblock::decode(&buf) {
			copies[i] = Some(sb);
		}
	}

	Ok(copies)
}

/// Set `superblock`'s active slot to `root` and recompute its checksums.
///
/// Shared with [`Store::format`](crate::Store::format), which publishes onto a
/// superblock that no [`Volume`] has been mounted around yet. Keeping one
/// implementation means the on-disk write and the in-memory view cannot
/// disagree about what "published" means.
///
/// `compute_checksums`, not bare field assignments: the active slot's checksum
/// covers what was just written into it, and a stale one makes
/// `Superblock::decode` reject the copy this exists to publish.
pub(crate) fn publish_root_into(superblock: &mut Superblock, root: &Root) {
	superblock
		.active_root_id
		.copy_from_slice(root.file_tree_root.object_id.as_bytes());
	superblock.active_commit_seq = root.commit_seq;
	superblock.active_root_hint = root.file_tree_root.hint.0;
	superblock.compute_checksums();
}

/// `Some(total_blocks)` when every copy that decoded is a valid, checksum-clean
/// superblock with `active_commit_seq == 0`, i.e. the volume is intact and
/// nothing has ever been committed to it.
///
/// This is the image initialiser's post-condition, and it is a format state
/// rather than a fault: there is no root because no `commit` has run yet.
/// `None` means at
/// least one decoded copy *is* committed (so the failure is something else) or
/// nothing decoded at all (damage).
fn uncommitted_total_blocks(copies: &[Option<Superblock>; NUM_SUPERBLOCK_COPIES]) -> Option<u64> {
	let mut total_blocks = None;

	for copy in copies.iter().flatten() {
		if copy.active_commit_seq > 0 || copy.active_root_id != [0u8; 32] {
			return None;
		}
		total_blocks.get_or_insert(copy.total_blocks);
	}

	total_blocks
}

/// Which superblock copy `mount_select` chose.
fn selected_copy_index(selection: &MountSelection) -> Option<usize> {
	match selection.selected_root.source {
		CandidateSource::ActiveSlot { copy } => Some(copy),
		CandidateSource::RlrEntry { copy, .. } => Some(copy),
	}
}

// endregion: --- Support

// region:    --- Tests

#[cfg(test)]
mod tests {
	use super::*;
	use crate::device::MemBlockDevice;

	type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

	#[test]
	fn test_volume_copy_lbas_are_the_leading_and_trailing_pair() -> Result<()> {
		// The copies sit at `[0, 1, N-2, N-1]`. If that ever changes, this is
		// where it should be noticed rather than at a mount that reads four
		// empty blocks.
		let lbas = superblock_copy_lbas(4096);
		assert_eq!(lbas, [0, 1, 4094, 4095]);

		// A partition small enough that the trailing pair would collide with
		// the leading pair must saturate rather than underflow.
		assert_eq!(superblock_copy_lbas(1), [0, 1, 0, 0]);

		Ok(())
	}

	#[test]
	fn test_volume_copy_status_reports_a_lagging_copy() -> Result<()> {
		// A volume whose primary copy names a committed root while the other
		// three still hold the null root the image initialiser wrote, the state
		// the data volume is left in after its first boot (see the crate docs).
		let copies = [
			Some(superblock([0xAB; 32], 1, 4096)),
			Some(superblock([0u8; 32], 0, 4096)),
			Some(superblock([0u8; 32], 0, 4096)),
			Some(superblock([0u8; 32], 0, 4096)),
		];
		let selection = ledgerfs::superblock::mount_select(&copies)?;
		let volume = Volume {
			superblock: copies[0].clone().unwrap(),
			copies,
			selection,
		};

		// -- Exec
		let status = volume.copy_status();

		// -- Check
		assert_eq!(status.selected, 0);
		assert_eq!(status.lbas, [0, 1, 4094, 4095]);
		assert_eq!(status.decoded_count(), 4);
		assert_eq!(status.commit_seqs, [Some(1), Some(0), Some(0), Some(0)]);
		assert!(status.agree[0]);
		assert!(!status.agree[1]);
		assert!(
			!status.is_healthy(),
			"three copies lagging the selected one must not read as healthy"
		);

		Ok(())
	}

	/// §43.1: a volume whose four copies were written by a newer format has no
	/// acceptable copy, so the mount refuses it. The block is checksum-clean,
	/// so the refusal is the version check rather than damage.
	#[test]
	fn test_volume_of_a_newer_format_does_not_mount() -> Result<()> {
		// -- Setup & Fixtures
		const BLOCKS: u64 = 64;
		let device = MemBlockDevice::new(BLOCKS);
		for &lba in &superblock_copy_lbas(BLOCKS) {
			let mut sb = Superblock::new(BLOCKS);
			sb.version = ledgerfs::SUPERBLOCK_VERSION + 1;
			let mut buf = [0u8; BLOCK_SIZE as usize];
			sb.encode(&mut buf)?;
			device.write_block(lba, &buf)?;
		}

		// -- Exec
		let result = read_superblock_copies(&device);

		// -- Check
		match result {
			Err(Error::SuperblockUnreadable { detail, .. }) => {
				assert!(
					detail.contains("format version"),
					"the refusal should name the version, got: {detail}"
				);
			}
			other => panic!("a newer-format volume was read: {other:?}"),
		}

		Ok(())
	}

	/// A mixed set can only come from a writer that stopped partway through the
	/// four copies. The older copies then describe the last complete commit and
	/// still mount (§43.1).
	#[test]
	fn test_volume_with_a_newer_copy_falls_back_to_the_older_ones() -> Result<()> {
		// -- Setup & Fixtures
		const BLOCKS: u64 = 64;
		let device = MemBlockDevice::new(BLOCKS);
		let lbas = superblock_copy_lbas(BLOCKS);

		let mut committed = Superblock::new(BLOCKS);
		committed.active_root_id = [7u8; 32];
		committed.active_commit_seq = 1;
		for &lba in &lbas {
			let mut buf = [0u8; BLOCK_SIZE as usize];
			committed.encode(&mut buf)?;
			device.write_block(lba, &buf)?;
		}

		// A newer writer replaced the leading copy, then stopped.
		let mut newer = Superblock::new(BLOCKS);
		newer.version = ledgerfs::SUPERBLOCK_VERSION + 1;
		let mut buf = [0u8; BLOCK_SIZE as usize];
		newer.encode(&mut buf)?;
		device.write_block(lbas[0], &buf)?;

		// -- Exec
		let copies = read_superblock_copies(&device)?;
		let selection = ledgerfs::superblock::mount_select(&copies)?;

		// -- Check
		assert_eq!(copies.iter().filter(|c| c.is_some()).count(), 3);
		assert_eq!(selection.selected_root.commit_seq, 1);

		Ok(())
	}

	/// Build a superblock the way the builders do: encode, then decode, so it
	/// carries whatever checksum `encode` maintains.
	fn superblock(root_id: [u8; 32], commit_seq: u64, total_blocks: u64) -> Superblock {
		let mut sb = Superblock::new(total_blocks);
		sb.active_root_id = root_id;
		sb.active_commit_seq = commit_seq;

		let mut buf = [0u8; BLOCK_SIZE as usize];
		sb.encode(&mut buf).expect("superblock encode");
		Superblock::decode(&buf).expect("superblock decode")
	}
}

// endregion: --- Tests
