//! Workspace model: bounded directory and repository discovery, repository
//! identity, per-worktree serialization of heavy Git operations, and
//! repository summaries.
//!
//! Scans are sessions the caller owns and resumes: every call does bounded
//! work ([`ScanBudget`]) and says truthfully whether it finished
//! ([`ScanStatus`]). Nothing here collects a whole directory tree first.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime};

use crate::format::ChangeType;
use crate::gitrun::{CancelToken, RunOptions};
use crate::gitsrc::{Git, GitError};
use crate::transfer::SourceKind;

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// Work one scan call may do.
#[derive(Debug, Clone)]
pub struct ScanBudget {
	/// Directory entries read, whatever becomes of them.
	pub max_visited: usize,
	pub deadline: Option<Instant>,
	pub cancel: Option<CancelToken>,
}

impl ScanBudget {
	pub fn visits(max_visited: usize) -> Self {
		Self {
			max_visited,
			deadline: None,
			cancel: None,
		}
	}

	/// Why the scan must stop now, if it must.
	fn stop(&self, visited: usize) -> Option<ScanStatus> {
		if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
			Some(ScanStatus::Cancelled)
		} else if self.deadline.is_some_and(|d| Instant::now() >= d) {
			Some(ScanStatus::TimedOut)
		} else if visited >= self.max_visited {
			Some(ScanStatus::More)
		} else {
			None
		}
	}
}

/// How a scan call ended. Only `Complete` means everything was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStatus {
	Complete,
	/// The call's budget ran out; call again to continue.
	More,
	/// A result cap was reached; the rest was not looked at.
	LimitReached,
	/// The walk ended, but some directories were left out: deeper than the
	/// limit, or unreadable. The pages listed each one.
	Incomplete,
	Cancelled,
	TimedOut,
}

// ---------------------------------------------------------------------------
// One directory
// ---------------------------------------------------------------------------

/// What identifies a directory's contents between calls: a change means a
/// resumed listing could skip or repeat entries, so the scan fails instead.
/// The modification time is only as fine as the filesystem keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirStamp {
	#[cfg(unix)]
	dev_ino: (u64, u64),
	#[cfg(windows)]
	created: Option<SystemTime>,
	modified: Option<SystemTime>,
}

fn stamp(path: &Path) -> io::Result<DirStamp> {
	let m = fs::metadata(path)?;
	Ok(DirStamp {
		#[cfg(unix)]
		dev_ino: {
			use std::os::unix::fs::MetadataExt;
			(m.dev(), m.ino())
		},
		#[cfg(windows)]
		created: m.created().ok(),
		modified: m.modified().ok(),
	})
}

/// One directory entry with its exact OS name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
	pub name: OsString,
	pub directory: bool,
	pub symlink: bool,
}

impl ScanEntry {
	/// The name when it is valid UTF-8. A name that is not has no string
	/// form that names this file: rendering it lossily could name another.
	pub fn utf8_name(&self) -> Option<&str> {
		self.name.to_str()
	}
}

#[derive(Debug)]
pub struct ScanPage {
	/// Directories first, then by name bytes, within this page only;
	/// across pages the order is the OS enumeration order.
	pub entries: Vec<ScanEntry>,
	pub visited: usize,
	pub status: ScanStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
	#[error("the directory changed during the scan; start it again")]
	Changed,
	#[error(transparent)]
	Io(#[from] io::Error),
}

/// A resumable listing of one directory. It owns the OS iterator, so each
/// call continues where the last one stopped; no call reads the whole
/// directory. `.git` is visited but not listed.
pub struct DirectoryScan {
	path: PathBuf,
	stamp: DirStamp,
	iter: Option<fs::ReadDir>,
}

impl DirectoryScan {
	pub fn open(path: &Path) -> io::Result<Self> {
		let stamp = stamp(path)?;
		Ok(Self {
			iter: Some(fs::read_dir(path)?),
			path: path.to_path_buf(),
			stamp,
		})
	}

	fn check(&self) -> Result<(), ScanError> {
		if stamp(&self.path)? != self.stamp {
			return Err(ScanError::Changed);
		}
		Ok(())
	}

	pub fn next_page(
		&mut self,
		budget: &ScanBudget,
	) -> Result<ScanPage, ScanError> {
		self.check()?;
		let mut entries = Vec::new();
		let mut visited = 0;
		let status = loop {
			let Some(iter) = self.iter.as_mut() else {
				break ScanStatus::Complete;
			};
			if let Some(stop) = budget.stop(visited) {
				break stop;
			}
			let Some(entry) = iter.next() else {
				self.iter = None;
				break ScanStatus::Complete;
			};
			visited += 1;
			let entry = entry?;
			let name = entry.file_name();
			if name == ".git" {
				continue;
			}
			let kind = entry.file_type()?;
			entries.push(ScanEntry {
				name,
				directory: kind.is_dir(),
				symlink: kind.is_symlink(),
			});
		};
		// A change while this page was read makes it unreliable too.
		self.check()?;
		sort_entries(&mut entries);
		Ok(ScanPage {
			entries,
			visited,
			status,
		})
	}

