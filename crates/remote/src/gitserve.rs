//! Git view request handling on the worker.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use snip_core::browser::{MAX_BLOB_BYTES, MAX_TREE_ENTRIES, PREVIEW_LIMIT};
use snip_core::gitrun::{CancelToken, GitPool};
use snip_core::gitsrc::{GitError, GitSource};
use snip_core::gitview::{
	LocalRepo, Read, ReadProfile, RepoView, StatusSummary, MAX_CHANGE_ROWS,
	MAX_COMMIT_FILES, SERVED_MAX_STDOUT,
};
use snip_core::workspace::{RepoIdentity, ScanBudget};

use crate::proto::{
	valid_log_query, valid_rel_path, valid_rev, valid_tips, ErrorCode,
	GitQuery, GitReply, RepoScan, Response, ScannedRepo, REMOTE_MAX_LOG_LIMIT,
};
use crate::worker::{io_error, SharedRoot};

/// Answers a ScanRepos request within the shared root.
pub(crate) fn scan(
	root: &SharedRoot,
	under: Option<&str>,
	cancel: &CancelToken,
	scan_deadline: Duration,
) -> Response {
	let under_dir = match under {
		None | Some("") => None,
		Some(u) => {
			if !valid_rel_path(u, false) {
				return Response::Error {
					code: ErrorCode::BadRequest,
					message: "invalid folder".into(),
				};
			}
			let resolved = match snip_core::browser::inside(&root.path, u) {
				Ok(p) => p,
				Err(err) => return io_error(err),
			};
			if !resolved.is_dir() {
				return Response::Error {
					code: ErrorCode::NotFound,
					message: "not a directory".into(),
				};
			}
			Some(resolved)
		}
	};

	let mut opts = ReadProfile::Interactive.options(Some(cancel.clone()));
	opts.pool = GitPool::Served;
	opts.max_stdout = opts.max_stdout.min(SERVED_MAX_STDOUT);

	// Reserve 5s for network write / cleanup when scan_deadline > 6s;
	// for short test deadlines, clamp to half the deadline to avoid underflow.
	let reserve = Duration::from_secs(5);
	let budget_duration = if scan_deadline > Duration::from_secs(6) {
		scan_deadline.saturating_sub(reserve)
	} else {
		scan_deadline / 2
	};

	let budget = ScanBudget {
		max_visited: 200_000,
		deadline: Some(Instant::now() + budget_duration),
		cancel: Some(cancel.clone()),
	};

	let ws_scan = snip_core::gitview::scan_repos_within(
		&root.path,
		under_dir.as_deref(),
		8,
		256,
		&budget,
		&opts,
	);

	let canonical_share =
		dunce::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone());

	let mut repos = Vec::with_capacity(ws_scan.repos.len());
	let mut errors = Vec::with_capacity(ws_scan.errors.len());

	for r in ws_scan.repos {
		let r_root = dunce::canonicalize(&r.root).unwrap_or(r.root);
		match r_root.strip_prefix(&canonical_share) {
			Ok(rel_path) => {
				let utf8 = rel_path.to_str().is_some();
				let rel = if rel_path.as_os_str().is_empty() {
					"".to_string()
				} else {
					rel_path
						.components()
						.map(|c| c.as_os_str().to_string_lossy())
						.collect::<Vec<_>>()
						.join("/")
				};
				let summary = match r.summary {
					Ok(s) => Ok(StatusSummary {
						head: s.head,
						branch: s.branch,
						changes: s.changes,
					}),
					Err(msg) => Err(scrub(&msg, &root.path)),
				};
				repos.push(ScannedRepo {
					rel,
					utf8,
					name: r.name,
					kind: r.kind,
					summary,
				});
			}
			Err(_) => {
				// Repo root is not under the share root: drop it and add an error row
				errors.push((
					".".to_string(),
					scrub(
						"repository root is outside the shared folder",
						&root.path,
					),
				));
			}
		}
	}

	for (err_path, err_msg) in ws_scan.errors {
		let canon_path = dunce::canonicalize(&err_path).unwrap_or(err_path);
		let rel = match canon_path.strip_prefix(&canonical_share) {
			Ok(rel_path) => {
				if rel_path.as_os_str().is_empty() {
					".".to_string()
				} else {
					rel_path
						.components()
						.map(|c| c.as_os_str().to_string_lossy())
						.collect::<Vec<_>>()
						.join("/")
				}
			}
			Err(_) => ".".to_string(),
		};
		errors.push((rel, scrub(&err_msg, &root.path)));
	}

	let depth_limited: Vec<String> = ws_scan
		.depth_limited
		.into_iter()
		.map(|p| {
			let canon = dunce::canonicalize(&p).unwrap_or(p);
			match canon.strip_prefix(&canonical_share) {
				Ok(rel_path) => {
					if rel_path.as_os_str().is_empty() {
						".".to_string()
					} else {
						rel_path
							.components()
							.map(|c| c.as_os_str().to_string_lossy())
							.collect::<Vec<_>>()
							.join("/")
					}
				}
				Err(_) => ".".to_string(),
			}
		})
		.collect();

	Response::Repos(RepoScan {
		repos,
		errors,
		error_overflow: ws_scan.error_overflow,
		depth_limited,
		depth_overflow: ws_scan.depth_overflow,
		status: ws_scan.status,
	})
}

