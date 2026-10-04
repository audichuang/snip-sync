//! `snip remote`: work on another machine's folders, for scripts and for
//! a machine without the desktop app. Hosts are the ones `~/.ssh/config`
//! names; each command starts `snip serve --stdio` there over ssh.

use std::io::{self, Write};
use std::sync::Arc;

use clap::Subcommand;
use snip_core::format::ChangeType;
use snip_core::gitsrc::GitSource;
use snip_core::gitview::{ChangeSource, Read, ReadProfile, RepoView};
use snip_core::workspace::ScanStatus;
use snip_remote::proto::EntryKind;
use snip_remote::{
	device_name, Client, RemoteHost, RemoteRepo, RemoteWorkspace,
};

#[derive(Subcommand)]
pub enum RemoteCommand {
	/// List the hosts in ~/.ssh/config.
	Hosts,
	/// List a folder of a workspace (a folder path on the host, or ~/…).
	Ls {
		host: String,
		workspace: String,
		#[arg(default_value = "")]
		path: String,
	},
	/// Show what a path is, its size and modification time.
	Stat {
		host: String,
		workspace: String,
		path: String,
	},
	/// Print a text file (binary and non-UTF-8 files are refused).
	Cat {
		host: String,
		workspace: String,
		path: String,
	},
	/// Find Git repositories within a workspace.
	Repos { host: String, workspace: String },
	/// Changed files in a repository.
	Changes {
		host: String,
		workspace: String,
		#[arg(id = "repo_path", value_name = "REPO")]
		repo: Option<String>,
	},
	/// Commit history from visible tips.
	Log {
		host: String,
		workspace: String,
		#[arg(id = "repo_path", value_name = "REPO")]
		repo: Option<String>,
		#[arg(short = 'n', default_value = "50")]
		n: usize,
	},
	/// Changed paths in a commit.
	Show {
		host: String,
		workspace: String,
		args: Vec<String>,
	},
	/// Diff a file against the working tree, index, or a commit.
	Diff {
		host: String,
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

fn client(host: &str) -> Client {
	Client::new(RemoteHost::ssh(host), device_name())
}

/// `workspace` is a folder on the host: absolute, or `~` / `~/…`.
fn find_workspace(
	client: &Client,
	workspace: &str,
) -> Result<RemoteWorkspace, String> {
	client.open_workspace(workspace).map_err(|e| e.to_string())
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
		RemoteCommand::Hosts => {
			let mut out = io::stdout().lock();
			for host in snip_remote::ssh::config_hosts() {
				let _ = writeln!(out, "{host}");
			}
			Ok(())
		}
		RemoteCommand::Ls {
			host,
			workspace,
			path,
		} => {
			let c = client(&host);
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
			host,
			workspace,
			path,
		} => {
			let c = client(&host);
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
			host,
			workspace,
			path,
		} => {
			let c = client(&host);
			let ws = find_workspace(&c, &workspace)?;
			match c.read(&ws.id, &path).map_err(|e| e.to_string())? {
				Some(text) => {
					let _ = io::stdout().lock().write_all(text.as_bytes());
					Ok(())
				}
				None => Err(format!("{path} is binary or not UTF-8")),
			}
		}
		RemoteCommand::Repos { host, workspace } => {
			let c = client(&host);
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
			host,
			workspace,
			repo,
		} => {
			let c = client(&host);
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
			host,
			workspace,
			repo,
			n,
		} => {
			let c = client(&host);
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
			host,
			workspace,
			args,
		} => {
			let (repo, sha) = match args.as_slice() {
				[sha] => ("", sha.as_str()),
				[repo, sha] => (repo.as_str(), sha.as_str()),
				[] => return Err("missing commit sha".into()),
				_ => return Err("too many arguments".into()),
			};
			let c = client(&host);
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
			host,
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
			let c = client(&host);
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
