//! Whether the filesystem under a folder folds case (`B.txt` and `b.txt` are
//! one file there), asked of the filesystem by reading only.
//!
//! Planning writes nothing, so nothing is made to probe with. The answer has
//! three values: a filesystem that is known to fold case, one known not to,
//! and one that cannot be told (an empty mount root of a FUSE volume, say).
//! The last is treated as folding: a plan then refuses two targets that differ
//! only in case, and one folder spelled two ways is made once, which costs a
//! rename at worst, where the other mistake overwrites a file.
//!
//! `transfer` (target collisions, new folder identity) and `commits` (the
//! replay layout) both ask [`fs_is_case_insensitive`]; there is no second
//! detector.

use std::fs;
use std::io;
use std::path::Path;

/// What the filesystem says about case. Only Linux can answer `Sensitive`:
/// Windows asks nothing, and macOS cannot tell a case sensitive volume.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseSensitivity {
	/// Known to fold case.
	Insensitive,
	/// Known to tell `a` from `A`.
	Sensitive,
	/// Could not be told.
	Unknown,
}

/// How a Linux `statfs` magic reads.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MagicClass {
	Insensitive,
	Sensitive,
	/// Sensitive unless the folder carries `FS_CASEFOLD_FL`.
	CasefoldCapable,
	Unknown,
}

/// Reads the capability of the filesystem under `root`, then, only when it
/// cannot be told, the names in and above `root`.
pub(crate) fn fs_is_case_insensitive(root: &Path) -> bool {
	#[cfg(test)]
	if let Some((query, names)) = PROBE_OVERRIDE.with(|c| c.get()) {
		return decide(query, || names);
	}
	decide(query_filesystem(root), || probe_names(root))
}

