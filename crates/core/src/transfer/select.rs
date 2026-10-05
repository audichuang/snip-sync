use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::gitrun::{CancelToken, RunOptions};
use crate::paths;
use crate::settings::Settings;
use crate::transfer::{
	CanonicalRootId, ExportItem, ExportPlan, ExportSelection, SourceKind,
	TransferError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderExpansion {
	pub sel: ExportSelection,
	/// Walked files the payload cannot carry (see `folder_file_rel`).
	pub skipped: usize,
	/// The walk stopped at the file limit with files left.
	pub truncated: bool,
	/// When `truncated` is true, the item count in `sel.items` at the moment
	/// truncation occurred. Items beyond this index are subsequent picked items
	/// that sort after the truncated folder, and must not be planned until the
	/// folder is fully expanded or the prefix alone satisfies the file limit.
	pub truncated_at: Option<usize>,
}

/// The payload path of a walked file, or None when the export would refuse
/// it: a name a header cannot carry (`< > : " | ? *`, control characters,
/// a trailing space, `\` on Unix), non-UTF-8, a dangling or out-of-root
/// symlink, a FIFO/socket/device, or a file this user cannot open.
pub(crate) fn folder_file_rel(root: &Path, path: &Path) -> Option<String> {
	let rel = path
		.strip_prefix(root)
		.ok()?
		.components()
		.map(|c| c.as_os_str().to_str())
		.collect::<Option<Vec<_>>>()?
		.join("/");
	if !paths::is_exportable_relative_path(&rel)
		|| !std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
		|| paths::escapes_all_roots(&[root], path)
		|| std::fs::File::open(path).is_err()
	{
		return None;
	}
	Some(rel)
}

/// A symlink at `path` that a browser or a copy may treat as a folder: it
/// resolves to a directory inside `root`, and not to `.git` or inside one.
pub fn is_safe_dir_symlink(root: &Path, path: &Path) -> bool {
	if let Ok(meta) = std::fs::metadata(path) {
		if meta.is_dir() {
			if let Ok(canonical) = dunce::canonicalize(path) {
				return !paths::escapes_all_roots(&[root], &canonical)
					&& !canonical
						.components()
						.any(|c| c.as_os_str() == ".git");
			}
		}
	}
	false
}

/// What one copy produced: the payload and the counts its status line
/// reports. `copied == 0` means nothing could be copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyOutcome {
	pub payload: String,
	pub copied: usize,
	/// UTF-16 code units of the payload.
	pub chars: usize,
	pub lines: usize,
	/// Files left out: unreadable, over the size limit, or a name a folder
	/// walk could not carry.
	pub skipped: usize,
	/// The file limit cut the copy short.
	pub truncated: bool,
}

/// [`CopyOutcome`] plus the detail a local CLI prints: per-file skip
/// reasons and the size/unreadable split its notes report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReport {
	pub outcome: CopyOutcome,
	pub files: Vec<crate::format::PayloadFile>,
	pub skipped_file_size: usize,
	/// Plan-level unreadable plus the folder walk's refusals.
	pub skipped_unreadable: usize,
	pub file_limit_reached: bool,
}

/// The copy engine for a selection, shared by the desktop app, a remote
/// worker and the local CLI: expands folders in progressively doubling
/// batches starting at `file_limit` (so files a filter later excludes
/// cannot hide the eligible ones behind the first batch, exactly as
/// [`plan_export_expanding`] does), in input order — the TS copy layout —
/// plans the payload (at most [`super::CLIPBOARD_PAYLOAD_MAX`]), runs
/// `hold` (an e2e hook), and checks the sources did not change meanwhile.
pub fn copy_selection(
	sel: ExportSelection,
	settings: &Settings,
	file_limit: usize,
	opts: &RunOptions,
	hold: impl FnOnce(&ExportPlan),
) -> Result<CopyOutcome, TransferError> {
	match copy_selection_detailed(sel, settings, file_limit, opts, hold) {
		Ok(report) => Ok(report.outcome),
		// Every file under the folders was skipped: the wire answer is an
		// empty copy with one skip, not an error.
		Err(TransferError::EmptySelection) => Ok(CopyOutcome {
			payload: String::new(),
			copied: 0,
			chars: 0,
			lines: 0,
			skipped: 1,
			truncated: false,
		}),
		Err(err) => Err(err),
	}
}

