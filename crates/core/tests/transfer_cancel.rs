//! Cancellation for export planning. The runner budget is process-global, so
//! these tests take a lock. In-flight cases cancel only after the blocked
//! operation has started, so an early return on an already-cancelled token
//! cannot satisfy them.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use snip_core::commits::{
	CommitFile, CommitRecord, CommitsPayload, FileChange,
};
use snip_core::copy;
use snip_core::format::ChangeType;
use snip_core::gitrun::{
	in_flight, leaked_slots, CancelToken, Overflow, RunOptions,
};
use snip_core::gitsrc::{self, Git, GitError, GitSource};
use snip_core::settings::Settings;
use snip_core::transfer::{
	plan_commit_export_exact_with, plan_export, plan_export_with,
	plan_import_with, CanonicalRootId, CommitReplayPreview, ExportItem,
	ExportSelection, ImportMapping, SourceKind, TransferError,
};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
	SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct TestRepo {
	_dir: tempfile::TempDir,
	repo_path: PathBuf,
	cfg: PathBuf,
}

impl TestRepo {
	fn new(name: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let cfg = dir.path().join("empty.gitconfig");
		fs::write(&cfg, "").unwrap();
		let raw_path = dir.path().join(name);
		fs::create_dir_all(&raw_path).unwrap();
		let repo_path = dunce::canonicalize(&raw_path).unwrap_or(raw_path);
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

	fn path(&self) -> &Path {
		&self.repo_path
	}

	fn canonical_id(&self) -> CanonicalRootId {
		CanonicalRootId::new(&self.repo_path).unwrap()
	}

	fn git(&self, args: &[&str]) -> String {
		let mut cmd = Command::new("git");
		cmd.args(args)
			.current_dir(&self.repo_path)
			.env("GIT_CONFIG_GLOBAL", &self.cfg)
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("LC_ALL", "C");
		let out = cmd.output().unwrap();
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
			fs::create_dir_all(parent).unwrap();
		}
		fs::write(&target, content).unwrap();
	}

	fn commit(&self, msg: &str) -> String {
		self.git(&["add", "-A"]);
		self.git(&["commit", "-q", "-m", msg]);
		self.git(&["rev-parse", "HEAD"])
	}
}

fn file_item(repo: &TestRepo, rel: &str, source: SourceKind) -> ExportItem {
	ExportItem {
		root: repo.canonical_id(),
		relative_path: rel.to_string(),
		source,
		change_type: None,
	}
}

fn selection(repo: &TestRepo, items: Vec<ExportItem>) -> ExportSelection {
	ExportSelection::new(vec![repo.path().to_path_buf()], None, items).unwrap()
}

fn cancelled_opts(token: CancelToken) -> RunOptions {
	RunOptions {
		cancel: Some(token),
		timeout: Duration::from_secs(30),
		queue_timeout: Duration::from_secs(10),
		..RunOptions::default()
	}
}

fn assert_cancelled(err: TransferError) {
	assert!(
		matches!(err, TransferError::Git(GitError::Cancelled { .. })),
		"expected GitError::Cancelled, got {err:?}"
	);
}

fn assert_runner_idle() {
	assert_eq!(in_flight(), 0, "git slot still held");
	assert_eq!(leaked_slots(), 0, "git permit leaked");
}

/// Safe stand-in for `git`. Stalls when an argument equals `SNIP_STALL_PHASE`
/// and otherwise runs the real git with the stdio it inherited from the runner.
const GIT_SHIM_RS: &str = r#"
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
    if let Ok(log_path) = env::var("SNIP_GIT_LOG") {
        let mut line = String::new();
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                line.push('\t');
            }
            line.push_str(&arg.to_string_lossy());
        }
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .expect("git log");
        use std::io::Write;
        file.write_all(line.as_bytes()).expect("git log write");
    }
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

