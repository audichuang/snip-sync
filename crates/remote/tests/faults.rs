//! What a master does when a worker fails in each way the fault engine can
//! make it fail (#230, `snip_remote::fault`): every request kind that reads,
//! under every fault, on a fresh and on a reused connection. The expected
//! outcome is worked out from the fault and the connection, not written per
//! cell.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use snip_core::gitsrc::GitSource;
use snip_core::gitview::ReadProfile;
use snip_core::restore::RestoreSelection;
use snip_core::settings::Settings;
use snip_core::transfer::SourceKind;
use snip_remote::fault::{
	Class, Fault, FaultRule, Match, RequestKind, ALL_REQUEST_KINDS,
};
use snip_remote::proto::{
	GitQuery, ImportExpect, PasteMapping, PROTOCOL_VERSION,
};
use snip_remote::{
	Client, ConnectFailureKind, Connection, ErrorCode, ExportTarget,
	RemoteError, RemoteHost, Request, Response, Transport, WaitEnd, Worker,
	WorkerOptions,
};

fn scaled(d: Duration) -> Duration {
	let scale = std::env::var("SNIP_E2E_TIMEOUT_SCALE")
		.ok()
		.and_then(|v| v.parse::<u32>().ok())
		.unwrap_or(1)
		.max(1);
	d.saturating_mul(scale)
}

