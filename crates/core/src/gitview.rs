//! Git inspection, change listing, commit details and repository discovery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::browser::{
	BlobText, CommitSummary, GitPreview, LogQuery, RefSnapshot, TreeEntry,
};
use crate::format::ChangeType;
use crate::gitrun::{CancelToken, GitPool, RunOptions};
use crate::gitsrc::{self, ChangedPaths, Git, GitError, GitSource};
use crate::workspace::{
	declared_submodules, status_details, summarize_with_details,
	summarize_with_identity, ChangeCounts, DiscoveredRepo, Discovery,
	RepoIdentity, RepoKind, RepoSummary, ScanBudget, ScanStatus,
	SubmoduleState,
};

/// Rows the Changes list keeps per repository (was main.rs MAX_CHANGES_PER_REPO).
pub const MAX_CHANGE_ROWS: usize = 2_000;
/// Files listed for one commit or compare (was history.rs MAX_COMMIT_FILES).
pub const MAX_COMMIT_FILES: usize = 5_000;
/// Commit message bytes kept for the details pane.
pub const MAX_DETAILS_MESSAGE: usize = 16 * 1024;
/// Branches listed as containing the selected commit; more are counted.
pub const MAX_CONTAINING_BRANCHES: usize = 20;
/// Longest `user.email` kept to mark the user's own commits.
pub const MAX_USER_EMAIL: usize = 256;

