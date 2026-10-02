//! `snip remote`: the master side of remote-node mode, for scripts and for
//! a machine without the desktop app. Pairings and the device identity live
//! in the same config folder the desktop app uses.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use snip_remote::proto::EntryKind;
use snip_remote::{
	default_config_dir, device_name, pair, Client, Fingerprint, Identity,
	PairedWorker, RemoteWorkspace, WORKERS_FILE,
};

#[derive(Subcommand)]
pub enum RemoteCommand {
	/// Pair with a worker using the code it prints.
	Pair {
		/// Tailscale IP or name, optionally with :port.
		addr: String,
		/// The worker's pairing code, e.g. ABCD-EFGH.
		code: String,
	},
	/// List paired workers.
	Workers,
	/// Forget a paired worker.
	Forget { worker: String },
	/// List a worker's shared workspaces.
	Workspaces { worker: String },
	/// List a folder of a shared workspace.
	Ls {
		worker: String,
		workspace: String,
		#[arg(default_value = "")]
		path: String,
	},
	/// Show what a path is, its size and modification time.
	Stat {
		worker: String,
		workspace: String,
		path: String,
	},
	/// Print a text file (binary and non-UTF-8 files are refused).
	Cat {
		worker: String,
		workspace: String,
		path: String,
	},
}

type Outcome = Result<(), String>;

fn config_dir() -> Result<PathBuf, String> {
	default_config_dir().ok_or_else(|| {
		"no config folder: set SNIP_CONFIG_DIR or HOME".to_string()
	})
}

fn identity() -> Result<Arc<Identity>, String> {
	Identity::load_or_create(&config_dir()?)
		.map(Arc::new)
		.map_err(|e| format!("device identity: {e}"))
}

fn workers_file() -> Result<PathBuf, String> {
	Ok(config_dir()?.join(WORKERS_FILE))
}

fn load_workers() -> Result<Vec<PairedWorker>, String> {
	Ok(snip_remote::load_json(&workers_file()?))
}

fn save_workers(workers: &[PairedWorker]) -> Outcome {
	snip_remote::save_json(&workers_file()?, workers)
		.map_err(|e| format!("cannot save pairings: {e}"))
}

/// By name, address, or position in `snip remote workers` (1-based).
fn find_worker(workers: &[PairedWorker], key: &str) -> Result<usize, String> {
	if let Ok(n) = key.parse::<usize>() {
		if (1..=workers.len()).contains(&n) {
			return Ok(n - 1);
		}
	}
	let hits: Vec<usize> = workers
		.iter()
		.enumerate()
		.filter(|(_, w)| w.name == key || w.addr == key)
		.map(|(i, _)| i)
		.collect();
	match hits.as_slice() {
		[one] => Ok(*one),
		[] => Err(format!(
			"no paired worker named {key}; see `snip remote workers`"
		)),
		_ => Err(format!("{key} names several workers; use its number")),
	}
}

fn client(key: &str) -> Result<Client, String> {
	let workers = load_workers()?;
	let worker = workers[find_worker(&workers, key)?].clone();
	Client::new(worker, identity()?, device_name()).map_err(|e| e.to_string())
}

/// By id or, when unique, by name.
fn find_workspace(
	client: &Client,
	key: &str,
) -> Result<RemoteWorkspace, String> {
	let items = client.list_workspaces().map_err(|e| e.to_string())?;
	if let Some(ws) = items.iter().find(|w| w.id == key) {
		return Ok(ws.clone());
	}
	let hits: Vec<&RemoteWorkspace> =
		items.iter().filter(|w| w.name == key).collect();
	match hits.as_slice() {
		[one] => Ok((*one).clone()),
		[] => Err(format!("the worker shares no workspace named {key}")),
		_ => Err(format!("{key} names several workspaces; use its id")),
	}
}

fn short(fingerprint: &str) -> String {
	Fingerprint::from_hex(fingerprint)
		.map(|f| f.short())
		.unwrap_or_else(|| "?".into())
}

pub fn run(cmd: RemoteCommand) -> Outcome {
	match cmd {
		RemoteCommand::Pair { addr, code } => {
			let id = identity()?;
			let worker = pair(&addr, &code, &id, &device_name())
				.map_err(|e| format!("pairing failed: {e}"))?;
			let mut workers = load_workers()?;
			workers.retain(|w| {
				w.fingerprint != worker.fingerprint && w.addr != worker.addr
			});
			println!(
				"paired with {} at {} (fingerprint {}; compare it with the worker's)",
				worker.name,
				worker.addr,
				short(&worker.fingerprint)
			);
			println!("this device: {}", id.fingerprint().short());
			workers.insert(0, worker);
			save_workers(&workers)
		}
		RemoteCommand::Workers => {
			for (i, w) in load_workers()?.iter().enumerate() {
				println!(
					"{}\t{}\t{}\t{}",
					i + 1,
					w.name,
					w.addr,
					short(&w.fingerprint)
				);
			}
			Ok(())
		}
		RemoteCommand::Forget { worker } => {
			let mut workers = load_workers()?;
			let ix = find_worker(&workers, &worker)?;
			let gone = workers.remove(ix);
			save_workers(&workers)?;
			println!("forgot {} at {}", gone.name, gone.addr);
			Ok(())
		}
		RemoteCommand::Workspaces { worker } => {
			let c = client(&worker)?;
			for ws in c.list_workspaces().map_err(|e| e.to_string())? {
				println!("{}\t{}\t{}", ws.id, ws.name, ws.path);
			}
			Ok(())
		}
		RemoteCommand::Ls {
			worker,
			workspace,
			path,
		} => {
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let (entries, truncated) =
				c.list_dir(&ws.id, &path).map_err(|e| e.to_string())?;
			let mut out = io::stdout().lock();
			for e in entries {
				let _ = writeln!(
					out,
					"{}{}",
					e.name,
					if e.directory { "/" } else { "" }
				);
			}
			if truncated {
				eprintln!("(listing truncated)");
			}
			Ok(())
		}
		RemoteCommand::Stat {
			worker,
			workspace,
			path,
		} => {
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let st = c.stat(&ws.id, &path).map_err(|e| e.to_string())?;
			let kind = match st.kind {
				EntryKind::File => "file",
				EntryKind::Directory => "directory",
				EntryKind::Other => "other",
			};
			let modified =
				st.modified.map(|m| m.to_string()).unwrap_or_default();
			println!("{kind}\t{}\t{modified}", st.size);
			Ok(())
		}
		RemoteCommand::Cat {
			worker,
			workspace,
			path,
		} => {
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			match c.read(&ws.id, &path).map_err(|e| e.to_string())? {
				Some(text) => {
					let _ = io::stdout().lock().write_all(text.as_bytes());
					Ok(())
				}
				None => Err(format!("{path} is binary or not UTF-8")),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn w(name: &str, addr: &str) -> PairedWorker {
		PairedWorker {
			name: name.into(),
			addr: addr.into(),
			fingerprint: String::new(),
		}
	}

	#[test]
	fn workers_are_found_by_number_name_or_address() {
		let ws = [w("ubuntu", "100.1.1.1"), w("win", "100.2.2.2")];
		assert_eq!(find_worker(&ws, "2"), Ok(1));
		assert_eq!(find_worker(&ws, "ubuntu"), Ok(0));
		assert_eq!(find_worker(&ws, "100.2.2.2"), Ok(1));
		assert!(find_worker(&ws, "3").is_err());
		assert!(find_worker(&ws, "mac").is_err());
		let dup = [w("same", "a"), w("same", "b")];
		assert!(find_worker(&dup, "same").is_err());
	}
}
