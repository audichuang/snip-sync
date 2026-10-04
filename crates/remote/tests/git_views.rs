//! Loopback integration tests for worker git views and repository scanning.

use std::fs;
use std::net::TcpStream;
use std::path::Path;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
	SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn timeout_scale() -> u32 {
	std::env::var("SNIP_E2E_TIMEOUT_SCALE")
		.ok()
		.and_then(|v| v.parse::<u32>().ok())
		.unwrap_or(1)
		.max(1)
}

fn scaled(d: Duration) -> Duration {
	d.saturating_mul(timeout_scale())
}

use rustls::StreamOwned;
use snip_core::browser::LogQuery;
use snip_core::format::ChangeType;
use snip_core::gitsrc::{GitError, GitSource};
use snip_core::gitview::{
	LocalRepo, Read, ReadProfile, RepoView, MAX_CHANGE_ROWS, MAX_COMMIT_FILES,
};
use snip_core::workspace::ScanStatus;
use snip_remote::proto::{
	read_frame, write_frame, ErrorCode, GitQuery, GitReply, Request,
	REMOTE_MAX_LOG_LIMIT, REMOTE_MAX_TIPS,
};
#[cfg(unix)]
use snip_remote::proto::{Response, PROTOCOL_MAX, PROTOCOL_VERSION};
use snip_remote::tls::{client_config, server_name};
use snip_remote::{
	pair, Client, Connection, Identity, RemoteError, RemoteRepo, Worker,
	WorkerOptions,
};

#[cfg(unix)]
static SHIM_INIT: std::sync::Once = std::sync::Once::new();

#[cfg(unix)]
fn init_git_shim() {
	SHIM_INIT.call_once(|| {
		let path_var = std::env::var("PATH").unwrap_or_default();
		let real_git = path_var
			.split(':')
			.map(|dir| Path::new(dir).join("git"))
			.find(|p| is_executable(p))
			.expect("could not find real git executable on PATH");

		let temp_dir = tempfile::tempdir().unwrap();
		let shim_dir = temp_dir.path().to_path_buf();
		// Leak the TempDir so the folder persists for the test run
		let _ = Box::leak(Box::new(temp_dir));
		let git_shim = shim_dir.join("git");

		let script = format!(
			r#"#!/bin/sh
if [ -f "$PWD/.slow_active" ]; then
  if [ -f "$PWD/.slow_pid_target" ]; then
    TARGET_FILE=$(cat "$PWD/.slow_pid_target")
    echo $$ > "$TARGET_FILE"
  elif [ -n "$SLOW_GIT_PID_FILE" ]; then
    echo $$ > "$SLOW_GIT_PID_FILE"
  fi
  if [ -f "$PWD/.slow_sleep" ]; then
    SLEEP_SECS=$(cat "$PWD/.slow_sleep")
  elif [ -n "$SLOW_GIT_SLEEP" ]; then
    SLEEP_SECS="$SLOW_GIT_SLEEP"
  else
    SLEEP_SECS=30
  fi
  exec sleep "$SLEEP_SECS"
else
  exec "{}" "$@"
fi
"#,
			real_git.display()
		);

		fs::write(&git_shim, script).unwrap();
		use std::os::unix::fs::PermissionsExt;
		let mut perms = fs::metadata(&git_shim).unwrap().permissions();
		perms.set_mode(0o755);
		fs::set_permissions(&git_shim, perms).unwrap();

		let new_path = format!("{}:{}", shim_dir.display(), path_var);
		std::env::set_var("PATH", new_path);
	});
}

#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
	std::process::Command::new("kill")
		.args(["-0", &pid.to_string()])
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
	use std::os::unix::fs::PermissionsExt;
	fs::metadata(p)
		.map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
		.unwrap_or(false)
}

fn run_git(cwd: &Path, args: &[&str]) {
	let _ = fs::create_dir_all(cwd);
	let mut cmd = std::process::Command::new("git");
	cmd.args(["-c", "user.name=t", "-c", "user.email=t@t"])
		.args(args)
		.current_dir(cwd)
		.env("GIT_CONFIG_GLOBAL", "/dev/null");
	let out = match cmd.output() {
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
		"git {args:?} failed in {}: {}",
		cwd.display(),
		String::from_utf8_lossy(&out.stderr)
	);
}

fn test_worker(
	roots: &[&Path],
	max_protocol: Option<u32>,
) -> (Worker, Identity) {
	#[cfg(unix)]
	init_git_shim();

	let id = Identity::generate().unwrap();
	let w = Worker::start(
		"127.0.0.1:0".parse().unwrap(),
		&id,
		WorkerOptions {
			name: "test-worker".into(),
			trust_file: None,
			max_protocol,
		},
	)
	.unwrap();
	let roots: Vec<_> = roots.iter().map(|p| p.to_path_buf()).collect();
	assert!(w.set_roots(&roots).is_empty());
	(w, id)
}

fn addr(w: &Worker) -> String {
	w.local_addr().to_string()
}

fn paired_client(w: &Worker) -> (Client, String) {
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(w), &code, &master, "mac").unwrap();
	let client = Client::new(paired, master, "mac".into()).unwrap();
	let ws = client.list_workspaces().unwrap()[0].id.clone();
	(client, ws)
}

#[test]
fn scan_of_a_multi_repo_workspace() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	// Repo alpha: tracked file modified + untracked file
	let alpha = ws.join("alpha");
	run_git(&alpha, &["init", "-b", "main"]);
	fs::write(alpha.join("tracked.txt"), "v1").unwrap();
	run_git(&alpha, &["add", "tracked.txt"]);
	run_git(&alpha, &["commit", "-m", "init"]);
	fs::write(alpha.join("tracked.txt"), "v2").unwrap();
	fs::write(alpha.join("untracked.txt"), "new").unwrap();

	// Repo beta: clean
	let beta = ws.join("beta");
	run_git(&beta, &["init", "-b", "main"]);
	fs::write(beta.join("clean.txt"), "content").unwrap();
	run_git(&beta, &["add", "clean.txt"]);
	run_git(&beta, &["commit", "-m", "init"]);

	// Non-repo folder plain/
	let plain = ws.join("plain");
	fs::create_dir_all(&plain).unwrap();
	fs::write(plain.join("note.txt"), "plain content").unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	assert_eq!(scan.status, ScanStatus::Complete);
	assert_eq!(scan.repos.len(), 2);

	let alpha_repo = scan
		.repos
		.iter()
		.find(|r| r.rel == "alpha")
		.expect("alpha must be present");
	assert_eq!(alpha_repo.name, "alpha");
	assert!(alpha_repo.utf8);
	let alpha_summary = alpha_repo.summary.as_ref().unwrap();
	assert_eq!(alpha_summary.branch.as_deref(), Some("main"));
	assert_eq!(alpha_summary.changes.unstaged, 1);
	assert_eq!(alpha_summary.changes.untracked, 1);

	let beta_repo = scan
		.repos
		.iter()
		.find(|r| r.rel == "beta")
		.expect("beta must be present");
	assert_eq!(beta_repo.name, "beta");
	assert!(beta_repo.utf8);
	let beta_summary = beta_repo.summary.as_ref().unwrap();
	assert_eq!(beta_summary.branch.as_deref(), Some("main"));
	assert_eq!(beta_summary.changes.unstaged, 0);
	assert_eq!(beta_summary.changes.untracked, 0);

	assert!(!scan.repos.iter().any(|r| r.rel == "plain"));

	#[cfg(unix)]
	{
		// Variant where the share root is spelled through a symlink
		let link_ws = tmp.path().join("link_ws");
		std::os::unix::fs::symlink(&ws, &link_ws).unwrap();
		let (w_link, _) = test_worker(&[&link_ws], None);
		let (client_link, link_id) = paired_client(&w_link);

		let scan_link = client_link.scan_repos(&link_id, None, None).unwrap();
		assert_eq!(scan_link.status, ScanStatus::Complete);
		assert_eq!(scan_link.repos.len(), 2);
		assert!(scan_link.repos.iter().any(|r| r.rel == "alpha"));
		assert!(scan_link.repos.iter().any(|r| r.rel == "beta"));
	}
}

#[test]
fn scan_of_a_single_repo_workspace_is_the_repo_itself() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("repo");
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("README"), "hello").unwrap();
	run_git(&ws, &["add", "README"]);
	run_git(&ws, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	assert_eq!(scan.status, ScanStatus::Complete);
	assert_eq!(scan.repos.len(), 1);
	assert_eq!(scan.repos[0].rel, "");
	assert!(scan.repos[0].utf8);
	assert_eq!(
		scan.repos[0].summary.as_ref().unwrap().branch.as_deref(),
		Some("main")
	);
}

#[test]
fn scan_of_a_non_repo_workspace_is_empty_and_complete() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("plain_ws");
	fs::create_dir_all(ws.join("folder")).unwrap();
	fs::write(ws.join("folder").join("file.txt"), "text").unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	assert_eq!(scan.status, ScanStatus::Complete);
	assert!(scan.repos.is_empty());
}

#[test]
fn scan_never_reports_a_parent_repository() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outer = tmp.path().join("outer");
	run_git(&outer, &["init", "-b", "main"]);
	fs::write(outer.join("outer-dirty.txt"), "dirty").unwrap();

	let inner = outer.join("inner");
	fs::create_dir_all(&inner).unwrap();
	fs::write(inner.join("file.txt"), "clean").unwrap();

	let (w, _) = test_worker(&[&inner], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	assert!(scan.repos.is_empty());

	let json = serde_json::to_string(&scan).unwrap();
	assert!(!json.contains("outer-dirty"));
	assert!(!json.contains(&outer.display().to_string()));
}