/// Clips a string at a UTF-8 character boundary and appends an ellipsis.
pub fn clip_utf8(mut s: String, max: usize) -> String {
	if s.len() > max {
		let mut end = max;
		while !s.is_char_boundary(end) {
			end -= 1;
		}
		s.truncate(end);
		s.push('…');
	}
	s.into_boxed_str().into_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSummary {
	pub head: Option<String>,
	pub branch: Option<String>,
	pub changes: ChangeCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRow {
	pub path: String,
	pub change_type: Option<ChangeType>,
	pub source: ChangeSource,
	pub conflict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeSource {
	Staged,
	Unstaged,
	Working,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeList {
	/// Present when the repository's identity was known (one status feeds both).
	pub summary: Option<StatusSummary>,
	/// Sorted by path, at most `max_rows`.
	pub rows: Vec<ChangeRow>,
	/// Rows git reported, before the cap. `total > rows.len()` = truncated.
	pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPathList {
	pub paths: ChangedPaths,
	pub gitlinks: Vec<String>,
	pub total: usize,
}

/// What the details pane shows beyond the log row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetails {
	pub sha: String,
	pub parents: Vec<String>,
	pub message: String,
	pub author: String,
	pub author_email: String,
	pub author_date: String,
	pub committer: String,
	pub committer_email: String,
	pub commit_date: String,
	pub branches: Vec<String>,
	/// More branches contain the commit than `branches` lists.
	pub branches_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FoundKind {
	Main,
	LinkedWorktree,
	Submodule,
	UninitializedSubmodule,
}

/// One repository a workspace scan found; a failed read stays an error row.
#[derive(Debug, Clone)]
pub struct FoundRepo {
	pub root: PathBuf,
	pub name: String,
	pub kind: FoundKind,
	pub identity: Option<RepoIdentity>,
	pub summary: Result<RepoSummary, String>,
}

pub fn change_list_with(
	git: &Git,
	known: Option<&RepoIdentity>,
	max_rows: usize,
	opts: &RunOptions,
) -> Result<ChangeList, GitError> {
	let (summary, details) = match known {
		Some(id) => {
			let (sum, details) = summarize_with_details(git, id, opts)?;
			(
				Some(StatusSummary {
					head: sum.head,
					branch: sum.branch,
					changes: sum.changes,
				}),
				details,
			)
		}
		None => {
			let details = status_details(git, opts)?;
			(None, details)
		}
	};
	let mut rows = Vec::new();
	for (path, change_type) in details.staged {
		rows.push(ChangeRow {
			path,
			change_type,
			source: ChangeSource::Staged,
			conflict: false,
		});
	}
	for (path, change_type) in details.unstaged {
		rows.push(ChangeRow {
			path,
			change_type,
			source: ChangeSource::Unstaged,
			conflict: false,
		});
	}
	for path in details.untracked {
		rows.push(ChangeRow {
			path,
			change_type: Some(ChangeType::New),
			source: ChangeSource::Working,
			conflict: false,
		});
	}
	for path in details.conflicted {
		rows.push(ChangeRow {
			path,
			change_type: Some(ChangeType::Modified),
			source: ChangeSource::Working,
			conflict: true,
		});
	}
	rows.sort_by(|a, b| a.path.cmp(&b.path));
	let total = rows.len();
	rows.truncate(max_rows);
	Ok(ChangeList {
		summary,
		rows,
		total,
	})
}

pub fn changed_paths_with(
	git: &Git,
	source: &GitSource,
	max: usize,
	opts: &RunOptions,
) -> Result<ChangedPathList, GitError> {
	let (mut paths, gitlinks) =
		gitsrc::list_changed_paths_and_gitlinks_with(git, source, opts)?;
	let total = paths.len();
	paths.truncate(max);
	Ok(ChangedPathList {
		paths,
		gitlinks,
		total,
	})
}

pub fn commit_details_with(
	git: &Git,
	sha: &str,
	opts: &RunOptions,
) -> Result<CommitDetails, GitError> {
	let out = git.run_with(
		&[
			"show",
			"-s",
			"--no-show-signature",
			"--encoding=UTF-8",
			"--format=%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B",
			sha,
			"--",
		],
		opts,
	)?;
	let text = String::from_utf8_lossy(&out.stdout);
	let mut f = text.splitn(8, '\0');
	let parents: Vec<String> = f
		.next()
		.unwrap_or_default()
		.split_whitespace()
		.take(64)
		.map(str::to_string)
		.collect();
	let author = f.next().unwrap_or_default().to_string();
	let author_email = f.next().unwrap_or_default().to_string();
	let author_date = f.next().unwrap_or_default().to_string();
	let committer = f.next().unwrap_or_default().to_string();
	let committer_email = f.next().unwrap_or_default().to_string();
	let commit_date = f.next().unwrap_or_default().to_string();
	let message = f.next().unwrap_or_default().trim().to_string();
	// Best effort: a failure here only hides the branch list.
	let contains = git
		.run_with(
			&[
				"for-each-ref",
				&format!("--count={}", MAX_CONTAINING_BRANCHES + 1),
				"--contains",
				sha,
				"--format=%(refname:short)",
				"refs/heads",
				"refs/remotes",
			],
			opts,
		)
		.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
		.unwrap_or_default();
	let mut branches: Vec<String> = contains
		.lines()
		.filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
		.map(|l| clip_utf8(l.to_string(), 200))
		.collect();
	let branches_more = branches.len() > MAX_CONTAINING_BRANCHES;
	branches.truncate(MAX_CONTAINING_BRANCHES);
	Ok(CommitDetails {
		sha: sha.to_string(),
		parents,
		message: clip_utf8(message, MAX_DETAILS_MESSAGE),
		author: clip_utf8(author, 200),
		author_email: clip_utf8(author_email, 200),
		author_date: clip_utf8(author_date, 64),
		committer: clip_utf8(committer, 200),
		committer_email: clip_utf8(committer_email, 200),
		commit_date: clip_utf8(commit_date, 64),
		branches,
		branches_more,
	})
}

/// `user.email`, when set, to mark the user's own commits.
pub fn user_email_with(git: &Git, opts: &RunOptions) -> Option<String> {
	let out = git
		.run_with(&["config", "--get", "user.email"], opts)
		.ok()?;
	let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
	(!email.is_empty() && email.len() <= MAX_USER_EMAIL).then_some(email)
}

fn check_alternates_recursive(
	alternates_file: &Path,
	objects_dir: &Path,
	boundary: &Path,
	depth: usize,
	visited: &mut HashSet<PathBuf>,
) -> Result<(), GitError> {
	if depth > 5 {
		return Err(GitError::OutsideBoundary {
			what: "alternate object store",
		});
	}
	let metadata = alternates_file.symlink_metadata()?;
	if metadata.len() > 64 * 1024 {
		return Err(GitError::OutsideBoundary {
			what: "alternate object store",
		});
	}
	let bytes = std::fs::read(alternates_file)?;
	if bytes.len() > 64 * 1024 {
		return Err(GitError::OutsideBoundary {
			what: "alternate object store",
		});
	}
	let content =
		std::str::from_utf8(&bytes).map_err(|_| GitError::OutsideBoundary {
			what: "alternate object store",
		})?;
	for raw_line in content.lines() {
		let line = raw_line.trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		if line.starts_with('"') {
			return Err(GitError::OutsideBoundary {
				what: "alternate object store",
			});
		}
		let alt_path = Path::new(line);
		let full_alt = if alt_path.is_relative() {
			objects_dir.join(alt_path)
		} else {
			alt_path.to_path_buf()
		};
		let canon_alt = dunce::canonicalize(&full_alt).map_err(|_| {
			GitError::OutsideBoundary {
				what: "alternate object store",
			}
		})?;
		if !canon_alt.starts_with(boundary) {
			return Err(GitError::OutsideBoundary {
				what: "alternate object store",
			});
		}
		if visited.insert(canon_alt.clone()) {
			let nested_alt_file = canon_alt.join("info/alternates");
			if nested_alt_file.symlink_metadata().is_ok() {
				check_alternates_recursive(
					&nested_alt_file,
					&canon_alt,
					boundary,
					depth + 1,
					visited,
				)?;
			}
		}
	}
	Ok(())
}

pub(crate) fn check_repo_boundary(
	git: &Git,
	boundary: &Path,
	opts: &RunOptions,
) -> Result<RepoIdentity, GitError> {
	let boundary = dunce::canonicalize(boundary)?;

	let identity = RepoIdentity::resolve(git, opts)?;
	let git_dir_inside = identity.git_dir.starts_with(&boundary);
	let common_dir_inside = identity.common_dir.starts_with(&boundary);
	if !git_dir_inside || !common_dir_inside {
		if identity.kind == RepoKind::LinkedWorktree
			|| git.root().join(".git").is_file()
		{
			return Err(GitError::OutsideBoundary {
				what: "main repository of this linked worktree (share the folder that holds the main repository)",
			});
		}
		return Err(GitError::OutsideBoundary { what: "git dir" });
	}

	let out = git.run_with(
		&[
			"rev-parse",
			"--git-path",
			"objects",
			"--git-path",
			"objects/info/alternates",
			"--git-path",
			"refs",
			"--git-path",
			"packed-refs",
			"--git-path",
			"index",
			"--git-path",
			"shallow",
			"--git-path",
			"HEAD",
			"--git-path",
			"logs",
			"--git-path",
			"config",
			"--git-path",
			"reftable",
		],
		opts,
	)?;
	if out.truncated {
		return Err(GitError::OutputLimit {
			args: "rev-parse --git-path".into(),
			limit: opts.max_stdout,
		});
	}
	let stdout = std::str::from_utf8(&out.stdout).map_err(|_| {
		GitError::Malformed("rev-parse output not utf-8".into())
	})?;
	let lines: Vec<&str> = stdout.lines().collect();
	if lines.len() != 10 {
		return Err(GitError::Malformed(
			"rev-parse --git-path output must be exactly 10 lines".into(),
		));
	}

	const PATH_WHATS: [&str; 10] = [
		"object store",
		"alternate object store",
		"refs",
		"refs",
		"index",
		"shallow file",
		"git dir",
		"git dir",
		"config",
		"refs",
	];

	for (i, line) in lines.iter().enumerate() {
		let what = PATH_WHATS[i];
		let p = Path::new(line);
		let full = if p.is_relative() {
			git.root().join(p)
		} else {
			p.to_path_buf()
		};
		if full.symlink_metadata().is_ok() {
			let canon = dunce::canonicalize(&full)
				.map_err(|_| GitError::OutsideBoundary { what })?;
			if !canon.starts_with(&boundary) {
				return Err(GitError::OutsideBoundary { what });
			}
		}
	}

	let alternates_path = {
		let p = Path::new(lines[1]);
		if p.is_relative() {
			git.root().join(p)
		} else {
			p.to_path_buf()
		}
	};
	if alternates_path.symlink_metadata().is_ok() {
		let mut visited = HashSet::new();
		let objects_dir = alternates_path
			.parent()
			.and_then(|p| p.parent())
			.unwrap_or_else(|| git.root());
		check_alternates_recursive(
			&alternates_path,
			objects_dir,
			&boundary,
			0,
			&mut visited,
		)?;
	}

	Ok(identity)
}

/// Checks `budget` (deadline, cancel) between repositories; repos it did not
/// reach are returned as error rows "not read: time limit", never dropped.
pub fn identify_repos(
	found: Vec<DiscoveredRepo>,
	boundary: Option<&Path>,
	budget: &ScanBudget,
	opts: &RunOptions,
) -> (Vec<FoundRepo>, Vec<(PathBuf, String)>) {
	let mut seen_identities: HashSet<(PathBuf, PathBuf)> = HashSet::new();
	let mut seen_roots: HashSet<PathBuf> = HashSet::new();
	let mut list = Vec::new();
	let mut errors = Vec::new();

	let canonical_boundary = boundary.and_then(|b| dunce::canonicalize(b).ok());

	let mut iter = found.into_iter().enumerate();
	while let Some((idx, r)) = iter.next() {
		if let Some(stop) = budget.stop(idx) {
			let err_msg = match stop {
				ScanStatus::Cancelled => "not read: cancelled".to_string(),
				_ => "not read: time limit".to_string(),
			};
			let name = r
				.path
				.file_name()
				.map(|n| n.to_string_lossy().into_owned())
				.unwrap_or_else(|| r.path.display().to_string());
			list.push(FoundRepo {
				root: r.path,
				name,
				kind: FoundKind::Main,
				identity: None,
				summary: Err(err_msg.clone()),
			});
			for (_, rem) in iter {
				let name = rem
					.path
					.file_name()
					.map(|n| n.to_string_lossy().into_owned())
					.unwrap_or_else(|| rem.path.display().to_string());
				list.push(FoundRepo {
					root: rem.path,
					name,
					kind: FoundKind::Main,
					identity: None,
					summary: Err(err_msg.clone()),
				});
			}
			break;
		}

		let canonical_path =
			dunce::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone());
		let name = r
			.path
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| r.path.display().to_string());

		let (git, id) = if let Some(b) = boundary {
			match Git::open_within(&r.path, b, opts).and_then(|git| {
				let id = check_repo_boundary(&git, b, opts)?;
				Ok((git, id))
			}) {
				Ok(pair) => pair,
				Err(err) => {
					if seen_roots.insert(canonical_path) {
						list.push(FoundRepo {
							root: r.path,
							name,
							kind: FoundKind::Main,
							identity: None,
							summary: Err(err.to_string()),
						});
					}
					continue;
				}
			}
		} else {
			match Git::open_with(&r.path, opts) {
				Ok(git) => match RepoIdentity::resolve(&git, opts) {
					Ok(id) => (git, id),
					Err(err) => {
						let root = git.root().to_path_buf();
						if seen_roots.insert(root.clone()) {
							list.push(FoundRepo {
								root,
								name,
								kind: FoundKind::Main,
								identity: None,
								summary: Err(format!(
									"repository identity: {err}"
								)),
							});
						}
						continue;
					}
				},
				Err(e) => {
					if seen_roots.insert(canonical_path) {
						list.push(FoundRepo {
							root: r.path,
							name,
							kind: FoundKind::Main,
							identity: None,
							summary: Err(e.to_string()),
						});
					}
					continue;
				}
			}
		};

		if !seen_identities.insert((id.toplevel.clone(), id.git_dir.clone())) {
			continue;
		}
		seen_roots.insert(canonical_path.clone());
		let kind = match id.kind {
			RepoKind::LinkedWorktree => FoundKind::LinkedWorktree,
			RepoKind::Submodule => FoundKind::Submodule,
			RepoKind::Main => FoundKind::Main,
		};
		let root = id.toplevel.clone();
		let name = root
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or(name);
		let summary =
			summarize_with_identity(&git, &id, opts).map_err(|e| e.to_string());

		list.push(FoundRepo {
			root: root.clone(),
			name,
			kind,
			identity: Some(id),
			summary,
		});

		let submodules = match declared_submodules(&git, opts) {
			Ok(submodules) => submodules,
			Err(err) => {
				errors.push((root, format!("submodules: {err}")));
				Vec::new()
			}
		};
		for subm in submodules {
			if boundary.is_some() {
				let sp = Path::new(&subm.path);
				if sp.is_absolute()
					|| sp
						.components()
						.any(|c| matches!(c, std::path::Component::ParentDir))
				{
					continue;
				}
				let full_sub = git.root().join(&subm.path);
				if full_sub.symlink_metadata().is_ok() {
					let Ok(canon_sub) = dunce::canonicalize(&full_sub) else {
						continue;
					};
					if let Some(ref cb) = canonical_boundary {
						if !canon_sub.starts_with(cb) {
							continue;
						}
					}
				}
			}
			let sub_path = git.root().join(&subm.path);
			let canonical_sub = dunce::canonicalize(&sub_path)
				.unwrap_or_else(|_| sub_path.clone());
			match subm.state {
				SubmoduleState::NotCheckedOut
					if seen_roots.insert(canonical_sub.clone()) =>
				{
					list.push(FoundRepo {
						root: sub_path,
						name: subm.name,
						kind: FoundKind::UninitializedSubmodule,
						identity: None,
						summary: Err(
							"Submodule not checked out (uninitialized)".into(),
						),
					});
				}
				SubmoduleState::Unreadable(reason)
					if seen_roots.insert(canonical_sub) =>
				{
					list.push(FoundRepo {
						root: sub_path,
						name: subm.name,
						kind: FoundKind::UninitializedSubmodule,
						identity: None,
						summary: Err(format!("Submodule unreadable: {reason}")),
					});
				}
				_ => {}
			}
		}
	}

	(list, errors)
}

pub fn identify_repo(
	path: &Path,
	boundary: Option<&Path>,
	opts: &RunOptions,
) -> FoundRepo {
	let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
	let (root, kind, identity, summary) = if let Some(b) = boundary {
		match Git::open_within(&path, b, opts).and_then(|git| {
			let id = check_repo_boundary(&git, b, opts)?;
			Ok((git, id))
		}) {
			Ok((git, id)) => {
				let kind = match id.kind {
					RepoKind::LinkedWorktree => FoundKind::LinkedWorktree,
					RepoKind::Submodule => FoundKind::Submodule,
					RepoKind::Main => FoundKind::Main,
				};
				let summary = summarize_with_identity(&git, &id, opts)
					.map_err(|err| err.to_string());
				(id.toplevel.clone(), kind, Some(id), summary)
			}
			Err(err) => {
				(path.clone(), FoundKind::Main, None, Err(err.to_string()))
			}
		}
	} else {
		match Git::open_with(&path, opts) {
			Ok(git) => {
				let root = git.root().to_path_buf();
				match RepoIdentity::resolve(&git, opts) {
					Ok(id) => {
						let kind = match id.kind {
							RepoKind::LinkedWorktree => {
								FoundKind::LinkedWorktree
							}
							RepoKind::Submodule => FoundKind::Submodule,
							RepoKind::Main => FoundKind::Main,
						};
						let summary = summarize_with_identity(&git, &id, opts)
							.map_err(|err| err.to_string());
						(id.toplevel.clone(), kind, Some(id), summary)
					}
					Err(err) => (
						root,
						FoundKind::Main,
						None,
						Err(format!("repository identity: {err}")),
					),
				}
			}
			Err(err) => {
				(path.clone(), FoundKind::Main, None, Err(err.to_string()))
			}
		}
	};
	let name = root
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_else(|| root.display().to_string());
	FoundRepo {
		root,
		name,
		kind,
		identity,
		summary,
	}
}

/// Errors and depth-limited folders a scan keeps; the rest are counted.
pub const MAX_SCAN_NOTES: usize = 64;

/// A whole scan of one shared folder, bounded by one budget (worker only).
#[derive(Debug, Clone)]
pub struct WorkspaceScan {
	pub repos: Vec<FoundRepo>,
	pub errors: Vec<(PathBuf, String)>, // <= MAX_SCAN_NOTES
	pub error_overflow: usize,
	pub depth_limited: Vec<PathBuf>, // <= MAX_SCAN_NOTES
	pub depth_overflow: usize,
	/// TimedOut/Cancelled/LimitReached/Incomplete keep what was found.
	pub status: ScanStatus,
}

/// Discovery pages + identify under ONE budget. `under`: a folder inside
/// `root` to continue a depth-limited branch from.
pub fn scan_repos_within(
	root: &Path,
	under: Option<&Path>,
	max_depth: usize,
	max_repos: usize,
	budget: &ScanBudget,
	opts: &RunOptions,
) -> WorkspaceScan {
	let canonical_root = match dunce::canonicalize(root) {
		Ok(c) => c,
		Err(e) => {
			return WorkspaceScan {
				repos: Vec::new(),
				errors: vec![(root.to_path_buf(), e.to_string())],
				error_overflow: 0,
				depth_limited: Vec::new(),
				depth_overflow: 0,
				status: ScanStatus::Incomplete,
			};
		}
	};

	let start = match under {
		Some(u) => {
			let canonical_under = match dunce::canonicalize(u) {
				Ok(c) => c,
				Err(e) => {
					return WorkspaceScan {
						repos: Vec::new(),
						errors: vec![(u.to_path_buf(), e.to_string())],
						error_overflow: 0,
						depth_limited: Vec::new(),
						depth_overflow: 0,
						status: ScanStatus::Incomplete,
					};
				}
			};
			if !canonical_under.starts_with(&canonical_root) {
				return WorkspaceScan {
					repos: Vec::new(),
					errors: vec![(
						u.to_path_buf(),
						"outside the shared folder".to_string(),
					)],
					error_overflow: 0,
					depth_limited: Vec::new(),
					depth_overflow: 0,
					status: ScanStatus::Incomplete,
				};
			}
			canonical_under
		}
		None => canonical_root.clone(),
	};

	let mut discovery = match Discovery::new(&start, max_depth, max_repos) {
		Ok(d) => d,
		Err(e) => {
			return WorkspaceScan {
				repos: Vec::new(),
				errors: vec![(start, e.to_string())],
				error_overflow: 0,
				depth_limited: Vec::new(),
				depth_overflow: 0,
				status: ScanStatus::Incomplete,
			};
		}
	};

	let page = discovery.next_page(budget);
	let (repos, id_errors) =
		identify_repos(page.repos, Some(&canonical_root), budget, opts);

	let mut all_errors = page.errors;
	all_errors.extend(id_errors);

	let (errors, error_overflow) = if all_errors.len() > MAX_SCAN_NOTES {
		let overflow = all_errors.len() - MAX_SCAN_NOTES;
		all_errors.truncate(MAX_SCAN_NOTES);
		(all_errors, overflow)
	} else {
		(all_errors, 0)
	};

	let mut depth_limited = page.depth_limited;
	let (depth_limited, depth_overflow) =
		if depth_limited.len() > MAX_SCAN_NOTES {
			let overflow = depth_limited.len() - MAX_SCAN_NOTES;
			depth_limited.truncate(MAX_SCAN_NOTES);
			(depth_limited, overflow)
		} else {
			(depth_limited, 0)
		};

	let mut status = page.status;
	if status == ScanStatus::Complete
		&& repos.iter().any(|r| {
			r.summary
				.as_ref()
				.err()
				.is_some_and(|msg| msg.starts_with("not read: "))
		}) {
		status = budget.stop(0).unwrap_or(ScanStatus::TimedOut);
	}

	WorkspaceScan {
		repos,
		errors,
		error_overflow,
		depth_limited,
		depth_overflow,
		status,
	}
}

/// The limits a view read runs under. Each variant is one combination the
/// desktop uses today; `options` rebuilds it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadProfile {
	/// RunOptions::interactive — lists, refs, log, details.
	Interactive,
	/// interactive + max_stdout = PREVIEW_LIMIT, Overflow::Error.
	InteractivePreview,
	/// preview() + max_stdout = PREVIEW_LIMIT, Overflow::Error.
	PreviewStrict,
	/// plain RunOptions::preview — 15 s, 1 MiB, Overflow::Truncate.
	Preview,
}

impl ReadProfile {
	pub fn options(self, cancel: Option<CancelToken>) -> RunOptions {
		match self {
			Self::Interactive => RunOptions::interactive(cancel),
			Self::InteractivePreview => RunOptions {
				max_stdout: crate::browser::PREVIEW_LIMIT,
				overflow: crate::gitrun::Overflow::Error,
				..RunOptions::interactive(cancel)
			},
			Self::PreviewStrict => RunOptions {
				max_stdout: crate::browser::PREVIEW_LIMIT,
				overflow: crate::gitrun::Overflow::Error,
				..RunOptions::preview(cancel)
			},
			Self::Preview => RunOptions::preview(cancel),
		}
	}
}

#[derive(Debug, Clone)]
pub struct Read {
	pub profile: ReadProfile,
	pub cancel: Option<CancelToken>,
}

/// Read-only view of a Git repository.
pub trait RepoView: Send + Sync {
	fn change_list(
		&self,
		max_rows: usize,
		read: &Read,
	) -> Result<ChangeList, GitError>;
	fn refs(&self, read: &Read) -> Result<RefSnapshot, GitError>;
	fn resolve_commit(
		&self,
		rev: &str,
		read: &Read,
	) -> Result<String, GitError>;
	fn log_from_tips(
		&self,
		tips: &[String],
		skip: usize,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<CommitSummary>, bool), GitError>;
	fn history_query(
		&self,
		reference: Option<&str>,
		query: &LogQuery,
		skip: usize,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<CommitSummary>, bool), GitError>;
	fn commit_details(
		&self,
		sha: &str,
		read: &Read,
	) -> Result<CommitDetails, GitError>;
	fn user_email(&self, read: &Read) -> Option<String>;
	fn changed_paths(
		&self,
		source: &GitSource,
		max: usize,
		read: &Read,
	) -> Result<ChangedPathList, GitError>;
	/// `listed`: the change type and, for a log row, the parents, when the
	/// caller listed the path itself. Honoured for Commit and Range only.
	fn preview(
		&self,
		source: &GitSource,
		path: &str,
		listed: Option<(ChangeType, Option<&[String]>)>,
		read: &Read,
	) -> Result<GitPreview, GitError>;
	fn changed_file_text(
		&self,
		source: &GitSource,
		path: &str,
		max: u64,
		read: &Read,
	) -> Result<Option<String>, GitError>;
	fn commit_directory(
		&self,
		rev: &str,
		dir: &str,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<TreeEntry>, bool), GitError>;
	fn commit_blob(
		&self,
		rev: &str,
		path: &str,
		max: u64,
		read: &Read,
	) -> Result<BlobText, GitError>;
}

/// Maximum stdout bytes allowed for a served Git process.
pub const SERVED_MAX_STDOUT: usize = 4 * 1024 * 1024;

/// Local repository view over filesystem and git processes.
#[derive(Debug, Clone)]
pub struct LocalRepo {
	git: Git,
	identity: Option<RepoIdentity>,
	served: bool,
}

impl LocalRepo {
	/// Spawns no process; identity cloned in.
	pub fn known(identity: &RepoIdentity) -> Self {
		Self {
			git: Git::at_known_root(identity),
			identity: Some(identity.clone()),
			served: false,
		}
	}

	/// Opens the repository at `root` under the given read profile; identity is `None`.
	pub fn open(root: &Path, read: &Read) -> Result<Self, GitError> {
		let opts = Self::base_opts(read);
		let git = Git::open_with(root, &opts)?;
		Ok(Self {
			git,
			identity: None,
			served: false,
		})
	}

	/// Worker only: opens with every boundary check; served = true.
	pub fn open_within(
		dir: &Path,
		boundary: &Path,
		read: &Read,
	) -> Result<Self, GitError> {
		let mut opts = Self::base_opts(read);
		opts.pool = GitPool::Served;
		opts.max_stdout = opts.max_stdout.min(SERVED_MAX_STDOUT);
		let git = Git::open_within(dir, boundary, &opts)?;
		let identity = check_repo_boundary(&git, boundary, &opts)?;
		Ok(Self {
			git,
			identity: Some(identity),
			served: true,
		})
	}

	/// Worker cache hit: no process, boundary env and served limits stay on.
	pub fn known_within(identity: &RepoIdentity, boundary: &Path) -> Self {
		let b = dunce::canonicalize(boundary)
			.unwrap_or_else(|_| boundary.to_path_buf());
		Self {
			git: Git::at_known_root(identity).with_boundary(Some(b)),
			identity: Some(identity.clone()),
			served: true,
		}
	}

	pub fn identity(&self) -> Option<&RepoIdentity> {
		self.identity.as_ref()
	}

	pub fn git(&self) -> &Git {
		&self.git
	}

	pub fn is_served(&self) -> bool {
		self.served
	}

	fn base_opts(read: &Read) -> RunOptions {
		read.profile.options(read.cancel.clone())
	}

	pub(crate) fn opts(&self, read: &Read) -> RunOptions {
		let mut opts = Self::base_opts(read);
		if self.served {
			opts.pool = GitPool::Served;
			opts.max_stdout = opts.max_stdout.min(SERVED_MAX_STDOUT);
		}
		opts
	}
}

impl RepoView for LocalRepo {
	fn change_list(
		&self,
		max_rows: usize,
		read: &Read,
	) -> Result<ChangeList, GitError> {
		let opts = self.opts(read);
		change_list_with(&self.git, self.identity.as_ref(), max_rows, &opts)
	}

	fn refs(&self, read: &Read) -> Result<RefSnapshot, GitError> {
		let opts = self.opts(read);
		crate::browser::refs_with(&self.git, &opts)
	}

	fn resolve_commit(
		&self,
		rev: &str,
		read: &Read,
	) -> Result<String, GitError> {
		let opts = self.opts(read);
		self.git.resolve_commit_with(rev, &opts)
	}

	fn log_from_tips(
		&self,
		tips: &[String],
		skip: usize,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<CommitSummary>, bool), GitError> {
		let opts = self.opts(read);
		crate::browser::log_from_tips_with(&self.git, tips, skip, limit, &opts)
	}

	fn history_query(
		&self,
		reference: Option<&str>,
		query: &LogQuery,
		skip: usize,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<CommitSummary>, bool), GitError> {
		let opts = self.opts(read);
		crate::browser::history_query_with(
			&self.git, reference, query, skip, limit, &opts,
		)
	}

	fn commit_details(
		&self,
		sha: &str,
		read: &Read,
	) -> Result<CommitDetails, GitError> {
		let opts = self.opts(read);
		commit_details_with(&self.git, sha, &opts)
	}

	fn user_email(&self, read: &Read) -> Option<String> {
		let opts = self.opts(read);
		user_email_with(&self.git, &opts)
	}

	fn changed_paths(
		&self,
		source: &GitSource,
		max: usize,
		read: &Read,
	) -> Result<ChangedPathList, GitError> {
		let opts = self.opts(read);
		changed_paths_with(&self.git, source, max, &opts)
	}

	fn preview(
		&self,
		source: &GitSource,
		path: &str,
		listed: Option<(ChangeType, Option<&[String]>)>,
		read: &Read,
	) -> Result<GitPreview, GitError> {
		let opts = self.opts(read);
		let parents = if self.served {
			None
		} else {
			listed.and_then(|(_, p)| p)
		};
		match (listed, source) {
			(
				Some((change, _)),
				GitSource::Commit(_) | GitSource::Range(..),
			) => crate::browser::git_preview_for(
				&self.git, source, path, change, parents, &opts,
			),
			_ => {
				crate::browser::git_preview_with(&self.git, source, path, &opts)
			}
		}
	}

	fn changed_file_text(
		&self,
		source: &GitSource,
		path: &str,
		max: u64,
		read: &Read,
	) -> Result<Option<String>, GitError> {
		let opts = self.opts(read);
		crate::gitsrc::read_changed_file_with(
			&self.git, source, path, max, &opts,
		)
		.map(|opt| opt.and_then(|f| f.content))
	}

	fn commit_directory(
		&self,
		rev: &str,
		dir: &str,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<TreeEntry>, bool), GitError> {
		let opts = self.opts(read);
		crate::browser::commit_directory_with(&self.git, rev, dir, limit, &opts)
	}

	fn commit_blob(
		&self,
		rev: &str,
		path: &str,
		max: u64,
		read: &Read,
	) -> Result<BlobText, GitError> {
		let opts = self.opts(read);
		crate::browser::commit_blob_with(&self.git, rev, path, max, &opts)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	static SERVED_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

	fn has_git() -> bool {
		match std::process::Command::new("git").arg("--version").output() {
			Ok(out) if out.status.success() => true,
			_ => {
				assert!(
					std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
					"git is required when SNIP_REQUIRE_ALL_TESTS is set"
				);
				false
			}
		}
	}

	fn run_git(cwd: &Path, args: &[&str]) {
		let _ = std::fs::create_dir_all(cwd);
		let out = match std::process::Command::new("git")
			.args(["-c", "user.name=t", "-c", "user.email=t@t"])
			.args(args)
			.current_dir(cwd)
			.output()
		{
			Ok(out) => out,
			Err(e) => {
				assert!(
					std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
					"git failed to run with SNIP_REQUIRE_ALL_TESTS set: {e}"
				);
				panic!("git failed to run: {e}");
			}
		};
		assert!(
			out.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
	}

	#[test]
	fn staged_rename_read_change_list() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		std::fs::write(root.join("old-name.txt"), "index bytes\n").unwrap();
		run_git(root, &["add", "old-name.txt"]);
		run_git(root, &["commit", "-qm", "initial"]);
		run_git(root, &["mv", "old-name.txt", "new-name.txt"]);

		let git = Git::open_with(root, &RunOptions::default()).unwrap();
		let list =
			change_list_with(&git, None, usize::MAX, &RunOptions::default())
				.unwrap();
		let staged_items: Vec<_> = list
			.rows
			.iter()
			.filter(|r| r.source == ChangeSource::Staged)
			.collect();
		assert_eq!(staged_items.len(), 1);
		assert_eq!(staged_items[0].path, "new-name.txt");
		assert_eq!(staged_items[0].change_type, Some(ChangeType::Moved));
		assert!(!list.rows.iter().any(|r| r.path == "old-name.txt"));
	}

	#[test]
	fn test_discovery_processing_symlink_dedup_and_submodules() {
		if !has_git() {
			return;
		}
		let temp = tempfile::tempdir().unwrap();
		let root = temp.path();

		let main = root.join("main");
		std::fs::create_dir_all(&main).unwrap();
		run_git(&main, &["init"]);
		std::fs::write(main.join("file.txt"), "hello").unwrap();
		run_git(&main, &["add", "."]);
		run_git(&main, &["commit", "-m", "init"]);

		#[cfg(unix)]
		{
			std::os::unix::fs::symlink(&main, root.join("main-alias")).unwrap();
		}

		let gitmodules_content = "[submodule \"vendor/sub\"]\n\tpath = vendor/sub\n\turl = https://example.invalid/sub.git\n";
		std::fs::write(main.join(".gitmodules"), gitmodules_content).unwrap();

		let opts = RunOptions::default();
		#[allow(unused_mut)]
		let mut disc = vec![DiscoveredRepo {
			path: main.clone(),
			marker: crate::workspace::GitMarker::Directory,
		}];
		#[cfg(unix)]
		{
			disc.push(DiscoveredRepo {
				path: root.join("main-alias"),
				marker: crate::workspace::GitMarker::Directory,
			});
		}

		let (list, _errors) =
			identify_repos(disc, None, &ScanBudget::visits(usize::MAX), &opts);

		let main_entries: Vec<_> = list
			.iter()
			.filter(|r| r.name == "main" || r.name == "main-alias")
			.collect();
		assert_eq!(
			main_entries.len(),
			1,
			"symlink alias must be deduplicated: {main_entries:?}"
		);
		assert_eq!(main_entries[0].kind, FoundKind::Main);

		let sub_entries: Vec<_> =
			list.iter().filter(|r| r.name == "vendor/sub").collect();
		assert_eq!(
			sub_entries.len(),
			1,
			"uninitialized submodule must be listed: {list:?}"
		);
		assert_eq!(sub_entries[0].kind, FoundKind::UninitializedSubmodule);
		assert!(sub_entries[0].identity.is_none());
		assert!(sub_entries[0].summary.is_err());

		#[cfg(unix)]
		{
			let alias_repo =
				identify_repo(&root.join("main-alias"), None, &opts);
			assert_eq!(alias_repo.kind, FoundKind::Main);
			assert!(alias_repo.summary.is_ok());
			let canonical_main = dunce::canonicalize(&main).unwrap_or(main);
			assert_eq!(alias_repo.root, canonical_main);
		}
	}

	#[test]
	fn change_list_with_caps_rows_and_reports_total() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		for i in 1..=7 {
			std::fs::write(
				root.join(format!("file_{i}.txt")),
				format!("content {i}"),
			)
			.unwrap();
		}
		let git = Git::open_with(root, &RunOptions::default()).unwrap();
		let list =
			change_list_with(&git, None, 3, &RunOptions::default()).unwrap();
		assert_eq!(list.rows.len(), 3);
		assert_eq!(list.total, 7);
		assert_eq!(list.rows[0].path, "file_1.txt");
		assert_eq!(list.rows[1].path, "file_2.txt");
		assert_eq!(list.rows[2].path, "file_3.txt");
	}

	#[test]
	fn changed_paths_with_caps_and_reports_total() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		for i in 1..=5 {
			std::fs::write(
				root.join(format!("f{i}.txt")),
				format!("content {i}"),
			)
			.unwrap();
		}
		run_git(root, &["add", "."]);
		run_git(root, &["commit", "-qm", "commit 5 files"]);
		let git = Git::open_with(root, &RunOptions::default()).unwrap();
		let head = git.resolve_commit("HEAD").unwrap();
		let res = changed_paths_with(
			&git,
			&GitSource::Commit(head),
			2,
			&RunOptions::default(),
		)
		.unwrap();
		assert_eq!(res.paths.len(), 2);
		assert_eq!(res.total, 5);
	}

	#[test]
	fn identify_repos_marks_unreached_repos_when_budget_is_spent() {
		let disc = vec![
			DiscoveredRepo {
				path: PathBuf::from("/nonexistent/repo1"),
				marker: crate::workspace::GitMarker::Directory,
			},
			DiscoveredRepo {
				path: PathBuf::from("/nonexistent/repo2"),
				marker: crate::workspace::GitMarker::Directory,
			},
		];
		let budget = ScanBudget {
			max_visited: usize::MAX,
			deadline: Some(
				std::time::Instant::now()
					.checked_sub(std::time::Duration::from_secs(10))
					.unwrap(),
			),
			cancel: None,
		};
		let (list, errors) =
			identify_repos(disc, None, &budget, &RunOptions::default());
		assert!(errors.is_empty());
		assert_eq!(list.len(), 2);
		assert_eq!(list[0].root, PathBuf::from("/nonexistent/repo1"));
		assert_eq!(list[0].name, "repo1");
		assert_eq!(list[0].kind, FoundKind::Main);
		assert!(list[0].identity.is_none());
		assert!(list[0].summary.as_ref().unwrap_err().contains("not read"));

		assert_eq!(list[1].root, PathBuf::from("/nonexistent/repo2"));
		assert_eq!(list[1].name, "repo2");
		assert_eq!(list[1].kind, FoundKind::Main);
		assert!(list[1].identity.is_none());
		assert!(list[1].summary.as_ref().unwrap_err().contains("not read"));
	}

	#[test]
	fn commit_details_with_reads_a_commit() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		run_git(root, &["config", "user.name", "Tester"]);
		run_git(root, &["config", "user.email", "tester@test.local"]);
		std::fs::write(root.join("hello.txt"), "hello world\n").unwrap();
		run_git(root, &["add", "hello.txt"]);
		run_git(
			root,
			&[
				"-c",
				"user.name=Tester",
				"-c",
				"user.email=tester@test.local",
				"commit",
				"-qm",
				"initial commit message",
			],
		);

		let git = Git::open_with(root, &RunOptions::default()).unwrap();
		let head = git.resolve_commit("HEAD").unwrap();
		let details =
			commit_details_with(&git, &head, &RunOptions::default()).unwrap();
		assert_eq!(details.sha, head);
		assert_eq!(details.author, "Tester");
		assert_eq!(details.author_email, "tester@test.local");
		assert_eq!(details.message, "initial commit message");
		assert!(details.parents.is_empty());
		assert!(details.branches.iter().any(|b| b == "main"));
	}

	#[test]
	fn user_email_with_returns_configured_email() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q"]);
		run_git(root, &["config", "user.email", "custom@example.com"]);
		let git = Git::open_with(root, &RunOptions::default()).unwrap();
		let email = user_email_with(&git, &RunOptions::default());
		assert_eq!(email.as_deref(), Some("custom@example.com"));
	}

	#[test]
	fn serde_round_trips() {
		fn check<
			T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
		>(
			val: T,
		) {
			let json = serde_json::to_string(&val).expect("serialize");
			let de: T = serde_json::from_str(&json).expect("deserialize");
			assert_eq!(val, de);
		}

		// browser types
		check(crate::browser::CommitSummary {
			sha: "abc1234".into(),
			parents: vec!["def5678".into()],
			author_name: "Author".into(),
			author_email: "author@test.local".into(),
			author_date: "2026-01-01T00:00:00Z".into(),
			subject: "subject line".into(),
		});

		check(crate::browser::GitReference {
			name: "refs/heads/main".into(),
			sha: "abc1234".into(),
		});

		check(crate::browser::RefSnapshot {
			refs: vec![crate::browser::GitReference {
				name: "refs/heads/main".into(),
				sha: "abc1234".into(),
			}],
			head: Some("abc1234".into()),
			detached: false,
			shallow: vec!["shallow1".into()],
		});

		check(crate::browser::LogQuery {
			text: "needle".into(),
			regex: true,
			match_case: true,
			author: Some("author".into()),
			since: Some("2026-01-01".into()),
			until: Some("2026-12-31".into()),
			paths: vec!["src/lib.rs".into()],
		});

		check(crate::browser::TreeKind::Blob);
		check(crate::browser::TreeKind::Tree);
		check(crate::browser::TreeKind::Submodule);

		check(crate::browser::TreeEntry {
			path: "src/lib.rs".into(),
			name: "lib.rs".into(),
			kind: crate::browser::TreeKind::Blob,
		});

		check(crate::browser::BlobText::Text("hello".into()));
		check(crate::browser::BlobText::Binary);
		check(crate::browser::BlobText::TooLarge(12345));
		check(crate::browser::BlobText::NotUtf8);

		check(crate::browser::GitPreview {
			content: Some("preview content".into()),
			patch: "--- a/f\n+++ b/f\n".into(),
			patch_truncated: false,
		});

		// gitsrc types
		check(crate::gitsrc::GitSource::Working);
		check(crate::gitsrc::GitSource::Staged);
		check(crate::gitsrc::GitSource::Commit("sha1".into()));
		check(crate::gitsrc::GitSource::Range(
			"sha1".into(),
			"sha2".into(),
		));

		// format types
		check(crate::format::ChangeType::New);
		check(crate::format::ChangeType::Modified);
		check(crate::format::ChangeType::Deleted);
		check(crate::format::ChangeType::Moved);

		// workspace types
		check(crate::workspace::ChangeCounts {
			staged: 1,
			unstaged: 2,
			untracked: 3,
			conflicted: 4,
		});

		check(crate::workspace::ScanStatus::Complete);
		check(crate::workspace::ScanStatus::More);
		check(crate::workspace::ScanStatus::LimitReached);
		check(crate::workspace::ScanStatus::Incomplete);
		check(crate::workspace::ScanStatus::Cancelled);
		check(crate::workspace::ScanStatus::TimedOut);

		// gitview types
		check(StatusSummary {
			head: Some("head_sha".into()),
			branch: Some("main".into()),
			changes: crate::workspace::ChangeCounts::default(),
		});

		check(ChangeRow {
			path: "a/b.txt".into(),
			change_type: Some(crate::format::ChangeType::New),
			source: ChangeSource::Working,
			conflict: false,
		});

		check(ChangeSource::Staged);
		check(ChangeSource::Unstaged);
		check(ChangeSource::Working);

		check(ChangeList {
			summary: Some(StatusSummary {
				head: Some("head".into()),
				branch: Some("main".into()),
				changes: crate::workspace::ChangeCounts::default(),
			}),
			rows: vec![ChangeRow {
				path: "f.txt".into(),
				change_type: Some(crate::format::ChangeType::Modified),
				source: ChangeSource::Staged,
				conflict: false,
			}],
			total: 1,
		});

		check(ChangedPathList {
			paths: vec![(
				"f.txt".into(),
				Some(crate::format::ChangeType::Modified),
			)],
			gitlinks: vec!["subm".into()],
			total: 1,
		});

		check(CommitDetails {
			sha: "abc".into(),
			parents: vec!["parent1".into()],
			message: "msg".into(),
			author: "Author".into(),
			author_email: "a@b.com".into(),
			author_date: "date".into(),
			committer: "Committer".into(),
			committer_email: "c@d.com".into(),
			commit_date: "cdate".into(),
			branches: vec!["main".into()],
			branches_more: false,
		});

		check(FoundKind::Main);
		check(FoundKind::LinkedWorktree);
		check(FoundKind::Submodule);
		check(FoundKind::UninitializedSubmodule);

		// RepositoryHistory PartialEq test
		let h1 = crate::browser::RepositoryHistory {
			root: "root".into(),
			commits: vec![],
			refs: vec![],
			head: None,
			has_more: false,
		};
		let h2 = h1.clone();
		assert_eq!(h1, h2);
	}

	#[test]
	fn read_profile_options_match_todays_run_options() {
		use crate::gitrun::{CancelToken, GitPool, Overflow, RunOptions};

		// 1. Interactive
		let opt = ReadProfile::Interactive.options(None);
		let exp = RunOptions::interactive(None);
		assert_eq!(opt.timeout, exp.timeout);
		assert_eq!(opt.queue_timeout, exp.queue_timeout);
		assert_eq!(opt.max_stdout, exp.max_stdout);
		assert_eq!(opt.overflow, exp.overflow);
		assert_eq!(opt.pool, GitPool::Local);
		assert!(opt.cancel.is_none());

		// 2. InteractivePreview
		let opt = ReadProfile::InteractivePreview.options(None);
		let exp = RunOptions {
			max_stdout: crate::browser::PREVIEW_LIMIT,
			overflow: Overflow::Error,
			..RunOptions::interactive(None)
		};
		assert_eq!(opt.timeout, exp.timeout);
		assert_eq!(opt.queue_timeout, exp.queue_timeout);
		assert_eq!(opt.max_stdout, exp.max_stdout);
		assert_eq!(opt.overflow, exp.overflow);
		assert_eq!(opt.pool, GitPool::Local);
		assert!(opt.cancel.is_none());

		// 3. PreviewStrict
		let opt = ReadProfile::PreviewStrict.options(None);
		let exp = RunOptions {
			max_stdout: crate::browser::PREVIEW_LIMIT,
			overflow: Overflow::Error,
			..RunOptions::preview(None)
		};
		assert_eq!(opt.timeout, exp.timeout);
		assert_eq!(opt.queue_timeout, exp.queue_timeout);
		assert_eq!(opt.max_stdout, exp.max_stdout);
		assert_eq!(opt.overflow, exp.overflow);
		assert_eq!(opt.pool, GitPool::Local);
		assert!(opt.cancel.is_none());

		// 4. Preview
		let opt = ReadProfile::Preview.options(None);
		let exp = RunOptions::preview(None);
		assert_eq!(opt.timeout, exp.timeout);
		assert_eq!(opt.queue_timeout, exp.queue_timeout);
		assert_eq!(opt.max_stdout, exp.max_stdout);
		assert_eq!(opt.overflow, exp.overflow);
		assert_eq!(opt.pool, GitPool::Local);
		assert!(opt.cancel.is_none());

		// Check cancel token carried
		for profile in [
			ReadProfile::Interactive,
			ReadProfile::InteractivePreview,
			ReadProfile::PreviewStrict,
			ReadProfile::Preview,
		] {
			let token = CancelToken::new();
			let opt = profile.options(Some(token));
			assert!(opt.cancel.is_some());
		}

		// Serde round trip for all variants of ReadProfile
		for profile in [
			ReadProfile::Interactive,
			ReadProfile::InteractivePreview,
			ReadProfile::PreviewStrict,
			ReadProfile::Preview,
		] {
			let json = serde_json::to_string(&profile).expect("serialize");
			let de: ReadProfile =
				serde_json::from_str(&json).expect("deserialize");
			assert_eq!(profile, de);
		}
	}

	#[test]
	fn local_repo_matches_the_direct_core_calls() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		run_git(root, &["config", "user.name", "Tester"]);
		run_git(root, &["config", "user.email", "tester@test.local"]);

		// Commit 1: a.txt and unmodified.txt
		std::fs::write(root.join("a.txt"), "hello world\n").unwrap();
		std::fs::write(root.join("unmodified.txt"), "unmodified content\n")
			.unwrap();
		run_git(root, &["add", "a.txt", "unmodified.txt"]);
		run_git(
			root,
			&[
				"-c",
				"user.name=Tester",
				"-c",
				"user.email=tester@test.local",
				"commit",
				"-qm",
				"initial commit",
			],
		);

		// Commit 2: modify a.txt
		std::fs::write(root.join("a.txt"), "hello world\nsecond line\n")
			.unwrap();
		run_git(
			root,
			&[
				"-c",
				"user.name=Tester",
				"-c",
				"user.email=tester@test.local",
				"commit",
				"-am",
				"second commit",
			],
		);

		// Working tree state:
		// a.txt: modified in working tree
		std::fs::write(
			root.join("a.txt"),
			"hello world\nsecond line\nworking line\n",
		)
		.unwrap();
		// staged.txt: staged
		std::fs::write(root.join("staged.txt"), "staged content\n").unwrap();
		run_git(root, &["add", "staged.txt"]);
		// untracked.txt: untracked
		std::fs::write(root.join("untracked.txt"), "untracked content\n")
			.unwrap();

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let opts = read.profile.options(None);

		let open_repo = LocalRepo::open(root, &read).unwrap();
		let git = Git::open_with(root, &opts).unwrap();
		let identity = RepoIdentity::resolve(&git, &opts).unwrap();
		let known_repo = LocalRepo::known(&identity);

		assert_eq!(open_repo.identity(), None);
		assert_eq!(known_repo.identity(), Some(&identity));

		// 1. change_list
		let cl_open = open_repo.change_list(100, &read).unwrap();
		let cl_direct_open = change_list_with(&git, None, 100, &opts).unwrap();
		assert_eq!(cl_open, cl_direct_open);
		assert!(cl_open.summary.is_none());

		let cl_known = known_repo.change_list(100, &read).unwrap();
		let cl_direct_known =
			change_list_with(&git, Some(&identity), 100, &opts).unwrap();
		assert_eq!(cl_known, cl_direct_known);
		assert!(cl_known.summary.is_some());
		assert_eq!(cl_known.rows, cl_open.rows);
		assert_eq!(cl_known.total, cl_open.total);

		// 2. refs
		let refs = known_repo.refs(&read).unwrap();
		let direct_refs = crate::browser::refs_with(&git, &opts).unwrap();
		assert_eq!(refs, direct_refs);

		// 3. resolve_commit
		let head = known_repo.resolve_commit("HEAD", &read).unwrap();
		let direct_head = git.resolve_commit_with("HEAD", &opts).unwrap();
		assert_eq!(head, direct_head);

		// 4. log_from_tips
		let tips = refs.tips();
		let log = known_repo.log_from_tips(&tips, 0, 10, &read).unwrap();
		let direct_log =
			crate::browser::log_from_tips_with(&git, &tips, 0, 10, &opts)
				.unwrap();
		assert_eq!(log, direct_log);

		// 5. history_query
		let query = crate::browser::LogQuery::default();
		let hq = known_repo
			.history_query(None, &query, 0, 10, &read)
			.unwrap();
		let direct_hq = crate::browser::history_query_with(
			&git, None, &query, 0, 10, &opts,
		)
		.unwrap();
		assert_eq!(hq, direct_hq);

		// 6. commit_details
		let details = known_repo.commit_details(&head, &read).unwrap();
		let direct_details = commit_details_with(&git, &head, &opts).unwrap();
		assert_eq!(details, direct_details);

		// 7. user_email
		let email = known_repo.user_email(&read);
		let direct_email = user_email_with(&git, &opts);
		assert_eq!(email, direct_email);

		// 8. changed_paths
		let src_commit = GitSource::Commit(head.clone());
		let cp = known_repo.changed_paths(&src_commit, 100, &read).unwrap();
		let direct_cp =
			changed_paths_with(&git, &src_commit, 100, &opts).unwrap();
		assert_eq!(cp, direct_cp);

		// 9. commit_directory
		let cd = known_repo.commit_directory(&head, "", 100, &read).unwrap();
		let direct_cd =
			crate::browser::commit_directory_with(&git, &head, "", 100, &opts)
				.unwrap();
		assert_eq!(cd, direct_cd);

		// 10. commit_blob
		let cb = known_repo
			.commit_blob(&head, "a.txt", 1024 * 1024, &read)
			.unwrap();
		let direct_cb = crate::browser::commit_blob_with(
			&git,
			&head,
			"a.txt",
			1024 * 1024,
			&opts,
		)
		.unwrap();
		assert_eq!(cb, direct_cb);

		// 11. changed_file_text
		let cft = known_repo
			.changed_file_text(&GitSource::Working, "a.txt", 1024 * 1024, &read)
			.unwrap();
		let direct_cft = crate::gitsrc::read_changed_file_with(
			&git,
			&GitSource::Working,
			"a.txt",
			1024 * 1024,
			&opts,
		)
		.unwrap()
		.and_then(|f| f.content);
		assert_eq!(cft, direct_cft);

		// 12. preview (Working/Staged/Commit with and without listed)
		// Working without listed:
		let p_work = known_repo
			.preview(&GitSource::Working, "a.txt", None, &read)
			.unwrap();
		let direct_p_work = crate::browser::git_preview_with(
			&git,
			&GitSource::Working,
			"a.txt",
			&opts,
		)
		.unwrap();
		assert_eq!(p_work, direct_p_work);

		// Working with listed:
		let p_work_listed = known_repo
			.preview(
				&GitSource::Working,
				"a.txt",
				Some((ChangeType::Modified, None)),
				&read,
			)
			.unwrap();
		assert_eq!(p_work_listed, direct_p_work);

		// Staged without listed:
		let p_stage = known_repo
			.preview(&GitSource::Staged, "staged.txt", None, &read)
			.unwrap();
		let direct_p_stage = crate::browser::git_preview_with(
			&git,
			&GitSource::Staged,
			"staged.txt",
			&opts,
		)
		.unwrap();
		assert_eq!(p_stage, direct_p_stage);

		// Staged with listed:
		let p_stage_listed = known_repo
			.preview(
				&GitSource::Staged,
				"staged.txt",
				Some((ChangeType::New, None)),
				&read,
			)
			.unwrap();
		assert_eq!(p_stage_listed, direct_p_stage);

		// Commit without listed:
		let p_commit = known_repo
			.preview(&src_commit, "a.txt", None, &read)
			.unwrap();
		let direct_p_commit =
			crate::browser::git_preview_with(&git, &src_commit, "a.txt", &opts)
				.unwrap();
		assert_eq!(p_commit, direct_p_commit);

		// Commit with listed:
		let p_commit_listed = known_repo
			.preview(
				&src_commit,
				"a.txt",
				Some((ChangeType::Modified, Some(&details.parents))),
				&read,
			)
			.unwrap();
		let direct_p_commit_listed = crate::browser::git_preview_for(
			&git,
			&src_commit,
			"a.txt",
			ChangeType::Modified,
			Some(&details.parents),
			&opts,
		)
		.unwrap();
		assert_eq!(p_commit_listed, direct_p_commit_listed);
	}

	#[test]
	fn local_repo_preview_uses_membership_for_working_and_staged() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		run_git(root, &["config", "user.name", "Tester"]);
		run_git(root, &["config", "user.email", "tester@test.local"]);

		std::fs::write(root.join("unmodified.txt"), "committed\n").unwrap();
		std::fs::write(root.join("modified.txt"), "v1\n").unwrap();
		run_git(root, &["add", "unmodified.txt", "modified.txt"]);
		run_git(root, &["commit", "-qm", "initial"]);

		// Modify modified.txt in working copy
		std::fs::write(root.join("modified.txt"), "v2\n").unwrap();

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let repo = LocalRepo::open(root, &read).unwrap();
		let opts = read.profile.options(None);
		let git = Git::open_with(root, &opts).unwrap();
		let head = git.resolve_commit_with("HEAD", &opts).unwrap();

		// For Working and Staged, passing listed = Some(...) for unmodified file
		// must return GitError::Malformed("Path is not in this Git source")
		let err_work = repo
			.preview(
				&GitSource::Working,
				"unmodified.txt",
				Some((ChangeType::Modified, None)),
				&read,
			)
			.unwrap_err();
		assert!(
			matches!(&err_work, GitError::Malformed(msg) if msg == "Path is not in this Git source"),
			"expected Malformed, got {err_work:?}"
		);

		let err_stage = repo
			.preview(
				&GitSource::Staged,
				"unmodified.txt",
				Some((ChangeType::Modified, None)),
				&read,
			)
			.unwrap_err();
		assert!(
			matches!(&err_stage, GitError::Malformed(msg) if msg == "Path is not in this Git source"),
			"expected Malformed, got {err_stage:?}"
		);

		// With GitSource::Commit(head) and listed = Some(..), for a real changed path,
		// result equals git_preview_for
		let commit_preview = repo
			.preview(
				&GitSource::Commit(head.clone()),
				"modified.txt",
				Some((ChangeType::New, None)),
				&read,
			)
			.unwrap();
		let direct_commit_preview = crate::browser::git_preview_for(
			&git,
			&GitSource::Commit(head),
			"modified.txt",
			ChangeType::New,
			None,
			&opts,
		)
		.unwrap();
		assert_eq!(commit_preview, direct_commit_preview);

		// Working preview of genuinely modified file with listed = None and with Some gives identical results
		let p_none = repo
			.preview(&GitSource::Working, "modified.txt", None, &read)
			.unwrap();
		let p_some = repo
			.preview(
				&GitSource::Working,
				"modified.txt",
				Some((ChangeType::Modified, None)),
				&read,
			)
			.unwrap();
		assert_eq!(p_none, p_some);
	}

	#[test]
	fn local_repo_trait_object_and_send_sync() {
		fn assert_send_sync<T: Send + Sync>() {}
		fn take_repo_view(_view: &dyn RepoView) {}

		assert_send_sync::<LocalRepo>();

		let identity = RepoIdentity {
			toplevel: PathBuf::from("/nonexistent/repo"),
			git_dir: PathBuf::from("/nonexistent/repo/.git"),
			common_dir: PathBuf::from("/nonexistent/repo/.git"),
			kind: RepoKind::Main,
		};
		let repo = LocalRepo::known(&identity);
		take_repo_view(&repo);
	}

	#[cfg(unix)]
	#[test]
	fn open_within_accepts_a_boundary_spelled_through_a_symlink() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let real_share = dir.path().join("real_share");
		std::fs::create_dir_all(&real_share).unwrap();
		let repo_dir = real_share.join("repo");
		run_git(&repo_dir, &["init", "-q", "-b", "main"]);
		std::fs::write(repo_dir.join("f.txt"), "ok").unwrap();
		run_git(&repo_dir, &["add", "f.txt"]);
		run_git(&repo_dir, &["commit", "-qm", "init"]);

		let sym_share = dir.path().join("sym_share");
		std::os::unix::fs::symlink(&real_share, &sym_share).unwrap();

		let opts = RunOptions::default();
		let git = Git::open_within(&sym_share.join("repo"), &sym_share, &opts);
		assert!(git.is_ok(), "expected Ok, got {git:?}");

		let other_share = dir.path().join("other_share");
		std::fs::create_dir_all(&other_share).unwrap();
		let git_outside =
			Git::open_within(&sym_share.join("repo"), &other_share, &opts);
		assert!(
			matches!(git_outside, Err(GitError::OutsideBoundary { .. })),
			"expected OutsideBoundary, got {git_outside:?}"
		);
	}

	#[test]
	fn open_within_refuses_a_parent_repository() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let outer = dir.path().join("outer");
		run_git(&outer, &["init", "-q", "-b", "main"]);
		std::fs::write(outer.join("outer-dirty.txt"), "dirty").unwrap();

		let inner = outer.join("inner");
		std::fs::create_dir_all(&inner).unwrap();

		let opts = RunOptions::default();
		let res = Git::open_within(&inner, &inner, &opts);
		assert!(
			matches!(res, Err(GitError::NotARepository(_))),
			"expected NotARepository, got {res:?}"
		);
	}

	#[test]
	fn open_within_refuses_an_empty_git_dir_that_would_reach_the_parent() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let outer = dir.path().join("outer");
		run_git(&outer, &["init", "-q", "-b", "main"]);

		let sub = outer.join("sub");
		std::fs::create_dir_all(sub.join(".git")).unwrap();

		let opts = RunOptions::default();
		let res = Git::open_within(&sub, &outer, &opts);
		assert!(res.is_err(), "expected error, got {res:?}");
		if let Ok(git) = res {
			assert_ne!(git.root(), outer);
		}
	}

	#[test]
	fn open_within_refuses_core_worktree_outside() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let repo = share.join("repo");
		run_git(&repo, &["init", "-q", "-b", "main"]);
		run_git(
			&repo,
			&["config", "core.worktree", outside.to_str().unwrap()],
		);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let res = LocalRepo::open_within(&repo, &share, &read);
		assert!(
			matches!(res, Err(GitError::OutsideBoundary { .. })),
			"expected OutsideBoundary, got {res:?}"
		);
	}

	#[test]
	fn open_within_refuses_alternates_outside() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let repo_b = outside.join("b");
		run_git(&repo_b, &["init", "-q", "-b", "main"]);
		std::fs::write(repo_b.join("secret"), "TOPSECRET_ALTERNATES\n")
			.unwrap();
		run_git(&repo_b, &["add", "secret"]);
		run_git(&repo_b, &["commit", "-qm", "secret commit"]);
		let opts = RunOptions::default();
		let git_b = Git::open_with(&repo_b, &opts).unwrap();
		let b_sha = git_b.resolve_commit_with("HEAD", &opts).unwrap();

		let repo_a = share.join("a");
		run_git(
			dir.path(),
			&[
				"clone",
				"--shared",
				repo_b.to_str().unwrap(),
				repo_a.to_str().unwrap(),
			],
		);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		// Demonstrate pre-fix danger: with plain open, B's secret blob is readable
		let plain_repo = LocalRepo::open(&repo_a, &read).unwrap();
		let blob = plain_repo
			.commit_blob(&b_sha, "secret", 1024, &read)
			.unwrap();
		assert!(
			matches!(blob, BlobText::Text(ref s) if s.contains("TOPSECRET_ALTERNATES")),
			"expected secret blob to be readable without boundary"
		);

		// With open_within, alternates outside share must be refused
		let res = LocalRepo::open_within(&repo_a, &share, &read);
		assert!(
			matches!(
				res,
				Err(GitError::OutsideBoundary {
					what: "alternate object store"
				})
			),
			"expected OutsideBoundary for alternates, got {res:?}"
		);
	}

	#[test]
	fn open_within_accepts_alternates_inside_the_share() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		std::fs::create_dir_all(&share).unwrap();

		let repo_a = share.join("a");
		run_git(&repo_a, &["init", "-q", "-b", "main"]);
		std::fs::write(repo_a.join("f.txt"), "hello").unwrap();
		run_git(&repo_a, &["add", "f.txt"]);
		run_git(&repo_a, &["commit", "-qm", "init"]);

		let repo_b = share.join("b");
		run_git(
			&share,
			&[
				"clone",
				"--shared",
				repo_a.to_str().unwrap(),
				repo_b.to_str().unwrap(),
			],
		);

		let repo_c = share.join("c");
		run_git(
			&share,
			&[
				"clone",
				"--shared",
				repo_b.to_str().unwrap(),
				repo_c.to_str().unwrap(),
			],
		);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let res = LocalRepo::open_within(&repo_c, &share, &read);
		assert!(
			res.is_ok(),
			"expected Ok for alternates inside share, got {res:?}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn open_within_refuses_a_symlinked_object_store_outside() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let repo = share.join("repo");
		run_git(&repo, &["init", "-q", "-b", "main"]);
		std::fs::write(repo.join("f.txt"), "data").unwrap();
		run_git(&repo, &["add", "f.txt"]);
		run_git(&repo, &["commit", "-qm", "init"]);

		let repo_objects = repo.join(".git/objects");
		let outside_objects = outside.join("objects");
		std::fs::rename(&repo_objects, &outside_objects).unwrap();
		std::os::unix::fs::symlink(&outside_objects, &repo_objects).unwrap();

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let res = LocalRepo::open_within(&repo, &share, &read);
		assert!(
			matches!(
				res,
				Err(GitError::OutsideBoundary {
					what: "object store"
				})
			),
			"expected OutsideBoundary for object store, got {res:?}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn open_within_refuses_a_symlinked_index_outside() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let repo = share.join("repo");
		run_git(&repo, &["init", "-q", "-b", "main"]);
		std::fs::write(repo.join("f.txt"), "data").unwrap();
		run_git(&repo, &["add", "f.txt"]);
		run_git(&repo, &["commit", "-qm", "init"]);

		let repo_index = repo.join(".git/index");
		let outside_index = outside.join("index");
		std::fs::rename(&repo_index, &outside_index).unwrap();
		std::os::unix::fs::symlink(&outside_index, &repo_index).unwrap();

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let res = LocalRepo::open_within(&repo, &share, &read);
		assert!(
			matches!(res, Err(GitError::OutsideBoundary { what: "index" })),
			"expected OutsideBoundary for index, got {res:?}"
		);
	}

	#[test]
	fn open_within_refuses_a_linked_worktree_of_an_outside_repo() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let main_repo = outside.join("main");
		run_git(&main_repo, &["init", "-q", "-b", "main"]);
		std::fs::write(main_repo.join("f.txt"), "data").unwrap();
		run_git(&main_repo, &["add", "f.txt"]);
		run_git(&main_repo, &["commit", "-qm", "init"]);

		let wt = share.join("wt");
		run_git(&main_repo, &["worktree", "add", wt.to_str().unwrap()]);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let res = LocalRepo::open_within(&wt, &share, &read);
		assert!(res.is_err());
		let err_text = res.unwrap_err().to_string();
		assert!(
			err_text.contains("main repository of this linked worktree")
				|| err_text.contains(
					"share the folder that holds the main repository"
				),
			"unexpected error text: {err_text}"
		);
		let outside_str = outside.to_string_lossy().into_owned();
		assert!(
			!err_text.contains(&outside_str),
			"error text leaked outside path: {err_text}"
		);
	}

	#[test]
	fn open_within_refused_errors_name_no_outside_path() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();
		let outside_str = outside.to_string_lossy().into_owned();

		// 1. core.worktree outside
		let repo_wt = share.join("repo_wt");
		run_git(&repo_wt, &["init", "-q", "-b", "main"]);
		run_git(
			&repo_wt,
			&["config", "core.worktree", outside.to_str().unwrap()],
		);
		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let err1 = LocalRepo::open_within(&repo_wt, &share, &read).unwrap_err();
		assert!(
			!err1.to_string().contains(&outside_str),
			"err1 leaked outside path: {err1}"
		);

		// 2. alternates outside
		let repo_b = outside.join("b");
		run_git(&repo_b, &["init", "-q", "-b", "main"]);
		std::fs::write(repo_b.join("f"), "b").unwrap();
		run_git(&repo_b, &["add", "f"]);
		run_git(&repo_b, &["commit", "-qm", "b"]);
		let repo_a = share.join("repo_a");
		run_git(
			dir.path(),
			&[
				"clone",
				"--shared",
				repo_b.to_str().unwrap(),
				repo_a.to_str().unwrap(),
			],
		);
		let err2 = LocalRepo::open_within(&repo_a, &share, &read).unwrap_err();
		assert!(
			!err2.to_string().contains(&outside_str),
			"err2 leaked outside path: {err2}"
		);

		// 3. linked worktree outside
		let main_repo = outside.join("main");
		run_git(&main_repo, &["init", "-q", "-b", "main"]);
		std::fs::write(main_repo.join("f"), "m").unwrap();
		run_git(&main_repo, &["add", "f"]);
		run_git(&main_repo, &["commit", "-qm", "m"]);
		let wt = share.join("wt_outside");
		run_git(&main_repo, &["worktree", "add", wt.to_str().unwrap()]);
		let err3 = LocalRepo::open_within(&wt, &share, &read).unwrap_err();
		assert!(
			!err3.to_string().contains(&outside_str),
			"err3 leaked outside path: {err3}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn boundary_read_working_refuses_a_symlink_out() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let secret_file = outside.join("secret.txt");
		std::fs::write(&secret_file, "TOPSECRET\n").unwrap();

		let repo = share.join("repo");
		run_git(&repo, &["init", "-q", "-b", "main"]);

		let tracked = repo.join("tracked.txt");
		std::fs::write(&tracked, "normal\n").unwrap();
		run_git(&repo, &["add", "tracked.txt"]);
		run_git(&repo, &["commit", "-qm", "init"]);

		std::fs::remove_file(&tracked).unwrap();
		std::os::unix::fs::symlink(&secret_file, &tracked).unwrap();

		let untracked_sym = repo.join("untracked_sym");
		std::os::unix::fs::symlink(&secret_file, &untracked_sym).unwrap();

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let repo_within = LocalRepo::open_within(&repo, &share, &read).unwrap();

		let err_text1 = repo_within
			.changed_file_text(
				&GitSource::Working,
				"untracked_sym",
				1024,
				&read,
			)
			.unwrap_err();
		assert!(
			matches!(err_text1, GitError::OutsideBoundary { what: "working file" }),
			"expected OutsideBoundary for untracked_sym text, got {err_text1:?}"
		);

		let err_text2 = repo_within
			.changed_file_text(&GitSource::Working, "tracked.txt", 1024, &read)
			.unwrap_err();
		assert!(
			matches!(
				err_text2,
				GitError::OutsideBoundary {
					what: "working file"
				}
			),
			"expected OutsideBoundary for tracked.txt text, got {err_text2:?}"
		);

		let err_prev1 = repo_within
			.preview(&GitSource::Working, "untracked_sym", None, &read)
			.unwrap_err();
		assert!(
			matches!(
				err_prev1,
				GitError::OutsideBoundary { what: "working file" }
					| GitError::Io(_)
			),
			"expected OutsideBoundary or Io refusal for untracked_sym preview, got {err_prev1:?}"
		);

		let err_prev2 = repo_within
			.preview(&GitSource::Working, "tracked.txt", None, &read)
			.unwrap_err();
		assert!(
			matches!(
				err_prev2,
				GitError::OutsideBoundary { what: "working file" }
					| GitError::Io(_)
			),
			"expected OutsideBoundary or Io refusal for tracked.txt preview, got {err_prev2:?}"
		);

		assert!(!err_text1.to_string().contains("TOPSECRET"));
		assert!(!err_text2.to_string().contains("TOPSECRET"));
		assert!(!err_prev1.to_string().contains("TOPSECRET"));
		assert!(!err_prev2.to_string().contains("TOPSECRET"));

		// Working preview reaches the file through browser::git_preview_with (membership check)
		// and read_changes (which validates boundary membership for GitSource::Working when a boundary is set).

		// The same repo opened WITHOUT boundary still reads it (local behaviour unchanged)
		let repo_open = LocalRepo::open(&repo, &read).unwrap();
		let open_text = repo_open
			.changed_file_text(&GitSource::Working, "tracked.txt", 1024, &read)
			.unwrap();
		assert_eq!(open_text.as_deref(), Some("TOPSECRET\n"));
	}

	#[cfg(unix)]
	#[test]
	fn fsmonitor_in_repo_config_does_not_run() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		std::fs::create_dir_all(&share).unwrap();

		let repo = share.join("repo");
		run_git(&repo, &["init", "-q", "-b", "main"]);
		std::fs::write(repo.join("f.txt"), "hello").unwrap();
		run_git(&repo, &["add", "f.txt"]);
		run_git(&repo, &["commit", "-qm", "init"]);
		std::fs::write(repo.join("untracked.txt"), "untracked").unwrap();

		let marker = dir.path().join("fsmonitor_ran.marker");
		let hook = repo.join("fsmonitor_hook.sh");
		std::fs::write(
			&hook,
			format!("#!/bin/sh\ntouch \"{}\"\nexit 0\n", marker.display()),
		)
		.unwrap();
		let mut perms = std::fs::metadata(&hook).unwrap().permissions();
		perms.set_mode(0o755);
		std::fs::set_permissions(&hook, perms).unwrap();

		run_git(&repo, &["config", "core.fsmonitor", hook.to_str().unwrap()]);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};

		// 1. Control run without boundary: git should invoke the fsmonitor hook.
		let repo_open = LocalRepo::open(&repo, &read).unwrap();
		let _ = repo_open.change_list(10, &read);

		if !marker.exists() {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"git did not run the fsmonitor control"
			);
			return;
		}

		// Delete the marker file produced by the control run.
		std::fs::remove_file(&marker).unwrap();

		// 2. Hardened run with boundary: git should NOT invoke the fsmonitor hook.
		let repo_within = LocalRepo::open_within(&repo, &share, &read).unwrap();
		let _ = repo_within.change_list(10, &read);

		assert!(
			!marker.exists(),
			"core.fsmonitor hook should not have run under boundary hardening"
		);
	}

	#[test]
	fn identify_repos_without_a_boundary_reports_identity_failure_as_before() {
		if !has_git() {
			return;
		}
		let temp = tempfile::tempdir().unwrap();
		let dir = dunce::canonicalize(temp.path()).unwrap();
		let repo = dir.join("repo");
		run_git(&repo, &["init", "-q", "-b", "main"]);
		std::fs::write(repo.join("f.txt"), "hello").unwrap();
		run_git(&repo, &["add", "f.txt"]);
		run_git(&repo, &["commit", "-qm", "init"]);

		// Point core.worktree to a nonexistent directory so that Git::open_with
		// succeeds (rev-parse --show-toplevel prints the configured path), but
		// RepoIdentity::resolve fails because git commands cannot run in that
		// nonexistent directory.
		let nonexistent = dir.join("nonexistent");
		run_git(
			&repo,
			&["config", "core.worktree", nonexistent.to_str().unwrap()],
		);

		let opts = RunOptions::default();
		let disc = vec![DiscoveredRepo {
			path: repo.clone(),
			marker: crate::workspace::GitMarker::Directory,
		}];
		let (list, errors) =
			identify_repos(disc, None, &ScanBudget::visits(usize::MAX), &opts);
		assert!(errors.is_empty(), "expected no scan errors, got {errors:?}");
		assert_eq!(list.len(), 1);
		let found = &list[0];
		assert_eq!(found.root, nonexistent);
		assert_eq!(found.kind, FoundKind::Main);
		assert!(found.identity.is_none());
		let err_msg = found.summary.as_ref().unwrap_err();
		assert!(
			err_msg.starts_with("repository identity: "),
			"expected 'repository identity: ...', got '{err_msg}'"
		);
	}

	#[cfg(unix)]
	#[test]
	fn partial_clone_missing_blob_is_an_error_not_a_fetch() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let check = std::process::Command::new("git")
			.args(["clone", "--filter=blob:none", "--help"])
			.output();
		if check.is_err() || !check.unwrap().status.success() {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"git partial clone is required when SNIP_REQUIRE_ALL_TESTS is set"
			);
			return;
		}

		let (tx, rx) = std::sync::mpsc::channel();
		let handle = std::thread::spawn(move || {
			let dir = tempfile::tempdir().unwrap();
			let src = dir.path().join("src");
			run_git(&src, &["init", "-q", "-b", "main"]);
			run_git(&src, &["config", "uploadpack.allowFilter", "true"]);
			run_git(&src, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
			std::fs::write(src.join("file1.txt"), "hello").unwrap();
			run_git(&src, &["add", "file1.txt"]);
			run_git(&src, &["commit", "-qm", "init"]);
			std::fs::write(src.join("file2.txt"), "missing blob content")
				.unwrap();
			run_git(&src, &["add", "file2.txt"]);
			run_git(&src, &["commit", "-qm", "commit2"]);

			let share = dir.path().join("share");
			std::fs::create_dir_all(&share).unwrap();
			let clone = share.join("clone");
			run_git(
				dir.path(),
				&[
					"clone",
					"--filter=blob:none",
					"--no-checkout",
					&format!("file://{}", src.display()),
					clone.to_str().unwrap(),
				],
			);
			std::fs::remove_dir_all(&src).unwrap();

			let read = Read {
				profile: ReadProfile::Interactive,
				cancel: None,
			};
			let repo = LocalRepo::open_within(&clone, &share, &read).unwrap();
			let blob_res = repo.commit_blob("HEAD", "file2.txt", 1024, &read);
			assert!(blob_res.is_err());
			let prev_res = repo.preview(
				&GitSource::Commit("HEAD".into()),
				"file2.txt",
				None,
				&read,
			);
			assert!(prev_res.is_err());
			tx.send(()).unwrap();
		});

		assert!(
			rx.recv_timeout(std::time::Duration::from_secs(60)).is_ok(),
			"partial clone missing blob test timed out after 60s"
		);
		handle.join().unwrap();
	}

	#[test]
	fn served_local_repo_uses_the_served_pool_and_caps_stdout() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		std::fs::write(root.join("f.txt"), "hello\n").unwrap();
		run_git(root, &["add", "f.txt"]);
		run_git(root, &["commit", "-qm", "init"]);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let repo_served = LocalRepo::open_within(root, root, &read).unwrap();
		let opts_served = repo_served.opts(&read);
		assert_eq!(opts_served.pool, GitPool::Served);
		assert!(opts_served.max_stdout <= SERVED_MAX_STDOUT);

		let identity = repo_served.identity().unwrap();
		let repo_known_within = LocalRepo::known_within(identity, root);
		let opts_known_within = repo_known_within.opts(&read);
		assert_eq!(opts_known_within.pool, GitPool::Served);
		assert!(opts_known_within.max_stdout <= SERVED_MAX_STDOUT);

		let repo_known = LocalRepo::known(identity);
		let opts_known = repo_known.opts(&read);
		assert_eq!(opts_known.pool, GitPool::Local);
		assert_eq!(
			opts_known.max_stdout,
			read.profile.options(None).max_stdout
		);
	}

	#[test]
	fn served_preview_ignores_listed_parents() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		std::fs::write(root.join("f.txt"), "line1\n").unwrap();
		run_git(root, &["add", "f.txt"]);
		run_git(root, &["commit", "-qm", "first"]);
		std::fs::write(root.join("f.txt"), "line1\nline2\n").unwrap();
		run_git(root, &["add", "f.txt"]);
		run_git(root, &["commit", "-qm", "second"]);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let repo_served = LocalRepo::open_within(root, root, &read).unwrap();
		let git = Git::open_with(root, &read.profile.options(None)).unwrap();
		let sha = git
			.resolve_commit_with("HEAD", &read.profile.options(None))
			.unwrap();

		let bogus_marker = tempfile::tempdir()
			.unwrap()
			.path()
			.join("snip_bogus_output");
		let bogus_parent = format!("--output={}", bogus_marker.display());
		let bogus_parents = vec![bogus_parent];

		let res = repo_served
			.preview(
				&GitSource::Commit(sha.clone()),
				"f.txt",
				Some((ChangeType::Modified, Some(&bogus_parents))),
				&read,
			)
			.unwrap();
		assert!(res.patch.contains("+line2"));
		assert!(!bogus_marker.exists());

		let repo_local = LocalRepo::open(root, &read).unwrap();
		let local_res = repo_local.preview(
			&GitSource::Commit(sha.clone()),
			"f.txt",
			Some((ChangeType::Modified, Some(&bogus_parents))),
			&read,
		);
		let direct_res = crate::browser::git_preview_for(
			&git,
			&GitSource::Commit(sha),
			"f.txt",
			ChangeType::Modified,
			Some(&bogus_parents),
			&read.profile.options(None),
		);
		match (&local_res, &direct_res) {
			(Ok(a), Ok(b)) => assert_eq!(a, b),
			(Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
			_ => panic!("local_res {local_res:?} != direct_res {direct_res:?}"),
		}
	}

	#[test]
	fn known_within_runs_no_process_and_keeps_the_boundary() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		run_git(root, &["init", "-q", "-b", "main"]);
		std::fs::write(root.join("f.txt"), "hello\n").unwrap();
		run_git(root, &["add", "f.txt"]);
		run_git(root, &["commit", "-qm", "init"]);

		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let repo_open = LocalRepo::open_within(root, root, &read).unwrap();
		let identity = repo_open.identity().unwrap().clone();

		let repo_known = LocalRepo::known_within(&identity, root);
		assert!(repo_known.is_served());
		let b = repo_known.git().boundary().unwrap();
		assert_eq!(b, dunce::canonicalize(root).unwrap());

		let cmd = repo_known.git().command();
		let args: Vec<_> = cmd.get_args().collect();
		assert!(args.len() >= 4);
		assert_eq!(args[0], "-c");
		assert_eq!(args[1], "core.fsmonitor=false");
	}

	#[test]
	fn identify_repos_with_a_boundary_keeps_refused_repos_as_error_rows() {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();
		let outside_str = outside.to_string_lossy().into_owned();

		// 1. Good repo
		let good = share.join("good");
		run_git(&good, &["init", "-q", "-b", "main"]);
		std::fs::write(good.join("f"), "ok").unwrap();
		run_git(&good, &["add", "f"]);
		run_git(&good, &["commit", "-qm", "init"]);

		// 2. Shared clone (alternates outside)
		let repo_b = outside.join("b");
		run_git(&repo_b, &["init", "-q", "-b", "main"]);
		std::fs::write(repo_b.join("f"), "b").unwrap();
		run_git(&repo_b, &["add", "f"]);
		run_git(&repo_b, &["commit", "-qm", "init"]);
		let shared_clone = share.join("shared_clone");
		run_git(
			dir.path(),
			&[
				"clone",
				"--shared",
				repo_b.to_str().unwrap(),
				shared_clone.to_str().unwrap(),
			],
		);

		// 3. Outside worktree
		let main_repo = outside.join("main");
		run_git(&main_repo, &["init", "-q", "-b", "main"]);
		std::fs::write(main_repo.join("f"), "m").unwrap();
		run_git(&main_repo, &["add", "f"]);
		run_git(&main_repo, &["commit", "-qm", "init"]);
		let outside_wt = share.join("outside_wt");
		run_git(
			&main_repo,
			&["worktree", "add", outside_wt.to_str().unwrap()],
		);

		// 4. Folder with empty .git
		let empty_git = share.join("empty_git");
		std::fs::create_dir_all(empty_git.join(".git")).unwrap();

		let budget = ScanBudget::visits(usize::MAX);
		let opts = RunOptions::default();
		let mut disc = Discovery::new(&share, 3, 10).unwrap();
		let page = disc.next_page(&budget);

		let (bounded_list, _errors) =
			identify_repos(page.repos.clone(), Some(&share), &budget, &opts);
		assert_eq!(bounded_list.len(), 4);

		let good_row = bounded_list.iter().find(|r| r.name == "good").unwrap();
		assert!(good_row.summary.is_ok());

		let shared_row = bounded_list
			.iter()
			.find(|r| r.name == "shared_clone")
			.unwrap();
		assert!(shared_row.summary.is_err());
		let shared_err = shared_row.summary.as_ref().unwrap_err();
		assert!(!shared_err.contains(&outside_str));

		let wt_row = bounded_list
			.iter()
			.find(|r| r.name == "outside_wt")
			.unwrap();
		assert!(wt_row.summary.is_err());
		let wt_err = wt_row.summary.as_ref().unwrap_err();
		assert!(!wt_err.contains(&outside_str));

		let empty_row =
			bounded_list.iter().find(|r| r.name == "empty_git").unwrap();
		assert!(empty_row.summary.is_err());
		let empty_err = empty_row.summary.as_ref().unwrap_err();
		assert!(!empty_err.contains(&outside_str));

		// Boundary-less call over the same list
		let (unbounded_list, _) =
			identify_repos(page.repos, None, &budget, &opts);
		assert_eq!(unbounded_list.len(), 4);
		let unbounded_good =
			unbounded_list.iter().find(|r| r.name == "good").unwrap();
		assert!(unbounded_good.summary.is_ok());
	}

	#[test]
	fn identify_repos_with_a_boundary_drops_uninitialized_submodule_paths_outside(
	) {
		if !has_git() {
			return;
		}
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let repo = share.join("main");
		run_git(&repo, &["init", "-q", "-b", "main"]);
		std::fs::write(repo.join("f.txt"), "hello").unwrap();
		run_git(&repo, &["add", "f.txt"]);
		run_git(&repo, &["commit", "-qm", "init"]);

		#[cfg(unix)]
		std::os::unix::fs::symlink(&outside, repo.join("sub")).unwrap();
		#[cfg(windows)]
		let _ = std::os::windows::fs::symlink_dir(&outside, repo.join("sub"));

		let gitmodules = "[submodule \"escape\"]\n\tpath = sub\n\turl = https://example.invalid/escape.git\n";
		std::fs::write(repo.join(".gitmodules"), gitmodules).unwrap();

		let budget = ScanBudget::visits(usize::MAX);
		let opts = RunOptions::default();
		let disc = vec![DiscoveredRepo {
			path: repo.clone(),
			marker: crate::workspace::GitMarker::Directory,
		}];

		let (bounded_list, _) =
			identify_repos(disc.clone(), Some(&share), &budget, &opts);
		assert!(
			!bounded_list.iter().any(|r| r.name == "escape"),
			"uninitialized submodule with escape path must be dropped with boundary"
		);

		let (unbounded_list, _) = identify_repos(disc, None, &budget, &opts);
		assert!(
			unbounded_list.iter().any(|r| r.name == "escape"),
			"uninitialized submodule must be present without boundary"
		);
	}

	#[test]
	fn scan_repos_within_finds_repos_and_reports_refused_ones() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let alpha = share.join("alpha");
		run_git(&alpha, &["init", "-q", "-b", "main"]);
		std::fs::write(alpha.join("f"), "a").unwrap();
		run_git(&alpha, &["add", "f"]);
		run_git(&alpha, &["commit", "-qm", "a"]);

		let beta = share.join("beta");
		run_git(&beta, &["init", "-q", "-b", "main"]);
		std::fs::write(beta.join("f"), "b").unwrap();
		run_git(&beta, &["add", "f"]);
		run_git(&beta, &["commit", "-qm", "b"]);

		// Refused repo (core.worktree pointing outside)
		let refused = share.join("refused");
		run_git(&refused, &["init", "-q", "-b", "main"]);
		run_git(
			&refused,
			&["config", "core.worktree", outside.to_str().unwrap()],
		);

		// Plain folder
		std::fs::create_dir_all(share.join("plain")).unwrap();

		let budget = ScanBudget::visits(usize::MAX);
		let opts = RunOptions::default();
		let scan = scan_repos_within(&share, None, 3, 10, &budget, &opts);

		assert!(matches!(
			scan.status,
			ScanStatus::Complete | ScanStatus::Incomplete
		));
		assert!(scan
			.repos
			.iter()
			.any(|r| r.name == "alpha" && r.summary.is_ok()));
		assert!(scan
			.repos
			.iter()
			.any(|r| r.name == "beta" && r.summary.is_ok()));
		assert!(scan
			.repos
			.iter()
			.any(|r| r.name == "refused" && r.summary.is_err()));
	}

	#[test]
	fn scan_repos_within_returns_partial_on_deadline() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		std::fs::create_dir_all(&share).unwrap();

		let repo1 = share.join("repo1");
		run_git(&repo1, &["init", "-q", "-b", "main"]);
		std::fs::write(repo1.join("f"), "1").unwrap();
		run_git(&repo1, &["add", "f"]);
		run_git(&repo1, &["commit", "-qm", "1"]);

		let repo2 = share.join("repo2");
		run_git(&repo2, &["init", "-q", "-b", "main"]);
		std::fs::write(repo2.join("f"), "2").unwrap();
		run_git(&repo2, &["add", "f"]);
		run_git(&repo2, &["commit", "-qm", "2"]);

		let budget = ScanBudget {
			max_visited: usize::MAX,
			deadline: Some(
				std::time::Instant::now() + std::time::Duration::from_millis(5),
			),
			cancel: None,
		};
		let opts = RunOptions::default();
		let scan = scan_repos_within(&share, None, 3, 10, &budget, &opts);
		assert_eq!(scan.status, ScanStatus::TimedOut);
		assert_eq!(scan.repos.len(), 2);
		assert!(scan.repos.iter().any(|r| {
			r.summary
				.as_ref()
				.err()
				.is_some_and(|msg| msg.contains("not read: time limit"))
		}));
	}

	#[test]
	fn scan_repos_within_under_continues_a_depth_limited_branch() {
		if !has_git() {
			return;
		}
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let share = dir.path().join("share");
		let outside = dir.path().join("outside");
		std::fs::create_dir_all(&share).unwrap();
		std::fs::create_dir_all(&outside).unwrap();

		let deep_repo = share.join("l1/l2/deep_repo");
		std::fs::create_dir_all(&deep_repo).unwrap();
		run_git(&deep_repo, &["init", "-q", "-b", "main"]);
		std::fs::write(deep_repo.join("f"), "deep").unwrap();
		run_git(&deep_repo, &["add", "f"]);
		run_git(&deep_repo, &["commit", "-qm", "deep"]);

		let budget = ScanBudget::visits(usize::MAX);
		let opts = RunOptions::default();

		// max_depth = 1 so l1 is depth limited
		let scan1 = scan_repos_within(&share, None, 1, 10, &budget, &opts);
		assert!(scan1.repos.is_empty());
		assert!(!scan1.depth_limited.is_empty());
		let limited = &scan1.depth_limited[0];

		// Continuing with under = Some(limited) finds deep_repo
		let scan2 =
			scan_repos_within(&share, Some(limited), 3, 10, &budget, &opts);
		assert!(scan2
			.repos
			.iter()
			.any(|r| r.name == "deep_repo" && r.summary.is_ok()));

		// under outside the share -> error row, status Incomplete, no repos
		let scan_out =
			scan_repos_within(&share, Some(&outside), 3, 10, &budget, &opts);
		assert!(scan_out.repos.is_empty());
		assert_eq!(scan_out.errors.len(), 1);
		assert_eq!(scan_out.status, ScanStatus::Incomplete);
	}

	#[test]
	fn scan_notes_are_capped_with_overflow() {
		let _lock = SERVED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let level1 = root.join("level1");
		std::fs::create_dir_all(&level1).unwrap();
		for i in 0..70 {
			std::fs::create_dir(level1.join(format!("sub_{i}"))).unwrap();
		}
		let budget = ScanBudget::visits(usize::MAX);
		let opts = RunOptions::default();
		let scan = scan_repos_within(root, None, 1, 100, &budget, &opts);
		assert_eq!(scan.depth_limited.len(), MAX_SCAN_NOTES);
		assert_eq!(scan.depth_overflow, 70 - MAX_SCAN_NOTES);
	}
}
