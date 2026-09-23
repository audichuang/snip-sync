//! File-mode copy: collect files and build the payload.
//!
//! Port of `copy.ts` (`collectCopyFiles` / `collectCopyTextFiles`),
//! synchronous. TS reads in batches of 16 but documents the result as
//! byte-identical to a serial walk, so this is the serial walk.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::filter::{directory_excluded, file_matches_filters};
use crate::format::{build_payload, BuildPayloadOptions, PayloadFile};
use crate::fsutil::{list_files_recursive, read_text_file, WalkItem};
use crate::paths::{source_root_name, to_clipboard_path_from_roots};
use crate::settings::Settings;
use crate::stats::{payload_stats, PayloadStats};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyResult {
	pub files: Vec<PayloadFile>,
	pub payload: String,
	pub copied_file_count: usize,
	pub skipped_file_size_count: usize,
	/// Not decodable as UTF-8, binary, or unreadable (files and directories).
	pub skipped_unreadable_count: usize,
	pub file_limit_reached: bool,
}

impl CopyResult {
	/// Notification numbers, computed from the whole payload.
	pub fn stats(&self) -> PayloadStats {
		payload_stats(&self.payload)
	}
}

/// Already-in-memory text (e.g. open editor buffers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyTextFile {
	pub absolute_path: PathBuf,
	pub content: String,
	/// Defaults to the UTF-8 byte length of `content`.
	pub size_bytes: Option<u64>,
}

#[derive(Default)]
struct State {
	files: Vec<PayloadFile>,
	seen: HashSet<PathBuf>,
	copied_file_count: usize,
	skipped_file_size_count: usize,
	skipped_unreadable_count: usize,
	file_limit_reached: bool,
}

enum Content {
	Text(String),
	Unreadable,
}

impl State {
	/// The limit is checked before dedup and filtering, as in TS: the flag
	/// trips on the next candidate after the limit, whatever it is.
	fn limit_hit(&mut self, settings: &Settings) -> bool {
		if settings.set_max_file_count
			&& self.copied_file_count as f64 >= settings.file_count_limit
		{
			self.file_limit_reached = true;
		}
		self.file_limit_reached
	}

	/// Returns the clipboard path when the candidate is new and passes the
	/// filters.
	fn admit<P: AsRef<Path>>(
		&mut self,
		roots: &[P],
		absolute: &Path,
		settings: &Settings,
	) -> Option<String> {
		if !self.seen.insert(absolute.to_path_buf()) {
			return None;
		}
		let relative = to_clipboard_path_from_roots(roots, absolute, None);
		if settings.use_filters
			&& !file_matches_filters(
				&relative,
				&settings.filter_rules,
				settings.use_include_filters,
				settings.use_exclude_filters,
				Some(&absolute.to_string_lossy()),
			) {
			return None;
		}
		Some(relative)
	}

	/// `read` runs only when the size fits.
	fn push(
		&mut self,
		relative_path: String,
		size: u64,
		settings: &Settings,
		read: impl FnOnce() -> Content,
	) {
		if size as f64 > settings.max_file_size_kb * 1024.0 {
			self.skipped_file_size_count += 1;
			self.files.push(PayloadFile {
				path: relative_path,
				skipped_reason: Some(format!(
					"size exceeds limit ({size} bytes)"
				)),
				..Default::default()
			});
			return;
		}
		match read() {
			// An empty string is a real, copyable file.
			Content::Text(content) => {
				self.files.push(PayloadFile {
					path: relative_path,
					content: Some(content),
					..Default::default()
				});
				self.copied_file_count += 1;
			}
			Content::Unreadable => self.skipped_unreadable_count += 1,
		}
	}