#[test]
fn broken_git_dir_is_an_error_row_not_the_parent() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let parent = tmp.path().join("parent");
	run_git(&parent, &["init", "-b", "main"]);

	let ws = parent.join("ws");
	let broken = ws.join("broken");
	fs::create_dir_all(broken.join(".git")).unwrap(); // empty dir

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	assert!(
		!scan.repos.iter().any(|r| r.name == "parent"),
		"must not report parent repo"
	);

	let broken_row = scan.repos.iter().find(|r| r.rel == "broken");
	assert!(broken_row.is_some(), "must have row for broken");
	assert!(broken_row.unwrap().summary.is_err());
}

#[test]
fn worktree_of_an_outside_repo_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	run_git(&outside, &["init", "-b", "main"]);
	fs::write(outside.join("file.txt"), "main").unwrap();
	run_git(&outside, &["add", "file.txt"]);
	run_git(&outside, &["commit", "-m", "init"]);

	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let wt = ws.join("wt");
	run_git(
		&outside,
		&["worktree", "add", wt.to_str().unwrap(), "-b", "wt-branch"],
	);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	let wt_row = scan
		.repos
		.iter()
		.find(|r| r.rel == "wt")
		.expect("wt row present");
	assert!(wt_row.summary.is_err());
	let err_msg = wt_row.summary.as_ref().unwrap_err();
	assert!(
		err_msg.contains("linked worktree"),
		"unexpected error: {err_msg}"
	);
	assert!(
		err_msg.contains("main repository"),
		"unexpected error: {err_msg}"
	);

	let json = serde_json::to_string(&scan).unwrap();
	assert!(!json.contains(&outside.display().to_string()));
	let canon = dunce::canonicalize(&outside).unwrap();
	assert!(!json.contains(&canon.display().to_string()));
}

#[test]
fn main_repo_with_worktree_outside_the_share_is_served() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let share = tmp.path().join("share");
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&share).unwrap();
	fs::create_dir_all(&outside).unwrap();

	let main = share.join("main");
	run_git(&main, &["init", "-b", "main"]);
	fs::write(main.join("file1.txt"), "v1").unwrap();
	run_git(&main, &["add", "file1.txt"]);
	run_git(&main, &["commit", "-m", "commit1"]);

	fs::write(main.join("file2.txt"), "v2").unwrap();
	run_git(&main, &["add", "file2.txt"]);
	run_git(&main, &["commit", "-m", "commit2"]);

	fs::write(main.join("file1.txt"), "v1 modified").unwrap();
	fs::write(main.join("untracked.txt"), "new").unwrap();

	let wt = outside.join("wt");
	run_git(
		&main,
		&["worktree", "add", wt.to_str().unwrap(), "-b", "wt-branch"],
	);

	let (w, _) = test_worker(&[&share], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	assert!(
		scan.errors.is_empty(),
		"expected no scan errors, got {:?}",
		scan.errors
	);
	assert_eq!(scan.repos.len(), 1);

	let main_row = scan
		.repos
		.iter()
		.find(|r| r.rel == "main")
		.expect("main row present");
	assert!(main_row.summary.is_ok());
	let summary = main_row.summary.as_ref().unwrap();
	assert_eq!(summary.changes.unstaged, 1);
	assert_eq!(summary.changes.untracked, 1);
	assert!(scan.repos.iter().all(|r| r.summary.is_ok()));

	let json = serde_json::to_string(&scan).unwrap();
	assert!(!json.contains(&outside.display().to_string()));
	let canon = dunce::canonicalize(&outside).unwrap();
	assert!(!json.contains(&canon.display().to_string()));

	let changes_reply = client
		.git(
			&ws_id,
			"main",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap();
	let changes = match changes_reply {
		GitReply::ChangeList(cl) => cl,
		other => panic!("expected ChangeList, got {other:?}"),
	};
	assert_eq!(changes.rows.len(), 2);
	assert!(changes.rows.iter().any(|r| r.path == "file1.txt"));
	assert!(changes.rows.iter().any(|r| r.path == "untracked.txt"));

	let head = match client
		.git(
			&ws_id,
			"main",
			ReadProfile::Interactive,
			GitQuery::ResolveCommit { rev: "HEAD".into() },
			None,
		)
		.unwrap()
	{
		GitReply::Commit(sha) => sha,
		other => panic!("expected Commit, got {other:?}"),
	};

	let log_reply = client
		.git(
			&ws_id,
			"main",
			ReadProfile::Interactive,
			GitQuery::LogFromTips {
				tips: vec![head],
				skip: 0,
				limit: 10,
			},
			None,
		)
		.unwrap();
	let commits = match log_reply {
		GitReply::Log { commits, .. } => commits,
		other => panic!("expected Log, got {other:?}"),
	};
	assert_eq!(commits.len(), 2);
}

#[test]
fn gitdir_file_pointing_outside_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	run_git(&outside, &["init", "-b", "main"]);

	let ws = tmp.path().join("ws");
	let fake = ws.join("fake");
	fs::create_dir_all(&fake).unwrap();
	fs::write(
		fake.join(".git"),
		format!("gitdir: {}\n", outside.join(".git").display()),
	)
	.unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	let fake_row = scan
		.repos
		.iter()
		.find(|r| r.rel == "fake")
		.expect("fake row present");
	assert!(fake_row.summary.is_err());

	let json = serde_json::to_string(&scan).unwrap();
	assert!(!json.contains(&outside.display().to_string()));
	let canon = dunce::canonicalize(&outside).unwrap();
	assert!(!json.contains(&canon.display().to_string()));
}

#[cfg(unix)]
#[test]
fn symlinked_git_dir_pointing_outside_is_never_reported() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	run_git(&outside, &["init", "-b", "main"]);

	let ws = tmp.path().join("ws");
	let sym_repo = ws.join("sym_repo");
	fs::create_dir_all(&sym_repo).unwrap();
	std::os::unix::fs::symlink(outside.join(".git"), sym_repo.join(".git"))
		.unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	// Core discovery ignores a symlinked .git; the boundary check in core guards the case where git is reached some other way.
	assert!(!scan.repos.iter().any(|r| r.rel == "sym_repo"));
	assert!(scan.repos.is_empty());
	assert!(
		scan.status == ScanStatus::Complete
			|| scan.status == ScanStatus::Incomplete
	);

	let json = serde_json::to_string(&scan).unwrap();
	assert!(!json.contains(&outside.display().to_string()));
	let canon = dunce::canonicalize(&outside).unwrap();
	assert!(!json.contains(&canon.display().to_string()));
}

#[test]
fn alternates_pointing_outside_are_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	run_git(&outside, &["init", "-b", "main"]);
	fs::write(outside.join("file.txt"), "main").unwrap();
	run_git(&outside, &["add", "file.txt"]);
	run_git(&outside, &["commit", "-m", "init"]);

	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(
		&ws,
		&["clone", "--shared", outside.to_str().unwrap(), "borrowed"],
	);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	let borrowed_row = scan
		.repos
		.iter()
		.find(|r| r.rel == "borrowed")
		.expect("borrowed row present");
	assert!(borrowed_row.summary.is_err());

	let json = serde_json::to_string(&scan).unwrap();
	assert!(!json.contains(&outside.display().to_string()));
	let canon = dunce::canonicalize(&outside).unwrap();
	assert!(!json.contains(&canon.display().to_string()));
}

#[test]
fn refused_repo_error_does_not_name_outside_paths() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside_secret");
	run_git(&outside, &["init", "-b", "main"]);
	fs::write(outside.join("secret.txt"), "pass").unwrap();
	run_git(&outside, &["add", "secret.txt"]);
	run_git(&outside, &["commit", "-m", "init"]);

	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	// 1. linked worktree
	run_git(
		&outside,
		&[
			"worktree",
			"add",
			ws.join("wt").to_str().unwrap(),
			"-b",
			"wt",
		],
	);

	// 2. gitdir file
	let fake = ws.join("fake");
	fs::create_dir_all(&fake).unwrap();
	fs::write(
		fake.join(".git"),
		format!("gitdir: {}\n", outside.join(".git").display()),
	)
	.unwrap();

	// 3. alternates clone
	run_git(
		&ws,
		&["clone", "--shared", outside.to_str().unwrap(), "borrowed"],
	);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	let json = serde_json::to_string(&scan).unwrap();

	let outside_raw = outside.display().to_string();
	let outside_canon =
		dunce::canonicalize(&outside).unwrap().display().to_string();

	assert!(
		!json.contains(&outside_raw),
		"serialized reply leaked outside path: {json}"
	);
	assert!(
		!json.contains(&outside_canon),
		"serialized reply leaked canonical outside path: {json}"
	);
}

#[test]
fn scan_under_a_subfolder_scans_only_that_branch() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let sub = ws.join("sub");
	let repo1 = sub.join("repo1");
	run_git(&repo1, &["init", "-b", "main"]);
	fs::write(repo1.join("a.txt"), "a").unwrap();
	run_git(&repo1, &["add", "a.txt"]);
	run_git(&repo1, &["commit", "-m", "init"]);

	let repo2 = ws.join("repo2");
	run_git(&repo2, &["init", "-b", "main"]);
	fs::write(repo2.join("b.txt"), "b").unwrap();
	run_git(&repo2, &["add", "b.txt"]);
	run_git(&repo2, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let scan = client.scan_repos(&ws_id, Some("sub"), None).unwrap();
	assert_eq!(scan.repos.len(), 1);
	assert_eq!(scan.repos[0].rel, "sub/repo1");
	assert!(!scan.repos.iter().any(|r| r.rel == "repo2"));
}

