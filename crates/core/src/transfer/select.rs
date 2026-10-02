use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::gitrun::CancelToken;
use crate::paths;
use crate::transfer::{
	CanonicalRootId, ExportItem, ExportSelection, SourceKind, TransferError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderExpansion {
	pub sel: ExportSelection,
	/// Walked files the payload cannot carry (see `folder_file_rel`).
	pub skipped: usize,
	/// The walk stopped at the file limit with files left.
	pub truncated: bool,
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
pub fn expand_folder_items(
	sel: ExportSelection,
	limit: usize,
	cancel: &CancelToken,
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
			if let Ok(meta) = std::fs::metadata(&path) {
				if meta.is_dir() {
					if let Ok(canonical) = dunce::canonicalize(&path) {
						return !paths::escapes_all_roots(
							&[item.root.path()],
							&canonical,
						);
					}
				}
			}
		}
		false
	};
	if !sel.items.iter().any(is_folder) {
		return Ok(FolderExpansion {
			sel,
			skipped: 0,
			truncated: false,
		});
	}
	let picked = sel.items.iter().filter(|item| !is_folder(item)).count();
	let mut budget = limit.saturating_sub(picked);
	let mut skipped = 0usize;
	let mut truncated = false;
	let mut seen: HashSet<(PathBuf, String)> = sel
		.items
		.iter()
		.map(|item| {
			(item.root.path().to_path_buf(), item.relative_path.clone())
		})
		.collect();
	let mut items = Vec::with_capacity(sel.items.len());
	for item in sel.items {
		if !is_folder(&item) {
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
			if seen.contains(&(root.to_path_buf(), rel.clone())) {
				continue;
			}
			if budget == 0 {
				truncated = true;
				break;
			}
			budget -= 1;
			seen.insert((root.to_path_buf(), rel.clone()));
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
	let sel = ExportSelection::new(
		sel.roots.iter().map(|r| r.path().to_path_buf()).collect(),
		sel.primary_root.map(|r| r.path().to_path_buf()),
		items,
	)?
	.with_source_root(source_root);
	Ok(FolderExpansion {
		sel,
		skipped,
		truncated,
	})
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
			&& (|| -> bool {
				if let Ok(meta) = std::fs::metadata(&entry_path) {
					if meta.is_dir() {
						if let Ok(canonical) = dunce::canonicalize(&entry_path)
						{
							return !paths::escapes_all_roots(
								&[canonical_root.path()],
								&canonical,
							);
						}
					}
				}
				false
			})();

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
	.with_source_root(source_root);

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
}
