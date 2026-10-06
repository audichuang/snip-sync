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

	// A change outside the subdir is not copied; stderr reports it.
	fs::write(repo.join("outside.txt"), "outside\n").unwrap();
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
	assert!(payload.contains("[MODIFIED] a.txt"), "{payload}");
	assert!(!payload.contains("outside.txt"), "{payload}");
	assert!(
		text(&out.stderr).contains("1 change(s) outside --repo not copied."),
		"{}",
		text(&out.stderr)
	);
	assert!(text(&out.stderr).contains("1 Git file(s) copied."));

	// A commit source copies the entire commit relative to the repo toplevel,
	// even when --repo points to a subdirectory.
	fs::create_dir_all(repo.join("other")).unwrap();
	fs::write(repo.join("sub/a.txt"), "three\n").unwrap();
	fs::write(repo.join("other/b.txt"), "other b\n").unwrap();
	commit(&repo, "touch both", "2020-01-02T00:00:00+00:00");
	let out = snip(
		&[
			"--repo",
			sub.to_str().unwrap(),
			"copy",
			"--commit",
			"HEAD",
			"--stdout",
		],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let payload = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(payload.contains("// clipcode-root: r"), "{payload}");
	assert!(payload.contains("sub/a.txt"), "{payload}");
	assert!(payload.contains("other/b.txt"), "{payload}");
	assert!(!stderr.contains("outside --repo not copied"), "{stderr}");
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
		&[
			"--repo",
			dst_s,
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
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

#[cfg(unix)]
#[test]
fn copy_paths_refuses_git_dir_symlinks() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);
	fs::write(repo.join("file.txt"), "hello").unwrap();
	commit(&repo, "initial", "2024-01-01T00:00:00+00:00");

	let link = repo.join("link_to_git");
	let link2 = repo.join("link_to_refs");
	std::os::unix::fs::symlink(".git", &link).unwrap();
	std::os::unix::fs::symlink(".git/refs", &link2).unwrap();

	let repo_s = repo.to_str().unwrap();
	let link_s = link.to_str().unwrap();
	let link2_s = link2.to_str().unwrap();
	let file_s = repo.join("file.txt");
	let file_s = file_s.to_str().unwrap();

	let out = snip(&["--repo", repo_s, "copy", link_s, "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	let stderr = text(&out.stderr);
	assert!(stderr.contains("No files selected"), "{stderr}");

	let out2 = snip(&["--repo", repo_s, "copy", link2_s, "--stdout"], None);
	assert_eq!(code(&out2), 1, "{}", text(&out2.stderr));
	let stderr2 = text(&out2.stderr);
	assert!(stderr2.contains("No files selected"), "{stderr2}");

	let out_sibling = snip(
		&["--repo", repo_s, "copy", link_s, file_s, "--stdout"],
		None,
	);
	assert_eq!(code(&out_sibling), 0, "{}", text(&out_sibling.stderr));
	let stdout = text(&out_sibling.stdout);
	assert!(stdout.contains("// file: file.txt"), "{stdout}");
	assert!(!stdout.contains(".git"), "{stdout}");
	assert!(!stdout.contains("link_to_git"), "{stdout}");
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

#[test]
fn copy_paths_repo_with_dot_dot_normalizes_root_and_labels() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	let b = repo.join("b");
	let sub = repo.join("sub");
	fs::create_dir_all(&b).unwrap();
	fs::create_dir_all(&sub).unwrap();
	init_repo(&repo);
	let file = b.join("b01.txt");
	fs::write(&file, "content of b01\n").unwrap();

	let repo_arg = sub.join("..");
	let repo_s = repo_arg.to_str().unwrap();
	let out = snip_in_dir(
		&["--repo", repo_s, "copy", "b/b01.txt", "--stdout"],
		None,
		&repo,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	assert!(
		stdout.contains("// clipcode-root: repo"),
		"missing clipcode-root repo: {stdout}"
	);
	assert!(
		stdout.contains("// file: b/b01.txt"),
		"missing relative file label: {stdout}"
	);
	assert!(
		!stdout.contains(&format!("// file: {}", file.display())),
		"unexpected absolute path label: {stdout}"
	);
	for line in stdout.lines() {
		if let Some(path) = line.strip_prefix("// file: ") {
			assert!(
				!Path::new(path).is_absolute(),
				"label must not be absolute: {path}"
			);
		}
	}
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

#[test]
fn git_copy_byte_parity_old_vs_new_including_symlinks() {
	use snip_core::gitsrc::{collect_payload, Git, GitSource};
	use snip_core::settings::Settings;

	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	// Setup history for commit and range
	fs::write(repo.join("base.txt"), "base content\n").unwrap();
	fs::write(repo.join("mod.txt"), "initial mod\n").unwrap();
	fs::write(repo.join("del.txt"), "del content\n").unwrap();
	fs::write(repo.join("bin.dat"), b"commit A bin\0content\n").unwrap();
	commit(&repo, "commit A", "2020-01-01T00:00:00+00:00");
	let sha_a = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

	fs::write(repo.join("mod.txt"), "modified in B\n").unwrap();
	fs::write(repo.join("added.txt"), "added in B\n").unwrap();
	fs::write(repo.join("bin.dat"), b"commit B bin\0content\n").unwrap();
	git(&repo, &["rm", "del.txt"]);
	commit(&repo, "commit B", "2020-01-01T00:01:00+00:00");
	let sha_b = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

	let check_sources = |repo_path: &Path| {
		let git_repo = Git::open(repo_path).unwrap();
		let settings = Settings::default();
		let repo_str = repo_path.to_str().unwrap();

		// 1. --commit HEAD
		let out = snip(
			&["--repo", repo_str, "copy", "--commit", "HEAD", "--stdout"],
			None,
		);
		assert_eq!(code(&out), 0, "{}", text(&out.stderr));
		let legacy_commit = collect_payload(
			&git_repo,
			&GitSource::Commit("HEAD".into()),
			&[repo_path],
			&settings,
		)
		.unwrap();
		assert_eq!(text(&out.stdout), legacy_commit.payload);
		assert!(!text(&out.stdout).contains("bin.dat"));

		// 2. --range sha_a..sha_b (includes deleted file del.txt reading its base content)
		let range_arg = format!("{sha_a}..{sha_b}");
		let out = snip(
			&[
				"--repo", repo_str, "copy", "--range", &range_arg, "--stdout",
			],
			None,
		);
		assert_eq!(code(&out), 0, "{}", text(&out.stderr));
		let legacy_range = collect_payload(
			&git_repo,
			&GitSource::Range(sha_a.clone(), sha_b.clone()),
			&[repo_path],
			&settings,
		)
		.unwrap();
		assert_eq!(text(&out.stdout), legacy_range.payload);
		assert!(!text(&out.stdout).contains("bin.dat"));
	};

	check_sources(&repo);

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let symlink_repo = symlink_dir.path().join("link_repo");
		std::os::unix::fs::symlink(&repo, &symlink_repo).unwrap();
		check_sources(&symlink_repo);
	}

	// Now test --working: modified + untracked + deleted + modified binary
	fs::write(repo.join("mod.txt"), "working mod\n").unwrap();
	fs::write(repo.join("untracked.txt"), "untracked content\n").unwrap();
	fs::write(repo.join("bin.dat"), b"working bin\0content\n").unwrap();
	fs::remove_file(repo.join("added.txt")).unwrap();

	let check_working = |repo_path: &Path| {
		let git_repo = Git::open(repo_path).unwrap();
		let settings = Settings::default();
		let repo_str = repo_path.to_str().unwrap();

		let out =
			snip(&["--repo", repo_str, "copy", "--working", "--stdout"], None);
		assert_eq!(code(&out), 0, "{}", text(&out.stderr));
		let legacy_working = collect_payload(
			&git_repo,
			&GitSource::Working,
			&[repo_path],
			&settings,
		)
		.unwrap();
		assert_eq!(text(&out.stdout), legacy_working.payload);
		assert!(!text(&out.stdout).contains("bin.dat"));
	};

	check_working(&repo);

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let symlink_repo = symlink_dir.path().join("link_repo_work");
		std::os::unix::fs::symlink(&repo, &symlink_repo).unwrap();
		check_working(&symlink_repo);
	}

	// Now test --staged: modified + added + deleted + unstaged binary
	git(&repo, &["checkout", "--", "added.txt"]);
	fs::remove_file(repo.join("untracked.txt")).unwrap();
	fs::write(repo.join("staged_new.txt"), "staged new content\n").unwrap();
	git(&repo, &["add", "staged_new.txt"]);
	git(&repo, &["add", "mod.txt"]);
	git(&repo, &["rm", "base.txt"]);

	let check_staged = |repo_path: &Path| {
		let git_repo = Git::open(repo_path).unwrap();
		let settings = Settings::default();
		let repo_str = repo_path.to_str().unwrap();

		let out =
			snip(&["--repo", repo_str, "copy", "--staged", "--stdout"], None);
		assert_eq!(code(&out), 0, "{}", text(&out.stderr));
		let legacy_staged = collect_payload(
			&git_repo,
			&GitSource::Staged,
			&[repo_path],
			&settings,
		)
		.unwrap();
		assert_eq!(text(&out.stdout), legacy_staged.payload);
		assert!(!text(&out.stdout).contains("bin.dat"));
	};

	check_staged(&repo);

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let symlink_repo = symlink_dir.path().join("link_repo_stage");
		std::os::unix::fs::symlink(&repo, &symlink_repo).unwrap();
		check_staged(&symlink_repo);
	}
}

#[test]
fn git_copy_oversize_binary_dropped_like_old_engine() {
	use snip_core::gitsrc::{collect_payload, Git, GitSource};
	use snip_core::settings::Settings;

	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("oversize_repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	let repo_str = repo.to_str().unwrap();
	let git_repo = Git::open(&repo).unwrap();
	let settings = Settings::default();

	// Initial commit so HEAD has a parent
	fs::write(repo.join("base.txt"), "base\n").unwrap();
	commit(&repo, "initial", "2020-01-01T00:00:00+00:00");

	// Commit img.bin containing 700_005 bytes with NULs; also a commit with a changed small text file.
	let mut img_data = vec![b'x'; 700_005];
	img_data[10] = 0; // contains NUL
	fs::write(repo.join("img.bin"), &img_data).unwrap();
	fs::write(repo.join("text.txt"), "small text\n").unwrap();
	commit(
		&repo,
		"commit oversize binary and text",
		"2020-01-01T00:01:00+00:00",
	);

	// (a) copy --commit HEAD --stdout where the commit changes img.bin plus a small text file:
	// stdout == collect_payload payload, and must not contain "img.bin" nor "File skipped".
	let out = snip(
		&["--repo", repo_str, "copy", "--commit", "HEAD", "--stdout"],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let legacy_commit = collect_payload(
		&git_repo,
		&GitSource::Commit("HEAD".into()),
		&[&repo],
		&settings,
	)
	.unwrap();
	assert_eq!(text(&out.stdout), legacy_commit.payload);
	assert!(!text(&out.stdout).contains("img.bin"));
	assert!(!text(&out.stdout).contains("File skipped"));

	// (b) working tree: modify img.bin (oversize binary) and ALSO a text file:
	// copy --working --stdout payload == collect_payload and contains no "img.bin".
	img_data[20] = 1;
	fs::write(repo.join("img.bin"), &img_data).unwrap();
	fs::write(repo.join("text.txt"), "modified small text in working\n")
		.unwrap();

	let out =
		snip(&["--repo", repo_str, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let legacy_working =
		collect_payload(&git_repo, &GitSource::Working, &[&repo], &settings)
			.unwrap();
	assert_eq!(text(&out.stdout), legacy_working.payload);
	assert!(!text(&out.stdout).contains("img.bin"));

	// (c) a repo state where the oversize binary is the ONLY working change:
	// copy --working exits 1 with "No Git changes found to copy." and the clipboard is untouched
	// (use --stdout style like other tests; assert stdout empty).
	git(&repo, &["checkout", "--", "text.txt"]);
	let out =
		snip(&["--repo", repo_str, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	assert!(out.stdout.is_empty());
	assert!(
		text(&out.stderr).contains("No Git changes found to copy."),
		"expected 'No Git changes found to copy.' in stderr: {}",
		text(&out.stderr)
	);

	// Also an oversize non-UTF-8 (e.g. 700_005 bytes of 0xFF... without NUL) variant for --working.
	let non_utf8_data = vec![0xffu8; 700_005];
	fs::write(repo.join("img.bin"), &non_utf8_data).unwrap();
	let out =
		snip(&["--repo", repo_str, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	assert!(out.stdout.is_empty());
	assert!(
		text(&out.stderr).contains("No Git changes found to copy."),
		"expected 'No Git changes found to copy.' in stderr: {}",
		text(&out.stderr)
	);

	// (d) with --settings '{"maxFileSizeKB":100000}' a 33 MiB binary changed in --working
	// is dropped and the text file is still copied, exit 0 (use a sparse-ish write of repeated bytes; keep it fast).
	fs::write(repo.join("text.txt"), "text for 33 MiB test\n").unwrap();
	{
		use std::io::Write;
		let bin_chunk = vec![0u8; 1024 * 1024];
		let mut f = fs::File::create(repo.join("img.bin")).unwrap();
		for _ in 0..33 {
			f.write_all(&bin_chunk).unwrap();
		}
	}
	let out = snip(
		&[
			"--repo",
			repo_str,
			"copy",
			"--working",
			"--stdout",
			"--settings",
			"{\"maxFileSizeKB\":100000}",
		],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	assert!(!text(&out.stdout).contains("img.bin"));
	assert!(text(&out.stdout).contains("text for 33 MiB test"));
}

#[test]
fn git_copy_memory_bounded_huge_file_and_many_files() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("bounded_repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);
	fs::write(repo.join("init.txt"), "init\n").unwrap();
	commit(&repo, "init", "2020-01-01T00:00:00+00:00");
	let repo_s = repo.to_str().unwrap();

	// (1) Single huge changed file (> 33 MiB, untracked) with maxFileSizeKB raised
	let chunk = "A".repeat(1024 * 1024);
	let huge_path = repo.join("huge.txt");
	{
		let mut f = fs::File::create(&huge_path).unwrap();
		for _ in 0..34 {
			f.write_all(chunk.as_bytes()).unwrap();
		}
	}
	let out = snip(
		&[
			"--repo",
			repo_s,
			"copy",
			"--working",
			"--stdout",
			"--settings",
			"{\"maxFileSizeKB\": 1000000}",
		],
		None,
	);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	assert!(
		out.stdout.is_empty(),
		"stdout must be empty on limit failure"
	);
	assert!(
		text(&out.stderr).contains("33554432"),
		"stderr must mention limit: {}",
		text(&out.stderr)
	);

	// Remove huge file before (2)
	fs::remove_file(huge_path).unwrap();

	// (2) Many untracked files (40 files of 1 MiB each = 40 MiB > 32 MiB)
	for i in 0..40 {
		fs::write(repo.join(format!("file_{i:02}.txt")), &chunk).unwrap();
	}
	let out = snip(
		&[
			"--repo",
			repo_s,
			"copy",
			"--working",
			"--stdout",
			"--settings",
			"{\"maxFileSizeKB\": 1000000, \"setMaxFileCount\": false}",
		],
		None,
	);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	assert!(
		out.stdout.is_empty(),
		"stdout must be empty on limit failure"
	);
	assert!(
		text(&out.stderr).contains("33554432"),
		"stderr must mention limit: {}",
		text(&out.stderr)
	);
}

#[test]
fn git_copy_working_empty_changes_exits_1() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("clean_repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);
	fs::write(repo.join("file.txt"), "hello\n").unwrap();
	commit(&repo, "init", "2020-01-01T00:00:00+00:00");
	let repo_s = repo.to_str().unwrap();

	let out = snip(&["--repo", repo_s, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	assert!(out.stdout.is_empty());
	assert!(
		text(&out.stderr).contains("No Git changes found to copy."),
		"{}",
		text(&out.stderr)
	);
}

#[cfg(unix)]
#[test]
fn copy_working_skips_dangling_symlink() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("r");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	fs::write(repo.join("a.txt"), "committed\n").unwrap();
	commit(&repo, "init", "2020-01-01T00:00:00+00:00");
	fs::write(repo.join("a.txt"), "modified\n").unwrap();

	std::os::unix::fs::symlink("does-not-exist", repo.join("dangling"))
		.unwrap();

	let repo_s = repo.to_str().unwrap();
	let out = snip(&["--repo", repo_s, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(stdout.contains("[MODIFIED] a.txt"), "{stdout}");
	assert!(!stdout.contains("dangling"), "{stdout}");
	assert!(
		stderr.contains("1 skipped: not UTF-8 text or unreadable"),
		"{stderr}"
	);

	// Also test a tracked symlink retargeted to a missing path
	fs::remove_file(repo.join("dangling")).unwrap();
	fs::write(repo.join("target.txt"), "target\n").unwrap();
	std::os::unix::fs::symlink("target.txt", repo.join("tracked_symlink"))
		.unwrap();
	commit(&repo, "add tracked symlink", "2020-01-02T00:00:00+00:00");

	fs::write(repo.join("a.txt"), "modified again\n").unwrap();
	fs::remove_file(repo.join("tracked_symlink")).unwrap();
	std::os::unix::fs::symlink("missing-target", repo.join("tracked_symlink"))
		.unwrap();

	let out = snip(&["--repo", repo_s, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(stdout.contains("[MODIFIED] a.txt"), "{stdout}");
	assert!(!stdout.contains("tracked_symlink"), "{stdout}");
	assert!(
		stderr.contains("1 skipped: not UTF-8 text or unreadable"),
		"{stderr}"
	);
}

#[cfg(unix)]
#[test]
fn copy_working_skips_symlink_outside_repo() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("r");
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&repo).unwrap();
	fs::create_dir_all(&outside).unwrap();
	init_repo(&repo);

	fs::write(repo.join("a.txt"), "committed\n").unwrap();
	commit(&repo, "init", "2020-01-01T00:00:00+00:00");
	fs::write(repo.join("a.txt"), "modified\n").unwrap();

	let outside_file = outside.join("secret.txt");
	fs::write(&outside_file, "SECRET-OUTSIDE\n").unwrap();
	std::os::unix::fs::symlink(&outside_file, repo.join("leak")).unwrap();

	let repo_s = repo.to_str().unwrap();
	let out = snip(&["--repo", repo_s, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(stdout.contains("[MODIFIED] a.txt"), "{stdout}");
	assert!(stdout.contains("modified"), "{stdout}");
	assert!(!stdout.contains("SECRET-OUTSIDE"), "{stdout}");
	assert!(!stdout.contains("leak"), "{stdout}");
	assert!(
		stderr.contains("1 skipped: not UTF-8 text or unreadable"),
		"{stderr}"
	);
}

#[test]
fn copy_paths_oversize_binary_keeps_size_skipped_marker() {
	let tmp = tempfile::tempdir().unwrap();
	let folder = tmp.path().join("folder");
	fs::create_dir_all(&folder).unwrap();

	let mut bin_data = vec![b'x'; 700_005];
	bin_data[10] = 0; // contains NUL
	fs::write(folder.join("big.bin"), &bin_data).unwrap();
	fs::write(folder.join("small.txt"), "small content\n").unwrap();

	let folder_s = folder.to_str().unwrap();
	let out = snip(&["--repo", folder_s, "copy", folder_s, "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));
	let stdout = text(&out.stdout);
	let stderr = text(&out.stderr);
	assert!(stdout.contains("// file: big.bin"), "{stdout}");
	assert!(stdout.contains("size exceeds limit"), "{stdout}");
	assert!(stderr.contains("1 skipped: size exceeded"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn copy_working_skips_symlink_to_directory() {
	use snip_core::gitsrc::{collect_payload, Git, GitSource};
	use snip_core::settings::Settings;

	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	let v1_dir = repo.join("v1");
	fs::create_dir_all(&v1_dir).unwrap();
	fs::write(v1_dir.join("f.txt"), "v1 content\n").unwrap();
	fs::write(repo.join("tracked.txt"), "tracked base\n").unwrap();
	std::os::unix::fs::symlink("v1", repo.join("latest")).unwrap();
	commit(&repo, "initial", "2020-01-01T00:00:00+00:00");

	// Repoint latest -> v2 (create v2 dir) and modify a tracked text file
	let v2_dir = repo.join("v2");
	fs::create_dir_all(&v2_dir).unwrap();
	let latest_path = repo.join("latest");
	fs::remove_file(&latest_path).unwrap();
	std::os::unix::fs::symlink("v2", &latest_path).unwrap();

	fs::write(repo.join("tracked.txt"), "tracked modified\n").unwrap();

	let repo_s = repo.to_str().unwrap();
	let out = snip(&["--repo", repo_s, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));

	let git_repo = Git::open(&repo).unwrap();
	let settings = Settings::default();
	let legacy_working =
		collect_payload(&git_repo, &GitSource::Working, &[&repo], &settings)
			.unwrap();

	let stdout = text(&out.stdout);
	assert_eq!(stdout, legacy_working.payload);
	assert!(stdout.contains("tracked modified"), "{stdout}");
	assert!(!stdout.contains("latest"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn copy_working_skips_unreadable_file() {
	use snip_core::gitsrc::{collect_payload, Git, GitSource};
	use snip_core::settings::Settings;
	use std::os::unix::fs::PermissionsExt;

	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	fs::write(repo.join("tracked.txt"), "tracked base\n").unwrap();
	commit(&repo, "initial", "2020-01-01T00:00:00+00:00");

	fs::write(repo.join("tracked.txt"), "tracked modified\n").unwrap();
	let unreadable = repo.join("unreadable.txt");
	fs::write(&unreadable, "secret unreadable\n").unwrap();
	fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
		.unwrap();

	if fs::read(&unreadable).is_ok() {
		assert!(
			std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
			"running as root: cannot test unreadable file permissions"
		);
		let _ =
			fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644));
		return;
	}

	let repo_s = repo.to_str().unwrap();
	let out = snip(&["--repo", repo_s, "copy", "--working", "--stdout"], None);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));

	let git_repo = Git::open(&repo).unwrap();
	let settings = Settings::default();
	let legacy_working =
		collect_payload(&git_repo, &GitSource::Working, &[&repo], &settings)
			.unwrap();

	let stdout = text(&out.stdout);
	assert_eq!(stdout, legacy_working.payload);
	assert!(stdout.contains("tracked modified"), "{stdout}");
	assert!(!stdout.contains("secret unreadable"), "{stdout}");

	let _ = fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644));
}

#[test]
fn copy_commit_with_subrepo_matches_workspace_relative_filter() {
	use snip_core::gitsrc::{collect_payload, Git, GitSource};
	use snip_core::settings::Settings;

	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	let d_e = repo.join("d/e");
	fs::create_dir_all(&d_e).unwrap();
	fs::write(d_e.join("x.txt"), "initial d/e/x\n").unwrap();
	fs::write(repo.join("outside.txt"), "initial outside\n").unwrap();
	commit(&repo, "initial", "2020-01-01T00:00:00+00:00");

	fs::write(d_e.join("x.txt"), "modified d/e/x\n").unwrap();
	fs::write(repo.join("outside.txt"), "modified outside\n").unwrap();
	commit(
		&repo,
		"change d/e/x and outside",
		"2020-01-02T00:00:00+00:00",
	);

	let sub_d = repo.join("d");
	let sub_d_s = sub_d.to_str().unwrap();
	let settings_str = r#"{"useFilters":true,"useIncludeFilters":true,"filterRules":[{"type":"PATH","action":"INCLUDE","value":"e","enabled":true}]}"#;
	let settings: Settings = serde_json::from_str(settings_str).unwrap();

	let out = snip(
		&[
			"--repo",
			sub_d_s,
			"copy",
			"--commit",
			"HEAD",
			"--stdout",
			"--settings",
			settings_str,
		],
		None,
	);
	assert_eq!(code(&out), 0, "{}", text(&out.stderr));

	let git_repo = Git::open(&repo).unwrap();
	let legacy = collect_payload(
		&git_repo,
		&GitSource::Commit("HEAD".into()),
		&[repo.join("d")],
		&settings,
	)
	.unwrap();

	let stdout = text(&out.stdout);
	assert_eq!(stdout, legacy.payload);
	assert!(stdout.contains("d/e/x.txt"), "{stdout}");

	let settings_repo_rel = r#"{"useFilters":true,"useIncludeFilters":true,"filterRules":[{"type":"PATH","action":"INCLUDE","value":"d/e","enabled":true}]}"#;
	let out_repo_rel = snip(
		&[
			"--repo",
			sub_d_s,
			"copy",
			"--commit",
			"HEAD",
			"--stdout",
			"--settings",
			settings_repo_rel,
		],
		None,
	);
	assert_eq!(code(&out_repo_rel), 1, "{}", text(&out_repo_rel.stderr));
	assert!(text(&out_repo_rel.stderr).contains("No source copied."));
}

#[test]
fn paste_f1_case_alias_existing_path() {
	let tmp = tempfile::tempdir().unwrap();
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&dst).unwrap();
	let probe = dst.join("probe_x");
	let probe_upper = dst.join("PROBE_X");
	fs::write(&probe, "x").unwrap();
	let case_insensitive = probe_upper.exists();
	let _ = fs::remove_file(&probe);

	fs::write(dst.join("a.txt"), "original a\n").unwrap();
	let payload = "// file: A.txt\nnew A content\n// file: [DELETED] a.txt\n";
	let dst_s = dst.to_str().unwrap();

	let out = snip(
		&[
			"--repo",
			dst_s,
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
		Some(payload.as_bytes()),
	);

	if case_insensitive {
		assert_eq!(code(&out), 1, "{}", text(&out.stderr));
		let err = text(&out.stderr);
		assert!(
			err.contains("target collision"),
			"expected error mentioning 'target collision', got: {err}"
		);
		// Count the message, not the path: Windows temp dirs come back as 8.3
		// short names while the CLI prints the long canonical spelling.
		assert_eq!(
			err.matches("multiple operations target").count(),
			1,
			"path must occur once in stderr, got: {err}"
		);
		assert!(
			!err.contains("identity"),
			"identity must not occur in stderr, got: {err}"
		);
		assert!(err.contains("previous was 'create A.txt'"));
		assert!(err.contains("current is 'delete a.txt'"));
		assert_eq!(
			fs::read_to_string(dst.join("a.txt")).unwrap(),
			"original a\n"
		);
	} else {
		assert_eq!(code(&out), 0, "{}", text(&out.stderr));
		assert_eq!(
			fs::read_to_string(dst.join("A.txt")).unwrap(),
			"new A content"
		);
		assert!(!dst.join("a.txt").exists());
	}
}

#[test]
fn paste_d10_case_alias_both_new() {
	let tmp = tempfile::tempdir().unwrap();
	let probe = tmp.path().join("probe_x");
	let probe_upper = tmp.path().join("PROBE_X");
	fs::write(&probe, "x").unwrap();
	let case_insensitive = probe_upper.exists();
	let _ = fs::remove_file(&probe);

	// Case A: [NEW] B.txt + [NEW] b.txt
	let dst1 = tmp.path().join("dst1");
	fs::create_dir_all(&dst1).unwrap();
	let payload1 = "// file: B.txt\ncontent B\n// file: b.txt\ncontent b\n";
	let out1 = snip(
		&[
			"--repo",
			dst1.to_str().unwrap(),
			"paste",
			"--apply",
			"--stdin",
		],
		Some(payload1.as_bytes()),
	);
	if case_insensitive {
		assert_eq!(code(&out1), 1, "{}", text(&out1.stderr));
		let err = text(&out1.stderr);
		assert!(err.contains("target collision"), "{err}");
		assert_eq!(
			err.matches("multiple operations target").count(),
			1,
			"path must occur once in stderr, got: {err}"
		);
		assert!(
			!err.contains("identity"),
			"identity must not occur in stderr, got: {err}"
		);
		assert!(err.contains("previous was 'create B.txt'"));
		assert!(err.contains("current is 'create b.txt'"));
		assert!(!dst1.join("B.txt").exists());
		assert!(!dst1.join("b.txt").exists());
	} else {
		assert_eq!(code(&out1), 0, "{}", text(&out1.stderr));
		assert_eq!(
			fs::read_to_string(dst1.join("B.txt")).unwrap(),
			"content B"
		);
		assert_eq!(
			fs::read_to_string(dst1.join("b.txt")).unwrap(),
			"content b"
		);
	}

	// Case B: D/x.txt + d/x.txt (D absent)
	let dst2 = tmp.path().join("dst2");
	fs::create_dir_all(&dst2).unwrap();
	let payload2 = "// file: D/x.txt\ncontent 1\n// file: d/x.txt\ncontent 2\n";
	let out2 = snip(
		&[
			"--repo",
			dst2.to_str().unwrap(),
			"paste",
			"--apply",
			"--stdin",
		],
		Some(payload2.as_bytes()),
	);
	if case_insensitive {
		assert_eq!(code(&out2), 1, "{}", text(&out2.stderr));
		let err = text(&out2.stderr);
		assert!(err.contains("target collision"), "{err}");
		assert_eq!(
			err.matches("multiple operations target").count(),
			1,
			"path must occur once in stderr, got: {err}"
		);
		assert!(
			!err.contains("identity"),
			"identity must not occur in stderr, got: {err}"
		);
		assert!(err.contains("previous was 'create D/x.txt'"));
		assert!(err.contains("current is 'create d/x.txt'"));
		assert!(!dst2.join("D").exists());
		assert!(!dst2.join("d").exists());
	} else {
		assert_eq!(code(&out2), 0, "{}", text(&out2.stderr));
		assert_eq!(
			fs::read_to_string(dst2.join("D/x.txt")).unwrap(),
			"content 1"
		);
		assert_eq!(
			fs::read_to_string(dst2.join("d/x.txt")).unwrap(),
			"content 2"
		);
	}
}

#[test]
fn paste_root_internal_absolute_path() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();

	#[cfg(unix)]
	let repo_target = {
		let sym = tmp.path().join("sym_repo");
		std::os::unix::fs::symlink(&repo, &sym).unwrap();
		sym
	};
	#[cfg(not(unix))]
	let repo_target = repo.clone();

	let repo_target_s = repo_target.to_str().unwrap();
	let header_path = repo_target.join("src/c.ts");
	let header_path_s = header_path.to_str().unwrap();
	let payload = format!("// file: {header_path_s}\nconsole.log(42);\n");

	let dry = snip(
		&["--repo", repo_target_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	assert!(
		text(&dry.stdout).contains("create\tsrc/c.ts"),
		"{}",
		text(&dry.stdout)
	);

	let apply = snip(
		&["--repo", repo_target_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&apply), 0, "{}", text(&apply.stderr));
	assert_eq!(
		fs::read_to_string(repo.join("src/c.ts")).unwrap(),
		"console.log(42);"
	);
}

#[test]
fn paste_cross_machine_suffix() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("proj_suffix");
	fs::create_dir_all(&repo).unwrap();
	let repo_s = repo.to_str().unwrap();

	let payload =
		"// file: /Users/bob/proj_suffix/src/a.ts\nexport const a = 1;\n";

	let dry = snip(
		&["--repo", repo_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	assert!(
		text(&dry.stdout).contains("create\tsrc/a.ts"),
		"{}",
		text(&dry.stdout)
	);

	let apply = snip(
		&["--repo", repo_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&apply), 0, "{}", text(&apply.stderr));
	assert_eq!(
		fs::read_to_string(repo.join("src/a.ts")).unwrap(),
		"export const a = 1;"
	);
}

#[test]
fn paste_absolute_deleted_unresolved_skipped() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("my_repo");
	fs::create_dir_all(&repo).unwrap();
	let repo_s = repo.to_str().unwrap();

	let foreign_path = "/foreign_disk/other_proj/src/secret.txt";
	let nested = repo.join("foreign_disk/other_proj/src/secret.txt");
	fs::create_dir_all(nested.parent().unwrap()).unwrap();
	fs::write(&nested, "preserved secret\n").unwrap();

	let payload = format!("// file: [DELETED] {foreign_path}\n");

	let dry = snip(
		&["--repo", repo_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	let dry_out = text(&dry.stdout);
	assert!(
		dry_out.contains(
			"skip\t/foreign_disk/other_proj/src/secret.txt\tUNRESOLVED_PATH"
		),
		"{dry_out}"
	);

	let apply = snip(
		&["--repo", repo_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&apply), 0, "{}", text(&apply.stderr));
	assert!(nested.exists(), "file at nested path must not be deleted");
	assert_eq!(fs::read_to_string(&nested).unwrap(), "preserved secret\n");
}

#[test]
fn paste_commits_overwrite_gate() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	init_repo(&src);
	init_repo(&dst);

	fs::write(src.join("file.txt"), "src v1\n").unwrap();
	commit(&src, "add file", "2024-01-01T00:00:00+00:00");

	fs::write(dst.join("file.txt"), "dst initial\n").unwrap();
	commit(&dst, "initial dst", "2024-01-01T00:00:00+00:00");

	let src_s = src.to_str().unwrap();
	let dst_s = dst.to_str().unwrap();

	let copy_out = snip(
		&["--repo", src_s, "copy", "--commits", "-n", "1", "--stdout"],
		None,
	);
	assert_eq!(code(&copy_out), 0, "{}", text(&copy_out.stderr));
	let payload = copy_out.stdout;

	let head_before = git(&dst, &["rev-parse", "HEAD"]);

	// Apply without --overwrite -> exit 2, unchanged
	let apply_no_flag = snip(
		&["--repo", dst_s, "paste", "--apply", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&apply_no_flag), 2, "{}", text(&apply_no_flag.stderr));
	assert!(
		text(&apply_no_flag.stderr).contains(
			"destination file(s) already exist; commit payloads need --overwrite"
		),
		"{}",
		text(&apply_no_flag.stderr)
	);
	assert_eq!(
		fs::read_to_string(dst.join("file.txt")).unwrap(),
		"dst initial\n"
	);
	assert_eq!(git(&dst, &["rev-parse", "HEAD"]), head_before);

	// Apply with --overwrite -> exit 0, commit created
	let apply_overwrite = snip(
		&[
			"--repo",
			dst_s,
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
		Some(&payload),
	);
	assert_eq!(
		code(&apply_overwrite),
		0,
		"{}",
		text(&apply_overwrite.stderr)
	);
	assert!(text(&apply_overwrite.stdout).contains("Created 1 commit(s)."));
	assert_eq!(
		fs::read_to_string(dst.join("file.txt")).unwrap(),
		"src v1\n"
	);
	assert_ne!(git(&dst, &["rev-parse", "HEAD"]), head_before);

	// A payload whose targets do not exist needs no flag
	let dst2 = tmp.path().join("dst2");
	fs::create_dir_all(&dst2).unwrap();
	init_repo(&dst2);
	fs::write(dst2.join("other.txt"), "other\n").unwrap();
	commit(&dst2, "other commit", "2024-01-01T00:00:00+00:00");
	let dst2_s = dst2.to_str().unwrap();

	let apply_fresh = snip(
		&["--repo", dst2_s, "paste", "--apply", "--stdin"],
		Some(&payload),
	);
	assert_eq!(code(&apply_fresh), 0, "{}", text(&apply_fresh.stderr));
	assert!(text(&apply_fresh.stdout).contains("Created 1 commit(s)."));
	assert_eq!(
		fs::read_to_string(dst2.join("file.txt")).unwrap(),
		"src v1\n"
	);
}

#[test]
fn paste_commits_dry_run_warns_about_overwrite_when_target_exists() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	let dst_absent = tmp.path().join("dst_absent");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	fs::create_dir_all(&dst_absent).unwrap();
	init_repo(&src);
	init_repo(&dst);
	init_repo(&dst_absent);

	fs::write(src.join("file.txt"), "src v1\n").unwrap();
	commit(&src, "add file", "2024-01-01T00:00:00+00:00");

	fs::write(dst.join("file.txt"), "dst initial\n").unwrap();
	commit(&dst, "initial dst", "2024-01-01T00:00:00+00:00");

	let copy_out = snip(
		&[
			"--repo",
			src.to_str().unwrap(),
			"copy",
			"--commits",
			"-n",
			"1",
			"--stdout",
		],
		None,
	);
	assert_eq!(code(&copy_out), 0, "{}", text(&copy_out.stderr));
	let payload = copy_out.stdout;

	// Target absent: exit 0, no hint
	let dry_absent = snip(
		&[
			"--repo",
			dst_absent.to_str().unwrap(),
			"paste",
			"--dry-run",
			"--stdin",
		],
		Some(&payload),
	);
	assert_eq!(code(&dry_absent), 0, "{}", text(&dry_absent.stderr));
	assert!(
		!text(&dry_absent.stderr).contains("destination file(s) already exist"),
		"{}",
		text(&dry_absent.stderr)
	);

	// Target exists: exit 0, prints hint
	let dry_exists = snip(
		&[
			"--repo",
			dst.to_str().unwrap(),
			"paste",
			"--dry-run",
			"--stdin",
		],
		Some(&payload),
	);
	assert_eq!(code(&dry_exists), 0, "{}", text(&dry_exists.stderr));
	assert!(
		text(&dry_exists.stderr).contains(
			"1 destination file(s) already exist; --apply will need --overwrite."
		),
		"{}",
		text(&dry_exists.stderr)
	);

	// Target exists with --overwrite: exit 0, no hint
	let dry_overwrite = snip(
		&[
			"--repo",
			dst.to_str().unwrap(),
			"paste",
			"--dry-run",
			"--overwrite",
			"--stdin",
		],
		Some(&payload),
	);
	assert_eq!(code(&dry_overwrite), 0, "{}", text(&dry_overwrite.stderr));
	assert!(
		!text(&dry_overwrite.stderr)
			.contains("destination file(s) already exist"),
		"{}",
		text(&dry_overwrite.stderr)
	);
}

#[test]
fn paste_commits_two_commits_same_path_dedupes_existing() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	init_repo(&src);
	init_repo(&dst);

	fs::write(src.join("file.txt"), "src v1\n").unwrap();
	commit(&src, "commit 1", "2024-01-01T00:00:00+00:00");
	fs::write(src.join("file.txt"), "src v2\n").unwrap();
	commit(&src, "commit 2", "2024-01-01T00:00:01+00:00");

	fs::write(dst.join("file.txt"), "dst initial\n").unwrap();
	commit(&dst, "initial dst", "2024-01-01T00:00:00+00:00");

	let copy_out = snip(
		&[
			"--repo",
			src.to_str().unwrap(),
			"copy",
			"--commits",
			"-n",
			"2",
			"--stdout",
		],
		None,
	);
	assert_eq!(code(&copy_out), 0, "{}", text(&copy_out.stderr));
	let payload = copy_out.stdout;

	// Dry run with two commits touching same existing file reports 1 destination file
	let dry_out = snip(
		&[
			"--repo",
			dst.to_str().unwrap(),
			"paste",
			"--dry-run",
			"--stdin",
		],
		Some(&payload),
	);
	assert_eq!(code(&dry_out), 0, "{}", text(&dry_out.stderr));
	assert!(
		text(&dry_out.stderr).contains(
			"1 destination file(s) already exist; --apply will need --overwrite."
		),
		"{}",
		text(&dry_out.stderr)
	);
	assert!(
		!text(&dry_out.stderr).contains("2 destination file(s)"),
		"{}",
		text(&dry_out.stderr)
	);

	// Apply refusal with two commits touching same existing file reports 1 destination file
	let apply_out = snip(
		&[
			"--repo",
			dst.to_str().unwrap(),
			"paste",
			"--apply",
			"--stdin",
		],
		Some(&payload),
	);
	assert_eq!(code(&apply_out), 2, "{}", text(&apply_out.stderr));
	assert!(
		text(&apply_out.stderr).contains(
			"1 destination file(s) already exist; commit payloads need --overwrite"
		),
		"{}",
		text(&apply_out.stderr)
	);
	assert!(
		!text(&apply_out.stderr).contains("2 destination file(s)"),
		"{}",
		text(&apply_out.stderr)
	);
}

#[test]
fn paste_commits_disallowed_flags_exit_two() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	init_repo(&src);
	init_repo(&dst);

	fs::write(src.join("foo.txt"), "foo\n").unwrap();
	commit(&src, "add foo", "2024-01-01T00:00:00+00:00");

	fs::write(dst.join("bar.txt"), "bar\n").unwrap();
	commit(&dst, "initial dst", "2024-01-01T00:00:00+00:00");

	let src_s = src.to_str().unwrap();
	let dst_s = dst.to_str().unwrap();

	let copy_out = snip(
		&["--repo", src_s, "copy", "--commits", "-n", "1", "--stdout"],
		None,
	);
	assert_eq!(code(&copy_out), 0, "{}", text(&copy_out.stderr));
	let payload = copy_out.stdout;

	let head_before = git(&dst, &["rev-parse", "HEAD"]);

	let flag_combos = [
		vec!["--apply", "--skip-existing"],
		vec!["--apply", "--adjust-paths"],
		vec!["--dry-run", "--skip-existing"],
		vec!["--dry-run", "--adjust-paths"],
	];

	for flags in &flag_combos {
		let mut args = vec!["--repo", dst_s, "paste"];
		args.extend(flags);
		args.push("--stdin");
		let out = snip(&args, Some(&payload));
		assert_eq!(
			code(&out),
			2,
			"flags {flags:?} did not exit 2: {}",
			text(&out.stderr)
		);
		assert_eq!(
			git(&dst, &["rev-parse", "HEAD"]),
			head_before,
			"HEAD changed under flags {flags:?}"
		);
		assert!(
			!dst.join("foo.txt").exists(),
			"file written under flags {flags:?}"
		);
	}
}

#[test]
fn paste_adjust_paths_strip_and_add() {
	let tmp = tempfile::tempdir().unwrap();

	// 1. Strip scenario:
	// Repo folder named "proj", payload starts with "proj/..."
	let proj = tmp.path().join("proj");
	fs::create_dir_all(&proj).unwrap();
	init_repo(&proj);
	let proj_s = proj.to_str().unwrap();

	let payload_strip =
		"// clipcode-root: workspace\n// file: proj/sub/hello.txt\nhello content\n";

	// Without --adjust-paths: hint on stderr, unadjusted path in dry-run
	let dry_unadjusted = snip(
		&["--repo", proj_s, "paste", "--dry-run", "--stdin"],
		Some(payload_strip.as_bytes()),
	);
	assert_eq!(code(&dry_unadjusted), 0, "{}", text(&dry_unadjusted.stderr));
	let dry_stderr = text(&dry_unadjusted.stderr);
	assert!(
		dry_stderr.contains("These paths look like they belong elsewhere in this folder. Pass --adjust-paths to remove the leading \"proj/\""),
		"{dry_stderr}"
	);
	assert!(
		text(&dry_unadjusted.stdout).contains("create\tproj/sub/hello.txt"),
		"{}",
		text(&dry_unadjusted.stdout)
	);

	// With --adjust-paths: adjusted dry-run
	let dry_adjusted = snip(
		&[
			"--repo",
			proj_s,
			"paste",
			"--dry-run",
			"--adjust-paths",
			"--stdin",
		],
		Some(payload_strip.as_bytes()),
	);
	assert_eq!(code(&dry_adjusted), 0, "{}", text(&dry_adjusted.stderr));
	let dry_adj_stderr = text(&dry_adjusted.stderr);
	assert!(
		dry_adj_stderr
			.contains("Adjusting paths: remove the leading \"proj/\""),
		"{dry_adj_stderr}"
	);
	assert!(
		text(&dry_adjusted.stdout).contains("create\tsub/hello.txt"),
		"{}",
		text(&dry_adjusted.stdout)
	);

	// With --adjust-paths: apply writes at adjusted location
	let apply_strip = snip(
		&[
			"--repo",
			proj_s,
			"paste",
			"--apply",
			"--adjust-paths",
			"--stdin",
		],
		Some(payload_strip.as_bytes()),
	);
	assert_eq!(code(&apply_strip), 0, "{}", text(&apply_strip.stderr));
	assert_eq!(
		fs::read_to_string(proj.join("sub/hello.txt")).unwrap(),
		"hello content"
	);
	assert!(!proj.join("proj").exists());

	// 2. Add scenario:
	// Repo folder named "myrepo" with subfolder "backend", payload relative to backend
	let myrepo = tmp.path().join("myrepo");
	fs::create_dir_all(myrepo.join("backend")).unwrap();
	init_repo(&myrepo);
	let myrepo_s = myrepo.to_str().unwrap();

	let payload_add =
		"// clipcode-root: backend\n// file: src/app.rs\nfn run() {}\n";

	// Without --adjust-paths: hint on stderr, unadjusted path in dry-run
	let dry_unadjusted_add = snip(
		&["--repo", myrepo_s, "paste", "--dry-run", "--stdin"],
		Some(payload_add.as_bytes()),
	);
	assert_eq!(
		code(&dry_unadjusted_add),
		0,
		"{}",
		text(&dry_unadjusted_add.stderr)
	);
	let dry_add_stderr = text(&dry_unadjusted_add.stderr);
	assert!(
		dry_add_stderr.contains("These paths look like they belong elsewhere in this folder. Pass --adjust-paths to place everything under \"backend/\""),
		"{dry_add_stderr}"
	);
	assert!(
		text(&dry_unadjusted_add.stdout).contains("create\tsrc/app.rs"),
		"{}",
		text(&dry_unadjusted_add.stdout)
	);

	// With --adjust-paths: adjusted dry-run
	let dry_adjusted_add = snip(
		&[
			"--repo",
			myrepo_s,
			"paste",
			"--dry-run",
			"--adjust-paths",
			"--stdin",
		],
		Some(payload_add.as_bytes()),
	);
	assert_eq!(
		code(&dry_adjusted_add),
		0,
		"{}",
		text(&dry_adjusted_add.stderr)
	);
	let dry_adj_add_stderr = text(&dry_adjusted_add.stderr);
	assert!(
		dry_adj_add_stderr
			.contains("Adjusting paths: place everything under \"backend/\""),
		"{dry_adj_add_stderr}"
	);
	assert!(
		text(&dry_adjusted_add.stdout).contains("create\tbackend/src/app.rs"),
		"{}",
		text(&dry_adjusted_add.stdout)
	);

	// With --adjust-paths: apply writes at adjusted location
	let apply_add = snip(
		&[
			"--repo",
			myrepo_s,
			"paste",
			"--apply",
			"--adjust-paths",
			"--stdin",
		],
		Some(payload_add.as_bytes()),
	);
	assert_eq!(code(&apply_add), 0, "{}", text(&apply_add.stderr));
	assert_eq!(
		fs::read_to_string(myrepo.join("backend/src/app.rs")).unwrap(),
		"fn run() {}"
	);
	assert!(!myrepo.join("src").exists());
}

#[test]
fn paste_symlink_spelled_repo_round_trip() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src_real");
	let dst = tmp.path().join("dst_real");
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();

	fs::write(src.join("hello.txt"), "hello from symlink\n").unwrap();

	#[cfg(unix)]
	let (src_run, dst_run) = {
		let sym_src = tmp.path().join("sym_src");
		let sym_dst = tmp.path().join("sym_dst");
		std::os::unix::fs::symlink(&src, &sym_src).unwrap();
		std::os::unix::fs::symlink(&dst, &sym_dst).unwrap();
		(sym_src, sym_dst)
	};
	#[cfg(not(unix))]
	let (src_run, dst_run) = (src.clone(), dst.clone());

	let copy_out = snip_in_dir(
		&[
			"--repo",
			src_run.to_str().unwrap(),
			"copy",
			"hello.txt",
			"--stdout",
		],
		None,
		&src_run,
	);
	assert_eq!(code(&copy_out), 0, "{}", text(&copy_out.stderr));

	let paste_out = snip(
		&[
			"--repo",
			dst_run.to_str().unwrap(),
			"paste",
			"--apply",
			"--stdin",
		],
		Some(&copy_out.stdout),
	);
	assert_eq!(code(&paste_out), 0, "{}", text(&paste_out.stderr));
	assert_eq!(
		fs::read_to_string(dst.join("hello.txt")).unwrap(),
		"hello from symlink"
	);
}

#[test]
fn paste_refuses_when_destination_is_directory() {
	let tmp = tempfile::tempdir().unwrap();
	let dst = tmp.path().join("dst");
	fs::create_dir_all(dst.join("build")).unwrap();

	let payload = "// file: build\nhello\n";
	let out = snip(
		&[
			"--repo",
			dst.to_str().unwrap(),
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&out), 1, "{}", text(&out.stderr));
	let err = text(&out.stderr);
	assert!(
		err.contains("refuses to overwrite"),
		"expected stderr to contain 'refuses to overwrite', got: {err}"
	);
	assert!(
		!err.contains("exported"),
		"expected stderr not to contain 'exported', got: {err}"
	);
}

#[test]
fn paste_refuses_git_path_segment() {
	let tmp = tempfile::tempdir().unwrap();
	let repo = tmp.path().join("repo");
	fs::create_dir_all(&repo).unwrap();
	init_repo(&repo);

	let git_config = repo.join(".git").join("config");
	assert!(
		git_config.exists(),
		".git/config must exist after init_repo"
	);
	let original_config = fs::read_to_string(&git_config).unwrap();

	// 1. Dry run with .git/config payload reports UNRESOLVED_PATH and skipped
	let git_payload = "// file: .git/config\n[malicious]\nhacked = true\n";
	let dry = snip(
		&[
			"--repo",
			repo.to_str().unwrap(),
			"paste",
			"--dry-run",
			"--stdin",
		],
		Some(git_payload.as_bytes()),
	);
	assert_eq!(code(&dry), 0, "{}", text(&dry.stderr));
	let dry_stdout = text(&dry.stdout);
	assert!(
		dry_stdout.contains("skip\t.git/config\tUNRESOLVED_PATH"),
		"expected dry-run to report skip with UNRESOLVED_PATH, got: {dry_stdout}"
	);
	assert!(
		text(&dry.stderr).contains("Skipped 1."),
		"expected stderr summary to mention Skipped 1."
	);

	// 2. Apply with --overwrite refuses .git/config and leaves it untouched
	let apply = snip(
		&[
			"--repo",
			repo.to_str().unwrap(),
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
		Some(git_payload.as_bytes()),
	);
	assert_eq!(code(&apply), 0, "{}", text(&apply.stderr));
	assert!(
		text(&apply.stderr).contains("Skipped 1."),
		"expected apply stderr to report Skipped 1."
	);
	assert_eq!(
		fs::read_to_string(&git_config).unwrap(),
		original_config,
		".git/config must not be modified"
	);

	// 3. Payload with both .git/config and .gitignore writes .gitignore while skipping .git/config
	let mixed_payload = "// file: .git/config\n[malicious]\nhacked = true\n// file: .gitignore\n*.log\n";
	let mixed = snip(
		&[
			"--repo",
			repo.to_str().unwrap(),
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
		Some(mixed_payload.as_bytes()),
	);
	assert_eq!(code(&mixed), 0, "{}", text(&mixed.stderr));
	assert!(
		text(&mixed.stderr).contains("skip 1 operation(s)"),
		"expected summary to mention skip 1 operation(s)"
	);
	assert_eq!(
		fs::read_to_string(&git_config).unwrap(),
		original_config,
		".git/config must remain unmodified"
	);
	assert_eq!(
		fs::read_to_string(repo.join(".gitignore")).unwrap(),
		"*.log"
	);

	// 4. Payload with [DELETED] .git/config leaves .git/config intact
	let del_payload = "// file: [DELETED] .git/config\n";
	let dry_del = snip(
		&[
			"--repo",
			repo.to_str().unwrap(),
			"paste",
			"--dry-run",
			"--stdin",
		],
		Some(del_payload.as_bytes()),
	);
	assert_eq!(code(&dry_del), 0, "{}", text(&dry_del.stderr));
	let dry_del_stdout = text(&dry_del.stdout);
	assert!(
		dry_del_stdout.contains("skip\t.git/config\tUNRESOLVED_PATH"),
		"expected dry-run to report skip with UNRESOLVED_PATH, got: {dry_del_stdout}"
	);

	let apply_del = snip(
		&[
			"--repo",
			repo.to_str().unwrap(),
			"paste",
			"--apply",
			"--overwrite",
			"--stdin",
		],
		Some(del_payload.as_bytes()),
	);
	assert_eq!(code(&apply_del), 0, "{}", text(&apply_del.stderr));
	assert!(
		text(&apply_del.stderr).contains("Skipped 1."),
		"expected apply stderr to report Skipped 1."
	);
	assert_eq!(
		fs::read_to_string(&git_config).unwrap(),
		original_config,
		".git/config must remain intact after deleted paste"
	);
}

#[test]
fn paste_into_missing_repo_exits_one_naming_the_path() {
	let tmp = tempfile::tempdir().unwrap();
	let nope = tmp.path().join("nope");
	let nope_s = nope.to_str().unwrap();
	let payload = "// file: a/b.txt\nhi\n";

	let apply = snip(
		&["--repo", nope_s, "paste", "--apply", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&apply), 1);
	let apply_err = text(&apply.stderr);
	assert!(apply_err.contains("nope"), "stderr was: {apply_err}");
	assert!(!nope.exists());

	let dry = snip(
		&["--repo", nope_s, "paste", "--dry-run", "--stdin"],
		Some(payload.as_bytes()),
	);
	assert_eq!(code(&dry), 1);
	let dry_err = text(&dry.stderr);
	assert!(dry_err.contains("nope"), "stderr was: {dry_err}");
	assert!(!nope.exists());
}

/// `snip paste --dry-run | head -1` closes the read end while the plan is
/// still being printed: the command must end quietly with success, the
/// way every Unix filter behaves, not panic inside a println macro
/// (exit 101).
#[test]
fn paste_dry_run_ends_quietly_when_stdout_closes() {
	let tmp = tempfile::tempdir().unwrap();
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&dst).unwrap();
	let dst_s = dst.to_str().unwrap();
	let payload = "// file: a.txt\nhello\n// file: b.txt\nbravo\n";

	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.arg("--repo")
		.arg(dst_s)
		.args(["paste", "--dry-run", "--stdin"])
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	// Drop the read end before the child writes: every later stdout write
	// fails with EPIPE, like a reader that went away after the first line.
	drop(child.stdout.take());
	let mut stdin = child.stdin.take().unwrap();
	let _ = stdin.write_all(payload.as_bytes());
	drop(stdin);
	let out = child.wait_with_output().unwrap();
	assert_eq!(
		out.status.code(),
		Some(0),
		"a closed pipe must end the command with success: {}",
		text(&out.stderr)
	);
	assert!(
		!text(&out.stderr).contains("panicked"),
		"stdout must not panic on a closed pipe: {}",
		text(&out.stderr)
	);
}

/// A write failure that is not a closed pipe must reach the exit code:
/// `/dev/full` fails every write, and the old `let _ = writeln!(…)`
/// swallowed it into a green exit. Linux-only: other platforms have no
/// /dev/full to fail with.
#[cfg(target_os = "linux")]
#[test]
fn remote_hosts_reports_a_failing_stdout_target() {
	let home = tempfile::tempdir().unwrap();
	fs::create_dir_all(home.path().join(".ssh")).unwrap();
	fs::write(
		home.path().join(".ssh/config"),
		"Host alpha\n  user u\nHost beta\n",
	)
	.unwrap();
	let full = fs::OpenOptions::new()
		.write(true)
		.open("/dev/full")
		.unwrap();
	let out = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["remote", "hosts"])
		.env("HOME", home.path())
		.stdin(Stdio::null())
		.stdout(Stdio::from(full))
		.stderr(Stdio::piped())
		.output()
		.unwrap();
	assert_eq!(
		out.status.code(),
		Some(1),
		"a failing stdout target must exit non-zero"
	);
	assert!(
		!text(&out.stderr).contains("panicked"),
		"the write error must be reported, not panicked: {}",
		text(&out.stderr)
	);
}

/// Same contract for a main-command print loop: a failing stdout target
/// exits 1 with the error on stderr instead of panicking (the old
/// println! behavior).
#[cfg(target_os = "linux")]
#[test]
fn paste_dry_run_reports_a_failing_stdout_target() {
	let tmp = tempfile::tempdir().unwrap();
	let dst = tmp.path().join("dst");
	fs::create_dir_all(&dst).unwrap();
	let dst_s = dst.to_str().unwrap();
	let payload = "// file: a.txt\nhello\n";

	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.arg("--repo")
		.arg(dst_s)
		.args(["paste", "--dry-run", "--stdin"])
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.stdin(Stdio::piped())
		.stdout(
			fs::OpenOptions::new()
				.write(true)
				.open("/dev/full")
				.unwrap()
				.into(),
		)
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let mut stdin = child.stdin.take().unwrap();
	let _ = stdin.write_all(payload.as_bytes());
	drop(stdin);
	let out = child.wait_with_output().unwrap();
	assert_eq!(
		out.status.code(),
		Some(1),
		"a failing stdout target must exit 1, not panic: {}",
		text(&out.stderr)
	);
	assert!(
		!text(&out.stderr).contains("panicked"),
		"the write error must be reported, not panicked: {}",
		text(&out.stderr)
	);
}