fn git_shim_dir() -> &'static Path {
	static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
	DIR.get_or_init(|| {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("shim.rs");
		fs::write(&src, GIT_SHIM_RS).unwrap();
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
			"git shim failed to compile: {}",
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
	assert!(
		out.status.success(),
		"git is required: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	let text = String::from_utf8(out.stdout).unwrap();
	PathBuf::from(text.lines().next().expect("git path").trim())
}

struct EnvGuard {
	path: Option<OsString>,
}

impl EnvGuard {
	fn arm(phase: &str, ready: &Path) -> Self {
		let guard = Self {
			path: Some(std::env::var_os("PATH").unwrap_or_default()),
		};
		std::env::set_var("SNIP_STALL_PHASE", phase);
		std::env::set_var("SNIP_STALL_READY", ready);
		std::env::set_var("SNIP_REAL_GIT", real_git());
		let mut path = git_shim_dir().as_os_str().to_os_string();
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
		for key in [
			"SNIP_STALL_PHASE",
			"SNIP_STALL_READY",
			"SNIP_REAL_GIT",
			"SNIP_GIT_LOG",
		] {
			std::env::remove_var(key);
		}
	}
}

struct ExportThread {
	cancel: CancelToken,
	pid: Option<u32>,
	handle: Option<JoinHandle<()>>,
}

impl Drop for ExportThread {
	fn drop(&mut self) {
		self.cancel.cancel();
		if let Some(pid) = self.pid.take() {
			kill_pid(pid);
		}
		if let Some(handle) = self.handle.take() {
			let _ = handle.join();
		}
	}
}

fn kill_pid(pid: u32) {
	#[cfg(unix)]
	{
		let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
	}
	#[cfg(windows)]
	{
		let _ = Command::new("taskkill")
			.args(["/F", "/PID", &pid.to_string()])
			.status();
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

/// Cancels only after `block_arg` is inside the git shim. The first result
/// must already be `Cancelled` before cleanup kills that process.
/// Caller holds [`serial`] so the PATH edit does not race other tests.
fn assert_inflight_git_cancel(block_arg: &str, selection: ExportSelection) {
	let ready_dir = tempfile::tempdir().unwrap();
	let ready = ready_dir.path().join("ready");
	let _env = EnvGuard::arm(block_arg, &ready);
	let cancel = CancelToken::new();
	let token = cancel.clone();
	let (tx, rx) = mpsc::channel();
	let handle = thread::spawn(move || {
		let opts = cancelled_opts(token);
		let settings = Settings::default();
		let res = plan_export_with(&selection, &settings, None, &opts);
		let _ = tx.send(res);
	});
	let mut owned = ExportThread {
		cancel: cancel.clone(),
		pid: None,
		handle: Some(handle),
	};

	let deadline = Instant::now() + Duration::from_secs(8);
	let pid = loop {
		if owned.handle.as_ref().is_some_and(|h| h.is_finished()) {
			panic!(
				"{block_arg} returned before the shim stalled: {:?}",
				rx.try_recv()
			);
		}
		if let Ok(text) = fs::read_to_string(&ready) {
			if let Ok(pid) = text.trim().parse::<u32>() {
				if pid > 0 {
					break pid;
				}
			}
		}
		assert!(
			Instant::now() < deadline,
			"git shim never wrote {}",
			ready.display()
		);
		thread::sleep(Duration::from_millis(10));
	};
	owned.pid = Some(pid);
	assert!(in_flight() >= 1, "{block_arg} did not hold a runner slot");
	cancel.cancel();
	let first = rx.recv_timeout(Duration::from_secs(5));
	let result = match first {
		Ok(result) => result,
		Err(timeout) => {
			panic!(
				"{block_arg} did not return within 5s ({timeout}); opts were not seen by that git call"
			);
		}
	};
	assert_cancelled(result.expect_err("cancelled export returned a plan"));
	assert_dead(pid);
	assert_runner_idle();
	owned.pid = None;
	if let Some(handle) = owned.handle.take() {
		handle.join().expect("export thread panicked");
	}
}

#[test]
fn precancel_returns_cancelled_without_a_plan_or_a_leaked_slot() {
	let _s = serial();
	let repo = TestRepo::new("precancel");
	repo.write("a.txt", "hello\n");
	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	let cancel = CancelToken::new();
	cancel.cancel();
	let err = plan_export_with(
		&selection,
		&Settings::default(),
		None,
		&cancelled_opts(cancel),
	)
	.expect_err("precancel must not build a plan");
	assert_cancelled(err);
	assert_runner_idle();
}

#[test]
fn precancel_revalidate_is_cancelled_not_fresh() {
	let _s = serial();
	let repo = TestRepo::new("revalidate-precancel");
	repo.write("a.txt", "hello\n");
	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	let plan = plan_export(&selection, &Settings::default(), None).unwrap();
	assert!(plan.revalidate().is_ok());
	let cancel = CancelToken::new();
	cancel.cancel();
	let err = plan
		.revalidate_with(&cancelled_opts(cancel))
		.expect_err("cancelled revalidate must not look fresh");
	assert_cancelled(err);
	assert_runner_idle();
}

#[test]
fn default_options_match_the_old_export_and_the_legacy_bytes() {
	let _s = serial();
	let repo = TestRepo::new("parity");
	repo.write("src/a.rs", "fn a() {}\n");
	repo.commit("base");
	repo.write("src/a.rs", "fn a() { changed }\n");
	let sha = repo.commit("edit");

	let settings = Settings::default();
	let file_selection =
		selection(&repo, vec![file_item(&repo, "src/a.rs", SourceKind::File)]);
	let copy_res = copy::collect_copy_files(
		&[repo.path()],
		&[repo.path().join("src/a.rs")],
		&settings,
	);
	let old_file = plan_export(&file_selection, &settings, None).unwrap();
	let new_file = plan_export_with(
		&file_selection,
		&settings,
		None,
		&RunOptions::default(),
	)
	.unwrap();
	assert_eq!(new_file.payload, old_file.payload);
	assert_eq!(new_file.payload, copy_res.payload);
	assert_eq!(new_file.copied_file_count, copy_res.copied_file_count);
	assert_eq!(new_file.file_limit_reached, copy_res.file_limit_reached);

	let commit_selection = selection(
		&repo,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "src/a.rs".to_string(),
			source: SourceKind::Commit { rev: sha.clone() },
			change_type: Some(ChangeType::Modified),
		}],
	);
	let git = Git::open(repo.path()).unwrap();
	let legacy = gitsrc::collect_payload(
		&git,
		&GitSource::Commit(sha),
		&[repo.path()],
		&settings,
	)
	.unwrap();
	let old_commit = plan_export(&commit_selection, &settings, None).unwrap();
	let new_commit = plan_export_with(
		&commit_selection,
		&settings,
		None,
		&RunOptions::default(),
	)
	.unwrap();
	assert_eq!(new_commit.payload, old_commit.payload);
	assert_eq!(new_commit.payload, legacy.payload);
	assert_eq!(new_commit.copied_file_count, legacy.copied_file_count);
	assert_runner_idle();
}

#[test]
fn cumulative_payload_limit_is_unchanged() {
	let _s = serial();
	let repo = TestRepo::new("budget");
	repo.write("a.txt", "hello budget\n");
	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	let settings = Settings::default();
	let full =
		plan_export_with(&selection, &settings, None, &RunOptions::default())
			.unwrap();
	let limit = full.payload.len() - 1;
	let old_err = plan_export(&selection, &settings, Some(limit)).unwrap_err();
	let new_err = plan_export_with(
		&selection,
		&settings,
		Some(limit),
		&RunOptions::default(),
	)
	.unwrap_err();
	match (old_err, new_err) {
		(
			TransferError::PayloadLimitExceeded {
				limit: left_limit,
				actual: left_actual,
				reason: left_reason,
			},
			TransferError::PayloadLimitExceeded {
				limit: right_limit,
				actual: right_actual,
				reason: right_reason,
			},
		) => {
			assert_eq!(left_limit, limit);
			assert_eq!(right_limit, limit);
			assert_eq!(left_actual, right_actual);
			assert!(left_actual > limit, "actual {left_actual} limit {limit}");
			assert_eq!(left_reason, right_reason);
		}
		other => panic!("expected matching payload limits, got {other:?}"),
	}
	assert_runner_idle();
}

#[test]
fn deleted_absence_still_invalidates_before_clipboard_handoff() {
	let _s = serial();
	let repo = TestRepo::new("absence");
	repo.write("gone.txt", "old body\n");
	repo.commit("c1");
	fs::remove_file(repo.path().join("gone.txt")).unwrap();
	let selection = selection(
		&repo,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "gone.txt".to_string(),
			source: SourceKind::Working,
			change_type: Some(ChangeType::Deleted),
		}],
	);
	let plan = plan_export_with(
		&selection,
		&Settings::default(),
		None,
		&RunOptions::default(),
	)
	.unwrap();
	assert!(plan.payload.contains("old body"));
	assert!(plan
		.freshness
		.working_files
		.get(&(repo.canonical_id(), "gone.txt".to_string()))
		.is_some_and(Option::is_none));
	plan.revalidate_with(&RunOptions::default()).unwrap();
	repo.write("gone.txt", "recreated\n");
	let err = plan.revalidate_with(&RunOptions::default()).unwrap_err();
	match err {
		TransferError::StaleSource { reason, .. } => {
			assert!(reason.contains("gone.txt"), "{reason}");
		}
		other => panic!("expected StaleSource, got {other:?}"),
	}
	assert_runner_idle();
}

