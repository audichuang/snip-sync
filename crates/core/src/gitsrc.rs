//! Git source: read changed files and diffs from a repository.
//!
//! Replaces the VS Code git API layer of ClipCodeVSCode (`gitCopy.ts`,
//! `gitHistory.ts`, `graphCopy.ts`, `catFile.ts`, commit 0aa24c8) with direct
//! git plumbing calls (porting-notes section 5). `collect` only yields the
//! files; `collect_payload` applies filters, limits and counts on top.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub use crate::blob::{
	read_batch_header, read_batch_response, CatFile, CatObject,
	DELETED_FILE_MARKER,
};
use crate::blob::{BlobRead, BlobReader, DeletedContent};
use crate::copy::CopyResult;
use crate::filter::file_matches_filters;
use crate::format::{
	build_git_payload, build_payload, BuildPayloadOptions, ChangeType,
	PayloadFile,
};
use crate::fsutil::{decode_utf8_or_skip, read_text_file};
use crate::gitrun::{self, CancelToken, RunOptions, RunOutput};
use crate::paths::{source_root_name, to_clipboard_path_from_roots};
use crate::settings::Settings;
use crate::workspace::RepoIdentity;

/// The well-known OID of git's empty tree: the "parent" of a root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The empty tree OID of a SHA-256 repository: the same stand-in there.
pub const SHA256_EMPTY_TREE: &str =
	"6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";
/// Stands in for content the requested revision could not supply.
pub const UNREADABLE_FILE_MARKER: &str = "// Unable to read file content";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
	#[error(
		"git executable not found: install git and make sure it is on PATH"
	)]
	NotFound,
	#[error("git {args} failed: {stderr}")]
	Failed {
		args: String,
		stderr: String,
		/// Exit code; `None` when killed by a signal.
		code: Option<i32>,
	},
	#[error("git {args} timed out after {secs}s")]
	Timeout { args: String, secs: u64 },
	#[error("git {args} was cancelled")]
	Cancelled { args: String },
	#[error("git {args} gave up waiting for a free Git process slot")]
	QueueTimeout { args: String },
	#[error("git {args} refused: too many callers already wait for Git")]
	QueueFull { args: String },
	#[error(
		"git {args} refused: another heavy Git operation runs in this worktree"
	)]
	WorktreeBusy { args: String },
	#[error("git {args} output exceeds {limit} bytes")]
	OutputLimit { args: String, limit: usize },
	#[error("git {args} exited but a detached process kept its output open")]
	OutputHeldOpen { args: String },
	#[error(
		"git {args} was started while this thread already runs a Git process"
	)]
	NestedProcess { args: String },
	#[error("cleaning up git {args} failed: {message}")]
	Cleanup { args: String, message: String },
	#[error(
		"{} is not inside a git repository (commit mode and git sources need one)",
		.0.display()
	)]
	NotARepository(PathBuf),
	#[error(
		"{} is inside a parent repository, not a repository root itself",
		.0.display()
	)]
	ToplevelAbove(PathBuf),
	#[error("invalid revision: {0}")]
	InvalidRevision(String),
	#[error(
		"Repository history is shallow, so the parent of {0} is not available locally. Run 'git fetch --unshallow' and try again."
	)]
	Shallow(String),
	#[error("unexpected git output: {0}")]
	Malformed(String),
	#[error("outside the shared folder: {what}")]
	OutsideBoundary { what: &'static str },
	#[error("repository too large to verify: {what}")]
	VerifyLimit { what: &'static str },
	#[error("{0}")]
	Host(String),
	#[error(transparent)]
	Io(#[from] io::Error),
}

/// Runs the system git CLI inside one repository, never through a shell.
/// Every process goes through [`crate::gitrun`]: global budget, deadline,
/// output cap and process-tree cleanup.
#[derive(Debug, Clone)]
pub struct Git {
	root: PathBuf,
	boundary: Option<PathBuf>, /* canonical */
}

#[cfg(test)]
thread_local! {
	/// `git --version` runs started by this thread.
	static VERSION_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn git_command() -> Command {
	let mut cmd = Command::new("git");
	// Byte-stable output, and never block on a credential prompt.
	// CREATE_NO_WINDOW is set by the runner (command-group owns the flags).
	cmd.env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
	cmd
}

/// A path git printed, byte for byte: on Unix it need not be UTF-8, and
/// elsewhere git prints UTF-8.
pub(crate) fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf, GitError> {
	#[cfg(unix)]
	{
		use std::os::unix::ffi::OsStrExt;
		Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
	}
	#[cfg(not(unix))]
	{
		std::str::from_utf8(bytes)
			.map(PathBuf::from)
			.map_err(|_| GitError::Malformed("path is not UTF-8".into()))
	}
}

/// `rev-parse --verify --quiet` and `symbolic-ref --quiet` say "no such
/// thing" with exit status 1 and no stderr; anything else is a failure.
fn is_quiet_miss(e: &GitError) -> bool {
	matches!(e, GitError::Failed { code: Some(1), stderr, .. } if stderr.is_empty())
}

pub(crate) fn already_cancelled(
	opts: &RunOptions,
	args: &str,
) -> Result<(), GitError> {
	if opts.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
		return Err(GitError::Cancelled {
			args: args.to_string(),
		});
	}
	Ok(())
}

/// Metadata cannot be interpreted from a truncated prefix, even when the
/// same caller explicitly permits truncating a display-only patch.
fn run_strict(
	git: &Git,
	args: &[&str],
	opts: &RunOptions,
) -> Result<Vec<u8>, GitError> {
	let output = git.run_with(args, opts)?;
	if output.truncated {
		return Err(GitError::OutputLimit {
			args: args.join(" "),
			limit: opts.max_stdout,
		});
	}
	Ok(output.stdout)
}

/// Maximum index file bytes copied for a served diff (256 MiB).
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// A temporary copy of a repository index file, cleaned up on drop.
#[derive(Debug)]
pub(crate) struct TempIndexFile {
	_dir: tempfile::TempDir,
	path: PathBuf,
}

#[cfg(unix)]
pub(crate) fn open_index_source(
	source: &Path,
) -> Result<(std::fs::File, std::fs::Metadata), GitError> {
	use std::os::unix::fs::OpenOptionsExt;

	// Open with O_NONBLOCK so a FIFO swapped in cannot hang the process indefinitely.
	let file = std::fs::OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_NONBLOCK)
		.open(source)?;
	let meta = file.metadata()?;
	if !meta.is_file() {
		return Err(GitError::OutsideBoundary { what: "index" });
	}
	Ok((file, meta))
}

#[cfg(not(unix))]
pub(crate) fn open_index_source(
	source: &Path,
) -> Result<(std::fs::File, std::fs::Metadata), GitError> {
	let file = std::fs::File::open(source)?;
	let meta = file.metadata()?;
	if !meta.is_file() {
		return Err(GitError::OutsideBoundary { what: "index" });
	}
	Ok((file, meta))
}

impl TempIndexFile {
	pub(crate) fn create_from(source: &Path) -> Result<Self, GitError> {
		Self::create_from_with_cap(source, MAX_INDEX_BYTES)
	}

	pub(crate) fn create_from_with_cap(
		source: &Path,
		max_bytes: u64,
	) -> Result<Self, GitError> {
		// Stat before opening to avoid blocking indefinitely on a FIFO.
		let symlink_meta = std::fs::symlink_metadata(source)?;
		if !symlink_meta.is_file() && !symlink_meta.file_type().is_symlink() {
			return Err(GitError::OutsideBoundary { what: "index" });
		}
		let path_meta = std::fs::metadata(source)?;
		if !path_meta.is_file() {
			return Err(GitError::OutsideBoundary { what: "index" });
		}
		if path_meta.len() > max_bytes {
			return Err(GitError::OutputLimit {
				args: "index".into(),
				limit: usize::try_from(max_bytes).unwrap_or(usize::MAX),
			});
		}
		let (mut src, meta) = open_index_source(source)?;
		if meta.len() > max_bytes {
			return Err(GitError::OutputLimit {
				args: "index".into(),
				limit: usize::try_from(max_bytes).unwrap_or(usize::MAX),
			});
		}
		let mut builder = tempfile::Builder::new();
		builder.prefix("snip-diff-index-");
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			builder.permissions(std::fs::Permissions::from_mode(0o700));
		}
		let dir = builder.tempdir()?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			let _ = std::fs::set_permissions(
				dir.path(),
				std::fs::Permissions::from_mode(0o700),
			);
		}
		let path = dir.path().join("index");
		let mut temp = std::fs::File::create(&path)?;
		let mut total: u64 = 0;
		let mut buf = [0u8; 64 * 1024];
		loop {
			let n = src.read(&mut buf)?;
			if n == 0 {
				break;
			}
			total += n as u64;
			if total > max_bytes {
				return Err(GitError::OutputLimit {
					args: "index".into(),
					limit: usize::try_from(max_bytes).unwrap_or(usize::MAX),
				});
			}
			temp.write_all(&buf[..n])?;
		}
		temp.flush()?;
		// Preserve source mtime so git's racy-clean check remains accurate.
		let mtime = meta.modified()?;
		temp.set_modified(mtime)?;
		Ok(Self { _dir: dir, path })
	}

	pub(crate) fn path(&self) -> &Path {
		&self.path
	}
}

impl Git {
	/// Checks that git is installed, then resolves the repository top level
	/// containing `dir`.
	pub fn open(dir: &Path) -> Result<Self, GitError> {
		Self::open_with(dir, &RunOptions::default())
	}

	/// [`Git::open`] with explicit limits, deadline and cancellation.
	///
	/// Both `git --version` and `rev-parse --show-toplevel` use `opts`.
	/// A truncated read is [`GitError::OutputLimit`], not a partial root.
	/// A directory outside a repository is still [`GitError::NotARepository`].
	pub fn open_with(dir: &Path, opts: &RunOptions) -> Result<Self, GitError> {
		Self::open_shared(dir, None, opts)
	}

	/// Worker only: opens with boundary enforcement.
	pub fn open_within(
		dir: &Path,
		boundary: &Path,
		opts: &RunOptions,
	) -> Result<Self, GitError> {
		Self::open_shared(dir, Some(boundary), opts)
	}

	fn open_shared(
		dir: &Path,
		boundary: Option<&Path>,
		opts: &RunOptions,
	) -> Result<Self, GitError> {
		already_cancelled(opts, "--version")?;
		// `git --version` runs once per `PATH`: a changed `PATH` may find
		// another git, or none. Only success is remembered.
		// `git --version` touches no repository and stays as is without boundary.
		static CHECKED_PATH: std::sync::Mutex<Option<std::ffi::OsString>> =
			std::sync::Mutex::new(None);
		let path = std::env::var_os("PATH").unwrap_or_default();
		let checked = CHECKED_PATH
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.as_ref() == Some(&path);
		if !checked {
			#[cfg(test)]
			VERSION_RUNS.with(|n| n.set(n.get() + 1));
			let mut version = git_command();
			version.arg("--version");
			let version = gitrun::run(version, "--version", None, opts)?;
			if version.truncated {
				return Err(GitError::OutputLimit {
					args: "--version".into(),
					limit: opts.max_stdout,
				});
			}
			if version.status.is_some_and(|s| s.success()) {
				*CHECKED_PATH
					.lock()
					.unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
			}
		}
		let canonical_boundary = match boundary {
			Some(b) => Some(dunce::canonicalize(b)?),
			None => None,
		};
		let probe = Self {
			root: dir.to_path_buf(),
			boundary: canonical_boundary.clone(),
		};
		already_cancelled(opts, "rev-parse --show-toplevel")?;
		let out = probe
			.run_with(&["rev-parse", "--show-toplevel"], opts)
			.map_err(|e| match e {
				GitError::Failed { ref stderr, .. }
					if stderr.contains("not a git repository") =>
				{
					GitError::NotARepository(dir.to_path_buf())
				}
				e => e,
			})?;
		if out.truncated {
			return Err(GitError::OutputLimit {
				args: "rev-parse --show-toplevel".into(),
				limit: opts.max_stdout,
			});
		}
		// Exactly one terminating newline: a path may end in another.
		let top = out.stdout.strip_suffix(b"\n").ok_or_else(|| {
			GitError::Malformed("rev-parse --show-toplevel output".into())
		})?;
		let top_path = path_from_git_bytes(top)?;
		if let Some(ref b) = canonical_boundary {
			let canonical_top = dunce::canonicalize(&top_path)?;
			let canonical_dir = dunce::canonicalize(dir)?;
			if !canonical_top.starts_with(b) {
				return Err(GitError::OutsideBoundary { what: "repository" });
			}
			if canonical_top != canonical_dir {
				return Err(GitError::ToplevelAbove(dir.to_path_buf()));
			}
		}
		Ok(Self {
			root: top_path,
			boundary: canonical_boundary,
		})
	}

	/// 已知 identity 的 repository，不啟動 git；toplevel 應來自
	/// `RepoIdentity::resolve`（git 自身拼法）；收 `&RepoIdentity` 讓呼叫端無法直接傳入使用者路徑（例如 macOS 的 `/var` 對 `/private/var`）。
	pub fn at_known_root(known: &RepoIdentity) -> Self {
		Self {
			root: known.toplevel.clone(),
			boundary: None,
		}
	}

	/// Boundary of this repository, when bounded.
	pub fn boundary(&self) -> Option<&Path> {
		self.boundary.as_deref()
	}

	/// Sets or clears the boundary for this repository.
	pub fn with_boundary(mut self, boundary: Option<PathBuf>) -> Self {
		self.boundary = boundary;
		self
	}

	/// The repository top level; every git path is relative to it.
	pub fn root(&self) -> &Path {
		&self.root
	}