#[test]
fn scan_under_dotdot_or_absolute_or_symlink_out_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let dotdot = client.scan_repos(&ws_id, Some("../outside"), None);
	match dotdot {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::BadRequest);
		}
		other => panic!("expected BadRequest for ../, got {other:?}"),
	}

	let abs = client.scan_repos(&ws_id, Some("/etc"), None);
	match abs {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::BadRequest);
		}
		other => panic!("expected BadRequest for absolute, got {other:?}"),
	}

	#[cfg(unix)]
	{
		let outside = tmp.path().join("outside_dir");
		fs::create_dir_all(&outside).unwrap();
		std::os::unix::fs::symlink(&outside, ws.join("link_out")).unwrap();

		let link_err = client.scan_repos(&ws_id, Some("link_out"), None);
		match link_err {
			Err(RemoteError::Refused { code, .. }) => {
				assert_eq!(code, ErrorCode::Forbidden);
			}
			other => {
				panic!("expected Forbidden for symlink out, got {other:?}")
			}
		}
	}
}

#[test]
fn git_requests_need_a_paired_master() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let master = Arc::new(Identity::generate().unwrap());
	let mut conn = Connection::open(&addr(&w), &master, None, "mac").unwrap();
	assert!(!conn.paired());

	let scan_req = Request::ScanRepos {
		workspace: "any".into(),
		under: None,
	};
	let scan_res = conn.call(&scan_req, None, scaled(Duration::from_secs(5)));
	match scan_res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::NotPaired);
		}
		other => panic!("expected NotPaired refusal for scan, got {other:?}"),
	}

	let git_req = Request::GitView {
		workspace: "any".into(),
		repo: "".into(),
		profile: snip_core::gitview::ReadProfile::Interactive,
		query: GitQuery::ChangeList,
	};
	let git_res = conn.call(&git_req, None, scaled(Duration::from_secs(5)));
	match git_res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::NotPaired);
		}
		other => {
			panic!("expected NotPaired refusal for git_view, got {other:?}")
		}
	}
}

#[test]
fn write_and_rename_stay_unsupported() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let write_res = client.call(&Request::Write {
		workspace: ws_id.clone(),
		path: "f.txt".into(),
		content: "c".into(),
	});
	match write_res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::Unsupported);
		}
		other => panic!("expected Unsupported, got {other:?}"),
	}

	let rename_res = client.call(&Request::Rename {
		workspace: ws_id,
		from: "a".into(),
		to: "b".into(),
	});
	match rename_res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::Unsupported);
		}
		other => panic!("expected Unsupported, got {other:?}"),
	}
}

#[test]
fn v1_connection_never_sees_pending() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();

	let (config, _) =
		client_config(&master, Some(paired.pin().unwrap())).unwrap();
	let tcp = TcpStream::connect(w.local_addr()).unwrap();
	tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
		.unwrap();
	tcp.set_write_timeout(Some(scaled(Duration::from_secs(5))))
		.unwrap();
	let conn = rustls::ClientConnection::new(config, server_name()).unwrap();
	let mut tls = StreamOwned::new(conn, tcp);

	// Hand-written v1 hello without max_version
	let v1_hello = serde_json::json!({
		"op": "hello",
		"version": 1,
		"name": "v1_master"
	});
	write_frame(&mut tls, &v1_hello).unwrap();
	let _hello_resp: serde_json::Value = read_frame(&mut tls).unwrap().unwrap();

	let ws_id = w.roots()[0].id.clone();

	// ListDir works
	let list_req = serde_json::json!({
		"op": "list_dir",
		"workspace": ws_id,
		"path": ""
	});
	write_frame(&mut tls, &list_req).unwrap();
	let list_reply: serde_json::Value = read_frame(&mut tls).unwrap().unwrap();
	assert_eq!(
		list_reply.get("reply").and_then(|r| r.as_str()),
		Some("dir")
	);

	// ScanRepos receives Unsupported without any Pending frame
	let scan_req = serde_json::json!({
		"op": "scan_repos",
		"workspace": ws_id,
		"under": null
	});
	write_frame(&mut tls, &scan_req).unwrap();
	let scan_reply: serde_json::Value = read_frame(&mut tls).unwrap().unwrap();
	assert_eq!(
		scan_reply.get("reply").and_then(|r| r.as_str()),
		Some("error")
	);
	assert_eq!(
		scan_reply.get("code").and_then(|c| c.as_str()),
		Some("unsupported")
	);
}

#[cfg(unix)]
#[test]
fn dropping_the_connection_cancels_the_worker_git() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let pid_file = tmp.path().join("slow_git.pid");
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	// Repo with slow- marker
	let slow_repo = ws.join("slow-x");
	run_git(&slow_repo, &["init", "-b", "main"]);
	fs::write(slow_repo.join("file.txt"), "content").unwrap();
	run_git(&slow_repo, &["add", "file.txt"]);
	run_git(&slow_repo, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&ws], None);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();

	fs::write(slow_repo.join(".slow_active"), "").unwrap();
	fs::write(
		slow_repo.join(".slow_pid_target"),
		pid_file.to_str().unwrap(),
	)
	.unwrap();

	let (config, _) =
		client_config(&master, Some(paired.pin().unwrap())).unwrap();
	let tcp = TcpStream::connect(w.local_addr()).unwrap();
	tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
		.unwrap();
	tcp.set_write_timeout(Some(scaled(Duration::from_secs(5))))
		.unwrap();
	let conn = rustls::ClientConnection::new(config, server_name()).unwrap();
	let mut tls = StreamOwned::new(conn, tcp);

	write_frame(
		&mut tls,
		&Request::Hello {
			version: PROTOCOL_VERSION,
			max_version: Some(PROTOCOL_MAX),
			name: "test".into(),
		},
	)
	.unwrap();
	let _ = read_frame::<Response>(&mut tls).unwrap().unwrap();

	let ws_id = w.roots()[0].id.clone();
	write_frame(
		&mut tls,
		&Request::ScanRepos {
			workspace: ws_id,
			under: None,
		},
	)
	.unwrap();

	// Read the first Pending frame
	let first_frame = read_frame::<Response>(&mut tls).unwrap().unwrap();
	assert_eq!(first_frame, Response::Pending);

	// Drop socket connection
	drop(tls);

	// Read pid from pid_file
	let pid_deadline = Instant::now() + scaled(Duration::from_secs(5));
	let mut slow_pid = None;
	while Instant::now() < pid_deadline {
		if let Ok(content) = fs::read_to_string(&pid_file) {
			if let Ok(p) = content.trim().parse::<i32>() {
				slow_pid = Some(p);
				break;
			}
		}
		std::thread::sleep(Duration::from_millis(20));
	}
	let pid = slow_pid.expect("slow git PID file was not written");

	// Verify within bounded wait that running_jobs is 0 and process is dead
	let wait_deadline = Instant::now() + scaled(Duration::from_secs(10));
	let mut jobs_zero = false;
	let mut proc_dead = false;

	while Instant::now() < wait_deadline {
		if !jobs_zero && w.running_jobs() == 0 {
			jobs_zero = true;
		}
		if !proc_dead && !is_pid_alive(pid) {
			proc_dead = true;
		}
		if jobs_zero && proc_dead {
			break;
		}
		std::thread::sleep(Duration::from_millis(25));
	}

	assert!(
		jobs_zero,
		"worker.running_jobs() should return to 0 within deadline"
	);
	assert!(
		proc_dead,
		"slow git process {pid} should be terminated within deadline"
	);
}

#[cfg(unix)]
#[test]
fn vanished_master_does_not_keep_git_running() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let pid_file = tmp.path().join("vanished_git.pid");
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let slow_repo = ws.join("slow-vanished");
	run_git(&slow_repo, &["init", "-b", "main"]);
	fs::write(slow_repo.join("file.txt"), "content").unwrap();
	run_git(&slow_repo, &["add", "file.txt"]);
	run_git(&slow_repo, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&ws], None);
	// Short scan deadline for this test
	w.set_deadlines_for_tests(
		scaled(Duration::from_secs(60)),
		scaled(Duration::from_millis(1500)),
	);

	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();

	fs::write(slow_repo.join(".slow_active"), "").unwrap();
	fs::write(
		slow_repo.join(".slow_pid_target"),
		pid_file.to_str().unwrap(),
	)
	.unwrap();

	let (config, _) =
		client_config(&master, Some(paired.pin().unwrap())).unwrap();
	let tcp = TcpStream::connect(w.local_addr()).unwrap();
	let conn = rustls::ClientConnection::new(config, server_name()).unwrap();
	let mut tls = StreamOwned::new(conn, tcp);

	write_frame(
		&mut tls,
		&Request::Hello {
			version: PROTOCOL_VERSION,
			max_version: Some(PROTOCOL_MAX),
			name: "test".into(),
		},
	)
	.unwrap();
	let _ = read_frame::<Response>(&mut tls).unwrap().unwrap();

	let ws_id = w.roots()[0].id.clone();
	write_frame(
		&mut tls,
		&Request::ScanRepos {
			workspace: ws_id,
			under: None,
		},
	)
	.unwrap();

	// Master stops reading without dropping connection
	let pid_deadline = Instant::now() + scaled(Duration::from_secs(5));
	let mut slow_pid = None;
	while Instant::now() < pid_deadline {
		if let Ok(content) = fs::read_to_string(&pid_file) {
			if let Ok(p) = content.trim().parse::<i32>() {
				slow_pid = Some(p);
				break;
			}
		}
		std::thread::sleep(Duration::from_millis(20));
	}
	let pid = slow_pid.expect("slow git PID file was not written");

	// Within deadline, job deadline fires, worker cancels git and running_jobs -> 0
	let wait_deadline = Instant::now() + scaled(Duration::from_secs(10));
	let mut jobs_zero = false;
	let mut proc_dead = false;

	while Instant::now() < wait_deadline {
		if !jobs_zero && w.running_jobs() == 0 {
			jobs_zero = true;
		}
		if !proc_dead && !is_pid_alive(pid) {
			proc_dead = true;
		}
		if jobs_zero && proc_dead {
			break;
		}
		std::thread::sleep(Duration::from_millis(25));
	}

	assert!(
		jobs_zero,
		"worker.running_jobs() should return to 0 after job deadline"
	);
	assert!(
		proc_dead,
		"slow git process {pid} should be terminated after job deadline"
	);

	drop(tls);
}

