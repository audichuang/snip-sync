//! Filesystem helpers (reading, encoding detection, safe writes).
//!
//! Port of `fileSystem.ts`, synchronous.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Strict UTF-8 decode; `None` means "not copyable as text".
///
/// A NUL byte marks the file as binary (this also rejects UTF-16 with or
/// without a BOM). Never lossy: a Big5 / Shift_JIS file decoded with U+FFFD
/// replacements would be written back as mojibake on restore. A leading BOM
/// is kept, as TS does with `ignoreBOM: true`.
pub fn decode_utf8_or_skip(bytes: Vec<u8>) -> Option<String> {
	if bytes.contains(&0) {
		return None;
	}
	String::from_utf8(bytes).ok()
}

pub fn read_text_file(path: &Path) -> io::Result<Option<String>> {
	fs::read(path).map(decode_utf8_or_skip)
}

/// 8 MiB. A larger restore target is reported as unverifiable, not read.
const ENCODING_CHECK_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetEncoding {
	Absent,
	Utf8,
	Other,
	Unverifiable,
}

/// What is on disk at `path` right now. Fails closed: a file that cannot be
/// read, or is too large to read, is `Unverifiable`.
pub fn target_encoding(path: &Path) -> TargetEncoding {
	let Ok(info) = fs::metadata(path) else {
		return TargetEncoding::Absent;
	};
	if !info.is_file() {
		return TargetEncoding::Absent;
	}
	if info.len() > ENCODING_CHECK_LIMIT {
		return TargetEncoding::Unverifiable;
	}
	match read_text_file(path) {
		Ok(Some(_)) => TargetEncoding::Utf8,
		Ok(None) => TargetEncoding::Other,
		Err(_) => TargetEncoding::Unverifiable,
	}
}

/// True when the bytes on disk must not be replaced with UTF-8 text.
pub fn must_not_overwrite(path: &Path) -> bool {
	matches!(
		target_encoding(path),
		TargetEncoding::Other | TargetEncoding::Unverifiable
	)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkItem {
	File(PathBuf),
	/// A directory whose listing failed; its subtree is an omission the
	/// caller must report.
	UnreadableDir(PathBuf),
}

/// Lazy depth-first walk of `input`, children in sorted order.
///
/// `should_enter` prunes a directory's whole subtree when it returns false.
/// A directory symlink is walked only when it is `input` itself; during
/// recursion it is skipped (cross-linked pnpm / Bazel trees explode
/// otherwise). Other symlinks, including dangling ones, are yielded as files.
pub fn list_files_recursive<F: FnMut(&Path) -> bool>(
	input: &Path,
	should_enter: F,
) -> ListFiles<F> {
	ListFiles {
		stack: vec![(input.to_path_buf(), true)],
		should_enter,
	}
}

pub struct ListFiles<F> {
	/// Pending paths with their `is_selection` flag, next on top.
	stack: Vec<(PathBuf, bool)>,
	should_enter: F,
}

impl<F: FnMut(&Path) -> bool> Iterator for ListFiles<F> {
	type Item = WalkItem;

	fn next(&mut self) -> Option<WalkItem> {
		while let Some((path, is_selection)) = self.stack.pop() {
			let Ok(info) = fs::symlink_metadata(&path) else {
				continue;
			};
			if info.file_type().is_symlink() {
				if !fs::metadata(&path).is_ok_and(|t| t.is_dir()) {
					return Some(WalkItem::File(path));
				}
				if !is_selection {
					continue;
				}
			} else if !info.is_dir() {
				return Some(WalkItem::File(path));
			}

			if !(self.should_enter)(&path) {
				continue;
			}
			let entries: io::Result<Vec<_>> =
				fs::read_dir(&path).and_then(|dir| {
					dir.map(|e| e.map(|e| e.file_name())).collect()
				});
			let Ok(mut names) = entries else {
				return Some(WalkItem::UnreadableDir(path));
			};
			// JS Array.prototype.sort compares UTF-16 code units.
			names.sort_by_cached_key(|n| {
				n.to_string_lossy().encode_utf16().collect::<Vec<u16>>()
			});
			self.stack
				.extend(names.into_iter().rev().map(|n| (path.join(n), false)));
		}
		None
	}
}

/// Writes UTF-8 `content`, creating parent directories as needed.
pub fn write_text_file(path: &Path, content: &str) -> io::Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}
	fs::write(path, content)
}

