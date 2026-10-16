//! Transfer planning: safe multi-repo export and import planning with freshness validation.
//!
//! Provides pure/shared planning for exporting from and importing to Git repositories
//! and workspace roots, preserving v1 wire format compatibility while enforcing
//! explicit root mapping, conflict detection, cumulative bounded reads, and freshness validation.
//!
//! `plan_export_with`, `plan_import_with`, `CommitReplayPreview::capture_with`
//! and `CommitReplayPreview::apply_with` thread one `RunOptions` through Git
//! and freshness reads; see `apply_with` for its cancel boundary and error shape.
//! A regular-file `read` cannot be stopped mid-syscall; the token is polled before
//! open, between chunks and after the read. A FIFO or other non-regular file is
//! rejected before `open`. Confirmed `TransferImportPlan::apply` and
//! `CommitReplayPreview::apply_with` do not poll a cancel token during replay.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::blob::{BlobRead, BlobReader, DeletedContent};
use crate::commits::{self, CommitError, CommitExport, CommitsPayload};
use crate::filter;
use crate::foldernames::{DirKey, NewFolderNames, TargetId};
use crate::format::{
	self, BorrowedPayloadOptions, ChangeType, ParsedEntry, PayloadFile,
};
use crate::fsutil::decode_utf8_or_skip;
use crate::gitrun::{CancelToken, RunOptions, RunOutput};
use crate::gitsrc::{
	Git, GitError, DELETED_FILE_MARKER, UNREADABLE_FILE_MARKER,
};
use crate::paths::{self, escapes_all_roots, sanitize_relative_path};
use crate::reason::{io_reason, Reason, StaleChange};
use crate::restore::{
	self, CreateOperation, DeleteOperation, RestoreExecutionResult,
	RestorePlan, RestoreSelection, SkipReason, SkippedOperation,
};
use crate::settings::Settings;
use crate::stats::{payload_stats, PayloadStats};
use crate::workspace::{lock_heavy, HeavyGuard, RepoIdentity};

mod changes;
mod select;

pub use crate::commits::ReplayPreviewUnavailable;
pub use changes::{changed_items, ChangedItems};
pub use select::{
	copy_selection, copy_selection_detailed, expand_folder_items,
	expand_folder_items_in_input_order, is_safe_dir_symlink,
	plan_export_expanding, selection_from_paths, CopyOutcome, CopyReport,
	FolderExpansion, PathSelection,
};

/// A rename has two visible rows: deleting the source and writing the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayFileSide {
	Target,
	RenamedFrom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayPreviewRead {
	Text(String),
	Unavailable(ReplayPreviewUnavailable),
}

/// Plain file contents, excluding commit metadata. An unavailable comparison
/// is explicit; callers must not render its empty before as an added file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayFilePreview {
	pub path: String,
	pub before: String,
	pub after: String,
	pub unavailable: Option<ReplayPreviewUnavailable>,
}

/// Bounded, cancellable reader shared by local paste and the remote worker.
/// Replay replaces symlinks, so their targets must never become its baseline.
pub fn read_replay_preview_with(
	path: &Path,
	limit: usize,
	opts: &RunOptions,
) -> Result<ReplayPreviewRead, TransferError> {
	use ReplayPreviewUnavailable as Why;
	let unavailable = |why| Ok(ReplayPreviewRead::Unavailable(why));
	cancelled_err(opts, "replay-preview-read")?;
	let meta = match fs::symlink_metadata(path) {
		Ok(meta) => meta,
		Err(_) => return unavailable(Why::Unreadable),
	};
	if meta.file_type().is_symlink() {
		return unavailable(Why::Symlink);
	}
	if meta.is_dir() {
		return unavailable(Why::Directory);
	}
	if !meta.is_file() {
		return unavailable(Why::Unreadable);
	}
	let limit = limit.min(crate::browser::PREVIEW_LIMIT);
	if meta.len() > limit as u64 {
		return unavailable(Why::TooLarge);
	}
	let mut open = fs::OpenOptions::new();
	open.read(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
	}
	#[cfg(windows)]
	{
		use std::os::windows::fs::OpenOptionsExt;
		// FILE_FLAG_OPEN_REPARSE_POINT: open the link, not its target.
		open.custom_flags(0x00200000);
	}
	let mut file = match open.open(path) {
		Ok(file) => file,
		Err(_) => return unavailable(Why::Unreadable),
	};
	if !file.metadata()?.file_type().is_file() {
		return unavailable(Why::Unreadable);
	}
	let mut bytes = Vec::new();
	let mut chunk = [0u8; FILE_IO_CHUNK];
	loop {
		cancelled_err(opts, "replay-preview-read")?;
		let cap = chunk.len().min(limit.saturating_add(1) - bytes.len());
		let n = file.read(&mut chunk[..cap])?;
		if n == 0 {
			break;
		}
		bytes.extend_from_slice(&chunk[..n]);
		if bytes.len() > limit {
			return unavailable(Why::TooLarge);
		}
	}
	cancelled_err(opts, "replay-preview-read")?;
	Ok(match decode_utf8_or_skip(bytes) {
		Some(text) => ReplayPreviewRead::Text(text),
		None => ReplayPreviewRead::Unavailable(Why::Binary),
	})
}

/// A stable canonical identifier for an existing, resolved workspace or repository root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalRootId(PathBuf);

impl CanonicalRootId {
	/// Creates an ID by canonicalizing `path`. Requires the path to exist and be resolvable.
	pub fn new(path: impl AsRef<Path>) -> Result<Self, io::Error> {
		let p = path.as_ref();
		let canonical = dunce::canonicalize(p)?;
		Ok(Self(canonical))
	}

	/// Validates that this ID represents an existing, canonicalized absolute path.
	/// Protects against unchecked construction via deserialization.
	pub fn validate(&self) -> Result<(), io::Error> {
		let canonical = dunce::canonicalize(&self.0)?;
		if canonical != self.0 {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!(
					"root path '{}' is not canonical (resolves to '{}')",
					self.0.display(),
					canonical.display()
				),
			));
		}
		Ok(())
	}

	pub fn path(&self) -> &Path {
		&self.0
	}

	/// Heap storage owned by this canonical path; the enclosing owner counts
	/// the inline `CanonicalRootId` once. Uses capacity, including spare bytes.
	pub fn retained_heap_bytes(&self) -> usize {
		self.0.capacity()
	}
}

impl std::fmt::Display for CanonicalRootId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0.display())
	}
}

/// A paste folder that may not exist yet: its nearest existing ancestor
/// (canonical) and the names below it still to be made. A folder that
/// exists has an empty tail and is exactly its `CanonicalRootId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlacedRoot {
	anchor: CanonicalRootId,
	tail: Vec<String>,
}

/// Why a paste folder cannot be used as a destination.
#[derive(Debug, thiserror::Error)]
pub enum DestinationError {
	#[error("'{}' is not a folder", .path.display())]
	NotADirectory { path: PathBuf },
	#[error("'{}' is outside the workspace", .path.display())]
	OutsideBound { path: PathBuf },
	#[error("'{name}' cannot be a folder name here")]
	InvalidName { name: String },
	#[error(transparent)]
	Io(#[from] io::Error),
}

impl DestinationError {
	/// What a front end words: the variant survives into `PasteError::reason()`.
	pub fn reason(&self) -> Reason {
		match self {
			Self::NotADirectory { path } => Reason::NotRegularFile {
				path: path.display().to_string(),
			},
			Self::OutsideBound { path } => Reason::OutsideRoot {
				path: path.display().to_string(),
			},
			Self::InvalidName { name } => {
				Reason::UnsafePath { path: name.clone() }
			}
			Self::Io(e) => io_reason(e, ""),
		}
	}
}

/// Wraps the error as the payload of an `io::Error` (NotADirectory keeps the
/// kind of that name, OutsideBound is PermissionDenied, InvalidName is
/// InvalidInput; `Io(e)` is `e` itself), so `PasteError::Destination` keeps
/// its shape and `PasteError::reason()` can still downcast it.
impl From<DestinationError> for io::Error {
	fn from(err: DestinationError) -> Self {
		let kind = match &err {
			DestinationError::NotADirectory { .. } => {
				io::ErrorKind::NotADirectory
			}
			DestinationError::OutsideBound { .. } => {
				io::ErrorKind::PermissionDenied
			}
			DestinationError::InvalidName { .. } => io::ErrorKind::InvalidInput,
			DestinationError::Io(_) => {
				let DestinationError::Io(e) = err else {
					unreachable!()
				};
				return e;
			}
		};
		io::Error::new(kind, err)
	}
}

impl From<CanonicalRootId> for PlacedRoot {
	fn from(anchor: CanonicalRootId) -> Self {
		Self {
			anchor,
			tail: Vec::new(),
		}
	}
}

impl PlacedRoot {
	/// Reads `path` (absolute) and finds the folder it names: the deepest
	/// ancestor that exists as the anchor, the names below it as the tail.
	/// Writes nothing. `bound` is a folder the anchor must lie in, symlinks
	/// resolved; a name below the anchor is made there, so it cannot leave.
	///
	/// A name that is not made yet must be a plain, ordinary folder name:
	/// no `..`, `.git`, character Windows or a path separator refuses,
	/// trailing dot or space, or Windows reserved name; and nothing is made
	/// inside a Git directory. A `..` in the part that exists is resolved by
	/// the OS, as `CanonicalRootId::new` always did.
	pub fn resolve(
		bound: Option<&Path>,
		path: &Path,
	) -> Result<Self, DestinationError> {
		if !path.is_absolute() {
			return Err(DestinationError::InvalidName {
				name: path.display().to_string(),
			});
		}
		// Windows reads `new\..\x` as `x` without looking at `new`, so a
		// `..` after a name that does not exist would pass there and be
		// refused elsewhere: refused on every system.
		let mut prefix = PathBuf::new();
		let mut missing = false;
		for part in path.components() {
			if missing && part == std::path::Component::ParentDir {
				return Err(DestinationError::InvalidName {
					name: "..".to_string(),
				});
			}
			prefix.push(part);
			if !missing && matches!(part, std::path::Component::Normal(_)) {
				missing = fs::symlink_metadata(&prefix).is_err_and(|e| {
					matches!(
						e.kind(),
						io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
					)
				});
			}
		}
		let mut cur = path;
		let mut tail: Vec<String> = Vec::new();
		let anchor = loop {
			match fs::symlink_metadata(cur) {
				Ok(meta) if meta.is_dir() => break CanonicalRootId::new(cur)?,
				Ok(meta) if meta.file_type().is_symlink() => {
					if fs::metadata(cur).is_ok_and(|m| m.is_dir()) {
						break CanonicalRootId::new(cur)?;
					}
					return Err(DestinationError::NotADirectory {
						path: cur.to_path_buf(),
					});
				}
				Ok(_) => {
					return Err(DestinationError::NotADirectory {
						path: cur.to_path_buf(),
					})
				}
				Err(e)
					if matches!(
						e.kind(),
						io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
					) =>
				{
					match cur.components().next_back() {
						Some(std::path::Component::Normal(name)) => {
							let Some(name) = name.to_str() else {
								return Err(DestinationError::InvalidName {
									name: name.to_string_lossy().into_owned(),
								});
							};
							tail.push(name.to_string());
						}
						Some(std::path::Component::CurDir) => {}
						Some(other) => {
							return Err(DestinationError::InvalidName {
								name: other
									.as_os_str()
									.to_string_lossy()
									.into_owned(),
							})
						}
						None => return Err(DestinationError::Io(e)),
					}
					match cur.parent() {
						Some(parent) => cur = parent,
						None => return Err(DestinationError::Io(e)),
					}
				}
				Err(e) => return Err(DestinationError::Io(e)),
			}
		};
		tail.reverse();
		for name in &tail {
			let plain = sanitize_relative_path(name).as_deref()
				== Some(name.as_str())
				&& !name.contains('/')
				&& !paths::has_git_segment(name)
				&& !paths::is_unfit_new_folder_name(name);
			if !plain {
				return Err(DestinationError::InvalidName {
					name: name.clone(),
				});
			}
		}
		if let Some(bound) = bound {
			let bound_real = dunce::canonicalize(bound)?;
			if !paths::is_contained(anchor.path(), &bound_real) {
				return Err(DestinationError::OutsideBound {
					path: anchor.path().to_path_buf(),
				});
			}
		}
		let placed = Self { anchor, tail };
		if !placed.tail.is_empty() && paths::lands_in_git_dir(&placed.path()) {
			return Err(DestinationError::InvalidName {
				name: placed.tail[0].clone(),
			});
		}
		Ok(placed)
	}

	/// The nearest existing ancestor.
	pub fn anchor(&self) -> &CanonicalRootId {
		&self.anchor
	}

	/// The names below the anchor that are not made yet, shallow to deep.
	pub fn tail(&self) -> &[String] {
		&self.tail
	}

	/// The tail joined with `/`; empty when the folder exists.
	pub fn tail_rel(&self) -> String {
		self.tail.join("/")
	}

	/// The folder itself, lexically: the anchor with the tail below it.
	pub fn path(&self) -> PathBuf {
		let mut path = self.anchor.path().to_path_buf();
		path.extend(&self.tail);
		path
	}

	/// anchor/t1, anchor/t1/t2, ... (shallow to deep); empty when it exists.
	/// Built with `PathBuf::push` per name, never by joining strings: the
	/// keys must compare equal to `missing_dirs`, which come from
	/// `op.absolute_path.parent()` (Windows separators included).
	pub fn new_chain(&self) -> Vec<PathBuf> {
		let mut path = self.anchor.path().to_path_buf();
		self.tail
			.iter()
			.map(|name| {
				path.push(name);
				path.clone()
			})
			.collect()
	}

	/// Heap storage owned by this value; the enclosing owner counts the
	/// inline struct once.
	pub fn retained_heap_bytes(&self) -> usize {
		self.tail.iter().fold(
			self.anchor.retained_heap_bytes().saturating_add(
				self.tail.capacity() * std::mem::size_of::<String>(),
			),
			|bytes, name| bytes.saturating_add(name.capacity()),
		)
	}
}

/// Where a file was selected from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
	/// Workspace file mode (equivalent to copy.rs file collection).
	File,
	/// Uncommitted Git working tree change: the SCM view (legacy
	/// `GitSource::Working`), a deletion reads HEAD.
	Working,
	/// Unstaged change only (working tree against the index): content from
	/// disk, a deletion reads the index.
	Unstaged,
	/// Staged Git index change.
	Staged,
	/// Git commit change.
	Commit { rev: String },
	/// Git revision range comparison (endpoint comparison of base and tip).
	Range { base: String, tip: String },
}

/// An individual item in an export selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportItem {
	pub root: CanonicalRootId,
	pub relative_path: String,
	pub source: SourceKind,
	pub change_type: Option<ChangeType>,
	pub gitlink: bool,
}

/// An export selection spanning one or more verified canonical roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSelection {
	pub roots: Vec<CanonicalRootId>,
	pub primary_root: Option<CanonicalRootId>,
	pub items: Vec<ExportItem>,
	pub source_root: Option<String>,
	pub spelled_root: Option<PathBuf>,
	/// Base used to spell the path that Commit/Range items are filter-matched against;
	/// do not reuse spelled_root (that is the root's own spelling and feeds only the File branch).
	pub filter_root: Option<PathBuf>,
}

impl ExportSelection {
	pub fn new(
		roots: Vec<PathBuf>,
		primary_root: Option<PathBuf>,
		items: Vec<ExportItem>,
	) -> Result<Self, TransferError> {
		let mut canonical_roots = Vec::with_capacity(roots.len());
		for r in roots {
			let id = CanonicalRootId::new(&r)?;
			id.validate()?;
			canonical_roots.push(id);
		}
		let primary = match primary_root {
			Some(p) => {
				let id = CanonicalRootId::new(&p)?;
				id.validate()?;
				Some(id)
			}
			None => None,
		};
		let sel = Self {
			roots: canonical_roots,
			primary_root: primary,
			items,
			source_root: None,
			spelled_root: None,
			filter_root: None,
		};
		validate_export_selection(&sel)?;
		Ok(sel)
	}

	pub fn with_source_root(mut self, source_root: Option<String>) -> Self {
		self.source_root = source_root;
		self
	}

	pub fn with_spelled_root(mut self, spelled_root: Option<PathBuf>) -> Self {
		self.spelled_root = spelled_root;
		self
	}

	/// Base used to spell the path that Commit/Range items are filter-matched against;
	/// do not reuse spelled_root (that is the root's own spelling and feeds only the File branch).
	pub fn with_filter_root(mut self, filter_root: Option<PathBuf>) -> Self {
		self.filter_root = filter_root;
		self
	}

	/// A selection over the same roots with `items` replaced, carrying over
	/// the optional fields [`ExportSelection::new`] does not set
	/// (`source_root`, `spelled_root`, `filter_root`): the folder walk and
	/// the truncation-prefix rebuilds must not drop them, or a commit copy
	/// from a subfolder loses the root its relative filters match against.
	pub fn with_items(
		&self,
		items: Vec<ExportItem>,
	) -> Result<Self, TransferError> {
		Ok(ExportSelection::new(
			self.roots.iter().map(|r| r.path().to_path_buf()).collect(),
			self.primary_root.as_ref().map(|r| r.path().to_path_buf()),
			items,
		)?
		.with_source_root(self.source_root.clone())
		.with_spelled_root(self.spelled_root.clone())
		.with_filter_root(self.filter_root.clone()))
	}
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
	#[error("no files or changes selected for export")]
	EmptySelection,
	#[error("unsafe path: '{0}' escapes root boundaries or contains invalid characters")]
	UnsafePath(String),
	#[error("special file '{0}' is not a regular file and cannot be exported")]
	SpecialFile(String),
	#[error(
		"'{0}' exists and is not a regular file; paste refuses to overwrite it"
	)]
	DestinationNotRegular(String),
	#[error("path not found: '{}'", .0.display())]
	PathNotFound(PathBuf),
	#[error("path '{}' is outside root", .0.display())]
	PathOutsideRoot(PathBuf),
	#[error("unknown root: '{}' was not declared in selection or destination roots", .0.display())]
	UnknownRoot(PathBuf),
	#[error(
		"conflict: file '{path}' in '{}' is selected as both Staged and Working",
		.root.display()
	)]
	StagedWorkingConflict { root: PathBuf, path: String },
	#[error(
		"collision: file '{path}' in '{}' has conflicting sources: {msg}",
		.root.display()
	)]
	MixedSourceCollision {
		root: PathBuf,
		path: String,
		msg: String,
	},
	#[error(
		"ambiguous root basenames: multiple roots share basename '{basename}': {}",
		.roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
	)]
	AmbiguousRootBasenames {
		basename: String,
		roots: Vec<PathBuf>,
	},
	#[error(
		"wire header collision: multiple items map to identical wire path '{wire_path}'"
	)]
	WireHeaderCollision { wire_path: String },
	#[error("target collision: multiple operations target '{}': {msg}", .path.display())]
	TargetCollision { path: PathBuf, msg: String },
	#[error(
		"payload limit exceeded: limit {limit}, actual {actual} ({reason})"
	)]
	PayloadLimitExceeded {
		limit: usize,
		actual: usize,
		reason: String,
		/// The file that did not fit (wire path), as data; `None` when the
		/// wrapper overhead alone is over the limit.
		path: Option<String>,
	},
	#[error("stale source in '{}': {reason}", .root.display())]
	StaleSource {
		root: PathBuf,
		reason: String,
		/// What changed, as data (`reason` is the English text).
		change: StaleChange,
	},
	#[error("stale destination in '{}': {reason}", .root.display())]
	StaleDestination {
		root: PathBuf,
		reason: String,
		/// What changed, as data (`reason` is the English text).
		change: StaleChange,
	},
	#[error(
		"unmapped entry: '{raw_path}' cannot be routed to any destination root"
	)]
	UnmappedEntry { raw_path: String },
	#[error("cross-repository commit replay is not supported")]
	CrossRepoCommitsNotSupported,
	#[error(
		"commits are not contiguous: following first parents back from {tip}, {at} has {}, so {base} is never reached",
		first_parent.as_deref().map_or("no parent".to_string(), |p| format!("first parent {p}"))
	)]
	DiscontinuousCommits {
		base: String,
		tip: String,
		at: String,
		first_parent: Option<String>,
	},
	#[error(
		"{} folder(s) would be created without consent: {}",
		.dirs.len(),
		.dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
	)]
	NewDirsNotConfirmed { dirs: Vec<PathBuf> },
	#[error(transparent)]
	Git(#[from] GitError),
	#[error(transparent)]
	Commit(#[from] CommitError),
	#[error(transparent)]
	Io(#[from] io::Error),
	/// The Apply was refused after it made empty files or folders, and
	/// taking some of them back failed: they are still there.
	#[error(
		"{cause}; what this paste made could not all be taken back: {}",
		.left.join(", ")
	)]
	NotTakenBack {
		cause: Box<TransferError>,
		/// `path: why`, each.
		left: Vec<String>,
	},
}

impl TransferError {
	/// `cause`, and `left` when taking back what was made failed for some.
	fn left_behind(cause: Self, left: Vec<NotTakenBack>) -> Self {
		if left.is_empty() {
			return cause;
		}
		Self::NotTakenBack {
			cause: Box::new(cause),
			left: left
				.iter()
				.map(|(path, e)| format!("{}: {e}", path.display()))
				.collect(),
		}
	}

	/// Why this failed, as data for a front end to word. The `Display` text
	/// stays the CLI's. An exhaustive match: a new variant must decide.
	pub fn reason(&self) -> Reason {
		let shown = |p: &Path| p.display().to_string();
		match self {
			Self::EmptySelection => Reason::EmptySelection,
			Self::UnsafePath(p) => Reason::UnsafePath { path: p.clone() },
			Self::SpecialFile(p) | Self::DestinationNotRegular(p) => {
				Reason::NotRegularFile { path: p.clone() }
			}
			Self::PathNotFound(p) => Reason::NotFound { path: shown(p) },
			Self::PathOutsideRoot(p) | Self::UnknownRoot(p) => {
				Reason::OutsideRoot { path: shown(p) }
			}
			Self::UnmappedEntry { raw_path } => Reason::OutsideRoot {
				path: raw_path.clone(),
			},
			Self::StagedWorkingConflict { path, .. } => {
				Reason::StagedAndWorking { path: path.clone() }
			}
			Self::MixedSourceCollision { path, .. } => {
				Reason::Collision { path: path.clone() }
			}
			Self::WireHeaderCollision { wire_path } => Reason::Collision {
				path: wire_path.clone(),
			},
			Self::TargetCollision { path, .. } => {
				Reason::Collision { path: shown(path) }
			}
			Self::AmbiguousRootBasenames { basename, roots } => {
				Reason::AmbiguousRoots {
					basename: basename.clone(),
					roots: roots.iter().map(|r| shown(r)).collect(),
				}
			}
			Self::PayloadLimitExceeded {
				limit,
				actual,
				path,
				..
			} => Reason::PayloadTooLarge {
				limit: *limit as u64,
				actual: *actual as u64,
				path: path.clone(),
			},
			Self::StaleSource { change, .. } => Reason::SourceChanged {
				change: change.clone(),
			},
			Self::StaleDestination { change, .. } => {
				Reason::DestinationChanged {
					change: change.clone(),
				}
			}
			Self::NewDirsNotConfirmed { dirs } => Reason::NewDirsNotConfirmed {
				count: dirs.len() as u64,
			},
			Self::CrossRepoCommitsNotSupported => Reason::CrossRepoCommits,
			Self::DiscontinuousCommits {
				base,
				tip,
				at,
				first_parent,
			} => Reason::Discontinuous {
				base: base.clone(),
				tip: tip.clone(),
				at: at.clone(),
				first_parent: first_parent.clone(),
			},
			Self::Git(e) => e.reason(),
			Self::Commit(e) => e.reason(),
			Self::Io(e) => io_reason(e, ""),
			// Why it was refused: what is left is in the text.
			Self::NotTakenBack { cause, .. } => cause.reason(),
		}
	}

	pub fn paste_message(&self) -> String {
		match self {
			Self::SpecialFile(path) | Self::DestinationNotRegular(path) => {
				format!(
					"'{path}' exists and is not a regular file; paste refuses to overwrite it"
				)
			}
			other => other.to_string(),
		}
	}
}

/// Cryptographic and timestamp identity of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFreshness {
	pub size: u64,
	pub mtime: SystemTime,
	pub content_hash: [u8; 32],
}

/// Freshness state of a repository (HEAD commit, symbolic ref, and index).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFreshness {
	pub head_commit: Option<String>,
	pub head_ref: Option<String>,
	pub index_hash: Option<[u8; 32]>,
}

/// Freshness snapshot captured at export planning time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFreshnessSnapshot {
	pub repos: HashMap<CanonicalRootId, RepoFreshness>,
	pub frozen_commits: HashMap<(CanonicalRootId, String), String>,
	pub working_files:
		HashMap<(CanonicalRootId, String), Option<FileFreshness>>,
}

impl SourceFreshnessSnapshot {
	pub fn capture(selection: &ExportSelection) -> Result<Self, TransferError> {
		for root in &selection.roots {
			root.validate()?;
		}
		if let Some(ref primary) = selection.primary_root {
			primary.validate()?;
		}
		validate_export_selection(selection)?;

		let opts = RunOptions::default();
		let mut repos = HashMap::new();
		for root in &selection.roots {
			repos.insert(
				root.clone(),
				capture_repo_freshness(root.path(), &opts)?,
			);
		}

		let mut frozen_commits = HashMap::new();
		let mut working_files = HashMap::new();

		for item in &selection.items {
			if item.gitlink
				&& item.change_type != Some(ChangeType::Deleted)
				&& matches!(
					item.source,
					SourceKind::Working | SourceKind::Unstaged
				) {
				continue;
			}
			match &item.source {
				SourceKind::Working
				| SourceKind::Unstaged
				| SourceKind::File => {
					let file_path = item.root.path().join(&item.relative_path);
					let freshness = capture_file_freshness(
						&file_path,
						&opts,
						TransferError::SpecialFile,
					)?;
					working_files.insert(
						(item.root.clone(), item.relative_path.clone()),
						freshness,
					);
				}
				SourceKind::Commit { rev } => {
					let key = (item.root.clone(), rev.clone());
					if let std::collections::hash_map::Entry::Vacant(e) =
						frozen_commits.entry(key)
					{
						let git = Git::open_with(item.root.path(), &opts)?;
						let oid = git.resolve_commit_with(rev, &opts)?;
						e.insert(oid);
					}
				}
				SourceKind::Range { base, tip } => {
					for rev in [base, tip] {
						let key = (item.root.clone(), rev.clone());
						if let std::collections::hash_map::Entry::Vacant(e) =
							frozen_commits.entry(key)
						{
							let git = Git::open_with(item.root.path(), &opts)?;
							let oid = git.resolve_commit_with(rev, &opts)?;
							e.insert(oid);
						}
					}
				}
				SourceKind::Staged => {}
			}
		}

		Ok(Self {
			repos,
			frozen_commits,
			working_files,
		})
	}

	pub fn revalidate(&self) -> Result<(), TransferError> {
		self.revalidate_with(&RunOptions::default())
	}

	/// Revalidates with the caller's runner options so a cancel token is still
	/// honored immediately before the clipboard handoff.
	pub fn revalidate_with(
		&self,
		opts: &RunOptions,
	) -> Result<(), TransferError> {
		cancelled_err(opts, "revalidate-source")?;
		for (root, prev_repo) in &self.repos {
			cancelled_err(opts, "revalidate-source")?;
			revalidate_repo_freshness(root.path(), prev_repo, opts)?;
		}

		for ((root, rel), prev_file) in &self.working_files {
			cancelled_err(opts, "revalidate-source")?;
			let file_path = root.path().join(rel);
			let current = capture_file_freshness(
				&file_path,
				opts,
				TransferError::SpecialFile,
			)?;
			if current != *prev_file {
				return Err(TransferError::StaleSource {
					root: root.path().to_path_buf(),
					reason: format!(
						"working file '{rel}' was modified or deleted externally"
					),
					change: StaleChange::Modified {
						rel: rel.to_string(),
					},
				});
			}
		}

		Ok(())
	}
}

/// The result of an export plan ready to be serialized to clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
	pub files: Vec<PayloadFile>,
	pub payload: String,
	pub stats: PayloadStats,
	pub freshness: SourceFreshnessSnapshot,
	pub copied_file_count: usize,
	pub skipped_file_size_count: usize,
	pub skipped_unreadable_count: usize,
	pub file_limit_reached: bool,
}

impl ExportPlan {
	/// Revalidates freshness right before clipboard writing.
	pub fn revalidate(&self) -> Result<(), TransferError> {
		self.revalidate_with(&RunOptions::default())
	}

	/// [`ExportPlan::revalidate`] with the caller's cancel token and limits.
	pub fn revalidate_with(
		&self,
		opts: &RunOptions,
	) -> Result<(), TransferError> {
		self.freshness.revalidate_with(opts)
	}
}

/// Single copy cap for CLI and GUI (32 MiB).
///
/// Guaranteed to not exceed GUI paste preview budget; exceeding the cap
/// is a strict error, never silent truncation.
pub const CLIPBOARD_PAYLOAD_MAX: usize = 32 * 1024 * 1024;

/// Explicit destination mapping for a specific entry path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMapping {
	pub root: CanonicalRootId,
	pub relative_path: Option<String>,
}

/// Explicit destination mappings for importing clipboard payloads.
#[derive(Debug, Clone, Default)]
pub struct ImportMapping {
	/// A destination folder may not exist yet: it is a [`PlacedRoot`], the
	/// anchor the engine plans against plus the names still to be made.
	pub primary_destination: Option<PlacedRoot>,
	/// Nest every entry that lands in the primary destination under this directory (applies to relative entries only).
	pub primary_prefix: Option<String>,
	pub prefix_destinations: HashMap<String, PlacedRoot>,
	pub entry_destinations: HashMap<String, EntryMapping>,
	pub blocked_prefixes: HashSet<String>,
}

impl ImportMapping {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn with_primary(root: impl Into<PlacedRoot>) -> Self {
		Self {
			primary_destination: Some(root.into()),
			..Default::default()
		}
	}

	pub fn from_restore_base(
		suggestion: &restore::RestoreBaseSuggestion,
		primary: impl Into<PlacedRoot>,
	) -> Self {
		let primary = primary.into();
		match &suggestion.base {
			restore::RestoreBase::Strip { segment } => {
				let mut mapping = Self::with_primary(primary.clone());
				mapping.map_prefix(segment, primary);
				mapping
			}
			restore::RestoreBase::Add { prefix } => Self {
				primary_destination: Some(primary),
				primary_prefix: Some(prefix.clone()),
				..Default::default()
			},
		}
	}

	pub fn map_prefix(
		&mut self,
		prefix: impl Into<String>,
		root: impl Into<PlacedRoot>,
	) -> &mut Self {
		self.prefix_destinations.insert(prefix.into(), root.into());
		self
	}

	pub fn map_entry(
		&mut self,
		entry_path: impl Into<String>,
		root: CanonicalRootId,
		relative_path: Option<String>,
	) -> &mut Self {
		self.entry_destinations.insert(
			entry_path.into(),
			EntryMapping {
				root,
				relative_path,
			},
		);
		self
	}

	pub fn block_prefix(&mut self, prefix: impl Into<String>) -> &mut Self {
		self.blocked_prefixes.insert(prefix.into());
		self
	}
}

/// Target file state recorded at preview time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFileFreshness {
	pub root: CanonicalRootId,
	pub relative_path: String,
	pub existed: bool,
	pub file_state: Option<FileFreshness>,
}

