//! Read-only workspace navigation and history for the desktop workbench.
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use ts_rs::TS;

use crate::fsutil::decode_utf8_or_skip;
use crate::gitsrc::{self, Git, GitError, GitSource, EMPTY_TREE};

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

/// Topological pages across every local ref, including remote-tracking branches
/// and annotated tags. Browsing never checks out a branch or contacts a remote.
pub fn history(
	git: &Git,
	reference: Option<&str>,
	query: &str,
	skip: usize,
	limit: usize,
) -> Result<RepositoryHistory, GitError> {
	let raw = git.run(&[
		"for-each-ref",
		"--format=%(refname)%00%(objectname)%00%(*objectname)",
		"refs/heads",
		"refs/remotes",
		"refs/tags",
	])?;
	let refs = String::from_utf8_lossy(&raw)
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
		format!("-n{}", limit + 1),
		"--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x1e".into(),
	];
	let query = query.trim();
	let resolved_query =
		if query.len() >= 4 && query.bytes().all(|b| b.is_ascii_hexdigit()) {
			git.resolve_commit(query).ok()
		} else {
			None
		};
	if let Some(sha) = resolved_query {
		args.push(sha);
	} else {
		if let Some(reference) = reference.filter(|s| !s.is_empty()) {
			args.push(git.resolve_commit(reference)?);
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
	let output =
		git.run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
	let mut commits = parse_log(&String::from_utf8_lossy(&output));
	let has_more = commits.len() > limit;
	commits.truncate(limit);
	Ok(RepositoryHistory {
		root: git.root().to_string_lossy().into_owned(),
		commits,
		refs,
		head: git.resolve_commit("HEAD").ok(),
		has_more,
	})
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

/// One directory at a time: large monorepos and dependency trees stay lazy.
pub fn directory(
	root: &Path,
	relative: &str,
) -> io::Result<Vec<DirectoryEntry>> {
	let mut entries = Vec::new();
	for entry in fs::read_dir(inside(root, relative)?)? {
		let entry = entry?;
		let name = entry.file_name().to_string_lossy().into_owned();
		if name == ".git" {
			continue;
		}
		let kind = entry.file_type()?;
		entries.push(DirectoryEntry {
			path: if relative.is_empty() {
				name.clone()
			} else {
				format!("{relative}/{name}")
			},
			name,
			directory: kind.is_dir(),
			symlink: kind.is_symlink(),
		});
	}
	entries.sort_by(|a, b| {
		b.directory.cmp(&a.directory).then(a.name.cmp(&b.name))
	});
	Ok(entries)
}

#[derive(Debug, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SourcePreview {
	pub content: Option<String>,
	pub patch: String,
}

const PREVIEW_LIMIT: usize = 1024 * 1024;

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

pub fn git_preview(
	git: &Git,
	source: &GitSource,
	path: &str,
) -> Result<SourcePreview, GitError> {
	// Verify membership before accepting a path from the WebView.
	if !gitsrc::list_changed_paths(git, source)?
		.iter()
		.any(|(p, _)| p == path)
	{
		return Err(GitError::Malformed(
			"Path is not in this Git source".into(),
		));
	}
	if matches!(source, GitSource::Working) && git.root().join(path).exists() {
		inside(git.root(), path)?;
	}
	let file = gitsrc::read_changed_file(git, source, path)?
		.ok_or_else(|| GitError::Malformed("Path disappeared".into()))?;
	if file
		.content
		.as_ref()
		.is_some_and(|s| s.len() > PREVIEW_LIMIT)
	{
		return Err(io::Error::other("Preview exceeds 1 MiB").into());
	}
	let mut args = vec![
		"diff".to_string(),
		"--no-ext-diff".into(),
		"--no-textconv".into(),
		"--no-color".into(),
	];
	match source {
		GitSource::Working => args.push(
			git.resolve_commit("HEAD")
				.unwrap_or_else(|_| EMPTY_TREE.into()),
		),
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
	let mut patch = String::from_utf8_lossy(
		&git.run(&args.iter().map(String::as_str).collect::<Vec<_>>())?,
	)
	.into_owned();
	if patch.is_empty()
		&& file.change_type == Some(crate::format::ChangeType::New)
	{
		if let Some(content) = &file.content {
			patch = similar::TextDiff::from_lines("", content)
				.unified_diff()
				.header("/dev/null", &format!("b/{path}"))
				.to_string();
		}
	}
	Ok(SourcePreview {
		content: file.content,
		patch,
	})
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
}
