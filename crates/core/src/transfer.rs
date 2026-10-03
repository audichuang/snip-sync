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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::blob::{BlobRead, BlobReader, DeletedContent};
use crate::commits::{self, CommitError, CommitExport, CommitsPayload};
use crate::filter;
use crate::format::{
	self, BorrowedPayloadOptions, ChangeType, ParsedEntry, PayloadFile,
};
use crate::fsutil::decode_utf8_or_skip;
use crate::gitrun::{CancelToken, RunOptions, RunOutput};
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

mod changes;
mod select;

pub use changes::{changed_items, ChangedItems};
pub use select::{
	expand_folder_items, expand_folder_items_in_input_order,
	is_safe_dir_symlink, plan_export_expanding, selection_from_paths,
	FolderExpansion, PathSelection,
};

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
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
	#[error("no files or changes selected for export")]
	EmptySelection,
	#[error("unsafe path: '{0}' escapes root boundaries or contains invalid characters")]
	UnsafePath(String),
	#[error("special file '{0}' is not a regular file and cannot be exported")]
	SpecialFile(String),
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
					let freshness = capture_file_freshness(&file_path, &opts)?;
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
			let current = capture_file_freshness(&file_path, opts)?;
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
	pub primary_destination: Option<CanonicalRootId>,
	/// Nest every entry that lands in the primary destination under this directory (applies to relative entries only).
	pub primary_prefix: Option<String>,
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

	pub fn from_restore_base(
		suggestion: &restore::RestoreBaseSuggestion,
		primary: CanonicalRootId,
	) -> Self {
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
			let file_state = capture_file_freshness(&op.absolute_path, opts)?;
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
			let file_state = capture_file_freshness(&op.absolute_path, opts)?;
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

		cancelled_err(opts, "import-freshness")?;
		Ok(Self {
			roots,
			target_files,
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
			let file_state = capture_file_freshness(abs, &opts)?;
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
		})
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
				let current_state = capture_file_freshness(path, &opts)?;
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
		return Err(TransferError::SpecialFile(
			path.to_string_lossy().into_owned(),
		));
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

pub(crate) fn fs_is_case_insensitive(root: &Path) -> bool {
	probe_fs_case_insensitive(root)
		.unwrap_or(cfg!(any(windows, target_os = "macos")))
}

fn swap_ascii_case(s: &str) -> Option<String> {
	let mut swapped = String::with_capacity(s.len());
	let mut has_ascii_alpha = false;
	for c in s.chars() {
		if c.is_ascii_alphabetic() {
			has_ascii_alpha = true;
			if c.is_ascii_lowercase() {
				swapped.push(c.to_ascii_uppercase());
			} else {
				swapped.push(c.to_ascii_lowercase());
			}
		} else {
			swapped.push(c);
		}
	}
	if has_ascii_alpha {
		Some(swapped)
	} else {
		None
	}
}

#[cfg(unix)]
fn is_same_file(p1: &Path, p2: &Path) -> io::Result<bool> {
	use std::os::unix::fs::MetadataExt;
	let m1 = fs::symlink_metadata(p1)?;
	let m2 = fs::symlink_metadata(p2)?;
	Ok(m1.dev() == m2.dev() && m1.ino() == m2.ino())
}

#[cfg(not(unix))]
fn is_same_file(p1: &Path, p2: &Path) -> io::Result<bool> {
	let c1 = dunce::canonicalize(p1)?;
	let c2 = dunce::canonicalize(p2)?;
	Ok(c1 == c2)
}

fn probe_fs_case_insensitive(root: &Path) -> io::Result<bool> {
	if let Ok(entries) = fs::read_dir(root) {
		for entry in entries.flatten() {
			let name = entry.file_name();
			let name_str = name.to_string_lossy();
			if let Some(swapped) = swap_ascii_case(&name_str) {
				let orig_path = root.join(&name);
				let swapped_path = root.join(&swapped);
				match fs::symlink_metadata(&swapped_path) {
					Ok(_) => {
						if let Ok(same) =
							is_same_file(&orig_path, &swapped_path)
						{
							return Ok(same);
						}
					}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {
						return Ok(false);
					}
					Err(_) => {}
				}
			}
		}
	}

	let pid = std::process::id();
	let nanos = SystemTime::now()
		.duration_since(SystemTime::UNIX_EPOCH)
		.map(|d| d.as_nanos())
		.unwrap_or(0);
	let probe_name = format!(".snip-case-probe-{pid}-{nanos}");
	let probe_path = root.join(&probe_name);
	let swapped_name = format!(".SNIP-CASE-PROBE-{pid}-{nanos}");
	let swapped_path = root.join(&swapped_name);

	let probe_file = fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&probe_path);

	match probe_file {
		Ok(f) => {
			drop(f);
			let is_ci = match fs::symlink_metadata(&swapped_path) {
				Ok(_) => {
					is_same_file(&probe_path, &swapped_path).unwrap_or(true)
				}
				Err(e) if e.kind() == io::ErrorKind::NotFound => false,
				Err(_) => cfg!(any(windows, target_os = "macos")),
			};
			let _ = fs::remove_file(&probe_path);
			let _ = fs::remove_file(&swapped_path);
			Ok(is_ci)
		}
		Err(e) => Err(e),
	}
}

