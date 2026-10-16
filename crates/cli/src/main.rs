//! `snip`: copy and paste code between machines through the clipboard.
//!
//! A thin shell over `snip-core`; messages mirror the VS Code extension
//! (`extension.ts`, `notify.ts`). Exit codes: 0 success, 1 failure,
//! 2 usage error.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgGroup, CommandFactory, Parser, Subcommand};
use serde::Serialize;
use snip_core::clip::{self, Mode};
use snip_core::commits::{self, CommitCopySummary, CommitsPayload};
use snip_core::copy::CopyResult;
use snip_core::format::parse_clipboard;
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::reason::Reason;
use snip_core::restore::{
	apply_restore_base, is_relative, RestoreBaseSuggestion, RestorePlan,
	RestoreSelection,
};
use snip_core::settings::Settings;
use snip_core::transfer::{
	changed_items, copy_selection_detailed, plan_commit_export_with,
	plan_export_with, selection_from_paths, CanonicalRootId, ExportSelection,
	TransferError, CLIPBOARD_PAYLOAD_MAX,
};
use snip_remote::{PasteError, PasteRoute, PasteTarget};

mod remote;

/// Re-exec argument for the Linux clipboard daemon (arboard's
/// `examples/daemonize.rs`).
#[cfg(target_os = "linux")]
const DAEMON_ARG: &str = "__snip_clipboard_daemon";

const TOKEN_WARN_THRESHOLD: usize = 1_000_000;
const TOKEN_DANGER_THRESHOLD: usize = 2_000_000;

/// snip: copy and paste code between machines through the clipboard.
#[derive(Parser)]
#[command(name = "snip", version)]
struct Cli {
	/// Repository or folder to work in.
	#[arg(long, global = true, default_value = ".")]
	repo: PathBuf,
	/// Settings as a JSON object, or a path to a JSON file.
	#[arg(long, global = true)]
	settings: Option<String>,
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Copy files, git changes, or a range of commits.
	#[command(group(
		ArgGroup::new("source")
			.required(true)
			.args(["paths", "working", "staged", "commit", "range", "commits"])
	))]
	Copy {
		/// Files or folders (their current contents on disk).
		paths: Vec<PathBuf>,
		/// Uncommitted changes (working tree, untracked, index).
		#[arg(long)]
		working: bool,
		/// Staged (index) content.
		#[arg(long)]
		staged: bool,
		/// The changes of one commit.
		#[arg(long, value_name = "SHA")]
		commit: Option<String>,
		/// Endpoint comparison of two revisions.
		#[arg(long, value_name = "A..B")]
		range: Option<String>,
		/// Commit mode: `-n <N>` from HEAD, or `<a>..<b>`.
		#[arg(long, value_name = "A..B", num_args = 0..=1)]
		commits: Option<Option<String>>,
		/// With --commits: the last N commits on HEAD.
		#[arg(short = 'n', value_name = "N", requires = "commits")]
		count: Option<usize>,
		/// Print the payload instead of writing the clipboard.
		#[arg(long)]
		stdout: bool,
	},
	/// Restore the clipboard contents (mode detected automatically).
	#[command(group(
		ArgGroup::new("run").required(true).args(["dry_run", "apply"])
	))]
	Paste {
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
		/// Send the entries under `PREFIX/` to the folder PATH (relative to
		/// --repo; `.` is --repo itself) instead of --repo. Repeatable.
		#[arg(long = "map", value_name = "PREFIX=PATH")]
		map: Vec<String>,
		/// Make the folders --repo and --map name that do not exist yet.
		#[arg(long)]
		create_dirs: bool,
		/// Read the payload from stdin instead of the clipboard.
		#[arg(long)]
		stdin: bool,
	},
	/// Answer a snip-sync master on stdin and stdout. A master starts this
	/// over ssh (`ssh <host> snip serve --stdio`); it is not run by hand.
	Serve {
		/// Speak the protocol on stdin and stdout (the only mode).
		#[arg(long, required = true)]
		stdio: bool,
		/// Test knob, pretend to be an older worker.
		#[arg(long, hide = true, value_name = "N")]
		max_protocol: Option<u32>,
	},
	/// Work on another machine's folders over ssh (hosts from ~/.ssh/config).
	#[command(subcommand)]
	Remote(remote::RemoteCommand),
}

/// A failure after argument parsing. Usage errors exit through clap (2).
type Outcome = Result<(), String>;

fn usage(msg: impl std::fmt::Display) -> ! {
	Cli::command()
		.error(clap::error::ErrorKind::ArgumentConflict, msg)
		.exit()
}

