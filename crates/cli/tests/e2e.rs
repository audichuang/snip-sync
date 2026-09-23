//! Layer 2 (CLI end to end) and layer 3 (cross-tool against the pinned TS
//! reference in `.ts-ref`) of plan.md section 5.
//!
//! Every run goes through the real `snip` binary with `--stdout` /
//! `--stdin`, so no system clipboard is needed. The cross-tool tests need
//! `node` (>= 22.15, for `module.registerHooks`) and the `.ts-ref` checkout;
//! without them they print why and pass as skipped. `SNIP_TS_REF` points
//! them at a `.ts-ref` directory elsewhere.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn run(mut cmd: Command, stdin: Option<&[u8]>) -> Output {
	let mut child = cmd
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
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

fn text(bytes: &[u8]) -> String {
	String::from_utf8_lossy(bytes).into_owned()
}

/// Runs `snip --repo <repo> <args>`; panics unless it exits 0.
fn snip(repo: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
	let mut cmd = Command::new(env!("CARGO_BIN_EXE_snip"));
	cmd.arg("--repo").arg(repo).args(args);
	let out = run(cmd, stdin);
	assert!(
		out.status.success(),
		"snip {args:?} exited {:?}: {}",
		out.status.code(),
		text(&out.stderr)
	);
	out
}

fn git(dir: &Path, args: &[&str]) -> String {
	let mut cmd = Command::new("git");
	cmd.args(args).current_dir(dir);
	let out = run(cmd, None);
	assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
	text(&out.stdout)
}

fn init_repo(dir: &Path) {
	fs::create_dir_all(dir).unwrap();
	git(dir, &["init", "-q", "-b", "main"]);
	configure(dir);
}

fn configure(dir: &Path) {
	git(dir, &["config", "user.name", "Committer"]);
	git(dir, &["config", "user.email", "committer@example.com"]);
}

fn clone(from: &Path, to: &Path) {
	let parent = to.parent().unwrap();
	let (from, to) = (from.to_str().unwrap(), to.to_str().unwrap());
	git(parent, &["clone", "-q", from, to]);
	configure(Path::new(to));
}

fn commit(dir: &Path, message: &str, date: &str) -> String {
	git(dir, &["add", "-A"]);
	let date = format!("--date={date}");
	let author = "--author=Alice Ünicode <alice@example.com>";
	git(dir, &["commit", "-q", author, &date, "-m", message]);
	git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn write(dir: &Path, rel: &str, content: impl AsRef<[u8]>) {
	let path = dir.join(rel);
	fs::create_dir_all(path.parent().unwrap()).unwrap();
	fs::write(path, content).unwrap();
}

/// Every file under `dir` except `.git`, keyed by `/`-separated path.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
	fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
		for entry in fs::read_dir(dir).unwrap() {
			let path = entry.unwrap().path();
			if path.file_name().unwrap() == ".git" {
				continue;
			}
			if path.is_dir() {
				walk(root, &path, out);
			} else {
				let rel = path.strip_prefix(root).unwrap();
				let rel = rel.to_str().unwrap().replace('\\', "/");
				out.insert(rel, fs::read(&path).unwrap());
			}
		}
	}
	let mut out = BTreeMap::new();
	walk(dir, dir, &mut out);
	out
}

/// Top-level entries of a repo except `.git`, as `snip copy` arguments.
fn worktree_entries(dir: &Path) -> Vec<String> {
	let mut names: Vec<String> = fs::read_dir(dir)
		.unwrap()
		.map(|e| e.unwrap().path())
		.filter(|p| p.file_name().unwrap() != ".git")
		.map(|p| p.to_str().unwrap().to_string())
		.collect();
	names.sort();
	names
}

/// The shared file-mode fixture: plain edits, a deletion, a rename and an
/// octopus merge of two side branches. Contents carry no trailing newline
/// because the format does not keep one.
struct Fixture {
	base: String,
	edit: String,
	delete: String,
	rename: String,
	octopus: String,
}

