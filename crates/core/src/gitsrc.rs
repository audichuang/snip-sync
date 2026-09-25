//! Git source: read changed files and diffs from a repository.
//!
//! Replaces the VS Code git API layer of ClipCodeVSCode (`gitCopy.ts`,
//! `gitHistory.ts`, `graphCopy.ts`, `catFile.ts`, commit 0aa24c8) with direct
//! git plumbing calls (porting-notes section 5). `collect` only yields the
//! files; `collect_payload` applies filters, limits and counts on top.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::copy::CopyResult;
use crate::filter::file_matches_filters;
use crate::format::{
	build_git_payload, build_payload, BuildPayloadOptions, ChangeType,
	PayloadFile,
};
use crate::fsutil::{decode_utf8_or_skip, read_text_file};
use crate::paths::{source_root_name, to_clipboard_path_from_roots};
use crate::settings::Settings;

/// The well-known OID of git's empty tree: the "parent" of a root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Content of a deleted file whose pre-deletion content no parent can supply.
pub const DELETED_FILE_MARKER: &str =
	"// This file has been deleted in this change";
/// Stands in for content the requested revision could not supply.
pub const UNREADABLE_FILE_MARKER: &str = "// Unable to read file content";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
	#[error(
		"git executable not found: install git and make sure it is on PATH"
	)]
	NotFound,
	#[error("git {args} failed: {stderr}")]
	Failed { args: String, stderr: String },
	#[error(
		"{} is not inside a git repository (commit mode and git sources need one)",
		.0.display()
	)]
	NotARepository(PathBuf),
	#[error("invalid revision: {0}")]
	InvalidRevision(String),
	#[error(
		"Repository history is shallow, so the parent of {0} is not available locally. Run 'git fetch --unshallow' and try again."
	)]
	Shallow(String),
	#[error("unexpected git output: {0}")]
	Malformed(String),
	#[error(transparent)]
	Io(#[from] io::Error),
}

/// Runs the system git CLI inside one repository, never through a shell.
#[derive(Debug, Clone)]
pub struct Git {
	root: PathBuf,
}

fn git_command() -> Command {
	let mut cmd = Command::new("git");
	// Byte-stable output, and never block on a credential prompt.
	cmd.env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		// CREATE_NO_WINDOW: a GUI process must not flash a console.
		cmd.creation_flags(0x0800_0000);
	}
	cmd
}

fn spawn_error(e: io::Error) -> GitError {
	if e.kind() == io::ErrorKind::NotFound {
		GitError::NotFound
	} else {
		GitError::Io(e)
	}
}

impl Git {
	/// Checks that git is installed, then resolves the repository top level
	/// containing `dir`.
	pub fn open(dir: &Path) -> Result<Self, GitError> {
		git_command()
			.arg("--version")
			.stdin(Stdio::null())
			.output()
			.map_err(spawn_error)?;
		let probe = Self {
			root: dir.to_path_buf(),
		};
		let out = probe.run(&["rev-parse", "--show-toplevel"]).map_err(
			|e| match e {
				GitError::Failed { ref stderr, .. }
					if stderr.contains("not a git repository") =>
				{
					GitError::NotARepository(dir.to_path_buf())
				}
				e => e,
			},
		)?;
		let top = String::from_utf8(out).map_err(|_| {
			GitError::Malformed("repository path is not UTF-8".into())
		})?;
		Ok(Self {
			root: PathBuf::from(top.trim_end_matches(['\n', '\r'])),
		})
	}

	/// The repository top level; every git path is relative to it.
	pub fn root(&self) -> &Path {
		&self.root
	}

	pub fn command(&self) -> Command {
		let mut cmd = git_command();
		cmd.current_dir(&self.root);
		cmd
	}