fn main() -> ExitCode {
	#[cfg(target_os = "linux")]
	if std::env::args().nth(1).as_deref() == Some(DAEMON_ARG) {
		let mut text = String::new();
		if io::stdin().read_to_string(&mut text).is_err() {
			return ExitCode::FAILURE;
		}
		return match clip::write_text_and_wait(&text) {
			Ok(()) => ExitCode::SUCCESS,
			Err(_) => ExitCode::FAILURE,
		};
	}

	let cli = Cli::parse();
	let settings = load_settings(cli.settings.as_deref());
	let repo = std::path::absolute(&cli.repo).unwrap_or(cli.repo.clone());
	let result = match cli.command {
		Command::Copy {
			paths,
			working,
			staged,
			commit,
			range,
			commits,
			count,
			stdout,
		} => {
			let source = if working {
				Some(GitSource::Working)
			} else if staged {
				Some(GitSource::Staged)
			} else if let Some(sha) = commit {
				Some(GitSource::Commit(sha))
			} else if let Some(r) = range {
				let (a, b) = split_range(&r);
				Some(GitSource::Range(a, b))
			} else {
				None
			};
			if let Some(commits) = commits {
				copy_commits(&repo, commits, count, stdout)
			} else if let Some(source) = source {
				copy_git(&repo, &source, &settings, stdout)
			} else {
				copy_paths(&repo, &paths, &settings, stdout)
			}
		}
		Command::Paste {
			apply,
			overwrite,
			skip_existing,
			adjust_paths,
			map,
			create_dirs,
			stdin,
			..
		} => {
			let opts = PasteOptions {
				apply,
				overwrite,
				skip_existing,
				adjust_paths,
				maps: local_maps(&repo, &parse_maps(&map)),
				create_dirs,
				repo: Some(repo.clone()),
			};
			read_paste_text(stdin).and_then(|text| {
				paste(
					&PasteTarget::Local {
						dest: repo.clone(),
						bound: None,
					},
					&text,
					&settings,
					&opts,
				)
			})
		}
		Command::Remote(cmd) => remote::run(cmd, &settings),
		Command::Serve { max_protocol, .. } => {
			snip_remote::serve_stdio(snip_remote::WorkerOptions {
				name: snip_remote::device_name(),
				max_protocol,
			})
			.map_err(|err| format!("serve: {err}"))
		}
	};
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(msg) => {
			eprintln!("{msg}");
			ExitCode::FAILURE
		}
	}
}

fn load_settings(arg: Option<&str>) -> Settings {
	let Some(arg) = arg else {
		return Settings::default();
	};
	let json = if arg.trim_start().starts_with('{') {
		arg.to_string()
	} else {
		fs::read_to_string(arg).unwrap_or_else(|e| {
			usage(format!("cannot read settings file {arg}: {e}"))
		})
	};
	serde_json::from_str(&json)
		.unwrap_or_else(|e| usage(format!("invalid settings JSON: {e}")))
}

/// `a..b` with both endpoints present; `a...b` is not a range here.
fn split_range(range: &str) -> (String, String) {
	match range.split_once("..") {
		Some((a, b))
			if !a.is_empty() && !b.is_empty() && !b.starts_with('.') =>
		{
			(a.to_string(), b.to_string())
		}
		_ => usage(format!("expected <a>..<b>, got '{range}'")),
	}
}

// ---- copy ----

fn map_transfer_err(err: TransferError) -> String {
	match err {
		TransferError::EmptySelection => "No files selected.".to_string(),
		other => other.to_string(),
	}
}

fn copy_paths(
	repo: &Path,
	paths: &[PathBuf],
	settings: &Settings,
	stdout: bool,
) -> Outcome {
	let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
	let path_sel =
		selection_from_paths(repo, &cwd, paths).map_err(map_transfer_err)?;
	let cancel = CancelToken::new();
	// The same engine a remote copy and the desktop run, batches doubling
	// past what the filter excludes, so all three surfaces agree.
	let report = copy_selection_detailed(
		path_sel.sel,
		settings,
		crate::remote::expand_limit(settings),
		&RunOptions {
			cancel: Some(cancel),
			..RunOptions::default()
		},
		|_| {},
	)
	.map_err(map_transfer_err)?;

	if report.outcome.copied == 0 {
		return Err("No files selected.".to_string());
	}

	let result = CopyResult {
		files: report.files,
		payload: report.outcome.payload,
		copied_file_count: report.outcome.copied,
		skipped_file_size_count: report.skipped_file_size,
		skipped_unreadable_count: report.skipped_unreadable + path_sel.skipped,
		file_limit_reached: report.file_limit_reached,
	};

	emit(&result.payload, stdout)?;
	let suffix = size_suffix(&result);
	let message = format!(
		"{} file(s) copied{suffix}.{}{}",
		result.copied_file_count,
		limit_note(&result, settings),
		unreadable_note(&result)
	);
	notify_copied(&message, &result, settings);
	Ok(())
}