/// [`copy_selection`] with the detail a local CLI prints: the per-file
/// skip reasons and the size/unreadable split its notes report.
/// [`TransferError::EmptySelection`] reaches the caller, so a surface can
/// say「every file in the selected folders was skipped」its own way.
pub fn copy_selection_detailed(
	sel: ExportSelection,
	settings: &Settings,
	file_limit: usize,
	opts: &RunOptions,
	hold: impl FnOnce(&ExportPlan),
) -> Result<CopyReport, TransferError> {
	let cancel = opts.cancel.clone().unwrap_or_default();
	// Expands in doubling batches, as `plan_export_expanding` plans: when
	// a batch truncates, the prefix alone answers if the file limit is
	// already reached; otherwise the next batch doubles, because the items
	// this prefix plans away (a filter's exclusions) say nothing about
	// what the rest of the folder holds.
	let mut limit = file_limit;
	let (expanded, plan) = loop {
		let expanded =
			expand_folder_items_in_input_order(sel.clone(), limit, &cancel)?;
		if !expanded.truncated {
			let plan = super::plan_export_with(
				&expanded.sel,
				settings,
				Some(super::CLIPBOARD_PAYLOAD_MAX),
				opts,
			)?;
			break (expanded, plan);
		}
		let trunc_idx =
			expanded.truncated_at.unwrap_or(expanded.sel.items.len());
		let prefix_items = expanded.sel.items[..trunc_idx].to_vec();
		let roots = sel.roots.iter().map(|r| r.path().to_path_buf()).collect();
		let primary_root =
			sel.primary_root.as_ref().map(|r| r.path().to_path_buf());
		match ExportSelection::new(roots, primary_root, prefix_items) {
			Ok(prefix_sel) => {
				let prefix_sel = prefix_sel
					.with_source_root(sel.source_root.clone())
					.with_spelled_root(sel.spelled_root.clone());
				match super::plan_export_with(
					&prefix_sel,
					settings,
					Some(super::CLIPBOARD_PAYLOAD_MAX),
					opts,
				) {
					Ok(plan) if plan.file_limit_reached => {
						break (expanded, plan)
					}
					Ok(_) => {}
					Err(e) => return Err(e),
				}
			}
			Err(TransferError::EmptySelection) => {}
			Err(e) => return Err(e),
		}
		if limit == usize::MAX {
			let plan = super::plan_export_with(
				&expanded.sel,
				settings,
				Some(super::CLIPBOARD_PAYLOAD_MAX),
				opts,
			)?;
			break (expanded, plan);
		}
		limit = limit.saturating_mul(2);
	};
	let skipped = plan.skipped_unreadable_count
		+ plan.skipped_file_size_count
		+ expanded.skipped;
	if plan.files.is_empty() {
		return Ok(CopyReport {
			outcome: CopyOutcome {
				payload: String::new(),
				copied: 0,
				chars: 0,
				lines: 0,
				skipped,
				truncated: false,
			},
			files: plan.files,
			skipped_file_size: plan.skipped_file_size_count,
			skipped_unreadable: plan.skipped_unreadable_count
				+ expanded.skipped,
			file_limit_reached: plan.file_limit_reached,
		});
	}
	hold(&plan);
	plan.revalidate_with(opts)?;
	Ok(CopyReport {
		outcome: CopyOutcome {
			copied: plan.copied_file_count,
			chars: plan.stats.chars,
			lines: plan.stats.lines,
			skipped,
			truncated: expanded.truncated || plan.file_limit_reached,
			payload: plan.payload,
		},
		files: plan.files,
		skipped_file_size: plan.skipped_file_size_count,
		skipped_unreadable: plan.skipped_unreadable_count + expanded.skipped,
		file_limit_reached: plan.file_limit_reached,
	})
}

/// A selected folder copies its files, walked in the copy job: a folder
/// item itself cannot export. A repo's files (and `.git`) never come
/// along, not even when the selected folder is one; a path already
/// selected is not added twice. A file the export would refuse is skipped
/// and counted, never failing the whole copy.
///
/// Picked files (and Changes/Log items) are never starved: the folders
/// share the `limit` left after them in basket order (root path, then the
/// folder's relative path, as the Project selection is sorted), and the
/// walk stops one file past it so a huge folder is never held in full.
///
/// Pre-seeds `seen` with all explicit items before walking so that explicit
/// items retain their position (GUI basket-order semantics).
pub fn expand_folder_items(
	sel: ExportSelection,
	limit: usize,
	cancel: &CancelToken,
) -> Result<FolderExpansion, TransferError> {
	expand_folder_items_inner(sel, limit, cancel, false)
}

