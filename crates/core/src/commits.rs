//! Commit mode: serialize and replay a range of commits (spec section 4).
//!
//! snip-sync only; the IDE plugins have no counterpart. Selection follows
//! first parents, each commit carries its diff against its first parent, and
//! replay writes the files then commits ONLY those paths, so anything the
//! user had staged stays out of the new commits.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::fsutil::{must_not_overwrite, write_text_file};
use crate::gitsrc::{parse_raw_z, Git, GitError, EMPTY_TREE};
use crate::paths::{escapes_all_roots, resolve_write_target};

/// First line of a commit-mode payload; the rest is JSON.
pub use crate::clip::COMMIT_MARKER;

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
	#[error(transparent)]
	Git(#[from] GitError),
	#[error("no commits selected")]
	Empty,
	#[error("requested {requested} commits, but HEAD has only {available} along its first parents")]
	NotEnoughCommits { requested: usize, available: usize },
	#[error("commits are not contiguous: following first parents back from {tip}, {at} has {}, so {base} is never reached", first_parent.as_deref().map_or("no parent".to_string(), |p| format!("first parent {p}")))]
	Discontinuous {
		base: String,
		tip: String,
		/// The oldest commit of the chain that is not in `base`'s history.
		at: String,
		first_parent: Option<String>,
	},
	#[error("not a snip-sync commits payload")]
	NotCommitPayload,
	#[error("invalid commits payload: {0}")]
	InvalidPayload(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileChange {
	Added,
	Modified,
	Deleted,
	/// `old_path` holds the source path.
	Renamed,
}

/// Why a file is listed without content. Replay neither writes nor deletes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NotCopiedReason {
	Binary,
	NonUtf8,
	/// The path itself is not UTF-8; `path` is a lossy rendering.
	NonUtf8Path,
	/// A symlink or submodule: not a regular text file.
	UnsupportedType,
	Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitFile {
	pub path: String,
	pub old_path: Option<String>,
	pub change: FileChange,
	/// Full content after the change; `None` for deletions and not-copied.
	pub content: Option<String>,
	pub not_copied: Option<NotCopiedReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitRecord {
	/// Raw message (`%B`), replayed verbatim.
	pub message: String,
	pub author_name: String,
	pub author_email: String,
	/// Strict ISO 8601 with the original offset (`%aI`).
	pub author_date: String,
	pub files: Vec<CommitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitsPayload {
	/// Oldest first, the order they are replayed in.
	pub commits: Vec<CommitRecord>,
}

fn contiguous_chain(
	git: &Git,
	rev_list_out: &[u8],
) -> Result<Vec<(String, Option<String>)>, CommitError> {
	// `rev-list --parents` lines: `<sha> [<first parent> ...]`, newest first.
	let chain: Vec<(String, Option<String>)> =
		String::from_utf8_lossy(rev_list_out)
			.lines()
			.filter_map(|l| {
				let mut it = l.split(' ');
				let sha = it.next().filter(|s| !s.is_empty())?;
				Some((sha.to_string(), it.next().map(str::to_string)))
			})
			.collect();
	if let Some((oldest, None)) = chain.last() {
		// A shallow boundary looks parentless; it is not a real root.
		if git.is_shallow()? {
			return Err(GitError::Shallow(oldest.clone()).into());
		}
	}
	Ok(chain)
}

/// The commits of `base..tip` (base excluded), oldest first. Every commit
/// must lie on `tip`'s first-parent chain down to `base`.
pub fn select_range(
	git: &Git,
	base: &str,
	tip: &str,
) -> Result<Vec<String>, CommitError> {
	let base = git.resolve_commit(base)?;
	let tip = git.resolve_commit(tip)?;
	let range = format!("{base}..{tip}");
	let out = git.run(&["rev-list", "--first-parent", "--parents", &range])?;
	let chain = contiguous_chain(git, &out)?;
	let Some((oldest, first_parent)) = chain.last() else {
		return Err(CommitError::Empty);
	};
	// `base..tip` drops everything reachable from base, so the chain is
	// contiguous exactly when its oldest commit sits directly on base.
	if first_parent.as_deref() != Some(base.as_str()) {
		return Err(CommitError::Discontinuous {
			at: oldest.clone(),
			first_parent: first_parent.clone(),
			base,
			tip,
		});
	}
	Ok(chain.into_iter().rev().map(|(sha, _)| sha).collect())
}

/// The last `n` commits on HEAD's first-parent chain, oldest first.
pub fn select_last(git: &Git, n: usize) -> Result<Vec<String>, CommitError> {
	select_last_from(git, "HEAD", n)
}

/// Select from an explicit tip, including a root on a non-current branch.
pub fn select_last_from(
	git: &Git,
	tip: &str,
	n: usize,
) -> Result<Vec<String>, CommitError> {
	if n == 0 {
		return Err(CommitError::Empty);
	}
	let tip = git.resolve_commit(tip)?;
	let count = n.to_string();
	let out = git.run(&[
		"rev-list",
		"--first-parent",
		"--parents",
		"-n",
		&count,
		&tip,
	])?;
	let chain = contiguous_chain(git, &out)?;
	if chain.len() < n {
		return Err(CommitError::NotEnoughCommits {
			requested: n,
			available: chain.len(),
		});
	}
	Ok(chain.into_iter().rev().map(|(sha, _)| sha).collect())
}

fn is_special_mode(mode: &str) -> bool {
	// 120000 = symlink, 160000 = submodule (gitlink).
	mode == "120000" || mode == "160000"
}

fn lossy(bytes: &[u8]) -> String {
	String::from_utf8_lossy(bytes).into_owned()
}

/// Reads one commit: metadata plus its change against the first parent.
fn read_commit(
	git: &Git,
	cat: &mut crate::gitsrc::CatFile,
	sha: &str,
) -> Result<CommitRecord, CommitError> {
	let out =
		git.run(&["log", "-1", "-z", "--format=%an%x00%ae%x00%aI%x00%B", sha])?;
	let mut fields = out.splitn(4, |&b| b == 0);
	let mut next = || fields.next().map(lossy);
	let (Some(author_name), Some(author_email), Some(author_date)) =
		(next(), next(), next())
	else {
		return Err(GitError::Malformed("commit metadata".into()).into());
	};
	let mut message = next().unwrap_or_default();
	if message.ends_with('\0') {
		message.pop();
	}

	let parent = match git.parents(sha)?.into_iter().next() {
		Some(p) => p,
		None if git.is_shallow()? => {
			return Err(GitError::Shallow(sha.to_string()).into())
		}
		None => EMPTY_TREE.to_string(),
	};
	let raw = git.run(&[
		"diff-tree",
		"-r",
		"-z",
		"--raw",
		"--no-abbrev",
		"--no-commit-id",
		"-M",
		&parent,
		sha,
	])?;

	let mut files = Vec::new();
	for e in parse_raw_z(&raw)? {
		let change = match e.status {
			b'A' | b'C' => FileChange::Added,
			b'D' => FileChange::Deleted,
			b'R' => FileChange::Renamed,
			_ => FileChange::Modified,
		};
		let path_ok = std::str::from_utf8(&e.path).is_ok()
			&& e.old_path
				.as_deref()
				.is_none_or(|p| std::str::from_utf8(p).is_ok());
		let special = if change == FileChange::Deleted {
			is_special_mode(&e.old_mode)
		} else {
			is_special_mode(&e.new_mode)
				|| (change == FileChange::Renamed
					&& is_special_mode(&e.old_mode))
		};
		let (content, not_copied) = if !path_ok {
			(None, Some(NotCopiedReason::NonUtf8Path))
		} else if special {
			(None, Some(NotCopiedReason::UnsupportedType))
		} else if change == FileChange::Deleted {
			(None, None)
		} else {
			match cat.read(&e.new_oid).map_err(GitError::from)? {
				None => (None, Some(NotCopiedReason::Unreadable)),
				Some(b) if b.contains(&0) => {
					(None, Some(NotCopiedReason::Binary))
				}
				Some(b) => match String::from_utf8(b) {
					Ok(s) => (Some(s), None),
					Err(_) => (None, Some(NotCopiedReason::NonUtf8)),
				},
			}
		};
		files.push(CommitFile {
			path: lossy(&e.path),
			old_path: e.old_path.as_deref().map(lossy),
			change,
			content,
			not_copied,
		});
	}
	Ok(CommitRecord {
		message,
		author_name,
		author_email,
		author_date,
		files,
	})
}

/// Reads `shas` (oldest first, as the selectors return them).
pub fn copy_commits(
	git: &Git,
	shas: &[String],
) -> Result<CommitsPayload, CommitError> {
	let mut cat = git.cat_file()?;
	let commits = shas
		.iter()
		.map(|sha| read_commit(git, &mut cat, sha))
		.collect::<Result<_, _>>()?;
	Ok(CommitsPayload { commits })
}

/// The clipboard text: marker line, then JSON.
pub fn to_clipboard_text(payload: &CommitsPayload) -> String {
	let json = serde_json::to_string(payload)
		.expect("commit payload always serializes");
	format!("{COMMIT_MARKER}\n{json}")
}

/// Same rule as [`crate::clip::detect_mode`]: the marker is the first line.
pub fn is_commit_payload(text: &str) -> bool {
	crate::clip::detect_mode(text) == crate::clip::Mode::Commits
}

pub fn parse_commit_payload(text: &str) -> Result<CommitsPayload, CommitError> {
	if !is_commit_payload(text) {
		return Err(CommitError::NotCommitPayload);
	}
	let json = text.split_once('\n').map_or("", |(_, rest)| rest);
	serde_json::from_str(json)
		.map_err(|e| CommitError::InvalidPayload(e.to_string()))
}

/// Copy notification numbers (spec 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitCopySummary {
	pub commit_count: usize,
	pub file_count: usize,
	/// UTF-16 code units of the clipboard text.
	pub chars: usize,
	pub not_copied_count: usize,
}

pub fn copy_summary(payload: &CommitsPayload, text: &str) -> CommitCopySummary {
	let files = payload.commits.iter().flat_map(|c| &c.files);
	CommitCopySummary {
		commit_count: payload.commits.len(),
		file_count: files.clone().count(),
		chars: text.encode_utf16().count(),
		not_copied_count: files.filter(|f| f.not_copied.is_some()).count(),
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReplayAction {
	/// Write the content (a rename also deletes `old_path` first).
	Write,
	Delete,
	Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReplaySkipReason {
	/// See the file's `not_copied`.
	NotCopied,
	/// Rejected by the path rules (`paths`), or not repo-relative.
	UnsafePath,
	/// The file on disk is not UTF-8 or cannot be verified.
	NonUtf8Target,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct FilePlan {
	pub path: String,
	pub old_path: Option<String>,
	pub change: FileChange,
	pub action: ReplayAction,
	/// The target exists now: a write overwrites, a delete removes it.
	pub existed: bool,
	pub not_copied: Option<NotCopiedReason>,
	pub skip_reason: Option<ReplaySkipReason>,
	pub absolute_path: Option<PathBuf>,
	pub old_absolute_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitPlan {
	pub message: String,
	pub author_name: String,
	pub author_email: String,
	pub author_date: String,
	/// Index-aligned with the payload commit's `files`.
	pub files: Vec<FilePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitReplayPlan {
	pub root: PathBuf,
	pub commits: Vec<CommitPlan>,
}

/// A repo-relative path that passes the restore path rules. Anything the
/// resolver would rewrite (absolute, root-labelled, `./`) is refused: git
/// only ever produces plain relative paths.
fn target(root: &Path, path: &str) -> Option<PathBuf> {
	// Like git ("beyond a symbolic link"), refuse paths whose parent
	// directories go through a symlink: containment alone would let the
	// write or delete land on another tracked path.
	let mut dir = root.to_path_buf();
	let parents = path.split('/').collect::<Vec<_>>();
	for segment in &parents[..parents.len().saturating_sub(1)] {
		dir.push(segment);
		if is_symlink(&dir) {
			return None;
		}
	}
	resolve_write_target(&[root], path)
		.ok()
		.filter(|t| t.relative_path == path)
		.map(|t| t.absolute_path)
}

fn plan_file(root: &Path, f: &CommitFile) -> FilePlan {
	let mut plan = FilePlan {
		path: f.path.clone(),
		old_path: f.old_path.clone(),
		change: f.change,
		action: ReplayAction::Skip,
		existed: false,
		not_copied: f.not_copied,
		skip_reason: None,
		absolute_path: None,
		old_absolute_path: None,
	};
	let deleted = f.change == FileChange::Deleted;
	if !deleted && f.content.is_none() && plan.not_copied.is_none() {
		plan.not_copied = Some(NotCopiedReason::Unreadable);
	}
	if plan.not_copied.is_some() {
		plan.skip_reason = Some(ReplaySkipReason::NotCopied);
		return plan;
	}
	let old = f.old_path.as_deref().map(|p| target(root, p));
	let (Some(abs), None | Some(Some(_))) = (target(root, &f.path), &old)
	else {
		plan.skip_reason = Some(ReplaySkipReason::UnsafePath);
		return plan;
	};
	// A symlink is replaced, not written through: its target is irrelevant.
	if !deleted && !is_symlink(&abs) && must_not_overwrite(&abs) {
		plan.skip_reason = Some(ReplaySkipReason::NonUtf8Target);
		return plan;
	}
	plan.action = if deleted {
		ReplayAction::Delete
	} else {
		ReplayAction::Write
	};
	plan.existed = abs.exists();
	plan.absolute_path = Some(abs);
	plan.old_absolute_path = old.flatten();
	plan
}

fn plan_commit(root: &Path, c: &CommitRecord) -> CommitPlan {
	CommitPlan {
		message: c.message.clone(),
		author_name: c.author_name.clone(),
		author_email: c.author_email.clone(),
		author_date: c.author_date.clone(),
		files: c.files.iter().map(|f| plan_file(root, f)).collect(),
	}
}

/// Preview: what replaying `payload` onto the current disk state would do.
pub fn plan_commit_replay(
	git: &Git,
	payload: &CommitsPayload,
) -> CommitReplayPlan {
	let root = git.root().to_path_buf();
	CommitReplayPlan {
		commits: payload
			.commits
			.iter()
			.map(|c| plan_commit(&root, c))
			.collect(),
		root,
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayFailure {
	/// Index into the payload's commits.
	pub index: usize,
	pub message: String,
	pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
	/// New commit OIDs, in replay order. Never rolled back.
	pub created: Vec<String>,
	/// The commit replay stopped at, if any.
	pub failure: Option<ReplayFailure>,
}

/// Replays every commit onto the current HEAD, stopping at the first
/// failure. Each commit is re-planned right before it runs, so the path and
/// encoding checks see the disk as earlier commits left it.
pub fn replay(git: &Git, payload: &CommitsPayload) -> ReplayResult {
	let mut result = ReplayResult::default();
	for (index, commit) in payload.commits.iter().enumerate() {
		match replay_commit(git, commit) {
			Ok(sha) => result.created.push(sha),
			Err(error) => {
				result.failure = Some(ReplayFailure {
					index,
					message: commit.message.clone(),
					error,
				});
				break;
			}
		}
	}
	result
}

fn replay_commit(git: &Git, commit: &CommitRecord) -> Result<String, String> {
	let root = git.root();
	let plan = plan_commit(root, commit);
	// Paths whose change is on disk now; they alone go into the commit.
	let mut paths: Vec<&str> = Vec::new();
	// Deleted paths: staged only if HEAD tracks them. A path that is only in
	// the index (a staged new file) has nothing to commit, and `commit
	// --only` would reject it once `add` dropped it from the index.
	let mut deleted: Vec<&str> = Vec::new();

	// Recheck every write target before anything destructive happens, so a
	// target that changed since planning stops the replay instead of
	// leaving a commit that only records the rename's deletion.
	for (f, src) in plan.files.iter().zip(&commit.files) {
		let (ReplayAction::Write, Some(abs), Some(_)) =
			(f.action, &f.absolute_path, &src.content)
		else {
			continue;
		};
		if escapes_all_roots(&[root], abs) {
			return Err(format!("{}: unsafe path", f.path));
		}
		if !is_symlink(abs) && must_not_overwrite(abs) {
			return Err(format!(
				"{}: target is not UTF-8 or cannot be verified",
				f.path
			));
		}
	}

	// Deletions first: a rename swap or a rename onto a re-added path must
	// not delete what this commit just wrote.
	for f in &plan.files {
		let old = f.old_absolute_path.as_deref().zip(f.old_path.as_deref());
		let del = (f.action == ReplayAction::Delete)
			.then(|| f.absolute_path.as_deref().map(|a| (a, f.path.as_str())))
			.flatten();
		for (abs, rel) in old.into_iter().chain(del) {
			delete(root, abs, rel)?;
			deleted.push(rel);
		}
	}
	for (f, src) in plan.files.iter().zip(&commit.files) {
		let (ReplayAction::Write, Some(abs), Some(content)) =
			(f.action, &f.absolute_path, &src.content)
		else {
			continue;
		};
		if escapes_all_roots(&[root], abs) {
			return Err(format!("{}: unsafe path", f.path));
		}
		if is_symlink(abs) {
			// Replace the link itself; writing would follow it.
			fs::remove_file(abs).map_err(|e| format!("{}: {e}", f.path))?;
		}
		write_text_file(abs, content)
			.map_err(|e| format!("{}: {e}", f.path))?;
		paths.push(&f.path);
	}

	let err = |e: GitError| e.to_string();
	if !deleted.is_empty() {
		let mut args = vec![
			"--literal-pathspecs",
			"ls-tree",
			"-z",
			"--name-only",
			"HEAD",
			"--",
		];
		args.extend(&deleted);
		// An unborn HEAD tracks nothing.
		let out = if git.run(&["rev-parse", "-q", "--verify", "HEAD"]).is_ok() {
			git.run(&args).map_err(err)?
		} else {
			Vec::new()
		};
		let tracked: Vec<&[u8]> =
			out.split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
		paths
			.extend(deleted.iter().filter(|p| tracked.contains(&p.as_bytes())));
	}
	paths.sort_unstable();
	paths.dedup();

	if !paths.is_empty() {
		// `-f`: the source tracked it, even if it is ignored here.
		let mut args = vec!["--literal-pathspecs", "add", "-A", "-f", "--"];
		args.extend(&paths);
		git.run(&args).map_err(err)?;
	}
	// `--only` with no paths still commits HEAD's tree, never the index.
	let mut args = vec![
		"--literal-pathspecs",
		"commit",
		"--quiet",
		"--only",
		"--no-verify",
		"--allow-empty",
		"--cleanup=verbatim",
		"-F",
		"-",
		"--",
	];
	args.extend(&paths);
	run_commit(git, &args, commit).map_err(err)?;
	let head = git.run(&["rev-parse", "HEAD"]).map_err(err)?;
	Ok(String::from_utf8_lossy(&head).trim().to_string())
}

/// Removes `abs`; already absent is fine. Parent directories left empty go
/// too (as git checkout does), so a later write may put a file there.
fn delete(root: &Path, abs: &Path, rel: &str) -> Result<(), String> {
	if escapes_all_roots(&[root], abs) {
		return Err(format!("{rel}: unsafe path"));
	}
	match fs::remove_file(abs) {
		Ok(()) => {}
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(e) => return Err(format!("{rel}: {e}")),
	}
	let mut dir = abs.parent();
	// `remove_dir` fails on a non-empty directory, which ends the walk.
	while let Some(d) = dir.filter(|d| *d != root && d.starts_with(root)) {
		if fs::remove_dir(d).is_err() {
			break;
		}
		dir = d.parent();
	}
	Ok(())
}

fn is_symlink(p: &Path) -> bool {
	fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn run_commit(
	git: &Git,
	args: &[&str],
	commit: &CommitRecord,
) -> Result<(), GitError> {
	// Author through the environment: `--author` would treat a value
	// without `<email>` as a search pattern. The committer stays local.
	let mut child = git
		.command()
		.args(args)
		.env("GIT_AUTHOR_NAME", &commit.author_name)
		.env("GIT_AUTHOR_EMAIL", &commit.author_email)
		.env("GIT_AUTHOR_DATE", &commit.author_date)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	if let Some(mut stdin) = child.stdin.take() {
		stdin.write_all(commit.message.as_bytes())?;
	}
	let out = child.wait_with_output()?;
	if !out.status.success() {
		return Err(GitError::Failed {
			args: args.join(" "),
			stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
		});
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::Command;

	struct Repo {
		dir: tempfile::TempDir,
		cfg: PathBuf,
	}

	impl Repo {
		fn new(branch: &str) -> Self {
			let dir = tempfile::tempdir().unwrap();
			let cfg = dir.path().join("empty.gitconfig");
			fs::write(&cfg, "").unwrap();
			fs::create_dir(dir.path().join("r")).unwrap();
			let repo = Self { dir, cfg };
			repo.git(&["init", "-q", "-b", branch]);
			repo.git(&["config", "user.name", "Local"]);
			repo.git(&["config", "user.email", "local@example.com"]);
			repo.git(&["config", "core.autocrlf", "false"]);
			repo.git(&["config", "commit.gpgsign", "false"]);
			repo
		}

		fn path(&self) -> PathBuf {
			self.dir.path().join("r")
		}

		fn cmd(&self, args: &[&str]) -> Command {
			let mut c = Command::new("git");
			c.args(args)
				.current_dir(self.path())
				.env("GIT_CONFIG_GLOBAL", &self.cfg)
				.env("GIT_CONFIG_NOSYSTEM", "1");
			c
		}

		fn git(&self, args: &[&str]) -> String {
			let out = self.cmd(args).output().unwrap();
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

		/// Commits everything as `Alice` at `date`.
		fn commit(&self, msg: &str, date: &str) -> String {
			self.git(&["add", "-A"]);
			let out = self
				.cmd(&[
					"commit",
					"-q",
					"--allow-empty",
					"--cleanup=verbatim",
					"-m",
					msg,
				])
				.env("GIT_AUTHOR_NAME", "Alice")
				.env("GIT_AUTHOR_EMAIL", "alice@example.com")
				.env("GIT_AUTHOR_DATE", date)
				.output()
				.unwrap();
			assert!(out.status.success());
			self.git(&["rev-parse", "HEAD"])
		}

		fn open(&self) -> Git {
			Git::open(&self.path()).unwrap()
		}

		/// `%an|%ae|%aI` of `rev`, plus its raw message.
		fn meta(&self, rev: &str) -> (String, String) {
			let who = self.git(&["log", "-1", "--format=%an|%ae|%aI", rev]);
			let raw = self.git(&["cat-file", "commit", rev]);
			let msg = raw.split_once("\n\n").unwrap().1.to_string();
			(who, msg)
		}
	}

	/// Source history: base, then a merge, a rename + binary, a delete.
	fn source() -> (Repo, String, Vec<String>) {
		let r = Repo::new("main");
		r.write("keep.txt", b"keep\n");
		r.write("old.txt", b"rename me\nline 2\nline 3\n");
		r.write("gone.txt", b"to be deleted\n");
		let base = r.commit("base", "2020-01-01T00:00:00+00:00");
		r.git(&["checkout", "-q", "-b", "side"]);
		r.write("side.txt", b"from side\n");
		r.commit("side work", "2020-01-01T12:00:00+00:00");
		r.git(&["checkout", "-q", "main"]);
		r.write("keep.txt", b"keep v2\n");
		r.commit("main work", "2020-01-01T13:00:00+00:00");
		// First parent main, second parent side: only side.txt differs
		// from the first parent.
		let out = r
			.cmd(&["merge", "-q", "--no-ff", "--no-edit", "side"])
			.env("GIT_AUTHOR_NAME", "Alice")
			.env("GIT_AUTHOR_EMAIL", "alice@example.com")
			.env("GIT_AUTHOR_DATE", "2020-01-02T03:04:05+09:00")
			.output()
			.unwrap();
		assert!(out.status.success());
		let merge = r.git(&["rev-parse", "HEAD"]);
		fs::create_dir(r.path().join("dir")).unwrap();
		r.git(&["mv", "old.txt", "dir/new.txt"]);
		r.write("img.bin", &[0x89, b'P', b'N', b'G', 0, 1, 2]);
		r.write("big5.txt", &[0xa4, 0xe9, 0xa5, 0xbb]);
		let second = r.commit(
			"rename and binary\n\n\nbody with two blank lines  \n# not a comment\n",
			"2021-06-07T08:09:10-05:30",
		);
		r.git(&["rm", "-q", "gone.txt"]);
		let third = r.commit("delete", "2022-02-03T04:05:06+01:00");
		let _ = base;
		(r, merge.clone(), vec![merge, second, third])
	}

	#[test]
	fn commits_select_copy_and_replay_round_trip() {
		let (src, _, expected) = source();
		let g = src.open();
		let shas = select_last(&g, 3).unwrap();
		assert_eq!(shas, expected);
		let range =
			select_range(&g, &format!("{}^", expected[0]), "HEAD").unwrap();
		assert_eq!(range, expected);

		let payload = copy_commits(&g, &shas).unwrap();
		let text = to_clipboard_text(&payload);
		assert!(text.starts_with("// snip-sync commits v1\n{"));
		assert!(is_commit_payload(&text));
		assert!(!is_commit_payload("// clipcode-root: x\n"));
		let parsed = parse_commit_payload(&text).unwrap();
		assert_eq!(parsed, payload);

		// Merge: only the diff against the first parent.
		let merge = &payload.commits[0];
		assert_eq!(merge.files.len(), 1);
		assert_eq!(merge.files[0].path, "side.txt");
		assert_eq!(merge.files[0].change, FileChange::Added);
		let second = &payload.commits[1];
		let find = |p: &str| second.files.iter().find(|f| f.path == p).unwrap();
		assert_eq!(find("dir/new.txt").change, FileChange::Renamed);
		assert_eq!(find("dir/new.txt").old_path.as_deref(), Some("old.txt"));
		assert_eq!(find("img.bin").not_copied, Some(NotCopiedReason::Binary));
		assert_eq!(find("img.bin").content, None);
		assert_eq!(find("big5.txt").not_copied, Some(NotCopiedReason::NonUtf8));
		let third = &payload.commits[2];
		assert_eq!(third.files[0].change, FileChange::Deleted);
		assert_eq!(third.files[0].content, None);
		let summary = copy_summary(&payload, &text);
		assert_eq!(
			(
				summary.commit_count,
				summary.file_count,
				summary.not_copied_count
			),
			(3, 5, 2)
		);
		assert_eq!(summary.chars, text.encode_utf16().count());

		// Target: unrelated history on another branch, with a foreign
		// staged file that must stay out of every replayed commit.
		let dst = Repo::new("feature");
		dst.write("old.txt", b"rename me\nline 2\nline 3\n");
		dst.write("gone.txt", b"to be deleted\n");
		dst.write("keep.txt", b"target keep\n");
		dst.commit("target root", "2019-01-01T00:00:00+00:00");
		dst.write("foreign.txt", b"staged by the user\n");
		dst.git(&["add", "foreign.txt"]);
		let tg = dst.open();

		let plan = plan_commit_replay(&tg, &parsed);
		assert_eq!(plan.commits.len(), 3);
		let p2 = &plan.commits[1].files;
		let pf = |p: &str| p2.iter().find(|f| f.path == p).unwrap();
		assert_eq!(pf("dir/new.txt").action, ReplayAction::Write);
		assert!(pf("dir/new.txt").old_absolute_path.is_some());
		assert_eq!(pf("img.bin").action, ReplayAction::Skip);
		assert_eq!(
			pf("img.bin").skip_reason,
			Some(ReplaySkipReason::NotCopied)
		);
		assert_eq!(plan.commits[2].files[0].action, ReplayAction::Delete);
		assert!(plan.commits[2].files[0].existed);

		let result = replay(&tg, &parsed);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 3);
		assert_eq!(dst.git(&["rev-parse", "HEAD"]), result.created[2]);

		for (i, created) in result.created.iter().enumerate() {
			assert_eq!(dst.meta(created), src.meta(&expected[i]), "commit {i}");
			let committer = dst.git(&["log", "-1", "--format=%cn", created]);
			assert_eq!(committer, "Local");
			let tree = dst.git(&["ls-tree", "-r", "--name-only", created]);
			assert!(!tree.contains("foreign.txt"), "commit {i}: {tree}");
		}
		assert_eq!(
			dst.git(&["diff", "--cached", "--name-only"]),
			"foreign.txt"
		);
		let show = |r: &Repo, spec: &str| r.git(&["show", spec]);
		assert_eq!(show(&dst, "HEAD:side.txt"), show(&src, "HEAD:side.txt"));
		assert_eq!(
			show(&dst, "HEAD:dir/new.txt"),
			show(&src, "HEAD:dir/new.txt")
		);
		assert_eq!(show(&dst, "HEAD:keep.txt"), "target keep");
		let tree = dst.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
		assert_eq!(tree, "dir/new.txt\nkeep.txt\nside.txt");
		// The second commit contains exactly the rename.
		let changed = dst.git(&[
			"diff-tree",
			"-r",
			"--name-status",
			"--no-commit-id",
			"-M",
			&result.created[1],
		]);
		assert_eq!(changed, "R100\told.txt\tdir/new.txt");
		assert!(!dst.path().join("img.bin").exists());
	}

	#[test]
	fn commits_non_contiguous_selection_is_refused() {
		let (src, merge, _) = source();
		let g = src.open();
		// `side work` is merged in through the second parent only.
		let side = src.git(&["rev-parse", "side"]);
		let err = select_range(&g, &side, "HEAD").unwrap_err();
		match &err {
			CommitError::Discontinuous {
				at, first_parent, ..
			} => {
				let main_work = src.git(&["rev-parse", &format!("{merge}^1")]);
				assert_eq!(at, &main_work);
				let base = src.git(&["rev-parse", &format!("{merge}^1^")]);
				assert_eq!(first_parent.as_deref(), Some(base.as_str()));
			}
			e => panic!("{e}"),
		}
		assert!(err.to_string().contains("not contiguous"));
		assert!(matches!(
			select_range(&g, "HEAD", "HEAD"),
			Err(CommitError::Empty)
		));
		assert!(matches!(
			select_last(&g, 99),
			Err(CommitError::NotEnoughCommits { available: 5, .. })
		));
	}

	#[test]
	fn commits_payload_detection_and_errors() {
		assert!(is_commit_payload("// snip-sync commits v1\r\n{}"));
		assert!(!is_commit_payload("// snip-sync commits v2\n{}"));
		assert!(matches!(
			parse_commit_payload("// snip-sync commits v1\n{bad"),
			Err(CommitError::InvalidPayload(_))
		));
		assert!(matches!(
			parse_commit_payload("x"),
			Err(CommitError::NotCommitPayload)
		));
	}

	#[test]
	fn commits_unsafe_paths_are_skipped_and_replay_stops_on_failure() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let record = |msg: &str, files| CommitRecord {
			message: msg.into(),
			author_name: "Bob".into(),
			author_email: "bob@example.com".into(),
			author_date: "2020-01-01T00:00:00+00:00".into(),
			files,
		};
		let payload = CommitsPayload {
			commits: vec![
				record(
					"ok\n",
					vec![
						file("../escape.txt"),
						file("/abs.txt"),
						file("b.txt"),
					],
				),
				record("bad date\n", vec![file("c.txt")]),
				record("never\n", vec![file("d.txt")]),
			],
		};
		let mut payload = payload;
		payload.commits[1].author_date = "not a date".into();
		let g = dst.open();
		let plan = plan_commit_replay(&g, &payload);
		let skips: Vec<_> = plan.commits[0]
			.files
			.iter()
			.map(|f| f.skip_reason)
			.collect();
		assert_eq!(
			skips,
			vec![
				Some(ReplaySkipReason::UnsafePath),
				Some(ReplaySkipReason::UnsafePath),
				None
			]
		);
		let result = replay(&g, &payload);
		assert_eq!(result.created.len(), 1);
		let failure = result.failure.unwrap();
		assert_eq!(failure.index, 1);
		assert!(failure.error.contains("commit"), "{}", failure.error);
		assert!(!dst.dir.path().join("escape.txt").exists());
		let tree =
			dst.git(&["ls-tree", "-r", "--name-only", &result.created[0]]);
		assert_eq!(tree, "a.txt\nb.txt");
	}

	#[test]
	fn commits_empty_commit_keeps_index_and_untracked_delete_is_fine() {
		let dst = Repo::new("main");
		dst.write("a.txt", b"a\n");
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let before = dst.git(&["rev-parse", "HEAD^{tree}"]);
		dst.write("foreign.txt", b"staged\n");
		dst.git(&["add", "foreign.txt"]);
		// Untracked at the target; the source deletes it.
		dst.write("gone.txt", b"untracked\n");
		let record = |msg: &str, file: CommitFile| CommitRecord {
			message: msg.into(),
			author_name: "Bob".into(),
			author_email: "bob@example.com".into(),
			author_date: "2020-01-01T00:00:00+00:00".into(),
			files: vec![file],
		};
		let file = |path: &str, change, not_copied| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: None,
			not_copied,
		};
		let payload = CommitsPayload {
			commits: vec![
				record(
					"only binary\n",
					file(
						"img.bin",
						FileChange::Added,
						Some(NotCopiedReason::Binary),
					),
				),
				record(
					"delete untracked\n",
					file("gone.txt", FileChange::Deleted, None),
				),
			],
		};
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 2);
		for created in &result.created {
			let tree = dst.git(&["rev-parse", &format!("{created}^{{tree}}")]);
			assert_eq!(tree, before);
		}
		assert!(!dst.path().join("gone.txt").exists());
		assert_eq!(
			dst.git(&["diff", "--cached", "--name-only"]),
			"foreign.txt"
		);
	}

	#[cfg(unix)]
	#[test]
	fn commits_replay_replaces_symlink_dir_and_keeps_staged_new_file() {
		let dst = Repo::new("main");
		dst.write("real.txt", b"real\n");
		dst.write("a/b", b"nested\n");
		std::os::unix::fs::symlink("real.txt", dst.path().join("l")).unwrap();
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		// A new file only in the index; the replayed commit deletes it.
		dst.write("f", b"staged new\n");
		dst.git(&["add", "f"]);
		let file = |path: &str, change, content: Option<&str>| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: content.map(Into::into),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "mixed\n".into(),
				author_name: "Bob".into(),
				author_email: "bob@example.com".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![
					file("a/b", FileChange::Deleted, None),
					file("a", FileChange::Added, Some("now a file\n")),
					file("l", FileChange::Modified, Some("now regular\n")),
					file("f", FileChange::Deleted, None),
				],
			}],
		};
		let result = replay(&dst.open(), &payload);
		assert_eq!(result.failure, None);
		assert_eq!(result.created.len(), 1);
		let tree = dst.git(&["ls-tree", "-r", "HEAD"]);
		assert!(tree.contains("100644 blob"), "{tree}");
		assert!(!tree.contains("120000"), "{tree}");
		assert_eq!(dst.git(&["show", "HEAD:a"]), "now a file");
		assert_eq!(dst.git(&["show", "HEAD:l"]), "now regular");
		assert_eq!(
			fs::read_to_string(dst.path().join("real.txt")).unwrap(),
			"real\n"
		);
		// `f` never reached HEAD, so it stays staged and out of the commit.
		assert_eq!(dst.git(&["diff", "--cached", "--name-only"]), "f");
		let names = dst.git(&["ls-tree", "-r", "--name-only", "HEAD"]);
		assert_eq!(names, "a\nl\nreal.txt");
	}

	#[cfg(unix)]
	#[test]
	fn commits_replay_refuses_paths_through_a_symlinked_directory() {
		let dst = Repo::new("main");
		dst.write("other/x.txt", b"x\n");
		dst.write("other/y.txt", b"y\n");
		std::os::unix::fs::symlink("other", dst.path().join("d")).unwrap();
		dst.commit("root", "2019-01-01T00:00:00+00:00");
		let file = |path: &str, change, content: Option<&str>| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: content.map(Into::into),
			not_copied: None,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "through link\n".into(),
				author_name: "Bob".into(),
				author_email: "bob@example.com".into(),
				author_date: "2020-01-01T00:00:00+00:00".into(),
				files: vec![
					file("d/y.txt", FileChange::Deleted, None),
					file("d/x.txt", FileChange::Modified, Some("NEW\n")),
				],
			}],
		};
		let plan = plan_commit_replay(&dst.open(), &payload);
		assert!(plan.commits[0]
			.files
			.iter()
			.all(|f| f.action == ReplayAction::Skip));
		replay(&dst.open(), &payload);
		// The real files behind the link are untouched.
		assert_eq!(
			fs::read_to_string(dst.path().join("other/x.txt")).unwrap(),
			"x\n"
		);
		assert!(dst.path().join("other/y.txt").exists());
	}
}
