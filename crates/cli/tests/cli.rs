//! End-to-end runs of the `snip` binary through `--stdout` / `--stdin`.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn snip_with_timeout_in_dir(
	args: &[&str],
	stdin: Option<&[u8]>,
	cwd: Option<&Path>,
	timeout: std::time::Duration,
	timeout_msg: &str,
) -> Output {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip"));
	cmd.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	if let Some(dir) = cwd {
		cmd.current_dir(dir);
	}
	let mut child = cmd.spawn().unwrap();
	let mut stdout_pipe = child.stdout.take().unwrap();
	let mut stderr_pipe = child.stderr.take().unwrap();
	let stdout_reader = std::thread::spawn(move || {
		let mut buf = Vec::new();
		let _ = stdout_pipe.read_to_end(&mut buf);
		buf
	});
	let stderr_reader = std::thread::spawn(move || {
		let mut buf = Vec::new();
		let _ = stderr_pipe.read_to_end(&mut buf);
		buf
	});
	if let Some(mut input) = child.stdin.take() {
		if let Some(bytes) = stdin {
			let bytes = bytes.to_vec();
			std::thread::spawn(move || {
				let _ = input.write_all(&bytes);
			});
		}
	}
	let start = std::time::Instant::now();
	let status = loop {
		match child.try_wait().unwrap() {
			Some(s) => break s,
			None if start.elapsed() < timeout => {
				std::thread::sleep(std::time::Duration::from_millis(50));
			}
			None => {
				let _ = child.kill();
				let _ = child.wait();
				panic!("{timeout_msg}");
			}
		}
	};
	let stdout = stdout_reader.join().unwrap();
	let stderr = stderr_reader.join().unwrap();
	Output {
		status,
		stdout,
		stderr,
	}
}

fn snip(args: &[&str], stdin: Option<&[u8]>) -> Output {
	snip_with_timeout_in_dir(
		args,
		stdin,
		None,
		std::time::Duration::from_secs(60),
		"snip process timed out",
	)
}

