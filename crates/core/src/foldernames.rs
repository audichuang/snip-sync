//! Which folder a path means, for the folders a paste has to make.
//!
//! A folder that does not exist yet has no identity of its own: it is *the
//! folder `anchor/t1/t2`* where `anchor` is the nearest folder that exists.
//! That folder is the filesystem's own, so it is told by the object the
//! filesystem has (its device and inode, on Windows its file index; see
//! [`DirKey`]), never by how its path is spelled, and never folded: a
//! folder has many spellings (`R/Sub` and `R/sub` on a filesystem that folds
//! case, `/private/x` and `/System/Volumes/Data/private/x` on macOS, two
//! mount points of one bind mount), and a path-based key would miss some of
//! them. The object is read with a stat, which needs no right to read the
//! folder and holds nothing open. Only the names below the
//! anchor, which the filesystem has not seen yet, fold case, and only when
//! the anchor's filesystem does; they are Unicode-normalized everywhere
//! ([`name_key`], the one rule for which names are one). Two folders under two existing folders are
//! two folders however alike the paths read: `Α/new` and `α/new` are one
//! folder only if `Α` and `α` are (the same object).
//!
//! [`FolderId`] is the only type that decides this, and its fields are
//! private to this module: a caller cannot build one from a lowercased whole
//! path, and cannot compare paths for folder identity without going through
//! it. (An earlier version folded the whole path; two roots that differ in
//! case were merged, and the folder the user named under the second one was
//! made without asking.)

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::fscase::fs_is_case_insensitive;
use crate::restore;

/// Which existing folder: the filesystem's object, not a spelling of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum DirKey {
	/// The folder's own device and inode (Windows: volume serial number
	/// and file index), read once. Nothing is held open: one key per
	/// distinct anchor must not cost a file descriptor.
	Object(u64, u64),
	/// Only when the folder cannot be statted at all: its canonical path.
	Path(PathBuf),
}

impl DirKey {
	/// What the filesystem says `path` is. A stat needs the right to search
	/// the folders above it, not to read the folder itself, so a drop box
	/// (`-wx`) is told by its object like any other.
	pub(crate) fn of(path: &Path) -> Self {
		match object_of(path) {
			Some((volume, index)) => Self::Object(volume, index),
			None => Self::Path(
				dunce::canonicalize(path)
					.unwrap_or_else(|_| path.to_path_buf()),
			),
		}
	}
}

#[cfg(unix)]
fn object_of(path: &Path) -> Option<(u64, u64)> {
	use std::os::unix::fs::MetadataExt;
	let md = fs::metadata(path).ok()?;
	Some((md.dev(), md.ino()))
}

/// Opened with access mode 0 (no read or write right, so a folder one
/// cannot read still has an identity) and `FILE_FLAG_BACKUP_SEMANTICS` (a
/// folder can be opened at all), and released as soon as the index is read.
#[cfg(windows)]
fn object_of(path: &Path) -> Option<(u64, u64)> {
	use std::os::windows::fs::OpenOptionsExt;
	const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
	let file = fs::OpenOptions::new()
		.access_mode(0)
		.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
		.open(path)
		.ok()?;
	let handle = winapi_util::Handle::from_file(file);
	let info = winapi_util::file::information(&handle).ok()?;
	Some((info.volume_serial_number(), info.file_index()))
}

/// A folder to be made, by the exact identity of the folder it is made in
/// and the (folded) names below that.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FolderId {
	anchor: DirKey,
	/// Whether the anchor's filesystem folds case; decides `tail`. Equal
	/// anchors always carry the same value.
	folds: bool,
	tail: Vec<String>,
}

impl FolderId {
	fn at(anchor: DirKey, folds: bool) -> Self {
		Self {
			anchor,
			folds,
			tail: Vec::new(),
		}
	}

	fn child(&self, name: &OsStr) -> Self {
		let mut tail = self.tail.clone();
		tail.push(name_key(&name.to_string_lossy(), self.folds));
		Self {
			anchor: self.anchor.clone(),
			folds: self.folds,
			tail,
		}
	}
}

