//! Exact commit selection: historical tips, gaps, and the strict clipboard cap.
//!
//! The selector resolves every ref with the caller's `RunOptions`. A cap is
//! the whole clipboard document. Commits are not dropped to finish under it.

use std::time::Duration;

use snip_core::commits::{
	to_clipboard_text, CommitError, CommitRecord, CommitsPayload,
};
use snip_core::gitrun::{CancelToken, Overflow, RunOptions};
use snip_core::gitsrc::{Git, GitError};
use snip_core::transfer::{
	plan_commit_export_exact, plan_commit_export_exact_with, TransferError,
};

struct Repo {
	_dir: tempfile::TempDir,
	repo_path: std::path::PathBuf,
	cfg: std::path::PathBuf,
}

impl Repo {
	fn new(name: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let cfg = dir.path().join("empty.gitconfig");
		std::fs::write(&cfg, "").unwrap();
		let raw = dir.path().join(name);
		std::fs::create_dir_all(&raw).unwrap();
		let repo_path = dunce::canonicalize(&raw).unwrap_or(raw);
		let repo = Self {
			_dir: dir,
			repo_path,
			cfg,
		};
		repo.git(&["init", "-q", "-b", "main"]);
		repo.git(&["config", "user.name", "Test User"]);
		repo.git(&["config", "user.email", "test@example.com"]);
		repo.git(&["config", "core.autocrlf", "false"]);
		repo.git(&["config", "commit.gpgsign", "false"]);
		repo
	}

	fn git(&self, args: &[&str]) -> String {
		let out = std::process::Command::new("git")
			.args(args)
			.current_dir(&self.repo_path)
			.env("GIT_CONFIG_GLOBAL", &self.cfg)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("LC_ALL", "C")
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"git {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8_lossy(&out.stdout).trim().to_string()
	}

	fn write(&self, rel: &str, content: &str) {
		let target = self.repo_path.join(rel);
		if let Some(parent) = target.parent() {
			std::fs::create_dir_all(parent).unwrap();
		}
		std::fs::write(target, content).unwrap();
	}

	fn commit(&self, msg: &str) -> String {
		self.git(&["add", "-A"]);
		self.git(&["commit", "-q", "-m", msg]);
		self.git(&["rev-parse", "HEAD"])
	}

	fn open(&self) -> Git {
		Git::open(&self.repo_path).unwrap()
	}
}

fn empty_document_len(count: usize) -> usize {
	let empty = CommitRecord {
		message: String::new(),
		author_name: String::new(),
		author_email: String::new(),
		author_date: String::new(),
		files: Vec::new(),
	};
	to_clipboard_text(&CommitsPayload {
		commits: vec![empty; count],
	})
	.len()
}

fn payload_limit(err: TransferError) -> (usize, usize) {
	match err {
		TransferError::Commit(CommitError::PayloadLimit { limit, actual }) => {
			(limit, actual)
		}
		other => panic!("expected PayloadLimit, got {other}"),
	}
}

#[test]
fn exact_root_at_historical_tip_is_not_replaced_by_head() {
	let repo = Repo::new("historical-root");
	repo.write("root.txt", "root\n");
	let root = repo.commit("root");
	repo.write("main.txt", "main\n");
	let head = repo.commit("head");
	let git = repo.open();
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
	assert_ne!(root, head);

	let only_root =
		plan_commit_export_exact(&git, &root, std::slice::from_ref(&root))
			.unwrap();
	assert_eq!(only_root.commits.len(), 1);
	assert!(only_root.commits[0].message.starts_with("root"));
	assert!(only_root.commits[0]
		.files
		.iter()
		.any(|f| f.path == "root.txt"));
	assert!(!only_root.commits[0]
		.files
		.iter()
		.any(|f| f.path == "main.txt"));

	let repeated =
		plan_commit_export_exact(&git, &root, &[root.clone(), root.clone()])
			.unwrap();
	assert_eq!(repeated, only_root);

	repo.git(&["checkout", "-q", "--detach", &root]);
	assert_eq!(repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD");
	let detached = repo.open();
	let from_detached = plan_commit_export_exact(
		&detached,
		&head,
		&[head.clone(), root.clone()],
	)
	.unwrap();
	assert_eq!(from_detached.commits.len(), 2);
	assert!(from_detached.commits[0].message.starts_with("root"));
	assert!(from_detached.commits[1].message.starts_with("head"));
	assert_ne!(repo.git(&["rev-parse", "HEAD"]), head);
}

#[test]
fn exact_gaps_and_detached_side_commits_are_refused() {
	let repo = Repo::new("gaps");
	repo.write("root.txt", "root\n");
	let root = repo.commit("root");
	repo.write("main.txt", "main\n");
	let mid = repo.commit("mid");
	repo.write("tip.txt", "tip\n");
	let tip = repo.commit("tip");
	repo.git(&["checkout", "-q", "-b", "side", &root]);
	repo.write("side.txt", "side\n");
	let side = repo.commit("side");
	repo.git(&["checkout", "-q", "--detach", &mid]);
	let git = repo.open();
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), mid);

	let empty = plan_commit_export_exact(&git, &tip, &[]).unwrap_err();
	assert!(matches!(empty, TransferError::EmptySelection), "{empty}");

	let gap =
		plan_commit_export_exact(&git, &tip, &[tip.clone(), root.clone()])
			.unwrap_err();
	assert!(
		matches!(gap, TransferError::DiscontinuousCommits { .. }),
		"{gap}"
	);

	let cross = plan_commit_export_exact(
		&git,
		&tip,
		&[tip.clone(), side.clone(), root.clone()],
	)
	.unwrap_err();
	assert!(
		matches!(cross, TransferError::DiscontinuousCommits { .. }),
		"{cross}"
	);

	let missing_tip =
		plan_commit_export_exact(&git, &tip, &[mid.clone(), root.clone()])
			.unwrap_err();
	assert!(
		matches!(missing_tip, TransferError::DiscontinuousCommits { .. }),
		"{missing_tip}"
	);

	let side_chain =
		plan_commit_export_exact(&git, &side, &[root.clone(), side.clone()])
			.unwrap();
	assert_eq!(side_chain.commits.len(), 2);
	assert!(side_chain.commits[1]
		.files
		.iter()
		.any(|f| f.path == "side.txt"));
	assert!(!side_chain
		.commits
		.iter()
		.any(|c| c.files.iter().any(|f| f.path == "tip.txt")));
	assert_eq!(repo.git(&["rev-parse", "HEAD"]), mid);
}

