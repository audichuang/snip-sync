//! Protocol v2 and Git view negotiation loopback tests.

use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::{ServerConnection, StreamOwned};
use snip_core::gitrun::CancelToken;
use snip_core::gitview::ReadProfile;
use snip_remote::client::CALL_LIMIT_DEFAULT;
use snip_remote::proto::{
	read_frame, write_frame, ErrorCode, GitQuery, GitReply, Request, Response,
	PROTOCOL_MAX, PROTOCOL_VERSION,
};
use snip_remote::tls::{client_config, server_config, server_name};
use snip_remote::{
	pair, Client, Connection, Identity, RemoteError, Worker, WorkerOptions,
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

fn worker(
	roots: &[&std::path::Path],
	max_protocol: Option<u32>,
) -> (Worker, Identity) {
	let id = Identity::generate().unwrap();
	let w = Worker::start(
		"127.0.0.1:0".parse().unwrap(),
		&id,
		WorkerOptions {
			name: "test-worker".into(),
			trust_file: None,
			max_protocol,
		},
	)
	.unwrap();
	let roots: Vec<_> = roots.iter().map(|p| p.to_path_buf()).collect();
	assert!(w.set_roots(&roots).is_empty());
	(w, id)
}

fn addr(w: &Worker) -> String {
	w.local_addr().to_string()
}

#[test]
fn an_old_worker_is_reported_too_old_before_any_git_request() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()], Some(1));
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	let client = Client::new(paired, master, "mac".into()).unwrap();

	// List workspaces works on protocol 1
	let spaces = client.list_workspaces().unwrap();
	assert_eq!(spaces.len(), 1);
	let ws = spaces[0].id.clone();

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
	assert_eq!(client.list_workspaces().unwrap().len(), 1);
}

#[test]
fn a_restarted_newer_worker_is_used_after_too_old() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("share");
	fs::create_dir_all(&shared).unwrap();
	let trust_file = tmp.path().join("trusted.json");
	let worker_id = Identity::generate().unwrap();

	let mut worker_a = Worker::start(
		"127.0.0.1:0".parse().unwrap(),
		&worker_id,
		WorkerOptions {
			name: "worker".into(),
			trust_file: Some(trust_file.clone()),
			max_protocol: Some(1),
		},
	)
	.unwrap();
	worker_a.set_roots(std::slice::from_ref(&shared));
	let worker_addr = worker_a.local_addr();

	let master = Arc::new(Identity::generate().unwrap());
	let code = worker_a.open_pairing();
	let paired = pair(&worker_addr.to_string(), &code, &master, "mac").unwrap();
	let client = Client::new(paired, master, "mac".into()).unwrap();

	let ws = client.list_workspaces().unwrap()[0].id.clone();

	let err = client
		.git(
			&ws,
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();
	assert!(matches!(err, RemoteError::WorkerTooOld { .. }));

	// Stop worker A
	worker_a.stop();
	drop(worker_a);

	// Start worker B on the same address (retry bind within 5 s)
	let deadline = Instant::now() + scaled(Duration::from_secs(5));
	let worker_b = loop {
		match Worker::start(
			worker_addr,
			&worker_id,
			WorkerOptions {
				name: "worker".into(),
				trust_file: Some(trust_file.clone()),
				max_protocol: None,
			},
		) {
			Ok(w) => break w,
			Err(_) if Instant::now() < deadline => {
				std::thread::sleep(Duration::from_millis(50));
			}
			Err(e) => panic!("failed to bind to {worker_addr} within 5s: {e}"),
		}
	};
	worker_b.set_roots(&[shared]);

	// Next git call must not report too old: reaches worker B and gets NotARepository
	let reply_err = client
		.git(
			&ws,
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();
	match reply_err {
		RemoteError::Refused { code, .. } => {
			assert_eq!(code, ErrorCode::NotARepository);
		}
		other => panic!("expected Refused(NotARepository), got {other:?}"),
	}
	assert_eq!(worker_b.git_requests_seen(), 1);
}

#[test]
fn hand_written_v1_hello_still_lists_workspaces() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()], None);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();

	// Open raw TLS connection
	let (config, _) =
		client_config(&master, Some(paired.pin().unwrap())).unwrap();
	let tcp = TcpStream::connect(w.local_addr()).unwrap();
	tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
		.unwrap();
	tcp.set_write_timeout(Some(scaled(Duration::from_secs(5))))
		.unwrap();
	let conn = rustls::ClientConnection::new(config, server_name()).unwrap();
	let mut tls = StreamOwned::new(conn, tcp);

	// Send v1 hello without max_version
	let v1_hello = serde_json::json!({
		"op": "hello",
		"version": 1,
		"name": "old"
	});
	write_frame(&mut tls, &v1_hello).unwrap();

	let reply: serde_json::Value = read_frame(&mut tls).unwrap().unwrap();
	assert_eq!(reply.get("version").and_then(|v| v.as_u64()), Some(1));
	assert_eq!(reply.get("max_version").and_then(|v| v.as_u64()), Some(1));

	// List workspaces works
	let list_req = serde_json::json!({
		"op": "list_workspaces"
	});
	write_frame(&mut tls, &list_req).unwrap();
	let list_reply: serde_json::Value = read_frame(&mut tls).unwrap().unwrap();
	assert_eq!(
		list_reply.get("reply").and_then(|r| r.as_str()),
		Some("workspaces")
	);

	// Send git_view on v1-negotiated connection
	let git_req = serde_json::json!({
		"op": "git_view",
		"workspace": "w",
		"repo": "",
		"profile": "Interactive",
		"query": { "q": "change_list" }
	});
	write_frame(&mut tls, &git_req).unwrap();
	let git_reply: serde_json::Value = read_frame(&mut tls).unwrap().unwrap();
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
fn new_master_negotiates_two_with_a_new_worker() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()], None);
	let master = Arc::new(Identity::generate().unwrap());
	let conn = Connection::open(&addr(&w), &master, None, "mac").unwrap();
	assert_eq!(conn.version(), 2);
}

