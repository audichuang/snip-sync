//! End-to-end runs of the `snip` binary through `--stdout` / `--stdin`.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn snip(args: &[&str], stdin: Option<&[u8]>) -> Output {
	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let mut input = child.stdin.take().unwrap();
	if let Some(bytes) = stdin {
		input.write_all(bytes).unwrap();
	}
	drop(input);
	child.wait_with_output().unwrap()
}

fn code(out: &Output) -> i32 {
	out.status.code().unwrap()
}

fn text(bytes: &[u8]) -> String {
	String::from_utf8_lossy(bytes).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
	let out = Command::new("git")
		.args(args)
		.current_dir(dir)
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.output()
		.unwrap();
	assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
	text(&out.stdout)
}

fn init_repo(dir: &Path) {
	git(dir, &["init", "-q", "-b", "main"]);
	git(dir, &["config", "user.name", "Committer"]);
	git(dir, &["config", "user.email", "committer@example.com"]);
}

fn commit(dir: &Path, message: &str, date: &str) {
	git(dir, &["add", "-A"]);
	let author = "--author=Alice <alice@example.com>";
	let date = format!("--date={date}");
	git(dir, &["commit", "-q", author, &date, "-m", message]);
}

#[test]
fn file_mode_round_trip() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	fs::create_dir_all(src.join("sub")).unwrap();
	fs::create_dir_all(&dst).unwrap();
	fs::write(src.join("a.txt"), "hello\nworld").unwrap();
	fs::write(src.join("sub/b.rs"), "fn main() {}").unwrap();
	let src_s = src.to_str().unwrap();
	let dst_s = dst.to_str().unwrap();

	let out = snip(&["--repo", src_s, "copy", src_s, "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let payload = out.stdout;
	assert!(text(&payload).contains("// file: sub/b.rs"));
	assert!(text(&out.stderr).contains("2 file(s) copied."));
	assert!(text(&out.stderr).contains("chars ·"));

	let out = snip(
		&["--repo", dst_s, "paste", "--dry-run", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(text(&out.stdout).contains("create\ta.txt"));
	assert!(text(&out.stdout).contains("create\tsub/b.rs"));
	assert!(!dst.join("a.txt").exists(), "dry run must not write");

	let out = snip(
		&["--repo", dst_s, "paste", "--apply", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(text(&out.stdout).contains("Created 2"));
	assert_eq!(
		fs::read_to_string(dst.join("a.txt")).unwrap(),
		"hello\nworld"
	);
	assert_eq!(
		fs::read_to_string(dst.join("sub/b.rs")).unwrap(),
		"fn main() {}"
	);

	// Existing targets need an explicit choice.
	fs::write(dst.join("a.txt"), "changed").unwrap();
	let out = snip(
		&["--repo", dst_s, "paste", "--apply", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 2);
	let args = ["--repo", dst_s, "paste", "--apply", "--skip-existing"];
	let out = snip(&[&args[..], &["--stdin"]].concat(), Some(&payload));
	assert_eq!(code(&out), 0);
	assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "changed");
	let args = ["--repo", dst_s, "paste", "--apply", "--overwrite"];
	let out = snip(&[&args[..], &["--stdin"]].concat(), Some(&payload));
	assert_eq!(code(&out), 0);
	assert!(text(&out.stdout).contains("Overwritten 2"));
	assert_eq!(
		fs::read_to_string(dst.join("a.txt")).unwrap(),
		"hello\nworld"
	);
}

#[test]
fn commit_mode_round_trip() {
	let tmp = tempfile::tempdir().unwrap();
	let a = tmp.path().join("a");
	let b = tmp.path().join("b");
	fs::create_dir_all(&a).unwrap();
	fs::create_dir_all(&b).unwrap();
	init_repo(&a);
	init_repo(&b);

	fs::write(a.join("base.txt"), "base\n").unwrap();
	commit(&a, "base", "2024-01-01T00:00:00+00:00");
	fs::write(a.join("one.txt"), "one\n").unwrap();
	fs::write(a.join("bin.dat"), [0u8, 1, 2, 0xff]).unwrap();
	commit(&a, "add one\n\nbody line", "2024-02-01T10:00:00+08:00");
	fs::rename(a.join("one.txt"), a.join("renamed.txt")).unwrap();
	commit(&a, "rename one", "2024-03-01T10:00:00-05:00");
	fs::remove_file(a.join("base.txt")).unwrap();
	commit(&a, "delete base", "2024-04-01T10:00:00+00:00");

	fs::write(b.join("base.txt"), "base\n").unwrap();
	fs::write(b.join("local.txt"), "local\n").unwrap();
	commit(&b, "b root", "2024-01-05T00:00:00+00:00");

	let a_s = a.to_str().unwrap();
	let b_s = b.to_str().unwrap();
	let out = snip(
		&["--repo", a_s, "copy", "--commits", "-n", "3", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let payload = out.stdout;
	assert!(text(&payload).starts_with("// snip-sync commits v1\n"));
	let note = text(&out.stderr);
	assert!(note.contains("3 commit(s) copied"), "{note}");
	assert!(note.contains("1 file(s) not copied"), "{note}");
	assert!(
		note.contains("commit 1: bin.dat not copied (BINARY)"),
		"{note}"
	);

	let out = snip(
		&["--repo", b_s, "paste", "--dry-run", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let plan = text(&out.stdout);
	assert!(plan.contains("[1/3] add one"), "{plan}");
	assert!(plan.contains("one.txt -> renamed.txt"), "{plan}");
	assert!(plan.contains("skip\tadded\tbin.dat\tBINARY"), "{plan}");

	let out = snip(
		&["--repo", b_s, "paste", "--apply", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(text(&out.stdout).contains("Created 3 commit(s)."));

	let log = |dir: &Path| {
		git(dir, &["log", "-3", "--format=%B%x00%an%x00%ae%x00%aI%x01"])
	};
	assert_eq!(log(&a), log(&b));
	assert_eq!(fs::read_to_string(b.join("renamed.txt")).unwrap(), "one\n");
	assert!(!b.join("base.txt").exists());
	assert!(!b.join("one.txt").exists());
	assert!(!b.join("bin.dat").exists());
	assert_eq!(fs::read_to_string(b.join("local.txt")).unwrap(), "local\n");
}

#[test]
fn git_commit_source_copies_changes() {
	let tmp = tempfile::tempdir().unwrap();
	let a = tmp.path();
	init_repo(a);
	fs::write(a.join("x.txt"), "x").unwrap();
	commit(a, "x", "2024-01-01T00:00:00+00:00");
	let a_s = a.to_str().unwrap();
	let out = snip(
		&["--repo", a_s, "copy", "--commit", "HEAD", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(text(&out.stdout).contains("[NEW] x.txt"));
	assert!(text(&out.stderr).contains("1 file(s) copied."));
}

#[test]
fn usage_errors_exit_2() {
	let tmp = tempfile::tempdir().unwrap();
	let dir = tmp.path().to_str().unwrap();
	let cases: &[&[&str]] = &[
		&["copy"],
		&["copy", "--working", "--staged"],
		&["copy", "-n", "3"],
		&["copy", "--commits", "--stdout"],
		&["copy", "--range", "abc", "--stdout"],
		&["paste", "--stdin"],
		&["paste", "--dry-run", "--apply", "--stdin"],
		&["paste", "--apply", "--overwrite", "--skip-existing"],
		&["--settings", "{nope", "copy", "--working"],
	];
	for args in cases {
		let out = snip(&[&["--repo", dir][..], args].concat(), Some(b""));
		assert_eq!(code(&out), 2, "{args:?}: {}", text(&out.stderr));
	}
}

#[test]
fn empty_input_and_git_failures_exit_1() {
	let tmp = tempfile::tempdir().unwrap();
	let dir = tmp.path().to_str().unwrap();
	let out = snip(
		&["--repo", dir, "paste", "--dry-run", "--stdin"],
		Some(b" \n"),
	);
	assert_eq!(code(&out), 1);
	let out = snip(
		&["--repo", dir, "paste", "--dry-run", "--stdin"],
		Some(b"no headers here"),
	);
	assert_eq!(code(&out), 1);
	// Not a git repository.
	let out = snip(&["--repo", dir, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 1);
}

#[test]
fn discontinuous_commits_are_refused() {
	let tmp = tempfile::tempdir().unwrap();
	let a = tmp.path();
	init_repo(a);
	fs::write(a.join("x.txt"), "x").unwrap();
	commit(a, "root", "2024-01-01T00:00:00+00:00");
	git(a, &["checkout", "-q", "-b", "side"]);
	fs::write(a.join("side.txt"), "s").unwrap();
	commit(a, "side", "2024-01-02T00:00:00+00:00");
	git(a, &["checkout", "-q", "main"]);
	fs::write(a.join("main.txt"), "m").unwrap();
	commit(a, "main", "2024-01-03T00:00:00+00:00");
	let a_s = a.to_str().unwrap();
	let out = snip(
		&["--repo", a_s, "copy", "--commits", "side..main", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 1);
	assert!(
		text(&out.stderr).contains("not contiguous"),
		"{}",
		text(&out.stderr)
	);
}

#[test]
fn git_sources_label_paths_against_repo_subdirectory() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("r");
	fs::create_dir_all(repo.join("sub")).unwrap();
	init_repo(&repo);
	fs::write(repo.join("sub/a.txt"), "one").unwrap();
	commit(&repo, "init", "2020-01-01T00:00:00+00:00");
	fs::write(repo.join("sub/a.txt"), "two").unwrap();
	let sub = repo.join("sub");
	let out = snip(
		&[
			"--repo",
			sub.to_str().unwrap(),
			"copy",
			"--working",
			"--stdout",
		],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let payload = text(&out.stdout);
	// Same labelling as `snip copy <paths>` with the same --repo.
	assert!(payload.contains("[MODIFIED] a.txt"), "{payload}");
	assert!(!payload.contains("sub/a.txt"), "{payload}");
}