#[test]
fn preview_truncation_is_not_a_successful_plan() {
	let _s = serial();
	let repo = TestRepo::new("truncate");
	repo.write("a.txt", "hello\n");
	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	let opts = RunOptions {
		max_stdout: 1,
		overflow: Overflow::Truncate,
		timeout: Duration::from_secs(30),
		..RunOptions::default()
	};
	let err = plan_export_with(&selection, &Settings::default(), None, &opts)
		.expect_err("truncated git metadata must not become a plan");
	assert!(
		matches!(
			err,
			TransferError::Git(GitError::OutputLimit { limit: 1, .. })
		),
		"preview truncation must surface OutputLimit, got {err:?}"
	);
	assert_runner_idle();
}

#[test]
fn inflight_cancel_during_head_ref_is_not_a_plan() {
	let _s = serial();
	let repo = TestRepo::new("head-ref-cancel");
	repo.write("a.txt", "hello\n");
	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	assert_inflight_git_cancel("symbolic-ref", selection);
}

#[test]
fn inflight_cancel_during_cat_file_is_not_a_plan() {
	let _s = serial();
	let repo = TestRepo::new("cat-file-cancel");
	repo.write("blob.txt", "blob body\n");
	let sha = repo.commit("add blob");
	let selection = selection(
		&repo,
		vec![ExportItem {
			root: repo.canonical_id(),
			relative_path: "blob.txt".to_string(),
			source: SourceKind::Commit { rev: sha },
			change_type: Some(ChangeType::Modified),
		}],
	);
	assert_inflight_git_cancel("--batch", selection);
}

