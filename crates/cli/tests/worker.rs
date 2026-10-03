//! `snip worker` as its own process: a master pairs with the code it
//! prints, then lists and reads the shared folder over TLS.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use snip_remote::{pair, Client, Identity};

struct Kill(Child);

impl Drop for Kill {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Starts `snip worker` sharing `shared` with extra arguments; returns it
/// with the address and pairing code it printed.
fn start_worker_with(
	shared: &std::path::Path,
	config: &std::path::Path,
	extra_args: &[&str],
) -> (Kill, String, String) {
	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["worker", "--listen", "127.0.0.1:0", "--share"])
		.arg(shared)
		.args(extra_args)
		.env("SNIP_CONFIG_DIR", config)
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()
		.unwrap();
	let stdout = child.stdout.take().unwrap();
	let _guard = Kill(child);
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if tx.send(line).is_err() {
				break;
			}
		}
	});
	let (mut addr, mut code) = (None, None);
	while addr.is_none() || code.is_none() {
		let line = rx
			.recv_timeout(Duration::from_secs(30))
			.expect("snip worker printed no address and code within 30 s");
		if let Some(rest) = line.strip_prefix("snip-sync worker listening on ")
		{
			addr = Some(rest.trim().to_string());
		}
		if let Some(rest) = line.strip_prefix("pairing code ") {
			code = rest.split_whitespace().next().map(str::to_string);
		}
	}
	(_guard, addr.unwrap(), code.unwrap())
}

/// Starts `snip worker` sharing `shared`; returns it with the address and
/// pairing code it printed.
fn start_worker(
	shared: &std::path::Path,
	config: &std::path::Path,
) -> (Kill, String, String) {
	start_worker_with(shared, config, &[])
}

#[test]
fn cli_worker_pairs_and_serves_its_shared_folder() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir(&shared).unwrap();
	std::fs::write(shared.join("a.txt"), "hello from the worker\n").unwrap();
	let config = tmp.path().join("cfg");
	let (_worker, addr, code) = start_worker(&shared, &config);

	let master = Arc::new(Identity::generate().unwrap());
	let worker = pair(&addr, &code, &master, "mac").unwrap();
	assert!(
		config.join(snip_remote::TRUSTED_FILE).is_file(),
		"the worker keeps its pairings"
	);
	let client = Client::new(worker, master, "mac".into()).unwrap();
	let spaces = client.list_workspaces().unwrap();
	assert_eq!(spaces.len(), 1);
	assert_eq!(spaces[0].name, "proj");
	let (entries, _) = client.list_dir(&spaces[0].id, "").unwrap();
	assert_eq!(entries[0].name, "a.txt");
	assert_eq!(
		client.read(&spaces[0].id, "a.txt").unwrap().as_deref(),
		Some("hello from the worker\n")
	);
}

#[test]
fn cli_worker_needs_a_shared_folder() {
	let out = Command::new(env!("CARGO_BIN_EXE_snip"))
		.arg("worker")
		.output()
		.unwrap();
	assert_eq!(out.status.code(), Some(2));
}

