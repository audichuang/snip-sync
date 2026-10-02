//! `snip worker` as its own process: a master pairs with the code it
//! prints, then lists and reads the shared folder over TLS.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use snip_remote::{pair, Client, Identity};

struct Kill(Child);

impl Drop for Kill {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

#[test]
fn cli_worker_pairs_and_serves_its_shared_folder() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir(&shared).unwrap();
	std::fs::write(shared.join("a.txt"), "hello from the worker\n").unwrap();
	let config = tmp.path().join("cfg");
	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["worker", "--listen", "127.0.0.1:0", "--share"])
		.arg(&shared)
		.env("SNIP_CONFIG_DIR", &config)
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()
		.unwrap();
	let stdout = child.stdout.take().unwrap();
	let _guard = Kill(child);
	let (tx, rx) = mpsc::channel();
	std::thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if tx.send(line).is_err() {
				break;
			}
		}
	});
	let (mut addr, mut code) = (None, None);
	while addr.is_none() || code.is_none() {
		let line = rx
			.recv_timeout(Duration::from_secs(30))
			.expect("snip worker printed no address and code within 30 s");
		if let Some(rest) = line.strip_prefix("snip-sync worker listening on ")
		{
			addr = Some(rest.trim().to_string());
		}
		if let Some(rest) = line.strip_prefix("pairing code ") {
			code = rest.split_whitespace().next().map(str::to_string);
		}
	}
	let (addr, code) = (addr.unwrap(), code.unwrap());

	let master = Arc::new(Identity::generate().unwrap());
	let worker = pair(&addr, &code, &master, "mac").unwrap();
	assert!(
		config.join(snip_remote::TRUSTED_FILE).is_file(),
		"the worker keeps its pairings"
	);
	let client = Client::new(worker, master, "mac".into()).unwrap();
	let spaces = client.list_workspaces().unwrap();
	assert_eq!(spaces.len(), 1);
	assert_eq!(spaces[0].name, "proj");
	let (entries, _) = client.list_dir(&spaces[0].id, "").unwrap();
	assert_eq!(entries[0].name, "a.txt");
	assert_eq!(
		client.read(&spaces[0].id, "a.txt").unwrap().as_deref(),
		Some("hello from the worker\n")
	);
}

#[test]
fn cli_worker_needs_a_shared_folder() {
	let out = Command::new(env!("CARGO_BIN_EXE_snip"))
		.arg("worker")
		.output()
		.unwrap();
	assert_eq!(out.status.code(), Some(2));
}
