//! Read-only workspace navigation and history for the desktop workbench.
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use ts_rs::TS;

use crate::fsutil::decode_utf8_or_skip;
use crate::gitrun::{Overflow, RunOptions};
use crate::gitsrc::{self, Git, GitError, GitSource, EMPTY_TREE};
use crate::workspace::{DirectoryScan, ScanBudget, ScanError, ScanStatus};

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct CommitSummary {
	pub sha: String,
	pub parents: Vec<String>,
	pub author_name: String,
	pub author_email: String,
	pub author_date: String,
	pub subject: String,
}

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct GitReference {
	pub name: String,
	pub sha: String,
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryHistory {
	pub root: String,
	pub commits: Vec<CommitSummary>,
	pub refs: Vec<GitReference>,
	pub head: Option<String>,
	pub has_more: bool,
}

pub fn parse_log(out: &str) -> Vec<CommitSummary> {
	out.split('\x1e')
		.filter_map(|record| {
			let mut f = record.trim_start_matches('\n').split('\0');
			Some(CommitSummary {
				sha: f.next().filter(|s| !s.is_empty())?.into(),
				parents: f
					.next()?
					.split_whitespace()
					.map(str::to_string)
					.collect(),
				author_name: f.next()?.into(),
				author_email: f.next()?.into(),
				author_date: f.next()?.into(),
				subject: f.next()?.into(),
			})
		})
		.collect()
}

/// Hard upper bound on stdout captured during commit history listing (16 MiB).
pub const MAX_GRAPH_BYTES: usize = 16 * 1024 * 1024;
/// Hard upper bound on commit records requested in a single history query.
pub const MAX_HISTORY_LIMIT: usize = 10_000;

/// Topological pages across every local ref, including remote-tracking branches
/// and annotated tags. Browsing never checks out a branch or contacts a remote.
pub fn history(
	git: &Git,
	reference: Option<&str>,
	query: &str,
	skip: usize,
	limit: usize,
) -> Result<RepositoryHistory, GitError> {
	history_with(git, reference, query, skip, limit, &RunOptions::default())
}

/// Topological pages across every local ref under explicit runner options.
pub fn history_with(
	git: &Git,
	reference: Option<&str>,
	query: &str,
	skip: usize,
	limit: usize,
	opts: &RunOptions,
) -> Result<RepositoryHistory, GitError> {
	let limit = limit.min(MAX_HISTORY_LIMIT);
	let mut run_opts = opts.clone();
	run_opts.max_stdout = run_opts.max_stdout.min(MAX_GRAPH_BYTES);
	let raw = git.run_with(
		&[
			"for-each-ref",
			"--format=%(refname)%00%(objectname)%00%(*objectname)",
			"refs/heads",
			"refs/remotes",
			"refs/tags",
		],
		&run_opts,
	)?;
	if raw.truncated {
		return Err(GitError::OutputLimit {
			args: "for-each-ref".into(),
			limit: opts.max_stdout,
		});
	}
	let refs_str = match std::str::from_utf8(&raw.stdout) {
		Ok(s) => s,
		Err(_) => {
			return Err(GitError::Malformed(
				"for-each-ref output not utf-8".into(),
			))
		}
	};
	let refs = refs_str
		.lines()
		.filter_map(|line| {
			let mut fields = line.split('\0');
			let name = fields.next()?.to_string();
			let object = fields.next()?;
			let peeled = fields.next()?;
			Some(GitReference {
				name,
				sha: if peeled.is_empty() { object } else { peeled }.into(),
			})
		})
		.collect();
	let mut args = vec![
		"log".to_string(),
		"--topo-order".into(),
		"--ignore-missing".into(),
		format!("--skip={skip}"),
		format!("-n{}", limit.saturating_add(1)),
		"--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e".into(),
	];
	let query = query.trim();
	let resolved_query =
		if query.len() >= 4 && query.bytes().all(|b| b.is_ascii_hexdigit()) {
			// Not a commit: search messages for it instead. A failing git
			// is still an error.
			match git.resolve_commit_with(query, opts) {
				Ok(sha) => Some(sha),
				Err(GitError::InvalidRevision(_)) => None,
				Err(e) => return Err(e),
			}
		} else {
			None
		};
	if let Some(sha) = resolved_query {
		args.push(sha);
	} else {
		if let Some(reference) = reference.filter(|s| !s.is_empty()) {
			args.push(git.resolve_commit_with(reference, opts)?);
		} else {
			args.extend(["--all".into(), "HEAD".into()]);
		}
		if !query.is_empty() {
			args.extend([
				"--fixed-strings".into(),
				"--regexp-ignore-case".into(),
				format!("--grep={query}"),
			]);
		}
	}
	args.push("--".into());
	let output = git.run_with(
		&args.iter().map(String::as_str).collect::<Vec<_>>(),
		&run_opts,
	)?;
	if output.truncated {
		return Err(GitError::OutputLimit {
			args: "log".into(),
			limit: opts.max_stdout,
		});
	}
	let log_text = match String::from_utf8(output.stdout) {
		Ok(s) => s,
		Err(_) => {
			return Err(GitError::Malformed("log output not utf-8".into()))
		}
	};
	let mut commits = parse_log(&log_text);
	let has_more = commits.len() > limit;
	commits.truncate(limit);
	Ok(RepositoryHistory {
		root: git.root().to_string_lossy().into_owned(),
		commits,
		refs,
		head: git.head_with(opts)?,
		has_more,
	})
}

/// History filtered by author, same format/parser as `history`.
pub fn history_by_author(
	git: &Git,
	author: &str,
	skip: usize,
	limit: usize,
) -> Result<(Vec<CommitSummary>, bool), GitError> {
	history_by_author_with(git, author, skip, limit, &RunOptions::default())
}

/// History filtered by author under explicit runner options.
pub fn history_by_author_with(
	git: &Git,
	author: &str,
	skip: usize,
	limit: usize,
	opts: &RunOptions,
) -> Result<(Vec<CommitSummary>, bool), GitError> {
	let limit = limit.min(MAX_HISTORY_LIMIT);
	let skip_arg = format!("--skip={skip}");
	let n_arg = format!("-n{}", limit.saturating_add(1));
	let author_arg = format!("--author={author}");
	let args = [
		"log",
		"--topo-order",
		"--ignore-missing",
		&skip_arg,
		&n_arg,
		"--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e",
		"--fixed-strings",
		"--regexp-ignore-case",
		&author_arg,
		"--all",
		"HEAD",
		"--",
	];
	let mut run_opts = opts.clone();
	run_opts.max_stdout = run_opts.max_stdout.min(MAX_GRAPH_BYTES);
	let out = git.run_with(&args, &run_opts)?;
	if out.truncated {
		return Err(GitError::OutputLimit {
			args: format!("log --author={author}"),
			limit: opts.max_stdout,
		});
	}
	let text = match String::from_utf8(out.stdout) {
		Ok(s) => s,
		Err(_) => {
			return Err(GitError::Malformed("log output not utf-8".into()))
		}
	};
	let mut commits = parse_log(&text);
	let more = commits.len() > limit;
	commits.truncate(limit);
	Ok((commits, more))
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
	pub path: String,
	pub name: String,
	pub directory: bool,
	pub symlink: bool,
}

fn inside(root: &Path, relative: &str) -> io::Result<PathBuf> {
	let rel = Path::new(relative);
	if rel.is_absolute()
		|| rel
			.components()
			.any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
	{
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"Path must stay inside the workspace",
		));
	}
	let root = dunce::canonicalize(root)?;
	let path = dunce::canonicalize(root.join(rel))?;
	if !path.starts_with(&root) {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			"Path leaves the workspace",
		));
	}
	Ok(path)
}

