#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snip_core::gitrun::{
	served_in_flight, served_queued, CancelToken, RunOptions,
	MAX_CONCURRENT_SERVED_GIT, MAX_QUEUED_SERVED_GIT,
};
use snip_core::gitsrc::Git;
use snip_core::gitview::ReadProfile;
use snip_remote::proto::{ErrorCode, GitQuery};
use snip_remote::{pair, Client, Identity, RemoteError, Worker, WorkerOptions};

static SHIM_INIT: std::sync::Once = std::sync::Once::new();

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
		let _ = Box::leak(Box::new(temp_dir));
		let git_shim = shim_dir.join("git");

		let script = format!(
			r#"#!/bin/sh
if [ -f "$PWD/.slow_active" ]; then
  if [ -f "$PWD/.slow_sleep" ]; then
    SLEEP_SECS=$(cat "$PWD/.slow_sleep")
  elif [ -n "$SLOW_GIT_SLEEP" ]; then
    SLEEP_SECS="$SLOW_GIT_SLEEP"
  else
    SLEEP_SECS=0.2
  fi
  sleep "$SLEEP_SECS"
fi
exec "{}" "$@"
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

#[test]
fn a_master_cannot_fill_the_git_queue() {
	init_git_shim();

	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();

	run_git(&ws, &["init", "-b", "main"]);
	fs::write(ws.join("file.txt"), "content").unwrap();
	run_git(&ws, &["add", "file.txt"]);
	run_git(&ws, &["commit", "-m", "init"]);
	fs::write(ws.join(".slow_sleep"), "0.2").unwrap();
	fs::write(ws.join(".slow_active"), "").unwrap();

	let local_repo = tmp.path().join("local_fast");
	run_git(&local_repo, &["init", "-b", "main"]);
	fs::write(local_repo.join("local.txt"), "fast").unwrap();
	run_git(&local_repo, &["add", "local.txt"]);
	run_git(&local_repo, &["commit", "-m", "init"]);

	let id = Identity::generate().unwrap();
	let mut w = Worker::start(
		"127.0.0.1:0".parse().unwrap(),
		&id,
		WorkerOptions {
			name: "queue-worker".into(),
			trust_file: None,
			max_protocol: None,
		},
	)
	.unwrap();
	assert!(w.set_roots(std::slice::from_ref(&ws)).is_empty());
	let ws_id = w.roots()[0].id.clone();

	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired =
		pair(&w.local_addr().to_string(), &code, &master, "mac").unwrap();

	// Create multiple clients so we exceed client-side limiter of 4 in flight
	const NUM_CLIENTS: usize = 20;
	let clients: Vec<Arc<Client>> = (0..NUM_CLIENTS)
		.map(|_| {
			Arc::new(
				Client::new(paired.clone(), master.clone(), "mac".into())
					.unwrap(),
			)
		})
		.collect();

	const CALLS: usize = 70;
	let cancel = CancelToken::new();
	let results = Arc::new(std::sync::Mutex::new(Vec::new()));
	let stopped = Arc::new(AtomicBool::new(false));

	let mut handles = Vec::new();
	for i in 0..CALLS {
		let client = clients[i % clients.len()].clone();
		let ws_clone = ws_id.clone();
		let cancel_clone = cancel.clone();
		let results_clone = results.clone();
		let stopped_clone = stopped.clone();
		handles.push(std::thread::spawn(move || {
			let res = client.git(
				&ws_clone,
				"",
				ReadProfile::Interactive,
				GitQuery::ChangeList,
				Some(&cancel_clone),
			);
			if !stopped_clone.load(Ordering::SeqCst) {
				results_clone.lock().unwrap().push(res);
			}
		}));
	}

	// Poll with bounded deadline: served pool in-flight + queued stays within limit
	let poll_deadline = Instant::now() + Duration::from_secs(10);
	let mut saw_busy = false;
	let mut saw_ok = false;

	while Instant::now() < poll_deadline {
		let in_flight = served_in_flight();
		let queued = served_queued();
		assert!(
			in_flight <= MAX_CONCURRENT_SERVED_GIT,
			"served pool running cap exceeded: in_flight={in_flight}"
		);
		assert!(
			in_flight + queued
				<= MAX_CONCURRENT_SERVED_GIT + MAX_QUEUED_SERVED_GIT,
			"served pool overflow: in_flight={in_flight}, queued={queued}"
		);

		let res = results.lock().unwrap();
		for r in res.iter() {
			match r {
				Ok(_) => saw_ok = true,
				Err(RemoteError::Refused {
					code: ErrorCode::Busy,
					..
				}) => saw_busy = true,
				_ => {}
			}
		}
		if saw_busy && saw_ok {
			break;
		}
		drop(res);
		std::thread::sleep(Duration::from_millis(20));
	}

	assert!(saw_busy, "expected at least one call to return Busy");
	assert!(saw_ok, "expected at least one call to succeed");

	// Meanwhile a same-process LOCAL git operation in a different non-slow repo succeeds
	let local_git = Git::open(&local_repo).unwrap();
	let local_out = local_git
		.run_with(&["status"], &RunOptions::default())
		.expect(
		"local Git::run_with must succeed even when served pool is saturated",
	);
	assert!(
		!local_out.truncated,
		"local git status must not be truncated"
	);

	// Cleanup: remove .slow_active, cancel tokens, stop worker
	let _ = fs::remove_file(ws.join(".slow_active"));
	stopped.store(true, Ordering::SeqCst);
	cancel.cancel();
	w.stop();

	let join_deadline = Instant::now() + Duration::from_secs(10);
	for h in handles {
		let remaining = join_deadline.saturating_duration_since(Instant::now());
		assert!(
			!remaining.is_zero(),
			"timed out waiting for worker threads to finish"
		);
		let _ = h.join();
	}
}
