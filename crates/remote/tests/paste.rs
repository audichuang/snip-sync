//! Paste through a worker: the worker plans and writes with the engine a
//! local paste runs, on its own disk, and refuses what a local paste
//! refuses (a stale destination, a Git directory, a path outside the
//! workspace).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use snip_core::gitrun::RunOptions;
use snip_core::restore::{
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	plan_import, CanonicalRootId, CommitReplayPreview, ImportMapping,
};
use snip_remote::proto::MAX_FRAME;
use snip_remote::{
	Client, ErrorCode, FilePastePlan, PasteError, PasteMapping, PasteRoute,
	PasteTarget, PreparedReplay, RemoteError, RemoteHost, Worker,
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
		..Default::default()
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
	let Err(RemoteError::Refused { code, message, .. }) = res else {
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
fn paste_targets_keep_protected_reasons_and_count_all_skips_once() {
	use snip_core::restore::SkipDetail;
	let _serial = serial();
	for remote in [false, true] {
		for mixed in [false, true] {
			let tmp = tempfile::tempdir().unwrap();
			let ws = tmp.path().join("ws");
			let repo = ws.join("target");
			init_repo(&repo);
			fs::write(repo.join("existing.txt"), "keep").unwrap();
			let mut text =
				entry("target/.git/hooks/snip-test-blocked", "blocked");
			if mixed {
				text.push_str(&entry("target/created.txt", "created"));
				text.push_str(&entry("target/existing.txt", "replace"));
			}
			let (target, route) = if remote {
				let w = worker(None);
				let (client, workspace) = open(&w, &ws);
				(
					PasteTarget::Remote {
						client: Arc::new(client),
						workspace,
						dest: String::new(),
					},
					PasteRoute {
						prefixes: vec![("target".into(), "target".into())],
						..Default::default()
					},
				)
			} else {
				(
					PasteTarget::Local {
						dest: ws.clone(),
						bound: None,
					},
					PasteRoute {
						prefixes: vec![("target".into(), repo.clone())],
						..Default::default()
					},
				)
			};
			let planned = target
				.plan_files(&text, &route, &RunOptions::default(), None)
				.unwrap();
			let skipped = &planned.restore_plan().skipped_operations;
			assert_eq!(skipped.len(), 1);
			assert_eq!(
				skipped[0].raw_path,
				"target/.git/hooks/snip-test-blocked"
			);
			assert_eq!(skipped[0].reason.as_str(), "UNRESOLVED_PATH");
			assert_eq!(
				skipped[0].detail,
				Some(SkipDetail::ProtectedGitMetadata)
			);
			let result = target
				.apply_files(
					&text,
					&route,
					&planned,
					&RestoreSelection {
						skip_existing: true,
						..Default::default()
					},
				)
				.unwrap();
			assert_eq!(
				result.planned_skipped_count, 1,
				"remote={remote}, mixed={mixed}"
			);
			assert_eq!(result.skipped_existing_count, usize::from(mixed));
			assert_eq!(result.total_skipped(), 1 + usize::from(mixed));
			assert_eq!(result.created_count, usize::from(mixed));
			assert!(result.errors.is_empty());
			assert!(!repo.join(".git/hooks/snip-test-blocked").exists());
			assert_eq!(
				fs::read_to_string(repo.join("existing.txt")).unwrap(),
				"keep"
			);
		}
	}
}

#[test]
fn commit_file_previews_match_local_virtual_baselines_without_writing() {
	use snip_core::transfer::ReplayFileSide;
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let src = tmp.path().join("src");
	let dst = tmp.path().join("dst");
	init_repo(&src);
	fs::write(src.join("common.txt"), "c1\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "first"]);
	let first = git_out(&src, &["rev-parse", "HEAD"]);
	fs::write(src.join("common.txt"), "c2\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "second"]);
	let tip = git_out(&src, &["rev-parse", "HEAD"]);
	let source = snip_core::gitsrc::Git::open(&src).unwrap();
	let export = snip_core::transfer::plan_commit_export_exact_with(
		&source,
		&tip,
		&[first, tip.clone()],
		&RunOptions::default(),
		snip_core::transfer::CLIPBOARD_PAYLOAD_MAX,
	)
	.unwrap();
	init_repo(&dst);
	fs::write(dst.join("common.txt"), "already\n").unwrap();
	let head = git_out(&dst, &["rev-parse", "HEAD"]);
	let status = git_out(&dst, &["status", "--porcelain"]);
	let w = worker(None);
	let (client, workspace) = open(&w, &dst);
	let targets = [
		PasteTarget::Local {
			dest: dst.clone(),
			bound: None,
		},
		PasteTarget::Remote {
			client: Arc::new(client),
			workspace,
			dest: String::new(),
		},
	];
	let mut previews = Vec::new();
	for target in &targets {
		let (plan, _prepared) = target
			.plan_commits(
				&export.text,
				export.payload.clone(),
				&RunOptions::default(),
			)
			.unwrap();
		let rows: Vec<_> = (0..2)
			.map(|index| {
				target
					.preview_commit_file(
						&plan,
						index,
						0,
						ReplayFileSide::Target,
						1024,
						&RunOptions::default(),
					)
					.unwrap()
			})
			.collect();
		previews.push(rows);
	}
	assert_eq!(previews[0], previews[1]);
	let rows = &previews[0];
	assert_eq!(rows[0].before, "already\n");
	assert_eq!(rows[0].after, "c1\n");
	assert_eq!(
		rows[1].before, "c1\n",
		"local and remote must use the preceding replay result"
	);
	assert_eq!(rows[1].after, "c2\n");
	assert!(rows.iter().all(|row| row.unavailable.is_none()));
	assert_eq!(
		fs::read_to_string(dst.join("common.txt")).unwrap(),
		"already\n"
	);
	assert_eq!(git_out(&dst, &["rev-parse", "HEAD"]), head);
	assert_eq!(git_out(&dst, &["status", "--porcelain"]), status);
	fs::write(dst.join("common.txt"), "changed externally\n").unwrap();
	for target in &targets {
		// Capture the old preview again from the saved snapshot, not from
		// the changed file. The payload itself remains on the master.
		let before =
			CommitReplayPreview::capture(&dst, &export.payload).unwrap();
		fs::write(dst.join("common.txt"), "changed once more\n").unwrap();
		let error = target
			.preview_commit_file(
				&before,
				0,
				0,
				ReplayFileSide::Target,
				1024,
				&RunOptions::default(),
			)
			.unwrap_err();
		assert!(error.is_stale(), "{error:?}");
		fs::write(dst.join("common.txt"), "changed externally\n").unwrap();
	}
}

#[test]
fn replay_destination_reads_are_typed_bounded_scoped_and_versioned() {
	use snip_core::transfer::{
		ReplayPreviewRead as Read, ReplayPreviewUnavailable as Why,
	};
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir(&ws).unwrap();
	fs::write(ws.join("text"), "original\n").unwrap();
	fs::write(ws.join("binary"), [0, 255]).unwrap();
	fs::write(ws.join("large"), vec![b'a'; 1025]).unwrap();
	fs::create_dir(ws.join("directory")).unwrap();
	fs::write(tmp.path().join("outside"), "private").unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let path = |name: &str| Path::new(&id).join(name);
	for (name, expected) in [
		("text", Read::Text("original\n".into())),
		("binary", Read::Unavailable(Why::Binary)),
		("large", Read::Unavailable(Why::TooLarge)),
		("directory", Read::Unavailable(Why::Directory)),
	] {
		assert_eq!(
			client
				.read_replay_preview(&id, &path(name), 1024, None)
				.unwrap(),
			expected,
			"{name}"
		);
	}
	assert_eq!(
		refused(client.read_replay_preview(
			&id,
			&path("../outside"),
			1024,
			None
		)),
		ErrorCode::BadRequest
	);
	assert_eq!(
		refused(client.read_replay_preview(
			&id,
			&tmp.path().join("outside"),
			1024,
			None
		)),
		ErrorCode::BadRequest
	);
	#[cfg(unix)]
	{
		std::os::unix::fs::symlink(ws.join("text"), ws.join("link")).unwrap();
		assert_eq!(
			client
				.read_replay_preview(&id, &path("link"), 1024, None)
				.unwrap(),
			Read::Unavailable(Why::Symlink)
		);
		std::os::unix::fs::symlink(
			tmp.path().join("outside"),
			ws.join("escape"),
		)
		.unwrap();
		assert_eq!(
			refused(client.read_replay_preview(
				&id,
				&path("escape"),
				1024,
				None
			)),
			ErrorCode::Forbidden
		);
	}
	let cancel = snip_core::gitrun::CancelToken::new();
	cancel.cancel();
	assert!(matches!(
		client.read_replay_preview(&id, &path("text"), 1024, Some(&cancel)),
		Err(RemoteError::Cancelled)
	));
	let old = worker(Some(5));
	let (old_client, old_id) = open(&old, &ws);
	assert!(matches!(
		old_client.read_replay_preview(&old_id, &path("text"), 1024, None),
		Err(RemoteError::WorkerTooOld {
			have: 5,
			need: 6,
			..
		})
	));
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
	// A legacy master echoes its old FilePlan shape. Display-only source
	// references must not turn an otherwise identical replay into stale.
	let mut legacy =
		serde_json::to_value(snip_remote::ReplayExpect::of(&preview)).unwrap();
	for commit in legacy["plan"]["commits"].as_array_mut().unwrap() {
		for file in commit["files"].as_array_mut().unwrap() {
			let file = file.as_object_mut().unwrap();
			file.remove("before");
			file.remove("oldBefore");
		}
	}
	let legacy: snip_remote::ReplayExpect =
		serde_json::from_value(legacy).unwrap();
	let old_preview = CommitReplayPreview::from_parts(
		legacy.destination,
		payload.clone(),
		legacy.plan,
		legacy.freshness,
	);
	client
		.replay_apply(&id, "dst", &text, &old_preview, true, None)
		.unwrap();
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
fn prepared_replay_requests_keep_the_token_through_checks_and_consume_it_on_apply(
) {
	use snip_remote::proto::{ReplayPrepared, Request, Response};
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
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "change a"]);
	let tip = git_out(&src, &["rev-parse", "HEAD"]);

	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let text = client
		.export_commits(&id, "src", &tip, vec![tip.clone(), first], None)
		.unwrap()
		.text;
	let dst = ws.join("dst");
	init_repo(&dst);

	let prepare = |client: &Client| -> ReplayPrepared {
		match client
			.call(&Request::PrepareReplay {
				workspace: id.clone(),
				dest: "dst".into(),
				text: text.clone(),
				retain: true,
			})
			.unwrap()
		{
			Response::ReplayPrepared(p) => p,
			other => panic!("expected a prepared replay, got {other:?}"),
		}
	};
	let check = |client: &Client, token: &str, digest: &str| {
		client.call(&Request::ApplyPreparedReplay {
			workspace: id.clone(),
			dest: "dst".into(),
			token: token.into(),
			digest: digest.into(),
			check_only: true,
		})
	};
	let apply = |client: &Client, token: &str, digest: &str| {
		client.call(&Request::ApplyPreparedReplay {
			workspace: id.clone(),
			dest: "dst".into(),
			token: token.into(),
			digest: digest.into(),
			check_only: false,
		})
	};

	let p = prepare(&client);
	let token = p.token.clone().expect("retained token");
	// A check keeps the token: two checks in a row both pass, and nothing
	// is written.
	assert!(matches!(
		check(&client, &token, &p.digest),
		Ok(Response::Fresh)
	));
	assert!(matches!(
		check(&client, &token, &p.digest),
		Ok(Response::Fresh)
	));
	assert_eq!(git_out(&dst, &["rev-list", "--count", "HEAD"]), "1");

	// A wrong digest is refused as stale and consumes the token.
	assert_eq!(
		refused(check(&client, &token, "not-the-digest")),
		ErrorCode::Stale
	);
	let p = prepare(&client);
	let token = p.token.clone().expect("retained token");
	match apply(&client, &token, &p.digest).unwrap() {
		Response::Replayed(r) => {
			assert_eq!(r.created.len(), 2, "{r:?}");
			assert!(r.failure.is_none());
		}
		other => panic!("expected a replay, got {other:?}"),
	}
	assert_eq!(git_out(&dst, &["rev-list", "--count", "HEAD"]), "3");

	// The consumed token is gone for good.
	match apply(&client, &token, &p.digest) {
		Err(RemoteError::Refused { code, message, .. }) => {
			assert_eq!(code, ErrorCode::Stale);
			assert!(message.contains("expired, was consumed"), "{message}");
		}
		other => panic!("expected a refusal, got {other:?}"),
	}

	// A worker older than protocol 8 refuses the prepared replay outright.
	let old = worker(Some(7));
	let (old_client, _) = open(&old, &ws);
	match old_client.call(&Request::PrepareReplay {
		workspace: id.clone(),
		dest: "dst".into(),
		text: text.clone(),
		retain: true,
	}) {
		Err(RemoteError::WorkerTooOld { .. }) => {}
		other => panic!("expected too old, got {other:?}"),
	}
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

/// `real` spelled through a symlink where the OS has them: a paste must
/// not depend on the spelling the user gave (AGENTS.md, cross-platform).
#[cfg(unix)]
fn spelled(tmp: &Path, real: &Path, name: &str) -> PathBuf {
	let link = tmp.join(name);
	std::os::unix::fs::symlink(real, &link).unwrap();
	link
}

#[cfg(not(unix))]
fn spelled(_tmp: &Path, real: &Path, _name: &str) -> PathBuf {
	real.to_path_buf()
}

#[test]
fn a_paste_target_writes_the_same_files_locally_and_on_a_worker() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let local_real = tmp.path().join("local_real");
	for dir in [&ws, &local_real] {
		fs::create_dir_all(dir.join("other")).unwrap();
		fs::create_dir_all(dir.join("src")).unwrap();
		fs::write(dir.join("a.txt"), "old\n").unwrap();
	}
	let local_dest = spelled(tmp.path(), &local_real, "local");
	let text = [
		entry("a.txt", "new"),
		entry("src/b.rs", "fn main() {}"),
		entry("lib/c.txt", "c"),
	]
	.concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let local = PasteTarget::Local {
		dest: local_dest.clone(),
		bound: None,
	};
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: String::new(),
	};
	let local_route = PasteRoute {
		prefixes: vec![("lib".into(), local_dest.join("other"))],
		..Default::default()
	};
	let remote_route = PasteRoute {
		prefixes: vec![("lib".into(), PathBuf::from("other"))],
		..Default::default()
	};
	let opts = RunOptions::default();
	let lp = local.plan_files(&text, &local_route, &opts, None).unwrap();
	let rp = remote
		.plan_files(&text, &remote_route, &opts, None)
		.unwrap();
	let creates = |p: &FilePastePlan| {
		p.restore_plan()
			.create_operations
			.iter()
			.map(|o| o.relative_path.clone())
			.collect::<Vec<_>>()
	};
	assert_eq!(creates(&lp), creates(&rp));
	assert!(!creates(&lp).is_empty());

	let sel = unchecked(&[]);
	let lres = local.apply_files(&text, &local_route, &lp, &sel).unwrap();
	let rres = remote.apply_files(&text, &remote_route, &rp, &sel).unwrap();
	assert_eq!(lres, rres);
	for rel in ["a.txt", "src/b.rs", "other/c.txt"] {
		assert_eq!(
			fs::read(ws.join(rel)).unwrap(),
			fs::read(local_real.join(rel)).unwrap(),
			"{rel} differs between the local paste and the worker's"
		);
	}
}

/// What a paste says about the restore base, as the CLI and the app hear it,
/// is the same wherever the engine runs, also when it refuses the plan.
#[test]
fn a_refused_plan_notes_the_same_restore_base_locally_and_on_a_worker() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let remote_dest = ws.join("proj");
	let local_dest = tmp.path().join("local").join("proj");
	fs::create_dir_all(&remote_dest).unwrap();
	fs::create_dir_all(&local_dest).unwrap();
	// Adjusted, `proj/a.txt` and `a.txt` are one file; as they stand the two
	// are not.
	let text = format!(
		"// clipcode-root: work\n{}{}",
		entry("proj/a.txt", "A"),
		entry("a.txt", "B")
	);
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let local = PasteTarget::Local {
		dest: local_dest,
		bound: None,
	};
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: "proj".into(),
	};
	let opts = RunOptions::default();
	let strip = snip_core::restore::RestoreBase::Strip {
		segment: "proj".into(),
	};
	for adjust_paths in [true, false] {
		let route = PasteRoute {
			adjust_paths,
			..Default::default()
		};
		let mut answers = Vec::new();
		for target in [&local, &remote] {
			let mut heard = Vec::new();
			let mut note = |s: &snip_core::restore::RestoreBaseSuggestion| {
				heard.push(s.base.clone())
			};
			let planned =
				target.plan_files(&text, &route, &opts, Some(&mut note));
			answers.push((heard, planned.map(|_| ())));
		}
		let [(local_heard, local_planned), (remote_heard, remote_planned)] =
			<[_; 2]>::try_from(answers).unwrap();
		assert_eq!(local_heard, vec![strip.clone()], "adjust={adjust_paths}");
		assert_eq!(remote_heard, local_heard, "adjust={adjust_paths}");
		if adjust_paths {
			assert!(
				local_planned.as_ref().is_err_and(PasteError::is_collision),
				"{local_planned:?}"
			);
			assert!(
				remote_planned.as_ref().is_err_and(PasteError::is_collision),
				"{remote_planned:?}"
			);
		} else {
			assert!(local_planned.is_ok(), "{local_planned:?}");
			assert!(remote_planned.is_ok(), "{remote_planned:?}");
		}
	}
}

