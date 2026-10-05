//! `snip remote`: work on another machine's folders, for scripts and for
//! a machine without the desktop app. Hosts are the ones `~/.ssh/config`
//! names; each command starts `snip serve --stdio` there over ssh.

use std::io::{self, Write};
use std::sync::Arc;

use clap::{ArgGroup, Subcommand};
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
	/// Copy files or changes of a workspace as one snip-sync payload, made
	/// on the host by the same engine a local copy uses.
	Copy {
		host: String,
		workspace: String,
		/// Paths relative to --in (default: the whole folder).
		paths: Vec<String>,
		/// The repository the paths are relative to, inside the workspace.
		#[arg(
			long = "in",
			id = "in_repo",
			value_name = "REPO",
			default_value = ""
		)]
		repo: String,
		/// Uncommitted changes (all of them without paths).
		#[arg(long, conflicts_with_all = ["staged", "commit"])]
		working: bool,
		/// Staged (index) content (all of it without paths).
		#[arg(long, conflicts_with = "commit")]
		staged: bool,
		/// The changes of one commit (all of them without paths).
		#[arg(long, value_name = "SHA")]
		commit: Option<String>,
		/// Print the payload instead of writing the clipboard.
		#[arg(long)]
		stdout: bool,
	},
	/// Copy commits of a repository as a commit payload: the newest first,
	/// then the rest of a contiguous first-parent chain.
	CopyCommits {
		host: String,
		workspace: String,
		#[arg(required = true)]
		shas: Vec<String>,
		/// The repository, inside the workspace.
		#[arg(
			long = "in",
			id = "in_repo",
			value_name = "REPO",
			default_value = ""
		)]
		repo: String,
		/// Print the payload instead of writing the clipboard.
		#[arg(long)]
		stdout: bool,
	},
	/// Restore the clipboard contents into a folder of the host (mode
	/// detected automatically), planned and written there by the same
	/// engine `snip paste` uses.
	#[command(group(
		ArgGroup::new("run").required(true).args(["dry_run", "apply"])
	))]
	Paste {
		host: String,
		workspace: String,
		/// The folder to paste into, inside the workspace (commit payloads:
		/// a repository).
		#[arg(
			long = "in",
			id = "in_repo",
			value_name = "REPO",
			default_value = ""
		)]
		repo: String,
		/// Only list what would happen.
		#[arg(long)]
		dry_run: bool,
		/// Perform the restore.
		#[arg(long)]
		apply: bool,
		/// Overwrite files that already exist.
		#[arg(long, conflicts_with = "skip_existing")]
		overwrite: bool,
		/// Leave files that already exist untouched.
		#[arg(long)]
		skip_existing: bool,
		/// Apply the suggested folder-level path adjustment.
		#[arg(long)]
		adjust_paths: bool,
		/// Read the payload from stdin instead of the clipboard.
		#[arg(long)]
		stdin: bool,
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

/// The folder-expansion cap a local copy derives from settings
/// (`plan_export_expanding`): doubling batches bounded by the file count
/// limit when it applies, unbounded otherwise.
fn expand_limit(settings: &snip_core::settings::Settings) -> usize {
	if settings.set_max_file_count {
		let count_limit = if settings.file_count_limit > 0.0 {
			settings.file_count_limit as usize
		} else {
			0
		};
		64usize.max(4usize.saturating_mul(count_limit))
	} else {
		usize::MAX
	}
}

fn emit(text: &str, stdout: bool) -> Result<(), String> {
	if stdout {
		let mut out = io::stdout().lock();
		return out
			.write_all(text.as_bytes())
			.and_then(|()| out.flush())
			.map_err(|e| e.to_string());
	}
	crate::write_clipboard(text)
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

/// `settings` is the global `--settings` (a paste's header format).
pub fn run(
	cmd: RemoteCommand,
	settings: &snip_core::settings::Settings,
) -> Outcome {
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
			for e in &entries {
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
		RemoteCommand::Copy {
			host,
			workspace,
			paths,
			repo,
			working,
			staged,
			commit,
			stdout,
		} => {
			use snip_core::transfer::SourceKind;
			let c = client(&host);
			let ws = find_workspace(&c, &workspace)?;
			let source = if working {
				SourceKind::Working
			} else if staged {
				SourceKind::Staged
			} else if let Some(rev) = commit {
				SourceKind::Commit { rev }
			} else {
				SourceKind::File
			};
			// The user's --settings reach the worker: filtering, size caps
			// and the header format are the local copy's, and the
			// expansion limit follows them too.
			let limit = expand_limit(settings);
			let out = if paths.is_empty() && source != SourceKind::File {
				// The selection is resolved on the worker with the same
				// `changed_items` a local copy runs, so the payload is the
				// local one — staged-only entries included.
				let git_source = match source {
					SourceKind::Working => GitSource::Working,
					SourceKind::Staged => GitSource::Staged,
					SourceKind::Commit { rev } => GitSource::Commit(rev),
					_ => unreachable!("change sources only"),
				};
				c.export_changes(
					&ws.id,
					&repo,
					&git_source,
					settings,
					limit,
					None,
				)
				.map_err(|e| e.to_string())?
			} else if paths.is_empty() {
				// The whole folder: ONE target the worker expands with the
				// shared copy engine, exactly as the desktop's folder row
				// does — nothing is listed on the client.
				let items = vec![snip_remote::ExportTarget {
					root: repo.clone(),
					path: String::new(),
					source: source.clone(),
					change_type: None,
				}];
				c.export_files(&ws.id, items, settings, limit, None)
					.map_err(|e| e.to_string())?
			} else {
				let items = paths
					.into_iter()
					.map(|path| snip_remote::ExportTarget {
						root: repo.clone(),
						path,
						source: source.clone(),
						change_type: None,
					})
					.collect();
				c.export_files(&ws.id, items, settings, limit, None)
					.map_err(|e| e.to_string())?
			};
			if out.copied == 0 {
				return Err("nothing could be copied".into());
			}
			emit(&out.payload, stdout)?;
			eprintln!(
				"copied {} files, {} chars{}{}",
				out.copied,
				out.chars,
				if out.skipped > 0 {
					format!(", {} skipped", out.skipped)
				} else {
					String::new()
				},
				if out.truncated {
					" (file limit reached)"
				} else {
					""
				},
			);
			Ok(())
		}
		RemoteCommand::CopyCommits {
			host,
			workspace,
			shas,
			repo,
			stdout,
		} => {
			let c = client(&host);
			let ws = find_workspace(&c, &workspace)?;
			let out = c
				.export_commits(&ws.id, &repo, &shas[0], shas.clone(), None)
				.map_err(|e| e.to_string())?;
			emit(&out.text, stdout)?;
			eprintln!(
				"copied {} commits, {} files, {} chars",
				out.commit_count, out.file_count, out.chars
			);
			Ok(())
		}
		RemoteCommand::Paste {
			host,
			workspace,
			repo,
			apply,
			overwrite,
			skip_existing,
			adjust_paths,
			stdin,
			..
		} => {
			let text = crate::read_paste_text(stdin)?;
			let c = client(&host);
			let ws = find_workspace(&c, &workspace)?;
			let opts = crate::PasteOptions {
				apply,
				overwrite,
				skip_existing,
				adjust_paths,
			};
			let at = crate::PasteAt::Remote {
				client: &c,
				workspace: &ws.id,
				dest: &repo,
			};
			crate::paste(at, &text, settings, &opts)
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