/// Destination freshness snapshot captured during import preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationFreshnessSnapshot {
	pub roots: HashMap<CanonicalRootId, RepoFreshness>,
	pub target_files: HashMap<PathBuf, TargetFileFreshness>,
	/// Every repository an Apply of any planned operation would lock (the
	/// nearest `.git` above each target's nearest existing folder), keyed by
	/// that folder. `None`: not recorded (commit replay, or an older master
	/// that dropped the field).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub repos: Option<HashMap<CanonicalRootId, RepoFreshness>>,
	/// Folders on a planned create's path that did not exist at preview.
	/// Apply accepts them still missing or now a real folder; a file or a
	/// symlink there is stale.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub missing_dirs: Option<HashSet<PathBuf>>,
	/// Folders on a planned target's path that were symlinks to a folder at
	/// preview (below the target's root), each with the canonical folder it
	/// pointed at then. A folder that is a symlink now but was not one then,
	/// or that points elsewhere, would carry the write somewhere the preview
	/// did not show, so it is stale. `None`: not recorded.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub symlinked_dirs: Option<HashMap<PathBuf, PathBuf>>,
	/// The folders of `missing_dirs` the user named as a destination or an
	/// import path (a `PlacedRoot`'s tail), as opposed to those a file path
	/// implies. Apply makes them only with consent, and one that exists by
	/// then, as anything, is stale. `None`: not recorded (no tail, an older
	/// peer).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub requested_dirs: Option<HashSet<PathBuf>>,
	/// Each placed folder with a tail (the primary destination, then each
	/// prefix by name; one entry for two that are the same anchor and
	/// tail): every level of its tail, spelled as it was resolved
	/// (`anchor/t1`, `anchor/t1/t2`, ...), what did not exist when the user
	/// placed the folder. Its place in this list is what the user agrees
	/// to (`RestoreSelection::confirmed_placed`). Apply holds the plan to
	/// these, apart from how planning classified the folders
	/// (`requested_dirs`): a level that exists is stale; one a kept row is
	/// routed through is made only with consent; and a level of one not
	/// agreed to that exists once this Apply made its folders (however the
	/// filesystem matched the names) refuses the whole paste. `None`: no
	/// tail, or an older peer.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub placed: Option<Vec<Vec<PathBuf>>>,
	/// The folders of `missing_dirs` each planned create needs, worked out
	/// where the plan was made ([`PlannedNewDirs`]). A master reads a
	/// worker's paths only as they are written: it never takes them apart
	/// by its own path rules (on Unix `\` is part of a name), so what a
	/// worker on another OS makes comes from here. Recorded only when the
	/// plan places a folder (`placed`), so a plan without one is what
	/// it was. `None`: no tail, or an older peer; then the folders are read
	/// off the paths, right only when both machines share path rules.
	/// Not compared by `matches_preview`: it follows from the plan the
	/// digest covers and `missing_dirs`, and a worker's Apply works it out
	/// again from its own plan.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub new_dirs: Option<PlannedNewDirs>,
}

/// The folders a plan's creates need made, as the planning machine reads
/// its own paths: [`DestinationFreshnessSnapshot::new_dirs`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedNewDirs {
	/// Every folder of `missing_dirs` a create needs, shallow to deep (the
	/// order [`NewDirs`] lists them in), each once.
	pub dirs: Vec<PathBuf>,
	/// For each create, by its index in the plan: the positions in `dirs`
	/// of the folders it needs, ascending. Empty for one that exists.
	pub needs: Vec<Vec<usize>>,
	/// For each create, by its index in the plan: the placed folder
	/// (`DestinationFreshnessSnapshot::placed`) its route goes through,
	/// as routing chose it, never as its path reads. Empty: not recorded
	/// (an older peer); then every placed folder counts as needed.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub routes: Vec<Option<usize>>,
}

impl PlannedNewDirs {
	/// Read off the paths of `creates` by this machine's path rules: a
	/// folder that exists has every ancestor existing, so what is missing
	/// is one run from the file upward.
	fn of(
		creates: &[CreateOperation],
		missing: &HashSet<PathBuf>,
		routes: Vec<Option<usize>>,
	) -> Self {
		let mut per_create: Vec<Vec<&Path>> = Vec::with_capacity(creates.len());
		let mut all: BTreeSet<(usize, &Path)> = BTreeSet::new();
		for op in creates {
			let mut chain = Vec::new();
			if !op.existed {
				let mut dir = op.absolute_path.parent();
				while let Some(d) = dir {
					if !missing.contains(d) {
						break;
					}
					all.insert((d.components().count(), d));
					chain.push(d);
					dir = d.parent();
				}
			}
			per_create.push(chain);
		}
		let position: HashMap<&Path, usize> =
			all.iter().enumerate().map(|(i, (_, d))| (*d, i)).collect();
		let needs = per_create
			.into_iter()
			.map(|chain| {
				let mut idx: Vec<usize> =
					chain.into_iter().map(|d| position[d]).collect();
				idx.sort_unstable();
				idx
			})
			.collect();
		Self {
			dirs: all.into_iter().map(|(_, d)| d.to_path_buf()).collect(),
			needs,
			routes,
		}
	}

	fn heap_bytes(&self) -> usize {
		let mut bytes = self.dirs.capacity() * std::mem::size_of::<PathBuf>()
			+ self.needs.capacity() * std::mem::size_of::<Vec<usize>>()
			+ self.routes.capacity() * std::mem::size_of::<Option<usize>>();
		for dir in &self.dirs {
			bytes = bytes.saturating_add(dir.capacity());
		}
		for need in &self.needs {
			bytes = bytes
				.saturating_add(need.capacity() * std::mem::size_of::<usize>());
		}
		bytes
	}
}

impl DestinationFreshnessSnapshot {
	/// Capacity estimate ONLY for fresh insertion-only capture tables below.
	/// Removal/tombstones can make HashMap::capacity understate its buckets;
	/// this is deliberately private, not an estimator for arbitrary public maps.
	fn fresh_capture_heap_bytes(&self) -> usize {
		let mut bytes = fresh_capture_table_bytes::<
			CanonicalRootId,
			RepoFreshness,
		>(self.roots.capacity())
		.saturating_add(fresh_capture_table_bytes::<
			PathBuf,
			TargetFileFreshness,
		>(self.target_files.capacity()));
		for (root, state) in &self.roots {
			bytes = bytes
				.saturating_add(root.0.capacity())
				.saturating_add(
					state.head_commit.as_ref().map_or(0, String::capacity),
				)
				.saturating_add(
					state.head_ref.as_ref().map_or(0, String::capacity),
				);
		}
		for (path, state) in &self.target_files {
			bytes = bytes
				.saturating_add(path.capacity())
				.saturating_add(state.root.0.capacity())
				.saturating_add(state.relative_path.capacity());
		}
		if let Some(repos) = &self.repos {
			bytes = bytes.saturating_add(fresh_capture_table_bytes::<
				CanonicalRootId,
				RepoFreshness,
			>(repos.capacity()));
			for (root, state) in repos {
				bytes = bytes
					.saturating_add(root.0.capacity())
					.saturating_add(
						state.head_commit.as_ref().map_or(0, String::capacity),
					)
					.saturating_add(
						state.head_ref.as_ref().map_or(0, String::capacity),
					);
			}
		}
		if let Some(dirs) = &self.missing_dirs {
			bytes = bytes
				.saturating_add(fresh_capture_table_bytes::<PathBuf, ()>(
					dirs.capacity(),
				));
			for dir in dirs {
				bytes = bytes.saturating_add(dir.capacity());
			}
		}
		if let Some(dirs) = &self.requested_dirs {
			bytes = bytes
				.saturating_add(fresh_capture_table_bytes::<PathBuf, ()>(
					dirs.capacity(),
				));
			for dir in dirs {
				bytes = bytes.saturating_add(dir.capacity());
			}
		}
		if let Some(placed) = &self.placed {
			bytes = bytes.saturating_add(
				placed.capacity() * std::mem::size_of::<Vec<PathBuf>>(),
			);
			for levels in placed {
				bytes = bytes.saturating_add(
					levels.capacity() * std::mem::size_of::<PathBuf>(),
				);
				for dir in levels {
					bytes = bytes.saturating_add(dir.capacity());
				}
			}
		}
		if let Some(new_dirs) = &self.new_dirs {
			bytes = bytes.saturating_add(new_dirs.heap_bytes());
		}
		if let Some(dirs) = &self.symlinked_dirs {
			bytes = bytes
				.saturating_add(fresh_capture_table_bytes::<PathBuf, PathBuf>(
					dirs.capacity(),
				));
			for (dir, target) in dirs {
				bytes = bytes
					.saturating_add(dir.capacity())
					.saturating_add(target.capacity());
			}
		}
		bytes
	}

	pub fn capture(
		destination_roots: &[PathBuf],
		plan: &RestorePlan,
	) -> Result<Self, TransferError> {
		Self::capture_with(destination_roots, plan, &RunOptions::default())
	}

	/// [`Self::capture`] with the caller's runner options.
	///
	/// Git open, HEAD, the symbolic ref, the index hash and each target hash
	/// use `opts`. The token is polled before every root and target. One
	/// freshness chunk is at most 8 KiB and cannot be interrupted mid-read.
	pub fn capture_with(
		destination_roots: &[PathBuf],
		plan: &RestorePlan,
		opts: &RunOptions,
	) -> Result<Self, TransferError> {
		cancelled_err(opts, "import-freshness")?;
		// Fresh tables, insertion only: preserve this creation invariant for
		// fresh_capture_heap_bytes. Do not reuse a previously edited snapshot.
		let mut roots = HashMap::new();
		for root in destination_roots {
			cancelled_err(opts, "import-freshness")?;
			let id = CanonicalRootId::new(root)?;
			id.validate()?;
			roots.insert(id, capture_repo_freshness(root, opts)?);
		}

		let mut target_files = HashMap::new();
		for op in &plan.create_operations {
			cancelled_err(opts, "import-freshness")?;
			let file_state = capture_file_freshness(
				&op.absolute_path,
				opts,
				TransferError::DestinationNotRegular,
			)?;
			target_files.insert(
				op.absolute_path.clone(),
				TargetFileFreshness {
					root: CanonicalRootId::new(&op.root_path)?,
					relative_path: op.relative_path.clone(),
					existed: op.existed,
					file_state,
				},
			);
		}

		for op in &plan.delete_operations {
			cancelled_err(opts, "import-freshness")?;
			let file_state = capture_file_freshness(
				&op.absolute_path,
				opts,
				TransferError::DestinationNotRegular,
			)?;
			let parent = op.absolute_path.parent().unwrap_or(&op.absolute_path);
			target_files.insert(
				op.absolute_path.clone(),
				TargetFileFreshness {
					root: CanonicalRootId::new(parent)?,
					relative_path: op.relative_path.clone(),
					existed: true,
					file_state,
				},
			);
		}

		let (missing_dirs, symlinked_dirs) =
			capture_folder_chain(plan, &roots, &target_files, opts)?;
		let repos = capture_destination_repos(&roots, &target_files, opts)?;

		cancelled_err(opts, "import-freshness")?;
		Ok(Self {
			roots,
			target_files,
			repos: Some(repos),
			missing_dirs: Some(missing_dirs),
			symlinked_dirs: Some(symlinked_dirs),
			requested_dirs: None,
			placed: None,
			new_dirs: None,
		})
	}

	/// Records repo HEAD/ref/index plus each target path's bytes or absence.
	/// Absence is a snapshot: a path that appears after preview is stale.
	///
	/// File reads use the cancellable freshness helpers with default runner
	/// options. Replay snapshots hash a symlink's own link text instead.
	pub fn capture_paths(
		destination_roots: &[PathBuf],
		targets: &[(PathBuf, String)],
	) -> Result<Self, TransferError> {
		let opts = RunOptions::default();
		let mut roots = HashMap::new();
		for root in destination_roots {
			let id = CanonicalRootId::new(root)?;
			id.validate()?;
			roots.insert(id, capture_repo_freshness(root, &opts)?);
		}

		let mut target_files = HashMap::new();
		for (abs, rel) in targets {
			let file_state = capture_file_freshness(
				abs,
				&opts,
				TransferError::DestinationNotRegular,
			)?;
			let existed = file_state.is_some();
			let root_path = destination_roots
				.iter()
				.find(|r| abs.starts_with(r))
				.cloned()
				.unwrap_or_else(|| abs.parent().unwrap_or(abs).to_path_buf());
			target_files.insert(
				abs.clone(),
				TargetFileFreshness {
					root: CanonicalRootId::new(&root_path)?,
					relative_path: rel.clone(),
					existed,
					file_state,
				},
			);
		}

		Ok(Self {
			roots,
			target_files,
			repos: None,
			missing_dirs: None,
			symlinked_dirs: None,
			requested_dirs: None,
			placed: None,
			new_dirs: None,
		})
	}

	/// Whether this snapshot, captured again on the worker, still describes the
	/// `preview` a master showed. A field the preview lacks is not compared: an
	/// older master drops fields it does not know, and the worker's own Apply
	/// still re-checks its fresh capture under the lock. `missing_dirs` is not
	/// compared: a planned folder that now exists as a folder is fine, and
	/// `revalidate` has already checked the preview's list against the disk.
	/// `symlinked_dirs` must be equal when the preview recorded it: a symlink
	/// that is here now but was not in the preview, that points elsewhere, or
	/// that the preview saw and is gone (replaced by a plain folder) is a
	/// change. A worker checks the old snapshot and then plans again, so a
	/// shrunken list would let the new plan, checked later against itself
	/// only, write where the preview did not show. `placed` must be
	/// equal when the preview recorded it (the agreement names its
	/// entries by place): the worker resolves the
	/// folders again, and a tail that came out different means a level of
	/// it appeared in between.
	pub fn matches_preview(&self, preview: &Self) -> bool {
		self.roots == preview.roots
			&& self.target_files == preview.target_files
			&& preview
				.repos
				.as_ref()
				.is_none_or(|r| self.repos.as_ref() == Some(r))
			&& preview
				.symlinked_dirs
				.as_ref()
				.is_none_or(|then| self.symlinked_dirs.as_ref() == Some(then))
			&& preview
				.requested_dirs
				.as_ref()
				.is_none_or(|then| self.requested_dirs.as_ref() == Some(then))
			&& preview
				.placed
				.as_ref()
				.is_none_or(|then| self.placed.as_ref() == Some(then))
	}

	/// Every folder a write is rooted in still resolves to the canonical
	/// path it had at preview: the destination root and each of its
	/// ancestors, a nested or prefix root, and the folder a delete was
	/// planned in. The folder chain below a root is compared separately;
	/// this is the part above it, which that chain never looks at. A root
	/// that was moved aside and replaced by a symlink keeps its path and
	/// its Git state, yet carries every write elsewhere.
	///
	/// A folder that cannot be resolved at all is not judged here: the
	/// checks of its repository and targets name what is gone.
	fn revalidate_root_identities(&self) -> Result<(), TransferError> {
		let mut seen = HashSet::new();
		let ids = self
			.roots
			.keys()
			.chain(self.target_files.values().map(|t| &t.root));
		for id in ids {
			if !seen.insert(id.path()) {
				continue;
			}
			if let Ok(now) = dunce::canonicalize(id.path()) {
				if now != id.path() {
					return Err(TransferError::StaleDestination {
						root: id.path().to_path_buf(),
						reason: format!(
							"destination folder '{}' now resolves to '{}'",
							id.path().display(),
							now.display()
						),
						change: StaleChange::Preview,
					});
				}
			}
		}
		Ok(())
	}

	pub fn revalidate(&self) -> Result<(), TransferError> {
		let opts = RunOptions::default();
		for (root, prev_repo) in &self.roots {
			revalidate_destination_repo_freshness(
				root.path(),
				prev_repo,
				&opts,
			)?;
		}

		// The repositories an Apply locks. The key sets are compared before
		// any of them is handed to git: a master's snapshot never decides
		// which folder git runs in.
		if let Some(prev) = &self.repos {
			let expected = destination_repo_set(
				self.target_files.keys().filter_map(|p| p.parent()),
				&opts,
			)?;
			let recorded: BTreeSet<&Path> =
				prev.keys().map(CanonicalRootId::path).collect();
			let wanted: BTreeSet<&Path> =
				expected.iter().map(PathBuf::as_path).collect();
			if let Some(differs) = recorded.symmetric_difference(&wanted).next()
			{
				return Err(TransferError::StaleDestination {
					root: differs.to_path_buf(),
					reason: "destination repositories changed after preview"
						.into(),
					change: StaleChange::Repository,
				});
			}
			for (root, prev_repo) in prev {
				if !self.roots.contains_key(root) {
					revalidate_destination_repo_freshness(
						root.path(),
						prev_repo,
						&opts,
					)?;
				}
			}
		}

		if self.missing_dirs.is_some()
			|| self.requested_dirs.is_some()
			|| self.placed.is_some()
		{
			// A folder the user named (a destination, an import path) is
			// strict: whatever is there now, it was not there when the
			// preview put this destination under an anchor, so the paths the
			// rows show are not the ones a write would reach. Every level of
			// a placed tail is such a folder, however planning classified it.
			let mut missing: Vec<&PathBuf> = self
				.missing_dirs
				.iter()
				.flatten()
				.chain(self.requested_dirs.iter().flatten())
				.chain(self.placed.iter().flatten().flatten())
				.collect();
			missing.sort();
			missing.dedup();
			for dir in missing {
				let requested = self
					.requested_dirs
					.as_ref()
					.is_some_and(|dirs| dirs.contains(dir))
					|| self
						.placed
						.iter()
						.flatten()
						.any(|levels| levels.contains(dir));
				match fs::symlink_metadata(dir) {
					Ok(meta) if meta.file_type().is_dir() && !requested => {}
					Ok(_) => {
						return Err(self.created_where_a_folder_was_needed(dir))
					}
					Err(e)
						if matches!(
							e.kind(),
							io::ErrorKind::NotFound
								| io::ErrorKind::NotADirectory
						) => {}
					Err(e) => return Err(TransferError::Io(e)),
				}
			}
		}

		// A created path needs a folder at its nearest existing ancestor.
		// And no folder on the way down to any planned target (one to create,
		// to overwrite or to delete) may be a symlink the preview did not see
		// or one that points elsewhere now: the write would land where the
		// preview did not show. This also guards a snapshot that carries no
		// `missing_dirs`.
		let mut checked = HashSet::new();
		let mut chain_ok: HashSet<(PathBuf, PathBuf)> = HashSet::new();
		for (path, target) in &self.target_files {
			let Some(parent) = path.parent() else {
				continue;
			};
			if !checked.insert((parent, target.existed, target.root.path())) {
				continue;
			}
			if !target.existed {
				let root = target.root.path();
				let mut found = false;
				for ancestor in
					parent.ancestors().take_while(|a| a.starts_with(root))
				{
					match fs::symlink_metadata(ancestor) {
						Ok(meta) => {
							let folder = meta.file_type().is_dir()
								|| (meta.file_type().is_symlink()
									&& fs::metadata(ancestor)
										.is_ok_and(|m| m.is_dir()));
							if !folder {
								return Err(
									created_where_a_folder_was_needed_at(
										root, ancestor,
									),
								);
							}
							found = true;
							break;
						}
						Err(e)
							if matches!(
								e.kind(),
								io::ErrorKind::NotFound
									| io::ErrorKind::NotADirectory
							) => {}
						Err(e) => return Err(TransferError::Io(e)),
					}
				}
				if !found {
					return Err(TransferError::Io(
						io::ErrorKind::NotFound.into(),
					));
				}
			}
			let Some(linked) = &self.symlinked_dirs else {
				continue;
			};
			let boundary = chain_boundary(path, target, &self.roots);
			for dir in parent
				.ancestors()
				.take_while(|a| *a != boundary && a.starts_with(&boundary))
			{
				// Verified up to this very boundary: the rest of the way
				// is verified too. Another boundary may reach further.
				if chain_ok.contains(&(dir.to_path_buf(), boundary.clone())) {
					break;
				}
				let now = match fs::symlink_metadata(dir) {
					Ok(meta) if meta.file_type().is_symlink() => {
						Some(dunce::canonicalize(dir).ok())
					}
					Ok(_) => None,
					Err(e)
						if matches!(
							e.kind(),
							io::ErrorKind::NotFound
								| io::ErrorKind::NotADirectory
						) =>
					{
						None
					}
					Err(e) => return Err(TransferError::Io(e)),
				};
				// Same symlink, same place it pointed at: fine. A symlink
				// that is new, or points elsewhere, or one that is gone, is
				// not what the preview showed.
				let same = match (&now, linked.get(dir)) {
					(Some(now), Some(then)) => now.as_ref() == Some(then),
					(None, None) => true,
					_ => false,
				};
				if !same {
					return Err(created_where_a_folder_was_needed_at(
						&boundary, dir,
					));
				}
				chain_ok.insert((dir.to_path_buf(), boundary.clone()));
			}
		}

		// After the chain, which names a folder that became a symlink the
		// way the user sees it (created externally); what is left is a root
		// that is no longer where the preview had it.
		self.revalidate_root_identities()?;

		for (path, target) in &self.target_files {
			let current_exists = path.exists();
			if !target.existed && current_exists {
				return Err(TransferError::StaleDestination {
					root: target.root.path().to_path_buf(),
					reason: format!(
						"target file '{}' was created externally after preview",
						target.relative_path
					),
					change: StaleChange::Created {
						rel: target.relative_path.to_string(),
					},
				});
			}
			if target.existed && !current_exists {
				return Err(TransferError::StaleDestination {
					root: target.root.path().to_path_buf(),
					reason: format!(
						"target file '{}' was deleted externally after preview",
						target.relative_path
					),
					change: StaleChange::Deleted {
						rel: target.relative_path.to_string(),
					},
				});
			}
			if target.existed && current_exists {
				let current_state = capture_file_freshness(
					path,
					&opts,
					TransferError::DestinationNotRegular,
				)?;
				if current_state != target.file_state {
					return Err(TransferError::StaleDestination {
						root: target.root.path().to_path_buf(),
						reason: format!(
							"target file '{}' was modified externally after preview",
							target.relative_path
						),
						change: StaleChange::Modified {
							rel: target.relative_path.to_string(),
						},
					});
				}
			}
		}

		Ok(())
	}

	/// The stale error for a folder `dir`, planned as missing at preview,
	/// that is now a file or a symlink.
	fn created_where_a_folder_was_needed(&self, dir: &Path) -> TransferError {
		let root = self
			.target_files
			.iter()
			.filter(|(path, _)| path.starts_with(dir))
			.min_by_key(|(path, _)| *path)
			.map(|(_, target)| target.root.path().to_path_buf())
			.or_else(|| dir.parent().map(Path::to_path_buf))
			.unwrap_or_else(|| dir.to_path_buf());
		created_where_a_folder_was_needed_at(&root, dir)
	}
}

/// `dir` (a folder the paste needs, below `root`) is a file or a symlink now.
fn created_where_a_folder_was_needed_at(
	root: &Path,
	dir: &Path,
) -> TransferError {
	let rel = slash_rel(root, dir);
	TransferError::StaleDestination {
		root: root.to_path_buf(),
		reason: format!(
			"'{rel}' was created externally after preview where the paste needs a folder"
		),
		change: StaleChange::Created { rel },
	}
}

/// `path` relative to `root`, always with `/`.
fn slash_rel(root: &Path, path: &Path) -> String {
	let rel = path.strip_prefix(root).unwrap_or(path);
	rel.components()
		.map(|c| c.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/")
}

/// The OS's own "not a directory" error: its text is the OS's words, never
/// ours, wherever a front end shows the detail.
fn not_a_directory() -> io::Error {
	#[cfg(unix)]
	{
		io::Error::from_raw_os_error(20) // ENOTDIR on Linux, macOS and the BSDs
	}
	#[cfg(windows)]
	{
		io::Error::from_raw_os_error(267) // ERROR_DIRECTORY
	}
}

/// Where the folder chain above a planned target stops being looked at.
///
/// It is read off the target's lexical path, never off the canonical folder a
/// delete records as its root: that folder is where the file really is, and a
/// delete reached through a symlink would have the chain end at the symlink's
/// destination, with the symlink itself left outside it. So the candidates
/// are the recorded root (when the path sits below it) and the deepest
/// destination root above the target's folder, and the shallower of the two
/// wins. A target that sits directly in a root (or whose folder is its own
/// recorded root, as a delete's is) goes up to the nearest destination root
/// above it, so the folder itself is looked at.
fn chain_boundary(
	path: &Path,
	target: &TargetFileFreshness,
	roots: &HashMap<CanonicalRootId, RepoFreshness>,
) -> PathBuf {
	let root = target.root.path();
	let Some(parent) = path.parent() else {
		return root.to_path_buf();
	};
	let above = roots
		.keys()
		.map(CanonicalRootId::path)
		.filter(|r| *r != parent && parent.starts_with(r))
		.max_by_key(|r| r.components().count());
	let recorded = parent.starts_with(root).then_some(root);
	[recorded, above]
		.into_iter()
		.flatten()
		.min_by_key(|r| r.components().count())
		.unwrap_or(root)
		.to_path_buf()
}

/// The folders on every planned target's path, below its root. For a create
/// of a new file: those that do not exist yet, and those that are symlinks to
/// a folder (with the canonical folder each points at). For a file that
/// exists (an overwrite or a delete): its symlinks only. A file or a symlink
/// that is not a folder where a folder is needed is refused here, the same on
/// every platform.
fn capture_folder_chain(
	plan: &RestorePlan,
	roots: &HashMap<CanonicalRootId, RepoFreshness>,
	target_files: &HashMap<PathBuf, TargetFileFreshness>,
	opts: &RunOptions,
) -> Result<(HashSet<PathBuf>, HashMap<PathBuf, PathBuf>), TransferError> {
	let mut missing = HashSet::new();
	let mut linked = HashMap::new();
	let mut seen = HashSet::new();
	let targets = plan
		.create_operations
		.iter()
		.map(|op| (&op.absolute_path, op.existed))
		.chain(
			plan.delete_operations
				.iter()
				.map(|op| (&op.absolute_path, true)),
		);
	for (path, existed) in targets {
		cancelled_err(opts, "import-freshness")?;
		let Some(target) = target_files.get(path) else {
			continue;
		};
		let boundary = chain_boundary(path, target, roots);
		let mut dir = path.parent();
		while let Some(d) = dir {
			// The root itself is not looked at: whether it is fit is checked
			// elsewhere.
			if d == boundary
				|| !d.starts_with(&boundary)
				|| !seen.insert((d.to_path_buf(), boundary.clone()))
			{
				break;
			}
			match fs::symlink_metadata(d) {
				Ok(meta) if meta.file_type().is_dir() => {}
				Ok(meta)
					if meta.file_type().is_symlink()
						&& fs::metadata(d).is_ok_and(|m| m.is_dir()) =>
				{
					linked.insert(
						d.to_path_buf(),
						dunce::canonicalize(d).map_err(TransferError::Io)?,
					);
				}
				// A file that exists is checked against its own record.
				Ok(_) if existed => {}
				Ok(_) => return Err(TransferError::Io(not_a_directory())),
				Err(e)
					if matches!(
						e.kind(),
						io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
					) =>
				{
					if !existed {
						missing.insert(d.to_path_buf());
					}
				}
				Err(e) => return Err(TransferError::Io(e)),
			}
			dir = d.parent();
		}
	}
	Ok((missing, linked))
}

/// The repositories an Apply of the snapshot's targets locks, with the state
/// each is in. A repository that is also a recorded root is not read twice.
fn capture_destination_repos(
	roots: &HashMap<CanonicalRootId, RepoFreshness>,
	target_files: &HashMap<PathBuf, TargetFileFreshness>,
	opts: &RunOptions,
) -> Result<HashMap<CanonicalRootId, RepoFreshness>, TransferError> {
	let candidates = destination_repo_set(
		target_files.keys().filter_map(|p| p.parent()),
		opts,
	)?;
	let mut repos = HashMap::new();
	for candidate in candidates {
		cancelled_err(opts, "import-freshness")?;
		let id = CanonicalRootId(candidate);
		let state = match roots.get(&id) {
			Some(state) => state.clone(),
			None => capture_repo_freshness(id.path(), opts)?,
		};
		repos.insert(id, state);
	}
	Ok(repos)
}

/// An immutable import plan containing the exact file restore operations and freshness token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferImportPlan {
	roots: Vec<PathBuf>,
	restore_plan: RestorePlan,
	destination_freshness: DestinationFreshnessSnapshot,
}

impl TransferImportPlan {
	/// Reconstitutes a worker's preview without reading its paths on this
	/// machine. The transport verifies its restore-plan digest before showing
	/// it; Apply still revalidates freshness (and a worker re-plans entirely).
	pub fn from_parts(
		roots: Vec<PathBuf>,
		restore_plan: RestorePlan,
		destination_freshness: DestinationFreshnessSnapshot,
	) -> Self {
		Self {
			roots,
			restore_plan,
			destination_freshness,
		}
	}

	/// Owned buffer capacities, excluding this inline struct and allocator
	/// bookkeeping. The private snapshot is captured into fresh insertion-only
	/// maps and is never mutated, as required by its table-size estimate.
	pub fn retained_heap_bytes(&self) -> usize {
		let mut bytes = self.roots.capacity() * std::mem::size_of::<PathBuf>();
		for root in &self.roots {
			bytes = bytes.saturating_add(root.capacity());
		}
		bytes
			.saturating_add(self.restore_plan.retained_heap_bytes())
			.saturating_add(
				self.destination_freshness.fresh_capture_heap_bytes(),
			)
	}

	pub fn roots(&self) -> &[PathBuf] {
		&self.roots
	}

	pub fn restore_plan(&self) -> &RestorePlan {
		&self.restore_plan
	}

	pub fn destination_freshness(&self) -> &DestinationFreshnessSnapshot {
		&self.destination_freshness
	}

	pub fn create_operations(&self) -> &[CreateOperation] {
		&self.restore_plan.create_operations
	}

	pub fn delete_operations(&self) -> &[DeleteOperation] {
		&self.restore_plan.delete_operations
	}

	pub fn skipped_operations(&self) -> &[SkippedOperation] {
		&self.restore_plan.skipped_operations
	}

	/// Every create's folders (what `--dry-run` lists).
	pub fn new_dirs(&self) -> NewDirs {
		self.new_dirs_for(&RestoreSelection::default())
	}

	/// The placed folders with a tail, by the place an agreement names them
	/// by (`RestoreSelection::confirmed_placed`): each one's levels,
	/// shallow to deep. Empty when every folder exists.
	pub fn placed_folders(&self) -> &[Vec<PathBuf>] {
		self.destination_freshness.placed.as_deref().unwrap_or(&[])
	}