#[test]
fn git_requests_need_a_paired_master() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()], None);
	let master = Arc::new(Identity::generate().unwrap());
	let mut conn = Connection::open(&addr(&w), &master, None, "mac").unwrap();
	assert!(!conn.paired());

	let req = Request::GitView {
		workspace: "ws".into(),
		repo: "".into(),
		profile: ReadProfile::Interactive,
		query: GitQuery::ChangeList,
	};
	let res = conn.call(&req, None, CALL_LIMIT_DEFAULT);
	match res {
		Err(RemoteError::Refused { code, .. }) => {
			assert_eq!(code, ErrorCode::NotPaired);
		}
		other => panic!("expected NotPaired refusal, got {other:?}"),
	}
}

#[test]
fn write_and_rename_stay_unsupported() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()], None);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	let client = Client::new(paired, master, "mac".into()).unwrap();
	let ws = client.list_workspaces().unwrap()[0].id.clone();

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

// ---- Fake worker tests for resend rules ----

fn run_fake_worker<F>(
	script: F,
) -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicBool>)
where
	F: Fn(StreamOwned<ServerConnection, TcpStream>, Arc<AtomicUsize>)
		+ Send
		+ Sync
		+ 'static,
{
	let worker_id = Identity::generate().unwrap();
	let config = server_config(&worker_id).unwrap();
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	listener.set_nonblocking(true).unwrap();
	let addr = listener.local_addr().unwrap();

	let git_views = Arc::new(AtomicUsize::new(0));
	let stop = Arc::new(AtomicBool::new(false));
	let script = Arc::new(script);

	let gv_clone = git_views.clone();
	let stop_clone = stop.clone();

	std::thread::spawn(move || {
		while !stop_clone.load(Ordering::SeqCst) {
			match listener.accept() {
				Ok((tcp, _)) => {
					tcp.set_nonblocking(false).unwrap();
					tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
						.unwrap();
					tcp.set_write_timeout(Some(scaled(Duration::from_secs(5))))
						.unwrap();
					let conn = ServerConnection::new(config.clone()).unwrap();
					let mut tls = StreamOwned::new(conn, tcp);

					// Handle Hello
					let hello = read_frame::<Request>(&mut tls).ok().flatten();
					if let Some(Request::Hello { .. }) = hello {
						let _ = write_frame(
							&mut tls,
							&Response::Hello {
								version: PROTOCOL_VERSION,
								name: "fake".into(),
								paired: true,
								max_version: Some(PROTOCOL_MAX),
							},
						);
						let gv = gv_clone.clone();
						let s = script.clone();
						std::thread::spawn(move || {
							s(tls, gv);
						});
					}
				}
				Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(10));
				}
				Err(_) => break,
			}
		}
	});

	(addr, git_views, stop)
}