#[cfg(unix)]
#[test]
fn fifo_git_index_is_rejected_before_open() {
	let _s = serial();
	let repo = TestRepo::new("index-fifo");
	repo.write("a.txt", "working content\n");
	let index = repo.path().join(".git/index");
	let _ = fs::remove_file(&index);
	let made = Command::new("mkfifo").arg(&index).status().unwrap();
	assert!(made.success(), "mkfifo failed");

	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	let (tx, rx) = mpsc::channel();
	let handle = thread::spawn(move || {
		let res = plan_export_with(
			&selection,
			&Settings::default(),
			Some(65_536),
			&RunOptions::default(),
		);
		let _ = tx.send(res);
	});
	let started = Instant::now();
	let result = rx.recv_timeout(Duration::from_secs(1));
	match result {
		Ok(Err(TransferError::SpecialFile(path))) => {
			assert!(
				path.contains("index"),
				"special-file path should name the index, got {path}"
			);
		}
		Ok(other) => panic!("expected SpecialFile, got {other:?}"),
		Err(_) => panic!(
			"plan_export_with still blocked on the index FIFO after {}ms",
			started.elapsed().as_millis()
		),
	}
	assert!(started.elapsed() < Duration::from_secs(1));
	handle.join().expect("export thread panicked");
	assert_runner_idle();
}