/// The key a new name is told by below an existing folder: two names with
/// one key are one folder or file. `folds` is whether that folder's
/// filesystem folds case. Every identity in this module (and so every
/// [`FolderId`] and [`TargetId`]) takes its names from here only.
///
/// The key errs towards merging: two names merged that the filesystem keeps
/// apart cost one more question or one more refused pair, while two kept
/// apart that it merges let a named folder be made without consent, or one
/// file be written twice (the preview showing two new files, Apply
/// overwriting the first with the second).
///
/// - Unicode normalization, on every filesystem: `é` composed and `e` with
///   a combining accent are one name. macOS (APFS, HFS+) looks either up as
///   the same entry; Linux and NTFS keep them apart, where merging them is
///   the safe side above.
/// - Case, only where the filesystem folds it: full Unicode case folding
///   (`unicase`), not `to_lowercase`. Folding merges what lowercasing keeps
///   apart, such as the final sigma `ς` and `σ` (one name to APFS) and `ß`
///   and `ss` (full folding; more than some filesystems merge).
///
/// The canonical caseless form (decompose, fold, decompose again), so a
/// decomposed capital meets the composed small letter. ASCII is its own
/// key, or its lowercase where case folds.
pub(crate) fn name_key(name: &str, folds: bool) -> String {
	use unicode_normalization::UnicodeNormalization;
	#[cfg(any(test, feature = "test-support"))]
	if let Some(key) = distinct_key(name) {
		return key;
	}
	if name.is_ascii() {
		return if folds {
			name.to_ascii_lowercase()
		} else {
			name.to_string()
		};
	}
	let decomposed: String = name.nfd().collect();
	if folds {
		unicase::UniCase::new(decomposed.as_str())
			.to_folded_case()
			.nfd()
			.collect()
	} else {
		decomposed
	}
}