#[test]
fn cancelled_call_is_not_resent() {
	let cancel_token = CancelToken::new();
	let ct = cancel_token.clone();

	let (addr, gv_count, stop) = run_fake_worker(move |mut tls, gv| {
		loop {
			match read_frame::<Request>(&mut tls) {
				Ok(Some(Request::GitView { .. })) => {
					gv.fetch_add(1, Ordering::SeqCst);
					// Send Pending frames every 100ms
					for _ in 0..50 {
						if write_frame(&mut tls, &Response::Pending).is_err() {
							return;
						}
						// Cancel after first pending arrives
						ct.cancel();
						std::thread::sleep(Duration::from_millis(100));
					}
					return;
				}
				Ok(Some(_)) => {
					let _ = write_frame(
						&mut tls,
						&Response::Workspaces { items: vec![] },
					);
				}
				_ => return,
			}
		}
	});

	let master = Arc::new(Identity::generate().unwrap());
	// Manually open connection with pin None
	let conn =
		Connection::open(&addr.to_string(), &master, None, "mac").unwrap();
	let paired = snip_remote::PairedWorker {
		name: "fake".into(),
		addr: addr.to_string(),
		fingerprint: conn.seen_fingerprint().to_hex(),
	};
	drop(conn);

	let client = Client::new(paired, master, "mac".into()).unwrap();
	let err = client
		.git(
			"ws",
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			Some(&cancel_token),
		)
		.unwrap_err();

	assert!(matches!(err, RemoteError::Cancelled));
	assert_eq!(gv_count.load(Ordering::SeqCst), 1);

	stop.store(true, Ordering::SeqCst);
}