#[cfg(unix)]
#[test]
fn disconnect_storm_stays_within_the_job_cap() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let slow_repo = ws.join("slow-storm");
	run_git(&slow_repo, &["init", "-b", "main"]);
	fs::write(slow_repo.join("file.txt"), "storm").unwrap();
	run_git(&slow_repo, &["add", "file.txt"]);
	run_git(&slow_repo, &["commit", "-m", "init"]);
	fs::write(slow_repo.join(".slow_active"), "").unwrap();
	fs::write(slow_repo.join(".slow_sleep"), "1").unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let w = Arc::new(w);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	let ws_id = w.roots()[0].id.clone();

	let max_observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
	let done = Arc::new(AtomicBool::new(false));

	let sampler_w = w.clone();
	let sampler_max = max_observed.clone();
	let sampler_done = done.clone();
	let sampler = std::thread::spawn(move || {
		while !sampler_done.load(std::sync::atomic::Ordering::SeqCst) {
			let running = sampler_w.running_jobs();
			sampler_max.fetch_max(running, std::sync::atomic::Ordering::SeqCst);
			std::thread::sleep(Duration::from_millis(5));
		}
	});

	let mut clients = Vec::new();
	for _ in 0..10 {
		let w_clone = w.clone();
		let master_clone = master.clone();
		let paired_pin = paired.pin().unwrap();
		let ws_id_clone = ws_id.clone();
		clients.push(std::thread::spawn(move || {
			let (config, _) =
				client_config(&master_clone, Some(paired_pin)).unwrap();
			if let Ok(tcp) = TcpStream::connect(w_clone.local_addr()) {
				let conn = rustls::ClientConnection::new(config, server_name())
					.unwrap();
				let mut tls = StreamOwned::new(conn, tcp);
				if write_frame(
					&mut tls,
					&Request::Hello {
						version: PROTOCOL_VERSION,
						max_version: Some(PROTOCOL_MAX),
						name: "storm".into(),
					},
				)
				.is_ok()
				{
					let _ = read_frame::<Response>(&mut tls);
					let _ = write_frame(
						&mut tls,
						&Request::ScanRepos {
							workspace: ws_id_clone,
							under: None,
						},
					);
					// Hold briefly then drop
					std::thread::sleep(Duration::from_millis(50));
				}
			}
		}));
	}

	for c in clients {
		let _ = c.join();
	}

	done.store(true, std::sync::atomic::Ordering::SeqCst);
	let _ = sampler.join();

	assert!(
		max_observed.load(std::sync::atomic::Ordering::SeqCst) <= 2,
		"running_jobs exceeded cap of 2: {}",
		max_observed.load(std::sync::atomic::Ordering::SeqCst)
	);

	let deadline = Instant::now() + scaled(Duration::from_secs(10));
	while w.running_jobs() > 0 && Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(25));
	}
	assert_eq!(w.running_jobs(), 0);
}

#[cfg(unix)]
#[test]
fn unsharing_mid_scan_cancels_and_refuses() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let slow_repo = ws.join("slow-unshare");
	run_git(&slow_repo, &["init", "-b", "main"]);
	fs::write(slow_repo.join("file.txt"), "unshare").unwrap();
	run_git(&slow_repo, &["add", "file.txt"]);
	run_git(&slow_repo, &["commit", "-m", "init"]);
	fs::write(slow_repo.join(".slow_active"), "").unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();

	let (config, _) =
		client_config(&master, Some(paired.pin().unwrap())).unwrap();
	let tcp = TcpStream::connect(w.local_addr()).unwrap();
	tcp.set_read_timeout(Some(scaled(Duration::from_secs(10))))
		.unwrap();
	let conn = rustls::ClientConnection::new(config, server_name()).unwrap();
	let mut tls = StreamOwned::new(conn, tcp);

	write_frame(
		&mut tls,
		&Request::Hello {
			version: PROTOCOL_VERSION,
			max_version: Some(PROTOCOL_MAX),
			name: "test".into(),
		},
	)
	.unwrap();
	let _ = read_frame::<Response>(&mut tls).unwrap().unwrap();

	let ws_id = w.roots()[0].id.clone();
	write_frame(
		&mut tls,
		&Request::ScanRepos {
			workspace: ws_id,
			under: None,
		},
	)
	.unwrap();

	// Wait for first Pending
	let frame: Response = read_frame(&mut tls).unwrap().unwrap();
	assert_eq!(frame, Response::Pending);

	// Unshare mid-scan
	w.set_roots(&[]);

	// Read reply frame
	let mut final_error = None;
	let deadline = Instant::now() + scaled(Duration::from_secs(8));
	while Instant::now() < deadline {
		match read_frame::<Response>(&mut tls).unwrap() {
			Some(Response::Pending) => {}
			Some(Response::Error { code, message }) => {
				final_error = Some((code, message));
				break;
			}
			Some(Response::Repos(_)) => {
				panic!("must never return Repos reply after unsharing");
			}
			other => panic!("unexpected response: {other:?}"),
		}
	}

	let (code, _) = final_error.expect("expected error response");
	assert!(
		code == ErrorCode::Forbidden || code == ErrorCode::Cancelled,
		"expected Forbidden or Cancelled, got {code:?}"
	);

	let wait_deadline = Instant::now() + scaled(Duration::from_secs(5));
	while w.running_jobs() > 0 && Instant::now() < wait_deadline {
		std::thread::sleep(Duration::from_millis(20));
	}
	assert_eq!(w.running_jobs(), 0);
}

#[cfg(unix)]
#[test]
fn scan_of_many_slow_repos_returns_partial_not_timeout() {
	let _serial = serial();
	let s = timeout_scale() as u64;
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	// Fast clean repo
	let fast = ws.join("fast-a");
	run_git(&fast, &["init", "-b", "main"]);
	fs::write(fast.join("fast.txt"), "f").unwrap();
	run_git(&fast, &["add", "fast.txt"]);
	run_git(&fast, &["commit", "-m", "init"]);

	// Several slow repos (slow-1 .. slow-4)
	let budget_secs = 6 * s;
	let slow_sleep_secs = budget_secs + 1;
	let slow_sleep_str = format!("{slow_sleep_secs}.0");
	for i in 1..=4 {
		let repo_dir = ws.join(format!("slow-{i}"));
		run_git(&repo_dir, &["init", "-b", "main"]);
		fs::write(repo_dir.join("f.txt"), "s").unwrap();
		run_git(&repo_dir, &["add", "f.txt"]);
		run_git(&repo_dir, &["commit", "-m", "init"]);
		fs::write(repo_dir.join(".slow_sleep"), &slow_sleep_str).unwrap();
		fs::write(repo_dir.join(".slow_active"), "").unwrap();
	}

	let (w, _) = test_worker(&[&ws], None);
	// Scan deadline = 5s (reserve) + 6s*s so budget = 6s*s
	let scan_deadline = Duration::from_secs(5 + budget_secs);
	w.set_deadlines_for_tests(Duration::from_secs(60 * s), scan_deadline);

	let (client, ws_id) = paired_client(&w);
	let start = Instant::now();
	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	let elapsed = start.elapsed();

	assert!(
		elapsed < scan_deadline,
		"should complete before the {scan_deadline:?} job deadline, took {elapsed:?}"
	);
	assert_eq!(scan.status, ScanStatus::TimedOut);

	// Discovery walks in `read_dir` order, which Linux does not sort, so
	// which repos fit the budget varies: every repo is still listed, and
	// those past the budget say they were not read.
	assert_eq!(scan.repos.len(), 5, "no repo dropped: {:?}", scan.repos);
	let unread = scan.repos.iter().filter(|r| {
		r.summary
			.as_ref()
			.err()
			.is_some_and(|msg| msg.contains("not read"))
	});
	assert!(unread.count() >= 1, "at least one repo timed out unread");
}