#[test]
fn a_paste_target_replays_the_same_commits_locally_and_on_a_worker() {
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
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();

	let dst = ws.join("dst");
	let local = tmp.path().join("local");
	init_repo(&dst);
	init_repo(&local);
	let local_target = PasteTarget::Local {
		dest: local.clone(),
		bound: None,
	};
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: "dst".into(),
	};
	for target in [&local_target, &remote] {
		let (preview, prepared) = target
			.plan_commits(&text, payload.clone(), &RunOptions::default())
			.unwrap();
		target.check_commits(&text, &preview, &prepared).unwrap();
		let result = target.apply_commits(&text, &preview, &prepared).unwrap();
		assert_eq!(result.created.len(), 2, "{result:?}");
		assert!(result.failure.is_none(), "{result:?}");
	}
	assert_eq!(history(&dst), history(&local));
	assert_eq!(fs::read(dst.join("d/b.txt")).unwrap(), b"b\n");
	assert_eq!(fs::read(local.join("d/b.txt")).unwrap(), b"b\n");
}

#[test]
fn a_paste_target_refuses_a_changed_destination_as_stale_on_both_sides() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let local_real = tmp.path().join("local_real");
	for dir in [&ws, &local_real] {
		fs::create_dir_all(dir).unwrap();
		fs::write(dir.join("a.txt"), "old\n").unwrap();
	}
	let local_dest = spelled(tmp.path(), &local_real, "local");
	let text = entry("a.txt", "new");
	let route = PasteRoute::default();
	let opts = RunOptions::default();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let local = PasteTarget::Local {
		dest: local_dest.clone(),
		bound: None,
	};
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: String::new(),
	};
	let lp = local.plan_files(&text, &route, &opts, None).unwrap();
	let rp = remote.plan_files(&text, &route, &opts, None).unwrap();
	for dir in [&ws, &local_real] {
		fs::write(dir.join("a.txt"), "external\n").unwrap();
	}
	let sel = unchecked(&[]);
	let mut reasons = Vec::new();
	for (name, target, plan) in
		[("local", &local, &lp), ("remote", &remote, &rp)]
	{
		let err = target
			.apply_files(&text, &route, plan, &sel)
			.err()
			.unwrap_or_else(|| {
				panic!("the {name} target wrote over a changed destination")
			});
		assert!(err.is_stale(), "{name}: {err:?}");
		// Local and remote say what changed the same way: as data, from the
		// engine, not read back out of a sentence. Over the wire it is the
		// worker's own reason, relative to the destination.
		assert!(
			matches!(
				err.reason(),
				snip_remote::Reason::DestinationChanged {
					change: snip_remote::StaleChange::Created { .. }
						| snip_remote::StaleChange::Modified { .. }
				}
			),
			"{name}: {:?}",
			err.reason()
		);
		reasons.push(err.reason());
	}
	let (lr, rr) = (reasons[0].clone(), reasons[1].clone());
	assert_eq!(lr, rr, "local and remote name the same change");
	assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap(), "external\n");
	assert_eq!(
		fs::read_to_string(local_real.join("a.txt")).unwrap(),
		"external\n"
	);
}

