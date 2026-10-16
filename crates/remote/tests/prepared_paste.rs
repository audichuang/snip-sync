//! File-paste wire and affinity checks against independent stdio workers.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use snip_core::gitrun::RunOptions;
use snip_core::gitsrc::Git;
use snip_core::restore::RestoreSelection;
use snip_core::workspace::{lock_heavy, RepoIdentity};
use snip_remote::{
	Client, PasteError, PasteRoute, PasteTarget, RemoteHost, Transport, Worker,
	WorkerOptions,
};

static ENVIRONMENT: Mutex<()> = Mutex::new(());
const CONFIG: &str = "SNIP_TEST_PREPARED_WORKER";

struct FrameMeter<T> {
	inner: T,
	dir: &'static str,
	trace: PathBuf,
	buffer: Vec<u8>,
	joined: String,
}

fn content_bytes(value: &Value) -> usize {
	match value {
		Value::Object(map) => map
			.iter()
			.map(|(key, value)| {
				if key == "content" {
					value.as_str().map_or(0, str::len)
				} else {
					content_bytes(value)
				}
			})
			.sum(),
		Value::Array(values) => values.iter().map(content_bytes).sum(),
		_ => 0,
	}
}

impl<T> FrameMeter<T> {
	fn new(inner: T, dir: &'static str, trace: &Path) -> Self {
		Self {
			inner,
			dir,
			trace: trace.into(),
			buffer: Vec::new(),
			joined: String::new(),
		}
	}

	fn observe(&mut self, bytes: &[u8]) -> io::Result<()> {
		self.buffer.extend_from_slice(bytes);
		while self.buffer.len() >= 4 {
			let len = u32::from_be_bytes(self.buffer[..4].try_into().unwrap())
				as usize;
			assert!(len <= snip_remote::proto::MAX_FRAME);
			if self.buffer.len() < len + 4 {
				break;
			}
			let frame: Value =
				serde_json::from_slice(&self.buffer[4..4 + len])?;
			self.buffer.drain(..4 + len);
			let kind = frame
				.get(if self.dir == "up" { "op" } else { "reply" })
				.and_then(Value::as_str)
				.unwrap_or("unknown");
			let mut logical = frame.clone();
			if kind == "frame_chunk" || (self.dir == "down" && kind == "chunk")
			{
				self.joined.push_str(frame["data"].as_str().unwrap());
				logical = Value::Null;
			} else if kind == "frame_join" || kind == "joined" {
				logical =
					serde_json::from_str(&std::mem::take(&mut self.joined))?;
			}
			let payload = if self.dir == "up" && kind == "chunk" {
				frame["data"].as_str().map_or(0, str::len)
			} else {
				logical["text"].as_str().map_or(0, str::len)
			};
			let logical_kind = logical
				.get(if self.dir == "up" { "op" } else { "reply" })
				.and_then(Value::as_str);
			let event = json!({"pid": std::process::id(), "direction": self.dir, "frame_bytes": len+4,
				"kind":kind, "logical_kind":logical_kind, "dest":logical.get("dest"), "payload_bytes":payload, "content_bytes":content_bytes(&logical)});
			writeln!(
				OpenOptions::new()
					.create(true)
					.append(true)
					.open(&self.trace)?,
				"{event}"
			)?;
		}
		Ok(())
	}
}

impl<T: Read> Read for FrameMeter<T> {
	fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buffer)?;
		self.observe(&buffer[..n])?;
		Ok(n)
	}
}
impl<T: Write> Write for FrameMeter<T> {
	fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
		let n = self.inner.write(buffer)?;
		self.observe(&buffer[..n])?;
		Ok(n)
	}
	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