#[cfg(unix)]
#[test]
fn symlink_to_regular_index_still_exports() {
	let _s = serial();
	let repo = TestRepo::new("index-symlink");
	repo.write("a.txt", "linked index body\n");
	repo.commit("add");
	let index = repo.path().join(".git/index");
	let real = repo.path().join(".git/index.real");
	fs::rename(&index, &real).unwrap();
	std::os::unix::fs::symlink("index.real", &index).unwrap();
	let selection =
		selection(&repo, vec![file_item(&repo, "a.txt", SourceKind::File)]);
	let plan = plan_export_with(
		&selection,
		&Settings::default(),
		None,
		&RunOptions::default(),
	)
	.unwrap();
	assert!(
		plan.payload.contains("linked index body\n"),
		"{}",
		plan.payload
	);
	plan.revalidate_with(&RunOptions::default()).unwrap();
	assert_runner_idle();
}

/// Cancels while the exact selector is walking parents of a non-HEAD
/// deletion tip. `rev-parse` must already have been logged, and the stalled
/// `rev-list` must be that tip. Default runner options would ignore the
/// token and the receive would time out.
#[test]
fn inflight_cancel_during_exact_selector_sees_real_work() {
	let _s = serial();
	let repo = TestRepo::new("exact-selector-cancel");
	repo.write("a.txt", "hello\n");
	let root = repo.commit("root");
	fs::remove_file(repo.path().join("a.txt")).unwrap();
	let tip = repo.commit("delete a");
	repo.write("b.txt", "head stays\n");
	let head = repo.commit("move head");
	assert_ne!(head, tip);
	assert_ne!(repo.git(&["rev-parse", "HEAD"]), tip);

	let git = Git::open(repo.path()).unwrap();
	let ready_dir = tempfile::tempdir().unwrap();
	let ready = ready_dir.path().join("ready");
	let log_path = ready_dir.path().join("git.log");
	std::env::set_var("SNIP_GIT_LOG", &log_path);
	let _env = EnvGuard::arm("rev-list", &ready);
	let cancel = CancelToken::new();
	let token = cancel.clone();
	let (tx, rx) = mpsc::channel();
	let tip_for_thread = tip.clone();
	let root_for_thread = root.clone();
	let handle = thread::spawn(move || {
		let opts = cancelled_opts(token);
		let res = plan_commit_export_exact_with(
			&git,
			&tip_for_thread,
			&[root_for_thread, tip_for_thread.clone()],
			&opts,
			usize::MAX,
		);
		let _ = tx.send(res);
	});
	let mut owned = ExportThread {
		cancel: cancel.clone(),
		pid: None,
		handle: Some(handle),
	};

	let deadline = Instant::now() + Duration::from_secs(8);
	let pid = loop {
		if owned.handle.as_ref().is_some_and(|h| h.is_finished()) {
			panic!(
				"selector returned before rev-list stalled: {:?}",
				rx.try_recv()
			);
		}
		if let Ok(text) = fs::read_to_string(&ready) {
			if let Ok(pid) = text.trim().parse::<u32>() {
				if pid > 0 {
					break pid;
				}
			}
		}
		assert!(
			Instant::now() < deadline,
			"selector never reached rev-list ({})",
			ready.display()
		);
		thread::sleep(Duration::from_millis(10));
	};
	owned.pid = Some(pid);
	let log = fs::read_to_string(&log_path).unwrap_or_default();
	let lines: Vec<&str> = log.lines().collect();
	let rev_parse_at = lines
		.iter()
		.position(|line| line.split('\t').any(|arg| arg == "rev-parse"));
	let rev_list_at = lines.iter().position(|line| {
		let args: Vec<&str> = line.split('\t').collect();
		args.contains(&"rev-list") && args.contains(&tip.as_str())
	});
	assert!(
		rev_parse_at.is_some(),
		"selector did no rev-parse before cancel; log:\n{log}"
	);
	let rev_list_at = rev_list_at.unwrap_or_else(|| {
		panic!("stalled rev-list was not the selected tip; log:\n{log}")
	});
	assert!(
		rev_parse_at.unwrap() < rev_list_at,
		"rev-list ran before any resolve; log:\n{log}"
	);
	assert!(
		lines.iter().all(|line| {
			!line
				.split('\t')
				.any(|arg| arg == "diff-tree" || arg == "cat-file")
		}),
		"export started before the selector stall; log:\n{log}"
	);
	assert!(in_flight() >= 1, "rev-list did not hold a runner slot");
	cancel.cancel();
	let first = rx.recv_timeout(Duration::from_secs(5));
	let result = match first {
		Ok(result) => result,
		Err(timeout) => {
			panic!(
				"selector rev-list did not return within 5s ({timeout}); opts were not connected to that git call"
			);
		}
	};
	assert_cancelled(
		result.expect_err("cancelled selector returned a payload"),
	);
	assert_dead(pid);
	assert_runner_idle();
	owned.pid = None;
	if let Some(handle) = owned.handle.take() {
		handle.join().expect("selector thread panicked");
	}
}

