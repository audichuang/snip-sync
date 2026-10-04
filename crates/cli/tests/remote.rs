//! `snip serve --stdio` and `snip remote …` as their own processes. The
//! master starts the worker through `SNIP_REMOTE_EXEC` (the built `snip
//! serve --stdio`) instead of ssh, so any host name reaches it.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long one `snip` process may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(120);

/// The worker command a master runs: the built binary, no ssh. The path
/// is split on whitespace, which the CI checkouts never contain.
fn serve_exec(extra: &str) -> String {
	let bin = env!("CARGO_BIN_EXE_snip");
	assert!(
		!bin.contains(char::is_whitespace),
		"SNIP_REMOTE_EXEC splits on whitespace; the snip binary path has \
		 some: {bin}"
	);
	format!("{bin} serve --stdio {extra}")
		.trim_end()
		.to_string()
}

/// Runs `cmd` to completion, failing the test after [`DEADLINE`].
fn output(mut cmd: Command, what: &str) -> (Option<i32>, String, String) {
	let child = cmd
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap_or_else(|err| panic!("cannot start {what}: {err}"));
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		let _ = tx.send(child.wait_with_output());
	});
	let out = rx
		.recv_timeout(DEADLINE)
		.unwrap_or_else(|_| panic!("{what} did not finish within {DEADLINE:?}"))
		.unwrap();
	(
		out.status.code(),
		String::from_utf8_lossy(&out.stdout).into_owned(),
		String::from_utf8_lossy(&out.stderr).into_owned(),
	)
}

/// A master: `snip remote …` with its own config folder and worker
/// command.
struct Master {
	exec: String,
	config: PathBuf,
	home: Option<PathBuf>,
}

impl Master {
	fn new(tmp: &Path) -> Self {
		Self::with_exec(tmp, serve_exec(""))
	}

	fn with_exec(tmp: &Path, exec: String) -> Self {
		Self {
			exec,
			config: tmp.join("cfg"),
			home: None,
		}
	}

	fn snip(&self, args: &[&str]) -> (Option<i32>, String, String) {
		let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip"));
		cmd.arg("remote")
			.args(args)
			.env("SNIP_CONFIG_DIR", &self.config)
			.env("SNIP_REMOTE_EXEC", &self.exec);
		if let Some(home) = &self.home {
			cmd.env("HOME", home).env("USERPROFILE", home);
		}
		output(cmd, &format!("snip remote {args:?}"))
	}

	fn ok(&self, args: &[&str]) -> String {
		let (status, stdout, stderr) = self.snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	}

	fn fails(&self, args: &[&str]) -> String {
		let (status, stdout, stderr) = self.snip(args);
		assert_eq!(
			status,
			Some(1),
			"snip remote {args:?} should fail: {stdout}{stderr}"
		);
		stderr
	}
}

fn s(path: &Path) -> &str {
	path.to_str().expect("temp paths are UTF-8")
}

#[test]
fn serve_stdio_prints_the_preamble_and_ends_when_stdin_closes() {
	let tmp = tempfile::tempdir().unwrap();
	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["serve", "--stdio"])
		.env("SNIP_CONFIG_DIR", tmp.path().join("cfg"))
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()
		.unwrap();
	let stdout = child.stdout.take().unwrap();
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		let mut line = String::new();
		let _ = BufReader::new(stdout).read_line(&mut line);
		let _ = tx.send(line);
	});
	let first = rx.recv_timeout(DEADLINE);
	if first.is_err() {
		let _ = child.kill();
		let _ = child.wait();
	}
	let first = first.expect("snip serve --stdio printed nothing in time");
	assert_eq!(first, "snip-serve-stdio/1\n");

	drop(child.stdin.take());
	let start = Instant::now();
	let status =
		loop {
			if let Some(status) = child.try_wait().unwrap() {
				break status;
			}
			if start.elapsed() > DEADLINE {
				let _ = child.kill();
				let _ = child.wait();
				panic!("snip serve --stdio still ran {DEADLINE:?} after stdin closed");
			}
			std::thread::sleep(Duration::from_millis(20));
		};
	assert!(status.success(), "{status:?}");
}

#[test]
fn serve_needs_the_stdio_flag() {
	let tmp = tempfile::tempdir().unwrap();
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip"));
	cmd.arg("serve")
		.env("SNIP_CONFIG_DIR", tmp.path().join("cfg"));
	let (status, _, _) = output(cmd, "snip serve");
	assert_eq!(status, Some(2));
}