/// Most entries [`directory`] returns; a bigger directory is an error.
pub const DIRECTORY_LIMIT: usize = 10_000;

/// One directory at a time: large monorepos and dependency trees stay lazy.
/// Strict and bounded: it reads at most [`DIRECTORY_LIMIT`] + 2 entries
/// (room for `.git`) and a bigger directory is an error, never a partial
/// list; resumable pages are [`crate::workspace::DirectoryScan`]. A name
/// that is not UTF-8 has no string path that names it, so it is left out
/// rather than rendered lossily into a path of some other file.
pub fn directory(
	root: &Path,
	relative: &str,
) -> io::Result<Vec<DirectoryEntry>> {
	let mut scan = DirectoryScan::open(&inside(root, relative)?)?;
	let page = scan
		.next_page(&ScanBudget::visits(DIRECTORY_LIMIT + 2))
		.map_err(|e| match e {
			ScanError::Io(e) => e,
			e => io::Error::other(e.to_string()),
		})?;
	if page.status != ScanStatus::Complete
		|| page.entries.len() > DIRECTORY_LIMIT
	{
		return Err(io::Error::other(format!(
			"Directory has more than {DIRECTORY_LIMIT} entries"
		)));
	}
	Ok(page
		.entries
		.into_iter()
		.filter_map(|e| {
			let name = e.utf8_name()?.to_string();
			Some(DirectoryEntry {
				path: if relative.is_empty() {
					name.clone()
				} else {
					format!("{relative}/{name}")
				},
				name,
				directory: e.directory,
				symlink: e.symlink,
			})
		})
		.collect())
}

/// Entries kept per listed commit directory.
pub const MAX_TREE_ENTRIES: usize = 2000;
/// Hard upper bound on stdout captured during commit directory listing (8 MiB).
pub const MAX_TREE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TreeKind {
	Blob,
	Tree,
	Submodule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TreeEntry {
	/// Repo-relative path.
	pub path: String,
	pub name: String,
	pub kind: TreeKind,
}

/// One directory of a commit's tree, without checkout.
pub fn commit_directory(
	git: &Git,
	rev: &str,
	dir: &str,
	limit: usize,
) -> Result<(Vec<TreeEntry>, bool), GitError> {
	commit_directory_with(git, rev, dir, limit, &RunOptions::default())
}

/// One directory of a commit's tree under explicit runner options.
pub fn commit_directory_with(
	git: &Git,
	rev: &str,
	dir: &str,
	limit: usize,
	opts: &RunOptions,
) -> Result<(Vec<TreeEntry>, bool), GitError> {
	let sha = git.resolve_commit_with(rev, opts)?;
	let spec;
	let mut args = vec!["ls-tree", "-z", sha.as_str()];
	let clean_dir = dir.trim_matches('/');
	if !clean_dir.is_empty() {
		spec = format!("{clean_dir}/");
		args.extend(["--", spec.as_str()]);
	}
	let limit = limit.min(MAX_TREE_ENTRIES);
	let mut run_opts = opts.clone();
	run_opts.max_stdout = run_opts.max_stdout.min(MAX_TREE_BYTES);
	let out = git.run_with(&args, &run_opts)?;
	let mut entries = Vec::new();
	let mut truncated = out.truncated;

	// In `ls-tree -z`, every complete record ends with a NUL byte.
	// Any trailing bytes after the last NUL belong to an incomplete (cut) record.
	let (records_slice, has_unclosed_tail) =
		match out.stdout.iter().rposition(|&b| b == 0) {
			Some(last_nul) => {
				let unclosed = last_nul + 1 < out.stdout.len();
				(&out.stdout[..=last_nul], unclosed)
			}
			None => {
				let unclosed = !out.stdout.is_empty();
				(&[][..], unclosed)
			}
		};

	if has_unclosed_tail {
		if out.truncated {
			// Discard trailing bytes from a truncated tail; never construct identity from partial bytes.
			truncated = true;
		} else {
			return Err(GitError::Malformed(
				"ls-tree output missing terminating NUL".into(),
			));
		}
	}

	for rec in records_slice.split(|&b| b == 0).filter(|r| !r.is_empty()) {
		if entries.len() >= limit {
			truncated = true;
			break;
		}
		let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
			return Err(GitError::Malformed(
				"ls-tree record missing tab".into(),
			));
		};
		let meta = match std::str::from_utf8(&rec[..tab]) {
			Ok(m) => m,
			Err(_) => {
				return Err(GitError::Malformed(
					"ls-tree metadata not utf-8".into(),
				))
			}
		};
		let kind = match meta.split(' ').nth(1) {
			Some("tree") => TreeKind::Tree,
			Some("commit") => TreeKind::Submodule,
			_ => TreeKind::Blob,
		};
		let path_bytes = &rec[tab + 1..];
		if path_bytes.is_empty() {
			return Err(GitError::Malformed(
				"ls-tree record has empty path".into(),
			));
		}
		let Ok(path_str) = std::str::from_utf8(path_bytes) else {
			return Err(GitError::Malformed(
				"unsupported non-UTF-8 path in commit directory".into(),
			));
		};
		let path = path_str.to_string();
		let name = path.rsplit('/').next().unwrap_or(&path).to_string();
		entries.push(TreeEntry { path, name, kind });
	}

	if out.truncated && entries.is_empty() && limit > 0 {
		return Err(GitError::OutputLimit {
			args: format!("ls-tree {sha}"),
			limit: opts.max_stdout,
		});
	}

	entries.sort_by(|a, b| {
		(b.kind == TreeKind::Tree)
			.cmp(&(a.kind == TreeKind::Tree))
			.then(a.name.cmp(&b.name))
	});
	Ok((entries, truncated))
}

