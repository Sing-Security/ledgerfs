//! What the numbers came from: the machine, the filesystem, and the device.
//!
//! # Why this is not optional
//!
//! A durability benchmark measures what a barrier costs. On a filesystem whose
//! `fsync` returns without touching a device — tmpfs, ramfs, overlayfs — that
//! cost is zero, and every engine being compared becomes indistinguishable while
//! the numbers stay plausible. The mistake is invisible in the output: a table
//! of fast numbers reads exactly like a table of correct ones.
//!
//! So the harness refuses to run on such a medium, and records enough about the
//! one it did run on that a reader can tell whether the result is transferable.
//! A number without its device, filesystem, and durability setting is not a
//! result.
//!
//! # What is recorded
//!
//! - the CPU, so "this is a laptop" is not a mystery
//! - the filesystem type, from `statfs(2)`'s magic rather than from a path
//! - whether that filesystem is one whose writes are not durable by construction
//! - the block device behind the mount point, and its model
//! - whether that device is dm-crypt, because an encrypted device adds a
//!   per-write cost that has nothing to do with the filesystem being measured
//! - free space, checked against what the run will need before it starts

use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// A refusal to measure. Kept separate from `io::Error` because it is a
/// deliberate stop, not a failure of the environment to cooperate.
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl std::error::Error for Refused {}

/// A refusal is the caller's problem to fix — point `--dir` somewhere else —
/// so it surfaces as a usage error rather than as an environment failure.
impl From<Refused> for crate::error::Error {
	fn from(r: Refused) -> Self {
		crate::error::Error::usage(r.0)
	}
}

/// Filesystem magics whose writes reach no device.
///
/// `overlayfs` is here for a different reason than the rest: its upper layer is
/// durable, but the barrier cost it exposes belongs to whatever backs that
/// layer, which is not knowable from the mount point. Measuring on it would
/// attribute someone else's fsync to this run.
const VOLATILE_MAGICS: &[(i64, &str)] = &[
	(0x0102_1994, "tmpfs"),
	(0x8584_58f6, "ramfs"),
	(0x0000_1373, "devtmpfs"),
	(0x794c_7630, "overlayfs"),
	(0x0000_9fa0, "proc"),
	(0x6265_6572, "sysfs"),
	(0x0000_6969, "nfs"),
	(0x0000_0102, "remote (afs)"),
	(0x0000_517b, "smb2"),
	(0xff53_4d42, "cifs"),
];

/// The magics worth naming in the output. Anything else prints as its number.
const KNOWN_MAGICS: &[(i64, &str)] = &[
	(0xef53, "ext4/ext2/ext3"),
	(0x9123_683e, "btrfs"),
	(0x5846_5342, "xfs"),
	(0x2fc1_2fc1, "zfs"),
	(0xf2f5_2010, "f2fs"),
	(0x0102_1994, "tmpfs"),
	(0x794c_7630, "overlayfs"),
];

/// Everything the harness knows about where it ran.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Medium {
	/// The directory the images live in, as given.
	pub path: String,
	/// Filesystem magic from `statfs(2)`, and the name if we recognise it.
	pub fs_magic: i64,
	pub fs_name: String,
	/// True if this filesystem's writes are not durable by construction.
	pub volatile: bool,
	/// The mount source — a device path, or a pseudo-name for a virtual fs.
	pub source: Option<String>,
	/// The device model behind the mount point, following one dm layer.
	pub device_model: Option<String>,
	/// True when the device is dm-crypt, so an encryption cost is included.
	pub encrypted: bool,
	pub free_bytes: u64,
	pub total_bytes: u64,
}

impl Medium {
	/// Is a barrier on this medium a real thing?
	pub fn barriers_are_real(&self) -> bool {
		!self.volatile
	}
}

impl fmt::Display for Medium {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{} on {}", self.path, self.fs_name)?;
		if let Some(src) = &self.source {
			write!(f, " ({src}")?;
			if let Some(model) = &self.device_model {
				write!(f, ", {model}")?;
			}
			if self.encrypted {
				write!(f, ", dm-crypt")?;
			}
			write!(f, ")")?;
		}
		Ok(())
	}
}

/// The machine, as far as `/proc` will say.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Machine {
	pub cpu: Option<String>,
	/// Logical processors, or `None` if `/proc/cpuinfo` was not readable.
	pub cores: Option<String>,
	pub memory_bytes: Option<u64>,
	pub kernel: Option<String>,
	pub hostname: Option<String>,
}

