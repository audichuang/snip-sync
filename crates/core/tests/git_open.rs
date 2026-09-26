//! `Git::open_with` and `Git::head_ref_with` against real Git.
//!
//! Cancellation replaces `PATH`, and the Git budget is process-global, so
//! every test in this binary holds `SERIAL`.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use snip_core::gitrun::{
	in_flight, leaked_slots, CancelToken, Overflow, RunOptions,
};
use snip_core::gitsrc::{Git, GitError};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
	SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn assert_idle() {
	assert_eq!(in_flight(), 0, "git budget slot still held");
	assert_eq!(leaked_slots(), 0, "git budget slot leaked");
}

struct Repo {
	dir: tempfile::TempDir,
}

impl Repo {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let repo = Self { dir };
		repo.git(["init", "-q", "-b", "main"]);
		repo.git(["config", "user.name", "Local"]);
		repo.git(["config", "user.email", "local@example.com"]);
		repo
	}

	fn path(&self) -> &Path {
		self.dir.path()
	}

	fn git<I, S>(&self, args: I) -> String
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		let out = Command::new("git")
			.args(args)
			.current_dir(self.path())
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"git failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout).unwrap().trim().to_string()
	}
}

fn not_a_repo(err: GitError, dir: &Path) {
	match err {
		GitError::NotARepository(path) => assert_eq!(path, dir),
		other => panic!("expected NotARepository, got {other}"),
	}
}

#[test]
fn open_with_matches_open_and_rejects_a_plain_directory() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	let opened = Git::open(repo.path()).unwrap();
	let with = Git::open_with(repo.path(), &RunOptions::default()).unwrap();
	assert_eq!(opened.root(), with.root());

	let plain = tempfile::tempdir().unwrap();
	not_a_repo(Git::open(plain.path()).unwrap_err(), plain.path());
	not_a_repo(
		Git::open_with(plain.path(), &RunOptions::default()).unwrap_err(),
		plain.path(),
	);
	assert_idle();
}

#[test]
fn head_ref_with_matches_branch_unborn_detached_and_real_failures() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	let git = Git::open(repo.path()).unwrap();
	// Unborn: symbolic name exists, commit does not.
	assert_eq!(git.head().unwrap(), None);
	assert_eq!(git.head_ref().unwrap().as_deref(), Some("refs/heads/main"));
	assert_eq!(
		git.head_ref_with(&RunOptions::default()).unwrap(),
		git.head_ref().unwrap()
	);

	fs::write(repo.path().join("a.txt"), b"a\n").unwrap();
	repo.git(["add", "a.txt"]);
	repo.git(["commit", "-qm", "root"]);
	repo.git(["checkout", "--detach", "HEAD"]);
	let git = Git::open(repo.path()).unwrap();
	assert!(git.head().unwrap().is_some());
	assert_eq!(git.head_ref().unwrap(), None);
	assert_eq!(git.head_ref_with(&RunOptions::default()).unwrap(), None);

	fs::write(repo.path().join(".git/HEAD"), b"garbage\n").unwrap();
	let err = git.head_ref_with(&RunOptions::default()).unwrap_err();
	assert!(matches!(err, GitError::Failed { .. }), "{err}");
	let err = git.head_ref().unwrap_err();
	assert!(matches!(err, GitError::Failed { .. }), "{err}");
	assert_idle();
}