	/// A git command in this repository; run it with [`Git::exec`] so it
	/// counts against the budget.
	pub(crate) fn command(&self) -> Command {
		let mut cmd = git_command();
		cmd.current_dir(&self.root);
		if let Some(ref b) = self.boundary {
			cmd.args([
				"-c",
				// Empty disables fsmonitor on every git version; before
				// 2.36 any non-empty value ("false" included) is a hook path.
				"core.fsmonitor=",
				"-c",
				"protocol.allow=never",
				"-c",
				#[cfg(windows)]
				"core.hooksPath=NUL",
				#[cfg(not(windows))]
				"core.hooksPath=/dev/null",
			]);
			if let Some(parent) = b.parent() {
				if !parent.as_os_str().is_empty() {
					cmd.env("GIT_CEILING_DIRECTORIES", parent);
				}
			}
			cmd.env("GIT_OPTIONAL_LOCKS", "0");
			cmd.env("GIT_NO_LAZY_FETCH", "1");
			for var in [
				"GIT_DIR",
				"GIT_WORK_TREE",
				"GIT_INDEX_FILE",
				"GIT_COMMON_DIR",
				"GIT_OBJECT_DIRECTORY",
				"GIT_ALTERNATE_OBJECT_DIRECTORIES",
				"GIT_NAMESPACE",
				"GIT_EXTERNAL_DIFF",
				"GIT_CONFIG_PARAMETERS",
				"GIT_CONFIG_COUNT",
			] {
				cmd.env_remove(var);
			}
		}
		cmd
	}

	/// Runs `git <args>` and returns stdout; a non-zero exit is an error.
	pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
		Ok(self.run_with(args, &RunOptions::default())?.stdout)
	}

	/// [`Git::run`] with explicit limits, deadline and cancellation.
	pub fn run_with(
		&self,
		args: &[&str],
		opts: &RunOptions,
	) -> Result<RunOutput, GitError> {
		let mut cmd = self.command();
		cmd.args(args);
		self.exec(cmd, &args.join(" "), None, opts)
	}

	/// Inspects diff argv to decide whether `git diff` reads the index/worktree.
	/// Two-tree diffs (e.g. `diff <a> <b>` or `diff a..b`) skip index reading.
	fn diff_reads_index(args: &[&str]) -> bool {
		let mut rev_or_positional = Vec::new();
		let mut has_cached = false;
		let before_dashdash = match args.iter().position(|&a| a == "--") {
			Some(pos) => &args[..pos],
			None => args,
		};
		for &arg in before_dashdash {
			if arg == "diff" {
				continue;
			}
			if arg == "--cached" || arg == "--staged" {
				has_cached = true;
				continue;
			}
			if !arg.starts_with('-') {
				rev_or_positional.push(arg);
			}
		}
		if has_cached {
			return true;
		}
		if rev_or_positional.iter().any(|arg| arg.contains("..")) {
			return false;
		}
		// 0 args (worktree vs index) or 1 arg (tree-ish vs worktree) read the index/worktree.
		// 2 or more args (e.g. tree1 tree2) diff two trees and do not read the index.
		rev_or_positional.len() < 2
	}

	/// Creates a command for `diff`, ensuring that for a worker (served pool
	/// or boundary mode) any index auto-refresh writes to a private
	/// temporary copy instead of the real index: the machine's own user may
	/// be running git in that repository.
	pub(crate) fn diff_command(
		&self,
		args: &[&str],
		opts: &RunOptions,
	) -> Result<(Command, Option<TempIndexFile>), GitError> {
		let mut cmd = self.command();
		cmd.args(args);
		let mut temp_index = None;
		let served = self.boundary.is_some()
			|| opts.pool == crate::gitrun::GitPool::Served;
		if served && Self::diff_reads_index(args) {
			let out =
				self.run_with(&["rev-parse", "--git-path", "index"], opts)?;
			let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
			if !rel.is_empty() {
				let index_path = self.root.join(rel);
				if index_path.symlink_metadata().is_ok() {
					let temp_file = TempIndexFile::create_from(&index_path)?;
					cmd.env("GIT_INDEX_FILE", temp_file.path());
					temp_index = Some(temp_file);
				}
			}
		}
		Ok((cmd, temp_index))
	}

	/// [`Git::run_with`] for `diff` invocations, isolating a worker's index writes.
	pub fn run_diff_with(
		&self,
		args: &[&str],
		opts: &RunOptions,
	) -> Result<RunOutput, GitError> {
		let (cmd, _temp_index) = self.diff_command(args, opts)?;
		self.exec(cmd, &args.join(" "), None, opts)
	}

	pub(crate) fn exec(
		&self,
		cmd: Command,
		label: &str,
		input: Option<&[u8]>,
		opts: &RunOptions,
	) -> Result<RunOutput, GitError> {
		let done = gitrun::run(cmd, label, input, opts)?;
		match done.status {
			Some(s) if !s.success() => Err(GitError::Failed {
				args: label.to_string(),
				stderr: String::from_utf8_lossy(&done.stderr)
					.trim()
					.to_string(),
				code: s.code(),
			}),
			// `None` only for an explicitly truncated run.
			_ => Ok(RunOutput {
				stdout: done.stdout,
				truncated: done.truncated,
			}),
		}
	}

	/// Resolves `rev` to a full commit OID. A leading `-` is refused so a
	/// Resolves `rev` to a full commit OID. A leading `-` is refused so a
	/// revision can never be read as an option by a later git call. Only a
	/// revision git cannot resolve is `InvalidRevision`; a failing git is
	/// reported as itself.
	pub fn resolve_commit(&self, rev: &str) -> Result<String, GitError> {
		self.resolve_commit_with(rev, &RunOptions::default())
	}

	/// Resolves `rev` to a full commit OID under explicit runner options.
	pub fn resolve_commit_with(
		&self,
		rev: &str,
		opts: &RunOptions,
	) -> Result<String, GitError> {
		if rev.is_empty() || rev.starts_with('-') {
			return Err(GitError::InvalidRevision(rev.to_string()));
		}
		let spec = format!("{rev}^{{commit}}");
		match self.run_with(&["rev-parse", "--verify", "--quiet", &spec], opts)
		{
			Ok(out) => {
				if out.truncated {
					return Err(GitError::OutputLimit {
						args: format!("rev-parse --verify {spec}"),
						limit: opts.max_stdout,
					});
				}
				let text = std::str::from_utf8(&out.stdout)
					.map_err(|_| {
						GitError::Malformed("rev-parse output not utf-8".into())
					})?
					.trim();
				// Full OID must be exactly 40 (SHA-1) or 64 (SHA-256) hex digits.
				if (text.len() != 40 && text.len() != 64)
					|| !text.bytes().all(|b| b.is_ascii_hexdigit())
				{
					return Err(GitError::Malformed(format!(
						"rev-parse returned invalid commit OID: {text}"
					)));
				}
				Ok(text.to_string())
			}
			Err(e) if is_quiet_miss(&e) => {
				Err(GitError::InvalidRevision(rev.to_string()))
			}
			Err(e) => Err(e),
		}
	}

	/// HEAD's commit under explicit runner options, `None` for an unborn branch.
	pub fn head_with(
		&self,
		opts: &RunOptions,
	) -> Result<Option<String>, GitError> {
		match self.resolve_commit_with("HEAD", opts) {
			Ok(sha) => Ok(Some(sha)),
			Err(GitError::InvalidRevision(_)) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// HEAD's commit, `None` for an unborn branch.
	pub fn head(&self) -> Result<Option<String>, GitError> {
		self.head_with(&RunOptions::default())
	}

	/// The empty tree OID of this repository's object format: the diff
	/// base of a root commit, and of an unborn HEAD. SHA-1 and SHA-256
	/// repositories have different well-known empty tree OIDs, so it is
	/// looked up per repository instead of spelled from a constant.
	pub fn empty_tree_with(
		&self,
		opts: &RunOptions,
	) -> Result<String, GitError> {
		const ARGS: &str = "rev-parse --show-object-format";
		let out =
			self.run_with(&["rev-parse", "--show-object-format"], opts)?;
		if out.truncated {
			return Err(GitError::OutputLimit {
				args: ARGS.into(),
				limit: opts.max_stdout,
			});
		}
		let format = std::str::from_utf8(&out.stdout)
			.map_err(|_| {
				GitError::Malformed("rev-parse output not utf-8".into())
			})?
			.trim();
		match format {
			"sha256" => Ok(SHA256_EMPTY_TREE.into()),
			"sha1" => Ok(EMPTY_TREE.into()),
			other => Err(GitError::Malformed(format!(
				"unknown object format: {other}"
			))),
		}
	}

	/// The symbolic ref HEAD points at, `None` when detached.
	/// An unborn branch still returns its symbolic name.
	pub fn head_ref(&self) -> Result<Option<String>, GitError> {
		self.head_ref_with(&RunOptions::default())
	}

	/// [`Git::head_ref`] under explicit runner options.
	///
	/// `symbolic-ref --quiet HEAD` uses `opts`. A quiet miss (detached HEAD)
	/// is still `Ok(None)`. Any other failure, including a truncated name,
	/// is returned as itself rather than a shortened ref.
	pub fn head_ref_with(
		&self,
		opts: &RunOptions,
	) -> Result<Option<String>, GitError> {
		const ARGS: &str = "symbolic-ref --quiet HEAD";
		already_cancelled(opts, ARGS)?;
		match self.run_with(&["symbolic-ref", "--quiet", "HEAD"], opts) {
			Ok(out) if out.truncated => Err(GitError::OutputLimit {
				args: ARGS.into(),
				limit: opts.max_stdout,
			}),
			Ok(out) => Ok(Some(
				String::from_utf8_lossy(&out.stdout).trim().to_string(),
			)),
			Err(e) if is_quiet_miss(&e) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// The parents of `sha`, in order (empty for a root or grafted commit).
	pub fn parents(&self, sha: &str) -> Result<Vec<String>, GitError> {
		self.parents_with(sha, &RunOptions::default())
	}

	/// [`Git::parents`] under explicit runner options. A truncated listing is
	/// [`GitError::OutputLimit`], not a shorter parent list.
	pub fn parents_with(
		&self,
		sha: &str,
		opts: &RunOptions,
	) -> Result<Vec<String>, GitError> {
		let label = format!("rev-list --parents -n 1 {sha}");
		let out =
			self.run_with(&["rev-list", "--parents", "-n", "1", sha], opts)?;
		if out.truncated {
			return Err(GitError::OutputLimit {
				args: label,
				limit: opts.max_stdout,
			});
		}
		Ok(String::from_utf8_lossy(&out.stdout)
			.split_ascii_whitespace()
			.skip(1)
			.map(str::to_string)
			.collect())
	}

	pub fn is_shallow(&self) -> Result<bool, GitError> {
		self.is_shallow_with(&RunOptions::default())
	}

	/// [`Git::is_shallow`] under explicit runner options.
	pub fn is_shallow_with(&self, opts: &RunOptions) -> Result<bool, GitError> {
		let out =
			self.run_with(&["rev-parse", "--is-shallow-repository"], opts)?;
		if out.truncated {
			return Err(GitError::OutputLimit {
				args: "rev-parse --is-shallow-repository".into(),
				limit: opts.max_stdout,
			});
		}
		Ok(out.stdout.trim_ascii() == b"true")
	}

	/// Starts a long-lived `git cat-file --batch`. It holds one budget slot
	/// until closed or dropped, so the same thread must not start another
	/// Git process meanwhile ([`GitError::NestedProcess`]).
	pub fn cat_file(&self) -> Result<CatFile, GitError> {
		self.cat_file_with(RunOptions::default())
	}

	/// [`Git::cat_file`] with an explicit per-request deadline and cancel.
	pub fn cat_file_with(&self, opts: RunOptions) -> Result<CatFile, GitError> {
		let mut cmd = self.command();
		cmd.args(["cat-file", "--batch"]);
		CatFile::spawn(cmd, opts)
	}
}

/// One record of `--raw -z --no-abbrev` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
	/// Status letter only; a rename score (`R100`) is dropped.
	pub status: u8,
	pub old_mode: String,
	pub new_mode: String,
	pub old_oid: String,
	pub new_oid: String,
	/// Source path of a rename or copy.
	pub old_path: Option<Vec<u8>>,
	pub path: Vec<u8>,
}

fn is_nul(b: &u8) -> bool {
	*b == 0
}

/// One-pass reader for [`parse_raw_z`]. Callers that must stop before the
/// end of a diff (a budget, a cancel) use this instead of collecting every
/// record first. Field splitting matches `split(0)`, including a trailing
/// empty field.
pub(crate) struct RawZ<'a> {
	fields: std::slice::Split<'a, u8, fn(&u8) -> bool>,
}

impl<'a> RawZ<'a> {
	pub(crate) fn new(out: &'a [u8]) -> Self {
		Self {
			fields: out.split(is_nul),
		}
	}

	pub(crate) fn next_entry(&mut self) -> Result<Option<RawEntry>, GitError> {
		let bad = || GitError::Malformed("raw diff record".into());
		let header = loop {
			match self.fields.next() {
				None => return Ok(None),
				Some([]) => continue,
				Some(header) => break header,
			}
		};
		let header =
			std::str::from_utf8(header.strip_prefix(b":").ok_or_else(bad)?)
				.map_err(|_| bad())?;
		let parts: Vec<&str> = header.split(' ').collect();
		let [old_mode, new_mode, old_oid, new_oid, status] = parts[..] else {
			return Err(bad());
		};
		let status = *status.as_bytes().first().ok_or_else(bad)?;
		let first = self.fields.next().ok_or_else(bad)?.to_vec();
		let (old_path, path) = if matches!(status, b'R' | b'C') {
			(Some(first), self.fields.next().ok_or_else(bad)?.to_vec())
		} else {
			(None, first)
		};
		Ok(Some(RawEntry {
			status,
			old_mode: old_mode.to_string(),
			new_mode: new_mode.to_string(),
			old_oid: old_oid.to_string(),
			new_oid: new_oid.to_string(),
			old_path,
			path,
		}))
	}
}

/// Parses `--raw -z` output at the byte level (paths may not be UTF-8):
/// `:<om> <nm> <ooid> <noid> <S>[score]\0<path>\0`, with two paths for
/// renames and copies.
pub fn parse_raw_z(out: &[u8]) -> Result<Vec<RawEntry>, GitError> {
	let mut records = RawZ::new(out);
	let mut entries = Vec::new();
	while let Some(entry) = records.next_entry()? {
		entries.push(entry);
	}
	Ok(entries)
}

/// Port of `mapGitStatusToChangeType` for porcelain letters.
pub fn change_type_for_status(status: u8) -> ChangeType {
	match status {
		b'A' | b'C' | b'U' => ChangeType::New,
		b'D' => ChangeType::Deleted,
		b'R' => ChangeType::Moved,
		_ => ChangeType::Modified,
	}
}

/// Which changes to copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitSource {
	/// Every uncommitted change, labelled and read like the SCM view:
	/// working tree, then untracked, then index; content from disk.
	Working,
	/// Index changes, content from the index.
	Staged,
	/// One commit; a merge is the union of its diffs against every parent.
	Commit(String),
	/// Endpoint comparison of two revisions.
	Range(String, String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitFiles {
	pub files: Vec<PayloadFile>,
	/// Dropped as non-UTF-8, binary or unreadable (including non-UTF-8
	/// path names, which cannot be put in a header).
	pub skipped_unreadable_count: usize,
}

/// One changed path after the union / dedupe step.
struct Change {
	status: u8,
	path: String,
	new_oid: String,
	/// Pre-deletion OIDs, in parent order; tried until one decodes.
	deleted_from: Vec<String>,
	/// The new side is a submodule commit (mode 160000), not a file.
	gitlink: bool,
}

/// Appends `entries` to `changes`, first entry per path wins (TS keys on
/// `renameUri ?? uri`). A later deletion of the same path adds its old OID.
fn union_into(
	changes: &mut Vec<Change>,
	entries: Vec<RawEntry>,
	skipped: &mut usize,
) {
	// Path -> position in `changes`, rebuilt per union because the Working
	// source unions four listings into one vector: a linear `find` per
	// entry made huge untracked-file listings quadratic.
	let mut index: HashMap<String, usize> = changes
		.iter()
		.enumerate()
		.map(|(i, c)| (c.path.clone(), i))
		.collect();
	for e in entries {
		let Ok(path) = String::from_utf8(e.path) else {
			*skipped += 1;
			continue;
		};
		if let Some(i) = index.get(&path).copied() {
			let c = &mut changes[i];
			if c.status == b'D' && e.status == b'D' {
				c.deleted_from.push(e.old_oid);
			}
			continue;
		}
		index.insert(path.clone(), changes.len());
		let deleted_from = if e.status == b'D' {
			vec![e.old_oid]
		} else {
			Vec::new()
		};
		changes.push(Change {
			status: e.status,
			path,
			new_oid: e.new_oid,
			deleted_from,
			gitlink: e.new_mode == "160000",
		});
	}
}

fn path_entry(status: u8, path: Vec<u8>) -> RawEntry {
	RawEntry {
		status,
		old_mode: String::new(),
		new_mode: String::new(),
		old_oid: String::new(),
		new_oid: String::new(),
		old_path: None,
		path,
	}
}

/// Unmerged paths from `git ls-files -u -z`, labelled like VS Code's merge
/// changes: a conflict missing ours or theirs (UD, DU, DD) is `D`, every
/// other kind (UU, AA, AU, UA) is `M`.
fn unmerged(
	git: &Git,
	only: &[&str],
	opts: &RunOptions,
) -> Result<Vec<RawEntry>, GitError> {
	let mut args = vec!["ls-files", "-u", "-z"];
	args.extend_from_slice(only);
	let out = run_strict(git, &args, opts)?;
	// Each record is `<mode> <oid> <stage>\t<path>`, one per stage.
	let mut stages: Vec<(Vec<u8>, [bool; 3])> = Vec::new();
	for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
		let bad = || GitError::Malformed("ls-files -u record".into());
		let tab = rec.iter().position(|&b| b == b'\t').ok_or_else(bad)?;
		let stage = match rec[..tab].last() {
			Some(b @ b'1'..=b'3') => usize::from(b - b'1'),
			_ => return Err(bad()),
		};
		let path = rec[tab + 1..].to_vec();
		// Stages of one path are adjacent.
		match stages.last_mut() {
			Some((p, s)) if *p == path => s[stage] = true,
			_ => {
				let mut s = [false; 3];
				s[stage] = true;
				stages.push((path, s));
			}
		}
	}
	Ok(stages
		.into_iter()
		.map(|(path, [base, ours, theirs])| {
			let deleted = base && !(ours && theirs);
			path_entry(if deleted { b'D' } else { b'M' }, path)
		})
		.collect())
}

const RAW: [&str; 4] = ["-z", "--raw", "--no-abbrev", "-M"];

fn run_strict_diff(
	git: &Git,
	args: &[&str],
	opts: &RunOptions,
) -> Result<Vec<u8>, GitError> {
	let output = git.run_diff_with(args, opts)?;
	if output.truncated {
		return Err(GitError::OutputLimit {
			args: args.join(" "),
			limit: opts.max_stdout,
		});
	}
	Ok(output.stdout)
}

fn diff(
	git: &Git,
	args: &[&str],
	opts: &RunOptions,
) -> Result<Vec<RawEntry>, GitError> {
	let mut all = vec!["diff"];
	all.extend_from_slice(args);
	parse_raw_z(&run_strict_diff(git, &all, opts)?)
}

/// Collects the changed files of `source` as payload files.
pub fn collect(git: &Git, source: &GitSource) -> Result<GitFiles, GitError> {
	let (all, mut skipped) = collect_raw(git, source)?;
	let mut files = Vec::with_capacity(all.len());
	for f in all {
		if f.content.is_some() {
			files.push(f);
		} else {
			skipped += 1;
		}
	}
	Ok(GitFiles {
		files,
		skipped_unreadable_count: skipped,
	})
}

/// Detailed listing of a changed path: relative path, change type, and whether
/// the entry is a gitlink (submodule pointer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPathEntry {
	pub path: String,
	pub change_type: ChangeType,
	pub gitlink: bool,
}