/// Largest blob read for preview (same as core's preview limit).
pub const MAX_BLOB_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobText {
	Text(String),
	Binary,
	TooLarge(u64),
	NotUtf8,
}

/// A file as it is in `rev`, size-checked before reading.
pub fn commit_blob(
	git: &Git,
	rev: &str,
	path: &str,
	max_bytes: u64,
) -> Result<BlobText, GitError> {
	commit_blob_with(git, rev, path, max_bytes, &RunOptions::default())
}

/// A file as it is in `rev`, size-checked before reading under explicit runner options.
pub fn commit_blob_with(
	git: &Git,
	rev: &str,
	path: &str,
	max_bytes: u64,
	opts: &RunOptions,
) -> Result<BlobText, GitError> {
	let clean_path = path.trim_start_matches('/');
	let sha = git.resolve_commit_with(rev, opts)?;
	let spec = format!("{sha}:{clean_path}");
	let size_run = git.run_with(&["cat-file", "-s", &spec], opts)?;
	if size_run.truncated {
		return Err(GitError::OutputLimit {
			args: format!("cat-file -s {spec}"),
			limit: opts.max_stdout,
		});
	}
	let size_out = size_run.stdout;
	let size: u64 = match std::str::from_utf8(&size_out) {
		Ok(s) => s
			.trim()
			.parse()
			.map_err(|_| GitError::Malformed("cat-file size".into()))?,
		Err(_) => {
			return Err(GitError::Malformed("cat-file size not utf-8".into()))
		}
	};
	let effective_max = max_bytes.min(MAX_BLOB_BYTES);
	if size > effective_max {
		return Ok(BlobText::TooLarge(size));
	}
	if size > opts.max_stdout as u64 {
		return Err(GitError::OutputLimit {
			args: format!("cat-file blob {spec}"),
			limit: opts.max_stdout,
		});
	}
	let mut read_opts = opts.clone();
	read_opts.max_stdout = read_opts.max_stdout.min(MAX_BLOB_BYTES as usize);
	let out = git.run_with(&["cat-file", "blob", &spec], &read_opts)?;
	if out.truncated {
		return Err(GitError::OutputLimit {
			args: format!("cat-file blob {spec}"),
			limit: opts.max_stdout,
		});
	}
	let bytes = out.stdout;
	if bytes.iter().take(8000).any(|&b| b == 0) {
		return Ok(BlobText::Binary);
	}
	Ok(match String::from_utf8(bytes) {
		Ok(s) => BlobText::Text(s),
		Err(_) => BlobText::NotUtf8,
	})
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SourcePreview {
	pub content: Option<String>,
	pub patch: String,
}

/// A preview whose patch may be cut ([`git_preview_with`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPreview {
	pub content: Option<String>,
	/// Whole hunks only: every hunk header counts exactly the lines kept.
	pub patch: String,
	pub patch_truncated: bool,
}

pub const PREVIEW_LIMIT: usize = 1024 * 1024;

pub fn file_preview(root: &Path, path: &str) -> io::Result<SourcePreview> {
	let mut bytes = Vec::new();
	fs::File::open(inside(root, path)?)?
		.take((PREVIEW_LIMIT + 1) as u64)
		.read_to_end(&mut bytes)?;
	if bytes.len() > PREVIEW_LIMIT {
		return Err(io::Error::other("Preview exceeds 1 MiB"));
	}
	Ok(SourcePreview {
		content: decode_utf8_or_skip(bytes),
		patch: String::new(),
	})
}

fn too_large() -> GitError {
	io::Error::other("Preview exceeds 1 MiB").into()
}

/// The desktop preview: content and patch are both strict, so a view that
/// cannot say "truncated" never shows a partial file or patch as whole.
pub fn git_preview(
	git: &Git,
	source: &GitSource,
	path: &str,
) -> Result<SourcePreview, GitError> {
	let opts = RunOptions {
		max_stdout: PREVIEW_LIMIT,
		overflow: Overflow::Error,
		..RunOptions::default()
	};
	let p = git_preview_with(git, source, path, &opts)?;
	Ok(SourcePreview {
		content: p.content,
		patch: p.patch,
	})
}