/// Replaces the share's absolute path (both raw and canonical) with share-relative
/// form, and replaces any remaining whitespace/quote delimited absolute path tokens
/// with `<outside the share>`.
pub(crate) fn scrub(msg: &str, share: &Path) -> String {
	let share_str = share.to_string_lossy();
	let share_canon = dunce::canonicalize(share)
		.map(|p| p.to_string_lossy().into_owned())
		.unwrap_or_else(|_| share_str.clone().into_owned());

	let mut candidates = Vec::new();
	let s1 = share_canon.trim_end_matches(['/', '\\']).to_string();
	if !s1.is_empty() {
		candidates.push(s1);
	}
	let s2 = share_str.trim_end_matches(['/', '\\']).to_string();
	if !s2.is_empty() && !candidates.contains(&s2) {
		candidates.push(s2);
	}
	candidates.sort_by_key(|a| std::cmp::Reverse(a.len()));

	let mut result = msg.to_string();
	for c in candidates {
		result = result.replace(&format!("{c}/"), "");
		result = result.replace(&format!("{c}\\"), "");
		result = result.replace(&c, ".");
	}

	scrub_outside_paths(&result)
}

fn is_delim(c: char) -> bool {
	c.is_whitespace() || c == '"' || c == '\'' || c == '`'
}

fn is_absolute_path(s: &str) -> bool {
	if s.starts_with('/') {
		return true;
	}
	let bytes = s.as_bytes();
	if bytes.len() >= 3
		&& bytes[0].is_ascii_alphabetic()
		&& bytes[1] == b':'
		&& (bytes[2] == b'\\' || bytes[2] == b'/')
	{
		return true;
	}
	false
}

fn scrub_outside_paths(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut chars = s.char_indices().peekable();

	while let Some(&(idx, ch)) = chars.peek() {
		if is_delim(ch) {
			out.push(ch);
			chars.next();
		} else {
			let start = idx;
			let mut end = idx + ch.len_utf8();
			chars.next();
			while let Some(&(next_idx, next_ch)) = chars.peek() {
				if is_delim(next_ch) {
					break;
				}
				end = next_idx + next_ch.len_utf8();
				chars.next();
			}
			let token = &s[start..end];
			let trimmed = token.trim_end_matches(|c| {
				matches!(c, ':' | ',' | '.' | ';' | ')' | ']' | '}')
			});
			if is_absolute_path(trimmed) {
				out.push_str("<outside the share>");
				out.push_str(&token[trimmed.len()..]);
			} else {
				out.push_str(token);
			}
		}
	}
	out
}

/// Cache of verified repository identities within shared workspaces.
pub(crate) struct RepoCache {
	entries: Vec<((String, String), (RepoIdentity, Instant))>,
	pub ttl: Duration,
	pub capacity: usize,
}