/// Expands folder items preserving first-occurrence deduplication in input order.
///
/// Unlike [`expand_folder_items`], `seen` starts empty and grows as items are
/// emitted. If an explicit item was already traversed by an earlier folder walk
/// or an earlier identical explicit item, it is dropped at its later position.
/// Used by the CLI copy path to preserve argument-order file layout matching TS.
pub fn expand_folder_items_in_input_order(
	sel: ExportSelection,
	limit: usize,
	cancel: &CancelToken,
) -> Result<FolderExpansion, TransferError> {
	expand_folder_items_inner(sel, limit, cancel, true)
}

fn expand_folder_items_inner(
	sel: ExportSelection,
	limit: usize,
	cancel: &CancelToken,
	input_order_dedupe: bool,
) -> Result<FolderExpansion, TransferError> {
	let is_folder = |item: &ExportItem| {
		if item.source != SourceKind::File {
			return false;
		}
		let path = item.root.path().join(&item.relative_path);
		let Ok(sym_meta) = std::fs::symlink_metadata(&path) else {
			return false;
		};
		if sym_meta.is_dir() {
			return true;
		}
		if sym_meta.is_symlink() {
			return is_safe_dir_symlink(item.root.path(), &path);
		}
		false
	};
	let is_refused_dir = |item: &ExportItem| {
		if item.source != SourceKind::File {
			return false;
		}
		let path = item.root.path().join(&item.relative_path);
		std::fs::metadata(&path).is_ok_and(|m| m.is_dir())
	};
	if !input_order_dedupe
		&& !sel
			.items
			.iter()
			.any(|item| is_folder(item) || is_refused_dir(item))
	{
		return Ok(FolderExpansion {
			sel,
			skipped: 0,
			truncated: false,
			truncated_at: None,
		});
	}
	let picked = sel
		.items
		.iter()
		.filter(|item| !is_folder(item) && !is_refused_dir(item))
		.count();
	let mut budget = limit.saturating_sub(picked);
	let mut skipped = 0usize;
	let mut truncated = false;
	let mut truncated_at = None;
	let mut seen: HashSet<(PathBuf, String)> = if input_order_dedupe {
		HashSet::new()
	} else {
		sel.items
			.iter()
			.map(|item| {
				(item.root.path().to_path_buf(), item.relative_path.clone())
			})
			.collect()
	};
	let mut items = Vec::with_capacity(sel.items.len());
	for item in sel.items {
		if !is_folder(&item) {
			if is_refused_dir(&item) {
				skipped += 1;
				continue;
			}
			if input_order_dedupe {
				let key = (
					item.root.path().to_path_buf(),
					item.relative_path.clone(),
				);
				if !seen.insert(key) {
					continue;
				}
			}
			items.push(item);
			continue;
		}
		let root = item.root.path();
		let dir = root.join(&item.relative_path);
		let walk = fsutil::list_files_recursive(&dir, |d| {
			!(d.file_name() == Some(".git".as_ref()) || d.join(".git").exists())
		});
		for walked in walk {
			if cancel.is_cancelled() || truncated {
				break;
			}
			let path = match walked {
				fsutil::WalkItem::File(path) => path,
				fsutil::WalkItem::UnreadableDir(_) => {
					skipped += 1;
					continue;
				}
			};
			if path.file_name() == Some(".git".as_ref()) {
				continue;
			}
			let Some(rel) = folder_file_rel(root, &path) else {
				skipped += 1;
				continue;
			};
			let key = (root.to_path_buf(), rel.clone());
			if seen.contains(&key) {
				continue;
			}
			if budget == 0 {
				truncated_at = Some(items.len());
				truncated = true;
				break;
			}
			budget -= 1;
			seen.insert(key);
			items.push(ExportItem {
				root: item.root.clone(),
				relative_path: rel,
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			});
		}
	}
	let source_root = sel.source_root.clone();
	let spelled_root = sel.spelled_root.clone();
	let sel = ExportSelection::new(
		sel.roots.iter().map(|r| r.path().to_path_buf()).collect(),
		sel.primary_root.map(|r| r.path().to_path_buf()),
		items,
	)?
	.with_source_root(source_root)
	.with_spelled_root(spelled_root);
	Ok(FolderExpansion {
		sel,
		skipped,
		truncated,
		truncated_at,
	})
}