/// Preview under explicit limits (e.g. [`RunOptions::preview`]). The content
/// is always strict at `opts.max_stdout`. The patch follows
/// `opts.overflow`: `Error` refuses an oversized patch, `Truncate` keeps
/// the whole hunks that fit and sets `patch_truncated`.
pub fn git_preview_with(
	git: &Git,
	source: &GitSource,
	path: &str,
	opts: &RunOptions,
) -> Result<GitPreview, GitError> {
	let max = opts.max_stdout;
	// Verify membership before accepting a path from the WebView: the one
	// listing `read_changed_file` does answers both.
	if matches!(source, GitSource::Working) && git.root().join(path).exists() {
		inside(git.root(), path)?;
	}
	// Content is strict: a partial file would read as the whole file.
	let file = match gitsrc::read_changed_file(git, source, path, max as u64) {
		Err(GitError::OutputLimit { .. }) => return Err(too_large()),
		other => other?.ok_or_else(|| {
			GitError::Malformed("Path is not in this Git source".into())
		})?,
	};
	let mut args = vec![
		"diff".to_string(),
		"--no-ext-diff".into(),
		"--no-textconv".into(),
		"--no-color".into(),
	];
	match source {
		GitSource::Working => {
			args.push(git.head()?.unwrap_or_else(|| EMPTY_TREE.into()))
		}
		GitSource::Staged => args.push("--cached".into()),
		GitSource::Commit(rev) => {
			let sha = git.resolve_commit(rev)?;
			args.push(
				git.parents(&sha)?
					.into_iter()
					.next()
					.unwrap_or_else(|| EMPTY_TREE.into()),
			);
			args.push(sha);
		}
		GitSource::Range(base, tip) => {
			args.push(git.resolve_commit(base)?);
			args.push(git.resolve_commit(tip)?);
		}
	}
	args.extend(["--".into(), format!(":(literal){path}")]);
	let diff = match git
		.run_with(&args.iter().map(String::as_str).collect::<Vec<_>>(), opts)
	{
		Err(GitError::OutputLimit { .. }) => return Err(too_large()),
		other => other?,
	};
	let (mut patch, mut patch_truncated) = if diff.truncated {
		(whole_hunks(diff.stdout), true)
	} else {
		(diff.stdout, false)
	};
	if patch.is_empty()
		&& file.change_type == Some(crate::format::ChangeType::New)
	{
		if let Some(content) = &file.content {
			let (synth, cut) = new_file_patch(path, content, max);
			if cut && opts.overflow == Overflow::Error {
				return Err(too_large());
			}
			(patch, patch_truncated) = (synth.into_bytes(), cut);
		}
	}
	Ok(GitPreview {
		content: file.content,
		// Cut at a line start, so only an invalid byte in the file itself
		// can be replaced here.
		patch: String::from_utf8_lossy(&patch).into_owned(),
		patch_truncated,
	})
}

/// Drops the hunk the cut went through: the last one started.
fn whole_hunks(mut patch: Vec<u8>) -> Vec<u8> {
	match patch.windows(4).rposition(|w| w == b"\n@@ ") {
		Some(i) => patch.truncate(i + 1),
		None if patch.starts_with(b"@@ ") => patch.clear(),
		None => {}
	}
	patch
}

