//! Bounded commit export against real Git and a runner shim.
//!
//! The budget is process-global and the cancellation fixture replaces
//! `PATH`, so every test in this binary holds `SERIAL`.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use snip_core::commits::{
	copy_commits, copy_commits_with, to_clipboard_text, CommitError,
	CommitExport, CommitsPayload, FileChange, NotCopiedReason,
};
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
	cfg: PathBuf,
}

impl Repo {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let cfg = dir.path().join("empty.gitconfig");
		fs::write(&cfg, "").unwrap();
		fs::create_dir(dir.path().join("r")).unwrap();
		let repo = Self { dir, cfg };
		repo.git(["init", "-q", "-b", "main"]);
		repo.git(["config", "user.name", "Local"]);
		repo.git(["config", "user.email", "local@example.com"]);
		repo.git(["config", "core.autocrlf", "false"]);
		repo.git(["config", "commit.gpgsign", "false"]);
		repo
	}

	fn path(&self) -> PathBuf {
		self.dir.path().join("r")
	}

	fn git<I, S>(&self, args: I) -> String
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		let out = self.cmd(args).output().unwrap();
		assert!(
			out.status.success(),
			"git failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout)
			.expect("git stdout is not utf-8")
			.trim()
			.to_string()
	}

	fn cmd<I, S>(&self, args: I) -> Command
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		let mut cmd = Command::new("git");
		cmd.args(args)
			.current_dir(self.path())
			.env("GIT_CONFIG_GLOBAL", &self.cfg)
			.env("GIT_CONFIG_NOSYSTEM", "1");
		cmd
	}

	fn git_with_input(&self, args: &[&str], input: &[u8]) -> String {
		let mut child = self
			.cmd(args)
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		child.stdin.take().unwrap().write_all(input).unwrap();
		let out = child.wait_with_output().unwrap();
		assert!(out.status.success(), "{:?}", out.stderr);
		String::from_utf8(out.stdout).unwrap().trim().to_string()
	}

	fn write(&self, name: &str, bytes: &[u8]) {
		let path = self.path().join(name);
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, bytes).unwrap();
	}

	fn commit(&self, msg: &str, date: &str, name: &str, email: &str) -> String {
		self.git(["add", "-A"]);
		self.commit_index(msg, date, name, email)
	}

	/// Commits the index as it stands. Use this after `update-index` so a
	/// later `git add -A` cannot drop a gitlink that is not in the worktree.
	fn commit_index(
		&self,
		msg: &str,
		date: &str,
		name: &str,
		email: &str,
	) -> String {
		// Keep large messages off Windows' command line and outside the index.
		let message_path = self.dir.path().join("commit-message");
		fs::write(&message_path, msg).unwrap();
		let out = self
			.cmd(["commit", "-q", "--allow-empty", "--cleanup=verbatim", "-F"])
			.arg(&message_path)
			.env("GIT_AUTHOR_NAME", name)
			.env("GIT_AUTHOR_EMAIL", email)
			.env("GIT_AUTHOR_DATE", date)
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"commit failed: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		self.git(["rev-parse", "HEAD"])
	}

	fn open(&self) -> Git {
		Git::open(&self.path()).unwrap()
	}
}

fn payload_limit(err: CommitError) -> (usize, usize) {
	match err {
		CommitError::PayloadLimit { limit, actual } => (limit, actual),
		other => panic!("expected PayloadLimit, got {other}"),
	}
}

fn legacy_text(git: &Git, shas: &[String]) -> (CommitsPayload, String) {
	let payload = copy_commits(git, shas).unwrap();
	let text = to_clipboard_text(&payload);
	(payload, text)
}

fn strict(
	git: &Git,
	shas: &[String],
	max: usize,
) -> Result<CommitExport, CommitError> {
	copy_commits_with(git, shas, &RunOptions::default(), max)
}