/// `snip remote …` as a master with its own config folder, against a
/// `snip worker` process.
#[test]
fn cli_master_pairs_lists_stats_and_cats_through_the_worker() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir_all(shared.join("src")).unwrap();
	std::fs::write(shared.join("src/main.rs"), "fn main() {}\n").unwrap();
	std::fs::write(shared.join("bin.dat"), [0u8, 1, 2]).unwrap();
	let (_worker, addr, code) = start_worker(&shared, &tmp.path().join("w"));
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr, &code]).starts_with("paired with "));
	let workers = ok(&["workers"]);
	assert!(
		workers.starts_with("1\t") && workers.contains(&addr),
		"{workers}"
	);
	assert!(ok(&["workspaces", "1"]).contains("\tproj\t"));
	assert_eq!(ok(&["ls", "1", "proj"]), "src/\nbin.dat\n");
	assert_eq!(ok(&["ls", "1", "proj", "src"]), "main.rs\n");
	assert!(ok(&["stat", "1", "proj", "src/main.rs"]).starts_with("file\t13\t"));
	assert_eq!(ok(&["cat", "1", "proj", "src/main.rs"]), "fn main() {}\n");

	let (status, _, stderr) = snip(&["cat", "1", "proj", "bin.dat"]);
	assert_eq!(status, Some(1));
	assert!(stderr.contains("binary"), "{stderr}");
	let (status, _, stderr) = snip(&["cat", "1", "proj", "../secret"]);
	assert_eq!(status, Some(1), "{stderr}");
	// Pairing again from a trusted master is a no-op success; another
	// master finds the code used up.
	ok(&["pair", &addr, &code]);
	let other = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["remote", "pair", &addr, &code])
		.env("SNIP_CONFIG_DIR", tmp.path().join("other"))
		.output()
		.unwrap();
	assert_eq!(other.status.code(), Some(1));
	assert!(String::from_utf8_lossy(&other.stderr).contains("pairing failed"));

	assert!(ok(&["forget", "1"]).starts_with("forgot "));
	assert_eq!(ok(&["workers"]), "");
	let (status, _, _) = snip(&["workspaces", "1"]);
	assert_eq!(status, Some(1));
}

#[test]
fn cli_two_workers_paired_forget_and_stale_store_instance() {
	let tmp = tempfile::tempdir().unwrap();
	let shared1 = tmp.path().join("proj1");
	let shared2 = tmp.path().join("proj2");
	std::fs::create_dir_all(&shared1).unwrap();
	std::fs::create_dir_all(&shared2).unwrap();
	std::fs::write(shared1.join("a.txt"), "one\n").unwrap();
	std::fs::write(shared2.join("b.txt"), "two\n").unwrap();

	let (_w1, addr1, code1) = start_worker(&shared1, &tmp.path().join("w1"));
	let (_w2, addr2, code2) = start_worker(&shared2, &tmp.path().join("w2"));
	let master_cfg = tmp.path().join("master");

	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr1, &code1]).starts_with("paired with "));
	assert!(ok(&["pair", &addr2, &code2]).starts_with("paired with "));

	let workers = ok(&["workers"]);
	assert_eq!(workers.lines().count(), 2);
	assert!(workers.contains(&addr1));
	assert!(workers.contains(&addr2));

	// Stale-list scenario: a second process-less WorkerStore instance forgets a decoy
	// fingerprint without dropping the CLI pairings.
	let store = snip_remote::WorkerStore::in_config_dir(&master_cfg);
	let forgotten = store
		.forget(
			"abababababababababababababababababababababababababababababababab",
		)
		.unwrap();
	assert!(forgotten.is_none());

	let workers_after_stale = ok(&["workers"]);
	assert_eq!(workers_after_stale.lines().count(), 2);
	assert!(workers_after_stale.contains(&addr1));
	assert!(workers_after_stale.contains(&addr2));

	// Forget worker 1 (the more recent one, addr2).
	assert!(ok(&["forget", "1"]).starts_with("forgot "));
	let remaining = ok(&["workers"]);
	assert_eq!(remaining.lines().count(), 1);
	assert!(remaining.starts_with("1\t"));
	assert!(remaining.contains(&addr1));
	assert!(!remaining.contains(&addr2));
}