impl RepoCache {
	pub fn new() -> Self {
		Self::with_ttl_and_cap(Duration::from_secs(10), 256)
	}

	pub fn with_ttl_and_cap(ttl: Duration, capacity: usize) -> Self {
		Self {
			entries: Vec::new(),
			ttl,
			capacity,
		}
	}

	pub fn get(&mut self, ws: &str, repo: &str) -> Option<RepoIdentity> {
		self.get_at(ws, repo, Instant::now())
	}

	pub fn get_at(
		&mut self,
		ws: &str,
		repo: &str,
		now: Instant,
	) -> Option<RepoIdentity> {
		self.evict_expired(now);
		if let Some(pos) = self
			.entries
			.iter()
			.position(|((w, r), _)| w == ws && r == repo)
		{
			let (key, (identity, instant)) = self.entries.remove(pos);
			self.entries.push((key, (identity.clone(), instant)));
			Some(identity)
		} else {
			None
		}
	}

	pub fn insert(&mut self, ws: String, repo: String, identity: RepoIdentity) {
		self.insert_at(ws, repo, identity, Instant::now());
	}

	pub fn insert_at(
		&mut self,
		ws: String,
		repo: String,
		identity: RepoIdentity,
		now: Instant,
	) {
		self.evict_expired(now);
		if let Some(pos) = self
			.entries
			.iter()
			.position(|((w, r), _)| w == &ws && r == &repo)
		{
			self.entries.remove(pos);
		}
		while self.entries.len() >= self.capacity {
			self.entries.remove(0);
		}
		self.entries.push(((ws, repo), (identity, now)));
	}

	pub fn clear(&mut self) {
		self.entries.clear();
	}

	fn evict_expired(&mut self, now: Instant) {
		let ttl = self.ttl;
		self.entries
			.retain(|(_, (_, ts))| now.saturating_duration_since(*ts) < ttl);
	}
}

/// Pure validation of request parameters before job admission.
pub(crate) fn validate(repo: &str, q: &GitQuery) -> Result<(), String> {
	if !valid_rel_path(repo, true) {
		return Err("invalid repository path".into());
	}
	match q {
		GitQuery::ChangeList | GitQuery::Refs | GitQuery::UserEmail => Ok(()),
		GitQuery::ResolveCommit { rev } => {
			if valid_rev(rev) {
				Ok(())
			} else {
				Err("invalid revision".into())
			}
		}
		GitQuery::LogFromTips { tips, limit, .. } => {
			if !valid_tips(tips) {
				return Err("invalid tips".into());
			}
			if *limit > REMOTE_MAX_LOG_LIMIT {
				return Err("limit exceeds maximum".into());
			}
			Ok(())
		}
		GitQuery::HistoryQuery {
			reference,
			query,
			limit,
			..
		} => {
			if let Some(r) = reference {
				if !valid_rev(r) {
					return Err("invalid revision".into());
				}
			}
			if !valid_log_query(query) {
				return Err("invalid log query".into());
			}
			if *limit > REMOTE_MAX_LOG_LIMIT {
				return Err("limit exceeds maximum".into());
			}
			Ok(())
		}
		GitQuery::CommitDetails { sha } => {
			if valid_rev(sha) {
				Ok(())
			} else {
				Err("invalid revision".into())
			}
		}
		GitQuery::ChangedPaths { source } => validate_source(source),
		GitQuery::Preview { source, path, .. } => {
			validate_source(source)?;
			if !valid_rel_path(path, false) {
				return Err("invalid path".into());
			}
			Ok(())
		}
		GitQuery::ChangedFileText { source, path, .. } => {
			validate_source(source)?;
			if !valid_rel_path(path, false) {
				return Err("invalid path".into());
			}
			Ok(())
		}
		GitQuery::CommitDirectory { rev, dir, .. } => {
			if !valid_rev(rev) {
				return Err("invalid revision".into());
			}
			if !valid_rel_path(dir, true) {
				return Err("invalid directory path".into());
			}
			Ok(())
		}
		GitQuery::CommitBlob { rev, path, .. } => {
			if !valid_rev(rev) {
				return Err("invalid revision".into());
			}
			if !valid_rel_path(path, false) {
				return Err("invalid path".into());
			}
			Ok(())
		}
	}
}