#[test]
fn cli_master_lists_stats_and_cats_a_folder() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir_all(shared.join("src")).unwrap();
	std::fs::write(shared.join("src/main.rs"), "fn main() {}\n").unwrap();
	std::fs::write(shared.join("bin.dat"), [0u8, 1, 2]).unwrap();
	let m = Master::new(tmp.path());
	let ws = s(&shared);

	assert_eq!(m.ok(&["ls", "anyhost", ws]), "src/\nbin.dat\n");
	assert_eq!(m.ok(&["ls", "anyhost", ws, "src"]), "main.rs\n");
	assert!(m
		.ok(&["stat", "anyhost", ws, "src/main.rs"])
		.starts_with("file\t13\t"));
	assert!(m
		.ok(&["stat", "anyhost", ws, "src"])
		.starts_with("directory\t"));
	assert_eq!(
		m.ok(&["cat", "anyhost", ws, "src/main.rs"]),
		"fn main() {}\n"
	);

	let stderr = m.fails(&["cat", "anyhost", ws, "bin.dat"]);
	assert!(stderr.contains("binary"), "{stderr}");
	m.fails(&["cat", "anyhost", ws, "../secret"]);
	m.fails(&["ls", "anyhost", s(&tmp.path().join("missing"))]);
}

/// `~` and `~/…` name folders under the worker's home.
#[test]
fn cli_master_opens_a_workspace_under_home() {
	let tmp = tempfile::tempdir().unwrap();
	let home = tmp.path().join("home");
	std::fs::create_dir_all(home.join("proj")).unwrap();
	std::fs::write(home.join("proj/a.txt"), "hi\n").unwrap();
	let mut m = Master::new(tmp.path());
	m.home = Some(home);

	assert_eq!(m.ok(&["ls", "anyhost", "~/proj"]), "a.txt\n");
	assert_eq!(m.ok(&["ls", "anyhost", "~"]), "proj/\n");
	assert_eq!(m.ok(&["cat", "anyhost", "~/proj", "a.txt"]), "hi\n");
}

#[test]
fn cli_master_refuses_a_relative_workspace() {
	let tmp = tempfile::tempdir().unwrap();
	let m = Master::new(tmp.path());
	let stderr = m.fails(&["ls", "anyhost", "relative/dir"]);
	assert!(stderr.contains("absolute"), "{stderr}");
}

#[test]
fn cli_master_reports_a_worker_command_that_cannot_run() {
	let tmp = tempfile::tempdir().unwrap();
	let missing = tmp.path().join("no-such-snip");
	let m =
		Master::with_exec(tmp.path(), format!("{} serve --stdio", s(&missing)));
	let stderr = m.fails(&["ls", "anyhost", s(tmp.path())]);
	assert!(stderr.contains("cannot run"), "{stderr}");
}

#[test]
fn cli_master_reports_a_worker_that_exits_at_once() {
	let tmp = tempfile::tempdir().unwrap();
	let m = Master::with_exec(tmp.path(), "false".into());
	let stderr = m.fails(&["ls", "anyhost", s(tmp.path())]);
	assert!(!stderr.trim().is_empty(), "a failure needs a message");
}

/// An older snip without `serve` answers clap's "unrecognized subcommand".
#[test]
fn cli_master_says_snip_is_too_old_when_serve_is_unknown() {
	let tmp = tempfile::tempdir().unwrap();
	let exec = format!("{} no-such-serve --stdio", env!("CARGO_BIN_EXE_snip"));
	let m = Master::with_exec(tmp.path(), exec);
	let stderr = m.fails(&["ls", "anyhost", s(tmp.path())]);
	assert!(stderr.contains("not installed"), "{stderr}");
}

/// What ssh's remote script does when no snip is on the far end.
#[cfg(unix)]
#[test]
fn cli_master_says_snip_is_not_installed_on_exit_127() {
	use std::os::unix::fs::PermissionsExt;

	let tmp = tempfile::tempdir().unwrap();
	let script = tmp.path().join("exit127.sh");
	std::fs::write(&script, "#!/bin/sh\nexit 127\n").unwrap();
	std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
		.unwrap();
	let m = Master::with_exec(tmp.path(), s(&script).to_string());
	let stderr = m.fails(&["ls", "anyhost", s(tmp.path())]);
	assert!(stderr.contains("not installed"), "{stderr}");
}

