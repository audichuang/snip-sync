//! Git inspection, change listing, commit details and repository discovery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::format::ChangeType;
use crate::gitrun::RunOptions;
use crate::gitsrc::{self, ChangedPaths, Git, GitError, GitSource};
use crate::workspace::{
	declared_submodules, status_details, summarize, summarize_with_details,
	summarize_with_identity, ChangeCounts, DiscoveredRepo, RepoIdentity,
	RepoKind, RepoSummary, ScanBudget, ScanStatus, SubmoduleState,
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

/// Checks `budget` (deadline, cancel) between repositories; repos it did not
/// reach are returned as error rows "not read: time limit", never dropped.
pub fn identify_repos(
	found: Vec<DiscoveredRepo>,
	budget: &ScanBudget,
	opts: &RunOptions,
) -> (Vec<FoundRepo>, Vec<(PathBuf, String)>) {
	let mut seen_identities: HashSet<(PathBuf, PathBuf)> = HashSet::new();
	let mut seen_roots: HashSet<PathBuf> = HashSet::new();
	let mut list = Vec::new();
	let mut errors = Vec::new();

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

		match Git::open_with(&r.path, opts) {
			Ok(git) => {
				let id_res = RepoIdentity::resolve(&git, opts);
				let (kind, identity) = match &id_res {
					Ok(id) => {
						// Symlink alias dedup:
						if !seen_identities
							.insert((id.toplevel.clone(), id.git_dir.clone()))
						{
							continue;
						}
						seen_roots.insert(canonical_path.clone());
						let k = match id.kind {
							RepoKind::LinkedWorktree => {
								FoundKind::LinkedWorktree
							}
							RepoKind::Submodule => FoundKind::Submodule,
							RepoKind::Main => FoundKind::Main,
						};
						(k, Some(id.clone()))
					}
					Err(err) => {
						let root = git.root().to_path_buf();
						if !seen_roots.insert(root.clone()) {
							continue;
						}
						list.push(FoundRepo {
							root,
							name,
							kind: FoundKind::Main,
							identity: None,
							summary: Err(format!("repository identity: {err}")),
						});
						continue;
					}
				};

				let root = identity
					.as_ref()
					.map(|id| id.toplevel.clone())
					.unwrap_or_else(|| git.root().to_path_buf());
				let name = root
					.file_name()
					.map(|n| n.to_string_lossy().into_owned())
					.unwrap_or(name);
				// The identity was just resolved; reuse it.
				let summary = match &identity {
					Some(id) => summarize_with_identity(&git, id, opts),
					None => summarize(&git, opts),
				}
				.map_err(|e| e.to_string());

				list.push(FoundRepo {
					root: root.clone(),
					name,
					kind,
					identity,
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
									"Submodule not checked out (uninitialized)"
										.into(),
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
								summary: Err(format!(
									"Submodule unreadable: {reason}"
								)),
							});
						}
						_ => {}
					}
				}
			}
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
			}
		}
	}

	(list, errors)
}

pub fn identify_repo(path: &Path, opts: &RunOptions) -> FoundRepo {
	let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
	let (root, kind, identity, summary) = match Git::open_with(&path, opts) {
		Ok(git) => {
			let root = git.root().to_path_buf();
			match RepoIdentity::resolve(&git, opts) {
				Ok(id) => {
					let kind = match id.kind {
						RepoKind::LinkedWorktree => FoundKind::LinkedWorktree,
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
		Err(err) => (path, FoundKind::Main, None, Err(err.to_string())),
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

#[cfg(test)]
mod tests {
	use super::*;

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
			identify_repos(disc, &ScanBudget::visits(usize::MAX), &opts);

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
			let alias_repo = identify_repo(&root.join("main-alias"), &opts);
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
			identify_repos(disc, &budget, &RunOptions::default());
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
}
