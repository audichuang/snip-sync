//! The Git runner against real processes: budget, queue, cancel, timeouts,
//! output caps and process-tree cleanup. Its own test binary, because the
//! budget is process-global; the tests take a lock so they do not queue
//! behind each other.

use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use snip_core::gitrun::{
	in_flight, leaked_slots, CancelToken, Overflow, RunOptions,
	MAX_CONCURRENT_GIT, MAX_QUEUED_GIT,
};
use snip_core::gitsrc::{CatObject, Git, GitError};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
	SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn repo() -> (tempfile::TempDir, Git) {
	let dir = tempfile::tempdir().unwrap();
	let out = Command::new("git")
		.args(["init", "-q", "-b", "main"])
		.current_dir(dir.path())
		.output()
		.unwrap();
	assert!(out.status.success());
	let git = Git::open(dir.path()).unwrap();
	(dir, git)
}

/// Runs a shell alias: git starts `sh`, which starts the rest, so the
/// process tree is git -> sh -> children on every platform (Git for
/// Windows ships sh and sleep).
fn alias(
	git: &Git,
	script: &str,
	opts: &RunOptions,
) -> Result<Vec<u8>, GitError> {
	let def = format!("alias.t=!{script}");
	git.run_with(&["-c", &def, "t"], opts).map(|o| o.stdout)
}

fn opts(timeout: Duration) -> RunOptions {
	RunOptions {
		timeout,
		..RunOptions::default()
	}
}

fn pid_of(out: &[u8]) -> u32 {
	String::from_utf8_lossy(out)
		.split_whitespace()
		.find_map(|w| w.parse().ok())
		.expect("script printed a pid")
}

/// Unix: the pid is gone or a zombie (killed, not yet reaped by init).
#[cfg(unix)]
fn assert_dead(pid: u32) {
	let deadline = Instant::now() + Duration::from_secs(5);
	loop {
		let out = Command::new("ps")
			.args(["-o", "stat=", "-p", &pid.to_string()])
			.output()
			.unwrap();
		let stat = String::from_utf8_lossy(&out.stdout);
		if stat.trim().is_empty() || stat.trim().starts_with('Z') {
			return;
		}
		assert!(Instant::now() < deadline, "pid {pid} still alive: {stat}");
		thread::sleep(Duration::from_millis(50));
	}
}

#[test]
fn timeout_kills_the_whole_tree_and_releases_the_slot() {
	let _s = serial();
	let (dir, git) = repo();
	let pidfile = dir.path().join("pid");
	let script = format!(
		"sleep 30 & echo $! > '{}'; wait",
		pidfile.display().to_string().replace('\\', "/")
	);
	let start = Instant::now();
	let err = alias(&git, &script, &opts(Duration::from_secs(1))).unwrap_err();
	let elapsed = start.elapsed();
	assert!(matches!(err, GitError::Timeout { .. }), "{err:?}");
	// If only git died, sleep would hold the pipes for 30 s and the reader
	// could not finish: returning quickly proves the tree was killed.
	assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
	assert_eq!(in_flight(), 0);
	assert_eq!(leaked_slots(), 0);
	#[cfg(unix)]
	{
		let pid = std::fs::read_to_string(&pidfile).unwrap();
		assert_dead(pid.trim().parse().unwrap());
	}
}