fn setup_rich_repo(dir: &Path) {
	run_git(dir, &["init", "-b", "main"]);
	fs::write(dir.join("initial.txt"), "v1\n").unwrap();
	fs::write(dir.join("to_rename.txt"), "rename me\n").unwrap();
	run_git(dir, &["add", "initial.txt", "to_rename.txt"]);
	run_git(dir, &["commit", "-m", "commit 1"]);
	run_git(dir, &["tag", "v1.0"]);

	run_git(dir, &["branch", "feat"]);

	run_git(dir, &["mv", "to_rename.txt", "renamed.txt"]);
	run_git(dir, &["commit", "-m", "commit 2: rename"]);
	run_git(dir, &["tag", "v2.0"]);

	run_git(dir, &["checkout", "feat"]);
	fs::write(dir.join("branch_file.txt"), "feat content\n").unwrap();
	run_git(dir, &["add", "branch_file.txt"]);
	run_git(dir, &["commit", "-m", "commit 3 on feat"]);

	run_git(dir, &["checkout", "main"]);
	run_git(dir, &["merge", "--no-ff", "feat", "-m", "merge feat"]);

	fs::write(dir.join("initial.txt"), "v1 modified\n").unwrap();
	fs::write(dir.join("staged.txt"), "staged content\n").unwrap();
	run_git(dir, &["add", "staged.txt"]);
	fs::write(dir.join("untracked.txt"), "untracked content\n").unwrap();
}

fn assert_repo_views_match(
	local: &LocalRepo,
	remote: &RemoteRepo,
	read: &Read,
) {
	let l_cl = local.change_list(MAX_CHANGE_ROWS, read).unwrap();
	let r_cl = remote.change_list(MAX_CHANGE_ROWS, read).unwrap();
	assert_eq!(l_cl.total, r_cl.total);
	assert_eq!(l_cl.rows.len(), r_cl.rows.len());
	assert_eq!(l_cl.rows, r_cl.rows);
	if l_cl.summary.is_some() {
		assert_eq!(l_cl.summary, r_cl.summary);
	} else {
		assert!(r_cl.summary.is_some());
	}

	let l_refs = local.refs(read).unwrap();
	let r_refs = remote.refs(read).unwrap();
	assert_eq!(l_refs.head, r_refs.head);
	assert_eq!(l_refs.detached, r_refs.detached);
	assert_eq!(l_refs.refs, r_refs.refs);

	let l_head = local.resolve_commit("HEAD", read).unwrap();
	let r_head = remote.resolve_commit("HEAD", read).unwrap();
	assert_eq!(l_head, r_head);
	let l_tag = local.resolve_commit("v1.0", read).unwrap();
	let r_tag = remote.resolve_commit("v1.0", read).unwrap();
	assert_eq!(l_tag, r_tag);

	let l_log = local
		.log_from_tips(std::slice::from_ref(&l_head), 0, 10, read)
		.unwrap();
	let r_log = remote
		.log_from_tips(std::slice::from_ref(&r_head), 0, 10, read)
		.unwrap();
	assert_eq!(l_log, r_log);

	let q = LogQuery {
		text: "".into(),
		regex: false,
		match_case: false,
		author: None,
		since: None,
		until: None,
		paths: vec![],
	};
	let l_hq = local.history_query(Some("HEAD"), &q, 0, 10, read).unwrap();
	let r_hq = remote.history_query(Some("HEAD"), &q, 0, 10, read).unwrap();
	assert_eq!(l_hq, r_hq);

	let l_cd = local.commit_details(&l_head, read).unwrap();
	let r_cd = remote.commit_details(&r_head, read).unwrap();
	assert_eq!(l_cd, r_cd);

	let l_ue = local.user_email(read);
	let r_ue = remote.user_email(read);
	assert_eq!(l_ue, r_ue);

	let l_cp_w = local
		.changed_paths(&GitSource::Working, MAX_COMMIT_FILES, read)
		.unwrap();
	let r_cp_w = remote
		.changed_paths(&GitSource::Working, MAX_COMMIT_FILES, read)
		.unwrap();
	assert_eq!(l_cp_w, r_cp_w);

	let l_cp_s = local
		.changed_paths(&GitSource::Staged, MAX_COMMIT_FILES, read)
		.unwrap();
	let r_cp_s = remote
		.changed_paths(&GitSource::Staged, MAX_COMMIT_FILES, read)
		.unwrap();
	assert_eq!(l_cp_s, r_cp_s);

	let l_cp_c = local
		.changed_paths(
			&GitSource::Commit(l_head.clone()),
			MAX_COMMIT_FILES,
			read,
		)
		.unwrap();
	let r_cp_c = remote
		.changed_paths(
			&GitSource::Commit(r_head.clone()),
			MAX_COMMIT_FILES,
			read,
		)
		.unwrap();
	assert_eq!(l_cp_c, r_cp_c);

	let l_cp_r = local
		.changed_paths(
			&GitSource::Range(l_tag.clone(), l_head.clone()),
			MAX_COMMIT_FILES,
			read,
		)
		.unwrap();
	let r_cp_r = remote
		.changed_paths(
			&GitSource::Range(r_tag.clone(), r_head.clone()),
			MAX_COMMIT_FILES,
			read,
		)
		.unwrap();
	assert_eq!(l_cp_r, r_cp_r);

	let l_prev_w1 = local
		.preview(&GitSource::Working, "initial.txt", None, read)
		.unwrap();
	let r_prev_w1 = remote
		.preview(&GitSource::Working, "initial.txt", None, read)
		.unwrap();
	assert_eq!(l_prev_w1, r_prev_w1);

	let l_prev_w2 = local
		.preview(
			&GitSource::Working,
			"initial.txt",
			Some((ChangeType::Modified, None)),
			read,
		)
		.unwrap();
	let r_prev_w2 = remote
		.preview(
			&GitSource::Working,
			"initial.txt",
			Some((ChangeType::Modified, None)),
			read,
		)
		.unwrap();
	assert_eq!(l_prev_w2, r_prev_w2);

	let l_prev_s = local
		.preview(&GitSource::Staged, "staged.txt", None, read)
		.unwrap();
	let r_prev_s = remote
		.preview(&GitSource::Staged, "staged.txt", None, read)
		.unwrap();
	assert_eq!(l_prev_s, r_prev_s);

	let l_prev_c = local
		.preview(
			&GitSource::Commit(l_head.clone()),
			"branch_file.txt",
			None,
			read,
		)
		.unwrap();
	let r_prev_c = remote
		.preview(
			&GitSource::Commit(r_head.clone()),
			"branch_file.txt",
			None,
			read,
		)
		.unwrap();
	assert_eq!(l_prev_c, r_prev_c);

	let l_prev_r = local
		.preview(
			&GitSource::Range(l_tag.clone(), l_head.clone()),
			"branch_file.txt",
			None,
			read,
		)
		.unwrap();
	let r_prev_r = remote
		.preview(
			&GitSource::Range(r_tag.clone(), r_head.clone()),
			"branch_file.txt",
			None,
			read,
		)
		.unwrap();
	assert_eq!(l_prev_r, r_prev_r);

	let l_cft_w = local
		.changed_file_text(&GitSource::Working, "initial.txt", 1024, read)
		.unwrap();
	let r_cft_w = remote
		.changed_file_text(&GitSource::Working, "initial.txt", 1024, read)
		.unwrap();
	assert_eq!(l_cft_w, r_cft_w);

	let l_cft_s = local
		.changed_file_text(&GitSource::Staged, "staged.txt", 1024, read)
		.unwrap();
	let r_cft_s = remote
		.changed_file_text(&GitSource::Staged, "staged.txt", 1024, read)
		.unwrap();
	assert_eq!(l_cft_s, r_cft_s);

	let l_cft_c = local
		.changed_file_text(
			&GitSource::Commit(l_tag.clone()),
			"initial.txt",
			1024,
			read,
		)
		.unwrap();
	let r_cft_c = remote
		.changed_file_text(
			&GitSource::Commit(r_tag.clone()),
			"initial.txt",
			1024,
			read,
		)
		.unwrap();
	assert_eq!(l_cft_c, r_cft_c);

	let l_cdir = local.commit_directory(&l_head, "", 100, read).unwrap();
	let r_cdir = remote.commit_directory(&r_head, "", 100, read).unwrap();
	assert_eq!(l_cdir, r_cdir);

	let l_cb = local
		.commit_blob(&l_tag, "initial.txt", 1024, read)
		.unwrap();
	let r_cb = remote
		.commit_blob(&r_tag, "initial.txt", 1024, read)
		.unwrap();
	assert_eq!(l_cb, r_cb);
}

#[test]
fn every_repo_view_method_matches_a_local_repo() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	setup_rich_repo(&repo);

	let (w, _) = test_worker(&[&repo], None);
	let (client, ws_id) = paired_client(&w);
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};
	let local = LocalRepo::open(&repo, &read).unwrap();
	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	assert_repo_views_match(&local, &remote, &read);

	// Subfolder in parent share
	let parent = tmp.path().join("parent");
	let sub_repo = parent.join("sub").join("repo");
	setup_rich_repo(&sub_repo);
	let (w2, _) = test_worker(&[&parent], None);
	let (client2, ws2_id) = paired_client(&w2);
	let local2 = LocalRepo::open(&sub_repo, &read).unwrap();
	let remote2 = RemoteRepo::new(Arc::new(client2), ws2_id, "sub/repo".into());
	assert_repo_views_match(&local2, &remote2, &read);

	#[cfg(unix)]
	{
		let sym_parent = tmp.path().join("sym_parent");
		fs::create_dir_all(&sym_parent).unwrap();
		let real_repo = sym_parent.join("real");
		setup_rich_repo(&real_repo);
		let sym_repo = sym_parent.join("sym");
		std::os::unix::fs::symlink(&real_repo, &sym_repo).unwrap();
		let (w3, _) = test_worker(&[&sym_repo], None);
		let (client3, ws3_id) = paired_client(&w3);
		let local3 = LocalRepo::open(&sym_repo, &read).unwrap();
		let remote3 = RemoteRepo::new(Arc::new(client3), ws3_id, "".into());
		assert_repo_views_match(&local3, &remote3, &read);
	}
}

