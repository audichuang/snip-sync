//! `snip remote`: the master side of remote-node mode, for scripts and for
//! a machine without the desktop app. Pairings and the device identity live
//! in the same config folder the desktop app uses.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use snip_core::format::ChangeType;
use snip_core::gitsrc::GitSource;
use snip_core::gitview::{ChangeSource, Read, ReadProfile, RepoView};
use snip_core::workspace::ScanStatus;
use snip_remote::proto::EntryKind;
use snip_remote::{
	default_config_dir, device_name, pair, Client, Fingerprint, Identity,
	RemoteRepo, RemoteWorkspace, WorkerStore,
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
	/// Find Git repositories within a shared workspace.
	Repos { worker: String, workspace: String },
	/// Changed files in a repository.
	Changes {
		worker: String,
		workspace: String,
		#[arg(id = "repo_path", value_name = "REPO")]
		repo: Option<String>,
	},
	/// Commit history from visible tips.
	Log {
		worker: String,
		workspace: String,
		#[arg(id = "repo_path", value_name = "REPO")]
		repo: Option<String>,
		#[arg(short = 'n', default_value = "50")]
		n: usize,
	},
	/// Changed paths in a commit.
	Show {
		worker: String,
		workspace: String,
		args: Vec<String>,
	},
	/// Diff a file against the working tree, index, or a commit.
	Diff {
		worker: String,
		workspace: String,
		args: Vec<String>,
		/// Staged (index) content.
		#[arg(long, conflicts_with = "commit")]
		staged: bool,
		/// The changes of one commit.
		#[arg(long, conflicts_with = "staged", value_name = "SHA")]
		commit: Option<String>,
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

fn change_char(change: Option<ChangeType>) -> char {
	match change {
		Some(ChangeType::New) => 'N',
		Some(ChangeType::Modified) => 'M',
		Some(ChangeType::Deleted) => 'D',
		Some(ChangeType::Moved) => 'R',
		None => '?',
	}
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
		RemoteCommand::Repos { worker, workspace } => {
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let scan = c
				.scan_repos(&ws.id, None, None)
				.map_err(|e| e.to_string())?;
			if scan.repos.is_empty() && scan.errors.is_empty() {
				eprintln!("no Git repository in {}", ws.name);
			} else {
				for repo in &scan.repos {
					let rel = if repo.rel.is_empty() { "." } else { &repo.rel };
					match &repo.summary {
						Ok(summary) => {
							let branch = summary
								.branch
								.as_deref()
								.unwrap_or("(detached)");
							println!(
								"{rel}\t{branch}\t{}\t{}\t{}\t{}",
								summary.changes.staged,
								summary.changes.unstaged,
								summary.changes.untracked,
								summary.changes.conflicted,
							);
						}
						Err(msg) => {
							println!("{rel}\terror: {msg}");
						}
					}
				}
				for (rel, msg) in &scan.errors {
					let rel = if rel.is_empty() || rel == "." {
						"."
					} else {
						rel.as_str()
					};
					println!("{rel}\terror: {msg}");
				}
				if scan.error_overflow > 0 {
					eprintln!("({} more errors)", scan.error_overflow);
				}
			}
			if scan.status != ScanStatus::Complete {
				eprintln!(
					"scan incomplete: {}",
					format!("{:?}", scan.status).to_ascii_lowercase()
				);
			}
			Ok(())
		}
		RemoteCommand::Changes {
			worker,
			workspace,
			repo,
		} => {
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let repo = repo.unwrap_or_default();
			let view = RemoteRepo::new(Arc::new(c), ws.id, repo);
			let read = Read {
				profile: ReadProfile::Interactive,
				cancel: None,
			};
			let list =
				view.change_list(2000, &read).map_err(|e| e.to_string())?;
			for row in &list.rows {
				let src_char = if row.conflict {
					'C'
				} else {
					match row.source {
						ChangeSource::Staged => 'S',
						ChangeSource::Unstaged => 'U',
						ChangeSource::Working => 'W',
					}
				};
				let type_char = change_char(row.change_type);
				println!("{src_char}\t{type_char}\t{}", row.path);
			}
			if list.total > list.rows.len() {
				eprintln!("showing {} of {}", list.rows.len(), list.total);
			}
			Ok(())
		}
		RemoteCommand::Log {
			worker,
			workspace,
			repo,
			n,
		} => {
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let repo = repo.unwrap_or_default();
			let view = RemoteRepo::new(Arc::new(c), ws.id, repo);
			let read = Read {
				profile: ReadProfile::Interactive,
				cancel: None,
			};
			let snap = view.refs(&read).map_err(|e| e.to_string())?;
			let (commits, more) = view
				.log_from_tips(&snap.tips(), 0, n, &read)
				.map_err(|e| e.to_string())?;
			for commit in commits {
				println!(
					"{}\t{}\t{}",
					commit.sha, commit.author_date, commit.subject
				);
			}
			if more {
				eprintln!("(more commits)");
			}
			Ok(())
		}
		RemoteCommand::Show {
			worker,
			workspace,
			args,
		} => {
			let (repo, sha) = match args.as_slice() {
				[sha] => ("", sha.as_str()),
				[repo, sha] => (repo.as_str(), sha.as_str()),
				[] => return Err("missing commit sha".into()),
				_ => return Err("too many arguments".into()),
			};
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let view = RemoteRepo::new(Arc::new(c), ws.id, repo.to_string());
			let read = Read {
				profile: ReadProfile::Interactive,
				cancel: None,
			};
			let full =
				view.resolve_commit(sha, &read).map_err(|e| e.to_string())?;
			let list = view
				.changed_paths(&GitSource::Commit(full), 5000, &read)
				.map_err(|e| e.to_string())?;
			for (path, change_type) in &list.paths {
				let type_char = change_char(*change_type);
				println!("{type_char}\t{path}");
			}
			Ok(())
		}
		RemoteCommand::Diff {
			worker,
			workspace,
			args,
			staged,
			commit,
		} => {
			let (repo, path) = match args.as_slice() {
				[path] => ("", path.as_str()),
				[repo, path] => (repo.as_str(), path.as_str()),
				[] => return Err("missing path".into()),
				_ => return Err("too many arguments".into()),
			};
			let c = client(&worker)?;
			let ws = find_workspace(&c, &workspace)?;
			let view = RemoteRepo::new(Arc::new(c), ws.id, repo.to_string());
			let read = Read {
				profile: ReadProfile::InteractivePreview,
				cancel: None,
			};
			let source = if staged {
				GitSource::Staged
			} else if let Some(sha) = commit {
				GitSource::Commit(sha)
			} else {
				GitSource::Working
			};
			let prev = view
				.preview(&source, path, None, &read)
				.map_err(|e| e.to_string())?;
			if !prev.patch.is_empty() {
				print!("{}", prev.patch);
			} else if let Some(content) = &prev.content {
				print!("{content}");
			}
			if prev.patch_truncated {
				eprintln!("(patch truncated)");
			}
			Ok(())
		}
	}
}