	/// Bytes this cursor retains in the process: the struct, the owned path
	/// heap, and the directory handle. The C library's `DIR` buffer is not
	/// visible to safe Rust, so it is not guessed.
	pub fn retained_bytes(&self) -> usize {
		std::mem::size_of::<Self>().saturating_add(self.path.capacity())
	}
}

fn sort_entries(entries: &mut [ScanEntry]) {
	entries.sort_by(|a, b| {
		b.directory.cmp(&a.directory).then_with(|| {
			a.name.as_encoded_bytes().cmp(b.name.as_encoded_bytes())
		})
	});
}

// ---------------------------------------------------------------------------
// Repository discovery
// ---------------------------------------------------------------------------

/// Deepest [`Discovery`] may go: each level keeps one directory handle
/// open, so this also caps its retained state.
pub const MAX_DISCOVERY_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitMarker {
	/// `.git` directory: a repository.
	Directory,
	/// `.git` file: a linked worktree or a submodule checkout.
	File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepo {
	pub path: PathBuf,
	pub marker: GitMarker,
}

#[derive(Debug)]
pub struct DiscoveryPage {
	pub repos: Vec<DiscoveredRepo>,
	/// Directories that could not be read. Reported, never skipped silently.
	pub errors: Vec<(PathBuf, String)>,
	/// Directories not entered because they are deeper than `max_depth`.
	/// Start a new [`Discovery`] at one to look inside.
	pub depth_limited: Vec<PathBuf>,
	pub visited: usize,
	/// `Complete` only when every directory was read; `Incomplete` when the
	/// walk ended with some left out (see `errors` and `depth_limited` of
	/// every page); `LimitReached` until [`Discovery::raise_repo_limit`].
	pub status: ScanStatus,
}

/// Sibling directories held so their `.git` can be classified before any of
/// those directories are listed. Past this bound the walk descends
/// immediately. It is not a queue of every directory in a wide tree.
const MAX_PENDING_DIRS: usize = 64;

/// Repositories added when a search continues past [`ScanStatus::LimitReached`].
pub const DISCOVERY_REPO_PAGE: usize = 256;

struct DiscoveryFrame {
	path: PathBuf,
	iter: fs::ReadDir,
	/// Directories in this folder whose `.git` was already classified.
	pending: Vec<PathBuf>,
}

/// A resumable depth-first search for repositories below a root, including
/// nested repositories, submodule checkouts and linked worktrees. Symlinks
/// are not followed; found repositories are searched too. Open directory
/// handles stay at one per level, at most `max_depth + 1`
/// ([`Discovery::retained`]). A directory's child repositories are recorded
/// while that directory is listed, before their contents are read, using at
/// most `MAX_PENDING_DIRS` (64) deferred paths per open directory. Declared but
/// not checked out submodules have no `.git`; list them with
/// [`declared_submodules`].
pub struct Discovery {
	stack: Vec<DiscoveryFrame>,
	/// The root, until it is opened on the first call.
	root: Option<PathBuf>,
	max_depth: usize,
	max_repos: usize,
	found: usize,
	/// Something was left out: the end is `Incomplete`, not `Complete`.
	left_out: bool,
}

impl Discovery {
	pub fn new(
		root: &Path,
		max_depth: usize,
		max_repos: usize,
	) -> io::Result<Self> {
		if max_depth > MAX_DISCOVERY_DEPTH {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("discovery depth is at most {MAX_DISCOVERY_DEPTH}"),
			));
		}
		let root = dunce::canonicalize(root)?;
		if !root.is_dir() {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"discovery root is not a directory",
			));
		}
		Ok(Self {
			stack: Vec::new(),
			root: Some(root),
			max_depth,
			max_repos,
			found: 0,
			left_out: false,
		})
	}

	/// Open directory handles held between calls.
	pub fn retained(&self) -> usize {
		self.stack.len()
	}

	/// Rust storage retained between pages: this value, stack allocation,
	/// each frame path and pending-path allocation, and the unopened root.
	/// `ReadDir`'s opaque OS/libc buffers are not visible to safe Rust and are
	/// excluded, as in [`DirectoryScan::retained_bytes`].
	pub fn retained_bytes(&self) -> usize {
		let mut bytes = std::mem::size_of::<Self>()
			.saturating_add(self.root.as_ref().map_or(0, PathBuf::capacity))
			.saturating_add(
				self.stack
					.capacity()
					.saturating_mul(std::mem::size_of::<DiscoveryFrame>()),
			);
		for frame in &self.stack {
			bytes = bytes.saturating_add(frame.path.capacity());
			bytes = bytes.saturating_add(
				frame
					.pending
					.capacity()
					.saturating_mul(std::mem::size_of::<PathBuf>()),
			);
			for path in &frame.pending {
				bytes = bytes.saturating_add(path.capacity());
			}
		}
		bytes
	}

	/// Repositories recorded so far. This is not the number of open handles.
	pub fn found_repos(&self) -> usize {
		self.found
	}

	pub fn repo_limit(&self) -> usize {
		self.max_repos
	}

	/// The walk can yield another page without starting over.
	pub fn has_cursor(&self) -> bool {
		self.root.is_some() || !self.stack.is_empty()
	}

	/// Lets a search that stopped at `LimitReached` continue where it was.
	pub fn raise_repo_limit(&mut self, max_repos: usize) {
		self.max_repos = self.max_repos.max(max_repos);
	}

	/// Continues a capped search by one repository page, from `found` rather
	/// than from the number of open directory handles.
	pub fn raise_found_page(&mut self) {
		self.raise_repo_limit(self.found.saturating_add(DISCOVERY_REPO_PAGE));
	}

	fn error(
		&mut self,
		page: &mut DiscoveryPage,
		path: PathBuf,
		e: &io::Error,
	) {
		self.left_out = true;
		page.errors.push((path, e.to_string()));
	}

	fn note_repo(&mut self, dir: &Path, page: &mut DiscoveryPage) {
		if self.found >= self.max_repos {
			return;
		}
		match classify_git(dir) {
			Ok(Some(marker)) => {
				page.repos.push(DiscoveredRepo {
					path: dir.to_path_buf(),
					marker,
				});
				self.found += 1;
			}
			Ok(None) => {}
			// Missing `.git` is not a repository. Permission denied on the
			// marker usually means the directory itself cannot be searched;
			// opening it reports that once. Any other metadata error stays
			// visible.
			Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {}
			Err(e) => self.error(page, dir.join(".git"), &e),
		}
	}

	fn open_frame(&mut self, dir: PathBuf, page: &mut DiscoveryPage) {
		match fs::read_dir(&dir) {
			Ok(iter) => self.stack.push(DiscoveryFrame {
				path: dir,
				iter,
				pending: Vec::new(),
			}),
			Err(e) => self.error(page, dir, &e),
		}
	}

	fn queue_child(
		&mut self,
		child: PathBuf,
		depth: usize,
		page: &mut DiscoveryPage,
	) {
		if depth >= self.max_depth {
			self.left_out = true;
			page.depth_limited.push(child);
			return;
		}
		let pending = self.stack.last().map(|f| f.pending.len()).unwrap_or(0);
		if pending < MAX_PENDING_DIRS {
			if let Some(frame) = self.stack.last_mut() {
				frame.pending.push(child);
			}
		} else {
			self.open_frame(child, page);
		}
	}

	pub fn next_page(&mut self, budget: &ScanBudget) -> DiscoveryPage {
		let mut page = DiscoveryPage {
			repos: Vec::new(),
			errors: Vec::new(),
			depth_limited: Vec::new(),
			visited: 0,
			status: ScanStatus::Complete,
		};
		page.status = loop {
			if self.found >= self.max_repos {
				break ScanStatus::LimitReached;
			}
			if let Some(stop) = budget.stop(page.visited) {
				break stop;
			}
			if let Some(root) = self.root.take() {
				self.note_repo(&root, &mut page);
				self.open_frame(root, &mut page);
				if self.found >= self.max_repos {
					break ScanStatus::LimitReached;
				}
				continue;
			}
			let step = {
				let Some(frame) = self.stack.last_mut() else {
					break if self.left_out {
						ScanStatus::Incomplete
					} else {
						ScanStatus::Complete
					};
				};
				if let Some(entry) = frame.iter.next() {
					DiscoveryStep::Entry(entry)
				} else if let Some(path) = frame.pending.pop() {
					DiscoveryStep::Pending(path)
				} else {
					DiscoveryStep::Pop
				}
			};
			match step {
				DiscoveryStep::Pop => {
					self.stack.pop();
				}
				DiscoveryStep::Pending(path) => {
					let depth = self.stack.len().saturating_sub(1);
					if depth >= self.max_depth {
						self.left_out = true;
						page.depth_limited.push(path);
					} else {
						self.open_frame(path, &mut page);
					}
				}
				DiscoveryStep::Entry(Err(e)) => {
					let dir = self
						.stack
						.last()
						.map(|f| f.path.clone())
						.unwrap_or_default();
					self.error(&mut page, dir, &e);
				}
				DiscoveryStep::Entry(Ok(entry)) => {
					page.visited += 1;
					if entry.file_name() == OsStr::new(".git") {
						continue;
					}
					let kind = match entry.file_type() {
						Ok(k) => k,
						Err(e) => {
							self.error(&mut page, entry.path(), &e);
							continue;
						}
					};
					if !kind.is_dir() {
						continue;
					}
					let child = entry.path();
					let depth = self.stack.len().saturating_sub(1);
					self.note_repo(&child, &mut page);
					self.queue_child(child, depth, &mut page);
					if self.found >= self.max_repos {
						break ScanStatus::LimitReached;
					}
				}
			}
		};
		page
	}
}