fn validate_source(source: &GitSource) -> Result<(), String> {
	match source {
		GitSource::Working | GitSource::Staged => Ok(()),
		GitSource::Commit(r) => {
			if valid_rev(r) {
				Ok(())
			} else {
				Err("invalid revision".into())
			}
		}
		GitSource::Range(a, b) => {
			if valid_rev(a) && valid_rev(b) {
				Ok(())
			} else {
				Err("invalid revision in range".into())
			}
		}
	}
}

fn resolve_source(
	source: &GitSource,
	view: &LocalRepo,
	read: &Read,
) -> Result<GitSource, GitError> {
	match source {
		GitSource::Working => Ok(GitSource::Working),
		GitSource::Staged => Ok(GitSource::Staged),
		GitSource::Commit(r) => {
			let sha = view.resolve_commit(r, read)?;
			Ok(GitSource::Commit(sha))
		}
		GitSource::Range(a, b) => {
			let sha_a = view.resolve_commit(a, read)?;
			let sha_b = view.resolve_commit(b, read)?;
			Ok(GitSource::Range(sha_a, sha_b))
		}
	}
}

/// Dispatches query to the underlying LocalRepo view.
pub(crate) fn answer(
	view: &LocalRepo,
	q: GitQuery,
	read: &Read,
) -> Result<GitReply, GitError> {
	match q {
		GitQuery::ChangeList => {
			let list = view.change_list(MAX_CHANGE_ROWS, read)?;
			Ok(GitReply::ChangeList(list))
		}
		GitQuery::Refs => {
			let refs = view.refs(read)?;
			Ok(GitReply::Refs(refs))
		}
		GitQuery::ResolveCommit { rev } => {
			let sha = view.resolve_commit(&rev, read)?;
			Ok(GitReply::Commit(sha))
		}
		GitQuery::LogFromTips { tips, skip, limit } => {
			let (commits, more) =
				view.log_from_tips(&tips, skip, limit, read)?;
			Ok(GitReply::Log { commits, more })
		}
		GitQuery::HistoryQuery {
			reference,
			query,
			skip,
			limit,
		} => {
			let ref_resolved = match reference {
				Some(r) => Some(view.resolve_commit(&r, read)?),
				None => None,
			};
			let (commits, more) = view.history_query(
				ref_resolved.as_deref(),
				&query,
				skip,
				limit,
				read,
			)?;
			Ok(GitReply::Log { commits, more })
		}
		GitQuery::CommitDetails { sha } => {
			let resolved = view.resolve_commit(&sha, read)?;
			let details = view.commit_details(&resolved, read)?;
			Ok(GitReply::Details(details))
		}
		GitQuery::UserEmail => Ok(GitReply::UserEmail(view.user_email(read))),
		GitQuery::ChangedPaths { source } => {
			let source = resolve_source(&source, view, read)?;
			let paths = view.changed_paths(&source, MAX_COMMIT_FILES, read)?;
			Ok(GitReply::ChangedPaths(paths))
		}
		GitQuery::Preview {
			source,
			path,
			change,
		} => {
			let source = resolve_source(&source, view, read)?;
			let preview =
				view.preview(&source, &path, change.map(|c| (c, None)), read)?;
			Ok(GitReply::Preview(preview))
		}
		GitQuery::ChangedFileText { source, path, max } => {
			let source = resolve_source(&source, view, read)?;
			let max = max.min(PREVIEW_LIMIT as u64);
			let text = view.changed_file_text(&source, &path, max, read)?;
			Ok(GitReply::FileText(text))
		}
		GitQuery::CommitDirectory { rev, dir, limit } => {
			let rev = view.resolve_commit(&rev, read)?;
			let limit = limit.min(MAX_TREE_ENTRIES);
			let (entries, more) =
				view.commit_directory(&rev, &dir, limit, read)?;
			Ok(GitReply::Directory { entries, more })
		}
		GitQuery::CommitBlob { rev, path, max } => {
			let rev = view.resolve_commit(&rev, read)?;
			let max = max.min(MAX_BLOB_BYTES);
			let blob = view.commit_blob(&rev, &path, max, read)?;
			Ok(GitReply::Blob(blob))
		}
	}
}