	/// The planning machine's answer when it sent one: a worker's paths are
	/// read here only as written.
	fn planned_new_dirs(&self) -> std::borrow::Cow<'_, PlannedNewDirs> {
		let freshness = &self.destination_freshness;
		match &freshness.new_dirs {
			Some(planned) => std::borrow::Cow::Borrowed(planned),
			None => std::borrow::Cow::Owned(PlannedNewDirs::of(
				self.create_operations(),
				freshness.missing_dirs.as_ref().unwrap_or(&HashSet::new()),
				Vec::new(),
			)),
		}
	}

	/// Only the folders the creates `selection` keeps need: an unchecked row,
	/// and an existing file, need none.
	pub fn new_dirs_for(&self, selection: &RestoreSelection) -> NewDirs {
		let empty = HashSet::new();
		let freshness = &self.destination_freshness;
		let requested = freshness.requested_dirs.as_ref().unwrap_or(&empty);
		let levels: HashSet<&PathBuf> =
			self.placed_folders().iter().flatten().collect();
		let planned = self.planned_new_dirs();
		let mut kept: BTreeSet<usize> = BTreeSet::new();
		for (i, op) in self.create_operations().iter().enumerate() {
			if op.existed || selection.unchecked_creates.contains(&i) {
				continue;
			}
			if let Some(need) = planned.needs.get(i) {
				kept.extend(need.iter().copied());
			}
		}
		let mut out = NewDirs::default();
		for dir in kept.into_iter().filter_map(|i| planned.dirs.get(i)) {
			if requested.contains(dir) || levels.contains(dir) {
				out.requested.push(dir.clone());
			} else {
				out.incidental.push(dir.clone());
			}
		}
		out.placed = self
			.routed_placed(selection, &planned)
			.into_iter()
			.collect();
		out
	}

	/// The placed folders a create `selection` keeps is routed through:
	/// told by the route planning chose, never by how a path reads. A plan
	/// that does not say (an older peer, or a route out of range) needs
	/// every one, as soon as it keeps a new file.
	fn routed_placed(
		&self,
		selection: &RestoreSelection,
		planned: &PlannedNewDirs,
	) -> BTreeSet<usize> {
		let count = self.placed_folders().len();
		let mut routed = BTreeSet::new();
		if count == 0 {
			return routed;
		}
		for (i, op) in self.create_operations().iter().enumerate() {
			if op.existed || selection.unchecked_creates.contains(&i) {
				continue;
			}
			match planned.routes.get(i) {
				Some(None) => {}
				Some(Some(id)) if *id < count => {
					routed.insert(*id);
				}
				_ => return (0..count).collect(),
			}
		}
		routed
	}

	/// Applies the planned restore after verifying destination freshness.
	/// If any destination repo or file changed after preview, returns a stale error
	/// and writes nothing. Folders the user named that do not exist are made
	/// only when `selection.confirmed_placed` names every placed folder the
	/// rows kept are routed through; a file's own intermediate folders are
	/// made with it.
	pub fn apply(
		&self,
		selection: &RestoreSelection,
	) -> Result<RestoreExecutionResult, TransferError> {
		self.apply_observed(selection, &mut |_, _| {})
	}

	/// [`Self::apply`] observing every committed write as it lands: a
	/// remote worker's e2e paste-hold pauses after the first.
	///
	/// Which two names are one folder or one file is the filesystem's to
	/// say, here, under the write lock: nothing in this method compares
	/// names. Before any content is written, the folders the rows need are
	/// made and every new file is made empty with `create_new`. A new file
	/// that is already there is someone else's (stale) or one this Apply
	/// just made under another name (a collision); a placed folder not
	/// agreed to that exists by then was made through another name of it.
	/// Either refuses the whole paste and takes back what was made.
	pub fn apply_observed(
		&self,
		selection: &RestoreSelection,
		after_write: &mut dyn FnMut(
			&restore::CreateOperation,
			&restore::CreateOutcome,
		),
	) -> Result<RestoreExecutionResult, TransferError> {
		// Lock before revalidating: a check made before the lock leaves a
		// gap another paste could write into.
		let directories = self
			.kept_creates(selection)
			.map(|(_, op)| op.absolute_path.parent().unwrap())
			.chain(
				self.kept_deletes(selection)
					.map(|op| op.absolute_path.parent().unwrap()),
			);
		let _locks =
			lock_destination_repos(directories, &RunOptions::default())?;
		self.destination_freshness.revalidate()?;

		// Stale outranks a missing consent: a plan that changed is previewed
		// again, and the agreement to the old one is not carried over.
		let placed = self.placed_folders();
		let routed = self.routed_placed(selection, &self.planned_new_dirs());
		let unconfirmed: Vec<PathBuf> = routed
			.iter()
			.filter(|id| !selection.confirmed_placed.contains(id))
			.flat_map(|&id| placed[id].iter().cloned())
			.collect();
		if !unconfirmed.is_empty() {
			return Err(TransferError::NewDirsNotConfirmed {
				dirs: unconfirmed,
			});
		}

		// Made after the lock and the checks: its pin budget counts the
		// descriptors they hold open.
		let mut made = Made::default();
		let reserved = match self.reserve(selection, &routed, &mut made) {
			Ok(reserved) => reserved,
			Err(e) => {
				let left = made.take_back();
				return Err(TransferError::left_behind(e, left));
			}
		};
		let (mut result, unwritten) = restore::execute_restore_plan_reserved(
			&self.restore_plan,
			selection,
			&reserved,
			after_write,
		);
		// An empty file left behind is reported: a later Apply would take
		// it for an existing file and skip it.
		for i in unwritten {
			let op = &self.create_operations()[i];
			if let Err(e) = made.release_file(&op.absolute_path) {
				result.fail(
					&op.relative_path,
					&format!("left empty, could not be taken back: {e}"),
					io_reason(&e, &op.relative_path),
				);
			}
		}
		let wrote = result.created_count
			+ result.overwritten_count
			+ result.deleted_count
			> 0;
		let (kept, left) = made.finish(wrote);
		result.created_dirs = kept;
		for (dir, e) in left {
			let shown = dir.display().to_string();
			result.fail(
				&shown,
				&format!("folder made could not be taken back: {e}"),
				io_reason(&e, &shown),
			);
		}
		Ok(result)
	}

	/// The creates `selection` keeps that write: new files, and existing
	/// ones when it overwrites.
	fn kept_creates<'a>(
		&'a self,
		selection: &'a RestoreSelection,
	) -> impl Iterator<Item = (usize, &'a CreateOperation)> + 'a {
		self.create_operations()
			.iter()
			.enumerate()
			.filter(|(i, op)| {
				!selection.unchecked_creates.contains(i)
					&& (!op.existed
						|| (selection.overwrite_existing
							&& !selection.skip_existing))
			})
	}

	fn kept_deletes<'a>(
		&'a self,
		selection: &'a RestoreSelection,
	) -> impl Iterator<Item = &'a DeleteOperation> + 'a {
		self.delete_operations()
			.iter()
			.enumerate()
			.filter(|(i, _)| !selection.unchecked_deletes.contains(i))
			.map(|(_, op)| op)
	}

	/// Everything an Apply makes before it writes content, under the lock
	/// and after `revalidate`, recorded in `made` (the caller takes it back
	/// on an error):
	///
	/// 1. the placed folders the kept rows are routed through (`routed`,
	///    agreed to), level by level;
	/// 2. each kept new file, empty, with `create_new`, and the folders on
	///    its way level by level. One already there is a collision when
	///    it is an object this Apply made or a file it overwrites or
	///    deletes, and stale otherwise;
	/// 3. a placed folder not agreed to must not exist by now.
	///
	/// Two existing files the rows write or delete must be two objects. A
	/// new file that cannot be made fails its row, as its write would.
	fn reserve(
		&self,
		selection: &RestoreSelection,
		routed: &BTreeSet<usize>,
		made: &mut Made,
	) -> Result<HashMap<usize, restore::Reservation>, TransferError> {
		let placed = self.placed_folders();
		for &id in routed {
			for dir in &placed[id] {
				self.make_placed_level(dir, made)?;
			}
		}

		// The files that are there: each one is one object, written or
		// deleted once (two cases of one name, two hard links).
		let mut existing: HashMap<DirKey, String> = HashMap::new();
		let rows =
			self.kept_creates(selection)
				.filter(|(_, op)| op.existed)
				.map(|(_, op)| ("create", &op.absolute_path, &op.relative_path))
				.chain(self.kept_deletes(selection).map(|op| {
					("delete", &op.absolute_path, &op.relative_path)
				}));
		for (what, path, rel) in rows {
			if fs::metadata(path).is_err() {
				continue;
			}
			let current = format!("{what} {rel}");
			if let Some(prev) =
				existing.insert(DirKey::of(path), current.clone())
			{
				return Err(TransferError::TargetCollision {
					path: path.clone(),
					msg: format!(
						"previous was '{prev}', current is '{current}'"
					),
				});
			}
		}

		let mut reserved = HashMap::new();
		let roots = &self.restore_plan.roots;
		for (i, op) in self.kept_creates(selection) {
			if op.existed || restore::create_is_refused(roots, op) {
				continue;
			}
			match self.make_new_file(op, made, &existing) {
				Ok(made_file) => {
					reserved.insert(i, restore::Reservation::Made(made_file));
				}
				Err(Reserve::Row(e)) => {
					reserved.insert(i, restore::Reservation::Failed(e));
				}
				Err(Reserve::Batch(e)) => return Err(e),
			}
		}

		// Whatever name reached it, a placed folder not agreed to that is
		// there now was made by this Apply's folders (or by someone else
		// since the lock): not what the user agreed to.
		for (id, levels) in placed.iter().enumerate() {
			if selection.confirmed_placed.contains(&id) {
				continue;
			}
			for dir in levels {
				match fs::symlink_metadata(dir) {
					Ok(_) if made.is_placed(&DirKey::of(dir)) => {}
					Ok(_) => {
						return Err(TransferError::NewDirsNotConfirmed {
							dirs: levels.clone(),
						})
					}
					Err(e)
						if matches!(
							e.kind(),
							io::ErrorKind::NotFound
								| io::ErrorKind::NotADirectory
						) =>
					{
						break
					}
					Err(e) => return Err(TransferError::Io(e)),
				}
			}
		}
		Ok(reserved)
	}

	/// Makes one level of a placed folder. One already there is this
	/// Apply's own only when it is an object this Apply made (another
	/// placed folder, another name of it); anything else is stale. What is
	/// there right after is checked as the object made, never as a
	/// spelling: HFS+ stores a composed name decomposed, which replaces
	/// nothing.
	fn make_placed_level(
		&self,
		dir: &Path,
		made: &mut Made,
	) -> Result<(), TransferError> {
		// The planner refuses such a folder first (`PlacedRoot::resolve`,
		// test P10); this is the guard for one that came to lie in a Git
		// directory since. A repository that appears under the anchor after
		// the preview is stopped even earlier, by the lock.
		if paths::lands_in_git_dir(dir) {
			return Err(TransferError::UnsafePath(dir.display().to_string()));
		}
		new_dir_hook(NewDirStage::BeforeCreate, dir)?;
		match fs::create_dir(dir) {
			Ok(()) => {
				let key = made.dir(dir, true);
				new_dir_hook(NewDirStage::AfterCreate, dir)?;
				// Someone put a symlink or another folder where the folder
				// was just made: the rows would land somewhere the preview
				// did not show.
				let same = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
					&& DirKey::of(dir) == key;
				if !same {
					return Err(TransferError::StaleDestination {
						root: dir.parent().unwrap_or(dir).to_path_buf(),
						reason: format!(
							"folder '{}' was replaced right after it was made",
							dir.display()
						),
						change: StaleChange::Preview,
					});
				}
				Ok(())
			}
			Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
				let ours = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
					&& made.has(&DirKey::of(dir));
				if ours {
					Ok(())
				} else {
					Err(self
						.destination_freshness
						.created_where_a_folder_was_needed(dir))
				}
			}
			Err(e) => Err(TransferError::Io(e)),
		}
	}

	/// Makes `op`'s file empty with `create_new`, and the folders on its
	/// way that are missing, level by level. Returns the file as made, for
	/// its row to write into only while it is still that file untouched.
	fn make_new_file(
		&self,
		op: &CreateOperation,
		made: &mut Made,
		existing: &HashMap<DirKey, String>,
	) -> Result<restore::Reserved, Reserve> {
		let path = &op.absolute_path;
		let mut missing: Vec<&Path> = Vec::new();
		let mut cur = path.parent();
		while let Some(dir) = cur {
			match fs::symlink_metadata(dir) {
				Ok(_) => {
					if !fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
						return Err(Reserve::Batch(
							self.met(op, dir, made, existing),
						));
					}
					break;
				}
				Err(e)
					if matches!(
						e.kind(),
						io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
					) =>
				{
					missing.push(dir);
					cur = dir.parent();
				}
				Err(e) => return Err(Reserve::Row(e)),
			}
		}
		for dir in missing.into_iter().rev() {
			match fs::create_dir(dir) {
				Ok(()) => {
					made.dir(dir, false);
				}
				Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
					// A folder by now (made under another name of it, or
					// one a path implies that someone made): the file is
					// still checked on its own below.
					if !fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
						return Err(Reserve::Batch(
							self.met(op, dir, made, existing),
						));
					}
				}
				Err(e) => return Err(Reserve::Row(e)),
			}
		}
		if let Err(e) = restore::fail_point("reserve", path) {
			return Err(Reserve::Batch(TransferError::Io(e)));
		}
		let mut options = fs::OpenOptions::new();
		options.write(true).create_new(true);
		match options.open(path) {
			Ok(file) => {
				// The object made, read from its handle: what is at the
				// name a moment later may be someone else's.
				let key =
					DirKey::of_file(&file).unwrap_or_else(|| DirKey::of(path));
				// Read by name, as the row will read it.
				let modified = fs::symlink_metadata(path)
					.and_then(|info| info.modified())
					.ok();
				let reserved = made.reserved(file, key, modified);
				made.file(path, &reserved, &op.relative_path);
				Ok(reserved)
			}
			Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
				Err(Reserve::Batch(self.met(op, path, made, existing)))
			}
			// Windows refuses to make a file over a folder (or a file being
			// deleted) as access denied: something is there all the same.
			Err(e)
				if cfg!(windows)
					&& e.kind() == io::ErrorKind::PermissionDenied
					&& fs::symlink_metadata(path).is_ok() =>
			{
				Err(Reserve::Batch(self.met(op, path, made, existing)))
			}
			Err(e) => Err(Reserve::Row(e)),
		}
	}

	/// `op` met something at `at` (its file, or a folder on its way): one
	/// of this Apply's own objects or a file it writes is the same target
	/// twice; anything else appeared after the preview.
	fn met(
		&self,
		op: &CreateOperation,
		at: &Path,
		made: &Made,
		existing: &HashMap<DirKey, String>,
	) -> TransferError {
		let key = DirKey::of(at);
		let current = format!("create {}", op.relative_path);
		if let Some(prev) = made.what(&key).or_else(|| existing.get(&key)) {
			return TransferError::TargetCollision {
				path: op.absolute_path.clone(),
				msg: format!("previous was '{prev}', current is '{current}'"),
			};
		}
		if at == op.absolute_path {
			TransferError::StaleDestination {
				root: op.root_path.clone(),
				reason: format!(
					"target file '{}' was created externally after preview",
					op.relative_path
				),
				change: StaleChange::Created {
					rel: op.relative_path.clone(),
				},
			}
		} else {
			created_where_a_folder_was_needed_at(&op.root_path, at)
		}
	}
}

/// How making one new file failed.
enum Reserve {
	/// Only this row: it fails, as its write would have.
	Row(io::Error),
	/// The whole paste.
	Batch(TransferError),
}

/// What an Apply made before writing content, each with the object it is,
/// so that only what is still that object is taken back.
///
/// An object is told by its number ([`DirKey`]), and a number says which
/// object only while that object lives: Linux filesystems (ext4, tmpfs)
/// give a freed inode number to the next object made, so a folder removed
/// and made again at once, by someone else, is the same `DirKey`, and its
/// timestamps tick too coarsely (about 4 ms) to tell them apart. On Linux
/// each object made is therefore pinned for the rest of the Apply: a handle
/// held open keeps its inode, and its number, from being freed, so another
/// object can never carry it. The pin is `O_PATH` (or the handle a file was
/// made with) and blocks nothing: the object can still be written, renamed
/// or removed. macOS and Windows hand a number out again only much later
/// (APFS object ids grow; an NTFS file index carries a sequence number),
/// and Windows cannot replace an open file, so nothing is held there.
///
/// Pins are descriptors, and the Apply's own rows and take-backs need
/// descriptors too: how many the process holds is a ceiling read from its
/// limit and what else is open ([`pin_ceiling`]), shared by every Apply
/// running in it; past it, or with no room at all, an object is told by
/// its number alone, as elsewhere. A pinned file is checked through its
/// pin, with no descriptor of its own.
struct Made {
	/// Folders in the order made; `true` for a placed folder's level.
	dirs: Vec<(PathBuf, DirKey, bool)>,
	/// Empty files made with `create_new`, as made (with their pins).
	files: Vec<(PathBuf, restore::Reserved)>,
	/// Every object made, with what made it.
	objects: HashMap<DirKey, String>,
	/// The handles that keep the folders made from being freed (Linux).
	#[cfg(target_os = "linux")]
	dir_pins: Vec<fs::File>,
	/// How many objects the process may hold pinned, as read when this
	/// Apply began, and how many this Apply pinned.
	#[cfg(target_os = "linux")]
	ceiling: usize,
	#[cfg(target_os = "linux")]
	pinned: usize,
}

/// At most this many objects are pinned in the whole process at once: a
/// quarter of the usual soft limit on open files (1024).
#[cfg(target_os = "linux")]
const PIN_CAP: usize = 256;

/// Descriptors kept free for everything else an Apply opens while its pins
/// are held: one per row at a time (its write handle, or the temporary
/// file of a replace), one to check an unpinned file, git's pipes, the
/// `/proc` listing, and whatever else the process does meanwhile. Its
/// repository locks are open already, and counted as used.
#[cfg(target_os = "linux")]
const PIN_RESERVE: usize = 64;

/// The objects pinned by every Apply in this process now: two Applies in
/// one process (two tabs pasting into two repositories) share its limit.
#[cfg(target_os = "linux")]
static PINNED: std::sync::atomic::AtomicUsize =
	std::sync::atomic::AtomicUsize::new(0);

/// How many objects the whole process may hold pinned, read now: half of
/// what the soft limit leaves past [`PIN_RESERVE`] and the descriptors
/// open other than pins, within [`PIN_CAP`]. It is a ceiling on the pins
/// of every Apply together, never an allowance each Apply takes for
/// itself: two Applies each given the room left would hold twice that.
/// When what is open cannot be counted, half the limit is taken as open.
#[cfg(target_os = "linux")]
fn pin_ceiling() -> usize {
	use nix::sys::resource::{getrlimit, Resource};
	use std::sync::atomic::Ordering;
	let Ok((soft, _)) = getrlimit(Resource::RLIMIT_NOFILE) else {
		return 0;
	};
	let soft = usize::try_from(soft).unwrap_or(usize::MAX);
	let pinned = PINNED.load(Ordering::Relaxed);
	let others = fs::read_dir("/proc/self/fd")
		.map(|fds| fds.count().saturating_sub(pinned))
		.unwrap_or(soft / 2);
	let room = soft.saturating_sub(others).saturating_sub(PIN_RESERVE) / 2;
	room.min(PIN_CAP)
}

// Derivable only where nothing is pinned.
#[cfg_attr(not(target_os = "linux"), allow(clippy::derivable_impls))]
impl Default for Made {
	fn default() -> Self {
		Self {
			dirs: Vec::new(),
			files: Vec::new(),
			objects: HashMap::new(),
			#[cfg(target_os = "linux")]
			dir_pins: Vec::new(),
			#[cfg(target_os = "linux")]
			ceiling: pin_ceiling(),
			#[cfg(target_os = "linux")]
			pinned: 0,
		}
	}
}

#[cfg(target_os = "linux")]
impl Drop for Made {
	fn drop(&mut self) {
		use std::sync::atomic::Ordering;
		PINNED.fetch_sub(self.pinned, Ordering::Relaxed);
	}
}

/// Something an Apply made that it meant to take back and could not: the
/// path, and why. An empty file left this way is still there, and a
/// later Apply would take it for one that exists.
pub(crate) type NotTakenBack = (PathBuf, io::Error);

impl Made {
	/// Records the folder just made at `path` and returns which object it
	/// is: read by following `path`, as every later check reads it.
	fn dir(&mut self, path: &Path, placed: bool) -> DirKey {
		let key = DirKey::of(path);
		self.pin_dir(path, &key);
		self.objects
			.entry(key.clone())
			.or_insert_with(|| format!("folder {}", path.display()));
		self.dirs.push((path.to_path_buf(), key.clone(), placed));
		key
	}

	/// Takes one pin, if the process holds fewer than the ceiling: counted
	/// and checked in one step, so Applies racing for the last ones never
	/// take more than there are.
	#[cfg(target_os = "linux")]
	fn take_pin(&mut self) -> bool {
		use std::sync::atomic::Ordering;
		let mut now = PINNED.load(Ordering::Acquire);
		while now < self.ceiling {
			match PINNED.compare_exchange_weak(
				now,
				now + 1,
				Ordering::AcqRel,
				Ordering::Acquire,
			) {
				Ok(_) => {
					self.pinned += 1;
					return true;
				}
				Err(seen) => now = seen,
			}
		}
		false
	}

	/// Pins the folder at `path` if it is still `key` (see [`Made`]).
	#[cfg(target_os = "linux")]
	fn pin_dir(&mut self, path: &Path, key: &DirKey) {
		use std::os::unix::fs::OpenOptionsExt;
		if PINNED.load(std::sync::atomic::Ordering::Relaxed) >= self.ceiling {
			return;
		}
		// The access mode is ignored with `O_PATH`: no permission on the
		// folder is needed, so a folder umask left unreadable is pinned too.
		let handle = fs::OpenOptions::new()
			.read(true)
			.custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
			.open(path);
		if let Ok(handle) = handle {
			if DirKey::of_file(&handle).as_ref() == Some(key) && self.take_pin()
			{
				self.dir_pins.push(handle);
			}
		}
	}

	#[cfg(not(target_os = "linux"))]
	fn pin_dir(&mut self, _path: &Path, _key: &DirKey) {}

	/// The file just made through `handle`, as `key`, last modified at
	/// `modified`: its handle kept as its pin on Linux within the ceiling
	/// (see [`Made`]), closed otherwise. One held per new file past it
	/// would run out of descriptors on a large paste, and Windows cannot
	/// replace an open file.
	fn reserved(
		&mut self,
		handle: fs::File,
		key: DirKey,
		modified: Option<std::time::SystemTime>,
	) -> restore::Reserved {
		#[allow(unused_mut)]
		let mut reserved = restore::Reserved::new(key, modified);
		#[cfg(target_os = "linux")]
		if self.take_pin() {
			reserved.pin = Some(std::sync::Arc::new(handle));
			return reserved;
		}
		drop(handle);
		reserved
	}

	fn file(&mut self, path: &Path, reserved: &restore::Reserved, rel: &str) {
		self.objects
			.entry(reserved.key.clone())
			.or_insert_with(|| format!("create {rel}"));
		self.files.push((path.to_path_buf(), reserved.clone()));
	}

	fn has(&self, key: &DirKey) -> bool {
		self.objects.contains_key(key)
	}

	fn what(&self, key: &DirKey) -> Option<&String> {
		self.objects.get(key)
	}

	/// A level of a placed folder this Apply made (agreed to).
	fn is_placed(&self, key: &DirKey) -> bool {
		self.dirs.iter().any(|(_, k, placed)| *placed && k == key)
	}

	/// Removes the empty file made at `path`, if it is still that file
	/// untouched (`Reserved::is_untouched`, the test its row wrote by, asked
	/// without any permission on the file's content, and through its pin
	/// when it has one). On macOS and other Unix without `O_PATH` the check
	/// is an `lstat`, so a residual, very small window remains between it
	/// and the unlink.
	///
	/// An error is what kept it from telling or from removing: the file
	/// may be left there empty, and the caller reports it. One that is
	/// someone else's by now, or gone, is no error.
	fn release_file(&self, path: &Path) -> io::Result<()> {
		let Some((_, reserved)) = self.files.iter().find(|(p, _)| p == path)
		else {
			return Ok(());
		};
		restore::fail_point("release", path)?;
		if !reserved.is_untouched(path)? {
			return Ok(());
		}
		match fs::remove_file(path) {
			Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
			_ => Ok(()),
		}
	}

	/// Removes, deepest first, the folders made that are still those
	/// folders and empty. A folder with something in it, or gone, stays
	/// as it is; one that could not be told or removed is returned.
	fn take_back_dirs(&self) -> Vec<NotTakenBack> {
		let mut left = Vec::new();
		for (path, key, _) in self.dirs.iter().rev() {
			match fs::symlink_metadata(path) {
				Ok(info) if info.is_dir() && DirKey::of(path) == *key => {}
				Ok(_) => continue,
				Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
				Err(e) => {
					left.push((path.clone(), e));
					continue;
				}
			}
			match fs::remove_dir(path) {
				Ok(()) => {}
				Err(e)
					if matches!(
						e.kind(),
						io::ErrorKind::NotFound
							| io::ErrorKind::DirectoryNotEmpty
					) => {}
				Err(e) => left.push((path.clone(), e)),
			}
		}
		left
	}

	/// Takes back everything made: no content was written. Returns what
	/// it could not.
	fn take_back(&self) -> Vec<NotTakenBack> {
		let mut left: Vec<NotTakenBack> = self
			.files
			.iter()
			.filter_map(|(path, _)| {
				self.release_file(path).err().map(|e| (path.clone(), e))
			})
			.collect();
		left.extend(self.take_back_dirs());
		left
	}

	/// After the rows ran: a paste that wrote and deleted nothing leaves no
	/// folder it made behind (one with something in it stays). Returns how
	/// many placed folders' levels it made are still there, and the
	/// folders it meant to take back and could not.
	fn finish(&self, wrote: bool) -> (usize, Vec<NotTakenBack>) {
		let left = if wrote {
			Vec::new()
		} else {
			self.take_back_dirs()
		};
		let kept = self
			.dirs
			.iter()
			.filter(|(path, _, placed)| {
				*placed && fs::symlink_metadata(path).is_ok()
			})
			.count();
		(kept, left)
	}
}

/// The folders an Apply would make, shallow to deep, each once: what a
/// front end lists. `requested` are the ones the user named (a placed
/// folder's levels, and what planning took for one of them under another
/// name), `incidental` the ones a file's path implies. What the user agrees
/// to is `placed`; the lists only show it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewDirs {
	pub requested: Vec<PathBuf>,
	pub incidental: Vec<PathBuf>,
	/// The placed folders the rows kept are routed through, by their place
	/// in [`TransferImportPlan::placed_folders`]: what the agreement must
	/// name (`RestoreSelection::confirmed_placed`).
	pub placed: Vec<usize>,
}

/// Where `make_placed_level` is in a folder: a test injects a failure or
/// an interloper here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewDirStage {
	BeforeCreate,
	AfterCreate,
}

#[cfg(test)]
type NewDirHook = Box<dyn Fn(NewDirStage, &Path) -> io::Result<()>>;

#[cfg(test)]
thread_local! {
	static NEW_DIR_HOOK: std::cell::RefCell<Option<NewDirHook>> =
		const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn new_dir_hook(stage: NewDirStage, dir: &Path) -> Result<(), TransferError> {
	NEW_DIR_HOOK
		.with(|slot| match slot.borrow().as_ref() {
			Some(hook) => hook(stage, dir),
			None => Ok(()),
		})
		.map_err(TransferError::Io)
}

#[cfg(not(test))]
fn new_dir_hook(_stage: NewDirStage, _dir: &Path) -> Result<(), TransferError> {
	Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The folders whose repositories an Apply writing into `directories`
/// locks: each directory's nearest existing folder, then its nearest `.git`
/// ancestor. The write lock and the freshness snapshot both use this, so the
/// snapshot always covers what the lock takes.
fn destination_repo_set<'a>(
	directories: impl IntoIterator<Item = &'a Path>,
	opts: &RunOptions,
) -> Result<BTreeSet<PathBuf>, TransferError> {
	let mut existing_cache = HashMap::new();
	let mut repo_cache = HashMap::new();
	let mut candidates = BTreeSet::new();
	for directory in directories {
		cancelled_err(opts, "paste-lock")?;
		let existing =
			existing_destination_directory(directory, &mut existing_cache)?;
		if let Some(root) =
			destination_repo_candidate(&existing, &mut repo_cache)?
		{
			candidates.insert(root);
		}
	}
	Ok(candidates)
}

/// Lock every repository containing a destination directory, all or none.
/// Discover candidate roots through the filesystem first, caching ancestor
/// lookups; Git processes scale with repositories, not existing directories.
/// Resolve aliases before discovering roots and deduplicate canonical git
/// dirs before locking, including worktrees/submodules with a `.git` file.
fn lock_destination_repos<'a>(
	directories: impl IntoIterator<Item = &'a Path>,
	opts: &RunOptions,
) -> Result<Vec<HeavyGuard>, TransferError> {
	let candidates = destination_repo_set(directories, opts)?;
	let mut repos = Vec::with_capacity(candidates.len());
	for root in candidates {
		let git = match Git::open_with(&root, opts) {
			Ok(g) => g,
			Err(GitError::NotARepository(_)) => continue,
			Err(e) => return Err(e.into()),
		};
		repos.push(RepoIdentity::resolve(&git, opts)?);
	}
	repos.sort_by(|a, b| a.git_dir.cmp(&b.git_dir));
	repos.dedup_by(|a, b| a.git_dir == b.git_dir);
	let mut held = Vec::with_capacity(repos.len());
	for repo in &repos {
		held.push(lock_heavy(repo)?);
	}
	Ok(held)
}

/// New folders (or files a replay will replace with folders) resolve to the
/// nearest existing directory. Canonicalize before walking repo ancestors:
/// lexical ancestors of a symlink alias may belong to a different repo.
fn existing_destination_directory(
	directory: &Path,
	cache: &mut HashMap<PathBuf, PathBuf>,
) -> io::Result<PathBuf> {
	let mut visited = Vec::new();
	let mut existing = None;
	for ancestor in directory.ancestors() {
		if let Some(found) = cache.get(ancestor) {
			existing = Some(found.clone());
			break;
		}
		visited.push(ancestor.to_path_buf());
		match fs::metadata(ancestor) {
			Ok(meta) if meta.is_dir() => {
				existing = Some(dunce::canonicalize(ancestor)?);
				break;
			}
			Ok(_) => continue,
			Err(e)
				if matches!(
					e.kind(),
					io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
				) =>
			{
				continue
			}
			Err(e) => return Err(e),
		}
	}
	let existing = existing.ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::NotFound,
			"no existing destination directory",
		)
	})?;
	for path in visited {
		cache.insert(path, existing.clone());
	}
	Ok(existing)
}

/// Cache both repo and non-repo ancestors, stopping at the nearest `.git`
/// item. Git validates the candidate, including gitdir files and symlinks;
/// an unreadable marker must fail rather than silently select an outer repo.
fn destination_repo_candidate(
	directory: &Path,
	cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> io::Result<Option<PathBuf>> {
	let mut visited = Vec::new();
	let mut candidate = None;
	for ancestor in directory.ancestors() {
		if let Some(found) = cache.get(ancestor) {
			candidate = found.clone();
			break;
		}
		visited.push(ancestor.to_path_buf());
		match fs::symlink_metadata(ancestor.join(".git")) {
			Ok(_) => {
				candidate = Some(ancestor.to_path_buf());
				break;
			}
			Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
			Err(e) => return Err(e),
		}
	}
	for path in visited {
		cache.insert(path, candidate.clone());
	}
	Ok(candidate)
}

/// Conservative requested table bytes for std's current SwissTable, for
/// insertion-only captures. A fresh table has at least half its buckets in
/// capacity (small tables leave one free; large tables leave one eighth).
/// Two buckets per capacity slot therefore overestimates storage. Include a
/// control byte per bucket, a trailing control group and alignment padding.
/// The supported std targets use groups at most 16 bytes wide; 64 is a
/// conservative allowance. This is not allocator usable-size/RSS accounting
/// and MUST be revisited if std changes its HashMap layout.
fn fresh_capture_table_bytes<K, V>(capacity: usize) -> usize {
	if capacity == 0 {
		return 0;
	}
	let alignment = std::mem::align_of::<(K, V)>().max(64);
	capacity
		.saturating_mul(2)
		.saturating_mul(std::mem::size_of::<(K, V)>().saturating_add(1))
		.saturating_add(64)
		.saturating_add(alignment - 1)
}

fn not_found_as_none<T>(
	res: io::Result<T>,
) -> Result<Option<T>, TransferError> {
	match res {
		Ok(v) => Ok(Some(v)),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(TransferError::Io(e)),
	}
}

/// Longest regular-file read that cannot observe cancellation. The kernel
/// does not interrupt an in-progress `read`; callers poll between chunks.
const FILE_IO_CHUNK: usize = 8 * 1024;

fn cancelled_err(opts: &RunOptions, args: &str) -> Result<(), TransferError> {
	if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
		Err(GitError::Cancelled {
			args: args.to_string(),
		}
		.into())
	} else {
		Ok(())
	}
}

/// Preview truncation is not metadata. A short `RunOptions` must surface
/// [`GitError::OutputLimit`] instead of a parsed prefix.
fn strict_git_stdout<'a>(
	out: &'a RunOutput,
	opts: &RunOptions,
	args: &str,
) -> Result<&'a [u8], TransferError> {
	if out.truncated {
		return Err(GitError::OutputLimit {
			args: args.to_string(),
			limit: opts.max_stdout,
		}
		.into());
	}
	Ok(out.stdout.as_slice())
}

