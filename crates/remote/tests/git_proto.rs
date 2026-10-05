//! Protocol v2 and Git view negotiation loopback tests.

use std::io::{PipeReader, PipeWriter};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use snip_core::gitview::ReadProfile;
use snip_remote::proto::{read_frame, write_frame, GitQuery, Request};
use snip_remote::{
	Client, Connection, ErrorCode, RemoteError, RemoteHost, Transport, Worker,
	WorkerOptions,
};

fn timeout_scale() -> u32 {
	std::env::var("SNIP_E2E_TIMEOUT_SCALE")
		.ok()
		.and_then(|v| v.parse::<u32>().ok())
		.unwrap_or(1)
		.max(1)
}

fn scaled(d: Duration) -> Duration {
	d.saturating_mul(timeout_scale())
}

fn worker(max_protocol: Option<u32>) -> Arc<Worker> {
	Arc::new(Worker::new(WorkerOptions {
		name: "test-worker".into(),
		max_protocol,
	}))
}

fn client_for(w: &Arc<Worker>, root: &std::path::Path) -> (Client, String) {
	let client = Client::new(RemoteHost::in_process(w.clone()), "mac".into());
	let ws = client.open_workspace(&root.display().to_string()).unwrap();
	(client, ws.id)
}

/// A raw connection: frames are written to the pipe and read back through
/// a thread, so a missing reply fails the test instead of hanging it.
struct Raw {
	writer: PipeWriter,
	frames: mpsc::Receiver<serde_json::Value>,
}

impl Raw {
	fn open(w: &Arc<Worker>) -> Self {
		let (writer, reader) = w.connect_in_process().unwrap();
		Self {
			writer,
			frames: pump(reader),
		}
	}

	fn send(&mut self, frame: &serde_json::Value) {
		write_frame(&mut self.writer, frame).unwrap();
	}

	fn recv(&self) -> serde_json::Value {
		self.frames
			.recv_timeout(scaled(Duration::from_secs(5)))
			.expect("the worker sent no frame within 5 s")
	}
}

fn pump(mut reader: PipeReader) -> mpsc::Receiver<serde_json::Value> {
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		while let Ok(Some(frame)) = read_frame::<serde_json::Value>(&mut reader)
		{
			if tx.send(frame).is_err() {
				return;
			}
		}
	});
	rx
}

#[test]
fn an_old_worker_is_reported_too_old_before_any_git_request() {
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(Some(1));
	// Opening a workspace works on protocol 1
	let (client, ws) = client_for(&w, tmp.path());

	let git_err = client
		.git(
			&ws,
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();
	match git_err {
		RemoteError::WorkerTooOld { have, need, .. } => {
			assert_eq!(have, 1);
			assert_eq!(need, 2);
		}
		other => panic!("expected WorkerTooOld, got {other:?}"),
	}

	let scan_err = client.scan_repos(&ws, None, None).unwrap_err();
	match scan_err {
		RemoteError::WorkerTooOld { have, need, .. } => {
			assert_eq!(have, 1);
			assert_eq!(need, 2);
		}
		other => panic!("expected WorkerTooOld, got {other:?}"),
	}

	assert_eq!(w.git_requests_seen(), 0);
	assert!(client.list_dir(&ws, "").is_ok());
}

#[test]
fn hand_written_v1_hello_still_opens_a_workspace() {
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(None);
	let mut raw = Raw::open(&w);

	// Send v1 hello without max_version
	raw.send(&serde_json::json!({
		"op": "hello",
		"version": 1,
		"name": "old"
	}));
	let reply = raw.recv();
	assert_eq!(reply.get("version").and_then(|v| v.as_u64()), Some(1));
	assert_eq!(reply.get("max_version").and_then(|v| v.as_u64()), Some(1));

	raw.send(&serde_json::json!({
		"op": "open_workspace",
		"path": tmp.path().display().to_string()
	}));
	let opened = raw.recv();
	assert_eq!(
		opened.get("reply").and_then(|r| r.as_str()),
		Some("workspace"),
		"{opened}"
	);
	let ws = opened
		.get("id")
		.and_then(|i| i.as_str())
		.unwrap()
		.to_string();

	// Send git_view on v1-negotiated connection
	raw.send(&serde_json::json!({
		"op": "git_view",
		"workspace": ws,
		"repo": "",
		"profile": "Interactive",
		"query": { "q": "change_list" }
	}));
	let git_reply = raw.recv();
	assert_eq!(
		git_reply.get("reply").and_then(|r| r.as_str()),
		Some("error")
	);
	assert_eq!(
		git_reply.get("code").and_then(|c| c.as_str()),
		Some("unsupported")
	);
}

#[test]
fn new_master_negotiates_the_newest_with_a_new_worker() {
	let w = worker(None);
	let conn = Connection::open(&Transport::InProcess(w), "mac").unwrap();
	assert_eq!(conn.version(), snip_remote::PROTOCOL_MAX);
}

#[test]
fn write_and_rename_stay_unsupported() {
	let tmp = tempfile::tempdir().unwrap();
	let w = worker(None);
	let (client, ws) = client_for(&w, tmp.path());

	let write_res = client.call(&Request::Write {
		workspace: ws.clone(),
		path: "test.txt".into(),
		content: "hello".into(),
	});
	match write_res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::Unsupported);
		}
		other => panic!("expected Refused(Unsupported), got {other:?}"),
	}

	let rename_res = client.call(&Request::Rename {
		workspace: ws,
		from: "a.txt".into(),
		to: "b.txt".into(),
	});
	match rename_res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::Unsupported);
		}
		other => panic!("expected Refused(Unsupported), got {other:?}"),
	}
}