/// Maps a GitError to protocol Response::Error, scrubbing paths and concealing stderr.
pub(crate) fn map_git_error(err: GitError, share: &Path) -> Response {
	let (code, message) = match err {
		GitError::NotARepository(_) => {
			(ErrorCode::NotARepository, "not a git repository".into())
		}
		GitError::InvalidRevision(msg) => {
			(ErrorCode::InvalidRevision, scrub(&msg, share))
		}
		GitError::Timeout { .. } => {
			(ErrorCode::Timeout, "operation timed out".into())
		}
		GitError::QueueTimeout { .. }
		| GitError::QueueFull { .. }
		| GitError::WorktreeBusy { .. } => (
			ErrorCode::Busy,
			"the worker is busy with other Git operations".into(),
		),
		GitError::Cancelled { .. } => {
			(ErrorCode::Cancelled, "cancelled".into())
		}
		GitError::OutsideBoundary { what } => {
			if what.contains("not the folder itself") {
				(ErrorCode::NotARepository, "not a git repository".into())
			} else if what.contains("linked worktree")
				|| what.contains("main repository")
			{
				(
					ErrorCode::OutsideShare,
					scrub(&format!("outside the shared folder: {what}"), share),
				)
			} else {
				(
					ErrorCode::OutsideShare,
					"this repository is outside the shared folder".into(),
				)
			}
		}
		GitError::OutputLimit { .. } => (
			ErrorCode::TooLarge,
			"the result is too large to send; narrow the request".into(),
		),
		GitError::Failed { .. } => {
			(ErrorCode::Io, "git could not read this repository".into())
		}
		other => (ErrorCode::Io, scrub(&other.to_string(), share)),
	};
	Response::Error { code, message }
}

/// Resolves repo and answers a GitView request.
pub(crate) fn handle_git_view(
	root: &SharedRoot,
	repo: &str,
	query: GitQuery,
	read: &Read,
	cache: &Mutex<RepoCache>,
) -> Response {
	let view = match open_repo(root, repo, read, cache) {
		Ok(v) => v,
		Err((code, message)) => return Response::Error { code, message },
	};
	match answer(&view, query, read) {
		Ok(reply) => Response::Git(reply),
		Err(err) => map_git_error(err, &root.path),
	}
}