/// Lists all changes for `source` with their change types and gitlink flags,
/// plus the number of entries dropped because their path name was not UTF-8.
pub fn list_changes_with(
	git: &Git,
	source: &GitSource,
	opts: &RunOptions,
) -> Result<(Vec<ChangedPathEntry>, usize), GitError> {
	let (changes, skipped) = collect_changes(git, source, opts)?;
	let entries = changes
		.into_iter()
		.map(|c| ChangedPathEntry {
			path: c.path,
			change_type: change_type_for_status(c.status),
			gitlink: c.gitlink,
		})
		.collect();
	already_cancelled(opts, "list changes")?;
	Ok((entries, skipped))
}

/// Changed paths for the browser, including files whose content cannot be
/// put on the clipboard (binary or unreadable), with cancellation, deadlines
/// and strict metadata output limits propagated through every nested Git
/// command.
pub fn list_changed_paths_with(
	git: &Git,
	source: &GitSource,
	opts: &RunOptions,
) -> Result<Vec<(String, Option<ChangeType>)>, GitError> {
	Ok(list_changed_paths_and_gitlinks_with(git, source, opts)?.0)
}

/// Changed paths and their change types, as the desktop browser lists them.
pub type ChangedPaths = Vec<(String, Option<ChangeType>)>;

/// [`list_changed_paths_with`], plus the listed paths whose new side is a
/// submodule commit (gitlink): they have no content to read or copy.
pub fn list_changed_paths_and_gitlinks_with(
	git: &Git,
	source: &GitSource,
	opts: &RunOptions,
) -> Result<(ChangedPaths, Vec<String>), GitError> {
	let (changes, _) = list_changes_with(git, source, opts)?;
	let gitlinks = changes
		.iter()
		.filter(|c| c.gitlink)
		.map(|c| c.path.clone())
		.collect();
	let paths = changes
		.into_iter()
		.map(|c| (c.path, Some(c.change_type)))
		.collect();
	already_cancelled(opts, "list changed paths")?;
	Ok((paths, gitlinks))
}

/// Ordered change metadata without reading file contents, plus the number of
/// entries dropped for a non-UTF-8 path name.
fn collect_changes(
	git: &Git,
	source: &GitSource,
	opts: &RunOptions,
) -> Result<(Vec<Change>, usize), GitError> {
	collect_changes_in(git, source, None, opts)
}

/// [`collect_changes`], limited to the one path `only` when given. Rename
/// detection then sees only that side: a rename target is an addition.
fn collect_changes_in(
	git: &Git,
	source: &GitSource,
	only: Option<&str>,
	opts: &RunOptions,
) -> Result<(Vec<Change>, usize), GitError> {
	already_cancelled(opts, "collect changes")?;
	let spec = only.map(|p| format!(":(literal){p}"));
	let only: Vec<&str> =
		spec.as_deref().map_or_else(Vec::new, |s| vec!["--", s]);
	let raw = |extra: &[&'static str]| {
		let mut args = extra.to_vec();
		args.extend_from_slice(&RAW);
		args
	};
	let mut skipped = 0;
	let mut changes = Vec::new();
	match source {
		GitSource::Working => {
			// VS Code keeps conflicts out of the working and index lists and
			// reports them as merge changes between untracked and index.
			let conflicts = unmerged(git, &only, opts)?;
			let conflict_paths: HashSet<Vec<u8>> =
				conflicts.iter().map(|c| c.path.clone()).collect();
			let resolved = |e: &RawEntry| !conflict_paths.contains(&e.path);
			let mut args = raw(&[]);
			args.extend_from_slice(&only);
			let worktree = diff(git, &args, opts)?
				.into_iter()
				.filter(resolved)
				.collect();
			union_into(&mut changes, worktree, &mut skipped);
			let mut args =
				vec!["ls-files", "--others", "--exclude-standard", "-z"];
			args.extend_from_slice(&only);
			let out = run_strict(git, &args, opts)?;
			let untracked = out
				.split(|&b| b == 0)
				// A trailing `/` is a nested repository, not a file.
				.filter(|p| !p.is_empty() && !p.ends_with(b"/"))
				.map(|p| path_entry(b'A', p.to_vec()))
				.collect();
			union_into(&mut changes, untracked, &mut skipped);
			let merge = conflicts.clone();
			union_into(&mut changes, merge, &mut skipped);
			let mut cached = raw(&["--cached"]);
			cached.extend_from_slice(&only);
			let index = diff(git, &cached, opts)?
				.into_iter()
				.filter(resolved)
				.collect();
			union_into(&mut changes, index, &mut skipped);
			// TS reads every deletion at `HEAD:<path>`; an unborn HEAD is
			// simply missing there, so the marker follows.
			for c in changes.iter_mut().filter(|c| c.status == b'D') {
				c.deleted_from = vec![format!("HEAD:{}", c.path)];
			}
		}
		GitSource::Staged => {
			let mut cached = raw(&["--cached"]);
			cached.extend_from_slice(&only);
			// A conflict is a merge change, not an index change.
			let index = diff(git, &cached, opts)?
				.into_iter()
				.filter(|e| e.status != b'U')
				.collect();
			union_into(&mut changes, index, &mut skipped);
		}
		GitSource::Commit(rev) => {
			let sha = git.resolve_commit_with(rev, opts)?;
			let mut parents = git.parents_with(&sha, opts)?;
			if parents.is_empty() {
				// A shallow clone grafts its boundary commits to look
				// parentless; diffing one against the empty tree would copy
				// the whole repository as "this commit's change".
				if git.is_shallow_with(opts)? {
					return Err(GitError::Shallow(sha));
				}
				parents.push(git.empty_tree_with(opts)?);
			}
			for parent in &parents {
				let mut args = vec![
					"diff-tree",
					"-r",
					"-z",
					"--raw",
					"--no-abbrev",
					"--no-commit-id",
					"-M",
					parent,
					&sha,
				];
				args.extend_from_slice(&only);
				let out = run_strict(git, &args, opts)?;
				union_into(&mut changes, parse_raw_z(&out)?, &mut skipped);
			}
		}
		GitSource::Range(from, to) => {
			let from = git.resolve_commit_with(from, opts)?;
			let to = git.resolve_commit_with(to, opts)?;
			let mut args = RAW.to_vec();
			args.extend([from.as_str(), to.as_str()]);
			args.extend_from_slice(&only);
			union_into(&mut changes, diff(git, &args, opts)?, &mut skipped);
		}
	}

	already_cancelled(opts, "collect changes")?;
	Ok((changes, skipped))
}

fn collect_raw(
	git: &Git,
	source: &GitSource,
) -> Result<(Vec<PayloadFile>, usize), GitError> {
	let opts = RunOptions::default();
	let (changes, skipped) = collect_changes(git, source, &opts)?;
	Ok((read_changes(git, source, changes, None, &opts)?, skipped))
}

/// Read only the clicked path; listing a large repository never reads blobs.
/// `None` when `path` is not a change of `source`: every listing is limited
/// to `path`, so its output stays small however large the change set is.
/// Rename detection then sees only this side, so a rename target is
/// labelled `New`, not `Moved`; with the listed label at hand, use
/// [`read_changed_file_for`], which also skips the listing. Content above `max` bytes is
/// [`GitError::OutputLimit`], judged from the blob header or file size
/// before it is read.
pub fn read_changed_file(
	git: &Git,
	source: &GitSource,
	path: &str,
	max: u64,
) -> Result<Option<PayloadFile>, GitError> {
	read_changed_file_inner(git, source, path, max, &RunOptions::default())
}