enum DiscoveryStep {
	Entry(io::Result<fs::DirEntry>),
	Pending(PathBuf),
	Pop,
}

fn classify_git(dir: &Path) -> Result<Option<GitMarker>, io::Error> {
	match fs::symlink_metadata(dir.join(".git")) {
		Ok(m) if m.is_dir() => Ok(Some(GitMarker::Directory)),
		Ok(m) if m.is_file() => Ok(Some(GitMarker::File)),
		Ok(_) => Ok(None),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(e),
	}
}

// ---------------------------------------------------------------------------
// Declared submodules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmoduleState {
	/// Its `.git` (file or directory) is there.
	CheckedOut,
	/// Declared, with no `.git` at its path: not initialized or deinited.
	NotCheckedOut,
	/// Its path could not be examined; the reason, never a guess.
	Unreadable(String),
	/// `.gitmodules` names a path that is not a safe relative path.
	InvalidPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredSubmodule {
	/// `submodule.<name>.path` in `.gitmodules`.
	pub name: String,
	pub path: String,
	pub state: SubmoduleState,
}

/// Every submodule `.gitmodules` declares, checked out or not. Reads the
/// file through `git config` (no shell), bounded by `opts`.
pub fn declared_submodules(
	git: &Git,
	opts: &RunOptions,
) -> Result<Vec<DeclaredSubmodule>, GitError> {
	match fs::symlink_metadata(git.root().join(".gitmodules")) {
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(e) => return Err(e.into()),
		Ok(m) if !m.is_file() => {
			return Err(GitError::Malformed(".gitmodules is not a file".into()))
		}
		Ok(_) => {}
	}
	let args = [
		"config",
		"-z",
		"--file",
		".gitmodules",
		"--get-regexp",
		r"^submodule\..*\.path$",
	];
	let out = match git.run_with(&args, opts) {
		Ok(out) => out.stdout,
		// No matching key.
		Err(GitError::Failed {
			code: Some(1),
			stderr,
			..
		}) if stderr.is_empty() => return Ok(Vec::new()),
		Err(e) => return Err(e),
	};
	let bad = || GitError::Malformed("config -z record".into());
	let mut list = Vec::new();
	// `-z`: `<key>\n<value>\0`; a value may contain newlines.
	for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
		let rec = std::str::from_utf8(rec).map_err(|_| bad())?;
		let (key, path) = rec.split_once('\n').ok_or_else(bad)?;
		let name = key
			.strip_prefix("submodule.")
			.and_then(|k| k.strip_suffix(".path"))
			.ok_or_else(bad)?;
		let safe = crate::paths::sanitize_relative_path(path)
			.is_some_and(|s| s == path);
		let state = if !safe {
			SubmoduleState::InvalidPath
		} else {
			match fs::symlink_metadata(git.root().join(path).join(".git")) {
				Ok(_) => SubmoduleState::CheckedOut,
				Err(e) if e.kind() == io::ErrorKind::NotFound => {
					SubmoduleState::NotCheckedOut
				}
				Err(e) => SubmoduleState::Unreadable(e.to_string()),
			}
		};
		list.push(DeclaredSubmodule {
			name: name.to_string(),
			path: path.to_string(),
			state,
		});
	}
	Ok(list)
}

// ---------------------------------------------------------------------------
// Repository identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
	Main,
	/// `git worktree add`: its own git dir, sharing the common dir.
	LinkedWorktree,
	/// Checked out inside a superproject.
	Submodule,
}

/// Who a working tree is: its top level, its own git dir (index, HEAD) and
/// the common dir (objects, refs) it shares with its other worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
	pub toplevel: PathBuf,
	pub git_dir: PathBuf,
	pub common_dir: PathBuf,
	pub kind: RepoKind,
}

/// One `rev-parse` path, exactly: one query per call, so a path containing
/// a newline cannot be mistaken for a record boundary; only the single
/// terminating newline is removed.
fn rev_parse_path(
	git: &Git,
	flag: &str,
	opts: &RunOptions,
) -> Result<Option<PathBuf>, GitError> {
	let out = git
		.run_with(&["rev-parse", "--path-format=absolute", flag], opts)?
		.stdout;
	let Some(bytes) = out.strip_suffix(b"\n") else {
		return if out.is_empty() {
			Ok(None)
		} else {
			Err(GitError::Malformed(format!("rev-parse {flag} output")))
		};
	};
	if bytes.is_empty() {
		return Ok(None);
	}
	let path = crate::gitsrc::path_from_git_bytes(bytes)?;
	// A path git just reported must resolve; if it does not, the identity
	// is unknown, not the unresolved spelling.
	Ok(Some(dunce::canonicalize(&path)?))
}

impl RepoIdentity {
	pub fn resolve(git: &Git, opts: &RunOptions) -> Result<Self, GitError> {
		let missing = |what: &str| GitError::Malformed(format!("no {what}"));
		let git_dir = rev_parse_path(git, "--git-dir", opts)?
			.ok_or_else(|| missing("git dir"))?;
		let common_dir = rev_parse_path(git, "--git-common-dir", opts)?
			.ok_or_else(|| missing("common dir"))?;
		let superproject =
			rev_parse_path(git, "--show-superproject-working-tree", opts)?;
		let kind = if superproject.is_some() {
			RepoKind::Submodule
		} else if git_dir != common_dir {
			RepoKind::LinkedWorktree
		} else {
			RepoKind::Main
		};
		Ok(Self {
			toplevel: git.root().to_path_buf(),
			git_dir,
			common_dir,
			kind,
		})
	}
}

// ---------------------------------------------------------------------------
// Heavy operations: one at a time per worktree
// ---------------------------------------------------------------------------

/// Callers allowed to wait for one worktree; one more is refused.
pub const MAX_HEAVY_WAITERS: usize = 4;

#[derive(Default)]
struct Slot {
	held: bool,
	waiting: usize,
}

static HEAVY: Mutex<Option<HashMap<PathBuf, Slot>>> = Mutex::new(None);
static HEAVY_FREED: Condvar = Condvar::new();

fn heavy() -> MutexGuard<'static, Option<HashMap<PathBuf, Slot>>> {
	HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Exclusive right to run index- or ref-changing Git work in one worktree
/// (keyed by its git dir: linked worktrees have their own index and HEAD).
pub struct HeavyGuard {
	key: PathBuf,
}

