//! Paste through a worker: the worker plans and writes with the engine a
//! local paste runs, on its own disk, and refuses what a local paste
//! refuses (a stale destination, a Git directory, a path outside the
//! workspace).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use snip_core::restore::{
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	plan_import, CanonicalRootId, CommitReplayPreview, ImportMapping,
};
use snip_remote::proto::MAX_FRAME;
use snip_remote::{
	Client, ErrorCode, PasteMapping, RemoteError, RemoteHost, Worker,
	WorkerOptions,
};

/// The worker's Git pool is one per process: tests that each run dozens
/// of Git calls take turns, or the pool's queue refuses the surplus.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
	SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn worker(max_protocol: Option<u32>) -> Arc<Worker> {
	Arc::new(Worker::new(WorkerOptions {
		name: "w".into(),
		max_protocol,
	}))
}

fn open(w: &Arc<Worker>, root: &Path) -> (Client, String) {
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let ws = client.open_workspace(&root.display().to_string()).unwrap();
	(client, ws.id)
}

fn git(dir: &Path, args: &[&str]) {
	let out = match std::process::Command::new("git")
		.args(["-c", "user.name=t", "-c", "user.email=t@t"])
		.args(args)
		.current_dir(dir)
		.output()
	{
		Ok(out) => out,
		Err(e) => {
			assert!(
				std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
				"git failed to run with SNIP_REQUIRE_ALL_TESTS set: {e}"
			);
			panic!("git failed to run: {e}");
		}
	};
	assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
	let out = std::process::Command::new("git")
		.args(args)
		.current_dir(dir)
		.output()
		.unwrap();
	String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn refused<T: std::fmt::Debug>(result: Result<T, RemoteError>) -> ErrorCode {
	match result {
		Err(RemoteError::Refused { code, .. }) => code,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

fn entry(path: &str, body: &str) -> String {
	format!("// File: {path}\n{body}\n")
}

/// The same paste run locally, for comparison.
fn local_paste(
	dest: &Path,
	text: &str,
	sel: &RestoreSelection,
) -> RestoreExecutionResult {
	let id = CanonicalRootId::new(dest).unwrap();
	let plan = plan_import(
		text,
		"",
		&[id.path().to_path_buf()],
		&ImportMapping::with_primary(id.clone()),
	)
	.unwrap();
	plan.apply(sel).unwrap()
}

/// What a local paste of `text` into `dest` answers, prefixes routed to
/// other folders as the app routes them: its operations, or its refusal in
/// the words the worker sends.
fn local_outcome(
	dest: &Path,
	text: &str,
	prefixes: &[(&str, &Path)],
) -> Result<RestorePlan, String> {
	let dest = dunce::canonicalize(dest).unwrap();
	let mut roots = vec![dest.clone()];
	let mut mapping =
		ImportMapping::with_primary(CanonicalRootId::new(&dest).unwrap());
	for (prefix, folder) in prefixes {
		let folder = dunce::canonicalize(folder).unwrap();
		mapping.map_prefix(*prefix, CanonicalRootId::new(&folder).unwrap());
		roots.push(folder);
	}
	plan_import(text, "", &roots, &mapping)
		.map(|p| p.restore_plan().clone())
		.map_err(|e| e.paste_message())
}

/// The worker's answer to the same paste, in the same shape.
fn remote_outcome(
	res: Result<snip_remote::ImportPlanned, RemoteError>,
) -> Result<RestorePlan, String> {
	match res {
		Ok(planned) => Ok(planned.plan.restore_plan().clone()),
		Err(RemoteError::Refused { message, .. }) => Err(message),
		Err(other) => panic!("not a worker answer: {other:?}"),
	}
}

/// A refusal, or a plan with nothing to write.
fn no_writes(outcome: &Result<RestorePlan, String>) -> bool {
	outcome.as_ref().map_or(true, |p| {
		p.create_operations.is_empty() && p.delete_operations.is_empty()
	})
}

fn unchecked(creates: &[usize]) -> RestoreSelection {
	RestoreSelection {
		overwrite_existing: true,
		skip_existing: false,
		unchecked_creates: creates.iter().copied().collect::<BTreeSet<_>>(),
		unchecked_deletes: BTreeSet::new(),
	}
}

#[test]
fn a_remote_file_paste_writes_what_a_local_paste_writes() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let local = tmp.path().join("local");
	for dir in [&ws, &local] {
		fs::create_dir_all(dir.join("src")).unwrap();
		fs::write(dir.join("a.txt"), "old\n").unwrap();
	}
	let text = [
		entry("a.txt", "new 中文\r\nline"),
		entry("src/b.rs", "fn main() {}"),
		entry("c.txt", "unchecked"),
	]
	.concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	let ops = planned.plan.create_operations();
	assert_eq!(
		ops.iter()
			.map(|o| (o.relative_path.as_str(), o.existed))
			.collect::<Vec<_>>(),
		[("a.txt", true), ("src/b.rs", false), ("c.txt", false)]
	);
	// The plan names the worker's own paths.
	let real = dunce::canonicalize(&ws).unwrap();
	assert_eq!(ops[1].absolute_path, real.join("src/b.rs"));

	let sel = unchecked(&[2]);
	let result = client
		.import_apply(&id, "", &text, &mapping, &sel, &planned.expect(), None)
		.unwrap();
	assert_eq!(result, local_paste(&local, &text, &sel));
	assert_eq!((result.created_count, result.overwritten_count), (1, 1));
	for rel in ["a.txt", "src/b.rs"] {
		assert_eq!(
			fs::read(ws.join(rel)).unwrap(),
			fs::read(local.join(rel)).unwrap(),
			"{rel}"
		);
	}
	assert!(
		!ws.join("c.txt").exists(),
		"an unchecked row is not written"
	);
}

#[test]
fn a_paste_into_a_subfolder_maps_prefixes_to_other_folders() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(ws.join("one")).unwrap();
	fs::create_dir_all(ws.join("two")).unwrap();
	let text = [entry("app/x.txt", "x"), entry("lib/y.txt", "y")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping {
		prefixes: vec![("lib".into(), "two".into())],
		..PasteMapping::default()
	};
	let planned = client
		.import_plan(&id, "one", &text, &mapping, None)
		.unwrap();
	let sel = unchecked(&[]);
	client
		.import_apply(
			&id,
			"one",
			&text,
			&mapping,
			&sel,
			&planned.expect(),
			None,
		)
		.unwrap();
	assert_eq!(fs::read_to_string(ws.join("one/app/x.txt")).unwrap(), "x");
	assert_eq!(fs::read_to_string(ws.join("two/y.txt")).unwrap(), "y");
}

#[test]
fn a_destination_changed_after_the_preview_is_refused_and_nothing_is_written() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	fs::write(ws.join("a.txt"), "old\n").unwrap();
	let text = [entry("a.txt", "new"), entry("b.txt", "b")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	fs::write(ws.join("a.txt"), "edited meanwhile\n").unwrap();
	let res = client.import_apply(
		&id,
		"",
		&text,
		&mapping,
		&unchecked(&[]),
		&planned.expect(),
		None,
	);
	let Err(RemoteError::Refused { code, message }) = res else {
		panic!("expected stale, got {res:?}");
	};
	assert_eq!(code, ErrorCode::Stale);
	assert!(
		message.contains("modified externally after preview"),
		"{message}"
	);
	assert_eq!(
		fs::read_to_string(ws.join("a.txt")).unwrap(),
		"edited meanwhile\n"
	);
	assert!(!ws.join("b.txt").exists());

	// A file created where the preview saw none is stale too.
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	fs::write(ws.join("b.txt"), "appeared\n").unwrap();
	let res = client.import_apply(
		&id,
		"",
		&text,
		&mapping,
		&unchecked(&[]),
		&planned.expect(),
		None,
	);
	assert_eq!(refused(res), ErrorCode::Stale);
	assert_eq!(fs::read_to_string(ws.join("b.txt")).unwrap(), "appeared\n");
}

#[test]
fn a_paste_never_writes_into_git_or_outside_the_workspace() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let repo = ws.join("repo");
	fs::create_dir_all(&repo).unwrap();
	git(&repo, &["init", "-q"]);
	let hooks = repo.join(".git/hooks");
	let before = fs::read_dir(&hooks).unwrap().count();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();

	// Entries naming `.git` are skipped, as a local paste skips them.
	let text = [
		entry(".git/hooks/pre-commit", "#!/bin/sh\necho owned"),
		entry("repo/.git/config", "[core]"),
	]
	.concat();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	assert!(planned.plan.create_operations().is_empty());
	assert_eq!(planned.plan.skipped_operations().len(), 2);
	client
		.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		)
		.unwrap();
	assert!(!ws.join(".git").exists());
	assert!(!hooks.join("pre-commit").exists());

	// A destination inside a Git directory gets the answer a local paste
	// gets there, and nothing is written.
	let bare = ws.join("bare.git");
	fs::create_dir_all(&bare).unwrap();
	git(&bare, &["init", "-q", "--bare"]);
	let bare_hooks = fs::read_dir(bare.join("hooks")).unwrap().count();
	let text = entry("pre-commit", "#!/bin/sh\necho owned");
	for dest in ["repo/.git", "repo/.git/hooks", "bare.git", "bare.git/hooks"] {
		let remote = remote_outcome(
			client.import_plan(&id, dest, &text, &mapping, None),
		);
		assert_eq!(remote, local_outcome(&ws.join(dest), &text, &[]), "{dest}");
		assert!(no_writes(&remote), "{dest}: {remote:?}");
	}
	assert_eq!(
		refused(client.import_plan(
			&id,
			"../ws/repo/.git",
			&text,
			&mapping,
			None
		)),
		ErrorCode::BadRequest
	);
	// A mapped prefix folder is held to the same rule.
	let mapped = PasteMapping {
		prefixes: vec![("x".into(), "repo/.git/hooks".into())],
		..PasteMapping::default()
	};
	let text = entry("x/pre-commit", "x");
	let remote =
		remote_outcome(client.import_plan(&id, "", &text, &mapped, None));
	assert_eq!(remote, local_outcome(&ws, &text, &[("x", &hooks)]));
	assert!(no_writes(&remote), "{remote:?}");
	assert!(!hooks.join("pre-commit").exists());
	assert_eq!(fs::read_dir(&hooks).unwrap().count(), before);
	assert_eq!(
		fs::read_dir(bare.join("hooks")).unwrap().count(),
		bare_hooks
	);
	assert!(!repo.join(".git/pre-commit").exists());
	assert!(!bare.join("pre-commit").exists());

	// Outside the workspace.
	let res = client.import_plan(&id, "../", &text, &mapping, None);
	assert!(matches!(
		refused(res),
		ErrorCode::Forbidden | ErrorCode::BadRequest
	));
	assert!(!tmp.path().join("pre-commit").exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_cannot_lead_a_paste_out_of_the_workspace_or_into_git() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let repo = ws.join("repo");
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&repo).unwrap();
	fs::create_dir_all(&outside).unwrap();
	git(&repo, &["init", "-q"]);
	std::os::unix::fs::symlink(&outside, ws.join("out")).unwrap();
	std::os::unix::fs::symlink(repo.join(".git"), ws.join("gitlink")).unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let text = entry("f.txt", "x");
	assert_eq!(
		refused(client.import_plan(&id, "out", &text, &mapping, None)),
		ErrorCode::Forbidden
	);
	// Into `.git` through a symlink: skipped rows, as locally.
	let remote = remote_outcome(
		client.import_plan(&id, "gitlink", &text, &mapping, None),
	);
	assert_eq!(remote, local_outcome(&ws.join("gitlink"), &text, &[]));
	assert!(no_writes(&remote), "{remote:?}");
	// An entry reaching `.git` through a symlinked folder inside the
	// workspace is a skipped row; the others are written.
	let text = [
		entry("gitlink/hooks/pre-commit", "x"),
		entry("ok.txt", "ok"),
	]
	.concat();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	assert_eq!(
		Ok(planned.plan.restore_plan().clone()),
		local_outcome(&ws, &text, &[])
	);
	assert_eq!(planned.plan.create_operations().len(), 1);
	client
		.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		)
		.unwrap();
	assert_eq!(fs::read_to_string(ws.join("ok.txt")).unwrap(), "ok");
	assert!(!repo.join(".git/hooks/pre-commit").exists());
	assert!(fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn a_payload_over_one_frame_goes_both_ways_in_chunks() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	// Quotes and newlines double in JSON: the reply is far over a frame.
	let line = "\"quoted\" text\tand a tab\n".repeat(64);
	let body = line.repeat(9 * 1024 * 1024 / line.len() / 4);
	let text: String = (0..4)
		.map(|i| entry(&format!("big{i}.txt"), body.trim_end()))
		.collect();
	assert!(text.len() > MAX_FRAME);
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	assert_eq!(planned.plan.create_operations().len(), 4);
	client
		.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		)
		.unwrap();
	for i in 0..4 {
		assert_eq!(
			fs::read_to_string(ws.join(format!("big{i}.txt"))).unwrap(),
			body.trim_end()
		);
	}
}

