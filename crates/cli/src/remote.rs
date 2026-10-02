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
	RemoteWorkspace, WorkerStore,
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

fn client(key: &str) -> Result<Client, String> {
	let store = WorkerStore::in_config_dir(&config_dir()?);
	let worker = store.find(key)?;
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
			println!(
				"paired with {} at {} (fingerprint {}; compare it with the worker's)",
				worker.name,
				worker.addr,
				short(&worker.fingerprint)
			);
			println!("this device: {}", id.fingerprint().short());
			let store = WorkerStore::in_config_dir(&config_dir()?);
			store
				.add(worker)
				.map_err(|e| format!("cannot save pairings: {e}"))
		}
		RemoteCommand::Workers => {
			let store = WorkerStore::in_config_dir(&config_dir()?);
			for (i, w) in store.load().iter().enumerate() {
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
			let store = WorkerStore::in_config_dir(&config_dir()?);
			let found = store.find(&worker)?;
			let gone = store
				.forget(&found.fingerprint)
				.map_err(|e| format!("cannot save pairings: {e}"))?
				.unwrap_or(found);
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