/// Spawned by the other tests as a real independent process. The harness
/// banner precedes the protocol preamble, just like an SSH login banner.
#[test]
fn prepared_worker_process() {
	let Ok(config_path) = std::env::var(CONFIG) else {
		return;
	};
	let cfg: Value =
		serde_json::from_slice(&fs::read(config_path).unwrap()).unwrap();
	let dir = PathBuf::from(cfg["dir"].as_str().unwrap());
	let trace = dir.join(format!("{}.jsonl", std::process::id()));
	if cfg["hold"].as_bool() == Some(true) {
		std::env::set_var("SNIP_E2E_PASTE_HOLD", dir.join("hold"));
	}
	let worker = Arc::new(Worker::new(WorkerOptions {
		name: "independent-worker".into(),
		max_protocol: cfg["version"].as_u64().map(|n| n as u32),
	}));
	if let Some(bytes) = cfg["cache_bytes"].as_u64() {
		worker.set_prepared_cache_limits_for_tests(
			bytes as usize,
			8,
			Duration::from_secs(300),
		);
	}
	let watched_worker = worker.clone();
	let control_dir = dir.clone();
	std::thread::spawn(move || {
		let pid = std::process::id();
		let stop = control_dir.join(format!("exit-{pid}"));
		let expire = control_dir.join(format!("expire-{pid}"));
		let expired = control_dir.join(format!("expired-{pid}"));
		let deadline = Instant::now() + Duration::from_secs(120);
		while Instant::now() < deadline {
			if stop.exists() {
				std::process::exit(77);
			}
			if expire.exists() && !expired.exists() {
				watched_worker.expire_prepared_for_tests();
				fs::write(&expired, "expired").unwrap();
			}
			std::thread::sleep(Duration::from_millis(10));
		}
	});
	let stdout = io::stdout();
	let mut output = stdout.lock();
	writeln!(output, "{}", snip_remote::worker::PREAMBLE).unwrap();
	output.flush().unwrap();
	worker
		.serve(
			FrameMeter::new(io::stdin().lock(), "up", &trace),
			FrameMeter::new(output, "down", &trace),
		)
		.unwrap();
}

struct Fixture {
	dir: tempfile::TempDir,
	prior: Option<std::ffi::OsString>,
}
impl Fixture {
	fn new(version: Option<u32>, hold: bool) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let config = dir.path().join("config.json");
		fs::write(
			&config,
			json!({"dir":dir.path(),"version":version,"hold":hold}).to_string(),
		)
		.unwrap();
		let prior = std::env::var_os(CONFIG);
		std::env::set_var(CONFIG, config);
		Self { dir, prior }
	}
	fn cache_bytes(&self, bytes: usize) {
		let path = self.dir.path().join("config.json");
		let mut cfg: Value =
			serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
		cfg["cache_bytes"] = json!(bytes);
		fs::write(path, cfg.to_string()).unwrap();
	}

	fn client(&self) -> Arc<Client> {
		Arc::new(Client::new(
			RemoteHost {
				name: "process-workers".into(),
				transport: Transport::Command(vec![
					std::env::current_exe().unwrap().into_os_string(),
					"--exact".into(),
					"prepared_worker_process".into(),
					"--nocapture".into(),
				]),
			},
			"master".into(),
		))
	}
	fn target(
		&self,
		client: Arc<Client>,
		workspace: &str,
		dest: &str,
	) -> PasteTarget {
		PasteTarget::Remote {
			client,
			workspace: workspace.into(),
			dest: dest.into(),
		}
	}
	fn events(&self) -> Vec<Value> {
		fs::read_dir(self.dir.path())
			.unwrap()
			.filter_map(Result::ok)
			.filter(|entry| {
				entry.path().extension().is_some_and(|e| e == "jsonl")
			})
			.flat_map(|entry| {
				fs::read_to_string(entry.path())
					.unwrap()
					.lines()
					.filter_map(|s| serde_json::from_str::<Value>(s).ok())
					.collect::<Vec<_>>()
			})
			.collect()
	}
	fn after_replies(&self, kind: &str, count: usize) -> Vec<Value> {
		wait("response meter finished", || {
			self.events()
				.iter()
				.filter(|e| {
					e["direction"] == "down" && e["logical_kind"] == kind
				})
				.count() >= count
		});
		self.events()
	}
}
impl Drop for Fixture {
	fn drop(&mut self) {
		if let Some(prior) = self.prior.take() {
			std::env::set_var(CONFIG, prior);
		} else {
			std::env::remove_var(CONFIG);
		}
	}
}