/// Probe `dir` and check that it is a medium a durability number can come from.
///
/// `need_bytes` is the space the run expects to occupy; the check is deliberately
/// generous, because running out of space halfway through is a failed run that
/// looks like a slow one.
pub fn require_durable(dir: &Path, need_bytes: u64) -> Result<Medium, Refused> {
	fs::create_dir_all(dir).map_err(|e| Refused(format!("cannot create {}: {e}", dir.display())))?;
	let medium = probe(dir).map_err(|e| Refused(format!("cannot probe {}: {e}", dir.display())))?;

	if medium.volatile {
		return Err(Refused(format!(
			"refusing to measure on {} — its writes do not reach a device, so every barrier is free \
			 and the comparison would be fiction. Point --dir at a real filesystem.",
			medium.fs_name
		)));
	}
	if medium.free_bytes < need_bytes {
		return Err(Refused(format!(
			"{} has {} MiB free but the run needs about {} MiB",
			medium.path,
			medium.free_bytes / (1024 * 1024),
			need_bytes / (1024 * 1024),
		)));
	}
	Ok(medium)
}

/// Everything about `path`'s filesystem and device, without judging it.
pub fn probe(path: &Path) -> io::Result<Medium> {
	let abs = fs::canonicalize(path)?;
	let (magic, free_bytes, total_bytes) = statfs_of(&abs)?;
	let fs_name = KNOWN_MAGICS
		.iter()
		.find(|(m, _)| *m == magic)
		.map(|(_, n)| (*n).to_string())
		.unwrap_or_else(|| format!("magic 0x{magic:x}"));
	let volatile = VOLATILE_MAGICS.iter().any(|(m, _)| *m == magic);
	// A volatile medium has no device to describe, and probing one would take a
	// lock on a nonexistent file.
	if volatile {
		return Ok(Medium {
			path: abs.display().to_string(),
			fs_magic: magic,
			fs_name,
			volatile,
			source: None,
			device_model: None,
			encrypted: false,
			free_bytes,
			total_bytes,
		});
	}

	let mounts = mount_entries();
	let (source, major, minor) = mounts
		.iter()
		.filter(|m| abs.starts_with(&m.mount_point))
		.max_by_key(|m| m.mount_point.as_os_str().len())
		.map(|m| (m.source.clone(), m.major_minor.0, m.major_minor.1))
		.unwrap_or_default();

	let (device_model, encrypted) = match device_ids(&source, major, minor) {
		Some((major, minor)) => describe_device(major, minor),
		None => (None, false),
	};

	Ok(Medium {
		path: abs.display().to_string(),
		fs_magic: magic,
		fs_name,
		volatile,
		source: if source.is_empty() { None } else { Some(source) },
		device_model,
		encrypted,
		free_bytes,
		total_bytes,
	})
}

pub fn machine() -> Machine {
	let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
	let field = |key: &str| {
		cpuinfo
			.lines()
			.find(|l| l.starts_with(key))
			.and_then(|l| l.split_once(':'))
			.map(|(_, v)| v.trim().to_string())
	};
	let cores = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
	Machine {
		cpu: field("model name"),
		cores: (cores > 0).then(|| cores.to_string()),
		memory_bytes: meminfo_bytes("MemTotal"),
		kernel: fs::read_to_string("/proc/sys/kernel/osrelease")
			.ok()
			.map(|s| s.trim().to_string()),
		hostname: fs::read_to_string("/proc/sys/kernel/hostname")
			.ok()
			.map(|s| s.trim().to_string()),
	}
}