/// Stalls `phase`, cancels only after that git is inside the shim, and
/// returns the error plus the argv log. The permit must already be free
/// and the descendant dead when this returns.
fn stall_then_cancel(
	phase: &str,
	run: impl FnOnce(RunOptions) -> Result<(), TransferError> + Send + 'static,
) -> (TransferError, String) {
	let ready_dir = tempfile::tempdir().unwrap();
	let ready = ready_dir.path().join("ready");
	let log_path = ready_dir.path().join("git.log");
	std::env::set_var("SNIP_GIT_LOG", &log_path);
	let _env = EnvGuard::arm(phase, &ready);
	let cancel = CancelToken::new();
	let token = cancel.clone();
	let (tx, rx) = mpsc::channel();
	let handle = thread::spawn(move || {
		let opts = cancelled_opts(token);
		let _ = tx.send(run(opts));
	});
	let mut owned = ExportThread {
		cancel: cancel.clone(),
		pid: None,
		handle: Some(handle),
	};
	let deadline = Instant::now() + Duration::from_secs(8);
	let pid = loop {
		if owned.handle.as_ref().is_some_and(|h| h.is_finished()) {
			panic!(
				"{phase} returned before the shim stalled: {:?}",
				rx.try_recv()
			);
		}
		if let Ok(text) = fs::read_to_string(&ready) {
			if let Ok(pid) = text.trim().parse::<u32>() {
				if pid > 0 {
					break pid;
				}
			}
		}
		assert!(Instant::now() < deadline, "git shim never reached {phase}");
		thread::sleep(Duration::from_millis(10));
	};
	owned.pid = Some(pid);
	let log = fs::read_to_string(&log_path).unwrap_or_default();
	assert!(
		in_flight() >= 1,
		"{phase} did not hold a runner slot; log:\n{log}"
	);
	cancel.cancel();
	let result = match rx.recv_timeout(Duration::from_secs(5)) {
		Ok(result) => result,
		Err(timeout) => panic!(
			"{phase} did not return within 5s ({timeout}); opts were not connected to that git call\n{log}"
		),
	};
	let err = result.expect_err("cancelled preview returned a plan");
	assert_cancelled(err_ref(&err));
	assert_dead(pid);
	assert_runner_idle();
	owned.pid = None;
	if let Some(handle) = owned.handle.take() {
		handle.join().expect("preview thread panicked");
	}
	(err, log)
}