#[test]
fn revision_syntax_beyond_oids_and_refnames_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("f.txt"), "hello").unwrap();
	run_git(&ws, &["add", "f.txt"]);
	run_git(&ws, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let pwned_path = tmp.path().join("pwned");
	let pwned_arg = format!("--output={}", pwned_path.display());

	let bad_revs = [
		pwned_arg,
		":/x".into(),
		"HEAD@{0}".into(),
		"HEAD:a".into(),
		"a..b".into(),
		"../../x".into(),
		"refs/heads/../x".into(),
		"-x".into(),
		"@{upstream}".into(),
		"HEAD~1".into(),
		"HEAD^".into(),
	];

	let mut req_count = 0;
	for bad in &bad_revs {
		let queries = [
			GitQuery::ResolveCommit { rev: bad.clone() },
			GitQuery::CommitDetails { sha: bad.clone() },
			GitQuery::CommitBlob {
				rev: bad.clone(),
				path: "f.txt".into(),
				max: 1024,
			},
			GitQuery::CommitDirectory {
				rev: bad.clone(),
				dir: "".into(),
				limit: 100,
			},
			GitQuery::ChangedPaths {
				source: GitSource::Commit(bad.clone()),
			},
			GitQuery::ChangedPaths {
				source: GitSource::Range(bad.clone(), "HEAD".into()),
			},
			GitQuery::ChangedPaths {
				source: GitSource::Range("HEAD".into(), bad.clone()),
			},
			GitQuery::Preview {
				source: GitSource::Commit(bad.clone()),
				path: "f.txt".into(),
				change: None,
			},
			GitQuery::Preview {
				source: GitSource::Range(bad.clone(), "HEAD".into()),
				path: "f.txt".into(),
				change: None,
			},
			GitQuery::Preview {
				source: GitSource::Range("HEAD".into(), bad.clone()),
				path: "f.txt".into(),
				change: None,
			},
			GitQuery::HistoryQuery {
				reference: Some(bad.clone()),
				query: LogQuery {
					text: "".into(),
					regex: false,
					match_case: false,
					author: None,
					since: None,
					until: None,
					paths: vec![],
				},
				skip: 0,
				limit: 10,
			},
		];

		for q in queries {
			let res = client.git(&ws_id, "", ReadProfile::Interactive, q, None);
			assert!(
				matches!(
					res,
					Err(RemoteError::Refused {
						code: ErrorCode::BadRequest,
						..
					})
				),
				"expected BadRequest for rev {bad:?}, got {res:?}"
			);
			req_count += 1;
		}
	}

	assert!(!pwned_path.exists(), "pwned file must not be created");
	assert_eq!(w.git_requests_seen(), req_count);

	// Bad paths
	let bad_paths = ["../x", "/etc/passwd", "-x", ":x", "", "foo\0bar"];
	for bad_p in bad_paths {
		let q_prev = GitQuery::Preview {
			source: GitSource::Working,
			path: bad_p.into(),
			change: None,
		};
		let res =
			client.git(&ws_id, "", ReadProfile::Interactive, q_prev, None);
		assert!(
			matches!(
				res,
				Err(RemoteError::Refused {
					code: ErrorCode::BadRequest,
					..
				})
			),
			"expected BadRequest for path {bad_p:?}"
		);

		let q_text = GitQuery::ChangedFileText {
			source: GitSource::Working,
			path: bad_p.into(),
			max: 1024,
		};
		let res =
			client.git(&ws_id, "", ReadProfile::Interactive, q_text, None);
		assert!(
			matches!(
				res,
				Err(RemoteError::Refused {
					code: ErrorCode::BadRequest,
					..
				})
			),
			"expected BadRequest for path {bad_p:?}"
		);

		let q_blob = GitQuery::CommitBlob {
			rev: "HEAD".into(),
			path: bad_p.into(),
			max: 1024,
		};
		let res =
			client.git(&ws_id, "", ReadProfile::Interactive, q_blob, None);
		assert!(
			matches!(
				res,
				Err(RemoteError::Refused {
					code: ErrorCode::BadRequest,
					..
				})
			),
			"expected BadRequest for path {bad_p:?}"
		);
	}

	// Limit > REMOTE_MAX_LOG_LIMIT
	let res = client.git(
		&ws_id,
		"",
		ReadProfile::Interactive,
		GitQuery::LogFromTips {
			tips: vec![],
			skip: 0,
			limit: REMOTE_MAX_LOG_LIMIT + 1,
		},
		None,
	);
	assert!(matches!(
		res,
		Err(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})
	));

	let res = client.git(
		&ws_id,
		"",
		ReadProfile::Interactive,
		GitQuery::HistoryQuery {
			reference: None,
			query: LogQuery {
				text: "".into(),
				regex: false,
				match_case: false,
				author: None,
				since: None,
				until: None,
				paths: vec![],
			},
			skip: 0,
			limit: REMOTE_MAX_LOG_LIMIT + 1,
		},
		None,
	);
	assert!(matches!(
		res,
		Err(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})
	));

	// Tips with non-hex entry
	let res = client.git(
		&ws_id,
		"",
		ReadProfile::Interactive,
		GitQuery::LogFromTips {
			tips: vec!["not_a_hex_oid".into()],
			skip: 0,
			limit: 10,
		},
		None,
	);
	assert!(matches!(
		res,
		Err(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})
	));

	// Tips > REMOTE_MAX_TIPS
	let res = client.git(
		&ws_id,
		"",
		ReadProfile::Interactive,
		GitQuery::LogFromTips {
			tips: vec!["abcd".into(); REMOTE_MAX_TIPS + 1],
			skip: 0,
			limit: 10,
		},
		None,
	);
	assert!(matches!(
		res,
		Err(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})
	));
}

#[test]
fn an_unknown_but_valid_ref_is_invalid_revision_not_bad_request() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("f.txt"), "hello").unwrap();
	run_git(&ws, &["add", "f.txt"]);
	run_git(&ws, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);
	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};
	let res = remote.resolve_commit("nosuchbranch", &read);
	assert!(
		matches!(res, Err(GitError::InvalidRevision(_))),
		"expected InvalidRevision, got {res:?}"
	);
}

#[test]
fn preview_parents_are_recomputed_on_the_worker() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	setup_rich_repo(&ws);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};

	let local = LocalRepo::open(&ws, &read).unwrap();
	let merge_sha = local.resolve_commit("HEAD", &read).unwrap();

	let raw = client
		.git(
			&ws_id,
			"",
			ReadProfile::Interactive,
			GitQuery::Preview {
				source: GitSource::Commit(merge_sha.clone()),
				path: "branch_file.txt".into(),
				change: Some(ChangeType::Modified),
			},
			None,
		)
		.unwrap();
	let GitReply::Preview(raw_prev) = raw else {
		panic!("expected Preview reply");
	};

	let local_prev = local
		.preview(
			&GitSource::Commit(merge_sha.clone()),
			"branch_file.txt",
			Some((ChangeType::Modified, None)),
			&read,
		)
		.unwrap();
	assert_eq!(raw_prev, local_prev);

	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	let bogus_parents = ["deadbeef".to_string()];
	let remote_bogus = remote
		.preview(
			&GitSource::Commit(merge_sha.clone()),
			"branch_file.txt",
			Some((ChangeType::Modified, Some(&bogus_parents))),
			&read,
		)
		.unwrap();
	let remote_none = remote
		.preview(
			&GitSource::Commit(merge_sha),
			"branch_file.txt",
			None,
			&read,
		)
		.unwrap();
	assert_eq!(remote_bogus, remote_none);
}

#[cfg(unix)]
#[test]
fn working_preview_of_a_symlink_out_of_the_share_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&outside).unwrap();
	fs::write(outside.join("secret.txt"), "TOPSECRET_PREVIEW_DATA").unwrap();

	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("tracked.txt"), "initial").unwrap();
	run_git(&ws, &["add", "tracked.txt"]);
	run_git(&ws, &["commit", "-m", "init"]);

	// 1. Untracked symlink out
	std::os::unix::fs::symlink(
		outside.join("secret.txt"),
		ws.join("sym_untracked.txt"),
	)
	.unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);
	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};

	let res =
		remote.preview(&GitSource::Working, "sym_untracked.txt", None, &read);
	assert!(res.is_err());
	let debug_text = format!("{res:?}");
	assert!(
		!debug_text.contains("TOPSECRET"),
		"secret leaked in error: {debug_text}"
	);

	// 2. Tracked file replaced by a symlink out
	fs::remove_file(ws.join("tracked.txt")).unwrap();
	std::os::unix::fs::symlink(
		outside.join("secret.txt"),
		ws.join("tracked.txt"),
	)
	.unwrap();

	let res = remote.preview(&GitSource::Working, "tracked.txt", None, &read);
	assert!(res.is_err());
	let debug_text = format!("{res:?}");
	assert!(
		!debug_text.contains("TOPSECRET"),
		"secret leaked in error: {debug_text}"
	);
}