/// Rejects anything that is not a regular file, or a symlink to one, before
/// [`File::open`](fs::File::open). `stat` does not block on a FIFO; `open` does.
fn ensure_regular_file(path: &Path) -> Result<(), TransferError> {
	let listed = fs::symlink_metadata(path)?;
	let target = if listed.file_type().is_symlink() {
		fs::metadata(path)?
	} else {
		listed
	};
	if target.file_type().is_file() {
		Ok(())
	} else {
		Err(TransferError::SpecialFile(
			path.to_string_lossy().into_owned(),
		))
	}
}

/// Reads at most `max_bytes` (`None` means the whole stream). Each `read`
/// slice is only the remaining cap, so a one-byte overflow probe never pulls
/// a full chunk it will discard. The token is checked before every read and
/// after the last one.
fn read_limited<R: Read>(
	reader: &mut R,
	max_bytes: Option<u64>,
	opts: &RunOptions,
	label: &str,
	mut on_chunk: impl FnMut(&[u8]),
) -> Result<u64, TransferError> {
	let mut buf = [0u8; FILE_IO_CHUNK];
	let mut total = 0u64;
	loop {
		cancelled_err(opts, label)?;
		let want = match max_bytes {
			Some(max) if total >= max => break,
			Some(max) => (max - total) as usize,
			None => FILE_IO_CHUNK,
		};
		let want = want.min(FILE_IO_CHUNK);
		if want == 0 {
			break;
		}
		let n = reader.read(&mut buf[..want])?;
		if n == 0 {
			break;
		}
		on_chunk(&buf[..n]);
		total += n as u64;
	}
	cancelled_err(opts, label)?;
	Ok(total)
}

/// Opens `path` only after [`ensure_regular_file`], then uses [`read_limited`].
fn for_each_chunk(
	path: &Path,
	max_bytes: Option<u64>,
	opts: &RunOptions,
	label: &str,
	on_chunk: impl FnMut(&[u8]),
) -> Result<u64, TransferError> {
	cancelled_err(opts, label)?;
	ensure_regular_file(path)?;
	let mut file = fs::File::open(path)?;
	cancelled_err(opts, label)?;
	read_limited(&mut file, max_bytes, opts, label, on_chunk)
}

fn hash_file(
	path: &Path,
	opts: &RunOptions,
) -> Result<[u8; 32], TransferError> {
	let mut hasher = Sha256::new();
	for_each_chunk(path, None, opts, "hash-file", |chunk| {
		#[cfg(test)]
		note_hash_chunk(path, chunk);
		hasher.update(chunk);
	})?;
	Ok(hasher.finalize().into())
}

/// Test-only record of one `hash_file` chunk. Production builds omit it.
#[cfg(test)]
struct HashChunkNote {
	path: PathBuf,
	len: usize,
	head: [u8; 4],
}

#[cfg(test)]
struct HashProbe {
	arm: PathBuf,
	cancel: CancelToken,
	notes: Vec<HashChunkNote>,
}

#[cfg(test)]
thread_local! {
	static HASH_PROBE: std::cell::RefCell<Option<HashProbe>> =
		const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct HashProbeGuard;

#[cfg(test)]
impl Drop for HashProbeGuard {
	fn drop(&mut self) {
		HASH_PROBE.with(|slot| *slot.borrow_mut() = None);
	}
}

/// Arms a thread-local probe. The first chunk of `arm` cancels `cancel`.
/// Other paths, including the Git index, are recorded and left running.
#[cfg(test)]
fn arm_hash_probe(arm: PathBuf, cancel: CancelToken) -> HashProbeGuard {
	HASH_PROBE.with(|slot| {
		*slot.borrow_mut() = Some(HashProbe {
			arm,
			cancel,
			notes: Vec::new(),
		});
	});
	HashProbeGuard
}

#[cfg(test)]
fn hash_probe_notes() -> Vec<HashChunkNote> {
	HASH_PROBE.with(|slot| {
		slot.borrow()
			.as_ref()
			.map(|probe| {
				probe
					.notes
					.iter()
					.map(|note| HashChunkNote {
						path: note.path.clone(),
						len: note.len,
						head: note.head,
					})
					.collect()
			})
			.unwrap_or_default()
	})
}

#[cfg(test)]
fn note_hash_chunk(path: &Path, chunk: &[u8]) {
	HASH_PROBE.with(|slot| {
		let mut guard = slot.borrow_mut();
		let Some(probe) = guard.as_mut() else {
			return;
		};
		let mut head = [0u8; 4];
		let n = chunk.len().min(4);
		head[..n].copy_from_slice(&chunk[..n]);
		let is_arm = path == probe.arm;
		probe.notes.push(HashChunkNote {
			path: path.to_path_buf(),
			len: chunk.len(),
			head,
		});
		if is_arm
			&& probe
				.notes
				.iter()
				.filter(|note| note.path == probe.arm)
				.count() == 1
		{
			probe.cancel.cancel();
		}
	});
}

#[cfg(test)]
struct FinalBoundaryProbe {
	target_stage: &'static str,
	token: CancelToken,
}

#[cfg(test)]
thread_local! {
	static FINAL_BOUNDARY_PROBE: std::cell::RefCell<Option<FinalBoundaryProbe>> =
		const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct FinalBoundaryGuard;

#[cfg(test)]
impl Drop for FinalBoundaryGuard {
	fn drop(&mut self) {
		FINAL_BOUNDARY_PROBE.with(|slot| *slot.borrow_mut() = None);
	}
}

#[cfg(test)]
fn arm_final_boundary_cancel(
	target_stage: &'static str,
	token: CancelToken,
) -> FinalBoundaryGuard {
	FINAL_BOUNDARY_PROBE.with(|slot| {
		*slot.borrow_mut() = Some(FinalBoundaryProbe {
			target_stage,
			token,
		});
	});
	FinalBoundaryGuard
}

#[cfg(test)]
fn final_boundary_cancel_hook(stage: &'static str) {
	FINAL_BOUNDARY_PROBE.with(|slot| {
		if let Some(probe) = slot.borrow().as_ref() {
			if probe.target_stage == stage {
				probe.token.cancel();
			}
		}
	});
}

fn capture_file_freshness(
	path: &Path,
	opts: &RunOptions,
	non_regular_err: fn(String) -> TransferError,
) -> Result<Option<FileFreshness>, TransferError> {
	cancelled_err(opts, "hash-file")?;
	let sym_meta = match not_found_as_none(fs::symlink_metadata(path))? {
		Some(m) => m,
		None => return Ok(None),
	};
	let target_meta = if sym_meta.file_type().is_symlink() {
		let canonical = dunce::canonicalize(path)?;
		match not_found_as_none(fs::metadata(&canonical))? {
			Some(m) => m,
			None => return Ok(None),
		}
	} else {
		sym_meta
	};
	if !target_meta.file_type().is_file() {
		return Err(non_regular_err(path.to_string_lossy().into_owned()));
	}
	let size = target_meta.len();
	let mtime = target_meta.modified()?;
	let content_hash = hash_file(path, opts)?;
	Ok(Some(FileFreshness {
		size,
		mtime,
		content_hash,
	}))
}

fn capture_repo_freshness(
	root: &Path,
	opts: &RunOptions,
) -> Result<RepoFreshness, TransferError> {
	cancelled_err(opts, "plan-export")?;
	let git = match Git::open_with(root, opts) {
		Ok(g) => g,
		Err(GitError::NotARepository(_)) => {
			return Ok(RepoFreshness {
				head_commit: None,
				head_ref: None,
				index_hash: None,
			});
		}
		Err(e) => return Err(TransferError::Git(e)),
	};

	// Unborn and detached are answers; any other Git failure is an error.
	let head_commit = git.head_with(opts)?;
	let head_ref = git.head_ref_with(opts)?;

	let index_hash = match git
		.run_with(&["rev-parse", "--git-path", "index"], opts)
	{
		Ok(out) => {
			let stdout =
				strict_git_stdout(&out, opts, "rev-parse --git-path index")?;
			let git_path_str =
				String::from_utf8_lossy(stdout).trim().to_string();
			let index_path = if Path::new(&git_path_str).is_absolute() {
				PathBuf::from(git_path_str)
			} else {
				git.root().join(git_path_str)
			};
			if not_found_as_none(fs::metadata(&index_path))?.is_some() {
				Some(hash_file(&index_path, opts)?)
			} else {
				None
			}
		}
		Err(e) => return Err(TransferError::Git(e)),
	};

	Ok(RepoFreshness {
		head_commit,
		head_ref,
		index_hash,
	})
}

fn revalidate_repo_freshness(
	root: &Path,
	prev: &RepoFreshness,
	opts: &RunOptions,
) -> Result<(), TransferError> {
	let current = capture_repo_freshness(root, opts)?;
	if current.head_commit != prev.head_commit {
		return Err(TransferError::StaleSource {
			root: root.to_path_buf(),
			reason: format!(
				"HEAD commit changed from {} to {}",
				prev.head_commit.as_deref().unwrap_or("<none>"),
				current.head_commit.as_deref().unwrap_or("<none>")
			),
			change: StaleChange::Repository,
		});
	}
	if current.head_ref != prev.head_ref {
		return Err(TransferError::StaleSource {
			root: root.to_path_buf(),
			reason: format!(
				"HEAD branch/ref changed from {} to {}",
				prev.head_ref.as_deref().unwrap_or("<detached>"),
				current.head_ref.as_deref().unwrap_or("<detached>")
			),
			change: StaleChange::Repository,
		});
	}
	if current.index_hash != prev.index_hash {
		return Err(TransferError::StaleSource {
			root: root.to_path_buf(),
			reason: "Git index changed".to_string(),
			change: StaleChange::Repository,
		});
	}
	Ok(())
}

fn revalidate_destination_repo_freshness(
	root: &Path,
	prev: &RepoFreshness,
	opts: &RunOptions,
) -> Result<(), TransferError> {
	let current = capture_repo_freshness(root, opts)?;
	if current.head_commit != prev.head_commit {
		return Err(TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: format!(
				"destination HEAD commit changed from {} to {}",
				prev.head_commit.as_deref().unwrap_or("<none>"),
				current.head_commit.as_deref().unwrap_or("<none>")
			),
			change: StaleChange::Repository,
		});
	}
	if current.head_ref != prev.head_ref {
		return Err(TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: format!(
				"destination HEAD branch/ref changed from {} to {}",
				prev.head_ref.as_deref().unwrap_or("<detached>"),
				current.head_ref.as_deref().unwrap_or("<detached>")
			),
			change: StaleChange::Repository,
		});
	}
	if current.index_hash != prev.index_hash {
		return Err(TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: "destination Git index changed".to_string(),
			change: StaleChange::Repository,
		});
	}
	Ok(())
}

fn root_basename(path: &Path) -> String {
	let lossy = path.to_string_lossy();
	let normalized = lossy.replace('\\', "/");
	normalized
		.trim_end_matches('/')
		.rsplit('/')
		.next()
		.unwrap_or("")
		.to_string()
}

pub(crate) use crate::fscase::fs_is_case_insensitive;

// ---------------------------------------------------------------------------
// Core Public APIs
// ---------------------------------------------------------------------------

/// Validates an export selection for empty selection, relative path trust boundaries,
/// staged vs working conflicts, duplicate root basenames, and actual wire header collisions.
pub fn validate_export_selection(
	selection: &ExportSelection,
) -> Result<(), TransferError> {
	if selection.items.is_empty() {
		return Err(TransferError::EmptySelection);
	}

	for root in &selection.roots {
		root.validate()?;
	}
	if let Some(ref primary) = selection.primary_root {
		primary.validate()?;
	}

	// 1. Root membership and relative path trust boundaries
	for item in &selection.items {
		if !selection.roots.contains(&item.root) {
			return Err(TransferError::UnknownRoot(
				item.root.path().to_path_buf(),
			));
		}
		let Some(sanitized) = sanitize_relative_path(&item.relative_path)
		else {
			return Err(TransferError::UnsafePath(item.relative_path.clone()));
		};
		if sanitized != item.relative_path {
			return Err(TransferError::UnsafePath(item.relative_path.clone()));
		}
		let full = item.root.path().join(&sanitized);
		if !matches!(item.source, SourceKind::Working | SourceKind::Unstaged)
			&& escapes_all_roots(&[item.root.path()], &full)
		{
			return Err(TransferError::UnsafePath(item.relative_path.clone()));
		}
	}

	if let Some(ref primary) = selection.primary_root {
		if !selection.roots.contains(primary) {
			return Err(TransferError::UnknownRoot(
				primary.path().to_path_buf(),
			));
		}
	}

	// 2. Staged vs Working conflict check
	let mut seen_items: HashMap<(&CanonicalRootId, &str), HashSet<SourceKind>> =
		HashMap::new();
	for item in &selection.items {
		let key = (&item.root, item.relative_path.as_str());
		let sources = seen_items.entry(key).or_default();
		let worktree_kinds =
			[SourceKind::Working, SourceKind::Unstaged, SourceKind::File];
		let is_worktree = worktree_kinds.contains(&item.source);
		let has_worktree = worktree_kinds.iter().any(|k| sources.contains(k));
		if (is_worktree && sources.contains(&SourceKind::Staged))
			|| (item.source == SourceKind::Staged && has_worktree)
		{
			return Err(TransferError::StagedWorkingConflict {
				root: item.root.path().to_path_buf(),
				path: item.relative_path.clone(),
			});
		}
		if !sources.insert(item.source.clone()) {
			return Err(TransferError::MixedSourceCollision {
				root: item.root.path().to_path_buf(),
				path: item.relative_path.clone(),
				msg: "duplicate item selected from the same source".to_string(),
			});
		}
	}

	// 3. Ambiguous root basenames check
	let primary = selection
		.primary_root
		.clone()
		.or_else(|| selection.roots.first().cloned());
	if selection.roots.len() > 1 {
		let mut basename_to_roots: HashMap<String, Vec<PathBuf>> =
			HashMap::new();
		for root in &selection.roots {
			let name = root_basename(root.path());
			if !name.is_empty() {
				basename_to_roots
					.entry(name)
					.or_default()
					.push(root.path().to_path_buf());
			}
		}
		for (name, roots) in basename_to_roots {
			if roots.len() > 1 {
				return Err(TransferError::AmbiguousRootBasenames {
					basename: name,
					roots,
				});
			}
		}
	}

	// 4. Actual wire header collision check
	let mut seen_wire_paths = HashSet::new();
	for item in &selection.items {
		let is_primary = primary.as_ref() == Some(&item.root);
		let wire_path = if is_primary {
			item.relative_path.clone()
		} else {
			let prefix = root_basename(item.root.path());
			format!("{prefix}/{}", item.relative_path)
		};

		if !seen_wire_paths.insert(wire_path.clone()) {
			return Err(TransferError::WireHeaderCollision { wire_path });
		}
	}

	Ok(())
}

/// Content (`None`: not UTF-8) and why it was skipped, if it was.
type ReadContent = (Option<String>, Option<String>);

struct BlobBudget {
	remaining_budget: Option<usize>,
	max_payload_bytes: Option<usize>,
	current_total_bytes: usize,
	max_file_size_kb: f64,
	bypass_per_file_size: bool,
}

impl BlobBudget {
	/// 計算目前的讀取上限。
	///
	/// 整數大小判定：`size > floor(limit)` 等價於 `size > limit`。
	fn cap(&self) -> u64 {
		let per_file = (self.max_file_size_kb * 1024.0) as u64;
		let mut cap = self.remaining_budget.map_or(u64::MAX, |b| b as u64);
		if !self.bypass_per_file_size {
			cap = cap.min(per_file);
		}
		cap
	}

	fn over_cap(
		&self,
		size: u64,
		cap: u64,
		wire_path: &str,
	) -> Result<ReadContent, TransferError> {
		if !self.bypass_per_file_size
			&& size as f64 > self.max_file_size_kb * 1024.0
		{
			return Ok((
				None,
				Some(format!("size exceeds limit ({size} bytes)")),
			));
		}
		Err(TransferError::PayloadLimitExceeded {
			limit: self.max_payload_bytes.unwrap_or(cap as usize),
			actual: self.current_total_bytes.saturating_add(size as usize),
			reason: format!(
				"file '{wire_path}' exceeds remaining payload budget"
			),
			path: Some(wire_path.to_string()),
		})
	}
}

/// The header fixes OID and size before any body byte; a tree or commit is
/// an error, not a missing file (blobs_only reader).
fn blob_content(
	read: BlobRead,
	spec: &str,
	cap: u64,
	wire_path: &str,
	budget: &BlobBudget,
) -> Result<Option<ReadContent>, TransferError> {
	match read {
		BlobRead::Missing => Ok(None),
		BlobRead::NotABlob { kind } => {
			Err(TransferError::Git(crate::blob::not_a_file(spec, &kind)))
		}
		BlobRead::Text(s) => Ok(Some((Some(s), None))),
		BlobRead::NotText(_) => Ok(Some((None, None))),
		BlobRead::TooLarge {
			not_text: Some(_), ..
		} => Ok(Some((None, None))),
		BlobRead::TooLarge { size, .. } => {
			budget.over_cap(size, cap, wire_path).map(Some)
		}
	}
}

fn gitlink_read(
	read: BlobRead,
	cap: u64,
	wire_path: &str,
	budget: &BlobBudget,
	is_staged: bool,
) -> Result<ReadContent, TransferError> {
	match read {
		BlobRead::Text(s) => Ok((Some(s), None)),
		BlobRead::Missing
		| BlobRead::NotText(_)
		| BlobRead::NotABlob { .. }
		| BlobRead::TooLarge {
			not_text: Some(_), ..
		} => {
			if is_staged {
				Ok((Some(UNREADABLE_FILE_MARKER.to_string()), None))
			} else {
				Ok((None, None))
			}
		}
		BlobRead::TooLarge { size, .. } => {
			budget.over_cap(size, cap, wire_path)
		}
	}
}

fn deleted_read(
	content: DeletedContent,
	cap: u64,
	wire_path: &str,
	budget: &BlobBudget,
) -> Result<ReadContent, TransferError> {
	match content {
		DeletedContent::Text(t) => Ok((Some(t), None)),
		DeletedContent::Marker => {
			Ok((Some(DELETED_FILE_MARKER.to_string()), None))
		}
		DeletedContent::TooLarge { size, .. } => {
			budget.over_cap(size, cap, wire_path)
		}
	}
}

/// The git revision spec for reading a path's stage-0 index content. The
/// stage must be spelled out: a bare `:<path>` turns a leading `0:` in the
/// name into a stage number, so `:0:a` reads `a` where the file `0:a` was
/// meant.
fn staged_index_spec(toplevel_rel: String) -> String {
	format!(":0:{toplevel_rel}")
}

fn make_payload_opts<'a, 'f>(
	settings: &'a Settings,
	source_root: Option<&'a str>,
	files: &'f [PayloadFile],
	include_empty_wrappers: bool,
) -> BorrowedPayloadOptions<'a, 'f> {
	BorrowedPayloadOptions {
		header_format: &settings.header_format,
		pre_text: &settings.pre_text,
		post_text: &settings.post_text,
		add_extra_line_between_files: settings.add_extra_line_between_files,
		source_root,
		files,
		include_empty_wrappers,
	}
}

/// Same bytes as [`format::write_payload_borrowed`], polling cancel between files.
fn write_export_payload<W: std::fmt::Write>(
	w: &mut W,
	payload_opts: &BorrowedPayloadOptions<'_, '_>,
	opts: &RunOptions,
) -> Result<(), TransferError> {
	cancelled_err(opts, "serialize-export")?;
	let mut writer = format::PayloadLineWriter::new(w);
	let custom = format::HeaderPattern::new(payload_opts.header_format);
	let custom = custom.as_ref();
	format::write_payload_envelope(&mut writer, payload_opts, custom)
		.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;
	for file in payload_opts.files {
		cancelled_err(opts, "serialize-export")?;
		format::write_payload_file(
			&mut writer,
			file,
			payload_opts.header_format,
			payload_opts.add_extra_line_between_files,
			custom,
		)
		.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;
	}
	cancelled_err(opts, "serialize-export")?;
	format::write_payload_footer(&mut writer, payload_opts, custom)
		.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;
	Ok(())
}

/// Closes a cat-file session before the caller observes success. Cleanup
/// errors win over a plan; `Drop` is not treated as a successful close.
fn finish_reader<T>(
	blobs: &mut BlobReader,
	outcome: Result<T, TransferError>,
) -> Result<T, TransferError> {
	match blobs.close() {
		Ok(()) => outcome,
		Err(err) => {
			let close_err = TransferError::from(err);
			let cleanup = matches!(
				close_err,
				TransferError::Git(GitError::Cleanup { .. })
			);
			if cleanup || outcome.is_ok() {
				Err(close_err)
			} else {
				outcome
			}
		}
	}
}

/// Plans and generates an export payload across one or multiple repositories.
/// Freezes revisions once, enforces cumulative budget before retention, and revalidates freshness.
pub fn plan_export(
	selection: &ExportSelection,
	settings: &Settings,
	max_payload_bytes: Option<usize>,
) -> Result<ExportPlan, TransferError> {
	plan_export_with(
		selection,
		settings,
		max_payload_bytes,
		&RunOptions::default(),
	)
}