fn open_repo(
	root: &SharedRoot,
	repo: &str,
	read: &Read,
	cache: &Mutex<RepoCache>,
) -> Result<LocalRepo, (ErrorCode, String)> {
	let cached_id = {
		let mut c = cache
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner);
		c.get(&root.id, repo)
	};
	if let Some(identity) = cached_id {
		return Ok(LocalRepo::known_within(&identity, &root.path));
	}

	let dir = if repo.is_empty() {
		root.path.clone()
	} else {
		match snip_core::browser::inside(&root.path, repo) {
			Ok(p) => p,
			Err(err) => {
				if err.kind() == std::io::ErrorKind::PermissionDenied
					|| err.to_string().contains("leaves the workspace")
				{
					return Err((
						ErrorCode::OutsideShare,
						"this repository is outside the shared folder".into(),
					));
				}
				return Err((
					ErrorCode::NotARepository,
					"not a git repository".into(),
				));
			}
		}
	};

	if !dir.is_dir() {
		return Err((ErrorCode::NotARepository, "not a git repository".into()));
	}

	match LocalRepo::open_within(&dir, &root.path, read) {
		Ok(view) => {
			if let Some(id) = view.identity() {
				let mut c = cache
					.lock()
					.unwrap_or_else(std::sync::PoisonError::into_inner);
				c.insert(root.id.clone(), repo.to_string(), id.clone());
			}
			Ok(view)
		}
		Err(err) => match map_git_error(err, &root.path) {
			Response::Error { code, message } => Err((code, message)),
			_ => unreachable!(),
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use snip_core::browser::LogQuery;
	use snip_core::workspace::RepoKind;
	use std::path::{Path, PathBuf};

	#[test]
	fn scrub_inside_path() {
		let share = Path::new("/var/workspace");
		let msg = "failed at /var/workspace/subdir/file.txt";
		assert_eq!(scrub(msg, share), "failed at subdir/file.txt");

		let bare = "root at /var/workspace";
		assert_eq!(scrub(bare, share), "root at .");
	}

	#[test]
	fn scrub_outside_path() {
		let share = Path::new("/var/workspace");
		let msg = "repository at /opt/secret/repo is outside";
		assert_eq!(
			scrub(msg, share),
			"repository at <outside the share> is outside"
		);
	}

	#[test]
	fn scrub_quoted_path() {
		let share = Path::new("/var/workspace");
		let msg = "cannot open '/opt/secret/repo': not found";
		assert_eq!(
			scrub(msg, share),
			"cannot open '<outside the share>': not found"
		);

		let dquote = "cannot open \"/opt/secret/repo\"";
		assert_eq!(scrub(dquote, share), "cannot open \"<outside the share>\"");
	}

	#[test]
	fn scrub_no_path() {
		let share = Path::new("/var/workspace");
		let msg = "operation failed with permission denied";
		assert_eq!(
			scrub(msg, share),
			"operation failed with permission denied"
		);
	}

	#[test]
	fn scrub_windows_looking_path() {
		let share = Path::new("C:/Workspace");
		let msg1 = r"failed at D:\Secret\repo\config: invalid";
		assert_eq!(
			scrub(msg1, share),
			"failed at <outside the share>: invalid"
		);

		let msg2 = "failed at D:/Secret/repo/config";
		assert_eq!(scrub(msg2, share), "failed at <outside the share>");
	}

	#[test]
	fn validate_table() {
		// Valid cases
		assert!(validate("", &GitQuery::ChangeList).is_ok());
		assert!(validate("sub/repo", &GitQuery::Refs).is_ok());
		assert!(
			validate("", &GitQuery::ResolveCommit { rev: "HEAD".into() })
				.is_ok()
		);
		assert!(
			validate("", &GitQuery::ResolveCommit { rev: "main".into() })
				.is_ok()
		);
		assert!(validate(
			"",
			&GitQuery::ResolveCommit {
				rev: "refs/heads/feature".into()
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::ResolveCommit {
				rev: "0123456789abcdef".into()
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::LogFromTips {
				tips: vec!["0123456789abcdef".into()],
				skip: 0,
				limit: 1000
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::CommitDetails {
				sha: "0123456789abcdef".into()
			}
		)
		.is_ok());
		assert!(validate("", &GitQuery::UserEmail).is_ok());
		assert!(validate(
			"",
			&GitQuery::ChangedPaths {
				source: GitSource::Working
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::ChangedPaths {
				source: GitSource::Staged
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::ChangedPaths {
				source: GitSource::Commit("HEAD".into())
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::ChangedPaths {
				source: GitSource::Range("main".into(), "feat".into())
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::Preview {
				source: GitSource::Working,
				path: "src/lib.rs".into(),
				change: None
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::ChangedFileText {
				source: GitSource::Working,
				path: "src/lib.rs".into(),
				max: 1024
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::CommitDirectory {
				rev: "HEAD".into(),
				dir: "src".into(),
				limit: 500
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::CommitDirectory {
				rev: "HEAD".into(),
				dir: "".into(),
				limit: 500
			}
		)
		.is_ok());
		assert!(validate(
			"",
			&GitQuery::CommitBlob {
				rev: "HEAD".into(),
				path: "src/lib.rs".into(),
				max: 4096
			}
		)
		.is_ok());

		// Bad repos
		assert!(validate("../x", &GitQuery::ChangeList).is_err());
		assert!(validate("/abs", &GitQuery::ChangeList).is_err());
		assert!(validate("-flag", &GitQuery::ChangeList).is_err());
		assert!(validate(":x", &GitQuery::ChangeList).is_err());

		// Bad revs
		let bad_revs = [
			"--output=pwned",
			":/x",
			"HEAD@{0}",
			"HEAD:a",
			"a..b",
			"../../x",
			"refs/heads/../x",
			"-x",
			"@{upstream}",
			"HEAD~1",
			"HEAD^",
		];
		for rev in bad_revs {
			assert!(validate("", &GitQuery::ResolveCommit { rev: rev.into() })
				.is_err());
			assert!(validate("", &GitQuery::CommitDetails { sha: rev.into() })
				.is_err());
			assert!(validate(
				"",
				&GitQuery::CommitDirectory {
					rev: rev.into(),
					dir: "".into(),
					limit: 10
				}
			)
			.is_err());
			assert!(validate(
				"",
				&GitQuery::CommitBlob {
					rev: rev.into(),
					path: "a.txt".into(),
					max: 100
				}
			)
			.is_err());
			assert!(validate(
				"",
				&GitQuery::ChangedPaths {
					source: GitSource::Commit(rev.into())
				}
			)
			.is_err());
			assert!(validate(
				"",
				&GitQuery::ChangedPaths {
					source: GitSource::Range(rev.into(), "main".into())
				}
			)
			.is_err());
			assert!(validate(
				"",
				&GitQuery::ChangedPaths {
					source: GitSource::Range("main".into(), rev.into())
				}
			)
			.is_err());
		}

		// Bad paths
		let bad_paths = ["../x", "/abs", "-x", ":x", "", "a\0b"];
		for p in bad_paths {
			assert!(validate(
				"",
				&GitQuery::Preview {
					source: GitSource::Working,
					path: p.into(),
					change: None
				}
			)
			.is_err());
			assert!(validate(
				"",
				&GitQuery::ChangedFileText {
					source: GitSource::Working,
					path: p.into(),
					max: 100
				}
			)
			.is_err());
			assert!(validate(
				"",
				&GitQuery::CommitBlob {
					rev: "HEAD".into(),
					path: p.into(),
					max: 100
				}
			)
			.is_err());
		}

		// Bad limits
		assert!(validate(
			"",
			&GitQuery::LogFromTips {
				tips: vec![],
				skip: 0,
				limit: 1001
			}
		)
		.is_err());
		assert!(validate(
			"",
			&GitQuery::HistoryQuery {
				reference: None,
				query: LogQuery {
					text: "".into(),
					regex: false,
					match_case: false,
					author: None,
					since: None,
					until: None,
					paths: vec![]
				},
				skip: 0,
				limit: 1001
			}
		)
		.is_err());

		// Bad tips
		assert!(validate(
			"",
			&GitQuery::LogFromTips {
				tips: vec!["not-hex".into()],
				skip: 0,
				limit: 10
			}
		)
		.is_err());
		assert!(validate(
			"",
			&GitQuery::LogFromTips {
				tips: vec!["abcd".into(); REMOTE_MAX_LOG_LIMIT + 50_000],
				skip: 0,
				limit: 10
			}
		)
		.is_err());
	}

	#[test]
	fn map_git_error_table() {
		let share = Path::new("/workspace");

		// NotARepository
		let r =
			map_git_error(GitError::NotARepository(PathBuf::from("/x")), share);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::NotARepository,
				..
			}
		));

		// InvalidRevision
		let r =
			map_git_error(GitError::InvalidRevision("bad rev".into()), share);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::InvalidRevision,
				..
			}
		));

		// Timeout
		let r = map_git_error(
			GitError::Timeout {
				args: "".into(),
				secs: 10,
			},
			share,
		);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::Timeout,
				..
			}
		));

		// Busy variants
		let r1 =
			map_git_error(GitError::QueueTimeout { args: "".into() }, share);
		assert!(matches!(
			r1,
			Response::Error {
				code: ErrorCode::Busy,
				..
			}
		));
		let r2 = map_git_error(GitError::QueueFull { args: "".into() }, share);
		assert!(matches!(
			r2,
			Response::Error {
				code: ErrorCode::Busy,
				..
			}
		));
		let r3 =
			map_git_error(GitError::WorktreeBusy { args: "".into() }, share);
		assert!(matches!(
			r3,
			Response::Error {
				code: ErrorCode::Busy,
				..
			}
		));

		// Cancelled
		let r = map_git_error(GitError::Cancelled { args: "".into() }, share);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::Cancelled,
				..
			}
		));

		// OutsideBoundary
		let r_wt = map_git_error(
			GitError::OutsideBoundary {
				what: "main repository of this linked worktree (share the folder that holds the main repository)",
			},
			share,
		);
		assert!(matches!(
			r_wt,
			Response::Error {
				code: ErrorCode::OutsideShare,
				..
			}
		));

		let r_norm = map_git_error(
			GitError::OutsideBoundary {
				what: "object store",
			},
			share,
		);
		assert!(matches!(
			r_norm,
			Response::Error {
				code: ErrorCode::OutsideShare,
				..
			}
		));

		let r_parent = map_git_error(
			GitError::OutsideBoundary {
				what: "repository (it is not the folder itself)",
			},
			share,
		);
		assert!(matches!(
			r_parent,
			Response::Error {
				code: ErrorCode::NotARepository,
				..
			}
		));

		// OutputLimit
		let r = map_git_error(
			GitError::OutputLimit {
				args: "".into(),
				limit: 100,
			},
			share,
		);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::TooLarge,
				..
			}
		));

		// Failed
		let r = map_git_error(
			GitError::Failed {
				args: "status".into(),
				stderr: "fatal: /outside/secret error".into(),
				code: Some(1),
			},
			share,
		);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::Io,
				..
			}
		));

		// Host
		let r = map_git_error(GitError::Host("host error".into()), share);
		assert!(matches!(
			r,
			Response::Error {
				code: ErrorCode::Io,
				..
			}
		));
	}

	#[test]
	fn repo_cache_expiry_cap_clear() {
		let mut cache =
			RepoCache::with_ttl_and_cap(Duration::from_millis(50), 3);
		let dummy_id = |s: &str| RepoIdentity {
			toplevel: PathBuf::from(s),
			git_dir: PathBuf::from(s).join(".git"),
			common_dir: PathBuf::from(s).join(".git"),
			kind: RepoKind::Main,
		};

		let t0 = Instant::now();
		cache.insert_at("ws".into(), "r1".into(), dummy_id("r1"), t0);
		cache.insert_at("ws".into(), "r2".into(), dummy_id("r2"), t0);

		// Hit
		assert_eq!(
			cache.get_at("ws", "r1", t0).map(|i| i.toplevel),
			Some(PathBuf::from("r1"))
		);

		// Expiry after ttl
		let t1 = t0 + Duration::from_millis(60);
		assert!(cache.get_at("ws", "r1", t1).is_none());

		// Capacity of 3: insert r1, r2, r3, r4 -> r1 evicted as oldest
		let t2 = Instant::now();
		cache.insert_at("ws".into(), "a".into(), dummy_id("a"), t2);
		cache.insert_at("ws".into(), "b".into(), dummy_id("b"), t2);
		cache.insert_at("ws".into(), "c".into(), dummy_id("c"), t2);
		cache.insert_at("ws".into(), "d".into(), dummy_id("d"), t2);

		assert_eq!(cache.entries.len(), 3);
		assert!(cache.get_at("ws", "a", t2).is_none());
		assert!(cache.get_at("ws", "b", t2).is_some());
		assert!(cache.get_at("ws", "c", t2).is_some());
		assert!(cache.get_at("ws", "d", t2).is_some());

		// Clear
		cache.clear();
		assert!(cache.get_at("ws", "b", t2).is_none());
		assert_eq!(cache.entries.len(), 0);
	}

	#[test]
	fn failed_stderr_never_forwarded() {
		let err = GitError::Failed {
			args: "rev-parse".into(),
			stderr: "secret leaked from /opt/private/key".into(),
			code: Some(128),
		};
		let resp = map_git_error(err, Path::new("/var/workspace"));
		let Response::Error { code, message } = resp else {
			panic!("expected Error response");
		};
		assert_eq!(code, ErrorCode::Io);
		assert_eq!(message, "git could not read this repository");
		assert!(!message.contains("/opt/private/key"));
	}
}