/// Plans an export by expanding folder items in progressively doubling batches,
/// avoiding traversing the entire directory tree when a file count limit is active.
///
/// Uses input-order folder expansion so that files keep their first occurrence
/// in input order matching the CLI / TS copy contract.
///
/// When an expansion batch is truncated by budget, only the prefix of items up to
/// the truncation point (`truncated_at`) is planned, ensuring that any subsequent
/// picked items sorting after the truncated folder cannot trigger a premature
/// `file_limit_reached` before the folder's earlier files are traversed.
pub fn plan_export_expanding(
	sel: &ExportSelection,
	settings: &Settings,
	max_payload: Option<usize>,
	opts: &RunOptions,
	cancel: &CancelToken,
) -> Result<(ExportPlan, usize), TransferError> {
	let mut limit = if settings.set_max_file_count {
		let count_limit = if settings.file_count_limit > 0.0 {
			settings.file_count_limit as usize
		} else {
			0
		};
		64usize.max(4usize.saturating_mul(count_limit))
	} else {
		usize::MAX
	};

	let mut plan_opts = opts.clone();
	if plan_opts.cancel.is_none() {
		plan_opts.cancel = Some(cancel.clone());
	}

	loop {
		let expanded =
			expand_folder_items_in_input_order(sel.clone(), limit, cancel)?;
		if !expanded.truncated {
			let plan = super::plan_export_with(
				&expanded.sel,
				settings,
				max_payload,
				&plan_opts,
			)?;
			return Ok((plan, expanded.skipped));
		}

		let trunc_idx =
			expanded.truncated_at.unwrap_or(expanded.sel.items.len());
		let prefix_items = expanded.sel.items[..trunc_idx].to_vec();
		let roots = sel.roots.iter().map(|r| r.path().to_path_buf()).collect();
		let primary_root =
			sel.primary_root.as_ref().map(|r| r.path().to_path_buf());
		match ExportSelection::new(roots, primary_root, prefix_items) {
			Ok(prefix_sel) => {
				let prefix_sel = prefix_sel
					.with_source_root(sel.source_root.clone())
					.with_spelled_root(sel.spelled_root.clone());
				match super::plan_export_with(
					&prefix_sel,
					settings,
					max_payload,
					&plan_opts,
				) {
					Ok(plan) if plan.file_limit_reached => {
						return Ok((plan, expanded.skipped));
					}
					Ok(_) => {}
					Err(e) => return Err(e),
				}
			}
			Err(TransferError::EmptySelection) => {}
			Err(e) => return Err(e),
		}

		if limit == usize::MAX {
			let plan = super::plan_export_with(
				&expanded.sel,
				settings,
				max_payload,
				&plan_opts,
			)?;
			return Ok((plan, expanded.skipped));
		}
		limit = limit.saturating_mul(2);
	}
}

/// Selection generated from user paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSelection {
	pub sel: ExportSelection,
	/// Items dropped before planning (FIFO/socket/device, out-of-root or dangling symlink, header-unrepresentable or non-UTF-8 name, unopenable file).
	pub skipped: usize,
}

fn lexical_normalize(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for c in path.components() {
		match c {
			std::path::Component::CurDir => {}
			std::path::Component::ParentDir => {
				out.pop();
			}
			other => out.push(other),
		}
	}
	if out.as_os_str().is_empty() {
		PathBuf::from(std::path::MAIN_SEPARATOR_STR)
	} else {
		out
	}
}