/// Waits (bounded by `opts.queue_timeout`, cancellable, at most
/// [`MAX_HEAVY_WAITERS`] waiters) for the worktree's heavy-operation lock.
pub fn lock_heavy(
	identity: &RepoIdentity,
	opts: &RunOptions,
) -> Result<HeavyGuard, GitError> {
	let key = identity.git_dir.clone();
	let label =
		|| format!("(heavy operation in {})", identity.toplevel.display());
	let mut map = heavy();
	let slot = map
		.get_or_insert_with(HashMap::new)
		.entry(key.clone())
		.or_default();
	if !slot.held {
		slot.held = true;
		return Ok(HeavyGuard { key });
	}
	if slot.waiting >= MAX_HEAVY_WAITERS {
		return Err(GitError::WorktreeBusy { args: label() });
	}
	slot.waiting += 1;
	let deadline = Instant::now() + opts.queue_timeout;
	let result = loop {
		let slot = map
			.get_or_insert_with(HashMap::new)
			.entry(key.clone())
			.or_default();
		if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
			break Err(GitError::Cancelled { args: label() });
		}
		if !slot.held {
			slot.held = true;
			break Ok(HeavyGuard { key: key.clone() });
		}
		let now = Instant::now();
		if now >= deadline {
			break Err(GitError::QueueTimeout { args: label() });
		}
		map = HEAVY_FREED
			.wait_timeout(
				map,
				(deadline - now).min(std::time::Duration::from_millis(20)),
			)
			.unwrap_or_else(PoisonError::into_inner)
			.0;
	};
	if let Some(slot) = map.get_or_insert_with(HashMap::new).get_mut(&key) {
		slot.waiting -= 1;
	}
	result
}