#[test]
fn exact_serialized_boundary_counts_escapes_and_metadata() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	let content = "\"quoted\"\n\\\\path\n\u{1}😀\n";
	repo.write("你好.txt", content.as_bytes());
	repo.write("plain.txt", b"ok\n");
	let sha = repo.commit(
		"say \"hi\"\nline\\\\two\n",
		"2024-03-04T05:06:07+08:00",
		"Alice \"A\" Z",
		"a@ex.com",
	);
	let git = repo.open();
	let shas = vec![sha];
	let (payload, text) = legacy_text(&git, &shas);
	assert!(text.contains("\\u0001"), "{text}");
	assert!(text.contains("Alice \\\"A\\\" Z"), "{text}");
	assert_eq!(payload.commits[0].author_name, "Alice \"A\" Z");
	let note = payload.commits[0]
		.files
		.iter()
		.find(|f| f.path == "你好.txt")
		.unwrap();
	assert_eq!(note.content.as_deref(), Some(content));

	let exact = strict(&git, &shas, text.len()).unwrap();
	assert_eq!(exact.text, text);
	assert_eq!(exact.payload, payload);
	assert_eq!(exact.text.len(), text.len());

	let (limit, actual) =
		payload_limit(strict(&git, &shas, text.len() - 1).unwrap_err());
	assert_eq!(limit, text.len() - 1);
	assert_eq!(actual, text.len());
	assert_idle();
}

#[test]
fn many_small_files_over_the_cap_refuse_the_whole_export() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	for i in 0..30 {
		repo.write(&format!("f{i:02}.txt"), b"small\n");
	}
	let sha = repo.commit("many", "2024-01-01T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();
	let shas = vec![sha];
	let (payload, text) = legacy_text(&git, &shas);
	assert_eq!(payload.commits[0].files.len(), 30);
	let one = text.len() / 30;
	assert!(one < text.len() / 2, "one file is not smaller than the sum");

	let over = strict(&git, &shas, text.len() / 2).unwrap_err();
	let (limit, actual) = payload_limit(over);
	assert_eq!(limit, text.len() / 2);
	assert!(actual > limit);
	let exact = strict(&git, &shas, text.len()).unwrap();
	assert_eq!(exact.text, text);
	assert_eq!(exact.payload.commits[0].files.len(), 30);
	assert_idle();
}

#[test]
fn huge_text_is_refused_from_the_header_without_keeping_the_body() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	let raw = "!".repeat(80_000);
	repo.write("huge.txt", raw.as_bytes());
	let sha = repo.commit("huge", "2024-01-02T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();
	let shas = vec![sha];
	let (_, text) = legacy_text(&git, &shas);

	let floor = to_clipboard_text(&CommitsPayload { commits: vec![] }).len();
	let (limit, actual) =
		payload_limit(strict(&git, &shas, floor + 1).unwrap_err());
	assert_eq!(limit, floor + 1);
	assert!(actual > limit);
	assert!(
		actual < 8_000,
		"metadata refusal reported {actual}, which includes the blob"
	);
	let kept = strict(&git, &shas, text.len()).unwrap();
	assert_eq!(kept.text, text);

	// Quotes expand under JSON. Header refusal reports raw size, not the
	// escaped document, so the body was not retained and re-serialized.
	let quotes = Repo::new();
	quotes.write("q.txt", "\"".repeat(80_000).as_bytes());
	let sha =
		quotes.commit("quotes", "2024-01-03T00:00:00Z", "Ada", "ada@ex.com");
	let (limit, actual) =
		payload_limit(strict(&quotes.open(), &[sha], 2_000).unwrap_err());
	assert_eq!(limit, 2_000);
	assert!(
		actual >= 80_000,
		"actual {actual} did not see the blob header"
	);
	assert!(
		actual < 80_000 + 8_000,
		"actual {actual} looks like the escaped body was counted"
	);
	assert_idle();
}