#[cfg(unix)]
#[test]
fn changed_file_text_of_a_symlink_out_of_the_share_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&outside).unwrap();
	fs::write(outside.join("secret.txt"), "TOPSECRET_FILE_TEXT_DATA").unwrap();

	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("tracked.txt"), "initial").unwrap();
	run_git(&ws, &["add", "tracked.txt"]);
	run_git(&ws, &["commit", "-m", "init"]);

	// 1. Untracked symlink out
	std::os::unix::fs::symlink(
		outside.join("secret.txt"),
		ws.join("sym_untracked.txt"),
	)
	.unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);
	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};

	let res = remote.changed_file_text(
		&GitSource::Working,
		"sym_untracked.txt",
		1024,
		&read,
	);
	assert!(res.is_err());
	let debug_text = format!("{res:?}");
	assert!(
		!debug_text.contains("TOPSECRET"),
		"secret leaked in error: {debug_text}"
	);

	// 2. Tracked file replaced by a symlink out
	fs::remove_file(ws.join("tracked.txt")).unwrap();
	std::os::unix::fs::symlink(
		outside.join("secret.txt"),
		ws.join("tracked.txt"),
	)
	.unwrap();

	let res = remote.changed_file_text(
		&GitSource::Working,
		"tracked.txt",
		1024,
		&read,
	);
	assert!(res.is_err());
	let debug_text = format!("{res:?}");
	assert!(
		!debug_text.contains("TOPSECRET"),
		"secret leaked in error: {debug_text}"
	);
}

#[test]
fn outside_object_by_sha_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	let inside = tmp.path().join("inside");
	fs::create_dir_all(&outside).unwrap();
	fs::create_dir_all(&inside).unwrap();

	// Repo B outside with secret
	let repo_b = outside.join("b");
	run_git(&repo_b, &["init", "-b", "main"]);
	fs::write(repo_b.join("secret.txt"), "SECRET_OBJECT_CONTENT").unwrap();
	run_git(&repo_b, &["add", "secret.txt"]);
	run_git(&repo_b, &["commit", "-m", "secret commit"]);
	let mut cmd = std::process::Command::new("git");
	let b_sha = String::from_utf8(
		cmd.args(["rev-parse", "HEAD"])
			.current_dir(&repo_b)
			.output()
			.unwrap()
			.stdout,
	)
	.unwrap()
	.trim()
	.to_string();

	// Repo A inside pointing alternates to B
	let repo_a = inside.join("a");
	run_git(&repo_a, &["init", "-b", "main"]);
	fs::write(repo_a.join("normal.txt"), "normal").unwrap();
	run_git(&repo_a, &["add", "normal.txt"]);
	run_git(&repo_a, &["commit", "-m", "init"]);

	let alternates = repo_a
		.join(".git")
		.join("objects")
		.join("info")
		.join("alternates");
	let _ = fs::create_dir_all(alternates.parent().unwrap());
	fs::write(
		&alternates,
		format!("{}\n", repo_b.join(".git").join("objects").display()),
	)
	.unwrap();

	// Repo C inside unrelated
	let repo_c = inside.join("c");
	run_git(&repo_c, &["init", "-b", "main"]);
	fs::write(repo_c.join("c.txt"), "c").unwrap();
	run_git(&repo_c, &["add", "c.txt"]);
	run_git(&repo_c, &["commit", "-m", "init"]);

	let (w, _) = test_worker(&[&inside], None);
	let (client, ws_id) = paired_client(&w);
	let client = Arc::new(client);
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};

	let remote_a = RemoteRepo::new(client.clone(), ws_id.clone(), "a".into());
	let res_a = remote_a.commit_blob(&b_sha, "secret.txt", 1024, &read);
	assert!(res_a.is_err(), "repo A with outside alternates must error");
	let debug_a = format!("{res_a:?}");
	assert!(!debug_a.contains("SECRET_OBJECT_CONTENT"));

	let remote_c = RemoteRepo::new(client, ws_id, "c".into());
	let res_c = remote_c.commit_blob(&b_sha, "secret.txt", 1024, &read);
	assert!(res_c.is_err(), "repo C with foreign sha must error");
	let debug_c = format!("{res_c:?}");
	assert!(!debug_c.contains("SECRET_OBJECT_CONTENT"));
}

#[test]
fn refused_git_view_error_does_not_name_outside_paths() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	let share = tmp.path().join("share");
	fs::create_dir_all(&outside).unwrap();
	fs::create_dir_all(&share).unwrap();

	let outside_repo = outside.join("repo");
	run_git(&outside_repo, &["init", "-b", "main"]);
	fs::write(outside_repo.join("file.txt"), "data").unwrap();
	run_git(&outside_repo, &["add", "file.txt"]);
	run_git(&outside_repo, &["commit", "-m", "init"]);

	// 1. Linked worktree of an outside repo
	let wt = share.join("wt");
	run_git(&outside_repo, &["worktree", "add", wt.to_str().unwrap()]);

	// 2. Gitdir file pointing outside
	let gitdir_repo = share.join("gitdir_repo");
	fs::create_dir_all(&gitdir_repo).unwrap();
	fs::write(
		gitdir_repo.join(".git"),
		format!("gitdir: {}\n", outside_repo.join(".git").display()),
	)
	.unwrap();

	// 3. Repo with core.worktree pointing outside
	let worktree_outside = share.join("worktree_outside");
	run_git(&worktree_outside, &["init", "-b", "main"]);
	run_git(
		&worktree_outside,
		&["config", "core.worktree", outside.to_str().unwrap()],
	);

	// 4. Folder inside parent repository
	let parent = share.join("parent");
	run_git(&parent, &["init", "-b", "main"]);
	let sub = parent.join("sub");
	fs::create_dir_all(&sub).unwrap();

	let (w, _) = test_worker(&[&share], None);
	let (client, ws_id) = paired_client(&w);

	let outside_str = outside.to_string_lossy().into_owned();
	let outside_canon = dunce::canonicalize(&outside)
		.unwrap()
		.to_string_lossy()
		.into_owned();

	for rel in ["wt", "gitdir_repo", "worktree_outside"] {
		let res = client.git(
			&ws_id,
			rel,
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		);
		match res {
			Err(RemoteError::Refused { code, message }) => {
				assert_eq!(
					code,
					ErrorCode::OutsideShare,
					"expected OutsideShare for {rel}, got {code:?}"
				);
				assert!(
					!message.contains(&outside_str)
						&& !message.contains(&outside_canon),
					"message leaked outside path: {message}"
				);
			}
			other => panic!("expected Refused error for {rel}, got {other:?}"),
		}
	}

	let res_parent = client.git(
		&ws_id,
		"parent/sub",
		ReadProfile::Interactive,
		GitQuery::ChangeList,
		None,
	);
	match res_parent {
		Err(RemoteError::Refused { code, message }) => {
			assert_eq!(
				code,
				ErrorCode::NotARepository,
				"expected NotARepository for parent/sub, got {code:?}"
			);
			assert!(
				!message.contains(&outside_str)
					&& !message.contains(&outside_canon),
				"message leaked outside path: {message}"
			);
		}
		other => panic!("expected Refused error for parent/sub, got {other:?}"),
	}
}

#[cfg(unix)]
#[test]
fn repo_through_a_symlink_out_of_the_share_is_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside");
	let share = tmp.path().join("share");
	fs::create_dir_all(&outside).unwrap();
	fs::create_dir_all(&share).unwrap();

	let outside_repo = outside.join("repo");
	run_git(&outside_repo, &["init", "-b", "main"]);
	fs::write(outside_repo.join("file.txt"), "data").unwrap();
	run_git(&outside_repo, &["add", "file.txt"]);
	run_git(&outside_repo, &["commit", "-m", "init"]);

	std::os::unix::fs::symlink(&outside_repo, share.join("sym_repo")).unwrap();

	let (w, _) = test_worker(&[&share], None);
	let (client, ws_id) = paired_client(&w);

	let res = client.git(
		&ws_id,
		"sym_repo",
		ReadProfile::Interactive,
		GitQuery::ChangeList,
		None,
	);
	assert!(
		matches!(
			res,
			Err(RemoteError::Refused {
				code: ErrorCode::OutsideShare,
				..
			})
		),
		"expected OutsideShare for symlink out, got {res:?}"
	);
}

#[test]
fn repo_dotdot_and_absolute_are_refused() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let res1 = client.git(
		&ws_id,
		"../outside",
		ReadProfile::Interactive,
		GitQuery::ChangeList,
		None,
	);
	assert!(matches!(
		res1,
		Err(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})
	));

	let res2 = client.git(
		&ws_id,
		"/outside",
		ReadProfile::Interactive,
		GitQuery::ChangeList,
		None,
	);
	assert!(matches!(
		res2,
		Err(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})
	));
}

#[test]
fn git_view_on_a_non_repo_share_is_not_the_parent() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outer = tmp.path().join("outer");
	let inner = outer.join("inner");
	fs::create_dir_all(&inner).unwrap();

	run_git(&outer, &["init", "-b", "main"]);
	fs::write(outer.join("dirty_outside_secret.txt"), "dirty").unwrap();

	let (w, _) = test_worker(&[&inner], None);
	let (client, ws_id) = paired_client(&w);

	let res = client.git(
		&ws_id,
		"",
		ReadProfile::Interactive,
		GitQuery::ChangeList,
		None,
	);
	match res {
		Err(RemoteError::Refused { code, message }) => {
			assert_eq!(code, ErrorCode::NotARepository);
			assert!(
				!message.contains("dirty_outside_secret.txt"),
				"message leaked parent dirty file: {message}"
			);
		}
		other => {
			panic!("expected NotARepository for non-repo share, got {other:?}")
		}
	}
}