/// `--max-protocol 1` makes the worker look like one from before Git
/// views: those are refused as too old, browsing still works. Without the
/// flag the Git request reaches the worker, which finds no repository.
#[test]
fn cli_master_git_views_refuse_an_old_worker() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir_all(&shared).unwrap();
	std::fs::write(shared.join("file.txt"), "hello").unwrap();
	let ws = s(&shared);

	let old = Master::with_exec(tmp.path(), serve_exec("--max-protocol 1"));
	for args in [["repos", "anyhost", ws], ["changes", "anyhost", ws]] {
		let stderr = old.fails(&args);
		assert!(
			stderr.contains("too old for this")
				&& stderr.contains("protocol 1, this needs 2"),
			"snip remote {args:?}: {stderr}"
		);
	}
	assert_eq!(old.ok(&["ls", "anyhost", ws]), "file.txt\n");
	assert_eq!(old.ok(&["cat", "anyhost", ws, "file.txt"]), "hello");

	let current = Master::new(tmp.path());
	let stderr = current.fails(&["changes", "anyhost", ws]);
	assert!(!stderr.contains("too old"), "{stderr}");
	assert!(!stderr.trim().is_empty(), "a failure needs a message");
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

fn git(dir: &Path, args: &[&str]) {
	git_out(dir, args);
}

fn git_out(dir: &Path, args: &[&str]) -> String {
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
	let ws_dir = tmp.path().join("ws");
	let alpha = ws_dir.join("alpha");
	let beta = ws_dir.join("beta");
	let plain = ws_dir.join("plain");
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

	// Then modify tracked a.txt, add untracked new.txt, git add staged.txt
	// with new content
	std::fs::write(alpha.join("a.txt"), "commit 2 a modified\n").unwrap();
	std::fs::write(alpha.join("new.txt"), "new file content\n").unwrap();
	std::fs::write(alpha.join("staged.txt"), "staged file content\n").unwrap();
	git(&alpha, &["add", "staged.txt"]);

	// Repo beta: clean, one commit
	git(&beta, &["init"]);
	std::fs::write(beta.join("b.txt"), "beta content\n").unwrap();
	git(&beta, &["add", "b.txt"]);
	git(&beta, &["commit", "-q", "-m", "beta commit"]);

	let m = Master::new(tmp.path());
	let ws = s(&ws_dir);
	let h = "anyhost";

	let repos_out = m.ok(&["repos", h, ws]);
	assert!(repos_out.contains("alpha\t"), "{repos_out}");
	assert!(repos_out.contains("beta\t"), "{repos_out}");
	assert!(!repos_out.contains("plain"), "{repos_out}");
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

	let changes_alpha = m.ok(&["changes", h, ws, "alpha"]);
	assert!(changes_alpha.contains("U\tM\ta.txt"), "{changes_alpha}");
	assert!(changes_alpha.contains("W\tN\tnew.txt"), "{changes_alpha}");
	assert!(
		changes_alpha.contains("S\tN\tstaged.txt"),
		"{changes_alpha}"
	);

	assert_eq!(m.ok(&["changes", h, ws, "beta"]), "");

	let log_alpha = m.ok(&["log", h, ws, "alpha", "-n", "50"]);
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

	let head_sha = git_out(&alpha, &["rev-parse", "HEAD"]).trim().to_string();
	let show_out = m.ok(&["show", h, ws, "alpha", &head_sha]);
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

	let diff_a = m.ok(&["diff", h, ws, "alpha", "a.txt"]);
	assert!(diff_a.contains("commit 2 a modified"), "{diff_a}");

	let diff_staged = m.ok(&["diff", h, ws, "alpha", "staged.txt", "--staged"]);
	assert!(diff_staged.contains("staged file content"), "{diff_staged}");

	let diff_commit =
		m.ok(&["diff", h, ws, "alpha", "a.txt", "--commit", &head_sha]);
	assert!(diff_commit.contains("commit 2 a"), "{diff_commit}");

	let stderr = m.fails(&["changes", h, ws, "plain"]);
	assert!(
		!stderr.is_empty(),
		"expected worker's NotARepository error in stderr"
	);
}

#[test]
fn cli_master_git_views_single_repo_folder_uses_the_empty_repo_path() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws_dir = tmp.path().join("single_repo");
	std::fs::create_dir_all(&ws_dir).unwrap();
	git(&ws_dir, &["init"]);
	std::fs::write(ws_dir.join("init.txt"), "hello single\n").unwrap();
	git(&ws_dir, &["add", "init.txt"]);
	git(&ws_dir, &["commit", "-q", "-m", "initial single"]);

	let m = Master::new(tmp.path());
	let ws = s(&ws_dir);

	let repos_out = m.ok(&["repos", "anyhost", ws]);
	let first_line = repos_out.lines().next().expect("repos line");
	assert_eq!(first_line.split('\t').next(), Some("."), "{repos_out}");

	assert_eq!(m.ok(&["changes", "anyhost", ws]), "");

	let head_sha = git_out(&ws_dir, &["rev-parse", "HEAD"]).trim().to_string();
	let show_out = m.ok(&["show", "anyhost", ws, &head_sha]);
	assert!(show_out.contains("N\tinit.txt"), "{show_out}");
}