/// [`plan_export`] with the caller's runner options. `opts.cancel` is polled
/// through Git, bounded file reads, serialization and the final revalidation.
/// Cancellation returns `TransferError::Git(GitError::Cancelled)` and never a
/// partially usable plan.
pub fn plan_export_with(
	selection: &ExportSelection,
	settings: &Settings,
	max_payload_bytes: Option<usize>,
	opts: &RunOptions,
) -> Result<ExportPlan, TransferError> {
	cancelled_err(opts, "plan-export")?;
	validate_export_selection(selection)?;

	// Freeze repo freshness and revisions once upfront (without hashing working files)
	let mut repos = HashMap::new();
	for root in &selection.roots {
		cancelled_err(opts, "plan-export")?;
		repos.insert(root.clone(), capture_repo_freshness(root.path(), opts)?);
	}

	// Every Git call except blob reads happens here, before a cat-file
	// holds this thread's Git slot.
	let mut gits: HashMap<CanonicalRootId, Git> = HashMap::new();
	for item in &selection.items {
		let needs_git = !matches!(item.source, SourceKind::File)
			|| item.change_type == Some(ChangeType::Deleted);
		if !needs_git || gits.contains_key(&item.root) {
			continue;
		}
		match Git::open_with(item.root.path(), opts) {
			Ok(git) => {
				gits.insert(item.root.clone(), git);
			}
			// A plain folder may still list a deleted file: it gets the marker.
			Err(GitError::NotARepository(_))
				if matches!(item.source, SourceKind::File) => {}
			Err(e) => return Err(e.into()),
		}
	}
	let mut root_offsets: HashMap<CanonicalRootId, Option<String>> =
		HashMap::new();
	for (root_id, git) in &gits {
		let git_root = dunce::canonicalize(git.root())
			.unwrap_or_else(|_| git.root().to_path_buf());
		let root_path = dunce::canonicalize(root_id.path())
			.unwrap_or_else(|_| root_id.path().to_path_buf());
		let offset = match root_path.strip_prefix(&git_root) {
			Ok(p) if !p.as_os_str().is_empty() => {
				Some(p.to_string_lossy().replace('\\', "/"))
			}
			_ => None,
		};
		root_offsets.insert(root_id.clone(), offset);
	}
	let mut frozen_commits = HashMap::new();
	// Frozen commit -> its parents, where deleted files are read.
	// Resolved before any cat-file session so this thread does not nest Git.
	let mut parents: HashMap<String, Vec<String>> = HashMap::new();
	for item in &selection.items {
		match &item.source {
			SourceKind::Commit { rev } => {
				let key = (item.root.clone(), rev.clone());
				if let std::collections::hash_map::Entry::Vacant(e) =
					frozen_commits.entry(key)
				{
					let git = gits.get(&item.root).ok_or_else(|| {
						TransferError::UnknownRoot(
							item.root.path().to_path_buf(),
						)
					})?;
					let oid = git.resolve_commit_with(rev, opts)?;
					parents.insert(oid.clone(), git.parents_with(&oid, opts)?);
					e.insert(oid);
				}
			}
			SourceKind::Range { base, tip } => {
				for rev in [base, tip] {
					let key = (item.root.clone(), rev.clone());
					if let std::collections::hash_map::Entry::Vacant(e) =
						frozen_commits.entry(key)
					{
						let git = gits.get(&item.root).ok_or_else(|| {
							TransferError::UnknownRoot(
								item.root.path().to_path_buf(),
							)
						})?;
						let oid = git.resolve_commit_with(rev, opts)?;
						e.insert(oid);
					}
				}
			}
			_ => {}
		}
	}
	let mut blobs = BlobReader::blobs_only(opts);
	let outcome = (|| -> Result<ExportPlan, TransferError> {
		let mut working_files = HashMap::new();

		let primary = selection
			.primary_root
			.clone()
			.or_else(|| selection.roots.first().cloned());

		// Commit and range selections begin with fallback = true (omitting empty wrappers);
		// Staged and Deleted only trigger fallback once an included entry is admitted.
		let is_graph = selection.items.iter().any(|item| {
			matches!(
				item.source,
				SourceKind::Commit { .. } | SourceKind::Range { .. }
			)
		});
		let mut fallback = is_graph;

		let default_source_root = if selection.roots.len() == 1 {
			selection.source_root.clone().or_else(|| {
				paths::source_root_name(&[selection.roots[0].path()])
			})
		} else {
			None
		};

		let custom_pattern =
			format::HeaderPattern::new(&settings.header_format);
		let custom = custom_pattern.as_ref();

		let mut initial_counter = format::CountingWriter::default();
		let mut initial_writer =
			format::PayloadLineWriter::new(&mut initial_counter);
		format::write_payload_envelope(
			&mut initial_writer,
			&make_payload_opts(
				settings,
				default_source_root.as_deref(),
				&[],
				!fallback,
			),
			custom,
		)
		.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;

		format::write_payload_footer(
			&mut initial_writer,
			&make_payload_opts(
				settings,
				default_source_root.as_deref(),
				&[],
				!fallback,
			),
			custom,
		)
		.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;

		let mut current_total_bytes = initial_counter.count;

		if let Some(max_bytes) = max_payload_bytes {
			if current_total_bytes > max_bytes {
				return Err(TransferError::PayloadLimitExceeded {
				limit: max_bytes,
				actual: current_total_bytes,
				reason:
					"serialized wrapper overhead (pre/post-text, markers) exceeds payload budget"
						.to_string(),
				path: None,
			});
			}
		}

		let mut files = Vec::new();
		let mut copied_file_count = 0usize;
		let mut skipped_file_size_count = 0usize;
		let mut skipped_unreadable_count = 0usize;
		let mut file_limit_reached = false;

		for item in &selection.items {
			cancelled_err(opts, "plan-export")?;
			let is_primary = primary.as_ref() == Some(&item.root);
			let wire_path = if is_primary {
				item.relative_path.clone()
			} else {
				let prefix = root_basename(item.root.path());
				format!("{prefix}/{}", item.relative_path)
			};

			let absolute = item.root.path().join(&item.relative_path);

			// Parity check:
			// File mode (copy.rs): count limit checked BEFORE filtering candidate.
			// Git sources (gitsrc.rs): filter checked BEFORE count limit.
			if matches!(item.source, SourceKind::File) {
				if settings.set_max_file_count
					&& copied_file_count as f64 >= settings.file_count_limit
				{
					file_limit_reached = true;
					break;
				}
				let filter_absolute =
					if selection.roots.len() == 1 || is_primary {
						selection
							.spelled_root
							.as_ref()
							.map(|s| s.join(&item.relative_path))
							.unwrap_or_else(|| absolute.clone())
					} else {
						absolute.clone()
					};
				if settings.use_filters
					&& !filter::file_matches_filters(
						&wire_path,
						&settings.filter_rules,
						settings.use_include_filters,
						settings.use_exclude_filters,
						Some(&filter_absolute.to_string_lossy()),
					) {
					continue;
				}
			} else {
				let filter_wire_path;
				let filter_path = match (&selection.filter_root, &item.source) {
					(
						Some(filter_root),
						SourceKind::Commit { .. } | SourceKind::Range { .. },
					) => {
						let filter_root_canonical =
							dunce::canonicalize(filter_root)
								.unwrap_or_else(|_| filter_root.clone());
						let p = paths::to_clipboard_path_from_roots(
							&[filter_root_canonical],
							&absolute,
							None,
						);
						if p.is_empty() {
							wire_path.as_str()
						} else {
							filter_wire_path = p;
							filter_wire_path.as_str()
						}
					}
					_ => wire_path.as_str(),
				};
				if settings.use_filters
					&& !filter::file_matches_filters(
						filter_path,
						&settings.filter_rules,
						settings.use_include_filters,
						settings.use_exclude_filters,
						Some(&absolute.to_string_lossy()),
					) {
					continue;
				}
				if settings.set_max_file_count
					&& copied_file_count as f64 >= settings.file_count_limit
				{
					file_limit_reached = true;
					break;
				}
			}

			let remaining_budget = max_payload_bytes
				.map(|m| m.saturating_sub(current_total_bytes));

			let blob_budget = |bypass: bool| BlobBudget {
				remaining_budget,
				max_payload_bytes,
				current_total_bytes,
				max_file_size_kb: settings.max_file_size_kb,
				bypass_per_file_size: bypass,
			};

			let deleted = item.change_type == Some(ChangeType::Deleted);
			if item.gitlink
				&& !deleted && matches!(
				item.source,
				SourceKind::Working | SourceKind::Unstaged
			) {
				continue;
			}
			let rel = &item.relative_path;
			let toplevel_rel =
				match root_offsets.get(&item.root).and_then(|o| o.as_deref()) {
					Some(off) => format!("{off}/{rel}"),
					None => rel.clone(),
				};
			// The outer `Option` says whether to record freshness; the inner
			// one is the file's state (`None` = absent).
			let (content, skipped_reason, freshness_info): (
				Option<String>,
				Option<String>,
				Option<Option<FileFreshness>>,
			) = match &item.source {
				SourceKind::Commit { rev } => {
					let frozen_oid = frozen_commits
						.get(&(item.root.clone(), rev.clone()))
						.ok_or_else(|| {
							TransferError::Git(GitError::InvalidRevision(
								rev.clone(),
							))
						})?;
					let git = gits.get(&item.root).ok_or_else(|| {
						TransferError::UnknownRoot(
							item.root.path().to_path_buf(),
						)
					})?;
					let (text, reason) = if deleted {
						// Every parent in order, like gitsrc: a merge may
						// delete a file only one side had. Deleted graph old
						// content bypasses the per-file size check.
						let specs = parents
							.get(frozen_oid)
							.into_iter()
							.flatten()
							.map(|p| format!("{p}:{toplevel_rel}"));
						let b = blob_budget(true);
						let cap = b.cap();
						let content =
							blobs.deleted_file_content(git, specs, cap)?;
						deleted_read(content, cap, &wire_path, &b)?
					} else if item.gitlink {
						let spec = format!("{frozen_oid}:{toplevel_rel}");
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read_lenient(git, &spec, cap)?;
						gitlink_read(read, cap, &wire_path, &b, false)?
					} else {
						let spec = format!("{frozen_oid}:{toplevel_rel}");
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read(git, &spec, cap)?;
						blob_content(read, &spec, cap, &wire_path, &b)?.ok_or(
							TransferError::Git(GitError::InvalidRevision(spec)),
						)?
					};
					(text, reason, None)
				}
				SourceKind::Range { base, tip } => {
					let frozen_base = frozen_commits
						.get(&(item.root.clone(), base.clone()))
						.ok_or_else(|| {
							TransferError::Git(GitError::InvalidRevision(
								base.clone(),
							))
						})?;
					let frozen_tip = frozen_commits
						.get(&(item.root.clone(), tip.clone()))
						.ok_or_else(|| {
							TransferError::Git(GitError::InvalidRevision(
								tip.clone(),
							))
						})?;
					let git = gits.get(&item.root).ok_or_else(|| {
						TransferError::UnknownRoot(
							item.root.path().to_path_buf(),
						)
					})?;
					let (text, reason) = if deleted {
						let spec = format!("{frozen_base}:{toplevel_rel}");
						let b = blob_budget(true);
						let cap = b.cap();
						let content =
							blobs.deleted_file_content(git, [spec], cap)?;
						deleted_read(content, cap, &wire_path, &b)?
					} else if item.gitlink {
						let spec = format!("{frozen_tip}:{toplevel_rel}");
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read_lenient(git, &spec, cap)?;
						gitlink_read(read, cap, &wire_path, &b, false)?
					} else {
						let spec = format!("{frozen_tip}:{toplevel_rel}");
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read(git, &spec, cap)?;
						blob_content(read, &spec, cap, &wire_path, &b)?.ok_or(
							TransferError::Git(GitError::InvalidRevision(spec)),
						)?
					};
					(text, reason, None)
				}
				SourceKind::Staged => {
					let git = gits.get(&item.root).ok_or_else(|| {
						TransferError::UnknownRoot(
							item.root.path().to_path_buf(),
						)
					})?;
					let (text, reason) = if deleted {
						// A staged deletion is gone from the index; HEAD has it.
						let b = blob_budget(false);
						let cap = b.cap();
						let content = blobs.deleted_file_content(
							git,
							[format!("HEAD:{toplevel_rel}")],
							cap,
						)?;
						deleted_read(content, cap, &wire_path, &b)?
					} else if item.gitlink {
						let spec = staged_index_spec(toplevel_rel);
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read_lenient(git, &spec, cap)?;
						gitlink_read(read, cap, &wire_path, &b, true)?
					} else {
						let spec = staged_index_spec(toplevel_rel);
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read(git, &spec, cap)?;
						let (text, reason) =
							blob_content(read, &spec, cap, &wire_path, &b)?
								.ok_or(TransferError::Git(
									GitError::InvalidRevision(spec),
								))?;
						if reason.is_none() && text.is_none() {
							(Some(UNREADABLE_FILE_MARKER.to_string()), None)
						} else {
							(text, reason)
						}
					};
					(text, reason, None)
				}
				SourceKind::Working
				| SourceKind::Unstaged
				| SourceKind::File
					if deleted =>
				{
					// Pre-deletion content: `Unstaged` is the worktree against
					// the index, so the index has it; `Working` is the SCM view
					// and reads HEAD like gitsrc (TS parity), as does `File`.
					let spec = if item.source == SourceKind::Unstaged {
						staged_index_spec(toplevel_rel)
					} else {
						format!("HEAD:{toplevel_rel}")
					};
					let b = blob_budget(false);
					let cap = b.cap();
					let (text, reason) = if let Some(git) = gits.get(&item.root)
					{
						let content =
							blobs.deleted_file_content(git, [spec], cap)?;
						deleted_read(content, cap, &wire_path, &b)?
					} else {
						deleted_read(
							DeletedContent::Marker,
							cap,
							&wire_path,
							&b,
						)?
					};
					// The absence is part of the snapshot: recreating the path
					// before the clipboard write invalidates the export.
					(text, reason, Some(None))
				}
				SourceKind::Working
				| SourceKind::Unstaged
				| SourceKind::File => 'read_file: {
					cancelled_err(opts, "read-file")?;
					let is_changed_item = matches!(
						item.source,
						SourceKind::Working | SourceKind::Unstaged
					);
					let sym_meta = match fs::symlink_metadata(&absolute) {
						Ok(m) => m,
						Err(_) if is_changed_item => {
							break 'read_file (None, None, None);
						}
						Err(e) => return Err(e.into()),
					};
					// The selection check no longer refuses changed items that
					// escape (symlinked parent directory or link target), so
					// they are skipped here before any read.
					if is_changed_item
						&& escapes_all_roots(&[item.root.path()], &absolute)
					{
						break 'read_file (None, None, None);
					}
					let target_meta = if sym_meta.file_type().is_symlink() {
						let roots = [item.root.path()];
						if escapes_all_roots(&roots, &absolute) {
							if is_changed_item {
								break 'read_file (None, None, None);
							}
							return Err(TransferError::UnsafePath(
								absolute.to_string_lossy().into_owned(),
							));
						}
						let canonical = match dunce::canonicalize(&absolute) {
							Ok(c) => c,
							Err(_) if is_changed_item => {
								break 'read_file (None, None, None);
							}
							Err(e) => return Err(e.into()),
						};
						if escapes_all_roots(&roots, &canonical) {
							if is_changed_item {
								break 'read_file (None, None, None);
							}
							return Err(TransferError::UnsafePath(
								canonical.to_string_lossy().into_owned(),
							));
						}
						match fs::metadata(&canonical) {
							Ok(m) => m,
							Err(_) if is_changed_item => {
								break 'read_file (None, None, None);
							}
							Err(e) => return Err(e.into()),
						}
					} else {
						sym_meta
					};

					if !target_meta.file_type().is_file() {
						if is_changed_item {
							break 'read_file (None, None, None);
						}
						return Err(TransferError::SpecialFile(
							absolute.to_string_lossy().into_owned(),
						));
					}

					let file_size = target_meta.len();
					let per_file_limit =
						(settings.max_file_size_kb * 1024.0) as u64;
					let over_per_file =
						file_size as f64 > settings.max_file_size_kb * 1024.0;
					let over_budget = remaining_budget
						.is_some_and(|b| file_size as usize > b);
					if is_changed_item && (over_per_file || over_budget) {
						let mut scan = crate::blob::ChunkTextScan::new();
						let scan_res = for_each_chunk(
							&absolute,
							None,
							opts,
							"read-file",
							|chunk| scan.feed(chunk),
						);
						match scan_res {
							Ok(_) => {}
							Err(e)
								if matches!(
									e,
									TransferError::Git(
										GitError::Cancelled { .. }
									)
								) || opts
									.cancel
									.as_ref()
									.is_some_and(CancelToken::is_cancelled) =>
							{
								return Err(e);
							}
							Err(_) => break 'read_file (None, None, None),
						}
						if scan.finish().is_some() {
							break 'read_file (None, None, None);
						}
						if over_per_file {
							(
								None,
								Some(format!(
									"size exceeds limit ({file_size} bytes)"
								)),
								None,
							)
						} else {
							let budget = remaining_budget.unwrap_or(usize::MAX);
							return Err(TransferError::PayloadLimitExceeded {
								limit: max_payload_bytes.unwrap_or(budget),
								actual: current_total_bytes
									+ file_size as usize,
								reason: format!(
									"file '{wire_path}' exceeds remaining payload budget"
								),
								path: Some(wire_path.to_string()),
							});
						}
					} else if over_per_file {
						(
							None,
							Some(format!(
								"size exceeds limit ({file_size} bytes)"
							)),
							None,
						)
					} else if over_budget {
						let budget = remaining_budget.unwrap_or(usize::MAX);
						return Err(TransferError::PayloadLimitExceeded {
							limit: max_payload_bytes.unwrap_or(budget),
							actual: current_total_bytes + file_size as usize,
							reason: format!(
								"file '{wire_path}' exceeds remaining payload budget"
							),
							path: Some(wire_path.to_string()),
						});
					} else {
						let read_cap = match remaining_budget {
							Some(b) => (b as u64).min(per_file_limit),
							None => per_file_limit,
						};
						// Admit the metadata size before retaining the body.
						cancelled_err(opts, "read-file")?;
						let mut bytes = Vec::with_capacity(file_size as usize);
						let mut hasher = Sha256::new();
						let read_res = for_each_chunk(
							&absolute,
							Some(read_cap.saturating_add(1)),
							opts,
							"read-file",
							|chunk| {
								bytes.extend_from_slice(chunk);
								hasher.update(chunk);
							},
						);
						let read_len = match read_res {
							Ok(len) => len,
							Err(e)
								if matches!(
									e,
									TransferError::Git(
										GitError::Cancelled { .. }
									)
								) || opts
									.cancel
									.as_ref()
									.is_some_and(CancelToken::is_cancelled) =>
							{
								return Err(e);
							}
							Err(_) if is_changed_item => {
								break 'read_file (None, None, None);
							}
							Err(e) => return Err(e),
						};
						if read_len > read_cap {
							if remaining_budget
								.is_some_and(|b| read_len as usize > b)
							{
								return Err(
									TransferError::PayloadLimitExceeded {
										limit: max_payload_bytes
											.unwrap_or(read_cap as usize),
										actual: current_total_bytes
											+ read_len as usize,
										reason: format!(
									"working file '{wire_path}' grew past limit during read"
								),
										path: Some(wire_path.to_string()),
									},
								);
							}
							(
								None,
								Some(format!(
									"size exceeds limit ({read_len} bytes)"
								)),
								None,
							)
						} else {
							let mtime = match target_meta.modified() {
								Ok(m) => m,
								Err(_) if is_changed_item => {
									break 'read_file (None, None, None);
								}
								Err(e) => return Err(e.into()),
							};
							let content_hash = hasher.finalize().into();
							let freshness = FileFreshness {
								size: read_len,
								mtime,
								content_hash,
							};
							(
								decode_utf8_or_skip(bytes),
								None,
								Some(Some(freshness)),
							)
						}
					}
				}
			};

			if let Some(f) = freshness_info {
				working_files.insert((item.root.clone(), rel.clone()), f);
			}

			// Drop unreadable/binary files (matching copy.rs and gitsrc.rs)
			if skipped_reason.is_none() && content.is_none() {
				skipped_unreadable_count += 1;
				continue;
			}

			if item.change_type == Some(ChangeType::Deleted)
				|| matches!(item.source, SourceKind::Staged)
			{
				fallback = true;
			}

			if skipped_reason.is_some() {
				skipped_file_size_count += 1;
			} else if content.as_deref() == Some(UNREADABLE_FILE_MARKER) {
				skipped_unreadable_count += 1;
			} else {
				copied_file_count += 1;
			}

			let payload_file = PayloadFile {
				path: wire_path,
				content,
				change_type: item.change_type,
				skipped_reason,
			};

			cancelled_err(opts, "serialize-export")?;
			let mut file_counter = format::CountingWriter::default();
			let mut file_writer = format::PayloadLineWriter::new_with_written(
				&mut file_counter,
				true,
			);
			format::write_payload_file(
				&mut file_writer,
				&payload_file,
				&settings.header_format,
				settings.add_extra_line_between_files,
				custom,
			)
			.map_err(|_| {
				TransferError::UnsafePath("formatting error".into())
			})?;

			let file_delta = file_counter.count;

			if let Some(max_bytes) = max_payload_bytes {
				if current_total_bytes + file_delta > max_bytes {
					return Err(TransferError::PayloadLimitExceeded {
						limit: max_bytes,
						actual: current_total_bytes + file_delta,
						reason: format!(
						"file '{}' serialized overhead exceeds remaining payload budget",
						payload_file.path
					),
						path: Some(payload_file.path.clone()),
					});
				}
			}
			current_total_bytes += file_delta;

			files.push(payload_file);
		}

		let final_source_root = if is_graph && files.is_empty() {
			None
		} else {
			default_source_root
		};

		// Revalidation runs Git: the cat-file slot must be free first.
		blobs.close()?;
		let freshness = SourceFreshnessSnapshot {
			repos,
			frozen_commits,
			working_files,
		};
		freshness.revalidate_with(opts)?;

		let payload_opts = make_payload_opts(
			settings,
			final_source_root.as_deref(),
			&files,
			!fallback,
		);
		let mut final_counter = format::CountingWriter::default();
		write_export_payload(&mut final_counter, &payload_opts, opts)?;

		if let Some(max_bytes) = max_payload_bytes {
			if final_counter.count > max_bytes {
				return Err(TransferError::PayloadLimitExceeded {
					limit: max_bytes,
					actual: final_counter.count,
					reason:
						"serialized payload with headers/wrappers exceeds limit"
							.to_string(),
					path: None,
				});
			}
		}

		cancelled_err(opts, "serialize-export")?;
		let mut payload = String::with_capacity(final_counter.count);
		write_export_payload(&mut payload, &payload_opts, opts)?;

		let stats = payload_stats(&payload);
		cancelled_err(opts, "serialize-export")?;

		Ok(ExportPlan {
			files,
			payload,
			stats,
			freshness,
			copied_file_count,
			skipped_file_size_count,
			skipped_unreadable_count,
			file_limit_reached,
		})
	})();
	finish_reader(&mut blobs, outcome)
}

/// Detects distinct top-level root prefixes from clipboard text entries.
pub fn detect_clipboard_prefixes(
	clipboard_text: &str,
	header_format: &str,
) -> Vec<String> {
	let entries = format::parse_clipboard(clipboard_text, header_format);
	let mut prefixes = HashSet::new();
	let mut list = Vec::new();
	for entry in entries {
		if let Some((first, _)) = entry.path.split_once('/') {
			if !first.is_empty() && prefixes.insert(first.to_string()) {
				list.push(first.to_string());
			}
		}
	}
	list
}

/// Plans an import against explicit destination mappings using core restore planning,
/// validating paths, detecting duplicate targets, and capturing destination freshness.
pub fn plan_import(
	clipboard_text: &str,
	header_format: &str,
	destination_roots: &[PathBuf],
	mapping: &ImportMapping,
) -> Result<TransferImportPlan, TransferError> {
	plan_import_with(
		clipboard_text,
		header_format,
		destination_roots,
		mapping,
		&RunOptions::default(),
	)
}

/// [`plan_import`] with the caller's runner options.
///
/// The clipboard parser and per-file restore planner are the ones
/// [`plan_import`] uses. Absolute entry paths not present in explicit entry
/// destinations are resolved against destination roots before sanitization;
/// unresolvable absolute deleted entries are skipped as unresolved paths rather
/// than deleting nested files, while unresolvable absolute writes keep their
/// literal nested path under the primary destination (or skip if drive-letter
/// paths). Case-insensitive destination filesystems are probed at runtime to
/// fold identities in target collision checks.
/// The token is polled before and after parsing, and
/// between roots, routed entries, per-file planning, target-identity checks
/// and freshness reads. Encoding classification may `read` up to 8 MiB in
/// one call, and one freshness chunk is at most 8 KiB; neither `read` can
/// be interrupted mid-syscall. `canonicalize` for a target identity is one
/// synchronous walk.
pub fn plan_import_with(
	clipboard_text: &str,
	header_format: &str,
	destination_roots: &[PathBuf],
	mapping: &ImportMapping,
	opts: &RunOptions,
) -> Result<TransferImportPlan, TransferError> {
	cancelled_err(opts, "plan-import")?;
	let entries = format::parse_clipboard(clipboard_text, header_format);
	cancelled_err(opts, "plan-import")?;
	plan_import_parsed_with(entries, destination_roots, mapping, opts)
		.map(|(plan, _)| plan)
}

/// Points in planning where a test changes the disk, deterministically:
/// what another process could do in between, in this order.
#[cfg(any(test, feature = "test-support"))]
pub mod test_hooks {
	use std::sync::Mutex;

	type Hook = Box<dyn Fn() + Send>;

	/// Process-wide: a worker in a test plans on its own thread. A test runs
	/// in its own process under nextest.
	static AFTER_TAIL_CHECK: Mutex<Option<Hook>> = Mutex::new(None);

	/// Removes the hook when dropped.
	pub struct HookGuard;

	impl Drop for HookGuard {
		fn drop(&mut self) {
			*AFTER_TAIL_CHECK.lock().unwrap_or_else(|e| e.into_inner()) = None;
		}
	}

	/// Runs `f` each time planning has checked that no level of a placed
	/// folder's tail exists, before it tells the folders the user named
	/// from the others.
	pub fn after_tail_check(f: impl Fn() + Send + 'static) -> HookGuard {
		*AFTER_TAIL_CHECK.lock().unwrap_or_else(|e| e.into_inner()) =
			Some(Box::new(f));
		HookGuard
	}

	/// Removes the switch below when dropped.
	pub struct NameKeyGuard;

	impl Drop for NameKeyGuard {
		fn drop(&mut self) {
			crate::foldernames::set_distinct_name_keys(false);
		}
	}

	/// Until the guard drops, every name is told from every other one,
	/// even from itself (`foldernames::name_key` gives each call its own
	/// key): what a filesystem merges that the key does not know. A safety
	/// check that still holds with this on does not rest on the key.
	pub fn distinct_name_keys() -> NameKeyGuard {
		crate::foldernames::set_distinct_name_keys(true);
		NameKeyGuard
	}

	pub(super) fn run_after_tail_check() {
		if let Some(hook) = AFTER_TAIL_CHECK
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
		{
			hook();
		}
	}
}

/// A folder an absolute entry path may name, with the anchor it is planned
/// against and the names below that anchor still to be made.
struct RouteCandidate {
	path: PathBuf,
	anchor: CanonicalRootId,
	tail: String,
	/// The placed folder (its place in `placed_list`) when it has a tail.
	placed: Option<usize>,
}

/// The placed folders with a tail, each once (one anchor and tail named
/// twice is one folder to agree to): the primary destination, then the
/// prefixes in the order of their names. Its order is the place an
/// agreement names a folder by (`DestinationFreshnessSnapshot::placed`).
fn placed_list(mapping: &ImportMapping) -> Vec<&PlacedRoot> {
	let mut prefixes: Vec<(&String, &PlacedRoot)> =
		mapping.prefix_destinations.iter().collect();
	prefixes.sort_by_key(|(prefix, _)| *prefix);
	let mut out: Vec<&PlacedRoot> = Vec::new();
	for placed in mapping
		.primary_destination
		.iter()
		.chain(prefixes.into_iter().map(|(_, placed)| placed))
	{
		if !placed.tail().is_empty() && !out.contains(&placed) {
			out.push(placed);
		}
	}
	out
}

/// Which placed folder (its place in `placed`) an absolute entry path
/// names, when that folder is not made yet and the path does not read as
/// it was placed: the existing folder nearest above the path is the
/// placed folder's anchor, told by the filesystem's object, and the names
/// below it start with its tail, by `name_key` where that filesystem folds
/// case. Returns the place and the path below the tail. A miss only leaves
/// the path to the rule for an absolute path outside every destination;
/// the deepest tail wins.
fn placed_for_absolute(
	placed: &[&PlacedRoot],
	normalized: &str,
) -> Option<(usize, String)> {
	if placed.is_empty() {
		return None;
	}
	let path = Path::new(normalized);
	let mut names: Vec<&str> = Vec::new();
	let mut cur = path;
	let anchor = loop {
		match fs::symlink_metadata(cur) {
			Ok(meta) if meta.is_dir() => break cur,
			Ok(_) => {
				if fs::metadata(cur).is_ok_and(|m| m.is_dir()) {
					break cur;
				}
				return None;
			}
			Err(_) => {
				names.push(cur.file_name()?.to_str()?);
				cur = cur.parent()?;
			}
		}
	};
	names.reverse();
	let key = DirKey::of(anchor);
	let folds = fs_is_case_insensitive(
		&dunce::canonicalize(anchor).unwrap_or_else(|_| anchor.to_path_buf()),
	);
	placed
		.iter()
		.enumerate()
		.filter(|(_, p)| {
			p.tail().len() < names.len()
				&& DirKey::of(p.anchor().path()) == key
				&& p.tail().iter().zip(&names).all(|(t, n)| {
					crate::foldernames::name_key(t, folds)
						== crate::foldernames::name_key(n, folds)
				})
		})
		.max_by_key(|(_, p)| p.tail().len())
		.map(|(id, p)| (id, names[p.tail().len()..].join("/")))
}

/// The folders an absolute entry path can name. Without a folder still to
/// be made this is exactly the destination roots, in their order. A root
/// that is only the anchor of a folder still to be made is not one: an
/// absolute path under it but outside the destination would otherwise land
/// in it.
fn route_candidates(
	canonical_dest_roots: &[CanonicalRootId],
	mapping: &ImportMapping,
	placed_ids: &[&PlacedRoot],
) -> Vec<RouteCandidate> {
	let mut placed: Vec<&PlacedRoot> = mapping
		.primary_destination
		.iter()
		.chain(mapping.prefix_destinations.values())
		.collect();
	let tail_anchors: HashSet<&CanonicalRootId> = placed
		.iter()
		.filter(|p| !p.tail().is_empty())
		.map(|p| p.anchor())
		.collect();
	let mut out: Vec<RouteCandidate> = Vec::new();
	if tail_anchors.is_empty() {
		out.extend(canonical_dest_roots.iter().map(|id| RouteCandidate {
			path: id.path().to_path_buf(),
			anchor: id.clone(),
			tail: String::new(),
			placed: None,
		}));
		return out;
	}
	let mut add = |path: PathBuf,
	               anchor: &CanonicalRootId,
	               tail: String,
	               placed: Option<usize>| {
		if !out.iter().any(|c| c.path == path) {
			out.push(RouteCandidate {
				path,
				anchor: anchor.clone(),
				tail,
				placed,
			});
		}
	};
	for id in canonical_dest_roots {
		if !tail_anchors.contains(id) {
			add(id.path().to_path_buf(), id, String::new(), None);
		}
	}
	placed.sort_by_key(|p| p.path());
	for p in placed {
		let id = placed_ids.iter().position(|q| *q == p);
		add(p.path(), p.anchor(), p.tail_rel(), id);
	}
	for em in mapping.entry_destinations.values() {
		add(em.root.path().to_path_buf(), &em.root, String::new(), None);
	}
	out
}

/// The folders each destination (the primary one, then the prefixes in the
/// order of their names) names below its anchor, shallow to deep, under the
/// spellings in `names`: the first spelling met, the named folders first.
fn named_folder_chains(
	mapping: &ImportMapping,
	names: &mut NewFolderNames,
) -> Vec<PathBuf> {
	let mut prefixes: Vec<(&String, &PlacedRoot)> =
		mapping.prefix_destinations.iter().collect();
	prefixes.sort_by_key(|(prefix, _)| *prefix);
	mapping
		.primary_destination
		.iter()
		.chain(prefixes.into_iter().map(|(_, placed)| placed))
		.flat_map(|placed| {
			names.name_chain(placed.anchor().path(), placed.tail())
		})
		.collect()
}

/// `rest` below a placed folder's tail (`tail/rest`), or `rest` itself when
/// the folder exists.
fn with_tail<'a>(
	tail: &str,
	rest: impl Into<std::borrow::Cow<'a, str>>,
) -> std::borrow::Cow<'a, str> {
	let rest = rest.into();
	if tail.is_empty() {
		rest
	} else {
		std::borrow::Cow::Owned(format!("{tail}/{rest}"))
	}
}

/// The path an entry that no prefix claims has below the primary
/// destination (its tail comes in front of it later): the entry's path,
/// under the restore base's directory when it is relative.
fn primary_relative<'a>(
	mapping: &ImportMapping,
	entry_path: &'a str,
) -> std::borrow::Cow<'a, str> {
	match &mapping.primary_prefix {
		Some(p) if restore::is_relative(entry_path) => {
			std::borrow::Cow::Owned(format!("{p}/{entry_path}"))
		}
		_ => std::borrow::Cow::Borrowed(entry_path),
	}
}

/// The same import engine, consuming entries already parsed with
/// [`format::parse_clipboard`]. Returned ordinals align with create_operations
/// and name each create's original parsed entry, before path routing or skips.
/// Consuming entries lets the worker reuse its parse without retaining a
/// second body table while it serializes a compact preview.
pub fn plan_import_parsed_with(
	entries: Vec<ParsedEntry>,
	destination_roots: &[PathBuf],
	mapping: &ImportMapping,
	opts: &RunOptions,
) -> Result<(TransferImportPlan, Vec<usize>), TransferError> {
	cancelled_err(opts, "plan-import")?;
	let mut canonical_dest_roots = Vec::new();
	for r in destination_roots {
		cancelled_err(opts, "plan-import")?;
		let id = CanonicalRootId::new(r)?;
		id.validate()?;
		canonical_dest_roots.push(id);
	}

	// Every mapped destination root must belong to the explicit destination_roots
	// (a placed folder by its anchor: the folder it names may not exist yet).
	if let Some(ref primary) = mapping.primary_destination {
		cancelled_err(opts, "plan-import")?;
		if !canonical_dest_roots.contains(primary.anchor()) {
			return Err(TransferError::UnknownRoot(
				primary.anchor().path().to_path_buf(),
			));
		}
	}
	for root in mapping.prefix_destinations.values() {
		cancelled_err(opts, "plan-import")?;
		if !canonical_dest_roots.contains(root.anchor()) {
			return Err(TransferError::UnknownRoot(
				root.anchor().path().to_path_buf(),
			));
		}
	}
	for entry_mapping in mapping.entry_destinations.values() {
		cancelled_err(opts, "plan-import")?;
		if !canonical_dest_roots.contains(&entry_mapping.root) {
			return Err(TransferError::UnknownRoot(
				entry_mapping.root.path().to_path_buf(),
			));
		}
	}

	// A placed folder's tail is what did not exist when it was resolved. One
	// layer of it that exists now (made by someone else in between) voids
	// that premise: the anchor is no longer the nearest existing ancestor,
	// and the folder the user named would be told from an incidental one by
	// an identity that no longer holds (two spellings of one new folder on
	// a case-folding filesystem would slip past consent).
	for placed in mapping
		.primary_destination
		.iter()
		.chain(mapping.prefix_destinations.values())
	{
		cancelled_err(opts, "plan-import")?;
		for dir in placed.new_chain() {
			if fs::symlink_metadata(&dir).is_ok() {
				return Err(created_where_a_folder_was_needed_at(
					placed.anchor().path(),
					&dir,
				));
			}
		}
	}

	#[cfg(any(test, feature = "test-support"))]
	test_hooks::run_after_tail_check();

	let mut root_case_insensitive: HashMap<PathBuf, bool> = HashMap::new();
	for root in &canonical_dest_roots {
		cancelled_err(opts, "plan-import")?;
		root_case_insensitive
			.entry(root.path().to_path_buf())
			.or_insert_with(|| fs_is_case_insensitive(root.path()));
	}

	// The folders the user named, each under one spelling. On a filesystem
	// that folds case `new` and `NEW` are one folder, and every place that
	// tells folders apart (what is agreed to, what is recorded as named,
	// what is made) goes by this one: the first spelling met, the named
	// folders first.
	let mut folder_names = NewFolderNames::new(root_case_insensitive.clone());
	let named_paths = named_folder_chains(mapping, &mut folder_names);

	let placed_ids = placed_list(mapping);
	let placed_id = |p: &PlacedRoot| placed_ids.iter().position(|q| *q == p);
	let route_candidates =
		route_candidates(&canonical_dest_roots, mapping, &placed_ids);
	let dest_root_paths: Vec<&Path> =
		route_candidates.iter().map(|c| c.path.as_path()).collect();
	let primary_dest_path: Option<PathBuf> =
		mapping.primary_destination.as_ref().map(PlacedRoot::path);

	let mut planned_entries: Vec<(
		CanonicalRootId,
		ParsedEntry,
		String,
		usize,
		Option<usize>,
	)> = Vec::new();
	let mut skipped_operations = Vec::new();

	for (entry_index, entry) in entries.into_iter().enumerate() {
		cancelled_err(opts, "plan-import")?;
		// Where the entry lands: the anchor, the names below it that a folder
		// still to be made adds, the path the route leaves below them, and,
		// when the route puts a folder in front of that path (Add), the path
		// the entry keeps for the file itself. What the entry keeps is
		// checked on its own before anything is put in front of it, so a path
		// that names no file cannot become the folder it is under.
		// `route` is the placed folder (its place in `placed_ids`) the
		// entry goes through, as routing chooses it here: what its folders
		// need agreed to, apart from how its path reads.
		let (target_root, tail, rel_path, own_path, route) = if let Some(em) =
			mapping.entry_destinations.get(&entry.path)
		{
			let rel =
				em.relative_path.as_deref().unwrap_or(entry.path.as_str());
			(
				&em.root,
				std::borrow::Cow::Borrowed(""),
				std::borrow::Cow::Borrowed(rel),
				None,
				None,
			)
		} else {
			let normalized = paths::normalize_system_path(&entry.path);
			if paths::is_absolute_path(&normalized) {
				if let Some((root_str, rel)) =
					paths::resolve_absolute_import_candidate(
						&dest_root_paths,
						primary_dest_path.as_deref(),
						&entry.path,
					) {
					if rel.is_empty() {
						skipped_operations.push(SkippedOperation {
							raw_path: entry.path.clone(),
							relative_path: None,
							reason: SkipReason::UnresolvedPath,
							detail: None,
						});
						continue;
					}
					let matched_root = route_candidates.iter().find(|c| {
						paths::path_key(&c.path.to_string_lossy())
							== paths::path_key(&root_str)
							|| dunce::canonicalize(Path::new(&root_str))
								.map(|canonical| {
									paths::path_key(
										&canonical.to_string_lossy(),
									) == paths::path_key(
										&c.path.to_string_lossy(),
									)
								})
								.unwrap_or(false)
					});
					match matched_root {
						Some(c) => (
							&c.anchor,
							std::borrow::Cow::Borrowed(c.tail.as_str()),
							std::borrow::Cow::Owned(rel),
							None,
							c.placed,
						),
						None => {
							skipped_operations.push(SkippedOperation {
								raw_path: entry.path.clone(),
								relative_path: None,
								reason: SkipReason::UnresolvedPath,
								detail: None,
							});
							continue;
						}
					}
				} else if let Some((id, rel)) =
					placed_for_absolute(&placed_ids, &normalized)
				{
					let placed = placed_ids[id];
					(
						placed.anchor(),
						std::borrow::Cow::Owned(placed.tail_rel()),
						std::borrow::Cow::Owned(rel),
						None,
						Some(id),
					)
				} else if entry.change_types.contains(&ChangeType::Deleted) {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
						detail: None,
					});
					continue;
				} else if let Some(ref primary) = mapping.primary_destination {
					(
						primary.anchor(),
						std::borrow::Cow::Owned(primary.tail_rel()),
						std::borrow::Cow::Borrowed(entry.path.as_str()),
						None,
						placed_id(primary),
					)
				} else {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
						detail: None,
					});
					continue;
				}
			} else if let Some((prefix, rest)) = entry.path.split_once('/') {
				if mapping.blocked_prefixes.contains(prefix) {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
						detail: None,
					});
					continue;
				}
				if let Some(dest) = mapping.prefix_destinations.get(prefix) {
					// Prefix consumed EXACTLY ONCE
					(
						dest.anchor(),
						std::borrow::Cow::Owned(dest.tail_rel()),
						std::borrow::Cow::Borrowed(rest),
						None,
						placed_id(dest),
					)
				} else if let Some(ref primary) = mapping.primary_destination {
					(
						primary.anchor(),
						std::borrow::Cow::Owned(primary.tail_rel()),
						primary_relative(mapping, &entry.path),
						Some(entry.path.as_str()),
						placed_id(primary),
					)
				} else {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
						detail: None,
					});
					continue;
				}
			} else if let Some(ref primary) = mapping.primary_destination {
				(
					primary.anchor(),
					std::borrow::Cow::Owned(primary.tail_rel()),
					primary_relative(mapping, &entry.path),
					Some(entry.path.as_str()),
					placed_id(primary),
				)
			} else {
				skipped_operations.push(SkippedOperation {
					raw_path: entry.path.clone(),
					relative_path: None,
					reason: SkipReason::UnresolvedPath,
					detail: None,
				});
				continue;
			}
		};

		if paths::has_git_segment(&entry.path)
			|| paths::has_git_segment(&rel_path)
		{
			skipped_operations.push(SkippedOperation {
				raw_path: entry.path.clone(),
				relative_path: None,
				reason: SkipReason::UnresolvedPath,
				detail: Some(restore::SkipDetail::ProtectedGitMetadata),
			});
			continue;
		}

		// What the entry keeps for the file itself, with nothing put in front
		// of it: a name, no `..`. A folder put in front (Add) or a tail
		// would otherwise make a path that names no file (`.`) the folder.
		if sanitize_relative_path(own_path.unwrap_or(&rel_path)).is_none() {
			skipped_operations.push(SkippedOperation {
				raw_path: entry.path.clone(),
				relative_path: None,
				reason: SkipReason::UnresolvedPath,
				detail: None,
			});
			continue;
		}

		// The file's own path: non-empty, no `..`, at least one name. Below
		// a folder to make it is what keeps the file below that folder.
		let Some(file_path) = sanitize_relative_path(&rel_path) else {
			skipped_operations.push(SkippedOperation {
				raw_path: entry.path.clone(),
				relative_path: None,
				reason: SkipReason::UnresolvedPath,
				detail: None,
			});
			continue;
		};
		let sanitized = with_tail(&tail, file_path).into_owned();

		if paths::has_git_segment(&sanitized) {
			skipped_operations.push(SkippedOperation {
				raw_path: entry.path.clone(),
				relative_path: None,
				reason: SkipReason::UnresolvedPath,
				detail: Some(restore::SkipDetail::ProtectedGitMetadata),
			});
			continue;
		}

		planned_entries.push((
			target_root.clone(),
			ParsedEntry {
				path: sanitized,
				content: entry.content,
				change_types: entry.change_types,
			},
			entry.path,
			entry_index,
			route,
		));
	}

	let mut combined_creates = Vec::new();
	let mut create_entry_indices = Vec::new();
	let mut create_routes: Vec<Option<usize>> = Vec::new();
	let mut combined_deletes = Vec::new();
	let mut all_skipped = skipped_operations;

	// Same `plan_restore` as before, one entry at a time so a cancel is
	// observed before the next file's encoding read.
	for (root_id, entry, raw_path, entry_index, route) in planned_entries {
		cancelled_err(opts, "plan-import")?;
		let sub_plan = restore::plan_restore(
			&[root_id.path()],
			std::slice::from_ref(&entry),
		);
		create_entry_indices.extend(std::iter::repeat_n(
			entry_index,
			sub_plan.create_operations.len(),
		));
		create_routes.extend(std::iter::repeat_n(
			route,
			sub_plan.create_operations.len(),
		));
		combined_creates.extend(sub_plan.create_operations);
		combined_deletes.extend(sub_plan.delete_operations);
		all_skipped.extend(sub_plan.skipped_operations.into_iter().map(
			|mut op| {
				op.raw_path.clone_from(&raw_path);
				op
			},
		));
	}

	// Reject two writes to one target file, as early as the preview can
	// tell. After every step that moves a target (routing, the restore
	// base, a folder's tail), so it sees the paths Apply writes, and by the
	// file they are as far as this plan can tell (`TargetId`: symlinks,
	// case aliases, two spellings of one folder). This is a warning that
	// errs towards refusing: Apply meets every pair it misses at
	// `create_new`.
	let mut targets: HashMap<TargetId, String> = HashMap::new();
	for op in &combined_creates {
		cancelled_err(opts, "plan-import")?;
		let current = format!("create {}", op.relative_path);
		if let Some(prev) = targets
			.insert(folder_names.target_id(&op.absolute_path), current.clone())
		{
			return Err(TransferError::TargetCollision {
				path: op.absolute_path.clone(),
				msg: format!("previous was '{prev}', current is '{current}'"),
			});
		}
	}
	for op in &combined_deletes {
		cancelled_err(opts, "plan-import")?;
		let current = format!("delete {}", op.relative_path);
		if let Some(prev) = targets
			.insert(folder_names.target_id(&op.absolute_path), current.clone())
		{
			return Err(TransferError::TargetCollision {
				path: op.absolute_path.clone(),
				msg: format!("previous was '{prev}', current is '{current}'"),
			});
		}
	}
	drop(targets);

	let restore_plan = RestorePlan {
		roots: destination_roots.to_vec(),
		create_operations: combined_creates,
		delete_operations: combined_deletes,
		skipped_operations: all_skipped,
	};

	cancelled_err(opts, "plan-import")?;
	let mut destination_freshness = DestinationFreshnessSnapshot::capture_with(
		destination_roots,
		&restore_plan,
		opts,
	)?;
	// The folders the user named: a tail's names that a create's path needs.
	// Only when there are some, so a plan without a tail is what it was.
	if let Some(missing) = &destination_freshness.missing_dirs {
		let requested = folder_names.requested(missing, &named_paths);
		if !requested.is_empty() {
			destination_freshness.requested_dirs = Some(requested);
		}
	}
	// What Apply holds the plan to, whatever the classification above made
	// of it: the tails as the folders were placed, each by its place, and
	// the place each create's route goes through.
	if !placed_ids.is_empty() {
		destination_freshness.placed =
			Some(placed_ids.iter().map(|p| p.new_chain()).collect());
		// Which of the folders to make each create needs, read here, where
		// the paths are this machine's: a master on another OS cannot.
		let empty = HashSet::new();
		destination_freshness.new_dirs = Some(PlannedNewDirs::of(
			&restore_plan.create_operations,
			destination_freshness
				.missing_dirs
				.as_ref()
				.unwrap_or(&empty),
			create_routes,
		));
	}

	Ok((
		TransferImportPlan {
			roots: destination_roots.to_vec(),
			restore_plan,
			destination_freshness,
		},
		create_entry_indices,
	))
}