#[test]
fn truncated_identity_commands_fail_instead_of_returning_a_prefix() {
	let _lock = serial();
	assert_idle();
	let parent = tempfile::tempdir().unwrap();
	let deep = parent.path().join("p".repeat(160));
	fs::create_dir_all(&deep).unwrap();
	let init = Command::new("git")
		.args(["init", "-q", "-b", "main"])
		.current_dir(&deep)
		.output()
		.unwrap();
	assert!(init.status.success());
	fs::write(deep.join("a.txt"), b"a\n").unwrap();
	for args in [
		["add", "a.txt"].as_slice(),
		["commit", "-qm", "root"].as_slice(),
	] {
		let out = Command::new("git")
			.args(args)
			.current_dir(&deep)
			.env("GIT_AUTHOR_NAME", "Local")
			.env("GIT_AUTHOR_EMAIL", "local@example.com")
			.env("GIT_COMMITTER_NAME", "Local")
			.env("GIT_COMMITTER_EMAIL", "local@example.com")
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{}",
			String::from_utf8_lossy(&out.stderr)
		);
	}
	let git = Git::open(&deep).unwrap();

	let version = Command::new("git").arg("--version").output().unwrap();
	assert!(version.status.success());
	let version_len = version.stdout.len();
	assert!(deep.as_os_str().len() > version_len);

	for overflow in [Overflow::Truncate, Overflow::Error] {
		let tiny = RunOptions {
			max_stdout: 1,
			overflow,
			..RunOptions::default()
		};
		let err = Git::open_with(&deep, &tiny).unwrap_err();
		assert!(
			matches!(err, GitError::OutputLimit { limit: 1, .. }),
			"{overflow:?} open -> {err}"
		);
		let err = git.head_ref_with(&tiny).unwrap_err();
		assert!(
			matches!(
				err,
				GitError::OutputLimit {
					limit: 1,
					ref args,
					..
				} if args.contains("symbolic-ref")
			),
			"{overflow:?} head_ref -> {err}"
		);
	}

	// Version fits; the toplevel line does not. Truncate must not become a root.
	let path_cap = RunOptions {
		max_stdout: version_len,
		overflow: Overflow::Truncate,
		..RunOptions::default()
	};
	let err = Git::open_with(&deep, &path_cap).unwrap_err();
	match err {
		GitError::OutputLimit { args, limit } => {
			assert!(args.contains("rev-parse"), "{args}");
			assert_eq!(limit, version_len);
		}
		other => panic!("{other}"),
	}
	assert_idle();
}

const SHIM_RS: &str = r#"
use std::env;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn stall() -> ! {
    let ready = env::var("SNIP_STALL_READY").expect("ready path");
    fs::write(&ready, format!("{}\n", std::process::id())).expect("ready");
    loop {
        thread::sleep(Duration::from_secs(30));
    }
}

fn main() {
    let phase = env::var("SNIP_STALL_PHASE").unwrap_or_default();
    let real = env::var("SNIP_REAL_GIT").expect("real git");
    let args: Vec<std::ffi::OsString> = env::args_os().skip(1).collect();
    if !phase.is_empty() && args.iter().any(|arg| arg == phase.as_str()) {
        stall();
    }
    let status = Command::new(&real)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("real git");
    std::process::exit(status.code().unwrap_or(1));
}
"#;

fn shim_dir() -> &'static Path {
	static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
	DIR.get_or_init(|| {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("shim.rs");
		fs::write(&src, SHIM_RS).unwrap();
		let exe =
			dir.path()
				.join(if cfg!(windows) { "git.exe" } else { "git" });
		let out = Command::new("rustc")
			.arg("--edition")
			.arg("2021")
			.arg(&src)
			.arg("-o")
			.arg(&exe)
			.output()
			.expect("rustc");
		assert!(
			out.status.success(),
			"shim rustc failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		dir
	})
	.path()
}

fn real_git() -> PathBuf {
	let out = if cfg!(windows) {
		Command::new("where").arg("git").output().unwrap()
	} else {
		Command::new("sh")
			.args(["-c", "command -v git"])
			.output()
			.unwrap()
	};
	assert!(out.status.success(), "cannot locate git");
	let text = String::from_utf8(out.stdout).unwrap();
	PathBuf::from(text.lines().next().unwrap().trim())
}

struct EnvGuard {
	path: Option<std::ffi::OsString>,
	keys: Vec<&'static str>,
}

impl EnvGuard {
	fn arm(phase: &str, ready: &Path) -> Self {
		let guard = Self {
			path: Some(std::env::var_os("PATH").unwrap_or_default()),
			keys: vec!["SNIP_STALL_PHASE", "SNIP_STALL_READY", "SNIP_REAL_GIT"],
		};
		std::env::set_var("SNIP_STALL_PHASE", phase);
		std::env::set_var("SNIP_STALL_READY", ready);
		std::env::set_var("SNIP_REAL_GIT", real_git());
		let mut path = shim_dir().as_os_str().to_os_string();
		path.push(if cfg!(windows) { ";" } else { ":" });
		path.push(guard.path.as_ref().unwrap());
		std::env::set_var("PATH", path);
		guard
	}
}