#[test]
fn cli_worker_max_protocol_flag_limits_negotiation() {
	use snip_core::gitview::ReadProfile;
	use snip_remote::proto::GitQuery;

	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir_all(&shared).unwrap();
	std::fs::write(shared.join("file.txt"), "hello").unwrap();

	// 1. Worker with --max-protocol 1: Git requests fail with WorkerTooOld, list_workspaces works
	let (_w1, addr1, code1) = start_worker_with(
		&shared,
		&tmp.path().join("w1"),
		&["--max-protocol", "1"],
	);
	let master1 = Arc::new(Identity::generate().unwrap());
	let paired1 = pair(&addr1, &code1, &master1, "mac").unwrap();
	let client1 = Client::new(paired1, master1, "mac".into()).unwrap();

	let spaces = client1.list_workspaces().unwrap();
	assert_eq!(spaces.len(), 1);
	let ws1 = spaces[0].id.clone();

	let err = client1
		.git(
			&ws1,
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();
	match err {
		snip_remote::RemoteError::WorkerTooOld { have, need, .. } => {
			assert_eq!(have, 1);
			assert_eq!(need, 2);
		}
		other => panic!("expected WorkerTooOld, got {other:?}"),
	}

	// 2. Worker with no flag: Git request reaches the worker and gets NotARepository (non-repo shared dir)
	let (_w2, addr2, code2) = start_worker(&shared, &tmp.path().join("w2"));
	let master2 = Arc::new(Identity::generate().unwrap());
	let paired2 = pair(&addr2, &code2, &master2, "mac").unwrap();
	let client2 = Client::new(paired2, master2, "mac".into()).unwrap();

	let spaces2 = client2.list_workspaces().unwrap();
	assert_eq!(spaces2.len(), 1);
	let ws2 = spaces2[0].id.clone();

	let err2 = client2
		.git(
			&ws2,
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();
	match err2 {
		snip_remote::RemoteError::Refused { code, .. } => {
			assert_eq!(code, snip_remote::proto::ErrorCode::NotARepository);
		}
		other => panic!("expected Refused(NotARepository), got {other:?}"),
	}
}

fn require_git() -> bool {
	match Command::new("git").arg("--version").output() {
		Ok(out) if out.status.success() => true,
		_ => {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"git is required"
			);
			false
		}
	}
}

fn git(dir: &std::path::Path, args: &[&str]) {
	let out = Command::new("git")
		.args(["-c", "user.name=t", "-c", "user.email=t@t"])
		.args(args)
		.current_dir(dir)
		.output()
		.expect("git failed to execute");
	assert!(
		out.status.success(),
		"git {args:?} failed in {}: {}",
		dir.display(),
		String::from_utf8_lossy(&out.stderr)
	);
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.args(["-c", "user.name=t", "-c", "user.email=t@t"])
		.args(args)
		.current_dir(dir)
		.output()
		.expect("git failed to execute");
	assert!(
		out.status.success(),
		"git {args:?} failed in {}: {}",
		dir.display(),
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).expect("git stdout not utf8")
}

#[test]
fn cli_master_git_views_repos_changes_log_show_and_diff() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let alpha = ws.join("alpha");
	let beta = ws.join("beta");
	let plain = ws.join("plain");
	std::fs::create_dir_all(&alpha).unwrap();
	std::fs::create_dir_all(&beta).unwrap();
	std::fs::create_dir_all(&plain).unwrap();
	std::fs::write(plain.join("file.txt"), "plain content\n").unwrap();

	// Repo alpha: two commits
	git(&alpha, &["init"]);
	std::fs::write(alpha.join("a.txt"), "commit 1 a\n").unwrap();
	git(&alpha, &["add", "a.txt"]);
	git(&alpha, &["commit", "-q", "-m", "first commit"]);

	std::fs::write(alpha.join("a.txt"), "commit 2 a\n").unwrap();
	std::fs::write(alpha.join("b.txt"), "commit 2 b\n").unwrap();
	git(&alpha, &["add", "a.txt", "b.txt"]);
	git(&alpha, &["commit", "-q", "-m", "second commit"]);

	// Then modify tracked a.txt, add untracked new.txt, git add staged.txt with new content
	std::fs::write(alpha.join("a.txt"), "commit 2 a modified\n").unwrap();
	std::fs::write(alpha.join("new.txt"), "new file content\n").unwrap();
	std::fs::write(alpha.join("staged.txt"), "staged file content\n").unwrap();
	git(&alpha, &["add", "staged.txt"]);

	// Repo beta: clean, one commit
	git(&beta, &["init"]);
	std::fs::write(beta.join("b.txt"), "beta content\n").unwrap();
	git(&beta, &["add", "b.txt"]);
	git(&beta, &["commit", "-q", "-m", "beta commit"]);

	let (_worker, addr, code) = start_worker(&ws, &tmp.path().join("w"));
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr, &code]).starts_with("paired with "));

	// repos 1 ws
	let repos_out = ok(&["repos", "1", "ws"]);
	assert!(repos_out.contains("alpha\t"));
	assert!(repos_out.contains("beta\t"));
	assert!(!repos_out.contains("plain"));
	let alpha_line = repos_out
		.lines()
		.find(|l| l.starts_with("alpha\t"))
		.expect("alpha line");
	let alpha_cols: Vec<&str> = alpha_line.split('\t').collect();
	assert_eq!(alpha_cols.len(), 6, "{alpha_line}");
	assert_eq!(alpha_cols[2], "1", "staged");
	assert_eq!(alpha_cols[3], "1", "unstaged");
	assert_eq!(alpha_cols[4], "1", "untracked");
	assert_eq!(alpha_cols[5], "0", "conflicted");

	// changes 1 ws alpha
	let changes_alpha = ok(&["changes", "1", "ws", "alpha"]);
	assert!(changes_alpha.contains("U\tM\ta.txt"), "{changes_alpha}");
	assert!(changes_alpha.contains("W\tN\tnew.txt"), "{changes_alpha}");
	assert!(
		changes_alpha.contains("S\tN\tstaged.txt"),
		"{changes_alpha}"
	);

	// changes 1 ws beta
	let changes_beta = ok(&["changes", "1", "ws", "beta"]);
	assert_eq!(changes_beta, "");

	// log 1 ws alpha -n 50
	let log_alpha = ok(&["log", "1", "ws", "alpha", "-n", "50"]);
	let log_lines: Vec<&str> = log_alpha.lines().collect();
	assert_eq!(log_lines.len(), 2, "{log_alpha}");
	let log_shas: std::collections::BTreeSet<&str> = log_lines
		.iter()
		.map(|l| l.split('\t').next().unwrap())
		.collect();
	let rev_list = git_out(&alpha, &["rev-list", "HEAD"]);
	let rev_shas: std::collections::BTreeSet<&str> = rev_list.lines().collect();
	assert_eq!(log_shas, rev_shas);
	assert!(log_lines[0].ends_with("second commit"), "{log_alpha}");

	// show 1 ws alpha <HEAD sha>
	let head_sha = git_out(&alpha, &["rev-parse", "HEAD"]).trim().to_string();
	let show_out = ok(&["show", "1", "ws", "alpha", &head_sha]);
	let diff_tree = git_out(
		&alpha,
		&["diff-tree", "--no-commit-id", "--name-status", "-r", "HEAD"],
	);
	let expected_show: String = diff_tree
		.lines()
		.map(|line| {
			if let Some(rest) = line.strip_prefix("A\t") {
				format!("N\t{rest}")
			} else {
				line.to_string()
			}
		})
		.collect::<Vec<_>>()
		.join("\n")
		+ "\n";
	assert_eq!(show_out, expected_show);
	assert!(show_out.contains("N\tb.txt"), "{show_out}");

	// diff
	let diff_a = ok(&["diff", "1", "ws", "alpha", "a.txt"]);
	assert!(diff_a.contains("commit 2 a modified"), "{diff_a}");

	let diff_staged =
		ok(&["diff", "1", "ws", "alpha", "staged.txt", "--staged"]);
	assert!(diff_staged.contains("staged file content"), "{diff_staged}");

	let diff_commit =
		ok(&["diff", "1", "ws", "alpha", "a.txt", "--commit", &head_sha]);
	assert!(diff_commit.contains("commit 2 a"), "{diff_commit}");

	// changes 1 ws plain
	let (status, _, stderr) = snip(&["changes", "1", "ws", "plain"]);
	assert_eq!(status, Some(1));
	assert!(
		!stderr.is_empty(),
		"expected worker's NotARepository error in stderr"
	);
}