#[test]
fn a_local_target_never_applies_a_workers_plan() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: String::new(),
	};
	let route = PasteRoute::default();
	let text = entry("n.txt", "x");
	let plan = remote
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();
	let local = PasteTarget::Local {
		dest: ws.clone(),
		bound: None,
	};
	let err = local
		.apply_files(&text, &route, &plan, &unchecked(&[]))
		.expect_err("a local target applied a worker's plan");
	assert!(err.is_stale(), "{err:?}");
	assert!(!ws.join("n.txt").exists());
}

/// A worker that hangs up on its next request, as a dead process would: a
/// call on the connection the master already holds sends its request and
/// hears no answer.
fn lose_the_answer(w: &Worker) {
	w.stop();
}

/// A commit payload from one repository, planned into another through a
/// `PasteTarget::Remote`: what the destination preview and Apply start from.
fn planned_replay(
	tmp: &Path,
	w: &Arc<Worker>,
) -> (PasteTarget, String, CommitReplayPreview, PreparedReplay) {
	let ws = tmp.join("ws");
	let src = ws.join("src");
	init_repo(&src);
	fs::write(src.join("a.txt"), "one\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "add a"]);
	let tip = git_out(&src, &["rev-parse", "HEAD"]);
	init_repo(&ws.join("dst"));
	let (client, id) = open(w, &ws);
	let text = client
		.export_commits(&id, "src", &tip, vec![tip.clone()], None)
		.unwrap()
		.text;
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();
	let target = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: "dst".into(),
	};
	let (preview, prepared) = target
		.plan_commits(&text, payload, &RunOptions::default())
		.unwrap();
	(target, text, preview, prepared)
}