	fn finish<P: AsRef<Path>>(
		self,
		roots: &[P],
		settings: &Settings,
	) -> CopyResult {
		let payload = build_payload(&BuildPayloadOptions {
			header_format: settings.header_format.clone(),
			pre_text: settings.pre_text.clone(),
			post_text: settings.post_text.clone(),
			add_extra_line_between_files: settings.add_extra_line_between_files,
			files: self.files.clone(),
			// Only a single-root copy has an unambiguous source root.
			source_root: source_root_name(roots),
		});
		CopyResult {
			files: self.files,
			payload,
			copied_file_count: self.copied_file_count,
			skipped_file_size_count: self.skipped_file_size_count,
			skipped_unreadable_count: self.skipped_unreadable_count,
			file_limit_reached: self.file_limit_reached,
		}
	}
}

pub fn collect_copy_files<P: AsRef<Path>, Q: AsRef<Path>>(
	workspace_roots: &[P],
	input_paths: &[Q],
	settings: &Settings,
) -> CopyResult {
	let mut state = State::default();
	let prune = |dir: &Path| {
		if !settings.use_filters || !settings.use_exclude_filters {
			return true;
		}
		let absolute = resolve(dir);
		!directory_excluded(
			&to_clipboard_path_from_roots(workspace_roots, &absolute, None),
			&settings.filter_rules,
			Some(&absolute.to_string_lossy()),
		)
	};

	'inputs: for input in input_paths {
		// The walker joins children onto the input without normalizing,
		// so resolve `.` / `..` up front (TS `path.join` does it per child).
		let input = resolve(input.as_ref());
		for item in list_files_recursive(&input, prune) {
			let path = match item {
				WalkItem::File(path) => path,
				WalkItem::UnreadableDir(_) => {
					state.skipped_unreadable_count += 1;
					continue;
				}
			};
			if state.limit_hit(settings) {
				break 'inputs;
			}
			let Some(relative) = state.admit(workspace_roots, &path, settings)
			else {
				continue;
			};
			// A dangling symlink or EACCES costs this file only.
			let Ok(meta) = fs::metadata(&path) else {
				state.skipped_unreadable_count += 1;
				continue;
			};
			state.push(
				relative,
				meta.len(),
				settings,
				|| match read_text_file(&path) {
					Ok(Some(text)) => Content::Text(text),
					_ => Content::Unreadable,
				},
			);
		}
	}
	state.finish(workspace_roots, settings)
}

pub fn collect_copy_text_files<P: AsRef<Path>>(
	workspace_roots: &[P],
	input_files: &[CopyTextFile],
	settings: &Settings,
) -> CopyResult {
	let mut state = State::default();
	for file in input_files {
		if state.limit_hit(settings) {
			break;
		}
		let absolute = resolve(&file.absolute_path);
		let Some(relative) = state.admit(workspace_roots, &absolute, settings)
		else {
			continue;
		};
		let size = file.size_bytes.unwrap_or(file.content.len() as u64);
		state.push(relative, size, settings, || {
			Content::Text(file.content.clone())
		});
	}
	state.finish(workspace_roots, settings)
}