#[test]
fn exact_cap_counts_the_whole_document_and_keeps_every_commit() {
	let repo = Repo::new("cap-boundary");
	repo.write("a.txt", "alpha \"quote\"\n");
	let root = repo.commit("root");
	repo.write("b.txt", "beta\n");
	let tip = repo.commit("tip");
	let git = repo.open();
	let selected = vec![root.clone(), tip.clone()];

	let legacy = plan_commit_export_exact(&git, &tip, &selected).unwrap();
	let text = to_clipboard_text(&legacy);
	assert!(text.starts_with("// snip-sync commits v1\n{"));
	assert_eq!(legacy.commits.len(), 2);

	let fitted = plan_commit_export_exact_with(
		&git,
		&tip,
		&selected,
		&RunOptions::default(),
		text.len(),
	)
	.unwrap();
	assert_eq!(fitted.payload, legacy);
	assert_eq!(fitted.text, text);
	assert_eq!(fitted.text.len(), text.len());

	let err = plan_commit_export_exact_with(
		&git,
		&tip,
		&selected,
		&RunOptions::default(),
		text.len() - 1,
	)
	.unwrap_err();
	let (limit, actual) = payload_limit(err);
	assert_eq!(limit, text.len() - 1);
	assert!(actual > limit, "actual {actual} limit {limit}");
}

#[test]
fn exact_cap_stops_before_the_rest_of_the_caller_list() {
	let repo = Repo::new("cap-admit");
	repo.write("a.txt", "alpha\n");
	let root = repo.commit("root");
	repo.write("b.txt", "beta\n");
	let tip = repo.commit("tip");
	let git = repo.open();

	let too_small = plan_commit_export_exact_with(
		&git,
		&tip,
		&["not-a-commit".into(), root.clone()],
		&RunOptions::default(),
		0,
	)
	.unwrap_err();
	let (limit, actual) = payload_limit(too_small);
	assert_eq!(limit, 0);
	assert!(actual >= empty_document_len(1));

	let one = empty_document_len(1);
	let err = plan_commit_export_exact_with(
		&git,
		&tip,
		&[root, tip.clone(), "not-a-commit".into()],
		&RunOptions::default(),
		one,
	)
	.unwrap_err();
	let (limit, actual) = payload_limit(err);
	assert_eq!(limit, one);
	assert!(actual > limit);
}

#[test]
fn exact_truncated_stdout_is_not_a_commit_id() {
	let repo = Repo::new("trunc-id");
	repo.write("a.txt", "alpha\n");
	let root = repo.commit("root");
	let git = repo.open();
	let opts = RunOptions {
		max_stdout: 1,
		overflow: Overflow::Truncate,
		timeout: Duration::from_secs(30),
		..RunOptions::default()
	};
	let err = plan_commit_export_exact_with(
		&git,
		&root,
		std::slice::from_ref(&root),
		&opts,
		usize::MAX,
	)
	.unwrap_err();
	assert!(
		matches!(err, TransferError::Git(GitError::OutputLimit { .. })),
		"{err}"
	);
}

#[test]
fn exact_precancel_does_not_export() {
	let repo = Repo::new("precancel-exact");
	repo.write("a.txt", "alpha\n");
	let root = repo.commit("root");
	let git = repo.open();
	let token = CancelToken::new();
	token.cancel();
	let opts = RunOptions {
		cancel: Some(token),
		timeout: Duration::from_secs(30),
		..RunOptions::default()
	};
	let err = plan_commit_export_exact_with(
		&git,
		&root,
		std::slice::from_ref(&root),
		&opts,
		usize::MAX,
	)
	.unwrap_err();
	assert!(
		matches!(err, TransferError::Git(GitError::Cancelled { .. })),
		"{err}"
	);
}
