//! Which folder a path means, for the folders a paste has to make.
//!
//! A folder that does not exist yet has no identity of its own: it is *the
//! folder `anchor/t1/t2`* where `anchor` is the nearest folder that exists.
//! That folder is the filesystem's own, so it is compared as it is (its
//! canonical path, as written) and never folded; only the names below it,
//! which the filesystem has not seen yet, fold case, and only when the
//! anchor's filesystem does. Two folders under two existing folders are two
//! folders however alike the paths read: `Α/new` and `α/new` are one folder
//! only if `Α` and `α` are.
//!
//! [`FolderId`] is the only type that decides this, and its fields are
//! private to this module: a caller cannot build one from a lowercased whole
//! path, and cannot compare paths for folder identity without going through
//! it. (An earlier version folded the whole path; two roots that differ in
//! case were merged, and the folder the user named under the second one was
//! made without asking.)

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::fscase::fs_is_case_insensitive;
use crate::restore;

/// A folder to be made, by the exact identity of the folder it is made in
/// and the (folded) names below that.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FolderId {
	anchor: PathBuf,
	/// Whether the anchor's filesystem folds case; decides `tail`. Equal
	/// anchors always carry the same value.
	folds: bool,
	tail: Vec<String>,
}

impl FolderId {
	fn at(anchor: PathBuf, folds: bool) -> Self {
		Self {
			anchor,
			folds,
			tail: Vec::new(),
		}
	}

	fn child(&self, name: &OsStr) -> Self {
		let name = name.to_string_lossy();
		let mut tail = self.tail.clone();
		tail.push(if self.folds {
			name.to_lowercase()
		} else {
			name.into_owned()
		});
		Self {
			anchor: self.anchor.clone(),
			folds: self.folds,
			tail,
		}
	}
}

/// An existing folder as the filesystem has it.
#[derive(Clone)]
struct Anchor {
	canonical: PathBuf,
	folds: bool,
}

/// One spelling for each folder a plan has to make, and which of them the
/// user named. On a filesystem that folds case `new` and `NEW` are one
/// folder, and every place that tells folders apart (what is agreed to,
/// what is recorded as named, what is made) goes through this.
pub(crate) struct NewFolderNames {
	/// Whether the filesystem under each destination root folds case.
	root_folds: HashMap<PathBuf, bool>,
	/// Existing folder, as spelled, to its anchor. One `canonicalize` each.
	anchors: HashMap<PathBuf, Anchor>,
	/// The first spelling asked for, per folder.
	spelled: HashMap<FolderId, PathBuf>,
	/// The folders the user named (a placed folder's tail).
	named: HashSet<FolderId>,
}

impl NewFolderNames {
	pub(crate) fn new(root_folds: HashMap<PathBuf, bool>) -> Self {
		Self {
			root_folds,
			anchors: HashMap::new(),
			spelled: HashMap::new(),
			named: HashSet::new(),
		}
	}

	/// Whether names below `canonical` fold: what its destination root
	/// says, else the filesystem under it.
	fn folds_at(&self, canonical: &Path) -> bool {
		if let Some(&folds) = self.root_folds.get(canonical) {
			return folds;
		}
		self.root_folds
			.iter()
			.filter(|(root, _)| canonical.starts_with(root))
			.max_by_key(|(root, _)| root.components().count())
			.map_or_else(|| fs_is_case_insensitive(canonical), |(_, &f)| f)
	}

	fn anchor(&mut self, existing: &Path) -> Anchor {
		if let Some(anchor) = self.anchors.get(existing) {
			return anchor.clone();
		}
		let canonical = dunce::canonicalize(existing)
			.unwrap_or_else(|_| existing.to_path_buf());
		let anchor = Anchor {
			folds: self.folds_at(&canonical),
			canonical,
		};
		self.anchors.insert(existing.to_path_buf(), anchor.clone());
		anchor
	}

	/// The folders of a placed destination, anchored at `anchor` (a folder
	/// that exists) with `tail` below it, shallow to deep, as the user named
	/// them: each under its first spelling, and all recorded as named.
	pub(crate) fn name_chain(
		&mut self,
		anchor: &Path,
		tail: &[String],
	) -> Vec<PathBuf> {
		let a = self.anchor(anchor);
		let mut id = FolderId::at(a.canonical, a.folds);
		let mut cur = anchor.to_path_buf();
		let mut chain = Vec::with_capacity(tail.len());
		for name in tail {
			id = id.child(OsStr::new(name));
			self.named.insert(id.clone());
			let next = cur.join(name);
			cur = self.spelled.entry(id.clone()).or_insert(next).clone();
			chain.push(cur.clone());
		}
		chain
	}