/// Lexical `path.resolve`: absolute against the cwd, `.` and `..` removed.
/// Symlinks are NOT resolved; the clipboard path names the link.
fn resolve(path: &Path) -> PathBuf {
	let absolute =
		std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
	let mut out = PathBuf::new();
	for component in absolute.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				out.pop();
			}
			other => out.push(other),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::format::{extract_source_root, parse_clipboard};
	use crate::fsutil::write_text_file;
	use crate::settings::{FilterAction, FilterRule, FilterType};
	use std::collections::BTreeMap;

	fn write(path: &Path, content: impl AsRef<[u8]>) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, content).unwrap();
	}

	fn paths(result: &CopyResult) -> Vec<&str> {
		result.files.iter().map(|f| f.path.as_str()).collect()
	}

	fn rule(kind: FilterType, value: &str) -> FilterRule {
		FilterRule {
			kind,
			action: FilterAction::Exclude,
			value: value.into(),
			enabled: true,
		}
	}

	#[test]
	fn copy_folders_recursively_empty_files_included() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("src/main.ts"), "main");
		write(&root.join("src/nested/empty.ts"), "");

		let result = collect_copy_files(
			&[root],
			&[root.join("src")],
			&Settings::default(),
		);

		assert_eq!(result.copied_file_count, 2);
		let mut got = paths(&result);
		got.sort();
		assert_eq!(got, ["src/main.ts", "src/nested/empty.ts"]);
		assert_eq!(result.files[1].content.as_deref(), Some(""));
	}

	#[test]
	fn copy_normalizes_dot_segments_in_input() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("src/a.ts"), "a");
		let input = root.join("src/./../src");

		let result =
			collect_copy_files(&[root], &[input], &Settings::default());

		assert_eq!(paths(&result), ["src/a.ts"]);
	}

	#[test]
	fn copy_prunes_excluded_directories() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("src/main.ts"), "main");
		write(&root.join("node_modules/dep/index.js"), "dep");
		let settings = Settings {
			use_filters: true,
			filter_rules: vec![rule(FilterType::Path, "node_modules")],
			..Settings::default()
		};

		let result = collect_copy_files(&[root], &[root], &settings);

		assert_eq!(result.copied_file_count, 1);
		assert_eq!(result.files[0].path, "src/main.ts");
		assert!(!result.payload.contains("node_modules"));
	}

	#[test]
	fn copy_oversized_files_as_skipped_markers_with_wrappers() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("large.txt"), "x".repeat(1100));
		let settings = Settings {
			pre_text: "<files>".into(),
			post_text: "</files>".into(),
			max_file_size_kb: 1.0,
			..Settings::default()
		};

		let result =
			collect_copy_files(&[root], &[root.join("large.txt")], &settings);

		assert_eq!(result.copied_file_count, 0);
		assert_eq!(result.skipped_file_size_count, 1);
		let base = root.file_name().unwrap().to_string_lossy().into_owned();
		assert_eq!(extract_source_root(&result.payload), Some(base));
		let body = &result.payload[result.payload.find('\n').unwrap() + 1..];
		assert_eq!(
			body,
			"<files>\n// file: large.txt\n\
			 // File skipped: size exceeds limit (1100 bytes)\n\n\
			 // clipcode-end\n</files>"
		);
		assert_eq!(result.stats(), payload_stats(&result.payload));
	}

	#[test]
	fn copy_text_files_through_filters_limits_and_multi_root_paths() {
		let dir = tempfile::tempdir().unwrap();
		let primary = dir.path().join("app");
		let sibling = dir.path().join("shared-lib");
		let settings = Settings {
			use_filters: true,
			filter_rules: vec![rule(FilterType::Pattern, "*.log")],
			..Settings::default()
		};
		let text = |p: PathBuf, c: &str| CopyTextFile {
			absolute_path: p,
			content: c.into(),
			size_bytes: None,
		};

		let result = collect_copy_text_files(
			&[&primary, &sibling],
			&[
				text(primary.join("src/main.ts"), "main"),
				text(primary.join("debug.log"), "debug"),
				text(sibling.join("src/util.ts"), "util"),
			],
			&settings,
		);

		assert_eq!(result.copied_file_count, 2);
		assert_eq!(paths(&result), ["src/main.ts", "shared-lib/src/util.ts"]);
	}

	#[test]
	fn copy_stops_after_limit_without_counting_skipped_markers() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let settings = Settings {
			max_file_size_kb: 1.0,
			file_count_limit: 1.0,
			..Settings::default()
		};
		let text = |name: &str, c: String| CopyTextFile {
			absolute_path: root.join(name),
			content: c,
			size_bytes: None,
		};

		let result = collect_copy_text_files(
			&[root],
			&[
				text("large.txt", "x".repeat(1100)),
				text("a.ts", "a".into()),
				text("b.ts", "b".into()),
			],
			&settings,
		);

		assert_eq!(result.copied_file_count, 1);
		assert_eq!(result.skipped_file_size_count, 1);
		assert!(result.file_limit_reached);
		assert_eq!(paths(&result), ["large.txt", "a.ts"]);
	}

	#[test]
	fn copy_files_limit_trips_only_when_more_files_follow() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("a.ts"), "a");
		write(&root.join("b.ts"), "b");
		let limit = |n: f64| Settings {
			file_count_limit: n,
			..Settings::default()
		};

		let exact = collect_copy_files(&[root], &[root], &limit(2.0));
		assert!(!exact.file_limit_reached);
		assert_eq!(exact.copied_file_count, 2);

		let over = collect_copy_files(&[root], &[root], &limit(1.0));
		assert!(over.file_limit_reached);
		assert_eq!(paths(&over), ["a.ts"]);
	}

	#[cfg(unix)]
	#[test]
	fn copy_one_unreadable_file_does_not_abort_folder() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("src/a.ts"), "a");
		write(&root.join("src/b.ts"), "b");
		std::os::unix::fs::symlink(
			root.join("nowhere"),
			root.join("src/dangling.ts"),
		)
		.unwrap();

		let result = collect_copy_files(
			&[root],
			&[root.join("src")],
			&Settings::default(),
		);

		assert_eq!(paths(&result), ["src/a.ts", "src/b.ts"]);
		assert_eq!(result.copied_file_count, 2);
		assert_eq!(result.skipped_unreadable_count, 1);
	}

	#[cfg(unix)]
	#[test]
	fn copy_walks_selected_directory_symlink_and_stops_cycles() {
		use std::os::unix::fs::symlink;
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("packages/ui/index.ts"), "ui");
		fs::create_dir_all(root.join("node_modules")).unwrap();
		symlink(root.join("packages/ui"), root.join("node_modules/ui"))
			.unwrap();
		symlink(root, root.join("packages/loop")).unwrap();

		let direct = collect_copy_files(
			&[root],
			&[root.join("node_modules/ui")],
			&Settings::default(),
		);
		assert_eq!(paths(&direct), ["node_modules/ui/index.ts"]);

		let whole = collect_copy_files(
			&[root],
			&[root.join("packages")],
			&Settings::default(),
		);
		assert!(paths(&whole).contains(&"packages/ui/index.ts"));
	}

	#[test]
	fn copy_skips_and_counts_non_utf8_file() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		write(&root.join("src/ok.ts"), "ok");
		write(&root.join("src/legacy.txt"), [0xa4, 0xe9, 0xa5, 0xbb]);

		let result = collect_copy_files(
			&[root],
			&[root.join("src")],
			&Settings::default(),
		);

		assert_eq!(paths(&result), ["src/ok.ts"]);
		assert_eq!(result.skipped_unreadable_count, 1);
	}

	fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
		list_files_recursive(root, |_| true)
			.filter_map(|item| match item {
				WalkItem::File(p) => Some(p),
				WalkItem::UnreadableDir(_) => None,
			})
			.map(|p| {
				let rel = p.strip_prefix(root).unwrap();
				(
					rel.to_string_lossy().replace('\\', "/"),
					fs::read(&p).unwrap(),
				)
			})
			.collect()
	}

	// ponytail: restores via parse_clipboard + write_text_file because
	// restore::plan_restore (T-05) is not merged yet; swap it in then.
	#[test]
	fn copy_round_trip_reproduces_file_tree() {
		let src = tempfile::tempdir().unwrap();
		let root = src.path();
		// The wire format drops a trailing newline and normalizes CRLF
		// (a `parse_clipboard` property), so contents avoid both.
		write(&root.join("src/main.ts"), "fn main() {}");
		write(&root.join("src/nested/empty.ts"), "");
		write(&root.join("README.md"), "\u{feff}# title\n// file: fake");
		let settings = Settings {
			pre_text: "PRE".into(),
			post_text: "POST".into(),
			set_max_file_count: false,
			..Settings::default()
		};

		let result = collect_copy_files(&[root], &[root], &settings);
		assert_eq!(result.copied_file_count, 3);

		let dst = tempfile::tempdir().unwrap();
		for entry in parse_clipboard(&result.payload, &settings.header_format) {
			write_text_file(&dst.path().join(&entry.path), &entry.content)
				.unwrap();
		}
		assert_eq!(tree(dst.path()), tree(root));
	}
}