/// #167: a remote file Apply whose answer is lost may have written, so
/// `PasteTarget` reports it as outcome unknown and not as a plain failure.
#[test]
fn a_lost_file_apply_answer_through_a_paste_target_is_outcome_unknown() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: String::new(),
	};
	let text = entry("a.txt", "new");
	let route = PasteRoute::default();
	let plan = remote
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();

	lose_the_answer(&w);
	let connections = w.started_connections_for_tests();
	let err = remote
		.apply_files(&text, &route, &plan, &unchecked(&[]))
		.expect_err("a hung-up worker cannot answer an Apply");
	assert!(matches!(err, PasteError::OutcomeUnknown(_)), "{err:?}");
	assert_eq!(
		w.started_connections_for_tests(),
		connections,
		"a lost Apply must not reconnect or resend"
	);
}

/// The same for a commit replay's Apply.
#[test]
fn a_lost_replay_apply_answer_through_a_paste_target_is_outcome_unknown() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(None);
	let (remote, text, preview, prepared) = planned_replay(tmp.path(), &w);

	lose_the_answer(&w);
	let connections = w.started_connections_for_tests();
	let err = remote
		.apply_commits(&text, &preview, &prepared)
		.expect_err("a hung-up worker cannot answer an Apply");
	assert!(matches!(err, PasteError::OutcomeUnknown(_)), "{err:?}");
	assert_eq!(
		w.started_connections_for_tests(),
		connections,
		"a lost replay Apply must not reconnect or resend"
	);
}

/// A check writes nothing, so a lost answer to it is a plain failure: only
/// an Apply reports outcome unknown.
#[test]
fn a_lost_replay_check_answer_through_a_paste_target_is_a_plain_failure() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(None);
	let (remote, text, preview, prepared) = planned_replay(tmp.path(), &w);

	lose_the_answer(&w);
	let err = remote
		.check_commits(&text, &preview, &prepared)
		.expect_err("a hung-up worker cannot answer a check");
	assert!(
		matches!(&err, PasteError::Remote(e) if e.outcome_unknown()),
		"{err:?}"
	);
}

#[test]
fn a_paste_target_refuses_a_changed_repo_under_a_non_repo_destination_on_both_sides(
) {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let local_real = tmp.path().join("local_real");
	for dir in [&ws, &local_real] {
		fs::create_dir_all(dir).unwrap();
		init_repo(&dir.join("repoA"));
	}
	// The two destinations must not lie in a repository themselves, or the
	// repository set this test is about changes with the temp directory.
	for dir in [&ws, &local_real] {
		assert!(
			matches!(
				snip_core::gitsrc::Git::open(dir),
				Err(snip_core::gitsrc::GitError::NotARepository(_))
			),
			"{dir:?} lies inside a git repository; this test needs a non-repo destination",
		);
	}
	let local_dest = spelled(tmp.path(), &local_real, "local");
	let text = entry("repoA/x.txt", "x");
	let route = PasteRoute::default();
	let opts = RunOptions::default();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let local = PasteTarget::Local {
		dest: local_dest,
		bound: None,
	};
	let remote = PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: String::new(),
	};
	let lp = local.plan_files(&text, &route, &opts, None).unwrap();
	let rp = remote.plan_files(&text, &route, &opts, None).unwrap();
	// A repository below the destination moves after the preview: the write
	// lock takes it, so the preview has to have seen it.
	for dir in [&ws, &local_real] {
		let repo = dir.join("repoA");
		fs::write(repo.join("other.txt"), "other\n").unwrap();
		git(&repo, &["add", "other.txt"]);
	}
	let sel = unchecked(&[]);
	let mut reasons = Vec::new();
	for (name, target, plan) in
		[("local", &local, &lp), ("remote", &remote, &rp)]
	{
		let err = target
			.apply_files(&text, &route, plan, &sel)
			.err()
			.unwrap_or_else(|| {
				panic!("the {name} target wrote into a changed repository")
			});
		assert!(err.is_stale(), "{name}: {err:?}");
		assert!(
			matches!(
				err.reason(),
				snip_remote::Reason::DestinationChanged {
					change: snip_remote::StaleChange::Repository
				}
			),
			"{name}: {:?}",
			err.reason()
		);
		reasons.push(err.reason());
	}
	assert_eq!(reasons[0], reasons[1], "both sides name the same change");
	for dir in [&ws, &local_real] {
		assert!(!dir.join("repoA/x.txt").exists());
	}
}

#[test]
fn a_file_where_a_new_folder_was_planned_is_stale_on_a_worker() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let text = [entry("new/a.txt", "a"), entry("keep.txt", "keep")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	fs::write(ws.join("new"), "someone's file").unwrap();
	let err = client
		.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		)
		.expect_err("a worker wrote around a file that took a folder's place");
	assert!(
		matches!(
			&err,
			RemoteError::Refused {
				code: ErrorCode::Stale,
				..
			}
		),
		"{err:?}"
	);
	assert_eq!(
		err.reason(),
		snip_remote::Reason::DestinationChanged {
			change: snip_remote::StaleChange::Created { rel: "new".into() }
		}
	);
	assert!(!ws.join("keep.txt").exists());
}

#[test]
fn a_folder_created_after_a_remote_preview_still_applies() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let text = entry("new/a.txt", "a");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	fs::create_dir(ws.join("new")).unwrap();
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
	assert_eq!(fs::read_to_string(ws.join("new/a.txt")).unwrap(), "a");
}

#[test]
fn a_forged_repo_key_is_refused_as_stale_before_git_runs_there() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let text = entry("a.txt", "a");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	let mut expect = planned.expect();
	// A folder that does not exist: git cannot be opened in it, so a worker
	// that ran git on it first would answer Io, not Stale.
	expect.freshness.repos.get_or_insert_default().insert(
		serde_json::from_value(serde_json::json!(ws.join("nowhere"))).unwrap(),
		snip_core::transfer::RepoFreshness {
			head_commit: None,
			head_ref: None,
			index_hash: None,
		},
	);
	let code = refused(client.import_apply(
		&id,
		"",
		&text,
		&mapping,
		&unchecked(&[]),
		&expect,
		None,
	));
	assert_eq!(code, ErrorCode::Stale);
	assert!(!ws.join("a.txt").exists());
}

#[test]
fn a_snapshot_naming_a_folder_outside_the_workspace_is_a_bad_request() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let text = entry("a.txt", "a");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	let elsewhere = tmp.path().join("elsewhere");

	let mut missing = planned.expect();
	missing
		.freshness
		.missing_dirs
		.get_or_insert_default()
		.insert(elsewhere.clone());
	let mut linked = planned.expect();
	linked
		.freshness
		.symlinked_dirs
		.get_or_insert_default()
		.insert(elsewhere.clone(), ws.clone());
	for (what, expect) in
		[("missing_dirs", missing), ("symlinked_dirs", linked)]
	{
		let code = refused(client.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&expect,
			None,
		));
		assert_eq!(code, ErrorCode::BadRequest, "{what}");
		assert!(!ws.join("a.txt").exists(), "{what}");
	}
}

#[cfg(unix)]
#[test]
fn a_symlink_repointed_after_a_remote_preview_is_stale_on_a_worker() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(ws.join("real1")).unwrap();
	fs::create_dir_all(ws.join("real2")).unwrap();
	std::os::unix::fs::symlink("real1", ws.join("link")).unwrap();
	let text = [entry("link/a.txt", "a"), entry("keep.txt", "keep")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	assert_eq!(
		planned
			.expect()
			.freshness
			.symlinked_dirs
			.as_ref()
			.map(|m| m.len()),
		Some(1)
	);
	fs::remove_file(ws.join("link")).unwrap();
	std::os::unix::fs::symlink("real2", ws.join("link")).unwrap();
	let err = client
		.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		)
		.expect_err("a worker wrote through a symlink that points elsewhere");
	assert!(
		matches!(
			&err,
			RemoteError::Refused {
				code: ErrorCode::Stale,
				..
			}
		),
		"{err:?}"
	);
	assert!(!ws.join("real2/a.txt").exists());
	assert!(!ws.join("real1/a.txt").exists());
	assert!(!ws.join("keep.txt").exists());
}