/// A new file as a unified diff, cut to whole lines within `max` bytes;
/// the hunk header counts exactly the lines kept. Returns whether it cut.
fn new_file_patch(path: &str, content: &str, max: usize) -> (String, bool) {
	let lines: Vec<&str> = content.split_inclusive('\n').collect();
	if lines.is_empty() {
		return (String::new(), false);
	}
	let head = format!("--- /dev/null\n+++ b/{path}\n");
	let mut body = String::new();
	let mut kept = 0;
	for line in &lines {
		let mut piece = format!("+{line}");
		if !line.ends_with('\n') {
			piece.push_str("\n\\ No newline at end of file\n");
		}
		let hunk = format!("@@ -0,0 +1,{} @@\n", kept + 1);
		if head.len() + hunk.len() + body.len() + piece.len() > max {
			break;
		}
		body.push_str(&piece);
		kept += 1;
	}
	let cut = kept < lines.len();
	if kept == 0 {
		return (String::new(), cut);
	}
	(format!("{head}@@ -0,0 +1,{kept} @@\n{body}"), cut)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::Command;
	fn run(root: &Path, args: &[&str]) -> String {
		let out = Command::new("git")
			.current_dir(root)
			.args(args)
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout).unwrap().trim().into()
	}
	fn commit(root: &Path, message: &str) -> String {
		run(root, &["add", "."]);
		run(
			root,
			&[
				"-c",
				"user.name=A",
				"-c",
				"user.email=a@x",
				"commit",
				"-qm",
				message,
			],
		);
		run(root, &["rev-parse", "HEAD"])
	}
	#[test]
	fn browses_unmerged_refs_pages_and_source_snapshots_without_checkout() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		let git = Git::open(root).unwrap();
		assert!(history(&git, None, "", 0, 2).unwrap().commits.is_empty());
		fs::write(root.join("file.txt"), "base\n").unwrap();
		let base = commit(root, "base");
		run(root, &["checkout", "-qb", "feature"]);
		fs::write(root.join("file.txt"), "side snapshot\n").unwrap();
		let side = commit(root, "unmerged feature");
		run(root, &["update-ref", "refs/remotes/origin/feature", &side]);
		run(
			root,
			&[
				"-c",
				"user.name=A",
				"-c",
				"user.email=a@x",
				"tag",
				"-am",
				"release",
				"v-test",
				&side,
			],
		);
		run(root, &["checkout", "-q", "main"]);
		fs::write(root.join("main.txt"), "main\n").unwrap();
		let head = commit(root, "main change");
		let full = history(&git, None, "", 0, 30).unwrap();
		assert_eq!(full.commits.len(), 3);
		assert_eq!(full.head.as_deref(), Some(head.as_str()));
		assert!(full
			.refs
			.iter()
			.any(|r| r.name == "refs/tags/v-test" && r.sha == side));
		assert!(full
			.refs
			.iter()
			.any(|r| r.name == "refs/remotes/origin/feature"));
		let first = history(&git, None, "", 0, 2).unwrap();
		assert!(first.has_more);
		let last = history(&git, None, "", 2, 2).unwrap();
		assert!(!last.has_more);
		assert_eq!(last.commits[0].sha, base);
		let filtered =
			history(&git, Some("refs/heads/feature"), "", 0, 30).unwrap();
		assert_eq!(filtered.commits.len(), 2);
		assert_eq!(filtered.commits[0].sha, side);
		assert_eq!(
			history(&git, None, "unmerged", 0, 30).unwrap().commits[0].sha,
			side
		);
		assert_eq!(
			crate::commits::select_last_from(&git, &side, 2).unwrap(),
			[base.clone(), side.clone()]
		);
		let preview =
			git_preview(&git, &GitSource::Commit(side), "file.txt").unwrap();
		assert_eq!(preview.content.as_deref(), Some("side snapshot\n"));
		assert!(
			preview.patch.contains("-base")
				&& preview.patch.contains("+side snapshot")
		);
		assert_eq!(run(root, &["rev-parse", "HEAD"]), head);
		assert_eq!(
			fs::read_to_string(root.join("file.txt")).unwrap(),
			"base\n"
		);
		assert!(git_preview(&git, &GitSource::Working, "../secret").is_err());
	}
	#[test]
	fn directory_is_lazy_and_previews_reject_escape_binary_and_large_files() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir_all(root.join("packages/api")).unwrap();
		fs::create_dir(root.join(".git")).unwrap();
		fs::write(root.join("packages/api/a.ts"), "source\n").unwrap();
		let entries = directory(root, "").unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].path, "packages");
		assert_eq!(
			directory(root, "packages/api").unwrap()[0].path,
			"packages/api/a.ts"
		);
		assert_eq!(
			file_preview(root, "packages/api/a.ts")
				.unwrap()
				.content
				.as_deref(),
			Some("source\n")
		);
		assert!(directory(root, "../").is_err());
		assert!(file_preview(root, "/etc/passwd").is_err());
		fs::write(root.join("binary"), [0, 255]).unwrap();
		assert!(file_preview(root, "binary").unwrap().content.is_none());
		fs::File::create(root.join("big"))
			.unwrap()
			.set_len((PREVIEW_LIMIT + 1) as u64)
			.unwrap();
		assert!(file_preview(root, "big").is_err());
	}
	#[test]
	fn full_listing_is_sorted_strict_and_bounded() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		for i in 0..23 {
			fs::write(root.join(format!("f{i:02}")), "").unwrap();
		}
		for d in ["zdir", "adir", "mdir"] {
			fs::create_dir(root.join(d)).unwrap();
		}
		let full: Vec<String> = directory(root, "")
			.unwrap()
			.into_iter()
			.map(|e| e.path)
			.collect();
		assert_eq!(full.len(), 26);
		assert_eq!(&full[..4], ["adir", "mdir", "zdir", "f00"]);
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn full_listing_never_turns_a_non_utf8_name_into_another_files_path() {
		use std::os::unix::ffi::OsStrExt;
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::write(root.join(std::ffi::OsStr::from_bytes(b"a\xff")), "x")
			.unwrap();
		fs::write(root.join("a\u{FFFD}"), "real").unwrap();
		let entries = directory(root, "").unwrap();
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].path, "a\u{FFFD}");
		assert_eq!(
			file_preview(root, &entries[0].path)
				.unwrap()
				.content
				.as_deref(),
			Some("real")
		);
	}

	/// Every hunk header counts exactly the lines that follow it.
	fn assert_hunks_consistent(patch: &str) {
		let mut expect: Option<(usize, usize)> = None;
		let mut seen = (0, 0);
		let mut hunks = 0;
		let check = |e: Option<(usize, usize)>, s: (usize, usize)| {
			if let Some(e) = e {
				assert_eq!(e, s, "hunk line counts");
			}
		};
		for line in patch.split_inclusive('\n') {
			// A cut line would still count; it must not exist.
			let line = line.strip_suffix('\n').expect("partial last line");
			if let Some(h) = line.strip_prefix("@@ ") {
				check(expect, seen);
				hunks += 1;
				let count = |part: &str| {
					part.split_once(',').map_or(1, |(_, n)| n.parse().unwrap())
				};
				let mut parts = h.split(' ');
				let old = count(parts.next().unwrap());
				let new = count(parts.next().unwrap());
				expect = Some((old, new));
				seen = (0, 0);
			} else if expect.is_some() {
				match line.as_bytes().first() {
					Some(b' ') => seen = (seen.0 + 1, seen.1 + 1),
					Some(b'-') => seen.0 += 1,
					Some(b'+') => seen.1 += 1,
					_ => {}
				}
			}
		}
		check(expect, seen);
		assert!(hunks > 0, "no hunks");
	}

	#[test]
	fn preview_patch_cut_keeps_whole_hunks_and_the_desktop_view_refuses() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		let lines = |every: usize| {
			(0..78_000)
				.map(|i| {
					let tag = if every > 0 && i % every == 0 {
						"new"
					} else {
						"old"
					};
					format!("{tag}-{i:08}\n")
				})
				.collect::<String>()
		};
		// ~9750 separate hunks (a change every 8 lines, 3 lines of context);
		// ~1 MB of content, a ~1.25 MB patch.
		fs::write(root.join("f.txt"), lines(0)).unwrap();
		commit(root, "old");
		fs::write(root.join("f.txt"), lines(8)).unwrap();
		let sha = commit(root, "new");
		let git = Git::open(root).unwrap();
		let source = GitSource::Commit(sha);
		let cut = git_preview_with(
			&git,
			&source,
			"f.txt",
			&RunOptions::preview(None),
		)
		.unwrap();
		assert!(cut.patch_truncated);
		assert!(cut.patch.len() <= PREVIEW_LIMIT);
		assert!(cut.patch.len() > PREVIEW_LIMIT / 2);
		assert_hunks_consistent(&cut.patch);
		assert_eq!(cut.content.as_deref(), Some(lines(8).as_str()));
		// The desktop view cannot show "truncated": it refuses instead.
		assert!(git_preview(&git, &source, "f.txt").is_err());

		fs::write(root.join("f.txt"), "x".repeat(PREVIEW_LIMIT + 1)).unwrap();
		let sha = commit(root, "huge");
		let huge = GitSource::Commit(sha);
		assert!(git_preview(&git, &huge, "f.txt").is_err());
		assert!(git_preview_with(
			&git,
			&huge,
			"f.txt",
			&RunOptions::preview(None)
		)
		.is_err());
	}

	#[test]
	fn synthesized_new_file_patch_obeys_the_limit_with_exact_counts() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		fs::write(root.join("base"), "b\n").unwrap();
		commit(root, "base");
		let git = Git::open(root).unwrap();
		// Untracked: `git diff` shows nothing, so the patch is synthesized.
		// 1 MiB of content turns into ~1.5 MiB of patch.
		fs::write(root.join("new.txt"), "a\n".repeat(PREVIEW_LIMIT / 2))
			.unwrap();
		let cut = git_preview_with(
			&git,
			&GitSource::Working,
			"new.txt",
			&RunOptions::preview(None),
		)
		.unwrap();
		assert!(cut.patch_truncated);
		assert!(cut.patch.len() <= PREVIEW_LIMIT);
		assert_hunks_consistent(&cut.patch);
		assert!(git_preview(&git, &GitSource::Working, "new.txt").is_err());

		fs::write(root.join("small.txt"), "x\ny").unwrap();
		let small =
			git_preview(&git, &GitSource::Working, "small.txt").unwrap();
		assert_eq!(
			small.patch,
			"--- /dev/null\n+++ b/small.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n\\ No newline at end of file\n"
		);
		assert_hunks_consistent(&small.patch);
	}

	#[cfg(unix)]
	#[test]
	fn resolved_workspace_accepts_root_alias_but_refuses_external_symlinks() {
		use std::os::unix::fs::symlink;
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("repo");
		fs::create_dir(&root).unwrap();
		fs::write(root.join("file"), "ok").unwrap();
		let alias = dir.path().join("alias");
		symlink(&root, &alias).unwrap();
		assert_eq!(
			file_preview(&alias, "file").unwrap().content.as_deref(),
			Some("ok")
		);
		fs::write(dir.path().join("outside"), "secret").unwrap();
		symlink(dir.path().join("outside"), root.join("escape")).unwrap();
		assert!(file_preview(&alias, "escape").is_err());
	}

	#[test]
	fn commit_directory_and_blob_match_git_without_checkout() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);

		fs::create_dir(root.join("sub")).unwrap();
		fs::write(root.join("sub/a.txt"), "hello from sub\n").unwrap();
		fs::write(root.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
		fs::write(root.join("root.txt"), "root file\n").unwrap();
		run(root, &["add", "."]);
		run(root, &["commit", "-qm", "commit one"]);
		let first = run(root, &["rev-parse", "HEAD"]);

		// Delete sub/a.txt in HEAD.
		fs::remove_file(root.join("sub/a.txt")).unwrap();
		run(root, &["add", "-A"]);
		run(root, &["commit", "-qm", "commit two"]);

		let g = Git::open(root).unwrap();

		// Directory at first commit includes 'sub', 'bin.dat', 'root.txt'
		let (entries, trunc) = commit_directory(&g, &first, "", 2000).unwrap();
		assert!(!trunc);
		// Directory should be sorted with trees first, then names
		assert_eq!(entries[0].name, "sub");
		assert_eq!(entries[0].kind, TreeKind::Tree);
		assert!(entries
			.iter()
			.any(|e| e.name == "bin.dat" && e.kind == TreeKind::Blob));
		assert!(entries
			.iter()
			.any(|e| e.name == "root.txt" && e.kind == TreeKind::Blob));

		// Subdirectory
		let (sub_entries, sub_trunc) =
			commit_directory(&g, &first, "sub", 2000).unwrap();
		assert!(!sub_trunc);
		assert_eq!(sub_entries.len(), 1);
		assert_eq!(sub_entries[0].path, "sub/a.txt");
		assert_eq!(sub_entries[0].kind, TreeKind::Blob);

		// Truncation limit
		let (limited, trunc) = commit_directory(&g, &first, "", 1).unwrap();
		assert!(trunc);
		assert_eq!(limited.len(), 1);

		// Blob reading
		match commit_blob(&g, &first, "sub/a.txt", 1024 * 1024).unwrap() {
			BlobText::Text(s) => assert_eq!(s, "hello from sub\n"),
			other => panic!("expected Text, got {other:?}"),
		}

		// Binary detection
		assert!(matches!(
			commit_blob(&g, &first, "bin.dat", 1024 * 1024).unwrap(),
			BlobText::Binary
		));

		// Size limit detection
		assert!(matches!(
			commit_blob(&g, &first, "sub/a.txt", 5).unwrap(),
			BlobText::TooLarge(_)
		));

		// Missing file in HEAD returns Err
		assert!(commit_blob(&g, "HEAD", "sub/a.txt", 1024 * 1024).is_err());
	}

	#[test]
	fn history_by_author_matches_filtered_log() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);

		for (who, msg) in [
			("Alice Smith", "feat: one"),
			("Bob Jones", "fix: two"),
			("alice smith", "refactor: three"),
		] {
			fs::write(root.join("f"), msg).unwrap();
			run(root, &["add", "."]);
			run(
				root,
				&[
					"-c",
					&format!("user.name={who}"),
					"-c",
					"user.email=dev@example.com",
					"commit",
					"-qm",
					msg,
				],
			);
		}

		let g = Git::open(root).unwrap();

		// Case-insensitive author search
		let (commits, more) = history_by_author(&g, "ALICE", 0, 10).unwrap();
		assert!(!more);
		let subjects: Vec<_> =
			commits.iter().map(|c| c.subject.as_str()).collect();
		assert_eq!(subjects, ["refactor: three", "feat: one"]);

		// Paging limit
		let (paged, more) = history_by_author(&g, "alice", 0, 1).unwrap();
		assert!(more);
		assert_eq!(paged.len(), 1);
		assert_eq!(paged[0].subject, "refactor: three");

		// Non-matching author
		let (empty, more) = history_by_author(&g, "Nobody", 0, 10).unwrap();
		assert!(!more);
		assert!(empty.is_empty());
	}

	#[test]
	fn commit_directory_invalid_raw_filename_explicit_error() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);

		let oid_raw = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["hash-object", "-w", "--stdin"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped());
			let mut child = cmd.spawn().unwrap();
			std::io::Write::write_all(
				child.stdin.as_mut().unwrap(),
				b"NON UTF8 ORIGINAL",
			)
			.unwrap();
			drop(child.stdin.take());
			let out = child.wait_with_output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};

		let mut tree_input = Vec::new();
		tree_input
			.extend_from_slice(format!("100644 blob {oid_raw}\t").as_bytes());
		tree_input.extend_from_slice(b"a\xff\0");

		let tree_oid = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["mktree", "-z"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped());
			let mut child = cmd.spawn().unwrap();
			std::io::Write::write_all(
				child.stdin.as_mut().unwrap(),
				&tree_input,
			)
			.unwrap();
			drop(child.stdin.take());
			let out = child.wait_with_output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};

		let commit_sha = run(root, &["commit-tree", &tree_oid, "-m", "init"]);
		run(root, &["update-ref", "refs/heads/main", &commit_sha]);

		let g = Git::open(root).unwrap();
		let err = commit_directory(&g, "HEAD", "", 2000).unwrap_err();
		assert!(
			matches!(
				err,
				GitError::Malformed(ref msg)
					if msg.contains("unsupported non-UTF-8 path in commit directory")
			),
			"{err:?}"
		);
	}

	#[test]
	fn commit_directory_valid_replacement_char_file_blob_works() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);

		let oid_utf8 = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["hash-object", "-w", "--stdin"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped());
			let mut child = cmd.spawn().unwrap();
			std::io::Write::write_all(
				child.stdin.as_mut().unwrap(),
				b"DIFFERENT UTF8 FILE",
			)
			.unwrap();
			drop(child.stdin.take());
			let out = child.wait_with_output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};

		let mut tree_input = Vec::new();
		tree_input
			.extend_from_slice(format!("100644 blob {oid_utf8}\t").as_bytes());
		tree_input.extend_from_slice(b"a\xef\xbf\xbd\0");

		let tree_oid = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["mktree", "-z"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped());
			let mut child = cmd.spawn().unwrap();
			std::io::Write::write_all(
				child.stdin.as_mut().unwrap(),
				&tree_input,
			)
			.unwrap();
			drop(child.stdin.take());
			let out = child.wait_with_output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};

		let commit_sha = run(root, &["commit-tree", &tree_oid, "-m", "init"]);
		run(root, &["update-ref", "refs/heads/main", &commit_sha]);

		let g = Git::open(root).unwrap();
		let (entries, trunc) = commit_directory(&g, "HEAD", "", 2000).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].name, "a\u{fffd}");
		assert_eq!(entries[0].path, "a\u{fffd}");
		assert_eq!(entries[0].kind, TreeKind::Blob);

		match commit_blob(&g, "HEAD", &entries[0].path, 1024).unwrap() {
			BlobText::Text(s) => assert_eq!(s, "DIFFERENT UTF8 FILE"),
			other => panic!("expected Text, got {other:?}"),
		}
	}

	#[test]
	fn commit_directory_valid_literal_unsupported_suffix_notes_works() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);

		fs::write(
			root.join("notes [unsupported non-UTF-8]"),
			"my normal notes\n",
		)
		.unwrap();
		run(root, &["add", "."]);
		run(root, &["commit", "-qm", "literal notes"]);

		let g = Git::open(root).unwrap();
		let (entries, trunc) = commit_directory(&g, "HEAD", "", 2000).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].name, "notes [unsupported non-UTF-8]");
		assert_eq!(entries[0].path, "notes [unsupported non-UTF-8]");
		assert_eq!(entries[0].kind, TreeKind::Blob);

		match commit_blob(&g, "HEAD", &entries[0].path, 1024).unwrap() {
			BlobText::Text(s) => assert_eq!(s, "my normal notes\n"),
			other => panic!("expected Text, got {other:?}"),
		}
	}

	#[test]
	fn tiny_budget_and_cancellation_regressions() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);
		fs::write(root.join("plain.txt"), "This content exceeds eight bytes\n")
			.unwrap();
		run(root, &["add", "."]);
		run(root, &["commit", "-qm", "initial commit"]);

		let g = Git::open(root).unwrap();

		// 1. Tiny budget (8 bytes) with Overflow::Truncate: all 3 APIs must error, not false complete
		let truncate_opts = RunOptions {
			max_stdout: 8,
			overflow: Overflow::Truncate,
			..RunOptions::default()
		};
		let blob_err =
			commit_blob_with(&g, "HEAD", "plain.txt", 1024, &truncate_opts)
				.unwrap_err();
		assert!(
			matches!(blob_err, GitError::OutputLimit { limit: 8, .. }),
			"{blob_err:?}"
		);

		let hist_err =
			history_by_author_with(&g, "Tester", 0, 10, &truncate_opts)
				.unwrap_err();
		assert!(
			matches!(hist_err, GitError::OutputLimit { limit: 8, .. }),
			"{hist_err:?}"
		);

		let dir_err = commit_directory_with(&g, "HEAD", "", 10, &truncate_opts)
			.unwrap_err();
		assert!(
			matches!(dir_err, GitError::OutputLimit { limit: 8, .. }),
			"{dir_err:?}"
		);

		// 2. Tiny budget (8 bytes) with Overflow::Error
		let error_opts = RunOptions {
			max_stdout: 8,
			overflow: Overflow::Error,
			..RunOptions::default()
		};
		assert!(commit_blob_with(&g, "HEAD", "plain.txt", 1024, &error_opts)
			.is_err());
		assert!(
			history_by_author_with(&g, "Tester", 0, 10, &error_opts).is_err()
		);
		assert!(commit_directory_with(&g, "HEAD", "", 10, &error_opts).is_err());

		// 3. Cancellation during resolution: all 3 APIs must enforce cancellation at every step
		let cancel = crate::gitrun::CancelToken::new();
		cancel.cancel();
		let cancel_opts = RunOptions {
			cancel: Some(cancel),
			..RunOptions::default()
		};

		let blob_cancel =
			commit_blob_with(&g, "HEAD", "plain.txt", 1024, &cancel_opts)
				.unwrap_err();
		assert!(
			matches!(blob_cancel, GitError::Cancelled { .. }),
			"{blob_cancel:?}"
		);

		let hist_cancel =
			history_by_author_with(&g, "Tester", 0, 10, &cancel_opts)
				.unwrap_err();
		assert!(
			matches!(hist_cancel, GitError::Cancelled { .. }),
			"{hist_cancel:?}"
		);

		let dir_cancel =
			commit_directory_with(&g, "HEAD", "", 10, &cancel_opts)
				.unwrap_err();
		assert!(
			matches!(dir_cancel, GitError::Cancelled { .. }),
			"{dir_cancel:?}"
		);
	}

	#[test]
	fn history_and_directory_limits_clamped_at_bounds() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);
		fs::write(root.join("plain.txt"), "content\n").unwrap();
		run(root, &["add", "."]);
		run(root, &["commit", "-qm", "commit msg"]);

		let g = Git::open(root).unwrap();

		// usize::MAX does not overflow command line or request unbounded log
		let (commits, more) =
			history_by_author(&g, "Tester", 0, usize::MAX).unwrap();
		assert!(!more);
		assert_eq!(commits.len(), 1);

		let (entries, trunc) =
			commit_directory(&g, "HEAD", "", usize::MAX).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 1);
	}

	#[test]
	fn commit_directory_nul_terminated_cut_within_path_and_metadata_never_invents_paths(
	) {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);
		fs::write(root.join("f1_alpha"), "alpha").unwrap();
		fs::write(root.join("f2_long_beta_path"), "beta").unwrap();
		fs::write(root.join("f3_gamma"), "gamma").unwrap();
		run(root, &["add", "."]);
		run(root, &["commit", "-qm", "three files"]);

		let g = Git::open(root).unwrap();
		let raw = Command::new("git")
			.current_dir(root)
			.args(["ls-tree", "-z", "HEAD"])
			.output()
			.unwrap()
			.stdout;

		// Find the start and end of "f2_long_beta_path" record
		let f1_nul = raw.iter().position(|&b| b == 0).unwrap();
		let f2_tab =
			raw[f1_nul + 1..].iter().position(|&b| b == b'\t').unwrap()
				+ f1_nul + 1;

		// 1. Cut in the middle of filename "f2_long_beta_path":
		// Must discard the cut record and return only f1_alpha with truncated = true
		let cut_mid_name = RunOptions {
			max_stdout: f2_tab + 6,
			overflow: Overflow::Truncate,
			..RunOptions::default()
		};
		let (entries, trunc) =
			commit_directory_with(&g, "HEAD", "", 2000, &cut_mid_name).unwrap();
		assert!(trunc);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].name, "f1_alpha");

		// 2. Cut in the middle of metadata of f2 (e.g. before the tab):
		let cut_meta = RunOptions {
			max_stdout: f1_nul + 15,
			overflow: Overflow::Truncate,
			..RunOptions::default()
		};
		let (entries, trunc) =
			commit_directory_with(&g, "HEAD", "", 2000, &cut_meta).unwrap();
		assert!(trunc);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].name, "f1_alpha");

		// 3. Cut before first entry finishes:
		let cut_first = RunOptions {
			max_stdout: 15,
			overflow: Overflow::Truncate,
			..RunOptions::default()
		};
		let err = commit_directory_with(&g, "HEAD", "", 2000, &cut_first)
			.unwrap_err();
		assert!(matches!(err, GitError::OutputLimit { .. }), "{err:?}");

		// 4. cat-file size metadata cut (e.g. 1 byte limit):
		let cut_size = RunOptions {
			max_stdout: 1,
			overflow: Overflow::Truncate,
			..RunOptions::default()
		};
		let err = commit_blob_with(&g, "HEAD", "f1_alpha", 1024, &cut_size)
			.unwrap_err();
		assert!(matches!(err, GitError::OutputLimit { .. }), "{err:?}");
	}

	#[test]
	fn commit_directory_path_with_escaped_newlines_preserved() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);

		let blob_oid = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["hash-object", "-w", "--stdin"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped());
			let mut child = cmd.spawn().unwrap();
			std::io::Write::write_all(
				child.stdin.as_mut().unwrap(),
				b"newline content\n",
			)
			.unwrap();
			drop(child.stdin.take());
			let out = child.wait_with_output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};

		let mut tree_input = Vec::new();
		tree_input
			.extend_from_slice(format!("100644 blob {blob_oid}\t").as_bytes());
		tree_input.extend_from_slice(b"file_with\nnewline.txt\0");

		let tree_oid = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["mktree", "-z"])
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped());
			let mut child = cmd.spawn().unwrap();
			std::io::Write::write_all(
				child.stdin.as_mut().unwrap(),
				&tree_input,
			)
			.unwrap();
			drop(child.stdin.take());
			let out = child.wait_with_output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};

		let commit_sha =
			run(root, &["commit-tree", &tree_oid, "-m", "newline file"]);
		run(root, &["update-ref", "refs/heads/main", &commit_sha]);

		let g = Git::open(root).unwrap();
		let (entries, trunc) = commit_directory(&g, "HEAD", "", 2000).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].name, "file_with\nnewline.txt");
		assert_eq!(entries[0].path, "file_with\nnewline.txt");

		match commit_blob(&g, "HEAD", &entries[0].path, 1024).unwrap() {
			BlobText::Text(s) => assert_eq!(s, "newline content\n"),
			other => panic!("expected Text, got {other:?}"),
		}
	}

	#[test]
	fn commit_directory_zero_and_count_limits() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run(root, &["init", "-q", "-b", "main"]);
		run(root, &["config", "user.name", "Tester"]);
		run(root, &["config", "user.email", "t@example.com"]);
		fs::write(root.join("f1"), "1").unwrap();
		fs::write(root.join("f2"), "2").unwrap();
		fs::write(root.join("f3"), "3").unwrap();
		run(root, &["add", "."]);
		run(root, &["commit", "-qm", "three files"]);

		let g = Git::open(root).unwrap();
		let opts = RunOptions::default();

		// limit = 0 on non-empty tree returns empty entries with truncated = true
		let (entries, trunc) =
			commit_directory_with(&g, "HEAD", "", 0, &opts).unwrap();
		assert!(trunc);
		assert_eq!(entries.len(), 0);

		// limit = 1 returns 1 entry with truncated = true
		let (entries, trunc) =
			commit_directory_with(&g, "HEAD", "", 1, &opts).unwrap();
		assert!(trunc);
		assert_eq!(entries.len(), 1);

		// limit = 2 returns 2 entries with truncated = true
		let (entries, trunc) =
			commit_directory_with(&g, "HEAD", "", 2, &opts).unwrap();
		assert!(trunc);
		assert_eq!(entries.len(), 2);

		// limit = 3 returns all 3 entries with truncated = false
		let (entries, trunc) =
			commit_directory_with(&g, "HEAD", "", 3, &opts).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 3);

		// Empty commit tree:
		let empty_tree = {
			let mut cmd = Command::new("git");
			cmd.current_dir(root)
				.args(["mktree"])
				.stdin(std::process::Stdio::null())
				.stdout(std::process::Stdio::piped());
			let out = cmd.output().unwrap();
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};
		let empty_commit =
			run(root, &["commit-tree", &empty_tree, "-m", "empty"]);

		// limit = 0 on empty tree returns empty entries with truncated = false
		let (entries, trunc) =
			commit_directory_with(&g, &empty_commit, "", 0, &opts).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 0);

		// limit = 10 on empty tree returns empty entries with truncated = false
		let (entries, trunc) =
			commit_directory_with(&g, &empty_commit, "", 10, &opts).unwrap();
		assert!(!trunc);
		assert_eq!(entries.len(), 0);
	}
}