fn selection() -> RestoreSelection {
	RestoreSelection {
		overwrite_existing: true,
		..Default::default()
	}
}
fn wait(why: &str, mut ready: impl FnMut() -> bool) {
	let deadline = Instant::now() + timeout();
	while !ready() {
		assert!(Instant::now() < deadline, "timed out: {why}");
		std::thread::sleep(Duration::from_millis(10));
	}
}

fn timeout() -> Duration {
	let scale = std::env::var("SNIP_E2E_TIMEOUT_SCALE")
		.ok()
		.and_then(|s| s.parse::<f64>().ok())
		.filter(|n| n.is_finite() && *n > 0.)
		.unwrap_or(1.);
	Duration::from_secs(15).mul_f64(scale)
}
fn totals(events: &[Value]) -> (u64, u64) {
	(
		events
			.iter()
			.map(|e| e["payload_bytes"].as_u64().unwrap())
			.sum(),
		events
			.iter()
			.filter(|e| e["direction"] == "down")
			.map(|e| e["content_bytes"].as_u64().unwrap())
			.sum(),
	)
}

#[test]
fn prepared_file_paste_sends_one_payload_and_no_response_body() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "");
	let body = "r".repeat(1024 * 1024);
	let text = format!("// FILE: body.rs\n{body}\n");
	let route = PasteRoute::default();
	let plan = target
		.plan_files(&text, &route, &RunOptions::default(), None)
		.unwrap();
	assert_eq!(plan.restore_plan().create_operations[0].content, body);
	let result = target
		.apply_files(&text, &route, &plan, &selection())
		.unwrap();
	assert_eq!(result.created_count, 1);
	assert_eq!(
		fs::read_to_string(workspace.path().join("body.rs")).unwrap(),
		body
	);
	wait("apply frame recorded", || {
		fixture.events().iter().any(|e| {
			matches!(
				e["logical_kind"].as_str(),
				Some("apply_prepared" | "import_apply")
			)
		})
	});
	let events = fixture.after_replies("imported", 1);
	let (uploaded, echoed) = totals(&events);
	eprintln!("wire uploaded_payload_bytes={uploaded} echoed_content_bytes={echoed} source_payload_bytes={}", text.len());
	assert_eq!(
		uploaded,
		text.len() as u64,
		"the source payload must upload only once"
	);
	assert_eq!(echoed, 0, "the plan response must not echo source content");
	assert!(events.iter().any(|e| e["logical_kind"] == "prepare_import"));
	assert!(events.iter().any(|e| e["logical_kind"] == "apply_prepared"));
}

#[test]
fn older_workers_keep_the_legacy_file_paste_contract() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	for version in [4, 5, 6] {
		let fixture = Fixture::new(Some(version), false);
		let workspace = tempfile::tempdir().unwrap();
		let client = fixture.client();
		let ws = client
			.open_workspace(workspace.path().to_str().unwrap())
			.unwrap();
		let target = fixture.target(client, &ws.id, "");
		let text = "// FILE: code.rs\nfn main() {}\n";
		let route = PasteRoute::default();
		let plan = target
			.plan_files(text, &route, &RunOptions::default(), None)
			.unwrap();
		target
			.apply_files(text, &route, &plan, &selection())
			.unwrap();
		assert_eq!(
			fs::read_to_string(workspace.path().join("code.rs")).unwrap(),
			"fn main() {}"
		);
		let events = fixture.after_replies("imported", 1);
		assert_eq!(
			totals(&events),
			((text.len() * 2) as u64, "fn main() {}".len() as u64),
			"protocol {version}"
		);
		assert!(!events.iter().any(|e| e["logical_kind"] == "prepare_import"));
	}
}