#[cfg(unix)]
#[test]
fn a_delete_through_a_symlink_repointed_after_a_remote_preview_is_stale_on_a_worker(
) {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(ws.join("real1")).unwrap();
	fs::create_dir_all(ws.join("real2")).unwrap();
	fs::write(ws.join("real1/a.txt"), "same\n").unwrap();
	std::os::unix::fs::symlink("real1", ws.join("link")).unwrap();
	let text = format!(
		"// File: [DELETED] link/a.txt\n{}",
		entry("keep.txt", "keep")
	);
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	assert_eq!(
		planned
			.expect()
			.freshness
			.symlinked_dirs
			.as_ref()
			.map(|m| m.len()),
		Some(1),
		"a delete planned through a symlink records it"
	);
	// A look-alike behind the symlink's new target: same bytes, same mtime.
	fs::write(ws.join("real2/a.txt"), "same\n").unwrap();
	let mtime = fs::metadata(ws.join("real1/a.txt"))
		.unwrap()
		.modified()
		.unwrap();
	fs::File::options()
		.write(true)
		.open(ws.join("real2/a.txt"))
		.unwrap()
		.set_modified(mtime)
		.unwrap();
	fs::remove_file(ws.join("link")).unwrap();
	std::os::unix::fs::symlink("real2", ws.join("link")).unwrap();
	let err = client
		.import_apply(
			&id,
			"",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		)
		.expect_err("a worker deleted through a symlink that points elsewhere");
	assert!(
		matches!(
			&err,
			RemoteError::Refused {
				code: ErrorCode::Stale,
				..
			}
		),
		"{err:?}"
	);
	assert!(ws.join("real2/a.txt").exists());
	assert!(ws.join("real1/a.txt").exists());
	assert!(!ws.join("keep.txt").exists());
}

#[test]
fn an_older_masters_snapshot_without_the_new_fields_still_applies() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	// The workspace is itself a repository, so the worker's own capture has
	// a non-empty `repos` to differ from the snapshot's absent one.
	init_repo(&ws);
	let text = entry("new/a.txt", "a");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = PasteMapping::default();
	let planned = client.import_plan(&id, "", &text, &mapping, None).unwrap();
	assert!(planned.expect().freshness.repos.is_some());
	let mut json = serde_json::to_value(planned.expect()).unwrap();
	let freshness = json["freshness"].as_object_mut().unwrap();
	freshness.remove("repos");
	freshness.remove("missing_dirs");
	freshness.remove("symlinked_dirs");
	let expect: snip_remote::proto::ImportExpect =
		serde_json::from_value(json).unwrap();
	assert!(expect.freshness.repos.is_none());
	assert!(expect.freshness.missing_dirs.is_none());
	assert!(expect.freshness.symlinked_dirs.is_none());
	client
		.import_apply(&id, "", &text, &mapping, &unchecked(&[]), &expect, None)
		.unwrap();
	assert_eq!(fs::read_to_string(ws.join("new/a.txt")).unwrap(), "a");
}

/// `dir` moved aside and a symlink to `to` put in its place.
#[cfg(unix)]
fn swap_for_symlink(dir: &Path, to: &Path) {
	let mut aside = dir.as_os_str().to_owned();
	aside.push(".moved");
	fs::rename(dir, &aside).unwrap();
	std::os::unix::fs::symlink(to, dir).unwrap();
}

/// A destination folder replaced after the preview by a symlink to another
/// folder is stale on a worker, for a new file, an overwrite and a delete:
/// the write would land where the preview did not show.
#[cfg(unix)]
#[test]
fn a_destination_replaced_by_a_symlink_after_a_remote_preview_is_stale_on_a_worker(
) {
	let _serial = serial();
	for what in ["create", "overwrite", "delete"] {
		let tmp = tempfile::tempdir().unwrap();
		// The replacement lies outside the workspace: the worker's own
		// planning refuses it, so only the check of the preview's snapshot
		// can answer stale.
		let outer = dunce::canonicalize(tmp.path()).unwrap();
		let ws = outer.join("ws");
		fs::create_dir_all(&ws).unwrap();
		let selected = ws.join("selected");
		let ext = outer.join("ext");
		fs::create_dir_all(&selected).unwrap();
		fs::create_dir_all(&ext).unwrap();
		let text = match what {
			"create" => entry("new.txt", "new"),
			"overwrite" => entry("a.txt", "new"),
			_ => "// File: [DELETED] a.txt\n".to_string(),
		};
		if what != "create" {
			fs::write(selected.join("a.txt"), "old\n").unwrap();
			fs::write(ext.join("a.txt"), "old\n").unwrap();
			let mtime = fs::metadata(selected.join("a.txt"))
				.unwrap()
				.modified()
				.unwrap();
			fs::File::options()
				.write(true)
				.open(ext.join("a.txt"))
				.unwrap()
				.set_modified(mtime)
				.unwrap();
		}
		let w = worker(None);
		let (client, id) = open(&w, &ws);
		let mapping = PasteMapping::default();
		let planned = client
			.import_plan(&id, "selected", &text, &mapping, None)
			.unwrap();
		swap_for_symlink(&selected, &ext);
		let code = refused(client.import_apply(
			&id,
			"selected",
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		));
		assert_eq!(code, ErrorCode::Stale, "{what}");
		assert!(!ext.join("new.txt").exists(), "{what}");
		if what != "create" {
			assert_eq!(
				fs::read_to_string(ext.join("a.txt")).unwrap(),
				"old\n",
				"{what}"
			);
		}
	}
}

/// The same for a folder above the destination and for a folder a prefix is
/// routed to.
#[cfg(unix)]
#[test]
fn an_ancestor_or_prefix_folder_replaced_after_a_remote_preview_is_stale_on_a_worker(
) {
	let _serial = serial();
	for what in ["ancestor", "prefix"] {
		let tmp = tempfile::tempdir().unwrap();
		// The replacement lies outside the workspace: the worker's own
		// planning refuses it, so only the check of the preview's snapshot
		// can answer stale.
		let outer = dunce::canonicalize(tmp.path()).unwrap();
		let ws = outer.join("ws");
		fs::create_dir_all(&ws).unwrap();
		let ext = outer.join("ext");
		fs::create_dir_all(ext.join("b/selected")).unwrap();
		fs::create_dir_all(ws.join("a/b/selected")).unwrap();
		fs::create_dir_all(ws.join("other")).unwrap();
		fs::create_dir_all(ws.join("primary")).unwrap();
		let (dest, text, mapping) = if what == "ancestor" {
			(
				"a/b/selected",
				entry("new.txt", "new"),
				PasteMapping::default(),
			)
		} else {
			let mapping = PasteMapping {
				prefixes: vec![("p".into(), "other".into())],
				..Default::default()
			};
			(
				"primary",
				[entry("p/x.txt", "x"), entry("y.txt", "y")].concat(),
				mapping,
			)
		};
		let w = worker(None);
		let (client, id) = open(&w, &ws);
		let planned = client
			.import_plan(&id, dest, &text, &mapping, None)
			.unwrap();
		if what == "ancestor" {
			swap_for_symlink(&ws.join("a"), &ext);
		} else {
			swap_for_symlink(&ws.join("other"), &ext);
		}
		let code = refused(client.import_apply(
			&id,
			dest,
			&text,
			&mapping,
			&unchecked(&[]),
			&planned.expect(),
			None,
		));
		assert_eq!(code, ErrorCode::Stale, "{what}");
		assert!(!ext.join("b/selected/new.txt").exists(), "{what}");
		assert!(!ext.join("x.txt").exists(), "{what}");
		assert!(!ws.join("primary/y.txt").exists(), "{what}");
	}
}