/// `statfs(2)`: the filesystem magic and the sizes, in one call.
fn statfs_of(path: &Path) -> io::Result<(i64, u64, u64)> {
	let c = CString::new(path.as_os_str().as_encoded_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
	let mut st: libc::statfs = unsafe { std::mem::zeroed() };
	// SAFETY: `c` is a valid NUL-terminated path and `st` is a live, correctly
	// sized `statfs` for the kernel to write into. The call touches no other
	// memory and its return value is checked before `st` is read.
	let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
	if rc != 0 {
		return Err(io::Error::last_os_error());
	}
	let frsize = if st.f_bsize > 0 { st.f_bsize as u64 } else { 4096 };
	Ok((
		st.f_type as i64,
		(st.f_bavail as u64).saturating_mul(frsize),
		(st.f_blocks as u64).saturating_mul(frsize),
	))
}

fn meminfo_bytes(key: &str) -> Option<u64> {
	let text = fs::read_to_string("/proc/meminfo").ok()?;
	let line = text.lines().find(|l| l.starts_with(key))?;
	let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
	Some(kb * 1024)
}

// region: --- mount table

struct MountEntry {
	mount_point: PathBuf,
	source: String,
	major_minor: (u32, u32),
}

/// Parse `/proc/self/mountinfo`.
///
/// The field order is documented in `proc(5)`, and the one that bites is the
/// fourth: `ID parent major:minor ROOT MOUNT-POINT options ... - fstype source
/// super-options`. The root and the mount point are different fields with the
/// same shape, and reading the root as the mount point makes every btrfs
/// subvolume mount — whose root is usually `/` or `/@` — look like it covers
/// the whole tree, so the wrong device gets reported for every path under it.
///
/// Mount points are octal-escaped (`\040` for a space), which is why the field
/// is unescaped rather than used raw.
fn mount_entries() -> Vec<MountEntry> {
	let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
		return Vec::new();
	};
	parse_mountinfo(&text)
}

fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
	text.lines()
		.filter_map(|line| {
			let (before, after) = line.split_once(" - ")?;
			let mut left = before.split_whitespace();
			let _id = left.next()?;
			let _parent = left.next()?;
			let major_minor = left.next()?;
			let _root = left.next()?;
			let mount_point = unescape(left.next()?);
			let (major, minor) = major_minor.split_once(':')?;
			let mut right = after.split_whitespace();
			let _fstype = right.next()?;
			let source = right.next()?.to_string();
			Some(MountEntry {
				mount_point: PathBuf::from(mount_point),
				source,
				major_minor: (major.parse().ok()?, minor.parse().ok()?),
			})
		})
		.collect()
}

fn unescape(s: &str) -> String {
	let bytes = s.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'\\' && i + 3 < bytes.len() {
			let oct = std::str::from_utf8(&bytes[i + 1..i + 4]).ok();
			if let Some(v) = oct.and_then(|o| u8::from_str_radix(o, 8).ok()) {
				out.push(v);
				i += 4;
				continue;
			}
		}
		out.push(bytes[i]);
		i += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Which block device to describe, given the mount's source and the numbers
/// `/proc/self/mountinfo` reports.
///
/// The source wins when it names a device node. `mountinfo`'s `major:minor` is
/// the *filesystem's* device numbering, and a btrfs subvolume reports the
/// filesystem's own anonymous device — `0:30`, say — which names nothing under
/// `/sys/dev/block`. On exactly the machine this matters for, a LUKS root, that
/// silently turns "dm-crypt" into "no encryption reported", which is worse than
/// reporting nothing.
///
/// `stat` on the source gives the real `st_rdev`, and that does name a device.
fn device_ids(source: &str, major: u32, minor: u32) -> Option<(u32, u32)> {
	if source.starts_with('/')
		&& let Ok(md) = fs::metadata(source)
		&& md.rdev() != 0
	{
		let rdev = md.rdev();
		return Some((libc::major(rdev), libc::minor(rdev)));
	}
	(major != 0 || minor != 0).then_some((major, minor))
}

/// Resolve a device to its model and whether it sits behind dm-crypt.
///
/// One dm layer is followed, which is the shape a LUKS root has: the mount
/// source is the mapping, and the model belongs to the device under it.
fn describe_device(major: u32, minor: u32) -> (Option<String>, bool) {
	let sys = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
	let Ok(dev) = fs::canonicalize(&sys) else {
		return (None, false);
	};

	let dm_uuid = read_trim(&dev.join("dm/uuid"));
	let encrypted = dm_uuid.as_deref().is_some_and(|u| u.starts_with("CRYPT-"));

	// A dm device with `slaves` is a mapping; its model belongs to the slave.
	// The slave entry is a symlink into another part of the sysfs tree, so it
	// is resolved before the search below: with the link left in place, the
	// parent of the path is the link's parent, not the disk's.
	let mut target = dev.clone();
	if let Ok(mut slaves) = fs::read_dir(dev.join("slaves"))
		&& let Some(Ok(entry)) = slaves.next()
	{
		target = fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path());
	}
	// A whole disk carries the model; a partition does not, and a partition is
	// what a dm mapping usually sits on, so one step up is tried before giving
	// up. The parent of `/sys/devices/.../block/nvme0n1/nvme0n1p2` is the disk.
	let model = read_trim(&target.join("device/model"))
		.or_else(|| target.parent().and_then(|d| read_trim(&d.join("device/model"))));
	(model, encrypted)
}

