//! A real worker on 127.0.0.1 and a real master, over TLS. Every socket
//! has a timeout (client.rs, worker.rs), so a hang fails instead of
//! blocking the run.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use snip_remote::proto::EntryKind;
use snip_remote::{
	pair, Client, ErrorCode, Identity, RemoteError, Request, Worker,
	WorkerOptions,
};

fn worker(roots: &[&Path]) -> (Worker, Identity) {
	let id = Identity::generate().unwrap();
	let w = Worker::start(
		"127.0.0.1:0".parse().unwrap(),
		&id,
		WorkerOptions {
			name: "win-worker".into(),
			trust_file: None,
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

fn refused(result: Result<impl std::fmt::Debug, RemoteError>) -> ErrorCode {
	match result {
		Err(RemoteError::Refused { code, .. }) => code,
		other => panic!("expected a refusal, got {other:?}"),
	}
}

#[test]
fn pairs_then_lists_stats_and_reads_inside_the_shared_root_only() {
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
	let (w, _) = worker(&[&shared]);
	let master = Arc::new(Identity::generate().unwrap());

	// Unpaired: only hello and pair are served.
	let mut conn =
		snip_remote::Connection::open(&addr(&w), &master, None, "mac").unwrap();
	assert!(!conn.paired());
	assert_eq!(
		refused(conn.call(&Request::ListWorkspaces)),
		ErrorCode::NotPaired
	);

	// No code open, then a wrong code.
	assert_eq!(
		refused(pair(&addr(&w), "AAAA-AAAA", &master, "mac")),
		ErrorCode::PairingRefused
	);
	let code = w.open_pairing();
	assert_eq!(
		refused(pair(&addr(&w), "ZZZZ-ZZZZ", &master, "mac")),
		ErrorCode::PairingRefused
	);
	let paired = pair(&addr(&w), &code.to_lowercase(), &master, "mac").unwrap();
	assert_eq!(paired.name, "win-worker");
	assert_eq!(paired.fingerprint, w.fingerprint().to_hex());
	assert!(!w.pairing_open(), "a code pairs one master");
	assert_eq!(w.trusted().len(), 1);

	let client = Client::new(paired, master.clone(), "mac".into()).unwrap();
	let spaces = client.list_workspaces().unwrap();
	assert_eq!(spaces.len(), 1);
	assert_eq!(spaces[0].name, "real-ws");
	let ws = spaces[0].id.clone();

	let (root, truncated) = client.list_dir(&ws, "").unwrap();
	assert!(!truncated);
	let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
	assert_eq!(names[0], "src", "directories first: {names:?}");
	assert!(names.contains(&"bin.dat"));
	let (src, _) = client.list_dir(&ws, "src").unwrap();
	let nested = src.iter().find(|e| e.name == "nested").unwrap();
	assert!(nested.directory && nested.nested_repo);

	let stat = client.stat(&ws, "src/main.rs").unwrap();
	assert_eq!((stat.kind, stat.size), (EntryKind::File, 13));
	assert_eq!(
		client.read(&ws, "src/main.rs").unwrap().as_deref(),
		Some("fn main() {}\n")
	);
	assert_eq!(
		client.read(&ws, "bin.dat").unwrap(),
		None,
		"binary: no text"
	);

	// Containment: parent components, absolute paths, symlinks out.
	for path in ["../secret.txt", "src/../../secret.txt"] {
		assert_eq!(refused(client.read(&ws, path)), ErrorCode::Forbidden);
		assert_eq!(refused(client.list_dir(&ws, path)), ErrorCode::Forbidden);
	}
	let absolute = tmp.path().join("secret.txt").display().to_string();
	assert_eq!(refused(client.read(&ws, &absolute)), ErrorCode::Forbidden);
	#[cfg(unix)]
	assert_eq!(
		refused(client.read(&ws, "escape.txt")),
		ErrorCode::Forbidden
	);
	assert_eq!(refused(client.read(&ws, "missing.rs")), ErrorCode::NotFound);
	assert_eq!(
		refused(client.list_dir("not-a-workspace", "")),
		ErrorCode::Forbidden
	);

	// Write, rename and Git are reserved for later slices.
	assert_eq!(
		refused(client.call(&Request::Git {
			workspace: ws.clone(),
			args: vec!["status".into()],
		})),
		ErrorCode::Unsupported
	);

	// Unsharing a root takes it away at once.
	w.set_roots(&[]);
	assert_eq!(
		refused(client.read(&ws, "src/main.rs")),
		ErrorCode::Forbidden
	);
}

#[test]
fn a_pinned_master_refuses_a_worker_with_another_certificate() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()]);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let mut paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	// Another worker now answers at an address the master knows.
	let (other, _) = worker(&[tmp.path()]);
	paired.addr = addr(&other);
	let client = Client::new(paired, master, "mac".into()).unwrap();
	let err = client.list_workspaces().unwrap_err();
	assert!(
		matches!(err, RemoteError::Io(_) | RemoteError::Tls(_)),
		"{err:?}"
	);
}

#[test]
fn a_worker_restarted_with_its_trust_file_still_knows_the_master() {
	let tmp = tempfile::tempdir().unwrap();
	let trust = tmp.path().join("cfg").join(snip_remote::TRUSTED_FILE);
	let id_dir = tmp.path().join("cfg");
	let start = || {
		let id = Identity::load_or_create(&id_dir).unwrap();
		let w = Worker::start(
			"127.0.0.1:0".parse().unwrap(),
			&id,
			WorkerOptions {
				name: "w".into(),
				trust_file: Some(trust.clone()),
			},
		)
		.unwrap();
		w.set_roots(&[tmp.path().to_path_buf()]);
		w
	};
	let master = Arc::new(Identity::generate().unwrap());
	let first = start();
	let code = first.open_pairing();
	let mut paired = pair(&addr(&first), &code, &master, "mac").unwrap();
	drop(first);
	let second = start();
	paired.addr = addr(&second);
	let client = Client::new(paired, master, "mac".into()).unwrap();
	assert_eq!(client.list_workspaces().unwrap().len(), 1);
}