#[test]
fn two_pending_plans_keep_affinity_with_two_independent_worker_processes() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	for dest in ["a", "b"] {
		fs::create_dir(workspace.path().join(dest)).unwrap();
	}
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let a = fixture.target(client.clone(), &ws.id, "a");
	let b = fixture.target(client, &ws.id, "b");
	let route = PasteRoute::default();
	let text_a = "// FILE: result.txt\nfrom a\n";
	let text_b = "// FILE: result.txt\nfrom b\n";
	let plan_a = a
		.plan_files(text_a, &route, &RunOptions::default(), None)
		.unwrap();
	let plan_b = b
		.plan_files(text_b, &route, &RunOptions::default(), None)
		.unwrap();
	fixture.after_replies("import_prepared", 2);
	let prepared: BTreeMap<_, _> = fixture
		.events()
		.iter()
		.filter(|e| e["logical_kind"] == "prepare_import")
		.map(|e| {
			(
				e["dest"].as_str().unwrap().to_string(),
				e["pid"].as_u64().unwrap(),
			)
		})
		.collect();
	assert_eq!(prepared.len(), 2);
	assert_ne!(prepared["a"], prepared["b"], "tokens must be held by independent OS processes, not one shared Worker state");
	// Applying in the opposite order must use each plan's original worker.
	b.apply_files(text_b, &route, &plan_b, &selection())
		.unwrap();
	a.apply_files(text_a, &route, &plan_a, &selection())
		.unwrap();
	let applied: BTreeMap<_, _> = fixture
		.after_replies("imported", 2)
		.iter()
		.filter(|e| e["logical_kind"] == "apply_prepared")
		.map(|e| {
			(
				e["dest"].as_str().unwrap().to_string(),
				e["pid"].as_u64().unwrap(),
			)
		})
		.collect();
	assert_eq!(applied, prepared);
	assert_eq!(
		fs::read_to_string(workspace.path().join("a/result.txt")).unwrap(),
		"from a"
	);
	assert_eq!(
		fs::read_to_string(workspace.path().join("b/result.txt")).unwrap(),
		"from b"
	);
}

#[test]
fn lost_prepared_apply_answer_is_unknown_and_never_resent_or_fallen_back() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, true);
	fs::write(fixture.dir.path().join("hold"), "hold").unwrap();
	let workspace = tempfile::tempdir().unwrap();
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "");
	let text = "// FILE: a.txt\nfirst\n// FILE: b.txt\nsecond\n";
	let route = PasteRoute::default();
	let plan = target
		.plan_files(text, &route, &RunOptions::default(), None)
		.unwrap();
	let pid = fixture
		.events()
		.iter()
		.find(|e| e["logical_kind"] == "prepare_import")
		.expect("prepared plan")["pid"]
		.as_u64()
		.unwrap();
	let (send, recv) = std::sync::mpsc::channel();
	let applying = std::thread::spawn(move || {
		send.send(target.apply_files(text, &route, &plan, &selection()))
			.unwrap();
	});
	wait(
		"the first committed file before the connection is cut",
		|| workspace.path().join("a.txt").is_file(),
	);
	assert!(
		!workspace.path().join("b.txt").exists(),
		"the real worker must be held between writes"
	);
	fs::write(fixture.dir.path().join(format!("exit-{pid}")), "cut").unwrap();
	let result = recv
		.recv_timeout(timeout())
		.expect("lost apply must return within its bound");
	applying.join().unwrap();
	assert!(
		matches!(result, Err(PasteError::OutcomeUnknown(_))),
		"{result:?}"
	);
	assert_eq!(
		fs::read_to_string(workspace.path().join("a.txt")).unwrap(),
		"first"
	);
	assert!(!workspace.path().join("b.txt").exists());
	let events = fixture.events();
	assert_eq!(
		events
			.iter()
			.filter(|e| e["logical_kind"] == "apply_prepared")
			.count(),
		1
	);
	assert!(
		!events.iter().any(|e| e["logical_kind"] == "import_apply"),
		"lost prepared Apply must never fall back to a second write"
	);
}

#[test]
fn metadata_without_a_retained_token_uses_the_explicit_legacy_apply_path() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	fixture.cache_bytes(1);
	let workspace = tempfile::tempdir().unwrap();
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "");
	let text = "// FILE: result.txt\nlegal fallback\n";
	let route = PasteRoute::default();
	let plan = target
		.plan_files(text, &route, &RunOptions::default(), None)
		.unwrap();
	assert!(!plan.uses_prepared_import());
	target
		.apply_files(text, &route, &plan, &selection())
		.unwrap();
	let events = fixture.after_replies("imported", 1);
	assert!(events.iter().any(|e| e["logical_kind"] == "prepare_import"));
	assert!(events.iter().any(|e| e["logical_kind"] == "import_apply"));
	assert!(!events.iter().any(|e| e["logical_kind"] == "apply_prepared"));
	assert_eq!(totals(&events), ((text.len() * 2) as u64, 0));
	assert_eq!(
		fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
		"legal fallback"
	);
}