fn git(dir: &Path, args: &[&str]) -> String {
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
	String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn init_repo(dir: &Path) {
	fs::create_dir_all(dir).unwrap();
	git(dir, &["init", "-q", "-b", "main"]);
	fs::write(dir.join("README"), "base\n").unwrap();
	git(dir, &["add", "."]);
	git(dir, &["commit", "-q", "-m", "base"]);
}

/// A workspace with a repo that has history (`src`), one a commit payload
/// can be replayed onto (`dst`), and a worker serving it.
struct Fixture {
	_tmp: tempfile::TempDir,
	worker: Arc<Worker>,
	ws: String,
	root: PathBuf,
	tip: String,
	commits: String,
	expect: snip_remote::ReplayExpect,
}

impl Fixture {
	fn new() -> Self {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path().join("ws");
		let src = root.join("src");
		init_repo(&src);
		fs::write(src.join("a.txt"), "one\n").unwrap();
		git(&src, &["add", "."]);
		git(&src, &["commit", "-q", "-m", "add a"]);
		let first = git(&src, &["rev-parse", "HEAD"]);
		fs::write(src.join("a.txt"), "two\n").unwrap();
		git(&src, &["add", "."]);
		git(&src, &["commit", "-q", "-m", "change a"]);
		let tip = git(&src, &["rev-parse", "HEAD"]);
		// Something to copy as a change.
		fs::write(src.join("a.txt"), "three\n").unwrap();
		init_repo(&root.join("dst"));

		let worker = Arc::new(Worker::new(WorkerOptions {
			name: "faulty".into(),
			..Default::default()
		}));
		let client =
			Client::new(RemoteHost::in_process(worker.clone()), "t".into());
		let ws = client
			.open_workspace(&root.display().to_string())
			.unwrap()
			.id;
		let commits = client
			.export_commits(&ws, "src", &tip, vec![tip.clone(), first], None)
			.unwrap()
			.text;
		let payload =
			snip_core::commits::parse_commit_payload(&commits).unwrap();
		let preview = client
			.replay_plan(&ws, "dst", &commits, payload, None)
			.unwrap();
		Self {
			_tmp: tmp,
			worker,
			ws,
			root: dunce::canonicalize(&root).unwrap(),
			tip,
			commits,
			expect: snip_remote::ReplayExpect::of(&preview),
		}
	}

	fn client(&self) -> Client {
		Client::new(RemoteHost::in_process(self.worker.clone()), "t".into())
	}

	/// A request of `kind` the worker accepts, or none for the pieces of a
	/// request, which are never sent alone. The writers are valid in shape
	/// only: a refusal as stale is how they end.
	fn sample(&self, kind: RequestKind) -> Option<Request> {
		let workspace = || self.ws.clone();
		let text = "// File: new.txt\nhello\n".to_string();
		Some(match kind {
			RequestKind::Hello => Request::Hello {
				version: PROTOCOL_VERSION,
				name: "t".into(),
				max_version: None,
			},
			RequestKind::OpenWorkspace => Request::OpenWorkspace {
				path: self.root.display().to_string(),
			},
			RequestKind::ListDir => Request::ListDir {
				workspace: workspace(),
				path: String::new(),
				folders_only: false,
			},
			RequestKind::Stat => Request::Stat {
				workspace: workspace(),
				path: "src/a.txt".into(),
			},
			RequestKind::Read => Request::Read {
				workspace: workspace(),
				path: "src/a.txt".into(),
			},
			RequestKind::ReplayRead => Request::ReplayRead {
				workspace: workspace(),
				path: self.root.join("src/a.txt"),
				limit: 1024,
			},
			RequestKind::Write => Request::Write {
				workspace: workspace(),
				path: "w.txt".into(),
				content: "x".into(),
			},
			RequestKind::Rename => Request::Rename {
				workspace: workspace(),
				from: "a".into(),
				to: "b".into(),
			},
			RequestKind::ScanRepos => Request::ScanRepos {
				workspace: workspace(),
				under: None,
			},
			RequestKind::GitView => Request::GitView {
				workspace: workspace(),
				repo: "src".into(),
				profile: ReadProfile::Interactive,
				query: GitQuery::Refs,
			},
			RequestKind::Export => Request::Export {
				workspace: workspace(),
				items: vec![ExportTarget {
					root: "src".into(),
					path: "a.txt".into(),
					source: SourceKind::File,
					change_type: None,
				}],
				settings: Settings::default(),
				file_limit: 100,
			},
			RequestKind::ExportCommits => Request::ExportCommits {
				workspace: workspace(),
				repo: "src".into(),
				tip: self.tip.clone(),
				selected: vec![self.tip.clone()],
			},
			RequestKind::ExportChanges => Request::ExportChanges {
				workspace: workspace(),
				repo: "src".into(),
				source: GitSource::Working,
				settings: Settings::default(),
				file_limit: 100,
			},
			RequestKind::ImportPlan => Request::ImportPlan {
				workspace: workspace(),
				dest: "dst".into(),
				text,
				mapping: PasteMapping::default(),
			},
			RequestKind::PrepareImport => Request::PrepareImport {
				workspace: workspace(),
				dest: "dst".into(),
				text,
				mapping: PasteMapping::default(),
				retain: false,
			},
			RequestKind::ApplyPrepared => Request::ApplyPrepared {
				workspace: workspace(),
				dest: "dst".into(),
				token: "none".into(),
				selection: RestoreSelection::default(),
				expect: stale_expect(),
			},
			RequestKind::ImportApply => Request::ImportApply {
				workspace: workspace(),
				dest: "dst".into(),
				text,
				mapping: PasteMapping::default(),
				selection: RestoreSelection::default(),
				expect: stale_expect(),
			},
			RequestKind::ReplayPlan => Request::ReplayPlan {
				workspace: workspace(),
				dest: "dst".into(),
				text: self.commits.clone(),
			},
			RequestKind::ReplayApply => Request::ReplayApply {
				workspace: workspace(),
				dest: "dst".into(),
				text: self.commits.clone(),
				expect: self.expect.clone(),
				check_only: false,
			},
			RequestKind::PrepareReplay => Request::PrepareReplay {
				workspace: workspace(),
				dest: "dst".into(),
				text: self.commits.clone(),
				retain: false,
			},
			RequestKind::ApplyPreparedReplay => Request::ApplyPreparedReplay {
				workspace: workspace(),
				dest: "dst".into(),
				token: "none".into(),
				digest: "none".into(),
				check_only: false,
			},
			RequestKind::Chunk
			| RequestKind::FrameChunk
			| RequestKind::FrameJoin => return None,
		})
	}

	fn seen(&self, kind: RequestKind) -> usize {
		self.worker.requests_seen_for_tests(kind)
	}
}

fn stale_expect() -> ImportExpect {
	ImportExpect {
		digest: "none".into(),
		freshness: Default::default(),
	}
}

fn rule(kind: RequestKind, times: usize, fault: Fault) -> FaultRule {
	FaultRule {
		on: Match::Kind(kind),
		workspace: None,
		skip: 0,
		times,
		fault,
	}
}

/// The kinds a master only reads with: no write can be in doubt.
fn reads() -> Vec<RequestKind> {
	ALL_REQUEST_KINDS
		.into_iter()
		.filter(|k| matches!(k.class(), Class::Inline | Class::Job))
		.filter(|k| !k.may_write())
		.collect()
}

#[test]
fn every_request_kind_has_a_class_and_a_sample() {
	let fx = Fixture::new();
	let client = fx.client();
	let mut classes = std::collections::HashMap::<String, usize>::new();
	for kind in ALL_REQUEST_KINDS {
		let sample = fx.sample(kind);
		match kind.class() {
			Class::Framing => assert!(sample.is_none(), "{kind:?}"),
			_ => {
				let request =
					sample.unwrap_or_else(|| panic!("{kind:?} has no sample"));
				assert_eq!(request.kind(), kind);
			}
		}
		*classes.entry(format!("{:?}", kind.class())).or_default() += 1;
	}
	assert_eq!(classes["Inline"], 4);
	assert_eq!(classes["RefusedAtOnce"], 2);
	// A read's sample must work: the last step of every cell of the grid
	// calls it again and expects it to.
	for kind in reads() {
		let request = fx.sample(kind).unwrap();
		match client.call_with(&request, None, scaled(Duration::from_secs(30)))
		{
			Ok(Response::Error { code, message, .. }) => {
				panic!("{kind:?} was refused: {code:?} {message}")
			}
			Ok(_) => {}
			Err(e) => panic!("{kind:?} failed: {e:?}"),
		}
	}
}

/// What a fault does to one call, on a connection that is fresh or already
/// in the pool, as the client's rules make of it: a refusal arrives as is; a
/// reply that ends with a heartbeat already heard is not sent again; one
/// that ends with nothing heard is sent again on a reused connection.
enum Expect {
	Refused(ErrorCode),
	IoError { seen: usize },
	Resent,
}

fn expect(fault: &Fault, reused: bool) -> Expect {
	match fault {
		Fault::Refuse(code) => Expect::Refused(*code),
		Fault::HeartbeatThenHangUp => Expect::IoError { seen: 1 },
		Fault::HangUp | Fault::Garbage | Fault::TruncatedReply => {
			if reused {
				Expect::Resent
			} else {
				Expect::IoError { seen: 1 }
			}
		}
		other => panic!("{other:?} is not in the grid"),
	}
}

#[test]
fn every_read_survives_every_fault_and_the_client_stays_usable() {
	let fx = Fixture::new();
	let limit = scaled(Duration::from_secs(10));
	// A worker slow enough that a heartbeat precedes the work's reply (a
	// loaded machine, reproduced: work past the one-second heartbeat).
	fx.worker.set_heartbeat_for_tests(Duration::from_millis(1));
	let faults = [
		Fault::Refuse(ErrorCode::Busy),
		Fault::Refuse(ErrorCode::NotFound),
		Fault::HangUp,
		Fault::Garbage,
		Fault::TruncatedReply,
		Fault::HeartbeatThenHangUp,
	];
	let mut cells = 0;
	for kind in reads() {
		let request = fx.sample(kind).unwrap();
		for fault in &faults {
			for reused in [false, true] {
				let cell = format!("{kind:?} x {fault:?} x reused={reused}");
				let client = fx.client();
				if reused {
					client.stat(&fx.ws, "", None).unwrap();
				}
				fx.worker.clear_faults_for_tests();
				fx.worker
					.inject_fault_for_tests(rule(kind, 1, fault.clone()));
				let before = fx.seen(kind);
				let result = client.call_with(&request, None, limit);
				match (expect(fault, reused), &result) {
					(
						Expect::Refused(want),
						Err(RemoteError::Refused { code, .. }),
					) => {
						assert_eq!(*code, want, "{cell}");
						assert_eq!(fx.seen(kind) - before, 1, "{cell}");
					}
					(Expect::IoError { seen }, Err(RemoteError::Io(_))) => {
						assert_eq!(fx.seen(kind) - before, seen, "{cell}");
					}
					(Expect::Resent, Ok(response)) => {
						assert!(
							!matches!(response, Response::Error { .. }),
							"{cell}: {response:?}"
						);
						assert_eq!(fx.seen(kind) - before, 2, "{cell}");
					}
					(_, other) => panic!("{cell}: unexpected {other:?}"),
				}
				// The fault is spent: the same call works, on whichever
				// connection the client now holds.
				match client.call_with(&request, None, limit) {
					Ok(Response::Error { code, message, .. }) => {
						panic!("{cell}: then refused {code:?} {message}")
					}
					Ok(_) => {}
					Err(e) => panic!("{cell}: then failed {e:?}"),
				}
				cells += 1;
			}
		}
	}
	assert_eq!(cells, reads().len() * faults.len() * 2);
}

#[test]
fn a_write_that_lost_its_reply_is_never_resent() {
	let fx = Fixture::new();
	let limit = scaled(Duration::from_secs(10));
	for kind in ALL_REQUEST_KINDS.into_iter().filter(|k| k.may_write()) {
		let request = fx.sample(kind).unwrap();
		for reused in [false, true] {
			let cell = format!("{kind:?} x reused={reused}");
			let client = fx.client();
			if reused {
				client.stat(&fx.ws, "", None).unwrap();
			}
			fx.worker.clear_faults_for_tests();
			fx.worker.inject_fault_for_tests(rule(
				kind,
				1,
				Fault::RunThenHangUp,
			));
			let before = fx.seen(kind);
			let err = client
				.call_with(&request, None, limit)
				.expect_err(&format!("{cell}: the reply was dropped"));
			assert!(err.outcome_unknown(), "{cell}: {err:?}");
			assert_eq!(fx.seen(kind) - before, 1, "{cell}: sent twice");
		}
	}
}

#[test]
fn a_throttled_reply_under_the_idle_bound_arrives_whole() {
	let fx = Fixture::new();
	let body = "0123456789abcdef".repeat(256 * 1024 / 16);
	fs::write(fx.root.join("big.txt"), &body).unwrap();
	assert_eq!(body.len(), 256 * 1024);
	fx.worker.inject_fault_for_tests(rule(
		RequestKind::Read,
		1,
		Fault::Throttle {
			chunk: 32 * 1024,
			pause: Duration::from_millis(300),
		},
	));
	let client = fx.client();
	let started = Instant::now();
	let got = client.read(&fx.ws, "big.txt", None).unwrap();
	assert_eq!(got.as_deref(), Some(body.as_str()));
	assert!(
		started.elapsed() >= Duration::from_millis(300),
		"the pauses were real"
	);
}

#[test]
fn hello_faults_become_start_errors() {
	let fx = Fixture::new();
	let open = || {
		let started = Instant::now();
		let result =
			Connection::open(&Transport::InProcess(fx.worker.clone()), "t");
		assert!(
			started.elapsed() < scaled(Duration::from_secs(15)),
			"the start took {:?}",
			started.elapsed()
		);
		result
	};
	for (fault, label) in [
		(Fault::HangUp, "hang up"),
		(Fault::Garbage, "garbage"),
		(
			Fault::Refuse(ErrorCode::VersionMismatch),
			"version mismatch",
		),
	] {
		fx.worker.clear_faults_for_tests();
		fx.worker.inject_fault_for_tests(rule(
			RequestKind::Hello,
			1,
			fault.clone(),
		));
		let before = fx.seen(RequestKind::Hello);
		let err = match open() {
			Err(e) => e,
			Ok(_) => panic!("{label}: the start worked"),
		};
		assert_eq!(fx.seen(RequestKind::Hello) - before, 1, "{label}");
		match (&fault, &err) {
			(
				Fault::Refuse(ErrorCode::VersionMismatch),
				RemoteError::Refused { code, .. },
			) => assert_eq!(*code, ErrorCode::VersionMismatch),
			(Fault::HangUp, RemoteError::Start { kind, .. }) => {
				// The stream ended before any hello.
				assert_ne!(*kind, ConnectFailureKind::Protocol, "{label}");
			}
			(Fault::Garbage, RemoteError::Start { kind, .. }) => {
				// A frame that is not a frame: bad data, a protocol failure.
				assert_eq!(*kind, ConnectFailureKind::Protocol, "{label}");
			}
			_ => panic!("{label}: {err:?}"),
		}
		// The fault is spent: the next start works.
		assert!(open().is_ok(), "{label}: then failed");
	}
}

#[test]
fn a_reply_that_never_starts_is_stalled_with_zero_bytes() {
	let fx = Fixture::new();
	fx.worker.inject_fault_for_tests(rule(
		RequestKind::Read,
		1,
		Fault::Stall(Duration::from_secs(7)),
	));
	let mut conn =
		Connection::open(&Transport::InProcess(fx.worker.clone()), "t")
			.unwrap();
	let started = Instant::now();
	let result = conn.call(
		&fx.sample(RequestKind::Read).unwrap(),
		None,
		scaled(Duration::from_secs(30)),
	);
	match result {
		Err(RemoteError::Stalled {
			received: 0,
			ended: WaitEnd::Silence,
			..
		}) => {}
		other => panic!("expected silence with 0 bytes, got {other:?}"),
	}
	assert!(
		started.elapsed() < scaled(Duration::from_secs(15)),
		"took {:?}",
		started.elapsed()
	);
}