/// [`read_changed_file`] with explicit runner options through membership,
/// revision resolution, deleted-content lookup and blob reads. Both metadata
/// and content are strict even with [`crate::gitrun::Overflow::Truncate`].
/// Content is capped at the smaller of `max` and `opts.max_stdout`.
pub fn read_changed_file_with(
	git: &Git,
	source: &GitSource,
	path: &str,
	max: u64,
	opts: &RunOptions,
) -> Result<Option<PayloadFile>, GitError> {
	read_changed_file_inner(
		git,
		source,
		path,
		max.min(opts.max_stdout as u64),
		opts,
	)
}

fn read_changed_file_inner(
	git: &Git,
	source: &GitSource,
	path: &str,
	max: u64,
	opts: &RunOptions,
) -> Result<Option<PayloadFile>, GitError> {
	let (changes, _) = collect_changes_in(git, source, Some(path), opts)?;
	Ok(read_changes(
		git,
		source,
		changes.into_iter().filter(|c| c.path == path).collect(),
		Some(max),
		opts,
	)?
	.pop())
}

/// Reads one change the caller already listed (`path` and `change_type` as
/// [`list_changed_paths_with`] returned them), without listing anything.
/// Content comes by name: the disk (`Working`), `:<path>` (`Staged`),
/// `<rev>:<path>` (`Commit`) or `<to>:<path>` (`Range`); a deletion reads
/// `HEAD:<path>`, `<from>:<path>`, or each commit parent in order (the
/// deleted marker when none decodes). For a commit, pass its `parents` when
/// known and a resolved `rev`; `None` looks the parents up. Content above
/// the smaller of `max` and `opts.max_stdout` is [`GitError::OutputLimit`].
pub fn read_changed_file_for(
	git: &Git,
	source: &GitSource,
	path: &str,
	change_type: ChangeType,
	parents: Option<&[String]>,
	max: u64,
	opts: &RunOptions,
) -> Result<PayloadFile, GitError> {
	already_cancelled(opts, "read changed file")?;
	let at = |rev: &str| format!("{rev}:{path}");
	let (new_oid, deleted_from) = match source {
		GitSource::Working => (String::new(), vec![at("HEAD")]),
		GitSource::Staged => (at(""), vec![at("HEAD")]),
		GitSource::Range(from, to) => (at(to), vec![at(from)]),
		GitSource::Commit(rev) => {
			let deleted_from = if change_type != ChangeType::Deleted {
				Vec::new()
			} else if let Some(parents) = parents {
				parents.iter().map(|p| at(p)).collect()
			} else {
				// Resolving first refuses an option-like `rev`.
				let sha = git.resolve_commit_with(rev, opts)?;
				git.parents_with(&sha, opts)?
					.iter()
					.map(|p| at(p))
					.collect()
			};
			(at(rev), deleted_from)
		}
	};
	let status = match change_type {
		ChangeType::New => b'A',
		ChangeType::Modified => b'M',
		ChangeType::Deleted => b'D',
		ChangeType::Moved => b'R',
	};
	let change = Change {
		status,
		path: path.to_string(),
		new_oid,
		deleted_from,
		gitlink: false,
	};
	let max = max.min(opts.max_stdout as u64);
	read_changes(git, source, vec![change], Some(max), opts)?
		.pop()
		.ok_or_else(|| GitError::Malformed("no file read".into()))
}

/// A working-tree file, at most `max` bytes when given. Unreadable is
/// `None` like the SCM view (TS parity); too large is an error.
fn read_working(
	path: &Path,
	max: Option<u64>,
) -> Result<Option<String>, GitError> {
	let Ok(meta) = std::fs::metadata(path) else {
		return Ok(None);
	};
	if !meta.is_file() {
		return Ok(None);
	}
	let Some(max) = max else {
		return Ok(read_text_file(path).ok().flatten());
	};
	let Ok(file) = std::fs::File::open(path) else {
		return Ok(None);
	};
	let mut bytes = Vec::new();
	file.take(max + 1).read_to_end(&mut bytes)?;
	if bytes.len() as u64 > max {
		return Err(GitError::OutputLimit {
			args: path.display().to_string(),
			limit: usize::try_from(max).unwrap_or(usize::MAX),
		});
	}
	Ok(decode_utf8_or_skip(bytes))
}

fn blob_output_limit(spec: &str, cap: u64) -> GitError {
	GitError::OutputLimit {
		args: format!("cat-file {spec}"),
		limit: usize::try_from(cap).unwrap_or(usize::MAX),
	}
}

fn copyable(
	read: BlobRead,
	spec: &str,
	cap: u64,
) -> Result<Option<String>, GitError> {
	match read {
		BlobRead::Text(s) => Ok(Some(s)),
		BlobRead::Missing | BlobRead::NotText(_) => Ok(None),
		// lenient reader 絕不產出 NotABlob，保留此分支僅為維持窮舉編譯。
		BlobRead::NotABlob { .. } => Ok(None),
		BlobRead::TooLarge { .. } => Err(blob_output_limit(spec, cap)),
	}
}

fn read_changes(
	git: &Git,
	source: &GitSource,
	changes: Vec<Change>,
	max: Option<u64>,
	opts: &RunOptions,
) -> Result<Vec<PayloadFile>, GitError> {
	already_cancelled(opts, "read changes")?;
	let mut blobs = BlobReader::new(opts);
	let cap = max.unwrap_or(u64::MAX);
	let mut files = Vec::new();
	for c in changes {
		already_cancelled(opts, "read changed file")?;
		let change_type = change_type_for_status(c.status);
		let content = if change_type == ChangeType::Deleted {
			match blobs.deleted_file_content(git, &c.deleted_from, cap)? {
				DeletedContent::Text(t) => Some(t),
				DeletedContent::Marker => Some(DELETED_FILE_MARKER.to_string()),
				DeletedContent::TooLarge { spec, .. } => {
					return Err(blob_output_limit(&spec, cap));
				}
			}
		} else {
			match source {
				GitSource::Working => {
					if git.boundary.is_some() {
						let file_path = git.root.join(&c.path);
						if file_path.symlink_metadata().is_ok() {
							match dunce::canonicalize(&file_path) {
								Ok(canonical_file) => {
									let canonical_root =
										dunce::canonicalize(&git.root)?;
									if !canonical_file
										.starts_with(&canonical_root)
									{
										return Err(
											GitError::OutsideBoundary {
												what: "working file",
											},
										);
									}
									if let Ok(m) = canonical_file.metadata() {
										if !m.is_file() && !m.is_dir() {
											return Err(
												GitError::OutsideBoundary {
													what: "working file",
												},
											);
										}
									}
								}
								Err(e)
									if e.kind()
										== std::io::ErrorKind::NotFound =>
								{
									// Dangling symlink: skip boundary check and fall through to
									// read_working, which yields Ok(None) matching local mode.
								}
								Err(e) => return Err(GitError::from(e)),
							}
						}
					}
					read_working(&git.root.join(&c.path), max)?
				}
				// Staged content was asked for: an index entry that cannot
				// be read is a visible placeholder, never a silent gap.
				// TS: `readRefContent(...) ?? UNREADABLE_FILE_MARKER`, so a
				// non-UTF-8 blob is the marker too.
				GitSource::Staged => Some(
					copyable(
						blobs.read(git, &c.new_oid, cap)?,
						&c.new_oid,
						cap,
					)?
					.unwrap_or_else(|| UNREADABLE_FILE_MARKER.to_string()),
				),
				_ => copyable(
					blobs.read(git, &c.new_oid, cap)?,
					&c.new_oid,
					cap,
				)?,
			}
		};
		already_cancelled(opts, "read changed file")?;
		files.push(PayloadFile {
			path: c.path,
			content,
			change_type: Some(change_type),
			skipped_reason: None,
		});
	}
	blobs.close()?;
	already_cancelled(opts, "read changes")?;
	Ok(files)
}

/// TS `normalizeFsPath`: a comparison key only, case-folded on Windows.
fn fs_path_key(path: &Path) -> String {
	let key = path.to_string_lossy().replace('\\', "/");
	let key = key.trim_end_matches('/');
	if cfg!(windows) {
		key.to_lowercase()
	} else {
		key.to_string()
	}
}

/// Test oracle: not called by product code.
///
/// Port of `collectGitPayloadFiles` (extension.ts) plus the payload build:
/// the files of `source` with filters, limits and counts applied, labelled
/// against `workspace_roots` (the repository root when there are none).
pub fn collect_payload<P: AsRef<Path>>(
	git: &Git,
	source: &GitSource,
	workspace_roots: &[P],
	settings: &Settings,
) -> Result<CopyResult, GitError> {
	collect_payload_with_selection(
		git,
		source,
		workspace_roots,
		settings,
		None,
		None,
	)
}