/// Expires the worker's token for `plan`'s preview, as the 5 minute TTL does.
/// Expires the token the first `request_kind` call of the fixture's worker
/// retained, once its reply (`reply_kind`) has been metered.
fn expire_token(fixture: &Fixture, reply_kind: &str, request_kind: &str) {
	let events = fixture.after_replies(reply_kind, 1);
	let pid = events
		.iter()
		.find(|e| e["logical_kind"] == request_kind)
		.unwrap()["pid"]
		.as_u64()
		.unwrap();
	fs::write(fixture.dir.path().join(format!("expire-{pid}")), "expire")
		.unwrap();
	wait("the worker advances its cache clock", || {
		fixture.dir.path().join(format!("expired-{pid}")).exists()
	});
}

#[test]
fn expired_prepared_token_falls_back_once_when_the_destination_is_unchanged() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "");
	let text = "// FILE: result.txt\nfallback applied\n";
	let route = PasteRoute::default();
	let plan = target
		.plan_files(text, &route, &RunOptions::default(), None)
		.unwrap();
	expire_token(&fixture, "import_prepared", "prepare_import");
	target
		.apply_files(text, &route, &plan, &selection())
		.expect(
		"a token the worker dropped must not refuse an unchanged destination",
	);
	assert_eq!(
		fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
		"fallback applied"
	);
	let events = fixture.after_replies("imported", 1);
	assert_eq!(
		events
			.iter()
			.filter(|e| e["logical_kind"] == "apply_prepared")
			.count(),
		1,
		"the expired token is tried once"
	);
	assert_eq!(
		events
			.iter()
			.filter(|e| e["logical_kind"] == "import_apply")
			.count(),
		1,
		"the full text is sent once, on the same connection"
	);
}

#[test]
fn expired_prepared_token_with_a_changed_destination_refuses_as_stale() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "");
	let text = "// FILE: result.txt\nnever applied\n";
	let route = PasteRoute::default();
	let plan = target
		.plan_files(text, &route, &RunOptions::default(), None)
		.unwrap();
	expire_token(&fixture, "import_prepared", "prepare_import");
	fs::write(workspace.path().join("result.txt"), "edited outside").unwrap();
	let error = target
		.apply_files(text, &route, &plan, &selection())
		.unwrap_err();
	let PasteError::Remote(snip_remote::RemoteError::Refused {
		code: snip_remote::ErrorCode::Stale,
		message,
		..
	}) = &error
	else {
		panic!("expected a stale refusal, got {error:?}");
	};
	assert!(
		!message.contains("expired") && !message.contains("preview again"),
		"the reason must be the real destination change: {message}"
	);
	assert!(
		!message.contains("prepared paste"),
		"the worker's token wording must never reach the user: {message}"
	);
	assert_eq!(
		fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
		"edited outside",
		"a refused Apply writes nothing"
	);
}