fn file_mode_fixture(dir: &Path) -> Fixture {
	init_repo(dir);
	write(dir, "a.txt", "alpha");
	write(dir, "b.txt", "bravo");
	write(
		dir,
		"keep/rename-me.txt",
		"rename me, long enough to be detected",
	);
	write(dir, "dir/d.txt", "delta\nsecond line");
	write(dir, "日本語/ファイル.txt", "héllo 世界");
	let base = commit(dir, "base", "2024-01-01T00:00:00+00:00");

	git(dir, &["checkout", "-q", "-b", "side1"]);
	write(dir, "s1.txt", "side one");
	write(dir, "dir/d.txt", "delta\nchanged on side1");
	commit(dir, "side1", "2024-01-02T00:00:00+00:00");
	git(dir, &["checkout", "-q", "-b", "side2", &base]);
	write(dir, "s2/x.txt", "side two");
	commit(dir, "side2", "2024-01-03T00:00:00+00:00");

	git(dir, &["checkout", "-q", "main"]);
	write(dir, "a.txt", "alpha, edited");
	let edit = commit(dir, "edit", "2024-02-01T00:00:00+08:00");
	fs::remove_file(dir.join("b.txt")).unwrap();
	let delete = commit(dir, "delete", "2024-02-02T00:00:00-05:00");
	fs::create_dir_all(dir.join("moved")).unwrap();
	fs::rename(
		dir.join("keep/rename-me.txt"),
		dir.join("moved/renamed.txt"),
	)
	.unwrap();
	let rename = commit(dir, "rename", "2024-02-03T00:00:00+00:00");
	git(
		dir,
		&["merge", "-q", "--no-ff", "-m", "octopus", "side1", "side2"],
	);
	let octopus = git(dir, &["rev-parse", "HEAD"]).trim().to_string();
	assert_eq!(
		git(dir, &["rev-list", "--parents", "-n1", "HEAD"])
			.split(' ')
			.count(),
		4
	);
	Fixture {
		base,
		edit,
		delete,
		rename,
		octopus,
	}
}

#[test]
fn file_mode_paths_round_trip_to_empty_dir() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	file_mode_fixture(&src);
	fs::create_dir_all(&dst).unwrap();

	let entries = worktree_entries(&src);
	let args: Vec<&str> = ["copy"]
		.into_iter()
		.chain(entries.iter().map(String::as_str))
		.chain(["--stdout"])
		.collect();
	let payload = snip(&src, &args, None).stdout;
	snip(&dst, &["paste", "--apply", "--stdin"], Some(&payload));
	assert_eq!(tree(&dst), tree(&src));
}

#[test]
fn file_mode_git_sources_restore_into_clone() {
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let fx = file_mode_fixture(&src);
	// The octopus merge is the union of its diffs against every parent, and
	// `base..HEAD` compares the endpoints: either brings `base` to HEAD.
	// A [MOVED] entry writes the new path only, so the rename's old path
	// stays behind (TS restore does the same).
	let mut expected = tree(&src);
	expected.insert(
		"keep/rename-me.txt".into(),
		b"rename me, long enough to be detected".to_vec(),
	);
	let range = format!("{}..HEAD", fx.base);
	let sources: [&[&str]; 2] =
		[&["--commit", &fx.octopus], &["--range", &range]];
	for (i, source) in sources.iter().enumerate() {
		let payload =
			snip(&src, &[&["copy"], *source, &["--stdout"]].concat(), None)
				.stdout;
		let payload_text = text(&payload);
		assert!(payload_text.contains("[DELETED] b.txt"), "{payload_text}");
		assert!(
			payload_text.contains("[MOVED] moved/renamed.txt"),
			"{payload_text}"
		);

		let dst = tmp.path().join(format!("clone{i}"));
		clone(&src, &dst);
		git(&dst, &["checkout", "-q", &fx.base]);
		write(&dst, "a.txt", "local edit, overwritten");
		let args = ["paste", "--apply", "--overwrite", "--stdin"];
		snip(&dst, &args, Some(&payload));
		assert_eq!(tree(&dst), expected, "{source:?}");
	}
}

