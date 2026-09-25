//! Transfer planning: safe multi-repo export and import planning with freshness validation.
//!
//! Provides pure/shared planning for exporting from and importing to Git repositories
//! and workspace roots, preserving v1 wire format compatibility while enforcing
//! explicit root mapping, conflict detection, cumulative bounded reads, and freshness validation.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::commits::{self, CommitError, CommitsPayload};
use crate::filter;
use crate::format::{
	self, BorrowedPayloadOptions, ChangeType, ParsedEntry, PayloadFile,
};
use crate::fsutil::decode_utf8_or_skip;
use crate::gitsrc::{
	Git, GitError, DELETED_FILE_MARKER, UNREADABLE_FILE_MARKER,
};
use crate::paths::{self, escapes_all_roots, sanitize_relative_path};
use crate::restore::{
	self, CreateOperation, DeleteOperation, RestoreExecutionResult,
	RestorePlan, RestoreSelection, SkipReason, SkippedOperation,
};
use crate::settings::Settings;
use crate::stats::{payload_stats, PayloadStats};

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
}

impl std::fmt::Display for CanonicalRootId {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0.display())
	}
}

/// Where a file was selected from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
	/// Workspace file mode (equivalent to copy.rs file collection).
	File,
	/// Uncommitted Git working tree change.
	Working,
	/// Staged Git index change.
	Staged,
	/// Git commit change.
	Commit { rev: String },
}

/// An individual item in an export selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportItem {
	pub root: CanonicalRootId,
	pub relative_path: String,
	pub source: SourceKind,
	pub change_type: Option<ChangeType>,
}