fn snip_in_dir(args: &[&str], stdin: Option<&[u8]>, cwd: &Path) -> Output {
	snip_with_timeout_in_dir(
		args,
		stdin,
		Some(cwd),
		std::time::Duration::from_secs(60),
		"snip process timed out",
	)
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

#[test]
fn paste_commits_dry_run_reports_layout_refusal_and_accurate_count() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	init_repo(&src);
	init_repo(&dst);
	fs::write(src.join("base.txt"), "base\n").unwrap();
	commit(&src, "initial", "2024-01-01T00:00:00+00:00");
	fs::create_dir_all(src.join("blocker")).unwrap();
	fs::write(src.join("blocker/file.txt"), "content\n").unwrap();
	commit(&src, "first commit", "2024-01-02T00:00:00+00:00");
	fs::write(src.join("fresh.txt"), "fresh\n").unwrap();
	commit(&src, "second commit", "2024-01-03T00:00:00+00:00");

	fs::write(dst.join("blocker"), "regular file\n").unwrap();
	commit(&dst, "initial", "2024-01-01T00:00:00+00:00");

	let src_s = src.to_str().unwrap();
	let dst_s = dst.to_str().unwrap();
	let out = snip(
		&["--repo", src_s, "copy", "--commits", "-n", "2", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let payload = out.stdout;

	let out = snip(
		&["--repo", dst_s, "paste", "--dry-run", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(
		stdout.contains("[1/2] first commit (refused: a file is in the way of its parent directory)"),
		"{stdout}"
	);
	assert!(stdout.contains("[2/2] second commit"), "{stdout}");
	assert!(
		!stdout.contains("[2/2] second commit (refused:"),
		"{stdout}"
	);
	assert!(
		stderr.contains(
			"0 commit(s) would be created; replay stops at commit #1 (refused); 1 not reached."
		),
		"{stderr}"
	);
}

#[test]
fn paste_commits_dry_run_three_commits_reports_stop_point_and_not_reached() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	init_repo(&src);
	init_repo(&dst);
	fs::write(src.join("base.txt"), "base\n").unwrap();
	commit(&src, "initial", "2024-01-01T00:00:00+00:00");
	fs::write(src.join("ok1.txt"), "ok1\n").unwrap();
	commit(&src, "commit ok 1", "2024-01-02T00:00:00+00:00");
	fs::create_dir_all(src.join("blocker")).unwrap();
	fs::write(src.join("blocker/file.txt"), "content\n").unwrap();
	commit(&src, "commit blocked", "2024-01-03T00:00:00+00:00");
	fs::write(src.join("ok2.txt"), "ok2\n").unwrap();
	commit(&src, "commit ok 2", "2024-01-04T00:00:00+00:00");

	fs::write(dst.join("blocker"), "regular file\n").unwrap();
	commit(&dst, "initial", "2024-01-01T00:00:00+00:00");

	let src_s = src.to_str().unwrap();
	let dst_s = dst.to_str().unwrap();
	let out = snip(
		&["--repo", src_s, "copy", "--commits", "-n", "3", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let payload = out.stdout;

	let out = snip(
		&["--repo", dst_s, "paste", "--dry-run", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(stdout.contains("[1/3] commit ok 1"), "{stdout}");
	assert!(
		stdout.contains("[2/3] commit blocked (refused: a file is in the way of its parent directory)"),
		"{stdout}"
	);
	assert!(stdout.contains("[3/3] commit ok 2"), "{stdout}");
	assert!(!stdout.contains("[3/3] commit ok 2 (refused:"), "{stdout}");
	assert!(
		stderr.contains(
			"1 commit(s) would be created; replay stops at commit #2 (refused); 1 not reached."
		),
		"{stderr}"
	);
}

/// `(path, change, content)`.
type PayloadFile<'a> = (&'a str, &'a str, Option<&'a str>);

/// A commits payload from `(message, files)`.
fn commits_payload(commits: &[(&str, &[PayloadFile])]) -> String {
	let commits: Vec<String> = commits
		.iter()
		.map(|(message, files)| {
			let files: Vec<String> = files
				.iter()
				.map(|(path, change, content)| {
					let content = content
						.map_or("null".to_string(), |c| format!("{c:?}"));
					format!(
						"{{\"path\":{path:?},\"oldPath\":null,\"change\":{change:?},\"content\":{content},\"notCopied\":null}}"
					)
				})
				.collect();
			format!(
				"{{\"message\":\"{message}\\n\",\"authorName\":\"QA\",\"authorEmail\":\"qa@example.com\",\"authorDate\":\"2026-06-02T09:00:00+08:00\",\"files\":[{}]}}",
				files.join(",")
			)
		})
		.collect();
	format!(
		"// snip-sync commits v1\n{{\"commits\":[{}]}}",
		commits.join(",")
	)
}

#[test]
fn paste_commits_dry_run_plans_each_commit_after_the_earlier_ones() {
	// Commit 1 deletes the regular file `newdir`, commit 2 writes under it:
	// the dry-run agrees with Apply that both are created.
	let tmp = tempfile::tempdir().unwrap();
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&dst).unwrap();
	init_repo(&dst);
	fs::write(dst.join("newdir"), "regular file\n").unwrap();
	commit(&dst, "initial", "2024-01-01T00:00:00+00:00");
	let dst_s = dst.to_str().unwrap();
	let payload = commits_payload(&[
		("remove blocker", &[("newdir", "DELETED", None)]),
		("write under it", &[("newdir/x.txt", "ADDED", Some("x\n"))]),
	]);
	let dry = snip(
		&["--repo", dst_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	let stdout = text(&dry.stdout);
	assert!(stdout.contains("[2/2] write under it\n"), "{stdout}");
	assert!(!stdout.contains("refused"), "{stdout}");
	assert!(
		text(&dry.stderr).contains("2 commit(s) would be created."),
		"{}",
		text(&dry.stderr)
	);
	let apply = snip(
		&["--repo", dst_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&apply), 0, "{}", text(&apply.stderr));
	assert!(
		text(&apply.stdout).contains("Created 2 commit(s)."),
		"{}",
		text(&apply.stdout)
	);

	// Commit 1 writes a file `newdir`, commit 2 writes under it: both stop
	// at commit 2.
	let dst = tmp.path().join("dst2");
	fs::create_dir_all(&dst).unwrap();
	init_repo(&dst);
	fs::write(dst.join("keep.txt"), "keep\n").unwrap();
	commit(&dst, "initial", "2024-01-01T00:00:00+00:00");
	let dst_s = dst.to_str().unwrap();
	let payload = commits_payload(&[
		("create blocker", &[("newdir", "ADDED", Some("f\n"))]),
		("write under it", &[("newdir/x.txt", "ADDED", Some("x\n"))]),
	]);
	let dry = snip(
		&["--repo", dst_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	let stdout = text(&dry.stdout);
	assert!(
		stdout.contains("[2/2] write under it (refused: a file is in the way of its parent directory)"),
		"{stdout}"
	);
	assert!(
		text(&dry.stderr).contains(
			"1 commit(s) would be created; replay stops at commit #2 (refused); 0 not reached."
		),
		"{}",
		text(&dry.stderr)
	);
	let apply = snip(
		&["--repo", dst_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	let all = format!("{}{}", text(&apply.stdout), text(&apply.stderr));
	assert!(all.contains("Created 1 commit(s)."), "{all}");
	assert!(
		all.contains("a file is in the way of its parent directory"),
		"{all}"
	);
}

#[test]
fn paste_commits_apply_replays_over_a_directory_an_earlier_commit_empties() {
	// Commit 1 deletes d/f.txt (leaving `d` empty and so removed), commit 2
	// writes a file at `d`: dry-run and Apply both create 2 commits.
	let tmp = tempfile::tempdir().unwrap();
	let dst = tmp.path().join("dst");
	fs::create_dir_all(dst.join("d")).unwrap();
	init_repo(&dst);
	fs::write(dst.join("d/f.txt"), "f\n").unwrap();
	commit(&dst, "initial", "2024-01-01T00:00:00+00:00");
	let dst_s = dst.to_str().unwrap();
	let payload = commits_payload(&[
		("empty the dir", &[("d/f.txt", "DELETED", None)]),
		("file at its path", &[("d", "ADDED", Some("now a file\n"))]),
	]);
	let dry = snip(
		&["--repo", dst_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	assert!(
		!text(&dry.stdout).contains("refused"),
		"{}",
		text(&dry.stdout)
	);
	let apply = snip(
		&["--repo", dst_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&apply), 0, "{}", text(&apply.stderr));
	assert!(
		text(&apply.stdout).contains("Created 2 commit(s)."),
		"{}",
		text(&apply.stdout)
	);
	assert_eq!(fs::read_to_string(dst.join("d")).unwrap(), "now a file\n");
}

#[test]
fn copy_commits_over_payload_cap_exits_one() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	fs::create_dir_all(&src).unwrap();
	init_repo(&src);
	let chunk = "0123456789abcdef\n";
	let repeats = (33 * 1024 * 1024) / chunk.len() + 1;
	fs::write(src.join("big.txt"), chunk.repeat(repeats)).unwrap();
	commit(&src, "oversize commit", "2024-01-01T00:00:00+00:00");

	let src_s = src.to_str().unwrap();
	let out = snip(
		&["--repo", src_s, "copy", "--commits", "-n", "1", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	let err = text(&out.stderr);
	assert!(
		err.contains("33554432"),
		"expected limit (33554432) mentioned in error, got: {err}"
	);
}

#[cfg(unix)]
#[test]
fn copy_paths_skips_fifo_without_hanging() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	let fifo_path = repo.join("test_fifo");
	let status = Command::new("mkfifo")
		.arg(&fifo_path)
		.status()
		.expect("failed to execute mkfifo");
	assert!(status.success(), "mkfifo failed");
	let regular_file = repo.join("regular.txt");
	fs::write(&regular_file, "regular content\n").unwrap();

	let repo_s = repo.to_str().unwrap();
	let out = snip_with_timeout_in_dir(
		&["--repo", repo_s, "copy", repo_s, "--stdout"],
		None,
		None,
		std::time::Duration::from_secs(60),
		"snip copy hung on a FIFO",
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(stdout.contains("// file: regular.txt"), "{stdout}");
	assert!(stdout.contains("regular content"), "{stdout}");
	assert!(!stdout.contains("test_fifo"), "{stdout}");
	assert!(stderr.contains("1 file(s) copied"), "{stderr}");
	assert!(stderr.contains("skipped"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn copy_paths_skips_symlink_pointing_outside_root() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&repo).unwrap();
	fs::create_dir_all(&outside).unwrap();
	let outside_file = outside.join("secret.txt");
	fs::write(&outside_file, "outside secret content\n").unwrap();
	let inside_file = repo.join("inside.txt");
	fs::write(&inside_file, "inside content\n").unwrap();
	let symlink = repo.join("symlink_outside.txt");
	std::os::unix::fs::symlink(&outside_file, &symlink).unwrap();

	let repo_s = repo.to_str().unwrap();
	let out = snip(&["--repo", repo_s, "copy", repo_s, "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	assert!(stdout.contains("// file: inside.txt"), "{stdout}");
	assert!(stdout.contains("inside content"), "{stdout}");
	assert!(!stdout.contains("outside secret content"), "{stdout}");
	assert!(!stdout.contains("symlink_outside.txt"), "{stdout}");
	assert!(!stdout.contains("secret.txt"), "{stdout}");
}

#[test]
fn copy_paths_never_includes_git_entries() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);
	fs::write(repo.join("file.txt"), "hello").unwrap();
	commit(&repo, "initial", "2024-01-01T00:00:00+00:00");

	// Add a nested git repository
	let nested = repo.join("nested_repo");
	fs::create_dir_all(&nested).unwrap();
	init_repo(&nested);
	fs::write(nested.join("nested.txt"), "nested hello").unwrap();
	commit(&nested, "nested init", "2024-01-01T00:00:00+00:00");

	let repo_s = repo.to_str().unwrap();
	let out = snip(&["--repo", repo_s, "copy", repo_s, "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	assert!(stdout.contains("// file: file.txt"), "{stdout}");
	assert!(!stdout.contains(".git"), "{stdout}");
	assert!(!stdout.contains("nested.txt"), "{stdout}");

	let out_dot =
		snip_in_dir(&["--repo", repo_s, "copy", ".", "--stdout"], None, &repo);
	assert_eq!(code(&out_dot), 0, "{}", text(&out_dot.stderr));
	let stdout_dot = text(&out_dot.stdout);
	assert!(stdout_dot.contains("// file: file.txt"), "{stdout_dot}");
	assert!(!stdout_dot.contains(".git"), "{stdout_dot}");
	assert!(!stdout_dot.contains("nested.txt"), "{stdout_dot}");
}

#[test]
fn copy_paths_empty_result_exits_one_and_leaves_clipboard_untouched() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	let empty_sub = repo.join("empty_sub");
	fs::create_dir_all(&empty_sub).unwrap();

	let repo_s = repo.to_str().unwrap();
	let sub_s = empty_sub.to_str().unwrap();

	let out = snip(&["--repo", repo_s, "copy", sub_s, "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	let stderr = text(&out.stderr);
	assert!(stderr.contains("No files selected."), "{stderr}");
	assert!(out.stdout.is_empty(), "stdout must be empty");
}

#[test]
fn copy_paths_missing_path_exits_one() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();

	let repo_s = repo.to_str().unwrap();
	let missing = repo.join("typo_nonexistent_file.txt");
	let missing_s = missing.to_str().unwrap();

	let out = snip(&["--repo", repo_s, "copy", missing_s, "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	let stderr = text(&out.stderr);
	assert!(
		stderr.contains("typo_nonexistent_file.txt"),
		"stderr should mention missing path: {stderr}"
	);
}

#[test]
fn copy_paths_outside_repo_exits_one() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&repo).unwrap();
	fs::create_dir_all(&outside).unwrap();
	let outside_file = outside.join("outside.txt");
	fs::write(&outside_file, "content").unwrap();

	let repo_s = repo.to_str().unwrap();
	let outside_s = outside_file.to_str().unwrap();

	let out = snip(&["--repo", repo_s, "copy", outside_s, "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	let stderr = text(&out.stderr);
	assert!(
		stderr.contains("outside root"),
		"stderr should mention outside root: {stderr}"
	);
}

#[test]
fn copy_paths_relative_path_resolves_against_cwd() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	let sub = repo.join("subdir");
	fs::create_dir_all(&sub).unwrap();
	fs::write(sub.join("target.txt"), "sub target content\n").unwrap();

	let repo_s = repo.to_str().unwrap();
	let out = snip_in_dir(
		&["--repo", repo_s, "copy", "target.txt", "--stdout"],
		None,
		&sub,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	assert!(stdout.contains("// file: subdir/target.txt"), "{stdout}");
	assert!(stdout.contains("sub target content"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn copy_paths_labels_follow_the_spelled_root_and_dir_symlinks() {
	let tmp = tempfile::tempdir().unwrap();
	let real = tmp.path().join("real");
	fs::create_dir_all(real.join("sub")).unwrap();
	fs::write(real.join("sub/s.txt"), "hello from sub\n").unwrap();
	std::os::unix::fs::symlink("sub", real.join("linkdir")).unwrap();

	let link = tmp.path().join("link");
	std::os::unix::fs::symlink(&real, &link).unwrap();

	let link_s = link.to_str().unwrap();
	let file_arg = link.join("linkdir/s.txt");
	let file_s = file_arg.to_str().unwrap();

	let out1 = snip(&["--repo", link_s, "copy", file_s, "--stdout"], None);
	assert_eq!(code(&out1), 0, "{}", text(&out1.stderr));
	let stdout1 = text(&out1.stdout);
	assert!(
		stdout1.contains("// clipcode-root: link"),
		"missing clipcode-root link: {stdout1}"
	);
	assert!(
		stdout1.contains("// file: linkdir/s.txt"),
		"missing file linkdir/s.txt: {stdout1}"
	);
	assert!(
		stdout1.contains("hello from sub"),
		"missing body: {stdout1}"
	);

	let dir_arg = link.join("linkdir");
	let dir_s = dir_arg.to_str().unwrap();
	let out2 = snip(&["--repo", link_s, "copy", dir_s, "--stdout"], None);
	assert_eq!(code(&out2), 0, "{}", text(&out2.stderr));
	let stdout2 = text(&out2.stdout);
	assert!(
		stdout2.contains("// clipcode-root: link"),
		"missing clipcode-root link: {stdout2}"
	);
	assert!(
		stdout2.contains("// file: linkdir/s.txt"),
		"missing file linkdir/s.txt: {stdout2}"
	);
	assert!(
		stdout2.contains("hello from sub"),
		"missing body: {stdout2}"
	);
}

#[test]
fn copy_paths_large_stdout_payload_does_not_deadlock() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	let large_file = repo.join("large.txt");
	let content = "a".repeat(1_500_000);
	fs::write(&large_file, &content).unwrap();

	let repo_s = repo.to_str().unwrap();
	let large_s = large_file.to_str().unwrap();
	let out = snip_with_timeout_in_dir(
		&[
			"--settings",
			r#"{"maxFileSizeKB": 2048}"#,
			"--repo",
			repo_s,
			"copy",
			large_s,
			"--stdout",
		],
		None,
		None,
		std::time::Duration::from_secs(60),
		"snip timed out (deadlock on large stdout)",
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(
		out.stdout.len() >= 1_500_000,
		"stdout was truncated or too small: {}",
		out.stdout.len()
	);
}

#[cfg(unix)]
#[test]
fn copy_paths_absolute_exclude_filter_rule_with_symlink_repo() {
	let tmp = tempfile::tempdir().unwrap();
	let probe = tmp.path();
	let real = probe.join("real");
	let sub = real.join("sub");
	let inner = sub.join("inner");
	fs::create_dir_all(&inner).unwrap();
	fs::write(real.join("a.txt"), "hello a\n").unwrap();
	fs::write(sub.join("s.txt"), "hello s\n").unwrap();
	fs::write(inner.join("i.txt"), "hello i\n").unwrap();
	let link = probe.join("link");
	std::os::unix::fs::symlink(&real, &link).unwrap();

	let rule_path = link.join("sub");
	let settings_json = format!(
		r#"{{"useFilters":true,"filterRules":[{{"type":"PATH","action":"EXCLUDE","value":"{}","enabled":true}}]}}"#,
		rule_path.display()
	);

	let link_s = link.to_str().unwrap();
	let a_s = link.join("a.txt");
	let sub_s = link.join("sub");
	let out = snip_with_timeout_in_dir(
		&[
			"--settings",
			&settings_json,
			"--repo",
			link_s,
			"copy",
			a_s.to_str().unwrap(),
			sub_s.to_str().unwrap(),
			"--stdout",
		],
		None,
		None,
		std::time::Duration::from_secs(60),
		"snip timed out (absolute exclude rule with symlinked repo)",
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout_s = text(&out.stdout);
	assert!(
		stdout_s.contains("a.txt"),
		"expected a.txt in stdout: {stdout_s}"
	);
	assert!(
		!stdout_s.contains("s.txt"),
		"sub/s.txt leaked into stdout: {stdout_s}"
	);
	assert!(
		!stdout_s.contains("i.txt"),
		"sub/inner/i.txt leaked into stdout: {stdout_s}"
	);
}

#[cfg(unix)]
#[test]
fn copy_paths_absolute_include_filter_rule_with_symlink_repo() {
	let tmp = tempfile::tempdir().unwrap();
	let probe = tmp.path();
	let real = probe.join("real");
	let sub = real.join("sub");
	let inner = sub.join("inner");
	fs::create_dir_all(&inner).unwrap();
	fs::write(real.join("a.txt"), "hello a\n").unwrap();
	fs::write(sub.join("s.txt"), "hello s\n").unwrap();
	fs::write(inner.join("i.txt"), "hello i\n").unwrap();
	let link = probe.join("link");
	std::os::unix::fs::symlink(&real, &link).unwrap();

	let rule_path = link.join("sub");
	let settings_json = format!(
		r#"{{"useFilters":true,"filterRules":[{{"type":"PATH","action":"INCLUDE","value":"{}","enabled":true}}]}}"#,
		rule_path.display()
	);

	let link_s = link.to_str().unwrap();
	let a_s = link.join("a.txt");
	let sub_s = link.join("sub");
	let out = snip_with_timeout_in_dir(
		&[
			"--settings",
			&settings_json,
			"--repo",
			link_s,
			"copy",
			a_s.to_str().unwrap(),
			sub_s.to_str().unwrap(),
			"--stdout",
		],
		None,
		None,
		std::time::Duration::from_secs(60),
		"snip timed out (absolute include rule with symlinked repo)",
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout_s = text(&out.stdout);
	assert!(
		!stdout_s.contains("a.txt"),
		"a.txt should be excluded: {stdout_s}"
	);
	assert!(
		stdout_s.contains("s.txt"),
		"sub/s.txt missing from stdout: {stdout_s}"
	);
	assert!(
		stdout_s.contains("i.txt"),
		"sub/inner/i.txt missing from stdout: {stdout_s}"
	);
}

#[test]
fn copy_paths_batched_expansion_avoids_walking_large_tree() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	for i in 0..30 {
		fs::write(
			repo.join(format!("root_{i:02}.txt")),
			format!("root content {i}\n"),
		)
		.unwrap();
	}
	let big = repo.join("big");
	fs::create_dir_all(&big).unwrap();
	for i in 0..5000 {
		fs::write(big.join(format!("sub_{i:04}.txt")), "b\n").unwrap();
	}

	let repo_s = repo.to_str().unwrap();
	let out = snip_with_timeout_in_dir(
		&["--repo", repo_s, "copy", repo_s, "--stdout"],
		None,
		None,
		std::time::Duration::from_secs(60),
		"snip timed out walking large tree",
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(
		text(&out.stderr).contains("30 file(s) copied"),
		"expected '30 file(s) copied' in stderr: {}",
		text(&out.stderr)
	);
}