impl Drop for HeavyGuard {
	fn drop(&mut self) {
		let mut map = heavy();
		let map = map.get_or_insert_with(HashMap::new);
		if let Some(slot) = map.get_mut(&self.key) {
			slot.held = false;
			if slot.waiting == 0 {
				map.remove(&self.key);
			}
		}
		HEAVY_FREED.notify_all();
	}
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
	/// Paths with an index change (X of `XY`).
	pub staged: usize,
	/// Tracked paths with a worktree change (Y of `XY`). A partly staged
	/// file counts here and in `staged`.
	pub unstaged: usize,
	/// Untracked entries as `git status` lists them (a new directory is one).
	pub untracked: usize,
	pub conflicted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSummary {
	pub identity: RepoIdentity,
	/// `None` on an unborn branch.
	pub head: Option<String>,
	/// `None` when detached.
	pub branch: Option<String>,
	pub changes: ChangeCounts,
}

impl RepoSummary {
	/// The transfer sources this repository can offer right now.
	pub fn sources(&self) -> Vec<SourceKind> {
		let c = self.changes;
		let mut out = Vec::new();
		if c.staged + c.unstaged + c.untracked + c.conflicted > 0 {
			out.push(SourceKind::Working);
		}
		if c.unstaged + c.untracked > 0 {
			out.push(SourceKind::Unstaged);
		}
		if c.staged > 0 {
			out.push(SourceKind::Staged);
		}
		if let Some(head) = &self.head {
			out.push(SourceKind::Commit { rev: head.clone() });
		}
		out
	}
}

/// Parses `git status --porcelain=v2 -z --branch`.
fn parse_status(
	out: &[u8],
) -> Result<(Option<String>, Option<String>, ChangeCounts), GitError> {
	let bad = || GitError::Malformed("status --porcelain=v2 record".into());
	let mut head = None;
	let mut branch = None;
	let mut c = ChangeCounts::default();
	let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
	while let Some(rec) = records.next() {
		let text = String::from_utf8_lossy(rec);
		if let Some(oid) = text.strip_prefix("# branch.oid ") {
			head = (oid != "(initial)").then(|| oid.to_string());
		} else if let Some(name) = text.strip_prefix("# branch.head ") {
			branch = (name != "(detached)").then(|| name.to_string());
		} else if text.starts_with('#') || text.starts_with('!') {
		} else if text.starts_with("? ") {
			c.untracked += 1;
		} else if text.starts_with("u ") {
			c.conflicted += 1;
		} else if let Some(rest) =
			text.strip_prefix("1 ").or_else(|| text.strip_prefix("2 "))
		{
			let xy = rest.as_bytes();
			let (&x, &y) =
				(xy.first().ok_or_else(bad)?, xy.get(1).ok_or_else(bad)?);
			c.staged += usize::from(x != b'.');
			c.unstaged += usize::from(y != b'.');
			// A rename record carries its original path as the next field.
			if text.starts_with("2 ") {
				records.next().ok_or_else(bad)?;
			}
		} else {
			return Err(bad());
		}
	}
	Ok((head, branch, c))
}

/// One `git status` call. Bounded by `opts` (use
/// [`RunOptions::interactive`]); an oversized status is an error, never a
/// partial count.
pub fn summarize(
	git: &Git,
	opts: &RunOptions,
) -> Result<RepoSummary, GitError> {
	let identity = RepoIdentity::resolve(git, opts)?;
	let out = git.run_with(
		&[
			"status",
			"--porcelain=v2",
			"-z",
			"--branch",
			"--untracked-files=normal",
			"--no-renames",
		],
		&RunOptions {
			overflow: crate::gitrun::Overflow::Error,
			..opts.clone()
		},
	)?;
	let (head, branch, changes) = parse_status(&out.stdout)?;
	Ok(RepoSummary {
		identity,
		head,
		branch,
		changes,
	})
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusDetails {
	pub staged: Vec<(String, Option<ChangeType>)>,
	pub unstaged: Vec<(String, Option<ChangeType>)>,
	pub untracked: Vec<String>,
	pub conflicted: Vec<String>,
}

/// Categorizes changed paths into Staged, Unstaged, Untracked, and Conflicted.
pub fn status_details(
	git: &Git,
	opts: &RunOptions,
) -> Result<StatusDetails, GitError> {
	let out = git.run_with(
		&[
			"status",
			"--porcelain=v2",
			"-z",
			"--untracked-files=normal",
			"--no-renames",
		],
		&RunOptions {
			overflow: crate::gitrun::Overflow::Error,
			..opts.clone()
		},
	)?;
	parse_status_details(&out.stdout)
}

fn parse_status_details(out: &[u8]) -> Result<StatusDetails, GitError> {
	let bad = || GitError::Malformed("status --porcelain=v2 record".into());
	let mut details = StatusDetails::default();
	let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
	while let Some(rec) = records.next() {
		let text = String::from_utf8_lossy(rec);
		if text.starts_with('#') || text.starts_with('!') {
			continue;
		}
		if let Some(path) = text.strip_prefix("? ") {
			details.untracked.push(path.to_string());
		} else if let Some(rest) = text.strip_prefix("u ") {
			let parts: Vec<&str> = rest.splitn(10, ' ').collect();
			let path = parts.get(9).ok_or_else(bad)?;
			details.conflicted.push(path.to_string());
		} else if let Some(rest) = text.strip_prefix("1 ") {
			let parts: Vec<&str> = rest.splitn(8, ' ').collect();
			let xy = parts.first().ok_or_else(bad)?.as_bytes();
			let path = parts.get(7).ok_or_else(bad)?;
			let (&x, &y) =
				(xy.first().ok_or_else(bad)?, xy.get(1).ok_or_else(bad)?);
			if x != b'.' {
				details.staged.push((
					path.to_string(),
					Some(crate::gitsrc::change_type_for_status(x)),
				));
			}
			if y != b'.' {
				details.unstaged.push((
					path.to_string(),
					Some(crate::gitsrc::change_type_for_status(y)),
				));
			}
		} else if let Some(rest) = text.strip_prefix("2 ") {
			let parts: Vec<&str> = rest.splitn(9, ' ').collect();
			let xy = parts.first().ok_or_else(bad)?.as_bytes();
			let path = parts.get(8).ok_or_else(bad)?;
			let (&x, &y) =
				(xy.first().ok_or_else(bad)?, xy.get(1).ok_or_else(bad)?);
			if x != b'.' {
				details.staged.push((
					path.to_string(),
					Some(crate::gitsrc::change_type_for_status(x)),
				));
			}
			if y != b'.' {
				details.unstaged.push((
					path.to_string(),
					Some(crate::gitsrc::change_type_for_status(y)),
				));
			}
			records.next(); // skip origPath
		}
	}
	Ok(details)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;
	use std::process::Command;
	use std::time::{Duration, Instant};

	fn git(dir: &Path, args: &[&str]) -> String {
		let out = Command::new("git")
			.args([
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@x",
				"-c",
				"protocol.file.allow=always",
			])
			.args(args)
			.current_dir(dir)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"git {args:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8_lossy(&out.stdout).trim().to_string()
	}

	fn init(dir: &Path) {
		fs::create_dir_all(dir).unwrap();
		git(dir, &["init", "-q", "-b", "main"]);
	}

	fn commit_file(dir: &Path, name: &str, body: &str) {
		fs::write(dir.join(name), body).unwrap();
		git(dir, &["add", name]);
		git(dir, &["commit", "-qm", name]);
	}

	#[test]
	fn discovery_retained_bytes_counts_cursor_paths_and_spare_stack_slots() {
		let temp = tempfile::tempdir().unwrap();
		let mut discovery = Discovery::new(temp.path(), 3, 10).unwrap();
		let before = discovery.retained_bytes();
		let old_root = discovery.root.as_ref().unwrap().capacity();
		discovery.root.as_mut().unwrap().reserve(8192);
		assert_eq!(
			discovery.retained_bytes() - before,
			discovery.root.as_ref().unwrap().capacity() - old_root
		);
		let mut path = PathBuf::with_capacity(2048);
		path.push(temp.path());
		let mut pending = Vec::with_capacity(7);
		let mut child = PathBuf::with_capacity(4096);
		child.push(temp.path().join("not-yet-opened"));
		pending.push(child);
		discovery.stack = Vec::with_capacity(3);
		discovery.stack.push(DiscoveryFrame {
			path,
			iter: fs::read_dir(temp.path()).unwrap(),
			pending,
		});
		let frame = &discovery.stack[0];
		let frame_heap = frame.path.capacity()
			+ frame.pending.capacity() * std::mem::size_of::<PathBuf>()
			+ frame.pending.iter().map(PathBuf::capacity).sum::<usize>();
		let after = discovery.retained_bytes();
		discovery.stack.clear();
		assert_eq!(after - discovery.retained_bytes(), frame_heap);
		assert_eq!(
			discovery.retained_bytes(),
			std::mem::size_of::<Discovery>()
				+ discovery.root.as_ref().unwrap().capacity()
				+ discovery.stack.capacity()
					* std::mem::size_of::<DiscoveryFrame>()
		);
	}

	#[test]
	fn directory_scan_does_bounded_work_per_call_and_resumes() {
		let dir = tempfile::tempdir().unwrap();
		for i in 0..3000 {
			fs::write(dir.path().join(format!("f{i:04}")), "").unwrap();
		}
		fs::create_dir(dir.path().join(".git")).unwrap();
		let mut scan = DirectoryScan::open(dir.path()).unwrap();
		let mut names = std::collections::HashSet::new();
		let mut calls = 0;
		loop {
			let page = scan.next_page(&ScanBudget::visits(100)).unwrap();
			calls += 1;
			assert!(page.visited <= 100, "visited {}", page.visited);
			for e in page.entries {
				assert!(names.insert(e.name), "duplicate entry");
			}
			if page.status == ScanStatus::Complete {
				break;
			}
			assert_eq!(page.status, ScanStatus::More);
		}
		assert_eq!(names.len(), 3000);
		// 3001 entries including `.git`, 100 per call.
		assert!(calls >= 31, "{calls} calls");
		// Complete stays complete.
		assert_eq!(
			scan.next_page(&ScanBudget::visits(100)).unwrap().status,
			ScanStatus::Complete
		);
	}

	#[test]
	fn directory_scan_honours_cancel_and_deadline_before_any_work() {
		let dir = tempfile::tempdir().unwrap();
		for i in 0..50 {
			fs::write(dir.path().join(format!("f{i}")), "").unwrap();
		}
		let mut scan = DirectoryScan::open(dir.path()).unwrap();
		let cancel = CancelToken::new();
		cancel.cancel();
		let page = scan
			.next_page(&ScanBudget {
				cancel: Some(cancel),
				..ScanBudget::visits(1000)
			})
			.unwrap();
		assert_eq!((page.visited, page.status), (0, ScanStatus::Cancelled));
		let page = scan
			.next_page(&ScanBudget {
				deadline: Some(Instant::now()),
				..ScanBudget::visits(1000)
			})
			.unwrap();
		assert_eq!((page.visited, page.status), (0, ScanStatus::TimedOut));
		// Nothing was consumed: the full listing is still there.
		let page = scan.next_page(&ScanBudget::visits(1000)).unwrap();
		assert_eq!(
			(page.entries.len(), page.status),
			(50, ScanStatus::Complete)
		);
	}

	#[test]
	fn directory_scan_is_invalidated_when_the_directory_changes() {
		let dir = tempfile::tempdir().unwrap();
		for i in 0..10 {
			fs::write(dir.path().join(format!("f{i}")), "").unwrap();
		}
		let mut scan = DirectoryScan::open(dir.path()).unwrap();
		scan.next_page(&ScanBudget::visits(3)).unwrap();
		// Coarse timestamps: make sure the change is visible.
		std::thread::sleep(Duration::from_millis(20));
		fs::write(dir.path().join("new"), "").unwrap();
		assert!(matches!(
			scan.next_page(&ScanBudget::visits(3)),
			Err(ScanError::Changed)
		));
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn non_utf8_names_keep_their_exact_identity() {
		use std::os::unix::ffi::OsStrExt;
		let dir = tempfile::tempdir().unwrap();
		let raw = [OsStr::from_bytes(b"a\xff"), OsStr::from_bytes(b"a\xfe")];
		for n in raw {
			fs::write(dir.path().join(n), n.as_bytes()).unwrap();
		}
		// What a lossy rendering of both would produce.
		fs::write(dir.path().join("a\u{FFFD}"), "real").unwrap();
		let page = DirectoryScan::open(dir.path())
			.unwrap()
			.next_page(&ScanBudget::visits(100))
			.unwrap();
		assert_eq!(page.entries.len(), 3);
		let utf8: Vec<_> = page
			.entries
			.iter()
			.filter_map(ScanEntry::utf8_name)
			.collect();
		assert_eq!(utf8, ["a\u{FFFD}"]);
		for n in raw {
			let e = page.entries.iter().find(|e| e.name == n).unwrap();
			assert_eq!(e.utf8_name(), None);
			// The exact name still opens exactly that file.
			assert_eq!(
				fs::read(dir.path().join(&e.name)).unwrap(),
				n.as_bytes()
			);
		}
	}

	#[test]
	fn discovery_finds_nested_repos_submodules_and_worktrees_resumably() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		let main = root.join("main");
		init(&main);
		commit_file(&main, "a.txt", "a\n");
		let lib = root.join("lib-src");
		init(&lib);
		commit_file(&lib, "l.txt", "l\n");
		git(
			&main,
			&[
				"submodule",
				"add",
				"-q",
				lib.to_str().unwrap(),
				"vendor/lib",
			],
		);
		init(&main.join("nested/inner"));
		git(
			&main,
			&["worktree", "add", "-q", root.join("wt").to_str().unwrap()],
		);
		for i in 0..40 {
			fs::create_dir_all(root.join(format!("noise/d{i}"))).unwrap();
		}

		let mut discovery = Discovery::new(&root, 6, 100).unwrap();
		let mut found = Vec::new();
		let mut pages = 0;
		loop {
			let page = discovery.next_page(&ScanBudget::visits(10));
			pages += 1;
			assert!(page.visited <= 10);
			assert!(page.errors.is_empty(), "{:?}", page.errors);
			found.extend(page.repos);
			if page.status == ScanStatus::Complete {
				break;
			}
			assert_eq!(page.status, ScanStatus::More);
		}
		assert!(pages > 5, "{pages}");
		let has = |p: PathBuf, m| {
			found.contains(&DiscoveredRepo { path: p, marker: m })
		};
		assert!(has(main.clone(), GitMarker::Directory));
		assert!(has(lib.clone(), GitMarker::Directory));
		assert!(has(main.join("nested/inner"), GitMarker::Directory));
		assert!(has(main.join("vendor/lib"), GitMarker::File));
		assert!(has(root.join("wt"), GitMarker::File));
		assert_eq!(found.len(), 5);

		// The repository cap stops the search and says so.
		let page = Discovery::new(&root, 6, 2)
			.unwrap()
			.next_page(&ScanBudget::visits(100_000));
		assert_eq!(
			(page.repos.len(), page.status),
			(2, ScanStatus::LimitReached)
		);

		// Identities tell the kinds apart.
		let id = |p: &Path| {
			RepoIdentity::resolve(
				&Git::open(p).unwrap(),
				&RunOptions::default(),
			)
			.unwrap()
		};
		let (m, w, s) = (
			id(&main),
			id(&root.join("wt")),
			id(&main.join("vendor/lib")),
		);
		assert_eq!(m.kind, RepoKind::Main);
		assert_eq!(w.kind, RepoKind::LinkedWorktree);
		assert_eq!(w.common_dir, m.git_dir);
		assert_ne!(w.git_dir, m.git_dir);
		assert_eq!(s.kind, RepoKind::Submodule);
	}

	#[cfg(unix)]
	#[test]
	fn discovery_reports_unreadable_directories() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let locked = dir.path().join("locked");
		fs::create_dir(&locked).unwrap();
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
			.unwrap();
		if fs::read_dir(&locked).is_ok() {
			// Running as root: permissions do not apply.
			fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
				.unwrap();
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"needs a non-root user"
			);
			return;
		}
		let page = Discovery::new(dir.path(), 3, 10)
			.unwrap()
			.next_page(&ScanBudget::visits(100));
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
			.unwrap();
		// Everything reachable was walked, but not everything: not Complete.
		assert_eq!(page.status, ScanStatus::Incomplete);
		assert_eq!(page.errors.len(), 1);
		assert!(page.errors[0].0.ends_with("locked"));
	}

