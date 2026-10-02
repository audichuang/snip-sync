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
use snip_core::commits::{
	self, CommitCopySummary, CommitsPayload, ReplayAction,
};
use snip_core::copy::CopyResult;
use snip_core::format::{extract_source_root, parse_clipboard};
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::restore::{
	apply_restore_base, is_relative, suggest_restore_base, FsProbe,
	RestorePlan, RestoreSelection,
};
use snip_core::settings::Settings;
use snip_core::transfer::{
	changed_items, plan_commit_export_with, plan_export_expanding,
	plan_export_with, plan_import_with, selection_from_paths, CanonicalRootId,
	CommitReplayPreview, ExportSelection, ImportMapping, TransferError,
	CLIPBOARD_PAYLOAD_MAX,
};

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
		/// Read the payload from stdin instead of the clipboard.
		#[arg(long)]
		stdin: bool,
	},
	/// Serve folders to a paired snip-sync desktop app (remote-node
	/// worker). Runs until stopped; prints a one-time pairing code.
	Worker {
		/// A folder the master may browse (repeatable). Nothing outside
		/// these folders is served.
		#[arg(long = "share", value_name = "DIR", required = true)]
		shares: Vec<PathBuf>,
		/// Address to listen on, e.g. the machine's Tailscale IP.
		#[arg(long, value_name = "ADDR:PORT", default_value = snip_remote::DEFAULT_LISTEN)]
		listen: std::net::SocketAddr,
	},
	/// Operate a paired worker's shared folders (remote-node master).
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
			stdin,
			..
		} => {
			let opts = PasteOptions {
				apply,
				overwrite,
				skip_existing,
				adjust_paths,
			};
			paste(&repo, &settings, &opts, stdin)
		}
		Command::Remote(cmd) => remote::run(cmd),
		Command::Worker { shares, listen } => {
			let config = snip_remote::default_config_dir();
			match snip_remote::run_headless_worker(
				listen,
				&shares,
				config.as_deref(),
			) {
				Ok(never) => match never {},
				Err(err) => Err(format!("cannot start the worker: {err}")),
			}
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
	let (plan, expanded_skipped) = plan_export_expanding(
		&path_sel.sel,
		settings,
		Some(CLIPBOARD_PAYLOAD_MAX),
		&RunOptions::default(),
		&cancel,
	)
	.map_err(map_transfer_err)?;

	if plan.files.is_empty() {
		return Err("No files selected.".to_string());
	}

	let result = CopyResult {
		files: plan.files,
		payload: plan.payload,
		copied_file_count: plan.copied_file_count,
		skipped_file_size_count: plan.skipped_file_size_count,
		skipped_unreadable_count: plan.skipped_unreadable_count
			+ path_sel.skipped
			+ expanded_skipped,
		file_limit_reached: plan.file_limit_reached,
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

fn emit(text: &str, stdout: bool) -> Outcome {
	if stdout {
		let mut out = io::stdout().lock();
		return out
			.write_all(text.as_bytes())
			.and_then(|()| out.flush())
			.map_err(|e| e.to_string());
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

struct PasteOptions {
	apply: bool,
	overwrite: bool,
	skip_existing: bool,
	adjust_paths: bool,
}

fn paste(
	repo: &Path,
	settings: &Settings,
	opts: &PasteOptions,
	stdin: bool,
) -> Outcome {
	let text = if stdin {
		let mut text = String::new();
		io::stdin()
			.read_to_string(&mut text)
			.map_err(|e| e.to_string())?;
		text
	} else {
		clip::read_text().map_err(|e| e.to_string())?
	};
	// JS `trim()`: Unicode whitespace plus the BOM.
	if text
		.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
		.is_empty()
	{
		return Err("Clipboard is empty or does not contain text.".into());
	}
	match clip::detect_mode(&text) {
		Mode::Commits => paste_commits(repo, &text, opts),
		Mode::Files => paste_files(repo, &text, settings, opts),
	}
}

fn format_transfer_error(err: TransferError) -> String {
	match err {
		TransferError::TargetCollision { .. } => {
			format!("Snipcode refused to paste: {err}")
		}
		TransferError::StaleDestination { .. } => {
			format!("Snipcode refused to paste: {err}; re-run to inspect updated destinations")
		}
		TransferError::SpecialFile(_)
		| TransferError::DestinationNotRegular(_) => err.paste_message(),
		other => other.to_string(),
	}
}

fn paste_files(
	repo: &Path,
	text: &str,
	settings: &Settings,
	opts: &PasteOptions,
) -> Outcome {
	let entries = parse_clipboard(text, &settings.header_format);
	if entries.is_empty() {
		return Err("No Snipcode file headers found in clipboard.".into());
	}

	// TS asks in a modal; here the suggestion is printed and applied only
	// with --adjust-paths.
	let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
	let suggestion = suggest_restore_base(
		&repo.to_string_lossy(),
		&paths,
		&FsProbe,
		extract_source_root(text).as_deref(),
	);
	if let Some(ref s) = suggestion {
		let example = paths
			.iter()
			.find(|p| is_relative(p) && p.contains('/'))
			.map(|p| {
				format!(" Example: {p} → {}", apply_restore_base(&s.base, p))
			})
			.unwrap_or_default();
		if opts.adjust_paths {
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

	let root_id = CanonicalRootId::new(repo).map_err(|e| {
		format!(
			"--repo {} does not exist or cannot be resolved: {e}",
			repo.display()
		)
	})?;
	let mapping = match (&suggestion, opts.adjust_paths) {
		(Some(s), true) => ImportMapping::from_restore_base(s, root_id),
		_ => ImportMapping::with_primary(root_id),
	};

	let import_plan = plan_import_with(
		text,
		&settings.header_format,
		&[repo.to_path_buf()],
		&mapping,
		&RunOptions::default(),
	)
	.map_err(format_transfer_error)?;

	let plan = import_plan.restore_plan();
	if !opts.apply {
		print_plan(plan);
	}
	if plan.create_operations.is_empty() && plan.delete_operations.is_empty() {
		eprintln!(
			"No actionable files found. Skipped {}.",
			plan.skipped_operations.len()
		);
		return Ok(());
	}
	eprintln!("{}", confirmation_summary(plan));
	if !opts.apply {
		return Ok(());
	}

	let existing = plan.create_operations.iter().filter(|o| o.existed).count();
	if existing > 0 && !opts.overwrite && !opts.skip_existing {
		usage(format!(
			"{existing} file(s) already exist; pass --overwrite or --skip-existing"
		));
	}
	let result = import_plan
		.apply(&RestoreSelection {
			overwrite_existing: opts.overwrite,
			skip_existing: opts.skip_existing,
			..Default::default()
		})
		.map_err(format_transfer_error)?;

	let parts: Vec<String> = [
		("Created", result.created_count),
		("Overwritten", result.overwritten_count),
		("Skipped", result.skipped_existing_count),
		("Deleted", result.deleted_count),
	]
	.iter()
	.filter(|(_, n)| *n > 0)
	.map(|(label, n)| format!("{label} {n}"))
	.collect();
	if parts.is_empty() {
		println!("No files changed.");
	} else {
		println!("{}", parts.join(", "));
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

fn print_plan(plan: &RestorePlan) {
	for op in &plan.create_operations {
		let action = if op.existed { "overwrite" } else { "create" };
		println!("{action}\t{}", op.relative_path);
	}
	for op in &plan.delete_operations {
		println!("delete\t{}", op.relative_path);
	}
	for op in &plan.skipped_operations {
		println!("skip\t{}\t{}", op.raw_path, op.reason.as_str());
	}
}

/// `confirmationSummary`.
fn confirmation_summary(plan: &RestorePlan) -> String {
	let overwrite = plan.create_operations.iter().filter(|o| o.existed).count();
	format!(
		"Snipcode will create {}, overwrite {overwrite}, delete {}, and skip {} operation(s).",
		plan.create_operations.len() - overwrite,
		plan.delete_operations.len(),
		plan.skipped_operations.len()
	)
}

fn paste_commits(repo: &Path, text: &str, opts: &PasteOptions) -> Outcome {
	if opts.skip_existing {
		usage("commit payloads do not support --skip-existing");
	}
	if opts.adjust_paths {
		usage("commit payloads do not support --adjust-paths");
	}
	let payload =
		commits::parse_commit_payload(text).map_err(|e| e.to_string())?;

	let preview = CommitReplayPreview::capture(repo, &payload)
		.map_err(format_transfer_error)?;

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
			println!(
				"[{}/{total}] {subject}{refused_suffix}\n      {} <{}> {}",
				i + 1,
				c.author_name,
				c.author_email,
				c.author_date
			);
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
				println!(
					"  {}\t{}\t{path}{why}",
					tag(&f.action).to_lowercase(),
					tag(&f.change).to_lowercase()
				);
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
		} else {
			eprintln!("{total} commit(s) would be created.");
		}
		if !opts.overwrite {
			let existing = distinct_existing_paths(plan);
			if existing > 0 {
				eprintln!(
					"{existing} destination file(s) already exist; --apply will need --overwrite."
				);
			}
		}
		return Ok(());
	}

	let existing = distinct_existing_paths(preview.plan());
	if existing > 0 && !opts.overwrite {
		preview.revalidate().map_err(format_transfer_error)?;
		usage(format!(
			"{existing} destination file(s) already exist; commit payloads need --overwrite"
		));
	}

	let result = preview.apply().map_err(format_transfer_error)?;
	println!("Created {} commit(s).", result.created.len());
	for sha in &result.created {
		println!("  {sha}");
	}
	match result.failure {
		None => Ok(()),
		Some(f) => Err(format!(
			"Commit {} of {} failed ({}): {}",
			f.index + 1,
			payload.commits.len(),
			f.message.lines().next().unwrap_or(""),
			f.error
		)),
	}
}

fn is_existing_write(f: &commits::FilePlan) -> bool {
	f.layout_conflict.is_none() && f.action == ReplayAction::Write && f.existed
}

fn distinct_existing_paths(plan: &commits::CommitReplayPlan) -> usize {
	plan.commits
		.iter()
		.flat_map(|c| &c.files)
		.filter(|f| is_existing_write(f))
		.map(|f| &f.path)
		.collect::<std::collections::BTreeSet<_>>()
		.len()
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
}
