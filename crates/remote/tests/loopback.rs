//! A real worker and a real master in one process, over pipes. Every
//! master read has a timeout (client.rs), so a hang fails instead of
//! blocking the run.

use std::fs;
use std::sync::Arc;

use snip_remote::proto::EntryKind;
use snip_remote::{
	Client, ErrorCode, RemoteError, RemoteHost, Request, Worker, WorkerOptions,
};

fn worker() -> Arc<Worker> {
	Arc::new(Worker::new(WorkerOptions {
		name: "win-worker".into(),
		..Default::default()
	}))
}

#[cfg(unix)]
fn client_for(w: &Arc<Worker>, root: &std::path::Path) -> (Client, String) {
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let ws = client.open_workspace(&root.display().to_string()).unwrap();
	(client, ws.id)
}

fn refused(result: Result<impl std::fmt::Debug, RemoteError>) -> ErrorCode {
	match result {
		Err(RemoteError::Refused { code, .. }) => code,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

#[test]
fn lists_stats_and_reads_inside_the_workspace_only() {
	let tmp = tempfile::tempdir().unwrap();
	let real = tmp.path().join("real-ws");
	fs::create_dir_all(real.join("src/nested/.git")).unwrap();
	fs::write(real.join("src/main.rs"), "fn main() {}\n").unwrap();
	fs::write(real.join("bin.dat"), [0u8, 1, 2, 255]).unwrap();
	fs::write(tmp.path().join("secret.txt"), "outside").unwrap();
	// The root is shared through a symlinked spelling, as macOS /var is.
	#[cfg(unix)]
	let shared = {
		let link = tmp.path().join("ws-link");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		std::os::unix::fs::symlink(
			tmp.path().join("secret.txt"),
			real.join("escape.txt"),
		)
		.unwrap();
		link
	};
	#[cfg(not(unix))]
	let shared = real.clone();
	let w = worker();
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let opened = client
		.open_workspace(&shared.display().to_string())
		.unwrap();
	assert_eq!(opened.name, "real-ws");
	assert_eq!(
		opened.id,
		dunce::canonicalize(&real).unwrap().display().to_string(),
		"a workspace is named by its real path"
	);
	let ws = opened.id.clone();

	let (root, truncated) = client.list_dir(&ws, "", None).unwrap();
	assert!(!truncated);
	let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
	assert_eq!(names[0], "src", "directories first: {names:?}");
	assert!(names.contains(&"bin.dat"));
	let (src, _) = client.list_dir(&ws, "src", None).unwrap();
	let nested = src.iter().find(|e| e.name == "nested").unwrap();
	assert!(nested.directory && nested.nested_repo);

	let stat = client.stat(&ws, "src/main.rs", None).unwrap();
	assert_eq!((stat.kind, stat.size), (EntryKind::File, 13));
	assert_eq!(
		client.read(&ws, "src/main.rs", None).unwrap().as_deref(),
		Some("fn main() {}\n")
	);
	assert_eq!(
		client.read(&ws, "bin.dat", None).unwrap(),
		None,
		"binary: no text"
	);

	// Containment: parent components, absolute paths, symlinks out.
	for path in ["../secret.txt", "src/../../secret.txt"] {
		assert_eq!(refused(client.read(&ws, path, None)), ErrorCode::Forbidden);
		assert_eq!(
			refused(client.list_dir(&ws, path, None)),
			ErrorCode::Forbidden
		);
	}
	let absolute = tmp.path().join("secret.txt").display().to_string();
	assert_eq!(
		refused(client.read(&ws, &absolute, None)),
		ErrorCode::Forbidden
	);
	#[cfg(unix)]
	assert_eq!(
		refused(client.read(&ws, "escape.txt", None)),
		ErrorCode::Forbidden
	);
	assert_eq!(
		refused(client.read(&ws, "missing.rs", None)),
		ErrorCode::NotFound
	);
	assert_eq!(
		refused(client.open_workspace("relative/dir")),
		ErrorCode::Forbidden
	);
	let missing = tmp.path().join("missing-ws").display().to_string();
	assert_eq!(
		refused(client.list_dir(&missing, "", None)),
		ErrorCode::NotFound
	);

	// GitView on a non-repo share returns NotARepository.
	assert_eq!(
		refused(client.call(&Request::GitView {
			workspace: ws.clone(),
			repo: "".into(),
			profile: snip_core::gitview::ReadProfile::Interactive,
			query: snip_remote::proto::GitQuery::ChangeList,
		})),
		ErrorCode::NotARepository
	);
}

/// A symlink to a folder inside the share lists as a folder and opens; one
/// that leads out of the share or into `.git` stays a plain entry the
/// master cannot open. The share is spelled through a symlink, as macOS
/// /var is.
#[cfg(unix)]
#[test]
fn a_folder_symlink_inside_the_share_lists_as_a_folder() {
	use std::os::unix::fs::symlink;
	let tmp = tempfile::tempdir().unwrap();
	let real = tmp.path().join("real-ws");
	fs::create_dir_all(real.join("src/deep")).unwrap();
	fs::create_dir_all(real.join(".git/objects")).unwrap();
	fs::create_dir_all(tmp.path().join("outside")).unwrap();
	fs::write(real.join("src/main.rs"), "fn main() {}\n").unwrap();
	fs::write(tmp.path().join("outside/secret.txt"), "outside").unwrap();
	symlink(real.join("src"), real.join("inner-link")).unwrap();
	symlink("src/deep", real.join("relative-link")).unwrap();
	symlink(tmp.path().join("outside"), real.join("escape-dir")).unwrap();
	symlink(real.join(".git"), real.join("git-link")).unwrap();
	symlink(real.join("src/main.rs"), real.join("file-link")).unwrap();
	let shared = tmp.path().join("ws-link");
	symlink(&real, &shared).unwrap();
	let w = worker();
	let (client, ws) = client_for(&w, &shared);

	let (root, _) = client.list_dir(&ws, "", None).unwrap();
	let entry = |name: &str| {
		root.iter()
			.find(|e| e.name == name)
			.unwrap_or_else(|| panic!("{name} not listed: {root:?}"))
	};
	for name in ["inner-link", "relative-link"] {
		assert!(entry(name).directory && entry(name).symlink, "{name}");
	}
	for name in ["escape-dir", "git-link", "file-link"] {
		assert!(!entry(name).directory && entry(name).symlink, "{name}");
	}
	let folders: Vec<_> = root
		.iter()
		.take_while(|e| e.directory)
		.map(|e| e.name.as_str())
		.collect();
	assert_eq!(folders, ["inner-link", "relative-link", "src"]);

	let (inner, _) = client.list_dir(&ws, "inner-link", None).unwrap();
	let names: Vec<_> = inner.iter().map(|e| e.name.as_str()).collect();
	assert_eq!(names, ["deep", "main.rs"]);
	assert_eq!(
		client
			.read(&ws, "inner-link/main.rs", None)
			.unwrap()
			.as_deref(),
		Some("fn main() {}\n")
	);
	assert_eq!(
		refused(client.list_dir(&ws, "escape-dir", None)),
		ErrorCode::Forbidden
	);
}

/// The whole listing arrives in ONE reply: every entry exactly once, in
/// sorted order, folders first, with no continuation to ask for.
#[test]
fn a_whole_listing_arrives_once_sorted() {
	let tmp = tempfile::tempdir().unwrap();
	let real = tmp.path().join("many");
	fs::create_dir_all(real.join("many/z-dir")).unwrap();
	for i in 0..1199 {
		fs::write(real.join("many").join(format!("f{i:04}")), "").unwrap();
	}
	fs::write(real.join("many/z-dir/inner"), "").unwrap();
	let w = worker();
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let ws = client
		.open_workspace(&real.display().to_string())
		.unwrap()
		.id;

	let (entries, truncated) = client.list_dir(&ws, "many", None).unwrap();
	assert!(!truncated, "1200 entries are under the cap");
	assert_eq!(entries.len(), 1200, "every entry arrives");
	let mut sorted: Vec<String> =
		entries.iter().map(|e| e.name.clone()).collect();
	sorted.sort();
	sorted.dedup();
	assert_eq!(sorted.len(), 1200, "no duplicates");
	// The reply is the sorted order itself, folders first.
	assert!(entries.first().is_some_and(|e| e.name == "z-dir"));
	assert!(entries.iter().any(|e| e.name == "f0000"));
	assert!(entries.last().is_some_and(|e| e.name == "f1198"));
}

/// A listing over the cap is ONE truncated reply: the master asks for
/// nothing more, because there is no continuation.
#[test]
fn a_listing_over_the_cap_reports_truncated_with_no_continuation() {
	let tmp = tempfile::tempdir().unwrap();
	let real = tmp.path().join("big");
	fs::create_dir_all(&real).unwrap();
	for i in 0..50 {
		fs::write(real.join(format!("f{i:03}")), "").unwrap();
	}
	let w = worker();
	w.set_dir_cap_for_tests(20);
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let ws = client
		.open_workspace(&real.display().to_string())
		.unwrap()
		.id;

	let (entries, truncated) = client.list_dir(&ws, "", None).unwrap();
	assert!(truncated);
	assert_eq!(entries.len(), 20, "exactly the cap, in sorted order");
	assert_eq!(entries[0].name, "f000");
	assert_eq!(entries[19].name, "f019");
}