#[test]
fn cli_master_git_views_single_repo_share_uses_the_empty_repo_path() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("single_repo");
	std::fs::create_dir_all(&ws).unwrap();
	git(&ws, &["init"]);
	std::fs::write(ws.join("init.txt"), "hello single\n").unwrap();
	git(&ws, &["add", "init.txt"]);
	git(&ws, &["commit", "-q", "-m", "initial single"]);

	let (_worker, addr, code) = start_worker(&ws, &tmp.path().join("w"));
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr, &code]).starts_with("paired with "));

	let repos_out = ok(&["repos", "1", "single_repo"]);
	let first_line = repos_out.lines().next().expect("repos line");
	assert_eq!(first_line.split('\t').next(), Some("."));

	let changes_out = ok(&["changes", "1", "single_repo"]);
	assert_eq!(changes_out, "");

	let head_sha = git_out(&ws, &["rev-parse", "HEAD"]).trim().to_string();
	let show_out = ok(&["show", "1", "single_repo", &head_sha]);
	assert!(show_out.contains("N\tinit.txt"), "{show_out}");
}

#[test]
fn cli_master_repos_of_a_non_repo_share_says_so() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("plain_share");
	std::fs::create_dir_all(&ws).unwrap();
	std::fs::write(ws.join("note.txt"), "just a note\n").unwrap();

	let (_worker, addr, code) = start_worker(&ws, &tmp.path().join("w"));
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};

	let out = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["remote", "pair", &addr, &code])
		.env("SNIP_CONFIG_DIR", &master_cfg)
		.output()
		.unwrap();
	assert_eq!(out.status.code(), Some(0));

	let (status, stdout, stderr) = snip(&["repos", "1", "plain_share"]);
	assert_eq!(status, Some(0));
	assert_eq!(stdout, "");
	assert!(
		stderr.contains("no Git repository in"),
		"expected 'no Git repository in' in stderr, got: {stderr}"
	);
}