/// An export selection spanning one or more verified canonical roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSelection {
	pub roots: Vec<CanonicalRootId>,
	pub primary_root: Option<CanonicalRootId>,
	pub items: Vec<ExportItem>,
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
		};
		validate_export_selection(&sel)?;
		Ok(sel)
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
	},
	#[error("stale source in '{}': {reason}", .root.display())]
	StaleSource { root: PathBuf, reason: String },
	#[error("stale destination in '{}': {reason}", .root.display())]
	StaleDestination { root: PathBuf, reason: String },
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
	#[error(transparent)]
	Git(#[from] GitError),
	#[error(transparent)]
	Commit(#[from] CommitError),
	#[error(transparent)]
	Io(#[from] io::Error),
}

/// Cryptographic and timestamp identity of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFreshness {
	pub size: u64,
	pub mtime: SystemTime,
	pub content_hash: [u8; 32],
}

/// Freshness state of a repository (HEAD commit, symbolic ref, and index).
#[derive(Debug, Clone, PartialEq, Eq)]
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

		let mut repos = HashMap::new();
		for root in &selection.roots {
			repos.insert(root.clone(), capture_repo_freshness(root.path())?);
		}

		let mut frozen_commits = HashMap::new();
		let mut working_files = HashMap::new();

		for item in &selection.items {
			match &item.source {
				SourceKind::Working | SourceKind::File => {
					let file_path = item.root.path().join(&item.relative_path);
					let freshness = capture_file_freshness(&file_path)?;
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
						let git = Git::open(item.root.path())?;
						let oid = git.resolve_commit(rev)?;
						e.insert(oid);
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
		for (root, prev_repo) in &self.repos {
			revalidate_repo_freshness(root.path(), prev_repo)?;
		}

		for ((root, rel), prev_file) in &self.working_files {
			let file_path = root.path().join(rel);
			let current = capture_file_freshness(&file_path)?;
			if current != *prev_file {
				return Err(TransferError::StaleSource {
					root: root.path().to_path_buf(),
					reason: format!(
						"working file '{rel}' was modified or deleted externally"
					),
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
		self.freshness.revalidate()
	}
}

/// Explicit destination mapping for a specific entry path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMapping {
	pub root: CanonicalRootId,
	pub relative_path: Option<String>,
}

/// Explicit destination mappings for importing clipboard payloads.
#[derive(Debug, Clone, Default)]
pub struct ImportMapping {
	pub primary_destination: Option<CanonicalRootId>,
	pub prefix_destinations: HashMap<String, CanonicalRootId>,
	pub entry_destinations: HashMap<String, EntryMapping>,
	pub blocked_prefixes: HashSet<String>,
}

impl ImportMapping {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn with_primary(root: CanonicalRootId) -> Self {
		Self {
			primary_destination: Some(root),
			..Default::default()
		}
	}

	pub fn map_prefix(
		&mut self,
		prefix: impl Into<String>,
		root: CanonicalRootId,
	) -> &mut Self {
		self.prefix_destinations.insert(prefix.into(), root);
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFileFreshness {
	pub root: CanonicalRootId,
	pub relative_path: String,
	pub existed: bool,
	pub file_state: Option<FileFreshness>,
}

/// Destination freshness snapshot captured during import preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationFreshnessSnapshot {
	pub roots: HashMap<CanonicalRootId, RepoFreshness>,
	pub target_files: HashMap<PathBuf, TargetFileFreshness>,
}

impl DestinationFreshnessSnapshot {
	pub fn capture(
		destination_roots: &[PathBuf],
		plan: &RestorePlan,
	) -> Result<Self, TransferError> {
		let mut roots = HashMap::new();
		for root in destination_roots {
			let id = CanonicalRootId::new(root)?;
			id.validate()?;
			roots.insert(id, capture_repo_freshness(root)?);
		}

		let mut target_files = HashMap::new();
		for op in &plan.create_operations {
			let file_state = capture_file_freshness(&op.absolute_path)?;
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
			let file_state = capture_file_freshness(&op.absolute_path)?;
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

		Ok(Self {
			roots,
			target_files,
		})
	}

	pub fn revalidate(&self) -> Result<(), TransferError> {
		for (root, prev_repo) in &self.roots {
			revalidate_destination_repo_freshness(root.path(), prev_repo)?;
		}

		for (path, target) in &self.target_files {
			let current_exists = path.exists();
			if !target.existed && current_exists {
				return Err(TransferError::StaleDestination {
					root: target.root.path().to_path_buf(),
					reason: format!(
						"target file '{}' was created externally after preview",
						target.relative_path
					),
				});
			}
			if target.existed && !current_exists {
				return Err(TransferError::StaleDestination {
					root: target.root.path().to_path_buf(),
					reason: format!(
						"target file '{}' was deleted externally after preview",
						target.relative_path
					),
				});
			}
			if target.existed && current_exists {
				let current_state = capture_file_freshness(path)?;
				if current_state != target.file_state {
					return Err(TransferError::StaleDestination {
						root: target.root.path().to_path_buf(),
						reason: format!(
							"target file '{}' was modified externally after preview",
							target.relative_path
						),
					});
				}
			}
		}

		Ok(())
	}
}

/// An immutable import plan containing the exact file restore operations and freshness token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferImportPlan {
	roots: Vec<PathBuf>,
	restore_plan: RestorePlan,
	destination_freshness: DestinationFreshnessSnapshot,
}

impl TransferImportPlan {
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

	/// Applies the planned restore after verifying destination freshness.
	/// If any destination repo or file changed after preview, returns a stale error
	/// and writes nothing.
	pub fn apply(
		&self,
		selection: &RestoreSelection,
	) -> Result<RestoreExecutionResult, TransferError> {
		self.destination_freshness.revalidate()?;
		let result =
			restore::execute_restore_plan(&self.restore_plan, selection);
		Ok(result)
	}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn not_found_as_none<T>(
	res: io::Result<T>,
) -> Result<Option<T>, TransferError> {
	match res {
		Ok(v) => Ok(Some(v)),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(TransferError::Io(e)),
	}
}

fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
	let mut file = fs::File::open(path)?;
	let mut hasher = Sha256::new();
	let mut buf = [0u8; 8192];
	loop {
		let n = file.read(&mut buf)?;
		if n == 0 {
			break;
		}
		hasher.update(&buf[..n]);
	}
	Ok(hasher.finalize().into())
}

fn capture_file_freshness(
	path: &Path,
) -> Result<Option<FileFreshness>, TransferError> {
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
		return Err(TransferError::SpecialFile(
			path.to_string_lossy().into_owned(),
		));
	}
	let size = target_meta.len();
	let mtime = target_meta.modified()?;
	let content_hash = hash_file(path)?;
	Ok(Some(FileFreshness {
		size,
		mtime,
		content_hash,
	}))
}

fn capture_repo_freshness(root: &Path) -> Result<RepoFreshness, TransferError> {
	let git = match Git::open(root) {
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

	let head_commit =
		match git.run(&["rev-parse", "--verify", "--quiet", "HEAD"]) {
			Ok(out) => {
				let s = String::from_utf8_lossy(&out).trim().to_string();
				if s.is_empty() {
					None
				} else {
					Some(s)
				}
			}
			Err(GitError::Failed { ref stderr, .. })
				if stderr.is_empty()
					|| stderr.contains("Needed a single revision") =>
			{
				None
			}
			Err(e) => return Err(TransferError::Git(e)),
		};

	let head_ref = match git.run(&["symbolic-ref", "--quiet", "HEAD"]) {
		Ok(out) => Some(String::from_utf8_lossy(&out).trim().to_string()),
		Err(GitError::Failed { ref stderr, .. })
			if stderr.is_empty() || stderr.contains("not a symbolic ref") =>
		{
			None
		}
		Err(e) => return Err(TransferError::Git(e)),
	};

	let index_hash = match git.run(&["rev-parse", "--git-path", "index"]) {
		Ok(out) => {
			let git_path_str = String::from_utf8_lossy(&out).trim().to_string();
			let index_path = if Path::new(&git_path_str).is_absolute() {
				PathBuf::from(git_path_str)
			} else {
				git.root().join(git_path_str)
			};
			if not_found_as_none(fs::metadata(&index_path))?.is_some() {
				Some(hash_file(&index_path)?)
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
) -> Result<(), TransferError> {
	let current = capture_repo_freshness(root)?;
	if current.head_commit != prev.head_commit {
		return Err(TransferError::StaleSource {
			root: root.to_path_buf(),
			reason: format!(
				"HEAD commit changed from {} to {}",
				prev.head_commit.as_deref().unwrap_or("<none>"),
				current.head_commit.as_deref().unwrap_or("<none>")
			),
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
		});
	}
	if current.index_hash != prev.index_hash {
		return Err(TransferError::StaleSource {
			root: root.to_path_buf(),
			reason: "Git index changed".to_string(),
		});
	}
	Ok(())
}

fn revalidate_destination_repo_freshness(
	root: &Path,
	prev: &RepoFreshness,
) -> Result<(), TransferError> {
	let current = capture_repo_freshness(root)?;
	if current.head_commit != prev.head_commit {
		return Err(TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: format!(
				"destination HEAD commit changed from {} to {}",
				prev.head_commit.as_deref().unwrap_or("<none>"),
				current.head_commit.as_deref().unwrap_or("<none>")
			),
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
		});
	}
	if current.index_hash != prev.index_hash {
		return Err(TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: "destination Git index changed".to_string(),
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

/// Resolves target identity across symlinks and platform case folding.
fn canonical_target_identity(path: &Path) -> String {
	if let Ok(c) = dunce::canonicalize(path) {
		return paths::path_key(&c.to_string_lossy());
	}
	let mut current = path.to_path_buf();
	let mut trail = Vec::new();
	while !current.exists() {
		if let Some(name) = current.file_name() {
			trail.push(name.to_os_string());
		}
		if !current.pop() {
			break;
		}
	}
	let canonical_ancestor = dunce::canonicalize(&current).unwrap_or(current);
	let mut full = canonical_ancestor;
	for segment in trail.into_iter().rev() {
		full.push(segment);
	}
	paths::path_key(&full.to_string_lossy())
}

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
		if escapes_all_roots(&[item.root.path()], &full) {
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
		let is_worktree =
			matches!(item.source, SourceKind::Working | SourceKind::File);
		let has_worktree = sources.contains(&SourceKind::Working)
			|| sources.contains(&SourceKind::File);
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

/// Helper to inspect Git blob size before reading content.
fn git_blob_size(git: &Git, oid: &str) -> Result<u64, TransferError> {
	let out = git.run(&["cat-file", "-s", oid])?;
	let s = String::from_utf8_lossy(&out).trim().to_string();
	s.parse::<u64>().map_err(|_| {
		TransferError::Git(GitError::Malformed(
			"invalid cat-file -s output".into(),
		))
	})
}

/// Helper to resolve Git blob OID distinguishing missing objects from permission/corruption errors.
fn resolve_blob_oid(
	git: &Git,
	spec: &str,
) -> Result<Option<String>, TransferError> {
	match git.run(&["rev-parse", "--verify", spec]) {
		Ok(out) => {
			let oid = String::from_utf8_lossy(&out).trim().to_string();
			if oid.is_empty() {
				Ok(None)
			} else {
				Ok(Some(oid))
			}
		}
		Err(GitError::Failed { args, stderr }) => {
			if stderr.is_empty()
				|| stderr.contains("Needed a single revision")
				|| stderr.contains("Not a valid object name")
				|| stderr.contains("does not exist")
				|| stderr.contains("ambiguous argument")
			{
				Ok(None)
			} else {
				Err(TransferError::Git(GitError::Failed { args, stderr }))
			}
		}
		Err(e) => Err(TransferError::Git(e)),
	}
}

struct BlobBudget {
	remaining_budget: Option<usize>,
	max_payload_bytes: Option<usize>,
	current_total_bytes: usize,
	max_file_size_kb: f64,
	bypass_per_file_size: bool,
}

/// Helper to read Git blob by immutable OID with strict budget cap.
fn read_git_blob_bounded(
	git: &Git,
	oid: &str,
	wire_path: &str,
	budget: &BlobBudget,
) -> Result<(Option<String>, Option<String>, usize), TransferError> {
	let blob_size = git_blob_size(git, oid)?;
	if !budget.bypass_per_file_size
		&& blob_size as f64 > budget.max_file_size_kb * 1024.0
	{
		return Ok((
			None,
			Some(format!("size exceeds limit ({blob_size} bytes)")),
			0,
		));
	}
	if let Some(b) = budget.remaining_budget {
		if blob_size as usize > b {
			return Err(TransferError::PayloadLimitExceeded {
				limit: budget.max_payload_bytes.unwrap_or(b),
				actual: budget.current_total_bytes + blob_size as usize,
				reason: format!(
					"file '{wire_path}' exceeds remaining payload budget"
				),
			});
		}
	}
	let out = git.run(&["cat-file", "-p", oid])?;
	let text = decode_utf8_or_skip(out);
	let actual_size = text.as_ref().map_or(0, |s| s.len());
	Ok((text, None, actual_size))
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

/// Plans and generates an export payload across one or multiple repositories.
/// Freezes revisions once, enforces cumulative budget before retention, and revalidates freshness.
pub fn plan_export(
	selection: &ExportSelection,
	settings: &Settings,
	max_payload_bytes: Option<usize>,
) -> Result<ExportPlan, TransferError> {
	validate_export_selection(selection)?;

	// Freeze repo freshness and revisions once upfront (without hashing working files)
	let mut repos = HashMap::new();
	for root in &selection.roots {
		repos.insert(root.clone(), capture_repo_freshness(root.path())?);
	}

	let mut frozen_commits = HashMap::new();
	for item in &selection.items {
		if let SourceKind::Commit { rev } = &item.source {
			let key = (item.root.clone(), rev.clone());
			if let std::collections::hash_map::Entry::Vacant(e) =
				frozen_commits.entry(key)
			{
				let git = Git::open(item.root.path())?;
				let oid = git.resolve_commit(rev)?;
				e.insert(oid);
			}
		}
	}

	let mut working_files = HashMap::new();

	let primary = selection
		.primary_root
		.clone()
		.or_else(|| selection.roots.first().cloned());

	// Commit selections begin with fallback = true (omitting empty wrappers);
	// Staged and Deleted only trigger fallback once an included entry is admitted.
	let is_commit = selection
		.items
		.iter()
		.any(|item| matches!(item.source, SourceKind::Commit { .. }));
	let mut fallback = is_commit;

	let default_source_root = if selection.roots.len() == 1 {
		paths::source_root_name(&[selection.roots[0].path()])
	} else {
		None
	};

	let custom_pattern = format::HeaderPattern::new(&settings.header_format);
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
			});
		}
	}

	let mut files = Vec::new();
	let mut copied_file_count = 0usize;
	let mut skipped_file_size_count = 0usize;
	let mut skipped_unreadable_count = 0usize;
	let mut file_limit_reached = false;

	for item in &selection.items {
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
			if settings.use_filters
				&& !filter::file_matches_filters(
					&wire_path,
					&settings.filter_rules,
					settings.use_include_filters,
					settings.use_exclude_filters,
					Some(&absolute.to_string_lossy()),
				) {
				continue;
			}
		} else {
			if settings.use_filters
				&& !filter::file_matches_filters(
					&wire_path,
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

		let remaining_budget =
			max_payload_bytes.map(|m| m.saturating_sub(current_total_bytes));

		let blob_budget = |bypass: bool| BlobBudget {
			remaining_budget,
			max_payload_bytes,
			current_total_bytes,
			max_file_size_kb: settings.max_file_size_kb,
			bypass_per_file_size: bypass,
		};

		let (content, skipped_reason, _, freshness_info) = match &item.source {
			SourceKind::Commit { rev } => {
				let frozen_oid = frozen_commits
					.get(&(item.root.clone(), rev.clone()))
					.ok_or_else(|| {
						TransferError::Git(GitError::InvalidRevision(
							rev.clone(),
						))
					})?;
				let git = Git::open(item.root.path())?;

				if item.change_type == Some(ChangeType::Deleted) {
					let parents = git.parents(frozen_oid)?;
					let parent_oid = parents.first().cloned();
					let (text, reason, bytes) = if let Some(p) = parent_oid {
						let blob_rev = format!("{p}:{}", item.relative_path);
						if let Some(blob_oid) =
							resolve_blob_oid(&git, &blob_rev)?
						{
							// Deleted graph old content bypasses per-file size check!
							read_git_blob_bounded(
								&git,
								&blob_oid,
								&wire_path,
								&blob_budget(true),
							)?
						} else {
							(
								Some(DELETED_FILE_MARKER.to_string()),
								None,
								DELETED_FILE_MARKER.len(),
							)
						}
					} else {
						(
							Some(DELETED_FILE_MARKER.to_string()),
							None,
							DELETED_FILE_MARKER.len(),
						)
					};
					(text, reason, bytes, None)
				} else {
					let blob_rev =
						format!("{frozen_oid}:{}", item.relative_path);
					let blob_oid = resolve_blob_oid(&git, &blob_rev)?.ok_or(
						TransferError::Git(GitError::InvalidRevision(blob_rev)),
					)?;
					let (text, reason, bytes) = read_git_blob_bounded(
						&git,
						&blob_oid,
						&wire_path,
						&blob_budget(false),
					)?;
					(text, reason, bytes, None)
				}
			}
			SourceKind::Staged => {
				let git = Git::open(item.root.path())?;
				let (text, reason, bytes) = if item.change_type
					== Some(ChangeType::Deleted)
				{
					let blob_rev = format!("HEAD:{}", item.relative_path);
					if let Some(blob_oid) = resolve_blob_oid(&git, &blob_rev)? {
						read_git_blob_bounded(
							&git,
							&blob_oid,
							&wire_path,
							&blob_budget(false),
						)?
					} else {
						(
							Some(DELETED_FILE_MARKER.to_string()),
							None,
							DELETED_FILE_MARKER.len(),
						)
					}
				} else {
					let blob_rev = format!(":{}", item.relative_path);
					let blob_oid = resolve_blob_oid(&git, &blob_rev)?.ok_or(
						TransferError::Git(GitError::InvalidRevision(blob_rev)),
					)?;
					let (text, reason, bytes) = read_git_blob_bounded(
						&git,
						&blob_oid,
						&wire_path,
						&blob_budget(false),
					)?;
					if reason.is_none() && text.is_none() {
						(
							Some(UNREADABLE_FILE_MARKER.to_string()),
							None,
							UNREADABLE_FILE_MARKER.len(),
						)
					} else {
						(text, reason, bytes)
					}
				};
				(text, reason, bytes, None)
			}
			SourceKind::Working | SourceKind::File => {
				if item.change_type == Some(ChangeType::Deleted) {
					let git = Git::open(item.root.path());
					let mut resolved = None;
					if let Ok(ref g) = git {
						for spec in &[
							format!(":{}", item.relative_path),
							format!("HEAD:{}", item.relative_path),
						] {
							if let Some(oid) = resolve_blob_oid(g, spec)? {
								resolved = Some((g, oid));
								break;
							}
						}
					}
					let (text, reason, bytes) =
						if let Some((g, blob_oid)) = resolved {
							read_git_blob_bounded(
								g,
								&blob_oid,
								&wire_path,
								&blob_budget(false),
							)?
						} else {
							(
								Some(DELETED_FILE_MARKER.to_string()),
								None,
								DELETED_FILE_MARKER.len(),
							)
						};
					(text, reason, bytes, None)
				} else {
					let sym_meta = fs::symlink_metadata(&absolute)?;
					let target_meta = if sym_meta.file_type().is_symlink() {
						let roots = [item.root.path()];
						if escapes_all_roots(&roots, &absolute) {
							return Err(TransferError::UnsafePath(
								absolute.to_string_lossy().into_owned(),
							));
						}
						let canonical = dunce::canonicalize(&absolute)?;
						if escapes_all_roots(&roots, &canonical) {
							return Err(TransferError::UnsafePath(
								canonical.to_string_lossy().into_owned(),
							));
						}
						fs::metadata(&canonical)?
					} else {
						sym_meta
					};

					if !target_meta.file_type().is_file() {
						return Err(TransferError::SpecialFile(
							absolute.to_string_lossy().into_owned(),
						));
					}

					let file_size = target_meta.len();
					let per_file_limit =
						(settings.max_file_size_kb * 1024.0) as u64;
					if file_size as f64 > settings.max_file_size_kb * 1024.0 {
						(
							None,
							Some(format!(
								"size exceeds limit ({file_size} bytes)"
							)),
							0,
							None,
						)
					} else {
						if let Some(budget) = remaining_budget {
							if file_size as usize > budget {
								return Err(
									TransferError::PayloadLimitExceeded {
										limit: max_payload_bytes
											.unwrap_or(budget),
										actual: current_total_bytes
											+ file_size as usize,
										reason: format!(
											"file '{wire_path}' exceeds remaining payload budget"
										),
									},
								);
							}
						}
						let read_cap = match remaining_budget {
							Some(b) => (b as u64).min(per_file_limit),
							None => per_file_limit,
						};
						let mut file = fs::File::open(&absolute)?;
						let mut handle = (&mut file).take(read_cap + 1);
						let mut bytes = Vec::with_capacity(file_size as usize);
						handle.read_to_end(&mut bytes)?;
						if bytes.len() as u64 > read_cap {
							if remaining_budget.is_some_and(|b| bytes.len() > b)
							{
								return Err(
									TransferError::PayloadLimitExceeded {
										limit: max_payload_bytes
											.unwrap_or(read_cap as usize),
										actual: current_total_bytes
											+ bytes.len(),
										reason: format!(
											"working file '{wire_path}' grew past limit during read"
										),
									},
								);
							} else {
								(
									None,
									Some(format!(
										"size exceeds limit ({} bytes)",
										bytes.len()
									)),
									0,
									None,
								)
							}
						} else {
							let mtime = target_meta.modified()?;
							let content_hash = Sha256::digest(&bytes).into();
							let freshness = FileFreshness {
								size: bytes.len() as u64,
								mtime,
								content_hash,
							};
							let text = decode_utf8_or_skip(bytes);
							let actual_len =
								text.as_ref().map_or(0, |s| s.len());
							(text, None, actual_len, Some(freshness))
						}
					}
				}
			}
		};

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
		.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;

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
				});
			}
		}
		current_total_bytes += file_delta;

		if let Some(f) = freshness_info {
			working_files.insert(
				(item.root.clone(), item.relative_path.clone()),
				Some(f),
			);
		}

		files.push(payload_file);
	}

	let final_source_root = if is_commit && files.is_empty() {
		None
	} else {
		default_source_root
	};

	let freshness = SourceFreshnessSnapshot {
		repos,
		frozen_commits,
		working_files,
	};
	freshness.revalidate()?;

	let mut final_counter = format::CountingWriter::default();
	format::write_payload_borrowed(
		&mut final_counter,
		&make_payload_opts(
			settings,
			final_source_root.as_deref(),
			&files,
			!fallback,
		),
	)
	.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;

	if let Some(max_bytes) = max_payload_bytes {
		if final_counter.count > max_bytes {
			return Err(TransferError::PayloadLimitExceeded {
				limit: max_bytes,
				actual: final_counter.count,
				reason:
					"serialized payload with headers/wrappers exceeds limit"
						.to_string(),
			});
		}
	}

	let mut payload = String::with_capacity(final_counter.count);
	format::write_payload_borrowed(
		&mut payload,
		&make_payload_opts(
			settings,
			final_source_root.as_deref(),
			&files,
			!fallback,
		),
	)
	.map_err(|_| TransferError::UnsafePath("formatting error".into()))?;

	let stats = payload_stats(&payload);

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
	let mut canonical_dest_roots = Vec::with_capacity(destination_roots.len());
	for r in destination_roots {
		let id = CanonicalRootId::new(r)?;
		id.validate()?;
		canonical_dest_roots.push(id);
	}

	// Every mapped destination root must belong to the explicit destination_roots
	if let Some(ref primary) = mapping.primary_destination {
		if !canonical_dest_roots.contains(primary) {
			return Err(TransferError::UnknownRoot(
				primary.path().to_path_buf(),
			));
		}
	}
	for root in mapping.prefix_destinations.values() {
		if !canonical_dest_roots.contains(root) {
			return Err(TransferError::UnknownRoot(root.path().to_path_buf()));
		}
	}
	for entry_mapping in mapping.entry_destinations.values() {
		if !canonical_dest_roots.contains(&entry_mapping.root) {
			return Err(TransferError::UnknownRoot(
				entry_mapping.root.path().to_path_buf(),
			));
		}
	}

	let entries = format::parse_clipboard(clipboard_text, header_format);

	let mut root_to_entries: HashMap<CanonicalRootId, Vec<ParsedEntry>> =
		HashMap::new();
	let mut skipped_operations = Vec::new();

	for entry in entries {
		let (target_root, rel_path) =
			if let Some(em) = mapping.entry_destinations.get(&entry.path) {
				let rel =
					em.relative_path.as_deref().unwrap_or(entry.path.as_str());
				(&em.root, rel)
			} else if let Some((prefix, rest)) = entry.path.split_once('/') {
				if mapping.blocked_prefixes.contains(prefix) {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
					});
					continue;
				}
				if let Some(dest) = mapping.prefix_destinations.get(prefix) {
					// Prefix consumed EXACTLY ONCE
					(dest, rest)
				} else if let Some(ref primary) = mapping.primary_destination {
					(primary, entry.path.as_str())
				} else {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
					});
					continue;
				}
			} else if let Some(ref primary) = mapping.primary_destination {
				(primary, entry.path.as_str())
			} else {
				skipped_operations.push(SkippedOperation {
					raw_path: entry.path.clone(),
					relative_path: None,
					reason: SkipReason::UnresolvedPath,
				});
				continue;
			};

		let Some(sanitized) = sanitize_relative_path(rel_path) else {
			skipped_operations.push(SkippedOperation {
				raw_path: entry.path.clone(),
				relative_path: None,
				reason: SkipReason::UnresolvedPath,
			});
			continue;
		};

		root_to_entries
			.entry(target_root.clone())
			.or_default()
			.push(ParsedEntry {
				path: sanitized,
				content: entry.content,
				change_types: entry.change_types,
			});
	}

	let mut combined_creates = Vec::new();
	let mut combined_deletes = Vec::new();
	let mut all_skipped = skipped_operations;

	// Call core plan_restore for each destination root
	for (root_id, root_entries) in root_to_entries {
		let sub_plan = restore::plan_restore(&[root_id.path()], &root_entries);
		combined_creates.extend(sub_plan.create_operations);
		combined_deletes.extend(sub_plan.delete_operations);
		all_skipped.extend(sub_plan.skipped_operations);
	}

	// Reject if two entries map to the same target file (including symlinks and case aliases)
	let mut target_identities: HashMap<String, String> = HashMap::new();
	for op in &combined_creates {
		let identity = canonical_target_identity(&op.absolute_path);
		if let Some(prev) = target_identities
			.insert(identity.clone(), format!("create {}", op.relative_path))
		{
			return Err(TransferError::TargetCollision {
				path: op.absolute_path.clone(),
				msg: format!(
					"multiple operations target '{}' (identity '{}'): previous was '{}', current is 'create {}'",
					op.absolute_path.display(),
					identity,
					prev,
					op.relative_path
				),
			});
		}
	}
	for op in &combined_deletes {
		let identity = canonical_target_identity(&op.absolute_path);
		if let Some(prev) = target_identities
			.insert(identity.clone(), format!("delete {}", op.relative_path))
		{
			return Err(TransferError::TargetCollision {
				path: op.absolute_path.clone(),
				msg: format!(
					"multiple operations target '{}' (identity '{}'): previous was '{}', current is 'delete {}'",
					op.absolute_path.display(),
					identity,
					prev,
					op.relative_path
				),
			});
		}
	}

	let restore_plan = RestorePlan {
		roots: destination_roots.to_vec(),
		create_operations: combined_creates,
		delete_operations: combined_deletes,
		skipped_operations: all_skipped,
	};

	let destination_freshness = DestinationFreshnessSnapshot::capture(
		destination_roots,
		&restore_plan,
	)?;

	Ok(TransferImportPlan {
		roots: destination_roots.to_vec(),
		restore_plan,
		destination_freshness,
	})
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

/// Plans and extracts commits along a contiguous first-parent chain.
pub fn plan_commit_export(
	git: &Git,
	range: Option<(&str, &str)>,
	last: Option<usize>,
) -> Result<CommitsPayload, TransferError> {
	let shas = match (range, last) {
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
			})?,
		(None, Some(n)) => {
			commits::select_last(git, n).map_err(TransferError::Commit)?
		}
		_ => return Err(TransferError::EmptySelection),
	};

	commits::copy_commits(git, &shas).map_err(TransferError::Commit)
}
