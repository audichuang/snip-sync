//! Copy through a worker: the payload the worker's copy engine makes, sent
//! back over the protocol (in chunks when large), equals a local copy.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use snip_core::format::parse_clipboard;
use snip_core::gitrun::RunOptions;
use snip_core::settings::Settings;
use snip_core::transfer::{
	copy_selection, CanonicalRootId, ExportItem, ExportSelection, SourceKind,
};
use snip_remote::proto::CHUNK_BYTES;
use snip_remote::{
	Client, ErrorCode, ExportTarget, RemoteError, RemoteHost, Worker,
	WorkerOptions,
};

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

fn file(path: &str) -> ExportTarget {
	ExportTarget {
		root: String::new(),
		path: path.into(),
		source: SourceKind::File,
		change_type: None,
	}
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

fn local_copy(root: &Path, paths: &[&str]) -> String {
	let id = CanonicalRootId::new(root).unwrap();
	let items = paths
		.iter()
		.map(|p| ExportItem {
			root: id.clone(),
			relative_path: p.to_string(),
			source: SourceKind::File,
			change_type: None,
			gitlink: false,
		})
		.collect();
	let sel = ExportSelection::new(
		vec![root.to_path_buf()],
		Some(root.to_path_buf()),
		items,
	)
	.unwrap();
	copy_selection(
		sel,
		&Settings::default(),
		10_000,
		&RunOptions::default(),
		|_| {},
	)
	.unwrap()
	.payload
}

#[test]
fn a_remote_copy_of_files_and_folders_equals_a_local_copy() {
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(ws.join("src/deep")).unwrap();
	fs::write(ws.join("a.txt"), "alpha\n").unwrap();
	fs::write(ws.join("src/deep/中文.txt"), "深層\r\nline\n").unwrap();
	fs::write(ws.join("src/no-eol.txt"), "tail").unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let out = client
		.export_files(
			&id,
			vec![file("a.txt"), file("src")],
			&Settings::default(),
			10_000,
			None,
		)
		.unwrap();
	assert_eq!(out.copied, 3);
	assert_eq!(out.payload, local_copy(&ws, &["a.txt", "src"]));
	let parsed =
		parse_clipboard(&out.payload, &Settings::default().header_format);
	let deep = parsed
		.iter()
		.find(|e| e.path == "src/deep/中文.txt")
		.expect("deep file");
	// The format carries CRLF as LF (spec §1).
	assert_eq!(deep.content, "深層\nline");
}

#[test]
fn a_copy_over_one_chunk_arrives_whole() {
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	// Each file under the 500 KB default limit; together several chunks,
	// with text JSON must escape and a multi-byte char on a boundary.
	let mut paths = Vec::new();
	for n in 0..8 {
		let body = format!("{}中\t\"q\"\\\n", "x".repeat(400_000 - n));
		let name = format!("f{n}.txt");
		fs::write(ws.join(&name), body).unwrap();
		paths.push(name);
	}
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	let targets = paths.iter().map(|p| file(p)).collect();
	let out = client
		.export_files(&id, targets, &Settings::default(), 10_000, None)
		.unwrap();
	assert!(out.payload.len() > 2 * CHUNK_BYTES, "{}", out.payload.len());
	let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
	assert_eq!(out.payload, local_copy(&ws, &refs));
}

#[test]
fn staged_changes_and_commits_copy_through_the_worker() {
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	let repo = ws.join("repo");
	fs::create_dir_all(&repo).unwrap();
	git(&repo, &["init", "-q", "-b", "main"]);
	fs::write(repo.join("a.txt"), "one\n").unwrap();
	git(&repo, &["add", "a.txt"]);
	git(&repo, &["commit", "-q", "-m", "first"]);
	fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
	git(&repo, &["add", "a.txt"]);
	let w = worker(None);
	let (client, id) = open(&w, &ws);

	let staged = client
		.export_files(
			&id,
			vec![ExportTarget {
				root: "repo".into(),
				path: "a.txt".into(),
				source: SourceKind::Staged,
				change_type: None,
			}],
			&Settings::default(),
			10_000,
			None,
		)
		.unwrap();
	assert_eq!(staged.copied, 1);
	assert!(staged.payload.contains("one\ntwo"), "{}", staged.payload);

	let head = git_out(&repo, &["rev-parse", "HEAD"]);
	let commits = client
		.export_commits(&id, "repo", &head, vec![head.clone()], None)
		.unwrap();
	assert_eq!(commits.commit_count, 1);
	assert_eq!(commits.file_count, 1);
	assert!(
		commits.text.contains("\"message\":\"first"),
		"{}",
		commits.text
	);
}

fn refused(result: Result<impl std::fmt::Debug, RemoteError>) -> ErrorCode {
	match result {
		Err(RemoteError::Refused { code, .. }) => code,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

#[test]
fn a_copy_never_leaves_its_workspace() {
	let tmp = tempfile::tempdir().unwrap();
	let ws = tmp.path().join("ws");
	fs::create_dir_all(&ws).unwrap();
	fs::write(tmp.path().join("secret.txt"), "outside").unwrap();
	let w = worker(None);
	let (client, id) = open(&w, &ws);
	for bad in ["../secret.txt", "/etc/passwd", "a/../../secret.txt"] {
		let code = refused(client.export_files(
			&id,
			vec![file(bad)],
			&Settings::default(),
			10_000,
			None,
		));
		assert_eq!(code, ErrorCode::BadRequest, "{bad}");
	}
	let code = refused(client.export_files(
		&id,
		vec![ExportTarget {
			root: "..".into(),
			path: "secret.txt".into(),
			source: SourceKind::File,
			change_type: None,
		}],
		&Settings::default(),
		10_000,
		None,
	));
	assert_eq!(code, ErrorCode::BadRequest);
}

#[test]
fn a_worker_without_copy_says_it_is_too_old() {
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(Some(2));
	let (client, id) = open(&w, tmp.path());
	match client.export_files(
		&id,
		vec![file("x")],
		&Settings::default(),
		10_000,
		None,
	) {
		Err(RemoteError::WorkerTooOld { have, need, .. }) => {
			assert_eq!((have, need), (2, 3));
		}
		other => panic!("expected too old, got {other:?}"),
	}
}

#[test]
fn whole_folder_copy_requires_protocol_5_but_named_targets_still_work() {
	let tmp = tempfile::tempdir().unwrap();
	fs::create_dir(tmp.path().join("sub")).unwrap();
	fs::write(tmp.path().join("a.txt"), "alpha\n").unwrap();
	fs::write(tmp.path().join("sub/b.txt"), "beta\n").unwrap();
	for version in [4, 5] {
		let w = worker(Some(version));
		let (client, id) = open(&w, tmp.path());
		let named = client
			.export_files(
				&id,
				vec![file("a.txt"), file("sub")],
				&Settings::default(),
				10_000,
				None,
			)
			.unwrap();
		assert_eq!(named.copied, 2);
		for root in ["", "sub"] {
			let target = ExportTarget {
				root: root.into(),
				..file("")
			};
			let result = client.export_files(
				&id,
				vec![target],
				&Settings::default(),
				10_000,
				None,
			);
			if version == 4 {
				let err =
					result.expect_err("whole-folder export needs protocol 5");
				assert!(
					matches!(
						err,
						RemoteError::WorkerTooOld {
							have: 4,
							need: 5,
							..
						}
					),
					"{err:?}"
				);
				let message = err.to_string();
				assert!(
					message.contains("too old")
						&& message.contains("update it there"),
					"{message}"
				);
				assert!(!message.contains("unsafe path"), "{message}");
			} else {
				assert_eq!(
					result.unwrap().copied,
					if root.is_empty() { 2 } else { 1 }
				);
			}
		}
	}
}
