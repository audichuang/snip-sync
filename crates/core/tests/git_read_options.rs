//! Real nested reads through the public preview API. This binary deliberately
//! has one test: its temporary PATH shim cannot race unrelated tests, and the
//! runner's process budget is checked only while this process is otherwise idle.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use snip_core::browser;
use snip_core::format::ChangeType;
use snip_core::gitrun::{
	in_flight, leaked_slots, CancelToken, Overflow, RunOptions,
};
use snip_core::gitsrc::{self, Git, GitError, GitSource};

fn git(dir: &Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.current_dir(dir)
		.args(args)
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn init(dir: &Path) {
	fs::create_dir_all(dir).unwrap();
	git(dir, &["init", "-q", "-b", "main"]);
	git(dir, &["config", "user.name", "Read Test"]);
	git(dir, &["config", "user.email", "read@example.com"]);
	git(dir, &["config", "commit.gpgsign", "false"]);
	git(dir, &["config", "core.autocrlf", "false"]);
}

fn assert_idle() {
	assert_eq!(in_flight(), 0, "nested Git process still owns a permit");
	assert_eq!(leaked_slots(), 0, "Git cleanup leaked a permit");
}

// Same compiled Git-shim approach as git_open/transfer_cancel. A release file
// lets failure cleanup unblock the OLD uncancellable implementation without
// killing unrelated processes or waiting for its 300-second default timeout.
const SHIM: &str = r#"
use std::{env, fs, process::{Command, Stdio}, thread, time::Duration};
fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let phase = env::var("SNIP_READ_PHASE").unwrap_or_default();
    if !phase.is_empty() && args.iter().any(|arg| arg == phase.as_str()) {
        use std::io::{Read, Write};
        fs::write(env::var("SNIP_READ_READY").unwrap(), std::process::id().to_string()).unwrap();
        match env::var("SNIP_READ_PROTOCOL").as_deref() {
            Ok("body_fail") => {
                let mut request = [0u8; 1];
                while std::io::stdin().read(&mut request).unwrap() == 1 && request[0] != b'\n' {}
                print!("{} blob 2\nok\n", "0".repeat(40));
                std::io::stdout().flush().unwrap();
                eprintln!("failure after a complete body");
                std::process::exit(42);
            }
            Ok("partial_success") => {
                print!("{} blob 4\nx", "0".repeat(40));
                std::io::stdout().flush().unwrap();
                std::process::exit(0);
            }
            #[cfg(unix)]
            Ok("eof_live") => {
                use std::os::fd::FromRawFd;
                drop(unsafe { fs::File::from_raw_fd(1) });
            }
            _ => {}
        }
        if env::var("SNIP_READ_FAIL").as_deref() == Ok("1") {
            eprintln!("injected nested Git failure");
            std::io::stderr().write_all(&vec![b'x'; 100_000]).unwrap();
            std::process::exit(42);
        }
        let release = env::var("SNIP_READ_RELEASE").unwrap();
        while !std::path::Path::new(&release).exists() {
            thread::sleep(Duration::from_millis(10));
        }
    }
    let mut cmd = Command::new(env::var_os("SNIP_READ_REAL_GIT").unwrap());
    cmd.args(args).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    #[cfg(unix)] {
        use std::os::unix::process::CommandExt;
        panic!("exec real git: {}", cmd.exec());
    }
    #[cfg(not(unix))] { std::process::exit(cmd.status().unwrap().code().unwrap_or(1)); }
}
"#;

struct Shim {
	dir: tempfile::TempDir,
	old_path: std::ffi::OsString,
}

impl Shim {
	fn new() -> Self {
		let old_path = std::env::var_os("PATH").unwrap();
		let real = if cfg!(windows) {
			Command::new("where").arg("git").output().unwrap()
		} else {
			Command::new("sh")
				.args(["-c", "command -v git"])
				.output()
				.unwrap()
		};
		assert!(real.status.success());
		let real = String::from_utf8(real.stdout).unwrap();
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("git.rs");
		fs::write(&src, SHIM).unwrap();
		let exe =
			dir.path()
				.join(if cfg!(windows) { "git.exe" } else { "git" });
		let out = Command::new("rustc")
			.args(["--edition", "2021"])
			.arg(src)
			.arg("-o")
			.arg(exe)
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{}",
			String::from_utf8_lossy(&out.stderr)
		);
		std::env::set_var(
			"SNIP_READ_REAL_GIT",
			real.lines().next().unwrap().trim(),
		);
		let paths = std::iter::once(dir.path().to_path_buf())
			.chain(std::env::split_paths(&old_path));
		std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
		Self { dir, old_path }
	}

	fn arm(&self, phase: &str, fail: bool) -> (PathBuf, PathBuf) {
		let ready = self.dir.path().join("ready");
		let release = self.dir.path().join("release");
		let _ = fs::remove_file(&ready);
		let _ = fs::remove_file(&release);
		std::env::set_var("SNIP_READ_PHASE", phase);
		std::env::set_var("SNIP_READ_READY", &ready);
		std::env::set_var("SNIP_READ_RELEASE", &release);
		std::env::set_var("SNIP_READ_FAIL", if fail { "1" } else { "0" });
		std::env::remove_var("SNIP_READ_PROTOCOL");
		(ready, release)
	}
}