fn op_is_case_insensitive(
	op_root: Option<&Path>,
	op_absolute: &Path,
	canonical_dest_roots: &[CanonicalRootId],
	root_case_insensitive: &HashMap<PathBuf, bool>,
	primary: Option<&CanonicalRootId>,
) -> bool {
	if let Some(r) = op_root {
		if let Some(&ci) = root_case_insensitive.get(r) {
			return ci;
		}
		for cr in canonical_dest_roots {
			if cr.path() == r
				|| paths::path_key(&cr.path().to_string_lossy())
					== paths::path_key(&r.to_string_lossy())
			{
				if let Some(&ci) = root_case_insensitive.get(cr.path()) {
					return ci;
				}
			}
		}
	}
	for cr in canonical_dest_roots {
		if op_absolute.starts_with(cr.path()) {
			if let Some(&ci) = root_case_insensitive.get(cr.path()) {
				return ci;
			}
		}
	}
	if let Some(p) = primary {
		if let Some(&ci) = root_case_insensitive.get(p.path()) {
			return ci;
		}
	}
	if let Some(cr) = canonical_dest_roots.first() {
		if let Some(&ci) = root_case_insensitive.get(cr.path()) {
			return ci;
		}
	}
	cfg!(any(windows, target_os = "macos"))
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
						let spec = format!(":{toplevel_rel}");
						let b = blob_budget(false);
						let cap = b.cap();
						let read = blobs.read_lenient(git, &spec, cap)?;
						gitlink_read(read, cap, &wire_path, &b, true)?
					} else {
						let spec = format!(":{toplevel_rel}");
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
						format!(":{toplevel_rel}")
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
	let mut canonical_dest_roots = Vec::new();
	for r in destination_roots {
		cancelled_err(opts, "plan-import")?;
		let id = CanonicalRootId::new(r)?;
		id.validate()?;
		canonical_dest_roots.push(id);
	}

	// Every mapped destination root must belong to the explicit destination_roots
	if let Some(ref primary) = mapping.primary_destination {
		cancelled_err(opts, "plan-import")?;
		if !canonical_dest_roots.contains(primary) {
			return Err(TransferError::UnknownRoot(
				primary.path().to_path_buf(),
			));
		}
	}
	for root in mapping.prefix_destinations.values() {
		cancelled_err(opts, "plan-import")?;
		if !canonical_dest_roots.contains(root) {
			return Err(TransferError::UnknownRoot(root.path().to_path_buf()));
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

	let mut root_case_insensitive: HashMap<PathBuf, bool> = HashMap::new();
	for root in &canonical_dest_roots {
		cancelled_err(opts, "plan-import")?;
		root_case_insensitive
			.entry(root.path().to_path_buf())
			.or_insert_with(|| fs_is_case_insensitive(root.path()));
	}

	let dest_root_paths: Vec<&Path> =
		canonical_dest_roots.iter().map(|id| id.path()).collect();
	let primary_dest_path =
		mapping.primary_destination.as_ref().map(|id| id.path());

	cancelled_err(opts, "plan-import")?;
	let entries = format::parse_clipboard(clipboard_text, header_format);
	cancelled_err(opts, "plan-import")?;

	let mut planned_entries: Vec<(CanonicalRootId, ParsedEntry)> = Vec::new();
	let mut skipped_operations = Vec::new();

	for entry in entries {
		cancelled_err(opts, "plan-import")?;
		let (target_root, rel_path) = if let Some(em) =
			mapping.entry_destinations.get(&entry.path)
		{
			let rel =
				em.relative_path.as_deref().unwrap_or(entry.path.as_str());
			(&em.root, std::borrow::Cow::Borrowed(rel))
		} else {
			let normalized = paths::normalize_system_path(&entry.path);
			if paths::is_absolute_path(&normalized) {
				if let Some((root_str, rel)) =
					paths::resolve_absolute_import_candidate(
						&dest_root_paths,
						primary_dest_path,
						&entry.path,
					) {
					if rel.is_empty() {
						skipped_operations.push(SkippedOperation {
							raw_path: entry.path.clone(),
							relative_path: None,
							reason: SkipReason::UnresolvedPath,
						});
						continue;
					}
					let matched_root = canonical_dest_roots.iter().find(|id| {
						paths::path_key(&id.path().to_string_lossy())
							== paths::path_key(&root_str)
							|| dunce::canonicalize(Path::new(&root_str))
								.map(|c| {
									paths::path_key(&c.to_string_lossy())
										== paths::path_key(
											&id.path().to_string_lossy(),
										)
								})
								.unwrap_or(false)
					});
					match matched_root {
						Some(root) => (root, std::borrow::Cow::Owned(rel)),
						None => {
							skipped_operations.push(SkippedOperation {
								raw_path: entry.path.clone(),
								relative_path: None,
								reason: SkipReason::UnresolvedPath,
							});
							continue;
						}
					}
				} else if entry.change_types.contains(&ChangeType::Deleted) {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
					});
					continue;
				} else if let Some(ref primary) = mapping.primary_destination {
					(primary, std::borrow::Cow::Borrowed(entry.path.as_str()))
				} else {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
					});
					continue;
				}
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
					(dest, std::borrow::Cow::Borrowed(rest))
				} else if let Some(ref primary) = mapping.primary_destination {
					let rel = match &mapping.primary_prefix {
						Some(p) if restore::is_relative(&entry.path) => {
							std::borrow::Cow::Owned(format!(
								"{p}/{}",
								entry.path
							))
						}
						_ => std::borrow::Cow::Borrowed(entry.path.as_str()),
					};
					(primary, rel)
				} else {
					skipped_operations.push(SkippedOperation {
						raw_path: entry.path.clone(),
						relative_path: None,
						reason: SkipReason::UnresolvedPath,
					});
					continue;
				}
			} else if let Some(ref primary) = mapping.primary_destination {
				let rel = match &mapping.primary_prefix {
					Some(p) if restore::is_relative(&entry.path) => {
						std::borrow::Cow::Owned(format!("{p}/{}", entry.path))
					}
					_ => std::borrow::Cow::Borrowed(entry.path.as_str()),
				};
				(primary, rel)
			} else {
				skipped_operations.push(SkippedOperation {
					raw_path: entry.path.clone(),
					relative_path: None,
					reason: SkipReason::UnresolvedPath,
				});
				continue;
			}
		};

		let Some(sanitized) = sanitize_relative_path(&rel_path) else {
			skipped_operations.push(SkippedOperation {
				raw_path: entry.path.clone(),
				relative_path: None,
				reason: SkipReason::UnresolvedPath,
			});
			continue;
		};

		planned_entries.push((
			target_root.clone(),
			ParsedEntry {
				path: sanitized,
				content: entry.content,
				change_types: entry.change_types,
			},
		));
	}

	let mut combined_creates = Vec::new();
	let mut combined_deletes = Vec::new();
	let mut all_skipped = skipped_operations;

	// Same `plan_restore` as before, one entry at a time so a cancel is
	// observed before the next file's encoding read.
	for (root_id, entry) in &planned_entries {
		cancelled_err(opts, "plan-import")?;
		let sub_plan = restore::plan_restore(
			&[root_id.path()],
			std::slice::from_ref(entry),
		);
		combined_creates.extend(sub_plan.create_operations);
		combined_deletes.extend(sub_plan.delete_operations);
		all_skipped.extend(sub_plan.skipped_operations);
	}

	// Reject if two entries map to the same target file (including symlinks and case aliases)
	let mut target_identities: HashMap<String, String> = HashMap::new();
	for op in &combined_creates {
		cancelled_err(opts, "plan-import")?;
		let is_ci = op_is_case_insensitive(
			Some(&op.root_path),
			&op.absolute_path,
			&canonical_dest_roots,
			&root_case_insensitive,
			mapping.primary_destination.as_ref(),
		);
		let mut identity = canonical_target_identity(&op.absolute_path);
		if is_ci {
			identity = identity.to_lowercase();
		}
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
		cancelled_err(opts, "plan-import")?;
		let is_ci = op_is_case_insensitive(
			None,
			&op.absolute_path,
			&canonical_dest_roots,
			&root_case_insensitive,
			mapping.primary_destination.as_ref(),
		);
		let mut identity = canonical_target_identity(&op.absolute_path);
		if is_ci {
			identity = identity.to_lowercase();
		}
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

	cancelled_err(opts, "plan-import")?;
	let destination_freshness = DestinationFreshnessSnapshot::capture_with(
		destination_roots,
		&restore_plan,
		opts,
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
	payload: CommitsPayload,
	replay: commits::CommitReplayPlan,
	freshness: DestinationFreshnessSnapshot,
}

impl CommitReplayPreview {
	pub fn destination(&self) -> &Path {
		&self.destination
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

	/// [`Self::capture`] with the caller's runner options.
	///
	/// Opens the destination with `Git::open_with`. Eligibility planning polls
	/// `opts` between files, then HEAD, the symbolic ref, the index and each
	/// target hash use the same options. Nothing is written.
	pub fn capture_with(
		dest: &Path,
		payload: &CommitsPayload,
		opts: &RunOptions,
	) -> Result<Self, TransferError> {
		cancelled_err(opts, "replay-preview")?;
		let git = Git::open_with(dest, opts)?;
		let root = git.root().to_path_buf();
		let replay = replay_plan(&git, payload, opts)?;
		let freshness = capture_replay_freshness(&root, &replay, opts)?;
		// Plan first, then the snapshot, then plan again. A change between
		// those reads makes the preview unusable instead of storing a mix.
		cancelled_err(opts, "replay-preview")?;
		let again = replay_plan(&git, payload, opts)?;
		if again != replay {
			return Err(TransferError::StaleDestination {
				root: root.clone(),
				reason:
					"destination changed while the replay preview was captured"
						.into(),
			});
		}
		revalidate_replay_freshness(&freshness, opts)?;
		let preview = Self {
			destination: root,
			payload: payload.clone(),
			replay,
			freshness,
		};
		#[cfg(test)]
		final_boundary_cancel_hook("replay-preview-capture");
		cancelled_err(opts, "replay-preview")?;
		Ok(preview)
	}

	pub fn apply(&self) -> Result<commits::ReplayResult, TransferError> {
		self.apply_with(&RunOptions::default())
	}

	/// Replays this preview's own payload onto its destination, or refuses as stale. `opts` reaches
	/// Git open, the worktree lock wait and the re-validation under that lock; once the first
	/// commit starts nothing is cancelled. Cancel at any point before the first write is
	/// `Err(TransferError::Git(GitError::Cancelled))`. A non-cancel failure inside
	/// `ReplaySession::begin` (the worktree lock, `RepoIdentity::resolve`, or creating the empty hooks directory) is
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
		let session =
			match commits::ReplaySession::begin(&git, &self.payload, opts) {
				Ok(s) => s,
				Err(refused) => {
					cancelled_err(opts, "replay-apply")?;
					return Ok(refused);
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
		let now = replay_plan(&git, &self.payload, opts)?;
		if now != self.replay {
			return Err(TransferError::StaleDestination {
				root: self.destination.clone(),
				reason: "replay eligibility changed after preview".into(),
			});
		}
		#[cfg(test)]
		final_boundary_cancel_hook("replay-preview-revalidate");
		cancelled_err(opts, "replay-preview")?;
		Ok(())
	}
}

fn replay_plan(
	git: &Git,
	payload: &CommitsPayload,
	opts: &RunOptions,
) -> Result<commits::CommitReplayPlan, TransferError> {
	commits::plan_commit_replay_with(git, payload, opts).map_err(
		|err| match err {
			CommitError::Git(git_err) => TransferError::Git(git_err),
			other => TransferError::Commit(other),
		},
	)
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
		return Err(TransferError::SpecialFile(
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
		let reason = if !target.existed && existed {
			format!(
				"target file '{}' was created externally after preview",
				target.relative_path
			)
		} else if target.existed && !existed {
			format!(
				"target file '{}' was deleted externally after preview",
				target.relative_path
			)
		} else {
			format!(
				"target file '{}' was modified externally after preview",
				target.relative_path
			)
		};
		return Err(TransferError::StaleDestination {
			root: target.root.path().to_path_buf(),
			reason,
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