/// Validates that a commit selection stays strictly within one repository.
pub fn validate_commit_selection(repos: &[&Git]) -> Result<(), TransferError> {
	if repos.len() > 1 {
		return Err(TransferError::CrossRepoCommitsNotSupported);
	}
	if repos.is_empty() {
		return Err(TransferError::EmptySelection);
	}
	Ok(())
}

fn select_commit_shas(
	git: &Git,
	range: Option<(&str, &str)>,
	last: Option<usize>,
) -> Result<Vec<String>, TransferError> {
	match (range, last) {
		(Some((base, tip)), None) => commits::select_range(git, base, tip)
			.map_err(|e| match e {
				CommitError::Discontinuous {
					base,
					tip,
					at,
					first_parent,
				} => TransferError::DiscontinuousCommits {
					base,
					tip,
					at,
					first_parent,
				},
				other => TransferError::Commit(other),
			}),
		(None, Some(n)) => {
			commits::select_last(git, n).map_err(TransferError::Commit)
		}
		_ => Err(TransferError::EmptySelection),
	}
}

/// Plans and extracts commits along a contiguous first-parent chain.
pub fn plan_commit_export(
	git: &Git,
	range: Option<(&str, &str)>,
	last: Option<usize>,
) -> Result<CommitsPayload, TransferError> {
	let shas = select_commit_shas(git, range, last)?;
	commits::copy_commits(git, &shas).map_err(TransferError::Commit)
}

/// [`plan_commit_export`] with the caller's runner options and a hard cap on
/// the clipboard document.
///
/// Over cap => [`CommitError::PayloadLimit`] (never truncate).
pub fn plan_commit_export_with(
	git: &Git,
	range: Option<(&str, &str)>,
	last: Option<usize>,
	opts: &RunOptions,
	max_serialized_bytes: usize,
) -> Result<CommitExport, TransferError> {
	let shas = select_commit_shas(git, range, last)?;
	commits::copy_commits_with(git, &shas, opts, max_serialized_bytes)
		.map_err(TransferError::Commit)
}

/// Exports exactly the commits in `selected`, which must be the contiguous
/// first-parent chain that ends at `tip`. The selected tip is preserved when
/// the chain includes a root; HEAD is never substituted.
pub fn plan_commit_export_exact(
	git: &Git,
	tip: &str,
	selected: &[String],
) -> Result<CommitsPayload, TransferError> {
	let chain =
		select_exact_chain(git, tip, selected, &RunOptions::default(), None)?;
	commits::copy_commits(git, &chain).map_err(TransferError::Commit)
}

/// [`plan_commit_export_exact`] with the caller's runner options and a hard
/// cap on the clipboard document.
///
/// Every ref resolution uses `opts`. A truncated id is
/// [`GitError::OutputLimit`], not a short OID. Cancellation is polled across
/// the walk and after the chain is known, including a root that has no blob
/// to read. `max_serialized_bytes` is the whole document
/// [`commits::copy_commits_with`] counts. A selection that cannot fit is
/// [`CommitError::PayloadLimit`]; commits are not dropped to succeed.
/// [`plan_commit_export_exact`] stays uncapped.
pub fn plan_commit_export_exact_with(
	git: &Git,
	tip: &str,
	selected: &[String],
	opts: &RunOptions,
	max_serialized_bytes: usize,
) -> Result<CommitExport, TransferError> {
	let chain = select_exact_chain(
		git,
		tip,
		selected,
		opts,
		Some(max_serialized_bytes),
	)?;
	commits::copy_commits_with(git, &chain, opts, max_serialized_bytes)
		.map_err(TransferError::Commit)
}

/// First-parent chain of `selected` ending at `tip`, oldest first.
///
/// Each selected entry is resolved with `opts` and is not resolved again on
/// the parent walk. A repeated `tip` string is resolved once. The retained
/// OID set is not reserved to `selected.len()`.
fn select_exact_chain(
	git: &Git,
	tip: &str,
	selected: &[String],
	opts: &RunOptions,
	limit: Option<usize>,
) -> Result<Vec<String>, TransferError> {
	cancelled_err(opts, "select-commits")?;
	if selected.is_empty() {
		return Err(TransferError::EmptySelection);
	}
	if let Some(max) = limit {
		let floor = commits::min_commit_document_len(1);
		if floor > max {
			return Err(CommitError::PayloadLimit {
				limit: max,
				actual: floor,
			}
			.into());
		}
	}

	let mut wanted = HashSet::new();
	let mut resolved_tip: Option<String> = None;
	for sha in selected {
		cancelled_err(opts, "select-commits")?;
		let oid = if sha == tip {
			if let Some(oid) = resolved_tip.clone() {
				oid
			} else {
				let oid = git.resolve_commit_with(sha, opts)?;
				resolved_tip = Some(oid.clone());
				oid
			}
		} else {
			git.resolve_commit_with(sha, opts)?
		};
		if wanted.contains(&oid) {
			continue;
		}
		if let Some(max) = limit {
			let floor = commits::min_commit_document_len(wanted.len() + 1);
			if floor > max {
				return Err(CommitError::PayloadLimit {
					limit: max,
					actual: floor,
				}
				.into());
			}
		}
		wanted.insert(oid);
	}
	cancelled_err(opts, "select-commits")?;
	let tip_oid = match resolved_tip {
		Some(oid) => oid,
		None => git.resolve_commit_with(tip, opts)?,
	};
	if !wanted.contains(&tip_oid) {
		return Err(TransferError::DiscontinuousCommits {
			base: selected.first().cloned().unwrap_or_default(),
			tip: tip_oid,
			at: tip.to_string(),
			first_parent: None,
		});
	}

	let mut chain = Vec::new();
	let mut cursor = tip_oid.clone();
	loop {
		cancelled_err(opts, "select-commits")?;
		if !wanted.remove(&cursor) {
			cancelled_err(opts, "select-commits")?;
			let parent = git.parents_with(&cursor, opts)?.into_iter().next();
			return Err(TransferError::DiscontinuousCommits {
				base: wanted.iter().next().cloned().unwrap_or_default(),
				tip: tip_oid,
				at: cursor,
				first_parent: parent,
			});
		}
		chain.push(cursor.clone());
		if wanted.is_empty() {
			break;
		}
		cancelled_err(opts, "select-commits")?;
		match git.parents_with(&cursor, opts)?.into_iter().next() {
			Some(parent) => cursor = parent,
			None => {
				return Err(TransferError::DiscontinuousCommits {
					base: wanted.iter().next().cloned().unwrap_or_default(),
					tip: tip_oid,
					at: chain.last().cloned().unwrap_or_default(),
					first_parent: None,
				});
			}
		}
	}
	// Chain is complete, including a root. No blob is read on this path.
	cancelled_err(opts, "select-commits")?;
	chain.reverse();
	Ok(chain)
}

/// Preview of replaying one commit payload onto `dest`, with destination
/// HEAD, ref, index, content, and absence captured before any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReplayPreview {
	destination: PathBuf,
	/// The write scope: the folder the user pasted into, at most the
	/// repository root. Targets outside it are refused, so a whole-repo
	/// replay requires opening the repo root.
	scope: PathBuf,
	payload: CommitsPayload,
	replay: commits::CommitReplayPlan,
	freshness: DestinationFreshnessSnapshot,
}

impl CommitReplayPreview {
	/// Compare one row against the destination after the preceding commits.
	/// Only original destination text needs I/O; the engine resolves prior
	/// payload contents and absence itself. Readers must honor `opts` and the
	/// supplied `limit`, as [`read_replay_preview_with`] does.
	pub fn file_preview_with<E: From<TransferError>>(
		&self,
		commit_index: usize,
		file_index: usize,
		side: ReplayFileSide,
		limit: usize,
		opts: &RunOptions,
		mut read: impl FnMut(&Path, &RunOptions) -> Result<ReplayPreviewRead, E>,
	) -> Result<ReplayFilePreview, E> {
		use commits::{ReplayAction, ReplayContentSource};
		cancelled_err(opts, "replay-file-preview")?;
		let invalid = || {
			TransferError::Commit(CommitError::InvalidPayload(
				commits::PayloadFault::preview_row(),
			))
		};
		let file = self
			.replay
			.commits
			.get(commit_index)
			.and_then(|c| c.files.get(file_index))
			.ok_or_else(invalid)?;
		let incoming = self
			.payload
			.commits
			.get(commit_index)
			.and_then(|c| c.files.get(file_index))
			.ok_or_else(invalid)?;
		let (path, absolute, source, after) = match side {
			ReplayFileSide::Target => (
				file.path.as_str(),
				file.absolute_path.as_deref(),
				file.before,
				if file.action == ReplayAction::Write {
					incoming.content.as_deref().unwrap_or("")
				} else {
					""
				},
			),
			ReplayFileSide::RenamedFrom => (
				file.old_path.as_deref().ok_or_else(invalid)?,
				file.old_absolute_path.as_deref(),
				file.old_before,
				"",
			),
		};
		let mut preview = ReplayFilePreview {
			path: path.into(),
			before: String::new(),
			after: String::new(),
			unavailable: None,
		};
		if file.action == ReplayAction::Skip
			|| file.layout_conflict.is_some()
			|| self.replay.commits[..=commit_index]
				.iter()
				.any(|c| c.refused_by().is_some())
		{
			preview.unavailable = Some(ReplayPreviewUnavailable::Skipped);
			return Ok(preview);
		}
		let limit = limit.min(crate::browser::PREVIEW_LIMIT);
		if after.len() > limit {
			preview.unavailable =
				Some(ReplayPreviewUnavailable::ContentTooLarge);
			return Ok(preview);
		}
		preview.after = after.into();
		let before = match source {
			ReplayContentSource::Absent => {
				ReplayPreviewRead::Text(String::new())
			}
			ReplayContentSource::Unavailable(why) => {
				ReplayPreviewRead::Unavailable(why)
			}
			ReplayContentSource::Payload {
				commit_index: c,
				file_index: f,
			} => {
				if c >= commit_index {
					return Err(invalid().into());
				}
				let text = self
					.payload
					.commits
					.get(c)
					.and_then(|c| c.files.get(f))
					.and_then(|f| f.content.as_deref())
					.ok_or_else(invalid)?;
				if text.len() > limit {
					ReplayPreviewRead::Unavailable(
						ReplayPreviewUnavailable::ContentTooLarge,
					)
				} else {
					ReplayPreviewRead::Text(text.into())
				}
			}
			ReplayContentSource::Destination => {
				let absolute = absolute.ok_or_else(invalid)?;
				let expected = self
					.freshness
					.target_files
					.get(absolute)
					.and_then(|t| t.file_state.as_ref())
					.ok_or_else(invalid)?;
				let value = read(absolute, opts)?;
				cancelled_err(opts, "replay-file-preview")?;
				if let ReplayPreviewRead::Text(text) = &value {
					let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
					if text.len() as u64 != expected.size
						|| hash != expected.content_hash
					{
						return Err(TransferError::StaleDestination {
							root: self.destination.clone(), reason: format!("target file '{path}' was modified externally after preview"),
							change: StaleChange::Modified { rel: path.to_string() },
						}.into());
					}
				}
				value
			}
		};
		match before {
			ReplayPreviewRead::Text(text) if text.len() <= limit => {
				preview.before = text
			}
			ReplayPreviewRead::Text(_) => {
				preview.unavailable = Some(ReplayPreviewUnavailable::TooLarge)
			}
			ReplayPreviewRead::Unavailable(why) => {
				preview.unavailable = Some(why)
			}
		}
		cancelled_err(opts, "replay-file-preview")?;
		Ok(preview)
	}

	pub fn destination(&self) -> &Path {
		&self.destination
	}

	/// The replay's write scope ([`Self::capture_in`]).
	pub fn scope(&self) -> &Path {
		&self.scope
	}

	pub fn payload(&self) -> &CommitsPayload {
		&self.payload
	}

	pub fn plan(&self) -> &commits::CommitReplayPlan {
		&self.replay
	}

	pub fn freshness(&self) -> &DestinationFreshnessSnapshot {
		&self.freshness
	}

	/// Owned buffer capacities, excluding this inline struct and allocator
	/// bookkeeping. The conservative table estimate assumes the insertion-only
	/// construction in capture_replay_freshness, which the private fields guarantee.
	pub fn retained_heap_bytes(&self) -> usize {
		self.destination
			.capacity()
			.saturating_add(self.scope.capacity())
			.saturating_add(self.payload.retained_heap_bytes())
			.saturating_add(self.replay.retained_heap_bytes())
			.saturating_add(self.freshness.fresh_capture_heap_bytes())
	}

	pub fn capture(
		dest: &Path,
		payload: &CommitsPayload,
	) -> Result<Self, TransferError> {
		Self::capture_with(dest, payload, &RunOptions::default())
	}

	/// [`Self::capture`] with the caller's runner options. The write scope
	/// is the repository root: the destination's whole repository may be
	/// replayed. Nothing is written.
	pub fn capture_with(
		dest: &Path,
		payload: &CommitsPayload,
		opts: &RunOptions,
	) -> Result<Self, TransferError> {
		Self::capture_in(dest, dest, payload, opts)
	}

	/// [`Self::capture_with`] with the replay's write scope: only
	/// `scope`'s targets may be written or deleted. Both front ends pass
	/// the folder the user pasted into, so a subfolder paste refuses
	/// targets outside it; a whole-repository replay passes the
	/// repository root.
	///
	/// Opens the destination with `Git::open_with`. Eligibility planning polls
	/// `opts` between files, then HEAD, the symbolic ref, the index and each
	/// target hash use the same options. Nothing is written.
	pub fn capture_in(
		dest: &Path,
		scope: &Path,
		payload: &CommitsPayload,
		opts: &RunOptions,
	) -> Result<Self, TransferError> {
		cancelled_err(opts, "replay-preview")?;
		let git = Git::open_with(dest, opts)?;
		let root = git.root().to_path_buf();
		let replay = replay_plan_in(&git, scope, payload, opts)?;
		let freshness = capture_replay_freshness(&root, &replay, opts)?;
		// Plan first, then the snapshot, then plan again. A change between
		// those reads makes the preview unusable instead of storing a mix.
		cancelled_err(opts, "replay-preview")?;
		let again = replay_plan_in(&git, scope, payload, opts)?;
		if again != replay {
			return Err(TransferError::StaleDestination {
				root: root.clone(),
				reason:
					"destination changed while the replay preview was captured"
						.into(),
				change: StaleChange::Preview,
			});
		}
		revalidate_replay_freshness(&freshness, opts)?;
		let preview = Self {
			destination: root,
			scope: scope.to_path_buf(),
			payload: payload.clone(),
			replay,
			freshness,
		};
		#[cfg(test)]
		final_boundary_cancel_hook("replay-preview-capture");
		cancelled_err(opts, "replay-preview")?;
		Ok(preview)
	}

	/// A preview captured elsewhere (a remote worker) put back together with
	/// its payload. Nothing here is trusted: [`Self::apply_with`] and
	/// [`Self::revalidate_with`] re-plan the payload and re-read every
	/// recorded path before anything is written. The scope is the
	/// destination; callers that captured with a narrower scope restate it
	/// with [`Self::from_parts_scoped`].
	pub fn from_parts(
		destination: PathBuf,
		payload: CommitsPayload,
		replay: commits::CommitReplayPlan,
		freshness: DestinationFreshnessSnapshot,
	) -> Self {
		let scope = destination.clone();
		Self::from_parts_scoped(destination, scope, payload, replay, freshness)
	}

	/// [`Self::from_parts`] with the replay's write scope restored.
	pub fn from_parts_scoped(
		destination: PathBuf,
		scope: PathBuf,
		payload: CommitsPayload,
		replay: commits::CommitReplayPlan,
		freshness: DestinationFreshnessSnapshot,
	) -> Self {
		Self {
			destination,
			scope,
			payload,
			replay,
			freshness,
		}
	}

	pub fn apply(&self) -> Result<commits::ReplayResult, TransferError> {
		self.apply_with(&RunOptions::default())
	}

	/// Replays this preview's own payload onto its destination, or refuses as stale. `opts` reaches
	/// Git open and the re-validation under the worktree lock; once the first commit starts
	/// nothing is cancelled. Cancel at any point before the first write is
	/// `Err(TransferError::Git(GitError::Cancelled))`. A lock another paste holds is
	/// `Err(TransferError::Git(GitError::WorktreeBusy))`, refused at once. Any other failure
	/// to start (`RepoIdentity::resolve`, the lock file, creating the empty hooks directory) is
	/// `Ok(ReplayResult)` with failure at index 0 and nothing written; `Git::open_with` and
	/// re-validation errors, `QueueTimeout` included, are `Err`.
	/// Overwrites follow spec 4.3 (直接覆蓋): the desktop's "allow overwrite first" gate is that
	/// session's rule and is checked before calling this.
	pub fn apply_with(
		&self,
		opts: &RunOptions,
	) -> Result<commits::ReplayResult, TransferError> {
		cancelled_err(opts, "replay-apply")?;
		let git = Git::open_with(&self.destination, opts)?;
		let target_dirs = self.replay.commits.iter().flat_map(|commit| {
			commit
				.files
				.iter()
				.filter(|file| file.action != commits::ReplayAction::Skip)
				.flat_map(|file| {
					file.absolute_path
						.iter()
						.chain(file.old_absolute_path.iter())
				})
				.filter_map(|path| path.parent())
		});
		// The replay also writes HEAD/index, even for an empty commit. Take
		// that repo and every file destination in one sorted, deduplicated set.
		let directories = std::iter::once(git.root()).chain(target_dirs);
		let guards = match lock_destination_repos(directories, opts) {
			Ok(g) => g,
			Err(e @ TransferError::Git(GitError::WorktreeBusy { .. })) => {
				return Err(e)
			}
			Err(e) => {
				cancelled_err(opts, "replay-apply")?;
				return Ok(commits::ReplaySession::refused(
					&self.payload,
					e.to_string(),
					e.reason(),
				));
			}
		};
		let session = match commits::ReplaySession::begin_in(
			&self.scope,
			&self.payload,
			guards,
		) {
			Ok(s) => s,
			Err(refused) => {
				cancelled_err(opts, "replay-apply")?;
				return Ok(*refused);
			}
		};
		self.revalidate_with(opts)?; // under the heavy lock, right before the first write
		cancelled_err(opts, "replay-apply")?;
		Ok(session.run(&git, &self.payload))
	}

	/// Refuses when HEAD, the branch ref, the index, recorded bytes, absence,
	/// or replay eligibility changed since [`Self::capture`]. A skipped
	/// non-UTF-8 or unsafe path that becomes writable is stale. `NotCopied`
	/// stays skipped because the payload itself has no bytes to write.
	///
	/// Checking freshness does not write. Confirmed replay is performed by
	/// [`Self::apply`] or [`Self::apply_with`] and is not cancelled here.
	pub fn revalidate(&self) -> Result<(), TransferError> {
		self.revalidate_with(&RunOptions::default())
	}

	/// [`Self::revalidate`] with the caller's runner options. Read-only.
	pub fn revalidate_with(
		&self,
		opts: &RunOptions,
	) -> Result<(), TransferError> {
		cancelled_err(opts, "replay-preview")?;
		revalidate_replay_freshness(&self.freshness, opts)?;
		let git = Git::open_with(&self.destination, opts)?;
		let now = replay_plan_in(&git, &self.scope, &self.payload, opts)?;
		if !now.same_operations(&self.replay) {
			return Err(TransferError::StaleDestination {
				root: self.destination.clone(),
				reason: "replay eligibility changed after preview".into(),
				change: StaleChange::Preview,
			});
		}
		#[cfg(test)]
		final_boundary_cancel_hook("replay-preview-revalidate");
		cancelled_err(opts, "replay-preview")?;
		Ok(())
	}
}

fn replay_plan_in(
	git: &Git,
	scope: &Path,
	payload: &CommitsPayload,
	opts: &RunOptions,
) -> Result<commits::CommitReplayPlan, TransferError> {
	commits::plan_commit_replay_in(git, scope, payload, opts).map_err(|err| {
		match err {
			CommitError::Git(git_err) => TransferError::Git(git_err),
			other => TransferError::Commit(other),
		}
	})
}

/// Repo freshness plus every path the replay plan named. Symlinks are hashed
/// as the link itself; their targets are not opened.
fn capture_replay_freshness(
	root: &Path,
	replay: &commits::CommitReplayPlan,
	opts: &RunOptions,
) -> Result<DestinationFreshnessSnapshot, TransferError> {
	cancelled_err(opts, "replay-freshness")?;
	// Fresh tables, insertion only: preserve this creation invariant for
	// fresh_capture_heap_bytes. Do not reuse a previously edited snapshot.
	let mut roots = HashMap::new();
	let id = CanonicalRootId::new(root)?;
	id.validate()?;
	roots.insert(id, capture_repo_freshness(root, opts)?);

	let mut target_files = HashMap::new();
	for commit in &replay.commits {
		cancelled_err(opts, "replay-freshness")?;
		for file in &commit.files {
			cancelled_err(opts, "replay-freshness")?;
			for (abs, rel) in file.freshness_targets() {
				if target_files.contains_key(abs) {
					continue;
				}
				let file_state = capture_replay_file_freshness(abs, opts)?;
				target_files.insert(
					abs.clone(),
					TargetFileFreshness {
						root: CanonicalRootId::new(root)?,
						relative_path: rel.to_string(),
						existed: file_state.is_some(),
						file_state,
					},
				);
			}
		}
	}
	Ok(DestinationFreshnessSnapshot {
		roots,
		target_files,
		repos: None,
		missing_dirs: None,
		symlinked_dirs: None,
		requested_dirs: None,
		placed: None,
		new_dirs: None,
	})
}

/// Identity of a replay target. A symlink contributes its own link text, not
/// the bytes reached by opening it.
fn capture_replay_file_freshness(
	path: &Path,
	opts: &RunOptions,
) -> Result<Option<FileFreshness>, TransferError> {
	cancelled_err(opts, "replay-freshness")?;
	// A file standing in for a parent directory (ENOTDIR on Unix, NotFound on
	// Windows) is absent, like `commits::delete`; the planner already turns it
	// into an unsafe-path skip.
	let meta = match fs::symlink_metadata(path) {
		Ok(meta) => meta,
		Err(e)
			if matches!(
				e.kind(),
				io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
			) =>
		{
			return Ok(None);
		}
		Err(e) => return Err(TransferError::Io(e)),
	};
	if meta.file_type().is_symlink() {
		// Link text only. `read_link` does not open the target.
		let link = fs::read_link(path)?;
		let bytes = link.as_os_str().as_encoded_bytes();
		let mut hasher = Sha256::new();
		hasher.update(bytes);
		cancelled_err(opts, "replay-freshness")?;
		return Ok(Some(FileFreshness {
			size: bytes.len() as u64,
			mtime: meta.modified()?,
			content_hash: hasher.finalize().into(),
		}));
	}
	// A directory at a replay target is not a file to hash. The planner either
	// refuses the commit for it or, when an earlier commit of the batch
	// empties it, replays over it, so it counts as absent here: a file that
	// appears in its place still changes the state, and the plan comparison
	// notices any layout change.
	if meta.file_type().is_dir() {
		return Ok(None);
	}
	if !meta.file_type().is_file() {
		return Err(TransferError::DestinationNotRegular(
			path.to_string_lossy().into_owned(),
		));
	}
	Ok(Some(FileFreshness {
		size: meta.len(),
		mtime: meta.modified()?,
		content_hash: hash_file(path, opts)?,
	}))
}

/// Same identity [`capture_replay_file_freshness`] stored. File-import
/// revalidation follows symlinks; replay must not.
fn revalidate_replay_freshness(
	snapshot: &DestinationFreshnessSnapshot,
	opts: &RunOptions,
) -> Result<(), TransferError> {
	for (root, prev_repo) in &snapshot.roots {
		revalidate_destination_repo_freshness(root.path(), prev_repo, opts)?;
	}
	for (path, target) in &snapshot.target_files {
		let current = capture_replay_file_freshness(path, opts)?;
		let existed = current.is_some();
		if existed == target.existed && current == target.file_state {
			continue;
		}
		let rel = target.relative_path.to_string();
		let (what, change) = if !target.existed && existed {
			("created", StaleChange::Created { rel })
		} else if target.existed && !existed {
			("deleted", StaleChange::Deleted { rel })
		} else {
			("modified", StaleChange::Modified { rel })
		};
		return Err(TransferError::StaleDestination {
			root: target.root.path().to_path_buf(),
			reason: format!(
				"target file '{}' was {what} externally after preview",
				target.relative_path
			),
			change,
		});
	}
	Ok(())
}

#[cfg(test)]
mod chunk_loop {
	use std::io::{self, Read};

	use super::{
		read_limited, CancelToken, GitError, RunOptions, TransferError,
		FILE_IO_CHUNK,
	};

	/// Records the slice length of every `read` and can cancel after one.
	struct Scripted {
		data: Vec<u8>,
		pos: usize,
		requests: Vec<usize>,
		cancel_after: Option<(usize, CancelToken)>,
	}

	impl Read for Scripted {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			self.requests.push(buf.len());
			let n = buf.len().min(self.data.len() - self.pos);
			if n == 0 {
				return Ok(0);
			}
			buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
			self.pos += n;
			if let Some((after, token)) = &self.cancel_after {
				if self.requests.len() == *after {
					token.cancel();
				}
			}
			Ok(n)
		}
	}

	#[test]
	fn capped_read_requests_only_the_remaining_bytes() {
		let mut reader = Scripted {
			data: vec![b'x'; 100],
			pos: 0,
			requests: Vec::new(),
			cancel_after: None,
		};
		let mut got = Vec::new();
		let n = read_limited(
			&mut reader,
			Some(5),
			&RunOptions::default(),
			"read-file",
			|chunk| got.extend_from_slice(chunk),
		)
		.unwrap();
		assert_eq!(n, 5);
		assert_eq!(got, vec![b'x'; 5]);
		assert_eq!(reader.requests, vec![5]);
		assert_eq!(reader.pos, 5);
	}

	#[test]
	fn zero_cap_does_not_call_read() {
		let mut reader = Scripted {
			data: vec![b'x'; 8],
			pos: 0,
			requests: Vec::new(),
			cancel_after: None,
		};
		let n = read_limited(
			&mut reader,
			Some(0),
			&RunOptions::default(),
			"read-file",
			|_| panic!("zero cap delivered a chunk"),
		)
		.unwrap();
		assert_eq!(n, 0);
		assert!(reader.requests.is_empty());
	}

	#[test]
	fn cancel_after_a_chunk_does_not_read_the_next_one() {
		let token = CancelToken::new();
		let mut reader = Scripted {
			data: vec![b'y'; FILE_IO_CHUNK + 32],
			pos: 0,
			requests: Vec::new(),
			cancel_after: Some((1, token.clone())),
		};
		let err = read_limited(
			&mut reader,
			None,
			&RunOptions {
				cancel: Some(token),
				..RunOptions::default()
			},
			"hash-file",
			|_| {},
		)
		.unwrap_err();
		assert!(matches!(
			err,
			TransferError::Git(GitError::Cancelled { .. })
		));
		assert_eq!(reader.requests, vec![FILE_IO_CHUNK]);
		assert_eq!(reader.pos, FILE_IO_CHUNK);
	}
}