fn read_trim(path: &Path) -> Option<String> {
	fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

// endregion: --- mount table

/// A one-line summary for the console, so a run's provenance is in the log too.
pub fn describe(machine: &Machine, medium: &Medium) -> String {
	let cpu = machine.cpu.clone().unwrap_or_else(|| "unknown CPU".into());
	let mem = machine
		.memory_bytes
		.map(|b| format!("{:.1} GiB", b as f64 / (1024.0 * 1024.0 * 1024.0)))
		.unwrap_or_else(|| "unknown memory".into());
	format!("{cpu} ({mem}) — {medium}")
}

// region:    --- Tests

#[cfg(test)]
mod tests {
	type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

	use super::*;

	/// Two lines with the shape that caused the bug this parser was fixed for:
	/// a btrfs subvolume whose *root* is `/@home` mounted at `/home`, and a
	/// filesystem whose root is `/` mounted somewhere unrelated. Reading the
	/// root instead of the mount point makes the second one cover every path.
	const FIXTURE: &str = "\
33 2 0:30 /@ / rw,noatime shared:1 - btrfs /dev/mapper/luks-abc rw,subvol=/@
32 29 0:99 / /run/user/1000/doc rw,nosuid shared:838 - fuse.portal portal rw,user_id=1000
63 33 0:30 /@home /home rw,noatime shared:139 - btrfs /dev/mapper/luks-abc rw,subvol=/@home
17 33 8:1 / /mnt/with\\040space rw shared:9 - ext4 /dev/sda1 rw
";

	fn best_for<'a>(entries: &'a [MountEntry], path: &str) -> Option<&'a MountEntry> {
		entries
			.iter()
			.filter(|m| Path::new(path).starts_with(&m.mount_point))
			.max_by_key(|m| m.mount_point.as_os_str().len())
	}

	#[test]
	fn test_mount_point_is_the_fifth_field() -> TestResult<()> {
		// -- Setup & Fixtures
		let entries = parse_mountinfo(FIXTURE);

		// -- Exec / Check: the deepest mount point wins.
		assert_eq!(
			best_for(&entries, "/home/user/x").unwrap().source,
			"/dev/mapper/luks-abc"
		);
		assert_eq!(best_for(&entries, "/home").unwrap().major_minor, (0, 30));
		assert_eq!(best_for(&entries, "/etc/hosts").unwrap().source, "/dev/mapper/luks-abc");
		assert_eq!(best_for(&entries, "/run/user/1000/doc/x").unwrap().source, "portal");
		Ok(())
	}

	#[test]
	fn test_escaped_mount_point_is_unescaped() -> TestResult<()> {
		// -- Setup & Fixtures
		let entries = parse_mountinfo(FIXTURE);

		// -- Exec: a path inside the directory whose name holds a space.
		let hit = best_for(&entries, "/mnt/with space/file");

		// -- Check: octal escapes are decoded, or the match above would fail.
		assert_eq!(hit.unwrap().source, "/dev/sda1");
		Ok(())
	}

	#[test]
	fn test_unescape_leaves_a_lone_backslash_alone() {
		// Not an octal escape, and not one of the four escapes the kernel
		// writes — passing it through is the only safe reading.
		assert_eq!(unescape(r"a\b"), r"a\b");
		assert_eq!(unescape(r"a\040b"), "a b");
	}

	#[test]
	fn test_guard_refuses_tmpfs() -> TestResult<()> {
		// -- Setup & Fixtures: `/dev/shm` is a tmpfs on every Linux system
		// that has one, which is the case this guard exists for.
		let dir = Path::new("/dev/shm/ledgerfs-bench-guard-test");
		if !Path::new("/dev/shm").is_dir() {
			return Ok(());
		}

		// -- Exec
		let outcome = require_durable(dir, 0);

		// -- Check
		let err = outcome.expect_err("a tmpfs must be refused");
		assert!(
			err.0.contains("tmpfs"),
			"the refusal should name the filesystem, got: {err}"
		);
		let _ = fs::remove_dir_all(dir);
		Ok(())
	}
}

// endregion: --- Tests
