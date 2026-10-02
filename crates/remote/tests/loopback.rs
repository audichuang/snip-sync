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