#[test]
fn cli_master_repos_of_a_non_repo_folder_says_so() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws_dir = tmp.path().join("plain_share");
	std::fs::create_dir_all(&ws_dir).unwrap();
	std::fs::write(ws_dir.join("note.txt"), "just a note\n").unwrap();

	let m = Master::new(tmp.path());
	let (status, stdout, stderr) = m.snip(&["repos", "anyhost", s(&ws_dir)]);
	assert_eq!(status, Some(0), "{stderr}");
	assert_eq!(stdout, "");
	assert!(
		stderr.contains("no Git repository in"),
		"expected 'no Git repository in' in stderr, got: {stderr}"
	);
}

#[test]
fn cli_master_git_view_rejects_escaping_paths() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let ws_dir = tmp.path().join("ws");
	let alpha = ws_dir.join("alpha");
	std::fs::create_dir_all(&alpha).unwrap();
	git(&alpha, &["init"]);
	std::fs::write(alpha.join("a.txt"), "content\n").unwrap();
	git(&alpha, &["add", "a.txt"]);
	git(&alpha, &["commit", "-q", "-m", "init"]);

	let m = Master::new(tmp.path());
	let ws = s(&ws_dir);
	m.fails(&["diff", "anyhost", ws, "alpha", "../../secret.txt"]);
	m.fails(&["show", "anyhost", ws, "alpha", "--", "--output=x"]);
	m.fails(&["changes", "anyhost", ws, "../.."]);
}

/// `snip copy` of a folder, run locally, as the payload a remote copy of
/// the same folder must equal.
fn local_copy(dir: &Path) -> String {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip"));
	cmd.args(["copy", ".", "--stdout"])
		.current_dir(dir)
		.env("SNIP_CONFIG_DIR", dir.join("../cfg-local"));
	let (status, stdout, stderr) = output(cmd, "snip copy");
	assert_eq!(status, Some(0), "snip copy: {stderr}");
	stdout
}

#[test]
fn cli_remote_copy_of_a_folder_equals_a_local_copy() {
	let tmp = tempfile::tempdir().unwrap();
	let proj = tmp.path().join("proj");
	std::fs::create_dir_all(proj.join("sub")).unwrap();
	std::fs::create_dir_all(proj.join(".hidden")).unwrap();
	std::fs::write(proj.join("a.txt"), "alpha\n").unwrap();
	std::fs::write(proj.join("z.txt"), "zed").unwrap();
	std::fs::write(proj.join("sub/中文.txt"), "深層\r\nx\n").unwrap();
	std::fs::write(proj.join(".hidden/h.txt"), "h\n").unwrap();
	let m = Master::new(tmp.path());
	let remote = m.ok(&["copy", "h", s(&proj), "--stdout"]);
	assert_eq!(remote, local_copy(&proj));
	let one = m.ok(&["copy", "h", s(&proj), "sub", "--stdout"]);
	assert!(one.contains("// file: sub/中文.txt"), "{one}");
	assert!(!one.contains("a.txt"), "{one}");
}

#[test]
fn cli_remote_copy_of_staged_changes_and_commits() {
	if !require_git() {
		return;
	}
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	std::fs::create_dir_all(&repo).unwrap();
	git(&repo, &["init", "-q", "-b", "main"]);
	std::fs::write(repo.join("a.txt"), "one\n").unwrap();
	git(&repo, &["add", "a.txt"]);
	git(&repo, &["commit", "-q", "-m", "first"]);
	std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
	git(&repo, &["add", "a.txt"]);
	std::fs::write(repo.join("b.txt"), "untracked\n").unwrap();
	let m = Master::new(tmp.path());
	let staged = m.ok(&["copy", "h", s(&repo), "--staged", "--stdout"]);
	assert!(staged.contains("// file: [MODIFIED] a.txt"), "{staged}");
	assert!(!staged.contains("b.txt"), "{staged}");
	let head = git_out(&repo, &["rev-parse", "HEAD"]);
	let commits =
		m.ok(&["copy-commits", "h", s(&repo), head.trim(), "--stdout"]);
	assert!(commits.starts_with("// snip-sync commits v1"), "{commits}");
	assert!(commits.contains("\"message\":\"first"), "{commits}");
}

#[test]
fn cli_remote_copy_refuses_a_worker_without_copy() {
	let tmp = tempfile::tempdir().unwrap();
	std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
	let m = Master::with_exec(tmp.path(), serve_exec("--max-protocol 2"));
	let err = m.fails(&["copy", "h", s(tmp.path()), "a.txt", "--stdout"]);
	assert!(err.contains("too old"), "{err}");
}