#[test]
fn five_wrong_codes_withdraw_the_open_one() {
	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()]);
	let master = Identity::generate().unwrap();
	let code = w.open_pairing();
	for _ in 0..snip_remote::worker::PAIRING_ATTEMPTS {
		let _ = pair(&addr(&w), "QQQQ-QQQQ", &master, "mac");
	}
	assert!(!w.pairing_open());
	assert_eq!(
		refused(pair(&addr(&w), &code, &master, "mac")),
		ErrorCode::PairingRefused
	);
}

/// Found by the two-machine test: 20 parallel `snip remote cat` calls had
/// some connections reset. A connection over the limit now waits for a
/// slot instead, and fails only after [`SLOT_WAIT`].
#[test]
fn a_connection_over_the_limit_waits_for_a_slot_instead_of_being_reset() {
	use snip_remote::worker::{MAX_CONNECTIONS, SLOT_WAIT};
	use std::sync::mpsc;
	use std::time::{Duration, Instant};

	let tmp = tempfile::tempdir().unwrap();
	let (w, _) = worker(&[tmp.path()]);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	// Every slot held by an idle connection, as pooled masters hold them.
	let mut held: Vec<_> = (0..MAX_CONNECTIONS)
		.map(|_| {
			snip_remote::Connection::open(&addr(&w), &master, None, "m")
				.unwrap()
		})
		.collect();
	assert_eq!(w.active_connections(), MAX_CONNECTIONS);

	let (tx, rx) = mpsc::channel();
	let (p, m) = (paired.clone(), master.clone());
	std::thread::spawn(move || {
		let client = Client::new(p, m, "mac".into()).unwrap();
		let _ = tx.send(client.list_workspaces().map(|items| items.len()));
	});
	assert!(
		rx.recv_timeout(Duration::from_millis(500)).is_err(),
		"the extra connection must wait, not be refused"
	);
	held.pop();
	let served = rx
		.recv_timeout(Duration::from_secs(10))
		.expect("the waiting connection got no answer in 10 s");
	assert_eq!(served.unwrap(), 1);

	// With every slot kept, the wait ends in a failure, not a hang.
	held.push(
		snip_remote::Connection::open(&addr(&w), &master, None, "m").unwrap(),
	);
	let started = Instant::now();
	let client = Client::new(paired, master, "mac".into()).unwrap();
	assert!(client.list_workspaces().is_err());
	let waited = started.elapsed();
	assert!(
		waited >= SLOT_WAIT - Duration::from_millis(500)
			&& waited < Duration::from_secs(15),
		"waited {waited:?}"
	);
}

#[test]
fn unwritable_trust_file_refuses_pairing_and_leaves_code_open() {
	let tmp = tempfile::tempdir().unwrap();
	let blocking = tmp.path().join("blocking_file");
	fs::write(&blocking, "not a directory").unwrap();
	let trust_file = blocking.join("remote-trusted-masters.json");

	let id = Identity::generate().unwrap();
	let w = Worker::start(
		"127.0.0.1:0".parse().unwrap(),
		&id,
		WorkerOptions {
			name: "win-worker".into(),
			trust_file: Some(trust_file.clone()),
		},
	)
	.unwrap();
	let code = w.open_pairing();
	let master = Arc::new(Identity::generate().unwrap());

	// First attempt: trust file cannot be written.
	let err = pair(&addr(&w), &code, &master, "mac").unwrap_err();
	match err {
		RemoteError::Refused {
			code: ErrorCode::Io,
			message,
		} => {
			assert!(
				message.contains("cannot save the trusted master"),
				"unexpected message: {message}"
			);
		}
		other => panic!("expected ErrorCode::Io refusal, got {other:?}"),
	}
	assert!(
		w.pairing_open(),
		"pairing window must remain open on save failure"
	);
	assert!(w.trusted().is_empty());

	// Make the path writable by removing the blocking file and creating a directory.
	fs::remove_file(&blocking).unwrap();
	fs::create_dir(&blocking).unwrap();

	// Second attempt with the same pairing code succeeds.
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	assert_eq!(paired.name, "win-worker");
	assert!(!w.pairing_open(), "pairing window consumed after success");
	assert_eq!(w.trusted().len(), 1);
	assert!(trust_file.is_file());
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
	let (w, _) = worker(&[&shared]);
	let master = Arc::new(Identity::generate().unwrap());
	let code = w.open_pairing();
	let paired = pair(&addr(&w), &code, &master, "mac").unwrap();
	let client = Client::new(paired, master, "mac".into()).unwrap();
	let ws = client.list_workspaces().unwrap()[0].id.clone();

	let (root, _) = client.list_dir(&ws, "").unwrap();
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

	let (inner, _) = client.list_dir(&ws, "inner-link").unwrap();
	let names: Vec<_> = inner.iter().map(|e| e.name.as_str()).collect();
	assert_eq!(names, ["deep", "main.rs"]);
	assert_eq!(
		client.read(&ws, "inner-link/main.rs").unwrap().as_deref(),
		Some("fn main() {}\n")
	);
	assert_eq!(
		refused(client.list_dir(&ws, "escape-dir")),
		ErrorCode::Forbidden
	);
}