#[test]
fn oversized_change_list_is_capped_with_its_total() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("init.txt"), "init").unwrap();
	run_git(&ws, &["add", "init.txt"]);
	run_git(&ws, &["commit", "-m", "init"]);

	// 2100 untracked files
	for i in 0..2100 {
		fs::write(ws.join(format!("u{i:04}.txt")), "").unwrap();
	}

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);
	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};

	let cl = remote.change_list(MAX_CHANGE_ROWS, &read).unwrap();
	assert_eq!(cl.rows.len(), MAX_CHANGE_ROWS);
	assert_eq!(cl.total, 2100);

	// Remove untracked files before creating the 5050 files commit
	for i in 0..2100 {
		let _ = fs::remove_file(ws.join(format!("u{i:04}.txt")));
	}

	// 5050 committed files
	for i in 0..5050 {
		fs::write(ws.join(format!("f{i:04}.txt")), "data").unwrap();
	}
	run_git(&ws, &["add", "-A"]);
	run_git(&ws, &["commit", "-qm", "5050 files"]);

	let head = remote.resolve_commit("HEAD", &read).unwrap();
	let cp = remote
		.changed_paths(&GitSource::Commit(head), MAX_COMMIT_FILES, &read)
		.unwrap();
	assert_eq!(cp.paths.len(), MAX_COMMIT_FILES);
	assert_eq!(cp.total, 5050);
}

#[test]
fn refs_of_a_tag_heavy_repo_are_an_error_not_empty() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("f.txt"), "content").unwrap();
	run_git(&ws, &["add", "f.txt"]);
	run_git(&ws, &["commit", "-qm", "init"]);

	let mut cmd = std::process::Command::new("git");
	let head_sha = String::from_utf8(
		cmd.args(["rev-parse", "HEAD"])
			.current_dir(&ws)
			.output()
			.unwrap()
			.stdout,
	)
	.unwrap()
	.trim()
	.to_string();

	// Create 40_000 tags in packed-refs so refs output exceeds 4 MiB
	let padding = "x".repeat(70);
	let mut packed = String::from("# pack-refs with: sorted-keys\n");
	for i in 0..40_000 {
		packed
			.push_str(&format!("{head_sha} refs/tags/tag_{i:06}_{padding}\n"));
	}
	fs::write(ws.join(".git").join("packed-refs"), packed).unwrap();

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);
	let remote = RemoteRepo::new(Arc::new(client), ws_id, "".into());
	let read = Read {
		profile: ReadProfile::Interactive,
		cancel: None,
	};

	let res = remote.refs(&read);
	assert!(
		res.is_err(),
		"expected Err from oversized refs output, got Ok"
	);
}

#[test]
fn a_preview_too_large_for_a_frame_is_too_large_not_empty() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("init.txt"), "init").unwrap();
	run_git(&ws, &["add", "init.txt"]);
	run_git(&ws, &["commit", "-qm", "init"]);

	// Write ~2.5 MiB of quote characters. In JSON, quotes escape to `\"`,
	// expanding to ~5 MiB for content + ~5 MiB for diff patch > MAX_FRAME (8 MiB).
	let huge_content = "\"".repeat(2_500_000);
	fs::write(ws.join("huge.txt"), &huge_content).unwrap();
	run_git(&ws, &["add", "huge.txt"]);
	run_git(&ws, &["commit", "-qm", "huge quotes"]);

	let (w, _) = test_worker(&[&ws], None);
	let (client, ws_id) = paired_client(&w);

	let res = client.git(
		&ws_id,
		"",
		ReadProfile::Interactive,
		GitQuery::Preview {
			source: GitSource::Commit("HEAD".into()),
			path: "huge.txt".into(),
			change: Some(ChangeType::New),
		},
		None,
	);

	assert!(
		matches!(
			res,
			Err(RemoteError::Refused {
				code: ErrorCode::TooLarge,
				..
			})
		),
		"expected TooLarge error, got {res:?}"
	);
}

#[test]
fn a_git_view_for_an_unshared_workspace_is_forbidden() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	run_git(&ws, &["init", "-b", "main"]);

	let (w, _) = test_worker(&[&ws], None);
	let (client, _) = paired_client(&w);

	let res = client.git(
		"nosuchworkspace",
		"",
		ReadProfile::Interactive,
		GitQuery::ChangeList,
		None,
	);
	assert!(
		matches!(
			res,
			Err(RemoteError::Refused {
				code: ErrorCode::Forbidden,
				..
			})
		),
		"expected Forbidden, got {res:?}"
	);
}

#[test]
fn scan_with_outside_missing_submodule_gitdir_hides_outside_paths() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let outside = tmp.path().join("outside_secret_zone");
	let share = tmp.path().join("share");
	fs::create_dir_all(&share).unwrap();

	let repo = share.join("repo");
	run_git(&repo, &["init", "-b", "main"]);
	fs::write(repo.join("file.txt"), "hello").unwrap();
	run_git(&repo, &["add", "file.txt"]);
	run_git(&repo, &["commit", "-m", "init"]);

	// Create submodule inside repo with .git pointing to an outside missing gitdir
	let sub = repo.join("sub");
	fs::create_dir_all(&sub).unwrap();
	let outside_gitdir = outside.join("modules").join("sub");
	fs::write(
		sub.join(".git"),
		format!("gitdir: {}\n", outside_gitdir.display()),
	)
	.unwrap();
	fs::write(
		repo.join(".gitmodules"),
		"[submodule \"sub\"]\n\tpath = sub\n\turl = https://example.com/sub.git\n",
	)
	.unwrap();
	run_git(&repo, &["add", ".gitmodules"]);
	let head_rev = {
		let out = std::process::Command::new("git")
			.args(["rev-parse", "HEAD"])
			.current_dir(&repo)
			.output()
			.unwrap();
		String::from_utf8(out.stdout).unwrap().trim().to_string()
	};
	run_git(
		&repo,
		&[
			"update-index",
			"--add",
			"--cacheinfo",
			"160000",
			&head_rev,
			"sub",
		],
	);

	let (w, _) = test_worker(&[&share], None);
	let (client, ws_id) = paired_client(&w);

	let scan_res = client
		.scan_repos(&ws_id, None, None)
		.expect("scan_repos succeeds");
	for r in &scan_res.repos {
		if let Err(ref msg) = r.summary {
			assert!(
				!msg.contains("outside_secret_zone")
					&& !msg.contains("modules/sub"),
				"repo summary leaked outside path: {msg}"
			);
		}
	}
	for (rel, err_msg) in &scan_res.errors {
		assert!(
			!err_msg.contains("outside_secret_zone")
				&& !err_msg.contains("modules/sub"),
			"scan error row for {rel} leaked outside path: {err_msg}"
		);
	}
}

#[cfg(unix)]
#[test]
fn scan_with_admission_wait_returns_incomplete_not_timeout() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	let s = timeout_scale() as u64;

	// Create a slow repo
	let slow_repo = ws.join("slow_repo");
	run_git(&slow_repo, &["init", "-b", "main"]);
	fs::write(slow_repo.join("f.txt"), "hello").unwrap();
	run_git(&slow_repo, &["add", "f.txt"]);
	run_git(&slow_repo, &["commit", "-m", "init"]);
	let slow_sleep_str = format!("{:.1}", 2.0 * s as f64);
	fs::write(slow_repo.join(".slow_sleep"), &slow_sleep_str).unwrap();
	fs::write(slow_repo.join(".slow_active"), "").unwrap();

	let (w, _) = test_worker(&[&ws], None);
	// Set scan deadline to 3000ms * s: budget reserve is 1500ms * s (scan_deadline / 2),
	// so budget deadline is entry + 1500ms * s. Admission wait of ~2000ms * s exceeds budget
	// but is well within the 3000ms * s job deadline.
	let scan_deadline = Duration::from_millis(3000 * s);
	w.set_deadlines_for_tests(Duration::from_secs(60 * s), scan_deadline);

	let (client, ws_id) = paired_client(&w);
	let client = Arc::new(client);
	let client1 = client.clone();
	let client2 = client.clone();
	let ws_id2 = ws_id.clone();

	// Client 1 starts scan on ws, holding the scan slot while slow_repo runs (~2s*s)
	let t1 = std::thread::spawn(move || client1.scan_repos(&ws_id, None, None));

	// Wait until client 1 is running
	let wait_deadline = Instant::now() + Duration::from_secs(5 * s);
	while w.running_jobs() == 0 && Instant::now() < wait_deadline {
		std::thread::sleep(Duration::from_millis(10));
	}

	// Client 2 starts scan while scan slot is held
	let res2 = client2.scan_repos(&ws_id2, None, None);

	let _ = t1.join().unwrap();

	// Client 2 must receive an Incomplete scan, NOT a Timeout error
	match res2 {
		Ok(scan) => {
			assert!(
				matches!(
					scan.status,
					snip_core::workspace::ScanStatus::Incomplete
						| snip_core::workspace::ScanStatus::TimedOut
				),
				"expected Incomplete or TimedOut scan status, got {:?}",
				scan.status
			);
		}
		Err(other) => {
			panic!("expected Incomplete scan result, got error {other:?}")
		}
	}
}