#[test]
fn busy_refusal_before_the_write_lock_keeps_the_preview_retryable() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let status = std::process::Command::new("git")
		.arg("init")
		.arg("-q")
		.current_dir(workspace.path())
		.status()
		.unwrap();
	assert!(status.success());
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "");
	let text = "// FILE: result.txt\nretried\n";
	let route = PasteRoute::default();
	let plan = target
		.plan_files(text, &route, &RunOptions::default(), None)
		.unwrap();
	// Another paste holds this repository's write lock.
	let git = Git::open(workspace.path()).unwrap();
	let identity = RepoIdentity::resolve(&git, &RunOptions::default()).unwrap();
	let held = lock_heavy(&identity).unwrap();
	let busy = target
		.apply_files(text, &route, &plan, &selection())
		.unwrap_err();
	assert!(busy.is_repo_busy(), "{busy:?}");
	assert!(!workspace.path().join("result.txt").exists());
	drop(held);
	target
		.apply_files(text, &route, &plan, &selection())
		.expect("the same preview must apply once the lock is free");
	assert_eq!(
		fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
		"retried"
	);
	let events = fixture.after_replies("imported", 1);
	assert_eq!(
		events
			.iter()
			.filter(|e| e["logical_kind"] == "apply_prepared")
			.count(),
		2,
		"the retry reuses the retained token"
	);
	assert!(
		!events.iter().any(|e| e["logical_kind"] == "import_apply"),
		"a Busy refusal needs no full-text fallback"
	);
	let again = target
		.apply_files(text, &route, &plan, &selection())
		.unwrap_err();
	assert!(
		matches!(
			again,
			PasteError::Remote(snip_remote::RemoteError::Refused {
				code: snip_remote::ErrorCode::Stale,
				..
			})
		),
		"an applied preview is used up: {again:?}"
	);
}

#[test]
fn dropping_the_last_plan_clone_reaps_its_worker_and_releases_its_slot() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client.clone(), &ws.id, "");
	let plan = target
		.plan_files(
			"// FILE: result.txt\nnot applied\n",
			&PasteRoute::default(),
			&RunOptions::default(),
			None,
		)
		.unwrap();
	let cloned = plan.clone();
	let events = fixture.after_replies("import_prepared", 1);
	let _pid = events
		.iter()
		.find(|e| e["logical_kind"] == "prepare_import")
		.unwrap()["pid"]
		.as_u64()
		.unwrap();
	drop(plan);
	assert_eq!(
		client.prepared_imports_for_tests(),
		1,
		"one clone still owns the lease"
	);
	let started = Instant::now();
	drop(cloned);
	assert!(
		started.elapsed() < timeout() / 15,
		"dropping a GUI plan must not wait for its worker"
	);
	wait("the lease reaper confirms the child has ended", || {
		client.prepared_imports_for_tests() == 0
	});
	#[cfg(unix)]
	wait("the actual worker PID is gone", || {
		!std::process::Command::new("kill")
			.args(["-0", &_pid.to_string()])
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::null())
			.status()
			.unwrap()
			.success()
	});
	assert!(!workspace.path().join("result.txt").exists());
}

/// Runs `git` in `dir` with a fixed identity, failing the test on error.
fn git(dir: &Path, args: &[&str]) -> String {
	let out = std::process::Command::new("git")
		.arg("-C")
		.arg(dir)
		.args(["-c", "user.name=t", "-c", "user.email=t@t"])
		.args(args)
		.output()
		.unwrap();
	assert!(
		out.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&out.stderr)
	);
	String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A destination repository with one base commit, as the replay expects.
fn init_dst(dir: &Path) {
	fs::create_dir_all(dir).unwrap();
	git(dir, &["init", "-q", "-b", "main"]);
	fs::write(dir.join("README"), "base\n").unwrap();
	git(dir, &["add", "."]);
	git(dir, &["commit", "-q", "-m", "base"]);
}

/// A commit payload of two commits in `ws/src`, the first adding a file of
/// about 75 KB. It is exported by an unmetered in-process worker, so only
/// the paste under test reaches the frame meter.
fn large_commit_payload(ws: &Path) -> String {
	let src = ws.join("src");
	init_dst(&src);
	let body: String = (0..1500)
		.map(|i| format!("line {i} {}\n", "y".repeat(40)))
		.collect();
	fs::write(src.join("big.txt"), &body).unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "big file"]);
	let first = git(&src, &["rev-parse", "HEAD"]);
	fs::write(src.join("b.txt"), "b\n").unwrap();
	git(&src, &["add", "."]);
	git(&src, &["commit", "-q", "-m", "add b"]);
	let tip = git(&src, &["rev-parse", "HEAD"]);
	let client = Client::new(
		RemoteHost::in_process(Arc::new(Worker::new(WorkerOptions::default()))),
		"master".into(),
	);
	let id = client.open_workspace(ws.to_str().unwrap()).unwrap().id;
	client
		.export_commits(&id, "src", &tip, vec![tip.clone(), first], None)
		.unwrap()
		.text
}