/// Real `CommitReplayPreview::capture_with` / `plan_import_with` paths.
/// The probe sits in production `hash_file`, so a chunk is evidence that
/// `read_limited` already copied bytes from that path.
#[cfg(test)]
mod preview_target_hash_cancel {
	use std::fs;
	use std::process::Command;

	use super::{
		arm_final_boundary_cancel, arm_hash_probe, hash_probe_notes,
		CancelToken, CanonicalRootId, CommitReplayPreview, GitError,
		ImportMapping, RunOptions, TransferError, FILE_IO_CHUNK,
	};
	use crate::commits::{
		CommitFile, CommitRecord, CommitsPayload, FileChange,
	};

	struct Repo {
		_dir: tempfile::TempDir,
		path: std::path::PathBuf,
		cfg: std::path::PathBuf,
	}

	impl Repo {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			let cfg = dir.path().join("empty.gitconfig");
			fs::write(&cfg, b"").unwrap();
			let raw = dir.path().join("repo");
			fs::create_dir(&raw).unwrap();
			let path = dunce::canonicalize(&raw).unwrap();
			let repo = Self {
				_dir: dir,
				path,
				cfg,
			};
			repo.git(&["init", "-q", "-b", "main"]);
			repo.git(&["config", "user.name", "Test User"]);
			repo.git(&["config", "user.email", "test@example.com"]);
			repo.git(&["config", "commit.gpgsign", "false"]);
			repo
		}

		fn git(&self, args: &[&str]) -> String {
			let out = Command::new("git")
				.args(args)
				.current_dir(&self.path)
				.env("GIT_CONFIG_GLOBAL", &self.cfg)
				.env("GIT_CONFIG_NOSYSTEM", "1")
				.env("LC_ALL", "C")
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			String::from_utf8_lossy(&out.stdout).trim().to_string()
		}

		fn write(&self, name: &str, bytes: &[u8]) {
			fs::write(self.path.join(name), bytes).unwrap();
		}
	}

	struct Disk {
		head: String,
		branch: String,
		index: Vec<u8>,
		first: Vec<u8>,
		second: Vec<u8>,
	}

	fn disk(repo: &Repo, first: &str, second: &str) -> Disk {
		Disk {
			head: repo.git(&["rev-parse", "HEAD"]),
			branch: repo.git(&["symbolic-ref", "HEAD"]),
			index: fs::read(repo.path.join(".git/index")).unwrap(),
			first: fs::read(repo.path.join(first)).unwrap(),
			second: fs::read(repo.path.join(second)).unwrap(),
		}
	}

	fn assert_same(before: &Disk, after: &Disk) {
		assert_eq!(after.head, before.head);
		assert_eq!(after.branch, before.branch);
		assert_eq!(after.index, before.index);
		assert_eq!(after.first, before.first);
		assert_eq!(after.second, before.second);
	}

	fn replay_record(files: Vec<CommitFile>) -> CommitRecord {
		CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files,
		}
	}

	#[test]
	fn mapped_protected_skip_keeps_the_original_payload_path_and_counts_once() {
		let repo = Repo::new();
		let id = CanonicalRootId::new(&repo.path).unwrap();
		let mut mapping = ImportMapping::with_primary(id.clone());
		mapping.map_prefix("target", id);
		repo.write("binary", &[0xff]);
		let plan = super::plan_import_with("// FILE: target/.git/hooks/pre-commit\nowned\n// FILE: target/binary\nreplacement\n", "", std::slice::from_ref(&repo.path), &mapping, &RunOptions::default()).unwrap();
		assert_eq!(plan.skipped_operations().len(), 2);
		let skipped = &plan.skipped_operations()[0];
		assert_eq!(skipped.raw_path, "target/.git/hooks/pre-commit");
		assert_eq!(skipped.reason, crate::restore::SkipReason::UnresolvedPath);
		assert_eq!(
			skipped.detail,
			Some(crate::restore::SkipDetail::ProtectedGitMetadata)
		);
		assert_eq!(plan.skipped_operations()[1].raw_path, "target/binary");
		let result = plan
			.apply(&crate::restore::RestoreSelection::default())
			.unwrap();
		assert_eq!(result.total_skipped(), 2);
		assert_eq!(result.planned_skipped_count, 2);
		assert!(!repo.path.join(".git/hooks/pre-commit").exists());
		assert_eq!(fs::read(repo.path.join("binary")).unwrap(), [0xff]);
	}

	fn replay_file(path: &str, content: Option<&str>) -> CommitFile {
		CommitFile {
			path: path.into(),
			old_path: None,
			change: if content.is_some() {
				FileChange::Modified
			} else {
				FileChange::Deleted
			},
			content: content.map(str::to_string),
			not_copied: None,
		}
	}

	fn row_preview(
		preview: &CommitReplayPreview,
		c: usize,
		f: usize,
		side: super::ReplayFileSide,
	) -> super::ReplayFilePreview {
		preview
			.file_preview_with(
				c,
				f,
				side,
				crate::browser::PREVIEW_LIMIT,
				&RunOptions::default(),
				|path, opts| {
					super::read_replay_preview_with(
						path,
						crate::browser::PREVIEW_LIMIT,
						opts,
					)
				},
			)
			.unwrap()
	}

	#[test]
	fn commit_row_preview_uses_the_preceding_commit_without_writing() {
		use super::ReplayFileSide::Target;
		let repo = Repo::new();
		repo.write("common.txt", b"already\n");
		repo.write("untouched.txt", b"keep\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let before = disk(&repo, "common.txt", "untouched.txt");
		let payload = CommitsPayload {
			commits: vec![
				replay_record(vec![replay_file("common.txt", Some("c1\n"))]),
				replay_record(vec![replay_file("common.txt", Some("c2\n"))]),
			],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		let first = row_preview(&preview, 0, 0, Target);
		let second = row_preview(&preview, 1, 0, Target);
		assert_eq!(
			(first.before.as_str(), first.after.as_str()),
			("already\n", "c1\n")
		);
		assert_eq!(
			(second.before.as_str(), second.after.as_str()),
			("c1\n", "c2\n")
		);
		assert!(first.unavailable.is_none() && second.unavailable.is_none());
		assert!(preview.plan().commits.iter().all(|c| c.files[0].existed));
		assert_same(&before, &disk(&repo, "common.txt", "untouched.txt"));
	}

	#[test]
	fn commit_row_preview_tracks_rename_deletion_and_skipped_content() {
		use super::ReplayFileSide::{RenamedFrom, Target};
		let repo = Repo::new();
		repo.write("old.txt", b"old\n");
		repo.write("kept.txt", b"kept\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let before = disk(&repo, "old.txt", "kept.txt");
		let mut rename = replay_file("moved.txt", Some("second\n"));
		rename.old_path = Some("new.txt".into());
		rename.change = FileChange::Renamed;
		let mut skipped = replay_file("kept.txt", None);
		skipped.not_copied = Some(crate::commits::NotCopiedReason::Binary);
		let payload = CommitsPayload {
			commits: vec![
				replay_record(vec![replay_file("new.txt", Some("first\n"))]),
				replay_record(vec![
					rename,
					replay_file("old.txt", None),
					skipped,
				]),
				replay_record(vec![
					replay_file("moved.txt", Some("third\n")),
					replay_file("old.txt", Some("again\n")),
					replay_file("kept.txt", Some("changed\n")),
				]),
			],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		let cases = [
			(0, 0, Target, "", "first\n"),
			(1, 0, RenamedFrom, "first\n", ""),
			(1, 0, Target, "", "second\n"),
			(1, 1, Target, "old\n", ""),
			(2, 0, Target, "second\n", "third\n"),
			(2, 1, Target, "", "again\n"),
			(2, 2, Target, "kept\n", "changed\n"),
		];
		for (c, f, side, old, new) in cases {
			let row = row_preview(&preview, c, f, side);
			assert_eq!(row.unavailable, None, "{c}/{f}/{side:?}");
			assert_eq!(
				(row.before.as_str(), row.after.as_str()),
				(old, new),
				"{c}/{f}/{side:?}"
			);
		}
		assert_eq!(
			row_preview(&preview, 1, 2, Target).unavailable,
			Some(super::ReplayPreviewUnavailable::Skipped)
		);
		assert_same(&before, &disk(&repo, "old.txt", "kept.txt"));
		assert!(!repo.path.join("new.txt").exists());
		assert!(!repo.path.join("moved.txt").exists());
		let applied = preview.apply().unwrap();
		assert!(applied.failure.is_none(), "{applied:?}");
		assert_eq!(applied.created.len(), 3);
		assert_eq!(fs::read(repo.path.join("moved.txt")).unwrap(), b"third\n");
		assert!(!repo.path.join("new.txt").exists());
	}

	#[test]
	fn commit_row_preview_applies_all_rename_deletions_before_writes() {
		use super::ReplayFileSide::{RenamedFrom, Target};
		let repo = Repo::new();
		repo.write("a", b"a\n");
		repo.write("b", b"b\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let rename = |old: &str, new: &str, text: &str| CommitFile {
			old_path: Some(old.into()),
			change: FileChange::Renamed,
			..replay_file(new, Some(text))
		};
		let payload = CommitsPayload {
			commits: vec![
				replay_record(vec![
					rename("a", "b", "from-a\n"),
					rename("b", "c", "from-b\n"),
				]),
				replay_record(vec![
					replay_file("b", Some("next-b\n")),
					replay_file("c", Some("next-c\n")),
				]),
			],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		assert_eq!(row_preview(&preview, 0, 1, RenamedFrom).before, "b\n");
		assert_eq!(row_preview(&preview, 1, 0, Target).before, "from-a\n");
		assert_eq!(row_preview(&preview, 1, 1, Target).before, "from-b\n");
	}

	#[test]
	fn commit_row_preview_file_directory_transitions_have_absent_file_baselines(
	) {
		use super::ReplayFileSide::Target;
		for file_to_directory in [true, false] {
			let repo = Repo::new();
			let (removed, added) = if file_to_directory {
				repo.write("a", b"old file\n");
				("a", "a/b")
			} else {
				fs::create_dir(repo.path.join("a")).unwrap();
				repo.write("a/b", b"old child\n");
				("a/b", "a")
			};
			repo.git(&["add", "-A"]);
			repo.git(&["commit", "-q", "-m", "base"]);
			let before_head = repo.git(&["rev-parse", "HEAD"]);
			let before_index = fs::read(repo.path.join(".git/index")).unwrap();
			let before_file = fs::read(repo.path.join(removed)).unwrap();
			let payload = CommitsPayload {
				commits: vec![
					replay_record(vec![
						replay_file(removed, None),
						replay_file(added, Some("new\n")),
					]),
					replay_record(vec![replay_file(added, Some("next\n"))]),
				],
			};
			let preview =
				CommitReplayPreview::capture(&repo.path, &payload).unwrap();
			let row = row_preview(&preview, 0, 1, Target);
			assert_eq!(
				row.unavailable, None,
				"file_to_directory={file_to_directory}"
			);
			assert_eq!(
				(row.before.as_str(), row.after.as_str()),
				("", "new\n")
			);
			assert_eq!(row_preview(&preview, 1, 0, Target).before, "new\n");
			assert_eq!(repo.git(&["rev-parse", "HEAD"]), before_head);
			assert_eq!(
				fs::read(repo.path.join(".git/index")).unwrap(),
				before_index
			);
			assert_eq!(fs::read(repo.path.join(removed)).unwrap(), before_file);
		}
	}

	#[test]
	fn commit_row_preview_of_a_large_new_file_is_content_too_large_not_destination(
	) {
		use super::{
			ReplayFileSide::Target,
			ReplayPreviewUnavailable::{ContentTooLarge, TooLarge},
		};
		let repo = Repo::new();
		repo.write("base", b"base\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let big = "x".repeat(64);
		let payload = CommitsPayload {
			commits: vec![replay_record(vec![replay_file("new", Some(&big))])],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		let opts = RunOptions::default();
		let new_file = preview
			.file_preview_with::<TransferError>(
				0,
				0,
				Target,
				16,
				&opts,
				|_, _| panic!("oversized incoming must not read"),
			)
			.unwrap();
		assert_eq!(new_file.unavailable, Some(ContentTooLarge));

		// An existing destination over the limit is still the destination's fault.
		repo.write("existing", big.as_bytes());
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "existing"]);
		let payload = CommitsPayload {
			commits: vec![replay_record(vec![replay_file(
				"existing",
				Some("tiny\n"),
			)])],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		let dest = preview
			.file_preview_with::<TransferError>(
				0,
				0,
				Target,
				16,
				&opts,
				|path, o| super::read_replay_preview_with(path, 16, o),
			)
			.unwrap();
		assert_eq!(dest.unavailable, Some(TooLarge));
	}

	#[test]
	fn commit_row_preview_refuses_stale_bytes_and_honors_cancel_and_limit() {
		use super::{
			ReplayFileSide::Target, ReplayPreviewUnavailable::ContentTooLarge,
		};
		let repo = Repo::new();
		repo.write("a", b"before\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let payload = CommitsPayload {
			commits: vec![replay_record(vec![replay_file(
				"a",
				Some("after\n"),
			)])],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		let limited = preview
			.file_preview_with::<TransferError>(
				0,
				0,
				Target,
				2,
				&RunOptions::default(),
				|_, _| panic!("oversized incoming must not read"),
			)
			.unwrap();
		assert_eq!(limited.unavailable, Some(ContentTooLarge));
		let token = CancelToken::new();
		token.cancel();
		let opts = RunOptions {
			cancel: Some(token),
			..RunOptions::default()
		};
		assert!(matches!(
			preview.file_preview_with::<TransferError>(
				0,
				0,
				Target,
				1024,
				&opts,
				|_, _| panic!("cancelled read")
			),
			Err(TransferError::Git(GitError::Cancelled { .. }))
		));
		repo.write("a", b"externally changed\n");
		assert!(matches!(
			preview.file_preview_with(
				0,
				0,
				Target,
				1024,
				&RunOptions::default(),
				|path, opts| super::read_replay_preview_with(path, 1024, opts)
			),
			Err(TransferError::StaleDestination { .. })
		));
	}

	#[test]
	fn commit_row_preview_legacy_metadata_does_not_change_apply_eligibility() {
		let repo = Repo::new();
		repo.write("a", b"before\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let payload = CommitsPayload {
			commits: vec![replay_record(vec![replay_file(
				"a",
				Some("after\n"),
			)])],
		};
		let mut preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		for commit in &mut preview.replay.commits {
			for file in &mut commit.files {
				file.before = Default::default();
				file.old_before = Default::default();
			}
		}
		preview.revalidate().unwrap();
		assert_eq!(
			row_preview(&preview, 0, 0, super::ReplayFileSide::Target)
				.unavailable,
			Some(super::ReplayPreviewUnavailable::Unreadable)
		);
	}

	#[cfg(unix)]
	#[test]
	fn commit_row_preview_never_reads_a_symlink_and_tracks_its_replacement() {
		use super::{
			ReplayFileSide::Target, ReplayPreviewUnavailable::Symlink,
		};
		let repo = Repo::new();
		repo.write("sentinel", b"secret\n");
		std::os::unix::fs::symlink("sentinel", repo.path.join("link")).unwrap();
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let payload = CommitsPayload {
			commits: vec![
				replay_record(vec![replay_file("link", Some("replacement\n"))]),
				replay_record(vec![replay_file("link", Some("next\n"))]),
			],
		};
		let preview =
			CommitReplayPreview::capture(&repo.path, &payload).unwrap();
		let first = preview
			.file_preview_with::<TransferError>(
				0,
				0,
				Target,
				1024,
				&RunOptions::default(),
				|_, _| panic!("must not follow a link"),
			)
			.unwrap();
		assert_eq!(first.unavailable, Some(Symlink));
		assert_eq!(row_preview(&preview, 1, 0, Target).before, "replacement\n");
		assert_eq!(
			super::read_replay_preview_with(
				&repo.path.join("link"),
				1024,
				&RunOptions::default()
			)
			.unwrap(),
			super::ReplayPreviewRead::Unavailable(Symlink)
		);
		assert_eq!(fs::read(repo.path.join("sentinel")).unwrap(), b"secret\n");
	}

	fn assert_stopped_on_first_target(
		err: TransferError,
		arm: &std::path::Path,
		second: &std::path::Path,
		mark: [u8; 4],
	) {
		match err {
			TransferError::Git(GitError::Cancelled { ref args }) => {
				assert!(
					args.contains("hash-file"),
					"cancel did not come from hash_file: {args}"
				);
			}
			other => panic!("expected hash_file cancellation, got {other}"),
		}
		let notes = hash_probe_notes();
		let arm_at = notes.iter().position(|note| note.path == arm);
		let arm_at = arm_at.unwrap_or_else(|| {
			panic!(
				"production hash_file never read {}:\n{}",
				arm.display(),
				notes
					.iter()
					.map(|note| format!(
						"{} ({} bytes)",
						note.path.display(),
						note.len
					))
					.collect::<Vec<_>>()
					.join("\n")
			)
		});
		assert!(
			notes[..arm_at].iter().any(|note| {
				note.path.file_name().and_then(|n| n.to_str()) == Some("index")
					&& note.head[..4] == *b"DIRC"
			}),
			"index hash did not precede the armed target"
		);
		let armed: Vec<_> =
			notes.iter().filter(|note| note.path == arm).collect();
		assert_eq!(armed.len(), 1, "more than one chunk of the first target");
		assert_eq!(armed[0].len, FILE_IO_CHUNK);
		assert_eq!(armed[0].head, mark);
		assert!(
			notes.iter().all(|note| note.path != second),
			"second target was hashed: {}",
			second.display()
		);
	}

	#[test]
	fn replay_preview_cancels_after_first_skipped_target_chunk() {
		let repo = Repo::new();
		let skip = repo.path.join("skip.bin");
		let other = repo.path.join("other.txt");
		let skip_bytes = vec![0xFFu8; FILE_IO_CHUNK + 64];
		repo.write("skip.bin", &skip_bytes);
		repo.write("other.txt", b"other\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let before = disk(&repo, "skip.bin", "other.txt");
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "incoming\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-26T00:00:00+00:00".into(),
				files: vec![
					CommitFile {
						path: "skip.bin".into(),
						old_path: None,
						change: FileChange::Modified,
						content: Some("text\n".into()),
						not_copied: None,
					},
					CommitFile {
						path: "other.txt".into(),
						old_path: None,
						change: FileChange::Modified,
						content: Some("changed\n".into()),
						not_copied: None,
					},
				],
			}],
		};
		let token = CancelToken::new();
		let opts = RunOptions {
			cancel: Some(token.clone()),
			..RunOptions::default()
		};
		let _probe = arm_hash_probe(skip.clone(), token);
		let err =
			CommitReplayPreview::capture_with(&repo.path, &payload, &opts)
				.expect_err("preview must stop while hashing skip.bin");
		assert_stopped_on_first_target(err, &skip, &other, [0xFF; 4]);
		assert_same(&before, &disk(&repo, "skip.bin", "other.txt"));
	}

	#[test]
	fn import_preview_cancels_after_first_target_chunk() {
		let repo = Repo::new();
		let wide = repo.path.join("wide.txt");
		let other = repo.path.join("other.txt");
		let wide_bytes = vec![b'a'; FILE_IO_CHUNK + 64];
		repo.write("wide.txt", &wide_bytes);
		repo.write("other.txt", b"other\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let before = disk(&repo, "wide.txt", "other.txt");
		let text = "\
// file: wide.txt
replacement
// file: other.txt
replacement-two
";
		let token = CancelToken::new();
		let opts = RunOptions {
			cancel: Some(token.clone()),
			..RunOptions::default()
		};
		let _probe = arm_hash_probe(wide.clone(), token);
		let err = super::plan_import_with(
			text,
			"// file: $FILE_PATH",
			std::slice::from_ref(&repo.path),
			&ImportMapping::with_primary(
				CanonicalRootId::new(&repo.path).unwrap(),
			),
			&opts,
		)
		.expect_err("import preview must stop while hashing wide.txt");
		assert_stopped_on_first_target(err, &wide, &other, [b'a'; 4]);
		assert_same(&before, &disk(&repo, "wide.txt", "other.txt"));
	}

	#[test]
	fn replay_preview_capture_cancels_at_final_boundary_after_payload_clone() {
		let repo = Repo::new();
		repo.write("file.txt", b"hello\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let before = disk(&repo, "file.txt", "file.txt");
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "incoming\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-26T00:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "file.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some("changed\n".into()),
					not_copied: None,
				}],
			}],
		};
		let token = CancelToken::new();
		let opts = RunOptions {
			cancel: Some(token.clone()),
			..RunOptions::default()
		};
		let _guard = arm_final_boundary_cancel("replay-preview-capture", token);
		let err =
			CommitReplayPreview::capture_with(&repo.path, &payload, &opts)
				.expect_err(
				"capture_with must fail at final boundary when token cancelled",
			);
		match err {
			TransferError::Git(GitError::Cancelled { ref args }) => {
				assert_eq!(args, "replay-preview");
			}
			other => {
				panic!("expected Cancelled(replay-preview), got {other:?}")
			}
		}
		assert_same(&before, &disk(&repo, "file.txt", "file.txt"));
	}

	#[test]
	fn replay_preview_revalidate_cancels_at_final_boundary() {
		let repo = Repo::new();
		repo.write("file.txt", b"hello\n");
		repo.git(&["add", "-A"]);
		repo.git(&["commit", "-q", "-m", "base"]);
		let before = disk(&repo, "file.txt", "file.txt");
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "incoming\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-26T00:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "file.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some("changed\n".into()),
					not_copied: None,
				}],
			}],
		};
		let preview = CommitReplayPreview::capture(&repo.path, &payload)
			.expect("setup capture must succeed");
		let token = CancelToken::new();
		let opts = RunOptions {
			cancel: Some(token.clone()),
			..RunOptions::default()
		};
		let _guard =
			arm_final_boundary_cancel("replay-preview-revalidate", token);
		let err = preview.revalidate_with(&opts).expect_err(
			"revalidate_with must fail at final boundary when token cancelled",
		);
		match err {
			TransferError::Git(GitError::Cancelled { ref args }) => {
				assert_eq!(args, "replay-preview");
			}
			other => {
				panic!("expected Cancelled(replay-preview), got {other:?}")
			}
		}
		assert_same(&before, &disk(&repo, "file.txt", "file.txt"));
	}
}

#[cfg(test)]
mod root_retained_tests {
	use super::*;

	#[test]
	fn canonical_root_heap_accessor_includes_spare_path_storage() {
		let temp = tempfile::tempdir().unwrap();
		let mut root = CanonicalRootId::new(temp.path()).unwrap();
		let before = root.retained_heap_bytes();
		root.0.reserve(8192);
		assert!(root.retained_heap_bytes() > before);
		assert_eq!(root.retained_heap_bytes(), root.0.capacity());
		assert!(root.retained_heap_bytes() > root.path().as_os_str().len());
	}
}

#[cfg(test)]
mod staged_index_spec_tests {
	use super::*;

	/// The staged content is read through a git revision spec. A bare
	/// `:<path>` misparses a file named `0:a` as stage 0 of `a`, so the
	/// spec must always spell the stage: `:0:<path>`.
	///
	/// Every product entry point refuses a `:` in a relative path before
	/// any read (`sanitize_relative_path` — the payload format cannot
	/// restore such a path), so the selection here is a struct literal and
	/// the spec is asserted through [`staged_index_spec`]: a misparsing
	/// spec would hand the wrong blob to `git cat-file`.
	#[test]
	fn staged_spec_spells_stage_zero_so_a_leading_stage_number_cannot_misparse()
	{
		assert_eq!(staged_index_spec("a".into()), ":0:a");
		assert_eq!(
			staged_index_spec("0:a".into()),
			":0:0:a",
			"the naive `:<path>` form yields `:0:a` here, which git reads \
			 as stage 0 of `a`"
		);
		assert_eq!(
			staged_index_spec("dir/0:b".into()),
			":0:dir/0:b",
			"only the first colon separates the stage; the path stays whole"
		);
	}

	/// End to end through a real index: the spec `staged_spec_spells…`
	/// pins must read each file's own staged bytes. Both files are staged
	/// edits whose worktree matches the index, so a wrong spec is the only
	/// way the read contents can disagree.
	// Unix only: Windows refuses `:` in a file name (NTFS reads `0:a` as
	// the stream `a` of a file `0`).
	#[cfg(unix)]
	#[test]
	fn staged_read_returns_each_index_entrys_own_bytes() {
		let dir = tempfile::tempdir().unwrap();
		let repo_raw = dir.path().join("repo");
		fs::create_dir_all(&repo_raw).unwrap();
		let repo = dunce::canonicalize(&repo_raw).unwrap();
		let cfg = dir.path().join("cfg");
		fs::write(&cfg, "").unwrap();
		let run = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.args(args)
				.current_dir(&repo)
				.env("GIT_CONFIG_GLOBAL", &cfg)
				.env("GIT_CONFIG_NOSYSTEM", "1")
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			String::from_utf8_lossy(&out.stdout).trim().to_string()
		};
		run(&["init", "-q", "-b", "main"]);
		run(&["config", "user.name", "T"]);
		run(&["config", "user.email", "t@example.com"]);
		fs::write(repo.join("a"), "head a\n").unwrap();
		fs::write(repo.join("0:a"), "head 0a\n").unwrap();
		run(&["add", "-A"]);
		run(&["commit", "-q", "-m", "base"]);
		fs::write(repo.join("a"), "staged a\n").unwrap();
		fs::write(repo.join("0:a"), "staged 0a\n").unwrap();
		run(&["add", "-A"]);

		let git = Git::open(&repo).unwrap();
		let mut blobs = BlobReader::blobs_only(&RunOptions::default());
		let mut read = |rel: &str| {
			let spec = staged_index_spec(rel.to_string());
			match blobs.read(&git, &spec, u64::MAX).unwrap() {
				BlobRead::Text(s) => s,
				other => panic!("expected text for `{spec}`, got {other:?}"),
			}
		};
		assert_eq!(read("a"), "staged a\n");
		assert_eq!(
			read("0:a"),
			"staged 0a\n",
			"`:0:a` reads stage 0 of `a` instead of the file `0:a`"
		);
	}
}

#[cfg(test)]
mod freshness_error_tests {
	use super::*;

	#[test]
	fn transfer_error_reasons_keep_what_the_user_must_act_on() {
		let amb = TransferError::AmbiguousRootBasenames {
			basename: "src".into(),
			roots: vec![PathBuf::from("/a/src"), PathBuf::from("/b/src")],
		};
		assert_eq!(
			amb.reason(),
			Reason::AmbiguousRoots {
				basename: "src".into(),
				roots: vec!["/a/src".into(), "/b/src".into()],
			}
		);
		let stale = TransferError::StaleSource {
			root: "/r".into(),
			reason: "Git index changed".into(),
			change: StaleChange::Repository,
		};
		assert_eq!(
			stale.reason(),
			Reason::SourceChanged {
				change: StaleChange::Repository
			}
		);
		assert_eq!(
			TransferError::PayloadLimitExceeded {
				limit: 1,
				actual: 2,
				reason: "x".into(),
				path: Some("a/b.txt".into())
			}
			.reason(),
			Reason::PayloadTooLarge {
				limit: 1,
				actual: 2,
				path: Some("a/b.txt".into())
			}
		);
		assert_eq!(
			TransferError::Git(GitError::NotFound).reason(),
			Reason::GitMissing
		);
	}

	#[test]
	fn non_regular_path_yields_special_file_on_export_and_destination_not_regular_on_import(
	) {
		let dir = tempfile::tempdir().unwrap();
		let root_raw = dir.path().join("root");
		fs::create_dir_all(root_raw.join("subdir")).unwrap();
		let root_canon = dunce::canonicalize(&root_raw).unwrap();
		let root_id = CanonicalRootId::new(&root_canon).unwrap();

		// Export side: SourceFreshnessSnapshot::capture calls the source snapshot
		// helper on a directory where a working file is expected, asserting SpecialFile.
		let export_selection = ExportSelection {
			roots: vec![root_id.clone()],
			primary_root: Some(root_id.clone()),
			items: vec![ExportItem {
				root: root_id.clone(),
				relative_path: "subdir".to_string(),
				source: SourceKind::Working,
				change_type: Some(ChangeType::Modified),
				gitlink: false,
			}],
			source_root: None,
			spelled_root: None,
			filter_root: None,
		};
		let export_err = SourceFreshnessSnapshot::capture(&export_selection)
			.expect_err("export capture must fail on non-regular file");
		match export_err {
			TransferError::SpecialFile(p) => {
				assert!(
					p.ends_with("subdir"),
					"expected SpecialFile path to end with 'subdir', got {p}"
				);
			}
			other => {
				panic!("expected TransferError::SpecialFile, got {other:?}")
			}
		}

		// Import side: DestinationFreshnessSnapshot::capture_paths asserts DestinationNotRegular.
		let import_target = (root_canon.join("subdir"), "subdir".to_string());
		let import_err = DestinationFreshnessSnapshot::capture_paths(
			std::slice::from_ref(&root_canon),
			&[import_target],
		)
		.expect_err("import capture_paths must fail on non-regular file");
		match import_err {
			TransferError::DestinationNotRegular(p) => {
				assert!(
					p.ends_with("subdir"),
					"expected DestinationNotRegular path to end with 'subdir', got {p}"
				);
			}
			other => {
				panic!("expected TransferError::DestinationNotRegular, got {other:?}")
			}
		}

		// Direct capture_file_freshness checks with respective error constructors.
		let direct_export = capture_file_freshness(
			&root_canon.join("subdir"),
			&RunOptions::default(),
			TransferError::SpecialFile,
		)
		.expect_err("capture_file_freshness must fail on directory");
		assert!(matches!(direct_export, TransferError::SpecialFile(_)));

		let direct_import = capture_file_freshness(
			&root_canon.join("subdir"),
			&RunOptions::default(),
			TransferError::DestinationNotRegular,
		)
		.expect_err("capture_file_freshness must fail on directory");
		assert!(matches!(
			direct_import,
			TransferError::DestinationNotRegular(_)
		));
	}
}

#[cfg(test)]
mod wire_tests {
	use super::*;

	/// A remote worker sends an import plan and its freshness snapshot to the
	/// master and gets the snapshot back: both must survive JSON unchanged,
	/// map keys included.
	#[test]
	fn import_plan_and_freshness_round_trip_through_json() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		fs::write(root.join("a.txt"), "old\n").unwrap();
		let payload = "// File: a.txt\nnew\n\n// File: b.txt\nb\n";
		let mapping =
			ImportMapping::with_primary(CanonicalRootId::new(&root).unwrap());
		let plan =
			plan_import(payload, "", std::slice::from_ref(&root), &mapping)
				.unwrap();
		assert_eq!(plan.create_operations().len(), 2);
		let json = serde_json::to_string(&plan).unwrap();
		let back: TransferImportPlan = serde_json::from_str(&json).unwrap();
		assert_eq!(back, plan);
		let fresh =
			serde_json::to_string(plan.destination_freshness()).unwrap();
		let back: DestinationFreshnessSnapshot =
			serde_json::from_str(&fresh).unwrap();
		assert_eq!(&back, plan.destination_freshness());
		back.revalidate().unwrap();
		let sel = RestoreSelection {
			overwrite_existing: true,
			unchecked_creates: [1].into_iter().collect(),
			..Default::default()
		};
		let back: RestoreSelection =
			serde_json::from_str(&serde_json::to_string(&sel).unwrap())
				.unwrap();
		assert_eq!(back, sel);
	}
}

#[cfg(test)]
mod parsed_import_tests {
	use super::*;