#[test]
fn cli_master_git_views_refuse_an_old_worker() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir_all(&shared).unwrap();
	std::fs::write(shared.join("file.txt"), "hello").unwrap();

	let (_worker, addr, code) = start_worker_with(
		&shared,
		&tmp.path().join("w"),
		&["--max-protocol", "1"],
	);
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr, &code]).starts_with("paired with "));

	let (status_repos, _, stderr_repos) = snip(&["repos", "1", "proj"]);
	assert_eq!(status_repos, Some(1));
	assert!(
		stderr_repos.contains("too old"),
		"expected 'too old' in stderr, got: {stderr_repos}"
	);

	let (status_changes, _, stderr_changes) = snip(&["changes", "1", "proj"]);
	assert_eq!(status_changes, Some(1));
	assert!(
		stderr_changes.contains("too old"),
		"expected 'too old' in stderr, got: {stderr_changes}"
	);

	assert_eq!(snip(&["workspaces", "1"]).0, Some(0));
	assert_eq!(snip(&["ls", "1", "proj"]).0, Some(0));
}

#[test]
fn cli_master_git_view_rejects_escaping_paths() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let alpha = ws.join("alpha");
	std::fs::create_dir_all(&alpha).unwrap();
	git(&alpha, &["init"]);
	std::fs::write(alpha.join("a.txt"), "content\n").unwrap();
	git(&alpha, &["add", "a.txt"]);
	git(&alpha, &["commit", "-q", "-m", "init"]);

	let (_worker, addr, code) = start_worker(&ws, &tmp.path().join("w"));
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr, &code]).starts_with("paired with "));

	let (status_diff, _, _) =
		snip(&["diff", "1", "ws", "alpha", "../../secret.txt"]);
	assert_eq!(status_diff, Some(1));

	let (status_show, _, _) =
		snip(&["show", "1", "ws", "alpha", "--", "--output=x"]);
	assert_eq!(status_show, Some(1));
}