pub fn delete_file(path: &Path) -> io::Result<()> {
	fs::remove_file(path)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn walk(input: &Path, root: &Path) -> Vec<String> {
		list_files_recursive(input, |_| true)
			.map(|item| match item {
				WalkItem::File(p) => p
					.strip_prefix(root)
					.unwrap()
					.to_string_lossy()
					.replace('\\', "/"),
				WalkItem::UnreadableDir(p) => format!("!{}", p.display()),
			})
			.collect()
	}

	#[test]
	fn decode_is_strict_and_keeps_bom() {
		assert_eq!(
			decode_utf8_or_skip(b"\xEF\xBB\xBFabc".to_vec()).as_deref(),
			Some("\u{FEFF}abc")
		);
		assert_eq!(decode_utf8_or_skip(Vec::new()).as_deref(), Some(""));
		// Big5, UTF-16 LE with BOM, embedded NUL.
		assert_eq!(decode_utf8_or_skip(vec![0xa4, 0xe9, 0xa5, 0xbb]), None);
		assert_eq!(decode_utf8_or_skip(vec![0xff, 0xfe, 0x41, 0x00]), None);
		assert_eq!(decode_utf8_or_skip(b"a\0b".to_vec()), None);
	}

	#[test]
	fn target_encoding_classifies_what_is_on_disk() {
		let dir = tempfile::tempdir().unwrap();
		let p = |n: &str| dir.path().join(n);
		fs::write(p("plain.txt"), "ascii\n").unwrap();
		fs::write(p("legacy.txt"), [0xa4, 0xe9, 0xa5, 0xbb, 0x0a]).unwrap();
		let big = fs::File::create(p("big.txt")).unwrap();
		big.set_len(ENCODING_CHECK_LIMIT + 1).unwrap();

		assert_eq!(target_encoding(&p("missing")), TargetEncoding::Absent);
		assert_eq!(target_encoding(dir.path()), TargetEncoding::Absent);
		assert_eq!(target_encoding(&p("plain.txt")), TargetEncoding::Utf8);
		assert_eq!(target_encoding(&p("legacy.txt")), TargetEncoding::Other);
		assert_eq!(
			target_encoding(&p("big.txt")),
			TargetEncoding::Unverifiable
		);
		assert!(must_not_overwrite(&p("legacy.txt")));
		assert!(must_not_overwrite(&p("big.txt")));
		assert!(!must_not_overwrite(&p("plain.txt")));
		assert!(!must_not_overwrite(&p("missing")));
	}

	#[cfg(unix)]
	#[test]
	fn unreadable_target_is_unverifiable() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let p = dir.path().join("unreadable.bin");
		fs::write(&p, [0xff, 0xfe, 0x41, 0x00]).unwrap();
		fs::set_permissions(&p, fs::Permissions::from_mode(0o200)).unwrap();
		// Root reads anything; the check is meaningless there.
		if fs::read(&p).is_err() {
			assert_eq!(target_encoding(&p), TargetEncoding::Unverifiable);
			assert!(must_not_overwrite(&p));
		}
		fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
	}

	#[test]
	fn walks_recursively_sorted_with_empty_files_and_pruning() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir_all(root.join("src/nested")).unwrap();
		fs::create_dir_all(root.join("node_modules/dep")).unwrap();
		fs::write(root.join("src/main.ts"), "main").unwrap();
		fs::write(root.join("src/nested/empty.ts"), "").unwrap();
		fs::write(root.join("src/B.ts"), "b").unwrap();
		fs::write(root.join("node_modules/dep/index.js"), "dep").unwrap();

		assert_eq!(
			walk(&root.join("src"), root),
			["src/B.ts", "src/main.ts", "src/nested/empty.ts"]
		);
		assert_eq!(
			read_text_file(&root.join("src/nested/empty.ts")).unwrap(),
			Some(String::new())
		);

		let pruned: Vec<_> =
			list_files_recursive(root, |d| !d.ends_with("node_modules"))
				.collect();
		assert_eq!(pruned.len(), 3);
		assert!(pruned.iter().all(|i| matches!(i, WalkItem::File(p) if !p.starts_with(root.join("node_modules")))));

		// A plain file input yields itself; a missing one yields nothing.
		assert_eq!(walk(&root.join("src/main.ts"), root), ["src/main.ts"]);
		assert!(walk(&root.join("missing"), root).is_empty());
	}

	#[cfg(unix)]
	#[test]
	fn symlinks_follow_only_when_selected() {
		use std::os::unix::fs::symlink;
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir_all(root.join("packages/ui")).unwrap();
		fs::create_dir_all(root.join("node_modules")).unwrap();
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("packages/ui/index.ts"), "ui").unwrap();
		fs::write(root.join("src/a.ts"), "a").unwrap();
		symlink(root.join("packages/ui"), root.join("node_modules/ui"))
			.unwrap();
		symlink(root, root.join("packages/loop")).unwrap();
		symlink(root.join("nowhere"), root.join("src/dangling.ts")).unwrap();

		// The selected link is walked.
		assert_eq!(
			walk(&root.join("node_modules/ui"), root),
			["node_modules/ui/index.ts"]
		);
		// A link back to an ancestor is not followed during recursion.
		assert_eq!(
			walk(&root.join("packages"), root),
			["packages/ui/index.ts"]
		);
		// A dangling link is yielded; reading it is the caller's failure.
		assert_eq!(
			walk(&root.join("src"), root),
			["src/a.ts", "src/dangling.ts"]
		);
		assert!(read_text_file(&root.join("src/dangling.ts")).is_err());
	}

	#[cfg(unix)]
	#[test]
	fn unreadable_directory_is_reported_not_fatal() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir_all(root.join("locked")).unwrap();
		fs::write(root.join("ok.ts"), "ok").unwrap();
		let locked = root.join("locked");
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
			.unwrap();
		let items: Vec<_> = list_files_recursive(root, |_| true).collect();
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))
			.unwrap();
		if fs::read_dir(&locked).is_ok() && items.len() == 1 {
			return; // running as root
		}
		assert_eq!(
			items,
			[
				WalkItem::UnreadableDir(locked),
				WalkItem::File(root.join("ok.ts"))
			]
		);
	}

	#[test]
	fn write_creates_parents_and_delete_removes() {
		let dir = tempfile::tempdir().unwrap();
		let p = dir.path().join("a/b/c.txt");
		write_text_file(&p, "héllo").unwrap();
		assert_eq!(fs::read_to_string(&p).unwrap(), "héllo");
		delete_file(&p).unwrap();
		assert!(!p.exists());
		assert!(delete_file(&p).is_err());
	}
}