fn err_ref(err: &TransferError) -> TransferError {
	match err {
		TransferError::Git(GitError::Cancelled { args }) => {
			TransferError::Git(GitError::Cancelled { args: args.clone() })
		}
		other => panic!("expected Cancelled, got {other:?}"),
	}
}

fn assert_index_git_after_head(log: &str) {
	let lines: Vec<&str> = log.lines().collect();
	let head = lines.iter().position(|line| {
		let args: Vec<&str> = line.split('\t').collect();
		args.contains(&"symbolic-ref")
	});
	let index = lines.iter().position(|line| {
		let args: Vec<&str> = line.split('\t').collect();
		args.contains(&"--git-path")
	});
	let head = head.unwrap_or_else(|| panic!("no symbolic-ref; log:\n{log}"));
	let index = index.unwrap_or_else(|| panic!("no index git; log:\n{log}"));
	assert!(head < index, "index git ran before HEAD ref; log:\n{log}");
	assert!(
		lines[..head]
			.iter()
			.any(|line| line.split('\t').any(|arg| arg == "rev-parse")),
		"no rev-parse before HEAD ref; log:\n{log}"
	);
}

fn repo_fingerprint(repo: &TestRepo) -> (String, String, Vec<u8>, Vec<u8>) {
	let head = repo.git(&["rev-parse", "HEAD"]);
	let branch = repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
	let index = fs::read(repo.path().join(".git/index")).unwrap();
	let keep = fs::read(repo.path().join("keep.txt")).unwrap();
	(head, branch, index, keep)
}

#[test]
fn inflight_cancel_during_replay_preview_after_head_and_index_git() {
	let _s = serial();
	let repo = TestRepo::new("replay-preview-cancel");
	repo.write("keep.txt", "keep\n");
	repo.write("big.txt", &"x".repeat(32 * 1024));
	repo.commit("base");
	let before = repo_fingerprint(&repo);
	let big_before = fs::read(repo.path().join("big.txt")).unwrap();
	assert!(!repo.path().join("added.txt").exists());
	let path = repo.path().to_path_buf();
	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files: vec![
				CommitFile {
					path: "big.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some("incoming\n".into()),
					not_copied: None,
				},
				CommitFile {
					path: "added.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("new\n".into()),
					not_copied: None,
				},
			],
		}],
	};
	let (_err, log) = stall_then_cancel("--git-path", move |opts| {
		CommitReplayPreview::capture_with(&path, &payload, &opts).map(|_| ())
	});
	assert_index_git_after_head(&log);
	assert_eq!(repo_fingerprint(&repo), before);
	assert_eq!(fs::read(repo.path().join("big.txt")).unwrap(), big_before);
	assert!(!repo.path().join("added.txt").exists());
	assert_runner_idle();
}

#[test]
fn inflight_cancel_during_plan_import_after_head_and_index_git() {
	let _s = serial();
	let repo = TestRepo::new("import-preview-cancel");
	repo.write("keep.txt", "keep\n");
	repo.commit("base");
	let before = repo_fingerprint(&repo);
	assert!(!repo.path().join("incoming.txt").exists());
	let path = repo.path().to_path_buf();
	let root_id = repo.canonical_id();
	let text = "\
// file: incoming.txt
hello preview
// file: more.txt
second
";
	let (_err, log) = stall_then_cancel("--git-path", move |opts| {
		let mapping = ImportMapping::with_primary(root_id);
		plan_import_with(text, "// file: $FILE_PATH", &[path], &mapping, &opts)
			.map(|_| ())
	});
	assert_index_git_after_head(&log);
	assert_eq!(repo_fingerprint(&repo), before);
	assert!(!repo.path().join("incoming.txt").exists());
	assert!(!repo.path().join("more.txt").exists());
	assert_runner_idle();
}