impl Drop for EnvGuard {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			std::env::set_var("PATH", path);
		}
		for key in self.keys.drain(..) {
			std::env::remove_var(key);
		}
	}
}

struct Running<T> {
	cancel: CancelToken,
	handle: Option<JoinHandle<Result<T, GitError>>>,
}

impl<T: std::fmt::Debug + Send + 'static> Running<T> {
	fn spawn<F>(cancel: CancelToken, work: F) -> Self
	where
		F: FnOnce() -> Result<T, GitError> + Send + 'static,
	{
		let handle = thread::spawn(work);
		Self {
			cancel,
			handle: Some(handle),
		}
	}

	fn wait_ready(&mut self, path: &Path) -> u32 {
		let deadline = Instant::now() + Duration::from_secs(8);
		loop {
			if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
				let result = self.handle.take().unwrap().join();
				panic!("call ended before the shim was ready: {result:?}");
			}
			if let Ok(text) = fs::read_to_string(path) {
				if let Ok(pid) = text.trim().parse::<u32>() {
					if pid > 0 {
						return pid;
					}
				}
			}
			assert!(
				Instant::now() < deadline,
				"git shim never wrote {}",
				path.display()
			);
			thread::sleep(Duration::from_millis(10));
		}
	}
}

impl<T> Drop for Running<T> {
	fn drop(&mut self) {
		self.cancel.cancel();
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

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
		thread::sleep(Duration::from_millis(20));
	}
}

#[cfg(windows)]
fn assert_dead(pid: u32) {
	let out = Command::new("tasklist")
		.args(["/FI", &format!("PID eq {pid}"), "/NH"])
		.output()
		.unwrap();
	let text = String::from_utf8_lossy(&out.stdout);
	assert!(
		!text.split_whitespace().any(|word| word == pid.to_string()),
		"pid {pid} still listed: {text}"
	);
}

fn expect_cancelled(err: GitError, needle: &str) {
	match err {
		GitError::Cancelled { args } => {
			assert!(args.contains(needle), "cancelled args {args:?}");
		}
		other => panic!("{other}"),
	}
}

#[test]
fn open_and_head_ref_honor_pre_cancel_and_in_flight_cancel() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	fs::write(repo.path().join("a.txt"), b"a\n").unwrap();
	repo.git(["add", "a.txt"]);
	repo.git(["commit", "-qm", "root"]);
	let git = Git::open(repo.path()).unwrap();

	let cancel = CancelToken::new();
	cancel.cancel();
	let opts = RunOptions {
		cancel: Some(cancel),
		..RunOptions::default()
	};
	expect_cancelled(
		Git::open_with(repo.path(), &opts).unwrap_err(),
		"--version",
	);
	expect_cancelled(git.head_ref_with(&opts).unwrap_err(), "symbolic-ref");
	assert_idle();

	let phases: &[(&str, &str)] = &[
		("--version", "--version"),
		("rev-parse", "rev-parse"),
		("symbolic-ref", "symbolic-ref"),
	];
	for (phase, needle) in phases {
		let ready = repo.dir.path().join(format!("ready-{phase}"));
		let _ = fs::remove_file(&ready);
		let _env = EnvGuard::arm(phase, &ready);
		let cancel = CancelToken::new();
		let token = cancel.clone();
		let path = repo.path().to_path_buf();
		let git = git.clone();
		let phase = *phase;
		let started = Instant::now();
		let mut running = Running::spawn(cancel.clone(), move || {
			let opts = RunOptions {
				cancel: Some(token),
				timeout: Duration::from_secs(15),
				..RunOptions::default()
			};
			if phase == "symbolic-ref" {
				git.head_ref_with(&opts).map(|_| ())
			} else {
				Git::open_with(&path, &opts).map(|_| ())
			}
		});
		let pid = running.wait_ready(&ready);
		cancel.cancel();
		let err = match running.handle.take().unwrap().join() {
			Ok(Err(err)) => err,
			Ok(Ok(())) => panic!("{phase}: call succeeded after cancel"),
			Err(_) => panic!("{phase}: thread panicked"),
		};
		assert!(
			started.elapsed() < Duration::from_secs(20),
			"{phase} took {:?}",
			started.elapsed()
		);
		expect_cancelled(err, needle);
		assert_dead(pid);
		assert_idle();
	}
}