#[test]
fn commit_mode_replays_onto_another_clones_branch() {
	let tmp = tempfile::tempdir().unwrap();
	let a = tmp.path().join("a");
	let b = tmp.path().join("b");
	init_repo(&a);
	write(&a, "a.txt", "to be renamed, with enough content\n");
	write(&a, "b.txt", "to be deleted\n");
	write(&a, "c.txt", "changed on the side branch\n");
	let base = commit(&a, "base", "2024-01-01T00:00:00+00:00");
	clone(&a, &b);

	git(&a, &["checkout", "-q", "-b", "side"]);
	write(&a, "c.txt", "changed on the side branch\nfor real\n");
	write(&a, "bin.dat", [0u8, 1, 2, 0xff, 0]);
	commit(&a, "side: edit c, add bin", "2024-01-15T09:30:00+09:00");
	git(&a, &["checkout", "-q", "main"]);
	fs::create_dir_all(a.join("ren")).unwrap();
	fs::rename(a.join("a.txt"), a.join("ren/a.txt")).unwrap();
	commit(
		&a,
		"rename a\n\nwith a body\nof two lines",
		"2024-02-01T10:00:00+08:00",
	);
	fs::remove_file(a.join("b.txt")).unwrap();
	commit(&a, "delete b", "2024-03-01T10:00:00-05:00");
	let date = "--date=2024-04-01T10:00:00+05:30";
	let author = "--author=Bob <bob@example.com>";
	git(&a, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
	git(&a, &["commit", "-q", date, author, "-m", "merge side"]);

	// A different branch of the other clone, with a commit of its own.
	git(&b, &["checkout", "-q", "-b", "feature", &base]);
	write(&b, "local.txt", "local\n");
	commit(&b, "local work", "2024-01-05T00:00:00+00:00");

	let out = snip(&a, &["copy", "--commits", "-n", "3", "--stdout"], None);
	let note = text(&out.stderr);
	assert!(note.contains("3 commit(s) copied"), "{note}");
	assert!(
		note.contains("commit 3: bin.dat not copied (BINARY)"),
		"{note}"
	);
	let out = snip(&b, &["paste", "--apply", "--stdin"], Some(&out.stdout));
	assert!(text(&out.stdout).contains("Created 3 commit(s)."));

	assert_eq!(
		git(&b, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
		"feature"
	);
	assert_eq!(
		git(&b, &["log", "-1", "--format=%s", "HEAD~3"]).trim(),
		"local work"
	);
	assert_eq!(git(&b, &["status", "--porcelain"]), "");
	let src = git(&a, &["rev-list", "--first-parent", "-3", "HEAD"]);
	let dst = git(&b, &["rev-list", "-3", "HEAD"]);
	for (ca, cb) in src.lines().zip(dst.lines()) {
		let meta = "--format=%B%x00%an%x00%ae%x00%aI";
		let show = |dir: &Path, c: &str| git(dir, &["log", "-1", meta, c]);
		assert_eq!(show(&a, ca), show(&b, cb));
		// Same changes against the first parent, minus the binary file.
		let changes = |dir: &Path, c: &str| {
			let parent = format!("{c}^1");
			git(dir, &["diff-tree", "-r", "-M", "--name-status", &parent, c])
				.lines()
				.filter(|l| !l.ends_with("bin.dat"))
				.map(str::to_string)
				.collect::<Vec<_>>()
		};
		assert_eq!(changes(&a, ca), changes(&b, cb), "{ca} vs {cb}");
	}
	let mut expected = tree(&a);
	expected.remove("bin.dat");
	expected.insert("local.txt".into(), b"local\n".to_vec());
	assert_eq!(tree(&b), expected);
}

// ---- Layer 3: cross-tool against the TS reference ----

fn workspace_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `.ts-ref` checkout: `SNIP_TS_REF`, the workspace root, or the main
/// checkout's root when running from a linked git worktree.
fn ts_ref_dir() -> Option<PathBuf> {
	let mut candidates = Vec::new();
	if let Some(dir) = std::env::var_os("SNIP_TS_REF") {
		candidates.push(PathBuf::from(dir));
	}
	let root = workspace_root();
	candidates.push(root.join(".ts-ref"));
	let common = Command::new("git")
		.args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
		.current_dir(&root)
		.output();
	if let Some(out) = common.ok().filter(|o| o.status.success()) {
		let common = PathBuf::from(text(&out.stdout).trim());
		if let Some(main) = common.parent() {
			candidates.push(main.join(".ts-ref"));
		}
	}
	candidates
		.into_iter()
		.find(|d| d.join("src/copy.ts").is_file())
}

/// Why the cross-tool tests cannot run here, if they cannot.
fn ts_unavailable() -> Result<PathBuf, String> {
	let Some(ts_ref) = ts_ref_dir() else {
		return Err(
			".ts-ref not found (see docs/tickets/README.md); set SNIP_TS_REF"
				.into(),
		);
	};
	let out = Command::new("node").arg("--version").output();
	let Some(out) = out.ok().filter(|o| o.status.success()) else {
		return Err("node not found on PATH".into());
	};
	let version = text(&out.stdout);
	let mut parts = version.trim().trim_start_matches('v').split('.');
	let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
	let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
	if (major, minor) < (22, 15) {
		return Err(format!("node {} is older than 22.15", version.trim()));
	}
	Ok(ts_ref)
}

macro_rules! ts_ref_or_skip {
	() => {
		match ts_unavailable() {
			Ok(dir) => dir,
			Err(why) => {
				eprintln!("SKIPPED cross-tool test: {why}");
				return;
			}
		}
	};
}

/// Runs `scripts/ts-ref.mjs <cmd> <root> <args>`; panics unless it exits 0.
fn ts(
	ts_ref: &Path,
	cmd: &str,
	root: &Path,
	args: &[&str],
	stdin: Option<&[u8]>,
) -> Vec<u8> {
	let mut node = Command::new("node");
	node.arg("--experimental-strip-types")
		.arg("--no-warnings")
		.arg(workspace_root().join("scripts/ts-ref.mjs"))
		.arg(ts_ref)
		.arg(cmd)
		.arg(root)
		.args(args);
	let out = run(node, stdin);
	assert!(
		out.status.success(),
		"ts-ref {cmd} {args:?}: {}",
		text(&out.stderr)
	);
	out.stdout
}

/// Files that exercise the format's edges; they do not round-trip byte for
/// byte (CRLF, trailing blank lines), so they stay out of the layer-2 trees.
fn add_edge_files(dir: &Path) {
	write(dir, "edge/crlf.txt", "one\r\ntwo\r\n");
	write(dir, "edge/trailing.txt", "\n\nbody\n\n\n");
	write(dir, "edge/empty.txt", "");
	write(
		dir,
		"edge/fake-header.txt",
		"// file: not/a/header.txt\n  // file: indented",
	);
	write(dir, "edge/dollar$&$$.txt", "$FILE_PATH stays");
	write(dir, "edge/full\u{3000}width.txt", "\u{3000}// file: x");
	write(dir, "edge/binary.bin", [0u8, 0xff, 0xfe]);
	write(dir, "edge/latin1.txt", [b'c', b'a', b'f', 0xe9]);
}

#[test]
fn cross_tool_file_payload_is_byte_identical() {
	let ts_ref = ts_ref_or_skip!();
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	file_mode_fixture(&src);
	add_edge_files(&src);

	let entries = worktree_entries(&src);
	let subsets: Vec<Vec<String>> = vec![
		entries.clone(),
		vec![src.join("edge").to_str().unwrap().into()],
		vec![
			src.join("dir/d.txt").to_str().unwrap().into(),
			src.join("日本語").to_str().unwrap().into(),
		],
	];
	for paths in subsets {
		let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
		let args = [&["copy"], &paths[..], &["--stdout"]].concat();
		let rust = snip(&src, &args, None).stdout;
		let node = ts(&ts_ref, "files", &src, &paths, None);
		assert_eq!(text(&rust), text(&node), "{paths:?}");
		assert_eq!(rust, node);
		assert!(text(&rust).contains("// file: "), "empty payload");
	}
}

#[test]
fn cross_tool_commit_payload_is_byte_identical() {
	let ts_ref = ts_ref_or_skip!();
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let fx = file_mode_fixture(&src);
	// A commit whose content needs escaping and one git cannot decode.
	add_edge_files(&src);
	let edge = commit(&src, "edge files", "2024-03-01T00:00:00+00:00");

	let commits = [
		&fx.base,
		&fx.edit,
		&fx.delete,
		&fx.rename,
		&fx.octopus,
		&edge,
	];
	for sha in commits {
		let rust =
			snip(&src, &["copy", "--commit", sha, "--stdout"], None).stdout;
		let node = ts(&ts_ref, "commit", &src, &[sha], None);
		assert_eq!(text(&rust), text(&node), "commit {sha}");
		assert_eq!(rust, node);
		assert!(text(&rust).contains("// file: "), "empty payload");
	}
}

#[test]
fn cross_tool_restores_each_others_payloads() {
	let ts_ref = ts_ref_or_skip!();
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let fx = file_mode_fixture(&src);
	add_edge_files(&src);

	// Paths mode into empty directories.
	let entries = worktree_entries(&src);
	let paths: Vec<&str> = entries.iter().map(String::as_str).collect();
	let rust_payload =
		snip(&src, &[&["copy"], &paths[..], &["--stdout"]].concat(), None)
			.stdout;
	let ts_payload = ts(&ts_ref, "files", &src, &paths, None);
	let restore = |name: &str, payload: &[u8], by_ts: bool| {
		let dst = tmp.path().join(name);
		fs::create_dir_all(&dst).unwrap();
		if by_ts {
			ts(&ts_ref, "restore", &dst, &[], Some(payload));
		} else {
			snip(&dst, &["paste", "--apply", "--stdin"], Some(payload));
		}
		tree(&dst)
	};
	let rust_by_rust = restore("rr", &rust_payload, false);
	assert_eq!(
		restore("tr", &ts_payload, false),
		rust_by_rust,
		"Rust restores TS"
	);
	assert_eq!(
		restore("rt", &rust_payload, true),
		rust_by_rust,
		"TS restores Rust"
	);
	assert!(rust_by_rust.contains_key("edge/crlf.txt"));

	// Git source (octopus merge) into clones at the merge base.
	let rust_payload =
		snip(&src, &["copy", "--commit", &fx.octopus, "--stdout"], None).stdout;
	let ts_payload = ts(&ts_ref, "commit", &src, &[&fx.octopus], None);
	let restore = |name: &str, payload: &[u8], by_ts: bool| {
		let dst = tmp.path().join(name);
		clone(&src, &dst);
		git(&dst, &["checkout", "-q", &fx.base]);
		if by_ts {
			ts(&ts_ref, "restore", &dst, &[], Some(payload));
		} else {
			snip(
				&dst,
				&["paste", "--apply", "--overwrite", "--stdin"],
				Some(payload),
			);
		}
		tree(&dst)
	};
	let rust_by_rust = restore("grr", &rust_payload, false);
	assert!(!rust_by_rust.contains_key("b.txt"));
	assert_eq!(
		restore("gtr", &ts_payload, false),
		rust_by_rust,
		"Rust restores TS"
	);
	assert_eq!(
		restore("grt", &rust_payload, true),
		rust_by_rust,
		"TS restores Rust"
	);
}