fn init_repo(dir: &Path) {
	fs::create_dir_all(dir).unwrap();
	git(dir, &["init", "-q", "-b", "main"]);
	git(dir, &["config", "user.name", "t"]);
	git(dir, &["config", "user.email", "t@t"]);
	fs::write(dir.join("README"), "base\n").unwrap();
	git(dir, &["add", "."]);
	git(dir, &["commit", "-q", "-m", "base"]);
}

/// Everything about the replayed commits a replay decides: trees, authors
/// and messages (commit ids carry the committer's clock, and each base
/// commit its own).
fn history(dir: &Path) -> String {
	git_out(dir, &["log", "-2", "--format=%T %an <%ae> %ad %s"])
}

#[test]
fn a_remote_commit_replay_makes_the_commits_a_local_replay_makes() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let src = ws.join("src");
	init_repo(&src);
	fs::write(src.join("a.txt"), "one\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "add a"]);
	let first = git_out(&src, &["rev-parse", "HEAD"]);
	fs::write(src.join("a.txt"), "two\n").unwrap();
	fs::create_dir_all(src.join("d")).unwrap();
	fs::write(src.join("d/b.txt"), "b\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "change a, add b"]);
	let tip = git_out(&src, &["rev-parse", "HEAD"]);

	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let text = client
		.export_commits(&id, "src", &tip, vec![tip.clone(), first], None)
		.unwrap()
		.text;

	let dst = ws.join("dst");
	let local = tmp.path().join("local");
	init_repo(&dst);
	init_repo(&local);
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();
	let preview = client
		.replay_plan(&id, "dst", &text, payload.clone(), None)
		.unwrap();
	assert_eq!(preview.destination(), dunce::canonicalize(&dst).unwrap());
	assert_eq!(preview.plan().commits.len(), 2);
	// Unchanged: a re-check passes and writes nothing.
	let checked = client
		.replay_apply(&id, "dst", &text, &preview, true, None)
		.unwrap();
	assert!(checked.is_none());
	assert_eq!(git_out(&dst, &["rev-list", "--count", "HEAD"]), "1");

	let result = client
		.replay_apply(&id, "dst", &text, &preview, false, None)
		.unwrap()
		.unwrap();
	assert_eq!(result.created.len(), 2, "{result:?}");
	assert!(result.failure.is_none());
	let local_res = CommitReplayPreview::capture(&local, &payload)
		.unwrap()
		.apply()
		.unwrap();
	assert_eq!(local_res.created.len(), 2);
	assert_eq!(history(&dst), history(&local));
	assert_eq!(fs::read(dst.join("d/b.txt")).unwrap(), b"b\n");
	assert_eq!(git_out(&dst, &["status", "--porcelain"]), "");

	// Replaying the same preview again is stale: HEAD moved.
	let res = client.replay_apply(&id, "dst", &text, &preview, false, None);
	assert_eq!(refused(res), ErrorCode::Stale);
	assert_eq!(git_out(&dst, &["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn a_commit_replay_refuses_a_changed_repo_and_is_scoped_to_the_opened_folder() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let src = ws.join("src");
	init_repo(&src);
	fs::write(src.join("a.txt"), "one\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "add a"]);
	let tip = git_out(&src, &["rev-parse", "HEAD"]);
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let text = client
		.export_commits(&id, "src", &tip, vec![tip.clone()], None)
		.unwrap()
		.text;
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();

	let dst = ws.join("dst");
	init_repo(&dst);
	let preview = client
		.replay_plan(&id, "dst", &text, payload.clone(), None)
		.unwrap();
	fs::write(dst.join("a.txt"), "appeared\n").unwrap();
	assert_eq!(
		refused(client.replay_apply(&id, "dst", &text, &preview, true, None)),
		ErrorCode::Stale
	);
	assert_eq!(
		refused(client.replay_apply(&id, "dst", &text, &preview, false, None)),
		ErrorCode::Stale
	);
	assert_eq!(git_out(&dst, &["rev-list", "--count", "HEAD"]), "1");

	// A workspace inside a repository is the replay's write scope: a
	// commit whose targets reach outside the opened folder is refused,
	// and nothing is written there. Replaying the whole repository means
	// opening the repository root.
	let outer = tmp.path().join("outer");
	init_repo(&outer);
	fs::create_dir_all(outer.join("sub")).unwrap();
	let (client2, sub_id) = open(&w, &outer.join("sub"));
	let Err(RemoteError::Refused { message, .. }) =
		client2.replay_plan(&sub_id, "", &text, payload.clone(), None)
	else {
		panic!("expected a refusal, got a preview");
	};
	assert!(message.contains("outside the opened folder"), "{message}");
	assert!(
		!outer.join("a.txt").exists(),
		"nothing is written outside the opened folder"
	);

	let (client3, outer_id) = open(&w, &outer);
	let preview = client3
		.replay_plan(&outer_id, "", &text, payload, None)
		.unwrap();
	assert_eq!(preview.destination(), dunce::canonicalize(&outer).unwrap());
	let result = client3
		.replay_apply(&outer_id, "", &text, &preview, false, None)
		.unwrap()
		.unwrap();
	assert_eq!(result.created.len(), 1, "{result:?}");
	assert_eq!(fs::read_to_string(outer.join("a.txt")).unwrap(), "one\n");
}

#[test]
fn a_payload_over_the_clipboard_limit_is_refused_whole() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(None);
	let (client, id) = open(&w, tmp.path());
	let body = "x".repeat(snip_core::transfer::CLIPBOARD_PAYLOAD_MAX);
	let text = entry("big.txt", &body);
	let res =
		client.import_plan(&id, "", &text, &PasteMapping::default(), None);
	assert_eq!(refused(res), ErrorCode::TooLarge);
	assert!(!tmp.path().join("big.txt").exists());
	// The connection is still in step: the next paste is planned.
	let planned = client
		.import_plan(
			&id,
			"",
			&entry("a.txt", "a"),
			&PasteMapping::default(),
			None,
		)
		.unwrap();
	assert_eq!(planned.plan.create_operations().len(), 1);
}

#[test]
fn a_worker_without_paste_says_it_is_too_old() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(Some(3));
	let (client, id) = open(&w, tmp.path());
	let res = client.import_plan(
		&id,
		"",
		&entry("a.txt", "a"),
		&PasteMapping::default(),
		None,
	);
	match res {
		Err(RemoteError::WorkerTooOld { have, need, .. }) => {
			assert_eq!((have, need), (3, 4));
		}
		other => panic!("expected too old, got {other:?}"),
	}
	assert!(!tmp.path().join("a.txt").exists());
}

/// An Apply request whose freshness snapshot is far over one frame — many
/// long paths — goes out in bounded JSON pieces and the apply lands.
#[test]
fn an_apply_with_a_huge_freshness_snapshot_rides_back_in_chunks() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let filler = "x".repeat(300);
	let text: String = (0..4_000)
		.map(|i| {
			let dir = format!(
				"level-one/level-two/level-three/level-four/dir-{i:04}"
			);
			entry(&format!("{dir}/file-with-a-long-name-{i:04}.rs"), &filler)
		})
		.collect();
	assert!(text.len() > snip_remote::proto::CHUNK_BYTES);
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	let sel = unchecked(&[]);
	let result = client
		.import_apply(&id, "", &text, &mapping, &sel, &planned.expect(), None)
		.unwrap();
	assert_eq!(result.created_count, 4_000, "{result:?}");
	assert!(ws.join("level-one").join("level-two").is_dir());
	assert_eq!(
		fs::read_to_string(
			ws.join("level-one/level-two/level-three/level-four/dir-0000")
				.join("file-with-a-long-name-0000.rs")
		)
		.unwrap(),
		filler
	);
}

/// A relative hook path must not pause Apply even when that file exists.
#[test]
fn a_relative_paste_hold_path_does_not_pause_apply() {
	use std::time::{Duration, Instant};
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir(&ws).unwrap();
	let text = [entry("a.txt", "first"), entry("b.txt", "second")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	// A real file in the current directory (the crate root), addressed by
	// its relative name; nothing is written into the source tree.
	let relative = Path::new("Cargo.toml");
	assert!(!relative.is_absolute() && relative.is_file());
	std::env::set_var("SNIP_E2E_PASTE_HOLD", relative);
	let result = client.call_with(
		&snip_remote::Request::ImportApply {
			workspace: id,
			dest: String::new(),
			text,
			mapping,
			selection: unchecked(&[]),
			expect: planned.expect(),
		},
		None,
		Duration::from_secs(3),
	);
	std::env::remove_var("SNIP_E2E_PASTE_HOLD");
	// On the failing implementation, release and drain the paused worker
	// before reporting the failure so it cannot leak into the next test.
	let deadline = Instant::now() + Duration::from_secs(10);
	while !ws.join("b.txt").exists() && Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(10));
	}
	assert!(
		ws.join("b.txt").exists(),
		"apply must finish within the bound"
	);
	assert!(
		matches!(&result, Ok(snip_remote::Response::Imported(applied)) if applied.created_count == 2 && applied.errors.is_empty()),
		"a relative hook path must be ignored: {result:?}"
	);
	assert_eq!(fs::read(ws.join("a.txt")).unwrap(), b"first");
	assert_eq!(fs::read(ws.join("b.txt")).unwrap(), b"second");
}

/// `SNIP_E2E_PASTE_HOLD`: an Apply paused after its first committed write,
/// with the master's call cut before any answer came back, reports
/// outcome unknown while the worker keeps that first write — the L05
///「寫入途中斷線」case, made deterministic.
#[test]
fn a_cut_apply_reports_outcome_unknown_and_keeps_its_first_write() {
	use std::time::{Duration, Instant};

	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let hold = tmp.path().join("paste-hold");
	let text = [entry("a.txt", "first"), entry("b.txt", "second")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	let sel = unchecked(&[]);

	fs::write(&hold, b"").unwrap();
	std::env::set_var("SNIP_E2E_PASTE_HOLD", &hold);
	let req = snip_remote::Request::ImportApply {
		workspace: id,
		dest: String::new(),
		text,
		mapping,
		selection: sel,
		expect: planned.expect(),
	};
	let call = std::thread::spawn(move || {
		client.call_with(&req, None, Duration::from_secs(3))
	});
	// The worker's first write is on disk before the cut: the pause sits
	// between the two files, not before the first.
	let deadline = Instant::now() + Duration::from_secs(10);
	while !ws.join("a.txt").exists() && Instant::now() < deadline {
		std::thread::sleep(Duration::from_millis(10));
	}
	assert!(
		ws.join("a.txt").exists(),
		"the worker must commit its first write"
	);
	assert!(
		!ws.join("b.txt").exists(),
		"the hold sits between the two writes"
	);

	// The call is cut by its own deadline while the worker is still
	// parked; only the cut releases the hold.
	let answer = call.join().expect("the call thread must end");
	std::env::remove_var("SNIP_E2E_PASTE_HOLD");
	fs::remove_file(&hold).unwrap();
	let err = match answer {
		Err(err) => err,
		Ok(ok) => panic!("a cut apply cannot answer: {ok:?}"),
	};
	assert!(err.outcome_unknown(), "{err:?}");
	assert_eq!(
		fs::read(ws.join("a.txt")).unwrap(),
		b"first",
		"the worker wrote the first file before the cut"
	);
	// Lifted, the worker finishes the apply it already started.
	let resume = Instant::now() + Duration::from_secs(10);
	while !ws.join("b.txt").exists() && Instant::now() < resume {
		std::thread::sleep(Duration::from_millis(10));
	}
	assert!(
		ws.join("b.txt").exists(),
		"the worker finishes after the hold lifts"
	);
}