/// While set, [`name_key`] gives every call a key of its own, even for one
/// name twice: a test proves that no safety check rests on it. Process-wide
/// (a worker in a test plans on its own thread); a test runs in its own
/// process under nextest.
#[cfg(any(test, feature = "test-support"))]
static DISTINCT_NAME_KEYS: std::sync::atomic::AtomicBool =
	std::sync::atomic::AtomicBool::new(false);

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_distinct_name_keys(on: bool) {
	DISTINCT_NAME_KEYS.store(on, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(any(test, feature = "test-support"))]
fn distinct_key(name: &str) -> Option<String> {
	use std::sync::atomic::{AtomicUsize, Ordering};
	static NEXT: AtomicUsize = AtomicUsize::new(0);
	DISTINCT_NAME_KEYS
		.load(Ordering::SeqCst)
		.then(|| format!("{name}\u{0}{}", NEXT.fetch_add(1, Ordering::SeqCst)))
}

/// Which file a target path writes: the folder that exists nearest above
/// it, told by its object, and the names below that folder, folded when its
/// filesystem folds. Two targets with one `TargetId` are one file, however
/// their paths are spelled (two cases on a filesystem that folds, a symlink,
/// the macOS data volume's firmlink). A target that exists is read through
/// symlinks to the file it is, then named in its folder, so two hard links
/// stay two files.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TargetId(FolderId);

/// The folder `path` is written in that exists now (the anchor) and the
/// names from there to the target, outermost first.
fn target_anchor(path: &Path) -> (PathBuf, Vec<OsString>) {
	if fs::symlink_metadata(path).is_ok() {
		let resolved =
			dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
		return match (resolved.parent(), resolved.file_name()) {
			(Some(parent), Some(name)) => {
				(parent.to_path_buf(), vec![name.to_os_string()])
			}
			_ => (resolved, Vec::new()),
		};
	}
	let mut names: Vec<OsString> = Vec::new();
	let mut cur = path;
	loop {
		match fs::symlink_metadata(cur) {
			Err(e)
				if matches!(
					e.kind(),
					std::io::ErrorKind::NotFound
						| std::io::ErrorKind::NotADirectory
				) =>
			{
				match (cur.file_name(), cur.parent()) {
					(Some(name), Some(parent)) => {
						names.push(name.to_os_string());
						cur = parent;
					}
					_ => break,
				}
			}
			_ => break,
		}
	}
	names.reverse();
	(cur.to_path_buf(), names)
}

fn target_id(at: FolderId, names: &[OsString]) -> TargetId {
	TargetId(names.iter().fold(at, |id, name| id.child(name)))
}

/// Which folder to make a path is, read off the disk as it is now: Apply's
/// own answer, apart from what planning recorded. The nearest existing
/// folder above the path is the anchor, told by its object; the names below
/// it fold when its filesystem does (one answer per object).
#[derive(Default)]
pub(crate) struct LiveFolders {
	folds: HashMap<DirKey, bool>,
}

impl LiveFolders {
	/// `None` when `path` exists, or when no folder above it can be found.
	pub(crate) fn id_of(&mut self, path: &Path) -> Option<FolderId> {
		let mut names: Vec<&OsStr> = Vec::new();
		let mut cur = path;
		loop {
			match fs::symlink_metadata(cur) {
				Err(e)
					if matches!(
						e.kind(),
						std::io::ErrorKind::NotFound
							| std::io::ErrorKind::NotADirectory
					) =>
				{
					names.push(cur.file_name()?);
					cur = cur.parent()?;
				}
				_ => break,
			}
		}
		if names.is_empty() {
			return None;
		}
		let mut id = self.at(cur);
		for name in names.into_iter().rev() {
			id = id.child(name);
		}
		Some(id)
	}

	/// Which file `path` writes, as the disk is now ([`TargetId`]).
	pub(crate) fn target_id(&mut self, path: &Path) -> TargetId {
		let (anchor, names) = target_anchor(path);
		target_id(self.at(&anchor), &names)
	}

	/// The existing folder `dir`, with whether names below it fold.
	fn at(&mut self, dir: &Path) -> FolderId {
		let key = DirKey::of(dir);
		let folds = *self.folds.entry(key.clone()).or_insert_with(|| {
			fs_is_case_insensitive(
				&dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
			)
		});
		FolderId::at(key, folds)
	}
}

/// An existing folder as the filesystem has it.
#[derive(Clone)]
struct Anchor {
	key: DirKey,
	folds: bool,
}

/// One spelling for each folder a plan has to make, and which of them the
/// user named. On a filesystem that folds case `new` and `NEW` are one
/// folder, and every place that tells folders apart (what is agreed to,
/// what is recorded as named, what is made) goes through this.
pub(crate) struct NewFolderNames {
	/// Whether the filesystem under each destination root folds case.
	root_folds: HashMap<PathBuf, bool>,
	/// Which existing folder a path is.
	identify: Box<dyn Fn(&Path) -> DirKey>,
	/// Existing folder, as spelled, to its anchor. One lookup each.
	anchors: HashMap<PathBuf, Anchor>,
	/// Whether names below a folder fold, by folder: the answer for the
	/// first spelling met decides, so one folder never has two.
	folds: HashMap<DirKey, bool>,
	/// The first spelling asked for, per folder.
	spelled: HashMap<FolderId, PathBuf>,
	/// The folders the user named (a placed folder's tail).
	named: HashSet<FolderId>,
}

impl NewFolderNames {
	pub(crate) fn new(root_folds: HashMap<PathBuf, bool>) -> Self {
		Self::with_identity(root_folds, Box::new(DirKey::of))
	}

	/// [`Self::new`] with another way to tell which folder a path is.
	pub(crate) fn with_identity(
		root_folds: HashMap<PathBuf, bool>,
		identify: Box<dyn Fn(&Path) -> DirKey>,
	) -> Self {
		Self {
			root_folds,
			identify,
			anchors: HashMap::new(),
			folds: HashMap::new(),
			spelled: HashMap::new(),
			named: HashSet::new(),
		}
	}

	/// The destination root `absolute` lies under, for an op whose own root
	/// (`op_root`) is another spelling of the folders above it: the root
	/// that is the same folder as `op_root` if one is, else the deepest.
	/// `root_folds` holds every destination root.
	fn root_holding(&self, absolute: &Path, op_root: &Path) -> Option<PathBuf> {
		let own = (self.identify)(op_root);
		let mut under: Vec<&PathBuf> = self
			.root_folds
			.keys()
			.filter(|root| absolute.starts_with(root) && absolute != *root)
			.collect();
		under.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
		under
			.iter()
			.find(|root| (self.identify)(root) == own)
			.or_else(|| under.first())
			.map(|root| (*root).clone())
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
		let key = (self.identify)(existing);
		let folds = match self.folds.get(&key) {
			Some(&folds) => folds,
			None => {
				let canonical = dunce::canonicalize(existing)
					.unwrap_or_else(|_| existing.to_path_buf());
				let folds = self.folds_at(&canonical);
				self.folds.insert(key.clone(), folds);
				folds
			}
		};
		let anchor = Anchor { key, folds };
		self.anchors.insert(existing.to_path_buf(), anchor.clone());
		anchor
	}

	/// Which file `path` writes ([`TargetId`]), its folder told as this
	/// plan tells every existing folder.
	pub(crate) fn target_id(&mut self, path: &Path) -> TargetId {
		let (anchor, names) = target_anchor(path);
		let a = self.anchor(&anchor);
		target_id(FolderId::at(a.key, a.folds), &names)
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
		let mut id = FolderId::at(a.key, a.folds);
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
		let mut id = FolderId::at(a.key, a.folds);
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
		let depth = missing.len();
		let mut cur = base.clone();
		for (name, id) in missing.into_iter().zip(ids) {
			let next = cur.join(name);
			cur = self.spelled.entry(id).or_insert(next).clone();
		}
		let mut absolute = cur.join(file);
		// The folder's first spelling may lie under another spelling of the
		// folders above it (`/private/x` and `/System/Volumes/Data/private/x`
		// on macOS): the op moves to the destination root that spelling is
		// under, so one folder is one path in the plan, asked for and made
		// once. Without such a root, the op keeps its own spelling of the
		// existing folders and takes the unified names below them.
		let root = if absolute.starts_with(&op.root_path) {
			op.root_path.clone()
		} else if let Some(root) = self.root_holding(&absolute, &op.root_path) {
			root
		} else {
			let names: Vec<_> = cur.components().rev().take(depth).collect();
			absolute = base;
			for name in names.into_iter().rev() {
				absolute.push(name);
			}
			absolute.push(file);
			op.root_path.clone()
		};
		let Ok(rel) = absolute.strip_prefix(&root) else {
			return;
		};
		op.relative_path = rel
			.components()
			.map(|c| c.as_os_str().to_string_lossy())
			.collect::<Vec<_>>()
			.join("/");
		op.root_path = root;
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
			let mut id = FolderId::at(a.key, a.folds);
			for name in names.into_iter().rev() {
				id = id.child(name);
			}
			if self.named.contains(&id) {
				requested.insert(dir.clone());
			}
		}
		// A named folder, spelled as named, that a create needs is
		// requested whatever the identity above made of it. (Planning
		// refuses a placed folder whose tail already exists, so the anchor
		// here is the one `resolve` found; this is a second net for a
		// spelling the identity folds differently from the filesystem,
		// where asking is the safe side.)
		for dir in named_paths {
			if missing.contains(dir) {
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

	/// One name in Unicode's two forms (`é` composed, and `e` with a
	/// combining accent) is one name: macOS stores and looks up either as
	/// the same folder. On every filesystem, folding or not, so a folder the
	/// user named is never taken for another one.
	#[test]
	fn name_key_merges_the_composed_and_decomposed_forms() {
		for folds in [true, false] {
			assert_eq!(
				name_key("\u{e9}", folds),
				name_key("e\u{301}", folds),
				"folds: {folds}"
			);
			assert_eq!(
				name_key("caf\u{e9}/\u{c5}", folds),
				name_key("cafe\u{301}/A\u{30a}", folds),
				"folds: {folds}"
			);
		}
	}

	/// Case folds only where the filesystem does.
	#[test]
	fn name_key_folds_case_only_when_the_filesystem_does() {
		assert_eq!(name_key("New", true), name_key("NEW", true));
		assert_ne!(name_key("New", false), name_key("NEW", false));
		// A decomposed capital is the composed small letter.
		assert_eq!(name_key("E\u{301}", true), name_key("\u{e9}", true));
		assert_ne!(name_key("E\u{301}", false), name_key("\u{e9}", false));
	}

	/// The final sigma is the small sigma once case folds, as APFS has it
	/// (`to_lowercase` keeps the two apart), wherever it stands.
	#[test]
	fn name_key_folds_the_final_sigma_into_sigma() {
		assert_eq!(name_key("\u{3c2}", true), name_key("\u{3c3}", true));
		assert_eq!(name_key("\u{3c3}", true), name_key("\u{3a3}", true));
		assert_eq!(
			name_key("\u{3b1}\u{3c2}", true),
			name_key("\u{391}\u{3a3}", true)
		);
		assert_ne!(name_key("\u{3c2}", false), name_key("\u{3c3}", false));
	}

	/// `ß` takes full case folding, `ss`: it merges with `ss` and `SS` (and
	/// the capital `ẞ`). More than some filesystems merge, which only asks
	/// once more or refuses one more pair; never less.
	#[test]
	fn name_key_folds_sharp_s_fully() {
		assert_eq!(name_key("stra\u{df}e", true), name_key("STRASSE", true));
		assert_eq!(name_key("\u{1e9e}", true), name_key("ss", true));
		assert_ne!(name_key("\u{df}", false), name_key("ss", false));
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

	/// Names that spell one existing folder two ways: `/ws/Sub` and
	/// `/ws/sub` are one object, as on a filesystem that folds case (a
	/// canonical path keeps whichever spelling it was given).
	fn same_object(spellings: &'static [&'static str]) -> NewFolderNames {
		let roots = spellings.iter().map(|r| (PathBuf::from(r), true));
		NewFolderNames::with_identity(
			roots.collect(),
			Box::new(move |path| {
				let one = spellings.iter().any(|s| Path::new(s) == path);
				DirKey::Path(if one {
					PathBuf::from(spellings[0])
				} else {
					path.to_path_buf()
				})
			}),
		)
	}

	/// The folder the user named under one spelling of an existing folder is
	/// named when a path reaches it through another spelling of that folder.
	#[test]
	fn one_existing_folder_spelled_two_ways_is_one_anchor() {
		let mut n = same_object(&["/ws/Sub", "/ws/sub"]);
		let chain = n.name_chain(Path::new("/ws/Sub"), &tail(&["new"]));
		let missing: HashSet<PathBuf> = [PathBuf::from("/ws/sub/new")].into();
		assert_eq!(n.requested(&missing, &chain), missing);
		// By identity alone, without the spelling to fall back on.
		assert_eq!(n.requested(&missing, &[]), missing);
	}

	/// The same two spellings do not make a folder under a third one the
	/// named folder.
	#[test]
	fn another_existing_folder_is_another_anchor() {
		let mut n = same_object(&["/ws/Sub", "/ws/sub"]);
		n.name_chain(Path::new("/ws/Sub"), &tail(&["new"]));
		let missing: HashSet<PathBuf> = [PathBuf::from("/ws/other/new")].into();
		assert_eq!(n.requested(&missing, &[]), HashSet::new());
	}

	/// A folder of the tail that appears between resolving and planning
	/// moves the anchor of what is still missing. That is not a reason to
	/// panic: the folder named is asked about, and the plan is stale when
	/// applied.
	#[test]
	fn a_tail_folder_made_in_between_is_still_requested() {
		let mut n = names(true, &["/ws"]);
		let chain = n.name_chain(Path::new("/ws"), &tail(&["a", "b"]));
		let missing: HashSet<PathBuf> = [PathBuf::from("/ws/a/b")].into();
		assert_eq!(n.requested(&missing, &chain), missing);
	}

	/// Real folders: two spellings of one folder get one identity, two
	/// folders get two. Case is whatever the filesystem here does.
	#[test]
	fn the_filesystem_object_decides_which_folder() {
		let tmp = tempfile::tempdir().unwrap();
		let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
		fs::create_dir(&a).unwrap();
		fs::create_dir(&b).unwrap();
		assert_eq!(DirKey::of(&a), DirKey::of(&tmp.path().join("./a")));
		assert_ne!(DirKey::of(&a), DirKey::of(&b));
		assert_eq!(
			DirKey::of(&tmp.path().join("missing")),
			DirKey::Path(tmp.path().join("missing"))
		);
	}

	/// A folder that cannot be read (a drop box, `-wx`) is still told by its
	/// object: a stat needs only the right to search the folders above it.
	/// Told by a path instead, two spellings of it would be two folders.
	#[cfg(unix)]
	#[test]
	fn an_unreadable_folder_is_told_by_its_object() {
		use std::os::unix::fs::PermissionsExt;

		struct Restore(PathBuf);
		impl Drop for Restore {
			fn drop(&mut self) {
				let _ = fs::set_permissions(
					&self.0,
					fs::Permissions::from_mode(0o755),
				);
			}
		}

		let tmp = tempfile::tempdir().unwrap();
		let a = tmp.path().join("a");
		fs::create_dir(&a).unwrap();
		let _restore = Restore(a.clone());
		fs::set_permissions(&a, fs::Permissions::from_mode(0o311)).unwrap();
		let key = DirKey::of(&a);
		assert!(matches!(key, DirKey::Object(..)), "{key:?}");
		assert_eq!(key, DirKey::of(&tmp.path().join("./a")));
		assert_ne!(key, DirKey::of(&tmp.path().join("missing")));
	}

	/// A key holds no file descriptor: the plan keeps one per distinct
	/// existing folder, and a terminal's default limit is 256.
	#[cfg(unix)]
	#[test]
	fn keys_for_many_folders_do_not_run_out_of_descriptors() {
		use nix::sys::resource::{getrlimit, setrlimit, Resource};

		let tmp = tempfile::tempdir().unwrap();
		let dirs: Vec<PathBuf> = (0..300)
			.map(|i| {
				let d = tmp.path().join(format!("d{i}"));
				fs::create_dir(&d).unwrap();
				d
			})
			.collect();
		// One test per process under nextest, so the limit stays here.
		let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
		let low = 128.min(hard);
		setrlimit(Resource::RLIMIT_NOFILE, low.min(soft), hard).unwrap();
		let keys: Vec<DirKey> = dirs.iter().map(|d| DirKey::of(d)).collect();
		assert!(
			keys.iter().all(|k| matches!(k, DirKey::Object(..))),
			"{keys:?}"
		);
	}
}