/// #241 guard (R3/R4): a worker that was not asked for folders that may not
/// exist (the app's paste, or an older protocol) still refuses a missing
/// destination (`NotFound`: the folder does not resolve), and makes nothing.
#[test]
fn a_worker_refuses_a_missing_folder_it_was_not_asked_to_make() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let text = entry("a.txt", "x");
	let before: Vec<_> = fs::read_dir(&ws).unwrap().collect();
	assert!(before.is_empty());
	for max_protocol in [None, Some(8)] {
		let w = worker(max_protocol);
		let (client, id) = open(&w, &ws);
		let planned = client.import_plan(
			&id,
			"not/yet",
			&text,
			&PasteMapping::default(),
			None,
		);
		assert_eq!(refused(planned), ErrorCode::NotFound);
		let remote = PasteTarget::Remote {
			client: Arc::new(client),
			workspace: id,
			dest: "not/yet".into(),
		};
		match remote.plan_files(
			&text,
			&PasteRoute::default(),
			&RunOptions::default(),
			None,
		) {
			Err(PasteError::Remote(RemoteError::Refused {
				code: ErrorCode::NotFound,
				..
			})) => {}
			other => panic!("expected a NotFound refusal, got {other:?}"),
		}
		assert!(
			fs::read_dir(&ws).unwrap().next().is_none(),
			"the worker made a folder it was not asked to"
		);
	}
}

// ---------------------------------------------------------------------------
// #241: a destination or prefix folder that does not exist yet
// ---------------------------------------------------------------------------

fn tree(dir: &Path) -> Vec<PathBuf> {
	fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
		for entry in fs::read_dir(dir).unwrap() {
			let path = entry.unwrap().path();
			out.push(path.clone());
			if path.is_dir() {
				walk(&path, out);
			}
		}
	}
	let mut out = Vec::new();
	walk(dir, &mut out);
	out.sort();
	out
}

fn new_dirs_route(prefixes: &[(&str, &str)]) -> PasteRoute {
	PasteRoute {
		prefixes: prefixes
			.iter()
			.map(|(p, f)| (p.to_string(), PathBuf::from(f)))
			.collect(),
		allow_new_dirs: true,
		..Default::default()
	}
}

fn remote_target(client: Client, id: String, dest: &str) -> PasteTarget {
	PasteTarget::Remote {
		client: Arc::new(client),
		workspace: id,
		dest: dest.into(),
	}
}

fn confirm(plan: &FilePastePlan) -> RestoreSelection {
	RestoreSelection {
		confirmed_new_dirs: plan
			.new_dirs_for(&RestoreSelection::default())
			.requested
			.into_iter()
			.collect(),
		..Default::default()
	}
}

fn not_confirmed(err: PasteError, count: u64) {
	match err {
		PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			reason: Some(reason),
			..
		}) => assert_eq!(
			*reason,
			snip_core::reason::Reason::NewDirsNotConfirmed { count }
		),
		other => panic!("expected NewDirsNotConfirmed, got {other:?}"),
	}
}

/// R1: a worker plans a folder that is not there without making it, wants
/// the user's agreement to make it, and makes it with it: through a prepared
/// preview, and through the direct re-plan Apply.
#[test]
fn a_worker_plans_a_missing_destination_and_makes_it_only_when_agreed() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let real = dunce::canonicalize(&ws).unwrap();
	let text = entry("a.txt", "A");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let target = remote_target(client, id, "not/yet");
	let route = new_dirs_route(&[]);
	let before = tree(&ws);
	let plan = target
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();
	assert_eq!(tree(&ws), before, "planning must not write");
	assert!(plan.uses_prepared_import());
	assert_eq!(
		plan.new_dirs().requested,
		vec![real.join("not"), real.join("not/yet")]
	);
	not_confirmed(
		target
			.apply_files(&text, &route, &plan, &RestoreSelection::default())
			.unwrap_err(),
		2,
	);
	assert_eq!(tree(&ws), before, "a refused apply must not write");

	// A refused apply used the one-use preview up: preview again.
	let plan = target
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();
	let result = target
		.apply_files(&text, &route, &plan, &confirm(&plan))
		.unwrap();
	assert_eq!(result.created_count, 1);
	assert_eq!(result.created_dirs, 2);
	assert_eq!(fs::read_to_string(ws.join("not/yet/a.txt")).unwrap(), "A");

	// The same without a retained preview: the worker plans again.
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = new_dirs_route(&[]).mapping();
	let planned = client
		.import_plan(&id, "not/yet", &text, &mapping, None)
		.unwrap();
	assert!(tree(&ws).is_empty());
	let none = client.import_apply(
		&id,
		"not/yet",
		&text,
		&mapping,
		&RestoreSelection::default(),
		&planned.expect(),
		None,
	);
	assert_eq!(refused(none), ErrorCode::BadRequest);
	assert!(tree(&ws).is_empty());
	let agreed = RestoreSelection {
		confirmed_new_dirs: planned
			.plan
			.new_dirs()
			.requested
			.into_iter()
			.collect(),
		..Default::default()
	};
	let result = client
		.import_apply(
			&id,
			"not/yet",
			&text,
			&mapping,
			&agreed,
			&planned.expect(),
			None,
		)
		.unwrap();
	assert_eq!(result.created_dirs, 2);
	assert_eq!(fs::read_to_string(ws.join("not/yet/a.txt")).unwrap(), "A");
}

/// A worker's preview raced: `new` is made after planning checked the
/// tails and before it told the named folders from the others (primary
/// `new/sub`, prefix `q` at `NEW/SUB` on a filesystem that folds case, so
/// planning took `SUB` for an incidental folder). The Apply that follows is
/// refused, unagreed, and makes nothing: the master's snapshot carries the
/// tails as they were placed, and the worker's own plan checks its own.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_folder_made_while_a_worker_plans_does_not_take_consent_away() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let real = dunce::canonicalize(&ws).unwrap();
	fs::write(real.join(".case-probe"), "").unwrap();
	assert!(
		real.join(".CASE-PROBE").exists(),
		"needs a filesystem that folds case"
	);
	fs::remove_file(real.join(".case-probe")).unwrap();
	let text = entry("q/b.txt", "B");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = new_dirs_route(&[("q", "NEW/SUB")]).mapping();
	let planned = {
		let new = real.join("new");
		let _hook =
			snip_core::transfer::test_hooks::after_tail_check(move || {
				fs::create_dir_all(&new).unwrap()
			});
		client
			.import_plan(&id, "new/sub", &text, &mapping, None)
			.unwrap()
	};
	let result = client.import_apply(
		&id,
		"new/sub",
		&text,
		&mapping,
		&RestoreSelection::default(),
		&planned.expect(),
		None,
	);
	assert!(
		matches!(refused(result), ErrorCode::Stale | ErrorCode::BadRequest),
		"an unagreed Apply must be refused"
	);
	assert_eq!(tree(&ws), vec![ws.join("new")]);
}