#[cfg(test)]
thread_local! {
	static PROBE_OVERRIDE: std::cell::Cell<Option<(CaseSensitivity, Option<bool>)>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with [`fs_is_case_insensitive`] answering from `query` and, when
/// that is `Unknown`, `names`, whatever the disk says.
#[cfg(test)]
pub(crate) fn with_probe_override<T>(
	query: CaseSensitivity,
	names: Option<bool>,
	f: impl FnOnce() -> T,
) -> T {
	struct Reset;
	impl Drop for Reset {
		fn drop(&mut self) {
			PROBE_OVERRIDE.with(|c| c.set(None));
		}
	}
	PROBE_OVERRIDE.with(|c| c.set(Some((query, names))));
	let _reset = Reset;
	f()
}

/// Whether to fold case: a definite answer stands; when the filesystem could
/// not be told, the names decide if they can, and fold when they cannot.
fn decide(
	query: CaseSensitivity,
	names: impl FnOnce() -> Option<bool>,
) -> bool {
	match query {
		CaseSensitivity::Insensitive => true,
		CaseSensitivity::Sensitive => false,
		CaseSensitivity::Unknown => names().unwrap_or(true),
	}
}

/// What the filesystem under `root` says, by a query that writes nothing.
///
/// Windows folds case unless a folder was switched to case sensitive, which
/// is not asked: a folder that does is treated as folding, the safe side.
#[cfg(windows)]
fn query_filesystem(_root: &Path) -> CaseSensitivity {
	CaseSensitivity::Insensitive
}

/// Linux: the `statfs` magic, and for the filesystems that fold case in a
/// folder that carries `FS_CASEFOLD_FL` (ext4, f2fs, tmpfs, bcachefs) that folder's
/// flags.
#[cfg(target_os = "linux")]
fn query_filesystem(root: &Path) -> CaseSensitivity {
	let Ok(stat) = rustix::fs::statfs(root) else {
		return CaseSensitivity::Unknown;
	};
	#[allow(clippy::unnecessary_cast)]
	let magic = stat.f_type as u64;
	match classify_f_type(magic) {
		MagicClass::Insensitive => CaseSensitivity::Insensitive,
		MagicClass::Sensitive => CaseSensitivity::Sensitive,
		MagicClass::Unknown => CaseSensitivity::Unknown,
		MagicClass::CasefoldCapable => casefold_flag(root),
	}
}

/// `FS_CASEFOLD_FL` of the folder `root`.
#[cfg(target_os = "linux")]
fn casefold_flag(root: &Path) -> CaseSensitivity {
	use rustix::io::Errno;
	const FS_CASEFOLD_FL: u32 = 0x4000_0000;
	let Ok(dir) = fs::File::open(root) else {
		return CaseSensitivity::Unknown;
	};
	match rustix::fs::ioctl_getflags(&dir) {
		Ok(flags) if flags.bits() & FS_CASEFOLD_FL != 0 => {
			CaseSensitivity::Insensitive
		}
		Ok(_) => CaseSensitivity::Sensitive,
		// A filesystem that has no such flag has no folding folder either.
		Err(Errno::NOTTY | Errno::INVAL | Errno::OPNOTSUPP | Errno::NOSYS) => {
			CaseSensitivity::Sensitive
		}
		Err(_) => CaseSensitivity::Unknown,
	}
}

/// macOS: `pathconf(_PC_CASE_SENSITIVE)` would say, but it is unsafe to
/// call and the workspace forbids that; the type name settles the volumes
/// that always fold case, and APFS and HFS+ (which come both ways) are left
/// to the names.
#[cfg(target_os = "macos")]
fn query_filesystem(root: &Path) -> CaseSensitivity {
	match nix::sys::statfs::statfs(root) {
		Ok(stat) => classify_macos_fstype(stat.filesystem_type_name()),
		Err(_) => CaseSensitivity::Unknown,
	}
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn query_filesystem(_root: &Path) -> CaseSensitivity {
	CaseSensitivity::Unknown
}

/// How a Linux `statfs` magic reads. `f_type` is a signed word on some
/// targets, so a magic with the top bit set arrives sign-extended: only its
/// low 32 bits are the magic.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn classify_f_type(raw: u64) -> MagicClass {
	match raw & 0xFFFF_FFFF {
		// vfat, exfat, ntfs3, ntfs, cifs, smb2, sdcardfs
		0x4d44 | 0x2011_BAB0 | 0x7366_746e | 0x5346_544e | 0xFF53_4D42
		| 0xFE53_4D42 | 0x5dca_2df5 => MagicClass::Insensitive,
		// ext2/3/4, f2fs, tmpfs, and bcachefs (casefold per folder from
		// Linux 6.13; an older kernel answers ENOTTY and reads as sensitive).
		0xEF53 | 0xF2F5_2010 | 0x0102_1994 | 0xca45_1a4e => {
			MagicClass::CasefoldCapable
		}
		// btrfs, ramfs, squashfs, erofs, reiserfs: no case folding mode.
		0x9123_683E | 0x8584_58f6 | 0x7371_7368 | 0xE0F5_E1E2 | 0x5265_4973 => {
			MagicClass::Sensitive
		}
		// zfs (per dataset `casesensitivity`), xfs (`-n version=ci`), jfs
		// (OS/2 mode), fuse and fuseblk, overlayfs, nfs, 9p, virtiofs, hfs,
		// hfsplus and the rest: the type does not say.
		_ => MagicClass::Unknown,
	}
}

/// How a macOS `f_fstypename` reads.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn classify_macos_fstype(name: &str) -> CaseSensitivity {
	match name {
		"msdos" | "exfat" | "ntfs" => CaseSensitivity::Insensitive,
		_ => CaseSensitivity::Unknown,
	}
}

fn probe_names(root: &Path) -> Option<bool> {
	probe_names_io(root).ok()
}

fn swap_ascii_case(s: &str) -> Option<String> {
	let mut swapped = String::with_capacity(s.len());
	let mut has_ascii_alpha = false;
	for c in s.chars() {
		if c.is_ascii_alphabetic() {
			has_ascii_alpha = true;
			if c.is_ascii_lowercase() {
				swapped.push(c.to_ascii_uppercase());
			} else {
				swapped.push(c.to_ascii_lowercase());
			}
		} else {
			swapped.push(c);
		}
	}
	if has_ascii_alpha {
		Some(swapped)
	} else {
		None
	}
}

#[cfg(unix)]
fn is_same_file(p1: &Path, p2: &Path) -> io::Result<bool> {
	use std::os::unix::fs::MetadataExt;
	let m1 = fs::symlink_metadata(p1)?;
	let m2 = fs::symlink_metadata(p2)?;
	Ok(m1.dev() == m2.dev() && m1.ino() == m2.ino())
}

