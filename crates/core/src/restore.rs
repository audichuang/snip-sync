//! Restore planning: turn a parsed payload into file actions.
//!
//! Port of ClipCodeVSCode `src/restore.ts` and `src/restoreBase.ts`
//! (commit 0aa24c8).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::format::{ascii_trim, ChangeType, ParsedEntry};
use crate::fsutil::{delete_file, must_not_overwrite, write_text_file};
use crate::paths::{
	escapes_all_roots, resolve_delete_target, resolve_write_target,
	RejectReason,
};

/// One file from the payload; `parse_clipboard` produces these.
pub type RestoreEntry = ParsedEntry;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CreateOperation {
	pub relative_path: String,
	pub absolute_path: PathBuf,
	pub content: String,
	pub existed: bool,
	/// The root this target was validated against.
	pub root_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOperation {
	pub relative_path: String,
	pub absolute_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SkipReason {
	AlreadyAbsent,
	UnresolvedPath,
	AmbiguousPath,
	PlaceholderBody,
	/// The file on disk is not UTF-8 (or cannot be verified). Writing UTF-8
	/// over it would silently change its encoding.
	NonUtf8Target,
}

impl SkipReason {
	/// The exact TS reason string.
	pub fn as_str(self) -> &'static str {
		match self {
			Self::AlreadyAbsent => "ALREADY_ABSENT",
			Self::UnresolvedPath => "UNRESOLVED_PATH",
			Self::AmbiguousPath => "AMBIGUOUS_PATH",
			Self::PlaceholderBody => "PLACEHOLDER_BODY",
			Self::NonUtf8Target => "NON_UTF8_TARGET",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SkippedOperation {
	pub raw_path: String,
	pub relative_path: Option<String>,
	pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RestorePlan {
	/// Every root the plan was validated against; re-checked before writes.
	pub roots: Vec<PathBuf>,
	pub create_operations: Vec<CreateOperation>,
	pub delete_operations: Vec<DeleteOperation>,
	pub skipped_operations: Vec<SkippedOperation>,
}

/// What the user confirmed. Unchecked operations are indices into the
/// plan's `create_operations` / `delete_operations`; they are not run and
/// not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
pub struct RestoreSelection {
	pub overwrite_existing: bool,
	pub skip_existing: bool,
	pub unchecked_creates: BTreeSet<usize>,
	pub unchecked_deletes: BTreeSet<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RestoreExecutionResult {
	pub created_count: usize,
	pub overwritten_count: usize,
	pub skipped_existing_count: usize,
	pub deleted_count: usize,
	pub errors: Vec<String>,
}

pub fn plan_restore<P: AsRef<Path>>(
	roots: &[P],
	entries: &[RestoreEntry],
) -> RestorePlan {
	let mut plan = RestorePlan {
		roots: roots.iter().map(|r| r.as_ref().to_path_buf()).collect(),
		create_operations: Vec::new(),
		delete_operations: Vec::new(),
		skipped_operations: Vec::new(),
	};

	for entry in entries {
		let skip = |relative_path, reason| SkippedOperation {
			raw_path: entry.path.clone(),
			relative_path,
			reason,
		};

		if entry.change_types.contains(&ChangeType::Deleted) {
			match resolve_delete_target(roots, &entry.path) {
				Ok(t) => plan.delete_operations.push(DeleteOperation {
					relative_path: t.relative_path,
					absolute_path: t.absolute_path,
				}),
				Err(e) => {
					let reason = match e.reason {
						RejectReason::MissingPath => SkipReason::AlreadyAbsent,
						RejectReason::AmbiguousPath => {
							SkipReason::AmbiguousPath
						}
						_ => SkipReason::UnresolvedPath,
					};
					// An UNRESOLVED path carries no relative path, even one
					// refused for escaping through a symlink.
					let rel = if reason == SkipReason::UnresolvedPath {
						None
					} else {
						e.relative_path
					};
					plan.skipped_operations.push(skip(rel, reason));
				}
			}
			continue;
		}

		if is_placeholder_body(&entry.content) {
			plan.skipped_operations
				.push(skip(None, SkipReason::PlaceholderBody));
			continue;
		}

		let t = match resolve_write_target(roots, &entry.path) {
			Ok(t) => t,
			Err(e) => {
				let op = if e.reason == RejectReason::AmbiguousPath {
					skip(e.relative_path, SkipReason::AmbiguousPath)
				} else {
					skip(None, SkipReason::UnresolvedPath)
				};
				plan.skipped_operations.push(op);
				continue;
			}
		};

		if must_not_overwrite(&t.absolute_path) {
			plan.skipped_operations
				.push(skip(Some(t.relative_path), SkipReason::NonUtf8Target));
			continue;
		}

		let existed = t.existed || t.absolute_path.exists();
		plan.create_operations.push(CreateOperation {
			relative_path: t.relative_path,
			absolute_path: t.absolute_path,
			content: entry.content.clone(),
			existed,
			root_path: t.root_path,
		});
	}

	plan
}

/// The copy side substitutes a one-line comment for a file it could not
/// embed. Only the FIRST line is tested, so trailing noise (a footer, a
/// stray line) cannot disarm the guard. Accepted false positive: a real file
/// whose first line is one of these markers is skipped.
fn is_placeholder_body(content: &str) -> bool {
	let body = ascii_trim(content);
	let first = ascii_trim(body.split('\n').next().unwrap_or(""));
	first.starts_with("// File skipped: ")
		|| first == "// Unable to read file content"
		|| first == "// Error reading file content"
}

/// Runs the confirmed part of `plan`. Containment and encoding are
/// re-checked right before each write: the filesystem may have changed
/// while the user was looking at the preview.
///
/// Operations run serially in plan order, so ops that depend on each other
/// (see [`has_path_dependencies`]) are always deterministic.
pub fn execute_restore_plan(
	plan: &RestorePlan,
	selection: &RestoreSelection,
) -> RestoreExecutionResult {
	let mut result = RestoreExecutionResult::default();

	for (i, op) in plan.create_operations.iter().enumerate() {
		if selection.unchecked_creates.contains(&i) {
			continue;
		}
		// The plan's FULL root set, not only the op's own root.
		match run_create(&plan.roots, selection, op) {
			Ok(CreateOutcome::Created) => result.created_count += 1,
			Ok(CreateOutcome::Overwritten) => result.overwritten_count += 1,
			Ok(CreateOutcome::Skipped) => result.skipped_existing_count += 1,
			Err(message) => result.errors.push(message),
		}
	}

	for (i, op) in plan.delete_operations.iter().enumerate() {
		if selection.unchecked_deletes.contains(&i) {
			continue;
		}
		// A symlink appearing between plan and execute must not turn a
		// contained target into one outside the workspace.
		if escapes_all_roots(&plan.roots, &op.absolute_path) {
			result
				.errors
				.push(format!("{}: unsafe path", op.relative_path));
			continue;
		}
		if op.absolute_path.exists() {
			match delete_file(&op.absolute_path) {
				Ok(()) => result.deleted_count += 1,
				Err(e) => {
					result.errors.push(format!("{}: {e}", op.relative_path))
				}
			}
		}
	}

	result
}

enum CreateOutcome {
	Created,
	Overwritten,
	Skipped,
}

fn run_create(
	roots: &[PathBuf],
	selection: &RestoreSelection,
	op: &CreateOperation,
) -> Result<CreateOutcome, String> {
	if escapes_all_roots(roots, &op.absolute_path) {
		return Err(format!("{}: unsafe path", op.relative_path));
	}
	if must_not_overwrite(&op.absolute_path) {
		return Ok(CreateOutcome::Skipped);
	}
	let outcome = match fs::metadata(&op.absolute_path) {
		Ok(info) if info.is_dir() => return Ok(CreateOutcome::Skipped),
		Ok(_) if selection.skip_existing || !selection.overwrite_existing => {
			return Ok(CreateOutcome::Skipped)
		}
		Ok(_) => CreateOutcome::Overwritten,
		Err(_) => CreateOutcome::Created,
	};
	write_text_file(&op.absolute_path, &op.content)
		.map_err(|e| format!("{}: {e}", op.relative_path))?;
	Ok(outcome)
}

/// True if any path equals another, or is an ancestor directory of another
/// (a file at `src` plus a file at `src/a.ts`): such outcomes depend on
/// order. `src` vs `srcfoo` does not conflict.
pub fn has_path_dependencies<P: AsRef<Path>>(paths: &[P]) -> bool {
	let paths: Vec<String> = paths
		.iter()
		.map(|p| p.as_ref().to_string_lossy().into_owned())
		.collect();
	let set: HashSet<&str> = paths.iter().map(String::as_str).collect();
	if set.len() != paths.len() {
		return true;
	}
	paths.iter().any(|p| {
		p.char_indices()
			.any(|(i, c)| (c == '/' || c == '\\') && set.contains(&p[..i]))
	})
}

// ---- restoreBase: "off by one folder level" detection ----

/// A one-level offset applied to every relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RestoreBase {
	/// Drop a redundant leading `segment/`.
	Strip { segment: String },
	/// Nest everything under an existing `prefix/`.
	Add { prefix: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RestoreBaseSuggestion {
	pub base: RestoreBase,
	/// Human description for the confirmation prompt.
	pub label: String,
	/// Subdir-bearing files that land in an existing dir under this base.
	pub matched: usize,
	/// Subdir-bearing files considered.
	pub total: usize,
}

/// Directory lookups, as slash-joined path strings (as in TS).
pub trait DirProbe {
	fn is_dir(&self, abs_path: &str) -> bool;
	fn child_dirs(&self, root_abs_path: &str) -> Vec<String>;
}

pub fn apply_restore_base(base: &RestoreBase, relative_path: &str) -> String {
	match base {
		RestoreBase::Add { prefix } => format!("{prefix}/{relative_path}"),
		RestoreBase::Strip { segment } => match relative_path.split_once('/') {
			Some((first, rest)) if first == segment => rest.to_owned(),
			_ => relative_path.to_owned(),
		},
	}
}

/// Rejects POSIX absolutes, Windows drive paths and UNC paths.
fn is_relative(p: &str) -> bool {
	let b = p.as_bytes();
	let drive = b.len() >= 3
		&& b[0].is_ascii_alphabetic()
		&& b[1] == b':'
		&& (b[2] == b'/' || b[2] == b'\\');
	!p.is_empty() && !p.starts_with('/') && !drive && !p.starts_with('\\')
}

fn join_path(root: &str, rel: &str) -> String {
	format!("{}/{rel}", root.trim_end_matches('/'))
}

fn base_name_of(p: &str) -> String {
	let p = p.replace('\\', "/");
	p.trim_end_matches('/')
		.rsplit('/')
		.next()
		.unwrap_or("")
		.to_owned()
}

/// True when `rel`'s parent directory already exists under `primary_root`.
fn parent_exists(primary_root: &str, rel: &str, probe: &dyn DirProbe) -> bool {
	rel.rfind('/')
		.is_some_and(|i| probe.is_dir(&join_path(primary_root, &rel[..i])))
}

fn first_segments<'a>(multi: &[&'a str]) -> BTreeSet<&'a str> {
	multi
		.iter()
		.map(|p| p.split_once('/').map_or(*p, |(first, _)| first))
		.collect()
}

fn suggest_from_source_root(
	primary_root: &str,
	rels: &[&str],
	probe: &dyn DirProbe,
	source_root: &str,
) -> Option<RestoreBaseSuggestion> {
	if rels.is_empty() {
		return None;
	}
	let target_name = base_name_of(primary_root);
	// Same-named root: the paths are already anchored correctly.
	if target_name == source_root {
		return None;
	}
	let all = |base, label| {
		Some(RestoreBaseSuggestion {
			base,
			label,
			matched: rels.len(),
			total: rels.len(),
		})
	};

	let multi: Vec<&str> =
		rels.iter().copied().filter(|p| p.contains('/')).collect();
	let firsts = first_segments(&multi);
	// Relocating is only safe when the paths do NOT already land here: in a
	// flat-layout repo (proj/proj) the same-named folder always exists.
	let already_anchored =
		rels.iter().any(|p| parent_exists(primary_root, p, probe));

	// Copied from the parent: strip this repo's own folder name.
	if !multi.is_empty()
		&& firsts.len() == 1
		&& firsts.first() == Some(&target_name.as_str())
	{
		if already_anchored {
			return None;
		}
		let label = format!("remove the leading \"{target_name}/\"");
		return all(
			RestoreBase::Strip {
				segment: target_name,
			},
			label,
		);
	}

	// Copied with the repo as root, and that folder exists here.
	if probe.is_dir(&format!(
		"{}/{source_root}",
		primary_root.trim_end_matches('/')
	)) {
		if already_anchored {
			return None;
		}
		let label = format!("place everything under \"{source_root}/\"");
		let prefix = source_root.to_owned();
		return all(RestoreBase::Add { prefix }, label);
	}

	// A genuinely different location: don't guess.
	None
}

pub fn suggest_restore_base(
	primary_root: &str,
	relative_paths: &[String],
	probe: &dyn DirProbe,
	source_root: Option<&str>,
) -> Option<RestoreBaseSuggestion> {
	let rels: Vec<&str> = relative_paths
		.iter()
		.map(String::as_str)
		.filter(|p| is_relative(p))
		.collect();

	// Source-root metadata aligns deterministically, even for one file.
	if let Some(source_root) = source_root.filter(|s| !s.is_empty()) {
		return suggest_from_source_root(
			primary_root,
			&rels,
			probe,
			source_root,
		);
	}

	// The heuristic needs >= 2 subdir-bearing paths so one coincidental path
	// cannot drive a relocation.
	let multi: Vec<&str> =
		rels.iter().copied().filter(|p| p.contains('/')).collect();
	if multi.len() < 2 {
		return None;
	}

	let score = |base: Option<&RestoreBase>| {
		multi
			.iter()
			.filter(|p| match base {
				Some(b) => parent_exists(
					primary_root,
					&apply_restore_base(b, p),
					probe,
				),
				None => parent_exists(primary_root, p, probe),
			})
			.count()
	};
	let identity_score = score(None);

	let mut candidates: Vec<(RestoreBase, usize, String)> = Vec::new();

	// strip: every subdir-bearing path shares one leading segment AND it is
	// the workspace folder's own name (not a legit top folder like
	// `examples/`).
	let firsts = first_segments(&multi);
	if firsts.len() == 1
		&& firsts.first() == Some(&base_name_of(primary_root).as_str())
	{
		let segment = firsts.first().unwrap().to_string();
		let label = format!("remove the leading \"{segment}/\"");
		let base = RestoreBase::Strip { segment };
		candidates.push((base.clone(), score(Some(&base)), label));
	}

	// add: nest under each directory that already exists at the root.
	for prefix in probe.child_dirs(primary_root) {
		let label = format!("place everything under \"{prefix}/\"");
		let base = RestoreBase::Add { prefix };
		candidates.push((base.clone(), score(Some(&base)), label));
	}
	if candidates.is_empty() {
		return None;
	}

	// The winning score must be unique, except that a basename-anchored
	// strip may win a tie since it is unambiguous.
	let max_score = candidates.iter().map(|c| c.1).max().unwrap_or(0);
	let top: Vec<_> = candidates
		.into_iter()
		.filter(|c| c.1 == max_score)
		.collect();
	let best = if top.len() == 1 {
		top.into_iter().next()
	} else {
		top.into_iter()
			.find(|c| matches!(c.0, RestoreBase::Strip { .. }))
	}?;

	// Only a confident offset: beat identity and match a majority.
	if best.1 <= identity_score || best.1 * 2 < multi.len() {
		return None;
	}
	Some(RestoreBaseSuggestion {
		base: best.0,
		label: best.2,
		matched: best.1,
		total: multi.len(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::format::PayloadFile;
	use crate::format::{build_payload, parse_clipboard, BuildPayloadOptions};

	const HEADER: &str = "// file: $FILE_PATH";

	fn entry(path: &str, content: &str) -> RestoreEntry {
		RestoreEntry {
			path: path.into(),
			content: content.into(),
			change_types: BTreeSet::new(),
		}
	}

	fn deleted(path: &str) -> RestoreEntry {
		RestoreEntry {
			change_types: [ChangeType::Deleted].into(),
			..entry(path, "")
		}
	}

	fn tmp() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		(dir, root)
	}

	fn overwrite() -> RestoreSelection {
		RestoreSelection {
			overwrite_existing: true,
			..Default::default()
		}
	}

	fn rels(plan: &RestorePlan) -> Vec<&str> {
		plan.create_operations
			.iter()
			.map(|o| o.relative_path.as_str())
			.collect()
	}

	fn read(p: PathBuf) -> String {
		fs::read_to_string(p).unwrap()
	}

	#[test]
	fn restore_unmatched_absolute_paths_keep_their_directory_tree() {
		let (_d, root) = tmp();
		let paths = [
			(
				r"D:\Users\author\.m2\repository\library.jar!\com\example\Library$Inner.java",
				"D/Users/author/.m2/repository/library.jar!/com/example/Library$Inner.java",
			),
			("/foreign/checkout/src/New.kt", "foreign/checkout/src/New.kt"),
			(r"\\server\share\arbitrary\New.txt", "server/share/arbitrary/New.txt"),
		];
		let payload: Vec<String> = paths
			.iter()
			.enumerate()
			.map(|(i, (src, _))| format!("// file: {src}\ncontent-{i}"))
			.collect();
		let entries = parse_clipboard(&payload.join("\n"), HEADER);
		let plan = plan_restore(&[&root], &entries);
		assert_eq!(rels(&plan), paths.map(|p| p.1));
		assert!(plan.skipped_operations.is_empty());
		let result = execute_restore_plan(&plan, &RestoreSelection::default());
		assert_eq!(result.created_count, paths.len());
		assert!(result.errors.is_empty());
		for (i, (_, rel)) in paths.iter().enumerate() {
			assert_eq!(read(root.join(rel)), format!("content-{i}"));
		}
	}

	#[test]
	fn restore_creates_skips_overwrites_and_deletes() {
		let (_d, root) = tmp();
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src/old.ts"), "old").unwrap();
		fs::write(root.join("src/delete.ts"), "delete").unwrap();
		let entries = [
			entry("src/new.ts", "new"),
			entry("src/old.ts", "updated"),
			deleted("src/delete.ts"),
			entry("../bad.ts", "bad"),
		];
		let plan = plan_restore(&[&root], &entries);
		assert_eq!(plan.create_operations.len(), 2);
		assert_eq!(plan.delete_operations.len(), 1);
		assert_eq!(plan.skipped_operations.len(), 1);

		let only_creates = RestoreSelection {
			skip_existing: true,
			unchecked_deletes: [0].into(),
			..Default::default()
		};
		let skipped = execute_restore_plan(&plan, &only_creates);
		assert_eq!(skipped.created_count, 1);
		assert_eq!(skipped.skipped_existing_count, 1);
		assert_eq!(skipped.deleted_count, 0);
		assert_eq!(read(root.join("src/old.ts")), "old");
		assert!(root.join("src/delete.ts").exists());

		let overwrite_plan = plan_restore(&[&root], &entries[1..2]);
		let overwritten = execute_restore_plan(&overwrite_plan, &overwrite());
		assert_eq!(overwritten.overwritten_count, 1);
		assert_eq!(read(root.join("src/old.ts")), "updated");

		let deletes = RestorePlan {
			create_operations: vec![],
			..plan
		};
		let r = execute_restore_plan(&deletes, &RestoreSelection::default());
		assert_eq!(r.deleted_count, 1);
		assert!(!root.join("src/delete.ts").exists());
	}

	#[test]
	fn restore_unchecked_create_is_not_written() {
		let (_d, root) = tmp();
		let plan =
			plan_restore(&[&root], &[entry("a.ts", "a"), entry("b.ts", "b")]);
		let selection = RestoreSelection {
			unchecked_creates: [0].into(),
			..Default::default()
		};
		let r = execute_restore_plan(&plan, &selection);
		assert_eq!(r.created_count, 1);
		assert!(!root.join("a.ts").exists());
		assert_eq!(read(root.join("b.ts")), "b");
	}

	#[test]
	fn restore_large_batch_with_one_existing_file() {
		let (_d, root) = tmp();
		let entries: Vec<_> = (0..50)
			.map(|i| {
				entry(&format!("src/file-{i}.ts"), &format!("content-{i}"))
			})
			.collect();
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src/file-0.ts"), "stale").unwrap();
		let plan = plan_restore(&[&root], &entries);
		assert_eq!(plan.create_operations.len(), 50);
		let r = execute_restore_plan(&plan, &overwrite());
		assert_eq!(r.created_count, 49);
		assert_eq!(r.overwritten_count, 1);
		assert_eq!(r.skipped_existing_count, 0);
		assert!(r.errors.is_empty());
		for i in 0..50 {
			assert_eq!(
				read(root.join(format!("src/file-{i}.ts"))),
				format!("content-{i}")
			);
		}
	}

	#[test]
	fn restore_duplicate_path_creates_then_overwrites() {
		let (_d, root) = tmp();
		let plan = plan_restore(
			&[&root],
			&[entry("src/dup.ts", "first"), entry("src/dup.ts", "second")],
		);
		assert_eq!(plan.create_operations.len(), 2);
		let r = execute_restore_plan(&plan, &overwrite());
		assert_eq!(r.created_count, 1);
		assert_eq!(r.overwritten_count, 1);
		assert!(r.errors.is_empty());
		assert_eq!(read(root.join("src/dup.ts")), "second");
	}

	#[test]
	fn restore_has_path_dependencies_boundaries() {
		assert!(!has_path_dependencies(&["/r/a.ts", "/r/b.ts"]));
		assert!(has_path_dependencies(&["/r/a.ts", "/r/a.ts"]));
		assert!(has_path_dependencies(&["/r/src", "/r/src/a.ts"]));
		assert!(!has_path_dependencies(&["/r/src", "/r/srcfoo"]));
		assert!(has_path_dependencies(&["/r/a/b/c.ts", "/r/a"]));
	}

	#[test]
	fn restore_sibling_root_labels_and_ambiguous_legacy_paths() {
		let (_d, parent) = tmp();
		let primary = parent.join("app");
		let sibling = parent.join("shared-lib");
		fs::create_dir_all(primary.join("src")).unwrap();
		fs::create_dir_all(sibling.join("src")).unwrap();
		fs::write(primary.join("src/same.ts"), "primary").unwrap();
		fs::write(sibling.join("src/same.ts"), "sibling").unwrap();
		let plan = plan_restore(
			&[&primary, &sibling],
			&[
				entry("shared-lib/src/new.ts", "new"),
				entry("src/same.ts", "ambiguous"),
			],
		);
		assert_eq!(plan.create_operations.len(), 1);
		assert_eq!(
			plan.create_operations[0].absolute_path,
			sibling.join("src").join("new.ts")
		);
		assert_eq!(plan.skipped_operations.len(), 1);
		assert_eq!(
			plan.skipped_operations[0].reason,
			SkipReason::AmbiguousPath
		);
	}

	#[test]
	fn restore_placeholder_bodies_are_skipped() {
		let (_d, root) = tmp();
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src/big.log"), "the real 1.2 MB file").unwrap();
		let plan = plan_restore(
			&[&root],
			&[
				entry(
					"src/big.log",
					"// File skipped: size exceeds limit (1234567 bytes)",
				),
				entry("src/unreadable.ts", "// Unable to read file content"),
				entry("src/failed.ts", "// Error reading file content"),
				// Trailing noise must not disarm the first-line guard.
				entry(
					"src/real.ts",
					"// File skipped: size exceeds limit (1 bytes)\nbut there is real content too",
				),
			],
		);
		assert!(plan.create_operations.is_empty());
		assert_eq!(plan.skipped_operations.len(), 4);
		assert!(plan
			.skipped_operations
			.iter()
			.all(|o| o.reason == SkipReason::PlaceholderBody));
		execute_restore_plan(&plan, &overwrite());
		assert_eq!(read(root.join("src/big.log")), "the real 1.2 MB file");
	}

	#[test]
	fn restore_footer_does_not_turn_placeholder_into_content() {
		let (_d, root) = tmp();
		fs::create_dir_all(root.join("src")).unwrap();
		let original = "x".repeat(1100);
		fs::write(root.join("src/large.txt"), &original).unwrap();
		let post_text = "</files>";
		let payload = build_payload(&BuildPayloadOptions {
			header_format: HEADER.into(),
			post_text: post_text.into(),
			add_extra_line_between_files: true,
			files: vec![PayloadFile {
				path: "src/large.txt".into(),
				skipped_reason: Some("size exceeds limit (1100 bytes)".into()),
				..Default::default()
			}],
			..Default::default()
		});
		assert!(payload
			.contains("// File skipped: size exceeds limit (1100 bytes)"));
		assert!(payload.trim_end().ends_with(post_text));

		for (name, text) in [
			("as built", payload.clone()),
			("trailing newline", format!("{payload}\n")),
			("CRLF", payload.replace('\n', "\r\n")),
		] {
			let parsed = parse_clipboard(&text, HEADER);
			assert_eq!(parsed.len(), 1, "{name}");
			assert!(!parsed[0].content.contains(post_text), "{name}");
		}

		// A legacy payload without an end marker glues the footer on; the
		// guard must still hold.
		let legacy: Vec<&str> = payload
			.split('\n')
			.filter(|l| *l != "// clipcode-end")
			.collect();
		let legacy_entries = parse_clipboard(&legacy.join("\n"), HEADER);
		assert!(legacy_entries[0].content.contains(post_text));
		let legacy_plan = plan_restore(&[&root], &legacy_entries);
		assert!(legacy_plan.create_operations.is_empty());
		assert_eq!(
			legacy_plan.skipped_operations[0].reason,
			SkipReason::PlaceholderBody
		);

		let plan = plan_restore(&[&root], &parse_clipboard(&payload, HEADER));
		assert!(plan.create_operations.is_empty());
		assert_eq!(
			plan.skipped_operations[0].reason,
			SkipReason::PlaceholderBody
		);
		execute_restore_plan(&plan, &overwrite());
		assert_eq!(read(root.join("src/large.txt")), original);
	}

	#[test]
	fn restore_footer_is_not_appended_to_the_last_file() {
		let (_d, root) = tmp();
		let payload = build_payload(&BuildPayloadOptions {
			header_format: HEADER.into(),
			pre_text: "HEADER NOTE".into(),
			post_text: "Please review the code above.".into(),
			add_extra_line_between_files: true,
			files: ["A", "B"]
				.map(|n| PayloadFile {
					path: format!("src/{n}.ts"),
					content: Some(format!("class {n}")),
					..Default::default()
				})
				.into(),
			..Default::default()
		});
		let entries = parse_clipboard(&payload, HEADER);
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[1].content, "class B");
		let plan = plan_restore(&[&root], &entries);
		execute_restore_plan(&plan, &overwrite());
		assert_eq!(read(root.join("src/B.ts")), "class B");
	}

	#[test]
	fn restore_never_overwrites_non_utf8_target() {
		let (_d, root) = tmp();
		let big5 = [0xa4, 0xe9, 0xa5, 0xbb, 0x0a];
		fs::write(root.join("legacy.txt"), big5).unwrap();
		fs::write(root.join("plain.txt"), "ascii\n").unwrap();
		let plan = plan_restore(
			&[&root],
			&[
				entry("legacy.txt", "replacement"),
				entry("plain.txt", "replacement"),
			],
		);
		assert_eq!(rels(&plan), ["plain.txt"]);
		assert_eq!(
			plan.skipped_operations[0].reason,
			SkipReason::NonUtf8Target
		);
		execute_restore_plan(&plan, &overwrite());
		assert_eq!(fs::read(root.join("legacy.txt")).unwrap(), big5);
		assert_eq!(read(root.join("plain.txt")), "replacement");
	}

	#[cfg(unix)]
	#[test]
	fn restore_unverifiable_target_is_not_overwritten() {
		use std::os::unix::fs::PermissionsExt;
		let (_d, root) = tmp();
		let target = root.join("unreadable.bin");
		fs::write(&target, [0xff, 0xfe, 0x41, 0x00]).unwrap();
		fs::set_permissions(&target, fs::Permissions::from_mode(0o200))
			.unwrap();
		// Root can read anything; the check is meaningless there.
		if fs::read(&target).is_ok() {
			return;
		}
		let plan = plan_restore(&[&root], &[entry("unreadable.bin", "x")]);
		fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
			.unwrap();
		assert!(plan.create_operations.is_empty());
		assert_eq!(
			plan.skipped_operations[0].reason,
			SkipReason::NonUtf8Target
		);
	}

	#[test]
	fn restore_encoding_is_rechecked_at_write_time() {
		let (_d, root) = tmp();
		let target = root.join("race.txt");
		fs::write(&target, "original").unwrap();
		let plan = plan_restore(&[&root], &[entry("race.txt", "replacement")]);
		assert_eq!(plan.create_operations.len(), 1);
		let big5 = [0xa4, 0xe9, 0xa5, 0xbb];
		fs::write(&target, big5).unwrap();
		execute_restore_plan(&plan, &overwrite());
		assert_eq!(fs::read(&target).unwrap(), big5);
	}

	#[cfg(unix)]
	#[test]
	fn restore_containment_is_rechecked_at_write_time() {
		let (_d, parent) = tmp();
		let root = parent.join("root");
		let outside = parent.join("outside");
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(&outside).unwrap();
		let plan = plan_restore(&[&root], &[entry("sub/x.txt", "x")]);
		assert_eq!(plan.create_operations.len(), 1);
		std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
		let r = execute_restore_plan(&plan, &overwrite());
		assert_eq!(r.errors, ["sub/x.txt: unsafe path"]);
		assert!(!outside.join("x.txt").exists());
	}

	// ---- restoreBase ----

	struct Probe {
		dirs: HashSet<String>,
		children: Vec<String>,
	}

	impl DirProbe for Probe {
		fn is_dir(&self, p: &str) -> bool {
			self.dirs.contains(p.trim_end_matches('/'))
		}
		fn child_dirs(&self, _: &str) -> Vec<String> {
			self.children.clone()
		}
	}

	fn probe(dirs: &[&str], children: &[&str]) -> Probe {
		Probe {
			dirs: dirs.iter().map(|s| s.to_string()).collect(),
			children: children.iter().map(|s| s.to_string()).collect(),
		}
	}

	fn suggest(
		root: &str,
		paths: &[&str],
		p: &Probe,
		source_root: Option<&str>,
	) -> Option<RestoreBaseSuggestion> {
		let paths: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
		suggest_restore_base(root, &paths, p, source_root)
	}

	fn add(prefix: &str) -> RestoreBase {
		RestoreBase::Add {
			prefix: prefix.into(),
		}
	}

	fn strip(segment: &str) -> RestoreBase {
		RestoreBase::Strip {
			segment: segment.into(),
		}
	}

	const REPO: &str = "inv-svc-console";

	#[test]
	fn restore_base_apply_adds_and_strips() {
		assert_eq!(
			apply_restore_base(&add("repo"), "src/a.ts"),
			"repo/src/a.ts"
		);
		assert_eq!(
			apply_restore_base(&strip("repo"), "repo/src/a.ts"),
			"src/a.ts"
		);
		assert_eq!(apply_restore_base(&strip("repo"), "src/a.ts"), "src/a.ts");
	}

	#[test]
	fn restore_base_suggests_adding_missing_wrapper() {
		let p = probe(
			&[
				"/work/inv-svc-console",
				"/work/inv-svc-console/src",
				"/work/inv-svc-console/lib",
			],
			&[REPO],
		);
		let s =
			suggest("/work", &["src/a.ts", "src/b.ts", "lib/c.ts"], &p, None)
				.unwrap();
		assert_eq!(s.base, add(REPO));
		assert_eq!(s.matched, 3);
	}

	#[test]
	fn restore_base_suggests_stripping_redundant_wrapper() {
		let p = probe(
			&["/work/inv-svc-console/src", "/work/inv-svc-console/lib"],
			&["src", "lib"],
		);
		let s = suggest(
			"/work/inv-svc-console",
			&["inv-svc-console/src/a.ts", "inv-svc-console/lib/c.ts"],
			&p,
			None,
		)
		.unwrap();
		assert_eq!(s.base, strip(REPO));
	}

	#[test]
	fn restore_base_no_suggestion_cases() {
		let aligned = probe(&["/work/src", "/work/lib"], &["src", "lib"]);
		assert!(suggest("/work", &["src/a.ts", "lib/c.ts"], &aligned, None)
			.is_none());
		assert!(suggest(
			"/work",
			&["src/a.ts", "lib/c.ts"],
			&probe(&[], &[]),
			None
		)
		.is_none());
		let repo = probe(&["/work/inv-svc-console"], &[REPO]);
		assert!(suggest("/work", &["a.ts", "README.md"], &repo, None).is_none());
		// Ambiguous add: two child dirs match equally well.
		let two = probe(
			&[
				"/work/repo-a",
				"/work/repo-a/src",
				"/work/repo-b",
				"/work/repo-b/src",
			],
			&["repo-a", "repo-b"],
		);
		assert!(
			suggest("/work", &["src/a.ts", "src/b.ts"], &two, None).is_none()
		);
		// A top folder not named like the workspace is never stripped.
		assert!(suggest(
			"/work",
			&["examples/src/a.ts", "examples/lib/b.ts"],
			&aligned,
			None
		)
		.is_none());
		let one = probe(
			&["/work/inv-svc-console", "/work/inv-svc-console/src"],
			&[REPO],
		);
		assert!(suggest("/work", &["src/a.ts"], &one, None).is_none());
		// Majority required.
		assert!(suggest(
			"/work",
			&["src/a.ts", "lib/b.ts", "app/c.ts", "web/d.ts"],
			&one,
			None
		)
		.is_none());
	}

	#[test]
	fn restore_base_metadata_cases() {
		let p = probe(&["/work/inv-svc-console"], &[REPO, "other"]);
		assert_eq!(
			suggest("/work", &["src/a.ts", "lib/b.ts"], &p, Some(REPO))
				.unwrap()
				.base,
			add(REPO)
		);
		let p = probe(&["/work/inv-svc-console"], &[REPO]);
		assert_eq!(
			suggest("/work", &["src/a.ts"], &p, Some(REPO))
				.unwrap()
				.base,
			add(REPO)
		);
		assert_eq!(
			suggest("/work", &["README.md"], &p, Some(REPO))
				.unwrap()
				.base,
			add(REPO)
		);
		let s = suggest(
			"/work/inv-svc-console",
			&["inv-svc-console/src/a.ts", "inv-svc-console/lib/b.ts"],
			&probe(&[], &[]),
			Some("work"),
		);
		assert_eq!(s.unwrap().base, strip(REPO));
		let same = probe(&["/work/inv-svc-console/src"], &["src"]);
		assert!(suggest(
			"/work/inv-svc-console",
			&["src/a.ts", "lib/b.ts"],
			&same,
			Some(REPO)
		)
		.is_none());
		let bar = probe(&["/work/bar/src"], &["src"]);
		assert!(suggest(
			"/work/bar",
			&["src/a.ts", "lib/b.ts"],
			&bar,
			Some("foo")
		)
		.is_none());
		// Flat-layout repo: already-anchored paths are left alone.
		let flat = probe(
			&[
				"/t/mypkg-2/mypkg",
				"/t/mypkg-2/mypkg/sub",
				"/t/mypkg-2/tests",
			],
			&["mypkg", "tests"],
		);
		assert!(suggest(
			"/t/mypkg-2",
			&["mypkg/sub/core.py", "tests/test_core.py", "setup.py"],
			&flat,
			Some("mypkg")
		)
		.is_none());
	}
}