/// R2: the same payload into the same missing folder writes the same bytes
/// locally and on a worker.
#[test]
fn a_missing_destination_is_pasted_the_same_locally_and_on_a_worker() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let local = tmp.path().join("local");
	fs::create_dir_all(&ws).unwrap();
	fs::create_dir_all(&local).unwrap();
	let text = [entry("a.txt", "A"), entry("sub/b.txt", "B")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let remote = remote_target(client, id, "not/yet");
	let here = PasteTarget::Local {
		dest: local.join("not/yet"),
		bound: Some(local.clone()),
	};
	let route = new_dirs_route(&[]);
	let opts = RunOptions::default();
	let lp = here.plan_files(&text, &route, &opts, None).unwrap();
	let rp = remote.plan_files(&text, &route, &opts, None).unwrap();
	let rels = |p: &FilePastePlan| {
		p.restore_plan()
			.create_operations
			.iter()
			.map(|o| o.relative_path.clone())
			.collect::<Vec<_>>()
	};
	assert_eq!(rels(&lp), rels(&rp));
	let lres = here.apply_files(&text, &route, &lp, &confirm(&lp)).unwrap();
	let rres = remote
		.apply_files(&text, &route, &rp, &confirm(&rp))
		.unwrap();
	assert_eq!(lres, rres);
	for rel in ["not/yet/a.txt", "not/yet/sub/b.txt"] {
		assert_eq!(
			fs::read(ws.join(rel)).unwrap(),
			fs::read(local.join(rel)).unwrap(),
			"{rel}"
		);
	}
	// A local folder outside the bound is refused as such.
	let outside = tmp.path().join("outside");
	fs::create_dir_all(&outside).unwrap();
	let there = PasteTarget::Local {
		dest: outside.join("new"),
		bound: Some(local.clone()),
	};
	match there.plan_files(&text, &route, &opts, None) {
		Err(err @ PasteError::Destination { .. }) => assert!(matches!(
			err.reason(),
			snip_core::reason::Reason::OutsideRoot { .. }
		)),
		other => panic!("expected a refused destination, got {other:?}"),
	}
}

/// R4: a new worker on an older protocol refuses a missing folder, planning
/// nothing and making nothing, whatever the master asked.
#[test]
fn a_worker_on_protocol_8_refuses_a_missing_folder_even_when_asked() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let w = worker(Some(8));
	let (client, id) = open(&w, &ws);
	let text = entry("a.txt", "A");
	let mapping = new_dirs_route(&[]).mapping();
	assert!(mapping.allow_new_dirs);
	assert_eq!(
		refused(client.import_plan(&id, "not/yet", &text, &mapping, None)),
		ErrorCode::NotFound
	);
	assert!(tree(&ws).is_empty());
}

/// R5: through a paste target that same worker is "too old" when the folder
/// asked for is missing, and is refused as before for any other reason.
#[test]
fn a_worker_too_old_for_new_folders_says_so_only_when_a_folder_is_missing() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(ws.join("sub")).unwrap();
	let w = worker(Some(8));
	let (client, id) = open(&w, &ws);
	let target = remote_target(client, id, "not/yet");
	let text = entry("a.txt", "A");
	let opts = RunOptions::default();
	match target.plan_files(&text, &new_dirs_route(&[]), &opts, None) {
		Err(PasteError::Remote(RemoteError::WorkerTooOld {
			have: 8,
			need: 9,
			..
		})) => {}
		other => panic!("expected WorkerTooOld, got {other:?}"),
	}
	// A prefix folder that is missing is the same.
	let (client, id) = open(&w, &ws);
	let target = remote_target(client, id, "sub");
	match target.plan_files(
		&text,
		&new_dirs_route(&[("p", "nope")]),
		&opts,
		None,
	) {
		Err(PasteError::Remote(RemoteError::WorkerTooOld {
			need: 9, ..
		})) => {}
		other => panic!("expected WorkerTooOld, got {other:?}"),
	}
	// Every folder exists and the refusal is about something else.
	match target.plan_files(
		&text,
		&new_dirs_route(&[("a/b", "sub")]),
		&opts,
		None,
	) {
		Err(PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::BadRequest,
			..
		})) => {}
		other => panic!("expected the plain refusal, got {other:?}"),
	}
	assert!(tree(&ws).iter().all(|p| p.starts_with(ws.join("sub"))));
}

/// R6: the agreement is checked against the rows that are kept: unchecking
/// the row that needs a folder drops the need for it.
#[test]
fn consent_is_checked_against_the_rows_that_are_kept() {
	let _serial = serial();
	let text = [entry("a.txt", "A"), entry("p/b.txt", "B")].concat();
	for prepared in [true, false] {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		fs::create_dir_all(&ws).unwrap();
		let real = dunce::canonicalize(&ws).unwrap();
		let w = worker(None);
		let (client, id) = open(&w, &ws);
		let route = new_dirs_route(&[("p", "newp")]);
		let mapping = route.mapping();
		let client = Arc::new(client);
		let target = PasteTarget::Remote {
			client: client.clone(),
			workspace: id.clone(),
			dest: "not".into(),
		};
		// Previews, and applies, one way or the other. A refused Apply uses
		// a retained preview up, so each Apply has a preview of its own.
		let apply = |sel: &RestoreSelection| -> Result<_, PasteError> {
			if prepared {
				let plan = target.plan_files(
					&text,
					&route,
					&RunOptions::default(),
					None,
				)?;
				assert!(plan.uses_prepared_import());
				target.apply_files(&text, &route, &plan, sel)
			} else {
				let planned = client
					.import_plan(&id, "not", &text, &mapping, None)
					.map_err(PasteError::Remote)?;
				client
					.import_apply(
						&id,
						"not",
						&text,
						&mapping,
						sel,
						&planned.expect(),
						None,
					)
					.map_err(PasteError::Remote)
			}
		};
		let import = client
			.import_plan(&id, "not", &text, &mapping, None)
			.unwrap()
			.plan;
		assert_eq!(
			import.new_dirs().requested,
			vec![real.join("newp"), real.join("not")],
			"prepared: {prepared}"
		);
		let index = import
			.create_operations()
			.iter()
			.position(|o| o.relative_path.ends_with("b.txt"))
			.unwrap();
		// Agreed to the destination only, the prefix row kept: refused.
		let only_dest = RestoreSelection {
			confirmed_new_dirs: [real.join("not")].into(),
			..Default::default()
		};
		not_confirmed(apply(&only_dest).unwrap_err(), 1);
		assert!(tree(&ws).is_empty(), "prepared: {prepared}");
		// The prefix row unchecked: the destination alone is enough.
		let dropped = RestoreSelection {
			unchecked_creates: [index].into(),
			..only_dest
		};
		let result = apply(&dropped).unwrap();
		assert_eq!(result.created_count, 1, "prepared: {prepared}");
		assert_eq!(result.created_dirs, 1, "prepared: {prepared}");
		assert!(ws.join("not/a.txt").exists());
		assert!(!ws.join("newp").exists(), "prepared: {prepared}");
	}
}