fn copy_git(
	repo: &Path,
	source: &GitSource,
	settings: &Settings,
	stdout: bool,
) -> Outcome {
	let git = Git::open(repo).map_err(|e| e.to_string())?;
	let graph = matches!(source, GitSource::Commit(_) | GitSource::Range(..));
	let effective_root = if graph { git.root() } else { repo };
	let root_id =
		CanonicalRootId::new(effective_root).map_err(|e| e.to_string())?;
	let changed = changed_items(&root_id, &git, source, &RunOptions::default())
		.map_err(map_transfer_err)?;

	if changed.items.is_empty() {
		return Err(if graph {
			"No source copied.".into()
		} else {
			"No Git changes found to copy.".into()
		});
	}

	let sel = ExportSelection::new(
		vec![effective_root.to_path_buf()],
		Some(effective_root.to_path_buf()),
		changed.items,
	)
	.map_err(map_transfer_err)?;
	let sel = if graph {
		sel.with_filter_root(Some(repo.to_path_buf()))
	} else {
		sel
	};

	let plan = plan_export_with(
		&sel,
		settings,
		Some(CLIPBOARD_PAYLOAD_MAX),
		&RunOptions::default(),
	)
	.map_err(map_transfer_err)?;

	let result = CopyResult {
		files: plan.files,
		payload: plan.payload,
		copied_file_count: plan.copied_file_count,
		skipped_file_size_count: plan.skipped_file_size_count,
		skipped_unreadable_count: plan.skipped_unreadable_count
			+ changed.skipped_non_utf8,
		file_limit_reached: plan.file_limit_reached,
	};

	let message = if graph {
		// copyFullSourceAtCommit
		if result.copied_file_count == 0 && result.skipped_file_size_count == 0
		{
			return Err("No source copied.".into());
		}
		format!(
			"{} file(s) copied{}.{}{}",
			result.copied_file_count,
			size_suffix(&result),
			limit_note(&result, settings),
			unreadable_note(&result)
		)
	} else {
		// copyGitChanges
		if result.files.is_empty() {
			return Err("No Git changes found to copy.".into());
		}
		let mut reasons = Vec::new();
		if result.skipped_file_size_count > 0 {
			reasons.push(format!(
				"{} skipped: size exceeded",
				result.skipped_file_size_count
			));
		}
		if result.skipped_unreadable_count > 0 {
			reasons.push(format!(
				"{} skipped: not UTF-8 text or unreadable",
				result.skipped_unreadable_count
			));
		}
		let skipped = if reasons.is_empty() {
			String::new()
		} else {
			format!(" ({})", reasons.join(", "))
		};
		format!(
			"{} Git file(s) copied{skipped}.{}",
			result.copied_file_count,
			limit_note(&result, settings)
		)
	};
	emit(&result.payload, stdout)?;
	if !changed.out_of_scope.is_empty() {
		eprintln!(
			"{} change(s) outside --repo not copied.",
			changed.out_of_scope.len()
		);
	}
	notify_copied(&message, &result, settings);
	Ok(())
}

fn size_suffix(r: &CopyResult) -> String {
	if r.skipped_file_size_count > 0 {
		format!(" ({} skipped: size exceeded)", r.skipped_file_size_count)
	} else {
		String::new()
	}
}

fn limit_note(r: &CopyResult, settings: &Settings) -> String {
	if r.file_limit_reached {
		format!(" File limit {} reached.", settings.file_count_limit)
	} else {
		String::new()
	}
}

fn unreadable_note(r: &CopyResult) -> String {
	if r.skipped_unreadable_count > 0 {
		format!(
			" {} skipped: not UTF-8 text or unreadable.",
			r.skipped_unreadable_count
		)
	} else {
		String::new()
	}
}

/// `notifyCopied`: the message plus whole-payload stats, on stderr so
/// `--stdout` output stays the bare payload.
fn notify_copied(message: &str, result: &CopyResult, settings: &Settings) {
	if !settings.show_copy_notification {
		return;
	}
	let s = result.stats();
	let mut note = format!(
		"{message} {} chars · {} lines · {} words · ~{} tokens.",
		grouped(s.chars),
		grouped(s.lines),
		grouped(s.words),
		grouped(s.tokens)
	);
	if s.tokens >= TOKEN_DANGER_THRESHOLD {
		note += &format!(" Over {} tokens.", grouped(TOKEN_DANGER_THRESHOLD));
	} else if s.tokens >= TOKEN_WARN_THRESHOLD {
		note += &format!(" Over {} tokens.", grouped(TOKEN_WARN_THRESHOLD));
	}
	eprintln!("{note}");
	// The "Show skipped" details of the graph copy toast.
	for f in &result.files {
		if let Some(reason) = &f.skipped_reason {
			eprintln!("  skipped {}: {reason}", f.path);
		}
	}
}