#[test]
fn reset_after_pending_is_not_retried() {
	let (addr, gv_count, stop) = run_fake_worker(|mut tls, gv| {
		loop {
			match read_frame::<Request>(&mut tls) {
				Ok(Some(Request::ListWorkspaces)) => {
					let _ = write_frame(
						&mut tls,
						&Response::Workspaces { items: vec![] },
					);
				}
				Ok(Some(Request::GitView { .. })) => {
					gv.fetch_add(1, Ordering::SeqCst);
					// Send one Pending, then close socket
					let _ = write_frame(&mut tls, &Response::Pending);
					return; // drops TLS and closes socket
				}
				_ => return,
			}
		}
	});

	let master = Arc::new(Identity::generate().unwrap());
	let conn =
		Connection::open(&addr.to_string(), &master, None, "mac").unwrap();
	let paired = snip_remote::PairedWorker {
		name: "fake".into(),
		addr: addr.to_string(),
		fingerprint: conn.seen_fingerprint().to_hex(),
	};
	drop(conn);

	let client = Client::new(paired, master, "mac".into()).unwrap();
	// Run list_workspaces first to populate the pool
	client.list_workspaces().unwrap();

	// Now run git on reused connection. Fake sends Pending then closes.
	let err = client
		.git(
			"ws",
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();

	assert!(
		matches!(err, RemoteError::Io(_)),
		"expected Io error, got {err:?}"
	);
	assert_eq!(gv_count.load(Ordering::SeqCst), 1);

	stop.store(true, Ordering::SeqCst);
}

#[test]
fn reset_before_any_frame_on_a_reused_connection_is_retried_once() {
	let (addr, gv_count, stop) = run_fake_worker(|mut tls, gv| {
		loop {
			match read_frame::<Request>(&mut tls) {
				Ok(Some(Request::ListWorkspaces)) => {
					let _ = write_frame(
						&mut tls,
						&Response::Workspaces { items: vec![] },
					);
				}
				Ok(Some(Request::GitView { .. })) => {
					let count = gv.fetch_add(1, Ordering::SeqCst);
					if count == 0 {
						// First connection: close immediately without answering
						return;
					} else {
						// Second connection (retry): reply successfully
						let _ = write_frame(
							&mut tls,
							&Response::Git(GitReply::Commit("abc".into())),
						);
						return;
					}
				}
				_ => return,
			}
		}
	});

	let master = Arc::new(Identity::generate().unwrap());
	let conn =
		Connection::open(&addr.to_string(), &master, None, "mac").unwrap();
	let paired = snip_remote::PairedWorker {
		name: "fake".into(),
		addr: addr.to_string(),
		fingerprint: conn.seen_fingerprint().to_hex(),
	};
	drop(conn);

	let client = Client::new(paired, master, "mac".into()).unwrap();
	// Populate pool with connection 1
	client.list_workspaces().unwrap();

	// Git call reuses connection 1, connection closes before reply, retries on new connection
	let reply = client
		.git(
			"ws",
			"",
			ReadProfile::Interactive,
			GitQuery::ResolveCommit { rev: "HEAD".into() },
			None,
		)
		.unwrap();

	assert_eq!(reply, GitReply::Commit("abc".into()));
	assert_eq!(gv_count.load(Ordering::SeqCst), 2);

	stop.store(true, Ordering::SeqCst);
}

#[test]
fn downgraded_worker_on_pooled_v2_connection_returns_worker_too_old() {
	let worker_id = Identity::generate().unwrap();
	let config = server_config(&worker_id).unwrap();
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	listener.set_nonblocking(true).unwrap();
	let addr = listener.local_addr().unwrap();

	let conn_count = Arc::new(AtomicUsize::new(0));
	let stop = Arc::new(AtomicBool::new(false));

	let cc_clone = conn_count.clone();
	let stop_clone = stop.clone();

	std::thread::spawn(move || {
		while !stop_clone.load(Ordering::SeqCst) {
			match listener.accept() {
				Ok((tcp, _)) => {
					tcp.set_nonblocking(false).unwrap();
					tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
						.unwrap();
					tcp.set_write_timeout(Some(scaled(Duration::from_secs(5))))
						.unwrap();
					let conn = ServerConnection::new(config.clone()).unwrap();
					let mut tls = StreamOwned::new(conn, tcp);

					let count = cc_clone.fetch_add(1, Ordering::SeqCst);
					std::thread::spawn(move || {
						let hello =
							read_frame::<Request>(&mut tls).ok().flatten();
						if let Some(Request::Hello { .. }) = hello {
							if count <= 1 {
								// Handshake probe (count 0) and pooled connection (count 1): version 2
								let _ = write_frame(
									&mut tls,
									&Response::Hello {
										version: PROTOCOL_VERSION,
										name: "fake".into(),
										paired: true,
										max_version: Some(2),
									},
								);
								if count == 1 {
									// Answer one ListWorkspaces so it gets pooled
									if let Ok(Some(Request::ListWorkspaces)) =
										read_frame::<Request>(&mut tls)
									{
										let _ = write_frame(
											&mut tls,
											&Response::Workspaces {
												items: vec![],
											},
										);
									}
									// When next request comes (GitView), close socket immediately (EOF)
									let _ = read_frame::<Request>(&mut tls);
									// Drops TLS connection
								}
							} else {
								// Reconnected connection (count >= 2): downgraded to version 1
								let _ = write_frame(
									&mut tls,
									&Response::Hello {
										version: PROTOCOL_VERSION,
										name: "fake".into(),
										paired: true,
										max_version: Some(1),
									},
								);
								// Answer subsequent ListWorkspaces
								while let Ok(Some(req)) =
									read_frame::<Request>(&mut tls)
								{
									if let Request::ListWorkspaces = req {
										let _ = write_frame(
											&mut tls,
											&Response::Workspaces {
												items: vec![],
											},
										);
									}
								}
							}
						}
					});
				}
				Err(_) => std::thread::sleep(Duration::from_millis(10)),
			}
		}
	});

	let master = Arc::new(Identity::generate().unwrap());
	let deadline = Instant::now() + scaled(Duration::from_secs(5));
	let conn = loop {
		match Connection::open(&addr.to_string(), &master, None, "mac") {
			Ok(c) => break c,
			Err(_) if Instant::now() < deadline => {
				std::thread::sleep(Duration::from_millis(10));
			}
			Err(e) => panic!("Connection::open failed: {e}"),
		}
	};
	let paired = snip_remote::PairedWorker {
		name: "fake".into(),
		addr: addr.to_string(),
		fingerprint: conn.seen_fingerprint().to_hex(),
	};
	drop(conn);

	let client = Client::new(paired, master, "mac".into()).unwrap();
	// 1. Connection 1 is established and pooled in idle (has version 2)
	let ws = client.list_workspaces().unwrap();
	assert_eq!(ws.len(), 0);

	// 2. Next git call uses pooled connection 1, writes GitView, hits EOF on read,
	// triggers should_resend, reconnects (connection 2), receives max_version 1.
	// Must return WorkerTooOld, not Io.
	let err = client
		.git(
			"ws",
			"",
			ReadProfile::Interactive,
			GitQuery::ChangeList,
			None,
		)
		.unwrap_err();

	match err {
		RemoteError::WorkerTooOld { have, need, .. } => {
			assert_eq!(have, 1);
			assert_eq!(need, 2);
		}
		other => panic!("expected WorkerTooOld, got {other:?}"),
	}

	// 3. Fresh connection (version 1) was pooled into idle, so subsequent non-git call succeeds
	let ws2 = client.list_workspaces().unwrap();
	assert_eq!(ws2.len(), 0);

	stop.store(true, Ordering::SeqCst);
}