/// R7: what a master sends back is checked and never trusted as a place to
/// make anything.
#[test]
fn a_masters_snapshot_and_agreement_cannot_make_a_worker_write_elsewhere() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let evil = tmp.path().join("evil");
	fs::create_dir_all(&ws).unwrap();
	let real = dunce::canonicalize(&ws).unwrap();
	let text = entry("a.txt", "A");
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let mapping = new_dirs_route(&[]).mapping();
	let planned = client
		.import_plan(&id, "not", &text, &mapping, None)
		.unwrap();
	let agreed = RestoreSelection {
		confirmed_new_dirs: [real.join("not")].into(),
		..Default::default()
	};
	let send = |expect: &snip_remote::ImportExpect, sel: &RestoreSelection| {
		client.import_apply(&id, "not", &text, &mapping, sel, expect, None)
	};

	// A requested folder outside the workspace is not one this paste could
	// have recorded.
	let mut forged = planned.expect();
	forged.freshness.requested_dirs = Some([evil.clone()].into());
	assert_eq!(refused(send(&forged, &agreed)), ErrorCode::BadRequest);

	// So is a placed tail outside it.
	let mut forged = planned.expect();
	forged.freshness.placed_tails = Some(vec![evil.join("x")]);
	assert_eq!(refused(send(&forged, &agreed)), ErrorCode::BadRequest);

	// A tail that is not the one the worker places again is stale.
	let mut moved = planned.expect();
	moved.freshness.placed_tails = Some(vec![real.join("other")]);
	assert_eq!(refused(send(&moved, &agreed)), ErrorCode::Stale);

	// A shrunken list is not the plan the worker makes again.
	let mut shrunk = planned.expect();
	shrunk.freshness.requested_dirs = Some(Default::default());
	assert_eq!(refused(send(&shrunk, &agreed)), ErrorCode::Stale);
	assert!(tree(&ws).is_empty());

	// An agreement naming a folder outside the workspace is only an
	// agreement: the worker makes what it plans, nothing the master names.
	let wide = RestoreSelection {
		confirmed_new_dirs: [real.join("not"), evil.clone()].into(),
		..Default::default()
	};
	let result = send(&planned.expect(), &wide).unwrap();
	assert_eq!(result.created_dirs, 1);
	assert!(!evil.exists());
	assert!(ws.join("not/a.txt").exists());
}

// ---------------------------------------------------------------------------
// #241 review: a path below the folder, planning that writes nothing, one
// identity for a new folder
// ---------------------------------------------------------------------------

/// R7: a payload path that names no file below the folder to make is
/// skipped, on a worker and locally, and the folder is never made as a file.
#[test]
fn a_path_with_no_file_name_is_not_the_missing_destination_on_either_side() {
	let _serial = serial();
	for raw in [".", "/", "p/", "p/."] {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		let local = tmp.path().join("local");
		fs::create_dir_all(&ws).unwrap();
		fs::create_dir_all(&local).unwrap();
		let text = entry(raw, "BODY");
		let w = worker(None);
		let (client, id) = open(&w, &ws);
		let remote = remote_target(client, id, "not/yet");
		let here = PasteTarget::Local {
			dest: local.join("not/yet"),
			bound: Some(local.clone()),
		};
		let (there, on_disk) = (new_dirs_route(&[("p", "not/yet")]), &local);
		let here_route = PasteRoute {
			prefixes: vec![("p".to_string(), on_disk.join("not/yet"))],
			..there.clone()
		};
		for (name, target, dir, route) in [
			("worker", &remote, &ws, &there),
			("local", &here, &local, &here_route),
		] {
			let opts = RunOptions::default();
			let plan = target.plan_files(&text, route, &opts, None).unwrap();
			assert!(
				plan.restore_plan().create_operations.is_empty(),
				"{name} {raw:?}: planned {:?}",
				plan.restore_plan().create_operations
			);
			let result = target
				.apply_files(&text, route, &plan, &confirm(&plan))
				.unwrap();
			assert_eq!(result.created_count, 0, "{name} {raw:?}");
			assert!(
				tree(dir).is_empty(),
				"{name} {raw:?}: wrote {:?}",
				tree(dir)
			);
		}
	}
}

/// R10: when the restore base is Add, a path with no file name in the payload
/// is not the folder Add names, on a worker and locally: it is skipped, and
/// the files beside it land where Add puts them.
#[test]
fn add_does_not_turn_a_path_without_a_file_name_into_its_folder_on_either_side()
{
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let local = tmp.path().join("local");
	for root in [&ws, &local] {
		fs::create_dir_all(root.join("proj/src/lib")).unwrap();
	}
	let text = [
		entry("lib/a.rs", "A"),
		entry("lib/b.rs", "B"),
		entry(".", "DOT"),
		entry("./", "SLASH"),
		entry("c.txt", "C"),
	]
	.concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let remote = remote_target(client, id, "proj");
	let here = PasteTarget::Local {
		dest: local.join("proj"),
		bound: None,
	};
	let route = PasteRoute {
		adjust_paths: true,
		..Default::default()
	};
	let expected = ["src/c.txt", "src/lib/a.rs", "src/lib/b.rs"];
	for (name, target, dir) in
		[("worker", &remote, &ws), ("local", &here, &local)]
	{
		let mut base = None;
		let mut note = |s: &snip_core::restore::RestoreBaseSuggestion| {
			base = Some(s.base.clone())
		};
		let plan = target
			.plan_files(&text, &route, &RunOptions::default(), Some(&mut note))
			.unwrap();
		assert_eq!(
			base,
			Some(snip_core::restore::RestoreBase::Add {
				prefix: "src".into()
			}),
			"{name}"
		);
		let mut planned: Vec<String> = plan
			.restore_plan()
			.create_operations
			.iter()
			.map(|op| op.relative_path.clone())
			.collect();
		planned.sort();
		assert_eq!(planned, expected, "{name}");
		let result = target
			.apply_files(&text, &route, &plan, &RestoreSelection::default())
			.unwrap();
		assert_eq!(result.created_count, 3, "{name}");
		for rel in expected {
			assert!(dir.join("proj").join(rel).is_file(), "{name} {rel}");
		}
		assert!(dir.join("proj/src/lib").is_dir(), "{name}");
	}
}

/// R8: a worker's planning does not write into the empty folder it only
/// reads, the mtime the proof: a probe file made and removed there moves it.
#[cfg(unix)]
#[test]
fn a_worker_plans_a_missing_destination_without_touching_its_anchor() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let target = remote_target(client, id, "new");
	let old = std::time::SystemTime::UNIX_EPOCH
		+ std::time::Duration::from_secs(1_000_000_000);
	let handle = fs::File::open(&ws).unwrap();
	handle.set_modified(old).unwrap();
	drop(handle);
	let before = fs::metadata(&ws).unwrap().modified().unwrap();
	let text = entry("a.txt", "A");
	let route = new_dirs_route(&[]);
	target
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();
	assert_eq!(
		fs::metadata(&ws).unwrap().modified().unwrap(),
		before,
		"the worker's planning wrote into the anchor"
	);
	assert!(tree(&ws).is_empty());
}

/// R9: two spellings of one new folder are one folder on a worker too: it
/// asks for it once, makes it once, and both files land.
#[test]
fn a_worker_makes_one_folder_of_two_spellings_of_it() {
	let _serial = serial();
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(ws.join("base")).unwrap();
	let ci = {
		let probe = ws.join("probe_x");
		fs::write(&probe, "x").unwrap();
		let ci = ws.join("PROBE_X").exists();
		fs::remove_file(&probe).unwrap();
		ci
	};
	let text = [entry("p/a.txt", "A"), entry("q/b.txt", "B")].concat();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let target = remote_target(client, id, "base");
	let route = new_dirs_route(&[("p", "new"), ("q", "NEW")]);
	let plan = target
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();
	let asked = plan.new_dirs().requested;
	assert_eq!(asked.len(), if ci { 1 } else { 2 }, "{asked:?}");
	let result = target
		.apply_files(&text, &route, &plan, &confirm(&plan))
		.unwrap();
	assert_eq!(result.created_count, 2);
	assert_eq!(result.created_dirs, asked.len());
	assert_eq!(fs::read_to_string(ws.join("new/a.txt")).unwrap(), "A");
	let second = if ci { "new/b.txt" } else { "NEW/b.txt" };
	assert_eq!(fs::read_to_string(ws.join(second)).unwrap(), "B");
}