impl Drop for Shim {
	fn drop(&mut self) {
		std::env::set_var("PATH", &self.old_path);
		for key in [
			"SNIP_READ_REAL_GIT",
			"SNIP_READ_PHASE",
			"SNIP_READ_READY",
			"SNIP_READ_RELEASE",
			"SNIP_READ_FAIL",
			"SNIP_READ_PROTOCOL",
		] {
			std::env::remove_var(key);
		}
	}
}

struct ReadThread {
	cancel: CancelToken,
	release: PathBuf,
	thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ReadThread {
	fn drop(&mut self) {
		self.cancel.cancel();
		let _ = fs::write(&self.release, "release old implementation");
		if let Some(thread) = self.thread.take() {
			thread.join().unwrap();
		}
	}
}

fn nested_read(
	shim: &Shim,
	repo: &Path,
	source: GitSource,
	phase: &str,
	timeout: bool,
) {
	let git = Git::open(repo).unwrap();
	let (ready, release) = shim.arm(phase, false);
	let cancel = CancelToken::new();
	let opts = RunOptions {
		cancel: Some(cancel.clone()),
		timeout: Duration::from_secs(if timeout { 1 } else { 20 }),
		..RunOptions::preview(None)
	};
	let (tx, rx) = mpsc::channel();
	let thread = thread::spawn(move || {
		let result = browser::git_preview_with(&git, &source, "a.txt", &opts);
		let _ = tx.send(result);
	});
	let owned = ReadThread {
		cancel: cancel.clone(),
		release,
		thread: Some(thread),
	};
	let deadline = Instant::now() + Duration::from_secs(5);
	let pid: u32 = loop {
		if let Ok(text) = fs::read_to_string(&ready) {
			if let Ok(pid) = text.parse() {
				break pid;
			}
		}
		assert!(
			Instant::now() < deadline,
			"{phase} never entered nested Git: {:?}",
			rx.try_recv()
		);
		thread::sleep(Duration::from_millis(10));
	};
	assert_eq!(in_flight(), 1, "must cancel a live nested process");
	if !timeout {
		cancel.cancel();
	}
	let result = rx.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| {
		panic!("{phase} ignored nested cancellation/deadline")
	});
	let err = result.unwrap_err();
	if timeout {
		assert!(matches!(err, GitError::Timeout { .. }), "{phase}: {err}");
	} else {
		assert!(matches!(err, GitError::Cancelled { .. }), "{phase}: {err}");
	}
	// The result must arrive before the fallback release is written.
	assert!(!owned.release.exists());
	assert_idle();
	#[cfg(unix)]
	{
		let out = Command::new("ps")
			.args(["-o", "stat=", "-p", &pid.to_string()])
			.output()
			.unwrap();
		assert!(
			String::from_utf8_lossy(&out.stdout).trim().is_empty(),
			"nested child {pid} not reaped"
		);
	}
	#[cfg(windows)]
	{
		let out = Command::new("tasklist")
			.args(["/FI", &format!("PID eq {pid}"), "/NH"])
			.output()
			.unwrap();
		assert!(!String::from_utf8_lossy(&out.stdout)
			.split_whitespace()
			.any(|word| word == pid.to_string()));
	}
	drop(owned);
	std::env::remove_var("SNIP_READ_PHASE");
}