#[test]
fn precancel_replay_preview_and_plan_import_write_nothing() {
	let _s = serial();
	let repo = TestRepo::new("preview-precancel");
	repo.write("keep.txt", "keep\n");
	repo.commit("base");
	let before = repo_fingerprint(&repo);
	let token = CancelToken::new();
	token.cancel();
	let opts = cancelled_opts(token);
	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "added.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("new\n".into()),
				not_copied: None,
			}],
		}],
	};
	let replay_err =
		CommitReplayPreview::capture_with(repo.path(), &payload, &opts)
			.expect_err("precancel must not capture a replay preview");
	assert_cancelled(replay_err);
	let import_err = plan_import_with(
		"// file: incoming.txt\nhello\n",
		"// file: $FILE_PATH",
		&[repo.path().to_path_buf()],
		&ImportMapping::with_primary(repo.canonical_id()),
		&opts,
	)
	.expect_err("precancel must not build an import plan");
	assert_cancelled(import_err);
	assert_eq!(repo_fingerprint(&repo), before);
	assert!(!repo.path().join("added.txt").exists());
	assert!(!repo.path().join("incoming.txt").exists());
	assert_runner_idle();
}

#[test]
fn inflight_cancel_during_replay_revalidate_after_head_and_index_git() {
	let _s = serial();
	let repo = TestRepo::new("replay-revalidate-cancel");
	repo.write("keep.txt", "keep\n");
	repo.write("big.txt", &"x".repeat(32 * 1024));
	repo.commit("base");
	let before = repo_fingerprint(&repo);
	let big_before = fs::read(repo.path().join("big.txt")).unwrap();
	assert!(!repo.path().join("added.txt").exists());
	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files: vec![
				CommitFile {
					path: "big.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some("incoming\n".into()),
					not_copied: None,
				},
				CommitFile {
					path: "added.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("new\n".into()),
					not_copied: None,
				},
			],
		}],
	};
	let preview = CommitReplayPreview::capture(repo.path(), &payload)
		.expect("setup capture must succeed");
	let (_err, log) = stall_then_cancel("--git-path", move |opts| {
		preview.revalidate_with(&opts)
	});
	assert_index_git_after_head(&log);
	assert_eq!(repo_fingerprint(&repo), before);
	assert_eq!(fs::read(repo.path().join("big.txt")).unwrap(), big_before);
	assert!(!repo.path().join("added.txt").exists());
	assert_runner_idle();
}

#[test]
fn precancel_replay_revalidate_writes_nothing() {
	let _s = serial();
	let repo = TestRepo::new("replay-revalidate-precancel");
	repo.write("keep.txt", "keep\n");
	repo.commit("base");
	let before = repo_fingerprint(&repo);
	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "keep.txt".into(),
				old_path: None,
				change: FileChange::Modified,
				content: Some("new\n".into()),
				not_copied: None,
			}],
		}],
	};
	let preview = CommitReplayPreview::capture(repo.path(), &payload)
		.expect("setup capture must succeed");
	let token = CancelToken::new();
	token.cancel();
	let opts = cancelled_opts(token);
	let err = preview
		.revalidate_with(&opts)
		.expect_err("precancel must not revalidate");
	assert_cancelled(err);
	assert_eq!(repo_fingerprint(&repo), before);
	assert_runner_idle();
}

#[test]
fn precancel_replay_apply_writes_nothing() {
	let _s = serial();
	let repo = TestRepo::new("replay-apply-precancel");
	repo.write("keep.txt", "keep\n");
	repo.commit("base");
	let before = repo_fingerprint(&repo);
	let payload = CommitsPayload {
		commits: vec![CommitRecord {
			message: "incoming\n".into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-26T00:00:00+00:00".into(),
			files: vec![CommitFile {
				path: "keep.txt".into(),
				old_path: None,
				change: FileChange::Modified,
				content: Some("new\n".into()),
				not_copied: None,
			}],
		}],
	};
	let preview = CommitReplayPreview::capture(repo.path(), &payload)
		.expect("setup capture must succeed");
	let token = CancelToken::new();
	token.cancel();
	let opts = cancelled_opts(token);
	let err = preview
		.apply_with(&opts)
		.expect_err("precancel must not apply");
	assert_cancelled(err);
	assert_eq!(repo_fingerprint(&repo), before);
	assert_runner_idle();
}