fn commit_count(dir: &Path) -> usize {
	git(dir, &["rev-list", "--count", "HEAD"]).parse().unwrap()
}

/// Bytes the master sent for one request kind, as the meter counted them.
fn uploaded_bytes(events: &[Value]) -> u64 {
	events
		.iter()
		.filter(|e| e["direction"] == "up")
		.map(|e| e["payload_bytes"].as_u64().unwrap())
		.sum()
}

fn count_up(events: &[Value], kind: &str) -> usize {
	events
		.iter()
		.filter(|e| e["direction"] == "up" && e["logical_kind"] == kind)
		.count()
}

#[test]
fn prepared_commit_replay_sends_the_payload_once() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let text = large_commit_payload(workspace.path());
	assert!(text.len() >= 64 * 1024, "payload is {} bytes", text.len());
	let dst = workspace.path().join("dst");
	init_dst(&dst);
	let before = commit_count(&dst);
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "dst");
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();
	let (preview, prepared) = target
		.plan_commits(&text, payload, &RunOptions::default())
		.unwrap();
	assert!(
		prepared.is_prepared(),
		"a protocol-8 worker keeps its preview"
	);
	target.check_commits(&text, &preview, &prepared).unwrap();
	let result = target.apply_commits(&text, &preview, &prepared).unwrap();
	assert_eq!(result.created.len(), 2, "{result:?}");
	assert_eq!(commit_count(&dst), before + 2);

	let events = fixture.after_replies("replayed", 1);
	assert_eq!(
		uploaded_bytes(&events),
		text.len() as u64,
		"the payload must upload only once"
	);
	assert_eq!(count_up(&events, "prepare_replay"), 1);
	assert_eq!(count_up(&events, "apply_prepared_replay"), 2);
	assert_eq!(count_up(&events, "replay_plan"), 0);
	assert_eq!(count_up(&events, "replay_apply"), 0);
}

#[test]
fn older_workers_keep_the_legacy_commit_replay_contract() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(Some(7), false);
	let workspace = tempfile::tempdir().unwrap();
	let text = large_commit_payload(workspace.path());
	let dst = workspace.path().join("dst");
	init_dst(&dst);
	let before = commit_count(&dst);
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "dst");
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();
	let (preview, prepared) = target
		.plan_commits(&text, payload, &RunOptions::default())
		.unwrap();
	assert!(!prepared.is_prepared());
	let result = target.apply_commits(&text, &preview, &prepared).unwrap();
	assert_eq!(result.created.len(), 2, "{result:?}");
	assert_eq!(commit_count(&dst), before + 2);

	let events = fixture.after_replies("replayed", 1);
	assert_eq!(uploaded_bytes(&events), (2 * text.len()) as u64);
	assert_eq!(count_up(&events, "prepare_replay"), 0);
}

#[test]
fn an_expired_replay_token_falls_back_once_to_the_full_text() {
	let _serial = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
	let fixture = Fixture::new(None, false);
	let workspace = tempfile::tempdir().unwrap();
	let text = large_commit_payload(workspace.path());
	let dst = workspace.path().join("dst");
	init_dst(&dst);
	let before = commit_count(&dst);
	let client = fixture.client();
	let ws = client
		.open_workspace(workspace.path().to_str().unwrap())
		.unwrap();
	let target = fixture.target(client, &ws.id, "dst");
	let payload = snip_core::commits::parse_commit_payload(&text).unwrap();
	let (preview, prepared) = target
		.plan_commits(&text, payload, &RunOptions::default())
		.unwrap();
	expire_token(&fixture, "replay_prepared", "prepare_replay");
	let result = target.apply_commits(&text, &preview, &prepared).unwrap();
	assert_eq!(result.created.len(), 2, "{result:?}");
	assert_eq!(commit_count(&dst), before + 2);

	let events = fixture.after_replies("replayed", 1);
	assert_eq!(count_up(&events, "apply_prepared_replay"), 1);
	assert_eq!(count_up(&events, "replay_apply"), 1);
}