fn collect_root_children(
	canonical_root: &CanonicalRootId,
	source_path: &Path,
	items: &mut Vec<ExportItem>,
	seen: &mut HashSet<String>,
	skipped: &mut usize,
) -> Result<(), TransferError> {
	let read_dir = match std::fs::read_dir(canonical_root.path()) {
		Ok(rd) => rd,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			return Err(TransferError::PathNotFound(source_path.to_path_buf()));
		}
		Err(e) => return Err(TransferError::Io(e)),
	};
	let mut child_names = Vec::new();
	for entry in read_dir {
		let entry = entry?;
		let name = entry.file_name();
		if name == ".git" {
			continue;
		}
		child_names.push(name);
	}
	fsutil::sort_names_js_order(&mut child_names);

	for name in child_names {
		let Some(name_str) = name.to_str() else {
			*skipped += 1;
			continue;
		};
		let child_path = canonical_root.path().join(&name);
		let child_sym_meta = match std::fs::symlink_metadata(&child_path) {
			Ok(m) => m,
			Err(_) => {
				*skipped += 1;
				continue;
			}
		};
		if child_sym_meta.is_dir() {
			if seen.insert(name_str.to_string()) {
				items.push(ExportItem {
					root: canonical_root.clone(),
					relative_path: name_str.to_string(),
					source: SourceKind::File,
					change_type: None,
					gitlink: false,
				});
			}
		} else if let Some(valid_rel) =
			folder_file_rel(canonical_root.path(), &child_path)
		{
			if seen.insert(valid_rel.clone()) {
				items.push(ExportItem {
					root: canonical_root.clone(),
					relative_path: valid_rel,
					source: SourceKind::File,
					change_type: None,
					gitlink: false,
				});
			}
		} else {
			*skipped += 1;
		}
	}
	Ok(())
}