#[test]
fn huge_binary_and_non_utf8_remain_not_copied() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	let mut binary = vec![b'B'; 80_000];
	binary.push(0);
	repo.write("late.bin", &binary);
	repo.write("bad.txt", &vec![0xFF; 80_000]);
	repo.write("ok.txt", b"kept\n");
	let sha = repo.commit("mixed", "2024-04-01T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();
	let shas = vec![sha];
	let (payload, text) = legacy_text(&git, &shas);
	let find = |name: &str| {
		payload.commits[0]
			.files
			.iter()
			.find(|f| f.path == name)
			.unwrap()
	};
	assert_eq!(find("late.bin").not_copied, Some(NotCopiedReason::Binary));
	assert_eq!(find("late.bin").content, None);
	assert_eq!(find("bad.txt").not_copied, Some(NotCopiedReason::NonUtf8));
	assert_eq!(find("ok.txt").content.as_deref(), Some("kept\n"));
	assert!(text.len() < 3_000, "legacy text is {}", text.len());

	let export = strict(&git, &shas, 3_000).unwrap();
	assert_eq!(export.text, text);
	assert_eq!(export.payload, payload);
	assert!(!export.text.contains("BBBBBBBBBB"));
	assert_idle();
}

#[test]
fn legacy_bytes_metadata_and_replay_tree_match() {
	let _lock = serial();
	assert_idle();
	let src = Repo::new();
	let note = "\"quoted\"\n\\\\path\n\u{1}你好\n";
	src.write("keep.txt", b"keep\n");
	src.write("old.txt", b"rename me\n");
	src.write("gone.txt", b"gone\n");
	src.write("你好.txt", note.as_bytes());
	let root = src.commit(
		"root \"msg\"\n",
		"2020-01-01T00:00:00+00:00",
		"Alice \"A\" Z",
		"alice@ex.com",
	);
	src.git(["checkout", "-q", "-b", "side"]);
	src.write("side.txt", b"from side\n");
	src.commit(
		"side\n",
		"2020-01-01T01:00:00+00:00",
		"Alice \"A\" Z",
		"alice@ex.com",
	);
	src.git(["checkout", "-q", "main"]);
	src.write("keep.txt", b"keep v2\n");
	let main = src.commit(
		"main\n",
		"2020-01-01T02:00:00+00:00",
		"Alice \"A\" Z",
		"alice@ex.com",
	);
	let merged = src
		.cmd(["merge", "-q", "--no-ff", "--no-edit", "side"])
		.env("GIT_AUTHOR_NAME", "Alice \"A\" Z")
		.env("GIT_AUTHOR_EMAIL", "alice@ex.com")
		.env("GIT_AUTHOR_DATE", "2020-01-02T03:04:05+09:00")
		.output()
		.unwrap();
	assert!(
		merged.status.success(),
		"merge: {}",
		String::from_utf8_lossy(&merged.stderr)
	);
	let merge = src.git(["rev-parse", "HEAD"]);

	fs::create_dir(src.path().join("dir")).unwrap();
	src.git(["mv", "old.txt", "dir/new.txt"]);
	src.write("img.bin", &[0, 1, 2, 3, 4]);
	src.write("big5.txt", &[0xA4, 0xE9, 0xA5, 0xBB]);
	src.git(["add", "-A"]);
	src.git(["rm", "-q", "gone.txt"]);
	let mut hash = src
		.cmd(["hash-object", "-w", "--stdin"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	hash.stdin.take().unwrap().write_all(b"target\n").unwrap();
	let hashed = hash.wait_with_output().unwrap();
	assert!(
		hashed.status.success(),
		"{}",
		String::from_utf8_lossy(&hashed.stderr)
	);
	let link_oid = String::from_utf8(hashed.stdout).unwrap();
	let link_oid = link_oid.trim();
	src.git([
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("120000,{link_oid},the-link"),
	]);
	src.git([
		"update-index",
		"--add",
		"--cacheinfo",
		&format!("160000,{merge},vendor/lib"),
	]);
	let rename = src.commit_index(
		"rename\n",
		"2021-06-07T08:09:10-05:30",
		"Alice \"A\" Z",
		"alice@ex.com",
	);

	let git = src.open();
	let shas = vec![root.clone(), main.clone(), merge.clone(), rename.clone()];
	let (payload, text) = legacy_text(&git, &shas);
	let export = strict(&git, &shas, text.len()).unwrap();
	assert_eq!(export.text, text);
	assert_eq!(export.payload, payload);

	let root_files = &export.payload.commits[0].files;
	assert!(root_files.iter().any(|f| {
		f.path == "你好.txt"
			&& f.change == FileChange::Added
			&& f.content.as_deref() == Some(note)
	}));
	assert_eq!(export.payload.commits[0].author_name, "Alice \"A\" Z");
	// Keep Git's exact ISO representation (UTC may be Z or +00:00), while
	// independently pinning the source's actual author instant and offset.
	assert_eq!(
		export.payload.commits[0].author_date,
		src.git(["log", "-1", "--format=%aI", &root])
	);
	assert_eq!(
		src.git(["log", "-1", "--date=raw", "--format=%ad", &root]),
		"1577836800 +0000"
	);
	let merge_files = &export.payload.commits[2].files;
	assert_eq!(merge_files.len(), 1);
	assert_eq!(merge_files[0].path, "side.txt");
	assert_eq!(merge_files[0].change, FileChange::Added);

	let renamed = &export.payload.commits[3].files;
	let at = |name: &str| renamed.iter().find(|f| f.path == name).unwrap();
	assert_eq!(at("dir/new.txt").change, FileChange::Renamed);
	assert_eq!(at("dir/new.txt").old_path.as_deref(), Some("old.txt"));
	assert_eq!(at("dir/new.txt").content.as_deref(), Some("rename me\n"));
	assert_eq!(at("gone.txt").change, FileChange::Deleted);
	assert_eq!(at("gone.txt").content, None);
	assert_eq!(at("img.bin").not_copied, Some(NotCopiedReason::Binary));
	assert_eq!(at("big5.txt").not_copied, Some(NotCopiedReason::NonUtf8));
	assert_eq!(
		at("the-link").not_copied,
		Some(NotCopiedReason::UnsupportedType)
	);
	assert_eq!(
		at("vendor/lib").not_copied,
		Some(NotCopiedReason::UnsupportedType)
	);

	let dst = Repo::new();
	dst.write("base.txt", b"base\n");
	dst.commit(
		"target",
		"2019-01-01T00:00:00Z",
		"Local",
		"local@example.com",
	);
	dst.write("foreign.txt", b"staged\n");
	dst.git(["add", "foreign.txt"]);
	let result = snip_core::transfer::CommitReplayPreview::capture(
		&dst.path(),
		&export.payload,
	)
	.unwrap()
	.apply()
	.unwrap();
	assert_eq!(result.failure, None, "{:?}", result.failure);
	assert_eq!(result.created.len(), 4);
	for (created, source) in result.created.iter().zip(&shas) {
		let got = dst.git(["log", "-1", "--format=%an|%ae|%aI", created]);
		let expect = src.git(["log", "-1", "--format=%an|%ae|%aI", source]);
		assert_eq!(got, expect);
		let got_msg = dst.git(["log", "-1", "--format=%B", created]);
		let expect_msg = src.git(["log", "-1", "--format=%B", source]);
		assert_eq!(got_msg, expect_msg);
	}
	assert_eq!(dst.git(["show", "HEAD:dir/new.txt"]), "rename me");
	assert_eq!(dst.git(["show", "HEAD:你好.txt"]), note.trim_end());
	assert_eq!(dst.git(["show", "HEAD:keep.txt"]), "keep v2");
	assert_eq!(dst.git(["show", "HEAD:side.txt"]), "from side");
	let names = dst.git(["ls-tree", "-r", "--name-only", "HEAD"]);
	assert!(!names.contains("img.bin"), "{names}");
	assert!(!names.contains("gone.txt"), "{names}");
	assert!(!names.contains("the-link"), "{names}");
	assert!(!names.contains("vendor/lib"), "{names}");
	assert_eq!(dst.git(["diff", "--cached", "--name-only"]), "foreign.txt");
	assert_idle();
}

#[test]
fn per_call_stdout_limit_is_not_a_successful_export() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	repo.write("a.txt", b"hello\n");
	let sha = repo.commit("m", "2024-01-01T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();
	let shas = vec![sha];
	for overflow in [Overflow::Error, Overflow::Truncate] {
		let opts = RunOptions {
			max_stdout: 8,
			overflow,
			..RunOptions::default()
		};
		let err =
			copy_commits_with(&git, &shas, &opts, usize::MAX).unwrap_err();
		assert!(
			matches!(
				err,
				CommitError::Git(GitError::OutputLimit { limit: 8, .. })
			),
			"{overflow:?} -> {err}"
		);
	}
	assert_idle();
}

#[test]
fn missing_commit_is_a_git_error() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	repo.write("a.txt", b"hello\n");
	repo.commit("m", "2024-01-01T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();
	let bogus = "0123456789abcdef0123456789abcdef01234567".to_string();
	let err = strict(&git, &[bogus], usize::MAX).unwrap_err();
	assert!(
		matches!(err, CommitError::Git(GitError::Failed { .. })),
		"{err}"
	);
	assert_idle();
}

#[test]
fn empty_document_over_the_cap_does_not_need_a_blob() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	repo.write("a.txt", b"hello\n");
	let sha = repo.commit("m", "2024-01-01T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();
	let floor = to_clipboard_text(&CommitsPayload { commits: vec![] }).len();
	let err = strict(&git, &[sha], 0).unwrap_err();
	let (limit, actual) = payload_limit(err);
	assert_eq!((limit, actual), (0, floor));
	assert_idle();
}

#[test]
fn non_utf8_path_matches_legacy_not_copied() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	// A Git tree can contain arbitrary path bytes even on filesystems that
	// reject them (macOS), or platforms without non-UTF-8 OsString (Windows).
	let blob =
		repo.git_with_input(&["hash-object", "-w", "--stdin"], b"hello\n");
	let mut entry = format!("100644 blob {blob}\t").into_bytes();
	entry.extend_from_slice(b"weird-\xff.txt\0");
	let tree = repo.git_with_input(&["mktree", "-z"], &entry);
	let sha = repo.git(["commit-tree", &tree, "-m", "odd"]);
	let paths = repo
		.cmd(["ls-tree", "-rz", "--name-only", &sha])
		.output()
		.unwrap();
	assert!(paths.status.success());
	assert_eq!(paths.stdout, b"weird-\xff.txt\0");
	let git = repo.open();
	let shas = vec![sha];
	let (payload, text) = legacy_text(&git, &shas);
	assert_eq!(
		payload.commits[0].files[0].not_copied,
		Some(NotCopiedReason::NonUtf8Path)
	);
	assert_eq!(payload.commits[0].files[0].content, None);
	let export = strict(&git, &shas, text.len()).unwrap();
	assert_eq!(export.text, text);
	assert_eq!(export.payload, payload);
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
    let is_cat = args.first().is_some_and(|a| a == "cat-file");
    if phase == "close-cat-file" && is_cat {
        let _ = Command::new(&real)
            .args(&args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
        stall();
    }
    if !phase.is_empty()
        && phase != "close-cat-file"
        && args.iter().any(|arg| arg == phase.as_str())
    {
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
		let exe_name = if cfg!(windows) { "git.exe" } else { "git" };
		let exe = dir.path().join(exe_name);
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
	path: Option<OsString>,
	keys: Vec<&'static str>,
}

impl EnvGuard {
	fn new() -> Self {
		Self {
			path: None,
			keys: Vec::new(),
		}
	}

	fn set_path(&mut self, value: impl AsRef<OsStr>) {
		if self.path.is_none() {
			self.path = Some(std::env::var_os("PATH").unwrap_or_default());
		}
		std::env::set_var("PATH", value);
	}

	fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) {
		std::env::set_var(key, value);
		if !self.keys.contains(&key) {
			self.keys.push(key);
		}
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

fn prepend_path(front: &Path) -> OsString {
	let mut path = front.as_os_str().to_os_string();
	path.push(if cfg!(windows) { ";" } else { ":" });
	if let Some(rest) = std::env::var_os("PATH") {
		path.push(rest);
	}
	path
}

struct RunningExport {
	cancel: CancelToken,
	handle: Option<JoinHandle<Result<CommitExport, CommitError>>>,
}

impl RunningExport {
	fn spawn(git: Git, shas: Vec<String>, cancel: CancelToken) -> Self {
		let token = cancel.clone();
		let handle = thread::spawn(move || {
			copy_commits_with(
				&git,
				&shas,
				&RunOptions {
					cancel: Some(token),
					timeout: Duration::from_secs(15),
					..RunOptions::default()
				},
				usize::MAX,
			)
		});
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
				panic!("export ended before the shim was ready: {result:?}");
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

impl Drop for RunningExport {
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

#[test]
fn cancellation_is_preemptive_and_in_flight() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	repo.write("a.txt", b"one\n");
	let root = repo.commit("root", "2024-01-01T00:00:00Z", "Ada", "ada@ex.com");
	repo.write("a.txt", b"two\n");
	let child =
		repo.commit("child", "2024-01-02T00:00:00Z", "Ada", "ada@ex.com");
	let git = repo.open();

	let cancel = CancelToken::new();
	cancel.cancel();
	let err = copy_commits_with(
		&git,
		std::slice::from_ref(&child),
		&RunOptions {
			cancel: Some(cancel),
			..RunOptions::default()
		},
		usize::MAX,
	)
	.unwrap_err();
	assert!(
		matches!(err, CommitError::Git(GitError::Cancelled { .. })),
		"{err}"
	);
	assert_idle();

	let phases = [
		("log", child.as_str(), "log"),
		("rev-list", child.as_str(), "rev-list"),
		("rev-parse", root.as_str(), "rev-parse"),
		("diff-tree", child.as_str(), "diff-tree"),
		("cat-file", child.as_str(), "cat-file"),
		("close-cat-file", child.as_str(), "cat-file"),
	];
	for (phase, sha, args_needle) in phases {
		let ready = repo.dir.path().join(format!("ready-{phase}"));
		let _ = fs::remove_file(&ready);
		let mut env = EnvGuard::new();
		env.set("SNIP_STALL_PHASE", phase);
		env.set("SNIP_STALL_READY", ready.as_os_str());
		env.set("SNIP_REAL_GIT", real_git().as_os_str());
		env.set_path(prepend_path(shim_dir()));

		let cancel = CancelToken::new();
		let mut running = RunningExport::spawn(
			git.clone(),
			vec![sha.to_string()],
			cancel.clone(),
		);
		let started = Instant::now();
		let pid = running.wait_ready(&ready);
		cancel.cancel();
		let joined = running.handle.take().unwrap().join();
		let err = match joined {
			Ok(Err(err)) => err,
			Ok(Ok(_)) => panic!("{phase}: export succeeded after cancel"),
			Err(_) => panic!("{phase}: export thread panicked"),
		};
		assert!(
			started.elapsed() < Duration::from_secs(20),
			"{phase} took {:?}",
			started.elapsed()
		);
		match err {
			CommitError::Git(GitError::Cancelled { args }) => {
				assert!(
					args.contains(args_needle),
					"{phase}: cancelled args {args:?} missing {args_needle}"
				);
			}
			other => panic!("{phase}: {other}"),
		}
		assert_dead(pid);
		assert_idle();
	}
}

#[test]
fn exact_boundary_cap_n_vs_n_minus_1_with_escapes_metadata_skipped_and_merge() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	let escaped_content =
		"\"quoted\"\n\\\\backslash\r\n\t\u{0001}\u{0002}unicode 🚀🎉\n";
	repo.write("escaped.txt", escaped_content.as_bytes());
	let mut binary_data = vec![b'A'; 256];
	binary_data[64] = 0; // NUL byte marks binary
	repo.write("data.bin", &binary_data);
	let non_utf8_data = vec![0xFF, 0xFE, 0xFD, 0xFC];
	repo.write("invalid_utf8.txt", &non_utf8_data);
	let root = repo.commit(
		"root \"commit\"\nline\\two\r\n\t\u{0003}emoji ✨\n",
		"2024-05-01T12:34:56+09:00",
		"Alice \"A\" \\ Engineer",
		"alice@example.com",
	);

	// Create a branch and merge commit
	repo.git(["checkout", "-q", "-b", "feature"]);
	repo.write("feature.txt", b"feature content\n");
	let _feature = repo.commit(
		"feature",
		"2024-05-01T13:00:00+09:00",
		"Bob",
		"bob@example.com",
	);
	repo.git(["checkout", "-q", "main"]);
	repo.write("main_extra.txt", b"main extra\n");
	let main_c = repo.commit(
		"main",
		"2024-05-01T14:00:00+09:00",
		"Alice",
		"alice@example.com",
	);
	let merge_out = repo
		.cmd(["merge", "-q", "--no-ff", "--no-edit", "feature"])
		.env("GIT_AUTHOR_NAME", "Merger \"M\"")
		.env("GIT_AUTHOR_EMAIL", "merger@example.com")
		.env("GIT_AUTHOR_DATE", "2024-05-02T10:00:00+09:00")
		.output()
		.unwrap();
	assert!(merge_out.status.success());
	let merge = repo.git(["rev-parse", "HEAD"]);

	let git = repo.open();
	let shas = vec![root, main_c, merge];
	let (payload, text) = legacy_text(&git, &shas);
	let n = text.len();

	// Exact cap n must succeed
	let export = strict(&git, &shas, n).unwrap();
	assert_eq!(export.text, text);
	assert_eq!(export.text.len(), n);
	assert_eq!(export.payload, payload);

	// Verify not_copied reasons
	let first_commit_files = &export.payload.commits[0].files;
	let bin_file = first_commit_files
		.iter()
		.find(|f| f.path == "data.bin")
		.unwrap();
	assert_eq!(bin_file.not_copied, Some(NotCopiedReason::Binary));
	assert_eq!(bin_file.content, None);
	let utf8_file = first_commit_files
		.iter()
		.find(|f| f.path == "invalid_utf8.txt")
		.unwrap();
	assert_eq!(utf8_file.not_copied, Some(NotCopiedReason::NonUtf8));
	assert_eq!(utf8_file.content, None);

	// Exact cap n - 1 must reject with exact PayloadLimit
	let err = strict(&git, &shas, n - 1).unwrap_err();
	let (limit, actual) = payload_limit(err);
	assert_eq!(limit, n - 1);
	assert_eq!(actual, n);
	assert_idle();
}

#[test]
fn large_commit_message_metadata_over_budget_rejects_with_correct_classification(
) {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	// 64 KiB commit message
	let big_msg = "M".repeat(64 * 1024);
	repo.write("f.txt", b"content\n");
	let sha =
		repo.commit(&big_msg, "2024-01-01T00:00:00Z", "Ada", "ada@example.com");
	assert_eq!(repo.git(["log", "-1", "--format=%B", &sha]), big_msg);
	let git = repo.open();

	// Case 1: Within runner max_stdout (default 256 MiB), but clipboard byte budget is small (500 bytes).
	// Bounded allocation: stdout is capped at remaining (463 bytes), NOT reading 64 KiB or 256 MiB.
	// Must reject promptly with PayloadLimit, NOT OutputLimit.
	let err1 = copy_commits_with(
		&git,
		std::slice::from_ref(&sha),
		&RunOptions::default(),
		500,
	)
	.unwrap_err();
	match err1 {
		CommitError::PayloadLimit { limit, actual } => {
			assert_eq!(limit, 500);
			assert!(actual > 500, "actual {actual} must exceed limit 500");
			assert!(
				actual < 2000,
				"actual {actual} must reflect bounded measurement, not huge buffer"
			);
		}
		other => panic!("expected PayloadLimit, got {other:?}"),
	}
	assert_idle();

	// Case 2: Clipboard budget is large (100,000 bytes), but configured Git runner max_stdout is small (1024 bytes).
	// Must reject with GitError::OutputLimit, NOT PayloadLimit.
	let tight_opts = RunOptions {
		max_stdout: 1024,
		..RunOptions::default()
	};
	let err2 = copy_commits_with(
		&git,
		std::slice::from_ref(&sha),
		&tight_opts,
		100_000,
	)
	.unwrap_err();
	match err2 {
		CommitError::Git(GitError::OutputLimit { limit, .. }) => {
			assert_eq!(limit, 1024);
		}
		other => panic!("expected GitError::OutputLimit, got {other:?}"),
	}
	assert_idle();

	// Case 3: Message with characters that expand under JSON escaping (400 quotes).
	// Raw stdout is ~460 bytes. We set budget to 550 bytes.
	// Escaped message expands to >800 bytes, exceeding budget during metadata measurement.
	// Must reject with exact PayloadLimit before admitting or pushing the commit.
	let repo2 = Repo::new();
	repo2.write("f.txt", b"hello\n");
	let quoted_msg = "\"".repeat(400);
	let sha2 = repo2.commit(
		&quoted_msg,
		"2024-01-01T00:00:00Z",
		"Ada",
		"ada@example.com",
	);
	let git2 = repo2.open();
	let err3 = copy_commits_with(
		&git2,
		std::slice::from_ref(&sha2),
		&RunOptions::default(),
		550,
	)
	.unwrap_err();
	match err3 {
		CommitError::PayloadLimit { limit, actual } => {
			assert_eq!(limit, 550);
			assert!(
				actual > 800,
				"actual {actual} must reflect JSON-escaped quotes"
			);
		}
		other => panic!("expected PayloadLimit, got {other:?}"),
	}
	assert_idle();
}

#[test]
fn sha_list_admission_stops_at_budget_without_visiting_subsequent_shas() {
	let _lock = serial();
	assert_idle();
	let repo = Repo::new();
	repo.write("a.txt", b"hello\n");
	let sha1 =
		repo.commit("first", "2024-01-01T00:00:00Z", "Ada", "ada@example.com");
	repo.write("b.txt", b"world\n");
	let sha2 =
		repo.commit("second", "2024-01-02T00:00:00Z", "Ada", "ada@example.com");
	let git = repo.open();

	let (_payload1, text1) = legacy_text(&git, std::slice::from_ref(&sha1));
	let len1 = text1.len();

	// Pass a list of 50 SHAs: 2 real commits followed by 48 invalid SHAs.
	// Set budget to fit commit 1 (len1), but reject on commit 2.
	// It must reject with PayloadLimit on commit 2 without visiting or erroring on the remaining invalid SHAs.
	// (Zero upfront allocation for the SHA list is guaranteed by Vec::new() in export_commits_counted).
	let mut shas = vec![sha1, sha2];
	for i in 0..48 {
		shas.push(format!("ffffffffffffffffffffffffffffffff{:08x}", i));
	}

	let err = copy_commits_with(&git, &shas, &RunOptions::default(), len1 + 10)
		.unwrap_err();
	let (limit, actual) = payload_limit(err);
	assert_eq!(limit, len1 + 10);
	assert!(actual > limit);

	// Also verify small budget below floor (38 bytes) rejects immediately without touching git or allocating.
	let err_floor =
		copy_commits_with(&git, &shas, &RunOptions::default(), 10).unwrap_err();
	let (limit_floor, actual_floor) = payload_limit(err_floor);
	assert_eq!(limit_floor, 10);
	assert_eq!(actual_floor, 38);
	assert_idle();
}