	/// Moves `op` into the spellings of the folders it needs made: the
	/// folders on its path below the first one that does not exist. Only
	/// when one of them is a folder the user named: a plan that names none
	/// keeps the payload's spellings, as ClipCode does.
	pub(crate) fn unify(&mut self, op: &mut restore::CreateOperation) {
		if self.named.is_empty() {
			return;
		}
		let Some(parent) = op.absolute_path.parent() else {
			return;
		};
		if fs::symlink_metadata(parent).is_ok() {
			return;
		}
		let (Ok(below), Some(file)) = (
			parent.strip_prefix(&op.root_path),
			op.absolute_path.file_name(),
		) else {
			return;
		};
		let mut base = op.root_path.clone();
		let mut missing = Vec::new();
		for name in below.components() {
			if missing.is_empty() {
				let next = base.join(name);
				if fs::symlink_metadata(&next).is_ok() {
					base = next;
					continue;
				}
			}
			missing.push(name.as_os_str());
		}
		let a = self.anchor(&base);
		let mut id = FolderId::at(a.canonical, a.folds);
		let mut ids = Vec::with_capacity(missing.len());
		let mut names_one = false;
		for name in &missing {
			id = id.child(name);
			names_one |= self.named.contains(&id);
			ids.push(id.clone());
		}
		if !names_one {
			return;
		}
		let mut cur = base;
		for (name, id) in missing.into_iter().zip(ids) {
			let next = cur.join(name);
			cur = self.spelled.entry(id).or_insert(next).clone();
		}
		let absolute = cur.join(file);
		let Ok(rel) = absolute.strip_prefix(&op.root_path) else {
			return;
		};
		op.relative_path = rel
			.components()
			.map(|c| c.as_os_str().to_string_lossy())
			.collect::<Vec<_>>()
			.join("/");
		op.absolute_path = absolute;
	}

	/// The folders of `missing` (those a planned create has to make, as
	/// spelled) that are a folder the user named: they are made only with
	/// consent. `named_paths` are the spellings `name_chain` returned; one
	/// of them that is missing is always among the result.
	pub(crate) fn requested(
		&mut self,
		missing: &HashSet<PathBuf>,
		named_paths: &[PathBuf],
	) -> HashSet<PathBuf> {
		let mut requested = HashSet::new();
		if self.named.is_empty() {
			return requested;
		}
		for dir in missing {
			// What is missing is one run from the file upward, so the
			// nearest folder that exists is the first parent not missing.
			let mut names = Vec::new();
			let mut cur: &Path = dir;
			while missing.contains(cur) {
				let (Some(name), Some(parent)) =
					(cur.file_name(), cur.parent())
				else {
					break;
				};
				names.push(name);
				cur = parent;
			}
			let a = self.anchor(cur);
			let mut id = FolderId::at(a.canonical, a.folds);
			for name in names.into_iter().rev() {
				id = id.child(name);
			}
			if self.named.contains(&id) {
				requested.insert(dir.clone());
			}
		}
		// The invariant: a named folder, spelled as named, that a create
		// needs is requested. A gap here would make it with no consent.
		for dir in named_paths {
			if missing.contains(dir) {
				debug_assert!(
					requested.contains(dir),
					"named folder {} is not requested",
					dir.display()
				);
				requested.insert(dir.clone());
			}
		}
		requested
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn names(folds: bool, roots: &[&str]) -> NewFolderNames {
		NewFolderNames::new(
			roots.iter().map(|r| (PathBuf::from(r), folds)).collect(),
		)
	}

	fn tail(names: &[&str]) -> Vec<String> {
		names.iter().map(|n| (*n).to_string()).collect()
	}

	/// Two folders under two existing folders are two folders, however the
	/// new names below them fold.
	#[test]
	fn the_same_new_name_under_two_anchors_is_two_folders() {
		let (upper, lower) = ("/ws/\u{391}", "/ws/\u{3b1}");
		let mut n = names(true, &[upper, lower]);
		let first = n.name_chain(Path::new(upper), &tail(&["new"]));
		let second = n.name_chain(Path::new(lower), &tail(&["new"]));
		assert_eq!(first, [PathBuf::from(upper).join("new")]);
		assert_eq!(second, [PathBuf::from(lower).join("new")]);
		let missing: HashSet<PathBuf> =
			[PathBuf::from(lower).join("new")].into();
		assert_eq!(n.requested(&missing, &second), missing);
	}

	/// Below one anchor that folds, the spellings of a name are one folder
	/// and keep the first spelling; one that does not fold keeps both.
	#[test]
	fn names_below_one_anchor_fold_only_when_it_does() {
		for (folds, one) in [(true, true), (false, false)] {
			let mut n = names(folds, &["/ws"]);
			let a = n.name_chain(Path::new("/ws"), &tail(&["new"]));
			let b = n.name_chain(Path::new("/ws"), &tail(&["NEW"]));
			assert_eq!(a, [PathBuf::from("/ws/new")]);
			let want = if one { "/ws/new" } else { "/ws/NEW" };
			assert_eq!(b, [PathBuf::from(want)], "folds: {folds}");
		}
	}

	/// A missing folder is requested by identity, not by spelling, and only
	/// when it is the named one: a folder below it is incidental.
	#[test]
	fn a_missing_folder_is_requested_by_what_it_is() {
		let mut n = names(true, &["/ws"]);
		let chain = n.name_chain(Path::new("/ws"), &tail(&["new"]));
		let missing: HashSet<PathBuf> = [
			PathBuf::from("/ws/NEW"),
			PathBuf::from("/ws/NEW/sub"),
			PathBuf::from("/ws/other"),
		]
		.into();
		let got = n.requested(&missing, &[]);
		assert_eq!(got, [PathBuf::from("/ws/NEW")].into());
		assert_eq!(chain, [PathBuf::from("/ws/new")]);
	}
}