	#[test]
	fn discovery_of_a_wide_tree_keeps_only_one_handle_per_level() {
		let dir = tempfile::tempdir().unwrap();
		// 1500 sibling directories, no repositories anywhere.
		for i in 0..1500 {
			fs::create_dir_all(dir.path().join(format!("d{i:04}/x"))).unwrap();
		}
		let mut discovery = Discovery::new(dir.path(), 3, 10).unwrap();
		let mut visited = 0;
		let mut max_retained = 0;
		loop {
			let page = discovery.next_page(&ScanBudget::visits(40));
			assert!(page.visited <= 40);
			visited += page.visited;
			max_retained = max_retained.max(discovery.retained());
			// Never more state than one open directory per level.
			assert!(discovery.retained() <= 3 + 1, "{}", discovery.retained());
			assert!(page.repos.is_empty() && page.errors.is_empty());
			if page.status != ScanStatus::More {
				assert_eq!(page.status, ScanStatus::Complete);
				break;
			}
		}
		// Every d* and every d*/x was read: nothing was dropped to stay small.
		assert_eq!(visited, 3000);
		assert!(max_retained >= 2);
	}

	#[test]
	fn discovery_reports_depth_limits_and_resumes_past_a_raised_repo_limit() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		let deep = root.join("a/b/c");
		init(&deep.join("repo"));
		for r in ["r1", "r2", "r3"] {
			init(&root.join(r));
		}
		// Depth 2 reaches a/b; a/b/c is left out and said to be.
		let mut discovery = Discovery::new(&root, 2, 100).unwrap();
		let mut limited = Vec::new();
		let status = loop {
			let page = discovery.next_page(&ScanBudget::visits(1000));
			limited.extend(page.depth_limited);
			assert!(page.repos.iter().all(|r| !r.path.starts_with(&deep)));
			if page.status != ScanStatus::More {
				break page.status;
			}
		};
		assert_eq!(status, ScanStatus::Incomplete);
		assert_eq!(limited, std::slice::from_ref(&deep));
		// Looking inside is a new search rooted there.
		let page = Discovery::new(&deep, 2, 100)
			.unwrap()
			.next_page(&ScanBudget::visits(1000));
		assert_eq!(page.repos.len(), 1);
		assert_eq!(page.status, ScanStatus::Complete);