	#[test]
	fn parsed_import_indices_follow_entries_through_routing_skips_and_deletes()
	{
		let temp = tempfile::tempdir().unwrap();
		let base = dunce::canonicalize(temp.path()).unwrap();
		let a = base.join("one");
		let b = base.join("two");
		fs::create_dir(&a).unwrap();
		fs::create_dir(&b).unwrap();
		fs::write(a.join("gone.txt"), "delete me").unwrap();
		fs::write(a.join("binary.txt"), [0xff]).unwrap();
		let mut mapping =
			ImportMapping::with_primary(CanonicalRootId::new(&a).unwrap());
		mapping.map_prefix("moved", CanonicalRootId::new(&b).unwrap());
		mapping.map_entry(
			"explicit.txt",
			CanonicalRootId::new(&a).unwrap(),
			Some("renamed.txt".into()),
		);
		mapping.block_prefix("blocked");
		let text = format!("// FILE: .git/config\nunsafe\n// FILE: same.txt\nsame\n// FILE: [DELETED] gone.txt\n{}\n// FILE: moved/same2.txt\nsame\n// FILE: binary.txt\ntext\n// FILE: {}\nabsolute\n// FILE: blocked/no.txt\nblocked\n// FILE: explicit.txt\nexplicit\n// FILE: placeholder.txt\n// File skipped: size exceeds limit (1000 bytes)\n// FILE: tail.txt\ntail\n", DELETED_FILE_MARKER, b.join("absolute.txt").display());
		let entries = format::parse_clipboard(&text, "");
		assert_eq!(entries.len(), 10);
		let roots = [a.clone(), b.clone()];
		let (parsed, indices) = plan_import_parsed_with(
			entries.clone(),
			&roots,
			&mapping,
			&RunOptions::default(),
		)
		.unwrap();
		assert_eq!(indices, [1, 3, 5, 7, 9]);
		assert_eq!(parsed.delete_operations().len(), 1);
		assert_eq!(parsed.skipped_operations().len(), 4);
		assert_eq!(
			parsed
				.create_operations()
				.iter()
				.map(|op| op.absolute_path.clone())
				.collect::<Vec<_>>(),
			[
				a.join("same.txt"),
				b.join("same2.txt"),
				b.join("absolute.txt"),
				a.join("renamed.txt"),
				a.join("tail.txt")
			]
		);
		for (op, index) in parsed.create_operations().iter().zip(&indices) {
			assert_eq!(op.content, entries[*index].content);
		}
		assert_eq!(
			parsed,
			plan_import_with(
				&text,
				"",
				&roots,
				&mapping,
				&RunOptions::default()
			)
			.unwrap()
		);
		let reordered: Vec<_> = [9, 0, 5, 1, 8, 2, 3, 7, 4, 6]
			.into_iter()
			.map(|i| entries[i].clone())
			.collect();
		let (plan, indices) = plan_import_parsed_with(
			reordered.clone(),
			&roots,
			&mapping,
			&RunOptions::default(),
		)
		.unwrap();
		assert_eq!(indices, [0, 2, 3, 6, 7]);
		for (op, index) in plan.create_operations().iter().zip(indices) {
			assert_eq!(op.content, reordered[index].content);
		}
		assert!(!a.join("same.txt").exists());
		assert_eq!(fs::read(a.join("gone.txt")).unwrap(), b"delete me");
	}

	#[test]
	fn parsed_import_keeps_cancellation_and_explicit_root_validation() {
		let temp = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(temp.path()).unwrap();
		let entries = format::parse_clipboard("// FILE: a\nbody\n", "");
		let mapping =
			ImportMapping::with_primary(CanonicalRootId::new(&root).unwrap());
		let token = CancelToken::new();
		token.cancel();
		let options = RunOptions {
			cancel: Some(token),
			..RunOptions::default()
		};
		assert!(matches!(
			plan_import_parsed_with(
				entries.clone(),
				std::slice::from_ref(&root),
				&mapping,
				&options
			),
			Err(TransferError::Git(GitError::Cancelled { .. }))
		));
		assert!(matches!(
			plan_import_parsed_with(
				entries,
				&[],
				&mapping,
				&RunOptions::default()
			),
			Err(TransferError::UnknownRoot(_))
		));
		assert!(!root.join("a").exists());
	}
}

#[cfg(test)]
mod freshness_helper_tests {
	use super::*;

	#[test]
	fn not_a_directory_is_the_os_error_kind() {
		let e = not_a_directory();
		assert_eq!(e.kind(), io::ErrorKind::NotADirectory, "{e}");
		assert!(e.raw_os_error().is_some());
	}

	#[test]
	fn slash_rel_uses_forward_slashes_below_the_root() {
		let root = Path::new("root");
		assert_eq!(slash_rel(root, &root.join("a").join("b")), "a/b");
		assert_eq!(slash_rel(root, &root.join("a")), "a");
	}
}

#[cfg(test)]
mod new_dir_tests {
	use super::*;

	fn tmp() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dunce::canonicalize(dir.path()).unwrap();
		(dir, path)
	}

	/// Installs the hook for this thread until dropped. A test runs in its
	/// own process under nextest, and the slot is thread-local besides.
	struct HookGuard;

	impl Drop for HookGuard {
		fn drop(&mut self) {
			NEW_DIR_HOOK.with(|slot| *slot.borrow_mut() = None);
		}
	}

	fn hook(
		f: impl Fn(NewDirStage, &Path) -> io::Result<()> + 'static,
	) -> HookGuard {
		NEW_DIR_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(f)));
		HookGuard
	}

	/// A plan pasting `a.txt` into the folder `dest` (which may not exist),
	/// and the selection that agrees to the folders it would make.
	fn plan(ws: &Path, dest: &Path) -> (TransferImportPlan, RestoreSelection) {
		let placed = PlacedRoot::resolve(None, dest).unwrap();
		let plan = plan_import_with(
			"// file: a.txt\nA\n",
			"",
			&[placed.anchor().path().to_path_buf()],
			&ImportMapping::with_primary(placed),
			&RunOptions::default(),
		)
		.unwrap();
		let _ = ws;
		let selection = RestoreSelection {
			confirmed_placed: plan.new_dirs().placed.into_iter().collect(),
			..Default::default()
		};
		(plan, selection)
	}

	fn scan(dir: &Path) -> Vec<PathBuf> {
		fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
			for entry in fs::read_dir(dir).unwrap() {
				let path = entry.unwrap().path();
				out.push(path.clone());
				if path.is_dir() && !path.is_symlink() {
					walk(&path, out);
				}
			}
		}
		let mut out = Vec::new();
		walk(dir, &mut out);
		out.sort();
		out
	}

	/// A4: a folder that is there by the time it is made is a change the
	/// preview did not show, not something to carry on into.
	#[test]
	fn a_named_folder_that_exists_when_made_is_stale() {
		let (_tmp, ws) = tmp();
		let (plan, sel) = plan(&ws, &ws.join("new"));
		let _hook = hook(|stage, dir| {
			if stage == NewDirStage::BeforeCreate {
				fs::create_dir(dir)?;
			}
			Ok(())
		});
		match plan.apply(&sel) {
			Err(TransferError::StaleDestination {
				change: StaleChange::Created { rel },
				..
			}) => assert_eq!(rel, "new"),
			other => panic!("expected a stale Created, got {other:?}"),
		}
		assert_eq!(scan(&ws), vec![ws.join("new")], "no file written");
	}

	/// A5 (a): a failure part-way takes back the folders already made.
	#[test]
	fn a_failure_while_making_folders_takes_back_the_ones_made() {
		let (_tmp, ws) = tmp();
		let deeper = ws.join("new").join("deeper");
		let (plan, sel) = plan(&ws, &deeper);
		assert_eq!(sel.confirmed_placed, [0].into());
		assert_eq!(plan.placed_folders()[0].len(), 2);
		let target = deeper.clone();
		let _hook = hook(move |stage, dir| {
			if stage == NewDirStage::BeforeCreate && dir == target {
				return Err(io::ErrorKind::PermissionDenied.into());
			}
			Ok(())
		});
		match plan.apply(&sel) {
			Err(TransferError::Io(e)) => {
				assert_eq!(e.kind(), io::ErrorKind::PermissionDenied)
			}
			other => panic!("expected an io error, got {other:?}"),
		}
		assert!(scan(&ws).is_empty(), "{:?}", scan(&ws));
	}

	/// A5 (b): the last level taken by a file in the meantime is stale, and
	/// the level made before it is taken back as far as it can be (it holds
	/// the interloper, which stays).
	#[test]
	fn a_file_where_a_folder_was_to_be_made_is_stale_and_the_rest_is_taken_back(
	) {
		let (_tmp, ws) = tmp();
		let deeper = ws.join("new").join("deeper");
		let (plan, sel) = plan(&ws, &deeper);
		let target = deeper.clone();
		let _hook = hook(move |stage, dir| {
			if stage == NewDirStage::BeforeCreate && dir == target {
				fs::write(dir, "interloper")?;
			}
			Ok(())
		});
		match plan.apply(&sel) {
			Err(TransferError::StaleDestination {
				change: StaleChange::Created { rel },
				..
			}) => assert_eq!(rel, "new/deeper"),
			other => panic!("expected a stale Created, got {other:?}"),
		}
		assert_eq!(fs::read_to_string(&deeper).unwrap(), "interloper");
		assert_eq!(scan(&ws), vec![ws.join("new"), deeper]);
	}

	/// A5 (c): something put where the file goes, in a folder this Apply
	/// just made, is stale; the folder made stays, since it is not empty.
	#[test]
	fn a_made_folder_with_something_in_it_stays_when_nothing_was_written() {
		let (_tmp, ws) = tmp();
		let (plan, sel) = plan(&ws, &ws.join("new"));
		let _hook = hook(|stage, dir| {
			if stage == NewDirStage::AfterCreate {
				fs::create_dir(dir.join("a.txt"))?;
			}
			Ok(())
		});
		match plan.apply(&sel) {
			Err(TransferError::StaleDestination {
				change: StaleChange::Created { rel },
				..
			}) => assert_eq!(rel, "new/a.txt"),
			other => panic!("expected a stale Created, got {other:?}"),
		}
		assert!(ws.join("new/a.txt").is_dir());
	}

	/// A5 (d): a failure right after the last level was made takes back every
	/// level, that one included.
	#[test]
	fn a_failure_after_the_last_level_takes_back_every_level() {
		let (_tmp, ws) = tmp();
		let deeper = ws.join("new").join("deeper");
		let (plan, sel) = plan(&ws, &deeper);
		let target = deeper.clone();
		let _hook = hook(move |stage, dir| {
			if stage == NewDirStage::AfterCreate && dir == target {
				return Err(io::ErrorKind::PermissionDenied.into());
			}
			Ok(())
		});
		assert!(matches!(plan.apply(&sel), Err(TransferError::Io(_))));
		assert!(scan(&ws).is_empty(), "{:?}", scan(&ws));
	}

	/// A5 (d), the end of an Apply: nothing written leaves no empty folder it
	/// made; something written keeps them; a folder with content stays, and
	/// so does one that is another object by then.
	#[test]
	fn finishing_takes_back_empty_folders_only_when_nothing_was_written() {
		let (_tmp, ws) = tmp();
		let (a, b) = (ws.join("a"), ws.join("a").join("b"));
		let make = || {
			fs::create_dir_all(&b).unwrap();
			let mut made = Made::default();
			made.dir(&a, true);
			made.dir(&b, true);
			made
		};
		let made = make();
		assert_eq!(made.finish(true).0, 2);
		assert!(b.is_dir());
		assert_eq!(made.finish(false).0, 0);
		assert!(!a.exists());

		let made = make();
		fs::write(b.join("keep.txt"), "x").unwrap();
		let (kept, left) = made.finish(false);
		assert_eq!(kept, 2);
		assert!(
			left.is_empty(),
			"a folder with something in it is no failure: {left:?}"
		);
		assert!(b.join("keep.txt").exists());

		fs::remove_dir_all(&a).unwrap();
		let made = make();
		fs::remove_dir(&b).unwrap();
		fs::create_dir(&b).unwrap();
		assert!(made.finish(false).1.is_empty());
		assert!(b.is_dir(), "another folder in its place is not ours");
	}

	/// The file made, removed and made again empty at once by someone
	/// else, is another file even where the filesystem would reuse its
	/// inode number and its clock has not ticked (Linux): it is neither
	/// written nor taken back.
	#[test]
	fn a_new_file_replaced_by_another_empty_one_is_not_taken_back() {
		let (_tmp, ws) = tmp();
		let path = ws.join("new.txt");
		let file = fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&path)
			.unwrap();
		let key = DirKey::of_file(&file).unwrap();
		let mut made = Made::default();
		let modified = fs::symlink_metadata(&path)
			.and_then(|info| info.modified())
			.ok();
		let reserved = made.reserved(file, key, modified);
		made.file(&path, &reserved, "new.txt");
		fs::remove_file(&path).unwrap();
		// Made within the same clock tick: Linux stamps a file with a
		// coarse clock, so its time can equal the one recorded.
		let other = fs::File::create(&path).unwrap();
		other.set_modified(modified.unwrap()).unwrap();
		drop(other);
		assert!(!reserved.is_untouched(&path).unwrap());
		made.release_file(&path).unwrap();
		assert!(path.exists(), "another file in its place is not ours");
	}

	/// Restores the process umask when dropped. Safe to change here: each
	/// test is its own process under nextest.
	#[cfg(unix)]
	struct UmaskGuard(nix::sys::stat::Mode);

	#[cfg(unix)]
	impl UmaskGuard {
		fn set(mask: u32) -> Self {
			use nix::sys::stat::{umask, Mode};
			Self(umask(Mode::from_bits_truncate(mask as _)))
		}
	}

	#[cfg(unix)]
	impl Drop for UmaskGuard {
		fn drop(&mut self) {
			nix::sys::stat::umask(self.0);
		}
	}

	/// Under umask 0444 the file made is write-only: no permission to read
	/// it must not make it "not ours", or the row skips it and leaves an
	/// empty file with nothing written.
	#[cfg(unix)]
	#[test]
	fn a_new_file_that_umask_leaves_unreadable_is_still_written() {
		use std::os::unix::fs::PermissionsExt;
		let (_tmp, ws) = tmp();
		let (plan, sel) = plan(&ws, &ws);
		let guard = UmaskGuard::set(0o444);
		let result = plan.apply(&sel);
		drop(guard);
		let result = result.unwrap();
		assert_eq!(result.created_count, 1, "{result:?}");
		assert_eq!(result.skipped_existing_count, 0, "{result:?}");
		assert!(result.errors.is_empty(), "{result:?}");
		let path = ws.join("a.txt");
		fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
		assert_eq!(fs::read_to_string(&path).unwrap(), "A");
	}

	/// The same file, after a failed write: taking it back needs no read
	/// permission either.
	#[cfg(unix)]
	#[test]
	fn an_unreadable_empty_file_made_is_taken_back() {
		use std::os::unix::fs::PermissionsExt;
		let (_tmp, ws) = tmp();
		let path = ws.join("new.txt");
		let file = fs::File::create(&path).unwrap();
		let key = DirKey::of_file(&file).unwrap();
		drop(file);
		fs::set_permissions(&path, fs::Permissions::from_mode(0o222)).unwrap();
		let modified = fs::symlink_metadata(&path)
			.and_then(|info| info.modified())
			.ok();
		let reserved = restore::Reserved::new(key, modified);
		let mut made = Made::default();
		made.file(&path, &reserved, "new.txt");
		made.release_file(&path).unwrap();
		assert!(!path.exists(), "an empty write-only file made is ours");
	}

	/// A6: a symlink put where the folder was just made is stale, and nothing
	/// reaches what it points at.
	#[cfg(unix)]
	#[test]
	fn a_folder_swapped_for_a_symlink_right_after_it_was_made_is_stale() {
		let (_tmp, ws) = tmp();
		let elsewhere = ws.join("elsewhere");
		fs::create_dir(&elsewhere).unwrap();
		let (plan, sel) = plan(&ws, &ws.join("new"));
		let target = elsewhere.clone();
		let _hook = hook(move |stage, dir| {
			if stage == NewDirStage::AfterCreate {
				fs::remove_dir(dir)?;
				std::os::unix::fs::symlink(&target, dir)?;
			}
			Ok(())
		});
		match plan.apply(&sel) {
			Err(TransferError::StaleDestination {
				change: StaleChange::Preview,
				..
			}) => {}
			other => panic!("expected a stale Preview, got {other:?}"),
		}
		assert!(scan(&elsewhere).is_empty());
	}
}

#[cfg(test)]
mod case_policy_tests {
	use super::*;
	use crate::fscase::{with_probe_override, CaseSensitivity};

	fn tmp() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dunce::canonicalize(dir.path()).unwrap();
		(dir, path)
	}

	/// An empty folder on a filesystem that cannot be told and gives the
	/// names nothing to compare: two targets that differ only in case are
	/// one target to the plan, as on any folding filesystem.
	#[test]
	fn two_targets_that_differ_in_case_collide_when_case_cannot_be_told() {
		let (_tmp, ws) = tmp();
		let result =
			with_probe_override(CaseSensitivity::Unknown, None, || {
				plan_import(
					"// file: B.txt\nB\n// file: b.txt\nb\n",
					"",
					std::slice::from_ref(&ws),
					&ImportMapping::with_primary(
						CanonicalRootId::new(&ws).unwrap(),
					),
				)
			});
		assert!(
			matches!(result, Err(TransferError::TargetCollision { .. })),
			"{result:?}"
		);
	}

	/// The same folder spelled two ways, when case cannot be told: both
	/// spellings are listed as named, and each placed folder is agreed to
	/// on its own (whether they are one folder is the filesystem's to say
	/// when Apply makes them).
	#[test]
	fn one_new_folder_spelled_two_ways_is_asked_for_when_case_cannot_be_told() {
		let (_tmp, ws) = tmp();
		let placed =
			|name: &str| PlacedRoot::resolve(None, &ws.join(name)).unwrap();
		let mut mapping = ImportMapping::with_primary(placed("other"));
		mapping.map_prefix("p", placed("new"));
		mapping.map_prefix("q", placed("NEW"));
		let plan = with_probe_override(CaseSensitivity::Unknown, None, || {
			plan_import(
				"// file: p/a.txt\nA\n// file: q/b.txt\nB\n",
				"",
				std::slice::from_ref(&ws),
				&mapping,
			)
			.unwrap()
		});
		let new = plan.new_dirs();
		assert_eq!(new.requested, vec![ws.join("NEW"), ws.join("new")]);
		assert_eq!(new.incidental, Vec::<PathBuf>::new());
		assert_eq!(new.placed, vec![1, 2]);
	}

	/// The same answer decides for the folders an exact name differs
	/// under: a definite `Sensitive` keeps them apart.
	#[test]
	fn a_known_sensitive_filesystem_keeps_the_two_spellings_apart() {
		let (_tmp, ws) = tmp();
		let placed =
			|name: &str| PlacedRoot::resolve(None, &ws.join(name)).unwrap();
		let mut mapping = ImportMapping::with_primary(placed("other"));
		mapping.map_prefix("p", placed("new"));
		mapping.map_prefix("q", placed("NEW"));
		let plan =
			with_probe_override(CaseSensitivity::Sensitive, None, || {
				plan_import(
					"// file: p/a.txt\nA\n// file: q/b.txt\nB\n",
					"",
					std::slice::from_ref(&ws),
					&mapping,
				)
				.unwrap()
			});
		assert_eq!(plan.new_dirs().requested.len(), 2, "{:?}", plan.new_dirs());
	}

	/// Two destination roots that are two folders though their paths read
	/// alike in lower case (`\u{391}` and `\u{3b1}`), each with a new folder
	/// of the same name. When case cannot be told both fold, and the folder
	/// the user named under the second root must still ask for consent: it
	/// was once taken for the first root's, dropped from the agreement and
	/// made without asking. Linux only: macOS and Windows fold the two names
	/// into one folder, so the scenario cannot be built there, and a test
	/// that returned early would pass as a skip.
	#[cfg(target_os = "linux")]
	#[test]
	fn a_named_folder_under_another_root_is_not_the_first_roots_folder() {
		let (_tmp, ws) = tmp();
		let (upper, lower) = (ws.join("\u{391}"), ws.join("\u{3b1}"));
		fs::create_dir(&upper).unwrap();
		fs::create_dir(&lower)
			.expect("cannot make two folders that differ in case on Linux");
		let placed =
			|root: &Path| PlacedRoot::resolve(None, &root.join("new")).unwrap();
		let mut mapping = ImportMapping::with_primary(placed(&upper));
		mapping.map_prefix("q", placed(&lower));
		let roots = [upper.clone(), lower.clone()];
		let plan = with_probe_override(CaseSensitivity::Unknown, None, || {
			plan_import("// file: q/b.txt\nB\n", "", &roots, &mapping).unwrap()
		});
		let want = NewDirs {
			requested: vec![lower.join("new")],
			incidental: Vec::new(),
			placed: vec![1],
		};
		assert_eq!(plan.new_dirs(), want);
		match plan.apply(&RestoreSelection::default()) {
			Err(TransferError::NewDirsNotConfirmed { dirs }) => {
				assert_eq!(dirs, vec![lower.join("new")]);
			}
			other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
		}
		assert!(!lower.join("new").exists());
		assert!(!upper.join("new").exists());
	}
}

/// The folders the user named are made only with consent, whatever planning
/// made of them: Apply checks each placed folder's tail itself.
#[cfg(test)]
mod placed_consent_tests {
	use super::*;

	fn tmp() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dunce::canonicalize(dir.path()).unwrap();
		(dir, path)
	}

	fn scan(dir: &Path) -> Vec<PathBuf> {
		fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
			for entry in fs::read_dir(dir).unwrap() {
				let path = entry.unwrap().path();
				out.push(path.clone());
				if path.is_dir() && !path.is_symlink() {
					walk(&path, out);
				}
			}
		}
		let mut out = Vec::new();
		walk(dir, &mut out);
		out.sort();
		out
	}

	/// Primary `new/sub` and prefix `q` at `NEW/SUB`, a payload for `q`
	/// only, with `new` made by someone else after planning checked the
	/// tails and before it told the named folders from the others. On a
	/// filesystem that folds case `NEW/SUB` is then `sub` below an existing
	/// folder, and planning took it for an incidental one.
	#[cfg(any(target_os = "macos", windows))]
	fn plan_raced(ws: &Path) -> TransferImportPlan {
		let probe = ws.join(".case-probe");
		fs::write(&probe, "").unwrap();
		assert!(
			ws.join(".CASE-PROBE").exists(),
			"needs a filesystem that folds case"
		);
		fs::remove_file(&probe).unwrap();
		let placed =
			|rel: &str| PlacedRoot::resolve(None, &ws.join(rel)).unwrap();
		let mut mapping = ImportMapping::with_primary(placed("new/sub"));
		mapping.map_prefix("q", placed("NEW/SUB"));
		let new = ws.join("new");
		let _hook = test_hooks::after_tail_check(move || {
			fs::create_dir_all(&new).unwrap();
		});
		plan_import("// file: q/b.txt\nB\n", "", &[ws.to_path_buf()], &mapping)
			.unwrap()
	}

	/// The folder made in between is gone again by Apply: every level of
	/// the tail is missing, as the preview had it, and the folder the user
	/// named is made only with consent, though planning listed none.
	#[cfg(any(target_os = "macos", windows))]
	#[test]
	fn a_folder_made_while_planning_does_not_take_consent_away() {
		let (_tmp, ws) = tmp();
		let plan = plan_raced(&ws);
		fs::remove_dir(ws.join("new")).unwrap();
		match plan.apply(&RestoreSelection::default()) {
			Err(TransferError::NewDirsNotConfirmed { dirs }) => {
				assert!(!dirs.is_empty())
			}
			other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
		}
		assert_eq!(scan(&ws), Vec::<PathBuf>::new());
	}

	/// The folder made in between is still there: a level of the tail
	/// exists that did not when the folder was placed, so the plan is stale.
	#[cfg(any(target_os = "macos", windows))]
	#[test]
	fn a_folder_made_while_planning_makes_the_plan_stale() {
		let (_tmp, ws) = tmp();
		let plan = plan_raced(&ws);
		match plan.apply(&RestoreSelection::default()) {
			Err(TransferError::StaleDestination { .. }) => {}
			other => panic!("expected StaleDestination, got {other:?}"),
		}
		assert_eq!(scan(&ws), vec![ws.join("new")]);
	}

	/// A plan whose classification names no folder (what a planning
	/// mistake would leave) still lists the placed folder's levels and
	/// makes them only with consent: Apply reads the tail itself.
	#[test]
	fn apply_asks_for_a_placed_tail_the_plan_does_not_list() {
		let (_tmp, ws) = tmp();
		let placed = PlacedRoot::resolve(None, &ws.join("new/sub")).unwrap();
		let plan = plan_import(
			"// file: a.txt\nA\n",
			"",
			std::slice::from_ref(&ws),
			&ImportMapping::with_primary(placed),
		)
		.unwrap();
		let mut freshness = plan.destination_freshness().clone();
		freshness.requested_dirs = None;
		let plan = TransferImportPlan::from_parts(
			plan.roots().to_vec(),
			plan.restore_plan().clone(),
			freshness,
		);
		assert_eq!(
			plan.new_dirs().requested,
			vec![ws.join("new"), ws.join("new/sub")]
		);
		match plan.apply(&RestoreSelection::default()) {
			Err(TransferError::NewDirsNotConfirmed { dirs }) => {
				assert!(!dirs.is_empty())
			}
			other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
		}
		assert_eq!(scan(&ws), Vec::<PathBuf>::new());

		// With the tail agreed to, it is made and the file written.
		let agreed = RestoreSelection {
			confirmed_placed: [0].into(),
			..Default::default()
		};
		plan.apply(&agreed).unwrap();
		assert_eq!(fs::read_to_string(ws.join("new/sub/a.txt")).unwrap(), "A");
	}
}

/// One name spelled in Unicode's two forms (`é` composed, `e` with a
/// combining accent) is one folder to macOS, and to the plan on every
/// filesystem: a folder the user named is not made without consent, and
/// two writes to it are refused, however the payload spells it.
#[cfg(test)]
mod unicode_name_tests {
	use super::*;

	const COMPOSED: &str = "\u{e9}";
	const DECOMPOSED: &str = "e\u{301}";

	fn tmp() -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let path = dunce::canonicalize(dir.path()).unwrap();
		(dir, path)
	}

	fn scan(dir: &Path) -> Vec<PathBuf> {
		let mut out: Vec<PathBuf> = fs::read_dir(dir)
			.unwrap()
			.map(|e| e.unwrap().path())
			.collect();
		out.sort();
		out
	}

	/// The primary destination is the new folder `ws/é` (composed); `q`
	/// is `ws` itself, and the payload reaches the same folder through
	/// `q/é` decomposed.
	fn plan_named_through_the_other_form(ws: &Path) -> TransferImportPlan {
		let mut mapping = ImportMapping::with_primary(
			PlacedRoot::resolve(None, &ws.join(COMPOSED)).unwrap(),
		);
		mapping.map_prefix("q", CanonicalRootId::new(ws).unwrap());
		plan_import(
			&format!("// file: q/{DECOMPOSED}/a.txt\nA\n"),
			"",
			&[ws.to_path_buf()],
			&mapping,
		)
		.unwrap()
	}

	/// Planning lists the folder as named, and Apply makes nothing without
	/// consent: on macOS the filesystem says the two forms are one folder.
	/// (Where it keeps them apart they are two, and the engine does not
	/// say otherwise.)
	#[cfg(target_os = "macos")]
	#[test]
	fn the_named_folder_reached_in_the_other_form_needs_consent() {
		let (_tmp, ws) = tmp();
		let plan = plan_named_through_the_other_form(&ws);
		assert_eq!(plan.new_dirs().requested.len(), 1, "{:?}", plan.new_dirs());
		match plan.apply(&RestoreSelection::default()) {
			Err(TransferError::NewDirsNotConfirmed { dirs }) => {
				assert_eq!(dirs.len(), 1, "{dirs:?}");
			}
			other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
		}
		assert_eq!(scan(&ws), Vec::<PathBuf>::new());
	}

	/// A plan that lists nothing as named, its create spelled decomposed
	/// under the composed placed tail (a peer's plan, `from_parts`): Apply
	/// tells the two forms as one folder itself, and asks.
	#[test]
	fn apply_asks_for_the_named_folder_reached_in_the_other_form() {
		let (_tmp, ws) = tmp();
		let plan = plan_named_through_the_other_form(&ws);
		let mut freshness = plan.destination_freshness().clone();
		freshness.requested_dirs = None;
		let mut restore_plan = plan.restore_plan().clone();
		let op = &mut restore_plan.create_operations[0];
		op.absolute_path = ws.join(DECOMPOSED).join("a.txt");
		op.relative_path = format!("{DECOMPOSED}/a.txt");
		op.root_path.clone_from(&ws);
		freshness.new_dirs = Some(PlannedNewDirs {
			dirs: vec![ws.join(DECOMPOSED)],
			needs: vec![vec![0]],
			routes: Vec::new(),
		});
		let plan = TransferImportPlan::from_parts(
			plan.roots().to_vec(),
			restore_plan,
			freshness,
		);
		assert!(plan.new_dirs().requested.is_empty());
		match plan.apply(&RestoreSelection::default()) {
			Err(TransferError::NewDirsNotConfirmed { dirs }) => {
				assert_eq!(dirs, vec![ws.join(COMPOSED)]);
			}
			other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
		}
		assert_eq!(scan(&ws), Vec::<PathBuf>::new());
	}

	fn two_forms_mapping(ws: &Path) -> ImportMapping {
		let placed =
			|name: &str| PlacedRoot::resolve(None, &ws.join(name)).unwrap();
		let mut mapping = ImportMapping::with_primary(placed("other"));
		mapping.map_prefix("p", placed(COMPOSED));
		mapping.map_prefix("q", placed(DECOMPOSED));
		mapping
	}

	#[cfg(target_os = "macos")]
	fn agree_to_everything() -> RestoreSelection {
		RestoreSelection {
			overwrite_existing: true,
			confirmed_placed: (0..3).collect(),
			..Default::default()
		}
	}

	/// Two routes to the two forms of one new folder, each writing `a.txt`:
	/// one file written twice, refused by planning.
	#[test]
	fn two_routes_to_the_two_forms_of_one_folder_collide() {
		let (_tmp, ws) = tmp();
		let result = plan_import(
			"// file: p/a.txt\nA\n// file: q/a.txt\nB\n",
			"",
			std::slice::from_ref(&ws),
			&two_forms_mapping(&ws),
		);
		assert!(
			matches!(result, Err(TransferError::TargetCollision { .. })),
			"{result:?}"
		);
		assert_eq!(scan(&ws), Vec::<PathBuf>::new());
	}

	/// The same two writes in a plan that did not come from planning here
	/// (`from_parts`): Apply refuses them itself, before writing anything,
	/// where the filesystem says they are one file.
	#[cfg(target_os = "macos")]
	#[test]
	fn apply_refuses_two_writes_to_the_two_forms_of_one_file() {
		let (_tmp, ws) = tmp();
		let plan = plan_import(
			"// file: p/a.txt\nA\n// file: q/b.txt\nB\n",
			"",
			std::slice::from_ref(&ws),
			&two_forms_mapping(&ws),
		)
		.unwrap();
		let mut restore_plan = plan.restore_plan().clone();
		for op in &mut restore_plan.create_operations {
			let form = if op.content.starts_with('A') {
				COMPOSED
			} else {
				DECOMPOSED
			};
			op.absolute_path = ws.join(form).join("a.txt");
			op.relative_path = "a.txt".to_string();
			op.root_path = ws.join(form);
		}
		let plan = TransferImportPlan::from_parts(
			plan.roots().to_vec(),
			restore_plan,
			plan.destination_freshness().clone(),
		);
		match plan.apply(&agree_to_everything()) {
			Err(TransferError::TargetCollision { .. }) => {}
			other => panic!("expected TargetCollision, got {other:?}"),
		}
		assert_eq!(scan(&ws), Vec::<PathBuf>::new());
	}
}

#[cfg(test)]
mod fs_decides_tests;

#[cfg(test)]
mod fd_budget_tests;
