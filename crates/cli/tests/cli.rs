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
	commit(&repo, "commit A", "2020-01-01T00:00:00+00:00");
	let sha_a = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

	fs::write(repo.join("mod.txt"), "modified in B\n").unwrap();
	fs::write(repo.join("added.txt"), "added in B\n").unwrap();
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
	};

	check_sources(&repo);

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let symlink_repo = symlink_dir.path().join("link_repo");
		std::os::unix::fs::symlink(&repo, &symlink_repo).unwrap();
		check_sources(&symlink_repo);
	}

	// Now test --working: modified + untracked + deleted
	fs::write(repo.join("mod.txt"), "working mod\n").unwrap();
	fs::write(repo.join("untracked.txt"), "untracked content\n").unwrap();
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
	};

	check_working(&repo);

	#[cfg(unix)]
	{
		let symlink_dir = tempfile::tempdir().unwrap();
		let symlink_repo = symlink_dir.path().join("link_repo_work");
		std::os::unix::fs::symlink(&repo, &symlink_repo).unwrap();
		check_working(&symlink_repo);
	}

	// Now test --staged: modified + added + deleted
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