#[test]
fn descendant_holding_the_pipe_after_root_exit_is_killed() {
	let _s = serial();
	let (_dir, git) = repo();
	let start = Instant::now();
	// sh exits at once; the background sleep keeps stdout open.
	let out = alias(&git, "sleep 30 & echo $!", &opts(Duration::from_secs(15)))
		.unwrap();
	let elapsed = start.elapsed();
	assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
	let _pid = pid_of(&out);
	#[cfg(unix)]
	assert_dead(_pid);
	assert_eq!(in_flight(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn escaped_descendant_fails_the_call_instead_of_hanging() {
	let _s = serial();
	if Command::new("setsid").arg("--version").output().is_err() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"setsid is required for this test"
		);
		eprintln!("skipping: setsid not installed");
		return;
	}
	let (dir, git) = repo();
	let pidfile = dir.path().join("pid");
	// setsid leaves the process group: group kill cannot reach it.
	let script = format!("setsid sleep 30 & echo $! > '{}'", pidfile.display());
	let start = Instant::now();
	let err = alias(&git, &script, &opts(Duration::from_secs(2))).unwrap_err();
	let elapsed = start.elapsed();
	let pid: u32 = std::fs::read_to_string(&pidfile)
		.unwrap()
		.trim()
		.parse()
		.unwrap();
	let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
	assert!(matches!(err, GitError::OutputHeldOpen { .. }), "{err:?}");
	assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
	assert_eq!(in_flight(), 0);
}

#[test]
fn cancel_stops_a_running_process() {
	let _s = serial();
	let (_dir, git) = repo();
	let cancel = CancelToken::new();
	let trigger = cancel.clone();
	let canceller = thread::spawn(move || {
		thread::sleep(Duration::from_millis(300));
		trigger.cancel();
	});
	let start = Instant::now();
	let err = alias(
		&git,
		"sleep 30",
		&RunOptions {
			cancel: Some(cancel),
			..opts(Duration::from_secs(60))
		},
	)
	.unwrap_err();
	canceller.join().unwrap();
	assert!(matches!(err, GitError::Cancelled { .. }), "{err:?}");
	assert!(start.elapsed() < Duration::from_secs(10));
	assert_eq!(in_flight(), 0);
}

#[test]
fn full_budget_queues_then_cancel_or_queue_timeout_never_spawn() {
	let _s = serial();
	let (_dir, git) = repo();
	// Two long-lived cat-file sessions on other threads hold both slots.
	let (release_tx, release_rx) = mpsc::channel::<()>();
	let release_rx = std::sync::Arc::new(Mutex::new(release_rx));
	let (ready_tx, ready_rx) = mpsc::channel();
	let holders: Vec<_> = (0..MAX_CONCURRENT_GIT)
		.map(|_| {
			let git = git.clone();
			let ready = ready_tx.clone();
			let release = release_rx.clone();
			thread::spawn(move || {
				let cat = git.cat_file().unwrap();
				ready.send(()).unwrap();
				release.lock().unwrap().recv().unwrap();
				cat.close().unwrap();
			})
		})
		.collect();
	for _ in 0..MAX_CONCURRENT_GIT {
		ready_rx.recv().unwrap();
	}
	assert_eq!(in_flight(), MAX_CONCURRENT_GIT);

	let queued = RunOptions {
		queue_timeout: Duration::from_millis(300),
		..RunOptions::default()
	};
	let err = git.run_with(&["--version"], &queued).unwrap_err();
	assert!(matches!(err, GitError::QueueTimeout { .. }), "{err:?}");

	let cancel = CancelToken::new();
	cancel.cancel();
	let err = git
		.run_with(
			&["--version"],
			&RunOptions {
				cancel: Some(cancel),
				..RunOptions::default()
			},
		)
		.unwrap_err();
	assert!(matches!(err, GitError::Cancelled { .. }), "{err:?}");
	assert_eq!(in_flight(), MAX_CONCURRENT_GIT);

	// The queue itself is bounded: MAX_QUEUED_GIT waiters, then refusal.
	let stop_waiting = CancelToken::new();
	let waiters: Vec<_> = (0..MAX_QUEUED_GIT)
		.map(|_| {
			let git = git.clone();
			let cancel = stop_waiting.clone();
			thread::spawn(move || {
				git.run_with(
					&["--version"],
					&RunOptions {
						cancel: Some(cancel),
						queue_timeout: Duration::from_secs(60),
						..RunOptions::default()
					},
				)
			})
		})
		.collect();
	let deadline = Instant::now() + Duration::from_secs(10);
	while snip_core::gitrun::queued() < MAX_QUEUED_GIT {
		assert!(
			waiters.iter().all(|w| !w.is_finished()),
			"a waiter ended early"
		);
		assert!(Instant::now() < deadline, "waiters never queued");
		thread::sleep(Duration::from_millis(10));
	}
	let err = git
		.run_with(&["--version"], &RunOptions::default())
		.unwrap_err();
	assert!(matches!(err, GitError::QueueFull { .. }), "{err:?}");
	stop_waiting.cancel();
	for w in waiters {
		let err = w.join().unwrap().unwrap_err();
		assert!(matches!(err, GitError::Cancelled { .. }), "{err:?}");
	}
	assert_eq!(in_flight(), MAX_CONCURRENT_GIT);

	// A queued caller gets the slot once one is freed.
	let waiter = {
		let git = git.clone();
		thread::spawn(move || git.run(&["--version"]))
	};
	thread::sleep(Duration::from_millis(100));
	release_tx.send(()).unwrap();
	assert!(waiter.join().unwrap().is_ok());
	release_tx.send(()).unwrap();
	for h in holders {
		h.join().unwrap();
	}
	assert_eq!(in_flight(), 0);
}

#[test]
fn a_thread_holding_a_slot_cannot_start_a_second_process() {
	let _s = serial();
	let (_dir, git) = repo();
	let cat = git.cat_file().unwrap();
	let err = git.run(&["--version"]).unwrap_err();
	assert!(matches!(err, GitError::NestedProcess { .. }), "{err:?}");
	cat.close().unwrap();
	assert!(git.run(&["--version"]).is_ok());
	assert_eq!(in_flight(), 0);
}

fn big_blob(dir: &Path, git: &Git, len: usize) -> String {
	std::fs::write(dir.join("big.txt"), "x".repeat(len)).unwrap();
	let oid = git.run(&["hash-object", "-w", "big.txt"]).unwrap();
	String::from_utf8(oid).unwrap().trim().to_string()
}

#[test]
fn strict_overflow_errors_and_preview_truncation_is_explicit() {
	let _s = serial();
	let (dir, git) = repo();
	let oid = big_blob(dir.path(), &git, 200_000);
	let capped = |overflow| RunOptions {
		max_stdout: 1000,
		overflow,
		..RunOptions::default()
	};
	let err = git
		.run_with(&["cat-file", "blob", &oid], &capped(Overflow::Error))
		.unwrap_err();
	assert!(
		matches!(err, GitError::OutputLimit { limit: 1000, .. }),
		"{err:?}"
	);
	let cut = git
		.run_with(&["cat-file", "blob", &oid], &capped(Overflow::Truncate))
		.unwrap();
	assert!(cut.truncated);
	assert_eq!(cut.stdout.len(), 1000);
	let whole =
		git.run_with(&["cat-file", "blob", &oid], &RunOptions::default());
	let whole = whole.unwrap();
	assert!(!whole.truncated);
	assert_eq!(whole.stdout.len(), 200_000);
	assert_eq!(in_flight(), 0);
}

#[test]
fn cat_file_judges_size_from_the_header_and_stays_in_sync() {
	let _s = serial();
	let (dir, git) = repo();
	let big = big_blob(dir.path(), &git, 300_000);
	std::fs::write(dir.path().join("small.txt"), "small\n").unwrap();
	let small = git.run(&["hash-object", "-w", "small.txt"]).unwrap();
	let small = String::from_utf8(small).unwrap().trim().to_string();

	let mut cat = git.cat_file().unwrap();
	assert_eq!(
		cat.read_object(&big, 1024).unwrap(),
		CatObject::TooLarge {
			oid: big.clone(),
			size: 300_000
		}
	);
	// The skipped body did not desync the next answer.
	assert_eq!(
		cat.read_object(&small, 1024).unwrap(),
		CatObject::Found {
			oid: small,
			kind: "blob".into(),
			body: b"small\n".to_vec()
		}
	);
	assert_eq!(
		cat.read_object("HEAD:nope", 1024).unwrap(),
		CatObject::Missing
	);
	cat.close().unwrap();
	assert_eq!(in_flight(), 0);
}

#[test]
fn only_an_unresolvable_revision_is_invalid_a_failing_git_is_not() {
	let _s = serial();
	let (dir, git) = repo();
	// Unborn HEAD is an answer, not an error.
	assert_eq!(git.head().unwrap(), None);
	assert!(matches!(
		git.resolve_commit("no-such-ref"),
		Err(GitError::InvalidRevision(_))
	));
	// Break the repository: git itself now fails, which must not read as
	// "no such revision".
	std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();
	let err = git.resolve_commit("main").unwrap_err();
	assert!(matches!(err, GitError::Failed { .. }), "{err:?}");
	let err = git.head().unwrap_err();
	assert!(matches!(err, GitError::Failed { .. }), "{err:?}");
}