/// en-US digit grouping, as `toLocaleString('en-US')`.
fn grouped(n: usize) -> String {
	let digits = n.to_string();
	let mut out = String::new();
	for (i, c) in digits.chars().enumerate() {
		if i > 0 && (digits.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(c);
	}
	out
}

fn copy_commits(
	repo: &Path,
	range: Option<String>,
	count: Option<usize>,
	stdout: bool,
) -> Outcome {
	// Usage is checked before touching git.
	let range = match (range, count) {
		(Some(_), Some(_)) => {
			usage("--commits takes either -n <N> or <a>..<b>")
		}
		(None, None) => usage("--commits needs -n <N> or <a>..<b>"),
		(range, _) => range.map(|r| split_range(&r)),
	};
	let git = Git::open(repo).map_err(|e| e.to_string())?;
	let export = plan_commit_export_with(
		&git,
		range.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
		count,
		&RunOptions::default(),
		CLIPBOARD_PAYLOAD_MAX,
	)
	.map_err(|e| e.to_string())?;
	emit(&export.text, stdout)?;
	let CommitCopySummary {
		commit_count,
		file_count,
		chars,
		not_copied_count,
	} = commits::copy_summary(&export.payload, &export.text);
	let not_copied = if not_copied_count > 0 {
		format!(", {not_copied_count} file(s) not copied")
	} else {
		String::new()
	};
	eprintln!(
		"{commit_count} commit(s) copied: {file_count} file(s), {} chars{not_copied}.",
		grouped(chars)
	);
	print_not_copied(&export.payload);
	Ok(())
}

/// "Which commit is missing which files" (spec 4.2).
fn print_not_copied(payload: &CommitsPayload) {
	for (i, c) in payload.commits.iter().enumerate() {
		for f in &c.files {
			if let Some(reason) = f.not_copied {
				eprintln!(
					"  commit {}: {} not copied ({})",
					i + 1,
					f.path,
					tag(&reason)
				);
			}
		}
	}
}

/// The serialized (wire) name of a unit enum variant.
fn tag<T: Serialize>(value: &T) -> String {
	match serde_json::to_value(value) {
		Ok(serde_json::Value::String(s)) => s,
		_ => String::new(),
	}
}

/// Every stdout write goes through this one fallible writer: a reader
/// that closed the pipe (`… | head`) ends the command quietly with
/// success, any other write failure (a full target) comes back as the
/// command's error and exits non-zero.
pub(crate) fn stdout_write(text: &str) -> Outcome {
	let mut out = io::stdout().lock();
	match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
		Ok(()) => Ok(()),
		Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
		Err(e) => Err(e.to_string()),
	}
}

/// One stdout line through [`stdout_write`].
pub(crate) fn stdout_line(text: &str) -> Outcome {
	let mut line = String::with_capacity(text.len() + 1);
	line.push_str(text);
	line.push('\n');
	stdout_write(&line)
}

fn emit(text: &str, stdout: bool) -> Outcome {
	if stdout {
		return stdout_write(text);
	}
	write_clipboard(text)
}