		// The repository cap stops the walk; raising it continues it.
		let mut discovery = Discovery::new(&root, 1, 1).unwrap();
		let first = discovery.next_page(&ScanBudget::visits(1000));
		assert_eq!(
			(first.repos.len(), first.status),
			(1, ScanStatus::LimitReached)
		);
		assert_eq!(
			discovery.next_page(&ScanBudget::visits(1000)).status,
			ScanStatus::LimitReached
		);
		discovery.raise_repo_limit(10);
		let rest = discovery.next_page(&ScanBudget::visits(1000));
		let mut all: Vec<_> = first
			.repos
			.into_iter()
			.chain(rest.repos)
			.map(|r| r.path)
			.collect();
		all.sort();
		assert_eq!(all, [root.join("r1"), root.join("r2"), root.join("r3")]);
		// a/b is deeper than 1: left out, so not Complete.
		assert_eq!(rest.status, ScanStatus::Incomplete);
	}

	#[test]
	fn discovery_detects_repositories_before_large_content_traversal() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		let r1 = root.join("repo-1");
		let r2 = root.join("repo-2");
		init(&r1);
		init(&r2);
		let src = r1.join("src");
		fs::create_dir(&src).unwrap();
		for i in 0..200 {
			fs::write(src.join(format!("file_{i}.txt")), b"x").unwrap();
		}
		let mut discovery = Discovery::new(&root, 4, 10).unwrap();
		let page = discovery.next_page(&ScanBudget::visits(50));
		assert!(page.repos.iter().any(|r| r.path == r1));
	}

	#[test]
	fn discovery_records_every_sibling_repo_before_their_files() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		for i in 0..15 {
			let repo = root.join(format!("repo-{i:02}"));
			init(&repo);
			let src = repo.join("src");
			fs::create_dir(&src).unwrap();
			for j in 0..80 {
				fs::write(src.join(format!("f{j}.txt")), b"x").unwrap();
			}
		}
		let mut discovery = Discovery::new(&root, 6, 100).unwrap();
		let page = discovery.next_page(&ScanBudget::visits(40));
		assert_eq!(page.repos.len(), 15, "{:?}", page.repos);
		assert!(page.visited <= 40, "{}", page.visited);
		assert_eq!(page.status, ScanStatus::More);
		assert!(discovery.retained() <= 6 + 1);
		let mut all = page.repos;
		loop {
			let next = discovery.next_page(&ScanBudget::visits(500));
			all.extend(next.repos);
			if next.status != ScanStatus::More {
				assert_eq!(next.status, ScanStatus::Complete);
				break;
			}
		}
		assert_eq!(all.len(), 15, "a repository must be reported once");
	}

	#[test]
	fn raise_found_page_uses_discovered_repos_not_open_handles() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		for name in ["r1", "r2", "r3"] {
			init(&root.join(name));
		}
		let mut discovery = Discovery::new(&root, 2, 1).unwrap();
		let first = discovery.next_page(&ScanBudget::visits(1000));
		assert_eq!(first.status, ScanStatus::LimitReached);
		assert_eq!(discovery.found_repos(), 1);
		let handles = discovery.retained();
		assert!(handles < 8, "{handles}");
		discovery.raise_found_page();
		assert_eq!(discovery.repo_limit(), 1 + DISCOVERY_REPO_PAGE);
		assert!(discovery.repo_limit() > handles);
		assert_eq!(discovery.found_repos(), 1);
	}

	/// The standard 15×10k dataset is not on CI. Run with
	/// `--ignored standard_workload` and `SNIP_STANDARD_WORKLOAD_REQUIRED=1`.
	#[test]
	#[ignore = "local standard workload /tmp/snip-workload-standard-20260925"]
	fn standard_workload_discovers_fifteen_repos_before_finishing() {
		let root = PathBuf::from("/tmp/snip-workload-standard-20260925");
		let required =
			std::env::var_os("SNIP_STANDARD_WORKLOAD_REQUIRED").is_some();
		if !root.join("workload_manifest.json").is_file() {
			assert!(
				!required,
				"SNIP_STANDARD_WORKLOAD_REQUIRED but {root:?} is missing"
			);
			return;
		}
		let mut discovery = Discovery::new(&root, 8, 256).unwrap();
		let mut repos = Vec::new();
		let mut pages = 0usize;
		let mut saw_fifteen_early = false;
		let started = Instant::now();
		loop {
			let page = discovery.next_page(&ScanBudget::visits(2_000));
			pages += 1;
			assert!(page.visited <= 2_000, "{}", page.visited);
			repos.extend(page.repos);
			if repos.len() >= 15 && pages == 1 {
				saw_fifteen_early = true;
			}
			assert!(
				pages < 500,
				"discovery did not finish after {pages} pages"
			);
			if page.status != ScanStatus::More {
				assert!(
					matches!(
						page.status,
						ScanStatus::Complete | ScanStatus::Incomplete
					),
					"{:?}",
					page.status
				);
				break;
			}
		}
		assert!(
			saw_fifteen_early,
			"fifteen repos must be recorded on the first bounded page, not after walking every file"
		);
		assert_eq!(
			repos.len(),
			15,
			"each repository is reported once: {repos:?}"
		);
		let mut paths: Vec<_> =
			repos.into_iter().map(|repo| repo.path).collect();
		paths.sort();
		assert_eq!(paths.len(), 15, "{paths:?}");
		assert!(started.elapsed().as_secs() < 180, "walk took too long");
	}

	#[test]
	fn declared_submodules_include_those_not_checked_out() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		let lib = root.join("lib-src");
		init(&lib);
		commit_file(&lib, "l.txt", "l\n");
		let main = root.join("main");
		init(&main);
		commit_file(&main, "a.txt", "a\n");
		let git_main = Git::open(&main).unwrap();
		let opts = RunOptions::interactive(None);
		assert!(declared_submodules(&git_main, &opts).unwrap().is_empty());
		for p in ["vendor/one", "vendor/two"] {
			git(&main, &["submodule", "add", "-q", lib.to_str().unwrap(), p]);
		}
		git(&main, &["commit", "-qm", "subs"]);
		git(&main, &["submodule", "deinit", "-q", "-f", "vendor/two"]);
		let subs = declared_submodules(&git_main, &opts).unwrap();
		let state = |p: &str| {
			subs.iter().find(|s| s.path == p).map(|s| s.state.clone())
		};
		assert_eq!(subs.len(), 2);
		assert_eq!(state("vendor/one"), Some(SubmoduleState::CheckedOut));
		assert_eq!(state("vendor/two"), Some(SubmoduleState::NotCheckedOut));
		// Discovery alone cannot see the one that is not checked out.
		let mut found = Vec::new();
		let mut discovery = Discovery::new(&main, 4, 10).unwrap();
		loop {
			let page = discovery.next_page(&ScanBudget::visits(1000));
			found.extend(page.repos.into_iter().map(|r| r.path));
			if page.status != ScanStatus::More {
				break;
			}
		}
		assert!(found.contains(&main.join("vendor/one")));
		assert!(!found.contains(&main.join("vendor/two")));

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let vendor = main.join("vendor");
			fs::set_permissions(&vendor, fs::Permissions::from_mode(0o000))
				.unwrap();
			// Root ignores permissions; then this part cannot be checked.
			let bypassed = fs::read_dir(&vendor).is_ok();
			let subs = declared_submodules(&git_main, &opts);
			fs::set_permissions(&vendor, fs::Permissions::from_mode(0o755))
				.unwrap();
			if bypassed {
				assert!(
					std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
					"needs a non-root user"
				);
			} else {
				// An unreadable path is reported, never "not checked out".
				for s in subs.unwrap() {
					assert!(
						matches!(s.state, SubmoduleState::Unreadable(_)),
						"{s:?}"
					);
				}
			}
		}
	}

	#[cfg(unix)]
	#[test]
	fn identity_keeps_paths_with_newlines_exact() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		let repo = root.join("we\nird\n");
		init(&repo);
		let g = Git::open(&repo).unwrap();
		assert_eq!(g.root(), repo);
		let id = RepoIdentity::resolve(&g, &RunOptions::default()).unwrap();
		assert_eq!(id.toplevel, repo);
		assert_eq!(id.git_dir, repo.join(".git"));
		assert_eq!(id.common_dir, repo.join(".git"));
		assert_eq!(id.kind, RepoKind::Main);
	}

	#[test]
	fn identity_of_a_vanished_repository_is_an_error_not_a_guess() {
		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("r");
		init(&repo);
		let g = Git::open(&repo).unwrap();
		fs::remove_dir_all(&repo).unwrap();
		assert!(RepoIdentity::resolve(&g, &RunOptions::default()).is_err());
	}

	#[test]
	fn discovery_cancel_stops_before_any_work() {
		let dir = tempfile::tempdir().unwrap();
		let cancel = CancelToken::new();
		cancel.cancel();
		let page =
			Discovery::new(dir.path(), 3, 10)
				.unwrap()
				.next_page(&ScanBudget {
					cancel: Some(cancel),
					..ScanBudget::visits(100)
				});
		assert_eq!((page.visited, page.status), (0, ScanStatus::Cancelled));
	}

	#[test]
	fn heavy_operations_serialize_per_worktree_with_a_bounded_queue() {
		let dir = tempfile::tempdir().unwrap();
		let root = dunce::canonicalize(dir.path()).unwrap();
		let main = root.join("main");
		init(&main);
		commit_file(&main, "a.txt", "a\n");
		git(
			&main,
			&["worktree", "add", "-q", root.join("wt").to_str().unwrap()],
		);
		let id = |p: &Path| {
			RepoIdentity::resolve(
				&Git::open(p).unwrap(),
				&RunOptions::default(),
			)
			.unwrap()
		};
		let (m, w) = (id(&main), id(&root.join("wt")));

		let held = lock_heavy(&m, &RunOptions::default()).unwrap();
		// Another worktree of the same repository is independent.
		let other = lock_heavy(&w, &RunOptions::default()).unwrap();
		drop(other);
		let short = RunOptions {
			queue_timeout: Duration::from_millis(50),
			..RunOptions::default()
		};
		assert!(matches!(
			lock_heavy(&m, &short),
			Err(GitError::QueueTimeout { .. })
		));

		// Fill the waiting room, then one more is refused at once.
		let stop = CancelToken::new();
		let waiters: Vec<_> = (0..MAX_HEAVY_WAITERS)
			.map(|_| {
				let (m, stop) = (m.clone(), stop.clone());
				std::thread::spawn(move || {
					lock_heavy(
						&m,
						&RunOptions {
							cancel: Some(stop),
							queue_timeout: Duration::from_secs(60),
							..RunOptions::default()
						},
					)
					.map(drop)
				})
			})
			.collect();
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			let waiting = heavy()
				.as_ref()
				.and_then(|h| h.get(&m.git_dir))
				.map_or(0, |s| s.waiting);
			if waiting == MAX_HEAVY_WAITERS {
				break;
			}
			assert!(Instant::now() < deadline);
			std::thread::sleep(Duration::from_millis(10));
		}
		assert!(matches!(
			lock_heavy(&m, &short),
			Err(GitError::WorktreeBusy { .. })
		));
		stop.cancel();
		for w in waiters {
			assert!(matches!(
				w.join().unwrap(),
				Err(GitError::Cancelled { .. })
			));
		}
		drop(held);
		// Released: the next caller gets it at once.
		drop(lock_heavy(&m, &short).unwrap());
	}

	#[test]
	fn summaries_count_each_source_and_offer_matching_transfer_sources() {
		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("r");
		init(&repo);
		let git_open = || Git::open(&repo).unwrap();
		let unborn =
			summarize(&git_open(), &RunOptions::interactive(None)).unwrap();
		assert_eq!(
			(unborn.head.as_deref(), unborn.branch.as_deref()),
			(None, Some("main"))
		);
		assert!(unborn.sources().is_empty());

		commit_file(&repo, "a.txt", "a\n");
		commit_file(&repo, "b.txt", "b\n");
		commit_file(&repo, "c.txt", "c\n");
		fs::write(repo.join("a.txt"), "a2\n").unwrap(); // unstaged
		fs::write(repo.join("b.txt"), "b2\n").unwrap();
		git(&repo, &["add", "b.txt"]); // staged
		fs::write(repo.join("b.txt"), "b3\n").unwrap(); // and unstaged
		git(&repo, &["mv", "c.txt", "c2.txt"]); // staged rename
		fs::write(repo.join("new.txt"), "n\n").unwrap(); // untracked
		let s = summarize(&git_open(), &RunOptions::interactive(None)).unwrap();
		assert_eq!(
			s.changes,
			ChangeCounts {
				// b.txt, and the rename as its delete and add.
				staged: 3,
				unstaged: 2,
				untracked: 1,
				conflicted: 0,
			}
		);
		let head = git(&repo, &["rev-parse", "HEAD"]);
		assert_eq!(s.head.as_deref(), Some(head.as_str()));
		assert_eq!(
			s.sources(),
			[
				SourceKind::Working,
				SourceKind::Unstaged,
				SourceKind::Staged,
				SourceKind::Commit { rev: head.clone() }
			]
		);
		git(&repo, &["checkout", "-q", "--detach"]);
		let s = summarize(&git_open(), &RunOptions::interactive(None)).unwrap();
		assert_eq!(s.branch, None);
		assert_eq!(s.identity.kind, RepoKind::Main);
	}

	#[test]
	fn summaries_count_conflicts_and_refuse_oversized_status() {
		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("r");
		init(&repo);
		commit_file(&repo, "f.txt", "base\n");
		git(&repo, &["checkout", "-qb", "side"]);
		commit_file(&repo, "f.txt", "side\n");
		git(&repo, &["checkout", "-q", "main"]);
		commit_file(&repo, "f.txt", "main\n");
		let merge = Command::new("git")
			.args([
				"-c",
				"user.name=T",
				"-c",
				"user.email=t@x",
				"merge",
				"-q",
				"side",
			])
			.current_dir(&repo)
			.output()
			.unwrap();
		assert!(!merge.status.success());
		let git = Git::open(&repo).unwrap();
		let s = summarize(&git, &RunOptions::interactive(None)).unwrap();
		assert_eq!(s.changes.conflicted, 1);
		let tiny = RunOptions {
			max_stdout: 10,
			..RunOptions::interactive(None)
		};
		assert!(matches!(
			summarize(&git, &tiny),
			Err(GitError::OutputLimit { .. })
		));
	}

	#[test]
	fn status_details_categorizes_staged_unstaged_untracked_conflicted() {
		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("r");
		init(&repo);
		commit_file(&repo, "tracked.txt", "base\n");
		// Unstaged modification
		std::fs::write(repo.join("tracked.txt"), "modified\n").unwrap();
		// Staged new file
		std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
		git(&repo, &["add", "staged.txt"]);
		// Untracked file
		std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();

		let git = Git::open(&repo).unwrap();
		let details =
			status_details(&git, &RunOptions::interactive(None)).unwrap();
		assert_eq!(details.staged.len(), 1);
		assert_eq!(details.staged[0].0, "staged.txt");
		assert_eq!(details.unstaged.len(), 1);
		assert_eq!(details.unstaged[0].0, "tracked.txt");
		assert_eq!(details.untracked.len(), 1);
		assert_eq!(details.untracked[0], "untracked.txt");
		assert_eq!(details.conflicted.len(), 0);
	}
}