/// Creates an export selection from user-specified path arguments, resolving relative paths against `cwd`.
pub fn selection_from_paths(
	root: &Path,
	cwd: &Path,
	paths: &[PathBuf],
) -> Result<PathSelection, TransferError> {
	let canonical_root = CanonicalRootId::new(root)?;
	canonical_root.validate()?;

	let lexical_root = if root.is_absolute() {
		lexical_normalize(root)
	} else {
		lexical_normalize(&cwd.join(root))
	};
	let source_root = paths::source_root_name(&[&lexical_root]);

	let mut items = Vec::new();
	let mut skipped = 0usize;
	let mut seen: HashSet<String> = HashSet::new();

	for path in paths {
		let full_path = if path.is_absolute() {
			path.to_path_buf()
		} else {
			cwd.join(path)
		};
		let normalized = lexical_normalize(&full_path);

		let is_root = normalized == lexical_root
			|| normalized == canonical_root.path()
			|| dunce::canonicalize(&normalized).ok().as_deref()
				== Some(canonical_root.path());
		if is_root {
			collect_root_children(
				&canonical_root,
				path,
				&mut items,
				&mut seen,
				&mut skipped,
			)?;
			continue;
		}

		let (parent, file_name) =
			match (normalized.parent(), normalized.file_name()) {
				(Some(p), Some(f)) => (p, f),
				_ => return Err(TransferError::PathOutsideRoot(path.clone())),
			};

		let canonical_parent = match dunce::canonicalize(parent) {
			Ok(p) => p,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				if parent.strip_prefix(root).is_err()
					&& parent.strip_prefix(&lexical_root).is_err()
					&& parent.strip_prefix(canonical_root.path()).is_err()
				{
					return Err(TransferError::PathOutsideRoot(path.clone()));
				}
				return Err(TransferError::PathNotFound(path.clone()));
			}
			Err(e) => return Err(TransferError::Io(e)),
		};

		let canonical_target = canonical_parent.join(file_name);

		let fallback_rel = match canonical_target
			.strip_prefix(canonical_root.path())
		{
			Ok(r) => r,
			Err(_) => return Err(TransferError::PathOutsideRoot(path.clone())),
		};

		let (rel, strip_root) = if let Ok(r) =
			normalized.strip_prefix(&lexical_root)
		{
			(r, lexical_root.as_path())
		} else if let Ok(r) = normalized.strip_prefix(canonical_root.path()) {
			(r, canonical_root.path())
		} else {
			(fallback_rel, canonical_root.path())
		};

		if rel.as_os_str().is_empty() {
			collect_root_children(
				&canonical_root,
				path,
				&mut items,
				&mut seen,
				&mut skipped,
			)?;
			continue;
		}

		let rel_str = match rel
			.components()
			.map(|c| c.as_os_str().to_str())
			.collect::<Option<Vec<_>>>()
			.map(|parts| parts.join("/"))
		{
			Some(s) => s,
			None => {
				skipped += 1;
				continue;
			}
		};

		let entry_path = strip_root.join(rel);

		let sym_meta = match std::fs::symlink_metadata(&entry_path) {
			Ok(m) => m,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				return Err(TransferError::PathNotFound(path.clone()));
			}
			Err(e) => return Err(TransferError::Io(e)),
		};

		let is_dir_symlink = sym_meta.is_symlink()
			&& is_safe_dir_symlink(canonical_root.path(), &entry_path);

		if sym_meta.is_dir() || is_dir_symlink {
			if seen.insert(rel_str.clone()) {
				items.push(ExportItem {
					root: canonical_root.clone(),
					relative_path: rel_str,
					source: SourceKind::File,
					change_type: None,
					gitlink: false,
				});
			}
		} else if let Some(valid_rel) = folder_file_rel(strip_root, &entry_path)
		{
			if seen.insert(valid_rel.clone()) {
				items.push(ExportItem {
					root: canonical_root.clone(),
					relative_path: valid_rel,
					source: SourceKind::File,
					change_type: None,
					gitlink: false,
				});
			}
		} else {
			skipped += 1;
		}
	}

	if items.is_empty() {
		return Err(TransferError::EmptySelection);
	}

	let sel = ExportSelection::new(
		vec![root.to_path_buf()],
		Some(root.to_path_buf()),
		items,
	)?
	.with_source_root(source_root)
	.with_spelled_root(Some(lexical_root));

	Ok(PathSelection { sel, skipped })
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_selection_from_paths_child_sort_order_matches_list_files_recursive()
	{
		let dir = tempfile::tempdir().unwrap();
		let f1 = "\u{FF5E}.txt";
		let f2 = "\u{1F600}.txt";
		let p1 = dir.path().join(f1);
		let p2 = dir.path().join(f2);
		if std::fs::write(&p1, "1").is_err()
			|| std::fs::write(&p2, "2").is_err()
		{
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"filesystem refused UTF-16/surrogate test filenames"
			);
			return;
		}

		let sel =
			selection_from_paths(dir.path(), dir.path(), &[PathBuf::from(".")])
				.unwrap();
		let sel_names: Vec<String> = sel
			.sel
			.items
			.into_iter()
			.map(|item| item.relative_path)
			.collect();

		let walk_names: Vec<String> =
			fsutil::list_files_recursive(dir.path(), |_| true)
				.filter_map(|w| match w {
					fsutil::WalkItem::File(p) => p
						.file_name()
						.and_then(|n| n.to_str())
						.map(ToString::to_string),
					_ => None,
				})
				.collect();

		assert_eq!(sel_names, walk_names);
		assert_eq!(
			sel_names,
			vec!["\u{1F600}.txt".to_string(), "\u{FF5E}.txt".to_string()]
		);
	}

	#[test]
	fn test_plan_export_expanding_matches_unbounded() {
		use crate::settings::{FilterAction, FilterRule, FilterType};

		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		// Create 30 small files in root: 15 keep, 15 skip
		for i in 0..15 {
			std::fs::write(
				root.join(format!("root_keep_{i:02}.txt")),
				format!("rk {i}"),
			)
			.unwrap();
			std::fs::write(
				root.join(format!("root_skip_{i:02}.txt")),
				format!("rs {i}"),
			)
			.unwrap();
		}
		// Create a subdir with 100 files: 50 keep, 50 skip
		let sub = root.join("sub");
		std::fs::create_dir_all(&sub).unwrap();
		for i in 0..50 {
			std::fs::write(
				sub.join(format!("sub_keep_{i:02}.txt")),
				format!("sk {i}"),
			)
			.unwrap();
			std::fs::write(
				sub.join(format!("sub_skip_{i:02}.txt")),
				format!("ss {i}"),
			)
			.unwrap();
		}

		let path_sel =
			selection_from_paths(root, root, &[PathBuf::from(".")]).unwrap();
		let cancel = CancelToken::new();

		let filter = FilterRule {
			kind: FilterType::Pattern,
			action: FilterAction::Exclude,
			value: "*skip*".to_string(),
			enabled: true,
		};
		let settings = Settings {
			use_filters: true,
			filter_rules: vec![filter],
			set_max_file_count: true,
			file_count_limit: 30.0,
			..Settings::default()
		};

		// 1. With file count limit active:
		let (batched_plan, batched_skipped) = plan_export_expanding(
			&path_sel.sel,
			&settings,
			None,
			&RunOptions::default(),
			&cancel,
		)
		.unwrap();

		let unbounded_expansion =
			expand_folder_items(path_sel.sel.clone(), usize::MAX, &cancel)
				.unwrap();
		let unbounded_plan = crate::transfer::plan_export_with(
			&unbounded_expansion.sel,
			&settings,
			None,
			&RunOptions::default(),
		)
		.unwrap();

		assert_eq!(batched_plan.files, unbounded_plan.files);
		assert_eq!(batched_plan.payload, unbounded_plan.payload);
		assert_eq!(
			batched_plan.copied_file_count,
			unbounded_plan.copied_file_count
		);
		assert_eq!(
			batched_plan.file_limit_reached,
			unbounded_plan.file_limit_reached
		);
		assert!(batched_plan.file_limit_reached);
		assert_eq!(batched_plan.copied_file_count, 30);
		assert_eq!(batched_skipped, 0);

		// 2. With limit off:
		let mut settings_no_limit = settings.clone();
		settings_no_limit.set_max_file_count = false;

		let (batched_plan_nl, _) = plan_export_expanding(
			&path_sel.sel,
			&settings_no_limit,
			None,
			&RunOptions::default(),
			&cancel,
		)
		.unwrap();

		let unbounded_plan_nl = crate::transfer::plan_export_with(
			&unbounded_expansion.sel,
			&settings_no_limit,
			None,
			&RunOptions::default(),
		)
		.unwrap();

		assert_eq!(batched_plan_nl.files, unbounded_plan_nl.files);
		assert_eq!(batched_plan_nl.payload, unbounded_plan_nl.payload);
		assert_eq!(
			batched_plan_nl.copied_file_count,
			unbounded_plan_nl.copied_file_count
		);
		assert_eq!(
			batched_plan_nl.file_limit_reached,
			unbounded_plan_nl.file_limit_reached
		);
		assert!(!batched_plan_nl.file_limit_reached);
		assert_eq!(batched_plan_nl.copied_file_count, 65);
	}

	#[test]
	fn copy_selection_expands_past_an_excluded_prefix() {
		use crate::settings::{FilterAction, FilterRule, FilterType};

		// A folder whose first 70 files the filter excludes and whose one
		// eligible file sorts last: one expansion at the initial batch
		// never reaches it, so the copy must double the batch, exactly as
		// `plan_export_expanding` does for a local CLI copy.
		let dir = tempfile::tempdir().unwrap();
		let sub = dir.path().join("sub");
		std::fs::create_dir_all(&sub).unwrap();
		for i in 0..70 {
			std::fs::write(sub.join(format!("skip_{i:02}.txt")), "s\n")
				.unwrap();
		}
		std::fs::write(sub.join("zz.txt"), "keep me\n").unwrap();
		let settings = Settings {
			use_filters: true,
			filter_rules: vec![FilterRule {
				kind: FilterType::Pattern,
				action: FilterAction::Exclude,
				value: "*skip*".to_string(),
				enabled: true,
			}],
			..Settings::default()
		};
		let sel = selection_from_paths(
			dir.path(),
			dir.path(),
			&[PathBuf::from("sub")],
		)
		.unwrap()
		.sel;
		let out =
			copy_selection(sel, &settings, 64, &RunOptions::default(), |_| {})
				.unwrap();
		assert_eq!(out.copied, 1, "the eligible file is copied");
		assert!(out.payload.contains("zz.txt"), "{}", out.payload);
		assert!(!out.payload.contains("skip_"), "{}", out.payload);
	}

	#[test]
	fn test_plan_export_expanding_folder_truncation_prefix_matches_unbounded() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		// Folder a_bin sorting before b files
		let a_bin = root.join("a_bin");
		std::fs::create_dir_all(&a_bin).unwrap();
		// 100 binary files in a_bin that consume expansion budget but are skipped by plan_export_with
		for i in 0..100 {
			std::fs::write(a_bin.join(format!("bin_{i:03}.bin")), [0u8, 1, 2])
				.unwrap();
		}
		// 1 valid text file at the end of a_bin
		std::fs::write(a_bin.join("zz.txt"), "valid text in a_bin\n").unwrap();

		// 31 picked text files sorting after a_bin
		for i in 0..=30 {
			std::fs::write(
				root.join(format!("b{i:02}.txt")),
				format!("content b{i:02}\n"),
			)
			.unwrap();
		}

		let path_sel =
			selection_from_paths(root, root, &[PathBuf::from(".")]).unwrap();
		let cancel = CancelToken::new();
		let settings = Settings::default();
		assert!(settings.set_max_file_count);
		assert_eq!(settings.file_count_limit, 30.0);

		let (batched_plan, batched_skipped) = plan_export_expanding(
			&path_sel.sel,
			&settings,
			None,
			&RunOptions::default(),
			&cancel,
		)
		.unwrap();

		let unbounded_expansion =
			expand_folder_items(path_sel.sel.clone(), usize::MAX, &cancel)
				.unwrap();
		let unbounded_plan = crate::transfer::plan_export_with(
			&unbounded_expansion.sel,
			&settings,
			None,
			&RunOptions::default(),
		)
		.unwrap();

		assert_eq!(batched_plan.files, unbounded_plan.files);
		assert_eq!(batched_plan.payload, unbounded_plan.payload);
		assert_eq!(
			batched_plan.copied_file_count,
			unbounded_plan.copied_file_count
		);
		assert_eq!(
			batched_plan.file_limit_reached,
			unbounded_plan.file_limit_reached
		);
		assert!(batched_plan.file_limit_reached);
		assert_eq!(batched_plan.copied_file_count, 30);
		assert_eq!(batched_plan.files[0].path, "a_bin/zz.txt");
		assert_eq!(batched_plan.files[1].path, "b00.txt");
		assert_eq!(batched_plan.files[29].path, "b28.txt");
		assert_eq!(batched_skipped, 0);
	}

	#[cfg(unix)]
	#[test]
	fn test_refuse_git_dir_symlinks() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let git_dir = root.join(".git");
		let git_refs = git_dir.join("refs");
		std::fs::create_dir_all(&git_refs).unwrap();
		std::fs::write(git_refs.join("heads"), "dummy ref").unwrap();
		std::fs::write(root.join("regular.txt"), "regular content").unwrap();

		let link = root.join("link");
		let link2 = root.join("link2");
		std::os::unix::fs::symlink(&git_dir, &link).unwrap();
		std::os::unix::fs::symlink(&git_refs, &link2).unwrap();

		// 1. Through selection_from_paths with sibling regular file
		let path_sel = selection_from_paths(
			root,
			root,
			&[
				PathBuf::from("link"),
				PathBuf::from("link2"),
				PathBuf::from("regular.txt"),
			],
		)
		.unwrap();
		assert_eq!(path_sel.skipped, 2);
		for item in &path_sel.sel.items {
			assert!(!item.relative_path.starts_with("link"));
			assert!(!item.relative_path.starts_with("link2"));
			assert!(!item.relative_path.contains(".git"));
		}
		assert_eq!(path_sel.sel.items.len(), 1);
		assert_eq!(path_sel.sel.items[0].relative_path, "regular.txt");

		// 2. Through selection_from_paths with only links (all skipped)
		let res_only_links = selection_from_paths(
			root,
			root,
			&[PathBuf::from("link"), PathBuf::from("link2")],
		);
		assert!(matches!(res_only_links, Err(TransferError::EmptySelection)));

		// 3. Through expand_folder_items with sibling regular file
		let root_id = CanonicalRootId::new(root).unwrap();
		let items = vec![
			ExportItem {
				root: root_id.clone(),
				relative_path: "link".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: root_id.clone(),
				relative_path: "link2".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: root_id.clone(),
				relative_path: "regular.txt".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		];
		let sel = ExportSelection::new(
			vec![root.to_path_buf()],
			Some(root.to_path_buf()),
			items,
		)
		.unwrap();
		let cancel = CancelToken::new();
		let expanded = expand_folder_items(sel, 100, &cancel).unwrap();
		assert_eq!(expanded.skipped, 2);
		assert_eq!(expanded.sel.items.len(), 1);
		assert_eq!(expanded.sel.items[0].relative_path, "regular.txt");
		for item in &expanded.sel.items {
			assert!(!item.relative_path.starts_with("link"));
			assert!(!item.relative_path.starts_with("link2"));
			assert!(!item.relative_path.contains(".git"));
		}

		// 4. Through expand_folder_items with only links
		let items_only_links = vec![
			ExportItem {
				root: root_id.clone(),
				relative_path: "link".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
			ExportItem {
				root: root_id,
				relative_path: "link2".to_string(),
				source: SourceKind::File,
				change_type: None,
				gitlink: false,
			},
		];
		let sel_only_links = ExportSelection::new(
			vec![root.to_path_buf()],
			Some(root.to_path_buf()),
			items_only_links,
		)
		.unwrap();
		let res_expand_only_links =
			expand_folder_items(sel_only_links, 100, &cancel);
		assert!(matches!(
			res_expand_only_links,
			Err(TransferError::EmptySelection)
		));
	}
}
