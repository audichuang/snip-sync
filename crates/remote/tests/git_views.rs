//! Loopback integration tests for worker git views and repository scanning.

use std::fs;
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
	SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

use rustls::StreamOwned;
use snip_core::workspace::ScanStatus;
use snip_remote::proto::{
	read_frame, write_frame, ErrorCode, GitQuery, Request, Response,
	PROTOCOL_MAX, PROTOCOL_VERSION,
};
use snip_remote::tls::{client_config, server_name};
use snip_remote::{
	pair, Client, Connection, Identity, RemoteError, Worker, WorkerOptions,
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
	let scan_res = conn.call(&scan_req, None, Duration::from_secs(5));
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
	let git_res = conn.call(&git_req, None, Duration::from_secs(5));
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
	tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
	tcp.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
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
	tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
	tcp.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
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
	let pid_deadline = Instant::now() + Duration::from_secs(5);
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
	let wait_deadline = Instant::now() + Duration::from_secs(10);
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
	// Short scan deadline of 2 seconds for this test
	w.set_deadlines_for_tests(
		Duration::from_secs(60),
		Duration::from_millis(1500),
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
	let pid_deadline = Instant::now() + Duration::from_secs(5);
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
	let wait_deadline = Instant::now() + Duration::from_secs(10);
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

	let deadline = Instant::now() + Duration::from_secs(10);
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
	tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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
	let deadline = Instant::now() + Duration::from_secs(8);
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

	let wait_deadline = Instant::now() + Duration::from_secs(5);
	while w.running_jobs() > 0 && Instant::now() < wait_deadline {
		std::thread::sleep(Duration::from_millis(20));
	}
	assert_eq!(w.running_jobs(), 0);
}

#[cfg(unix)]
#[test]
fn scan_of_many_slow_repos_returns_partial_not_timeout() {
	let _serial = serial();
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
	for i in 1..=4 {
		let repo_dir = ws.join(format!("slow-{i}"));
		run_git(&repo_dir, &["init", "-b", "main"]);
		fs::write(repo_dir.join("f.txt"), "s").unwrap();
		run_git(&repo_dir, &["add", "f.txt"]);
		run_git(&repo_dir, &["commit", "-m", "init"]);
		fs::write(repo_dir.join(".slow_sleep"), "1").unwrap();
		fs::write(repo_dir.join(".slow_active"), "").unwrap();
	}

	let (w, _) = test_worker(&[&ws], None);
	// Scan deadline 8s -> budget is 8s - 5s = 3s
	w.set_deadlines_for_tests(Duration::from_secs(60), Duration::from_secs(8));

	let (client, ws_id) = paired_client(&w);
	let start = Instant::now();
	let scan = client.scan_repos(&ws_id, None, None).unwrap();
	let elapsed = start.elapsed();

	assert!(
		elapsed < Duration::from_secs(7),
		"should complete before the 8s job deadline, took {elapsed:?}"
	);
	assert_eq!(scan.status, ScanStatus::TimedOut);

	// Fast repo should have been summarized cleanly
	let fast_repo = scan
		.repos
		.iter()
		.find(|r| r.rel == "fast-a")
		.expect("fast-a should be found");
	assert!(fast_repo.summary.is_ok());

	// Unread repos should have error rows containing "not read"
	let unread = scan.repos.iter().filter(|r| {
		r.summary
			.as_ref()
			.err()
			.is_some_and(|msg| msg.contains("not read"))
	});
	assert!(unread.count() >= 1, "at least one repo timed out unread");
}
