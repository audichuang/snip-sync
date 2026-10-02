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

/// Starts `snip worker` sharing `shared`; returns it with the address and
/// pairing code it printed.
fn start_worker(
	shared: &std::path::Path,
	config: &std::path::Path,
) -> (Kill, String, String) {
	let mut child = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["worker", "--listen", "127.0.0.1:0", "--share"])
		.arg(shared)
		.env("SNIP_CONFIG_DIR", config)
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
	(_guard, addr.unwrap(), code.unwrap())
}

#[test]
fn cli_worker_pairs_and_serves_its_shared_folder() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir(&shared).unwrap();
	std::fs::write(shared.join("a.txt"), "hello from the worker\n").unwrap();
	let config = tmp.path().join("cfg");
	let (_worker, addr, code) = start_worker(&shared, &config);

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

/// `snip remote …` as a master with its own config folder, against a
/// `snip worker` process.
#[test]
fn cli_master_pairs_lists_stats_and_cats_through_the_worker() {
	let tmp = tempfile::tempdir().unwrap();
	let shared = tmp.path().join("proj");
	std::fs::create_dir_all(shared.join("src")).unwrap();
	std::fs::write(shared.join("src/main.rs"), "fn main() {}\n").unwrap();
	std::fs::write(shared.join("bin.dat"), [0u8, 1, 2]).unwrap();
	let (_worker, addr, code) = start_worker(&shared, &tmp.path().join("w"));
	let master_cfg = tmp.path().join("m");
	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr, &code]).starts_with("paired with "));
	let workers = ok(&["workers"]);
	assert!(
		workers.starts_with("1\t") && workers.contains(&addr),
		"{workers}"
	);
	assert!(ok(&["workspaces", "1"]).contains("\tproj\t"));
	assert_eq!(ok(&["ls", "1", "proj"]), "src/\nbin.dat\n");
	assert_eq!(ok(&["ls", "1", "proj", "src"]), "main.rs\n");
	assert!(ok(&["stat", "1", "proj", "src/main.rs"]).starts_with("file\t13\t"));
	assert_eq!(ok(&["cat", "1", "proj", "src/main.rs"]), "fn main() {}\n");

	let (status, _, stderr) = snip(&["cat", "1", "proj", "bin.dat"]);
	assert_eq!(status, Some(1));
	assert!(stderr.contains("binary"), "{stderr}");
	let (status, _, stderr) = snip(&["cat", "1", "proj", "../secret"]);
	assert_eq!(status, Some(1), "{stderr}");
	// Pairing again from a trusted master is a no-op success; another
	// master finds the code used up.
	ok(&["pair", &addr, &code]);
	let other = Command::new(env!("CARGO_BIN_EXE_snip"))
		.args(["remote", "pair", &addr, &code])
		.env("SNIP_CONFIG_DIR", tmp.path().join("other"))
		.output()
		.unwrap();
	assert_eq!(other.status.code(), Some(1));
	assert!(String::from_utf8_lossy(&other.stderr).contains("pairing failed"));

	assert!(ok(&["forget", "1"]).starts_with("forgot "));
	assert_eq!(ok(&["workers"]), "");
	let (status, _, _) = snip(&["workspaces", "1"]);
	assert_eq!(status, Some(1));
}

#[test]
fn cli_two_workers_paired_forget_and_stale_store_instance() {
	let tmp = tempfile::tempdir().unwrap();
	let shared1 = tmp.path().join("proj1");
	let shared2 = tmp.path().join("proj2");
	std::fs::create_dir_all(&shared1).unwrap();
	std::fs::create_dir_all(&shared2).unwrap();
	std::fs::write(shared1.join("a.txt"), "one\n").unwrap();
	std::fs::write(shared2.join("b.txt"), "two\n").unwrap();

	let (_w1, addr1, code1) = start_worker(&shared1, &tmp.path().join("w1"));
	let (_w2, addr2, code2) = start_worker(&shared2, &tmp.path().join("w2"));
	let master_cfg = tmp.path().join("master");

	let snip = |args: &[&str]| {
		let out = Command::new(env!("CARGO_BIN_EXE_snip"))
			.args(["remote"])
			.args(args)
			.env("SNIP_CONFIG_DIR", &master_cfg)
			.output()
			.unwrap();
		(
			out.status.code(),
			String::from_utf8_lossy(&out.stdout).into_owned(),
			String::from_utf8_lossy(&out.stderr).into_owned(),
		)
	};
	let ok = |args: &[&str]| {
		let (status, stdout, stderr) = snip(args);
		assert_eq!(status, Some(0), "snip remote {args:?}: {stderr}");
		stdout
	};

	assert!(ok(&["pair", &addr1, &code1]).starts_with("paired with "));
	assert!(ok(&["pair", &addr2, &code2]).starts_with("paired with "));

	let workers = ok(&["workers"]);
	assert_eq!(workers.lines().count(), 2);
	assert!(workers.contains(&addr1));
	assert!(workers.contains(&addr2));

	// Stale-list scenario: a second process-less WorkerStore instance forgets a decoy
	// fingerprint without dropping the CLI pairings.
	let store = snip_remote::WorkerStore::in_config_dir(&master_cfg);
	let forgotten = store
		.forget(
			"abababababababababababababababababababababababababababababababab",
		)
		.unwrap();
	assert!(forgotten.is_none());

	let workers_after_stale = ok(&["workers"]);
	assert_eq!(workers_after_stale.lines().count(), 2);
	assert!(workers_after_stale.contains(&addr1));
	assert!(workers_after_stale.contains(&addr2));

	// Forget worker 1 (the more recent one, addr2).
	assert!(ok(&["forget", "1"]).starts_with("forgot "));
	let remaining = ok(&["workers"]);
	assert_eq!(remaining.lines().count(), 1);
	assert!(remaining.starts_with("1\t"));
	assert!(remaining.contains(&addr1));
	assert!(!remaining.contains(&addr2));
}