/// Test oracle: not called by product code.
///
/// Constrain a source to a monorepo folder and optional
/// repository-relative paths without changing the CLI's all-files behavior.
pub fn collect_payload_with_selection<P: AsRef<Path>>(
	git: &Git,
	source: &GitSource,
	workspace_roots: &[P],
	settings: &Settings,
	scope: Option<&Path>,
	selected: Option<&HashSet<String>>,
) -> Result<CopyResult, GitError> {
	// git reports its toplevel fully resolved (macOS `/private/var`, Windows
	// long names), so the roots must be too or no file relativizes.
	let mut roots: Vec<PathBuf> = workspace_roots
		.iter()
		.map(|r| {
			dunce::canonicalize(r.as_ref())
				.unwrap_or_else(|_| r.as_ref().to_path_buf())
		})
		.collect();
	if roots.is_empty() {
		roots.push(git.root().to_path_buf());
	}
	let (collected, path_skips) = collect_raw(git, source)?;
	let mut files = Vec::new();
	let mut seen = HashSet::new();
	let mut copied = 0;
	let mut skipped_size = 0;
	let mut skipped_unreadable = path_skips;
	let mut limit_reached = false;
	// Commit and range copies are the graph surface, which always uses the
	// git builder; SCM falls back to it for deleted or index content.
	let graph = matches!(source, GitSource::Commit(_) | GitSource::Range(..));
	let mut fallback = graph;
	let scope = scope
		.map(|p| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
	for file in collected {
		let absolute = git.root().join(&file.path);
		if scope.as_ref().is_some_and(|p| !absolute.starts_with(p))
			|| selected.is_some_and(|paths| !paths.contains(&file.path))
		{
			continue;
		}
		// graphCopy keys on the exact path; only SCM case-folds on Windows.
		let key = if graph {
			absolute.to_string_lossy().into_owned()
		} else {
			fs_path_key(&absolute)
		};
		if !seen.insert(key) {
			continue;
		}
		let absolute = absolute.to_string_lossy().into_owned();
		let filter_path = to_clipboard_path_from_roots(&roots, &absolute, None);
		// graphCopy labels a single repo's files repo-relative and filters
		// on the workspace-relative spelling; SCM uses the latter for both.
		let path = if graph {
			file.path.clone()
		} else {
			filter_path.clone()
		};
		if settings.use_filters
			&& !file_matches_filters(
				&filter_path,
				&settings.filter_rules,
				settings.use_include_filters,
				settings.use_exclude_filters,
				Some(&absolute),
			) {
			continue;
		}
		// Checked after dedupe and filtering, so the flag means a copyable
		// candidate was actually dropped.
		if settings.set_max_file_count
			&& copied as f64 >= settings.file_count_limit
		{
			limit_reached = true;
			break;
		}
		// Both TS surfaces check the limit first, then drop unreadable
		// content; only graphCopy counts it (the SCM path just skips it).
		let Some(content) = file.content else {
			if graph {
				skipped_unreadable += 1;
			}
			continue;
		};
		if file.change_type == Some(ChangeType::Deleted)
			|| *source == GitSource::Staged
		{
			fallback = true;
		}
		let size = content.len();
		// graphCopy pushes a deleted file's old body before any size check.
		let deleted_on_graph =
			graph && file.change_type == Some(ChangeType::Deleted);
		if !deleted_on_graph && size as f64 > settings.max_file_size_kb * 1024.0
		{
			skipped_size += 1;
			files.push(PayloadFile {
				path,
				content: None,
				change_type: file.change_type,
				skipped_reason: Some(format!(
					"size exceeds limit ({size} bytes)"
				)),
			});
			continue;
		}
		// The marker travels in the payload but is not a copied file and
		// does not consume the limit.
		if content == UNREADABLE_FILE_MARKER {
			skipped_unreadable += 1;
		} else {
			copied += 1;
		}
		files.push(PayloadFile {
			path,
			content: Some(content),
			change_type: file.change_type,
			skipped_reason: None,
		});
	}
	let has_files = !files.is_empty();
	let options = BuildPayloadOptions {
		header_format: settings.header_format.clone(),
		pre_text: settings.pre_text.clone(),
		post_text: settings.post_text.clone(),
		add_extra_line_between_files: settings.add_extra_line_between_files,
		files,
		// graphCopy names the repository (`singleRepoRoot`), SCM the
		// single workspace root.
		source_root: if graph {
			// `singleRepoRoot(files)` is undefined for an empty payload.
			has_files.then(|| source_root_name(&[git.root()])).flatten()
		} else {
			source_root_name(&roots)
		},
	};
	let payload = if fallback {
		build_git_payload(&options)
	} else {
		build_payload(&options)
	};
	Ok(CopyResult {
		files: options.files,
		payload,
		copied_file_count: copied,
		skipped_file_size_count: skipped_size,
		skipped_unreadable_count: skipped_unreadable,
		file_limit_reached: limit_reached,
	})
}

#[cfg(test)]
mod tests {

	#[test]
	fn open_outside_a_repository_says_so() {
		let dir = tempfile::tempdir().unwrap();
		let err = Git::open(dir.path()).err().unwrap();
		assert!(matches!(err, GitError::NotARepository(_)), "{err}");
		assert!(err.to_string().contains("is not inside a git repository"));
	}

	/// `union_into` feeds `list_changes_with`, whose row order the desktop
	/// browser and the payload layout rely on: entries must come out in
	/// first-seen input order across repeated unions, whatever the HashMap
	/// iteration order inside is, and the merge rules must not change with
	/// the lookup.
	#[test]
	fn union_into_keeps_first_seen_order_across_unions() {
		let entry = |status: u8, path: &str| RawEntry {
			status,
			old_mode: String::new(),
			new_mode: "100644".into(),
			old_oid: format!("old-{path}"),
			new_oid: format!("new-{path}"),
			old_path: None,
			path: path.as_bytes().to_vec(),
		};
		let mut changes = Vec::new();
		let mut skipped = 0;

		// First union: b, c, a, g.
		union_into(
			&mut changes,
			vec![
				entry(b'A', "b.txt"),
				entry(b'M', "c.txt"),
				entry(b'A', "a.txt"),
				entry(b'D', "g.txt"),
			],
			&mut skipped,
		);
		// Second union repeats paths (a different listing sees them again)
		// and adds d.
		union_into(
			&mut changes,
			vec![
				entry(b'M', "c.txt"),
				entry(b'D', "a.txt"),
				entry(b'A', "d.txt"),
				entry(b'D', "g.txt"),
			],
			&mut skipped,
		);
		// A third deletion of a path already deleted appends its old OID.
		union_into(&mut changes, vec![entry(b'D', "g.txt")], &mut skipped);

		let paths: Vec<&str> =
			changes.iter().map(|c| c.path.as_str()).collect();
		assert_eq!(paths, ["b.txt", "c.txt", "a.txt", "g.txt", "d.txt"]);
		assert_eq!(skipped, 0);
		// First sighting wins: c stays an M with the first union's OIDs.
		let c = &changes[1];
		assert_eq!(c.status, b'M');
		assert_eq!(c.new_oid, "new-c.txt");
		assert_eq!(c.deleted_from, Vec::<String>::new());
		// A deletion behind an earlier non-deleted sighting changes nothing
		// on a's row.
		let a = &changes[2];
		assert_eq!(a.status, b'A');
		assert_eq!(a.new_oid, "new-a.txt");
		assert_eq!(a.deleted_from, Vec::<String>::new());
		// Deletions of a path first seen deleted accumulate on its row.
		let g = &changes[3];
		assert_eq!(g.status, b'D');
		assert_eq!(g.deleted_from, vec!["old-g.txt", "old-g.txt", "old-g.txt"]);
	}

	/// Initializes `root` as a SHA-256 repository, or returns `false` when
	/// this git is too old to know the format.
	fn init_sha256(root: &Path, cfg: &Path) -> bool {
		let out = std::process::Command::new("git")
			.args(["init", "-q", "-b", "main", "--object-format=sha256"])
			.current_dir(root)
			.env("GIT_CONFIG_GLOBAL", cfg)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.output()
			.unwrap();
		out.status.success()
	}

	fn assert_sha256_supported(reason: &str) {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"SNIP_REQUIRE_ALL_TESTS is set but {reason}"
		);
	}

	/// A SHA-256 repository has a different well-known empty tree OID. A
	/// root commit diffs against that tree, not against the SHA-1
	/// constant, or listing and copying it fail outright.
	#[test]
	fn root_commit_lists_against_the_sha256_empty_tree() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("r");
		fs::create_dir_all(&root).unwrap();
		let cfg = dir.path().join("cfg");
		fs::write(&cfg, "").unwrap();
		if !init_sha256(&root, &cfg) {
			assert_sha256_supported("git lacks --object-format=sha256");
			return;
		}
		let run = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.args(args)
				.current_dir(&root)
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
		run(&["config", "user.name", "T"]);
		run(&["config", "user.email", "t@example.com"]);
		fs::write(root.join("one.txt"), "one\n").unwrap();
		run(&["add", "-A"]);
		run(&["commit", "-q", "-m", "root"]);
		let sha = run(&["rev-parse", "HEAD"]);
		let git = Git::open(&root).unwrap();

		assert_eq!(
			git.empty_tree_with(&RunOptions::default()).unwrap(),
			SHA256_EMPTY_TREE
		);

		let (changes, skipped) = list_changes_with(
			&git,
			&GitSource::Commit(sha),
			&RunOptions::default(),
		)
		.unwrap();
		assert_eq!(skipped, 0);
		let paths: Vec<&str> =
			changes.iter().map(|c| c.path.as_str()).collect();
		assert_eq!(paths, ["one.txt"]);
	}

	/// A SHA-1 repository still gets the classic constant.
	#[test]
	fn sha1_repository_reports_the_classic_empty_tree() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let out = std::process::Command::new("git")
			.args(["init", "-q", "-b", "main"])
			.current_dir(root)
			.output()
			.unwrap();
		assert!(out.status.success());
		let git = Git::open(root).unwrap();
		assert_eq!(
			git.empty_tree_with(&RunOptions::default()).unwrap(),
			EMPTY_TREE
		);
	}

	#[test]
	fn open_checks_the_git_version_once_and_known_roots_start_nothing() {
		let r = Repo::new();
		let g = Git::open(&r.path()).unwrap();
		let runs = VERSION_RUNS.with(std::cell::Cell::get);
		let again = Git::open(&r.path()).unwrap();
		assert_eq!(VERSION_RUNS.with(std::cell::Cell::get), runs);
		assert_eq!(again.root(), g.root());
		let id =
			crate::workspace::RepoIdentity::resolve(&g, &RunOptions::default())
				.unwrap();
		let known = Git::at_known_root(&id);
		assert_eq!(known.root(), g.root());
		assert_eq!(known.head().unwrap(), g.head().unwrap());
	}

	use super::*;
	use std::fs;
	#[test]
	fn cat_file_process_round_trip() {
		let r = Repo::new();
		r.write("a.txt", b"hello\nworld");
		r.commit("init");
		let mut cat = Git::open(&r.path()).unwrap().cat_file().unwrap();
		assert_eq!(
			cat.read("HEAD:a.txt").unwrap().as_deref(),
			Some(&b"hello\nworld"[..])
		);
		assert_eq!(cat.read("HEAD:nope.txt").unwrap(), None);
		assert_eq!(cat.read("HEAD:a.txt").unwrap().map(|b| b.len()), Some(11));
	}

	#[test]
	// 放在 gitsrc::tests 是為了重用這裡的 Repo helper。
	fn read_classified_does_not_keep_an_oversize_body_and_stays_in_sync() {
		let r = Repo::new();
		let big = "你".repeat(12_000);
		r.write("big.txt", big.as_bytes());
		r.write("late.bin", &{
			let mut v = vec![b'B'; 20_000];
			v.push(0);
			v
		});
		r.write("bad.txt", &{
			let v = vec![0xFF; 10_000];
			v
		});
		r.write("small.txt", b"small\n");
		r.commit("blobs");
		let git = Git::open(&r.path()).unwrap();
		let big_oid = git.run(&["rev-parse", "HEAD:big.txt"]).unwrap();
		let big_oid = String::from_utf8(big_oid).unwrap();
		let big_oid = big_oid.trim();
		let mut cat = git.cat_file().unwrap();
		assert_eq!(
			cat.read_classified(big_oid, 64, false).unwrap(),
			BlobRead::TooLarge {
				size: big.len() as u64,
				not_text: None,
			}
		);
		match cat.read_classified("HEAD:late.bin", 32, false).unwrap() {
			BlobRead::TooLarge {
				not_text: Some(crate::blob::NotText::Binary),
				..
			} => {}
			other => panic!("{other:?}"),
		}
		match cat.read_classified("HEAD:bad.txt", 32, false).unwrap() {
			BlobRead::TooLarge {
				not_text: Some(crate::blob::NotText::NotUtf8),
				..
			} => {}
			other => panic!("{other:?}"),
		}
		assert_eq!(
			cat.read_classified("HEAD:small.txt", 64, false).unwrap(),
			BlobRead::Text("small\n".into())
		);
		cat.close().unwrap();
	}

	#[test]
	// 放在 gitsrc::tests 是為了重用這裡的 Repo helper。
	fn blob_reader_reopens_on_another_repository_and_close_is_ok_when_unused() {
		let mut unused = BlobReader::new(&RunOptions::default());
		assert!(unused.close().is_ok());

		let r_a = Repo::new();
		r_a.write("f.txt", b"repo A\n");
		r_a.commit("init A");
		let g_a = Git::open(&r_a.path()).unwrap();

		let r_b = Repo::new();
		r_b.write("f.txt", b"repo B\n");
		r_b.commit("init B");
		let g_b = Git::open(&r_b.path()).unwrap();

		let mut reader = BlobReader::new(&RunOptions::default());
		assert_eq!(
			reader.read(&g_a, "HEAD:f.txt", 1024).unwrap(),
			BlobRead::Text("repo A\n".into())
		);
		assert_eq!(
			reader.read(&g_b, "HEAD:f.txt", 1024).unwrap(),
			BlobRead::Text("repo B\n".into())
		);
		assert_eq!(
			reader.read(&g_a, "HEAD:f.txt", 1024).unwrap(),
			BlobRead::Text("repo A\n".into())
		);
		reader.close().unwrap();
	}

	#[test]
	// 放在 gitsrc::tests 是為了重用這裡的 Repo helper。
	fn blob_reader_polls_cancel_before_each_read() {
		let r = Repo::new();
		r.write("a.txt", b"ok\n");
		r.commit("init");
		let g = Git::open(&r.path()).unwrap();

		// (a) 開啟 session 成功讀取一次，接著取消 token，確認在已開啟的 session 上仍會遵守取消（回傳 Cancelled）。
		let token = CancelToken::new();
		let opts = RunOptions {
			cancel: Some(token.clone()),
			..RunOptions::default()
		};
		let mut reader = BlobReader::new(&opts);
		assert_eq!(
			reader.read(&g, "HEAD:a.txt", 1024).unwrap(),
			BlobRead::Text("ok\n".into())
		);
		assert!(reader.is_open());
		token.cancel();
		let err = reader.read(&g, "HEAD:a.txt", 1024).unwrap_err();
		match err {
			GitError::Cancelled { args } => {
				assert!(args.contains("cat-file"), "args was {args}");
			}
			other => panic!("expected Cancelled, got {other:?}"),
		}

		// (b) 首次讀取前即已取消：驗證立即回傳 Cancelled 且絕不 spawn 任何 cat-file session。
		let token_b = CancelToken::new();
		token_b.cancel();
		let opts_b = RunOptions {
			cancel: Some(token_b),
			..RunOptions::default()
		};
		let mut reader_b = BlobReader::new(&opts_b);
		let err_b = reader_b.read(&g, "HEAD:a.txt", 1024).unwrap_err();
		match err_b {
			GitError::Cancelled { args } => {
				assert!(args.contains("cat-file"), "args was {args}");
			}
			other => panic!("expected Cancelled, got {other:?}"),
		}
		assert!(!reader_b.is_open());
	}

	#[test]
	fn payload_gitlink_commit_object_copies_commit_text_as_content() {
		let r = Repo::new();
		r.write("a.txt", b"a\n");
		let base = r.commit("base");
		let g_base = Git::open(&r.path()).unwrap();
		let base_res = collect_payload(
			&g_base,
			&GitSource::Commit("HEAD".into()),
			&[r.path()],
			&Settings::default(),
		)
		.unwrap();

		r.git(&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{base},sub"),
		]);
		r.write("a.txt", b"b\n");
		r.git(&["add", "a.txt"]);

		// 1. Staged: gitlink whose commit object is in odb copies the commit object text as content
		let g_staged = Git::open(&r.path()).unwrap();
		let res_staged = collect_payload(
			&g_staged,
			&GitSource::Staged,
			&[r.path()],
			&Settings::default(),
		)
		.unwrap();
		assert!(res_staged.files.iter().any(|f| f.path == "sub"));
		let sub_staged =
			res_staged.files.iter().find(|f| f.path == "sub").unwrap();
		assert!(sub_staged.content.as_deref().unwrap().contains("tree "));
		assert_ne!(sub_staged.content.as_deref(), Some(UNREADABLE_FILE_MARKER));
		assert_eq!(
			res_staged.skipped_unreadable_count,
			base_res.skipped_unreadable_count
		);

		// 2. Commit: gitlink whose commit object is in odb copies the commit object text as content
		r.git(&["commit", "-q", "-m", "bump"]);
		let g = Git::open(&r.path()).unwrap();
		let res = collect_payload(
			&g,
			&GitSource::Commit("HEAD".into()),
			&[r.path()],
			&Settings::default(),
		)
		.unwrap();

		assert!(res.files.iter().any(|f| f.path == "sub"));
		let sub_commit = res.files.iter().find(|f| f.path == "sub").unwrap();
		assert!(sub_commit.content.as_deref().unwrap().contains("tree "));
		assert_ne!(sub_commit.content.as_deref(), Some(UNREADABLE_FILE_MARKER));
		assert_eq!(
			res.skipped_unreadable_count,
			base_res.skipped_unreadable_count
		);
	}

	#[test]
	fn read_changed_file_for_deleted_oversize_binary_is_the_marker() {
		let r = Repo::new();
		let mut bin = vec![b'a'; 20_000];
		bin[10] = 0;
		r.write("x.bin", &bin);
		let text = "a\n".repeat(10_000);
		r.write("x.txt", text.as_bytes());
		r.commit("add files");

		r.git(&["rm", "x.bin", "x.txt"]);
		let sha = r.commit("del files");
		let g = Git::open(&r.path()).unwrap();
		let opts = RunOptions::default();

		let bin_file = read_changed_file_for(
			&g,
			&GitSource::Commit(sha.clone()),
			"x.bin",
			ChangeType::Deleted,
			None,
			64,
			&opts,
		)
		.unwrap();
		assert_eq!(bin_file.content.as_deref(), Some(DELETED_FILE_MARKER));

		let text_err = read_changed_file_for(
			&g,
			&GitSource::Commit(sha),
			"x.txt",
			ChangeType::Deleted,
			None,
			64,
			&opts,
		)
		.unwrap_err();
		assert!(
			matches!(text_err, GitError::OutputLimit { .. }),
			"expected OutputLimit, got {text_err:?}"
		);
	}

	// ---- gitCopy.test.ts ----

	#[test]
	fn maps_porcelain_single_letters() {
		assert_eq!(change_type_for_status(b'A'), ChangeType::New);
		assert_eq!(change_type_for_status(b'M'), ChangeType::Modified);
		assert_eq!(change_type_for_status(b'D'), ChangeType::Deleted);
		assert_eq!(change_type_for_status(b'R'), ChangeType::Moved);
		assert_eq!(change_type_for_status(b'C'), ChangeType::New);
		assert_eq!(change_type_for_status(b'U'), ChangeType::New);
		assert_eq!(change_type_for_status(b'T'), ChangeType::Modified);
	}

	#[test]
	fn parses_raw_z_with_rename_and_non_utf8_path() {
		let o = "a".repeat(40);
		let n = "b".repeat(40);
		let mut out =
			format!(":100644 100644 {o} {n} R087\0old name\0new name\0")
				.into_bytes();
		out.extend_from_slice(
			format!(":000000 100644 {} {n} A\0", "0".repeat(40)).as_bytes(),
		);
		out.extend_from_slice(b"bad\xff.txt\0");
		let e = parse_raw_z(&out).unwrap();
		assert_eq!(e.len(), 2);
		assert_eq!(e[0].status, b'R');
		assert_eq!(e[0].old_path.as_deref(), Some(&b"old name"[..]));
		assert_eq!(e[0].path, b"new name");
		assert_eq!(
			(e[0].old_oid.as_str(), e[0].new_oid.as_str()),
			(o.as_str(), n.as_str())
		);
		assert_eq!(e[1].status, b'A');
		assert_eq!(e[1].new_mode, "100644");
		assert_eq!(e[1].path, b"bad\xff.txt");
		assert!(parse_raw_z(b":garbage\0x\0").is_err());
	}

	// ---- real repositories ----

	struct Repo {
		dir: tempfile::TempDir,
		cfg: PathBuf,
	}

	impl Repo {
		fn new() -> Self {
			let dir = tempfile::tempdir().unwrap();
			let cfg = dir.path().join("empty.gitconfig");
			fs::write(&cfg, "").unwrap();
			fs::create_dir(dir.path().join("r")).unwrap();
			let repo = Self { dir, cfg };
			repo.git(&["init", "-q", "-b", "main"]);
			repo.git(&["config", "user.name", "Tester"]);
			repo.git(&["config", "user.email", "t@example.com"]);
			repo.git(&["config", "core.autocrlf", "false"]);
			repo
		}

		fn path(&self) -> PathBuf {
			self.dir.path().join("r")
		}

		fn git(&self, args: &[&str]) -> String {
			let out = Command::new("git")
				.args(args)
				.current_dir(self.path())
				.env("GIT_CONFIG_GLOBAL", &self.cfg)
				.env("GIT_CONFIG_NOSYSTEM", "1")
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		}

		fn write(&self, name: &str, content: &[u8]) {
			let p = self.path().join(name);
			fs::create_dir_all(p.parent().unwrap()).unwrap();
			fs::write(p, content).unwrap();
		}

		fn commit(&self, msg: &str) -> String {
			self.git(&["add", "-A"]);
			self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
			self.git(&["rev-parse", "HEAD"])
		}

		fn collect(&self, source: GitSource) -> GitFiles {
			collect(&Git::open(&self.path()).unwrap(), &source).unwrap()
		}
	}

	fn file(path: &str, content: &str, t: ChangeType) -> PayloadFile {
		PayloadFile {
			path: path.into(),
			content: Some(content.into()),
			change_type: Some(t),
			skipped_reason: None,
		}
	}

	use ChangeType::{Deleted, Modified, Moved, New};

	#[test]
	fn listing_marks_submodule_commits_as_gitlinks() {
		let r = Repo::new();
		r.write("a.txt", b"a\n");
		let base = r.commit("base");
		// A submodule bump is a gitlink entry, never a readable file.
		r.git(&[
			"update-index",
			"--add",
			"--cacheinfo",
			&format!("160000,{base},sub"),
		]);
		r.write("a.txt", b"b\n");
		r.git(&["add", "a.txt"]);
		r.git(&["commit", "-q", "-m", "bump"]);
		let g = Git::open(&r.path()).unwrap();
		let source = GitSource::Commit("HEAD".into());
		let opts = RunOptions::default();
		let (paths, gitlinks) =
			list_changed_paths_and_gitlinks_with(&g, &source, &opts).unwrap();
		assert_eq!(
			paths,
			[
				("a.txt".to_string(), Some(Modified)),
				("sub".to_string(), Some(New))
			]
		);
		assert_eq!(gitlinks, ["sub"]);
		// The plain listing is unchanged.
		assert_eq!(list_changed_paths_with(&g, &source, &opts).unwrap(), paths);
	}

	#[test]
	fn root_commit_is_diffed_against_the_empty_tree() {
		let r = Repo::new();
		r.write("a.txt", b"alpha\n");
		r.write("dir/with space é.txt", b"unicode\n");
		r.write("big5.txt", &[0xa4, 0xe9, 0xa5, 0xbb]);
		let sha = r.commit("init");
		let got = r.collect(GitSource::Commit(sha));
		assert_eq!(
			got.files,
			vec![
				file("a.txt", "alpha\n", New),
				file("dir/with space é.txt", "unicode\n", New)
			]
		);
		assert_eq!(
			got.skipped_unreadable_count, 1,
			"non-UTF-8 is skipped and counted"
		);
	}

	#[test]
	fn commit_modify_delete_and_rename() {
		let r = Repo::new();
		r.write("keep.txt", b"v1\n");
		r.write("gone.txt", b"old body\n");
		r.write("bin.dat", &[0, 1, 2]);
		r.write("old.txt", b"same content for rename detection\n");
		r.commit("base");
		r.write("keep.txt", b"v2\n");
		fs::remove_file(r.path().join("gone.txt")).unwrap();
		fs::remove_file(r.path().join("bin.dat")).unwrap();
		r.git(&["mv", "old.txt", "new.txt"]);
		let sha = r.commit("change");
		let got = r.collect(GitSource::Commit(sha));
		assert_eq!(
			got.files,
			vec![
				file("bin.dat", DELETED_FILE_MARKER, Deleted),
				file("gone.txt", "old body\n", Deleted),
				file("keep.txt", "v2\n", Modified),
				file("new.txt", "same content for rename detection\n", Moved),
			]
		);
		assert_eq!(got.skipped_unreadable_count, 0);
	}

	#[test]
	fn merge_is_the_union_against_every_parent() {
		let r = Repo::new();
		r.write("base.txt", b"base\n");
		r.commit("base");
		for b in ["b1", "b2", "b3"] {
			r.git(&["checkout", "-q", "-b", b, "main"]);
			r.write(&format!("{b}.txt"), format!("{b}\n").as_bytes());
			r.commit(b);
		}
		r.git(&["checkout", "-q", "main"]);
		r.write("t1.txt", b"main\n");
		r.commit("main side");
		r.git(&["merge", "-q", "--no-edit", "b1", "b2", "b3"]);
		let got = r.collect(GitSource::Commit("HEAD".into()));
		let mut paths: Vec<_> =
			got.files.iter().map(|f| f.path.as_str()).collect();
		paths.sort();
		assert_eq!(
			paths,
			["b1.txt", "b2.txt", "b3.txt", "t1.txt"],
			"each path once"
		);
	}

	#[test]
	fn merge_deletion_reads_the_parent_that_still_had_it() {
		let r = Repo::new();
		r.write("base.txt", b"base\n");
		r.commit("base");
		r.git(&["checkout", "-q", "-b", "side"]);
		r.write("only.txt", b"only on side\n");
		r.commit("side");
		r.git(&["checkout", "-q", "main"]);
		r.write("m.txt", b"main\n");
		r.commit("main");
		r.git(&["merge", "-q", "--no-ff", "--no-commit", "side"]);
		r.git(&["rm", "-q", "-f", "only.txt"]);
		r.git(&["commit", "-q", "--no-edit"]);
		let got = r.collect(GitSource::Commit("HEAD".into()));
		// Against P1: m.txt unchanged, only.txt absent. Against P2: m.txt
		// added, only.txt deleted with its body.
		assert_eq!(
			got.files,
			vec![
				file("m.txt", "main\n", New),
				file("only.txt", "only on side\n", Deleted)
			]
		);
	}

	#[test]
	fn single_path_read_does_not_list_the_whole_source() {
		let r = Repo::new();
		r.write("a.txt", b"a\n");
		r.commit("base");
		r.write("a.txt", b"a2\n");
		for i in 0..100 {
			r.write(&format!("untracked-{i:03}.txt"), b"u\n");
		}
		let g = Git::open(&r.path()).unwrap();
		// The full untracked listing alone is far over this cap.
		let opts = RunOptions {
			max_stdout: 512,
			..RunOptions::default()
		};
		let got =
			read_changed_file_with(&g, &GitSource::Working, "a.txt", 64, &opts)
				.unwrap()
				.unwrap();
		assert_eq!(got, file("a.txt", "a2\n", Modified));
		let got = read_changed_file_with(
			&g,
			&GitSource::Working,
			"untracked-042.txt",
			64,
			&opts,
		)
		.unwrap()
		.unwrap();
		assert_eq!(got, file("untracked-042.txt", "u\n", New));
		let none =
			read_changed_file_with(&g, &GitSource::Working, "nope", 64, &opts)
				.unwrap();
		assert_eq!(none, None);
	}

	#[test]
	fn known_change_reads_without_listing() {
		let r = Repo::new();
		r.write("base.txt", b"base\n");
		r.write("old.txt", b"moved body\n");
		r.commit("base");
		r.git(&["checkout", "-q", "-b", "side"]);
		r.write("only.txt", b"only on side\n");
		r.commit("side");
		r.git(&["checkout", "-q", "main"]);
		r.git(&["mv", "old.txt", "new.txt"]);
		r.commit("rename");
		r.git(&["merge", "-q", "--no-ff", "--no-commit", "side"]);
		r.git(&["rm", "-q", "-f", "only.txt"]);
		r.git(&["commit", "-q", "--no-edit"]);
		let g = Git::open(&r.path()).unwrap();
		let merge = g.head().unwrap().unwrap();
		let parents = g.parents(&merge).unwrap();
		let opts = RunOptions::default();
		let source = GitSource::Commit(merge);
		let got = read_changed_file_for(
			&g,
			&source,
			"only.txt",
			Deleted,
			Some(&parents),
			1024,
			&opts,
		)
		.unwrap();
		assert_eq!(got, file("only.txt", "only on side\n", Deleted));
		// Parents looked up when not given.
		let got = read_changed_file_for(
			&g, &source, "only.txt", Deleted, None, 1024, &opts,
		)
		.unwrap();
		assert_eq!(got, file("only.txt", "only on side\n", Deleted));
		let rename = GitSource::Commit("HEAD^1".into());
		let got = read_changed_file_for(
			&g, &rename, "new.txt", Moved, None, 1024, &opts,
		)
		.unwrap();
		assert_eq!(got, file("new.txt", "moved body\n", Moved));
		// The listing agrees on the label.
		let listed =
			list_changed_paths_with(&g, &rename, &RunOptions::default())
				.unwrap();
		assert_eq!(listed, [("new.txt".to_string(), Some(Moved))]);
		// Staged content is the index entry, not the disk.
		r.write("new.txt", b"staged\n");
		r.git(&["add", "new.txt"]);
		r.write("new.txt", b"disk\n");
		let got = read_changed_file_for(
			&g,
			&GitSource::Staged,
			"new.txt",
			Modified,
			None,
			1024,
			&opts,
		)
		.unwrap();
		assert_eq!(got, file("new.txt", "staged\n", Modified));
		// Staged and working deletions read HEAD.
		r.git(&["rm", "-q", "base.txt"]);
		for source in [GitSource::Staged, GitSource::Working] {
			let got = read_changed_file_for(
				&g, &source, "base.txt", Deleted, None, 1024, &opts,
			)
			.unwrap();
			assert_eq!(got, file("base.txt", "base\n", Deleted));
		}
	}

	#[test]
	fn range_compares_endpoints_only() {
		let r = Repo::new();
		r.write("a.txt", b"a1\n");
		let a = r.commit("a");
		r.write("tmp.txt", b"tmp\n");
		r.commit("add tmp");
		fs::remove_file(r.path().join("tmp.txt")).unwrap();
		r.write("a.txt", b"a2\n");
		let b = r.commit("b");
		let got = r.collect(GitSource::Range(a, b));
		assert_eq!(got.files, vec![file("a.txt", "a2\n", Modified)]);
	}

	#[test]
	fn staged_reads_index_bytes_plus_deleted_and_renamed() {
		let r = Repo::new();
		r.write("same.txt", b"head\n");
		r.write("gone.txt", b"head:deleted\n");
		r.write("old.txt", b"a renamed file body that stays the same\n");
		r.commit("base");
		r.write("same.txt", b"index\n");
		r.git(&["add", "same.txt"]);
		r.write("same.txt", b"working\n");
		r.git(&["rm", "-q", "gone.txt"]);
		r.git(&["mv", "old.txt", "new.txt"]);
		r.write("unstaged.txt", b"not added\n");
		let got = r.collect(GitSource::Staged);
		assert_eq!(
			got.files,
			vec![
				file("gone.txt", "head:deleted\n", Deleted),
				file(
					"new.txt",
					"a renamed file body that stays the same\n",
					Moved
				),
				file("same.txt", "index\n", Modified),
			]
		);
	}

	#[test]
	fn working_unions_worktree_untracked_and_index() {
		let r = Repo::new();
		r.write("mod.txt", b"head\n");
		r.write("gone.txt", b"head:gone\n");
		r.write("staged.txt", b"head\n");
		r.commit("base");
		r.write("mod.txt", b"disk\n");
		fs::remove_file(r.path().join("gone.txt")).unwrap();
		r.write("staged.txt", b"staged on disk\n");
		r.git(&["add", "staged.txt"]);
		r.write("新 檔.txt", b"untracked\n");
		r.write("bin.dat", &[0xff, 0xfe, 0x00]);
		let got = r.collect(GitSource::Working);
		assert_eq!(
			got.files,
			vec![
				file("gone.txt", "head:gone\n", Deleted),
				file("mod.txt", "disk\n", Modified),
				file("新 檔.txt", "untracked\n", New),
				file("staged.txt", "staged on disk\n", Modified),
			]
		);
		assert_eq!(got.skipped_unreadable_count, 1);
	}

	#[test]
	fn shallow_boundary_commit_is_refused() {
		let r = Repo::new();
		r.write("a.txt", b"1\n");
		r.commit("one");
		r.write("a.txt", b"2\n");
		r.commit("two");
		let url = format!("file://{}", r.path().display());
		let dst = r.dir.path().join("shallow");
		r.git(&["clone", "-q", "--depth", "1", &url, dst.to_str().unwrap()]);
		let git = Git::open(&dst).unwrap();
		let err = collect(&git, &GitSource::Commit("HEAD".into())).unwrap_err();
		assert!(matches!(err, GitError::Shallow(_)), "{err}");
		assert!(err.to_string().contains("shallow"));
	}

	#[test]
	fn missing_repo_dir_is_not_reported_as_missing_git() {
		let dir = tempfile::tempdir().unwrap();
		let err = Git::open(&dir.path().join("nope")).unwrap_err();
		assert!(!matches!(err, GitError::NotFound), "{err}");
	}

	#[test]
	fn option_like_revisions_are_refused() {
		let r = Repo::new();
		r.commit("init");
		let git = Git::open(&r.path()).unwrap();
		let err =
			collect(&git, &GitSource::Commit("--output=x".into())).unwrap_err();
		assert!(matches!(err, GitError::InvalidRevision(_)));
	}

	#[test]
	fn working_deletion_reads_head_even_with_a_file_named_head() {
		let r = Repo::new();
		r.write("HEAD", b"a file named HEAD\n");
		r.write("d.txt", b"gone\n");
		r.commit("base");
		fs::remove_file(r.path().join("d.txt")).unwrap();
		let got = r.collect(GitSource::Working);
		assert_eq!(got.files, vec![file("d.txt", "gone\n", Deleted)]);
	}

	/// Conflicts: f.txt both modified (UU), del.txt deleted by them (UD).
	fn conflicted_repo() -> Repo {
		let r = Repo::new();
		r.write("f.txt", b"base\n");
		r.write("del.txt", b"base del\n");
		r.commit("base");
		r.git(&["checkout", "-q", "-b", "side"]);
		r.write("f.txt", b"side\n");
		fs::remove_file(r.path().join("del.txt")).unwrap();
		r.commit("side");
		r.git(&["checkout", "-q", "main"]);
		r.write("f.txt", b"main\n");
		r.write("del.txt", b"main del\n");
		r.commit("main");
		let out = Command::new("git")
			.args(["merge", "-q", "side"])
			.current_dir(r.path())
			.env("GIT_CONFIG_GLOBAL", &r.cfg)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.output()
			.unwrap();
		assert!(!out.status.success(), "merge must conflict");
		r
	}

	#[test]
	fn working_labels_conflicts_like_scm_merge_changes() {
		let r = conflicted_repo();
		let got = r.collect(GitSource::Working);
		let disk = fs::read_to_string(r.path().join("f.txt")).unwrap();
		assert_eq!(
			got.files,
			vec![
				file("del.txt", "main del\n", Deleted),
				file("f.txt", &disk, Modified),
			]
		);
	}

	#[test]
	fn staged_leaves_conflicts_out() {
		let r = conflicted_repo();
		assert_eq!(r.collect(GitSource::Staged), GitFiles::default());
	}

	// ---- collect_payload: stagedGitCopy.test.ts / graphCopy.test.ts ----

	use crate::settings::{FilterAction, FilterRule, FilterType};

	fn payload(r: &Repo, source: GitSource, settings: &Settings) -> CopyResult {
		let git = Git::open(&r.path()).unwrap();
		collect_payload(&git, &source, &[git.root()], settings).unwrap()
	}

	fn no_limit() -> Settings {
		Settings {
			set_max_file_count: false,
			..Settings::default()
		}
	}

	#[test]
	fn payload_staged_preserves_index_bytes_plus_deleted_and_renamed() {
		let r = Repo::new();
		r.write("same.ts", b"head\n");
		r.write("gone.ts", b"head:deleted\n");
		r.write("old.ts", b"a renamed file body that stays the same\n");
		r.commit("base");
		r.write("same.ts", b"index:same\n");
		r.git(&["add", "same.ts"]);
		r.write("same.ts", b"working\n");
		r.git(&["rm", "-q", "gone.ts"]);
		r.git(&["mv", "old.ts", "new.ts"]);
		let got = payload(&r, GitSource::Staged, &no_limit());
		assert_eq!(
			got.files,
			vec![
				file("gone.ts", "head:deleted\n", Deleted),
				file(
					"new.ts",
					"a renamed file body that stays the same\n",
					Moved
				),
				file("same.ts", "index:same\n", Modified),
			]
		);
		assert_eq!(got.copied_file_count, 3);
		assert!(!got.payload.contains("working"));
	}

	fn broken_index_repo() -> Repo {
		let r = Repo::new();
		r.write("broken.ts", b"head\n");
		r.write("real.ts", b"head\n");
		r.commit("base");
		r.write("broken.ts", &[0xa4, 0xe9, 0xa5, 0xbb]); // Big5
		r.write("real.ts", b"index:real\n");
		r.git(&["add", "-A"]);
		r.write("broken.ts", b"working\n");
		r
	}

	#[test]
	fn payload_unreadable_index_entry_is_the_marker_not_the_working_tree() {
		let r = broken_index_repo();
		let got = payload(&r, GitSource::Staged, &no_limit());
		assert_eq!(
			got.files,
			vec![
				file("broken.ts", UNREADABLE_FILE_MARKER, Modified),
				file("real.ts", "index:real\n", Modified),
			]
		);
		assert_eq!(got.copied_file_count, 1);
		assert_eq!(got.skipped_unreadable_count, 1);
	}

	#[test]
	fn payload_unreadable_placeholder_does_not_consume_the_limit() {
		let r = broken_index_repo();
		let settings = Settings {
			set_max_file_count: true,
			file_count_limit: 1.0,
			..Settings::default()
		};
		let got = payload(&r, GitSource::Staged, &settings);
		assert_eq!(
			got.files,
			vec![
				file("broken.ts", UNREADABLE_FILE_MARKER, Modified),
				file("real.ts", "index:real\n", Modified),
			]
		);
		assert_eq!(got.copied_file_count, 1);
		assert_eq!(got.skipped_unreadable_count, 1);
		assert!(!got.file_limit_reached);
	}

	#[test]
	fn payload_limit_trips_only_on_a_dropped_candidate() {
		let r = Repo::new();
		r.write("a.txt", b"a\n");
		r.write("b.txt", b"b\n");
		let sha = r.commit("init");
		let limit = |n: f64| Settings {
			set_max_file_count: true,
			file_count_limit: n,
			..Settings::default()
		};
		let got = payload(&r, GitSource::Commit(sha.clone()), &limit(1.0));
		assert_eq!(got.files, vec![file("a.txt", "a\n", New)]);
		assert!(got.file_limit_reached);
		// The limit exactly fits: nothing was dropped.
		let got = payload(&r, GitSource::Commit(sha), &limit(2.0));
		assert_eq!(got.copied_file_count, 2);
		assert!(!got.file_limit_reached);
	}

	#[test]
	fn payload_oversize_file_is_skipped_with_reason() {
		let r = Repo::new();
		r.write("b.ts", "x".repeat(2 * 1024).as_bytes());
		let sha = r.commit("init");
		let settings = Settings {
			max_file_size_kb: 1.0,
			..Settings::default()
		};
		let got = payload(&r, GitSource::Commit(sha), &settings);
		assert_eq!(got.skipped_file_size_count, 1);
		assert_eq!(got.copied_file_count, 0);
		assert_eq!(
			got.files[0].skipped_reason.as_deref(),
			Some("size exceeds limit (2048 bytes)")
		);
		assert_eq!(got.files[0].change_type, Some(New));
		assert!(got.payload.contains("File skipped: size exceeds limit"));
	}

	#[test]
	fn payload_honours_the_exclude_filters() {
		let r = Repo::new();
		r.write("secrets.env", b"SECRET\n");
		r.write("src/a.ts", b"ok\n");
		let sha = r.commit("init");
		let settings = Settings {
			use_filters: true,
			use_exclude_filters: true,
			filter_rules: vec![FilterRule {
				kind: FilterType::Path,
				action: FilterAction::Exclude,
				value: "secrets.env".into(),
				enabled: true,
			}],
			..Settings::default()
		};
		let got = payload(&r, GitSource::Commit(sha), &settings);
		assert!(!got.payload.contains("SECRET"));
		assert!(got.payload.contains("src/a.ts"));
		assert_eq!(got.copied_file_count, 1);
	}

	#[test]
	fn payload_counts_unreadable_only_after_filtering_on_the_graph() {
		let r = Repo::new();
		r.write("bin.dat", &[0xff, 0xfe, 0x00]);
		r.write("a.ts", b"ok\n");
		let sha = r.commit("init");
		let exclude = Settings {
			use_filters: true,
			use_exclude_filters: true,
			filter_rules: vec![FilterRule {
				kind: FilterType::Path,
				action: FilterAction::Exclude,
				value: "bin.dat".into(),
				enabled: true,
			}],
			..Settings::default()
		};
		let got = payload(&r, GitSource::Commit(sha.clone()), &exclude);
		assert_eq!(
			(got.skipped_unreadable_count, got.copied_file_count),
			(0, 1)
		);
		let got = payload(&r, GitSource::Commit(sha), &Settings::default());
		assert_eq!(
			(got.skipped_unreadable_count, got.copied_file_count),
			(1, 1)
		);
	}

	// macOS temp dirs live under the `/var` -> `/private/var` symlink, and
	// Windows can spell paths with `..` or 8.3 short names; git reports the
	// resolved toplevel. A root spelled differently from git's own spelling
	// must still relativize (the bug type fixed in 224971b).
	#[test]
	fn payload_relativizes_against_a_root_spelled_through_a_symlink() {
		let r = Repo::new();
		r.write("sub/a.txt", b"one");
		r.commit("init");
		r.write("sub/a.txt", b"two");
		#[cfg(unix)]
		let outer = tempfile::tempdir().unwrap();
		#[cfg(unix)]
		let spelled = {
			let link = outer.path().join("link");
			std::os::unix::fs::symlink(r.path(), &link).unwrap();
			link
		};
		#[cfg(not(unix))]
		let spelled = r.path().join("sub").join("..");
		let git = Git::open(&spelled.join("sub")).unwrap();
		let got = collect_payload(
			&git,
			&GitSource::Working,
			&[spelled.join("sub")],
			&Settings::default(),
		)
		.unwrap();
		assert!(
			got.payload.contains("// file: [MODIFIED] a.txt"),
			"{}",
			got.payload
		);
	}

	#[test]
	fn payload_graph_deleted_file_skips_the_size_limit() {
		let r = Repo::new();
		r.write("big.txt", "x".repeat(4096).as_bytes());
		r.commit("init");
		r.git(&["rm", "-q", "big.txt"]);
		let sha = r.commit("drop");
		let tiny = Settings {
			max_file_size_kb: 1.0,
			..Settings::default()
		};
		let got = payload(&r, GitSource::Commit(sha), &tiny);
		assert_eq!(
			(got.copied_file_count, got.skipped_file_size_count),
			(1, 0)
		);
	}

	#[test]
	fn payload_graph_empty_commit_has_no_root_line() {
		let r = Repo::new();
		r.write("a.ts", b"a\n");
		r.commit("init");
		r.git(&["commit", "-q", "--allow-empty", "-m", "empty"]);
		let sha = r.git(&["rev-parse", "HEAD"]).trim().to_string();
		let got = payload(&r, GitSource::Commit(sha), &Settings::default());
		assert_eq!(got.payload, "");
	}

	#[test]
	fn payload_working_unreadable_is_skipped_uncounted_like_scm() {
		let r = Repo::new();
		r.write("a.ts", b"ok\n");
		r.commit("init");
		r.write("bin.dat", &[0xff, 0xfe, 0x00]);
		let got = payload(&r, GitSource::Working, &Settings::default());
		assert_eq!(got.files, vec![]);
		assert_eq!(got.skipped_unreadable_count, 0);
	}

	#[test]
	fn payload_paths_are_workspace_relative_with_a_single_root_line() {
		let r = Repo::new();
		r.write("src/a.ts", b"a\n");
		r.write("top.ts", b"top\n");
		let sha = r.commit("init");
		let git = Git::open(&r.path()).unwrap();
		let src = git.root().join("src");
		r.write("src/a.ts", b"a2\n");
		r.git(&["add", "src/a.ts"]);
		let s = Settings::default();
		let got =
			collect_payload(&git, &GitSource::Staged, &[&src], &s).unwrap();
		assert_eq!(got.files[0].path, "a.ts");
		assert!(got.payload.starts_with("// clipcode-root: src\n"));
		r.git(&["reset", "-q", "--hard"]);
		// The graph surface stays repo-relative and names the repository,
		// while filters still see the workspace-relative path.
		let parent = git.root().parent().unwrap().to_path_buf();
		let repo = git.root().file_name().unwrap().to_string_lossy();
		let exclude = Settings {
			use_filters: true,
			use_exclude_filters: true,
			filter_rules: vec![FilterRule {
				kind: FilterType::Path,
				action: FilterAction::Exclude,
				value: format!("{repo}/top.ts"),
				enabled: true,
			}],
			..Settings::default()
		};
		let commit = GitSource::Commit(sha);
		let got = collect_payload(&git, &commit, &[&parent], &exclude).unwrap();
		assert_eq!(got.files, vec![file("src/a.ts", "a\n", New)]);
		assert!(got
			.payload
			.starts_with(&format!("// clipcode-root: {repo}\n")));
		// A deletion still carries its pre-deletion body.
		fs::remove_file(r.path().join("top.ts")).unwrap();
		let del = r.commit("rm");
		let got = payload(&r, GitSource::Commit(del), &Settings::default());
		assert_eq!(got.files, vec![file("top.ts", "top\n", Deleted)]);
		assert!(got.payload.contains("[DELETED] top.ts"));
	}

	#[test]
	fn desktop_selection_stays_inside_monorepo_folder() {
		let r = Repo::new();
		r.write("packages/api/a.ts", b"old\n");
		r.write("packages/api/b.ts", b"old\n");
		r.write("packages/web/a.ts", b"old\n");
		r.commit("base");
		r.write("packages/api/a.ts", b"new\n");
		r.write("packages/api/b.ts", b"new\n");
		r.write("packages/web/a.ts", b"new\n");
		let scope = r.path().join("packages/api");
		let git = Git::open(&scope).unwrap();
		let selected = HashSet::from([
			"packages/api/b.ts".to_string(),
			"packages/web/a.ts".to_string(),
		]);
		let result = collect_payload_with_selection(
			&git,
			&GitSource::Working,
			&[&scope],
			&Settings::default(),
			Some(&scope),
			Some(&selected),
		)
		.unwrap();
		assert_eq!(
			result
				.files
				.iter()
				.map(|f| f.path.as_str())
				.collect::<Vec<_>>(),
			["b.ts"]
		);
		assert!(result.payload.contains("b.ts"));
		assert!(!result.payload.contains("web"));
	}

	fn options(
		files: &[PayloadFile],
		roots: &[PathBuf],
	) -> BuildPayloadOptions {
		let s = Settings::default();
		BuildPayloadOptions {
			header_format: s.header_format,
			pre_text: s.pre_text,
			post_text: s.post_text,
			add_extra_line_between_files: s.add_extra_line_between_files,
			files: files.to_vec(),
			source_root: source_root_name(roots),
		}
	}

	#[test]
	fn payload_builder_follows_the_ts_fallback_rule() {
		let r = Repo::new();
		r.write("keep.txt", b"v1\n");
		r.write("gone.txt", b"gone\n");
		r.commit("base");
		let git = Git::open(&r.path()).unwrap();
		let roots = [git.root().to_path_buf()];
		let s = Settings::default();

		r.write("keep.txt", b"v2\n");
		let got =
			collect_payload(&git, &GitSource::Working, &roots, &s).unwrap();
		let regular = build_payload(&options(&got.files, &roots));
		assert_ne!(regular, build_git_payload(&options(&got.files, &roots)));
		assert_eq!(got.payload, regular, "working without deletions");

		fs::remove_file(r.path().join("gone.txt")).unwrap();
		let got =
			collect_payload(&git, &GitSource::Working, &roots, &s).unwrap();
		let opts = options(&got.files, &roots);
		assert_eq!(got.payload, build_git_payload(&opts), "working deletion");

		r.git(&["add", "keep.txt"]);
		let got =
			collect_payload(&git, &GitSource::Staged, &roots, &s).unwrap();
		let opts = options(&got.files, &roots);
		assert_eq!(got.payload, build_git_payload(&opts), "index content");

		let head = GitSource::Commit("HEAD".into());
		let got = collect_payload(&git, &head, &roots, &s).unwrap();
		let opts = options(&got.files, &roots);
		assert_eq!(got.payload, build_git_payload(&opts), "graph surface");
	}

	#[test]
	fn payload_without_workspace_roots_labels_against_the_repo() {
		let r = Repo::new();
		r.write("a.txt", b"a\n");
		let sha = r.commit("init");
		let git = Git::open(&r.path()).unwrap();
		let none: [&Path; 0] = [];
		let got = collect_payload(
			&git,
			&GitSource::Commit(sha),
			&none,
			&Settings::default(),
		)
		.unwrap();
		assert_eq!(got.files, vec![file("a.txt", "a\n", New)]);
		assert!(got.payload.starts_with("// clipcode-root: r\n"));
	}

	#[test]
	fn hardened_command_has_the_flags_and_env() {
		use std::ffi::OsStr;
		let dummy_root = PathBuf::from("/dummy/repo");
		let dummy_boundary = PathBuf::from("/dummy/repo/sub");
		let parent_of_boundary = dummy_boundary.parent().unwrap().to_path_buf();

		let hardened_git = Git {
			root: dummy_root.clone(),
			boundary: Some(dummy_boundary),
		};
		let cmd = hardened_git.command();
		let args: Vec<_> = cmd.get_args().collect();
		assert!(args.len() >= 4);
		assert_eq!(
			&args[..4],
			&[
				OsStr::new("-c"),
				OsStr::new("core.fsmonitor="),
				OsStr::new("-c"),
				OsStr::new("protocol.allow=never"),
			]
		);
		let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> =
			cmd.get_envs().collect();
		assert_eq!(
			envs.get(OsStr::new("GIT_CEILING_DIRECTORIES")),
			Some(&Some(parent_of_boundary.as_os_str()))
		);
		assert_eq!(
			envs.get(OsStr::new("GIT_OPTIONAL_LOCKS")),
			Some(&Some(OsStr::new("0")))
		);
		assert_eq!(
			envs.get(OsStr::new("GIT_NO_LAZY_FETCH")),
			Some(&Some(OsStr::new("1")))
		);
		for var in [
			"GIT_DIR",
			"GIT_WORK_TREE",
			"GIT_INDEX_FILE",
			"GIT_COMMON_DIR",
			"GIT_OBJECT_DIRECTORY",
			"GIT_ALTERNATE_OBJECT_DIRECTORIES",
			"GIT_NAMESPACE",
			"GIT_EXTERNAL_DIFF",
			"GIT_CONFIG_PARAMETERS",
			"GIT_CONFIG_COUNT",
		] {
			assert_eq!(
				envs.get(OsStr::new(var)),
				Some(&None),
				"expected {var} to be removed in env"
			);
		}

		let plain_git = Git {
			root: dummy_root,
			boundary: None,
		};
		let plain_cmd = plain_git.command();
		let plain_args: Vec<_> = plain_cmd.get_args().collect();
		assert_eq!(plain_args.len(), 0);
		let plain_envs: std::collections::HashMap<&OsStr, Option<&OsStr>> =
			plain_cmd.get_envs().collect();
		assert_eq!(plain_envs.get(OsStr::new("GIT_CEILING_DIRECTORIES")), None);
		assert_eq!(plain_envs.get(OsStr::new("GIT_OPTIONAL_LOCKS")), None);
		assert_eq!(plain_envs.get(OsStr::new("GIT_NO_LAZY_FETCH")), None);
		for var in [
			"GIT_DIR",
			"GIT_WORK_TREE",
			"GIT_INDEX_FILE",
			"GIT_COMMON_DIR",
			"GIT_OBJECT_DIRECTORY",
			"GIT_ALTERNATE_OBJECT_DIRECTORIES",
			"GIT_NAMESPACE",
			"GIT_EXTERNAL_DIFF",
			"GIT_CONFIG_PARAMETERS",
			"GIT_CONFIG_COUNT",
		] {
			assert_eq!(plain_envs.get(OsStr::new(var)), None);
		}
	}

	#[test]
	fn temp_index_file_preserves_source_mtime() {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("index");
		std::fs::write(&src, b"index data").unwrap();
		let target_time = std::time::SystemTime::UNIX_EPOCH
			+ std::time::Duration::from_secs(1_700_000_000);
		let file = std::fs::File::options().write(true).open(&src).unwrap();
		file.set_modified(target_time).unwrap();
		drop(file);

		let temp = TempIndexFile::create_from(&src).unwrap();
		let temp_meta = std::fs::metadata(temp.path()).unwrap();
		assert_eq!(
			temp_meta.modified().unwrap(),
			target_time,
			"temp copy must preserve source index mtime for racy-clean check"
		);
	}

	#[test]
	fn temp_index_file_refuses_oversized_file() {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("index");
		std::fs::write(&src, b"01234567890123456789").unwrap();
		let res = TempIndexFile::create_from_with_cap(&src, 10);
		assert!(
			matches!(res, Err(GitError::OutputLimit { limit: 10, .. })),
			"expected OutputLimit for index exceeding cap, got {res:?}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn temp_index_file_refuses_fifo_promptly() {
		let dir = tempfile::tempdir().unwrap();
		let fifo = dir.path().join("fifo_index");
		let status = std::process::Command::new("mkfifo").arg(&fifo).status();
		let Ok(status) = status else {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"mkfifo command unavailable"
			);
			return;
		};
		assert!(status.success(), "mkfifo failed");
		let res = TempIndexFile::create_from(&fifo);
		assert!(
			matches!(res, Err(GitError::OutsideBoundary { what: "index" })),
			"expected OutsideBoundary for FIFO index, got {res:?}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn temp_index_file_in_private_dir_cleaned_up_on_drop() {
		use std::os::unix::fs::PermissionsExt;

		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("index");
		std::fs::write(&src, b"dummy index content").unwrap();

		let temp_index = TempIndexFile::create_from(&src).expect(
			"TempIndexFile creation must succeed for valid regular file",
		);
		let index_path = temp_index.path().to_path_buf();
		let parent_dir = index_path
			.parent()
			.expect("temp index must have a parent directory")
			.to_path_buf();

		// Parent dir must not be the shared temp dir itself
		assert_ne!(
			parent_dir,
			std::env::temp_dir(),
			"temp index parent directory must be a private subdirectory, not the shared temp dir"
		);

		// Parent dir must have mode 0700
		let mode = parent_dir
			.metadata()
			.expect("parent directory metadata")
			.permissions()
			.mode() & 0o777;
		assert_eq!(
			mode, 0o700,
			"expected private directory mode 0700, got 0{:o}",
			mode
		);

		// A stray "<index>.lock" file next to it is removed when TempIndexFile drops
		let lock_file = parent_dir.join("index.lock");
		std::fs::write(&lock_file, b"lock content").unwrap();
		assert!(lock_file.exists());

		drop(temp_index);
		assert!(
			!parent_dir.exists(),
			"private directory should be removed on drop, but still exists"
		);
		assert!(
			!lock_file.exists(),
			"stray lock file inside private directory should be removed on drop"
		);
	}

	#[cfg(unix)]
	#[test]
	fn open_index_source_fifo_race_prompt_failure() {
		let dir = tempfile::tempdir().unwrap();
		let fifo = dir.path().join("fifo_race");
		let status = std::process::Command::new("mkfifo").arg(&fifo).status();
		let Ok(status) = status else {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"mkfifo command unavailable"
			);
			return;
		};
		assert!(status.success(), "mkfifo failed");

		let (tx, rx) = std::sync::mpsc::channel();
		let fifo_clone = fifo.clone();
		let _handle = std::thread::spawn(move || {
			let res = open_index_source(&fifo_clone);
			let _ = tx.send(res);
		});

		let res = rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
			"open_index_source on FIFO must return promptly without blocking",
		);
		assert!(
			matches!(res, Err(GitError::OutsideBoundary { what: "index" })),
			"expected OutsideBoundary for FIFO index, got {res:?}"
		);
	}

	#[test]
	fn diff_reads_index_classification() {
		// Plain working tree diff reads index
		assert!(Git::diff_reads_index(&[
			"diff",
			"-z",
			"--raw",
			"--no-abbrev",
			"-M"
		]));
		// Diff with HEAD (1 tree-ish vs worktree) reads index
		assert!(Git::diff_reads_index(&["diff", "HEAD", "--", "f.txt"]));
		// Diff cached reads index
		assert!(Git::diff_reads_index(&[
			"diff", "--cached", "HEAD", "--", "f.txt"
		]));
		assert!(Git::diff_reads_index(&["diff", "--cached"]));
		// Two-tree diffs do not read index
		assert!(!Git::diff_reads_index(&[
			"diff", "sha1", "sha2", "--", "f.txt"
		]));
		assert!(!Git::diff_reads_index(&["diff", "sha1..sha2"]));
		assert!(!Git::diff_reads_index(&["diff", "sha1...sha2"]));
	}
}