#[test]
fn nested_preview_options_and_source_fidelity() {
	assert_idle();
	let dir = tempfile::tempdir().unwrap();
	let repo = dir.path().join("repo");
	init(&repo);
	fs::write(repo.join("a.txt"), "BASE\n").unwrap();
	git(&repo, &["add", "."]);
	git(&repo, &["commit", "-qm", "root"]);
	let root = git(&repo, &["rev-parse", "HEAD"]);
	fs::write(repo.join("a.txt"), "STAGED_A\n".repeat(200)).unwrap();
	git(&repo, &["add", "."]);
	fs::write(repo.join("a.txt"), "WORKING_B\n").unwrap();
	let git_repo = Git::open(&repo).unwrap();
	let shim = Shim::new();
	for (source, phase) in [
		(GitSource::Staged, "--raw"),
		(GitSource::Staged, "--batch"),
		(GitSource::Working, "-u"),
		(GitSource::Working, "--others"),
		(GitSource::Working, "--verify"),
		(GitSource::Commit(root.clone()), "rev-list"),
		(GitSource::Commit(root.clone()), "--is-shallow-repository"),
		(GitSource::Commit(root.clone()), "diff-tree"),
	] {
		nested_read(&shim, &repo, source, phase, false);
	}
	nested_read(&shim, &repo, GitSource::Staged, "--batch", true);

	// Both overflow modes must reject partial metadata and partial content.
	for overflow in [Overflow::Error, Overflow::Truncate] {
		for max in [1, 512] {
			let opts = RunOptions {
				max_stdout: max,
				overflow,
				..RunOptions::default()
			};
			let err = browser::git_preview_with(
				&git_repo,
				&GitSource::Staged,
				"a.txt",
				&opts,
			)
			.unwrap_err();
			assert!(
				matches!(err, GitError::OutputLimit { limit, .. } if limit == max),
				"{overflow:?}/{max}: {err}"
			);
			assert_idle();
		}
	}
	let staged = browser::git_preview_with(
		&git_repo,
		&GitSource::Staged,
		"a.txt",
		&RunOptions::default(),
	)
	.unwrap();
	assert_eq!(
		staged.content.as_deref(),
		Some("STAGED_A\n".repeat(200).as_str())
	);
	assert!(!staged.patch.contains("WORKING_B"));
	let legacy =
		browser::git_preview(&git_repo, &GitSource::Staged, "a.txt").unwrap();
	assert_eq!(legacy.content, staged.content);
	assert_eq!(legacy.patch, staged.patch);
	assert_eq!(
		gitsrc::read_changed_file(
			&git_repo,
			&GitSource::Working,
			"a.txt",
			1024
		)
		.unwrap()
		.unwrap()
		.content
		.as_deref(),
		Some("WORKING_B\n")
	);

	// A real nested process failure stays Failed; staged content never falls
	// back to working bytes. The same holds for pre-deletion revision content.
	shim.arm("--raw", true);
	let err = browser::git_preview_with(
		&git_repo,
		&GitSource::Staged,
		"a.txt",
		&RunOptions::default(),
	)
	.unwrap_err();
	assert!(
		matches!(err, GitError::Failed { code: Some(42), .. }),
		"{err}"
	);
	shim.arm("--batch", true);
	let err = browser::git_preview_with(
		&git_repo,
		&GitSource::Staged,
		"a.txt",
		&RunOptions::default(),
	)
	.unwrap_err();
	match err {
		GitError::Failed {
			code: Some(42),
			stderr,
			..
		} => {
			assert!(stderr.starts_with("injected nested Git failure"));
			assert_eq!(stderr.len(), 64 * 1024, "stderr must remain bounded");
		}
		other => {
			panic!("abnormal batch EOF must preserve process status: {other}")
		}
	}
	assert_idle();
	shim.arm("--batch", false);
	std::env::set_var("SNIP_READ_PROTOCOL", "body_fail");
	let mut cat = git_repo.cat_file_with(RunOptions::default()).unwrap();
	assert!(matches!(cat.read_object("HEAD:a.txt", 100).unwrap(),
		gitsrc::CatObject::Found { body, .. } if body == b"ok"));
	assert!(matches!(cat.close().unwrap_err(),
		GitError::Failed { code: Some(42), stderr, .. } if stderr == "failure after a complete body"));
	assert_idle();
	shim.arm("--batch", false);
	std::env::set_var("SNIP_READ_PROTOCOL", "partial_success");
	let mut cat = git_repo.cat_file_with(RunOptions::default()).unwrap();
	let err = cat.read_object("HEAD:a.txt", 100).unwrap_err();
	assert!(
		matches!(err, GitError::Io(_) | GitError::Malformed(_)),
		"zero exit protocol truncation: {err}"
	);
	drop(cat);
	assert_idle();
	#[cfg(unix)]
	{
		shim.arm("--batch", false);
		std::env::set_var("SNIP_READ_PROTOCOL", "eof_live");
		let opts = RunOptions {
			timeout: Duration::from_millis(200),
			..RunOptions::default()
		};
		let mut cat = git_repo.cat_file_with(opts).unwrap();
		let err = cat.read_object("HEAD:a.txt", 100).unwrap_err();
		assert!(
			matches!(err, GitError::Timeout { .. }),
			"live child EOF must time out, not invent Failed: {err}"
		);
		drop(cat);
		assert_idle();
	}
	std::env::remove_var("SNIP_READ_PHASE");
	git(&repo, &["reset", "--hard", "HEAD"]);
	git(&repo, &["rm", "a.txt"]);
	git(&repo, &["commit", "-qm", "delete"]);
	let deleted = git(&repo, &["rev-parse", "HEAD"]);
	fs::write(repo.join("a.txt"), "WRONG_WORKING\n").unwrap();
	nested_read(
		&shim,
		&repo,
		GitSource::Commit(deleted.clone()),
		"--batch",
		false,
	);
	let source = GitSource::Commit(deleted);
	let preview = browser::git_preview_with(
		&git_repo,
		&source,
		"a.txt",
		&RunOptions::default(),
	)
	.unwrap();
	assert_eq!(preview.content.as_deref(), Some("BASE\n"));
	assert!(!preview.patch.contains("WRONG_WORKING"));
	let paths = gitsrc::list_changed_paths(&git_repo, &source).unwrap();
	assert_eq!(paths, [("a.txt".into(), Some(ChangeType::Deleted))]);
	assert_eq!(
		gitsrc::collect(&git_repo, &source).unwrap().files[0].content,
		preview.content
	);
	assert_idle();
}
