//! A real worker and a real master in one process, over pipes. Every
//! master read has a timeout (client.rs), so a hang fails instead of
//! blocking the run.

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use snip_remote::fault::{
	Fault, FaultRule, Match, RequestKind, ALL_REQUEST_KINDS,
};
use snip_remote::proto::EntryKind;
use snip_remote::{
	Client, Connection, ErrorCode, RemoteError, RemoteHost, Request, Response,
	Transport, Worker, WorkerOptions,
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

/// A folder picker lists folders only, visible ones only; `list_dir` still
/// returns everything.
#[test]
fn list_folders_returns_only_visible_folders() {
	let tmp = tempfile::tempdir().unwrap();
	let root = tmp.path().join("ws");
	fs::create_dir_all(root.join("a")).unwrap();
	fs::create_dir_all(root.join("b")).unwrap();
	fs::create_dir_all(root.join(".hidden")).unwrap();
	fs::write(root.join("f.txt"), "f").unwrap();
	fs::write(root.join(".dot"), "d").unwrap();
	let w = worker();
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let ws = client
		.open_workspace(&root.display().to_string())
		.unwrap()
		.id;

	let (folders, truncated) = client.list_folders(&ws, "", None).unwrap();
	assert!(!truncated);
	let names: Vec<_> = folders.iter().map(|e| e.name.as_str()).collect();
	assert_eq!(names, ["a", "b"]);
	let (all, _) = client.list_dir(&ws, "", None).unwrap();
	assert_eq!(all.len(), 5, "list_dir keeps every entry: {all:?}");

	// A symlink to a folder inside the workspace is a folder to the picker.
	#[cfg(unix)]
	{
		std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
		let (folders, _) = client.list_folders(&ws, "", None).unwrap();
		let names: Vec<_> = folders.iter().map(|e| e.name.as_str()).collect();
		assert_eq!(names, ["a", "b", "link"]);
		assert!(folders.iter().all(|e| e.directory));
	}
}

/// Longer than a heartbeat (1 s), shorter than the master's silence bound
/// before a reply starts (5 s). A floor, not a deadline, so it is not scaled.
const SLOW_WORK: Duration = Duration::from_millis(2500);

fn delay_rule(kind: RequestKind) -> FaultRule {
	FaultRule {
		on: Match::Kind(kind),
		workspace: None,
		skip: 0,
		times: 1,
		fault: Fault::Delay(SLOW_WORK),
	}
}

/// Opens `root` on `w` and returns the workspace id, with no fault armed.
fn open_ws(w: &Arc<Worker>, root: &std::path::Path) -> String {
	let mut conn =
		Connection::open(&Transport::InProcess(w.clone()), "t").unwrap();
	match conn
		.call(
			&Request::OpenWorkspace {
				path: root.display().to_string(),
			},
			None,
			Duration::from_secs(30),
		)
		.unwrap()
	{
		Response::Workspace(ws) => ws.id,
		other => panic!("expected a workspace, got {other:?}"),
	}
}

/// Whether the dispatch answers this request from `State::handle` with real
/// work behind it: the requests that must heartbeat while they run. Every
/// kind is named, so a new request has to decide here.
fn served_inline(kind: RequestKind) -> bool {
	match kind {
		RequestKind::OpenWorkspace
		| RequestKind::ListDir
		| RequestKind::Stat
		| RequestKind::Read => true,
		// Refused at once, with no work behind them.
		RequestKind::Hello | RequestKind::Write | RequestKind::Rename => false,
		// Jobs: they heartbeat through `run_job`.
		RequestKind::ReplayRead
		| RequestKind::ScanRepos
		| RequestKind::GitView
		| RequestKind::Export
		| RequestKind::ExportCommits
		| RequestKind::ExportChanges
		| RequestKind::ImportPlan
		| RequestKind::PrepareImport
		| RequestKind::ApplyPrepared
		| RequestKind::ImportApply
		| RequestKind::ReplayPlan
		| RequestKind::ReplayApply
		| RequestKind::PrepareReplay
		| RequestKind::ApplyPreparedReplay => false,
		// Pieces of a request, never dispatched alone.
		RequestKind::Chunk
		| RequestKind::FrameChunk
		| RequestKind::FrameJoin => false,
	}
}

fn inline_request(
	kind: RequestKind,
	ws: &str,
	root: &std::path::Path,
) -> Request {
	match kind {
		RequestKind::OpenWorkspace => Request::OpenWorkspace {
			path: root.display().to_string(),
		},
		RequestKind::ListDir => Request::ListDir {
			workspace: ws.into(),
			path: "".into(),
			folders_only: false,
		},
		RequestKind::Stat => Request::Stat {
			workspace: ws.into(),
			path: "a.txt".into(),
		},
		RequestKind::Read => Request::Read {
			workspace: ws.into(),
			path: "a.txt".into(),
		},
		other => panic!("{other:?} is not an inline request this test sends"),
	}
}

/// Every request the dispatch answers itself, from a folder listing to a
/// file read, sends a heartbeat while it runs: a master hears from a live
/// worker within a second, and tells it from a dead connection (#191).
#[test]
fn every_inline_request_heartbeats_while_it_runs() {
	let tmp = tempfile::tempdir().unwrap();
	let root = tmp.path().join("ws");
	fs::create_dir_all(&root).unwrap();
	fs::write(root.join("a.txt"), "hello").unwrap();
	let w = worker();
	let ws = open_ws(&w, &root);
	let kinds: Vec<RequestKind> = ALL_REQUEST_KINDS
		.into_iter()
		.filter(|k| served_inline(*k))
		.collect();
	assert_eq!(kinds.len(), 4, "{kinds:?}");
	for kind in &kinds {
		w.inject_fault_for_tests(delay_rule(*kind));
	}
	let before: Vec<usize> = kinds
		.iter()
		.map(|k| w.requests_seen_for_tests(*k))
		.collect();
	let mut conns: Vec<Connection> = kinds
		.iter()
		.map(|_| {
			Connection::open(&Transport::InProcess(w.clone()), "t").unwrap()
		})
		.collect();
	std::thread::scope(|scope| {
		for (kind, conn) in kinds.iter().zip(conns.iter_mut()) {
			let request = inline_request(*kind, &ws, &root);
			let kind = *kind;
			scope.spawn(move || {
				let started = Instant::now();
				let reply = conn
					.call(&request, None, Duration::from_secs(30))
					.unwrap_or_else(|e| panic!("{kind:?}: {e}"));
				assert!(started.elapsed() >= SLOW_WORK, "{kind:?}");
				let right = match (&kind, &reply) {
					(RequestKind::OpenWorkspace, Response::Workspace(_))
					| (RequestKind::ListDir, Response::Dir { .. })
					| (RequestKind::Stat, Response::Stat(_)) => true,
					(RequestKind::Read, Response::Text { content }) => {
						content.as_deref() == Some("hello")
					}
					_ => false,
				};
				assert!(right, "{kind:?}: {reply:?}");
				assert!(
					conn.frames_seen() >= 2,
					"{kind:?} answered in {} frame(s), with no heartbeat before it",
					conn.frames_seen()
				);
			});
		}
	});
	for (kind, before) in kinds.iter().zip(before) {
		assert_eq!(w.requests_seen_for_tests(*kind), before + 1, "{kind:?}");
	}
}

/// A master of protocol 1 does not know `Pending`: it gets the answer alone,
/// however long the work took.
#[test]
fn a_protocol_1_master_gets_no_heartbeat_for_inline_requests() {
	let tmp = tempfile::tempdir().unwrap();
	let root = tmp.path().join("ws");
	fs::create_dir_all(&root).unwrap();
	fs::write(root.join("a.txt"), "hello").unwrap();
	let w = Arc::new(Worker::new(WorkerOptions {
		name: "old".into(),
		max_protocol: Some(1),
	}));
	let ws = open_ws(&w, &root);
	w.inject_fault_for_tests(delay_rule(RequestKind::Read));
	let mut conn =
		Connection::open(&Transport::InProcess(w.clone()), "t").unwrap();
	let reply = conn
		.call(
			&inline_request(RequestKind::Read, &ws, &root),
			None,
			Duration::from_secs(30),
		)
		.unwrap();
	assert!(matches!(reply, Response::Text { .. }), "{reply:?}");
	assert_eq!(conn.frames_seen(), 1);
}

/// A delayed job still heartbeats: the delay runs where the work runs.
#[test]
fn a_delayed_job_request_still_heartbeats() {
	let tmp = tempfile::tempdir().unwrap();
	let root = tmp.path().join("ws");
	fs::create_dir_all(&root).unwrap();
	let w = worker();
	let ws = open_ws(&w, &root);
	w.inject_fault_for_tests(delay_rule(RequestKind::ScanRepos));
	let mut conn =
		Connection::open(&Transport::InProcess(w.clone()), "t").unwrap();
	let started = Instant::now();
	let reply = conn
		.call(
			&Request::ScanRepos {
				workspace: ws,
				under: None,
			},
			None,
			Duration::from_secs(30),
		)
		.unwrap();
	assert!(matches!(reply, Response::Repos(_)), "{reply:?}");
	assert!(started.elapsed() >= SLOW_WORK);
	assert!(conn.frames_seen() >= 2, "{}", conn.frames_seen());
}

fn page_delay_rule(kind: RequestKind, d: Duration) -> FaultRule {
	FaultRule {
		on: Match::Kind(kind),
		workspace: None,
		skip: 0,
		times: 1,
		fault: Fault::PageDelay(d),
	}
}

fn scaled(d: Duration) -> Duration {
	let scale = std::env::var("SNIP_E2E_TIMEOUT_SCALE")
		.ok()
		.and_then(|v| v.parse::<u32>().ok())
		.unwrap_or(1)
		.max(1);
	d.saturating_mul(scale)
}

/// A listing that outlives its deadline stops walking the folder: the pages
/// after the deadline are never read, and the Timeout comes back about when
/// the page in hand ends, not when the whole folder was read (#191).
#[test]
fn a_folder_listing_stops_walking_at_its_deadline() {
	let tmp = tempfile::tempdir().unwrap();
	let root = tmp.path().join("ws");
	fs::create_dir_all(&root).unwrap();
	// 11 pages of 256 visited entries.
	for i in 0..2800 {
		fs::write(root.join(format!("f{i:05}")), "").unwrap();
	}
	let w = worker();
	let ws = open_ws(&w, &root);
	let deadline = scaled(Duration::from_millis(300));
	let page = scaled(Duration::from_millis(100));
	w.set_deadlines_for_tests(deadline, Duration::from_secs(60));
	w.inject_fault_for_tests(FaultRule {
		times: usize::MAX,
		..page_delay_rule(RequestKind::ListDir, page)
	});
	let mut conn =
		Connection::open(&Transport::InProcess(w.clone()), "t").unwrap();
	let started = Instant::now();
	let reply = conn.call(
		&inline_request(RequestKind::ListDir, &ws, &root),
		None,
		Duration::from_secs(30),
	);
	let took = started.elapsed();
	assert_eq!(refused(reply), ErrorCode::Timeout);
	assert!(
		took < deadline + page * 5,
		"the listing read on after its deadline: {took:?}"
	);
}

/// Whatever an inline request is doing when its deadline passes, a call
/// that cannot be interrupted (a slow disk) keeps the master hearing from
/// the worker until it ends, and the answer is the Timeout (#191).
#[test]
fn every_inline_request_past_its_deadline_heartbeats_then_times_out() {
	let tmp = tempfile::tempdir().unwrap();
	let root = tmp.path().join("ws");
	fs::create_dir_all(&root).unwrap();
	fs::write(root.join("a.txt"), "hello").unwrap();
	let w = worker();
	let ws = open_ws(&w, &root);
	let deadline = scaled(Duration::from_millis(400));
	w.set_deadlines_for_tests(deadline, Duration::from_secs(60));
	// The work outlives the deadline by more than a heartbeat at any scale.
	let slow = SLOW_WORK.max(deadline + Duration::from_secs(2));
	let kinds: Vec<RequestKind> = ALL_REQUEST_KINDS
		.into_iter()
		.filter(|k| served_inline(*k))
		.collect();
	assert_eq!(kinds.len(), 4, "{kinds:?}");
	for kind in &kinds {
		w.inject_fault_for_tests(page_delay_rule(*kind, slow));
	}
	std::thread::scope(|scope| {
		for kind in &kinds {
			let mut conn =
				Connection::open(&Transport::InProcess(w.clone()), "t")
					.unwrap();
			let request = inline_request(*kind, &ws, &root);
			let kind = *kind;
			scope.spawn(move || {
				let reply = conn.call(&request, None, Duration::from_secs(30));
				assert_eq!(refused(reply), ErrorCode::Timeout, "{kind:?}");
				assert!(
					conn.frames_seen() >= 2,
					"{kind:?} went silent after its deadline: {} frame(s)",
					conn.frames_seen()
				);
			});
		}
	});
}
