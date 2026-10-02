use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::gitrun::CancelToken;
use crate::paths;
use crate::transfer::{ExportItem, ExportSelection, SourceKind, TransferError};

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
fn folder_file_rel(root: &Path, path: &Path) -> Option<String> {
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
		item.source == SourceKind::File
			&& std::fs::symlink_metadata(
				item.root.path().join(&item.relative_path),
			)
			.is_ok_and(|meta| meta.is_dir())
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
			});
		}
	}
	let sel = ExportSelection::new(
		sel.roots.iter().map(|r| r.path().to_path_buf()).collect(),
		sel.primary_root.map(|r| r.path().to_path_buf()),
		items,
	)?;
	Ok(FolderExpansion {
		sel,
		skipped,
		truncated,
	})
}