	/// Runs `git <args>` and returns stdout; a non-zero exit is an error.
	pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
		let out = self.command().args(args).stdin(Stdio::null()).output()?;
		if !out.status.success() {
			return Err(GitError::Failed {
				args: args.join(" "),
				stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
			});
		}
		Ok(out.stdout)
	}

	/// Resolves `rev` to a full commit OID. A leading `-` is refused so a
	/// revision can never be read as an option by a later git call.
	pub fn resolve_commit(&self, rev: &str) -> Result<String, GitError> {
		if rev.is_empty() || rev.starts_with('-') {
			return Err(GitError::InvalidRevision(rev.to_string()));
		}
		let spec = format!("{rev}^{{commit}}");
		let out = self
			.run(&["rev-parse", "--verify", "--quiet", &spec])
			.map_err(|_| GitError::InvalidRevision(rev.to_string()))?;
		Ok(String::from_utf8_lossy(&out).trim().to_string())
	}

	/// The parents of `sha`, in order (empty for a root or grafted commit).
	pub fn parents(&self, sha: &str) -> Result<Vec<String>, GitError> {
		let out = self.run(&["rev-list", "--parents", "-n", "1", sha])?;
		Ok(String::from_utf8_lossy(&out)
			.split_ascii_whitespace()
			.skip(1)
			.map(str::to_string)
			.collect())
	}

	pub fn is_shallow(&self) -> Result<bool, GitError> {
		let out = self.run(&["rev-parse", "--is-shallow-repository"])?;
		Ok(out.trim_ascii() == b"true")
	}

	/// Starts a long-lived `git cat-file --batch`.
	pub fn cat_file(&self) -> Result<CatFile, GitError> {
		let mut child = self
			.command()
			.args(["cat-file", "--batch"])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.spawn()?;
		let stdin = child.stdin.take();
		let stdout = child.stdout.take().map(BufReader::new);
		match (stdin, stdout) {
			(Some(stdin), Some(stdout)) => Ok(CatFile {
				child,
				stdin: Some(stdin),
				stdout,
			}),
			_ => Err(GitError::Malformed("cat-file pipes unavailable".into())),
		}
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

/// Parses `--raw -z` output at the byte level (paths may not be UTF-8):
/// `:<om> <nm> <ooid> <noid> <S>[score]\0<path>\0`, with two paths for
/// renames and copies.
pub fn parse_raw_z(out: &[u8]) -> Result<Vec<RawEntry>, GitError> {
	let bad = || GitError::Malformed("raw diff record".into());
	let mut fields = out.split(|&b| b == 0);
	let mut entries = Vec::new();
	while let Some(header) = fields.next() {
		if header.is_empty() {
			continue;
		}
		let header =
			std::str::from_utf8(header.strip_prefix(b":").ok_or_else(bad)?)
				.map_err(|_| bad())?;
		let parts: Vec<&str> = header.split(' ').collect();
		let [old_mode, new_mode, old_oid, new_oid, status] = parts[..] else {
			return Err(bad());
		};
		let status = *status.as_bytes().first().ok_or_else(bad)?;
		let first = fields.next().ok_or_else(bad)?.to_vec();
		let (old_path, path) = if matches!(status, b'R' | b'C') {
			(Some(first), fields.next().ok_or_else(bad)?.to_vec())
		} else {
			(None, first)
		};
		entries.push(RawEntry {
			status,
			old_mode: old_mode.to_string(),
			new_mode: new_mode.to_string(),
			old_oid: old_oid.to_string(),
			new_oid: new_oid.to_string(),
			old_path,
			path,
		});
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

/// A long-lived `git cat-file --batch`. Requests go one at a time (write,
/// flush, read the answer), so neither pipe can fill up and deadlock.
pub struct CatFile {
	child: Child,
	stdin: Option<ChildStdin>,
	stdout: BufReader<ChildStdout>,
}

impl CatFile {
	/// Reads an object (`<oid>` or `<rev>:<path>`). `None` = missing.
	pub fn read(&mut self, object: &str) -> io::Result<Option<Vec<u8>>> {
		if object.contains(['\n', '\r']) {
			// The protocol is line based; such a request would desync it.
			return Ok(None);
		}
		let stdin = self.stdin.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
		stdin.write_all(format!("{object}\n").as_bytes())?;
		stdin.flush()?;
		read_batch_response(&mut self.stdout)
	}
}

impl Drop for CatFile {
	fn drop(&mut self) {
		// Closing stdin ends the batch; then reap the process.
		self.stdin.take();
		let _ = self.child.wait();
	}
}

/// Reads one `git cat-file --batch` response: `<oid> <type> <size>\n`
/// followed by exactly `size` bytes and a LF, or `<object> missing\n`.
/// `None` means the object is missing (or ambiguous).
pub fn read_batch_response<R: BufRead>(
	reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
	let mut header = Vec::new();
	reader.read_until(b'\n', &mut header)?;
	if header.pop() != Some(b'\n') {
		return Err(io::ErrorKind::UnexpectedEof.into());
	}
	let header = String::from_utf8_lossy(&header);
	// "<oid> <type> <size>": size is the last field. `missing` and
	// `ambiguous` responses carry no body.
	let Ok(size) = header.rsplit(' ').next().unwrap_or("").parse::<usize>()
	else {
		return Ok(None);
	};
	let mut body = vec![0; size];
	reader.read_exact(&mut body)?;
	let mut lf = [0u8; 1];
	reader.read_exact(&mut lf)?;
	Ok(Some(body))
}

/// Which changes to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// Appends `entries` to `changes`, first entry per path wins (TS keys on
/// `renameUri ?? uri`). A later deletion of the same path adds its old OID.
fn union_into(
	changes: &mut Vec<Change>,
	entries: Vec<RawEntry>,
	skipped: &mut usize,
) {
	for e in entries {
		let Ok(path) = String::from_utf8(e.path) else {
			*skipped += 1;
			continue;
		};
		// ponytail: linear lookup, O(n^2) per union; index by a HashMap if
		// huge merges get slow.
		if let Some(c) = changes.iter_mut().find(|c| c.path == path) {
			if c.status == b'D' && e.status == b'D' {
				c.deleted_from.push(e.old_oid);
			}
			continue;
		}
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
		});
	}
}

fn is_zero_oid(oid: &str) -> bool {
	oid.bytes().all(|b| b == b'0')
}

fn read_text(cat: &mut CatFile, oid: &str) -> io::Result<Option<String>> {
	Ok(cat.read(oid)?.and_then(decode_utf8_or_skip))
}

/// The pre-deletion content from the first OID that decodes, else the marker.
fn deleted_content(cat: &mut CatFile, oids: &[String]) -> io::Result<String> {
	for oid in oids.iter().filter(|o| !is_zero_oid(o)) {
		if let Some(text) = read_text(cat, oid)? {
			return Ok(text);
		}
	}
	Ok(DELETED_FILE_MARKER.to_string())
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
fn unmerged(git: &Git) -> Result<Vec<RawEntry>, GitError> {
	let out = git.run(&["ls-files", "-u", "-z"])?;
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

fn diff(git: &Git, args: &[&str]) -> Result<Vec<RawEntry>, GitError> {
	let mut all = vec!["diff"];
	all.extend_from_slice(args);
	parse_raw_z(&git.run(&all)?)
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

/// Changed paths for the desktop browser, including files whose content
/// cannot be put on the clipboard (binary or unreadable).
pub fn list_changed_paths(
	git: &Git,
	source: &GitSource,
) -> Result<Vec<(String, Option<ChangeType>)>, GitError> {
	Ok(collect_changes(git, source)?
		.0
		.into_iter()
		.map(|c| (c.path, Some(change_type_for_status(c.status))))
		.collect())
}

/// Ordered change metadata without reading file contents, plus the number of
/// entries dropped for a non-UTF-8 path name.
fn collect_changes(
	git: &Git,
	source: &GitSource,
) -> Result<(Vec<Change>, usize), GitError> {
	let mut skipped = 0;
	let mut changes = Vec::new();
	match source {
		GitSource::Working => {
			// VS Code keeps conflicts out of the working and index lists and
			// reports them as merge changes between untracked and index.
			let conflicts = unmerged(git)?;
			let resolved =
				|e: &RawEntry| !conflicts.iter().any(|c| c.path == e.path);
			let worktree =
				diff(git, &RAW)?.into_iter().filter(resolved).collect();
			union_into(&mut changes, worktree, &mut skipped);
			let out =
				git.run(&["ls-files", "--others", "--exclude-standard", "-z"])?;
			let untracked = out
				.split(|&b| b == 0)
				// A trailing `/` is a nested repository, not a file.
				.filter(|p| !p.is_empty() && !p.ends_with(b"/"))
				.map(|p| path_entry(b'A', p.to_vec()))
				.collect();
			union_into(&mut changes, untracked, &mut skipped);
			let merge = conflicts.clone();
			union_into(&mut changes, merge, &mut skipped);
			let mut cached = vec!["--cached"];
			cached.extend_from_slice(&RAW);
			let index =
				diff(git, &cached)?.into_iter().filter(resolved).collect();
			union_into(&mut changes, index, &mut skipped);
			// TS reads every deletion at `HEAD:<path>`; an unborn HEAD is
			// simply missing there, so the marker follows.
			for c in changes.iter_mut().filter(|c| c.status == b'D') {
				c.deleted_from = vec![format!("HEAD:{}", c.path)];
			}
		}
		GitSource::Staged => {
			let mut cached = vec!["--cached"];
			cached.extend_from_slice(&RAW);
			// A conflict is a merge change, not an index change.
			let index = diff(git, &cached)?
				.into_iter()
				.filter(|e| e.status != b'U')
				.collect();
			union_into(&mut changes, index, &mut skipped);
		}
		GitSource::Commit(rev) => {
			let sha = git.resolve_commit(rev)?;
			let mut parents = git.parents(&sha)?;
			if parents.is_empty() {
				// A shallow clone grafts its boundary commits to look
				// parentless; diffing one against the empty tree would copy
				// the whole repository as "this commit's change".
				if git.is_shallow()? {
					return Err(GitError::Shallow(sha));
				}
				parents.push(EMPTY_TREE.to_string());
			}
			for parent in &parents {
				let out = git.run(&[
					"diff-tree",
					"-r",
					"-z",
					"--raw",
					"--no-abbrev",
					"--no-commit-id",
					"-M",
					parent,
					&sha,
				])?;
				union_into(&mut changes, parse_raw_z(&out)?, &mut skipped);
			}
		}
		GitSource::Range(from, to) => {
			let from = git.resolve_commit(from)?;
			let to = git.resolve_commit(to)?;
			let mut args = RAW.to_vec();
			args.extend([from.as_str(), to.as_str()]);
			union_into(&mut changes, diff(git, &args)?, &mut skipped);
		}
	}

	Ok((changes, skipped))
}

fn collect_raw(
	git: &Git,
	source: &GitSource,
) -> Result<(Vec<PayloadFile>, usize), GitError> {
	let (changes, skipped) = collect_changes(git, source)?;
	Ok((read_changes(git, source, changes)?, skipped))
}

/// Read only the clicked path; listing a large repository never reads blobs.
pub fn read_changed_file(
	git: &Git,
	source: &GitSource,
	path: &str,
) -> Result<Option<PayloadFile>, GitError> {
	let (changes, _) = collect_changes(git, source)?;
	Ok(read_changes(
		git,
		source,
		changes.into_iter().filter(|c| c.path == path).collect(),
	)?
	.pop())
}

fn read_changes(
	git: &Git,
	source: &GitSource,
	changes: Vec<Change>,
) -> Result<Vec<PayloadFile>, GitError> {
	let mut cat = git.cat_file()?;
	let mut files = Vec::new();
	for c in changes {
		let change_type = change_type_for_status(c.status);
		let content = if change_type == ChangeType::Deleted {
			Some(deleted_content(&mut cat, &c.deleted_from)?)
		} else {
			match source {
				GitSource::Working => {
					read_text_file(&git.root.join(&c.path)).ok().flatten()
				}
				// Staged content was asked for: an index entry that cannot
				// be read is a visible placeholder, never a silent gap.
				// TS: `readRefContent(...) ?? UNREADABLE_FILE_MARKER`, so a
				// non-UTF-8 blob is the marker too.
				GitSource::Staged => Some(
					cat.read(&c.new_oid)?
						.and_then(decode_utf8_or_skip)
						.unwrap_or_else(|| UNREADABLE_FILE_MARKER.to_string()),
				),
				_ => read_text(&mut cat, &c.new_oid)?,
			}
		};
		files.push(PayloadFile {
			path: c.path,
			content,
			change_type: Some(change_type),
			skipped_reason: None,
		});
	}
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

/// Desktop selection: constrain a source to a monorepo folder and optional
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

	use super::*;
	use std::fs;
	use std::io::Cursor;

	// ---- catFile.test.ts ----

	fn batch(entries: &[(&str, &[u8])]) -> Vec<u8> {
		let mut out = Vec::new();
		for (oid, body) in entries {
			if *oid == "missing" {
				out.extend_from_slice(b"abc:gone.ts missing\n");
			} else {
				out.extend_from_slice(
					format!("{oid} blob {}\n", body.len()).as_bytes(),
				);
				out.extend_from_slice(body);
				out.push(b'\n');
			}
		}
		out
	}

	fn text(r: &mut Cursor<Vec<u8>>) -> Option<String> {
		read_batch_response(r)
			.unwrap()
			.and_then(decode_utf8_or_skip)
	}

	#[test]
	fn cat_file_parses_blobs_in_request_order() {
		let mut r = Cursor::new(batch(&[
			("1111", b"const a = 1;\n"),
			("2222", b"export const b = 2;"),
		]));
		assert_eq!(text(&mut r).as_deref(), Some("const a = 1;\n"));
		assert_eq!(text(&mut r).as_deref(), Some("export const b = 2;"));
	}

	#[test]
	fn cat_file_reads_by_byte_count_not_lines() {
		let tricky = "line1\n0000 blob 5\nline2";
		let mut r = Cursor::new(batch(&[
			("aaaa", tricky.as_bytes()),
			("bbbb", b"next"),
		]));
		assert_eq!(text(&mut r).as_deref(), Some(tricky));
		assert_eq!(text(&mut r).as_deref(), Some("next"));
	}

	#[test]
	fn cat_file_missing_is_distinct_and_keeps_alignment() {
		let mut r =
			Cursor::new(batch(&[("missing", b""), ("3333", b"still here")]));
		assert_eq!(read_batch_response(&mut r).unwrap(), None);
		assert_eq!(text(&mut r).as_deref(), Some("still here"));
	}

	#[test]
	fn cat_file_binary_and_non_utf8_are_skipped() {
		let mut r = Cursor::new(batch(&[
			("4444", &[0x50, 0x4e, 0x47, 0x00, 0x44]),
			("aaa", &[0xa4, 0xe9, 0xa5, 0xbb]), // Big5
			("bbb", b"ok"),
		]));
		assert_eq!(text(&mut r), None);
		assert_eq!(text(&mut r), None);
		assert_eq!(text(&mut r).as_deref(), Some("ok"));
	}

	#[test]
	fn cat_file_empty_blob_is_empty_not_missing() {
		let mut r = Cursor::new(batch(&[("5555", b"")]));
		assert_eq!(read_batch_response(&mut r).unwrap(), Some(Vec::new()));
	}

	#[test]
	fn cat_file_truncated_output_is_an_error_after_good_entries() {
		let mut r =
			Cursor::new(b"6666 blob 4\nabcd\n7777 blob 100\nshort".to_vec());
		assert_eq!(text(&mut r).as_deref(), Some("abcd"));
		assert!(read_batch_response(&mut r).is_err());
	}

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

	// macOS temp dirs live under the `/var` -> `/private/var` symlink, and git
	// reports the resolved toplevel; a root spelled through the link must
	// still relativize.
	#[cfg(unix)]
	#[test]
	fn payload_relativizes_against_a_root_spelled_through_a_symlink() {
		let r = Repo::new();
		r.write("sub/a.txt", b"one");
		r.commit("init");
		r.write("sub/a.txt", b"two");
		let outer = tempfile::tempdir().unwrap();
		let link = outer.path().join("link");
		std::os::unix::fs::symlink(r.path(), &link).unwrap();
		let git = Git::open(&link.join("sub")).unwrap();
		let got = collect_payload(
			&git,
			&GitSource::Working,
			&[link.join("sub")],
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
}