#[cfg(not(unix))]
fn is_same_file(p1: &Path, p2: &Path) -> io::Result<bool> {
	let c1 = dunce::canonicalize(p1)?;
	let c2 = dunce::canonicalize(p2)?;
	Ok(c1 == c2)
}

/// Whether the filesystem holding `root` folds case, by reading only: a name
/// that exists is looked up under its other case, first among `root`'s own
/// entries, then `root` and its ancestors by their own names. Nothing is made
/// to probe with: planning is read-only, and a probe file made and removed in
/// an empty folder is a write a watcher sees and nobody agreed to.
fn probe_names_io(root: &Path) -> io::Result<bool> {
	if let Ok(entries) = fs::read_dir(root) {
		for entry in entries.flatten() {
			let name = entry.file_name();
			let name_str = name.to_string_lossy();
			if let Some(swapped) = swap_ascii_case(&name_str) {
				let orig_path = root.join(&name);
				let swapped_path = root.join(&swapped);
				match fs::symlink_metadata(&swapped_path) {
					Ok(_) => {
						if let Ok(same) =
							is_same_file(&orig_path, &swapped_path)
						{
							return Ok(same);
						}
					}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {
						return Ok(false);
					}
					Err(_) => {}
				}
			}
		}
	}

	// Nothing in `root` to compare: its own name, then its parents', each
	// looked up beside itself. A folder on another device than `root` (a
	// mount point) says nothing about it, and ends the walk.
	let mut cur = root;
	while let (Some(name), Some(parent)) = (cur.file_name(), cur.parent()) {
		if !same_device(root, parent) {
			break;
		}
		if let Some(swapped) = swap_ascii_case(&name.to_string_lossy()) {
			let swapped_path = parent.join(swapped);
			match fs::symlink_metadata(&swapped_path) {
				Ok(_) => {
					if let Ok(same) = is_same_file(cur, &swapped_path) {
						return Ok(same);
					}
				}
				Err(e) if e.kind() == io::ErrorKind::NotFound => {
					return Ok(false);
				}
				Err(_) => {}
			}
		}
		cur = parent;
	}
	Err(io::Error::new(
		io::ErrorKind::Unsupported,
		"no name to compare under its other case",
	))
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
	use std::os::unix::fs::MetadataExt;
	match (fs::metadata(a), fs::metadata(b)) {
		(Ok(a), Ok(b)) => a.dev() == b.dev(),
		_ => false,
	}
}