#[cfg(target_os = "linux")]
fn write_clipboard(text: &str) -> Outcome {
	use std::process::{Command, Stdio};
	let unset = |k: &str| std::env::var_os(k).is_none_or(|v| v.is_empty());
	if unset("DISPLAY") && unset("WAYLAND_DISPLAY") {
		return Err(
			"no clipboard: neither DISPLAY nor WAYLAND_DISPLAY is set (use --stdout)"
				.into(),
		);
	}
	// On Linux the owning process serves the contents until the next copy,
	// so hand them to a detached child and return.
	// ponytail: the child's clipboard errors are not reported back; add a
	// ready handshake if silent failures show up.
	let exe = std::env::current_exe().map_err(|e| e.to_string())?;
	let mut child = Command::new(exe)
		.arg(DAEMON_ARG)
		.stdin(Stdio::piped())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.current_dir("/")
		.spawn()
		.map_err(|e| e.to_string())?;
	child
		.stdin
		.take()
		.expect("stdin is piped")
		.write_all(text.as_bytes())
		.map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
fn write_clipboard(text: &str) -> Outcome {
	clip::write_text_and_wait(text).map_err(|e| e.to_string())
}

// ---- paste ----

pub(crate) struct PasteOptions {
	pub apply: bool,
	pub overwrite: bool,
	pub skip_existing: bool,
	pub adjust_paths: bool,
	/// `--map PREFIX=PATH`, each PATH already made the folder the target
	/// pastes into: absolute for a local target, relative to the workspace
	/// for a remote one.
	pub maps: Vec<(String, PathBuf)>,
	/// `--create-dirs`: the user agrees to make the folders that do not exist.
	pub create_dirs: bool,
	/// `--repo` of a local paste, to tell it from a `--map` folder in an error.
	pub repo: Option<PathBuf>,
}

/// `--map PREFIX=PATH` as given: exactly one `=`, a PREFIX that is one
/// non-empty name, each PREFIX once. Anything else is a usage error.
pub(crate) fn parse_maps(maps: &[String]) -> Vec<(String, String)> {
	let mut out: Vec<(String, String)> = Vec::new();
	for map in maps {
		let (prefix, path) = match map.split_once('=') {
			Some((prefix, path)) if !path.contains('=') => (prefix, path),
			_ => usage(format!("--map expects PREFIX=PATH, got '{map}'")),
		};
		if prefix.is_empty() || prefix.contains(['/', '\\']) {
			usage(format!(
				"--map PREFIX must be one folder name, got '{prefix}'"
			));
		}
		if out.iter().any(|(p, _)| p == prefix) {
			usage(format!("--map names '{prefix}' twice"));
		}
		out.push((prefix.to_string(), path.to_string()));
	}
	out
}

/// A local `--map` folder: an absolute PATH as it is, any other under
/// `--repo`.
fn local_maps(
	repo: &Path,
	maps: &[(String, String)],
) -> Vec<(String, PathBuf)> {
	maps.iter()
		.map(|(prefix, path)| {
			let path = Path::new(path);
			let folder = if path.is_absolute() {
				path.to_path_buf()
			} else {
				repo.join(path)
			};
			(prefix.clone(), folder)
		})
		.collect()
}

/// The payload: stdin, or the clipboard.
pub(crate) fn read_paste_text(stdin: bool) -> Result<String, String> {
	if stdin {
		let mut text = String::new();
		io::stdin()
			.read_to_string(&mut text)
			.map_err(|e| e.to_string())?;
		Ok(text)
	} else {
		clip::read_text().map_err(|e| e.to_string())
	}
}

pub(crate) fn paste(
	target: &PasteTarget,
	text: &str,
	settings: &Settings,
	opts: &PasteOptions,
) -> Outcome {
	// JS `trim()`: Unicode whitespace plus the BOM.
	if text
		.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
		.is_empty()
	{
		return Err("Clipboard is empty or does not contain text.".into());
	}
	match clip::detect_mode(text) {
		Mode::Commits => paste_commits(target, text, opts),
		Mode::Files => paste_files(target, text, settings, opts),
	}
}

/// A paste that did not plan, check or write, in the words `snip paste`
/// has always used.
fn format_paste_error(err: PasteError, repo: Option<&Path>) -> String {
	match err {
		// `--repo` unless the folder is another one the paste names (`--map`).
		PasteError::Destination { path, source } => format!(
			"{} {} does not exist or cannot be resolved: {source}",
			if repo.is_none_or(|repo| repo == path) {
				"--repo"
			} else {
				"--map folder"
			},
			path.display()
		),
		e if matches!(e.reason(), Reason::NewDirsNotConfirmed { .. }) => {
			format!(
				"Snipcode refused to paste: {e}; pass --create-dirs to create them"
			)
		}
		PasteError::OutcomeUnknown(e) => format!(
			"The connection was lost; whether the paste finished is unknown ({e}). \
			 Check the destination (snip remote changes / ls) before pasting again."
		),
		e if e.is_collision() => format!("Snipcode refused to paste: {e}"),
		e if e.is_stale() => format!(
			"Snipcode refused to paste: {e}; re-run to inspect updated destinations"
		),
		e if e.is_repo_busy() => format!(
			"Snipcode refused to paste: {e}; try again when the other paste finishes"
		),
		e => e.to_string(),
	}
}

/// Says what `--adjust-paths` would do, or does.
fn note_suggestion(
	suggestion: &snip_core::restore::RestoreBaseSuggestion,
	paths: &[String],
	adjust: bool,
) {
	let s = suggestion;
	let example = paths
		.iter()
		.find(|p| is_relative(p) && p.contains('/'))
		.map(|p| format!(" Example: {p} → {}", apply_restore_base(&s.base, p)))
		.unwrap_or_default();
	if adjust {
		eprintln!(
			"Adjusting paths: {} for all {} file(s).{example}",
			s.label, s.total
		);
	} else {
		eprintln!(
			"These paths look like they belong elsewhere in this folder. \
			 Pass --adjust-paths to {} for all {} file(s).{example}",
			s.label, s.total
		);
	}
}

fn paste_files(
	target: &PasteTarget,
	text: &str,
	settings: &Settings,
	opts: &PasteOptions,
) -> Outcome {
	let entries = parse_clipboard(text, &settings.header_format);
	if entries.is_empty() {
		return Err("No Snipcode file headers found in clipboard.".into());
	}

	// TS asks in a modal; here the suggestion is printed and applied only
	// with --adjust-paths. A worker suggests against its own folders.
	let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
	let route = PasteRoute {
		prefixes: opts.maps.clone(),
		adjust_paths: opts.adjust_paths,
		header_format: settings.header_format.clone(),
		// The folders --repo, --in and --map name may be ones to make: a
		// dry run lists them, an apply needs --create-dirs.
		allow_new_dirs: true,
	};
	let mut note = |s: &RestoreBaseSuggestion| {
		note_suggestion(s, &paths, opts.adjust_paths)
	};
	let import = target
		.plan_files(text, &route, &RunOptions::default(), Some(&mut note))
		.map_err(|e| format_paste_error(e, opts.repo.as_deref()))?;

	let plan = import.restore_plan();
	if !opts.apply {
		print_plan(plan)?;
	}
	if plan.create_operations.is_empty() && plan.delete_operations.is_empty() {
		eprintln!(
			"No actionable files found. Skipped {}.",
			plan.skipped_operations.len()
		);
		return Ok(());
	}
	let new_dirs = import.new_dirs();
	// A dry run names the folders it would make for --repo, --in and --map.
	// The folders a file's own path implies are listed beside them, and only
	// then: a paste into folders that all exist prints what it always did.
	if !opts.apply && !new_dirs.requested.is_empty() {
		for dir in &new_dirs.requested {
			eprintln!("would create folder: {}/", dir.display());
		}
		for dir in &new_dirs.incidental {
			eprintln!(
				"would create folder: {}/ (for file paths)",
				dir.display()
			);
		}
	}
	eprintln!("{}", confirmation_summary(plan));
	if !opts.apply {
		return Ok(());
	}

	let existing = plan.existing_targets();
	if existing > 0 && !opts.overwrite && !opts.skip_existing {
		usage(format!(
			"{existing} file(s) already exist; pass --overwrite or --skip-existing"
		));
	}
	if !new_dirs.requested.is_empty() && !opts.create_dirs {
		let folders: Vec<String> = new_dirs
			.requested
			.iter()
			.map(|d| format!("{}/", d.display()))
			.collect();
		return Err(format!(
			"Snipcode refused to paste: these folders do not exist: {}; pass --create-dirs to create them",
			folders.join(", ")
		));
	}
	let mut selection = RestoreSelection {
		overwrite_existing: opts.overwrite,
		skip_existing: opts.skip_existing,
		..Default::default()
	};
	if opts.create_dirs {
		// The agreement is to the folders the rows kept need, as the plan
		// lists them.
		selection.confirmed_new_dirs = import
			.new_dirs_for(&selection)
			.requested
			.into_iter()
			.collect();
	}
	let result = target
		.apply_files(text, &route, &import, &selection)
		.map_err(|e| format_paste_error(e, opts.repo.as_deref()))?;

	let parts: Vec<String> = [
		("Created folders", result.created_dirs),
		("Created", result.created_count),
		("Overwritten", result.overwritten_count),
		("Skipped", result.total_skipped()),
		("Deleted", result.deleted_count),
	]
	.iter()
	.filter(|(_, n)| *n > 0)
	.map(|(label, n)| format!("{label} {n}"))
	.collect();
	if parts.is_empty() {
		stdout_line("No files changed.")?;
	} else {
		stdout_line(&parts.join(", "))?;
	}
	if result.errors.is_empty() {
		return Ok(());
	}
	Err(format!(
		"Snipcode failed {} operation(s):\n  {}",
		result.errors.len(),
		result.errors.join("\n  ")
	))
}

fn print_plan(plan: &RestorePlan) -> Outcome {
	for op in &plan.create_operations {
		let action = if op.existed { "overwrite" } else { "create" };
		stdout_line(&format!("{action}\t{}", op.relative_path))?;
	}
	for op in &plan.delete_operations {
		stdout_line(&format!("delete\t{}", op.relative_path))?;
	}
	for op in &plan.skipped_operations {
		let detail = match op.detail {
			Some(snip_core::restore::SkipDetail::ProtectedGitMetadata) => {
				"\tprotected Git metadata"
			}
			None => "",
		};
		stdout_line(&format!(
			"skip\t{}\t{}{detail}",
			op.raw_path,
			op.reason.as_str()
		))?;
	}
	Ok(())
}

/// `confirmationSummary`.
fn confirmation_summary(plan: &RestorePlan) -> String {
	let overwrite = plan.existing_targets();
	format!(
		"Snipcode will create {}, overwrite {overwrite}, delete {}, and skip {} operation(s).",
		plan.create_operations.len() - overwrite,
		plan.delete_operations.len(),
		plan.skipped_operations.len()
	)
}

fn paste_commits(
	target: &PasteTarget,
	text: &str,
	opts: &PasteOptions,
) -> Outcome {
	if opts.skip_existing {
		usage("commit payloads do not support --skip-existing");
	}
	if opts.adjust_paths {
		usage("commit payloads do not support --adjust-paths");
	}
	if !opts.maps.is_empty() {
		usage("commit payloads do not support --map");
	}
	if opts.create_dirs {
		usage("commit payloads do not support --create-dirs");
	}
	let payload =
		commits::parse_commit_payload(text).map_err(|e| e.to_string())?;

	// The write scope is the folder pasted into: targets outside it are
	// refused, and a whole-repo replay opens the repo root.
	let (preview, prepared) = target
		.plan_commits(text, payload.clone(), &RunOptions::default())
		.map_err(|e| format_paste_error(e, opts.repo.as_deref()))?;

	if !opts.apply {
		let plan = preview.plan();
		let total = plan.commits.len();
		for (i, c) in plan.commits.iter().enumerate() {
			let subject = c.message.lines().next().unwrap_or("");
			let refused_suffix = match c.refused_by() {
				Some(conflict) => {
					format!(" (refused: {})", conflict.describe())
				}
				None => String::new(),
			};
			stdout_line(&format!(
				"[{}/{total}] {subject}{refused_suffix}\n      {} <{}> {}",
				i + 1,
				c.author_name,
				c.author_email,
				c.author_date
			))?;
			for f in &c.files {
				let path = match &f.old_path {
					Some(old) => format!("{old} -> {}", f.path),
					None => f.path.clone(),
				};
				let why = f
					.not_copied
					.map(|r| tag(&r))
					.or(f.skip_reason.map(|r| tag(&r)))
					.map(|r| format!("\t{r}"))
					.unwrap_or_default();
				stdout_line(&format!(
					"  {}\t{}\t{path}{why}",
					tag(&f.action).to_lowercase(),
					tag(&f.change).to_lowercase()
				))?;
			}
		}
		let first_refusal =
			plan.commits.iter().position(|c| c.refused_by().is_some());
		if let Some(first_idx) = first_refusal {
			let would_create = first_idx;
			let not_reached = total.saturating_sub(first_idx + 1);
			eprintln!(
				"{would_create} commit(s) would be created; replay stops at commit #{} (refused); {not_reached} not reached.",
				first_idx + 1
			);
			let hint =
				plan.commits[first_idx].refused_by().and_then(|c| c.hint());
			if let Some(hint) = hint {
				eprintln!("hint: {hint}");
			}
		} else {
			eprintln!("{total} commit(s) would be created.");
		}
		if !opts.overwrite {
			let existing = plan.existing_writes().len();
			if existing > 0 {
				eprintln!(
					"{existing} destination file(s) already exist; --apply will need --overwrite."
				);
			}
		}
		return Ok(());
	}

	let existing = preview.plan().existing_writes().len();
	if existing > 0 && !opts.overwrite {
		target
			.check_commits(text, &preview, &prepared)
			.map_err(|e| format_paste_error(e, opts.repo.as_deref()))?;
		usage(format!(
			"{existing} destination file(s) already exist; commit payloads need --overwrite"
		));
	}

	let result = target
		.apply_commits(text, &preview, &prepared)
		.map_err(|e| format_paste_error(e, opts.repo.as_deref()))?;
	stdout_line(&format!("Created {} commit(s).", result.created.len()))?;
	for sha in &result.created {
		stdout_line(&format!("  {sha}"))?;
	}
	match result.failure {
		None => Ok(()),
		Some(f) => Err(format!(
			"Commit {} of {} failed ({}): {}{}",
			f.index + 1,
			payload.commits.len(),
			f.message.lines().next().unwrap_or(""),
			f.error,
			f.layout_conflict
				.and_then(|c| c.hint())
				.map_or(String::new(), |h| format!("\nhint: {h}"))
		)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn grouped_matches_en_us() {
		assert_eq!(grouped(0), "0");
		assert_eq!(grouped(999), "999");
		assert_eq!(grouped(1000), "1,000");
		assert_eq!(grouped(1234567), "1,234,567");
	}

	/// The words `snip paste` refuses or fails with, captured from the
	/// formatters before the paste seam. A changed literal is a changed CLI.
	#[test]
	fn paste_error_texts_are_unchanged() {
		use snip_core::gitsrc::GitError;
		use snip_remote::{ErrorCode, RemoteError};
		fn missing(path: &Path, e: io::Error) -> String {
			format_paste_error(
				PasteError::Destination {
					path: path.to_path_buf(),
					source: e,
				},
				Some(path),
			)
		}
		fn local(e: TransferError) -> String {
			format_paste_error(PasteError::Local(e), None)
		}
		fn remote(e: RemoteError) -> String {
			format_paste_error(PasteError::Remote(e), None)
		}
		// What `apply_failure` does to an Apply's error.
		fn apply(e: RemoteError) -> String {
			format_paste_error(
				if e.outcome_unknown() {
					PasteError::OutcomeUnknown(e)
				} else {
					PasteError::Remote(e)
				},
				None,
			)
		}
		let refused = |code, message: &str| RemoteError::Refused {
			code,
			message: message.into(),
			reason: None,
			suggestion: None,
		};
		const COLLISION: &str =
			"target collision: multiple operations target 'a.txt': two entries";
		const STALE: &str = "stale destination in '/w': target file 'a' was created externally after preview";
		const BUSY: &str =
			"paste refused: another paste is writing to this repository";

		assert_eq!(
			missing(
				Path::new("/nope"),
				io::Error::new(io::ErrorKind::NotFound, "gone")
			),
			"--repo /nope does not exist or cannot be resolved: gone"
		);
		assert_eq!(
			local(TransferError::TargetCollision {
				path: "a.txt".into(),
				msg: "two entries".into()
			}),
			format!("Snipcode refused to paste: {COLLISION}")
		);
		assert_eq!(
			local(TransferError::StaleDestination { root: "/r".into(), reason: "target file 'a' was modified externally after preview".into(), change: snip_core::reason::StaleChange::Modified { rel: "a".into() } }),
			"Snipcode refused to paste: stale destination in '/r': target file 'a' was modified externally after preview; re-run to inspect updated destinations"
		);
		assert_eq!(
			local(TransferError::Git(GitError::WorktreeBusy { args: "paste".into() })),
			format!("Snipcode refused to paste: {BUSY}; try again when the other paste finishes")
		);
		for e in [
			TransferError::SpecialFile("p".into()),
			TransferError::DestinationNotRegular("p".into()),
		] {
			assert_eq!(local(e), "'p' exists and is not a regular file; paste refuses to overwrite it");
		}
		assert_eq!(
			local(TransferError::UnsafePath("../x".into())),
			"unsafe path: '../x' escapes root boundaries or contains invalid characters"
		);
		assert_eq!(
			remote(refused(ErrorCode::Collision, COLLISION)),
			format!("Snipcode refused to paste: {COLLISION}")
		);
		let stale_text = format!("Snipcode refused to paste: {STALE}; re-run to inspect updated destinations");
		assert_eq!(remote(refused(ErrorCode::Stale, STALE)), stale_text);
		// A worker says the repository is busy by its reason; the same words
		// from one that sends none (an old worker) are only a busy refusal.
		let repo_busy = RemoteError::Refused {
			code: ErrorCode::Busy,
			message: BUSY.into(),
			reason: Some(Box::new(snip_core::reason::Reason::RepoBusy)),
			suggestion: None,
		};
		assert_eq!(
			remote(repo_busy),
			format!("Snipcode refused to paste: {BUSY}; try again when the other paste finishes")
		);
		assert_eq!(remote(refused(ErrorCode::Busy, BUSY)), BUSY);
		assert_eq!(
			remote(refused(ErrorCode::Busy, "too many callers")),
			"too many callers"
		);
		assert_eq!(
			remote(RemoteError::Start {
				kind: snip_remote::ConnectFailureKind::Other,
				reason: None,
				detail: snip_core::reason::Datum::transport_stderr(
					"ssh: no route"
				),
				message: "ssh: no route".into(),
			}),
			"ssh: no route"
		);
		assert_eq!(
			apply(RemoteError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))),
			"The connection was lost; whether the paste finished is unknown (pipe closed). Check the destination (snip remote changes / ls) before pasting again."
		);
		assert_eq!(
			apply(RemoteError::TimedOut),
			"The connection was lost; whether the paste finished is unknown (the worker did not answer in time). Check the destination (snip remote changes / ls) before pasting again."
		);
		// A refused Apply is a refusal, not a lost answer.
		assert_eq!(apply(refused(ErrorCode::Stale, STALE)), stale_text);
		// A replay answered without its result keeps its bare words.
		assert_eq!(
			format_paste_error(PasteError::NoReplayResult, None),
			"the replay returned no result"
		);
		// A folder a `--map` names is not `--repo`.
		assert_eq!(
			format_paste_error(
				PasteError::Destination {
					path: "/lib".into(),
					source: io::Error::new(io::ErrorKind::NotFound, "gone")
				},
				Some(Path::new("/repo"))
			),
			"--map folder /lib does not exist or cannot be resolved: gone"
		);
		// A folder to make that was not agreed to, said by the engine here or
		// by a worker's reason, says how to agree.
		let not_confirmed = "Snipcode refused to paste: 1 folder(s) would be created without consent: /w/new; pass --create-dirs to create them";
		assert_eq!(
			local(TransferError::NewDirsNotConfirmed {
				dirs: vec!["/w/new".into()]
			}),
			not_confirmed
		);
		assert_eq!(
			remote(RemoteError::Refused {
				code: ErrorCode::BadRequest,
				message: "1 folder(s) would be created without consent: /w/new".into(),
				reason: Some(Box::new(Reason::NewDirsNotConfirmed { count: 1 })),
				suggestion: None,
			}),
			"Snipcode refused to paste: 1 folder(s) would be created without consent: /w/new; pass --create-dirs to create them"
		);
	}
}