#[cfg(not(unix))]
fn same_device(_: &Path, _: &Path) -> bool {
	true
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A filesystem that cannot be told and has no names to compare (the
	/// empty root of an exFAT mount on Linux) folds case for the plan: the
	/// old fallback was the platform default, which on Linux let `B.txt` and
	/// `b.txt` both be written.
	#[test]
	fn an_unknown_filesystem_with_no_names_folds_case() {
		assert!(decide(CaseSensitivity::Unknown, || None));
	}

	#[test]
	fn the_names_only_decide_when_the_filesystem_cannot_be_told() {
		assert!(!decide(CaseSensitivity::Unknown, || Some(false)));
		assert!(decide(CaseSensitivity::Unknown, || Some(true)));
	}

	#[test]
	fn a_definite_answer_is_not_overruled_by_the_names() {
		assert!(decide(CaseSensitivity::Insensitive, || Some(false)));
		assert!(decide(CaseSensitivity::Insensitive, || None));
		assert!(!decide(CaseSensitivity::Sensitive, || Some(true)));
		assert!(!decide(CaseSensitivity::Sensitive, || None));
	}

	#[test]
	fn linux_magics_that_fold_case() {
		for (name, magic) in [
			("vfat", 0x4d44u64),
			("exfat", 0x2011_BAB0),
			("ntfs3", 0x7366_746e),
			("ntfs", 0x5346_544e),
			("cifs", 0xFF53_4D42),
			("smb2", 0xFE53_4D42),
		] {
			assert_eq!(
				classify_f_type(magic),
				MagicClass::Insensitive,
				"{name}"
			);
		}
	}

	#[test]
	fn linux_magics_that_do_not_fold_case() {
		for (name, magic) in [
			("btrfs", 0x9123_683Eu64),
			("ramfs", 0x8584_58f6),
			("squashfs", 0x7371_7368),
			("erofs", 0xE0F5_E1E2),
			("reiserfs", 0x5265_4973),
		] {
			assert_eq!(classify_f_type(magic), MagicClass::Sensitive, "{name}");
		}
	}

	/// ext4, f2fs, tmpfs and bcachefs fold case only in a folder that
	/// carries the flag.
	#[test]
	fn linux_magics_that_fold_case_per_folder() {
		for (name, magic) in [
			("ext4", 0xEF53u64),
			("f2fs", 0xF2F5_2010),
			("tmpfs", 0x0102_1994),
			("bcachefs", 0xca45_1a4e),
		] {
			assert_eq!(
				classify_f_type(magic),
				MagicClass::CasefoldCapable,
				"{name}"
			);
		}
	}

	/// Nothing is known of what a FUSE volume, a network share or a stacked
	/// filesystem holds underneath, and these fold case by a setting the
	/// magic does not show: a ZFS dataset's `casesensitivity` (TrueNAS makes
	/// SMB datasets `insensitive`), `mkfs.xfs -n version=ci`, JFS's OS/2 mode.
	#[test]
	fn linux_magics_that_cannot_be_told() {
		for (name, magic) in [
			("zfs", 0x2FC1_2FC1u64),
			("xfs", 0x5846_5342),
			("jfs", 0x3153_464a),
			("fuse", 0x6573_5546),
			("overlayfs", 0x794c_7630),
			("nfs", 0x6969),
			("9p", 0x0102_1997),
			("unheard of", 0x1234_5678),
		] {
			assert_eq!(classify_f_type(magic), MagicClass::Unknown, "{name}");
		}
	}

	/// `f_type` is a signed word on some targets: a magic with the top bit
	/// set arrives sign-extended and must read as the 32 bits it is.
	#[test]
	fn a_sign_extended_magic_reads_as_its_32_bits() {
		let extended = i64::from(0xFF53_4D42u32 as i32) as u64;
		assert_ne!(extended, 0xFF53_4D42);
		assert_eq!(classify_f_type(extended), MagicClass::Insensitive);
	}

	#[test]
	fn macos_volumes_that_fold_case() {
		for name in ["msdos", "exfat", "ntfs"] {
			assert_eq!(
				classify_macos_fstype(name),
				CaseSensitivity::Insensitive,
				"{name}"
			);
		}
	}

	/// APFS and HFS+ come both ways: the name of the type says nothing.
	#[test]
	fn macos_volumes_of_either_kind_cannot_be_told() {
		for name in ["apfs", "hfs", "nfs", "smbfs", "zfs", ""] {
			assert_eq!(
				classify_macos_fstype(name),
				CaseSensitivity::Unknown,
				"{name}"
			);
		}
	}

	#[cfg(windows)]
	#[test]
	fn windows_folds_case() {
		let dir = tempfile::tempdir().unwrap();
		assert_eq!(query_filesystem(dir.path()), CaseSensitivity::Insensitive);
	}

	/// A filesystem that does not fold case is told so by asking it, with no
	/// file in the folder to compare: tmpfs and ext4 are the ones a Linux
	/// box has at hand.
	#[cfg(target_os = "linux")]
	#[test]
	fn a_linux_tmpfs_or_ext4_folder_is_known_to_be_sensitive() {
		use std::path::PathBuf;
		let mut candidates: Vec<PathBuf> = vec![std::env::temp_dir()];
		candidates
			.extend(["/dev/shm", "/tmp", "/run", "/var/tmp"].map(Into::into));
		let mut tried = Vec::new();
		for base in candidates {
			let Ok(stat) = rustix::fs::statfs(&base) else {
				continue;
			};
			let magic = (stat.f_type as u64) & 0xFFFF_FFFF;
			tried.push(format!("{} {magic:#x}", base.display()));
			if classify_f_type(magic) != MagicClass::CasefoldCapable {
				continue;
			}
			let dir = tempfile::tempdir_in(&base).unwrap();
			assert_eq!(
				query_filesystem(dir.path()),
				CaseSensitivity::Sensitive,
				"{}",
				base.display()
			);
			assert!(!fs_is_case_insensitive(dir.path()));
			return;
		}
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"no tmpfs or ext4 folder to ask: {tried:?}"
		);
	}
}
