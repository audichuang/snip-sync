//! snip-desktop-native: Native Rust GPUI Git Workbench.
//!
//! Shares `snip-core` directly for Git inspection, preview, and clipboard
//! operations. Stdout carries no logs in normal runs: `app_log!` events and
//! E2E probes are only emitted when `SNIP_NATIVE_E2E=1` (set by the tests);
//! `[READY:*]` markers for the memory harness are printed only in their
//! explicit `--mode`.

#![cfg_attr(
	all(target_os = "windows", not(test)),
	windows_subsystem = "windows"
)]

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

static E2E: OnceLock<bool> = OnceLock::new();

/// True only when the E2E harness opted in with `SNIP_NATIVE_E2E=1`.
pub fn e2e_on() -> bool {
	*E2E.get_or_init(|| {
		std::env::var_os("SNIP_NATIVE_E2E").is_some_and(|v| v == "1")
	})
}

/// Cancels the previous job in `slot` and returns the token for the new one.
pub(crate) fn arm_cancel(slot: &mut Option<CancelToken>) -> CancelToken {
	if let Some(prev) = slot.take() {
		prev.cancel();
	}
	let token = CancelToken::new();
	*slot = Some(token.clone());
	token
}

/// The native file-mode file cap. ClipCode's 30 suits a handful of picked
/// files; a Project folder brings every file under it, so the native app
/// (which has no settings UI) caps at this and lets the 32 MiB payload cap
/// bound the bytes. Hitting either is reported, never silent.
const NATIVE_FILE_COUNT_LIMIT: usize = 10_000;

fn native_export_settings() -> Settings {
	Settings {
		file_count_limit: NATIVE_FILE_COUNT_LIMIT as f64,
		..Settings::default()
	}
}

/// Interactive read options that carry `cancel` into `Git::open_with`.
pub(crate) fn interactive_read_opts(cancel: CancelToken) -> RunOptions {
	RunOptions {
		cancel: Some(cancel),
		..RunOptions::interactive(None)
	}
}

/// Replace the container so its backing store is dropped. `clear` keeps it.
fn release_vec<T>(slot: &mut Vec<T>) {
	*slot = Vec::new();
}

#[cfg(test)]
fn release_map<K, V, S: Default>(slot: &mut HashMap<K, V, S>) {
	*slot = HashMap::default();
}

fn release_path(slot: &mut PathBuf) {
	*slot = PathBuf::new();
}

/// Re-runs one test alone in a child process so it can read process-global counters (gitrun budgets) without other tests' git calls racing; true inside the child.
#[cfg(test)]
pub(crate) fn run_isolated(exact_test_path: &str) -> bool {
	if std::env::var_os("SNIP_TEST_ISOLATED").is_none() {
		let exe = std::env::current_exe().expect("current test exe");
		let output = std::process::Command::new(exe)
			.env("SNIP_TEST_ISOLATED", "1")
			.args([
				"--exact",
				exact_test_path,
				"--nocapture",
				"--test-threads=1",
			])
			.output()
			.expect("spawn isolated test subprocess");
		assert!(
			output.status.success(),
			"isolated subprocess test failed ({exact_test_path}): status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
			output.status,
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr)
		);
		false
	} else {
		true
	}
}

/// Test-harness event line. A background workspace tab's lines carry its
/// `ws_tab=<id>` ([`tabs::tag_line`]).
macro_rules! app_log {
	($($arg:tt)*) => {{
		let line = $crate::tabs::tag_line(format!($($arg)*));
		#[cfg(test)]
		$crate::test_log::push(&line);
		println!("{line}");
		let _ = std::io::Write::flush(&mut std::io::stdout());
	}};
}

/// The lines `app_log!` printed on this thread, for a test of what a driver
/// would read. A `#[gpui::test]` runs its foreground work on the test's own
/// thread, and each test is a process of its own under nextest, so one
/// test's lines never mix with another's.
#[cfg(test)]
pub(crate) mod test_log {
	use std::cell::RefCell;
	use std::collections::VecDeque;

	/// The oldest lines are dropped past this many, so a long test cannot
	/// grow the buffer without bound.
	const KEEP: usize = 10_000;

	thread_local! {
		static LINES: RefCell<VecDeque<String>> =
			const { RefCell::new(VecDeque::new()) };
	}

	pub(crate) fn push(line: &str) {
		LINES.with(|lines| {
			let mut lines = lines.borrow_mut();
			if lines.len() >= KEEP {
				lines.pop_front();
			}
			lines.push_back(line.to_string());
		});
	}

	/// The lines so far; the buffer is empty afterwards.
	pub(crate) fn take() -> Vec<String> {
		LINES.with(|lines| lines.borrow_mut().drain(..).collect())
	}
}

/// A commit copy's failure: the engine's own text (what the CLI prints, for
/// the e2e log) beside the message the UI words in the language then in force.
type CopyCommitsFailure = (String, Msg);

/// The `COPY_COMMITS_ERR` e2e line. Drivers match its text, so it is the
/// engine's, never a translation's wording.
fn copy_commits_err_line(engine_text: &str) -> String {
	format!("[APP:COPY_COMMITS_ERR: {engine_text}]")
}

/// Exports the selected commits of a local repository.
fn export_commits_local(
	repo_root: &std::path::Path,
	tip_sha: &str,
	selected: &[String],
	cancel: CancelToken,
) -> Result<(String, usize, Msg), CopyCommitsFailure> {
	let opts = interactive_read_opts(cancel);
	let git = Git::open_with(repo_root, &opts)
		// ALLOWED-ERR-TEXT: COPY_COMMITS_ERR prints the engine's Display (an e2e contract)
		.map_err(|e| (e.to_string(), crate::remote::git_error_msg(e)))?;
	let exported = plan_commit_export_exact_with(
		&git,
		tip_sha,
		selected,
		&opts,
		snip_core::transfer::CLIPBOARD_PAYLOAD_MAX,
	)
	// ALLOWED-ERR-TEXT: COPY_COMMITS_ERR prints the engine's Display (an e2e contract)
	.map_err(|e| (e.to_string(), crate::reason::reason_msg(&e.reason())))?;
	let out = exported.outcome();
	let status = commit_copied_status(&out);
	Ok((out.text, out.commit_count, status))
}

/// Most not-copied paths the commit copy toast names; the rest are counted.
const COMMIT_TOAST_PATHS: usize = 3;

/// The commit copy toast (spec 4.2): commit, file and character counts, and
/// when files were left out, which commit lost which files.
fn commit_copied_status(out: &snip_core::commits::CommitCopyOutcome) -> Msg {
	let mut args = vec![
		out.commit_count.to_string(),
		out.file_count.to_string(),
		out.chars.to_string(),
	];
	if out.not_copied.is_empty() {
		return Msg::new("status_commits_copied", args);
	}
	// "#n path, path; #m path", at most COMMIT_TOAST_PATHS paths.
	let mut parts: Vec<(usize, Vec<&str>)> = Vec::new();
	for (n, path) in out.not_copied.iter().take(COMMIT_TOAST_PATHS) {
		match parts.last_mut() {
			Some((m, paths)) if m == n => paths.push(path),
			_ => parts.push((*n, vec![path.as_str()])),
		}
	}
	let mut detail = parts
		.iter()
		.map(|(n, paths)| format!("#{} {}", n + 1, paths.join(", ")))
		.collect::<Vec<_>>()
		.join("; ");
	if out.not_copied.len() > COMMIT_TOAST_PATHS {
		detail.push_str(" …");
	}
	args.push(out.not_copied.len().to_string());
	args.push(detail);
	Msg::new("status_commits_copied_skipped", args)
}

/// The copy toast when nothing could be copied: a worker that skipped
/// every file it saw (unreadable, oversize) says so, as the local copy's
/// `EmptySelection` does. Local and remote copies report the same
/// [`snip_core::transfer::CopyOutcome`] shape.
fn remote_copy_nothing_status(out: &snip_core::transfer::CopyOutcome) -> Msg {
	if out.skipped > 0 {
		Msg::new("status_copy_nothing_skipped", [])
	} else {
		Msg::new("status_copy_nothing", [])
	}
}

/// The copy toast: a partial copy (file limit hit in the folder walk or
/// in the plan) always says so, with the limit. Local and remote copies
/// report the same [`snip_core::transfer::CopyOutcome`] shape.
fn copied_status(
	repo_name: String,
	out: &snip_core::transfer::CopyOutcome,
) -> Msg {
	let (words, tokens) =
		if out.words > 0 || out.tokens > 0 || out.payload.is_empty() {
			(out.words, out.tokens)
		} else {
			let s = snip_core::stats::payload_stats(&out.payload);
			(s.words, s.tokens)
		};
	let mut args = vec![
		repo_name,
		out.copied.to_string(),
		out.chars.to_string(),
		out.lines.to_string(),
		words.to_string(),
		tokens.to_string(),
		out.skipped.to_string(),
	];
	if !out.truncated {
		return Msg::new("status_copied", args);
	}
	if e2e_on() {
		app_log!("[APP:COPY_TRUNCATED: limit={NATIVE_FILE_COUNT_LIMIT}]");
	}
	args.push(NATIVE_FILE_COUNT_LIMIT.to_string());
	Msg::new("status_copied_limit", args)
}

/// Measurement harness readiness marker (`--mode idle|overview|preview`).
fn ready_marker(name: &str) {
	println!("[READY:{name}]");
	let _ = std::io::Write::flush(&mut std::io::stdout());
}

mod dirtree;
mod githost;
pub mod graph_view;
mod history;
pub mod i18n;
mod icons;
pub mod lifecycle;
mod menu;
mod multi_log;
pub mod open_tabs;
mod overlay;
pub mod paste;
mod paste_dest;
mod reader;
mod reason;
mod recent;
pub mod remote;
mod repo_set;
mod selector;
pub mod syntax;
pub mod tabs;
mod text_input;
pub mod theme;
pub mod tree;
mod ui;

use gpui::{
	actions, prelude::*, px, size, App, Application, Bounds, Context, Entity,
	FocusHandle, KeyBinding, PathPromptOptions, WindowBounds, WindowOptions,
};
use snip_core::browser::{self, CommitSummary, GitReference};
use snip_core::clip;
use snip_core::format::ChangeType;
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::graph::GraphLayout;
use snip_core::reason::{Failure, Reason};
use snip_core::settings::Settings;
use snip_core::transfer::{
	copy_selection_detailed, plan_commit_export_exact_with, CanonicalRootId,
	ExportItem, ExportSelection, SourceKind,
};
use snip_core::workspace::{
	DiscoveredRepo, Discovery, RepoIdentity, RepoSummary, ScanBudget,
	ScanStatus,
};

use crate::history::RevTree;
use crate::i18n::{Locale, Msg};
use crate::paste::PastePreviewPlan;
use crate::reader::{Preview, PreviewSource, Reader};
use crate::repo_set::{RepoOp, RepoSet};
use crate::syntax::Language;
use crate::text_input::{InputEvent, TextInput};
use crate::tree::FileTreeNode;
use crate::tree::{NodeKey, TreeCommand, TreeEffect, TreeIo};
use snip_core::browser::LogQuery;

actions!(
	workbench,
	[
		Quit,
		CopySelection,
		PastePreview,
		ApplyPaste,
		Refresh,
		NavUp,
		NavDown,
		NavToggle,
		NavCollapse,
		NavExpand,
		SelectRepo1,
		SelectRepo2,
		ToggleTab,
		FocusNext,
		FocusPrev,
		ShowProject,
		ShowChanges,
		ToggleLog,
		OpenRepoSelector,
		OpenRefSelector,
		CloseWorkspace,
		OpenWorkspace,
		ToggleLocale,
		HistoryNextPage,
		HistoryPrevPage,
		FindInFile,
		GotoLine,
		FindNext,
		FindPrev,
		ReaderCopy,
		ReaderSelectAll,
		ReaderUp,
		ReaderDown,
		ReaderPageUp,
		ReaderPageDown,
		TreeUp,
		TreeDown,
		TreeExpand,
		TreeCollapse,
		TreeOpen,
		TreeToggle,
		LogUp,
		LogDown,
		LogExtendUp,
		LogExtendDown,
		LogOpen,
		LogSearchFocus,
		LogHead,
		LogCopy,
		// IntelliJ chrome (IJ-2c).
		NextDiff,
		PrevDiff,
		MenuUp,
		MenuDown,
		MenuConfirm,
		LogParent,
		LogChild,
		LogPageUp,
		LogPageDown,
		ToolPageUp,
		ToolPageDown,
		HideToolWindow,
		OpenTabMenu,
		// Workspace tabs.
		PrevWorkspaceTab,
		NextWorkspaceTab,
		WorkspaceTab1,
		WorkspaceTab2,
		WorkspaceTab3,
		WorkspaceTab4,
		WorkspaceTab5,
		WorkspaceTab6,
		WorkspaceTab7,
		WorkspaceTab8,
		WorkspaceTab9,
	]
);

#[derive(Clone, Debug)]
pub struct FileChangeItem {
	pub path: String,
	pub change_type: Option<ChangeType>,
	pub source: SourceKind,
	pub is_conflict: bool,
	/// Index of the owning repo in `WorkbenchModel::change_repos`.
	pub repo: u32,
}

impl FileChangeItem {
	/// False when Git's path bytes were not UTF-8: the lossy name here does
	/// not exist on disk, so copying it would make the whole Copy fail.
	// ponytail: core's status listing is already lossy, so U+FFFD is the only
	// signal (a real U+FFFD name is refused too); carry raw bytes from core to
	// tell them apart.
	pub fn is_valid_utf8(&self) -> bool {
		!self.path.contains(char::REPLACEMENT_CHARACTER)
	}
}

type WorkingChangeTuple = (String, Option<ChangeType>, SourceKind, bool);

/// Rows kept per repo in the Changes tool window.
pub const MAX_CHANGES_PER_REPO: usize = snip_core::gitview::MAX_CHANGE_ROWS;

/// Status reads the Changes queue runs at once (the Git runner's own limit).
pub const MAX_CHANGES_READS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeRepoState {
	Loading,
	Loaded,
	Failed(Msg),
}

/// Failed repos the log's banner names before "…".
pub(crate) const FAILED_FEEDS_NAMED: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangesEmpty {
	NoWorkspace,
	Scanning,
	Loading,
	NoRepository,
	ScanFailed(Msg),
	NoMatch,
	CleanPartial,
	Clean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEmpty {
	NoWorkspace,
	Scanning,
	Loading,
	NoRepository,
	Failed(Msg),
	Empty,
}

/// One repository node of the Changes tool window. Its rows are the
/// contiguous run of `WorkbenchModel::files` tagged with this slot's index.
#[derive(Clone, Debug)]
pub struct ChangeRepo {
	pub root: PathBuf,
	pub name: String,
	pub state: ChangeRepoState,
	/// Changes Git reported; more than the kept rows when truncated.
	pub total: usize,
}

impl ChangeRepo {
	pub fn truncated(&self, kept: usize) -> bool {
		self.total > kept
	}
}

/// FIFO of pending reads with at most `max` in flight.
#[derive(Debug)]
pub struct ReadQueue<T> {
	pending: VecDeque<T>,
	in_flight: usize,
	max: usize,
}

impl<T: PartialEq> ReadQueue<T> {
	pub fn new(max: usize) -> Self {
		Self {
			pending: VecDeque::new(),
			in_flight: 0,
			max,
		}
	}

	pub fn push(&mut self, item: T) {
		if !self.pending.contains(&item) {
			self.pending.push_back(item);
		}
	}

	/// The next read to start, if a slot is free.
	pub fn take(&mut self) -> Option<T> {
		if self.in_flight >= self.max {
			return None;
		}
		let item = self.pending.pop_front()?;
		self.in_flight += 1;
		Some(item)
	}

	/// A started read ended (landed, failed, cancelled or went stale).
	pub fn finish(&mut self) {
		self.in_flight = self.in_flight.saturating_sub(1);
	}

	/// Drops the waiting reads; running ones still `finish`.
	pub fn clear_pending(&mut self) {
		self.pending = VecDeque::new();
	}

	pub fn in_flight(&self) -> usize {
		self.in_flight
	}
}

/// Marks `root`'s Changes repo rows expanded in every group.
fn expand_repo_everywhere(
	expanded: &mut Vec<(&'static str, PathBuf)>,
	root: &std::path::Path,
) {
	for (group, _) in menu::CHANGE_GROUPS {
		if !expanded.iter().any(|(g, r)| *g == group && r == root) {
			expanded.push((group, root.to_path_buf()));
		}
	}
}

/// Rows of slot `slot` in `files`, which is sorted by slot.
pub fn slot_range(
	files: &[FileChangeItem],
	slot: usize,
) -> std::ops::Range<usize> {
	let start = files.partition_point(|f| (f.repo as usize) < slot);
	let end = files.partition_point(|f| (f.repo as usize) <= slot);
	start..end
}

/// Index of `root`'s slot, inserting one in name order if it is new. Rows of
/// later slots are renumbered so `files` stays sorted by slot.
pub fn slot_insert(
	slots: &mut Vec<ChangeRepo>,
	files: &mut [FileChangeItem],
	root: &std::path::Path,
	name: &str,
) -> usize {
	if let Some(pos) = slots.iter().position(|s| s.root == root) {
		slots[pos].name = name.to_string();
		return pos;
	}
	let pos = slots.partition_point(|s| {
		(s.name.as_str(), s.root.as_path()) < (name, root)
	});
	slots.insert(
		pos,
		ChangeRepo {
			root: root.to_path_buf(),
			name: name.to_string(),
			state: ChangeRepoState::Loading,
			total: 0,
		},
	);
	for f in files.iter_mut().filter(|f| f.repo as usize >= pos) {
		f.repo += 1;
	}
	pos
}

/// Whether the slots are in `(name, root)` order.
pub fn slots_sorted(slots: &[ChangeRepo]) -> bool {
	slots.iter().map(|s| (&s.name, &s.root)).is_sorted()
}

/// Puts the slots back in `(name, root)` order after a rename moved some,
/// renumbering their rows. The rows of one slot keep their order.
pub fn slots_resort(slots: &mut Vec<ChangeRepo>, files: &mut [FileChangeItem]) {
	let mut order: Vec<usize> = (0..slots.len()).collect();
	order.sort_by(|&a, &b| {
		(slots[a].name.as_str(), slots[a].root.as_path())
			.cmp(&(slots[b].name.as_str(), slots[b].root.as_path()))
	});
	let mut new_of_old = vec![0u32; slots.len()];
	for (new, &old) in order.iter().enumerate() {
		new_of_old[old] = new as u32;
	}
	for f in files.iter_mut() {
		f.repo = new_of_old[f.repo as usize];
	}
	files.sort_by_key(|f| f.repo);
	let mut old: Vec<Option<ChangeRepo>> =
		std::mem::take(slots).into_iter().map(Some).collect();
	*slots = order
		.into_iter()
		.map(|i| old[i].take().expect("each slot moves once"))
		.collect();
}

/// Removes slot `slot` and its rows.
pub fn slot_remove(
	slots: &mut Vec<ChangeRepo>,
	files: &mut Vec<FileChangeItem>,
	slot: usize,
) {
	let range = slot_range(files, slot);
	files.drain(range);
	for f in files.iter_mut().filter(|f| f.repo as usize > slot) {
		f.repo -= 1;
	}
	slots.remove(slot);
}

/// Replaces slot `slot`'s rows with `rows` (already tagged with it).
pub fn slot_replace_rows(
	files: &mut Vec<FileChangeItem>,
	slot: usize,
	rows: Vec<FileChangeItem>,
) {
	let range = slot_range(files, slot);
	files.splice(range, rows);
}

/// Reads one repo's Changes list: the staged, unstaged, untracked and
/// conflicted rows sorted by path, plus the summary when `known` spares the
/// identity reads.
fn read_change_list(
	host: &crate::githost::GitHost,
	root: &std::path::Path,
	known: Option<&RepoIdentity>,
	cancel: CancelToken,
) -> Result<(Option<RepoSummary>, Vec<WorkingChangeTuple>, usize), Msg> {
	let read = snip_core::gitview::Read {
		profile: snip_core::gitview::ReadProfile::Interactive,
		cancel: Some(cancel),
	};
	let repo = host.open(root, known, &read)?;
	let list = repo
		.change_list(MAX_CHANGES_PER_REPO, &read)
		.map_err(crate::remote::git_error_msg)?;
	let summary = match (known, list.summary) {
		(Some(id), Some(s)) => Some(RepoSummary {
			identity: id.clone(),
			head: s.head,
			branch: s.branch,
			changes: s.changes,
		}),
		_ => None,
	};
	let total = list.total;
	let items = list
		.rows
		.into_iter()
		.map(|r| {
			let source = match r.source {
				snip_core::gitview::ChangeSource::Staged => SourceKind::Staged,
				snip_core::gitview::ChangeSource::Unstaged => {
					SourceKind::Unstaged
				}
				snip_core::gitview::ChangeSource::Working => {
					SourceKind::Working
				}
			};
			(r.path, r.change_type, source, r.conflict)
		})
		.collect();
	Ok((summary, items, total))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkbenchTab {
	GitChanges,
	FileExplorer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoEntryKind {
	Main,
	LinkedWorktree,
	Submodule,
	UninitializedSubmodule,
}

impl From<snip_core::gitview::FoundKind> for RepoEntryKind {
	fn from(kind: snip_core::gitview::FoundKind) -> Self {
		match kind {
			snip_core::gitview::FoundKind::Main => RepoEntryKind::Main,
			snip_core::gitview::FoundKind::LinkedWorktree => {
				RepoEntryKind::LinkedWorktree
			}
			snip_core::gitview::FoundKind::Submodule => {
				RepoEntryKind::Submodule
			}
			snip_core::gitview::FoundKind::UninitializedSubmodule => {
				RepoEntryKind::UninitializedSubmodule
			}
		}
	}
}

/// One discovered repository; a failed status read stays visible as an error
/// instead of silently disappearing or showing as clean.
#[derive(Clone, Debug)]
pub struct RepoEntry {
	pub root: PathBuf,
	pub name: String,
	pub kind: RepoEntryKind,
	pub identity: Option<RepoIdentity>,
	/// A failed read keeps the worker's or git's text for now and the
	/// reason a front end words.
	pub summary: Result<RepoSummary, Failure>,
}

impl From<snip_core::gitview::FoundRepo> for RepoEntry {
	fn from(repo: snip_core::gitview::FoundRepo) -> Self {
		RepoEntry {
			root: repo.root,
			name: repo.name,
			kind: repo.kind.into(),
			identity: repo.identity,
			summary: repo.summary,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Popover {
	Repo,
	Ref,
}

#[derive(Debug)]
enum PasteDiffOutcome {
	Same,
	Diff(String, bool),
	Binary,
	TooLarge,
	Unreadable,
	Notice(Msg),
}

impl PasteDiffOutcome {
	fn commit(
		request: paste::CommitDiffRequest,
		cancel: CancelToken,
		skip_notice: &'static str,
	) -> Self {
		use snip_core::transfer::ReplayPreviewUnavailable as Why;
		let opts = snip_core::gitrun::RunOptions::preview(Some(cancel.clone()));
		match request.target.preview_commit_file(
			&request.preview,
			request.commit,
			request.file,
			request.side,
			snip_core::browser::PREVIEW_LIMIT,
			&opts,
		) {
			Ok(preview) => match preview.unavailable {
				Some(why) => PasteDiffOutcome::Notice(Msg::new(
					match why {
						Why::Skipped => skip_notice,
						Why::Binary => "paste_diff_binary",
						Why::TooLarge => "paste_diff_too_large",
						Why::ContentTooLarge => "paste_diff_content_too_large",
						Why::Symlink => "paste_diff_symlink",
						Why::Directory => "paste_diff_directory",
						Why::Unreadable => "paste_diff_unreadable",
					},
					[],
				)),
				None if preview.before == preview.after => {
					PasteDiffOutcome::Same
				}
				None => {
					let (patch, truncated) = snip_core::textdiff::unified(
						&preview.path,
						&preview.before,
						&preview.after,
						snip_core::browser::PREVIEW_LIMIT,
					);
					PasteDiffOutcome::Diff(patch, truncated)
				}
			},
			Err(err) => {
				// A remote failure keeps its start reason as a message,
				// worded when drawn (#206 #208).
				let why = crate::reason::reason_msg(&err.reason());
				PasteDiffOutcome::Notice(Msg::with_msg_arg(
					"paste_diff_failed",
					[String::new()],
					0,
					why,
				))
			}
		}
	}
}

pub struct WorkbenchModel {
	pub workspace_root: PathBuf,
	/// `--restore-dir`: the paste destination of the launch's own tab only.
	pub restore_dir: Option<PathBuf>,
	pub repos: RepoSet,
	pub selected_repo_idx: Option<usize>,

	// Git log.
	pub commits: Vec<CommitSummary>,
	pub refs: Vec<GitReference>,
	pub head_sha: Option<String>,
	/// Layout of the loaded page as displayed (collapse applied).
	pub graph_layout: Option<GraphLayout>,
	pub active_ref_filter: Option<String>,
	pub commit_page: usize,
	pub history_has_more: bool,
	pub history_page_size: usize,
	pub history_error: Option<Msg>,
	pub page_checkpoints: Vec<Option<snip_core::graph::GraphCheckpoint>>,
	pub log_search: Option<LogQuery>,
	pub collapsed_merges: Vec<String>,
	/// Commits hidden on this page by collapsed merges.
	pub hidden_commits: Vec<String>,
	/// Snapshot and commit window later graph pages continue from.
	pub history_walk: Option<crate::history::HistoryWalk>,
	pub selected_commit: Option<String>,
	/// Range endpoint picked with shift (anchor is `selected_commit`).
	pub range_head: Option<String>,
	/// Every selected row id in display order, when more than one is
	/// selected (shift range or Cmd/Ctrl-click); one repository only.
	pub log_selected: Vec<String>,
	pub log_scroll: gpui::UniformListScrollHandle,
	pub select_head_after_load: bool,

	// Git log pane (IJ-2a).
	/// First page of the loaded window (`commit_page` is the last).
	pub log_first_page: usize,
	/// A neighbouring page is being read into the window.
	pub history_extending: bool,
	pub history_loaded: bool,
	/// Scrolling to either end reads the next page; off after a failed read
	/// until the user scrolls again.
	pub history_autoload: bool,
	/// Per loaded commit: reachable from HEAD (tinted rows).
	pub log_on_head: Vec<bool>,
	/// The filter bar as edited; `log_search` is what the log shows.
	pub log_filter: LogQuery,
	pub log_path_input: Entity<TextInput>,
	/// The Branch chip's filter field, and its opened sections.
	pub log_branch_menu_input: Entity<TextInput>,
	pub log_branch_menu_open: Vec<String>,
	pub log_since_input: Entity<TextInput>,
	pub log_until_input: Entity<TextInput>,
	/// The Date chip's custom range was refused as malformed.
	pub log_date_error: bool,
	/// Folders opened in the Paths chip's picker.
	pub log_paths_expanded: Vec<String>,
	pub branch_filter_input: Entity<TextInput>,
	/// Collapsed groups of the branches pane ("refs_local", …).
	pub branch_groups_collapsed: Vec<String>,
	pub log_branches_visible: bool,
	pub log_details_visible: bool,
	pub log_show_hash: bool,
	pub log_details_w: f32,
	/// The log panel's width at the last layout.
	pub log_width: std::rc::Rc<std::cell::Cell<f32>>,
	pub log_filter_layout: ui::LogFilterLayout,
	pub log_fit: ui::LogFit,
	/// Collapsed directories of the changed-files tree.
	pub changed_dirs_collapsed: Vec<String>,
	/// The changed-files pane groups by directory (else a flat list).
	pub log_details_by_dir: bool,
	/// The changed-files rows, keyed on a hash of what builds them: the
	/// tree sorts up to `MAX_COMMIT_FILES` paths, too slow for every frame.
	pub commit_rows_cache: std::cell::RefCell<
		Option<(u64, std::rc::Rc<Vec<crate::ui::ChangeItemRow>>)>,
	>,
	/// Details pane height under the changed files once dragged; `None`
	/// keeps the default share (the files get most of the pane).
	pub log_details_h: Option<f32>,
	/// The multi-selection's commit list is open (collapsed by default).
	pub log_selection_expanded: bool,
	/// Commits whose details list every containing branch read.
	pub log_branches_all: Vec<String>,
	pub details_generation: u64,
	pub details_cancel: Option<CancelToken>,
	pub git_user_email: Option<String>,

	/// Everything shown for the Log selection; replaced whole whenever the
	/// selection changes (`reset_commit_view`), so no member outlives it.
	pub commit_view: crate::history::CommitView,
	pub compare: Option<(String, String)>,

	// Tool windows.
	pub files: Vec<FileChangeItem>,
	/// `files` holds the current repo's finished status listing.
	pub changes_loaded: bool,
	pub file_tree: Option<FileTreeNode>,
	/// The workspace folder's tree, rooted at its canonical path: every
	/// file and folder outside the placed repos. When the folder is itself
	/// a repo that is open, `file_tree` is this tree and this is None.
	pub ws_tree: Option<FileTreeNode>,
	/// The workspace folder's canonical path, once repos are known.
	pub ws_home: Option<PathBuf>,
	pub rev_tree: Option<RevTree>,
	pub active_tab: WorkbenchTab,
	pub selected_file: Option<String>,
	pub selected_file_source: Option<SourceKind>,
	pub selected_file_root: Option<PathBuf>,
	pub tree_cursor: usize,
	pub selected_list_row: usize,

	// Reader.
	pub preview: Option<Preview>,
	pub preview_loading: bool,
	/// The selection the shown preview belongs to, held while a file read
	/// that may replace it is in flight. It follows the shown content, not
	/// one request: a read that supersedes another keeps it, and it ends only
	/// when the preview is replaced, cleared or its reads are dropped.
	held_selection: Option<ShownSelection>,
	/// The latest preview read came from the Log (commit, compare,
	/// multi-selection, commit file or commit tree). Only a Project/Changes
	/// read or `clear_commit_selection` resets it, so it outlives
	/// `preview_loading` (#202).
	pub preview_pending_from_log: bool,
	pub preview_error: Option<Msg>,
	pub reader: Reader,
	pub find_input: Entity<TextInput>,
	pub goto_input: Entity<TextInput>,
	pub log_search_input: Entity<TextInput>,
	pub selector_input: Entity<TextInput>,
	/// Every floating and inline layer, and the rules they share.
	pub(crate) overlays: overlay::OverlayStack,

	/// Project rows stay selected across a tree rebuild (repo switch,
	/// Refresh): an outgoing tree leaves its selected paths here, by root.
	pub tree_selections: Vec<(PathBuf, Vec<String>)>,
	pub history_cancel: Option<CancelToken>,
	pub preview_cancel: Option<CancelToken>,
	pub tree_cancel: Option<CancelToken>,
	pub rev_tree_cancel: Option<CancelToken>,
	pub repo_cancel: Option<CancelToken>,
	pub scan_cancel: Option<CancelToken>,
	pub copy_cancel: Option<CancelToken>,
	pub paste_diff_cancel: Option<CancelToken>,
	pub paste_diff_generation: u64,
	pub discovery: Option<Discovery>,
	pub discovery_status: Option<ScanStatus>,
	pub discovery_errors: Vec<(PathBuf, Failure)>,
	pub discovery_depth_limited: Vec<PathBuf>,
	pub discovery_error_overflow: usize,
	pub discovery_depth_overflow: usize,
	discovery_generation: u64,
	pinned_repo: Option<(PathBuf, PathBuf)>,
	/// The open repo kept across a rescan's wipe until the walk finds it
	/// again; still set when a complete walk ends means it is gone.
	carried_repo: Option<(PathBuf, PathBuf)>,
	/// A user Refresh also reloads the open repo once the rescan ends.
	refresh_reload: bool,
	manual_repos: Vec<RepoEntry>,
	tree_queue: VecDeque<TreeIo>,
	tree_worker: u64,
	tree_worker_alive: bool,
	restore_expanded: Vec<String>,
	/// Workspace-tree folders to reopen once a Refresh rebuilds it.
	restore_ws_expanded: Vec<String>,
	add_cancel: Option<CancelToken>,
	pub add_repo_input: Entity<TextInput>,
	pub paste: paste::preview::PastePreview,
	pub status: Msg,
	pub user_action_seq: u64,
	pub status_error_action: Option<u64>,
	/// A finished copy's card over the window (id, succeeded, text): the
	/// status bar alone is easy to miss.
	pub toast: Option<(u64, bool, Msg)>,
	toast_seq: u64,
	pub is_loading: bool,
	pub is_copying: bool,
	pub locale: Locale,
	pub generation: u64,
	pub preview_generation: u64,
	pub history_generation: u64,
	/// Replace loads of the log that got past the discovery wait: a test
	/// counts how many one repo-set change costs.
	#[cfg(test)]
	pub(crate) history_replace_loads: u64,
	pub tree_generation: u64,
	pub mode: String,
	pub focus_handle: FocusHandle,
	pub reader_focus: FocusHandle,
	pub tree_focus: FocusHandle,
	/// Left tool window held focus at the last render (IntelliJ-style
	/// active vs inactive selection).
	pub left_active: bool,
	pub log_active: bool,
	pub reader_active: bool,
	pub log_focus: FocusHandle,
	pub paste_focus: FocusHandle,
	/// The open floating layer's panel (one is open at a time): a control
	/// pressed inside it holds the keyboard under this handle, so the
	/// layer still knows it as its own once it closes (`OverlayStack`).
	pub panel_focus: FocusHandle,
	/// The find bar's container, for the same reason and for Escape.
	pub find_bar_focus: FocusHandle,
	// Tool window layout; stored sizes survive collapse/restore.
	pub left_w: f32,
	pub bottom_h: f32,
	pub left_visible: bool,
	pub bottom_visible: bool,
	pub dragging: Option<Splitter>,
	pub last_viewport: (i32, i32),
	/// Logical window height, for popups that must fit inside it.
	pub viewport_h: f32,
	/// E2E control-bounds reporting; `None` unless `SNIP_NATIVE_E2E=1`.
	pub probes: Option<ui::Probes>,
	/// Focus requested from a context without a `Window`; applied on render.
	pub pending_focus: Option<FocusHandle>,
	/// The frame's focus guard, registered once: a focused element that is
	/// no longer drawn returns the keyboard to `focus_handle`.
	pub(crate) focus_lost_guard: Option<gpui::Subscription>,
	pub(crate) escape_guard: Option<gpui::Subscription>,
	pub e2e_read_delay: Option<std::time::Duration>,
	/// Test hook: holds a finished Log commit read before it installs.
	pub e2e_log_read_delay: Option<std::time::Duration>,
	/// Test-only hold file for project-tree reads, honoured only in E2E mode.
	pub e2e_tree_hold: Option<PathBuf>,
	/// Test-only hold file for copy export revalidation, honoured only in E2E mode.
	pub e2e_export_hold: Option<PathBuf>,
	/// Test-only: holds an in-flight remote open at the test clock, so a
	/// result can land after a later user action.
	#[cfg(test)]
	pub(crate) e2e_remote_open_delay: Option<std::time::Duration>,
	/// Test-only: holds a finished remote copy at the test clock before its
	/// result lands, so the language can change while it is in flight.
	#[cfg(test)]
	pub(crate) e2e_copy_hold: Option<std::time::Duration>,
	/// Test-only: the same hold for a paste row's diff read.
	#[cfg(test)]
	pub(crate) e2e_paste_diff_hold: Option<std::time::Duration>,
	pub workspace_open: bool,
	pub workspace_path_input: Entity<TextInput>,
	/// The paste panel's typed destination folder. Open or closed is its
	/// inline layer's (`OverlayKind::PasteDestField`); the text lives here.
	pub paste_dest_input: Entity<TextInput>,
	/// Remembered workspaces, newest first.
	pub recent_workspaces: Vec<PathBuf>,
	pub lifecycle: lifecycle::Lifecycle,
	pub watch_running: bool,
	pub last_life_log: String,
	/// The selected repo's Project row is collapsed (its tree stays loaded).
	pub repo_collapsed: bool,
	/// The workspace repo's row is folded while another repo is open.
	pub ws_collapsed: bool,
	/// Context menus, speed search and Changes group state.
	pub chrome: menu::Chrome,
	/// Changes tool window: one node per workspace repo, in name order.
	pub change_repos: Vec<ChangeRepo>,
	/// Status reads of the repos other than the open one.
	pub changes_queue: ReadQueue<PathBuf>,
	pub changes_cancel: Option<CancelToken>,
	pub changes_generation: u64,
	pub(crate) last_changes_empty: std::cell::Cell<Option<&'static str>>,
	pub(crate) last_log_empty: std::cell::Cell<Option<&'static str>>,
	/// Repo of the failed preview load when `preview_error` is Some.
	pub preview_error_root: Option<PathBuf>,
	/// Repositories the log's Repository chip picked; empty is all of them.
	pub log_repo_filter: Vec<PathBuf>,
	/// Repositories the loaded log covers (two or more: the merged log).
	pub log_scope_key: Vec<PathBuf>,
	/// The merged log's per-repository feeds.
	pub log_feeds: Vec<multi_log::Feed>,
	/// A Replace load waited for discovery to reach the picked repositories.
	pub log_deferred: bool,
	/// The `generation` of the repo read whose landing reloads the Log: the
	/// scope sync leaves the reload to it, so one change reads the Log once.
	log_reload_for: Option<u64>,
	/// Remote workspaces: ssh hosts and an open remote workspace.
	pub remote: remote::MasterState,
	pub remote_path_input: Entity<TextInput>,
	/// The workspace tab this model is; None for a model that is the
	/// window itself (tests).
	pub ws_tab: Option<u32>,
	/// Window height above the model: the tab bar.
	pub top_inset: f32,
	/// The control that had the keyboard when the tab was left.
	pub resume_focus: Option<FocusHandle>,
}

impl gpui::EventEmitter<tabs::TabEvent> for WorkbenchModel {}

/// Everything a file read takes from the model while it is in flight, so a
/// refusal by the retained budget can name the still shown preview again.
/// The commit view goes back whole: the file list and its origins index
/// `log_selected`, and a compare's list belongs to its endpoints.
struct ShownSelection {
	file: Option<String>,
	source: Option<SourceKind>,
	root: Option<PathBuf>,
	commit: Option<String>,
	view: crate::history::CommitView,
	log_selected: Vec<String>,
	range_head: Option<String>,
	compare: Option<(String, String)>,
	log_selection_expanded: bool,
	log_branches_all: Vec<String>,
	/// Set aside by a read of the commit view's own file list: only the
	/// file selection (`file`, `source`, `root`, the view's selected file and
	/// its multi-selection) is held, the rest of the commit state is still
	/// on the model. `pending_from_log` is the flag that read replaced.
	commit_file_only: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Splitter {
	Left,
	Bottom,
	LogDetails,
	/// Between the log's changed files and the commit details.
	LogFiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusPriority {
	Background = 0,
	Normal = 1,
	Error = 2,
}

pub fn is_error_status(key: &str) -> bool {
	key.starts_with("error_")
		|| key.starts_with("paste_err_")
		|| key.ends_with("_failed")
		|| key.ends_with("_refused")
		|| key.ends_with("_rejected")
		|| key.ends_with("_error")
		|| key.starts_with("reason_refused_")
		|| matches!(
			key,
			"tree_name_not_utf8"
				| "tree_read_error"
				| "status_goto_invalid"
				| "status_copy_empty"
				| "status_copy_nothing"
				| "status_copy_nothing_skipped"
				| "workspace_bad_path"
				| "workspace_restore_skipped"
				| "remote_unsupported"
				| "workspace_not_open"
				| "preview_memory_limit"
				| "commit_subset_rejected"
				| "commit_overwrite_required"
				| "mapping_required"
				| "status_fold_stale"
				| "status_fold_too_large"
				| "commit_replay_partial_refused"
				| "workspace_drain_leaked"
				| "workspace_drain_timeout"
		)
}

pub fn is_background_status(key: &str) -> bool {
	matches!(
		key,
		"status_scanning"
			| "status_repos_loaded"
			| "status_repo_loading"
			| "status_repo_loaded"
			| "status_history_loaded"
			| "status_log_merged_cap"
			| "status_graph_fallback"
			| "status_loading"
	)
}

pub fn status_priority(key: &str) -> StatusPriority {
	if is_error_status(key) {
		StatusPriority::Error
	} else if is_background_status(key) {
		StatusPriority::Background
	} else {
		StatusPriority::Normal
	}
}

impl WorkbenchModel {
	pub fn new(
		workspace: Option<PathBuf>,
		restore_dir: Option<PathBuf>,
		mode: String,
		cx: &mut Context<Self>,
	) -> Self {
		Self::build(workspace, restore_dir, mode, None, cx)
	}

	/// A workspace tab of [`tabs::TabsRoot`]; `probes` is the window's one
	/// frame, so a tab switch reports the controls that left.
	pub fn new_tab(
		workspace: Option<PathBuf>,
		restore_dir: Option<PathBuf>,
		mode: String,
		id: u32,
		probes: Option<ui::Probes>,
		cx: &mut Context<Self>,
	) -> Self {
		Self::build(workspace, restore_dir, mode, Some((id, probes)), cx)
	}

	fn build(
		workspace: Option<PathBuf>,
		restore_dir: Option<PathBuf>,
		mode: String,
		tab: Option<(u32, Option<ui::Probes>)>,
		cx: &mut Context<Self>,
	) -> Self {
		let ws_tab = tab.as_ref().map(|t| t.0);
		let top_inset = if tab.is_some() { tabs::TAB_BAR_H } else { 0. };
		let probes = match tab {
			Some((_, probes)) => probes,
			None => ui::Probes::from_env(),
		};
		let loc = Locale::ZhTw;
		let find_input = cx
			.new(|cx| TextInput::new(i18n::t("find_placeholder", loc), 30, cx));
		let goto_input = cx
			.new(|cx| TextInput::new(i18n::t("goto_placeholder", loc), 31, cx));
		let log_search_input = cx.new(|cx| {
			TextInput::new(i18n::t("log_search_placeholder", loc), 40, cx)
		});
		let selector_input = cx.new(|cx| {
			TextInput::new(i18n::t("selector_filter_placeholder", loc), 0, cx)
				.borderless()
		});
		let log_path_input = cx.new(|cx| {
			TextInput::new(i18n::t("log_paths_placeholder", loc), 0, cx)
		});
		cx.subscribe(&log_path_input, |this, input, ev: &InputEvent, cx| {
			match ev {
				InputEvent::Submit => {
					let p = input.read(cx).text().trim().to_string();
					this.add_log_path(p, cx);
				}
				// The picker filters by the text as it is typed.
				InputEvent::Changed => cx.notify(),
				_ => {}
			}
		})
		.detach();
		let log_branch_menu_input = cx.new(|cx| {
			TextInput::new(i18n::t("log_branch_placeholder", loc), 0, cx)
		});
		cx.subscribe(&log_branch_menu_input, |_, _, ev: &InputEvent, cx| {
			if matches!(ev, InputEvent::Changed) {
				cx.notify();
			}
		})
		.detach();
		let date_input = |key: &'static str, cx: &mut Context<Self>| {
			let input = cx.new(|cx| TextInput::new(i18n::t(key, loc), 0, cx));
			cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
				if matches!(ev, InputEvent::Submit) {
					this.apply_log_date_range(cx);
				}
			})
			.detach();
			input
		};
		let log_since_input = date_input("log_date_from", cx);
		let log_until_input = date_input("log_date_to", cx);
		let branch_filter_input = cx.new(|cx| {
			TextInput::new(i18n::t("log_branch_placeholder", loc), 39, cx)
		});
		cx.subscribe(&branch_filter_input, |_, _, ev: &InputEvent, cx| {
			if matches!(ev, InputEvent::Changed) {
				cx.notify();
			}
		})
		.detach();
		cx.subscribe(&find_input, |this, input, ev: &InputEvent, cx| {
			let q = input.read(cx).text().to_string();
			match ev {
				InputEvent::Changed => this.run_find(&q, cx),
				InputEvent::Submit | InputEvent::Down
					if this.reader.matches.is_empty() =>
				{
					this.run_find(&q, cx)
				}
				InputEvent::Submit | InputEvent::Down => {
					this.find_step(true, cx)
				}
				InputEvent::SubmitPrev | InputEvent::Up => {
					this.find_step(false, cx)
				}
			}
		})
		.detach();
		cx.subscribe(&goto_input, |this, input, ev: &InputEvent, cx| {
			if matches!(ev, InputEvent::Submit) {
				let q = input.read(cx).text().to_string();
				this.goto_line(&q, cx);
				this.pending_focus = Some(this.reader_focus.clone());
			}
		})
		.detach();
		cx.subscribe(&log_search_input, |this, input, ev: &InputEvent, cx| {
			match ev {
				InputEvent::Submit => {
					let q = input.read(cx).text().trim().to_string();
					this.start_log_search(q, cx);
				}
				InputEvent::Changed => {
					let q = input.read(cx).text().trim().to_string();
					if q.is_empty() && this.log_search.is_some() {
						this.start_log_search(String::new(), cx);
					}
				}
				_ => {}
			}
		})
		.detach();
		cx.subscribe(&selector_input, |this, _, ev: &InputEvent, cx| {
			this.selector_event(ev.clone(), cx)
		})
		.detach();

		let add_repo_input = cx.new(|cx| {
			TextInput::new(i18n::t("add_repo_placeholder", loc), 0, cx)
		});
		cx.subscribe(&add_repo_input, |this, input, ev: &InputEvent, cx| {
			if matches!(ev, InputEvent::Submit) {
				let text = input.read(cx).text().trim().to_string();
				if !text.is_empty() {
					this.add_repo_path(PathBuf::from(text), cx);
					input.update(cx, |i, cx| i.set_text("", cx));
					this.close_overlay(
						overlay::OverlayKind::AddRepoField,
						overlay::CloseWhy::Picked,
						cx,
					);
				}
			}
		})
		.detach();

		let workspace_path_input = cx.new(|cx| {
			TextInput::new(i18n::t("workspace_path_placeholder", loc), 70, cx)
		});
		cx.subscribe(
			&workspace_path_input,
			|this, input, ev: &InputEvent, cx| {
				if matches!(ev, InputEvent::Submit) {
					let text = input.read(cx).text().trim().to_string();
					this.confirm_open_workspace(&text, cx);
				}
			},
		)
		.detach();
		let paste_dest_input = cx.new(|cx| {
			TextInput::new(i18n::t("paste_dest_placeholder", loc), 58, cx)
		});
		cx.subscribe(
			&paste_dest_input,
			|this, _, ev: &InputEvent, cx| match ev {
				InputEvent::Submit => this.submit_paste_dest(cx),
				InputEvent::Changed => this.paste_dest_changed(cx),
				_ => {}
			},
		)
		.detach();
		let remote_path_input = cx.new(|cx| {
			TextInput::new(i18n::t("remote_path_placeholder", loc), 71, cx)
		});
		cx.subscribe(&remote_path_input, |this, _, ev: &InputEvent, cx| {
			if matches!(ev, InputEvent::Submit) {
				this.open_remote_typed(cx);
			}
		})
		.detach();
		cx.on_release(|this, _| {
			this.lifecycle.cancel_cancellable();
			if e2e_on() {
				// A closed tab is never the shown one again.
				tabs::with_log_tab(this.ws_tab, || {
					app_log!(
						"[APP:LIFECYCLE: phase=released reason=on_release jobs={}]",
						this.lifecycle.unfinished()
					)
				});
			}
		})
		.detach();

		let mut model = Self {
			workspace_root: workspace.clone().unwrap_or_default(),
			restore_dir,
			repos: RepoSet::new(),
			selected_repo_idx: None,
			commits: Vec::new(),
			refs: Vec::new(),
			head_sha: None,
			graph_layout: None,
			active_ref_filter: None,
			commit_page: 0,
			history_has_more: false,
			history_page_size: 50,
			history_error: None,
			page_checkpoints: vec![None],
			log_search: None,
			collapsed_merges: Vec::new(),
			hidden_commits: Vec::new(),
			history_walk: None,
			selected_commit: None,
			range_head: None,
			log_selected: Vec::new(),
			log_scroll: gpui::UniformListScrollHandle::new(),
			select_head_after_load: false,
			log_first_page: 0,
			history_extending: false,
			history_loaded: false,
			history_autoload: true,
			log_on_head: Vec::new(),
			log_filter: LogQuery::default(),
			log_path_input,
			log_branch_menu_input,
			log_branch_menu_open: Vec::new(),
			log_since_input,
			log_until_input,
			log_date_error: false,
			log_paths_expanded: Vec::new(),
			branch_filter_input,
			branch_groups_collapsed: Vec::new(),
			log_branches_visible: true,
			log_details_visible: true,
			log_show_hash: false,
			log_details_w: theme::LOG_DETAILS_W_DEFAULT,
			log_width: Default::default(),
			log_filter_layout: Default::default(),
			log_fit: Default::default(),
			changed_dirs_collapsed: Vec::new(),
			log_details_by_dir: true,
			commit_rows_cache: Default::default(),
			log_details_h: None,
			log_selection_expanded: false,
			log_branches_all: Vec::new(),
			details_generation: 0,
			details_cancel: None,
			git_user_email: None,
			commit_view: Default::default(),
			compare: None,
			files: Vec::new(),
			changes_loaded: false,
			file_tree: None,
			ws_tree: None,
			ws_home: None,
			rev_tree: None,
			active_tab: WorkbenchTab::GitChanges,
			selected_file: None,
			selected_file_source: None,
			selected_file_root: None,
			tree_cursor: 0,
			selected_list_row: 0,
			preview: None,
			preview_loading: false,
			held_selection: None,
			preview_pending_from_log: false,
			preview_error: None,
			reader: Reader::default(),
			find_input,
			goto_input,
			log_search_input,
			selector_input,
			add_repo_input,
			overlays: overlay::OverlayStack::new(),
			tree_selections: Vec::new(),
			history_cancel: None,
			preview_cancel: None,
			tree_cancel: None,
			rev_tree_cancel: None,
			repo_cancel: None,
			scan_cancel: None,
			copy_cancel: None,
			discovery: None,
			discovery_status: None,
			discovery_errors: Vec::new(),
			discovery_depth_limited: Vec::new(),
			discovery_error_overflow: 0,
			discovery_depth_overflow: 0,
			discovery_generation: 0,
			pinned_repo: None,
			carried_repo: None,
			refresh_reload: false,
			manual_repos: Vec::new(),
			tree_queue: VecDeque::new(),
			tree_worker: 0,
			tree_worker_alive: false,
			restore_expanded: Vec::new(),
			restore_ws_expanded: Vec::new(),
			add_cancel: None,
			paste: paste::preview::PastePreview::new(ui::e2e_apply_delay()),
			paste_diff_cancel: None,
			paste_diff_generation: 0,
			status: if workspace.is_some() {
				Msg::new("status_scanning", [])
			} else {
				Msg::new("workspace_closed", [])
			},
			user_action_seq: 0,
			status_error_action: None,
			toast: None,
			toast_seq: 0,
			is_loading: workspace.is_some(),
			is_copying: false,
			locale: loc,
			generation: 0,
			preview_generation: 0,
			history_generation: 0,
			#[cfg(test)]
			history_replace_loads: 0,
			tree_generation: 0,
			mode,
			focus_handle: cx.focus_handle(),
			reader_focus: cx.focus_handle().tab_index(34).tab_stop(true),
			tree_focus: cx.focus_handle().tab_index(20).tab_stop(true),
			left_active: false,
			log_active: false,
			reader_active: false,
			log_focus: cx.focus_handle().tab_index(46).tab_stop(true),
			paste_focus: cx.focus_handle(),
			panel_focus: cx.focus_handle(),
			find_bar_focus: cx.focus_handle(),
			left_w: theme::LEFT_W_DEFAULT,
			bottom_h: theme::BOTTOM_H_DEFAULT,
			left_visible: true,
			bottom_visible: true,
			dragging: None,
			last_viewport: (0, 0),
			viewport_h: 0.,
			probes,
			pending_focus: None,
			focus_lost_guard: None,
			escape_guard: None,
			e2e_read_delay: ui::e2e_read_delay(),
			e2e_log_read_delay: None,
			e2e_tree_hold: ui::e2e_tree_hold(),
			e2e_export_hold: ui::e2e_export_hold(),
			#[cfg(test)]
			e2e_remote_open_delay: None,
			#[cfg(test)]
			e2e_copy_hold: None,
			#[cfg(test)]
			e2e_paste_diff_hold: None,
			workspace_open: workspace.is_some(),
			workspace_path_input,
			paste_dest_input,
			recent_workspaces: recent::load(),
			lifecycle: lifecycle::Lifecycle::new(1),
			watch_running: false,
			last_life_log: String::new(),
			chrome: menu::Chrome::new(cx),
			repo_collapsed: false,
			ws_collapsed: false,
			change_repos: Vec::new(),
			changes_queue: ReadQueue::new(MAX_CHANGES_READS),
			changes_cancel: None,
			changes_generation: 0,
			last_changes_empty: std::cell::Cell::new(None),
			last_log_empty: std::cell::Cell::new(None),
			preview_error_root: None,
			log_repo_filter: Vec::new(),
			log_scope_key: Vec::new(),
			log_feeds: Vec::new(),
			log_deferred: false,
			log_reload_for: None,
			remote: remote::MasterState {
				hosts: remote::load_hosts(),
				recent: remote::load_recent(),
				..Default::default()
			},
			remote_path_input,
			ws_tab,
			top_inset,
			resume_focus: None,
		};
		if let Some(path) = workspace {
			recent::remember(&mut model.recent_workspaces, &path);
			model.reload_repos(cx);
		}
		model
	}

	/// The repo the shown text was read from: the preview carries it, so no
	/// selection or commit view that moves on while a read is in flight
	/// changes it. A failed read names the repo it was read from; with
	/// nothing from a repo shown, the open repo.
	pub fn preview_root(&self) -> Option<PathBuf> {
		match (&self.preview, &self.preview_error_root) {
			(Some(p), _) => p.root.clone().or_else(|| self.repo_root()),
			(None, Some(root)) if self.preview_error.is_some() => {
				Some(root.clone())
			}
			_ => self.repo_root(),
		}
	}

	/// The repo a Changes row's preview was actually read from, loaded or
	/// failed; `None` while it is still loading or nothing is shown.
	fn shown_change_repo(&self) -> Option<PathBuf> {
		if self.preview_loading {
			return None;
		}
		match (&self.preview, &self.preview_error) {
			(Some(p), _) => p.root.clone(),
			(None, Some(_)) => self.preview_error_root.clone(),
			(None, None) => None,
		}
	}

	/// Returns a human-friendly display path for a repository root.
	/// In a remote session, maps paths under session root to worker_name:workspace_path[/rel].
	pub fn display_repo_path(&self, root: &std::path::Path) -> String {
		crate::remote::display_path(self.remote.session.as_ref(), root)
	}

	pub fn change_repo_tooltip(&self, root: &std::path::Path) -> String {
		self.display_repo_path(root)
	}

	pub fn project_repo_tooltip(&self, repo: &RepoEntry) -> String {
		let path = self.display_repo_path(&repo.root);
		match &repo.summary {
			Ok(_) => format!(
				"{}\n{}",
				path,
				crate::i18n::t("counts_tip", self.locale)
			),
			Err(e) => format!(
				"{}\n{}",
				path,
				crate::reason::failure_msg(e).render(self.locale)
			),
		}
	}

	/// The open repo's Changes slot.
	pub fn selected_change_slot(&self) -> Option<usize> {
		let root = self.repo_root()?;
		self.change_repos.iter().position(|s| s.root == root)
	}

	pub(crate) fn discovery_error_msg(&self) -> Msg {
		if let Some((_, err)) = self.discovery_errors.first() {
			Msg::with_msg_arg(
				"error_repo_status",
				[String::new()],
				0,
				crate::reason::failure_msg(err),
			)
		} else {
			let key = match self.discovery_status {
				Some(ScanStatus::Incomplete) => "discovery_incomplete",
				Some(ScanStatus::LimitReached) => "discovery_limit_reached",
				Some(ScanStatus::Cancelled) => "discovery_cancelled",
				Some(ScanStatus::TimedOut) => "discovery_timed_out",
				Some(_) => "discovery_failed",
				None => "discovery_not_run",
			};
			Msg::with_key_arg("error_repo_status", [key.to_string()], 0)
		}
	}

	pub(crate) fn changes_title_text(&self) -> String {
		let loc = self.locale;
		if self
			.change_repos
			.iter()
			.any(|s| matches!(s.state, ChangeRepoState::Loading))
		{
			crate::i18n::tf("changes_title", loc, &[&"…"])
		} else {
			crate::i18n::tf("changes_title", loc, &[&self.files.len()])
		}
	}

	/// Repos of a merged log that failed while others loaded.
	pub(crate) fn failed_feed_names(&self) -> Vec<String> {
		if self.log_feeds.len() <= 1 {
			return Vec::new();
		}
		let failed: Vec<String> = self
			.log_feeds
			.iter()
			.filter(|f| f.failed)
			.map(|f| f.name.clone())
			.collect();
		if failed.len() < self.log_feeds.len() {
			failed
		} else {
			Vec::new()
		}
	}

	/// The log's banner over those repos: the count and the first
	/// [`FAILED_FEEDS_NAMED`] names; the banner's tooltip lists them all.
	pub(crate) fn failed_feeds_msg(&self) -> Option<Msg> {
		let failed = self.failed_feed_names();
		if failed.is_empty() {
			return None;
		}
		let mut named =
			failed[..failed.len().min(FAILED_FEEDS_NAMED)].join(", ");
		if failed.len() > FAILED_FEEDS_NAMED {
			named.push_str(", …");
		}
		Some(Msg::new(
			"log_failed_feeds",
			[failed.len().to_string(), named],
		))
	}

	pub(crate) fn log_empty_state(&self) -> Option<LogEmpty> {
		let res = (|| {
			if !self.display_commits().is_empty() {
				return None;
			}
			if !self.workspace_open {
				if self.is_loading {
					return Some(LogEmpty::Loading);
				}
				return Some(LogEmpty::NoWorkspace);
			}
			if self.remote.session.is_some() {
				if let Some(msg) = &self.remote.scan_error {
					return Some(LogEmpty::Failed(msg.clone()));
				}
			}
			if self.is_loading
				|| self.discovery_status.is_none()
				|| self.discovery_status
					== Some(snip_core::workspace::ScanStatus::More)
			{
				return Some(LogEmpty::Scanning);
			}
			if self.repos.is_empty() {
				if self.discovery_status
					== Some(snip_core::workspace::ScanStatus::Complete)
					&& self.discovery_errors.is_empty()
				{
					return Some(LogEmpty::NoRepository);
				} else {
					return Some(LogEmpty::Failed(self.discovery_error_msg()));
				}
			}
			if let Some(err) = &self.history_error {
				return Some(LogEmpty::Failed(Msg::with_msg_arg(
					"error_history",
					[String::new()],
					0,
					err.clone(),
				)));
			}
			if !self.log_feeds.is_empty()
				&& self.log_feeds.iter().all(|f| f.failed)
			{
				let failed_names: Vec<&str> =
					self.log_feeds.iter().map(|f| f.name.as_str()).collect();
				return Some(LogEmpty::Failed(Msg::new(
					"log_failed_feeds",
					[self.log_feeds.len().to_string(), failed_names.join(", ")],
				)));
			}
			if !self.history_loaded {
				return Some(LogEmpty::Loading);
			}
			Some(LogEmpty::Empty)
		})();

		let state_str = match &res {
			None => None,
			Some(LogEmpty::NoWorkspace) => Some("no_workspace"),
			Some(LogEmpty::Scanning) => Some("scanning"),
			Some(LogEmpty::Loading) => Some("loading"),
			Some(LogEmpty::NoRepository) => Some("no_repository"),
			Some(LogEmpty::Failed(_)) => Some("failed"),
			Some(LogEmpty::Empty) => Some("empty"),
		};
		if state_str != self.last_log_empty.get() {
			self.last_log_empty.set(state_str);
			if let Some(s) = state_str {
				app_log!("[APP:LOG_EMPTY: state={s}]");
			}
		}
		res
	}

	/// The slot of `root`, made if it is new. Its name is the repo's name
	/// in the list now: `fallback` is the one a read took when it began,
	/// which a rename since has made stale.
	fn ensure_change_slot(
		&mut self,
		root: &std::path::Path,
		fallback: &str,
	) -> usize {
		let name = self
			.repos
			.iter()
			.find(|r| r.root == root)
			.map_or(fallback, |r| r.name.as_str());
		let pos =
			slot_insert(&mut self.change_repos, &mut self.files, root, name);
		if slots_sorted(&self.change_repos) {
			return pos;
		}
		slots_resort(&mut self.change_repos, &mut self.files);
		self.change_repos
			.iter()
			.position(|s| s.root == root)
			.unwrap_or(pos)
	}

	/// Installs one repo's status read into its slot: rows past
	/// `MAX_CHANGES_PER_REPO` are dropped (the node says so), and a failure
	/// leaves an error row instead of rows.
	fn install_slot_changes(
		&mut self,
		slot: usize,
		result: Result<(Vec<WorkingChangeTuple>, usize), Msg>,
	) {
		let (changes, total) = match result {
			Ok(pair) => pair,
			Err(err) => {
				slot_replace_rows(&mut self.files, slot, Vec::new());
				let s = &mut self.change_repos[slot];
				s.state = ChangeRepoState::Failed(err);
				s.total = 0;
				return;
			}
		};
		let rows: Vec<FileChangeItem> = changes
			.into_iter()
			.take(MAX_CHANGES_PER_REPO)
			.map(|(path, change_type, source, is_conflict)| FileChangeItem {
				path,
				change_type,
				source,
				is_conflict,
				repo: slot as u32,
			})
			.collect();
		slot_replace_rows(&mut self.files, slot, rows);
		let s = &mut self.change_repos[slot];
		s.state = ChangeRepoState::Loaded;
		s.total = total;
	}

	/// Mirrors the repo list into the Changes slots: vanished repos lose
	/// their node, new ones get a loading node.
	fn sync_change_slots(&mut self) {
		let mut i = 0;
		while i < self.change_repos.len() {
			let root = &self.change_repos[i].root;
			if self.repos.iter().any(|r| &r.root == root) {
				i += 1;
			} else {
				slot_remove(&mut self.change_repos, &mut self.files, i);
			}
		}
		for r in 0..self.repos.len() {
			let (root, name) =
				(self.repos[r].root.clone(), self.repos[r].name.clone());
			slot_insert(&mut self.change_repos, &mut self.files, &root, &name);
		}
		// A rename renames the node in place: the order follows the names.
		if !slots_sorted(&self.change_repos) {
			slots_resort(&mut self.change_repos, &mut self.files);
		}
	}

	/// Re-reads every repo's Changes except the open one, whose rows its
	/// own load owns. `only_unloaded` keeps rows that already landed.
	fn start_changes_queue(
		&mut self,
		only_unloaded: bool,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		self.changes_generation = self.changes_generation.wrapping_add(1);
		let _ = arm_cancel(&mut self.changes_cancel);
		self.changes_queue.clear_pending();
		let open = self.repo_root();
		for slot in &self.change_repos {
			if Some(&slot.root) == open.as_ref()
				|| (only_unloaded && slot.state == ChangeRepoState::Loaded)
			{
				continue;
			}
			self.changes_queue.push(slot.root.clone());
		}
		self.pump_changes(cx);
	}

	/// Starts queued status reads while fewer than `MAX_CHANGES_READS` run.
	fn pump_changes(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			self.changes_queue.clear_pending();
			return;
		}
		let Some(cancel) = self.changes_cancel.clone() else {
			return;
		};
		while let Some(root) = self.changes_queue.take() {
			let generation = self.changes_generation;
			let known = self
				.repos
				.iter()
				.find(|r| r.root == root)
				.and_then(|r| r.identity.clone());
			let host = self.git_host();
			let mut async_app = cx.to_async();
			let this = cx.weak_entity();
			let bg = cx.background_executor().clone();
			let cancel_bg = cancel.clone();
			self.spawn_owned(cx, Some(cancel.clone()), async move {
				let read_root = root.clone();
				let result = bg
					.spawn(async move {
						read_change_list(
							&host,
							&read_root,
							known.as_ref(),
							cancel_bg,
						)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					model.changes_queue.finish();
					if model.changes_generation == generation {
						model.land_queued_changes(&root, result);
					}
					model.pump_changes(cx);
					cx.notify();
				});
			});
		}
	}

	fn land_queued_changes(
		&mut self,
		root: &std::path::Path,
		result: Result<
			(Option<RepoSummary>, Vec<WorkingChangeTuple>, usize),
			Msg,
		>,
	) {
		let Some(slot) = self.change_repos.iter().position(|s| s.root == root)
		else {
			return;
		};
		// The open repo's own load owns its rows.
		if self.repo_root().as_deref() == Some(root) {
			return;
		}
		let name = self.change_repos[slot].name.clone();
		let result = result.map(|(summary, changes, total)| {
			if let Some(summary) = summary {
				self.repos.set_summary(root, summary);
			}
			(changes, total)
		});
		match &result {
			Ok((changes, _total)) => {
				app_log!(
					"[APP:CHANGES_LOADED: {} files={}]",
					name,
					changes.len()
				)
			}
			Err(_) => app_log!("[APP:CHANGES_ERROR: {}]", name),
		}
		self.install_slot_changes(slot, result);
		self.sync_list_row();
	}

	/// A tree row whose name is not UTF-8 cannot be addressed: say so, so
	/// the click is not silently ignored while the old preview stays.
	pub fn refuse_unaddressable_row(&mut self, cx: &mut Context<Self>) {
		app_log!("[APP:TREE_ROW_REFUSED: not-utf8]");
		self.set_status("tree_name_not_utf8", []);
		cx.notify();
	}

	pub fn user_action(&mut self) {
		self.user_action_seq = self.user_action_seq.wrapping_add(1);
	}

	/// Shows `msg` in the status bar. An error stays until the user's next
	/// action: a background status (`is_background_status`) arriving before
	/// then is dropped. Whether a key is an error comes only from its name
	/// (`is_error_status`), so a new error key must match one of its shapes
	/// or be added to its list, or scan progress will overwrite it.
	pub fn set_status_msg(&mut self, msg: Msg) {
		let incoming_prio = status_priority(msg.key);
		if incoming_prio == StatusPriority::Background {
			if let Some(err_seq) = self.status_error_action {
				if err_seq == self.user_action_seq {
					return;
				}
			}
		}
		if incoming_prio == StatusPriority::Error {
			self.status_error_action = Some(self.user_action_seq);
		} else {
			self.status_error_action = None;
		}
		self.status = msg;
	}

	pub fn set_status(
		&mut self,
		key: &'static str,
		args: impl crate::i18n::IntoMsgArgs,
	) {
		self.set_status_msg(Msg::new(key, args));
	}

	/// Shows `msg` in a card over the window for a few seconds, longer for a
	/// failure, which may list paths to read (a later toast replaces it and
	/// restarts the clock).
	pub fn show_toast(&mut self, ok: bool, msg: Msg, cx: &mut Context<Self>) {
		self.toast_seq = self.toast_seq.wrapping_add(1);
		let id = self.toast_seq;
		app_log!("[APP:TOAST: ok={ok}]");
		self.toast = Some((id, ok, msg));
		let tab = self.ws_tab;
		cx.spawn(async move |this, cx| {
			tabs::tagged(tab, async move {
				cx.background_executor()
					.timer(std::time::Duration::from_secs(if ok {
						4
					} else {
						12
					}))
					.await;
				let _ = this.update(cx, |model, cx| {
					if model.toast.as_ref().is_some_and(|t| t.0 == id) {
						model.toast = None;
						cx.notify();
					}
				});
			})
			.await
		})
		.detach();
	}

	/// The open repo. An index that no longer names the pinned identity (the
	/// list changed under it) is not trusted, so nothing acts on another repo.
	pub fn repo(&self) -> Option<&RepoEntry> {
		let entry = self.repos.get(self.selected_repo_idx?)?;
		match &self.pinned_repo {
			Some(key) if !repo_key_matches(entry, key) => None,
			_ => Some(entry),
		}
	}

	pub fn repo_root(&self) -> Option<PathBuf> {
		self.repo().map(|r| r.root.clone())
	}

	pub fn current_restore_destination(&self) -> PathBuf {
		// A remote workspace pastes into its own folders only.
		let remote_root = self.remote.session.as_ref().map(|s| &s.root);
		let restore_dir = self
			.restore_dir
			.as_ref()
			.filter(|d| remote_root.is_none_or(|root| d.starts_with(root)));
		if let Some(d) = restore_dir {
			d.clone()
		} else if let Some(root) = self.repo_root() {
			root
		} else {
			self.workspace_root.clone()
		}
	}

	pub fn set_preview(&mut self, p: Preview) -> bool {
		if let Err(err) = self.paste.admit_ordinary(Some(&p)) {
			self.preview_loading = false;
			self.preview_error = None;
			self.preview_error_root = None;
			self.set_status_msg(err);
			app_log!("[APP:PREVIEW_REFUSED: reason=retained_budget]");
			return false;
		}
		if e2e_on() {
			let (kind, rev) = match &p.source {
				PreviewSource::WorkingFile => ("working_file", "-".to_string()),
				PreviewSource::WorkingChanges => {
					("working_changes", "-".into())
				}
				PreviewSource::StagedChanges => ("staged_changes", "-".into()),
				PreviewSource::UnstagedChanges => {
					("unstaged_changes", "-".into())
				}
				PreviewSource::CommitDiff { sha } => {
					("commit_diff", sha.clone())
				}
				PreviewSource::CommitFile { sha } => {
					("commit_file", sha.clone())
				}
				PreviewSource::Compare { from, to } => {
					("compare", format!("{from}..{to}"))
				}
				PreviewSource::PasteItem => ("paste_item", "-".into()),
			};
			app_log!(
				"[APP:E2E_PREVIEW: source={} rev={} path={} lines={} fnv={:x}]",
				kind,
				rev,
				p.path.as_deref().unwrap_or("-"),
				p.lines.len(),
				p.fingerprint()
			);
		}
		self.held_selection = None;
		self.preview_error_root = None;
		self.preview = Some(p);
		self.preview_loading = false;
		self.preview_error = None;
		self.reader.reset_for_new_preview();
		// Re-run an active find against the new text.
		self.refind();
		true
	}

	pub(crate) fn clear_preview(&mut self) {
		self.held_selection = None;
		self.preview = None;
		self.preview_error_root = None;
		self.paste.release_ordinary();
	}

	/// Replace a failed read with its error body, releasing the hidden source.
	/// Capacity refusal instead keeps the previous preview in `set_preview`.
	pub(crate) fn show_preview_error(&mut self, error: Msg) {
		self.clear_preview();
		self.reader.release_retained();
		self.preview_loading = false;
		self.preview_error = Some(error);
	}

	pub(crate) fn can_copy_preview(&self) -> bool {
		self.preview.is_some() && self.preview_error.is_none()
	}

	/// A slot whose status read has landed; a loading one's rows are inert.
	pub fn change_slot_loaded(&self, slot: u32) -> bool {
		self.change_repos
			.get(slot as usize)
			.is_some_and(|s| s.state == ChangeRepoState::Loaded)
	}

	pub fn toggle_locale(&mut self, cx: &mut Context<Self>) {
		self.locale = match self.locale {
			Locale::ZhTw => Locale::En,
			Locale::En => Locale::ZhTw,
		};
		let loc = self.locale;
		for (input, key) in [
			(self.find_input.clone(), "find_placeholder"),
			(self.goto_input.clone(), "goto_placeholder"),
			(self.log_search_input.clone(), "log_search_placeholder"),
			(self.selector_input.clone(), "selector_filter_placeholder"),
			(self.log_path_input.clone(), "log_paths_placeholder"),
			(self.log_branch_menu_input.clone(), "log_branch_placeholder"),
			(self.log_since_input.clone(), "log_date_from"),
			(self.log_until_input.clone(), "log_date_to"),
			(self.branch_filter_input.clone(), "log_branch_placeholder"),
			(self.paste_dest_input.clone(), "paste_dest_placeholder"),
		] {
			input.update(cx, |i, _| i.set_placeholder(i18n::t(key, loc)));
		}
		app_log!("[APP:LOCALE: {:?}]", self.locale);
		cx.notify();
	}

	pub fn process_discovery_repos(
		discovered: Vec<DiscoveredRepo>,
		opts: &RunOptions,
	) -> (Vec<RepoEntry>, Vec<(PathBuf, Failure)>) {
		let (found, errors) = snip_core::gitview::identify_repos(
			discovered,
			None,
			&snip_core::workspace::ScanBudget::visits(usize::MAX),
			opts,
		);
		(found.into_iter().map(RepoEntry::from).collect(), errors)
	}

	pub fn disambiguate_repo_names(
		repos: &mut [RepoEntry],
		workspace_root: &std::path::Path,
	) {
		let mut counts: HashMap<String, usize> = HashMap::new();
		for r in repos.iter() {
			*counts.entry(r.name.clone()).or_insert(0) += 1;
		}
		for r in repos.iter_mut() {
			if counts.get(&r.name).copied().unwrap_or(0) > 1 {
				if let Ok(rel) = r.root.strip_prefix(workspace_root) {
					r.name = rel.display().to_string();
				} else if let Some(parent) =
					r.root.parent().and_then(|p| p.file_name())
				{
					r.name = format!("{}/{}", parent.to_string_lossy(), r.name);
				}
			}
		}

		// 第二輪：workspace root 的寫法不是 git 解析後的 toplevel（例如 symlink），或 repo 在 workspace 外，而且上一層資料夾同名。
		fn root_tail(root: &std::path::Path, k: usize) -> String {
			let normals: Vec<String> = root
				.components()
				.filter_map(|c| match c {
					std::path::Component::Normal(p) => {
						Some(p.to_string_lossy().into_owned())
					}
					_ => None,
				})
				.collect();
			if k >= normals.len() {
				root.display().to_string()
			} else {
				normals[normals.len() - k..].join("/")
			}
		}

		for k in 3.. {
			let mut round_counts: HashMap<String, usize> = HashMap::new();
			for r in repos.iter() {
				*round_counts.entry(r.name.clone()).or_insert(0) += 1;
			}
			let dup_indices: Vec<usize> = repos
				.iter()
				.enumerate()
				.filter(|(_, r)| {
					round_counts.get(&r.name).copied().unwrap_or(0) > 1
				})
				.map(|(i, _)| i)
				.collect();
			if dup_indices.is_empty() {
				break;
			}
			let max_normals = dup_indices
				.iter()
				.map(|&i| {
					repos[i]
						.root
						.components()
						.filter(|c| {
							matches!(c, std::path::Component::Normal(_))
						})
						.count()
				})
				.max()
				.unwrap_or(0);
			if max_normals < k {
				break;
			}
			for &i in &dup_indices {
				repos[i].name = root_tail(&repos[i].root, k);
			}
		}
	}

	pub(crate) fn accepting_work(&self) -> bool {
		self.workspace_open && !self.lifecycle.is_draining()
	}

	fn needs_watch(&mut self) -> bool {
		self.lifecycle.unfinished() > 0
			|| self.lifecycle.is_draining()
			|| self.paste.has_background_work()
	}

	/// Reaps owned jobs only while one is live or a drain is in progress.
	fn arm_watch(&mut self, cx: &mut Context<Self>) {
		if self.watch_running || !self.needs_watch() {
			return;
		}
		self.watch_running = true;
		let tab = self.ws_tab;
		cx.spawn(async move |this, cx| {
			tabs::tagged(tab, async move {
				loop {
					cx.background_executor()
						.timer(std::time::Duration::from_millis(40))
						.await;
					let keep = this.update(cx, |model, cx| {
						model.poll_paste(cx);
						model.poll_lifecycle(cx);
						if model.needs_watch() {
							return true;
						}
						model.watch_running = false;
						if model.needs_watch() {
							model.arm_watch(cx);
						}
						false
					});
					match keep {
						Ok(true) => continue,
						Ok(false) | Err(_) => break,
					}
				}
			})
			.await
		})
		.detach();
	}

	/// Runs `fut` as this tab's read. Pass the token the work polls: the
	/// lifecycle cancels it on a close or a quit. A job whose work polls no
	/// token passes `None` and is drained instead. The kind follows from
	/// the token, so a job cannot be recorded as cancellable without one;
	/// what the types cannot catch is a future that polls a token the
	/// caller never passed here.
	pub(crate) fn spawn_owned(
		&mut self,
		cx: &mut Context<Self>,
		cancel: Option<CancelToken>,
		fut: impl std::future::Future<Output = ()> + 'static,
	) -> u64 {
		let registered = self.lifecycle.register_read(cancel);
		self.spawn_registered(cx, registered, fut)
	}

	/// Runs a confirmed paste or replay: never cancelled, and a close or
	/// quit is refused while it runs.
	pub(crate) fn spawn_mutating(
		&mut self,
		cx: &mut Context<Self>,
		fut: impl std::future::Future<Output = ()> + 'static,
	) -> u64 {
		let registered = self.lifecycle.register_mutating();
		self.spawn_registered(cx, registered, fut)
	}

	fn spawn_registered(
		&mut self,
		cx: &mut Context<Self>,
		(id, flag): (u64, lifecycle::FinishFlag),
		fut: impl std::future::Future<Output = ()> + 'static,
	) -> u64 {
		let fut = tabs::tagged(self.ws_tab, fut);
		let task = cx.foreground_executor().spawn(async move {
			fut.await;
			drop(flag);
		});
		self.lifecycle.attach(id, task);
		self.arm_watch(cx);
		id
	}

	fn emit_life(&mut self, phase: &str, intent: &str, reason: Option<&str>) {
		let git = lifecycle::GitLoad::current();
		let jobs = self.lifecycle.unfinished();
		let line = lifecycle::format_line(
			phase,
			intent,
			reason,
			jobs,
			git,
			self.lifecycle.generation(),
		);
		if self.last_life_log == line {
			return;
		}
		self.last_life_log = line.clone();
		if e2e_on() {
			app_log!("[APP:LIFECYCLE: {line}]");
		}
	}

	/// Ctrl/Cmd-Q and the OS close button. Does not call `cx.quit`. A tab
	/// hands it to the root, which drains every tab.
	pub fn begin_quit(&mut self, cx: &mut Context<Self>) {
		if self.ws_tab.is_some() {
			cx.emit(tabs::TabEvent::QuitRequested);
			return;
		}
		if e2e_on() {
			app_log!("[APP:QUIT: deferred]");
		}
		self.request_user_close(lifecycle::Intent::Quit, cx);
	}

	/// Shared close for Quit, the OS window button, and workspace switching.
	/// Does not call `cx.quit`; that happens only after a clear drain.
	pub fn request_user_close(
		&mut self,
		intent: lifecycle::Intent,
		cx: &mut Context<Self>,
	) {
		if self.paste_busy() || self.lifecycle.has_mutating() {
			let name = intent.name();
			app_log!("[APP:PASTE_BUSY: refused={name}]");
			self.emit_life("refused", name, Some("applying"));
			self.set_status("workspace_busy_applying", []);
			cx.notify();
			return;
		}
		if !self.workspace_open
			&& !self.lifecycle.is_draining()
			&& self.lifecycle.unfinished() == 0
			&& matches!(intent, lifecycle::Intent::CloseWorkspace)
		{
			// An empty tab with no job has nothing to drain.
			if self.ws_tab.is_some() {
				self.paste.invalidate_job();
				cx.emit(tabs::TabEvent::Closed);
			}
			return;
		}
		match self
			.lifecycle
			.request(intent.clone(), std::time::Instant::now())
		{
			lifecycle::Request::RefusedApplying => {
				let name = intent.name();
				app_log!("[APP:PASTE_BUSY: refused={name}]");
				self.emit_life("refused", name, Some("applying"));
				self.set_status("workspace_busy_applying", []);
			}
			lifecycle::Request::Busy => {
				let intent = self.lifecycle.intent_name();
				self.emit_life("draining", intent, None);
				self.set_status("workspace_draining", []);
			}
			lifecycle::Request::Accepted => {
				// A workspace open/close is under way: an in-flight remote
				// open that lands later must not touch it (remote.rs).
				self.remote.open_seq = self.remote.open_seq.wrapping_add(1);
				self.generation = self.generation.wrapping_add(1);
				self.preview_generation =
					self.preview_generation.wrapping_add(1);
				self.held_selection = None;
				self.history_generation =
					self.history_generation.wrapping_add(1);
				self.tree_generation = self.tree_generation.wrapping_add(1);
				for slot in [
					&mut self.history_cancel,
					&mut self.preview_cancel,
					&mut self.tree_cancel,
					&mut self.rev_tree_cancel,
					&mut self.repo_cancel,
					&mut self.scan_cancel,
					&mut self.copy_cancel,
					&mut self.add_cancel,
					&mut self.changes_cancel,
				] {
					if let Some(token) = slot.as_ref() {
						token.cancel();
					}
				}
				self.changes_generation =
					self.changes_generation.wrapping_add(1);
				self.changes_queue.clear_pending();
				self.paste.invalidate_job();
				let name = intent.name();
				self.emit_life("draining", name, None);
				self.set_status("workspace_draining", []);
			}
		}
		self.arm_watch(cx);
		cx.notify();
	}

	/// The Git load a drain into `intent` waits on: a tab's own close or
	/// open waits for its own jobs only; Quit waits for every Git child.
	fn drain_load(
		&self,
		intent: Option<&lifecycle::Intent>,
	) -> lifecycle::GitLoad {
		match intent {
			Some(lifecycle::Intent::Quit) | None => {
				lifecycle::GitLoad::current()
			}
			Some(_) => lifecycle::GitLoad::own_tab(),
		}
	}

	fn poll_lifecycle(&mut self, cx: &mut Context<Self>) {
		let now = std::time::Instant::now();
		let git = self.drain_load(self.lifecycle.pending_intent());
		match self.lifecycle.poll_at(now, git) {
			lifecycle::Step::Idle | lifecycle::Step::Draining => {
				if self.lifecycle.is_draining() {
					let intent = self.lifecycle.intent_name();
					self.emit_life("draining", intent, None);
				}
			}
			lifecycle::Step::Ready(intent) => {
				let git = self.drain_load(Some(&intent));
				if self.lifecycle.unfinished() > 0 || !git.is_clear() {
					self.lifecycle.resume(intent, now);
					let intent = self.lifecycle.intent_name();
					self.emit_life("draining", intent, None);
					return;
				}
				self.finish_intent(intent, cx);
			}
			lifecycle::Step::Failed { intent, reason } => {
				let key = match reason {
					"leaked" => "workspace_drain_leaked",
					_ => "workspace_drain_timeout",
				};
				let name = intent.name();
				self.emit_life("failed", name, Some(reason));
				self.resume_after_drain(cx);
				self.set_status(key, []);
				if self.ws_tab.is_some() {
					cx.emit(tabs::TabEvent::DrainFailed);
				}
			}
		}
	}

	/// A drain that did not finish cancelled and outdated the open repo's
	/// reads and the workspace stays open: fetch what never landed. Also
	/// run for a tab whose quit drained when the quit stopped elsewhere.
	pub(crate) fn resume_after_drain(&mut self, cx: &mut Context<Self>) {
		self.resume_remote_restore(cx);
		self.resume_remote_scan(cx);
		self.preview_loading = false;
		self.tree_cancel = None;
		self.rev_tree_cancel = None;
		self.tree_queue.clear();
		self.tree_worker = self.tree_worker.wrapping_add(1);
		self.tree_worker_alive = false;
		if let Some(tree) = self.file_tree.as_mut() {
			tree.clear_loading();
		}
		self.resume_ws_tree(cx);
		// The drain cancelled and outdated the open repo's reads, and
		// the workspace stays open: fetch what never landed.
		if let (Some(idx), Some(_)) = (self.selected_repo_idx, self.repo()) {
			if !self.changes_loaded {
				self.select_repo_internal(idx, true, cx);
			} else if self.commits.is_empty() && self.history_error.is_none() {
				self.load_history(cx);
			}
		}
		self.start_changes_queue(true, cx);
		cx.notify();
	}

	fn finish_intent(
		&mut self,
		intent: lifecycle::Intent,
		cx: &mut Context<Self>,
	) {
		let git = self.drain_load(Some(&intent));
		if self.lifecycle.unfinished() > 0 || !git.is_clear() {
			if e2e_on() {
				app_log!(
					"[APP:LIFECYCLE: phase=blocked jobs={} inflight={} queued={} leaked={}]",
					self.lifecycle.live_jobs(),
					git.in_flight,
					git.queued,
					git.leaked
				);
			}
			self.lifecycle.resume(intent, std::time::Instant::now());
			return;
		}
		let name = intent.name();
		self.emit_life("drained", name, None);
		match intent {
			lifecycle::Intent::Quit if self.ws_tab.is_some() => {
				cx.emit(tabs::TabEvent::QuitDrained)
			}
			lifecycle::Intent::Quit => cx.quit(),
			lifecycle::Intent::CloseWorkspace => self.finish_close(cx),
			lifecycle::Intent::OpenWorkspace(path) => {
				self.finish_open(path, cx)
			}
			lifecycle::Intent::OpenRemoteWorkspace(target) => {
				self.finish_open_remote(*target, cx)
			}
		}
	}

	fn release_workspace_state(&mut self, cx: &mut Context<Self>) {
		self.drop_overlays(overlay::DropScope::All);
		self.selected_repo_idx = None;
		self.release_repo_state();
		self.ws_tree = None;
		self.ws_home = None;
		release_vec(&mut self.tree_selections);
		release_vec(&mut self.files);
		release_vec(&mut self.change_repos);
		self.last_changes_empty.set(None);
		if let Some(token) = self.changes_cancel.take() {
			token.cancel();
		}
		self.changes_generation = self.changes_generation.wrapping_add(1);
		self.changes_queue.clear_pending();
		self.preview_error_root = None;
		self.paste.invalidate_job();
		if !self.paste_busy() {
			self.paste.clear(self.preview.as_ref());
			// A vanished repo or a wiped remote scan ends the preview here,
			// not through `restore_log_after_paste`: its typed field too.
			self.drop_paste_dest_edit();
		}
		self.discovery = None;
		self.discovery_status = None;
		release_vec(&mut self.discovery_errors);
		release_vec(&mut self.discovery_depth_limited);
		self.discovery_error_overflow = 0;
		self.discovery_depth_overflow = 0;
		self.pinned_repo = None;
		self.carried_repo = None;
		self.refresh_reload = false;
		self.log_deferred = false;
		release_vec(&mut self.manual_repos);
		self.add_cancel = None;
		self.is_loading = false;
		self.is_copying = false;
		self.scan_cancel = None;
		self.copy_cancel = None;
		release_path(&mut self.workspace_root);
		self.remote.session = None;
		self.remote.scan_error = None;
		self.remote.scan_recovery = None;
		self.selected_file_root = None;
		self.clear_workspace_inputs(cx);
		// Last: the log's scope is released by now, so the settle has
		// nothing to reload, and the paste preview is already closed.
		let change = self.repos.apply(RepoOp::Release, &self.workspace_root);
		self.repos_changed(change, cx);
	}

	/// Drops everything that belongs to the open repo and stops its reads, so
	/// a late result cannot repopulate it.
	fn release_repo_state(&mut self) {
		self.generation = self.generation.wrapping_add(1);
		self.preview_generation = self.preview_generation.wrapping_add(1);
		self.held_selection = None;
		self.history_generation = self.history_generation.wrapping_add(1);
		self.tree_generation = self.tree_generation.wrapping_add(1);
		self.tree_worker = self.tree_worker.wrapping_add(1);
		for slot in [
			&mut self.history_cancel,
			&mut self.preview_cancel,
			&mut self.tree_cancel,
			&mut self.rev_tree_cancel,
			&mut self.repo_cancel,
			&mut self.details_cancel,
		] {
			if let Some(token) = slot.take() {
				token.cancel();
			}
		}
		release_vec(&mut self.commits);
		release_vec(&mut self.refs);
		self.head_sha = None;
		self.graph_layout = None;
		self.active_ref_filter = None;
		self.commit_page = 0;
		self.history_has_more = false;
		self.history_error = None;
		release_vec(&mut self.page_checkpoints);
		self.log_search = None;
		release_vec(&mut self.collapsed_merges);
		release_vec(&mut self.hidden_commits);
		self.history_walk = None;
		self.selected_commit = None;
		self.range_head = None;
		release_vec(&mut self.log_selected);
		self.select_head_after_load = false;
		self.reset_commit_view();
		release_vec(&mut self.log_branches_all);
		self.commit_rows_cache.take();
		self.log_first_page = 0;
		self.history_extending = false;
		self.history_loaded = false;
		self.last_log_empty.set(None);
		self.history_autoload = true;
		release_vec(&mut self.log_on_head);
		self.log_filter = LogQuery::default();
		self.drop_overlays(overlay::DropScope::AnyLogMenu);
		release_vec(&mut self.changed_dirs_collapsed);
		self.log_date_error = false;
		release_vec(&mut self.log_paths_expanded);
		release_vec(&mut self.log_branch_menu_open);
		release_vec(&mut self.log_feeds);
		release_vec(&mut self.log_scope_key);
		self.log_reload_for = None;
		self.details_generation = self.details_generation.wrapping_add(1);
		self.git_user_email = None;
		self.compare = None;
		self.changes_loaded = false;
		self.file_tree = None;
		self.rev_tree = None;
		self.selected_file = None;
		self.selected_file_source = None;
		self.selected_file_root = None;
		self.tree_cursor = 0;
		self.selected_list_row = 0;
		self.clear_preview();
		self.preview_loading = false;
		self.preview_error = None;
		self.reader.release_retained();
		self.tree_queue = VecDeque::new();
		self.tree_worker_alive = false;
		release_vec(&mut self.restore_expanded);
		release_vec(&mut self.restore_ws_expanded);
	}

	/// Drops workspace text without `InputEvent::Changed`, which would start
	/// a find or a log search.
	fn clear_workspace_inputs(&mut self, cx: &mut Context<Self>) {
		self.reader.find_query = String::new();
		let inputs: Vec<_> = self
			.text_inputs()
			.into_iter()
			.map(|(_, input)| input.clone())
			.collect();
		for input in inputs {
			input.update(cx, |field, _| field.clear_retained());
		}
	}

	fn finish_close(&mut self, cx: &mut Context<Self>) {
		self.release_workspace_state(cx);
		// A closed folder is not reconnected by a later stopped drain.
		self.remote.restoring = None;
		self.workspace_open = false;
		if self.ws_tab.is_some() {
			// The root drops the tab.
			if e2e_on() {
				app_log!(
					"[APP:WORKSPACE: state=closed generation={}]",
					self.lifecycle.generation()
				);
			}
			cx.emit(tabs::TabEvent::Closed);
			return;
		}
		self.open_overlay(
			overlay::OverlayKind::WorkspaceMenu,
			overlay::ReturnFocus::Root,
			Some(overlay::TriggerId::Workspace),
			None,
			cx,
		);
		if e2e_on() {
			app_log!(
				"[APP:WORKSPACE: state=closed generation={}]",
				self.lifecycle.generation()
			);
		}
		self.set_status("workspace_closed", []);
		cx.notify();
	}

	fn finish_open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
		// The Repository chip is remembered for the same workspace only (a
		// close has released `workspace_root`; the recent list still has it).
		if self.recent_workspaces.first() != Some(&path) {
			self.log_repo_filter.clear();
		}
		self.release_workspace_state(cx);
		self.workspace_root = path.clone();
		recent::remember(&mut self.recent_workspaces, &path);
		self.remote.restoring = None;
		self.workspace_open = true;
		if e2e_on() {
			app_log!(
				"[APP:WORKSPACE: state=open path={} generation={}]",
				path.display(),
				self.lifecycle.generation()
			);
		}
		self.set_status("workspace_opening", [path.display().to_string()]);
		self.reload_repos(cx);
	}

	/// The workspace button, without a window: the keyboard goes back to
	/// the root when the menu closes.
	pub fn toggle_workspace_menu(&mut self, cx: &mut Context<Self>) {
		self.toggle_workspace_menu_at(None, cx);
	}

	/// The workspace button (or Cmd+Shift+W / O): a press closes the menu
	/// it opened.
	pub(crate) fn toggle_workspace_menu_at(
		&mut self,
		window: Option<&mut gpui::Window>,
		cx: &mut Context<Self>,
	) {
		let ret = match window.as_deref() {
			Some(w) => self.return_here(w, cx),
			None => overlay::ReturnFocus::Root,
		};
		self.toggle_overlay(
			overlay::OverlayKind::WorkspaceMenu,
			Some(overlay::TriggerId::Workspace),
			ret,
			window,
			cx,
		);
	}

	/// The OS folder dialog. Without one (Linux with no portal) the typed
	/// path field opens instead.
	pub fn open_folder_dialog(&mut self, cx: &mut Context<Self>) {
		self.close_overlay(
			overlay::OverlayKind::WorkspaceMenu,
			overlay::CloseWhy::Picked,
			cx,
		);
		let picked = cx.prompt_for_paths(PathPromptOptions {
			files: false,
			directories: true,
			multiple: false,
			prompt: Some(i18n::t("workspace_open_confirm", self.locale).into()),
		});
		let tab = self.ws_tab;
		cx.spawn(async move |this, cx| {
			tabs::tagged(tab, async move {
				let picked = picked.await;
				let _ = this.update(cx, |this, cx| match picked {
					Ok(Ok(Some(paths))) => {
						if let Some(path) = paths.into_iter().next() {
							this.open_workspace_path(path, cx);
						}
					}
					Ok(Ok(None)) => {}
					_ => this.show_workspace_picker(cx),
				});
			})
			.await
		})
		.detach();
	}

	pub fn show_workspace_picker(&mut self, cx: &mut Context<Self>) {
		if !self.overlays.is_open(overlay::OverlayKind::WorkspaceMenu) {
			self.open_overlay(
				overlay::OverlayKind::WorkspaceMenu,
				overlay::ReturnFocus::Root,
				Some(overlay::TriggerId::Workspace),
				None,
				cx,
			);
		}
		// A callback that lands in a background tab opens nothing: no field
		// is drawn to take the keyboard when the tab is shown again.
		if self.overlays.is_open(overlay::OverlayKind::WorkspaceMenu) {
			self.overlays.set_workspace_picker(true);
			self.pending_focus =
				Some(self.workspace_path_input.read(cx).handle());
		}
		cx.notify();
	}

	pub fn confirm_open_workspace(
		&mut self,
		text: &str,
		cx: &mut Context<Self>,
	) {
		let text = text.trim();
		if text.is_empty() {
			return;
		}
		self.open_workspace_path(PathBuf::from(text), cx);
	}

	/// Opens `path` as the workspace after the usual close checks.
	pub fn open_workspace_path(
		&mut self,
		path: PathBuf,
		cx: &mut Context<Self>,
	) {
		if !path.is_dir() {
			self.set_status("workspace_bad_path", [path.display().to_string()]);
			cx.notify();
			return;
		}
		let path = dunce::canonicalize(&path).unwrap_or(path);
		self.workspace_path_input
			.update(cx, |input, _| input.clear_retained());
		self.route_open(tabs::OpenTarget::Local(path), cx);
	}

	/// A tab asks the root, which opens a new tab, switches to the tab that
	/// already shows the workspace, or fills this tab when it is empty.
	pub(crate) fn route_open(
		&mut self,
		target: tabs::OpenTarget,
		cx: &mut Context<Self>,
	) {
		if self.ws_tab.is_some() {
			self.close_overlay(
				overlay::OverlayKind::WorkspaceMenu,
				overlay::CloseWhy::Picked,
				cx,
			);
			cx.emit(tabs::TabEvent::Open(target));
			cx.notify();
		} else {
			self.open_in_place(target, cx);
		}
	}

	/// Opens `target` in this model after the usual close checks.
	pub(crate) fn open_in_place(
		&mut self,
		target: tabs::OpenTarget,
		cx: &mut Context<Self>,
	) {
		self.request_user_close(target.into_intent(), cx);
	}

	/// The workspace this tab shows, or the one its drain is opening.
	pub fn ws_identity(&self) -> Option<tabs::WsIdentity> {
		if let Some(intent) = self.lifecycle.pending_intent() {
			if let Some(id) = tabs::WsIdentity::of_intent(intent) {
				return Some(id);
			}
		}
		if !self.workspace_open {
			return self.remote.restoring.as_ref().map(|r| {
				tabs::WsIdentity::Remote {
					host: r.folder.host.clone(),
					path: r.folder.path.clone(),
				}
			});
		}
		Some(match &self.remote.session {
			Some(session) => tabs::WsIdentity::Remote {
				host: session.host().to_string(),
				path: session.workspace.id.clone(),
			},
			None => tabs::WsIdentity::Local(self.workspace_root.clone()),
		})
	}

	/// The tab's close (or a quit) is draining: it is about to go away.
	pub fn is_closing(&self) -> bool {
		matches!(
			self.lifecycle.pending_intent(),
			Some(lifecycle::Intent::CloseWorkspace | lifecycle::Intent::Quit)
		)
	}

	/// The tab's own close is draining (not a quit).
	pub fn is_closing_tab(&self) -> bool {
		matches!(
			self.lifecycle.pending_intent(),
			Some(lifecycle::Intent::CloseWorkspace)
		)
	}

	/// No workspace, none opening, no paste: an open may fill this tab.
	pub fn is_empty_tab(&self) -> bool {
		!self.workspace_open
			&& self.remote.restoring.is_none()
			&& !self.lifecycle.is_draining()
			&& !self.paste.is_open()
	}

	/// A confirmed paste or replay is being written.
	pub fn blocks_close(&self) -> bool {
		self.paste_busy() || self.lifecycle.has_mutating()
	}

	/// What the tab bar shows for this tab.
	pub fn tab_info(&self) -> tabs::TabInfo {
		let identity = self.ws_identity();
		let opening = self.lifecycle.pending_intent().is_some_and(|i| {
			matches!(i, lifecycle::Intent::OpenRemoteWorkspace(_))
		});
		let failed = self
			.remote
			.restoring
			.as_ref()
			.and_then(|r| r.failed.clone());
		let (full, conn) = match (&identity, &self.remote.session) {
			(Some(tabs::WsIdentity::Remote { host, path }), session) => {
				let conn = if failed.is_some() && !opening && !self.remote.busy
				{
					tabs::Conn::Failed
				} else if opening || session.is_none() {
					tabs::Conn::Connecting
				} else if self.remote.scan_error.is_some() {
					tabs::Conn::Failed
				} else {
					tabs::Conn::Connected
				};
				(format!("{host}:{path}"), Some(conn))
			}
			(Some(tabs::WsIdentity::Local(path)), _) => {
				(path.display().to_string(), None)
			}
			(None, _) if self.remote.busy => {
				(String::new(), Some(tabs::Conn::Connecting))
			}
			(None, _) => (String::new(), None),
		};
		tabs::TabInfo {
			identity,
			full,
			conn,
			pasting: self.blocks_close(),
			failed,
		}
	}

	/// The root shows another tab: this one keeps its state, but its focus
	/// guard must not pull the keyboard back to a handle nothing draws.
	pub(crate) fn leave_tab(
		&mut self,
		keep: Option<FocusHandle>,
		cx: &mut Context<Self>,
	) {
		self.resume_focus = keep;
		self.focus_lost_guard = None;
		self.escape_guard = None;
		// A floating layer needs the pointer and the frame: it does not
		// outlive the switch, and the host browse it owned ends with it. An
		// inline layer is part of its pane and stays.
		self.drop_overlays(overlay::DropScope::Floating);
		self.overlays.clear_pending_return();
		self.dragging = None;
		cx.notify();
	}

	/// True when this copy may write the clipboard. A cancelled token or a
	/// generation change drops the text.
	fn accept_copy_result(&mut self, ws_gen: u64, token: &CancelToken) -> bool {
		self.is_copying = false;
		let cancelled = token.is_cancelled();
		let stale = self.lifecycle.generation() != ws_gen;
		if !cancelled && !stale {
			return true;
		}
		let why = if cancelled { "cancelled" } else { "stale" };
		app_log!("[APP:COPY_DISCARDED: {why}]");
		if cancelled && !stale {
			self.set_status("status_copy_cancelled", []);
		}
		false
	}

	pub fn cancel_copy(&mut self, cx: &mut Context<Self>) {
		if !self.is_copying {
			return;
		}
		if let Some(token) = &self.copy_cancel {
			token.cancel();
		}
		self.set_status("status_copy_cancelled", []);
		app_log!("[APP:COPY_CANCEL]");
		cx.notify();
	}

	pub fn reload_repos(&mut self, cx: &mut Context<Self>) {
		self.user_action();
		if self.refresh_failed_restore(cx) {
			return;
		}
		if !self.accepting_work() {
			return;
		}
		self.discovery_errors.clear();
		self.discovery_depth_limited.clear();
		self.discovery_error_overflow = 0;
		self.discovery_depth_overflow = 0;
		// The first load selects a repo itself; a Refresh must also re-read
		// the open one, whose summary alone would otherwise update.
		self.refresh_reload = self.file_tree.is_some();
		// Rebuilt when the walk ends, so a Refresh re-reads plain folders;
		// its selection and open folders come back from here.
		self.keep_tree_selections();
		if let Some(tree) = self.ws_tree.take() {
			self.restore_ws_expanded.clear();
			tree.collect_expanded_paths(&mut self.restore_ws_expanded);
		}
		if let Some(session) = &self.remote.session {
			if self.selected_commit.is_none()
				&& self.selected_file_source == Some(SourceKind::File)
			{
				if let Some(path) = self.selected_file.clone() {
					let root = self
						.selected_file_root
						.clone()
						.unwrap_or_else(|| session.root.clone());
					self.select_file_in(
						Some(root),
						&path,
						SourceKind::File,
						cx,
					);
				}
			}
			self.launch_remote_scan(None, true, cx);
			return;
		}
		let ws = self.workspace_root.clone();
		self.launch_fresh_discovery(ws, true, cx);
	}

	pub fn continue_discovery(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work() || self.is_loading {
			return;
		}
		if self.remote.session.is_some() {
			let Some(path) = self.discovery_depth_limited.first().cloned()
			else {
				self.set_status(
					"remote_scan_incomplete",
					[self.repos.len().to_string()],
				);
				cx.notify();
				return;
			};
			self.discovery_depth_limited.remove(0);
			self.launch_remote_scan(Some(path), false, cx);
			return;
		}
		if matches!(self.discovery_status, Some(ScanStatus::LimitReached)) {
			if let Some(disc) = self.discovery.as_mut() {
				disc.raise_found_page();
			}
		}
		if self.discovery.as_ref().is_some_and(Discovery::has_cursor) {
			let Some(disc) = self.discovery.take() else {
				return;
			};
			self.launch_discovery_cursor(disc, false, cx);
			return;
		}
		let Some(path) = self.discovery_depth_limited.first().cloned() else {
			return;
		};
		self.discovery_depth_limited.remove(0);
		self.launch_fresh_discovery(path, false, cx);
	}

	pub fn add_repo_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
		if self.remote_blocks() {
			cx.notify();
			return;
		}
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.add_cancel);
		let cancel_job = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		self.spawn_owned(cx, Some(cancel_job), async move {
			let bg = async_app.background_executor().clone();
			let cancel_bg = cancel.clone();
			let outcome = bg
				.spawn(async move { resolve_added_repo(path, &cancel_bg) })
				.await;
			let _ = this.update(&mut async_app, |model, cx| {
				if cancel.is_cancelled() {
					return;
				}
				model.install_added_repo(outcome, cx);
			});
		});
	}

	fn launch_fresh_discovery(
		&mut self,
		root: PathBuf,
		wipe: bool,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		self.discovery_generation = self.discovery_generation.wrapping_add(1);
		let generation = self.discovery_generation;
		let cancel = arm_cancel(&mut self.scan_cancel);
		self.is_loading = true;
		self.set_status("status_scanning", []);
		let cancel_job = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		self.spawn_owned(cx, Some(cancel_job), async move {
			let bg = async_app.background_executor().clone();
			let open_root = root.clone();
			let opened = bg
				.spawn(async move { Discovery::new(&open_root, 8, 256) })
				.await;
			let disc = match opened {
				Ok(disc) => disc,
				Err(err) => {
					let _ = this.update(&mut async_app, |model, cx| {
						if model.discovery_generation != generation {
							return;
						}
						model.discovery = None;
						model.discovery_status = Some(ScanStatus::Incomplete);
						model.push_discovery_error((
							root.clone(),
							Failure::io(&err, &root.display().to_string()),
						));
						model.finish_discovery(cx);
					});
					return;
				}
			};
			drive_discovery(
				this,
				&mut async_app,
				disc,
				generation,
				cancel,
				wipe,
			)
			.await;
		});
	}

	fn launch_discovery_cursor(
		&mut self,
		disc: Discovery,
		wipe: bool,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		self.discovery_generation = self.discovery_generation.wrapping_add(1);
		let generation = self.discovery_generation;
		let cancel = arm_cancel(&mut self.scan_cancel);
		self.is_loading = true;
		self.set_status("status_scanning", []);
		let cancel_job = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		self.spawn_owned(cx, Some(cancel_job), async move {
			drive_discovery(
				this,
				&mut async_app,
				disc,
				generation,
				cancel,
				wipe,
			)
			.await;
		});
	}

	/// One page of a scan: the first of a rescan (`wipe`) first shrinks the
	/// list to the manual repos plus the open one, so the selection keeps
	/// naming the same repo while later pages arrive. A newer read of a known
	/// repo replaces it.
	fn apply_scan_page(
		&mut self,
		wipe: bool,
		page: Vec<RepoEntry>,
		cx: &mut Context<Self>,
	) {
		let keys: Vec<_> = page.iter().map(repo_key).collect();
		let op = if wipe {
			RepoOp::Rescan {
				kept: self.rescan_kept(),
				page,
			}
		} else {
			RepoOp::Merge(page)
		};
		let change = self.repos.apply(op, &self.workspace_root);
		// Found again by the walk: no longer a repo only kept.
		if self.carried_repo.as_ref().is_some_and(|c| keys.contains(c)) {
			self.carried_repo = None;
		}
		self.repos_changed(change, cx);
	}

	/// What a rescan keeps of the list: the manual repos plus the open one.
	/// A repo only kept (not a manual one) is carried until the walk finds
	/// it or proves it gone.
	fn rescan_kept(&mut self) -> Vec<RepoEntry> {
		let open = self.pinned_repo.as_ref().and_then(|key| {
			self.repos
				.iter()
				.find(|e| repo_key_matches(e, key))
				.cloned()
		});
		let mut keep = self.manual_repos.clone();
		keep.extend(open);
		// A manual repo is kept regardless of the walk.
		self.carried_repo = self.pinned_repo.clone().filter(|key| {
			!self.manual_repos.iter().any(|m| repo_key_matches(m, key))
		});
		keep
	}

	fn push_discovery_error(&mut self, item: (PathBuf, Failure)) {
		if self.discovery_errors.len() < 64 {
			self.discovery_errors.push(item);
		} else {
			self.discovery_error_overflow =
				self.discovery_error_overflow.saturating_add(1);
		}
	}

	fn push_depth_limit(&mut self, path: PathBuf) {
		if self.discovery_depth_limited.len() < 64 {
			self.discovery_depth_limited.push(path);
		} else {
			self.discovery_depth_overflow =
				self.discovery_depth_overflow.saturating_add(1);
		}
	}

	fn place_selection(&mut self, cx: &mut Context<Self>) {
		let Some(key) = self.pinned_repo.clone() else {
			if self.selected_repo_idx.is_none() && !self.repos.is_empty() {
				self.pinned_repo = self.repos.first().map(repo_key);
				self.select_repo_internal(0, false, cx);
			}
			return;
		};
		let Some(pos) = self
			.repos
			.iter()
			.position(|entry| repo_key_matches(entry, &key))
		else {
			// Any index left here would name a different repo.
			self.selected_repo_idx = None;
			return;
		};
		let root = self.repos[pos].root.clone();
		if self.repo_root().as_ref() == Some(&root) {
			self.selected_repo_idx = Some(pos);
		} else if self.file_tree.is_none() || self.selected_repo_idx.is_none() {
			self.select_repo_internal(pos, false, cx);
		} else {
			self.selected_repo_idx = Some(pos);
		}
	}

	/// The open repo's summary read by the same status as its Changes list,
	/// so the repo row's counts match the list.
	fn update_open_summary(&mut self, summary: Option<RepoSummary>) {
		let Some(summary) = summary else {
			return;
		};
		if self.repo().is_some() {
			if let Some(root) = self.repo_root() {
				self.repos.set_summary(&root, summary);
			}
		}
	}

	/// Removes a kept repo the rescan did not find. Returns its name when it
	/// was the open one, whose view is then released.
	fn drop_vanished_repo(
		&mut self,
		key: &(PathBuf, PathBuf),
		cx: &mut Context<Self>,
	) -> Option<String> {
		let pos = self.repos.position_of(key)?;
		let (gone_root, gone_name) =
			(self.repos[pos].root.clone(), self.repos[pos].name.clone());
		// Its kept selection names files that can no longer be read.
		self.tree_selections.retain(|(root, _)| root != &gone_root);
		let was_open = self.pinned_repo.as_ref() == Some(key);
		if was_open {
			self.pinned_repo = None;
			self.selected_repo_idx = None;
			self.release_repo_state();
		}
		let change = self
			.repos
			.apply(RepoOp::Remove(key.clone()), &self.workspace_root);
		self.repos_changed(change, cx);
		was_open.then_some(gone_name)
	}

	/// A paste preview opened (or remapped) before the scan finished lists
	/// only the repos known then: add the ones found since. Runs when the
	/// scan finishes and whenever a preview job lands, since a job in flight
	/// at the scan's end hands back the old candidates.
	fn refresh_paste_candidates(&mut self, cx: &mut Context<Self>) {
		let roots = self.repos.roots();
		match self.paste.refresh_candidates(&roots, self.preview.as_ref()) {
			Ok(true) => {
				app_log!("[APP:PASTE_CANDIDATES_REFRESHED]");
				cx.notify();
			}
			Ok(false) => {}
			Err(err) => self.set_status_msg(err),
		}
	}

	fn finish_discovery(&mut self, cx: &mut Context<Self>) {
		self.is_loading = false;
		// The rows past the kept ones become one row each, typed by what
		// they stand for: they are shown (the scan-error count, the retry
		// button, the empty states) as they always were.
		if self.discovery_error_overflow > 0 {
			let count = self.discovery_error_overflow;
			self.discovery_errors.push((
				self.workspace_root.clone(),
				Failure::new(
					format!("{count} more discovery errors omitted"),
					Reason::ScanErrorsOmitted {
						count: count as u64,
					},
				),
			));
			self.discovery_error_overflow = 0;
		}
		if self.discovery_depth_overflow > 0 {
			let count = self.discovery_depth_overflow;
			self.discovery_errors.push((
				self.workspace_root.clone(),
				Failure::new(
					format!("{count} more depth-limited directories omitted"),
					Reason::DepthLimitedOmitted {
						count: count as u64,
					},
				),
			));
			self.discovery_depth_overflow = 0;
		}
		self.place_selection(cx);
		self.ensure_ws_tree(cx);
		// Only a complete walk proves the kept repo is gone; a capped or
		// partial one may simply not have reached it yet.
		let carried =
			matches!(self.discovery_status, Some(ScanStatus::Complete))
				.then(|| self.carried_repo.take())
				.flatten();
		let vanished =
			carried.and_then(|key| self.drop_vanished_repo(&key, cx));
		let errors = self.repos.iter().filter(|r| r.summary.is_err()).count();
		self.set_status(
			"status_repos_loaded",
			[self.repos.len().to_string(), errors.to_string()],
		);
		if let Some(name) = vanished {
			// Releasing the repo also dropped the tree worker.
			self.resume_ws_tree(cx);
			app_log!("[APP:REPO_VANISHED: {name}]");
			self.set_status("status_repo_vanished", [name]);
			self.refresh_reload = false;
			// The log shows the workspace, not the dropped repository:
			// reload it over the repositories left.
			if !self.repos.is_empty() {
				self.load_history(cx);
			}
		} else if std::mem::take(&mut self.refresh_reload) {
			if let (Some(idx), Some(_)) = (self.selected_repo_idx, self.repo())
			{
				self.select_repo_internal(idx, true, cx);
			}
		}
		self.settle_repo_consumers(cx);
		self.start_changes_queue(false, cx);
		app_log!("[APP:READY_REPOS: {}]", self.repos.len());
		if e2e_on() {
			for r in &self.repos {
				match &r.summary {
					Ok(s) => app_log!(
						"[APP:E2E_REPO: name={} ok=true branch={} staged={} unstaged={} untracked={} conflicts={}]",
						r.name,
						s.branch.as_deref().unwrap_or(""),
						s.changes.staged,
						s.changes.unstaged,
						s.changes.untracked,
						s.changes.conflicted
					),
					Err(_) => app_log!("[APP:E2E_REPO: name={} ok=false]", r.name),
				}
			}
		}
		if self.mode == "overview"
			&& matches!(
				self.discovery_status,
				Some(ScanStatus::Complete | ScanStatus::Incomplete)
			) {
			ready_marker("OVERVIEW");
		}
		cx.notify();
	}

	fn install_added_repo(
		&mut self,
		outcome: Result<RepoEntry, Box<Failure>>,
		cx: &mut Context<Self>,
	) {
		let entry = match outcome {
			Ok(entry) => entry,
			Err(err) => {
				self.set_status_msg(Msg::with_msg_arg(
					"error_repo_status",
					[String::new()],
					0,
					crate::reason::failure_msg(&err),
				));
				cx.notify();
				return;
			}
		};
		let key = repo_key(&entry);
		if let Some(pos) = self
			.repos
			.iter()
			.position(|have| repo_key_matches(have, &key))
		{
			self.pinned_repo = Some(key);
			self.select_repo_internal(pos, false, cx);
			return;
		}
		if !self
			.manual_repos
			.iter()
			.any(|have| repo_key_matches(have, &key))
		{
			self.manual_repos.push(entry.clone());
		}
		let change = self
			.repos
			.apply(RepoOp::Merge(vec![entry]), &self.workspace_root);
		if let Some(pos) = self.repos.position_of(&key) {
			self.pinned_repo = Some(key);
			// Before the fan-out: the open repo's own reload then owns the
			// Log's, and the Log is not read twice.
			self.select_repo_internal(pos, false, cx);
		}
		self.repos_changed(change, cx);
		cx.notify();
	}

	/// A repo row's chevron folds or unfolds it; only the row itself
	/// opens another repo. The workspace repo's row, open around another
	/// repo, folds its tree without switching.
	pub fn toggle_repo_chevron(&mut self, idx: usize, cx: &mut Context<Self>) {
		if self.selected_repo_idx != Some(idx)
			&& self.ws_tree.is_some()
			&& self.ws_repo_idx() == Some(idx)
		{
			self.ws_collapsed = !self.ws_collapsed;
			app_log!("[APP:WS_COLLAPSED: {}]", self.ws_collapsed);
			cx.notify();
		} else {
			self.toggle_repo_row(idx, cx);
		}
	}

	/// A repo row click, as in IntelliJ's Project view: expands a collapsed
	/// root, collapses an expanded one. Another repo is selected expanded.
	/// Scripted drivers that re-click the open repo to reload it must click
	/// twice (collapse, then expand).
	pub fn toggle_repo_row(&mut self, idx: usize, cx: &mut Context<Self>) {
		let expanded =
			self.selected_repo_idx == Some(idx) && !self.repo_collapsed;
		self.set_repo_row_expanded(idx, !expanded, cx);
	}

	pub fn set_repo_row_expanded(
		&mut self,
		idx: usize,
		expanded: bool,
		cx: &mut Context<Self>,
	) {
		self.repo_collapsed = !expanded;
		if expanded {
			// Re-expanding the open repo re-reads it, so the rows shown are
			// current (one status and one log page).
			self.select_repo(idx, cx);
		} else {
			cx.notify();
		}
	}

	pub fn select_repo(&mut self, idx: usize, cx: &mut Context<Self>) {
		if let Some(entry) = self.repos.get(idx) {
			self.pinned_repo = Some(repo_key(entry));
			// Opening a repo reveals its changes in every group; the others
			// stay collapsed.
			expand_repo_everywhere(
				&mut self.chrome.expanded_repos,
				&entry.root,
			);
		}
		self.select_repo_internal(idx, false, cx);
	}

	pub fn dispatch_tree(
		&mut self,
		cmd: Option<TreeCommand>,
		cx: &mut Context<Self>,
	) {
		let Some(cmd) = cmd else {
			return;
		};
		if !self.accepting_work() {
			return;
		}
		match &cmd {
			TreeCommand::OpenFile(rel) => {
				let rel = rel.clone();
				app_log!("[APP:TREE_FILE_SELECTED: {}]", rel);
				self.select_file_with_source(&rel, SourceKind::File, cx);
				return;
			}
			TreeCommand::ToggleSelect(key) => {
				let Some(paths) = self
					.file_tree
					.as_ref()
					.and_then(|tree| tree.selection_for_toggle(key))
				else {
					return;
				};
				if let Some(rel) = key.utf8_rel() {
					app_log!("[APP:TREE_TOGGLED: {}]", rel);
				}
				self.install_tree_selection(false, paths);
				self.log_tree_selection();
				cx.notify();
				return;
			}
			TreeCommand::Collapse(key) => {
				if let Some(rel) = key.utf8_rel().filter(|rel| !rel.is_empty())
				{
					app_log!("[APP:TREE_EXPANDED: {}]", rel);
				}
			}
			_ => {}
		}
		let effect = self.file_tree.as_mut().map(|tree| tree.start(cmd));
		match effect {
			Some(TreeEffect::Io(io)) => self.submit_tree_io(io, cx),
			Some(TreeEffect::OpenFile(rel)) => {
				self.select_file_with_source(&rel, SourceKind::File, cx);
				app_log!("[APP:TREE_FILE_SELECTED: {}]", rel);
			}
			Some(TreeEffect::Idle) => cx.notify(),
			None => {}
		}
	}

	/// Runs project-tree reads one at a time as an owned job, so close and
	/// quit wait for the directory read itself and not only for Git children.
	fn submit_tree_io(&mut self, io: TreeIo, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		if self.tree_worker_alive {
			self.tree_queue.push_back(io);
			return;
		}
		self.tree_worker = self.tree_worker.wrapping_add(1);
		let worker = self.tree_worker;
		self.tree_worker_alive = true;
		let cancel = arm_cancel(&mut self.tree_cancel);
		let ws_gen = self.lifecycle.generation();
		let hold = self.e2e_tree_hold.clone();
		let remote = self.remote_target();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		self.spawn_owned(cx, Some(cancel.clone()), async move {
			let mut pending = Some(io);
			while let Some(io) = pending.take() {
				if cancel.is_cancelled() {
					break;
				}
				let cancel_bg = cancel.clone();
				let hold_bg = hold.clone();
				let remote_bg = remote.clone();
				let result = bg
					.spawn(async move {
						let result = match remote_bg {
							Some((client, ws, session_root)) => {
								remote::tree_io(
									&client,
									&ws,
									&session_root,
									io,
									Some(&cancel_bg),
								)
							}
							None => {
								crate::tree::execute_tree_io(io, &cancel_bg)
							}
						};
						hold_tree_read(hold_bg.as_deref());
						result
					})
					.await;
				pending = this
					.update(&mut async_app, |model, cx| {
						// A close or reopen bumps the lifecycle generation;
						// a repo switch bumps the worker id.
						if model.lifecycle.generation() != ws_gen
							|| model.tree_worker != worker
						{
							if e2e_on() {
								app_log!(
									"[APP:TREE_IO_DISCARDED: stale rel={}]",
									result.key.utf8_rel().unwrap_or_default()
								);
							}
							return None;
						}
						model.apply_tree_result(result);
						let next = model.next_tree_io();
						if next.is_none() {
							model.tree_worker_alive = false;
							model.tree_cancel = None;
						}
						cx.notify();
						next
					})
					.ok()
					.flatten();
			}
			// Not gated on the generation: after a failed drain the
			// workspace stays open and must be able to read again.
			let _ = this.update(&mut async_app, |model, _| {
				if model.tree_worker == worker {
					model.tree_worker_alive = false;
					model.tree_cancel = None;
				}
			});
		});
	}

	fn apply_tree_result(&mut self, result: crate::tree::TreeIoResult) {
		if self
			.ws_tree
			.as_ref()
			.is_some_and(|tree| tree.full_path == result.base)
		{
			self.apply_ws_tree_result(result);
			return;
		}
		let Some(tree) = self.file_tree.as_mut() else {
			return;
		};
		if tree.full_path != result.base {
			return;
		}
		let Some(applied) = tree.apply_io_result(result) else {
			return;
		};
		if applied.kind == crate::tree::TreeIoKind::Expand
			&& !applied.rel.is_empty()
		{
			app_log!("[APP:TREE_EXPANDED: {}]", applied.rel);
		}
		app_log!(
			"[APP:TREE_PAGE: rel={} kind={:?} children={} has_more={} selected={}]",
			applied.rel,
			applied.kind,
			applied.child_count,
			applied.has_more,
			applied.selected_count
		);
	}

	fn apply_ws_tree_result(&mut self, result: crate::tree::TreeIoResult) {
		let Some(tree) = self.ws_tree.as_mut() else {
			return;
		};
		let Some(applied) = tree.apply_io_result(result) else {
			return;
		};
		app_log!(
			"[APP:WS_TREE_PAGE: rel={} kind={:?} children={} has_more={} selected={}]",
			applied.rel,
			applied.kind,
			applied.child_count,
			applied.has_more,
			applied.selected_count
		);
	}

	/// The workspace tree's root, while `ws_tree` holds it.
	pub fn ws_root(&self) -> Option<PathBuf> {
		self.ws_tree.as_ref().map(|tree| tree.full_path.clone())
	}

	/// The repo whose root is the workspace folder itself.
	pub fn ws_repo_idx(&self) -> Option<usize> {
		let home = self.ws_home.as_ref()?;
		self.repos.iter().position(|repo| &repo.root == home)
	}

	/// Builds the workspace tree once repos are known. None while the
	/// workspace repo is open (its `file_tree` is the tree), and when the
	/// workspace sits strictly inside a repo: that repo's tree shows it.
	fn ensure_ws_tree(&mut self, cx: &mut Context<Self>) {
		if self.ws_tree.is_some() || !self.accepting_work() {
			return;
		}
		if let Some(session) = &self.remote.session {
			let root = session.root.clone();
			self.ws_home = Some(root.clone());
			if self
				.file_tree
				.as_ref()
				.is_some_and(|tree| tree.full_path == root)
			{
				return;
			}
			self.ws_tree = Some(FileTreeNode::unloaded_root(&root));
			self.resume_ws_tree(cx);
			return;
		}
		// Git reports resolved toplevels; root the tree the same way so a
		// repo dir matches its entry (macOS /var -> /private/var).
		let Ok(root) = CanonicalRootId::new(&self.workspace_root) else {
			return;
		};
		let root = root.path().to_path_buf();
		self.ws_home = Some(root.clone());
		if self
			.repos
			.iter()
			.any(|repo| repo.root != root && root.starts_with(&repo.root))
		{
			return;
		}
		if self
			.file_tree
			.as_ref()
			.is_some_and(|tree| tree.full_path == root)
		{
			return;
		}
		let saved = self.kept_tree_selection(&root);
		let mut tree = FileTreeNode::unloaded_root(&root);
		tree.apply_selection(&saved);
		self.ws_tree = Some(tree);
		self.resume_ws_tree(cx);
	}

	/// A dropped tree queue or worker also dropped the workspace tree's
	/// reads: unmark them and re-read its root if it never landed.
	pub(crate) fn resume_ws_tree(&mut self, cx: &mut Context<Self>) {
		let Some(tree) = self.ws_tree.as_mut() else {
			return;
		};
		tree.clear_loading();
		if tree.is_loaded {
			return;
		}
		if let TreeEffect::Io(io) =
			tree.start(TreeCommand::Expand(NodeKey::root()))
		{
			self.submit_tree_io(io, cx);
		}
	}

	/// Project-tree commands on the workspace tree (non-repo files).
	pub fn dispatch_ws_tree(
		&mut self,
		cmd: Option<TreeCommand>,
		cx: &mut Context<Self>,
	) {
		let Some(cmd) = cmd else {
			return;
		};
		if !self.accepting_work() {
			return;
		}
		let Some(root) = self.ws_root() else {
			return;
		};
		if let TreeCommand::ToggleSelect(key) = &cmd {
			let Some(paths) = self
				.ws_tree
				.as_ref()
				.and_then(|tree| tree.selection_for_toggle(key))
			else {
				return;
			};
			if let Some(rel) = key.utf8_rel() {
				app_log!("[APP:WS_TREE_TOGGLED: {}]", rel);
			}
			self.install_tree_selection(true, paths);
			self.log_tree_selection();
			cx.notify();
			return;
		}
		let effect = self.ws_tree.as_mut().map(|tree| tree.start(cmd));
		match effect {
			Some(TreeEffect::Io(io)) => self.submit_tree_io(io, cx),
			Some(TreeEffect::OpenFile(rel)) => {
				app_log!("[APP:WS_FILE_SELECTED: {}]", rel);
				self.select_file_in(Some(root), &rel, SourceKind::File, cx);
			}
			Some(TreeEffect::Idle) => cx.notify(),
			None => {}
		}
	}

	/// The Project view's selection spans both trees and the selections
	/// kept for trees not shown: selecting rows alone (click, range,
	/// right-click) drops all of them.
	pub fn select_tree_rows_alone(
		&mut self,
		ws: bool,
		rels: &[String],
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let tree = if ws { &self.ws_tree } else { &self.file_tree };
		let Some(paths) = tree.as_ref().map(|t| t.selection_for_rels(rels))
		else {
			return;
		};
		// Rows that cannot be selected (a nested repo folder) keep the
		// selection the user built rather than wiping it.
		if paths.is_empty() {
			return;
		}
		self.tree_selections.clear();
		app_log!("[APP:TREE_SELECTED: {}]", rels.join(","));
		self.install_tree_selection(!ws, Vec::new());
		self.install_tree_selection(ws, paths);
		self.log_tree_selection();
		cx.notify();
	}

	fn next_tree_io(&mut self) -> Option<TreeIo> {
		if let Some(io) = self.tree_queue.pop_front() {
			return Some(io);
		}
		for (pending, tree) in [
			(&mut self.restore_expanded, &mut self.file_tree),
			(&mut self.restore_ws_expanded, &mut self.ws_tree),
		] {
			// Its root read is still queued: keep the list for it.
			let Some(tree) = tree.as_mut().filter(|tree| tree.is_loaded) else {
				continue;
			};
			while !pending.is_empty() {
				let key = NodeKey::from_utf8_rel(&pending.remove(0));
				if !tree.contains_dir(&key) {
					continue;
				}
				if let TreeEffect::Io(io) = tree.start(TreeCommand::Expand(key))
				{
					return Some(io);
				}
			}
		}
		None
	}

	pub fn select_repo_internal(
		&mut self,
		idx: usize,
		preserve_anchors: bool,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		if idx >= self.repos.len() {
			return;
		}
		// A read in flight is dropped by this reload, so the selection must
		// name what is shown again before it is judged or kept: the model
		// holds the dropped read's path, and the shown preview's commit
		// state is in the snapshot.
		self.restore_shown_selection();
		// The log shows the workspace: a plain switch keeps it.
		let reload_log = self.log_reloads_on_repo_switch(preserve_anchors);
		// A Changes row is kept across a reload only when its shown preview
		// was read from the repo being reloaded. Any other row (another
		// repo's, or one still loading) is dropped: the reload shows this
		// repo's default view, never another repo's or a stale read.
		let preserve_row = !preserve_anchors
			|| self.selected_file_source.as_ref() == Some(&SourceKind::File)
			|| self.selected_file.is_none()
			|| self.shown_change_repo().as_ref() == Some(&self.repos[idx].root);
		if !preserve_row {
			self.selected_file = None;
			self.selected_file_source = None;
			self.selected_file_root = None;
			self.clear_preview();
			self.preview_error = None;
		}
		self.generation += 1;
		self.preview_generation += 1;
		// The reads that could have restored it are dropped.
		self.held_selection = None;
		let task_generation = self.generation;
		if reload_log {
			self.history_generation += 1;
			let _ = arm_cancel(&mut self.history_cancel);
			self.log_reload_for = Some(task_generation);
		}
		self.tree_generation += 1;
		self.preview_loading = false;
		let _ = arm_cancel(&mut self.preview_cancel);
		let _ = arm_cancel(&mut self.tree_cancel);
		let _ = arm_cancel(&mut self.rev_tree_cancel);
		let cancel = arm_cancel(&mut self.repo_cancel);

		let anchor_file = if preserve_anchors {
			self.selected_file.clone()
		} else {
			None
		};
		let anchor_source = if preserve_anchors {
			self.selected_file_source.clone()
		} else {
			None
		};
		let anchor_file_root = if preserve_anchors {
			self.selected_file_root.clone()
		} else {
			None
		};
		let anchor_commit = if preserve_anchors {
			self.selected_commit.clone()
		} else {
			None
		};
		let mut expanded_paths = Vec::new();
		if preserve_anchors {
			if let Some(ref t) = self.file_tree {
				t.collect_expanded_paths(&mut expanded_paths);
			}
		}

		// A superseded read of the previously open repo is dropped when it
		// lands; the queue reads that repo instead of leaving it loading.
		let previous = self.repo_root();
		if previous.as_ref() != Some(&self.repos[idx].root) {
			if let Some(prev) = previous.filter(|prev| {
				self.change_repos.iter().any(|s| {
					&s.root == prev && s.state == ChangeRepoState::Loading
				})
			}) {
				self.changes_queue.push(prev);
				self.pump_changes(cx);
			}
		}
		self.selected_repo_idx = Some(idx);
		self.selected_file = anchor_file.clone();
		self.selected_file_root = anchor_file_root;
		self.selected_commit = anchor_commit.clone();
		self.range_head = None;
		self.log_selected.clear();
		self.compare = None;
		// The same commit stays selected while the repo is read again, so
		// its details and its repository stay (the Log may be pinned to
		// another repo than the one now open); the rest is read anew.
		let details = self.commit_view.commit_details.take();
		let root = self.commit_view.log_commit_root.take();
		self.reset_commit_view();
		if self.selected_commit.is_some() {
			self.commit_view.commit_details = details;
			self.commit_view.log_commit_root = root;
		}
		self.changes_loaded = false;
		// Its rows stay shown but inert until the re-read lands.
		let reloading = self.repos[idx].root.clone();
		if let Some(slot) =
			self.change_repos.iter_mut().find(|s| s.root == reloading)
		{
			if slot.state == ChangeRepoState::Loaded {
				slot.state = ChangeRepoState::Loading;
			}
		}
		if reload_log {
			self.commits.clear();
			self.refs.clear();
			self.head_sha = None;
			self.graph_layout = None;
			self.commit_page = 0;
			self.log_first_page = 0;
			self.log_on_head.clear();
			self.history_extending = false;
			self.page_checkpoints = vec![None];
			self.collapsed_merges.clear();
			self.hidden_commits.clear();
			self.history_walk = None;
			self.history_error = None;
		}
		if !preserve_anchors && reload_log {
			self.log_filter = LogQuery::default();
			self.git_user_email = None;
			// A reload of the same repo keeps the log's branch filter and
			// search, which the history reload applies again.
			self.active_ref_filter = None;
			self.log_search = None;
		}
		if !preserve_anchors {
			self.clear_preview();
			self.preview_error = None;
			self.rev_tree = None;
		}
		self.tree_cursor = 0;

		let repo_root = self.repos[idx].root.clone();
		let repo_name = self.repos[idx].name.clone();
		app_log!(
			"[APP:REPO_SELECTING: {} ({}) root={}]",
			idx,
			repo_name,
			repo_root.display()
		);
		self.set_status("status_repo_loading", [repo_name.clone()]);
		let is_file_anchor = preserve_anchors
			&& anchor_source.as_ref() == Some(&SourceKind::File)
			&& anchor_file.is_some();
		if !is_file_anchor {
			if let Err(e) = &self.repos[idx].summary {
				self.show_preview_error(Msg::with_msg_arg(
					"error_repo_status",
					[String::new()],
					0,
					crate::reason::failure_msg(e),
				));
			}
		}

		self.keep_tree_selections();
		let saved_files = self.kept_tree_selection(&repo_root);
		self.tree_queue.clear();
		self.tree_worker = self.tree_worker.wrapping_add(1);
		self.tree_worker_alive = false;
		let _ = arm_cancel(&mut self.tree_cancel);
		let _ = arm_cancel(&mut self.rev_tree_cancel);
		let mut tree = FileTreeNode::unloaded_root(&repo_root);
		tree.apply_selection(&saved_files);
		self.restore_expanded = if preserve_anchors {
			expanded_paths
		} else {
			Vec::new()
		};
		// One tree per root: the workspace repo's tree passes between
		// `file_tree` (open) and `ws_tree` (another repo open), so the
		// folders the user opened stay open around the repos inside it.
		let home = self.ws_home.clone();
		if home.as_ref() == Some(&repo_root) {
			if let Some(ws) = self.ws_tree.take() {
				if !preserve_anchors {
					ws.collect_expanded_paths(&mut self.restore_expanded);
				}
			}
		} else if self
			.file_tree
			.as_ref()
			.is_some_and(|t| Some(&t.full_path) == home.as_ref())
		{
			self.ws_tree = self.file_tree.take();
		}
		self.file_tree = Some(tree);
		let root_io = self.file_tree.as_mut().and_then(|tree| {
			match tree.start(TreeCommand::Expand(NodeKey::root())) {
				TreeEffect::Io(io) => Some(io),
				_ => None,
			}
		});
		if let Some(io) = root_io {
			self.submit_tree_io(io, cx);
		}
		self.resume_ws_tree(cx);
		// Leaving the workspace repo with no tree to hand over (released).
		if self.ws_home.is_some() {
			self.ensure_ws_tree(cx);
		}

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();

		let repo_root_for_update = repo_root.clone();
		let known = self.repos[idx].identity.clone();
		let host = self.git_host();
		self.spawn_owned(cx, Some(cancel), async move {
			let working_res = bg
				.spawn(async move {
					read_change_list(
						&host,
						&repo_root,
						known.as_ref(),
						cancel_bg,
					)
				})
				.await;

			let _ = this.update(&mut async_app, |model, cx| {
				if model.generation != task_generation {
					return;
				}
				match working_res {
					Ok((summary, changes, total)) => {
						model.update_open_summary(summary);
						let slot = model.ensure_change_slot(
							&repo_root_for_update,
							&repo_name,
						);
						model.install_slot_changes(slot, Ok((changes, total)));
						let slot = slot_range(&model.files, slot);
						model.changes_loaded = true;
						app_log!(
							"[APP:REPO_LOADED: {} files={}]",
							repo_name,
							slot.len()
						);
						model.set_status(
							"status_repo_loaded",
							[repo_name.clone(), slot.len().to_string()],
						);
						if anchor_commit.is_some() {
							// The history reload re-selects the commit.
						} else if let Some(ref anchor) = anchor_file {
							let kept = anchor_source.as_ref().filter(|s| {
								**s == SourceKind::File
									|| model.files[slot.clone()].iter().any(
										|f| {
											&f.path == anchor && &f.source == *s
										},
									)
							});
							if let Some(source) = kept {
								if *source == SourceKind::File {
									let root = model
										.selected_file_root
										.clone()
										.or_else(|| {
											model
												.remote
												.session
												.as_ref()
												.map(|s| s.root.clone())
										})
										.or_else(|| model.repo_root());
									model.select_file_in(
										root,
										anchor,
										source.clone(),
										cx,
									);
								} else {
									model.select_file_with_source(
										anchor,
										source.clone(),
										cx,
									);
								}
							} else if model.files[slot.clone()]
								.iter()
								.any(|f| &f.path == anchor)
							{
								model.selected_file_root = None;
								model.select_file(anchor, cx);
							} else if let Some(first) =
								model.files[slot.clone()].first()
							{
								model.selected_file_root = None;
								let first_path = first.path.clone();
								model.select_file(&first_path, cx);
							} else {
								model.selected_file = None;
								model.selected_file_source = None;
								model.selected_file_root = None;
								model.clear_preview();
								if model.mode == "preview" {
									ready_marker("PREVIEW");
								}
							}
						} else if let Some(first) =
							model.files[slot.clone()].first()
						{
							model.selected_file_root = None;
							let first_path = first.path.clone();
							model.select_file(&first_path, cx);
						} else if model.mode == "preview" {
							ready_marker("PREVIEW");
						}
						model.sync_list_row();
					}
					Err(err) => {
						app_log!("[APP:REPO_ERROR: {}]", repo_name);
						let slot = model.ensure_change_slot(
							&repo_root_for_update,
							&repo_name,
						);
						model.install_slot_changes(slot, Err(err.clone()));
						model.set_status_msg(Msg::with_msg_arg(
							"error_repo_changes",
							[repo_name.clone(), String::new()],
							1,
							err.clone(),
						));
						if let (true, Some(anchor)) = (
							anchor_source.as_ref() == Some(&SourceKind::File),
							anchor_file.as_ref(),
						) {
							let root = model
								.selected_file_root
								.clone()
								.or_else(|| {
									model
										.remote
										.session
										.as_ref()
										.map(|s| s.root.clone())
								})
								.or_else(|| model.repo_root());
							model.select_file_in(
								root,
								anchor,
								SourceKind::File,
								cx,
							);
						} else {
							model.show_preview_error(Msg::with_msg_arg(
								"error_repo_changes",
								[repo_name.clone(), String::new()],
								1,
								err,
							));
						}
					}
				}
				if reload_log {
					model.log_reload_for = None;
					model.load_history(cx);
				} else {
					model.log_kept_on_repo_switch();
				}
				cx.notify();
			});
		});
	}

	/// Selects a file of the open repo, using its known source identity if
	/// it is in that repo's Changes rows.
	pub fn select_file(&mut self, path: &str, cx: &mut Context<Self>) {
		let rows = self
			.selected_change_slot()
			.map_or(0..0, |slot| slot_range(&self.files, slot));
		let source = self.files[rows]
			.iter()
			.find(|f| f.path == path)
			.map(|f| f.source.clone())
			.unwrap_or(SourceKind::Working);
		self.select_file_with_source(path, source, cx);
	}

	/// Opens Changes row `idx` from its own repo, whichever repo is open.
	pub fn select_change(&mut self, idx: usize, cx: &mut Context<Self>) {
		let Some(item) = self.files.get(idx) else {
			return;
		};
		let root = self
			.change_repos
			.get(item.repo as usize)
			.map(|slot| slot.root.clone());
		let (path, source) = (item.path.clone(), item.source.clone());
		self.select_file_in(root, &path, source, cx);
	}

	/// Opens a working-tree file (Project) or its staged/unstaged changes (Changes).
	pub fn select_file_with_source(
		&mut self,
		path: &str,
		source: SourceKind,
		cx: &mut Context<Self>,
	) {
		self.select_file_in(None, path, source, cx);
	}

	/// Opens `path` of repo `root` (the open repo when `None`).
	pub fn select_file_in(
		&mut self,
		root: Option<PathBuf>,
		path: &str,
		source: SourceKind,
		cx: &mut Context<Self>,
	) {
		self.user_action();
		if !self.accepting_work() {
			return;
		}
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		let Some(repo_root) = root.or_else(|| self.repo_root()) else {
			return;
		};
		self.hold_shown_selection(path, &source);
		let fs_only = matches!(source, SourceKind::File);
		if fs_only {
			self.selected_file_root = Some(repo_root.clone());
		} else {
			self.selected_file_root = None;
		}
		let shown_root = repo_root.clone();
		let file_path = path.to_string();
		self.preview_loading = true;
		self.preview_pending_from_log = false;
		self.preview_error = None;
		self.preview_error_root = None;

		let cancel = arm_cancel(&mut self.preview_cancel);
		let remote = self.remote_target();
		// A local working-file read polls no token (`browser::file_preview`);
		// every other read does, the remote one included (#191).
		let cancellable = remote.is_some() || !fs_only;
		let job_cancel = cancellable.then(|| cancel.clone());
		let delay = self.e2e_read_delay;
		let host = self.git_host();
		if e2e_on() {
			app_log!("[APP:PREVIEW_LOADING: {file_path}]");
		}

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let hold = bg.clone();

		self.spawn_owned(cx, job_cancel, async move {
			let for_bg = file_path.clone();
			let result = bg
				.spawn(async move {
					match remote {
						Some((client, ws, session_root)) if fs_only => {
							match remote::remote_rel(&session_root, &repo_root)
							{
								Some(prefix) => remote::read_preview(
									&client,
									&ws,
									&prefix,
									&for_bg,
									Some(&cancel),
								),
								None => Err(PreviewErr::Failed(Msg::new(
									"err_not_under_workspace",
									[],
								))),
							}
						}
						_ => read_preview(
							&host, &repo_root, &for_bg, &source, cancel,
						),
					}
				})
				.await;
			// The executor's timer, not a thread sleep: a test's clock holds
			// the finished read without blocking its gestures.
			if let Some(delay) = delay {
				hold.timer(delay).await;
			}

			let _ = this.update(&mut async_app, |model, cx| {
				if model.preview_generation != task_generation {
					if e2e_on() {
						app_log!(
							"[APP:PREVIEW_DISCARDED: stale path={file_path}]"
						);
					}
					return;
				}
				if model.apply_source_preview(
					file_path.clone(),
					shown_root.clone(),
					result,
				) {
					model.held_selection = None;
					app_log!("[APP:PREVIEW_LOADED: {}]", file_path);
					if model.mode == "preview" {
						ready_marker("PREVIEW");
					}
				} else if model.preview.is_some()
					&& model.preview_error.is_none()
				{
					// Refused by the retained budget: the previous text is
					// still shown, so the selection must name it again.
					model.restore_shown_selection();
				} else {
					model.preview_error_root = Some(shown_root);
				}
				cx.notify();
			});
		});
	}

	/// Names `path` as the selection and sets aside what the shown preview's
	/// selection was, unless an earlier read already did: its snapshot still
	/// describes the shown preview, while what is on the model now is only
	/// what that read left behind.
	fn hold_shown_selection(&mut self, path: &str, source: &SourceKind) {
		let file = self.selected_file.replace(path.to_string());
		let file_source = self.selected_file_source.replace(source.clone());
		let shown = self.preview.is_some() && self.preview_error.is_none();
		// A commit file read left the commit state on the model: this read
		// takes it, and the snapshot keeps that read's file selection.
		let partial = match self.held_selection.take() {
			Some(h) if h.commit_file_only.is_some() => Some(h),
			other => {
				self.held_selection = other;
				None
			}
		};
		if self.held_selection.is_some() {
			// The earlier read took the rest already.
			return;
		}
		let mut held = ShownSelection {
			file,
			source: file_source,
			root: self.selected_file_root.clone(),
			commit: self.selected_commit.take(),
			// Taking the view resets it.
			view: std::mem::take(&mut self.commit_view),
			log_selected: std::mem::take(&mut self.log_selected),
			range_head: self.range_head.take(),
			compare: self.compare.take(),
			log_selection_expanded: std::mem::take(
				&mut self.log_selection_expanded,
			),
			log_branches_all: std::mem::take(&mut self.log_branches_all),
			commit_file_only: None,
		};
		if let Some(early) = partial {
			held.file = early.file;
			held.source = early.source;
			held.root = early.root;
			held.view.selected_commit_file = early.view.selected_commit_file;
			held.view.commit_file_sel = early.view.commit_file_sel;
		}
		self.held_selection = shown.then_some(held);
	}

	/// Names `path` as the commit view's file and sets aside what the shown
	/// preview's file selection was, unless an earlier read already did.
	/// `clear_file` is for a read that leaves the working-file selection
	/// (a commit tree's file). The commit, its list and its details stay: a
	/// refusal puts the file selection back, a landed read releases it.
	pub(crate) fn hold_shown_commit_file(
		&mut self,
		path: &str,
		clear_file: bool,
	) {
		let shown = self.preview.is_some() && self.preview_error.is_none();
		if shown && self.held_selection.is_none() {
			self.held_selection = Some(ShownSelection {
				file: self.selected_file.clone(),
				source: self.selected_file_source.clone(),
				root: self.selected_file_root.clone(),
				commit: None,
				view: crate::history::CommitView {
					selected_commit_file: self
						.commit_view
						.selected_commit_file
						.clone(),
					commit_file_sel: self.commit_view.commit_file_sel.clone(),
					..Default::default()
				},
				log_selected: Vec::new(),
				range_head: None,
				compare: None,
				log_selection_expanded: false,
				log_branches_all: Vec::new(),
				commit_file_only: Some(self.preview_pending_from_log),
			});
		}
		if clear_file {
			self.selected_file = None;
			self.selected_file_root = None;
		}
		self.commit_view.selected_commit_file = Some(path.to_string());
		self.commit_view.commit_file_sel.clear();
	}

	/// Puts the held selection back; the shown preview is still its own.
	fn restore_shown_selection(&mut self) {
		let Some(held) = self.held_selection.take() else {
			return;
		};
		self.selected_file = held.file;
		self.selected_file_source = held.source;
		self.selected_file_root = held.root;
		if let Some(pending) = held.commit_file_only {
			self.commit_view.selected_commit_file =
				held.view.selected_commit_file;
			self.commit_view.commit_file_sel = held.view.commit_file_sel;
			self.preview_pending_from_log = pending;
			return;
		}
		self.selected_commit = held.commit;
		self.commit_view = held.view;
		self.log_selected = held.log_selected;
		self.range_head = held.range_head;
		self.compare = held.compare;
		self.log_selection_expanded = held.log_selection_expanded;
		self.log_branches_all = held.log_branches_all;
	}

	/// Turns a core `SourcePreview` into the single retained preview.
	pub fn apply_source_preview(
		&mut self,
		path: String,
		root: PathBuf,
		result: Result<(browser::SourcePreview, PreviewSource), PreviewErr>,
	) -> bool {
		match result {
			Ok((p, source)) => {
				if !p.patch.is_empty() {
					self.set_preview(
						Preview::new(
							source,
							Some(path),
							p.patch,
							true,
							Language::Diff,
						)
						.with_root(Some(root)),
					)
				} else if let Some(content) = p.content {
					let lang = Language::from_path_or_ext(&path, false);
					self.set_preview(
						Preview::new(source, Some(path), content, false, lang)
							.with_root(Some(root)),
					)
				} else {
					self.show_preview_error(Msg::new("error_binary", [path]));
					false
				}
			}
			Err(e) => {
				self.show_preview_error(e.msg(path));
				false
			}
		}
	}

	/// Keeps both trees' selections for when their roots come back.
	fn keep_tree_selections(&mut self) {
		for tree in [&self.file_tree, &self.ws_tree].into_iter().flatten() {
			let root = &tree.full_path;
			self.tree_selections.retain(|(have, _)| have != root);
			if !tree.selected_paths().is_empty() {
				self.tree_selections
					.push((root.clone(), tree.selected_paths().to_vec()));
			}
		}
	}

	fn kept_tree_selection(&self, root: &std::path::Path) -> Vec<String> {
		self.tree_selections
			.iter()
			.find(|(have, _)| have == root)
			.map(|(_, paths)| paths.clone())
			.unwrap_or_default()
	}

	/// Highlights exactly `paths` in the repo tree, or the workspace tree
	/// when `ws`; callers log the result once with `log_tree_selection`.
	fn install_tree_selection(&mut self, ws: bool, paths: Vec<String>) {
		let tree = if ws {
			&mut self.ws_tree
		} else {
			&mut self.file_tree
		};
		if let Some(tree) = tree {
			tree.install_selection(paths);
		}
	}

	fn log_tree_selection(&self) {
		if e2e_on() {
			// The highlighted Project rows, exactly.
			let rows = |tree: &Option<FileTreeNode>| {
				tree.as_ref()
					.map_or(String::new(), |t| t.selected_paths().join(","))
			};
			app_log!(
				"[APP:TREE_SELECTION: file=[{}] ws=[{}]]",
				rows(&self.file_tree),
				rows(&self.ws_tree)
			);
		}
	}

	/// The Copy of a node: its targets as one snip-sync payload.
	pub fn copy_targets(
		&mut self,
		targets: Vec<menu::CopyTarget>,
		cx: &mut Context<Self>,
	) {
		let name = targets
			.first()
			.map(|target| self.log_repo_name(&target.root))
			.unwrap_or_default();
		if let Some(session) = self.remote.session.clone() {
			return self.copy_remote_targets(session, targets, name, cx);
		}
		let items: Option<Vec<ExportItem>> = targets
			.into_iter()
			.map(|target| {
				Some(ExportItem {
					root: CanonicalRootId::new(&target.root).ok()?,
					relative_path: target.path,
					source: target.source,
					change_type: target.change_type,
					gitlink: false,
				})
			})
			.collect();
		match items {
			Some(items) => self.export_items_to_clipboard(items, name, cx),
			None => {
				self.set_status("error_selection_root", []);
				cx.notify();
			}
		}
	}

	/// Exports `items` (any roots) as one snip-sync payload to the
	/// clipboard: every node's Copy.
	pub fn export_items_to_clipboard(
		&mut self,
		items: Vec<ExportItem>,
		repo_name: String,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		if self.remote_blocks() {
			cx.notify();
			return;
		}
		if self.is_copying {
			app_log!("[APP:COPY_BUSY]");
			return;
		}
		let mut roots = Vec::new();
		for item in &items {
			let path = item.root.path().to_path_buf();
			if !roots.iter().any(|root| root == &path) {
				roots.push(path);
			}
		}
		if items.is_empty() {
			app_log!("[APP:COPY_REFUSED: empty_selection]");
			self.set_status("status_copy_empty", []);
			cx.notify();
			return;
		}
		let repo_root = self
			.repo_root()
			.filter(|root| roots.iter().any(|have| have == root))
			.unwrap_or_else(|| roots[0].clone());

		let export_sel =
			match ExportSelection::new(roots, Some(repo_root.clone()), items) {
				Ok(s) => s,
				Err(e) => {
					self.set_status_msg(Msg::with_msg_arg(
						"error_payload",
						[String::new()],
						0,
						crate::reason::reason_msg(&e.reason()),
					));
					cx.notify();
					return;
				}
			};

		self.is_copying = true;
		self.set_status("status_copying", [repo_name.clone()]);
		if e2e_on() {
			app_log!("[APP:COPY_PREP: files={}]", export_sel.items.len());
		}
		cx.notify();

		let cancel = arm_cancel(&mut self.copy_cancel);
		let job_token = cancel.clone();
		let run_token = cancel.clone();
		let export_hold = self.e2e_export_hold.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let ws_gen = self.lifecycle.generation();

		self.spawn_owned(
			cx,
			Some(job_token),
			async move {
					let result: Result<(String, usize, Msg), Msg> = bg
					.spawn(async move {
						let opts = interactive_read_opts(run_token.clone());
						let settings = native_export_settings();
						// The same engine a remote copy and the CLI run,
						// batches doubling past what a filter excludes.
						let report = copy_selection_detailed(
							export_sel,
							&settings,
							NATIVE_FILE_COUNT_LIMIT,
							&opts,
							|plan| {
								if let Some(ref hold) = export_hold {
									if hold.exists() {
										app_log!(
											"[APP:EXPORT_PLAN_READY: files={}]",
											plan.files.len()
										);
										while hold.exists() {
											if run_token.is_cancelled() {
												break;
											}
											std::thread::sleep(
												std::time::Duration::from_millis(20),
											);
										}
									}
								}
							},
						)
						.map_err(|e| {
							let reason = match &e {
								// Every file under the folders was skipped.
								snip_core::transfer::TransferError::EmptySelection => {
									return Msg::new(
										"status_copy_nothing_skipped",
										[],
									)
								}
								snip_core::transfer::TransferError::StaleSource { .. } => "stale_source",
								_ => "revalidate",
							};
							if e2e_on() {
								app_log!("[APP:COPY_FAILED: {reason}]");
							}
							Msg::with_msg_arg(
								"error_payload",
								[String::new()],
								0,
								crate::reason::reason_msg(&e.reason()),
							)
						})?;
						if report.outcome.copied == 0 {
							return Err(Msg::new("status_copy_nothing", []));
						}
						let msg = copied_status(repo_name, &report.outcome);
						Ok((
							report.outcome.payload,
							report.outcome.copied,
							msg,
						))
					})
					.await;

				if let Err(err) = this.update(&mut async_app, |model, cx| {
					model.finish_copy(ws_gen, &cancel, result, cx)
				}) {
					app_log!("[APP:COPY_IDLE_FAILED: {err}]");
				}
			},
		);
	}

	/// Writes a finished file copy (local or remote) to the clipboard and
	/// reports it.
	fn finish_copy(
		&mut self,
		ws_gen: u64,
		cancel: &CancelToken,
		result: Result<(String, usize, Msg), Msg>,
		cx: &mut Context<Self>,
	) {
		if !self.accept_copy_result(ws_gen, cancel) {
			cx.notify();
			return;
		}
		match result {
			Ok((text, copied_count, msg)) => {
				if let Err(e) = clip::write_text(&text) {
					self.set_status_msg(crate::reason::clipboard_failed(&e));
				} else {
					app_log!("[APP:COPY_DONE: copied={copied_count}]");
					self.set_status_msg(msg);
				}
			}
			Err(err) => self.set_status_msg(err),
		}
		let ok =
			matches!(self.status.key, "status_copied" | "status_copied_limit");
		self.show_toast(ok, self.status.clone(), cx);
		app_log!("[APP:COPY_IDLE]");
		cx.notify();
	}

	/// A remote workspace's Copy: the worker runs the same copy engine on
	/// its files and sends the payload back.
	fn copy_remote_targets(
		&mut self,
		session: crate::remote::RemoteSession,
		targets: Vec<menu::CopyTarget>,
		repo_name: String,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		if self.is_copying {
			app_log!("[APP:COPY_BUSY]");
			return;
		}
		if targets.is_empty() {
			app_log!("[APP:COPY_REFUSED: empty_selection]");
			self.set_status("status_copy_empty", []);
			cx.notify();
			return;
		}
		let items: Option<Vec<snip_remote::proto::ExportTarget>> = targets
			.into_iter()
			.map(|target| {
				Some(snip_remote::proto::ExportTarget {
					root: crate::remote::remote_rel(
						&session.root,
						&target.root,
					)?,
					path: target.path,
					source: target.source,
					change_type: target.change_type,
				})
			})
			.collect();
		let Some(items) = items else {
			self.set_status("error_selection_root", []);
			cx.notify();
			return;
		};

		self.is_copying = true;
		self.set_status("status_copying", [repo_name.clone()]);
		if e2e_on() {
			app_log!("[APP:COPY_PREP: files={}]", items.len());
		}
		cx.notify();

		let cancel = arm_cancel(&mut self.copy_cancel);
		let job_token = cancel.clone();
		let run_token = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let ws_gen = self.lifecycle.generation();
		#[cfg(test)]
		let copy_hold = self.e2e_copy_hold;
		self.spawn_owned(cx, Some(job_token), async move {
			let result: Result<(String, usize, Msg), Msg> = bg
				.spawn(async move {
					let out = session
						.client
						.export_files(
							&session.workspace.id,
							items,
							&native_export_settings(),
							NATIVE_FILE_COUNT_LIMIT,
							Some(&run_token),
						)
						.map_err(|e| {
							Msg::with_msg_arg(
								"error_payload",
								[String::new()],
								0,
								crate::remote::describe_msg(e),
							)
						})?;
					if out.copied == 0 {
						return Err(remote_copy_nothing_status(&out));
					}
					let msg = copied_status(repo_name, &out);
					Ok((out.payload, out.copied, msg))
				})
				.await;
			#[cfg(test)]
			if let Some(hold) = copy_hold {
				bg.timer(hold).await;
			}
			if let Err(err) = this.update(&mut async_app, |model, cx| {
				model.finish_copy(ws_gen, &cancel, result, cx)
			}) {
				app_log!("[APP:COPY_IDLE_FAILED: {err}]");
			}
		});
	}

	/// Exports the selected commit or first-parent commit range to the clipboard.
	pub fn copy_commits_to_clipboard(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		if self.is_copying {
			self.set_status("status_copying", []);
			cx.notify();
			return;
		}
		// The commits' own repository, not the selected one: the log shows
		// every repository of the workspace.
		let (repo_root, repo_name, tip_sha, selected) =
			match self.commit_copy_target() {
				Ok(target) => target,
				Err(reason) => {
					app_log!("[APP:COPY_COMMITS_REFUSED: {reason}]");
					self.set_status(
						if reason == "cross_repo" {
							"status_log_cross_repo"
						} else {
							"status_copy_empty"
						},
						[],
					);
					cx.notify();
					return;
				}
			};

		// A remote repo is named by its path under the workspace.
		let remote = match self.remote.session.clone() {
			Some(session) => {
				match crate::remote::remote_rel(&session.root, &repo_root) {
					Some(repo) => Some((session, repo)),
					None => {
						self.set_status("error_selection_root", []);
						cx.notify();
						return;
					}
				}
			}
			None => None,
		};

		self.is_copying = true;
		self.set_status("status_copying", [repo_name.clone()]);
		if e2e_on() {
			app_log!("[APP:COPY_COMMITS_PREP: commits={}]", selected.len());
		}
		cx.notify();

		let cancel = arm_cancel(&mut self.copy_cancel);
		let job_token = cancel.clone();
		let run_token = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let ws_gen = self.lifecycle.generation();
		#[cfg(test)]
		let copy_hold = self.e2e_copy_hold;

		self.spawn_owned(cx, Some(job_token), async move {
			// The error carries the engine's own text for the e2e log
			// line (a machine contract, never the UI's wording) and the
			// message the UI words.
			let result: Result<(String, usize, Msg), CopyCommitsFailure> = bg
				.spawn(async move {
					if let Some((session, repo)) = remote {
						let out = session
							.client
							.export_commits(
								&session.workspace.id,
								&repo,
								&tip_sha,
								selected,
								Some(&run_token),
							)
							.map_err(|e| {
								// ALLOWED-ERR-TEXT: COPY_COMMITS_ERR prints the engine's Display (an e2e contract)
								(e.to_string(), crate::remote::describe_msg(e))
							})?;
						let status = commit_copied_status(&out);
						return Ok((out.text, out.commit_count, status));
					}
					export_commits_local(
						&repo_root, &tip_sha, &selected, run_token,
					)
				})
				.await;
			#[cfg(test)]
			if let Some(hold) = copy_hold {
				bg.timer(hold).await;
			}

			let _ = this.update(&mut async_app, |model, cx| {
				if !model.accept_copy_result(ws_gen, &cancel) {
					cx.notify();
					return;
				}
				match result {
					Ok((text, n_commits, status)) => {
						if let Err(e) = clip::write_text(&text) {
							model.set_status_msg(
								crate::reason::clipboard_failed(&e),
							);
						} else {
							app_log!(
								"[APP:COPY_COMMITS_DONE: commits={n_commits}]"
							);
							model.status = status;
						}
					}
					Err((log, err)) => {
						app_log!("{}", copy_commits_err_line(&log));
						model.set_status_msg(Msg::with_msg_arg(
							"error_payload",
							[String::new()],
							0,
							err,
						));
					}
				}
				let ok = matches!(
					model.status.key,
					"status_commits_copied" | "status_commits_copied_skipped"
				);
				model.show_toast(ok, model.status.clone(), cx);
				cx.notify();
			});
		});
	}

	/// True while a confirmed plan is being written. The write is not
	/// cancellable, so every control that would change or discard the plan
	/// is refused until it finishes.
	pub fn paste_busy(&self) -> bool {
		self.paste.is_applying()
	}

	fn refuse_while_applying(
		&mut self,
		action: &str,
		cx: &mut Context<Self>,
	) -> bool {
		if !self.paste_busy() {
			return false;
		}
		app_log!("[APP:PASTE_BUSY: refused={action}]");
		self.set_status("paste_busy_refused", []);
		cx.notify();
		true
	}

	fn queue_paste(
		&mut self,
		request: paste::PasteRequest,
		cx: &mut Context<Self>,
	) {
		let retarget = matches!(request, paste::PasteRequest::Retarget { .. });
		if let Err(err) = self.paste.enqueue(request) {
			// A remap's plan is idle again, without its writes.
			self.refresh_paste_candidates(cx);
			if retarget {
				// The disarmed shell stays open on the folder the user
				// picked: it says why there is nothing to apply.
				self.set_paste_error(err);
			} else {
				self.set_status_msg(err);
			}
			app_log!("[APP:PASTE_ERR: preview_memory_limit]");
			self.restore_log_after_paste();
			cx.notify();
			return;
		}
		self.set_status("paste_loading", []);
		app_log!("[APP:PASTE_LOADING]");
		self.poll_paste(cx);
		self.arm_watch(cx);
		cx.notify();
	}

	/// Runs before the existing watch decides it can exit. A cancelled job
	/// still occupies this slot until its actual FinishFlag has dropped.
	fn poll_paste(&mut self, cx: &mut Context<Self>) {
		let live = self
			.paste
			.worker_id()
			.is_some_and(|id| self.lifecycle.is_live(id));
		let polled = self.paste.poll(live, self.preview.as_ref());
		let paste::preview::Polled::Settled { discarded, landed } = polled
		else {
			return;
		};
		if discarded {
			app_log!("[APP:PASTE_DISCARDED: cancelled]");
		}
		// Whatever landed, the preview is idle again (or closed): bring
		// its candidates up to date with the repos added while it was busy.
		// Before the plan is shown, so its candidates are logged as they are.
		if landed.is_some() {
			self.refresh_paste_candidates(cx);
		}
		match landed {
			Some(paste::preview::Landed::Shown { remap, retargeted }) => {
				self.show_landed_plan(remap, retargeted, cx);
				// A folder under the cursor has no file to read.
				let on_item =
					self.paste.plan().and_then(|p| match p.cursor() {
						paste::PasteCursor::Item(ix) => Some(ix),
						paste::PasteCursor::Dir(_) => None,
					});
				match on_item {
					Some(ix) => self.load_paste_diff(ix, cx),
					None => self.cancel_paste_diff(),
				}
				cx.notify();
			}
			Some(paste::preview::Landed::Refused { err, closed }) => {
				app_log!("[APP:PASTE_ERR: {}]", err.key);
				self.set_status_msg(err);
				if closed {
					self.restore_log_after_paste();
					self.pending_focus = Some(self.focus_handle.clone());
				}
				cx.notify();
			}
			Some(paste::preview::Landed::WorkerLost) => {
				// The worker thread ended without an answer: an internal
				// failure the user cannot cause.
				self.set_status_msg(Msg::with_msg_arg(
					"paste_err_plan",
					[String::new()],
					0,
					crate::reason::reason_msg(&Reason::Internal {
						detail: snip_core::reason::Datum::default(),
						condition: None,
					}),
				));
				cx.notify();
			}
			None => {}
		}
		if !self.accepting_work() {
			return;
		}
		match self.paste.start_job() {
			Ok(Some((work, cancel))) => {
				let bg = cx.background_executor().clone();
				let token = cancel.clone();
				let id =
					self.spawn_owned(cx, Some(cancel.clone()), async move {
						bg.spawn(async move {
							work.run(&interactive_read_opts(token));
						})
						.await;
					});
				self.paste.bind_job(id, cancel);
			}
			Ok(None) => {}
			Err(err) => {
				self.set_paste_error(err);
				app_log!("[APP:PASTE_ERR: preview_memory_limit]");
				cx.notify();
			}
		}
	}

	fn show_landed_plan(
		&mut self,
		remap: Option<paste::Remap>,
		retargeted: bool,
		cx: &mut Context<Self>,
	) {
		// `Landed::Shown` guarantees an installed plan (see its doc).
		let plan = self.paste.plan().expect("Shown installs a plan");
		let items_count = plan.items.len();
		if let Some(paste::Remap::Prefix { prefix, keep }) = &remap {
			let dest = prefix_target(plan, prefix, *keep);
			let keep_note = if *keep { " keep=primary" } else { "" };
			app_log!(
				"[APP:PASTE_MAPPED: prefix={}{} dest={} items={}]",
				prefix,
				keep_note,
				dest,
				items_count
			);
		} else if let Some(paste::Remap::AdjustPaths(on)) = &remap {
			app_log!("[APP:PASTE_ADJUSTED: on={} items={}]", on, items_count);
		} else {
			for choice in &plan.prefix_choices {
				for (idx, path) in choice.candidates.iter().enumerate() {
					app_log!(
						"[APP:PASTE_MAP_CANDIDATE: prefix={} idx={} path={}]",
						choice.prefix,
						idx,
						plan.shown(path)
					);
				}
			}
			if retargeted {
				app_log!(
					"[APP:PASTE_RETARGETED: items={} dest={} executable={} error={}]",
					items_count,
					plan.shown(&plan.destination),
					plan.executable(),
					plan.error.as_ref().map_or("-", |e| e.key)
				);
			} else {
				app_log!(
					"[APP:PASTE_PREVIEW: items={} dest={} mapping={}]",
					items_count,
					plan.shown(&plan.destination),
					plan.mapping_ready()
				);
			}
			// A retarget is a new plan for a new folder: the hint it
			// shows is what that folder's engine suggested.
			if let Some(hint) = plan.restore_hint() {
				let (kind, value) = match &hint.base {
					snip_core::restore::RestoreBase::Strip { segment } => {
						("strip", segment)
					}
					snip_core::restore::RestoreBase::Add { prefix } => {
						("add", prefix)
					}
				};
				app_log!(
					"[APP:PASTE_RESTORE_BASE: kind={} value={}]",
					kind,
					value
				);
			}
		}
		// A folder the engine refused is said at once, in its own words; a
		// mapping still to choose is not a refusal.
		let refusal = plan.error.clone().filter(|_| {
			retargeted && plan.mapping_ready() && !plan.executable()
		});
		match refusal {
			Some(err) => self.set_status_msg(err),
			None => self
				.set_status("status_paste_preview", [items_count.to_string()]),
		}
		if self.paste.collapse_log(&mut self.bottom_visible) {
			app_log!("[APP:LOG_PANEL: visible=false reason=paste_open]");
		}
		// A plan landing under the open destination field (a prefix chosen
		// with the pointer while typing) leaves the focus where the user
		// is typing: the panel's Enter would apply into the old folder.
		self.pending_focus = Some(if self.paste_dest_edit().is_some() {
			self.paste_dest_input.read(cx).handle()
		} else {
			self.paste_focus.clone()
		});
	}

	/// Keep full write/read diagnostics in status, but do not let a newly
	/// allocated diagnostic grow an already admitted plan past its tier.
	fn set_paste_error(&mut self, err: Msg) {
		self.set_status_msg(err.clone());
		self.paste.record_error(err, self.preview.as_ref());
	}

	pub fn trigger_paste_preview(&mut self, cx: &mut Context<Self>) {
		self.trigger_paste_preview_into(None, cx);
	}

	/// Opens the paste preview from the clipboard: into `folder`, or, when
	/// none is named, the default destination (`--restore-dir`, else the
	/// pinned repo, else the workspace folder).
	pub fn trigger_paste_preview_into(
		&mut self,
		folder: Option<paste::dest::DestFolder>,
		cx: &mut Context<Self>,
	) {
		self.user_action();
		if self.refuse_while_applying("preview", cx) {
			return;
		}
		if !self.workspace_open {
			self.set_status("workspace_not_open", []);
			cx.notify();
			return;
		}
		if !self.accepting_work() {
			return;
		}
		// A new paste always invalidates the previous plan first, so a failed
		// read/parse can never leave an older plan armed for Apply.
		self.paste.invalidate_job();
		if self.paste.plan().is_some() {
			app_log!("[APP:PASTE_PLAN_CLEARED]");
		}
		self.paste.clear(self.preview.as_ref());
		self.drop_paste_dest_edit();
		let text = match clip::read_text() {
			Ok(t) => t,
			Err(e) => {
				app_log!("[APP:PASTE_ERR: clipboard]");
				self.restore_log_after_paste();
				self.set_status_msg(crate::reason::clipboard_read_failed(&e));
				self.pending_focus = Some(self.focus_handle.clone());
				cx.notify();
				return;
			}
		};
		if text.len() > paste::MAX_RETAINED_PREVIEW_BYTES {
			self.set_status_msg(paste::preview_budget_error());
			self.restore_log_after_paste();
			self.pending_focus = Some(self.focus_handle.clone());
			app_log!("[APP:PASTE_ERR: preview_memory_limit]");
			cx.notify();
			return;
		}

		let folder = folder.unwrap_or_else(|| {
			self.dest_folder(self.current_restore_destination())
		});
		let target_dest = folder.path.clone();
		let known_roots = self.repos.roots();
		self.pending_focus = Some(self.paste_focus.clone());
		// After the early returns above (a failed read leaves no folder
		// behind) and after `clear`, which resets it.
		self.paste.set_folder(folder);
		self.queue_paste(
			paste::PasteRequest::Clipboard {
				text,
				dest: target_dest,
				roots: known_roots,
				generation: self.generation,
				remote: self
					.remote
					.session
					.clone()
					.map(|session| paste::RemotePaste { session }),
			},
			cx,
		);
	}

	pub fn choose_paste_keep(&mut self, prefix: &str, cx: &mut Context<Self>) {
		if self.refuse_while_applying("mapping", cx) {
			return;
		}
		self.rebuild_paste_plan(prefix.to_string(), None, cx);
	}

	/// The restore-base hint's [Adjust paths] (`true`) or [Use as-is]. The
	/// choice re-plans like a prefix row does, and the focus comes back to
	/// the panel once the new plan lands, so the next Enter applies it.
	pub fn choose_paste_adjust(&mut self, on: bool, cx: &mut Context<Self>) {
		if self.refuse_while_applying("mapping", cx) || !self.accepting_work() {
			return;
		}
		let r = self.paste.adjust_paths(on, self.preview.as_ref());
		self.queue_remapped(r, cx);
	}

	pub fn choose_paste_prefix(
		&mut self,
		prefix: &str,
		candidate_idx: usize,
		cx: &mut Context<Self>,
	) {
		if self.refuse_while_applying("mapping", cx) {
			return;
		}
		let Some(dest) = self.paste.plan().and_then(|plan| {
			plan.prefix_choices
				.iter()
				.find(|c| c.prefix == prefix)
				.and_then(|c| c.candidates.get(candidate_idx).cloned())
		}) else {
			return;
		};
		self.rebuild_paste_plan(prefix.to_string(), Some(dest), cx);
	}

	/// Replans the writes for the mapping just chosen. The choice is already
	/// on the visible plan and its old writes are dropped here, so neither a
	/// second choice nor Apply can act on the previous mapping.
	fn rebuild_paste_plan(
		&mut self,
		prefix: String,
		destination: Option<PathBuf>,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let r = self.paste.remap(prefix, destination, self.preview.as_ref());
		self.queue_remapped(r, cx);
	}

	fn queue_remapped(
		&mut self,
		remapped: paste::preview::Remapped,
		cx: &mut Context<Self>,
	) {
		match remapped {
			paste::preview::Remapped::NoPlan => {}
			paste::preview::Remapped::Invalid(err) => {
				// Idle again with its old mapping.
				self.refresh_paste_candidates(cx);
				self.set_status_msg(err);
				cx.notify();
			}
			paste::preview::Remapped::Dropped(err) => {
				self.set_status_msg(err);
				self.restore_log_after_paste();
				cx.notify();
			}
			paste::preview::Remapped::Ready(req) => self.queue_paste(req, cx),
		}
	}

	pub fn toggle_paste_overwrite(
		&mut self,
		idx: usize,
		cx: &mut Context<Self>,
	) {
		if self.refuse_while_applying("overwrite", cx)
			|| self.paste.is_loading()
		{
			return;
		}
		if let Some(st) = self.paste.toggle_overwrite(idx) {
			app_log!("[APP:PASTE_TOGGLED: idx={} state={}]", idx, st);
			cx.notify();
		}
	}

	pub fn toggle_paste_selected(
		&mut self,
		idx: usize,
		cx: &mut Context<Self>,
	) {
		if self.refuse_while_applying("include", cx) || self.paste.is_loading()
		{
			return;
		}
		if let Some(st) = self.paste.toggle_selected(idx) {
			app_log!("[APP:PASTE_SEL_TOGGLED: idx={} state={}]", idx, st);
			if let Some(plan) = self.paste.plan() {
				if plan.whole_commit
					&& plan.all_selected()
					&& self.status.key == "commit_subset_rejected"
				{
					self.set_status(
						"status_paste_preview",
						[plan.items.len().to_string()],
					);
				}
			}
			cx.notify();
		}
	}

	fn report_paste_nav(
		&mut self,
		nav: paste::preview::Nav,
		cx: &mut Context<Self>,
	) {
		match nav {
			paste::preview::Nav::Ignored => {}
			paste::preview::Nav::Moved(idx) => {
				app_log!("[APP:PASTE_NAV: idx={}]", idx);
				cx.notify();
			}
			paste::preview::Nav::MovedDir => {
				// A folder has no file to read; one still loading is dropped.
				self.cancel_paste_diff();
				if let Some(dir) = self.paste.plan().and_then(|p| {
					let key = p.cursor_dir.as_ref()?;
					Some(p.dir_label(key))
				}) {
					app_log!("[APP:PASTE_NAV_DIR: dir={}]", dir);
				}
				cx.notify();
			}
			paste::preview::Nav::Refused(err) => {
				self.set_status_msg(err);
				app_log!("[APP:PASTE_DETAIL_REFUSED: reason=retained_budget]");
				cx.notify();
			}
		}
	}

	pub fn select_paste_item(&mut self, idx: usize, cx: &mut Context<Self>) {
		let nav = self.paste.select(idx, self.preview.as_ref());
		let moved = matches!(nav, paste::preview::Nav::Moved(_));
		self.report_paste_nav(nav, cx);
		if moved {
			self.load_paste_diff(idx, cx);
		}
	}

	pub fn step_paste_selection(
		&mut self,
		forward: bool,
		cx: &mut Context<Self>,
	) {
		let nav = self.paste.step(forward, self.preview.as_ref());
		let moved = match nav {
			paste::preview::Nav::Moved(idx) => Some(idx),
			_ => None,
		};
		self.report_paste_nav(nav, cx);
		if let Some(idx) = moved {
			self.load_paste_diff(idx, cx);
		}
	}

	pub fn toggle_paste_commit(&mut self, c: usize, cx: &mut Context<Self>) {
		match self.paste.toggle_fold(c, self.preview.as_ref()) {
			paste::preview::Folded::NoPlan
			| paste::preview::Folded::Ignored => {}
			paste::preview::Folded::Kept => {
				app_log!("[APP:PASTE_COMMIT_TOGGLED: idx={}]", c);
				cx.notify();
			}
			paste::preview::Folded::Reselected(nav) => {
				app_log!("[APP:PASTE_COMMIT_TOGGLED: idx={}]", c);
				let moved = match nav {
					paste::preview::Nav::Moved(idx) => Some(idx),
					_ => None,
				};
				self.report_paste_nav(nav, cx);
				if let Some(idx) = moved {
					self.load_paste_diff(idx, cx);
				}
				cx.notify();
			}
		}
	}

	/// Drops the diff read in flight: its answer is for a row the cursor has
	/// left.
	fn cancel_paste_diff(&mut self) {
		if let Some(token) = self.paste_diff_cancel.take() {
			token.cancel();
		}
		self.paste_diff_generation = self.paste_diff_generation.wrapping_add(1);
	}

	/// A click on a folder or root row puts the cursor on it.
	pub fn select_paste_dir(
		&mut self,
		key: paste::DirKey,
		cx: &mut Context<Self>,
	) {
		let nav = self.paste.select_dir(key, self.preview.as_ref());
		self.report_paste_nav(nav, cx);
	}

	/// Folds or unfolds a folder; a fold that hides the cursor moves it
	/// onto the folder.
	pub fn toggle_paste_dir(
		&mut self,
		key: paste::DirKey,
		cx: &mut Context<Self>,
	) {
		let before = self
			.paste
			.plan()
			.map(|p| (p.dir_label(&key), p.collapsed_dirs.contains(&key)));
		let folded = self
			.paste
			.toggle_dir_fold(key.clone(), self.preview.as_ref());
		let (Some((label, was)), Some(plan)) = (before, self.paste.plan())
		else {
			return;
		};
		if matches!(folded, paste::preview::Folded::Ignored) {
			return;
		}
		// A refused fold is undone: nothing toggled, nothing to report.
		let now = plan.collapsed_dirs.contains(&key);
		if now != was {
			app_log!(
				"[APP:PASTE_DIR_TOGGLED: dir={} collapsed={}]",
				label,
				now
			);
		}
		if let paste::preview::Folded::Reselected(nav) = folded {
			let moved = match nav {
				paste::preview::Nav::Moved(idx) => Some(idx),
				_ => None,
			};
			self.report_paste_nav(nav, cx);
			if let Some(idx) = moved {
				self.load_paste_diff(idx, cx);
			}
		}
		cx.notify();
	}

	/// The include box of a folder or root row.
	pub fn toggle_paste_dir_selected(
		&mut self,
		key: paste::DirKey,
		cx: &mut Context<Self>,
	) {
		if self.refuse_while_applying("include", cx) || self.paste.is_loading()
		{
			return;
		}
		if let Some(state) = self.paste.toggle_dir_selected(&key) {
			if let Some(plan) = self.paste.plan() {
				app_log!(
					"[APP:PASTE_DIR_SEL_TOGGLED: dir={} state={}]",
					plan.dir_label(&key),
					state
				);
			}
			cx.notify();
		}
	}

	/// Left and Right in the paste tree: fold, unfold or walk to the parent
	/// or first child.
	pub fn paste_tree_step(&mut self, forward: bool, cx: &mut Context<Self>) {
		let Some(plan) = self.paste.plan() else {
			return;
		};
		let step = if forward {
			plan.nav_right()
		} else {
			plan.nav_left()
		};
		match step {
			paste::TreeStep::Nothing => {}
			paste::TreeStep::Fold(key) | paste::TreeStep::Unfold(key) => {
				self.toggle_paste_dir(key, cx)
			}
			paste::TreeStep::Move(cursor) => {
				let nav = self.paste.move_to(cursor, self.preview.as_ref());
				let moved = match nav {
					paste::preview::Nav::Moved(idx) => Some(idx),
					_ => None,
				};
				self.report_paste_nav(nav, cx);
				if let Some(idx) = moved {
					self.load_paste_diff(idx, cx);
				}
			}
		}
	}

	pub(crate) fn load_paste_diff(
		&mut self,
		idx: usize,
		cx: &mut Context<Self>,
	) {
		self.cancel_paste_diff();

		let Some(plan) = self.paste.plan() else {
			return;
		};
		let Some(item) = plan.items.get(idx) else {
			return;
		};
		if item.commit.is_none() && !item.overwritable() {
			return;
		}
		let commit_request = match self.paste.commit_diff_request(idx) {
			Ok(request) => request,
			Err(msg) => {
				self.set_status_msg(msg);
				cx.notify();
				return;
			}
		};
		let skip_notice = if matches!(
			item.op,
			paste::PlannedOp::Skip(_) | paste::PlannedOp::CommitRefused(_)
		) {
			crate::ui::paste_op(item).2
		} else {
			"paste_diff_not_applied"
		};

		let cancel = arm_cancel(&mut self.paste_diff_cancel);
		let task_gen = self.paste_diff_generation;
		let selected_idx = idx;
		let item_path = item.path.clone();
		// Commit reads already own a charged core preview. Their unused UI
		// body must not survive cancel/clear as an uncharged second copy.
		let item_content = if commit_request.is_some() {
			Arc::from("")
		} else {
			item.content.clone()
		};
		let dest_root = item.dest_root.clone();
		let dest_path = item.dest_path.clone();
		let remote = plan.remote.clone();
		let cancellable = remote.is_some() || commit_request.is_some();
		let job_cancel = cancellable.then(|| cancel.clone());
		let delay = self.e2e_read_delay;
		#[cfg(test)]
		let paste_hold = self.e2e_paste_diff_hold;

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();

		self.spawn_owned(cx, job_cancel, async move {
			let for_bg_path = item_path.clone();
			let outcome = bg
				.spawn(async move {
					let outcome = if let Some(request) = commit_request {
						PasteDiffOutcome::commit(
							request,
							cancel.clone(),
							skip_notice,
						)
					} else if let Some(ref rem) = remote {
						// The worker plans under its workspace's real path, which
						// is also the workspace id; never resolve it on this machine.
						let rel = remote::worker_rel(
							&rem.session.workspace.id,
							&dest_path,
						);
						let client = rem.session.client.clone();
						let ws = rem.session.workspace.id.clone();
						match rel {
							Some(rel_path) => {
								match client.read(&ws, &rel_path, Some(&cancel))
								{
									Ok(Some(text)) => {
										if text == *item_content {
											PasteDiffOutcome::Same
										} else {
											let (patch, trunc) = snip_core::textdiff::unified(
												&for_bg_path,
												&text,
												&item_content,
												snip_core::browser::PREVIEW_LIMIT,
											);
											if patch.is_empty() {
												PasteDiffOutcome::Same
											} else {
												PasteDiffOutcome::Diff(
													patch, trunc,
												)
											}
										}
									}
									Ok(None) => PasteDiffOutcome::Binary,
									// The worker's refusal code says it
									// was too large; its text is never read.
									Err(
										snip_remote::RemoteError::Refused {
											code: snip_remote::ErrorCode::TooLarge,
											..
										}
										| snip_remote::RemoteError::PatchTooLarge {
											..
										},
									) => PasteDiffOutcome::TooLarge,
									Err(_) => PasteDiffOutcome::Unreadable,
								}
							}
							None => PasteDiffOutcome::Unreadable,
						}
					} else {
						let rel = dest_path
							.strip_prefix(&dest_root)
							.map(|r| r.to_string_lossy().into_owned())
							.unwrap_or_else(|_| for_bg_path.clone());
						match snip_core::browser::file_preview(&dest_root, &rel)
						{
							Ok(p) => match p.content {
								Some(text) => {
									if text == *item_content {
										PasteDiffOutcome::Same
									} else {
										let (patch, trunc) = snip_core::textdiff::unified(
											&for_bg_path,
											&text,
											&item_content,
											snip_core::browser::PREVIEW_LIMIT,
										);
										if patch.is_empty() {
											PasteDiffOutcome::Same
										} else {
											PasteDiffOutcome::Diff(patch, trunc)
										}
									}
								}
								None => PasteDiffOutcome::Binary,
							},
							// `file_preview` refuses a file over the preview
							// limit with this kind.
							Err(err)
								if err.kind()
									== std::io::ErrorKind::FileTooLarge =>
							{
								PasteDiffOutcome::TooLarge
							}
							Err(_) => PasteDiffOutcome::Unreadable,
						}
					};
					if let Some(delay) = delay {
						std::thread::sleep(delay);
					}
					outcome
				})
				.await;
			#[cfg(test)]
			if let Some(hold) = paste_hold {
				bg.timer(hold).await;
			}

			let _ = this.update(&mut async_app, |model, cx| {
				if model.paste_diff_generation != task_gen {
					return;
				}
				let Some(plan) = model.paste.plan() else {
					return;
				};
				if plan.selected_item_idx != selected_idx
					|| plan.cursor_dir.is_some()
				{
					return;
				}
				let Some(item) = plan.items.get(selected_idx) else {
					return;
				};
				if item.path != item_path {
					return;
				}

				let (preview, state) = match outcome {
					PasteDiffOutcome::Notice(notice) => {
						let mut p = Preview::new(
							PreviewSource::PasteItem,
							Some(item.path.clone()),
							item.content.to_string(),
							false,
							Language::from_path_or_ext(&item.path, false),
						);
						p.notice = Some(notice);
						(p, "unavailable")
					}
					PasteDiffOutcome::Same => {
						let mut p = Preview::new(
							PreviewSource::PasteItem,
							Some(item.path.clone()),
							item.content.to_string(),
							false,
							Language::from_path_or_ext(&item.path, false),
						);
						p.notice = Some(Msg::new("paste_diff_same", []));
						(p, "same")
					}
					PasteDiffOutcome::Diff(patch, truncated) => {
						let mut p = Preview::new(
							PreviewSource::PasteItem,
							Some(item.path.clone()),
							patch,
							true,
							Language::Diff,
						);
						if truncated {
							p.notice = Some(crate::reader::truncated_notice());
						}
						(p, "diff")
					}
					PasteDiffOutcome::Binary => {
						let mut p = Preview::new(
							PreviewSource::PasteItem,
							Some(item.path.clone()),
							item.content.to_string(),
							false,
							Language::from_path_or_ext(&item.path, false),
						);
						p.notice = Some(Msg::new("paste_diff_binary", []));
						(p, "unavailable")
					}
					PasteDiffOutcome::TooLarge => {
						let mut p = Preview::new(
							PreviewSource::PasteItem,
							Some(item.path.clone()),
							item.content.to_string(),
							false,
							Language::from_path_or_ext(&item.path, false),
						);
						p.notice = Some(Msg::new("paste_diff_too_large", []));
						(p, "unavailable")
					}
					PasteDiffOutcome::Unreadable => {
						let mut p = Preview::new(
							PreviewSource::PasteItem,
							Some(item.path.clone()),
							item.content.to_string(),
							false,
							Language::from_path_or_ext(&item.path, false),
						);
						p.notice = Some(Msg::new("paste_diff_unreadable", []));
						(p, "unavailable")
					}
				};

				match model.paste.replace_detail(
					selected_idx,
					preview,
					model.preview.as_ref(),
				) {
					Ok(()) => {
						app_log!(
							"[APP:PASTE_DIFF: idx={} state={}]",
							selected_idx,
							state
						);
						cx.notify();
					}
					Err(_) => {
						app_log!("[APP:PASTE_DETAIL_REFUSED: reason=retained_budget]");
						cx.notify();
					}
				}
			});
		});
	}

	pub fn apply_paste_restore(&mut self, cx: &mut Context<Self>) {
		self.user_action();
		// A path typed but not yet confirmed would be pasted into the old
		// folder. The button is locked while the field is open; Enter in
		// the panel is refused here, whoever holds the focus.
		if self.paste_dest_edit().is_some() {
			app_log!("[APP:APPLY_IGNORED: editing]");
			self.set_status("paste_dest_editing", []);
			cx.notify();
			return;
		}
		let apply = match self.paste.begin_apply() {
			Ok(w) => w,
			Err(paste::preview::ApplyRefused::Loading) => {
				app_log!("[APP:APPLY_IGNORED: loading]");
				self.set_status("paste_loading_refused", []);
				cx.notify();
				return;
			}
			Err(paste::preview::ApplyRefused::NoPlan)
			| Err(paste::preview::ApplyRefused::NotExecutable) => {
				app_log!("[APP:APPLY_IGNORED: no_plan]");
				return;
			}
			Err(paste::preview::ApplyRefused::Busy) => {
				app_log!("[APP:PASTE_BUSY: refused=apply]");
				return;
			}
			Err(paste::preview::ApplyRefused::MappingRequired) => {
				app_log!("[APP:PASTE_ERR: mapping_required]");
				self.set_paste_error(Msg::new("mapping_required", []));
				cx.notify();
				return;
			}
		};

		self.pending_focus = Some(self.paste_focus.clone());
		self.set_status("paste_apply_busy", []);
		app_log!("[APP:PASTE_APPLYING]");
		cx.notify();

		let delay = self.paste.apply_delay();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();

		self.spawn_mutating(
			cx,
			async move {
				let exec_res = bg
					.spawn(async move {
						if let Some(d) = delay {
							std::thread::sleep(d);
						}
						apply.execute()
					})
					.await;

				// No generation check: the write already happened (or was
				// refused as stale), and its result must always be shown.
				// The plan cannot be replaced while applying.
				let _ = this.update(&mut async_app, |model, cx| {
					match exec_res {
						Ok(result) => {
							app_log!(
								"[APP:PASTE_DONE: created={} overwritten={} skipped={} deleted={} errors={} commits={}]",
								result.files.created_count,
								result.files.overwritten_count,
								result.files.total_skipped(),
								result.files.deleted_count,
								result.files.errors.len(),
								result.created_commits.len()
							);
							for error in &result.files.errors {
								app_log!("[APP:PASTE_FILE_ERROR: {error}]");
							}
							if let Some(token) = model.paste_diff_cancel.take() {
								token.cancel();
							}
							model.paste_diff_generation = model.paste_diff_generation.wrapping_add(1);
							model.paste.clear(model.preview.as_ref());
							model.restore_log_after_paste();
							model.pending_focus =
								Some(model.focus_handle.clone());
							// A background paste removed its PastePanel. Do not
							// restore that saved handle when the tab is shown:
							// it has no element to receive the next shortcut.
							model.resume_focus = None;
							// A rescan refreshes the destination's summary too,
							// then reloads the open repo keeping its anchors.
							model.reload_repos(cx);
							// A card, not the status line: the rescan above
							// rewrites the status at once, and the failed paths
							// must stay readable.
							let ok = result.files.errors.is_empty();
							model.show_toast(ok, result.status(), cx);
						}
						Err(err) => {
							app_log!(
								"[APP:PASTE_STALE_DETECTED: {}]",
								err.token.as_str()
							);
							model.paste.apply_failed(
								err.msg.clone(),
								model.preview.as_ref(),
							);
							// Idle again: the repos added while it applied
							// are candidates now.
							model.refresh_paste_candidates(cx);
							model.status = err.msg;
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// Called on every path that closes the paste preview.
	fn restore_log_after_paste(&mut self) {
		// The typed destination belongs to the preview that is closing.
		self.drop_paste_dest_edit();
		if self.paste.restore_log(&mut self.bottom_visible) {
			app_log!("[APP:LOG_PANEL: visible=true reason=paste_close]");
		}
	}

	/// Editor tabs open: the paste preview, or the one reader tab.
	pub fn open_tab_count(&self) -> usize {
		let paste = self.paste.is_open();
		let reader = self.preview.is_some()
			|| self.preview_loading
			|| self.selected_commit.is_some()
			|| self.compare.is_some();
		usize::from(paste || reader)
	}

	/// Closes editor tab `idx`. The workbench shows one tab at a time, so
	/// only index 0 exists: the paste preview is cancelled, a reader tab
	/// clears what it shows (the Git selection that opened it included).
	pub fn close_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
		if idx != 0 || self.open_tab_count() == 0 {
			return;
		}
		if self.paste.is_open() {
			self.cancel_paste_preview(cx);
			return;
		}
		// Drop in-flight reads for the closed tab.
		self.preview_generation += 1;
		let _ = arm_cancel(&mut self.preview_cancel);
		self.clear_preview();
		self.reader.release_retained();
		self.preview_loading = false;
		self.preview_error = None;
		self.selected_file = None;
		self.selected_file_source = None;
		self.selected_file_root = None;
		self.selected_commit = None;
		self.range_head = None;
		self.log_selected.clear();
		self.compare = None;
		self.reset_commit_view();
		app_log!("[APP:TAB_CLOSED: {idx}]");
		cx.notify();
	}

	/// With a single tab there is never another one to close.
	pub fn close_other_tabs(&mut self, idx: usize, cx: &mut Context<Self>) {
		let _ = idx;
		cx.notify();
	}

	pub fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
		self.close_tab(0, cx);
	}

	pub fn cancel_paste_preview(&mut self, cx: &mut Context<Self>) {
		self.user_action();
		if self.refuse_while_applying("cancel", cx) {
			return;
		}
		let was_loading = self.paste.is_loading();
		if let Some(token) = self.paste_diff_cancel.take() {
			token.cancel();
		}
		self.paste_diff_generation = self.paste_diff_generation.wrapping_add(1);
		self.paste.invalidate_job();
		if self.paste.plan().is_none() && !was_loading {
			return;
		}
		self.paste.clear(self.preview.as_ref());
		self.restore_log_after_paste();
		self.pending_focus = Some(self.focus_handle.clone());
		self.set_status("paste_cancelled", []);
		app_log!("[APP:PASTE_CANCELLED]");
		cx.notify();
	}

	pub fn copy_current_preview_content(&mut self, cx: &mut Context<Self>) {
		self.user_action();
		if !self.can_copy_preview() {
			return;
		}
		// Copies the selection if any, otherwise the whole retained preview.
		if self.copy_reader_selection(cx) {
			return;
		}
		if let Some(p) = &self.preview {
			let text = p.text.clone();
			let len = text.len();
			let fp = p.fingerprint();
			match clip::write_text(&text) {
				Ok(()) => {
					self.set_status("status_copied_preview", []);
					app_log!(
						"[APP:PREVIEW_COPIED: bytes={} fnv={:x}]",
						len,
						fp
					);
				}
				Err(e) => {
					self.set_status_msg(crate::reason::clipboard_failed(&e))
				}
			}
			cx.notify();
		}
	}
}

/// Where `prefix` lands, as printed in `[APP:PASTE_MAPPED]`.
fn prefix_target(plan: &PastePreviewPlan, prefix: &str, keep: bool) -> String {
	if keep {
		return plan.shown(&plan.destination);
	}
	plan.prefix_choices
		.iter()
		.find(|choice| choice.prefix == prefix)
		.and_then(|choice| choice.destination.as_ref())
		.map(|dest| plan.shown(dest))
		.unwrap_or_default()
}

/// E2E only: keeps a finished tree read on its background thread while the
/// hold file exists. `hold` is `None` in every normal run.
fn hold_tree_read(hold: Option<&std::path::Path>) {
	let Some(hold) = hold else {
		return;
	};
	if !hold.exists() {
		return;
	}
	app_log!("[APP:TREE_IO_HELD]");
	while hold.exists() {
		std::thread::sleep(std::time::Duration::from_millis(20));
	}
}

fn repo_key(entry: &RepoEntry) -> (PathBuf, PathBuf) {
	if let Some(id) = &entry.identity {
		(id.toplevel.clone(), id.git_dir.clone())
	} else {
		(entry.root.clone(), PathBuf::new())
	}
}

/// `repo_key(entry) == *key` without allocating.
fn repo_key_matches(entry: &RepoEntry, key: &(PathBuf, PathBuf)) -> bool {
	match &entry.identity {
		Some(id) => id.toplevel == key.0 && id.git_dir == key.1,
		None => entry.root == key.0 && key.1.as_os_str().is_empty(),
	}
}

async fn drive_discovery(
	this: gpui::WeakEntity<WorkbenchModel>,
	cx: &mut gpui::AsyncApp,
	mut disc: Discovery,
	generation: u64,
	cancel: CancelToken,
	mut wipe: bool,
) {
	loop {
		if cancel.is_cancelled() {
			let _ = this.update(cx, |model, cx| {
				if model.discovery_generation != generation {
					return;
				}
				model.discovery = Some(disc);
				model.discovery_status = Some(ScanStatus::Cancelled);
				model.is_loading = false;
				cx.notify();
			});
			return;
		}
		let cancel_page = cancel.clone();
		let bg = cx.background_executor().clone();
		let (returned, page) = bg
			.spawn(async move {
				if cancel_page.is_cancelled() {
					return (disc, None);
				}
				let mut budget = ScanBudget::visits(2_000);
				budget.cancel = Some(cancel_page);
				let page = disc.next_page(&budget);
				(disc, Some(page))
			})
			.await;
		disc = returned;
		let Some(page) = page else {
			let _ = this.update(cx, |model, cx| {
				if model.discovery_generation != generation {
					return;
				}
				model.discovery = Some(disc);
				model.discovery_status = Some(ScanStatus::Cancelled);
				model.is_loading = false;
				cx.notify();
			});
			return;
		};
		let status = page.status;
		let visited = page.visited;
		let found = page.repos;
		let page_errors = page.errors;
		let depth = page.depth_limited;
		let cancel_git = cancel.clone();
		let bg = cx.background_executor().clone();
		let processed = bg
			.spawn(async move {
				let opts = RunOptions {
					cancel: Some(cancel_git),
					..RunOptions::interactive(None)
				};
				WorkbenchModel::process_discovery_repos(found, &opts)
			})
			.await;
		let stop = !matches!(status, ScanStatus::More);
		let done = this.update(cx, |model, cx| {
			if model.discovery_generation != generation {
				return true;
			}
			model.apply_scan_page(wipe, processed.0, cx);
			wipe = false;
			for err in processed.1 {
				model.push_discovery_error(err);
			}
			for err in page_errors {
				model.push_discovery_error(err);
			}
			for path in depth {
				model.push_depth_limit(path);
			}
			model.discovery_status = Some(status);
			app_log!(
				"[APP:DISCOVERY_PROGRESS: repos={} status={status:?} visited={visited}]",
				model.repos.len()
			);
			model.place_selection(cx);
			cx.notify();
			stop
		});
		if done.unwrap_or(true) {
			let _ = this.update(cx, |model, cx| {
				if model.discovery_generation != generation {
					return;
				}
				model.discovery = Some(disc);
				model.finish_discovery(cx);
			});
			return;
		}
	}
}

/// A scan failure for a test: its text is all a test of the layout needs.
#[cfg(test)]
pub(crate) fn test_failure(text: &str) -> Failure {
	Failure::new(
		text,
		Reason::Io {
			detail: snip_core::reason::Datum::for_test(text),
		},
	)
}

fn resolve_added_repo(
	path: PathBuf,
	cancel: &CancelToken,
) -> Result<RepoEntry, Box<Failure>> {
	if cancel.is_cancelled() {
		return Err(Box::new(Failure::new("cancelled", Reason::Cancelled)));
	}
	let path = dunce::canonicalize(&path).map_err(|err| {
		Box::new(Failure::io(&err, &path.display().to_string()))
	})?;
	let opts = RunOptions {
		cancel: Some(cancel.clone()),
		..RunOptions::interactive(None)
	};
	Ok(RepoEntry::from(snip_core::gitview::identify_repo(
		&path, None, &opts,
	)))
}

/// Why a preview read gave no text (#193).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewErr {
	/// Over the 1 MiB preview limit, on any host.
	TooLarge,
	/// The diff output, not the file, passed the preview limit.
	PatchTooLarge,
	Failed(Msg),
}

impl From<Msg> for PreviewErr {
	fn from(m: Msg) -> Self {
		Self::Failed(m)
	}
}

impl PreviewErr {
	/// A Git read's failure; a start failure is worded when drawn.
	pub(crate) fn git(e: snip_core::gitsrc::GitError) -> Self {
		match e {
			snip_core::gitsrc::GitError::OutputLimit { .. } => Self::TooLarge,
			snip_core::gitsrc::GitError::PatchLimit { .. } => {
				Self::PatchTooLarge
			}
			other => Self::Failed(remote::git_error_msg(other)),
		}
	}
}

impl From<std::io::Error> for PreviewErr {
	fn from(e: std::io::Error) -> Self {
		if e.kind() == std::io::ErrorKind::FileTooLarge {
			Self::TooLarge
		} else {
			Self::Failed(crate::reason::reason_msg(
				&snip_core::reason::io_reason(&e, ""),
			))
		}
	}
}

impl std::fmt::Display for PreviewErr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::TooLarge => f.write_str("over the 1 MiB preview limit"),
			Self::PatchTooLarge => {
				f.write_str("diff output over the preview limit")
			}
			Self::Failed(m) => f.write_str(&m.render(Locale::ZhTw)),
		}
	}
}

impl PreviewErr {
	/// The message shown in the editor for `path`.
	pub(crate) fn msg(self, path: String) -> Msg {
		match self {
			Self::TooLarge => Msg::new("error_preview_too_large", [path]),
			Self::PatchTooLarge => {
				Msg::new("error_preview_patch_too_large", [path])
			}
			Self::Failed(e) => {
				Msg::with_msg_arg("error_preview", [path, String::new()], 1, e)
			}
		}
	}
}

fn read_preview(
	host: &crate::githost::GitHost,
	repo_root: &std::path::Path,
	path: &str,
	source: &SourceKind,
	cancel: CancelToken,
) -> Result<(browser::SourcePreview, PreviewSource), PreviewErr> {
	if matches!(source, SourceKind::File) {
		return browser::file_preview(repo_root, path)
			.map(|p| (p, PreviewSource::WorkingFile))
			.map_err(PreviewErr::from);
	}
	let (git_source, preview_source) = match source {
		SourceKind::Staged => (GitSource::Staged, PreviewSource::StagedChanges),
		SourceKind::Unstaged => {
			(GitSource::Working, PreviewSource::UnstagedChanges)
		}
		SourceKind::Working => {
			(GitSource::Working, PreviewSource::WorkingChanges)
		}
		SourceKind::Commit { rev } => (
			GitSource::Commit(rev.clone()),
			PreviewSource::CommitFile { sha: rev.clone() },
		),
		SourceKind::Range { base, tip } => (
			GitSource::Range(base.clone(), tip.clone()),
			PreviewSource::CommitFile { sha: tip.clone() },
		),
		SourceKind::File => (GitSource::Working, PreviewSource::WorkingFile),
	};
	let read = snip_core::gitview::Read {
		profile: snip_core::gitview::ReadProfile::InteractivePreview,
		cancel: Some(cancel),
	};
	let repo = host.open(repo_root, None, &read)?;
	match repo.preview(&git_source, path, None, &read) {
		Ok(p) => Ok((
			browser::SourcePreview {
				content: p.content,
				patch: p.patch,
			},
			preview_source,
		)),
		// A failed/cancelled Git source must never be replaced by current
		// working-tree bytes. Explicit File/tree sources are handled above.
		Err(e) => Err(PreviewErr::git(e)),
	}
}

/// The folder a launch opens when nothing restores tabs: `--workspace`,
/// else the last remembered local one (IntelliJ reopens the last project),
/// else the launch folder. A Finder or Explorer launch starts in `/` or the
/// home folder: that opens nothing rather than scanning it.
fn startup_choice(
	arg: Option<PathBuf>,
	last_local: Option<PathBuf>,
	cwd: Option<PathBuf>,
	home: Option<PathBuf>,
) -> Option<PathBuf> {
	if arg.is_some() {
		return arg;
	}
	if last_local.is_some() {
		return last_local;
	}
	cwd.filter(|cwd| cwd.parent().is_some() && Some(cwd) != home.as_ref())
}

/// What a launch shows. A normal launch reopens the tabs of the last
/// session and then opens or switches to `--workspace`; with no tabs ever
/// saved it falls back to [`startup_choice`]. A test mode opens one tab
/// and neither restores nor saves. Nothing is read or saved under
/// `cfg(test)` or in an e2e run without `SNIP_CONFIG_DIR`.
fn startup_launch(
	arg: Option<PathBuf>,
	normal: bool,
	restore_dir: Option<PathBuf>,
) -> tabs::Launch {
	launch_with(open_tabs::store_dir(), arg, normal, restore_dir, |arg| {
		startup_choice(
			arg,
			recent::load().into_iter().next(),
			std::env::current_dir().ok(),
			recent::home(),
		)
	})
}

/// [`startup_launch`] with the store folder and the fallback given.
fn launch_with(
	store: Option<PathBuf>,
	arg: Option<PathBuf>,
	normal: bool,
	restore_dir: Option<PathBuf>,
	fallback: impl FnOnce(Option<PathBuf>) -> Option<PathBuf>,
) -> tabs::Launch {
	let store = store.filter(|_| normal);
	let restore = store.as_deref().and_then(open_tabs::load_from);
	let first = if restore.is_some() {
		arg
	} else {
		fallback(arg)
	};
	tabs::Launch {
		restore,
		first: first.map_or(tabs::FirstTab::Empty, tabs::FirstTab::Local),
		restore_dir,
		store,
	}
}

type CliArgs = (Option<PathBuf>, String, Option<PathBuf>);

fn parse_cli_args() -> CliArgs {
	let mut workspace = None;
	let mut mode = "normal".to_string();
	let mut restore_dir = None;

	let args: Vec<String> = std::env::args().collect();

	// Reject --version or --help combined with other arguments
	if args.iter().any(|a| a == "--version" || a == "-V") && args.len() > 2 {
		eprintln!("Error: --version cannot be combined with other arguments");
		std::process::exit(2);
	}
	if args.iter().any(|a| a == "--help" || a == "-h") && args.len() > 2 {
		eprintln!("Error: --help cannot be combined with other arguments");
		std::process::exit(2);
	}

	let mut i = 1;
	while i < args.len() {
		match args[i].as_str() {
			"--version" | "-V" => {
				println!("snip-desktop-native {}", env!("CARGO_PKG_VERSION"));
				std::process::exit(0);
			}
			"--help" | "-h" => {
				println!("snip-desktop-native: GPUI Native Git Workbench");
				println!("Usage: snip-desktop-native [OPTIONS]");
				println!("Options:");
				println!("  --workspace <DIR>    Set workspace folder containing repos");
				println!("  --mode <MODE>        Run mode: normal, idle, overview, preview");
				println!("  --restore-dir <DIR>  Target folder for paste restore operations");
				println!("  -V, --version        Print version information");
				println!("  -h, --help           Print help");
				std::process::exit(0);
			}
			"--workspace" => {
				if i + 1 < args.len() && !args[i + 1].starts_with('-') {
					workspace = Some(PathBuf::from(&args[i + 1]));
					i += 1;
				} else {
					eprintln!(
						"Error: --workspace requires a directory argument"
					);
					std::process::exit(2);
				}
			}
			"--mode" => {
				if i + 1 < args.len() && !args[i + 1].starts_with('-') {
					let val = &args[i + 1];
					if !matches!(
						val.as_str(),
						"normal" | "idle" | "overview" | "preview"
					) {
						eprintln!(
							"Error: invalid --mode: {val}. Valid modes: normal, idle, overview, preview"
						);
						std::process::exit(2);
					}
					mode = val.clone();
					i += 1;
				} else {
					eprintln!("Error: --mode requires a mode argument");
					std::process::exit(2);
				}
			}
			"--restore-dir" => {
				if i + 1 < args.len() && !args[i + 1].starts_with('-') {
					restore_dir = Some(PathBuf::from(&args[i + 1]));
					i += 1;
				} else {
					eprintln!(
						"Error: --restore-dir requires a directory argument"
					);
					std::process::exit(2);
				}
			}
			unknown => {
				eprintln!("Error: unrecognized argument: {unknown}");
				std::process::exit(2);
			}
		}
		i += 1;
	}
	(workspace, mode, restore_dir)
}

fn key_bindings() -> Vec<KeyBinding> {
	let mut b = vec![
		KeyBinding::new("ctrl-q", Quit, None),
		KeyBinding::new("cmd-q", Quit, None),
		KeyBinding::new("ctrl-c", CopySelection, None),
		KeyBinding::new("cmd-c", CopySelection, None),
		KeyBinding::new("ctrl-v", PastePreview, None),
		KeyBinding::new("cmd-v", PastePreview, None),
		KeyBinding::new("ctrl-r", Refresh, None),
		KeyBinding::new("cmd-r", Refresh, None),
		KeyBinding::new("tab", FocusNext, None),
		KeyBinding::new("shift-tab", FocusPrev, None),
		KeyBinding::new("ctrl-tab", FocusNext, None),
		KeyBinding::new("ctrl-shift-tab", FocusPrev, None),
		// IntelliJ tool window shortcuts.
		KeyBinding::new("alt-1", ShowProject, None),
		KeyBinding::new("alt-0", ShowChanges, None),
		KeyBinding::new("alt-9", ToggleLog, None),
		// Repo selector has no IntelliJ counterpart; the branches popup is
		// IntelliJ's Ctrl+Shift+` (X11 may report the shifted key as `~`).
		KeyBinding::new("alt-shift-r", OpenRepoSelector, None),
		KeyBinding::new("secondary-shift-`", OpenRefSelector, None),
		KeyBinding::new("secondary-shift-~", OpenRefSelector, None),
		KeyBinding::new("secondary-~", OpenRefSelector, None),
		KeyBinding::new("shift-escape", HideToolWindow, None),
		KeyBinding::new("f7", NextDiff, None),
		KeyBinding::new("shift-f7", PrevDiff, None),
		// Both close the workspace tab.
		KeyBinding::new("ctrl-w", CloseWorkspace, None),
		KeyBinding::new("cmd-w", CloseWorkspace, None),
		KeyBinding::new("ctrl-shift-w", CloseWorkspace, None),
		KeyBinding::new("cmd-shift-w", CloseWorkspace, None),
		// Workspace tabs; a shifted bracket may arrive as the brace.
		KeyBinding::new("ctrl-shift-[", PrevWorkspaceTab, None),
		KeyBinding::new("cmd-shift-[", PrevWorkspaceTab, None),
		KeyBinding::new("ctrl-{", PrevWorkspaceTab, None),
		KeyBinding::new("cmd-{", PrevWorkspaceTab, None),
		KeyBinding::new("ctrl-shift-]", NextWorkspaceTab, None),
		KeyBinding::new("cmd-shift-]", NextWorkspaceTab, None),
		KeyBinding::new("ctrl-}", NextWorkspaceTab, None),
		KeyBinding::new("cmd-}", NextWorkspaceTab, None),
		KeyBinding::new("ctrl-1", WorkspaceTab1, None),
		KeyBinding::new("cmd-1", WorkspaceTab1, None),
		KeyBinding::new("ctrl-2", WorkspaceTab2, None),
		KeyBinding::new("cmd-2", WorkspaceTab2, None),
		KeyBinding::new("ctrl-3", WorkspaceTab3, None),
		KeyBinding::new("cmd-3", WorkspaceTab3, None),
		KeyBinding::new("ctrl-4", WorkspaceTab4, None),
		KeyBinding::new("cmd-4", WorkspaceTab4, None),
		KeyBinding::new("ctrl-5", WorkspaceTab5, None),
		KeyBinding::new("cmd-5", WorkspaceTab5, None),
		KeyBinding::new("ctrl-6", WorkspaceTab6, None),
		KeyBinding::new("cmd-6", WorkspaceTab6, None),
		KeyBinding::new("ctrl-7", WorkspaceTab7, None),
		KeyBinding::new("cmd-7", WorkspaceTab7, None),
		KeyBinding::new("ctrl-8", WorkspaceTab8, None),
		KeyBinding::new("cmd-8", WorkspaceTab8, None),
		KeyBinding::new("ctrl-9", WorkspaceTab9, None),
		KeyBinding::new("cmd-9", WorkspaceTab9, None),
		KeyBinding::new("ctrl-shift-o", OpenWorkspace, None),
		KeyBinding::new("cmd-shift-o", OpenWorkspace, None),
		KeyBinding::new("alt-l", ToggleLocale, None),
		KeyBinding::new("alt-n", HistoryNextPage, None),
		KeyBinding::new("alt-p", HistoryPrevPage, None),
		KeyBinding::new("ctrl-f", FindInFile, None),
		KeyBinding::new("ctrl-g", GotoLine, None),
		KeyBinding::new("f3", FindNext, None),
		KeyBinding::new("shift-f3", FindPrev, None),
		// Paste preview panel.
		KeyBinding::new("enter", ApplyPaste, Some("PastePanel")),
		KeyBinding::new("up", NavUp, Some("PastePanel")),
		KeyBinding::new("down", NavDown, Some("PastePanel")),
		KeyBinding::new("space", NavToggle, Some("PastePanel")),
		KeyBinding::new("left", NavCollapse, Some("PastePanel")),
		KeyBinding::new("right", NavExpand, Some("PastePanel")),
		// Reader.
		KeyBinding::new("ctrl-c", ReaderCopy, Some("Reader")),
		KeyBinding::new("cmd-c", ReaderCopy, Some("Reader")),
		KeyBinding::new("ctrl-a", ReaderSelectAll, Some("Reader")),
		KeyBinding::new("up", ReaderUp, Some("Reader")),
		KeyBinding::new("down", ReaderDown, Some("Reader")),
		KeyBinding::new("pageup", ReaderPageUp, Some("Reader")),
		KeyBinding::new("pagedown", ReaderPageDown, Some("Reader")),
		// Project / Changes list.
		KeyBinding::new("up", TreeUp, Some("ToolList")),
		KeyBinding::new("down", TreeDown, Some("ToolList")),
		KeyBinding::new("right", TreeExpand, Some("ToolList")),
		KeyBinding::new("left", TreeCollapse, Some("ToolList")),
		KeyBinding::new("enter", TreeOpen, Some("ToolList")),
		KeyBinding::new("space", TreeToggle, Some("ToolList")),
		KeyBinding::new("secondary-d", TreeOpen, Some("ToolList")),
		KeyBinding::new("pageup", ToolPageUp, Some("ToolList")),
		KeyBinding::new("pagedown", ToolPageDown, Some("ToolList")),
		// Context menu.
		KeyBinding::new("up", MenuUp, Some("ContextMenu")),
		KeyBinding::new("down", MenuDown, Some("ContextMenu")),
		KeyBinding::new("enter", MenuConfirm, Some("ContextMenu")),
		// Git log.
		KeyBinding::new("ctrl-c", LogCopy, Some("GitLog")),
		KeyBinding::new("cmd-c", LogCopy, Some("GitLog")),
		KeyBinding::new("up", LogUp, Some("GitLog")),
		KeyBinding::new("down", LogDown, Some("GitLog")),
		KeyBinding::new("shift-up", LogExtendUp, Some("GitLog")),
		KeyBinding::new("shift-down", LogExtendDown, Some("GitLog")),
		KeyBinding::new("enter", LogOpen, Some("GitLog")),
		KeyBinding::new("ctrl-f", LogSearchFocus, Some("GitLog")),
		KeyBinding::new("secondary-d", LogOpen, Some("GitLog")),
		KeyBinding::new("left", LogParent, Some("GitLog")),
		KeyBinding::new("right", LogChild, Some("GitLog")),
		KeyBinding::new("pagedown", LogPageDown, Some("GitLog")),
		KeyBinding::new("pageup", LogPageUp, Some("GitLog")),
		// No IntelliJ key for "go to HEAD" in the Log; kept.
		KeyBinding::new("h", LogHead, Some("GitLog")),
	];
	b.extend(text_input::bindings());
	b
}

fn main() {
	let (workspace, mode, restore_dir) = parse_cli_args();
	let launch = startup_launch(workspace, mode == "normal", restore_dir);
	let app = Application::new().with_assets(icons::Assets);

	app.run(move |cx: &mut App| {
		cx.bind_keys(key_bindings());
		theme::register_fonts(cx);

		let bounds = Bounds::centered(None, size(px(1080.0), px(720.0)), cx);
		let app_mode = mode.clone();

		let window_result = cx.open_window(
			WindowOptions {
				window_bounds: Some(WindowBounds::Windowed(bounds)),
				titlebar: Some(gpui::TitlebarOptions {
					title: Some("snip-sync".into()),
					..Default::default()
				}),
				app_id: Some("snip-desktop-native".to_string()),
				..Default::default()
			},
			|window, cx| {
				// Follow the OS light/dark appearance (unless SNIP_THEME pins it).
				theme::sync_appearance(window.appearance());
				window
					.observe_window_appearance(|window, _| {
						if theme::sync_appearance(window.appearance()) {
							window.refresh();
						}
					})
					.detach();
				if app_mode == "idle" {
					ready_marker("IDLE");
				}
				let root = cx.new(|cx| {
					tabs::TabsRoot::new(
						launch,
						app_mode,
						ui::Probes::from_env(),
						window,
						cx,
					)
				});
				let close_target = root.clone();
				window.on_window_should_close(cx, move |window, cx| {
					close_target
						.update(cx, |root, cx| root.begin_quit(window, cx));
					false
				});
				app_log!("[APP:WINDOW_READY]");
				root
			},
		);

		if let Err(e) = window_result {
			eprintln!("Failed to open native window: {:?}", e);
			std::process::exit(1);
		}
	});
}

#[cfg(test)]
mod tests {
	/// `--workspace` wins; then the last local workspace; then a launch
	/// folder other than `/` or home. Only a launch with no tabs saved
	/// gets that far.
	#[test]
	fn startup_opens_the_argument_else_the_last_local_workspace() {
		use super::{startup_choice, startup_launch};
		use crate::tabs::FirstTab;
		use std::path::PathBuf;
		let p = |s: &str| Some(PathBuf::from(s));
		assert_eq!(
			startup_choice(p("/arg"), p("/local"), p("/cwd"), None),
			p("/arg")
		);
		assert_eq!(
			startup_choice(None, p("/local"), p("/cwd"), None),
			p("/local")
		);
		assert_eq!(
			startup_choice(None, None, p("/home/u/w"), p("/home/u")),
			p("/home/u/w")
		);
		assert_eq!(
			startup_choice(None, None, p("/home/u"), p("/home/u")),
			None
		);
		assert_eq!(startup_choice(None, None, p("/"), None), None);
		// Nothing remembered is read, nor anything saved, under cfg(test).
		let launch = startup_launch(p("/arg"), true, p("/dest"));
		assert!(launch.restore.is_none() && launch.store.is_none());
		assert!(
			matches!(launch.first, FirstTab::Local(ref a) if a == &PathBuf::from("/arg"))
		);
		assert_eq!(launch.restore_dir, p("/dest"));
	}

	/// A normal launch restores the saved tabs and adds only `--workspace`;
	/// with nothing saved it falls back to the remembered folder. A test
	/// mode neither restores nor saves.
	#[test]
	fn a_normal_launch_restores_and_a_test_mode_does_not() {
		use super::launch_with;
		use crate::open_tabs::{save_to, OpenTabs, SavedTab};
		use crate::tabs::FirstTab;
		use std::path::PathBuf;
		let tmp = tempfile::tempdir().unwrap();
		let store = tmp.path().to_path_buf();
		let p = |s: &str| Some(PathBuf::from(s));
		let fallback = |arg: Option<PathBuf>| arg.or(p("/remembered"));

		let fresh =
			launch_with(Some(store.clone()), None, true, None, fallback);
		assert!(fresh.restore.is_none());
		assert_eq!(fresh.store, Some(store.clone()));
		assert!(
			matches!(fresh.first, FirstTab::Local(ref f) if f == &PathBuf::from("/remembered"))
		);

		let saved = OpenTabs {
			tabs: vec![SavedTab::Local {
				path: "/w/a".into(),
			}],
			active: Some(0),
		};
		save_to(&store, &saved).unwrap();
		let normal =
			launch_with(Some(store.clone()), None, true, None, fallback);
		assert_eq!(normal.restore, Some(saved.clone()));
		assert!(matches!(normal.first, FirstTab::Empty), "no fallback");
		let with_arg = launch_with(
			Some(store.clone()),
			p("/arg"),
			true,
			p("/d"),
			fallback,
		);
		assert!(
			matches!(with_arg.first, FirstTab::Local(ref f) if f == &PathBuf::from("/arg"))
		);

		let test_mode =
			launch_with(Some(store.clone()), p("/arg"), false, None, fallback);
		assert!(test_mode.restore.is_none() && test_mode.store.is_none());
		assert!(
			matches!(test_mode.first, FirstTab::Local(ref f) if f == &PathBuf::from("/arg"))
		);
	}

	/// A remote copy that copies nothing reports the same "skipped" status
	/// the local copy does when the worker skipped every file it saw.
	#[test]
	fn remote_copy_all_skipped_reports_the_skipped_status() {
		let outcome =
			|copied: usize, skipped: usize| snip_core::transfer::CopyOutcome {
				payload: String::new(),
				copied,
				chars: 0,
				lines: 0,
				words: 0,
				tokens: 0,
				skipped,
				truncated: false,
			};
		assert_eq!(
			super::remote_copy_nothing_status(&outcome(0, 3)).key,
			"status_copy_nothing_skipped"
		);
		assert_eq!(
			super::remote_copy_nothing_status(&outcome(0, 0)).key,
			"status_copy_nothing"
		);
	}

	/// Copy notification formats files, characters, lines, words, tokens and
	/// skipped count in both locales, for normal and limit-cut copies.
	#[test]
	fn copy_status_notification_contains_words_and_tokens() {
		use crate::i18n::{tf, Locale};
		let payload = "file alpha.txt\nhello world\nanother line with words\n";
		let expected_stats = snip_core::stats::payload_stats(payload);
		assert!(expected_stats.words > 0);
		assert!(expected_stats.tokens > 0);

		let outcome = snip_core::transfer::CopyOutcome {
			payload: payload.to_string(),
			copied: 1,
			chars: expected_stats.chars,
			lines: expected_stats.lines,
			words: expected_stats.words,
			tokens: expected_stats.tokens,
			skipped: 0,
			truncated: false,
		};

		let msg = crate::copied_status("test-repo".into(), &outcome);
		assert_eq!(msg.key, "status_copied");

		// ZhTw
		let zh = tf(msg.key, Locale::ZhTw, &msg.args);
		assert!(
			zh.contains(&format!("{} 字", expected_stats.words)),
			"ZhTw message '{zh}' must contain word count"
		);
		assert!(
			zh.contains(&format!("{} 個 token", expected_stats.tokens)),
			"ZhTw message '{zh}' must contain token count"
		);

		// En
		let en = tf(msg.key, Locale::En, &msg.args);
		assert!(
			en.contains(&format!("{} words", expected_stats.words)),
			"En message '{en}' must contain word count"
		);
		assert!(
			en.contains(&format!("~{} tokens", expected_stats.tokens)),
			"En message '{en}' must contain token count"
		);

		// Limit outcome
		let mut truncated_outcome = outcome.clone();
		truncated_outcome.truncated = true;
		let limit_msg =
			crate::copied_status("test-repo".into(), &truncated_outcome);
		assert_eq!(limit_msg.key, "status_copied_limit");

		let zh_lim = tf(limit_msg.key, Locale::ZhTw, &limit_msg.args);
		assert!(
			zh_lim.contains(&format!("{} 字", expected_stats.words)),
			"ZhTw limit message '{zh_lim}' must contain word count"
		);
		assert!(
			zh_lim.contains(&format!("{} 個 token", expected_stats.tokens)),
			"ZhTw limit message '{zh_lim}' must contain token count"
		);

		let en_lim = tf(limit_msg.key, Locale::En, &limit_msg.args);
		assert!(
			en_lim.contains(&format!("{} words", expected_stats.words)),
			"En limit message '{en_lim}' must contain word count"
		);
		assert!(
			en_lim.contains(&format!("~{} tokens", expected_stats.tokens)),
			"En limit message '{en_lim}' must contain token count"
		);
	}

	/// A preview read over the limit is `TooLarge` on every host, and any
	/// other failure keeps its text (#193).
	#[test]
	fn preview_err_says_too_large_for_any_host() {
		use super::PreviewErr;
		use snip_core::gitsrc::GitError;
		assert_eq!(
			PreviewErr::git(GitError::OutputLimit {
				args: "cat-file x:big".into(),
				limit: 1048576,
			},),
			PreviewErr::TooLarge
		);
		assert_eq!(
			PreviewErr::git(GitError::from(
				snip_remote::RemoteError::Refused {
					code: snip_remote::ErrorCode::TooLarge,
					message:
						"the result is too large to send; narrow the request"
							.into(),
					reason: None,
					suggestion: None,
				}
			),),
			PreviewErr::TooLarge
		);
		assert_eq!(
			PreviewErr::from(std::io::Error::new(
				std::io::ErrorKind::FileTooLarge,
				"x"
			)),
			PreviewErr::TooLarge
		);
		// The synthesized patch overflowing is not the file being too large
		// (#214).
		assert_eq!(
			PreviewErr::git(GitError::PatchLimit {
				path: "big.txt".into(),
				limit: 1048576,
			}),
			PreviewErr::PatchTooLarge
		);
		// ...and from a remote worker too: the same refusal code, told apart
		// by the worker's flag.
		assert_eq!(
			PreviewErr::git(GitError::from(
				snip_remote::RemoteError::PatchTooLarge {
					message: "the diff is too large to send".into(),
				}
			)),
			PreviewErr::PatchTooLarge
		);
		let text = PreviewErr::PatchTooLarge
			.msg("big.txt".into())
			.render(crate::i18n::Locale::ZhTw);
		assert!(text.contains("big.txt") && text.contains("差異"), "{text}");
		let text = PreviewErr::PatchTooLarge
			.msg("big.txt".into())
			.render(crate::i18n::Locale::En);
		assert!(text.contains("big.txt") && text.contains("diff"), "{text}");
		assert!(matches!(
			PreviewErr::git(GitError::remote(
				snip_core::reason::Reason::Protocol,
				"boom",
			)),
			PreviewErr::Failed(_)
		));
		let text = PreviewErr::TooLarge
			.msg("big.txt".into())
			.render(crate::i18n::Locale::ZhTw);
		assert!(text.contains("big.txt"), "{text}");
		assert!(!text.contains("bytes") && !text.contains("0 "), "{text}");
	}

	/// UI state driven in-process: no display, so these also run in the
	/// Windows and macOS Test jobs, which have no real-app GUI test.
	mod in_process {
		use crate::{ChangesEmpty, LogEmpty, WorkbenchModel, WorkbenchTab};
		use gpui::{Entity, TestAppContext, VisualTestContext};
		use snip_core::clip;
		use snip_core::format::parse_clipboard;
		use std::fs;
		use std::path::{Path, PathBuf};
		use std::process::Command;
		use std::sync::{Mutex, MutexGuard, PoisonError};
		use std::time::Duration;

		fn git(dir: &Path, args: &[&str]) {
			let out = Command::new("git")
				.current_dir(dir)
				.args(["-c", "user.name=t", "-c", "user.email=t@t"])
				.args(args)
				.output()
				.expect("git must run");
			assert!(out.status.success(), "git {args:?}: {out:?}");
		}

		/// A repo with `base.txt` committed and `untracked` written on top.
		fn repo(ws: &Path, name: &str, untracked: &[(&str, &str)]) -> PathBuf {
			let repo = ws.join(name);
			fs::create_dir(&repo).unwrap();
			git(&repo, &["init", "-q", "-b", "main"]);
			fs::write(repo.join("base.txt"), "base").unwrap();
			git(&repo, &["add", "."]);
			git(&repo, &["commit", "-q", "-m", "base"]);
			for (rel, body) in untracked {
				fs::write(repo.join(rel), body).unwrap();
			}
			repo
		}

		/// Opens `ws` with the real keymap and the workbench focused: the
		/// state a user's first keystroke finds.
		fn open(
			cx: &mut TestAppContext,
			ws: PathBuf,
			restore_dir: Option<PathBuf>,
		) -> (Entity<WorkbenchModel>, &mut VisualTestContext) {
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(Some(ws), restore_dir, "normal".into(), cx)
			});
			cx.run_until_parked();
			cx.update(|window, cx| {
				window.focus(&model.read(cx).focus_handle.clone())
			});
			(model, cx)
		}

		fn open_sized(
			cx: &mut TestAppContext,
			ws: PathBuf,
			restore_dir: Option<PathBuf>,
			w: f32,
			h: f32,
		) -> (Entity<WorkbenchModel>, &mut VisualTestContext) {
			use gpui::AppContext;
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let bounds = gpui::Bounds::new(
				gpui::Point::default(),
				gpui::size(gpui::px(w), gpui::px(h)),
			);
			let (model, window) = cx.update(|cx| {
				let window = cx
					.open_window(
						gpui::WindowOptions {
							window_bounds: Some(gpui::WindowBounds::Windowed(
								bounds,
							)),
							..Default::default()
						},
						|_, cx| {
							cx.new(|cx| {
								WorkbenchModel::new(
									Some(ws),
									restore_dir,
									"normal".into(),
									cx,
								)
							})
						},
					)
					.unwrap();
				let model = window.root(cx).unwrap();
				(model, window)
			});
			let cx = VisualTestContext::from_window(*window, cx).into_mut();
			cx.run_until_parked();
			cx.update(|window, cx| {
				window.focus(&model.read(cx).focus_handle.clone())
			});
			(model, cx)
		}

		/// Drains background work, then the 40 ms watch that moves a finished
		/// paste plan into the model: the test clock only moves when told to.
		fn settle(cx: &mut VisualTestContext) {
			for _ in 0..5 {
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}
			cx.run_until_parked();
		}

		/// arboard talks to the one OS clipboard, so tests that use it run one
		/// at a time, across threads and processes, and on Linux only under a
		/// display (CI uses xvfb-run).
		static CLIPBOARD: Mutex<()> = Mutex::new(());

		pub(crate) struct ClipboardTestGuard {
			_mutex: MutexGuard<'static, ()>,
			_file: std::fs::File,
		}

		impl Drop for ClipboardTestGuard {
			fn drop(&mut self) {
				let _ = self._file.unlock();
			}
		}

		fn clipboard() -> Option<ClipboardTestGuard> {
			let unset =
				|k: &str| std::env::var_os(k).is_none_or(|v| v.is_empty());
			if cfg!(target_os = "linux")
				&& unset("DISPLAY")
				&& unset("WAYLAND_DISPLAY")
			{
				assert!(
					std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
					"clipboard tests need DISPLAY or WAYLAND_DISPLAY"
				);
				eprintln!("skipping clipboard test: no display");
				return None;
			}
			let mutex =
				CLIPBOARD.lock().unwrap_or_else(PoisonError::into_inner);
			let lock_path =
				std::env::temp_dir().join("snip-test-os-clipboard.lock");
			let file = std::fs::OpenOptions::new()
				.read(true)
				.write(true)
				.create(true)
				.truncate(false)
				.open(&lock_path)
				.expect("open clipboard lock file");
			// Bounded: a test stuck holding the OS clipboard fails the waiter
			// with a message instead of hanging the run.
			let deadline =
				std::time::Instant::now() + std::time::Duration::from_secs(300);
			loop {
				match file.try_lock() {
					Ok(()) => break,
					Err(std::fs::TryLockError::WouldBlock)
						if std::time::Instant::now() < deadline =>
					{
						std::thread::sleep(std::time::Duration::from_millis(50))
					}
					Err(e) => panic!(
						"clipboard lock {}: {e:?} (another test held the OS \
						 clipboard for 300 s)",
						lock_path.display()
					),
				}
			}
			Some(ClipboardTestGuard {
				_mutex: mutex,
				_file: file,
			})
		}

		/// Serializes remote tests: the worker's Served git pool admits one process at a time and tests share the process.
		static REMOTE_LOCK: Mutex<()> = Mutex::new(());

		fn remote_lock() -> MutexGuard<'static, ()> {
			REMOTE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
		}

		/// A remote open goes through the shared close drain, which also
		/// waits for every git process in this test binary (GitLoad is
		/// process wide), so under parallel tests it may not end on its own
		/// here. Polled as if git were idle, the intent it queued lands.
		fn land_remote_open(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			model.update(cx, |m, cx| {
				assert!(
					!m.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu),
					"{:?}",
					m.remote.message
				);
				if m.remote.session.is_none() {
					assert_eq!(
						m.lifecycle.intent_name(),
						"open-remote-workspace"
					);
					let step = m.lifecycle.poll_at(
						std::time::Instant::now(),
						crate::lifecycle::GitLoad::idle(),
					);
					let crate::lifecycle::Step::Ready(
						crate::lifecycle::Intent::OpenRemoteWorkspace(target),
					) = step
					else {
						panic!("drain not ready: {step:?}");
					};
					// finish_intent would check the real GitLoad again.
					m.finish_open_remote(*target, cx);
				}
			});
		}

		/// A window with no workspace and an in-process worker as the only
		/// ssh host, its menu open.
		fn remote_menu(
			cx: &mut TestAppContext,
		) -> (Entity<WorkbenchModel>, &mut VisualTestContext) {
			let (_, model, cx) = remote_menu_hosts(cx, &["test-worker"]);
			(model, cx)
		}

		/// A window with its menu open and one in-process worker per name
		/// as the ssh hosts, in that order.
		fn remote_menu_hosts<'a>(
			cx: &'a mut TestAppContext,
			names: &[&str],
		) -> (
			Vec<std::sync::Arc<snip_remote::Worker>>,
			Entity<WorkbenchModel>,
			&'a mut VisualTestContext,
		) {
			let workers: Vec<_> = names
				.iter()
				.map(|_| {
					std::sync::Arc::new(snip_remote::Worker::new(
						snip_remote::WorkerOptions::default(),
					))
				})
				.collect();
			let hosts: Vec<_> = workers
				.iter()
				.zip(names)
				.map(|(worker, name)| {
					let mut host =
						snip_remote::RemoteHost::in_process(worker.clone());
					host.name = (*name).into();
					host
				})
				.collect();
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(None, None, "normal".into(), cx)
			});
			cx.run_until_parked();
			model.update(cx, |m, cx| {
				m.open_overlay(
					crate::overlay::OverlayKind::WorkspaceMenu,
					crate::overlay::ReturnFocus::Root,
					Some(crate::overlay::TriggerId::Workspace),
					None,
					cx,
				);
				m.remote.hosts = hosts;
				m.remote.recent.clear();
				cx.notify();
			});
			cx.run_until_parked();
			(workers, model, cx)
		}

		/// Clicks the row of subfolder `name` in the host's block.
		fn click_remote_folder(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			name: &str,
		) {
			let ix = model.read_with(cx, |m, _| {
				let b = m.remote.browse.as_ref().expect("a host browsed");
				let Some(Ok(listing)) = &b.listing else {
					panic!("not listed: {b:?}");
				};
				listing.folders.iter().position(|f| f == name).unwrap()
			});
			let id = format!("remote-folder:{ix}");
			click(cx, Box::leak(id.into_boxed_str()));
		}

		/// The folder shown in the host's block, and its subfolders.
		fn shown_folder(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> (String, Vec<String>) {
			model.read_with(cx, |m, _| {
				let b = m.remote.browse.as_ref().expect("a host browsed");
				let Some(Ok(listing)) = &b.listing else {
					panic!("not listed: {b:?}");
				};
				assert_eq!(b.path, listing.path);
				let mut folders = listing.folders.clone();
				folders.sort();
				(listing.path.clone(), folders)
			})
		}

		/// Settles until the one repo of the opened remote workspace has its
		/// changes loaded. Fails with a message when it never does, rather
		/// than leaving the test to a confusing panic further on.
		fn wait_remote_repo_loaded(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			let loaded = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					m.repos.len() == 1
						&& m.change_repos.first().is_some_and(|r| {
							r.state == crate::ChangeRepoState::Loaded
						})
				})
			};
			for _ in 0..50 {
				settle(cx);
				if loaded(cx) {
					return;
				}
			}
			let (repos, states) = model.read_with(cx, |m, _| {
				(
					m.repos.len(),
					m.change_repos
						.iter()
						.map(|r| r.state.clone())
						.collect::<Vec<_>>(),
				)
			});
			panic!(
				"the remote repo never loaded: {repos} repo(s), states {states:?}"
			);
		}

		/// Lists an in-process worker as the only ssh host, opens `shared`
		/// on it as the workspace, and awaits background tree and status
		/// reads.
		fn open_remote<'a>(
			cx: &'a mut TestAppContext,
			shared: &Path,
			opts: snip_remote::WorkerOptions,
		) -> (
			Entity<WorkbenchModel>,
			&'a mut VisualTestContext,
			std::sync::Arc<snip_remote::Worker>,
		) {
			let worker = std::sync::Arc::new(snip_remote::Worker::new(opts));
			let host = snip_remote::RemoteHost::in_process(worker.clone());

			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(None, None, "normal".into(), cx)
			});
			cx.run_until_parked();
			let path = shared.display().to_string();
			model.update(cx, |m, cx| {
				m.open_overlay(
					crate::overlay::OverlayKind::WorkspaceMenu,
					crate::overlay::ReturnFocus::Root,
					Some(crate::overlay::TriggerId::Workspace),
					None,
					cx,
				);
				m.remote.hosts = vec![host];
				m.remote.recent.clear();
				m.browse_remote_host(0, cx);
				m.open_remote_path(0, path, cx);
			});
			settle(cx);
			land_remote_open(&model, cx);
			for _ in 0..10 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					!m.is_loading
						&& (m.ws_tree.as_ref().is_some_and(|t| t.is_loaded)
							|| m.file_tree
								.as_ref()
								.is_some_and(|t| t.is_loaded)
							|| m.remote.scan_error.is_some())
						&& (m.repos.is_empty()
							|| m.change_repos.is_empty()
							|| m.change_repos[0].state
								!= crate::ChangeRepoState::Loading)
				});
				if done {
					break;
				}
			}
			(model, cx, worker)
		}

		/// Remote workspaces end to end in one process: a worker listed as
		/// the only host, its home listing, the folder opened by path, the
		/// Project tree and a file preview, all read through the worker.
		/// Every read has a deadline (snip-remote), so a hang fails rather
		/// than blocks.
		/// A remote listing the byte budget cuts short finishes through the
		/// ORDINARY LoadMore path: Expand fetches the whole listing once,
		/// the budget admits four of ten, and「顯示更多」walks the held
		/// tail to the end — no refetch, no name lost or doubled.
		#[gpui::test]
		fn a_remote_listing_cut_by_the_budget_finishes_through_load_more(
			_cx: &mut TestAppContext,
		) {
			use crate::tree::{
				execute_tree_io, listed_tree_result, FileTreeNode, ListedChild,
				NodeKey, TreeCommand, TreeEffect, TreeIo, TreeIoKind,
			};
			use snip_core::gitrun::CancelToken;
			use std::collections::VecDeque;
			let root = PathBuf::from("snip-remote://test/ws");
			let children: Vec<ListedChild> = (0..10)
				.map(|i| ListedChild {
					name: format!("f{i:02}"),
					utf8: true,
					directory: false,
					nested_repo: false,
				})
				.collect();
			let mut tree = FileTreeNode::unloaded_root(&root);

			// One row's cost, measured on a directly built io: the tree's
			// command state is not what this measures, and one Expand is
			// all an unloaded root will schedule.
			let probe = listed_tree_result(
				TreeIo {
					key: NodeKey::root(),
					epoch: 0,
					dir: root.clone(),
					base: root.clone(),
					depth: 1,
					byte_budget: usize::MAX,
					scan: None,
					held: VecDeque::new(),
					replace_children: true,
					kind: TreeIoKind::Expand,
				},
				Ok((children[..1].to_vec(), false)),
			);
			let cost = probe.children[0].retained_bytes();

			let TreeEffect::Io(mut io) =
				tree.start(TreeCommand::Expand(NodeKey::root()))
			else {
				panic!("expand must schedule io");
			};
			io.byte_budget = cost.saturating_mul(4).saturating_add(64);
			let first = listed_tree_result(io, Ok((children, false)));
			tree.apply_io_result(first).expect("the expand applies");
			assert_eq!(tree.children.len(), 4, "the budget admits four of ten");
			assert!(tree.has_more, "the rest is held for LoadMore");
			assert!(
				tree.flatten_visible(100).iter().any(|r| r.is_more_marker),
				"the「顯示更多」marker is visible"
			);

			// LoadMore through the real command path, with the workbench's
			// own budget logic.
			let TreeEffect::Io(io) =
				tree.start(TreeCommand::LoadMore(NodeKey::root()))
			else {
				panic!("load more must schedule io from the held tail");
			};
			let rest = execute_tree_io(io, &CancelToken::new());
			tree.apply_io_result(rest)
				.expect("the continuation applies");
			assert!(!tree.has_more);
			let names: Vec<&str> =
				tree.children.iter().map(|c| c.name.as_str()).collect();
			assert_eq!(names.len(), 10, "{names:?}");
			assert_eq!(
				names,
				[
					"f00", "f01", "f02", "f03", "f04", "f05", "f06", "f07",
					"f08", "f09"
				],
				"every name exactly once, in order"
			);
		}

		/// A 1,200-entry remote listing shows every name through LoadMore:
		/// the first Expand admits one page, the held tail carries the rest,
		/// and no name is lost or doubled on the way.
		#[gpui::test]
		fn a_remote_listing_of_1200_shows_every_name_through_load_more(
			_cx: &mut TestAppContext,
		) {
			use crate::tree::{
				execute_tree_io, listed_tree_result, FileTreeNode, ListedChild,
				NodeKey, TreeCommand, TreeEffect, MAX_DIR_ENTRIES,
			};
			use snip_core::gitrun::CancelToken;
			use std::collections::HashSet;
			let root = PathBuf::from("snip-remote://test/ws");
			let children: Vec<ListedChild> = (0..1200)
				.map(|i| ListedChild {
					name: format!("f{i:04}"),
					utf8: true,
					directory: false,
					nested_repo: false,
				})
				.collect();
			let mut tree = FileTreeNode::unloaded_root(&root);

			// The budget is not what this test is about (AGENTS: build the
			// state rather than rely on how many names fit), so each batch
			// runs with the budget open.
			let TreeEffect::Io(mut io) =
				tree.start(TreeCommand::Expand(NodeKey::root()))
			else {
				panic!("expand must schedule io");
			};
			io.byte_budget = usize::MAX;
			let result = listed_tree_result(io, Ok((children, false)));
			tree.apply_io_result(result).expect("the expand applies");
			assert_eq!(
				tree.children.len(),
				MAX_DIR_ENTRIES,
				"the first batch is the ordinary page"
			);
			assert!(tree.has_more);

			let mut guard = 0;
			while tree.has_more {
				guard += 1;
				assert!(guard < 30, "load more must converge");
				let TreeEffect::Io(mut io) =
					tree.start(TreeCommand::LoadMore(NodeKey::root()))
				else {
					panic!("load more must schedule io while names remain");
				};
				io.byte_budget = usize::MAX;
				let result = execute_tree_io(io, &CancelToken::new());
				tree.apply_io_result(result)
					.expect("a continuation applies");
			}
			assert_eq!(tree.children.len(), 1200);
			let seen: HashSet<&str> =
				tree.children.iter().map(|c| c.name.as_str()).collect();
			assert_eq!(seen.len(), 1200, "no duplicates");
			assert!(tree.children[0].name == "f0000");
			assert!(tree.children[1199].name == "f1199");
		}

		#[gpui::test]
		fn a_nested_remote_listing_of_6000_survives_cache_reclaim(
			_cx: &mut TestAppContext,
		) {
			use crate::tree::{
				execute_tree_io, listed_tree_result, FileTreeNode, ListedChild,
				NodeKey, TreeCommand, TreeEffect,
				MAX_RETAINED_WORKING_TREE_BYTES,
			};
			use snip_core::gitrun::CancelToken;
			use std::collections::HashSet;
			let root = PathBuf::from("snip-remote://test/ws");
			let mut tree = FileTreeNode::unloaded_root(&root);
			let TreeEffect::Io(io) =
				tree.start(TreeCommand::Expand(NodeKey::root()))
			else {
				panic!("root expansion must schedule io");
			};
			tree.apply_io_result(listed_tree_result(
				io,
				Ok((
					vec![ListedChild {
						name: "nested".into(),
						utf8: true,
						directory: true,
						nested_repo: false,
					}],
					false,
				)),
			))
			.unwrap();
			let key = NodeKey::from_utf8_rel("nested");
			let TreeEffect::Io(io) =
				tree.start(TreeCommand::Expand(key.clone()))
			else {
				panic!("nested expansion must schedule io");
			};
			let listing = (0..6000)
				.map(|i| ListedChild {
					name: format!("f{i:04}"),
					utf8: true,
					directory: false,
					nested_repo: false,
				})
				.collect();
			tree.apply_io_result(listed_tree_result(io, Ok((listing, false))))
				.unwrap();
			assert!(
				tree.children[0].is_expanded,
				"cache reclaim must not collapse the listing"
			);
			assert!(
				tree.retained_bytes() > tree.cache_bytes(),
				"the remote tail still counts toward aggregate storage"
			);
			assert!(!tree.children[0].children.is_empty());
			assert!(tree
				.flatten_visible(tree.visible_limit())
				.iter()
				.any(|row| row.rel_path == "nested/f0000"));
			tree.toggle_select("nested/f0000");
			let mut seen = HashSet::new();
			for page in 0..100 {
				assert!(tree.cache_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
				let folder = &tree.children[0];
				assert!(folder.is_expanded && folder.read_error.is_none());
				seen.extend(
					folder.children.iter().map(|node| node.name.clone()),
				);
				if !folder.has_more {
					break;
				}
				assert!(page < 99, "Load More must reach all 6000 names");
				assert!(tree
					.flatten_visible(tree.visible_limit())
					.iter()
					.any(|row| row.is_more_marker && row.rel_path == "nested"));
				let TreeEffect::Io(io) =
					tree.start(TreeCommand::LoadMore(key.clone()))
				else {
					panic!("Load More must retain the remote tail");
				};
				tree.apply_io_result(execute_tree_io(io, &CancelToken::new()))
					.unwrap();
			}
			assert_eq!(seen.len(), 6000);
			assert!((0..6000).all(|i| seen.contains(&format!("f{i:04}"))));
			assert!(tree
				.selected_paths()
				.contains(&"nested/f0000".to_string()));
		}

		#[gpui::test]
		fn an_empty_remote_listing_never_reads_the_master_directory(
			_cx: &mut TestAppContext,
		) {
			use crate::tree::{
				listed_tree_result, FileTreeNode, NodeKey, TreeCommand,
				TreeEffect,
			};
			let tmp = tempfile::tempdir().unwrap();
			fs::write(tmp.path().join("master-only.txt"), "local").unwrap();
			let mut tree = FileTreeNode::unloaded_root(tmp.path());
			for command in [
				TreeCommand::Expand(NodeKey::root()),
				TreeCommand::Retry(NodeKey::root()),
			] {
				let TreeEffect::Io(io) = tree.start(command) else {
					panic!("listing must schedule io");
				};
				tree.apply_io_result(listed_tree_result(
					io,
					Ok((Vec::new(), false)),
				))
				.unwrap();
				assert!(tree.is_loaded && tree.is_expanded);
				assert!(
					tree.children.is_empty(),
					"the worker's empty answer must stay empty"
				);
				assert!(tree.read_error.is_none() && !tree.has_more);
				assert!(tree.flatten_visible(tree.visible_limit()).is_empty());
				assert!(matches!(
					tree.start(TreeCommand::LoadMore(NodeKey::root())),
					TreeEffect::Idle
				));
			}
		}

		#[gpui::test]
		fn remote_workspace_opens_a_host_folder_and_previews_through_a_worker(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			use crate::tree::{NodeKey, TreeCommand};
			use snip_core::transfer::SourceKind;
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(shared.join("src")).unwrap();
			fs::write(shared.join("src/main.rs"), "fn main() {}\n").unwrap();
			fs::write(shared.join("README.md"), "# remote\n").unwrap();
			// A folder symlink inside the share is a folder in the tree.
			#[cfg(unix)]
			std::os::unix::fs::symlink(
				shared.join("src"),
				shared.join("zz-link"),
			)
			.unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "win-worker".into(),
					..Default::default()
				},
			);

			model.read_with(cx, |m, _| {
				assert_eq!(m.remote.message, None);
				assert_eq!(m.remote.hosts.len(), 1);
				// Opening from the menu ends the menu's browse (#187).
				assert!(m.remote.browse.is_none(), "browse ends with the open");
				let session = m.remote.session.as_ref().expect("open");
				assert_eq!(session.workspace.name, "shared");
				assert_eq!(m.remote.recent[0].path, session.workspace.id);

				assert!(
					m.workspace_open
						&& !m.overlays.is_open(
							crate::overlay::OverlayKind::WorkspaceMenu
						)
				);
				assert_eq!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::NoRepository)
				);
				let tree = m.ws_tree.as_ref().expect("remote tree");
				assert!(tree.is_loaded, "root listed: {tree:?}");
				let names: Vec<_> =
					tree.children.iter().map(|c| c.name.as_str()).collect();
				#[cfg(unix)]
				assert_eq!(names, ["src", "zz-link", "README.md"]);
				#[cfg(not(unix))]
				assert_eq!(names, ["src", "README.md"]);
			});

			#[cfg(unix)]
			{
				model.update(cx, |m, cx| {
					m.dispatch_ws_tree(
						Some(TreeCommand::Expand(NodeKey::from_utf8_rel(
							"zz-link",
						))),
						cx,
					);
				});
				settle(cx);
				model.read_with(cx, |m, _| {
					let tree = m.ws_tree.as_ref().unwrap();
					let link = &tree.children[1];
					assert!(link.is_dir && link.is_loaded, "link: {link:?}");
					let names: Vec<_> = link
						.children
						.iter()
						.map(|c| c.rel_path.as_str())
						.collect();
					assert_eq!(names, ["zz-link/main.rs"]);
				});
			}

			model.update(cx, |m, cx| {
				m.dispatch_ws_tree(
					Some(TreeCommand::Expand(NodeKey::from_utf8_rel("src"))),
					cx,
				);
			});
			settle(cx);
			model.update(cx, |m, cx| {
				let tree = m.ws_tree.as_ref().unwrap();
				let src = &tree.children[0];
				assert!(src.is_loaded, "src listed: {src:?}");
				assert_eq!(src.children[0].rel_path, "src/main.rs");
				let root = m.ws_root();
				m.select_file_in(root, "src/main.rs", SourceKind::File, cx);
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("remote preview");
				assert_eq!(&*p.text, "fn main() {}\n");
				assert_eq!(m.preview_error, None);
				// The breadcrumb names the host, never the internal root.
				let root = m.ws_root().unwrap();
				assert_eq!(m.log_repo_name(&root), "test-worker ▸ shared");
			});

			// Refresh re-reads the open file: deleted on the worker, it shows
			// an error, not its old text.
			model.update(cx, |m, cx| {
				let root = m.ws_root();
				m.select_file_in(root, "README.md", SourceKind::File, cx);
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("README preview");
				assert_eq!(&*p.text, "# remote\n");
			});
			fs::remove_file(shared.join("README.md")).unwrap();
			model.update(cx, |m, cx| m.reload_repos(cx));
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					m.preview_error.is_some(),
					"deleted file shown as error"
				);
				assert!(
					m.preview.as_ref().is_none_or(|p| &*p.text != "# remote\n"),
					"old text left in place"
				);
				let tree = m.ws_tree.as_ref().unwrap();
				assert!(tree.children.iter().all(|c| c.name != "README.md"));
			});
			fs::write(shared.join("README.md"), "# remote\n").unwrap();

			// A name that is not UTF-8 cannot be opened; the click says so.
			model.update(cx, |m, cx| m.refuse_unaddressable_row(cx));
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "tree_name_not_utf8");
			});

			// The folder goes away on the worker: the next read fails.
			fs::rename(&shared, shared.with_extension("gone")).unwrap();
			model.update(cx, |m, cx| {
				let root = m.ws_root();
				m.select_file_in(root, "README.md", SourceKind::File, cx);
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.preview_error.is_some(), "refused read shown");
			});
		}

		/// Clicking a folder of a host enters it and opens nothing; "up one
		/// level" goes back; "open this folder" opens the folder shown. The
		/// temp folder is reached through macOS's `/var` symlink, so the
		/// worker's real path differs from the one asked for.
		#[gpui::test]
		fn remote_folder_browser_enters_goes_up_and_opens_here(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let base = tmp.path().join("base");
			for d in ["a/x", "a/y", "b", ".hidden"] {
				fs::create_dir_all(base.join(d)).unwrap();
			}
			fs::write(base.join("file.txt"), "f").unwrap();
			let real = |p: &Path| {
				dunce::canonicalize(p).unwrap().display().to_string()
			};
			let (model, cx) = remote_menu(cx);

			model.update(cx, |m, cx| {
				m.browse_remote_folder(0, base.display().to_string(), cx);
			});
			settle(cx);
			let (path, folders) = shown_folder(&model, cx);
			assert_eq!(path, real(&base));
			assert_eq!(folders, ["a", "b"], "dot folders and files hidden");
			model.read_with(cx, |m, cx| {
				assert_eq!(m.remote_path_input.read(cx).text(), real(&base));
			});

			click_remote_folder(&model, cx, "a");
			settle(cx);
			let (path, folders) = shown_folder(&model, cx);
			assert_eq!(path, real(&base.join("a")));
			assert_eq!(folders, ["x", "y"]);
			model.read_with(cx, |m, cx| {
				assert!(m.remote.session.is_none(), "entering opens nothing");
				assert!(
					!m.workspace_open
						&& m.overlays.is_open(
							crate::overlay::OverlayKind::WorkspaceMenu
						)
				);
				assert_eq!(m.lifecycle.intent_name(), "none");
				assert_eq!(
					m.remote_path_input.read(cx).text(),
					real(&base.join("a"))
				);
			});

			click(cx, "remote-up");
			settle(cx);
			let (path, folders) = shown_folder(&model, cx);
			assert_eq!(path, real(&base));
			assert_eq!(folders, ["a", "b"]);

			click_remote_folder(&model, cx, "b");
			settle(cx);
			click(cx, "btn-remote-open-here");
			settle(cx);
			land_remote_open(&model, cx);
			settle(cx);
			model.read_with(cx, |m, _| {
				let session = m.remote.session.as_ref().expect("opened");
				assert_eq!(session.workspace.id, real(&base.join("b")));
				assert_eq!(m.remote.recent[0].path, session.workspace.id);
				assert!(m.workspace_open);
			});
		}

		/// A failed open from the menu keeps the browse's connection (#198).
		#[gpui::test]
		fn a_failed_menu_open_keeps_the_browse_connection(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let base = tmp.path().join("base");
			fs::create_dir_all(base.join("a")).unwrap();
			let (_, model, cx) = remote_menu_hosts(cx, &["test-worker"]);
			click(cx, "remote-host:0");
			settle(cx);
			model.update(cx, |m, cx| {
				m.browse_remote_folder(0, base.display().to_string(), cx)
			});
			settle(cx);
			let client_of = |m: &WorkbenchModel| {
				m.remote.browse.as_ref().and_then(|b| b.client_for_tests())
			};
			let c = model
				.read_with(cx, |m, _| client_of(m))
				.expect("browse client");
			let missing =
				tmp.path().join("no-such-folder").display().to_string();
			model.update(cx, |m, cx| {
				m.remote_path_input
					.update(cx, |f, cx| f.set_text(&missing, cx))
			});
			click(cx, "btn-remote-open");
			settle_until(cx, "the failed open", |cx| {
				model.read_with(cx, |m, _| {
					!m.remote.busy
						&& matches!(m.remote.message, Some((false, _)))
				})
			});
			model.read_with(cx, |m, _| {
				assert!(m.remote.session.is_none());
				assert!(
					m.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu),
					"the menu stays open"
				);
				let back =
					client_of(m).expect("the browse got its client back");
				assert!(std::sync::Arc::ptr_eq(&back, &c), "same connection");
			});
			click_remote_folder(&model, cx, "a");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					std::sync::Arc::ptr_eq(&client_of(m).unwrap(), &c),
					"folder click reuses it"
				)
			});
		}

		/// A host without snip, picked from the menu: the listing and the
		/// failed open both say so in the interface language, not in the
		/// English text the CLI prints (#204).
		#[cfg(unix)]
		#[gpui::test]
		fn a_host_without_snip_is_reported_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_, model, cx) = remote_menu_hosts(cx, &["bare-host"]);
			model.update(cx, |m, cx| {
				m.remote.hosts = vec![snip_remote::RemoteHost {
					name: "bare-host".into(),
					transport: snip_remote::Transport::Command(vec![
						"sh".into(),
						"-c".into(),
						"echo 'sh: snip: not found' >&2; exit 127".into(),
					]),
				}];
				cx.notify();
			});
			cx.run_until_parked();
			click(cx, "remote-host:0");
			settle_until(cx, "the listing to fail", |cx| {
				model.read_with(cx, |m, _| {
					m.remote
						.browse
						.as_ref()
						.is_some_and(|b| matches!(b.listing, Some(Err(_))))
				})
			});
			let zh = "那台機器沒有安裝 snip";
			model.read_with(cx, |m, _| {
				let Some(Err(text)) =
					&m.remote.browse.as_ref().unwrap().listing
				else {
					panic!("the listing failed");
				};
				let text = text.render(m.locale);
				assert!(text.starts_with(zh), "{text}");
				assert!(!text.contains("not installed"), "{text}");
			});
			// The open of a typed path fails the same way.
			model.update(cx, |m, cx| {
				m.remote_path_input.update(cx, |f, cx| f.set_text("/x", cx))
			});
			click(cx, "btn-remote-open");
			settle_until(cx, "the failed open", |cx| {
				model.read_with(cx, |m, _| {
					!m.remote.busy
						&& matches!(m.remote.message, Some((false, _)))
				})
			});
			model.read_with(cx, |m, _| {
				let Some((_, text)) = &m.remote.message else {
					panic!("a failure message");
				};
				let text = text.render(m.locale);
				assert!(text.starts_with(zh), "{text}");
				assert_eq!(m.status.key, "remote_open_failed");
				let status = m.status.render(m.locale);
				assert!(status.contains(zh), "{status}");
			});
		}

		/// A restored remote tab whose host has no snip: the status line, the
		/// menu's note, the restore's reason and the tab's hover all word the
		/// reason when read, so a switch to English rewords every one (#206
		/// #208). The reason used to be fixed when the open failed.
		#[cfg(unix)]
		#[gpui::test]
		fn restoring_a_host_without_snip_follows_a_language_switch(
			cx: &mut TestAppContext,
		) {
			use crate::remote::RecentRemote;
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let id = dunce::canonicalize(tmp.path())
				.unwrap()
				.display()
				.to_string();
			let (_, model, cx) = remote_menu_hosts(cx, &["bare-host"]);
			model.update(cx, |m, cx| {
				m.remote.hosts = vec![snip_remote::RemoteHost {
					name: "bare-host".into(),
					transport: snip_remote::Transport::Command(vec![
						"sh".into(),
						"-c".into(),
						"echo 'sh: snip: not found' >&2; exit 127".into(),
					]),
				}];
				m.restore_remote(
					RecentRemote {
						host: "bare-host".into(),
						path: id,
					},
					cx,
				);
			});
			settle_until(cx, "the restore to fail", |cx| {
				model.read_with(cx, |m, _| {
					m.remote
						.restoring
						.as_ref()
						.is_some_and(|r| r.failed.is_some())
				})
			});
			let zh = "那台機器沒有安裝 snip";
			let en = "is not installed";
			// Every surface, worded now in the current language.
			let read = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					let note =
						m.remote.message.as_ref().expect("a note").1.clone();
					let failed = m
						.remote
						.restoring
						.as_ref()
						.and_then(|r| r.failed.clone())
						.expect("a reason");
					let hover =
						crate::tabs::tab_hover("tab", &m.tab_info(), m.locale);
					[
						m.status.render(m.locale),
						note.render(m.locale),
						failed.render(m.locale),
						hover,
					]
				})
			};
			assert_eq!(status_key(&model, cx), "remote_open_failed");
			for text in read(cx) {
				assert!(text.contains(zh), "{text}");
				assert!(!text.contains(en), "{text}");
			}
			cx.simulate_keystrokes("alt-l");
			assert_eq!(locale_now(&model, cx), crate::i18n::Locale::En);
			for text in read(cx) {
				assert!(text.contains(en), "{text}");
				assert!(!text.contains(zh), "{text}");
			}
			// What the status bar drew follows too.
			let drawn = crate::ui::last_drawn("status-text").unwrap();
			assert!(drawn.contains(en), "{drawn}");
		}

		/// One browse connection per menu: it is handed to the open of the
		/// folder shown, and it ends with the menu (#187).
		#[gpui::test]
		fn menu_browse_keeps_one_client_hands_it_to_the_open_and_ends_with_the_menu(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let base = tmp.path().join("base");
			for d in ["a", "b"] {
				fs::create_dir_all(base.join(d)).unwrap();
			}
			let (_, model, cx) = remote_menu_hosts(cx, &["test-worker"]);

			click(cx, "remote-host:0");
			settle(cx);
			let client_of = |m: &WorkbenchModel| {
				m.remote
					.browse
					.as_ref()
					.and_then(|b| b.client_for_tests())
					.expect("the browse keeps its client")
			};
			model.read_with(cx, |m, _| {
				let b = m.remote.browse.as_ref().expect("host browsed");
				assert!(matches!(b.listing, Some(Ok(_))), "{:?}", b.listing);
			});

			// Into a subfolder, as a user does: the same connection.
			model.update(cx, |m, cx| {
				m.browse_remote_folder(0, base.display().to_string(), cx);
			});
			settle(cx);
			let c = model.read_with(cx, |m, _| client_of(m));
			click_remote_folder(&model, cx, "a");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					std::sync::Arc::ptr_eq(&client_of(m), &c),
					"same connection"
				);
			});

			// Opening the folder shown reuses it, and the menu's browse ends.
			click(cx, "btn-remote-open-here");
			settle(cx);
			land_remote_open(&model, cx);
			settle(cx);
			model.read_with(cx, |m, _| {
				let session = m.remote.session.as_ref().expect("opened");
				assert!(
					std::sync::Arc::ptr_eq(&session.client, &c),
					"open reuses it"
				);
				assert!(m.remote.browse.is_none(), "the menu's browse ended");
			});

			// A new menu browses again; Escape ends it, and its connection
			// is dropped with it.
			model.update(cx, |m, cx| m.toggle_workspace_menu(cx));
			click(cx, "remote-host:0");
			settle(cx);
			let weak = model
				.read_with(cx, |m, _| std::sync::Arc::downgrade(&client_of(m)));
			cx.simulate_keystrokes("escape");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					!m.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu),
					"Escape closes the menu"
				);
				assert!(m.remote.browse.is_none(), "browse ends with the menu");
			});
			let mut rounds = 0;
			while weak.upgrade().is_some() {
				assert!(rounds < 50, "the browse's client is still alive");
				settle(cx);
				rounds += 1;
			}
		}

		/// The path field shows the host being browsed, at once; a landing
		/// listing fills it only while the user has not typed (#190).
		#[gpui::test]
		fn remote_path_field_follows_the_host_being_browsed(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_, model, cx) = remote_menu_hosts(cx, &["one", "two"]);
			let field = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, cx| {
					m.remote_path_input.read(cx).text().to_string()
				})
			};

			click(cx, "remote-host:0");
			settle(cx);
			let (home, _) = shown_folder(&model, cx);
			assert_eq!(field(cx), home, "one's home path");

			cx.simulate_input("/typed");
			assert!(field(cx).ends_with("/typed"), "typed: {}", field(cx));

			// Host two: the field shows "~" the moment the browse starts, before
			// any listing can land (a click would drain the test clock first).
			model.update(cx, |m, cx| m.browse_remote_host(1, cx));
			assert_eq!(field(cx), "~", "shown at once, before the listing");
			settle(cx);
			let (two, _) = shown_folder(&model, cx);
			assert_eq!(field(cx), two, "two's listing path");

			click(cx, "remote-host:0");
			settle(cx);
			let (one_again, _) = shown_folder(&model, cx);
			assert_eq!(field(cx), one_again, "one's listing path again");

			// Typing while a listing is in flight survives its landing.
			model.update(cx, |m, cx| m.browse_remote_host(1, cx));
			cx.simulate_input("x");
			settle(cx);
			assert_eq!(field(cx), "~x", "the landing kept the typed text");
		}

		/// A host with many folders in a small window: the menu stays inside
		/// the window and the path field and both open buttons stay
		/// reachable, the folder list scrolling on its own.
		#[gpui::test]
		fn remote_menu_with_many_folders_fits_a_small_window(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let base = tmp.path().join("home");
			for i in 0..43 {
				fs::create_dir_all(base.join(format!("folder-{i:02}")))
					.unwrap();
			}
			let (model, cx) = remote_menu(cx);
			cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(600.)));
			model.update(cx, |m, cx| {
				m.browse_remote_folder(0, base.display().to_string(), cx);
			});
			settle(cx);
			for _ in 0..2 {
				// ALLOWED-TEST-REFRESH: test harness needs a redraw to recalculate workspace-menu bounds after resize
				cx.update(|window, _| window.refresh());
				settle(cx);
			}
			assert_eq!(shown_folder(&model, cx).1.len(), 43);
			let bottom = |b: gpui::Bounds<gpui::Pixels>| {
				f32::from(b.origin.y) + f32::from(b.size.height)
			};
			let menu = cx.debug_bounds("workspace-menu").expect("menu drawn");
			assert!(bottom(menu) <= 600., "menu runs off the window: {menu:?}");
			for id in [
				"remote-path-input",
				"btn-remote-open",
				"btn-remote-open-here",
			] {
				let b = cx
					.debug_bounds(id)
					.unwrap_or_else(|| panic!("{id} not drawn"));
				assert!(
					f32::from(b.origin.y) >= f32::from(menu.origin.y)
						&& bottom(b) <= bottom(menu),
					"{id} {b:?} outside the menu {menu:?}"
				);
			}
		}

		/// Ten recent workspaces and a browsed host in a 1080×720 window
		/// (WT40, #147): the recent rows keep their full height, so name
		/// and path do not overlap, and the host's folder list is not
		/// squeezed to nothing, its first folder visible and clickable.
		#[gpui::test]
		fn many_recents_leave_room_for_the_remote_folder_list(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let base = tmp.path().join("home");
			for i in 0..43 {
				fs::create_dir_all(base.join(format!("folder-{i:02}")))
					.unwrap();
			}
			let recents: Vec<PathBuf> = (0..10)
				.map(|i| {
					let d = tmp.path().join(format!("recent-{i:02}"));
					fs::create_dir_all(&d).unwrap();
					d
				})
				.collect();
			let (model, cx) = remote_menu(cx);
			cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(720.)));
			model.update(cx, |m, cx| {
				m.recent_workspaces = recents;
				m.browse_remote_folder(0, base.display().to_string(), cx);
				cx.notify();
			});
			settle(cx);
			assert_eq!(shown_folder(&model, cx).1.len(), 43);
			let bottom = |b: gpui::Bounds<gpui::Pixels>| {
				f32::from(b.origin.y) + f32::from(b.size.height)
			};
			let menu = cx.debug_bounds("workspace-menu").expect("menu drawn");
			for ix in 0..10 {
				let id: &'static str = Box::leak(
					format!("workspace-recent:{ix}").into_boxed_str(),
				);
				let row = cx
					.debug_bounds(id)
					.unwrap_or_else(|| panic!("{id} not drawn"));
				assert!(
					f32::from(row.size.height) >= 38.,
					"{id} squeezed to {row:?}: name and path overlap"
				);
			}
			let first = cx
				.debug_bounds("remote-folder:0")
				.expect("remote-folder:0 drawn");
			assert!(
				f32::from(first.size.height) >= 24.,
				"remote-folder:0 squeezed to {first:?}"
			);
			assert!(
				f32::from(first.origin.y) >= f32::from(menu.origin.y)
					&& bottom(first) <= bottom(menu)
					&& bottom(menu) <= 720.,
				"remote-folder:0 {first:?} outside the menu {menu:?}"
			);
			let list = cx
				.debug_bounds("remote-folders")
				.expect("remote folder list drawn");
			assert!(
				f32::from(list.size.height) >= 3. * 28.,
				"the folder list keeps at least three rows: {list:?}"
			);
			click_remote_folder(&model, cx, "folder-00");
			settle(cx);
			assert!(
				shown_folder(&model, cx).0.ends_with("folder-00"),
				"clicking remote-folder:0 opens it"
			);
		}

		/// An open host row has no tooltip (#203): it would cover the
		/// open-here button of the folder list below it, even when the click
		/// came before the tooltip showed.
		#[gpui::test]
		fn an_opened_host_row_has_no_tooltip(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let (_, _model, cx) = remote_menu_hosts(cx, &["one", "two"]);
			cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(720.)));
			settle(cx);
			let shown = |cx: &mut VisualTestContext| {
				cx.run_until_parked();
				cx.debug_bounds("tooltip:one").is_some()
			};
			let at = cx
				.debug_bounds("remote-host:0")
				.expect("row drawn")
				.center();
			cx.simulate_mouse_move(at, None, gpui::Modifiers::none());
			click(cx, "remote-host:0");
			cx.executor().advance_clock(Duration::from_millis(700));
			settle(cx);
			assert!(
				cx.debug_bounds("btn-remote-open-here").is_some(),
				"the host's folder block is open"
			);
			assert!(!shown(cx), "the opened row keeps no tooltip");
		}

		/// Opening a host's folder list and switching hosts never moves the
		/// other host rows: the recent box keeps four whole rows (#197).
		#[gpui::test]
		fn workspace_menu_host_rows_keep_their_place(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let base = tmp.path().join("home");
			for i in 0..43 {
				fs::create_dir_all(base.join(format!("folder-{i:02}")))
					.unwrap();
			}
			let recents: Vec<PathBuf> = (0..6)
				.map(|i| {
					let d = tmp.path().join(format!("recent-{i:02}"));
					fs::create_dir_all(&d).unwrap();
					d
				})
				.collect();
			let (_, model, cx) = remote_menu_hosts(cx, &["one", "two"]);
			cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(720.)));
			model.update(cx, |m, cx| {
				m.recent_workspaces = recents;
				cx.notify();
			});
			settle(cx);
			let y = |cx: &mut VisualTestContext, id: &'static str| {
				f32::from(
					cx.debug_bounds(id)
						.unwrap_or_else(|| panic!("{id} not drawn"))
						.origin
						.y,
				)
			};
			let h = |cx: &mut VisualTestContext, id: &'static str| {
				f32::from(
					cx.debug_bounds(id)
						.unwrap_or_else(|| panic!("{id} not drawn"))
						.size
						.height,
				)
			};
			let one = y(cx, "remote-host:0");
			let two = y(cx, "remote-host:1");
			assert_eq!(h(cx, "workspace-recents"), 4. * 38.);

			click(cx, "remote-host:0");
			settle(cx);
			model.update(cx, |m, cx| {
				m.browse_remote_folder(0, base.display().to_string(), cx);
			});
			settle(cx);
			assert_eq!(y(cx, "remote-host:0"), one, "host one moved");
			assert_eq!(h(cx, "workspace-recents"), 4. * 38.);
			assert!(
				y(cx, "remote-host:1") > two,
				"host two should sit below the open block"
			);

			click(cx, "remote-host:1");
			settle(cx);
			assert_eq!(y(cx, "remote-host:0"), one, "host one moved");
			assert_eq!(y(cx, "remote-host:1"), two, "host two moved");
			assert_eq!(h(cx, "workspace-recents"), 4. * 38.);
		}

		/// A listing that lands for a folder or host no longer shown is
		/// dropped.
		#[gpui::test]
		fn remote_folder_listing_for_a_folder_no_longer_shown_is_dropped(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			fs::create_dir_all(tmp.path().join("a")).unwrap();
			let (model, cx) = remote_menu(cx);
			model.update(cx, |m, cx| {
				m.browse_remote_folder(0, tmp.path().display().to_string(), cx);
			});
			settle(cx);
			let before = shown_folder(&model, cx);
			model.update(cx, |m, cx| {
				let seq = m.remote.browse.as_ref().unwrap().seq();
				m.remote_listing_landed(
					seq.wrapping_sub(1),
					Ok(crate::remote::FolderListing {
						path: "/elsewhere".into(),
						folders: vec!["zzz".into()],
					}),
					cx,
				);
			});
			assert_eq!(shown_folder(&model, cx), before);

			// Asked for while another listing was on its way: only the newer
			// one lands.
			model.update(cx, |m, cx| {
				m.enter_remote_folder("a", cx);
				let stale = m.remote.browse.as_ref().unwrap().seq();
				m.remote_up(cx);
				m.remote_listing_landed(
					stale,
					Err(crate::i18n::Msg::new(
						"error_repo_status",
						["late failure".to_string()],
					)),
					cx,
				);
				assert_eq!(m.remote.browse.as_ref().unwrap().listing, None);
			});
			settle(cx);
			assert_eq!(shown_folder(&model, cx).0, before.0.clone());
		}

		/// A remote folder opened before is listed in the menu's top-level
		/// recent list, after the local ones, and one click reopens it.
		#[gpui::test]
		fn remote_recent_is_in_the_top_level_list_and_reopens(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let local = tmp.path().join("local");
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&local).unwrap();
			fs::create_dir_all(&shared).unwrap();
			let shared_id =
				dunce::canonicalize(&shared).unwrap().display().to_string();
			let (model, cx) = remote_menu(cx);
			model.update(cx, |m, cx| {
				m.recent_workspaces = vec![local.clone()];
				m.remote.recent = vec![crate::remote::RecentRemote {
					host: m.remote.hosts[0].name.clone(),
					path: shared_id.clone(),
				}];
				cx.notify();
			});
			settle(cx);
			let local_row = cx
				.debug_bounds("workspace-recent:0")
				.expect("local recent listed");
			let remote_row = cx
				.debug_bounds("remote-recent:0")
				.expect("remote recent listed at the top level");
			assert!(
				local_row.origin.y < remote_row.origin.y,
				"after the local ones"
			);
			click(cx, "remote-recent:0");
			settle(cx);
			land_remote_open(&model, cx);
			settle(cx);
			model.read_with(cx, |m, _| {
				let session = m.remote.session.as_ref().expect("reopened");
				assert_eq!(session.workspace.id, shared_id);
				assert_eq!(m.remote.recent.len(), 1);
			});
		}

		/// A restored remote tab connects in the background; a folder or
		/// host that is gone keeps the tab on that folder and says why, on
		/// the status bar and the empty screen.
		#[gpui::test]
		fn a_restored_remote_tab_reconnects_or_says_why_not(
			cx: &mut TestAppContext,
		) {
			use crate::remote::RecentRemote;
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let id = dunce::canonicalize(tmp.path())
				.unwrap()
				.display()
				.to_string();
			let (model, cx) = remote_menu(cx);
			let host = model.update(cx, |m, _| {
				m.drop_overlays(crate::overlay::DropScope::Floating);
				m.remote.hosts[0].name.clone()
			});

			model.update(cx, |m, cx| {
				m.restore_remote(
					RecentRemote {
						host: "gone-host".into(),
						path: id.clone(),
					},
					cx,
				);
				assert_eq!(m.status.key, "remote_open_failed");
				assert!(!m.workspace_open && !m.remote.busy);
				let r = m.remote.restoring.as_ref().expect("the tab stays");
				assert!(r
					.failed
					.as_ref()
					.is_some_and(|w| w.render(m.locale).contains("gone-host")));
				assert_eq!(
					m.ws_identity(),
					Some(crate::tabs::WsIdentity::Remote {
						host: "gone-host".into(),
						path: id.clone(),
					})
				);
			});

			model.update(cx, |m, cx| {
				m.restore_remote(
					RecentRemote {
						host: host.clone(),
						path: format!("{id}/missing"),
					},
					cx,
				);
				assert_eq!(m.status.key, "remote_reconnecting");
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "remote_open_failed");
				assert!(!m.workspace_open && m.remote.session.is_none());
				assert!(matches!(m.remote.message, Some((false, _))));
				assert_eq!(m.tab_info().conn, Some(crate::tabs::Conn::Failed));
				assert!(m
					.remote
					.restoring
					.as_ref()
					.is_some_and(|r| r.failed.is_some()));
			});

			model.update(cx, |m, cx| {
				m.restore_remote(
					RecentRemote {
						host: host.clone(),
						path: id.clone(),
					},
					cx,
				);
			});
			settle(cx);
			land_remote_open(&model, cx);
			settle(cx);
			model.read_with(cx, |m, _| {
				let session = m.remote.session.as_ref().expect("reconnected");
				assert_eq!(session.workspace.id, id);
				assert!(m.workspace_open);
				assert_eq!(m.remote.restoring, None);
			});
		}

		#[gpui::test]
		fn test_display_repo_path_unit(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let local_path = tmp.path().join("local-repo");
			let (model, cx) = open(cx, local_path.clone(), None);

			model.read_with(cx, |m, _| {
				assert_eq!(
					m.display_repo_path(&local_path),
					local_path.display().to_string()
				);
			});

			let session_root = PathBuf::from(
				"snip-remote://5134d3b34076abcd1234567890abcdef5134d3b34076abcd1234567890abcdef/ws123",
			);
			model.update(cx, |m, _| {
				use std::sync::Arc;
				m.remote.session = Some(crate::remote::RemoteSession {
					client: Arc::new(snip_remote::Client::new(
						snip_remote::RemoteHost::ssh("ubuntu-ui"),
						"test-mac".into(),
					)),
					workspace: snip_remote::RemoteWorkspace {
						id: "ws123".into(),
						name: "gitws".into(),
						path: "/home/ubuntu/gitws".into(),
					},
					root: session_root.clone(),
				});
			});

			model.read_with(cx, |m, _| {
				let root_tip = m.display_repo_path(&session_root);
				assert_eq!(root_tip, "ubuntu-ui:/home/ubuntu/gitws");

				let nested = session_root.join("alpha");
				let nested_tip = m.display_repo_path(&nested);
				assert_eq!(nested_tip, "ubuntu-ui:/home/ubuntu/gitws/alpha");
			});
		}

		#[gpui::test]
		fn remote_failed_preview_crumb_names_the_failed_repo_not_open_repo(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			let beta = shared.join("beta");
			fs::create_dir_all(&alpha).unwrap();
			fs::create_dir_all(&beta).unwrap();

			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("a.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "a.txt"]);
			crate::paste::tests::git_run(
				&alpha,
				&["commit", "-m", "init alpha"],
			);
			fs::write(alpha.join("a.txt"), "hello modified alpha\n").unwrap();

			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("b.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "b.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init beta"]);
			fs::write(beta.join("b.txt"), "hello modified beta\n").unwrap();

			let (model, cx, worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			settle(cx);
			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					m.repos.len() >= 2
						&& m.change_repos.len() >= 2
						&& !m.is_loading && m.files.iter().any(|f| {
						m.change_repos
							.get(f.repo as usize)
							.is_some_and(|s| s.name == "beta")
					})
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			// Verify alpha is the open repo
			model.read_with(cx, |m, _| {
				assert_eq!(m.repo_root(), Some(m.repos[0].root.clone()));
				assert_eq!(m.repos[0].name, "alpha");
			});

			// Stop the worker so reading file preview from worker fails
			worker.stop();

			// Find beta's file in m.files
			let beta_idx = model.read_with(cx, |m, _| {
				m.files
					.iter()
					.position(|f| {
						let slot = f.repo as usize;
						m.change_repos
							.get(slot)
							.is_some_and(|s| s.name == "beta")
					})
					.expect("beta change item found in files")
			});

			model.update(cx, |m, cx| {
				m.select_change(beta_idx, cx);
			});

			settle(cx);
			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					!m.preview_loading && m.preview_error.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				assert!(m.preview_error.is_some(), "expected preview error");
				assert!(m.preview.is_none(), "preview should be none on error");
				let (tab, crumbs, _badge) = m.source_labels();
				assert_eq!(tab, "b.txt");
				assert!(
					crumbs.contains("beta"),
					"crumbs should contain beta, got: {crumbs}"
				);
				assert!(
					!crumbs.contains("alpha"),
					"crumbs must not contain alpha, got: {crumbs}"
				);
				assert!(
					crumbs.contains("b.txt"),
					"crumbs must contain file path b.txt, got: {crumbs}"
				);
			});

			model.update(cx, |m, _| {
				m.clear_preview();
			});
			model.read_with(cx, |m, _| {
				assert_eq!(m.preview_error_root, None);
			});
		}

		#[gpui::test]
		fn no_workspace_empty_state_is_no_workspace_not_scanning(
			cx: &mut TestAppContext,
		) {
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(None, None, "normal".into(), cx)
			});
			settle(cx);

			// (1) Fresh model with no workspace open
			model.read_with(cx, |m, _| {
				assert!(!m.workspace_open);
				assert_ne!(
					m.log_empty_state(),
					Some(crate::LogEmpty::Scanning),
					"fresh model log_empty_state must not be Scanning"
				);
				assert_ne!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::Scanning),
					"fresh model changes_empty_state must not be Scanning"
				);
				assert_eq!(
					m.log_empty_state(),
					Some(crate::LogEmpty::NoWorkspace),
					"fresh model log_empty_state should be NoWorkspace"
				);
				assert_eq!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::NoWorkspace),
					"fresh model changes_empty_state should be NoWorkspace"
				);
			});

			// (2) Open a local workspace and then close it
			let tmp = tempfile::tempdir().unwrap();
			let repo_dir = tmp.path().join("repo");
			fs::create_dir_all(&repo_dir).unwrap();
			crate::paste::tests::git_init(&repo_dir);

			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(Some(repo_dir), None, "normal".into(), cx)
			});
			settle(cx);

			model.read_with(cx, |m, _| {
				assert!(m.workspace_open);
			});

			model.update(cx, |m, cx| {
				m.finish_close(cx);
			});
			settle(cx);

			model.read_with(cx, |m, _| {
				assert!(!m.workspace_open);
				assert_ne!(
					m.log_empty_state(),
					Some(crate::LogEmpty::Scanning),
					"closed workspace log_empty_state must not be Scanning"
				);
				assert_ne!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::Scanning),
					"closed workspace changes_empty_state must not be Scanning"
				);
				assert_eq!(
					m.log_empty_state(),
					Some(crate::LogEmpty::NoWorkspace),
					"closed workspace log_empty_state should be NoWorkspace"
				);
				assert_eq!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::NoWorkspace),
					"closed workspace changes_empty_state should be NoWorkspace"
				);
			});
		}

		#[gpui::test]
		fn remote_repo_tooltips_do_not_expose_snip_remote_internal_scheme(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			let beta = shared.join("beta");
			fs::create_dir_all(&alpha).unwrap();
			fs::create_dir_all(&beta).unwrap();

			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("a.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "a.txt"]);
			crate::paste::tests::git_run(
				&alpha,
				&["commit", "-m", "init alpha"],
			);

			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("b.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "b.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init beta"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			settle(cx);
			for _ in 0..20 {
				let done = model
					.read_with(cx, |m, _| m.repos.len() >= 2 && !m.is_loading);
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 2);
				for repo in &m.repos {
					let display_path = m.display_repo_path(&repo.root);
					assert!(
						!display_path.contains("snip-remote://"),
						"display_repo_path must not contain snip-remote://, got: {display_path}"
					);
					assert!(
						display_path.starts_with("test-worker:"),
						"display_repo_path must start with test-worker:, got: {display_path}"
					);

					let chg_tip = m.change_repo_tooltip(&repo.root);
					assert!(
						!chg_tip.contains("snip-remote://"),
						"change_repo_tooltip must not contain snip-remote://, got: {chg_tip}"
					);
					assert!(
						chg_tip.contains("test-worker:"),
						"change_repo_tooltip must contain test-worker:, got: {chg_tip}"
					);

					let proj_tip = m.project_repo_tooltip(repo);
					assert!(
						!proj_tip.contains("snip-remote://"),
						"project_repo_tooltip must not contain snip-remote://, got: {proj_tip}"
					);
					assert!(
						proj_tip.contains("test-worker:"),
						"project_repo_tooltip must contain test-worker:, got: {proj_tip}"
					);
				}
			});
		}

		#[gpui::test]
		fn remote_multi_repo_workspace_lists_every_repo_with_its_changes(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			use crate::syntax::Language;
			use crate::tree::{NodeKey, TreeCommand};
			use snip_core::transfer::SourceKind;
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			let beta = shared.join("beta");
			let plain = shared.join("plain");
			fs::create_dir_all(alpha.join("src")).unwrap();
			fs::create_dir_all(&beta).unwrap();
			fs::create_dir_all(&plain).unwrap();

			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("src/a.txt"), "committed\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "src/a.txt"]);
			crate::paste::tests::git_run(
				&alpha,
				&["commit", "-m", "init alpha"],
			);
			fs::write(alpha.join("src/a.txt"), "modified\n").unwrap();
			fs::write(alpha.join("untracked.txt"), "untracked\n").unwrap();
			fs::write(alpha.join("staged.txt"), "staged\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "staged.txt"]);

			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("b.txt"), "clean\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "b.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init beta"]);

			fs::write(plain.join("note.txt"), "plain\n").unwrap();

			let status_out = crate::paste::tests::git_run(
				&alpha,
				&["status", "--porcelain=v2"],
			);
			let mut expected_alpha = Vec::new();
			for line in status_out.lines() {
				if line.starts_with("1 ") {
					let parts: Vec<&str> = line.split_whitespace().collect();
					let xy = parts[1];
					let path = parts[8];
					if xy.starts_with(|c| c != '.') {
						expected_alpha
							.push((path.to_string(), SourceKind::Staged));
					}
					if xy.chars().nth(1).is_some_and(|c| c != '.') {
						expected_alpha
							.push((path.to_string(), SourceKind::Unstaged));
					}
				} else if let Some(path) = line.strip_prefix("? ") {
					expected_alpha
						.push((path.trim().to_string(), SourceKind::Working));
				}
			}
			expected_alpha.sort_by(|a, b| a.0.cmp(&b.0));

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.repos.len() == 2
						&& m.change_repos.len() == 2
						&& m.change_repos
							.iter()
							.all(|r| r.state == crate::ChangeRepoState::Loaded)
						&& !m.commits.is_empty()
				});
				if done {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				let names: Vec<_> =
					m.repos.iter().map(|r| r.name.as_str()).collect();
				assert_eq!(names, ["alpha", "beta"]);
				assert_eq!(
					m.change_repos[0].state,
					crate::ChangeRepoState::Loaded
				);
				assert_eq!(
					m.change_repos[1].state,
					crate::ChangeRepoState::Loaded
				);
				assert_eq!(m.changes_empty_state(), None);

				let mut alpha_rows: Vec<(String, SourceKind)> = m
					.files
					.iter()
					.filter(|f| f.repo == 0)
					.map(|f| (f.path.clone(), f.source.clone()))
					.collect();
				alpha_rows.sort_by(|a, b| a.0.cmp(&b.0));
				assert_eq!(alpha_rows, expected_alpha);

				assert_eq!(m.log_feeds.len(), 2);
				assert!(!m.commits.is_empty());
				assert!(m.refs.iter().any(|r| r.name == "refs/heads/main"));
			});

			let mod_idx = model.read_with(cx, |m, _| {
				m.files
					.iter()
					.position(|f| {
						f.repo == 0
							&& f.path == "src/a.txt"
							&& f.source == SourceKind::Unstaged
					})
					.expect("modified row")
			});
			model.update(cx, |m, cx| {
				m.select_change(mod_idx, cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.preview.is_some() && !m.preview_loading
				});
				if done {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("modified preview");
				assert!(p.is_diff);
				assert_eq!(p.lang, Language::Diff);
				assert!(!p.text.is_empty());
			});

			let commit_sha = model.read_with(cx, |m, _| {
				m.commits.first().expect("commit exists").sha.clone()
			});
			model.update(cx, |m, cx| {
				m.select_commit(&commit_sha, cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					!m.commit_view.commit_files.is_empty()
				});
				if done {
					break;
				}
			}
			let commit_file = model.read_with(cx, |m, _| {
				assert!(!m.commit_view.commit_files.is_empty());
				m.commit_view.commit_files[0].0.clone()
			});
			model.update(cx, |m, cx| {
				m.select_commit_file(&commit_file, cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.preview.is_some() && !m.preview_loading
				});
				if done {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("commit file preview");
				assert!(p.is_diff);
				assert_eq!(p.lang, Language::Diff);
				assert!(!p.text.is_empty());
			});

			model.update(cx, |m, cx| {
				m.select_repo(0, cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.file_tree.as_ref().is_some_and(|t| t.is_loaded)
				});
				if done {
					break;
				}
			}
			model.update(cx, |m, cx| {
				m.dispatch_tree(
					Some(TreeCommand::Expand(NodeKey::from_utf8_rel("src"))),
					cx,
				);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.file_tree.as_ref().is_some_and(|t| {
						t.children
							.iter()
							.find(|c| c.name == "src")
							.is_some_and(|s| s.is_loaded)
					})
				});
				if done {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				let tree = m.file_tree.as_ref().unwrap();
				let src = tree
					.children
					.iter()
					.find(|c| c.name == "src")
					.expect("src dir");
				assert!(src.is_loaded);
				let child_rels: Vec<_> =
					src.children.iter().map(|c| c.rel_path.as_str()).collect();
				assert_eq!(child_rels, ["src/a.txt"]);
			});

			model.update(cx, |m, cx| {
				let root = m.repo().map(|r| r.root.clone());
				m.select_file_in(root, "src/a.txt", SourceKind::File, cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.preview.is_some() && !m.preview_loading
				});
				if done {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("tree file preview");
				assert_eq!(&*p.text, "modified\n");
				assert_eq!(m.preview_error, None);
			});
		}

		#[gpui::test]
		fn remote_single_repo_workspace_is_the_repo(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("repo");
			fs::create_dir_all(&shared).unwrap();
			crate::paste::tests::git_init(&shared);
			fs::write(shared.join("file.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&shared, &["add", "file.txt"]);
			crate::paste::tests::git_run(&shared, &["commit", "-m", "init"]);
			crate::paste::tests::git_run(&shared, &["tag", "v1"]);
			fs::write(shared.join("dirty.txt"), "dirty\n").unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.change_repos.first().is_some_and(|r| {
						r.state == crate::ChangeRepoState::Loaded
					}) && !m.files.is_empty()
						&& !m.commits.is_empty()
				});
				if done {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 1);
				let session_root =
					m.remote.session.as_ref().unwrap().root.clone();
				assert_eq!(m.repos[0].root, session_root);
				assert_eq!(m.ws_home.as_ref(), Some(&m.repo().unwrap().root));
				assert!(m.ws_tree.is_none());
				assert!(m.file_tree.is_some());
				assert!(m.changes_empty_state().is_none());
				assert!(!m.files.is_empty(), "Changes has rows");
				assert!(!m.commits.is_empty(), "log has commits");
				assert!(
					m.refs.iter().any(|r| r.name == "refs/heads/main"),
					"refs contain refs/heads/main: {:?}",
					m.refs
				);
				assert!(
					m.refs.iter().any(|r| r.name == "refs/tags/v1"),
					"refs contain refs/tags/v1: {:?}",
					m.refs
				);
			});
		}

		#[gpui::test]
		fn remote_non_repo_folder_says_no_repository(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("empty");
			fs::create_dir_all(&shared).unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			model.update(cx, |m, _| {
				m.probes = Some(crate::ui::Probes::for_test());
			});
			settle(cx);
			for _ in 0..50 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading && m.discovery_status.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}
			for _ in 0..2 {
				// ALLOWED-TEST-REFRESH: test harness needs a redraw after empty discovery completion
				cx.update(|w, _| w.refresh());
				settle(cx);
			}

			model.read_with(cx, |m, _| {
				assert!(m.repos.is_empty());
				assert_eq!(
					m.discovery_status,
					Some(snip_core::workspace::ScanStatus::Complete)
				);
				assert_eq!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::NoRepository)
				);
				assert_eq!(
					m.log_empty_state(),
					Some(crate::LogEmpty::NoRepository)
				);
				let drawn = m.probes.as_ref().unwrap().drawn();
				assert!(
					drawn.contains(&"changes-empty".to_string()),
					"drawn: {drawn:?}"
				);
				assert_eq!(m.last_changes_empty.get(), Some("no_repository"));
			});
		}

		#[gpui::test]
		fn remote_worker_too_old_records_scan_error(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "ancient".into(),
					max_protocol: Some(1),
				},
			);

			model.read_with(cx, |m, _| {
				let err = m.remote.scan_error.as_ref().expect("scan error");
				assert_eq!(err.key, "remote_worker_too_old");
				assert_eq!(err.args[0], "ancient");
				// The status bar says it too, never a scan still running.
				assert_eq!(m.status.key, "remote_worker_too_old");
			});
		}

		#[gpui::test]
		fn remote_scan_error_cleared_on_opening_local_workspace(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "ancient".into(),
					max_protocol: Some(1),
				},
			);

			model.read_with(cx, |m, _| {
				assert!(m.remote.scan_error.is_some());
				assert!(matches!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::ScanFailed(_))
				));
				assert!(matches!(
					m.log_empty_state(),
					Some(crate::LogEmpty::Failed(_))
				));
			});

			// Open a local clean repo:
			let local_repo = tmp.path().join("local_repo");
			fs::create_dir_all(&local_repo).unwrap();
			crate::paste::tests::git_init(&local_repo);
			fs::write(local_repo.join("file.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&local_repo, &["add", "file.txt"]);
			crate::paste::tests::git_run(
				&local_repo,
				&["commit", "-m", "init"],
			);

			model.update(cx, |m, cx| {
				m.open_workspace_path(local_repo.clone(), cx);
				if m.workspace_root != local_repo {
					let step = m.lifecycle.poll_at(
						std::time::Instant::now(),
						crate::lifecycle::GitLoad::idle(),
					);
					let crate::lifecycle::Step::Ready(
						crate::lifecycle::Intent::OpenWorkspace(p),
					) = step
					else {
						panic!("drain not ready: {step:?}");
					};
					m.finish_open(p, cx);
				}
			});

			for _ in 0..50 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading
						&& m.discovery_status
							== Some(snip_core::workspace::ScanStatus::Complete)
						&& !m.repos.is_empty()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				assert_eq!(m.remote.scan_error, None);
				assert_eq!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::Clean)
				);
				assert_eq!(m.log_empty_state(), None);
			});

			// Open a local non-repo folder:
			let local_non_repo = tmp.path().join("local_non_repo");
			fs::create_dir_all(&local_non_repo).unwrap();

			model.update(cx, |m, cx| {
				m.open_workspace_path(local_non_repo.clone(), cx);
				if m.workspace_root != local_non_repo {
					let step = m.lifecycle.poll_at(
						std::time::Instant::now(),
						crate::lifecycle::GitLoad::idle(),
					);
					let crate::lifecycle::Step::Ready(
						crate::lifecycle::Intent::OpenWorkspace(p),
					) = step
					else {
						panic!("drain not ready: {step:?}");
					};
					m.finish_open(p, cx);
				}
			});

			for _ in 0..50 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading
						&& m.discovery_status
							== Some(snip_core::workspace::ScanStatus::Complete)
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				assert_eq!(m.remote.scan_error, None);
				assert_eq!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::NoRepository)
				);
				assert_eq!(
					m.log_empty_state(),
					Some(crate::LogEmpty::NoRepository)
				);
			});
		}

		/// Opens a workspace of `alpha` (a.txt) and `beta` (b.txt), both with
		/// one untracked change, and waits until both repos' rows are listed.
		fn open_alpha_beta<'a>(
			cx: &'a mut TestAppContext,
			ws: &Path,
		) -> (Entity<WorkbenchModel>, &'a mut VisualTestContext) {
			repo(ws, "alpha", &[("a.txt", "alpha-1\n")]);
			repo(ws, "beta", &[("b.txt", "beta-1\n")]);
			let (model, cx) = open(cx, ws.to_path_buf(), None);
			for _ in 0..40 {
				settle(cx);
				let listed = model.read_with(cx, |m, _| {
					!m.is_loading
						&& m.files.iter().any(|f| f.path == "a.txt")
						&& m.files.iter().any(|f| f.path == "b.txt")
				});
				if listed {
					return (model, cx);
				}
			}
			panic!("alpha and beta rows never listed");
		}

		fn change_row(m: &WorkbenchModel, path: &str) -> usize {
			m.files.iter().position(|f| f.path == path).expect(path)
		}

		/// Whether the shown preview (a diff for a Changes row) has `line`.
		fn shows(m: &WorkbenchModel, line: &str) -> bool {
			m.preview
				.as_ref()
				.is_some_and(|p| p.text.lines().any(|l| l == line))
		}

		/// Refresh drops a Changes row of a repo other than the one it
		/// reloads: with the open repo unreadable, the row's path must not
		/// stay selected over that repo's error.
		#[gpui::test]
		fn refresh_drops_another_repos_change_preview(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			// `alpha` holds only an empty `.git`: listed, but unreadable.
			fs::create_dir_all(tmp.path().join("alpha/.git")).unwrap();
			repo(tmp.path(), "beta", &[("b.txt", "beta-1\n")]);
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			for _ in 0..40 {
				settle(cx);
				if model.read_with(cx, |m, _| {
					!m.is_loading && m.files.iter().any(|f| f.path == "b.txt")
				}) {
					break;
				}
			}
			model.update(cx, |m, cx| {
				assert_eq!(m.repo().map(|r| r.name.as_str()), Some("alpha"));
				let row = change_row(m, "b.txt");
				m.select_change(row, cx);
			});
			settle(cx);
			model.read_with(cx, |m, _| assert!(shows(m, "+beta-1")));

			model.update(cx, |m, cx| m.reload_repos(cx));
			for _ in 0..40 {
				settle(cx);
				if model.read_with(cx, |m, _| !m.is_loading) {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				assert_eq!(m.repo().map(|r| r.name.as_str()), Some("alpha"));
				assert_eq!(m.selected_file, None);
				assert!(!shows(m, "+beta-1"));
				assert!(m.preview_error.is_some(), "alpha's error shown");
			});
		}

		/// Refresh keeps the open repo's own Changes row and re-reads it.
		#[gpui::test]
		fn refresh_keeps_and_rereads_the_open_repos_change_row(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open_alpha_beta(cx, tmp.path());
			fs::write(tmp.path().join("alpha/z.txt"), "z\n").unwrap();
			model.update(cx, |m, cx| m.reload_repos(cx));
			for _ in 0..40 {
				settle(cx);
				if model.read_with(cx, |m, _| {
					m.changes_loaded
						&& m.files.iter().any(|f| f.path == "z.txt")
				}) {
					break;
				}
			}
			model.update(cx, |m, cx| {
				let row = change_row(m, "z.txt");
				m.select_change(row, cx);
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(shows(m, "+z"));
			});

			fs::write(tmp.path().join("alpha/z.txt"), "z-2\n").unwrap();
			model.update(cx, |m, cx| m.reload_repos(cx));
			for _ in 0..40 {
				settle(cx);
				if model
					.read_with(cx, |m, _| m.changes_loaded && shows(m, "+z-2"))
				{
					break;
				}
			}
			model.read_with(cx, |m, _| {
				assert_eq!(m.selected_file.as_deref(), Some("z.txt"));
				assert!(shows(m, "+z-2"));
			});
		}

		#[gpui::test]
		fn remote_refresh_preserves_selected_file_root_after_failed_preview(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let alpha = shared.join("alpha");
			fs::create_dir_all(&alpha).unwrap();
			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("README.md"), "alpha readme\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "README.md"]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);

			let beta = shared.join("beta");
			fs::create_dir_all(&beta).unwrap();
			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("file.txt"), "beta file\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "file.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init"]);

			fs::write(shared.join("README.md"), "root readme\n").unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			model.update(cx, |m, cx| {
				assert_eq!(m.repos.len(), 2);
				let alpha_idx = m
					.repos
					.iter()
					.position(|r| r.name == "alpha")
					.expect("alpha repo");
				if m.selected_repo_idx != Some(alpha_idx) {
					m.select_repo_internal(alpha_idx, false, cx);
				}
			});
			settle(cx);

			model.update(cx, |m, cx| {
				m.dispatch_ws_tree(
					Some(crate::TreeCommand::OpenFile("README.md".into())),
					cx,
				);
			});
			settle(cx);

			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					!m.preview_loading && m.preview.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			let ws_root = model.read_with(cx, |m, _| m.ws_root().unwrap());
			let repo_root = model.read_with(cx, |m, _| m.repo_root().unwrap());
			assert_ne!(ws_root, repo_root);

			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("preview loaded");
				assert_eq!(p.text.as_ref(), "root readme\n");
				assert_eq!(m.selected_file_root.as_ref(), Some(&ws_root));
			});

			// (1) Make preview fail (delete README on worker) then Refresh
			fs::remove_file(shared.join("README.md")).unwrap();
			model.update(cx, |m, cx| {
				m.reload_repos(cx);
			});

			for _ in 0..20 {
				let done = model
					.read_with(cx, |m, _| !m.is_loading && !m.preview_loading);
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				if let Some(p) = &m.preview {
					assert_ne!(
						p.text.as_ref(),
						"alpha readme\n",
						"must not read alpha/README.md on refresh"
					);
				}
				assert!(
					m.preview_error.is_some(),
					"expected preview error for missing root file"
				);
				assert_eq!(m.selected_file_root.as_ref(), Some(&ws_root));
			});

			// (2) With preview OK (re-create root README), Refresh -> still shows root README content
			fs::write(shared.join("README.md"), "root readme v2\n").unwrap();
			model.update(cx, |m, cx| {
				m.reload_repos(cx);
			});

			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading && !m.preview_loading && m.preview.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("preview restored");
				assert_eq!(p.text.as_ref(), "root readme v2\n");
				assert_eq!(m.selected_file_root.as_ref(), Some(&ws_root));
			});
		}

		#[gpui::test]
		fn remote_refresh_with_deleted_file_in_error_repo_shows_file_preview_error(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let nested = shared.join("nested");
			fs::create_dir_all(nested.join(".git")).unwrap();
			fs::write(shared.join("new.txt"), "hello new file\n").unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 1);
				assert_eq!(m.repos[0].name, "nested");
				assert!(
					m.repos[0].summary.is_err(),
					"nested should have error summary"
				);
			});

			model.update(cx, |m, cx| {
				m.dispatch_ws_tree(
					Some(crate::TreeCommand::OpenFile("new.txt".into())),
					cx,
				);
			});
			settle(cx);

			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					!m.preview_loading && m.preview.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("preview loaded");
				assert_eq!(p.text.as_ref(), "hello new file\n");
				assert_eq!(m.selected_file.as_deref(), Some("new.txt"));
			});

			// (1) Delete new.txt on worker, then Refresh -> preview error should name new.txt, not nested
			fs::remove_file(shared.join("new.txt")).unwrap();
			model.update(cx, |m, cx| {
				m.reload_repos(cx);
			});

			for _ in 0..20 {
				let done = model
					.read_with(cx, |m, _| !m.is_loading && !m.preview_loading);
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				assert_eq!(m.selected_file.as_deref(), Some("new.txt"));
				let err = m
					.preview_error
					.as_ref()
					.expect("preview error for deleted file");
				let err_text = err.render(m.locale);
				assert_eq!(err.key, "error_preview");
				assert!(
					err_text.contains("new.txt"),
					"preview error text should name new.txt, got: {err_text}"
				);
				assert!(
					!err_text.contains("nested"),
					"preview error text must not name nested, got: {err_text}"
				);
			});

			// (2) When file still exists, refresh keeps its content showing (no hijack)
			fs::write(shared.join("new.txt"), "hello restored\n").unwrap();
			model.update(cx, |m, cx| {
				m.reload_repos(cx);
			});

			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading && !m.preview_loading && m.preview.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("preview restored");
				assert_eq!(p.text.as_ref(), "hello restored\n");
				assert_eq!(m.selected_file.as_deref(), Some("new.txt"));
				assert!(m.preview_error.is_none());
			});
		}

		#[gpui::test]
		fn remote_refresh_shows_a_new_change_in_the_open_repo(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("repo");
			fs::create_dir_all(&shared).unwrap();
			crate::paste::tests::git_init(&shared);
			fs::write(shared.join("file.txt"), "committed\n").unwrap();
			crate::paste::tests::git_run(&shared, &["add", "file.txt"]);
			crate::paste::tests::git_run(&shared, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 1);
				assert!(m.files.is_empty(), "clean worktree has no changes");
				assert!(!m.files.iter().any(|f| f.path == "new_file.txt"));
			});

			fs::write(shared.join("new_file.txt"), "new content\n").unwrap();

			model.update(cx, |m, cx| {
				m.reload_repos(cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.change_repos.first().is_some_and(|r| {
						r.state == crate::ChangeRepoState::Loaded
					}) && m.files.iter().any(|f| f.path == "new_file.txt")
				});
				if done {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				assert!(
					m.files.iter().any(|f| f.path == "new_file.txt"),
					"new file found after refresh: {:?}",
					m.files
				);
			});
		}

		#[gpui::test]
		fn remote_single_repo_refresh_keeps_one_tree_and_its_open_folders(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			use crate::tree::{NodeKey, TreeCommand};
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("repo");
			let sub = shared.join("sub");
			fs::create_dir_all(&sub).unwrap();
			crate::paste::tests::git_init(&shared);
			fs::write(sub.join("inner.txt"), "inner\n").unwrap();
			crate::paste::tests::git_run(&shared, &["add", "sub/inner.txt"]);
			crate::paste::tests::git_run(&shared, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			model.read_with(cx, |m, _| {
				assert!(m.ws_tree.is_none());
				assert!(m.file_tree.is_some());
			});

			model.update(cx, |m, cx| {
				m.dispatch_tree(
					Some(TreeCommand::Expand(NodeKey::from_utf8_rel("sub"))),
					cx,
				);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					let tree = m.file_tree.as_ref().unwrap();
					tree.children
						.iter()
						.find(|c| c.name == "sub")
						.is_some_and(|s| s.is_loaded)
				});
				if done {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				let tree = m.file_tree.as_ref().unwrap();
				let sub_node =
					tree.children.iter().find(|c| c.name == "sub").unwrap();
				assert!(sub_node.is_expanded);
				assert!(sub_node.is_loaded);
			});

			model.update(cx, |m, cx| {
				m.reload_repos(cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.file_tree.as_ref().is_some_and(|t| {
						t.is_loaded
							&& t.children
								.iter()
								.find(|c| c.name == "sub")
								.is_some_and(|s| s.is_loaded)
					})
				});
				if done {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				assert!(m.ws_tree.is_none());
				let tree = m.file_tree.as_ref().expect("file_tree preserved");
				let sub_node =
					tree.children.iter().find(|c| c.name == "sub").unwrap();
				assert!(
					sub_node.is_expanded,
					"folder remains expanded after refresh"
				);
				assert!(
					sub_node.is_loaded,
					"folder remains loaded after refresh"
				);
				assert!(
					sub_node.children.iter().any(|c| c.name == "inner.txt"
						|| c.rel_path == "sub/inner.txt"),
					"sub/inner.txt present after refresh: {:?}",
					sub_node.children
				);
			});
		}

		#[gpui::test]
		fn remote_change_list_over_cap_shows_truncated_note(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("repo");
			fs::create_dir_all(&shared).unwrap();
			crate::paste::tests::git_init(&shared);
			for i in 0..2100 {
				fs::write(shared.join(format!("u{i:04}.txt")), "").unwrap();
			}

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			for _ in 0..100 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.change_repos.first().is_some_and(|r| {
						r.state == crate::ChangeRepoState::Loaded
							&& r.total == 2100
					})
				});
				if done {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 1);
				assert_eq!(m.change_repos.len(), 1);
				let slot = &m.change_repos[0];
				assert_eq!(slot.total, 2100);
				let kept = crate::slot_range(&m.files, 0).len();
				assert_eq!(kept, crate::MAX_CHANGES_PER_REPO);
				assert_eq!(m.files.len(), crate::MAX_CHANGES_PER_REPO);
				assert!(
					slot.truncated(kept),
					"truncated condition must be true"
				);
				assert!(slot.total > kept);
				let layout = crate::ui::ChangeLayout {
					by_dir: false,
					expanded: |_: usize, _: &str, _: &str| false,
				};
				let rows = crate::ui::change_rows(
					&m.change_repos,
					&m.files,
					|_| false,
					|_, _| true,
					"",
					&layout,
				);
				assert!(
					rows.iter().any(|r| matches!(
						r,
						crate::ui::ChangeItemRow::Note { slot: 0 }
					)),
					"note row emitted"
				);
			});
		}

		#[gpui::test]
		fn remote_copy_runs_on_the_worker(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			use crate::tree::{NodeKey, TreeCommand};
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			let beta = shared.join("beta");
			fs::create_dir_all(&alpha).unwrap();
			fs::create_dir_all(&beta).unwrap();

			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("init.txt"), "hello\n").unwrap();
			fs::create_dir_all(alpha.join("dir")).unwrap();
			fs::write(alpha.join("dir").join("c.txt"), "C\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "."]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);
			fs::write(alpha.join("dirty.txt"), "dirty\n").unwrap();

			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("init.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "init.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init"]);

			let notes = shared.join("notes");
			fs::create_dir_all(&notes).unwrap();
			fs::write(notes.join("n.txt"), "N\n").unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);

			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.change_repos.first().is_some_and(|r| {
						r.state == crate::ChangeRepoState::Loaded
					}) && !m.files.is_empty()
						&& !m.commits.is_empty()
						&& m.ws_tree.as_ref().is_some_and(|t| t.is_loaded)
				});
				if done {
					break;
				}
			}

			model.update(cx, |m, cx| {
				m.select_repo(0, cx);
			});
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.file_tree.as_ref().is_some_and(|t| t.is_loaded)
				});
				if done {
					break;
				}
			}

			// Runs one copy and waits for the worker's payload to land.
			let run = |model: &Entity<WorkbenchModel>,
			           cx: &mut VisualTestContext,
			           act: &dyn Fn(
				&mut WorkbenchModel,
				&mut gpui::Context<WorkbenchModel>,
			),
			           what: &str|
			 -> String {
				clip::write_text("before").unwrap();
				model.update(cx, |m, cx| {
					m.set_status("status_idle", []);
					act(m, cx);
				});
				for _ in 0..200 {
					settle(cx);
					if model.read_with(cx, |m, _| !m.is_copying) {
						break;
					}
					std::thread::sleep(Duration::from_millis(10));
				}
				model.read_with(cx, |m, _| {
					assert!(!m.is_copying, "{what}: still copying");
					assert!(
						matches!(
							m.status.key,
							"status_copied" | "status_commits_copied"
						),
						"{what}: {}",
						m.status
					);
				});
				clip::read_text().unwrap()
			};
			let files = |text: &str| -> Vec<(String, String)> {
				parse_clipboard(text, "")
					.into_iter()
					.map(|e| (e.path, e.content))
					.collect()
			};
			let copy_node = |targets: Vec<crate::menu::CopyTarget>| {
				move |m: &mut WorkbenchModel,
				      cx: &mut gpui::Context<WorkbenchModel>| {
					m.copy_targets(targets.clone(), cx)
				}
			};
			// The row's menu Copy, as the right-click handler opens it.
			let menu_copy = |m: &WorkbenchModel, ws: bool, rel: &str| {
				let tree = if ws { &m.ws_tree } else { &m.file_tree };
				let tree = tree.as_ref().expect("tree loaded");
				let row = tree
					.flatten_visible(tree.visible_limit())
					.into_iter()
					.find(|r| r.rel_path == rel)
					.unwrap_or_else(|| panic!("{rel} row"));
				assert!(row.selected, "{rel} is selected");
				copy_of(m.work_row_menu(&row, ws))
					.unwrap_or_else(|| panic!("{rel}: Copy enabled"))
			};

			// A Changes file row: the worker's working-tree bytes.
			let row = model.read_with(cx, |m, _| {
				let idx = m
					.files
					.iter()
					.position(|f| f.path == "dirty.txt")
					.expect("dirty.txt is a change");
				copy_of(m.change_row_menu(idx)).expect("the row offers Copy")
			});
			let text = run(&model, cx, &copy_node(row), "file row");
			assert_eq!(files(&text), entries(&[("dirty.txt", "dirty")]));

			// The Unstaged group.
			let group = model.read_with(cx, |m, _| {
				m.change_targets(|_, f| {
					crate::menu::change_group(f) == Some("unstaged")
				})
			});
			assert!(!group.is_empty());
			let text = run(&model, cx, &copy_node(group), "group");
			assert_eq!(files(&text), entries(&[("dirty.txt", "dirty")]));

			// A Project tree selection (multi-select).
			model.update(cx, |m, cx| {
				m.dispatch_tree(
					Some(TreeCommand::ToggleSelect(NodeKey::from_utf8_rel(
						"init.txt",
					))),
					cx,
				);
				m.dispatch_tree(
					Some(TreeCommand::ToggleSelect(NodeKey::from_utf8_rel(
						"dirty.txt",
					))),
					cx,
				);
			});
			let picked =
				model.read_with(cx, |m, _| menu_copy(m, false, "init.txt"));
			assert_eq!(picked.len(), 2, "{picked:?}");
			let text = run(&model, cx, &copy_node(picked), "project");
			assert_eq!(
				files(&text),
				entries(&[("dirty.txt", "dirty"), ("init.txt", "hello")])
			);

			// A repo subfolder right-click.
			model.update(cx, |m, cx| {
				m.select_tree_rows_alone(false, &["dir".into()], cx);
			});
			let targets =
				model.read_with(cx, |m, _| menu_copy(m, false, "dir"));
			let text = run(&model, cx, &copy_node(targets), "repo folder");
			assert_eq!(files(&text), entries(&[("dir/c.txt", "C")]));

			// A workspace-level non-repo folder right-click.
			model.update(cx, |m, cx| {
				m.select_tree_rows_alone(true, &["notes".into()], cx);
			});
			let targets =
				model.read_with(cx, |m, _| menu_copy(m, true, "notes"));
			let text = run(&model, cx, &copy_node(targets), "workspace folder");
			assert_eq!(files(&text), entries(&[("notes/n.txt", "N")]));

			// A file at a commit, after the worktree changed.
			// The log row's id names its repo; the menu splits it the same way.
			let (repo_root, sha) = model.read_with(cx, |m, _| {
				let id = &m.commits.first().expect("commit exists").sha;
				m.log_root_for(id).expect("the row's repo and sha")
			});
			fs::write(alpha.join("init.txt"), "changed\n").unwrap();
			let at = vec![crate::menu::CopyTarget {
				root: repo_root,
				path: "init.txt".into(),
				source: snip_core::transfer::SourceKind::Commit { rev: sha },
				change_type: None,
			}];
			let text = run(&model, cx, &copy_node(at), "commit file");
			assert_eq!(files(&text), entries(&[("init.txt", "hello")]));

			// The selected commit as a commit payload.
			model.update(cx, |m, cx| {
				let id = m.commits.first().expect("commit exists").sha.clone();
				m.select_commit(&id, cx);
			});
			settle(cx);
			let text = run(
				&model,
				cx,
				&|m: &mut WorkbenchModel,
				  cx: &mut gpui::Context<WorkbenchModel>| {
					m.copy_commits_to_clipboard(cx)
				},
				"commits",
			);
			assert!(text.contains("init.txt"), "{text}");
			assert!(
				matches!(
					snip_core::clip::detect_mode(&text),
					snip_core::clip::Mode::Commits
				),
				"{text}"
			);
		}

		/// Puts `payload` on the clipboard, opens its paste preview in the
		/// remote workspace and waits (bounded) for the worker's plan.
		fn remote_paste_preview(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			payload: &str,
		) {
			clip::write_text(payload).unwrap();
			model.update(cx, |m, cx| m.trigger_paste_preview(cx));
			for _ in 0..300 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.paste.plan().is_some() && !m.paste.is_loading()
				});
				if done {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}
			model.read_with(cx, |m, _| {
				assert!(
					m.paste.plan().is_some() && !m.paste.is_loading(),
					"no remote preview: {}",
					m.status
				);
			});
		}

		/// Applies, as the Apply button does, and waits (bounded) for the
		/// worker's answer.
		fn remote_paste_apply(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			model.update(cx, |m, cx| m.apply_paste_restore(cx));
			for _ in 0..300 {
				settle(cx);
				if !model.read_with(cx, |m, _| m.paste_busy()) {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}
			assert!(
				!model.read_with(cx, |m, _| m.paste_busy()),
				"remote apply never ended"
			);
		}

		/// Paste into a remote workspace through the real panel: the worker
		/// plans (rows, overwrite gating, unchecked rows), writes what was
		/// confirmed, refuses a destination changed behind the preview, never
		/// writes into `.git`, and replays a commit payload.
		#[gpui::test]
		fn remote_paste_runs_on_the_worker(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			fs::create_dir_all(&alpha).unwrap();
			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("init.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "init.txt"]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			wait_remote_repo_loaded(&model, cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			let real = dunce::canonicalize(&alpha).unwrap();

			// Rows come from the worker's plan, with its paths.
			remote_paste_preview(
				&model,
				cx,
				"// FILE: init.txt\nnew init\n// FILE: a.txt\nA\n// FILE: b.txt\nB\n",
			);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.remote.is_some());
				let rows: Vec<_> = plan
					.items
					.iter()
					.map(|i| (i.path.as_str(), i.op, i.dest_exists))
					.collect();
				assert_eq!(
					rows,
					[
						("init.txt", crate::paste::PlannedOp::Overwrite, true),
						("a.txt", crate::paste::PlannedOp::Create, false),
						("b.txt", crate::paste::PlannedOp::Create, false),
					]
				);
				assert_eq!(plan.items[1].dest_path, real.join("a.txt"));
				// Shown as the host's folder, never the internal root.
				assert!(
					plan.shown(&plan.destination).starts_with("test-worker:"),
					"{}",
					plan.shown(&plan.destination)
				);
				assert!(plan.overwrite_missing());
			});
			// Overwrite is off by default: Apply keeps init.txt. b.txt is
			// unticked.
			click(cx, "paste-include:2:b.txt");
			remote_paste_apply(&model, cx);
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().is_none(), "{}", m.status);
			});
			assert_eq!(fs::read_to_string(alpha.join("a.txt")).unwrap(), "A");
			assert_eq!(
				fs::read_to_string(alpha.join("init.txt")).unwrap(),
				"hello\n"
			);
			assert!(!alpha.join("b.txt").exists());

			// Allowing the overwrite writes it, byte for byte.
			remote_paste_preview(&model, cx, "// FILE: init.txt\nnew init\n");
			click(cx, "paste-overwrite:0:init.txt");
			remote_paste_apply(&model, cx);
			assert_eq!(
				fs::read_to_string(alpha.join("init.txt")).unwrap(),
				"new init"
			);

			// A target created behind the preview's back: refused as stale,
			// nothing written, the plan stays open.
			remote_paste_preview(
				&model,
				cx,
				"// FILE: c.txt\nfrom clipboard\n",
			);
			fs::write(alpha.join("c.txt"), "external").unwrap();
			remote_paste_apply(&model, cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "stale_created", "{}", m.status);
				let plan = m.paste.plan().expect("plan stays open");
				assert!(!plan.is_applying);
			});
			assert_eq!(
				fs::read_to_string(alpha.join("c.txt")).unwrap(),
				"external"
			);
			model.update(cx, |m, cx| m.cancel_paste_preview(cx));

			// `.git` entries are skipped rows; Apply writes nothing there.
			let hooks = alpha.join(".git/hooks");
			remote_paste_preview(
				&model,
				cx,
				"// FILE: .git/hooks/pre-commit\n#!/bin/sh\necho owned\n// FILE: d.txt\nD\n",
			);
			// `.git/` reads as a folder prefix: keep it under the destination,
			// as a user would to get past the mapping row.
			model.update(cx, |m, cx| m.choose_paste_keep(".git", cx));
			for _ in 0..300 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					!m.paste.is_loading()
						&& m.paste.plan().is_some_and(|p| p.executable())
				});
				if done {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				let paths: Vec<_> =
					plan.items.iter().map(|i| i.path.as_str()).collect();
				assert_eq!(paths, ["d.txt", ".git/hooks/pre-commit"]);
				assert!(plan.items[1].planned_skip());
				assert_eq!(
					crate::ui::paste_op(&plan.items[1]).2,
					"reason_skip_git_metadata"
				);
				assert_eq!(plan.counts().skips, 1);
			});
			remote_paste_apply(&model, cx);
			assert!(!hooks.join("pre-commit").exists());
			assert_eq!(fs::read_to_string(alpha.join("d.txt")).unwrap(), "D");

			// Commit mode: the worker replays the payload onto the repo.
			crate::paste::tests::git_run(&alpha, &["add", "."]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "pasted"]);
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "incoming\n".into(),
						author_name: "Author".into(),
						author_email: "author@example.invalid".into(),
						author_date: "2026-09-21T12:00:00+00:00".into(),
						files: vec![CommitFile {
							path: "replayed.txt".into(),
							old_path: None,
							change: FileChange::Added,
							content: Some("replayed\n".into()),
							not_copied: None,
						}],
					}],
				});
			remote_paste_preview(&model, cx, &payload);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.whole_commit, "{}", m.status);
				assert_eq!(plan.items.len(), 1);
				assert_eq!(plan.items[0].dest_root_name, "alpha");
				assert_eq!(plan.items[0].dest_path, real.join("replayed.txt"));
			});
			remote_paste_apply(&model, cx);
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().is_none(), "{}", m.status);
			});
			assert_eq!(
				crate::paste::tests::git_run(
					&alpha,
					&["log", "-1", "--format=%s %an"]
				)
				.trim(),
				"incoming Author"
			);
			assert_eq!(
				fs::read_to_string(alpha.join("replayed.txt")).unwrap(),
				"replayed\n"
			);

			// A paste several chunks long goes out in pieces; it is held to
			// the same preview limit (MAX_RETAINED_PREVIEW_BYTES) as a local
			// paste.
			let c1 = "a".repeat(900_000);
			let c2 = "b".repeat(900_000);
			let c3 = "c".repeat(900_000);
			let payload = format!(
				"// FILE: chunk_1.txt\n{c1}\n// FILE: chunk_2.txt\n{c2}\n// FILE: chunk_3.txt\n{c3}\n"
			);
			assert!(payload.len() > 2 * snip_remote::proto::CHUNK_BYTES);
			remote_paste_preview(&model, cx, &payload);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.items.len(), 3);
			});
			remote_paste_apply(&model, cx);
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().is_none(), "{}", m.status);
			});
			assert_eq!(
				fs::read_to_string(alpha.join("chunk_1.txt")).unwrap(),
				c1
			);
			assert_eq!(
				fs::read_to_string(alpha.join("chunk_2.txt")).unwrap(),
				c2
			);
			assert_eq!(
				fs::read_to_string(alpha.join("chunk_3.txt")).unwrap(),
				c3
			);
		}

		/// Prefix rows offer the remote workspace's repositories, shown as
		/// the host's folders, and route each prefix to the one picked.
		#[gpui::test]
		fn remote_paste_maps_prefixes_to_remote_repositories(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			for name in ["alpha", "beta"] {
				let repo = shared.join(name);
				fs::create_dir_all(&repo).unwrap();
				crate::paste::tests::git_init(&repo);
				fs::write(repo.join("init.txt"), "hello\n").unwrap();
				crate::paste::tests::git_run(&repo, &["add", "init.txt"]);
				crate::paste::tests::git_run(&repo, &["commit", "-m", "init"]);
			}
			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			for _ in 0..50 {
				settle(cx);
				if model.read_with(cx, |m, _| m.repos.len() == 2) {
					break;
				}
			}
			remote_paste_preview(
				&model,
				cx,
				"// FILE: alpha/x.txt\nx\n// FILE: beta/y.txt\ny\n",
			);
			for prefix in ["alpha", "beta"] {
				let idx = model.read_with(cx, |m, _| {
					let plan = m.paste.plan().unwrap();
					let choice = plan
						.prefix_choices
						.iter()
						.find(|c| c.prefix == prefix)
						.unwrap();
					let shown: Vec<_> = choice
						.candidates
						.iter()
						.map(|c| plan.shown(c))
						.collect();
					assert!(
						shown.iter().all(|s| s.starts_with("test-worker:")),
						"{shown:?}"
					);
					choice
						.candidates
						.iter()
						.position(|c| c.ends_with(prefix))
						.unwrap()
				});
				model
					.update(cx, |m, cx| m.choose_paste_prefix(prefix, idx, cx));
				for _ in 0..300 {
					settle(cx);
					if model.read_with(cx, |m, _| !m.paste.is_loading()) {
						break;
					}
					std::thread::sleep(Duration::from_millis(10));
				}
			}
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.executable(), "{}", m.status);
				let rows: Vec<_> = plan
					.items
					.iter()
					.map(|i| (i.dest_root_name.as_str(), i.path.as_str()))
					.collect();
				assert_eq!(rows, [("alpha", "x.txt"), ("beta", "y.txt")]);
			});
			remote_paste_apply(&model, cx);
			assert_eq!(
				fs::read_to_string(shared.join("alpha/x.txt")).unwrap(),
				"x"
			);
			assert_eq!(
				fs::read_to_string(shared.join("beta/y.txt")).unwrap(),
				"y"
			);
		}

		#[gpui::test]
		fn remote_paste_preview_shows_diff_for_overwritten_file(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			fs::create_dir_all(&alpha).unwrap();
			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("init.txt"), "hello remote\nline2\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "init.txt"]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			wait_remote_repo_loaded(&model, cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);

			remote_paste_preview(
				&model,
				cx,
				"// FILE: init.txt\nnew remote\nline2\n",
			);
			for _ in 0..50 {
				settle(cx);
				let has_diff = model.read_with(cx, |m, _| {
					m.paste.detail().is_some_and(|d| d.is_diff)
				});
				if has_diff {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}

			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert!(plan.remote.is_some());
				let detail = m.paste.detail().expect("detail present");
				assert!(
					detail.is_diff,
					"remote overwritable file must show diff"
				);
				assert!(detail.text.contains("-hello remote"));
				assert!(detail.text.contains("+new remote"));
			});
		}

		/// The worker refuses a read over the preview cap; the master names
		/// that reason instead of "unreadable" and keeps the source view.
		#[gpui::test]
		fn remote_paste_preview_over_the_cap_says_too_large(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			fs::create_dir_all(&alpha).unwrap();
			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("big.txt"), "y".repeat(2 * 1024 * 1024))
				.unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "big.txt"]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			wait_remote_repo_loaded(&model, cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);

			remote_paste_preview(&model, cx, "// FILE: big.txt\nsmall\n");
			let want = model.read_with(cx, |m, _| {
				crate::i18n::t("paste_diff_too_large", m.locale).to_string()
			});
			for _ in 0..50 {
				settle(cx);
				let noticed = model.read_with(cx, |m, _| {
					m.paste.detail().and_then(|d| d.notice.clone()).is_some()
				});
				if noticed {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().expect("plan open").remote.is_some());
				let detail = m.paste.detail().expect("detail present");
				assert!(!detail.is_diff);
				assert_eq!(
					detail
						.notice
						.as_ref()
						.map(|n| n.render(m.locale))
						.as_deref(),
					Some(want.as_str())
				);
			});
		}

		#[gpui::test]
		fn remote_depth_limited_folder_can_be_continued(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("deep");
			let deep_repo = shared.join("d1/d2/d3/d4/d5/d6/d7/d8/d9/repo");
			fs::create_dir_all(&deep_repo).unwrap();
			crate::paste::tests::git_init(&deep_repo);
			fs::write(deep_repo.join("base.txt"), "base\n").unwrap();
			crate::paste::tests::git_run(&deep_repo, &["add", "base.txt"]);
			crate::paste::tests::git_run(&deep_repo, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			model.read_with(cx, |m, _| {
				assert!(m.repos.is_empty(), "repos should be empty initially");
				assert!(
					!m.discovery_depth_limited.is_empty(),
					"discovery_depth_limited should be non-empty"
				);
				assert_ne!(
					m.discovery_status,
					Some(snip_core::workspace::ScanStatus::Complete),
					"status should not be Complete"
				);
			});

			let continued_folder = model.read_with(cx, |m, _| {
				m.discovery_depth_limited.first().cloned().unwrap()
			});

			let mut found = false;
			for _ in 0..5 {
				model.update(cx, |m, cx| m.continue_discovery(cx));
				for _ in 0..50 {
					settle(cx);
					found = model.read_with(cx, |m, _| {
						!m.is_loading
							&& m.repos.len() == 1 && m
							.change_repos
							.first()
							.is_some_and(|s| {
								s.state == crate::ChangeRepoState::Loaded
							})
					});
					if found {
						break;
					}
				}
				if found {
					break;
				}
			}
			assert!(
				found,
				"nested repo was not found after continue_discovery"
			);

			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 1);
				let session_root = &m.remote.session.as_ref().unwrap().root;
				assert!(
					m.repos[0].root.starts_with(session_root),
					"repo root should be under session root"
				);
				assert_eq!(m.change_repos.len(), 1);
				assert_eq!(
					m.change_repos[0].state,
					crate::ChangeRepoState::Loaded
				);
				assert!(
					!m.discovery_depth_limited.contains(&continued_folder),
					"continued folder should no longer be in depth_limited"
				);
			});
		}

		#[gpui::test]
		fn remote_continue_without_depth_folder_sets_incomplete_status(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			model.update(cx, |m, cx| {
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::TimedOut);
				m.discovery_depth_limited.clear();
				m.is_loading = false;
				let gen_before = m.discovery_generation;
				m.continue_discovery(cx);
				assert_eq!(m.status.key, "remote_scan_incomplete");
				assert!(!m.is_loading);
				assert_eq!(m.discovery_generation, gen_before);
			});
		}

		/// A tooltip that stays open while the language changes rewords
		/// itself (#208): the status bar's message and a Changes error
		/// row's read error are worded when drawn, not when the tooltip
		/// opened. The pointer does not move between the keystroke and the
		/// read; what is read is the tooltip's drawn text.
		#[cfg(unix)]
		#[gpui::test]
		fn shown_error_tooltips_follow_a_language_switch_under_a_still_pointer(
			cx: &mut TestAppContext,
		) {
			use crate::i18n::{Locale, Msg};
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();
			let _alpha = repo(&shared, "alpha", &[]);
			let beta = repo(&shared, "beta", &[]);
			fs::write(beta.join(".git/HEAD"), "garbage not a ref\n").unwrap();
			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);
			model.update(cx, |m, cx| {
				// The exact #208 shape: a worker start failure wrapped as
				// the remote scan failure, worded when drawn.
				m.set_status_msg(Msg::with_msg_arg(
					"remote_scan_failed",
					[String::new()],
					0,
					crate::reason::start_msg(
						snip_remote::StartReason::NoWorker,
						"",
					),
				));
				cx.notify();
			});
			settle(cx);
			let slot = model.read_with(cx, |m, _| {
				m.change_repos
					.iter()
					.position(|s| s.root.ends_with("beta"))
					.expect("beta slot")
			});
			let status_in = |cx: &mut VisualTestContext, loc: Locale| {
				model.read_with(cx, |m, _| m.status.render(loc))
			};
			let note_in = |cx: &mut VisualTestContext, loc: Locale| {
				model.read_with(cx, |m, _| {
					m.change_note_text(slot, loc).expect("a note").text
				})
			};
			let up = |cx: &mut VisualTestContext, text: &str| {
				cx.run_until_parked();
				let key: &'static str =
					Box::leak(format!("tooltip:{text}").into_boxed_str());
				cx.debug_bounds(key).is_some()
			};
			let hover = |cx: &mut VisualTestContext, id: &'static str| {
				cx.run_until_parked();
				let at = cx
					.debug_bounds(id)
					.unwrap_or_else(|| panic!("no rendered control {id}"))
					.center();
				cx.simulate_mouse_move(at, None, gpui::Modifiers::none());
				cx.executor().advance_clock(Duration::from_millis(700));
				settle(cx);
			};
			let (zh_s, en_s) =
				(status_in(cx, Locale::ZhTw), status_in(cx, Locale::En));
			let (zh_n, en_n) =
				(note_in(cx, Locale::ZhTw), note_in(cx, Locale::En));
			assert_ne!(zh_s, en_s);
			assert_ne!(zh_n, en_n);
			// The nested reason follows too, not just the wrapper.
			let reason = crate::reason::start_msg(
				snip_remote::StartReason::NoWorker,
				"",
			);
			let (zh_r, en_r) =
				(reason.render(Locale::ZhTw), reason.render(Locale::En));
			assert!(zh_s.contains(&zh_r) && !zh_s.contains(&en_r), "{zh_s}");
			assert!(en_s.contains(&en_r) && !en_s.contains(&zh_r), "{en_s}");

			// The status bar, Chinese up, then English with no mouse move.
			hover(cx, "status-text");
			assert!(up(cx, &zh_s), "status tooltip shown: {zh_s}");
			to_english(&model, cx);
			assert!(up(cx, &en_s), "status tooltip follows: {en_s}");
			assert!(!up(cx, &zh_s), "the old status text is gone");

			// The Changes error row, English up, then Chinese.
			hover(cx, "change-note:beta");
			assert!(up(cx, &en_n), "note tooltip shown: {en_n}");
			cx.simulate_keystrokes("alt-l");
			assert_eq!(locale_now(&model, cx), Locale::ZhTw);
			assert!(up(cx, &zh_n), "note tooltip follows: {zh_n}");
			assert!(!up(cx, &en_n), "the old note text is gone");
		}

		/// A tooltip that stays open while the repo list is reordered or
		/// shrunk under it must keep describing the same repo, or vanish,
		/// never describe another (#208).
		#[gpui::test]
		fn an_open_note_tooltip_follows_its_repo_not_its_row_position(
			cx: &mut TestAppContext,
		) {
			use crate::i18n::Msg;
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			let failed = |name: &str, why: &str| crate::ChangeRepo {
				root: tmp.path().join(name),
				name: name.into(),
				state: crate::ChangeRepoState::Failed(Msg::new(
					"error_repo_status",
					[why.to_string()],
				)),
				total: 0,
			};
			model.update(cx, |m, cx| {
				m.repos.replace_for_test(vec![
					make_test_repo(tmp.path().join("ra"), "ra"),
					make_test_repo(tmp.path().join("rb"), "rb"),
				]);
				m.change_repos =
					vec![failed("ra", "why-alpha"), failed("rb", "why-beta")];
				// Each repo keeps a row, so its note sits under it
				// instead of folding into the unreadable node.
				m.files = vec![
					make_test_file("a.txt", 0),
					make_test_file("b.txt", 1),
				];
				m.workspace_open = true;
				m.active_tab = WorkbenchTab::GitChanges;
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				// Repo rows start collapsed; open them to show the notes.
				let open: Vec<_> = m
					.change_item_rows()
					.into_iter()
					.filter_map(|r| match r {
						crate::ui::ChangeItemRow::Repo {
							slot,
							group_id,
							..
						} => Some((group_id, m.change_repos[slot].root.clone())),
						_ => None,
					})
					.collect();
				m.chrome.expanded_repos.extend(open);
				cx.notify();
			});
			settle(cx);
			let note = |cx: &mut VisualTestContext, name: &str| {
				model.read_with(cx, |m, _| {
					m.change_note_text_for(&tmp.path().join(name), m.locale)
						.expect("a note")
						.text
				})
			};
			let (a, b) = (note(cx, "ra"), note(cx, "rb"));
			assert_ne!(a, b);
			let up = |cx: &mut VisualTestContext, text: &str| {
				cx.run_until_parked();
				let key: &'static str =
					Box::leak(format!("tooltip:{text}").into_boxed_str());
				cx.debug_bounds(key).is_some()
			};
			cx.run_until_parked();
			let at = cx
				.debug_bounds("change-note:rb")
				.expect("rb note row")
				.center();
			cx.simulate_mouse_move(at, None, gpui::Modifiers::none());
			cx.executor().advance_clock(Duration::from_millis(700));
			settle(cx);
			let b_now = note(cx, "rb");
			assert!(up(cx, &b_now), "rb tooltip shown: {b_now}");

			// A clean repo found ahead of both (a rescan's new order): it
			// draws no row, so rb's note stays under the pointer while its
			// slot moves from 1 to 2, where a position would read ra's.
			model.update(cx, |m, cx| {
				m.change_repos.insert(
					0,
					crate::ChangeRepo {
						root: tmp.path().join("r0"),
						name: "r0".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 0,
					},
				);
				m.files = vec![
					make_test_file("a.txt", 1),
					make_test_file("b.txt", 2),
				];
				cx.notify();
			});
			settle(cx);
			assert!(up(cx, &b_now), "still rb's text after a reorder");
			assert!(!up(cx, &a), "never ra's text under rb's pointer");

			// Shrunk: rb is gone, so its tooltip is too, and ra's does not
			// take its place.
			model.update(cx, |m, cx| {
				m.change_repos.retain(|r| r.name != "rb");
				m.files = vec![make_test_file("a.txt", 1)];
				cx.notify();
			});
			settle(cx);
			assert!(!up(cx, &b_now), "rb's text is gone with rb");
			assert!(!up(cx, &a), "ra's text does not take over");
		}

		#[gpui::test]
		fn remote_unreadable_repo_is_an_error_not_clean(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let _alpha = repo(&shared, "alpha", &[]);
			let beta = repo(&shared, "beta", &[]);

			// Corrupt beta/.git/HEAD BEFORE opening
			fs::write(beta.join(".git/HEAD"), "garbage not a ref\n").unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			model.read_with(cx, |m, _| {
				let beta_slot = m
					.change_repos
					.iter()
					.position(|s| s.root.ends_with("beta"));
				assert!(beta_slot.is_some(), "beta slot should exist");
				let slot = &m.change_repos[beta_slot.unwrap()];
				assert!(
					matches!(slot.state, crate::ChangeRepoState::Failed(_)),
					"beta slot should be Failed, got: {:?}",
					slot.state
				);
				let layout = crate::ui::ChangeLayout {
					by_dir: false,
					expanded: |_, _, _| false,
				};
				let rows = crate::ui::change_rows(
					&m.change_repos,
					&m.files,
					|_| false,
					|_, _| false,
					"",
					&layout,
				);
				assert!(
					rows.iter().any(|r| matches!(
						r,
						crate::ui::ChangeItemRow::Note { .. }
					)),
					"change_rows should include a Note for beta"
				);
				assert_ne!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::Clean),
					"changes_empty_state should not be Clean"
				);
			});

			let gone = shared.with_extension("gone");
			fs::rename(&shared, &gone).unwrap();
			model.update(cx, |m, cx| m.reload_repos(cx));
			let mut scan_failed = false;
			for _ in 0..50 {
				settle(cx);
				scan_failed = model.read_with(cx, |m, _| {
					!m.is_loading
						&& matches!(
							m.changes_empty_state(),
							Some(crate::ChangesEmpty::ScanFailed(_))
						)
				});
				if scan_failed {
					break;
				}
			}
			assert!(scan_failed, "changes_empty_state should be ScanFailed");
			model.read_with(cx, |m, _| {
				assert_ne!(
					m.changes_empty_state(),
					Some(crate::ChangesEmpty::Clean)
				);
			});

			fs::rename(&gone, &shared).unwrap();
			model.update(cx, |m, cx| m.reload_repos(cx));
			let mut recovered = false;
			for _ in 0..50 {
				settle(cx);
				recovered = model.read_with(cx, |m, _| {
					!m.is_loading
						&& !m.repos.is_empty()
						&& !matches!(
							m.changes_empty_state(),
							Some(crate::ChangesEmpty::ScanFailed(_))
						)
				});
				if recovered {
					break;
				}
			}
			assert!(recovered, "repos should be back and not ScanFailed");
			model.read_with(cx, |m, _| {
				assert!(
					m.remote.scan_error.is_none(),
					"remote.scan_error should be None"
				);
			});
		}

		#[gpui::test]
		fn remote_old_worker_shows_too_old_not_clean(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();
			fs::write(shared.join("file.txt"), "hello\n").unwrap();
			let _r = repo(&shared, "repo", &[]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "ancient".into(),
					max_protocol: Some(1),
				},
			);

			// Without any manual refresh, wait (bounded) until ws_tree is loaded with top-level entries.
			for _ in 0..20 {
				settle(cx);
				let loaded = model.read_with(cx, |m, _| {
					m.ws_tree.as_ref().is_some_and(|t| {
						t.is_loaded
							&& t.children.iter().any(|c| c.name == "file.txt")
					})
				});
				if loaded {
					break;
				}
			}

			model.read_with(cx, |m, _| {
				let tree_has_file = m.ws_tree.as_ref().is_some_and(|t| {
					t.is_loaded
						&& t.children.iter().any(|c| c.name == "file.txt")
				});
				assert!(tree_has_file, "project tree should list file.txt");

				let scan_err = m
					.remote
					.scan_error
					.as_ref()
					.expect("scan_error expected");
				assert_eq!(scan_err.key, "remote_worker_too_old");

				let changes_state = m.changes_empty_state();
				assert!(
					matches!(&changes_state, Some(crate::ChangesEmpty::ScanFailed(msg)) if msg.key == "remote_worker_too_old"),
					"changes_empty_state should be ScanFailed(remote_worker_too_old), got: {changes_state:?}"
				);

				let log_state = m.log_empty_state();
				assert!(
					matches!(log_state, Some(crate::LogEmpty::Failed(_))),
					"log_empty_state should be Failed(..), got: {log_state:?}"
				);
				assert_ne!(log_state, Some(crate::LogEmpty::Empty));
			});

			model.update(cx, |m, cx| {
				m.dispatch_ws_tree(
					Some(crate::TreeCommand::OpenFile("file.txt".into())),
					cx,
				);
			});
			settle(cx);

			for _ in 0..20 {
				let done = model.read_with(cx, |m, _| {
					!m.preview_loading && m.preview.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			model.read_with(cx, |m, _| {
				let p = m.preview.as_ref().expect("preview loaded");
				assert_eq!(p.text.as_ref(), "hello\n");
				assert_eq!(m.selected_file.as_deref(), Some("file.txt"));
			});
		}

		#[cfg(unix)]
		#[gpui::test]
		fn served_git_in_flight_does_not_block_a_workspace_switch(
			_cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			if !crate::run_isolated(
				"tests::in_process::served_git_in_flight_does_not_block_a_workspace_switch",
			) {
				return;
			}
			use snip_core::gitrun::{served_in_flight, GitPool, RunOptions};
			use snip_core::gitsrc::Git;

			let dir = tempfile::tempdir().unwrap();
			crate::paste::tests::git_init(dir.path());
			let git = Git::open(dir.path()).unwrap();

			let load_before = crate::lifecycle::GitLoad::current();

			let opts = RunOptions {
				pool: GitPool::Served,
				timeout: Duration::from_secs(10),
				..Default::default()
			};
			let handle = std::thread::spawn(move || {
				git.run_with(&["-c", "alias.slow=!sleep 3", "slow"], &opts)
			});

			let deadline = std::time::Instant::now() + Duration::from_secs(5);
			while served_in_flight() == 0 {
				assert!(
					std::time::Instant::now() < deadline,
					"served_in_flight never became 1"
				);
				std::thread::sleep(Duration::from_millis(10));
			}

			assert_eq!(served_in_flight(), 1);
			let load_after = crate::lifecycle::GitLoad::current();
			assert_eq!(
				load_after, load_before,
				"GitLoad should not count Served pool git calls"
			);
			assert_eq!(load_after, crate::lifecycle::GitLoad::idle());

			let mut lc = crate::lifecycle::Lifecycle::new(1);
			let now = std::time::Instant::now();
			let req = lc.request(crate::lifecycle::Intent::CloseWorkspace, now);
			assert_eq!(req, crate::lifecycle::Request::Accepted);
			let step = lc.poll_at(now, load_after);
			assert_eq!(
				step,
				crate::lifecycle::Step::Ready(
					crate::lifecycle::Intent::CloseWorkspace
				),
				"drain should reach Ready right away while served git is in flight"
			);

			let res = handle.join().expect("slow git thread join");
			assert!(res.is_ok());
			assert_eq!(served_in_flight(), 0);
		}

		#[gpui::test]
		fn remote_repo_rows_never_reveal_a_local_path(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let _alpha = repo(&shared, "alpha", &[("change.txt", "mod\n")]);
			let _beta = repo(&shared, "beta", &[]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			let mut ready = false;
			for _ in 0..50 {
				settle(cx);
				ready =
					model.read_with(cx, |m, _| {
						!m.is_loading
							&& m.repos.len() == 2 && m.change_repos.len() == 2
							&& m.change_repos.iter().all(|s| {
								s.state == crate::ChangeRepoState::Loaded
							}) && !m.files.is_empty()
					});
				if ready {
					break;
				}
			}
			assert!(ready, "changes were not loaded");

			model.read_with(cx, |m, _| {
				let session = m.remote.session.as_ref().unwrap();
				let worker_ws = &session.workspace.path;

				let check_entries = |entries: Vec<crate::menu::MenuEntry>,
				                     label: &str| {
					let mut found_copy_path = false;
					for entry in entries {
						match entry {
							crate::menu::MenuEntry::Sep => {}
							crate::menu::MenuEntry::Item { id, act, .. } => {
								assert_ne!(
									id, "reveal",
									"{label}: entry has id 'reveal'"
								);
								assert!(
									!matches!(
										act,
										Some(crate::menu::MenuAct::Reveal(_))
									),
									"{label}: entry has MenuAct::Reveal"
								);
								if id == "copy-path" {
									found_copy_path = true;
									match act {
										Some(
											crate::menu::MenuAct::CopyText(p),
										) => {
											assert!(
												p.starts_with(worker_ws),
												"{label}: copy-path text '{p}' should start with worker ws '{worker_ws}'"
											);
											assert!(
												!p.contains("snip-remote://"),
												"{label}: copy-path text '{p}' contains 'snip-remote://'"
											);
										}
										other => panic!(
											"{label}: unexpected copy-path act: {other:?}"
										),
									}
								}
							}
						}
					}
					assert!(found_copy_path, "{label}: missing copy-path entry");
				};

				for i in 0..m.repos.len() {
					check_entries(
						m.repo_row_menu(i),
						&format!("repo_row_menu({i})"),
					);
				}

				for slot in 0..m.change_repos.len() {
					check_entries(
						m.change_repo_menu(slot, "unstaged"),
						&format!("change_repo_menu({slot})"),
					);
				}

				let change_idx =
					m.files.iter().position(|f| f.path == "change.txt");
				assert!(change_idx.is_some(), "expected change.txt row");
				check_entries(
					m.change_row_menu(change_idx.unwrap()),
					"change_row_menu",
				);
			});
		}

		#[gpui::test]
		fn remote_work_row_menu_copy_path_exact_worker_path(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let alpha = shared.join("alpha");
			fs::create_dir_all(alpha.join("src")).unwrap();
			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("src/a.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "src/a.txt"]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);

			let beta = shared.join("beta");
			fs::create_dir_all(&beta).unwrap();
			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("file.txt"), "beta\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "file.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init"]);

			fs::write(shared.join("README.md"), "readme\n").unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			settle(cx);
			for _ in 0..50 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading
						&& m.repos.len() == 2
						&& m.change_repos.len() == 2
						&& m.change_repos
							.iter()
							.all(|s| s.state == crate::ChangeRepoState::Loaded)
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}

			let copy_path_text =
				|entries: Vec<crate::menu::MenuEntry>| -> String {
					for entry in entries {
						if let crate::menu::MenuEntry::Item {
							id, act, ..
						} = entry
						{
							if id == "copy-path" {
								if let Some(crate::menu::MenuAct::CopyText(p)) =
									act
								{
									return p;
								}
							}
						}
					}
					panic!("missing copy-path entry in menu");
				};

			// 1. Nested repo alpha repo-tree row
			model.read_with(cx, |m, _| {
				let session = m.remote.session.as_ref().unwrap();
				let worker_ws = session.workspace.path.trim_end_matches('/');

				let repo_row = crate::tree::FlattenedTreeRow {
					name: "a.txt".into(),
					rel_path: "src/a.txt".into(),
					id_suffix: String::new(),
					key: crate::tree::NodeKey::from_utf8_rel("src/a.txt"),
					is_dir: false,
					is_nested_repo: false,
					is_expanded: false,
					is_truncation_marker: false,
					is_more_marker: false,
					is_view_limit: false,
					is_error: false,
					msg: None,
					is_loading: false,
					is_valid_utf8: true,
					selected: false,
					depth: 1,
				};
				let text = copy_path_text(m.work_row_menu(&repo_row, false));
				assert_eq!(text, format!("{worker_ws}/alpha/src/a.txt"));

				// 2. Multi-repo share ws-tree row
				let ws_row = crate::tree::FlattenedTreeRow {
					name: "README.md".into(),
					rel_path: "README.md".into(),
					id_suffix: String::new(),
					key: crate::tree::NodeKey::from_utf8_rel("README.md"),
					is_dir: false,
					is_nested_repo: false,
					is_expanded: false,
					is_truncation_marker: false,
					is_more_marker: false,
					is_view_limit: false,
					is_error: false,
					msg: None,
					is_loading: false,
					is_valid_utf8: true,
					selected: false,
					depth: 0,
				};
				let ws_text = copy_path_text(m.work_row_menu(&ws_row, true));
				assert_eq!(ws_text, format!("{worker_ws}/README.md"));
			});

			// 3. Single-repo share
			let single_repo = tmp.path().join("single_repo");
			fs::create_dir_all(single_repo.join("src")).unwrap();
			crate::paste::tests::git_init(&single_repo);
			fs::write(single_repo.join("src/a.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&single_repo, &["add", "src/a.txt"]);
			crate::paste::tests::git_run(
				&single_repo,
				&["commit", "-m", "init"],
			);

			let (single_model, single_cx, _worker2) = open_remote(
				cx,
				&single_repo,
				snip_remote::WorkerOptions {
					name: "worker2".into(),
					max_protocol: None,
				},
			);

			settle(single_cx);
			for _ in 0..50 {
				let done = single_model.read_with(single_cx, |m, _| {
					!m.is_loading
						&& m.repos.len() == 1
						&& m.change_repos.len() == 1
						&& m.change_repos[0].state
							== crate::ChangeRepoState::Loaded
				});
				if done {
					break;
				}
				single_cx.run_until_parked();
				single_cx
					.executor()
					.advance_clock(Duration::from_millis(50));
			}

			single_model.read_with(single_cx, |m, _| {
				let session = m.remote.session.as_ref().unwrap();
				let worker_ws = session.workspace.path.trim_end_matches('/');

				let repo_row = crate::tree::FlattenedTreeRow {
					name: "a.txt".into(),
					rel_path: "src/a.txt".into(),
					id_suffix: String::new(),
					key: crate::tree::NodeKey::from_utf8_rel("src/a.txt"),
					is_dir: false,
					is_nested_repo: false,
					is_expanded: false,
					is_truncation_marker: false,
					is_more_marker: false,
					is_view_limit: false,
					is_error: false,
					msg: None,
					is_loading: false,
					is_valid_utf8: true,
					selected: false,
					depth: 1,
				};
				let text = copy_path_text(m.work_row_menu(&repo_row, false));
				assert_eq!(text, format!("{worker_ws}/src/a.txt"));
			});
		}

		#[gpui::test]
		fn add_repo_path_is_refused_in_a_remote_session(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();
			let _alpha = repo(&shared, "alpha", &[]);

			let local_repo = tmp.path().join("local");
			let _local = repo(tmp.path(), "local", &[]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			let repos_count = model.read_with(cx, |m, _| m.repos.len());
			assert_eq!(repos_count, 1);

			model.update(cx, |m, cx| {
				m.add_repo_path(local_repo, cx);
				assert_eq!(m.status.key, "remote_unsupported");
				assert_eq!(m.repos.len(), repos_count);
				assert!(m.add_cancel.is_none());
			});
		}

		#[gpui::test]
		fn menu_act_reveal_refused_in_remote_session(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "worker".into(),
					max_protocol: None,
				},
			);

			model.update(cx, |m, _| m.set_status("status_idle", []));
			cx.update(|window, cx| {
				model.update(cx, |m, cx| {
					m.run_menu_act(
						crate::menu::MenuAct::Reveal(PathBuf::from(
							"/fake/path",
						)),
						window,
						cx,
					);
					assert_eq!(m.status.key, "remote_unsupported");
				});
			});
		}

		/// Keys reach the Workbench once the workspace menu closes. Opening a
		/// folder with the menu's button left the focus on that button, which
		/// went with the menu, and Escape in the path field left it on the
		/// field: either way the first Cmd+V did nothing.
		#[gpui::test]
		fn keys_reach_the_workbench_after_the_workspace_menu_closes(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(&shared).unwrap();
			let worker = std::sync::Arc::new(snip_remote::Worker::new(
				snip_remote::WorkerOptions::default(),
			));
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(None, None, "normal".into(), cx)
			});
			cx.run_until_parked();
			let reaches = |cx: &mut VisualTestContext| {
				cx.update(|window, cx| {
					model.read(cx).focus_handle.contains_focused(window, cx)
				})
			};
			model.update(cx, |m, cx| {
				m.toggle_workspace_menu(cx);
				m.remote.hosts =
					vec![snip_remote::RemoteHost::in_process(worker.clone())];
				m.remote.recent.clear();
				m.browse_remote_host(0, cx);
			});
			settle(cx);
			let path = shared.display().to_string();
			model.update(cx, |m, cx| {
				m.remote_path_input
					.update(cx, |i, cx| i.set_text(&path, cx));
			});
			settle(cx);
			let open = cx
				.debug_bounds("btn-remote-open")
				.expect("open button drawn");
			cx.simulate_click(open.center(), gpui::Modifiers::none());
			// The open replies on a worker thread; the menu closes with it.
			let deadline = std::time::Instant::now() + Duration::from_secs(10);
			while model.read_with(cx, |m, _| {
				m.overlays
					.is_open(crate::overlay::OverlayKind::WorkspaceMenu)
			}) {
				assert!(
					std::time::Instant::now() < deadline,
					"the open never replied"
				);
				settle(cx);
			}
			land_remote_open(&model, cx);
			settle(cx);
			model.read_with(cx, |m, _| assert!(m.remote.session.is_some()));
			assert!(reaches(cx), "after opening with the menu's button");

			model.update(cx, |m, cx| {
				m.toggle_workspace_menu(cx);
				m.browse_remote_host(0, cx);
			});
			settle(cx);
			cx.simulate_keystrokes("escape");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(!m
					.overlays
					.is_open(crate::overlay::OverlayKind::WorkspaceMenu))
			});
			assert!(reaches(cx), "after Escape in the path field");
		}

		/// The workspace menu closes on Escape in the pairing form and on a
		/// second press of its own button, as a popup menu should.
		#[gpui::test]
		fn workspace_menu_closes_on_escape_and_on_its_button(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let worker = std::sync::Arc::new(snip_remote::Worker::new(
				snip_remote::WorkerOptions::default(),
			));
			model.update(cx, |m, cx| {
				m.toggle_workspace_menu(cx);
				m.remote.hosts =
					vec![snip_remote::RemoteHost::in_process(worker.clone())];
				m.browse_remote_host(0, cx);
			});
			settle(cx);
			let field = model.read_with(cx, |m, cx| {
				assert!(m
					.overlays
					.is_open(crate::overlay::OverlayKind::WorkspaceMenu));
				m.remote_path_input.read(cx).handle()
			});
			assert!(
				cx.update(|window, _| field.is_focused(window)),
				"the path field has the focus"
			);
			cx.simulate_keystrokes("escape");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					!m.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu),
					"Escape in the path field"
				);
			});

			// Opened by its button, nothing inside focused: Escape still closes.
			let button = cx
				.debug_bounds("btn-workspace-menu")
				.expect("menu button drawn");
			cx.simulate_click(button.center(), gpui::Modifiers::none());
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m
					.overlays
					.is_open(crate::overlay::OverlayKind::WorkspaceMenu))
			});
			cx.simulate_keystrokes("escape");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					!m.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu),
					"Escape with no field focused"
				);
			});

			model.update(cx, |m, cx| m.toggle_workspace_menu(cx));
			settle(cx);
			let button = cx
				.debug_bounds("btn-workspace-menu")
				.expect("menu button drawn");
			cx.simulate_click(button.center(), gpui::Modifiers::none());
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					!m.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu),
					"second press of the button"
				);
			});
		}

		#[gpui::test]
		fn commit_files_cmd_and_shift_select_several(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let files = ["a.txt", "b.txt", "c.txt", "d.txt"];
			model.update(cx, |m, cx| {
				m.commit_view.commit_files =
					files.iter().map(|f| (f.to_string(), None)).collect();
				m.commit_view.selected_commit_file = Some("b.txt".into());
				m.toggle_commit_file("d.txt", cx);
				assert_eq!(m.commit_view.commit_file_sel, ["b.txt", "d.txt"]);
				m.toggle_commit_file("b.txt", cx);
				assert_eq!(m.commit_view.commit_file_sel, ["d.txt"]);
				m.extend_commit_files(&files, "d.txt", cx);
				assert_eq!(
					m.commit_view.commit_file_sel,
					["b.txt", "c.txt", "d.txt"]
				);
			});
		}

		/// Copy files on a selection of a folder and files copies each file
		/// once, deletions included (they go out as `[DELETED]`).
		#[gpui::test]
		fn commit_files_copy_folder_and_files_together(
			cx: &mut TestAppContext,
		) {
			use crate::menu::{MenuAct, MenuEntry};
			use snip_core::format::ChangeType::{Deleted, Modified};
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let copied = model.update(cx, |m, cx| {
				m.commit_view.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.commit_view.commit_files = [
					("src/a.rs", Modified),
					("src/gone.rs", Deleted),
					("pom.xml", Modified),
					("README.md", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				m.commit_view.selected_commit_file = Some("src/a.rs".into());
				m.toggle_commit_file("src/", cx);
				m.toggle_commit_file("pom.xml", cx);
				let menu = m.commit_file_menu("src", true);
				menu.into_iter()
					.find_map(|e| match e {
						MenuEntry::Item {
							act: Some(MenuAct::CopyNode(f)),
							..
						} => Some(f),
						_ => None,
					})
					.unwrap()
			});
			let got: Vec<_> = copied
				.iter()
				.map(|t| {
					(
						t.path.as_str(),
						t.change_type
							== Some(snip_core::format::ChangeType::Deleted),
					)
				})
				.collect();
			assert_eq!(
				got,
				[
					("src/a.rs", false),
					("src/gone.rs", true),
					("pom.xml", false)
				]
			);
		}

		/// Multi-selection copy follows screen order, not click order
		/// (Scenario A: emoji.txt clicked before common.txt, but screen
		/// order has common.txt before emoji.txt).
		#[gpui::test]
		fn commit_files_copy_files_screen_order(cx: &mut TestAppContext) {
			use crate::menu::{MenuAct, MenuEntry};
			use snip_core::format::ChangeType::{Deleted, Modified};
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let copied = model.update(cx, |m, cx| {
				m.commit_view.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.commit_view.commit_files = [
					("common.txt", Modified),
					("emoji.txt", Modified),
					("dir/gone.txt", Deleted),
					("dir/keep.txt", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				m.commit_view.selected_commit_file = Some("emoji.txt".into());
				m.toggle_commit_file("common.txt", cx);
				let menu = m.commit_file_menu("common.txt", false);
				menu.into_iter()
					.find_map(|e| match e {
						MenuEntry::Item {
							act: Some(MenuAct::CopyNode(f)),
							..
						} => Some(f),
						_ => None,
					})
					.unwrap()
			});
			let got: Vec<_> = copied
				.iter()
				.map(|t| {
					(
						t.path.as_str(),
						t.change_type
							== Some(snip_core::format::ChangeType::Deleted),
					)
				})
				.collect();
			assert_eq!(got, [("common.txt", false), ("emoji.txt", false)]);
		}

		/// Flat layout (`log_details_by_dir = false`) copies in screen order
		/// (natural path order), not click order: b.txt is clicked before
		/// z.txt, but the flat screen shows a/x.txt, b.txt, z.txt. (In-process
		/// tests default to the by-dir layout, which sorts dirs and names and
		/// hides a click-order bug.)
		#[gpui::test]
		fn commit_files_copy_flat_layout_screen_order(cx: &mut TestAppContext) {
			use crate::menu::{MenuAct, MenuEntry};
			use snip_core::format::ChangeType::Modified;
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let copied = model.update(cx, |m, cx| {
				m.commit_view.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.log_details_by_dir = false;
				m.commit_view.commit_files = [
					("z.txt", Modified),
					("a/x.txt", Modified),
					("b.txt", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				// Click order: b.txt, a/x.txt, z.txt.
				m.commit_view.selected_commit_file = Some("b.txt".into());
				m.toggle_commit_file("a/x.txt", cx);
				m.toggle_commit_file("z.txt", cx);
				let menu = m.commit_file_menu("z.txt", false);
				menu.into_iter()
					.find_map(|e| match e {
						MenuEntry::Item {
							act: Some(MenuAct::CopyNode(f)),
							..
						} => Some(f),
						_ => None,
					})
					.unwrap()
			});
			let got: Vec<_> = copied
				.iter()
				.map(|t| {
					(
						t.path.as_str(),
						t.change_type
							== Some(snip_core::format::ChangeType::Deleted),
					)
				})
				.collect();
			// Flat copy follows the natural screen order.
			assert_eq!(
				got,
				[("a/x.txt", false), ("b.txt", false), ("z.txt", false)]
			);
		}

		/// Folder overlap copy follows screen order and deduplicates
		/// (Scenario B: dir/keep.txt clicked before dir/, screen order has
		/// dir/gone.txt before dir/keep.txt, each file copied once).
		#[gpui::test]
		fn commit_files_copy_folder_overlap_screen_order(
			cx: &mut TestAppContext,
		) {
			use crate::menu::{MenuAct, MenuEntry};
			use snip_core::format::ChangeType::{Deleted, Modified};
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let copied = model.update(cx, |m, cx| {
				m.commit_view.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.commit_view.commit_files = [
					("common.txt", Modified),
					("emoji.txt", Modified),
					("dir/gone.txt", Deleted),
					("dir/keep.txt", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				m.commit_view.selected_commit_file =
					Some("dir/keep.txt".into());
				m.toggle_commit_file("dir/", cx);
				let menu = m.commit_file_menu("dir", true);
				menu.into_iter()
					.find_map(|e| match e {
						MenuEntry::Item {
							act: Some(MenuAct::CopyNode(f)),
							..
						} => Some(f),
						_ => None,
					})
					.unwrap()
			});
			let got: Vec<_> = copied
				.iter()
				.map(|t| {
					(
						t.path.as_str(),
						t.change_type
							== Some(snip_core::format::ChangeType::Deleted),
					)
				})
				.collect();
			assert_eq!(got, [("dir/gone.txt", true), ("dir/keep.txt", false)]);
		}

		/// Puts `payload` on the OS clipboard and opens its paste preview.
		fn paste(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			payload: &str,
		) {
			clip::write_text(payload).unwrap();
			cx.simulate_keystrokes("ctrl-v");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().is_some(), "no preview: {}", m.status)
			});
		}

		/// `dest_path` comes back canonical (macOS `/var` → `/private/var`,
		/// Windows short names), so the destination is canonical from the start.
		fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
			let tmp = tempfile::tempdir().unwrap();
			let root = dunce::canonicalize(tmp.path()).unwrap();
			(tmp, root)
		}

		#[gpui::test]
		fn opening_a_workspace_lists_each_repository_with_its_changes(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			for (name, dirty) in [("alpha", 1), ("beta", 2)] {
				let repo = ws.path().join(name);
				std::fs::create_dir(&repo).unwrap();
				git(&repo, &["init", "-q", "-b", "main"]);
				std::fs::write(repo.join("base.txt"), "base\n").unwrap();
				git(&repo, &["add", "."]);
				git(&repo, &["commit", "-q", "-m", "base"]);
				for i in 0..dirty {
					std::fs::write(repo.join(format!("new{i}.txt")), "x\n")
						.unwrap();
				}
			}
			let root = ws.path().to_path_buf();
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(Some(root), None, "normal".into(), cx)
			});
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				let mut seen: Vec<(String, usize)> = m
					.repos
					.iter()
					.map(|r| {
						let s = r.summary.as_ref().expect("repo summary");
						(r.name.clone(), s.changes.untracked)
					})
					.collect();
				seen.sort();
				assert_eq!(
					seen,
					[("alpha".to_string(), 1), ("beta".to_string(), 2)]
				);
			});
		}

		/// A keystroke goes through the real keymap and focus dispatch, the
		/// path a user's key takes, not a direct method call.
		#[gpui::test]
		fn alt_9_toggles_the_git_log_through_the_keymap(
			cx: &mut TestAppContext,
		) {
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let ws = tempfile::tempdir().unwrap();
			let root = ws.path().to_path_buf();
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(Some(root), None, "normal".into(), cx)
			});
			cx.run_until_parked();
			cx.update(|window, cx| {
				window.focus(&model.read(cx).focus_handle.clone())
			});
			let before = model.read_with(cx, |m, _| m.bottom_visible);
			cx.simulate_keystrokes("alt-9");
			cx.run_until_parked();
			assert_eq!(model.read_with(cx, |m, _| m.bottom_visible), !before);
			cx.simulate_keystrokes("alt-9");
			cx.run_until_parked();
			assert_eq!(model.read_with(cx, |m, _| m.bottom_visible), before);
		}

		/// With no file open the reader is never drawn, yet Esc in the
		/// Project tree hands it the focus: the handle stays valid, keys
		/// fell to the dispatch tree's root and Alt+9, Alt+1, Cmd+V all
		/// went dead. Driven the way the user drives it: Alt+1 takes the
		/// tree, Esc leaves it, then the keys must still work.
		#[gpui::test]
		fn keys_survive_escape_from_the_tree_without_a_preview(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);
			// Alt+1 opens the Project tool window and focuses its tree.
			cx.simulate_keystrokes("alt-1");
			cx.update(|window, cx| {
				assert!(
					model.read(cx).tree_focus.is_focused(window),
					"Alt+1 must focus the tree",
				);
			});
			cx.simulate_keystrokes("escape");
			settle(cx);
			// Alt+9 still toggles the log…
			let before = model.read_with(cx, |m, _| m.bottom_visible);
			cx.simulate_keystrokes("alt-9");
			assert_eq!(
				model.read_with(cx, |m, _| m.bottom_visible),
				!before,
				"Alt+9 must work after Esc in the tree",
			);
			cx.simulate_keystrokes("alt-9");
			assert_eq!(model.read_with(cx, |m, _| m.bottom_visible), before);
			// …and Alt+1 still opens the Project tool window.
			cx.simulate_keystrokes("alt-1");
			model.read_with(cx, |m, _| {
				assert!(m.left_visible);
				assert_eq!(m.active_tab, WorkbenchTab::FileExplorer);
			});
		}

		/// Alt+9 closing a log that held the focus left the focus on the
		/// no-longer-drawn list, and the next Alt+9 never reached the
		/// Workbench: the log would not come back.
		#[gpui::test]
		fn alt_9_reopens_the_log_after_closing_it_while_it_holds_the_focus(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);
			// The log starts open without the focus: close, then open the
			// way Alt+9 does, with the focus on the log.
			cx.simulate_keystrokes("alt-9");
			assert!(!model.read_with(cx, |m, _| m.bottom_visible));
			cx.simulate_keystrokes("alt-9");
			assert!(model.read_with(cx, |m, _| m.bottom_visible));
			cx.update(|window, cx| {
				assert!(model.read(cx).log_focus.is_focused(window));
			});
			// Closing it while it holds the focus must not strand the
			// keyboard on the hidden list.
			cx.simulate_keystrokes("alt-9");
			assert!(!model.read_with(cx, |m, _| m.bottom_visible));
			cx.simulate_keystrokes("alt-9");
			assert!(
				model.read_with(cx, |m, _| m.bottom_visible),
				"Alt+9 must reopen the log",
			);
		}

		/// The Repository menu changes only the Log scope. Its branch list
		/// and graph must agree even when the Project selector names another
		/// repository, or when there is no single checked-out branch.
		#[gpui::test]
		fn log_branch_head_glyph_follows_the_clicked_repository_scope(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			let beta = repo(ws.path(), "beta", &[]);
			let detached = repo(ws.path(), "detached", &[]);
			for root in [&alpha, &beta, &detached] {
				git(root, &["branch", "feature"]);
			}
			git(&beta, &["checkout", "-q", "feature"]);
			git(&detached, &["checkout", "-q", "--detach"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 3);
				assert!(!m.refs.is_empty(), "branch refs must be loaded");
			});

			let assert_scope =
				|cx: &mut VisualTestContext,
				 project: &str,
				 log: Option<(&Path, Option<&str>)>| {
					settle(cx);
					model.read_with(cx, |m, _| {
						assert_eq!(m.repo().unwrap().name, project);
						if let Some((root, branch)) = log {
							assert_eq!(
								m.log_scope_key,
								[dunce::canonicalize(root).unwrap()]
							);
							let commit = m
								.display_commits()
								.into_iter()
								.next()
								.expect("loaded log");
							let current = m.log_current_branch(&commit.sha);
							assert_eq!(
								current.as_deref(),
								branch,
								"graph repository"
							);
							let graph =
								m.graph_layout.as_ref().expect("loaded graph");
							let (badges, _) = crate::graph_view::visible_refs(
								&graph.rows[0].refs,
								current.as_deref(),
							);
							let label = crate::graph_view::combined_label(
								&badges,
								current.as_deref(),
							)
							.expect("HEAD graph label");
							assert!(
								label.current,
								"graph must mark HEAD, including detached"
							);
						} else {
							assert!(m.log_is_merged());
						}
					});
					let expected = log.and_then(|(_, branch)| branch);
					for (branch, row, head) in [
						(
							"main",
							"ref:refs/heads/main",
							"ref-head:refs/heads/main",
						),
						(
							"feature",
							"ref:refs/heads/feature",
							"ref-head:refs/heads/feature",
						),
					] {
						assert!(
							cx.debug_bounds(row).is_some(),
							"visible branch {row}"
						);
						assert_eq!(
							cx.debug_bounds(head).is_some(),
						expected == Some(branch),
						"Project={project}, Log={log:?}: {branch} HEAD glyph",
					);
					}
				};

			// Project A / Log B, using the repository text row, not its checkbox.
			click(cx, "btn-repo-selector");
			click(cx, "pick-repo:alpha");
			click(cx, "branch-group:refs_local");
			click(cx, "log-filter-repo");
			click(cx, "log-repo:beta");
			settle(cx);
			click(cx, "branch-group:refs_local");
			assert_scope(cx, "alpha", Some((&beta, Some("feature"))));

			// A Project switch preserves the fixed Log scope; same-repo control.
			click(cx, "btn-repo-selector");
			click(cx, "pick-repo:beta");
			assert_scope(cx, "beta", Some((&beta, Some("feature"))));

			// Reverse branches: Project feature / Log main.
			click(cx, "log-filter-repo");
			click(cx, "log-repo:alpha");
			assert_scope(cx, "beta", Some((&alpha, Some("main"))));
			click(cx, "btn-repo-selector");
			click(cx, "pick-repo:alpha");
			assert_scope(cx, "alpha", Some((&alpha, Some("main"))));

			click(cx, "log-filter-repo");
			click(cx, "log-repo:detached");
			assert_scope(cx, "alpha", Some((&detached, None)));

			// Merged branch names belong to several repos: no single HEAD glyph.
			click(cx, "log-filter-repo");
			click(cx, "log-repo:all");
			assert_scope(cx, "alpha", None);
		}

		/// Picking a branch in the Branch menu used to close the menu and
		/// strand the keyboard on the closed menu's field: Up/Down and
		/// Cmd+C stopped working. The log keeps the keyboard, as it does
		/// after Esc.
		#[gpui::test]
		fn picking_a_branch_keeps_the_log_keyboard(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			for i in 0..3 {
				fs::write(alpha.join(format!("f{i}.txt")), "x").unwrap();
				git(&alpha, &["add", "."]);
				git(&alpha, &["commit", "-q", "-m", &format!("c{i}")]);
			}
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			// The filter chips give way first on a narrow window. The bar
			// re-measures its search field on the first frames, which moves
			// the chips: wait until they stop moving, or the click below
			// lands on a neighbour.
			cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(752.)));
			let mut last_chip = None;
			for _ in 0..10 {
				settle(cx);
				let pos = cx
					.debug_bounds("log-filter-branch")
					.map(|b| (f32::from(b.origin.x), f32::from(b.origin.y)));
				if pos.is_some() && pos == last_chip {
					break;
				}
				last_chip = pos;
			}
			for _ in 0..10 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					m.display_commits().len() >= 3 && !m.refs.is_empty()
				});
				if ready {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				assert!(
					m.display_commits().len() >= 3 && !m.refs.is_empty(),
					"the log and its refs must be loaded",
				);
			});
			// Open the Branch menu, expand its Local section, pick a branch.
			{
				let b = cx.debug_bounds("log-filter-branch").unwrap();
				for (dx, dy) in [
					(0., 0.),
					(-8., -4.),
					(0., -6.),
					(8., 0.),
					(0., 4.),
					(-20., 0.),
					(-30., 0.),
				] {
					let pos = gpui::point(
						b.center().x + gpui::px(dx),
						b.center().y + gpui::px(dy),
					);
					cx.simulate_click(pos, gpui::Modifiers::none());
					let menu =
						model.read_with(cx, |m, _| m.overlays.log_menu());
					if menu.is_some() {
						break;
					}
				}
			}
			click(cx, "log-branch-group:refs_local");
			let branch = model.read_with(cx, |m, _| {
				m.refs
					.iter()
					.map(|r| r.name.clone())
					.find(|n| n.starts_with("refs/heads/"))
					.expect("a local branch")
			});
			click(
				cx,
				Box::leak(format!("log-branch:{branch}").into_boxed_str()),
			);
			settle(cx);
			let before = model.read_with(cx, |m, _| m.selected_commit.clone());
			cx.simulate_keystrokes("down");
			let after = model.read_with(cx, |m, _| m.selected_commit.clone());
			assert!(
				after.is_some() && after != before,
				"Down must move the log selection after a branch pick \
				 (before: {before:?}, after: {after:?})",
			);
		}

		/// A remote open that lands after the user opened a local workspace
		/// from the recents must leave that workspace and the status bar
		/// alone. The open is held at the test clock, so the worker's
		/// answer cannot land before the click.
		#[gpui::test]
		fn a_late_remote_open_leaves_a_local_workspace_alone(
			cx: &mut TestAppContext,
		) {
			let local = tempfile::tempdir().unwrap();
			let remote_ws = tempfile::tempdir().unwrap();
			let worker = std::sync::Arc::new(snip_remote::Worker::new(
				snip_remote::WorkerOptions::default(),
			));
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(None, None, "normal".into(), cx)
			});
			cx.run_until_parked();
			model.update(cx, |m, cx| {
				m.open_overlay(
					crate::overlay::OverlayKind::WorkspaceMenu,
					crate::overlay::ReturnFocus::Root,
					Some(crate::overlay::TriggerId::Workspace),
					None,
					cx,
				);
				m.recent_workspaces = vec![local.path().to_path_buf()];
				m.remote.hosts =
					vec![snip_remote::RemoteHost::in_process(worker.clone())];
				m.remote.recent.clear();
			});
			cx.run_until_parked();
			// The remote open starts and stays in flight at the test clock.
			model.update(cx, |m, cx| {
				m.e2e_remote_open_delay = Some(Duration::from_millis(100));
				m.open_remote_path(
					0,
					remote_ws.path().display().to_string(),
					cx,
				);
			});
			// While it is in flight, the user opens a local workspace.
			click(cx, "workspace-recent:0");
			// The answer lands now; it must be dropped.
			for _ in 0..5 {
				settle(cx);
			}
			settle(cx);
			// The open goes through the shared close drain, which waits for
			// every git child in this test binary (GitLoad is process wide);
			// under parallel tests it may not end on its own. Poll it as if
			// git were idle, like land_remote_open does.
			for _ in 0..10 {
				let open = model.read_with(cx, |m, _| m.workspace_open);
				if open {
					break;
				}
				model.update(cx, |m, cx| {
					let step = m.lifecycle.poll_at(
						std::time::Instant::now(),
						crate::lifecycle::GitLoad::idle(),
					);
					if let crate::lifecycle::Step::Ready(
						crate::lifecycle::Intent::OpenWorkspace(path),
					) = step
					{
						m.finish_open(path, cx);
					}
				});
				settle(cx);
			}
			let local_path = dunce::canonicalize(local.path()).unwrap();
			model.read_with(cx, |m, _| {
				assert!(m.workspace_open, "the local workspace must be open");
				assert!(
					m.remote.session.is_none(),
					"the late remote open must be dropped",
				);
				assert_eq!(m.workspace_root, local_path);
				assert!(
					!matches!(
						m.status.key,
						"remote_open_failed" | "remote_opened"
					),
					"the status bar was overwritten: {}",
					m.status.key,
				);
			});
		}

		/// Writes a file over the preview limit and commits it in `r`.
		fn commit_oversized(r: &Path) {
			fs::write(
				r.join("big.txt"),
				"x".repeat(snip_core::browser::PREVIEW_LIMIT + 10) + "\n",
			)
			.unwrap();
			git(r, &["add", "."]);
			git(r, &["commit", "-q", "-m", "add big"]);
		}

		/// Clicks the first log row, then checks the preview says the file
		/// is over the limit, with no byte count.
		fn assert_too_large_preview(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			settle_until(cx, "log rows", |cx| {
				model.read_with(cx, |m, _| !m.display_commits().is_empty())
			});
			let row = cx.debug_bounds("log-subject:0").expect("row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			settle_until(cx, "preview error", |cx| {
				model.read_with(cx, |m, _| m.preview_error.is_some())
			});
			let err = model.read_with(cx, |m, _| m.preview_error.clone());
			let err = err.expect("preview error");
			assert_eq!(err.key, "error_preview_too_large", "{err:?}");
			let text = err.render(crate::i18n::Locale::ZhTw);
			assert!(text.contains("big.txt"), "{text}");
			assert!(!text.contains("bytes"), "{text}");
		}

		#[gpui::test]
		fn log_preview_of_a_file_over_the_limit_says_too_large(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "big", &[]);
			commit_oversized(&r);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
			assert_too_large_preview(&model, cx);
		}

		#[gpui::test]
		fn remote_log_preview_of_a_file_over_the_limit_says_too_large(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let shared = tempfile::tempdir().unwrap();
			let r = repo(shared.path(), "big", &[]);
			commit_oversized(&r);
			let (model, cx, _worker) = open_remote(
				cx,
				shared.path(),
				snip_remote::WorkerOptions::default(),
			);
			assert_too_large_preview(&model, cx);
		}

		/// One fixture for both hosts (#214): a tracked file under the
		/// preview limit whose every line changed, so its diff passes the
		/// limit, and an untracked file of exactly the limit, whose
		/// synthesized patch does. Both say the DIFF is too large, never
		/// that the file is.
		fn patch_limit_fixture(ws: &Path) {
			let r = repo(ws, "big", &[]);
			let old: String =
				(0..52_000).map(|i| format!("old-{i:07}\n")).collect();
			fs::write(r.join("tracked.txt"), &old).unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "tracked"]);
			let new: String =
				(0..52_000).map(|i| format!("new-{i:07}\n")).collect();
			fs::write(r.join("tracked.txt"), &new).unwrap();
			let limit = snip_core::browser::PREVIEW_LIMIT;
			assert!(new.len() < limit && new.len() > limit / 2);
			assert!(old.len() + new.len() > limit);
			let untracked = "a\n".repeat(limit / 2);
			assert_eq!(untracked.len(), limit);
			fs::write(r.join("untracked.txt"), &untracked).unwrap();
		}

		fn assert_patch_too_large_previews(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			cx.simulate_keystrokes("alt-0");
			settle(cx);
			for (group, name) in
				[("unstaged", "tracked.txt"), ("untracked", "untracked.txt")]
			{
				model.update(cx, |m, cx| {
					m.preview_error = None;
					cx.notify();
				});
				click_probed(
					model,
					cx,
					&format!("change-row@big:{group}:{name}"),
				);
				settle_until(cx, "preview error", |cx| {
					model.read_with(cx, |m, _| m.preview_error.is_some())
				});
				let err = model
					.read_with(cx, |m, _| m.preview_error.clone())
					.expect("preview error");
				assert_eq!(err.key, "error_preview_patch_too_large", "{err:?}");
				let text = err.render(crate::i18n::Locale::ZhTw);
				assert!(text.contains(name), "{text}");
			}
		}

		#[gpui::test]
		fn local_changes_with_an_oversized_diff_say_the_diff_is_too_large(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			patch_limit_fixture(ws.path());
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
			assert_patch_too_large_previews(&model, cx);
		}

		#[gpui::test]
		fn remote_changes_with_an_oversized_diff_say_the_diff_is_too_large(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let shared = tempfile::tempdir().unwrap();
			patch_limit_fixture(shared.path());
			let (model, cx, _worker) = open_remote(
				cx,
				shared.path(),
				snip_remote::WorkerOptions::default(),
			);
			assert_patch_too_large_previews(&model, cx);
		}

		/// A connected remote workspace whose worker cannot be started again
		/// (#206): the client reconnects in the middle of a request and the
		/// restart fails, as when the worker was removed or the key revoked
		/// behind it. The restart is a real `Transport::Command` that exits
		/// 127, the way a host without snip answers, so no ssh is involved.
		#[cfg(unix)]
		fn lose_the_worker(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			lose_the_worker_at(model, cx, false);
		}

		/// [`lose_the_worker`]; with `mid_request` the client also holds a
		/// pooled connection that died, so the first request fails on it
		/// and the failed restart follows in the same call.
		#[cfg(unix)]
		fn lose_the_worker_at(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			mid_request: bool,
		) {
			model.update(cx, |m, _| {
				let session = m.remote.session.as_mut().expect("connected");
				let gone = std::sync::Arc::new(snip_remote::Client::new(
					snip_remote::RemoteHost {
						name: "gone-host".into(),
						transport: snip_remote::Transport::Command(vec![
							"sh".into(),
							"-c".into(),
							"echo 'sh: snip: not found' >&2; exit 127".into(),
						]),
					},
					"test-mac".into(),
				));
				if mid_request {
					gone.pool_a_dead_connection_for_tests();
				}
				session.client = gone.clone();
				// A paste plan keeps its own copy of the session.
				if let Some(remote) =
					m.paste.plan_mut().and_then(|p| p.remote.as_mut())
				{
					remote.session.client = gone;
				}
			});
		}

		/// The interface language's wording of a missing snip, and none of
		/// the CLI's English.
		#[cfg(unix)]
		fn assert_no_snip_in_zh(text: &str) {
			assert!(text.contains("那台機器沒有安裝 snip"), "{text}");
			assert!(!text.contains("is not installed"), "{text}");
		}

		/// A two-repository workspace on an in-process worker, loaded: the
		/// log has commits, the Project tree its folders.
		#[cfg(unix)]
		fn open_remote_to_lose(
			cx: &mut TestAppContext,
		) -> (
			tempfile::TempDir,
			Entity<WorkbenchModel>,
			&mut VisualTestContext,
			std::sync::Arc<snip_remote::Worker>,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			let beta = shared.join("beta");
			fs::create_dir_all(&alpha).unwrap();
			fs::create_dir_all(&beta).unwrap();
			crate::paste::tests::git_init(&alpha);
			fs::write(alpha.join("init.txt"), "hello\n").unwrap();
			let long: String =
				(1..=40).map(|i| format!("line {i}\n")).collect();
			fs::write(alpha.join("long.txt"), &long).unwrap();
			crate::paste::tests::git_run(&alpha, &["add", "."]);
			crate::paste::tests::git_run(&alpha, &["commit", "-m", "init"]);
			fs::write(alpha.join("dirty.txt"), "dirty\n").unwrap();
			// One edit in the middle: the diff folds the lines around it.
			fs::write(
				alpha.join("long.txt"),
				long.replace("line 20\n", "edited\n"),
			)
			.unwrap();
			crate::paste::tests::git_init(&beta);
			fs::write(beta.join("init.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&beta, &["add", "init.txt"]);
			crate::paste::tests::git_run(&beta, &["commit", "-m", "init"]);
			let notes = shared.join("notes");
			fs::create_dir_all(notes.join("sub")).unwrap();
			fs::write(notes.join("n.txt"), "N\n").unwrap();
			let (model, cx, worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			settle_until(cx, "the remote workspace to load", |cx| {
				model.read_with(cx, |m, _| {
					m.change_repos.first().is_some_and(|r| {
						r.state == crate::ChangeRepoState::Loaded
					}) && !m.commits.is_empty()
						&& m.ws_tree.as_ref().is_some_and(|t| t.is_loaded)
				})
			});
			(tmp, model, cx, worker)
		}

		/// Remote copy (Cmd+C on the Project tree's repository) when the
		/// worker cannot restart: the status says why in the interface
		/// language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_copy_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			lose_the_worker(&model, cx);
			cx.simulate_keystrokes("cmd-c");
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let status = model.read_with(cx, |m, _| m.status.render(m.locale));
			assert!(status.contains("產生 payload 失敗"), "{status}");
			assert_no_snip_in_zh(&status);
		}

		/// The first failure of a request that was in flight when the worker
		/// died (#206): the pooled connection breaks, the restart fails, and
		/// the status still says why in the interface language.
		#[cfg(unix)]
		#[gpui::test]
		fn remote_copy_losing_the_worker_mid_request_says_why_at_once(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			lose_the_worker_at(&model, cx, true);
			cx.simulate_keystrokes("cmd-c");
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let status = model.read_with(cx, |m, _| m.status.render(m.locale));
			assert!(status.contains("產生 payload 失敗"), "{status}");
			assert_no_snip_in_zh(&status);
		}

		/// Clicks the control `id` where its probe says it is drawn, the way
		/// an operator reads `[APP:CTRL_BOUNDS]`: the probe is switched on
		/// on first use and read after the next frame.
		fn click_probed(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			id: &str,
		) {
			if model.read_with(cx, |m, _| m.probes.is_none()) {
				model.update(cx, |m, cx| {
					m.probes = Some(crate::ui::Probes::for_test());
					cx.notify();
				});
				settle(cx);
			}
			let b = model
				.read_with(cx, |m, _| m.probes.as_ref().unwrap().bounds(id))
				.unwrap_or_else(|| panic!("no probed control {id}"));
			let scale = cx.update(|window, _| window.scale_factor());
			cx.simulate_click(
				gpui::point(
					gpui::px((b[0] + b[2] / 2) as f32 / scale),
					gpui::px((b[1] + b[3] / 2) as f32 / scale),
				),
				gpui::Modifiers::none(),
			);
			settle(cx);
		}

		#[cfg(unix)]
		fn status_key(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> &'static str {
			model.read_with(cx, |m, _| m.status.key)
		}

		/// The status line in the interface language.
		#[cfg(unix)]
		fn status_text(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> String {
			model.read_with(cx, |m, _| m.status.render(m.locale))
		}

		/// [`copying_non_contiguous_commits_logs_the_engines_text`] in a
		/// remote workspace: the worker's export refuses, and the log line
		/// still carries the engine's text, not the interface's.
		#[gpui::test]
		fn copying_non_contiguous_remote_commits_logs_the_engines_text(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let alpha = shared.join("alpha");
			fs::create_dir_all(&alpha).unwrap();
			crate::paste::tests::git_init(&alpha);
			for n in ["one", "two", "three"] {
				fs::write(alpha.join(format!("{n}.txt")), "x\n").unwrap();
				crate::paste::tests::git_run(&alpha, &["add", "."]);
				crate::paste::tests::git_run(
					&alpha,
					&["commit", "-qm", &format!("alpha {n}")],
				);
			}
			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			settle_until(cx, "the remote log", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 3)
			});
			let ix_of = |cx: &mut VisualTestContext, subject: &str| {
				model
					.read_with(cx, |m, _| {
						m.display_commits()
							.iter()
							.position(|c| c.subject == subject)
					})
					.expect("commit listed")
			};
			let newest = ix_of(cx, "alpha three");
			let oldest = ix_of(cx, "alpha one");
			let id = |ix: usize| -> &'static str {
				Box::leak(format!("log-subject:{ix}").into_boxed_str())
			};
			let row = cx.debug_bounds(id(newest)).expect("the newest row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			let row = cx.debug_bounds(id(oldest)).expect("the oldest row");
			cx.simulate_click(row.center(), gpui::Modifiers::secondary_key());
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.log_selected.len(), 2, "two commits are selected");
			});
			let _ = crate::test_log::take();
			cx.simulate_keystrokes("cmd-c");
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let lines = crate::test_log::take();
			assert!(
				lines
					.iter()
					.any(|l| l.starts_with("[APP:COPY_COMMITS_ERR: ")
						&& l.contains("not contiguous")),
				"{lines:#?}"
			);
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "error_payload");
				let zh = m.status.render(crate::i18n::Locale::ZhTw);
				assert!(zh.contains("產生 payload 失敗"), "{zh}");
				assert!(!zh.contains("not contiguous"), "{zh}");
			});
		}

		/// Copying the selected commit (Cmd+C in the Git Log) when the worker
		/// cannot restart (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_commit_copy_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			let row = cx.debug_bounds("log-subject:0").expect("a commit row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			settle_until(cx, "the commit to be selected", |cx| {
				model.read_with(cx, |m, _| m.selected_commit.is_some())
			});
			lose_the_worker(&model, cx);
			cx.simulate_keystrokes("cmd-c");
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let status = status_text(&model, cx);
			assert_no_snip_in_zh(&status);
		}

		/// Pasting into a remote workspace (Cmd+V) when the worker cannot
		/// restart: the destination message says why in the interface
		/// language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_paste_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			lose_the_worker(&model, cx);
			clip::write_text("// FILE: a.txt\nA\n").unwrap();
			cx.simulate_keystrokes("cmd-v");
			settle_until(cx, "the paste to fail", |cx| {
				!model.read_with(cx, |m, _| m.paste.is_loading())
			});
			let status = status_text(&model, cx);
			assert!(status.contains("無法使用貼上目的地"), "{status}");
			assert_no_snip_in_zh(&status);
		}

		/// A paste preview opened in one language outlives a switch to the
		/// other: when the worker is then lost and the mapping changes, the
		/// replan's reason is worded in the language now in force, whole,
		/// not as a wrapper in one language around the reason in the other
		/// (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_paste_replan_after_a_language_switch_uses_the_new_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			// The preview opens in English...
			cx.simulate_keystrokes("alt-l");
			assert_eq!(
				model.read_with(cx, |m, _| m.locale),
				crate::i18n::Locale::En
			);
			remote_paste_preview(
				&model,
				cx,
				"// FILE: .git/hooks/pre-commit\n#!/bin/sh\n// FILE: d.txt\nD\n",
			);
			// ...then the language is switched with the header button.
			click_probed(&model, cx, "btn-locale");
			assert_eq!(
				model.read_with(cx, |m, _| m.locale),
				crate::i18n::Locale::ZhTw
			);
			lose_the_worker(&model, cx);
			model.update(cx, |m, cx| m.choose_paste_keep(".git", cx));
			settle_until(cx, "the replan to fail", |cx| {
				model.read_with(cx, |m, _| {
					!m.paste.is_loading()
						&& m.status.key.starts_with("paste_err_destination")
				})
			});
			let status = status_text(&model, cx);
			assert!(status.contains("無法使用貼上目的地"), "{status}");
			assert_no_snip_in_zh(&status);
			// And back: the same stored message words itself in English.
			cx.simulate_keystrokes("alt-l");
			let status = status_text(&model, cx);
			assert!(
				status.contains("Cannot use paste destination"),
				"{status}"
			);
			assert!(status.contains("is not installed"), "{status}");
			assert!(!status.contains("沒有安裝"), "{status}");
		}

		/// Selecting a repository (a click on its row) when the worker cannot
		/// restart: the Changes read fails and the status says why in the
		/// interface language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_changes_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "repo-row:beta");
			settle_until(cx, "the changes error", |cx| {
				model.read_with(cx, |m, _| m.status.key == "error_repo_changes")
			});
			let status = status_text(&model, cx);
			assert!(status.contains("變更失敗"), "{status}");
			assert_no_snip_in_zh(&status);
		}

		/// Expanding a diff's folded lines (a click on the fold) when the
		/// worker cannot restart: the status says why in the interface
		/// language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_fold_expansion_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-0");
			settle(cx);
			click_probed(&model, cx, "change-repo-toggle:unstaged:alpha");
			click_probed(&model, cx, "change-row@alpha:unstaged:long.txt");
			settle_until(cx, "the diff preview", |cx| {
				model.read_with(cx, |m, _| {
					m.probes
						.as_ref()
						.is_some_and(|p| p.bounds("diff-fold:end").is_some())
				})
			});
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "diff-fold:end");
			settle_until(cx, "the fold error", |cx| {
				model.read_with(cx, |m, _| m.status.key == "status_fold_failed")
			});
			let status = status_text(&model, cx);
			assert_no_snip_in_zh(&status);
		}

		/// Previewing a Changes-pane file's diff when the worker cannot
		/// restart: the editor's error says why in the interface language
		/// (#206). This reaches `PreviewErr::git`, which also words the
		/// commit-file diff and a revision-tree file.
		#[cfg(unix)]
		#[gpui::test]
		fn remote_diff_preview_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-0");
			settle(cx);
			click_probed(&model, cx, "change-repo-toggle:unstaged:alpha");
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "change-row@alpha:unstaged:long.txt");
			settle_until(cx, "the preview error", |cx| {
				model.read_with(cx, |m, _| m.preview_error.is_some())
			});
			let err = model.read_with(cx, |m, _| {
				m.preview_error.as_ref().unwrap().render(m.locale)
			});
			assert!(err.contains("預覽"), "{err}");
			assert_no_snip_in_zh(&err);
		}

		/// Opening a Project tree file when the worker cannot restart: the
		/// editor's error says why in the interface language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_preview_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			click_probed(&model, cx, "ws-tree-row:notes");
			settle_until(cx, "the notes folder to open", |cx| {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| {
						p.bounds("ws-tree-row:notes/n.txt").is_some()
					})
				})
			});
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "ws-tree-row:notes/n.txt");
			settle_until(cx, "the preview error", |cx| {
				model.read_with(cx, |m, _| m.preview_error.is_some())
			});
			let err = model.read_with(cx, |m, _| {
				m.preview_error.as_ref().unwrap().render(m.locale)
			});
			assert!(err.contains("預覽"), "{err}");
			assert_no_snip_in_zh(&err);
		}

		/// Expanding a Project tree folder when the worker cannot restart:
		/// the row under it says why in the interface language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_tree_listing_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			click_probed(&model, cx, "ws-tree-row:notes");
			settle_until(cx, "the notes folder to open", |cx| {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| {
						p.bounds("ws-tree-row:notes/sub").is_some()
					})
				})
			});
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "ws-tree-row:notes/sub");
			settle_until(cx, "the listing error", |cx| {
				model.read_with(cx, |m, _| {
					m.ws_tree.as_ref().is_some_and(|t| {
						t.flatten_visible(300).iter().any(|r| r.is_error)
					})
				})
			});
			let err = model.read_with(cx, |m, _| {
				m.ws_tree
					.as_ref()
					.unwrap()
					.flatten_visible(300)
					.into_iter()
					.find(|r| r.is_error)
					.unwrap()
					.label(m.locale)
			});
			assert_no_snip_in_zh(&err);
			// Speed search words the error row in the current language: an
			// English word finds it after Option+L, and not before.
			let labels = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					m.tool_row_labels()
						.into_iter()
						.map(|l| l.to_lowercase())
						.collect::<Vec<_>>()
				})
			};
			assert!(
				labels(cx).iter().any(|l| l.contains("那台機器")),
				"the zh error row is searchable: {:?}",
				labels(cx)
			);
			assert!(!labels(cx).iter().any(|l| l.contains("not installed")));
			to_english(&model, cx);
			assert!(
				labels(cx).iter().any(|l| l.contains("not installed")),
				"speed search matches the English error row: {:?}",
				labels(cx)
			);
			assert!(!labels(cx).iter().any(|l| l.contains("那台機器")));
		}

		/// Refreshing a remote workspace (Cmd+R) when the worker cannot
		/// restart: the scan failure says why in the interface language
		/// (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_scan_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			// A pooled connection dies mid-request and the restart fails:
			// the known start reason, not the dead connection's I/O error,
			// is what the scan ends with.
			lose_the_worker_at(&model, cx, true);
			cx.simulate_keystrokes("cmd-r");
			settle_until(cx, "the scan to fail", |cx| {
				model.read_with(cx, |m, _| m.remote.scan_error.is_some())
			});
			// A known permanent start failure ends scan recovery at once:
			// no delayed retry is left armed (the dead connection's I/O
			// error would have kept one).
			model.read_with(cx, |m, _| {
				assert!(m.remote.scan_recovery.is_none(), "retry still armed");
			});
			let text = model.read_with(cx, |m, _| {
				m.remote.scan_error.as_ref().unwrap().render(m.locale)
			});
			assert!(text.contains("無法讀取遠端的 Git 儲存庫"), "{text}");
			assert_no_snip_in_zh(&text);
		}

		/// Refreshing the Git Log of a remote workspace when the worker
		/// cannot restart: the log's error says why in the interface
		/// language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_git_view_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			lose_the_worker(&model, cx);
			click(cx, "btn-log-refresh");
			settle_until(cx, "the log error", |cx| {
				model.read_with(cx, |m, _| m.history_error.is_some())
			});
			let err = model.read_with(cx, |m, _| {
				m.history_error.as_ref().map(|e| e.render(m.locale))
			});
			assert_no_snip_in_zh(&err.unwrap());
		}

		/// Selecting a commit of a remote workspace when the worker cannot
		/// restart: the file list's error says why in the interface language
		/// (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_commit_files_with_a_lost_worker_say_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			lose_the_worker(&model, cx);
			let row = cx.debug_bounds("log-subject:0").expect("a commit row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			settle_until(cx, "the preview error", |cx| {
				model.read_with(cx, |m, _| m.preview_error.is_some())
			});
			let err = model.read_with(cx, |m, _| {
				m.preview_error.as_ref().unwrap().render(m.locale)
			});
			assert_no_snip_in_zh(&err);
		}

		/// The English wording of a missing snip, and none of the Chinese.
		#[cfg(unix)]
		fn assert_snip_in_en(text: &str) {
			assert!(text.contains("is not installed"), "{text}");
			assert!(!text.contains("沒有安裝"), "{text}");
		}

		#[cfg(unix)]
		fn locale_now(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> crate::i18n::Locale {
			model.read_with(cx, |m, _| m.locale)
		}

		/// Switches to English with Option+L (the app starts in 繁中).
		#[cfg(unix)]
		fn to_english(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			assert_eq!(locale_now(model, cx), crate::i18n::Locale::ZhTw);
			cx.simulate_keystrokes("alt-l");
			assert_eq!(locale_now(model, cx), crate::i18n::Locale::En);
		}

		/// Switches to 繁中 with the header button, a real click.
		#[cfg(unix)]
		fn to_chinese_by_button(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			assert_eq!(locale_now(model, cx), crate::i18n::Locale::En);
			click_probed(model, cx, "btn-locale");
			assert_eq!(locale_now(model, cx), crate::i18n::Locale::ZhTw);
		}

		/// A request started in English, held in flight, with the language
		/// switched to 繁中 meanwhile: the reason it lands with is Chinese
		/// (#206). The wording used to be fixed when the request started.
		#[cfg(unix)]
		#[gpui::test]
		fn remote_copy_in_flight_across_a_language_switch_lands_in_the_new_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			let hold = Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_copy_hold = Some(hold));
			to_english(&model, cx);
			lose_the_worker(&model, cx);
			cx.simulate_keystrokes("cmd-c");
			settle(cx);
			assert!(model.read_with(cx, |m, _| m.is_copying), "held");
			to_chinese_by_button(&model, cx);
			cx.executor().advance_clock(hold);
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let status = status_text(&model, cx);
			assert!(status.contains("產生 payload 失敗"), "{status}");
			assert_no_snip_in_zh(&status);
		}

		/// [`remote_copy_in_flight_across_a_language_switch_lands_in_the_new_language`]
		/// for a copied commit.
		#[cfg(unix)]
		#[gpui::test]
		fn remote_commit_copy_in_flight_across_a_language_switch_lands_in_the_new_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			let row = cx.debug_bounds("log-subject:0").expect("a commit row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			settle_until(cx, "the commit to be selected", |cx| {
				model.read_with(cx, |m, _| m.selected_commit.is_some())
			});
			let hold = Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_copy_hold = Some(hold));
			to_english(&model, cx);
			lose_the_worker(&model, cx);
			cx.simulate_keystrokes("cmd-c");
			settle(cx);
			assert!(model.read_with(cx, |m, _| m.is_copying), "held");
			to_chinese_by_button(&model, cx);
			cx.executor().advance_clock(hold);
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let status = status_text(&model, cx);
			assert!(status.contains("產生 payload 失敗"), "{status}");
			assert_no_snip_in_zh(&status);
		}

		/// A commit's file list read in flight across a language switch
		/// (a Git view): its error lands in the new language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_commit_files_in_flight_across_a_language_switch_land_in_the_new_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			let hold = Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_log_read_delay = Some(hold));
			to_english(&model, cx);
			lose_the_worker(&model, cx);
			let row = cx.debug_bounds("log-subject:0").expect("a commit row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			settle(cx);
			assert!(
				model.read_with(cx, |m, _| m.preview_error.is_none()),
				"held"
			);
			to_chinese_by_button(&model, cx);
			cx.executor().advance_clock(hold);
			settle_until(cx, "the preview error", |cx| {
				model.read_with(cx, |m, _| m.preview_error.is_some())
			});
			let err = model.read_with(cx, |m, _| {
				m.preview_error.as_ref().unwrap().render(m.locale)
			});
			assert_no_snip_in_zh(&err);
		}

		/// A Project tree file read in flight across a language switch (a
		/// preview): its error lands in the new language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_preview_in_flight_across_a_language_switch_lands_in_the_new_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			click_probed(&model, cx, "ws-tree-row:notes");
			settle_until(cx, "the notes folder to open", |cx| {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| {
						p.bounds("ws-tree-row:notes/n.txt").is_some()
					})
				})
			});
			let hold = Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(hold));
			to_english(&model, cx);
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "ws-tree-row:notes/n.txt");
			assert!(
				model.read_with(cx, |m, _| m.preview_error.is_none()),
				"held"
			);
			to_chinese_by_button(&model, cx);
			cx.executor().advance_clock(hold);
			settle_until(cx, "the preview error", |cx| {
				model.read_with(cx, |m, _| m.preview_error.is_some())
			});
			let err = model.read_with(cx, |m, _| {
				m.preview_error.as_ref().unwrap().render(m.locale)
			});
			assert!(err.contains("預覽"), "{err}");
			assert_no_snip_in_zh(&err);
		}

		/// A remote file's preview read polls the tab's token, so the
		/// lifecycle must record it as a read a close cancels, not one it
		/// has to drain (#191). Only a local working-file read polls none.
		#[gpui::test]
		fn a_remote_file_preview_is_a_cancellable_job(cx: &mut TestAppContext) {
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			fs::create_dir_all(shared.join("notes")).unwrap();
			fs::write(shared.join("notes/n.txt"), "N\n").unwrap();
			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			settle_until(cx, "the Project tree to load", |cx| {
				model.read_with(cx, |m, _| {
					m.ws_tree.as_ref().is_some_and(|t| t.is_loaded)
				})
			});
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			click_probed(&model, cx, "ws-tree-row:notes");
			settle_until(cx, "the notes folder to open", |cx| {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| {
						p.bounds("ws-tree-row:notes/n.txt").is_some()
					})
				})
			});
			// The read finishes at once; the held timer keeps the job live.
			let hold = Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(hold));
			click_probed(&model, cx, "ws-tree-row:notes/n.txt");
			let (kinds, with_token) = model.read_with(cx, |m, _| {
				(
					m.lifecycle.live_kinds(),
					m.lifecycle.live_jobs_with_cancel(),
				)
			});
			assert!(
				!kinds.contains(&crate::lifecycle::JobKind::UncancellableRead),
				"a remote preview read polls a token, yet it is recorded as one the lifecycle must drain: {kinds:?}"
			);
			assert!(with_token >= 1, "a close cancels no live job: {kinds:?}");
			cx.executor().advance_clock(hold);
			settle_until(cx, "the preview to end", |cx| {
				model.read_with(cx, |m, _| !m.preview_loading)
			});
		}

		/// A scan failure shown in 繁中 follows a switch to English: the
		/// banner and the status line word the reason again (#208).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_scan_failure_shown_follows_a_language_switch(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			lose_the_worker_at(&model, cx, true);
			cx.simulate_keystrokes("cmd-r");
			settle_until(cx, "the scan to fail", |cx| {
				model.read_with(cx, |m, _| m.remote.scan_error.is_some())
			});
			let banner = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					m.remote.scan_error.as_ref().unwrap().render(m.locale)
				})
			};
			let zh = banner(cx);
			assert!(zh.contains("無法讀取遠端的 Git 儲存庫"), "{zh}");
			assert_no_snip_in_zh(&zh);
			to_english(&model, cx);
			let en = banner(cx);
			assert!(
				en.contains("Failed to read remote Git repositories"),
				"{en}"
			);
			assert_snip_in_en(&en);
			let status = status_text(&model, cx);
			assert_snip_in_en(&status);
			// And back with the button.
			to_chinese_by_button(&model, cx);
			assert_no_snip_in_zh(&banner(cx));
			assert_no_snip_in_zh(&status_text(&model, cx));
		}

		/// A log read that failed shows its reason in the log's banner and
		/// the status line; a language switch rewords both (#208).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_log_error_shown_follows_a_language_switch(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			lose_the_worker(&model, cx);
			click(cx, "btn-log-refresh");
			settle_until(cx, "the log error", |cx| {
				model.read_with(cx, |m, _| m.history_error.is_some())
			});
			// What the banner and the status bar drew, not a re-rendering
			// of the model's fields.
			let banner = |_: &mut VisualTestContext| {
				crate::ui::last_drawn("log-error").expect("the banner drew")
			};
			let bar = |_: &mut VisualTestContext| {
				crate::ui::last_drawn("status-text").expect("the bar drew")
			};
			assert_no_snip_in_zh(&banner(cx));
			assert_no_snip_in_zh(&bar(cx));
			to_english(&model, cx);
			assert_snip_in_en(&banner(cx));
			assert_snip_in_en(&bar(cx));
		}

		/// The row a failed Project tree listing leaves follows a switch of
		/// language too (#208).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_tree_error_row_shown_follows_a_language_switch(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			click_probed(&model, cx, "ws-tree-row:notes");
			settle_until(cx, "the notes folder to open", |cx| {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| {
						p.bounds("ws-tree-row:notes/sub").is_some()
					})
				})
			});
			lose_the_worker(&model, cx);
			click_probed(&model, cx, "ws-tree-row:notes/sub");
			// The text the tree row drew, not the row's own fields.
			let row =
				|_: &mut VisualTestContext| crate::ui::last_drawn("tree-error");
			settle_until(cx, "the listing error", |cx| row(cx).is_some());
			assert_no_snip_in_zh(&row(cx).unwrap());
			to_english(&model, cx);
			assert_snip_in_en(&row(cx).unwrap());
		}

		/// A remote open that failed says so on the status bar and in the
		/// menu's note; a language switch rewords both (#208).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_open_failure_shown_follows_a_language_switch(
			cx: &mut TestAppContext,
		) {
			use crate::remote::RecentRemote;
			let _serial = remote_lock();
			let tmp = tempfile::tempdir().unwrap();
			let id = dunce::canonicalize(tmp.path())
				.unwrap()
				.display()
				.to_string();
			let (model, cx) = remote_menu(cx);
			model.update(cx, |m, cx| {
				m.restore_remote(
					RecentRemote {
						host: "gone-host".into(),
						path: id,
					},
					cx,
				);
			});
			settle(cx);
			assert!(status_text(&model, cx).contains("gone-host"));
			assert!(status_text(&model, cx).contains("無法開啟遠端工作區"));
			to_english(&model, cx);
			let status = status_text(&model, cx);
			assert!(
				status.contains("Cannot open the remote workspace"),
				"{status}"
			);
			assert!(!status.contains("已經沒有主機"), "{status}");
			let note = model.read_with(cx, |m, _| {
				m.remote.message.as_ref().map(|(_, n)| n.render(m.locale))
			});
			let note = note.expect("a note");
			assert!(!note.contains("已經沒有主機"), "{note}");
			assert!(note.contains("no longer names host"), "{note}");
		}

		/// A pasted commit's file row whose diff cannot be read, shown in
		/// 繁中, words its reason in English after a switch (#208).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_paste_diff_failure_shown_follows_a_language_switch(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			paste_a_commit(&model, cx);
			lose_the_worker(&model, cx);
			click(cx, "paste-row:0:init.txt");
			wait_paste_detail(&model, cx);
			let notice = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					m.paste
						.detail()
						.and_then(|d| {
							d.notice.as_ref().map(|n| n.render(m.locale))
						})
						.expect("a notice")
				})
			};
			let zh = notice(cx);
			assert!(zh.contains("無法顯示差異"), "{zh}");
			assert_no_snip_in_zh(&zh);
			to_english(&model, cx);
			let en = notice(cx);
			assert!(en.contains("Diff unavailable"), "{en}");
			assert_snip_in_en(&en);
		}

		/// The same read in flight across a language switch (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_paste_diff_in_flight_across_a_language_switch_lands_in_the_new_language(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			paste_a_commit(&model, cx);
			lose_the_worker(&model, cx);
			let hold = Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_paste_diff_hold = Some(hold));
			to_english(&model, cx);
			click(cx, "paste-row:0:init.txt");
			settle(cx);
			assert!(
				model.read_with(cx, |m, _| {
					m.paste.detail().is_none_or(|d| d.notice.is_none())
				}),
				"held"
			);
			to_chinese_by_button(&model, cx);
			cx.executor().advance_clock(hold);
			wait_paste_detail(&model, cx);
			let notice = model.read_with(cx, |m, _| {
				m.paste
					.detail()
					.and_then(|d| d.notice.as_ref().map(|n| n.render(m.locale)))
					.expect("a notice")
			});
			assert!(notice.contains("無法顯示差異"), "{notice}");
			assert_no_snip_in_zh(&notice);
		}

		/// Pastes a one-file commit into the open remote workspace and waits
		/// for its plan.
		#[cfg(unix)]
		fn paste_a_commit(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "C1".into(),
						author_name: "Ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-10-13T12:00:00+00:00".into(),
						files: vec![CommitFile {
							path: "init.txt".into(),
							old_path: None,
							change: FileChange::Modified,
							content: Some("c1\n".into()),
							not_copied: None,
						}],
					}],
				});
			clip::write_text(&payload).unwrap();
			cx.simulate_keystrokes("cmd-v");
			settle_until(cx, "the commit paste to plan", |cx| {
				model.read_with(cx, |m, _| {
					m.paste.plan().is_some() && !m.paste.is_loading()
				})
			});
		}

		/// Six repositories with three feature branches of long names each,
		/// so the graph is wide and rows carry ref labels; shown in a
		/// `w` x 752 window and resized from the default one, with no input
		/// after the resize. What breaks the one policy on its rows.
		fn wide_log_violations(cx: &mut TestAppContext, w: f32) -> Vec<String> {
			let ws = tempfile::tempdir().unwrap();
			for i in 0..6 {
				let r = repo(ws.path(), &format!("repo{i}"), &[]);
				for b in 0..3 {
					let name = format!("feature/a-rather-long-branch-{i}-{b}");
					git(&r, &["checkout", "-q", "-b", &name, "main"]);
					fs::write(r.join(format!("f{b}.txt")), "x").unwrap();
					git(&r, &["add", "."]);
					git(
						&r,
						&["commit", "-q", "-m", &format!("commit {i} {b}")],
					);
				}
				git(&r, &["checkout", "-q", "main"]);
			}
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			enable_probes(&model, cx);
			cx.simulate_resize(gpui::size(gpui::px(w), gpui::px(752.)));
			settle(cx);
			let n = model.read_with(cx, |m, _| m.display_commits().len());
			assert!(n > 10, "log did not load: {n} commits");
			let mut bad = Vec::new();
			let mut gutters = Vec::new();
			let mut drawn = 0;
			for ix in 0..n {
				if bounds_of(cx, format!("log-row:{ix}")).is_none() {
					continue;
				}
				drawn += 1;
				bad.extend(
					row_policy_violations(&model, cx, ix, 0.)
						.into_iter()
						.map(|v| format!("{w}: row {ix}: {v}")),
				);
				if let Some(g) = seen_cell(&model, cx, ix, RowCell::Gutter) {
					gutters.push(g.w);
				}
				// From a 1080 window on, a row with refs has its labels.
				let has_refs = model.read_with(cx, |m, _| {
					m.graph_layout
						.as_ref()
						.is_some_and(|l| !l.rows[ix].refs.is_empty())
				});
				if w >= 1080.
					&& has_refs && seen_cell(&model, cx, ix, RowCell::Label(0))
					.is_none()
				{
					bad.push(format!("{w}: row {ix} lost its labels"));
				}
			}
			assert!(drawn > 0, "no rows were laid out");
			if gutters.iter().any(|g| (*g - gutters[0]).abs() > 0.01) {
				bad.push(format!("{w}: the gutters differ: {gutters:?}"));
			}
			bad
		}

		/// At 1080 the list is ~574px: every row keeps its subject's floor
		/// and a whole date, and a row with refs has its labels.
		#[gpui::test]
		fn wide_multi_repo_log_keeps_a_subject_at_1080(
			cx: &mut TestAppContext,
		) {
			let bad = wide_log_violations(cx, 1080.);
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// At 900 the list is ~430px: the gutter sits at its floor, the
		/// date stays whole and the author column or the labels give way.
		#[gpui::test]
		fn wide_multi_repo_log_keeps_a_subject_at_900(cx: &mut TestAppContext) {
			let bad = wide_log_violations(cx, 900.);
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// `git` with both dates fixed, so one repository's commits sort
		/// before another's in the merged log.
		fn git_dated(dir: &Path, args: &[&str], date: &str) {
			let out = Command::new("git")
				.current_dir(dir)
				.env("GIT_AUTHOR_DATE", date)
				.env("GIT_COMMITTER_DATE", date)
				.args(["-c", "user.name=t", "-c", "user.email=t@t"])
				.args(args)
				.output()
				.expect("git must run");
			assert!(out.status.success(), "git {args:?}: {out:?}");
		}

		/// Runs `ready` after each settle, at most 50 times, then fails.
		fn settle_until(
			cx: &mut VisualTestContext,
			what: &str,
			mut ready: impl FnMut(&mut VisualTestContext) -> bool,
		) {
			for _ in 0..50 {
				settle(cx);
				if ready(cx) {
					return;
				}
			}
			panic!("timed out waiting for {what}");
		}

		/// A HEAD commit with a tag on it: its branch label and its tag
		/// label sit right after the subject, and the tag is a label of its
		/// own (#188).
		#[gpui::test]
		fn ref_labels_sit_next_to_the_subject_and_tags_are_their_own_label(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "solo", &[]);
			fs::write(r.join("short.txt"), "s").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "short"]);
			git(&r, &["tag", "v0.1"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1600., 800.);
			settle_until(cx, "the log rows", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 2)
			});
			settle(cx);
			let subject = cx.debug_bounds("log-subject:0").expect("subject");
			let branch = cx.debug_bounds("log-ref-label:0:0").expect("branch");
			assert!(
				cx.debug_bounds("log-ref-label:0:1").is_some(),
				"the tag is not a label of its own"
			);
			let gap = f32::from(branch.left()) - f32::from(subject.right());
			assert!((0. ..=16.).contains(&gap), "label gap {gap}");
		}

		/// A HEAD with a branch and its remote, and a tag (#195). The branch
		/// and remote merge into `main, origin/main`.
		fn head_with_labels(dir: &Path) {
			let r = repo(dir, "solo", &[]);
			fs::write(r.join("long.txt"), "l").unwrap();
			git(&r, &["add", "."]);
			git(
				&r,
				&[
					"commit",
					"-q",
					"-m",
					"a subject long enough to want the whole row for itself and then some more words",
				],
			);
			git(&r, &["tag", "v0.3.0"]);
			git(&r, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
			git(
				&r,
				&[
					"symbolic-ref",
					"refs/remotes/origin/HEAD",
					"refs/remotes/origin/main",
				],
			);
		}

		/// The labels the HEAD row shows, by their text.
		fn head_label_texts(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> Vec<String> {
			model.read_with(cx, |m, _| {
				let row = &m.graph_layout.as_ref().unwrap().rows[0];
				let cur = m.log_current_branch(&m.display_commits()[0].sha);
				let (shown, _) =
					crate::graph_view::visible_refs(&row.refs, cur.as_deref());
				crate::graph_view::kind_labels(&shown, cur.as_deref())
					.into_iter()
					.map(|l| l.text)
					.collect()
			})
		}

		/// On a 1160 window the list is 654px: both labels fit whole beside
		/// the subject and the columns.
		#[gpui::test]
		fn head_row_labels_stay_whole_at_1160(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			let (model, cx) = open_log_window(cx, ws.path(), 1160., false);
			let texts = head_label_texts(&model, cx);
			assert_eq!(texts, ["main, origin/main", "v0.3.0"]);
			for (n, text) in texts.iter().enumerate() {
				let label =
					seen_cell(&model, cx, 0, RowCell::Label(n)).expect("label");
				assert!(
					label.visible >= label.natural - 1.0,
					"label {n} {text:?} cut: {label:?}"
				);
			}
			let subject =
				seen_cell(&model, cx, 0, RowCell::Subject).expect("subject");
			assert!(subject.visible >= subject.floor - 1., "{subject:?}");
		}

		/// On a 1080 window the list is 574px. The labels keep their
		/// rule's floor (the label kind: 6 characters), the subject keeps
		/// its own, and the HEAD row has its labels (#195).
		#[gpui::test]
		fn head_row_labels_stay_readable_at_1080(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			for hash in [false, true] {
				let (model, cx) = open_log_window(cx, ws.path(), 1080., hash);
				assert_eq!(
					head_label_texts(&model, cx),
					["main, origin/main", "v0.3.0"]
				);
				let bad = row_policy_violations(&model, cx, 0, 1080.);
				assert!(bad.is_empty(), "hash={hash}: {bad:?}");
				for n in 0..2 {
					let label = seen_cell(&model, cx, 0, RowCell::Label(n))
						.expect("label");
					assert!(label.rule_holds(), "hash={hash}: {label:?}");
					assert!(label.visible >= label.floor - 1., "{label:?}");
				}
			}
		}

		// ───────── #228: one fit rule for the log's texts (S0 helpers) ─────────

		/// Probes on, so `click_probed` and the probe readers have bounds.
		fn enable_probes(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			model.update(cx, |m, cx| {
				m.probes = Some(crate::ui::Probes::for_test());
				cx.notify();
			});
			settle(cx);
		}

		/// Like `open_sized`, but measuring with the OS text system and the
		/// embedded fonts, as the app does (`use_real_text_system` alone
		/// leaves "Inter" to fall back to a system font, 7% off). `None`
		/// where the platform has no real text system.
		///
		/// The text system is swapped once per `TestAppContext`. A `FontId`
		/// is an index into the text system that resolved it: a window
		/// shapes with the one it was opened under, but gpui paints with
		/// the app's current one (`paint_line_background` reads
		/// `cx.text_system()`), so a second swap with an older window still
		/// open painted ids of one cosmic-text instance against another and
		/// panicked out of bounds on Linux. For the same reason the windows
		/// opened under the Noop system before the swap are closed.
		fn open_real_text(
			cx: &mut TestAppContext,
			ws: PathBuf,
			w: f32,
			h: f32,
		) -> Option<(Entity<WorkbenchModel>, &mut VisualTestContext)> {
			let swapped = cx
				.text_system()
				.all_font_names()
				.iter()
				.any(|n| n == crate::theme::UI_FONT);
			if !swapped {
				cx.update(|cx| {
					for window in cx.windows() {
						let _ = window
							.update(cx, |_, window, _| window.remove_window());
					}
				});
				cx.run_until_parked();
				if !cx.use_real_text_system() {
					assert!(
						std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none()
							|| cfg!(windows),
						"no real text system on this OS"
					);
					return None;
				}
				cx.update(|cx| crate::theme::register_fonts(cx));
			}
			let names = cx.text_system().all_font_names();
			for family in [crate::theme::UI_FONT, crate::theme::CODE_FONT] {
				assert!(
					names.iter().any(|n| n == family),
					"the embedded font {family} does not resolve"
				);
			}
			let (model, cx) = open_sized(cx, ws, None, w, h);
			enable_probes(&model, cx);
			// The measuring really is the real one: CJK is about twice as
			// wide as the 0.6em NoopTextSystem reports.
			const TAG: &str = "\u{767c}\u{884c}\u{7248}\u{672c}-\u{4e2d}\u{6587}\u{6a19}\u{7c64}";
			let size = crate::theme::SMALL_TEXT;
			let real = cx.update(|w, _| crate::ui::text_width(w, TAG, size));
			let noop = TAG.chars().count() as f32 * 0.6 * size;
			assert!(real > noop * 1.3, "real {real} vs noop {noop}");
			Some((model, cx))
		}

		/// Like `head_with_labels`, but the HEAD commit has a long-format
		/// date ("2025/12/21 15:30", 16 characters).
		fn head_with_labels_dated(dir: &Path) {
			let r = repo(dir, "solo", &[]);
			fs::write(r.join("long.txt"), "l").unwrap();
			git(&r, &["add", "."]);
			git_dated(
				&r,
				&[
					"commit",
					"-q",
					"-m",
					"a subject long enough to want the whole row for itself and then some more words",
				],
				"2025-12-21T15:30:00Z",
			);
			git(&r, &["tag", "v0.3.0"]);
			git(&r, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
			git(
				&r,
				&[
					"symbolic-ref",
					"refs/remotes/origin/HEAD",
					"refs/remotes/origin/main",
				],
			);
		}

		/// Width of `s` in the UI font (`code`: the code font) at `size`.
		fn text_w(
			cx: &mut VisualTestContext,
			s: &str,
			code: bool,
			size: f32,
		) -> f32 {
			let s = s.to_string();
			cx.update(|w, _| {
				let family = if code {
					crate::theme::CODE_FONT
				} else {
					crate::theme::UI_FONT
				};
				let run = gpui::TextRun {
					len: s.len(),
					font: gpui::font(family),
					color: gpui::black(),
					background_color: None,
					underline: None,
					strikethrough: None,
				};
				f32::from(
					w.text_system()
						.shape_line(
							s.clone().into(),
							gpui::px(size),
							&[run],
							None,
						)
						.width,
				)
			})
		}

		/// The cells of a log row.
		#[derive(Clone, Copy, Debug, PartialEq, Eq)]
		enum RowCell {
			Gutter,
			Subject,
			Collapsed,
			Label(usize),
			Author,
			Date,
			Hash,
		}

		/// One drawn cell of a row: what `TEXT_FIT` (the graph gutter, which
		/// is no text: its `debug_bounds`) saw of it.
		#[derive(Clone, Debug)]
		struct SeenCell {
			cell: RowCell,
			role: Option<crate::ui::FitRole>,
			text: String,
			/// Where the box starts, and how wide it was drawn.
			x: f32,
			w: f32,
			/// How much of the box no ancestor clips away.
			visible: f32,
			/// What the text needs whole, and what its rule keeps at least.
			natural: f32,
			floor: f32,
			/// The width the solver gave.
			fit: Option<f32>,
			shown: usize,
			total: usize,
			key_graphemes: usize,
		}

		impl SeenCell {
			/// What is in sight: from the box's left to the visible width.
			fn vis_l(&self) -> f32 {
				self.x
			}

			fn vis_r(&self) -> f32 {
				self.x + self.visible
			}

			fn rule_holds(&self) -> bool {
				match self.role {
					Some(role) => crate::ui::rule_holds(
						role.rule(),
						self.shown,
						self.total,
						self.key_graphemes,
					),
					None => true,
				}
			}
		}

		fn bounds_of(
			cx: &mut VisualTestContext,
			selector: String,
		) -> Option<gpui::Bounds<gpui::Pixels>> {
			let s: &'static str = Box::leak(selector.into_boxed_str());
			cx.debug_bounds(s)
		}

		/// The `TEXT_FIT` of the text of row `ix` in `role`.
		fn row_text_fit(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			ix: usize,
			role: crate::ui::FitRole,
		) -> Option<crate::ui::TextFit> {
			model.read_with(cx, |m, _| {
				let key = m.log_row_key(m.display_commits()[ix]);
				m.probes
					.as_ref()
					.expect("probes are on")
					.text_fit(&format!("text-fit:row:{key}:{}", role.key()))
			})
		}

		/// Where row `ix` drew `cell`, `None` where it drew none (a dropped
		/// text has a `TEXT_FIT` of width 0, but is not drawn).
		fn seen_cell(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			ix: usize,
			cell: RowCell,
		) -> Option<SeenCell> {
			use crate::ui::FitRole;
			let role = match cell {
				RowCell::Gutter => {
					let b = bounds_of(cx, format!("log-gutter:{ix}"))?;
					return Some(SeenCell {
						cell,
						role: None,
						text: String::new(),
						x: f32::from(b.left()),
						w: f32::from(b.size.width),
						visible: f32::from(b.size.width),
						natural: f32::from(b.size.width),
						floor: f32::from(b.size.width),
						fit: Some(f32::from(b.size.width)),
						shown: 0,
						total: 0,
						key_graphemes: 0,
					});
				}
				RowCell::Subject => FitRole::Subject,
				RowCell::Collapsed => FitRole::Collapsed,
				RowCell::Author => FitRole::Author,
				RowCell::Date => FitRole::Date,
				RowCell::Hash => FitRole::Hash,
				// The first label is the branches' if the row has one.
				RowCell::Label(n) => {
					let mut present = Vec::new();
					for r in [FitRole::BranchLabel, FitRole::TagLabel] {
						if row_text_fit(model, cx, ix, r)
							.is_some_and(|f| f.fit != Some(0.))
						{
							present.push(r);
						}
					}
					*present.get(n)?
				}
			};
			let f = row_text_fit(model, cx, ix, role)?;
			if f.fit == Some(0.) {
				return None;
			}
			Some(SeenCell {
				cell,
				role: Some(role),
				text: f.text.to_string(),
				x: f.x,
				w: f.drawn,
				visible: f.visible,
				natural: f.natural,
				floor: f.floor,
				fit: f.fit,
				shown: f.shown,
				total: f.total,
				key_graphemes: f.key_graphemes,
			})
		}

		/// The row's cells that were drawn, in row order.
		fn row_cells(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			ix: usize,
		) -> Vec<SeenCell> {
			let mut out = Vec::new();
			for cell in [
				RowCell::Gutter,
				RowCell::Subject,
				RowCell::Collapsed,
				RowCell::Label(0),
				RowCell::Label(1),
				RowCell::Author,
				RowCell::Date,
				RowCell::Hash,
			] {
				if let Some(s) = seen_cell(model, cx, ix, cell) {
					out.push(s);
				}
			}
			out
		}

		/// A row's cells as (cell, drawn?, x, w), to compare two windows.
		type RowShape = Vec<(RowCell, bool, f32, f32)>;

		/// (cell, drawn?, x, w) of row `ix`, to compare two windows.
		fn log_row_shape(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			ix: usize,
		) -> RowShape {
			[
				RowCell::Gutter,
				RowCell::Subject,
				RowCell::Label(0),
				RowCell::Label(1),
				RowCell::Author,
				RowCell::Date,
				RowCell::Hash,
			]
			.into_iter()
			.map(|cell| match seen_cell(model, cx, ix, cell) {
				Some(s) => (cell, true, s.x, s.w),
				None => (cell, false, 0., 0.),
			})
			.collect()
		}

		/// What breaks the one fit policy on row `ix`, as text. Empty when
		/// the row is right.
		fn row_policy_violations(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			ix: usize,
			window_w: f32,
		) -> Vec<String> {
			let mut bad = Vec::new();
			let cells = row_cells(model, cx, ix);
			let Some(row) = bounds_of(cx, format!("log-row:{ix}")) else {
				return vec![format!("row {ix}: not drawn")];
			};
			let (row_l, row_r) =
				(f32::from(row.left()), f32::from(row.right()));
			// A1: what is in sight never overlaps, and stays in the row.
			for (i, a) in cells.iter().enumerate() {
				if a.visible <= 0.5 {
					continue;
				}
				if a.vis_l() < row_l - 0.5 || a.vis_r() > row_r + 0.5 {
					bad.push(format!("A1 {:?} leaves the row: {a:?}", a.cell));
				}
				for b in &cells[i + 1..] {
					if b.visible <= 0.5 {
						continue;
					}
					if a.vis_l() < b.vis_r() - 0.5
						&& b.vis_l() < a.vis_r() - 0.5
					{
						bad.push(format!(
							"A1 {:?} [{:.1},{:.1}] overlaps {:?} [{:.1},{:.1}]",
							a.cell,
							a.vis_l(),
							a.vis_r(),
							b.cell,
							b.vis_l(),
							b.vis_r()
						));
					}
				}
			}
			let cell = |c: RowCell| cells.iter().find(|s| s.cell == c);
			// A2: the subject keeps its readable floor.
			if let Some(s) = cell(RowCell::Subject) {
				if s.visible < s.floor - 1. {
					bad.push(format!(
						"A2 subject {:.1} < floor {:.1}: {s:?}",
						s.visible, s.floor
					));
				}
			}
			// A3: the shorter label is not squeezed below the longer one.
			if let (Some(a), Some(b)) =
				(cell(RowCell::Label(0)), cell(RowCell::Label(1)))
			{
				let (short, long) = if a.natural <= b.natural {
					(a, b)
				} else {
					(b, a)
				};
				let want = short.natural.min(long.visible) - 1.;
				if short.visible < want {
					bad.push(format!(
						"A3 label {:?} {:.1} < min({:.1}, other {:.1}) - 1",
						short.text, short.visible, short.natural, long.visible
					));
				}
			}
			// A4: no drift from the solver, and no ancestor clips a text.
			for s in &cells {
				if s.role.is_none() {
					continue;
				}
				if s.fit.is_some_and(|fit| (s.w - fit).abs() > 1.) {
					bad.push(format!("A4 {:?} drifts: {s:?}", s.cell));
				}
				if s.visible < s.w - 0.5 {
					bad.push(format!("A4 {:?} is clipped: {s:?}", s.cell));
				}
			}
			// A5: a date is whole, or absent.
			if let Some(d) = cell(RowCell::Date) {
				if d.visible < d.natural - 1. {
					bad.push(format!(
						"A5 date {:?} {:.1} < {:.1} - 1",
						d.text, d.visible, d.natural
					));
				}
			}
			// A6: from a 1080 window on, the HEAD row has its labels.
			if ix == 0
				&& window_w >= 1080.
				&& (cell(RowCell::Label(0)).is_none()
					|| cell(RowCell::Label(1)).is_none())
			{
				bad.push("A6 the HEAD row lost its labels".into());
			}
			bad
		}

		/// Opens `w`x720 on a fresh window with the hash column `hash`.
		fn open_log_window<'a>(
			cx: &'a mut TestAppContext,
			ws: &Path,
			w: f32,
			hash: bool,
		) -> (Entity<WorkbenchModel>, &'a mut VisualTestContext) {
			open_log_window_in(cx, ws, w, hash, false)
				.expect("the Noop text system is always there")
		}

		/// Like `open_log_window`; with `real` the OS text system measures
		/// (`None` where the platform has none).
		fn open_log_window_in<'a>(
			cx: &'a mut TestAppContext,
			ws: &Path,
			w: f32,
			hash: bool,
			real: bool,
		) -> Option<(Entity<WorkbenchModel>, &'a mut VisualTestContext)> {
			let (model, cx) = if real {
				open_real_text(cx, ws.to_path_buf(), w, 720.)?
			} else {
				let (model, cx) =
					open_sized(cx, ws.to_path_buf(), None, w, 720.);
				(model, cx)
			};
			settle_until(cx, "the log rows", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 2)
			});
			settle(cx);
			enable_probes(&model, cx);
			if hash {
				click_probed(&model, cx, "btn-log-more");
				click_probed(&model, cx, "log-more:hash");
			}
			assert_eq!(model.read_with(cx, |m, _| m.log_show_hash), hash);
			settle(cx);
			Some((model, cx))
		}

		/// A1, A2, A3, A5, A6 on the HEAD row at every window width and
		/// with the hash column on and off. A6, from 1080 on: the HEAD row
		/// has its labels.
		#[gpui::test]
		fn log_row_cells_follow_one_policy_at_every_width(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels_dated(ws.path());
			let mut bad = Vec::new();
			for w in [900., 1000., 1040., 1080., 1120., 1160., 1400.] {
				for hash in [false, true] {
					let (model, cx) = open_log_window(cx, ws.path(), w, hash);
					for v in row_policy_violations(&model, cx, 0, w) {
						bad.push(format!("{w} hash={hash}: {v}"));
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// A merge of a three-commit side branch at HEAD, tagged and on
		/// `origin/main`: collapsed, its row has the chip and two labels.
		fn merge_with_labels(dir: &Path) {
			let r = repo(dir, "solo", &[]);
			git(&r, &["checkout", "-q", "-b", "side"]);
			for i in 0..3 {
				fs::write(r.join(format!("s{i}.txt")), "s").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", &format!("side {i}")]);
			}
			git(&r, &["checkout", "-q", "main"]);
			fs::write(r.join("m.txt"), "m").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "main work"]);
			git(
				&r,
				&[
					"merge",
					"-q",
					"--no-ff",
					"side",
					"-m",
					"a merge subject long enough to want the whole row for itself and more",
				],
			);
			git(&r, &["tag", "v0.3.0"]);
			git(&r, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
		}

		/// A merge row collapsed to "N hidden": the subject keeps a readable
		/// floor and the chip is whole (or, with the hash column on, gone:
		/// the labels come before it).
		#[gpui::test]
		fn a_collapsed_merge_row_keeps_a_readable_subject(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			merge_with_labels(ws.path());
			let mut bad = Vec::new();
			for w in [1000., 1080., 1160.] {
				for hash in [false, true] {
					let (model, cx) = open_log_window(cx, ws.path(), w, hash);
					let sha7 = model.read_with(cx, |m, _| {
						m.display_commits()[0].sha[..7].to_string()
					});
					click_probed(&model, cx, &format!("collapse:{sha7}"));
					assert_eq!(
						model.read_with(cx, |m, _| m.collapsed_merges.len()),
						1,
						"the merge did not collapse"
					);
					let collapsed =
						seen_cell(&model, cx, 0, RowCell::Collapsed);
					match collapsed {
						// The chip is only an explanation: with the hash
						// column on it gives way to the labels.
						None if hash => {}
						None => bad.push(format!(
							"{w} hash={hash}: no collapsed chip"
						)),
						Some(c) if c.visible < c.natural - 1. => {
							bad.push(format!("{w} hash={hash}: chip cut {c:?}"))
						}
						Some(_) => {}
					}
					for v in row_policy_violations(&model, cx, 0, w) {
						bad.push(format!("{w} hash={hash}: {v}"));
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// A collapsed merge row from a 1080 window on: the labels are always
		/// there (the user's rule), the explanation chip is only drawn whole
		/// or not at all, and it is what gives way when both do not fit
		/// (ADR 0005). English and Chinese chips, Noop and real text widths.
		#[gpui::test]
		fn a_collapsed_merge_row_keeps_its_labels_over_the_chip(
			cx: &mut TestAppContext,
		) {
			use crate::i18n::Locale;
			let ws = tempfile::tempdir().unwrap();
			merge_with_labels(ws.path());
			let mut bad = Vec::new();
			let mut chip_dropped = 0;
			for real in [false, true] {
				for loc in [Locale::ZhTw, Locale::En] {
					for w in [1080., 1120., 1160.] {
						for hash in [false, true] {
							let Some((model, cx)) = open_log_window_in(
								cx,
								ws.path(),
								w,
								hash,
								real,
							) else {
								continue;
							};
							model.update(cx, |m, cx| {
								if m.locale != loc {
									m.toggle_locale(cx);
								}
							});
							let sha7 = model.read_with(cx, |m, _| {
								m.display_commits()[0].sha[..7].to_string()
							});
							click_probed(
								&model,
								cx,
								&format!("collapse:{sha7}"),
							);
							settle(cx);
							assert_eq!(
								model.read_with(cx, |m, _| m
									.collapsed_merges
									.len()),
								1,
								"the merge did not collapse"
							);
							let t =
								format!("{w} hash={hash} real={real} {loc:?}");
							match seen_cell(&model, cx, 0, RowCell::Collapsed) {
								None => chip_dropped += 1,
								Some(c) if c.visible < c.natural - 1. => {
									bad.push(format!("{t}: chip cut {c:?}"))
								}
								Some(_) => {}
							}
							// A6: both labels of the HEAD row are there.
							for v in row_policy_violations(&model, cx, 0, w) {
								bad.push(format!("{t}: {v}"));
							}
						}
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
			assert!(
				chip_dropped > 0,
				"no cell made the chip give way, so nothing was tested"
			);
		}

		/// On a list too narrow for the subject (it gets no width at all),
		/// the subject's box takes no margin: the solver did not pay the
		/// gap, so a margin would push the fixed columns after it out of the
		/// row. The box starts where the gutter ends.
		#[gpui::test]
		fn a_subject_without_room_leaves_no_margin(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			let mut unplaced = 0;
			let mut bad = Vec::new();
			for w in (440..=520).step_by(16).map(|w| w as f32) {
				for hash in [false, true] {
					let (model, cx) = open_log_window(cx, ws.path(), w, hash);
					let Some(f) = row_text_fit(
						&model,
						cx,
						0,
						crate::ui::FitRole::Subject,
					) else {
						continue;
					};
					if f.fit != Some(0.) {
						continue;
					}
					unplaced += 1;
					let gutter = seen_cell(&model, cx, 0, RowCell::Gutter)
						.expect("the gutter is drawn");
					if (f.x - (gutter.x + gutter.w)).abs() > 0.5 {
						bad.push(format!(
							"{w} hash={hash}: the subject starts at {} but the gutter ends at {}",
							f.x,
							gutter.x + gutter.w
						));
					}
				}
			}
			assert!(unplaced > 0, "no width left the subject without room");
			assert!(bad.is_empty(), "{}", bad.join("\n"));
		}

		/// A window resized where the filter bar's chips do not move: their
		/// container is held at the width of the "more" chip, so the bar's own
		/// canvas sees the same width and asks for no redraw. The list and
		/// the log panel have their own canvases for exactly this: without
		/// them the rows would keep the old width's columns until some
		/// input.
		#[gpui::test]
		fn log_rows_follow_a_resize_the_filter_bar_does_not_see(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			for (from, to) in [(640., 740.), (760., 640.)] {
				for hash in [false, true] {
					let (model, rcx) =
						open_log_window(cx, ws.path(), from, hash);
					let chips = |m: &WorkbenchModel| {
						(
							m.log_filter_layout.container_w.get(),
							m.log_filter_layout.last_mask.get(),
						)
					};
					let before = model.read_with(rcx, |m, _| chips(m));
					rcx.simulate_resize(gpui::size(
						gpui::px(to),
						gpui::px(720.),
					));
					settle(rcx);
					let after = model.read_with(rcx, |m, _| chips(m));
					assert_eq!(
						before, after,
						"the filter bar's chips moved: this resize is one it would have redrawn for"
					);
					let shape = log_row_shape(&model, rcx, 0);
					let (fresh_model, fcx) =
						open_log_window(cx, ws.path(), to, hash);
					let fresh = log_row_shape(&fresh_model, fcx, 0);
					assert_same_shape(
						&format!("{from} -> {to} hash={hash}"),
						&shape,
						&fresh,
					);
				}
			}
		}

		/// The `log_row_shape` of the HEAD row after `from` -> `to` resize with no
		/// input, the filter bar's mask before and after and its refresh count.
		fn row_shape_after_resize(
			cx: &mut TestAppContext,
			ws: &Path,
			hash: bool,
			from: f32,
			to: f32,
		) -> (RowShape, [u8; 2], u8) {
			let (model, cx) = open_log_window(cx, ws, from, hash);
			let has = model
				.read_with(cx, |m, _| m.log_filter_split_has_values(m.locale));
			assert_eq!(has, [false; 4], "no active chip");
			let before =
				model.read_with(cx, |m, _| m.log_filter_layout.last_mask.get());
			cx.simulate_resize(gpui::size(gpui::px(to), gpui::px(720.)));
			settle(cx);
			let (after, refreshes) = model.read_with(cx, |m, _| {
				(
					m.log_filter_layout.last_mask.get(),
					m.log_filter_layout.refresh_count.get(),
				)
			});
			(log_row_shape(&model, cx, 0), [before, after], refreshes)
		}

		fn assert_same_shape(
			tag: &str,
			resized: &[(RowCell, bool, f32, f32)],
			fresh: &[(RowCell, bool, f32, f32)],
		) {
			let mut bad = Vec::new();
			for (a, b) in resized.iter().zip(fresh) {
				if a.1 != b.1
					|| (a.2 - b.2).abs() > 0.5
					|| (a.3 - b.3).abs() > 0.5
				{
					bad.push(format!(
						"{:?}: resized (drawn={}, x={:.1}, w={:.1}) vs fresh (drawn={}, x={:.1}, w={:.1})",
						a.0, a.1, a.2, a.3, b.1, b.2, b.3
					));
				}
			}
			assert!(
				bad.is_empty(),
				"{tag}: the resized window differs from a fresh one:\n{}",
				bad.join("\n")
			);
		}

		/// A window widened from 1080 to 1600 draws its rows as a fresh
		/// 1600 window does, with no input in between. No filter chip is
		/// active, so the bar's split stays, and once the frames settle the
		/// bar has no redraw left to ask for (`refresh_count` is back to 0).
		/// The bar's container does move with this resize, so its canvas may
		/// well be what redraws the list here: the list's own canvas is
		/// pinned by `log_rows_follow_a_resize_the_filter_bar_does_not_see`.
		#[gpui::test]
		fn log_rows_follow_a_widening_without_input(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			for hash in [false, true] {
				let (shape, masks, refreshes) =
					row_shape_after_resize(cx, ws.path(), hash, 1080., 1600.);
				assert_eq!(masks[0], masks[1], "the chips' split changed");
				assert_eq!(refreshes, 0, "the filter bar asked for a redraw");
				let (model, fcx) = open_log_window(cx, ws.path(), 1600., hash);
				let fresh = log_row_shape(&model, fcx, 0);
				assert_same_shape(
					&format!("widen hash={hash}"),
					&shape,
					&fresh,
				);
			}
		}

		/// The same for a window narrowed from 1400 to 1160.
		#[gpui::test]
		fn log_rows_follow_a_narrowing_without_input(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			for hash in [false, true] {
				let (shape, masks, refreshes) =
					row_shape_after_resize(cx, ws.path(), hash, 1400., 1160.);
				assert_eq!(masks[0], masks[1], "the chips' split changed");
				assert_eq!(refreshes, 0, "the filter bar asked for a redraw");
				let (model, fcx) = open_log_window(cx, ws.path(), 1160., hash);
				let fresh = log_row_shape(&model, fcx, 0);
				assert_same_shape(
					&format!("narrow hash={hash}"),
					&shape,
					&fresh,
				);
			}
		}

		/// G6: the probe reports each fitted text of the HEAD row, the solver's
		/// width is the drawn one, a dropped group has width 0, and an idle
		/// frame prints nothing.
		#[gpui::test]
		fn text_fit_probe_reports_each_fitted_text_once(
			cx: &mut TestAppContext,
		) {
			use crate::ui::FitRole;
			let ws = tempfile::tempdir().unwrap();
			head_with_labels_dated(ws.path());
			{
				let (model, cx) = open_log_window(cx, ws.path(), 1080., false);
				for role in [
					FitRole::Subject,
					FitRole::BranchLabel,
					FitRole::TagLabel,
					FitRole::Author,
					FitRole::Date,
				] {
					let f = row_text_fit(&model, cx, 0, role)
						.unwrap_or_else(|| panic!("{role:?} has no TEXT_FIT"));
					let fit = f.fit.unwrap_or(0.);
					assert!(fit > 0., "{role:?}: {f:?}");
					assert!((f.drawn - fit).abs() <= 1., "{role:?}: {f:?}");
					assert!(!f.clipped, "{role:?}: {f:?}");
				}
				let (drift, clipped) = model.read_with(cx, |m, _| {
					let p = m.probes.as_ref().unwrap();
					(p.text_fit_drift(), p.text_fit_clipped())
				});
				assert!(drift.is_empty(), "drift: {drift:?}");
				assert!(clipped.is_empty(), "clipped: {clipped:?}");
			}
			// With the hash column on, a 900 window has no room for the
			// labels: the dropped group is reported with width 0.
			let (model, cx) = open_log_window(cx, ws.path(), 900., true);
			for role in [FitRole::BranchLabel, FitRole::TagLabel] {
				let f = row_text_fit(&model, cx, 0, role)
					.unwrap_or_else(|| panic!("{role:?} has no TEXT_FIT"));
				assert_eq!(f.fit, Some(0.), "{role:?}: {f:?}");
				assert!(f.cut && f.shown == 0, "{role:?}: {f:?}");
			}
			// Two more frames print no TEXT_FIT line: nothing changed.
			crate::test_log::take();
			for _ in 0..2 {
				model.update(cx, |_, cx| cx.notify());
				settle(cx);
			}
			let lines: Vec<String> = crate::test_log::take()
				.into_iter()
				.filter(|l| l.contains("[APP:TEXT_FIT"))
				.collect();
			assert!(lines.is_empty(), "an idle frame printed {lines:?}");
		}

		/// What the whole window's `TEXT_FIT`s say at `w`x`h`: ids whose box
		/// is not the solver's width, and ids an ancestor clips.
		fn fit_defects(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> (Vec<String>, Vec<String>) {
			model.read_with(cx, |m, _| {
				let p = m.probes.as_ref().expect("probes are on");
				(p.text_fit_drift(), p.text_fit_clipped())
			})
		}

		/// Two repositories with refs and a long author, in every window size
		/// the native smoke drives, with the real text system where there is
		/// one: no text of the window is drawn in a width the solver did not
		/// give it, and no ancestor clips one.
		#[gpui::test]
		fn no_text_drifts_or_is_clipped_at_the_smoke_viewports(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			real_text_fixture(ws.path());
			let r = repo(ws.path(), "second-repository-with-a-long-name", &[]);
			git(
				&r,
				&["branch", "feature/a-rather-long-branch-name-for-labels"],
			);
			let mut bad = Vec::new();
			for real in [false, true] {
				for (w, h) in [
					(700., 632.),
					(900., 600.),
					(1080., 720.),
					(1280., 860.),
					(1600., 900.),
				] {
					for hash in [false, true] {
						let opened = if real {
							open_real_text(cx, ws.path().to_path_buf(), w, h)
						} else {
							let (model, vcx) = open_sized(
								cx,
								ws.path().to_path_buf(),
								None,
								w,
								h,
							);
							enable_probes(&model, vcx);
							Some((model, vcx))
						};
						let Some((model, cx)) = opened else { continue };
						settle_until(cx, "the log rows", |cx| {
							model.read_with(cx, |m, _| {
								m.display_commits().len() >= 5
							})
						});
						settle(cx);
						if hash {
							click_probed(&model, cx, "btn-log-more");
							click_probed(&model, cx, "log-more:hash");
							click_probed(&model, cx, "btn-log-more");
							click_probed(&model, cx, "log-more:details");
						}
						settle(cx);
						let (drift, clipped) = fit_defects(&model, cx);
						for id in drift {
							bad.push(format!(
								"{w}x{h} real={real} hash={hash}: drift {id}"
							));
						}
						for id in clipped {
							bad.push(format!(
								"{w}x{h} real={real} hash={hash}: clipped {id}"
							));
						}
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} defects:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// Two repositories, the first with branches of 4, 6 .. 40 characters
		/// only it has: a ref row names the repo or not at all.
		fn repos_with_long_branch_names(dir: &Path) -> Vec<String> {
			let mut names = Vec::new();
			for name in ["repo-01-core", "repo-02-api"] {
				let r = repo(dir, name, &[]);
				if name == "repo-01-core" {
					for n in (4..=40).step_by(2) {
						let b: String = "b".repeat(n);
						git(&r, &["branch", &b]);
						names.push(b);
					}
				}
			}
			names
		}

		/// What a pane or menu row of the branch `b` breaks: its repo suffix
		/// is whole or absent, its name keeps its readable floor.
		fn ref_row_violations(
			cx: &mut VisualTestContext,
			b: &str,
			name_selector: Option<String>,
			repo_selector: String,
		) -> Vec<String> {
			use crate::theme::{SMALL_TEXT, UI_TEXT};
			let mut bad = Vec::new();
			let repo_nat = text_w(cx, "repo-01-core", false, SMALL_TEXT);
			if let Some(r) = bounds_of(cx, repo_selector.clone()) {
				let w = f32::from(r.size.width);
				if (0.5..repo_nat - 1.).contains(&w) {
					bad.push(format!(
						"{b}: repo {w:.1} is neither whole ({repo_nat:.1}) nor gone"
					));
				}
			}
			if let Some(sel) = name_selector {
				if let Some(n) = bounds_of(cx, sel) {
					let nat = text_w(cx, b, false, UI_TEXT);
					let floor = text_w(
						cx,
						&format!(
							"{}\u{2026}",
							b.chars().take(6).collect::<String>()
						),
						false,
						UI_TEXT,
					) + 1.;
					let w = f32::from(n.size.width);
					if w < nat.min(floor) - 1. {
						bad.push(format!(
							"{b}: name {w:.1} < min({nat:.1}, floor {floor:.1})"
						));
					}
				}
			}
			bad
		}

		/// #194: in the branches pane a repo suffix is whole or gone, never
		/// a piece of it, and a name keeps its readable floor.
		#[gpui::test]
		fn branches_pane_names_stay_readable_and_the_repo_is_whole_or_gone(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let names = repos_with_long_branch_names(ws.path());
			let mut bad = Vec::new();
			for (w, h) in [(1080., 720.), (1440., 900.)] {
				let (model, cx) =
					open_sized(cx, ws.path().to_path_buf(), None, w, h);
				settle_until(cx, "the two feeds", |cx| {
					model.read_with(cx, |m, _| {
						m.log_feeds.len() == 2 && m.display_commits().len() >= 2
					})
				});
				settle(cx);
				for b in &names {
					for v in ref_row_violations(
						cx,
						b,
						Some(format!("ref-name:refs/heads/{b}")),
						format!("ref-repo:refs/heads/{b}"),
					) {
						bad.push(format!("{w}: {v}"));
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// #194: the same in the branch filter's menu.
		#[gpui::test]
		fn branch_menu_rows_keep_a_readable_name_and_a_whole_repo(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let names = repos_with_long_branch_names(ws.path());
			let mut bad = Vec::new();
			for (w, h) in [(1080., 720.), (1440., 900.)] {
				let (model, cx) =
					open_sized(cx, ws.path().to_path_buf(), None, w, h);
				settle_until(cx, "the two feeds", |cx| {
					model.read_with(cx, |m, _| {
						m.log_feeds.len() == 2 && m.display_commits().len() >= 2
					})
				});
				settle(cx);
				enable_probes(&model, cx);
				click_probed(&model, cx, "log-filter-branch");
				click_probed(&model, cx, "log-branch-group:refs_local");
				for b in &names {
					for v in ref_row_violations(
						cx,
						b,
						Some(format!("menu-name:refs/heads/{b}")),
						format!("menu-repo:refs/heads/{b}"),
					) {
						bad.push(format!("{w}: {v}"));
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// Clicks `id` if it is drawn; whether it was.
		fn click_if_probed(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			id: &str,
		) -> bool {
			let drawn = model
				.read_with(cx, |m, _| m.probes.as_ref().unwrap().bounds(id));
			if drawn.is_some() {
				click_probed(model, cx, id);
			}
			drawn.is_some()
		}

		/// #205: two active chips (a long repo name and a short branch) at
		/// a width where both are on the bar: the shorter value stays whole
		/// when a longer one is cut, and every value keeps its floor.
		#[gpui::test]
		fn the_shorter_active_chip_value_stays_whole(cx: &mut TestAppContext) {
			use crate::theme::UI_TEXT;
			let ws = tempfile::tempdir().unwrap();
			for name in ["repo-01-core-with-a-long-name", "repo-02-api"] {
				let r = repo(ws.path(), name, &[]);
				git(&r, &["checkout", "-q", "-b", "topic-03"]);
				fs::write(r.join("b.txt"), "b").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", "topic work"]);
				git(&r, &["checkout", "-q", "main"]);
			}
			let mut bad = Vec::new();
			let mut w = 1030.;
			while w <= 1090. {
				let (model, cx) =
					open_sized(cx, ws.path().to_path_buf(), None, w, 720.);
				settle_until(cx, "the two feeds", |cx| {
					model.read_with(cx, |m, _| {
						m.log_feeds.len() == 2 && m.display_commits().len() >= 4
					})
				});
				settle(cx);
				enable_probes(&model, cx);
				click_probed(&model, cx, "ref:refs/heads/topic-03");
				if !click_if_probed(&model, cx, "log-filter-repo") {
					click_probed(&model, cx, "log-filter-more");
					click_probed(&model, cx, "log-filter-more:repo");
				}
				click_probed(
					&model,
					cx,
					"log-repo:repo-01-core-with-a-long-name",
				);
				settle(cx);
				settle(cx);
				let loc = model.read_with(cx, |m, _| m.locale);
				// (chip id, "<key>: <value>"), the values being the chips'
				// texts.
				let chips = [
					(
						"log-filter-repo",
						format!(
							"{}: repo-01-core-with-a-long-name",
							crate::i18n::t("log_chip_repo", loc)
						),
						format!("{}: ", crate::i18n::t("log_chip_repo", loc)),
					),
					(
						"log-filter-branch",
						format!(
							"{}: topic-03",
							crate::i18n::t("log_chip_branch", loc)
						),
						format!("{}: ", crate::i18n::t("log_chip_branch", loc)),
					),
				];
				// The chip's padding, gap and icon around its text.
				const CHROME: f32 = 27.;
				let mut on_bar: Vec<(f32, f32, f32)> = Vec::new();
				for (id, text, key) in &chips {
					let Some(b) = bounds_of(cx, id.to_string()) else {
						continue;
					};
					let natural = text_w(cx, text, false, UI_TEXT);
					let drawn = f32::from(b.size.width) - CHROME;
					// The key whole and 4 characters of the value, "…", 1px.
					let value: String = text
						.strip_prefix(key.as_str())
						.unwrap_or(text)
						.chars()
						.take(4)
						.collect();
					let floor = text_w(
						cx,
						&format!("{key}{value}\u{2026}"),
						false,
						UI_TEXT,
					) + 1.;
					if drawn < floor.min(natural) - 2. {
						bad.push(format!(
							"{w}: {id} {drawn:.1} < floor {floor:.1} ({natural:.1})"
						));
					}
					on_bar.push((natural, drawn, floor));
				}
				for (i, a) in on_bar.iter().enumerate() {
					for b in &on_bar[i + 1..] {
						let (short, long) =
							if a.0 <= b.0 { (a, b) } else { (b, a) };
						// `short` is cut while `long` has more than its floor.
						if short.1 < short.0 - 2. && long.1 > long.2 + 1. {
							bad.push(format!(
								"{w}: the shorter value is cut ({:.1} of {:.1}) while a longer one has {:.1} of {:.1}, its floor {:.1}",
								short.1, short.0, long.1, long.0, long.2
							));
						}
					}
				}
				w += 10.;
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// The rows of the long-text fixture: Chinese and long Latin
		/// subjects, authors, a CJK tag, many tags, remote branches.
		fn real_text_fixture(dir: &Path) {
			let r = repo(dir, "solo", &[]);
			let commit =
				|name: &str, subject: &str, author: &str, date: &str| {
					fs::write(r.join(name), name).unwrap();
					git(&r, &["add", "."]);
					git_dated(
						&r,
						&["commit", "-q", "-m", subject, "--author", author],
						date,
					);
				};
			commit(
				"c1.txt",
				"fix",
				"\u{9673}\u{5927}\u{6587}\u{6e2c}\u{8a66}\u{4f5c}\u{8005}\u{540d}\u{7a31} <c@x>",
				"2025-12-28T15:59:00Z",
			);
			git(
				&r,
				&["branch", "feature/a-rather-long-branch-name-for-labels"],
			);
			commit(
				"c2.txt",
				"\u{4fee}\u{6b63}\u{9060}\u{7aef}\u{5de5}\u{4f5c}\u{5340}\u{5728}\u{6162}\u{901f}\u{9023}\u{7dda}\u{4e0b}\u{8b80}\u{53d6}\u{5927}\u{578b}\u{6a94}\u{6848}\u{903e}\u{6642}\u{5f8c}\u{7121}\u{6cd5}\u{91cd}\u{8a66}\u{7684}\u{554f}\u{984c}",
				"Alexandra Konstantinopoulou <a@x>",
				"2025-11-27T15:58:00Z",
			);
			git(
				&r,
				&["tag", "\u{767c}\u{884c}\u{7248}\u{672c}-\u{4e2d}\u{6587}\u{6a19}\u{7c64}"],
			);
			commit(
				"c3.txt",
				"bump dependencies and regenerate the lockfile for the release",
				"t <t@t>",
				"2025-12-20T15:30:00Z",
			);
			for t in ["v1.0.0", "v1.0.1", "v1.0.2", "v1.0.3", "v1.0.4"] {
				git(&r, &["tag", t]);
			}
			commit(
				"c4.txt",
				"a subject long enough to want the whole row for itself and then some more words",
				"t <t@t>",
				"2025-12-21T15:30:00Z",
			);
			git(&r, &["tag", "v0.3.0"]);
			git(&r, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
			git(
				&r,
				&[
					"symbolic-ref",
					"refs/remotes/origin/HEAD",
					"refs/remotes/origin/main",
				],
			);
		}

		/// With the OS text system and the embedded fonts, the rows of the
		/// long-text fixture keep every text within its rule: what a cell
		/// shows (graphemes before the ellipsis) satisfies `rule_holds`, and
		/// a date is whole. Measured only for what the text system gives
		/// (CJK relies on the system's CJK font).
		#[gpui::test]
		fn real_text_log_rows_follow_the_rules(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			real_text_fixture(ws.path());
			let mut bad = Vec::new();
			// (window width, hash column, English).
			for (w, hash, english) in [
				(1080., true, false),
				(900., true, false),
				(900., false, true),
			] {
				let Some((model, cx)) =
					open_real_text(cx, ws.path().to_path_buf(), w, 720.)
				else {
					return;
				};
				settle_until(cx, "the log rows", |cx| {
					model.read_with(cx, |m, _| m.display_commits().len() >= 5)
				});
				settle(cx);
				if english {
					cx.simulate_keystrokes("alt-l");
					settle(cx);
				}
				if hash {
					click_probed(&model, cx, "btn-log-more");
					click_probed(&model, cx, "log-more:hash");
				}
				settle(cx);
				for ix in 0..4 {
					for s in row_cells(&model, cx, ix) {
						let Some(role) = s.role else { continue };
						if !s.rule_holds() {
							bad.push(format!(
								"{w} hash={hash} en={english} row {ix} {:?} {:?}: {}/{} in {:.1}px breaks {:?}",
								s.cell,
								s.text,
								s.shown,
								s.total,
								s.visible,
								role.rule()
							));
						}
						// A4: the solver's width is the drawn one, and no
						// ancestor clips the text.
						if s.fit.is_some_and(|fit| (s.w - fit).abs() > 1.)
							|| s.visible < s.w - 0.5
						{
							bad.push(format!(
								"{w} hash={hash} en={english} row {ix} {:?} drifts or is clipped: {s:?}",
								s.cell
							));
						}
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// Inter's digits are proportional (1 is far narrower than 4), so a
		/// date is only whole where the box fits its widest digits: dates with
		/// "10/24" and "04:44" in them (zh and en long forms, and the today and
		/// yesterday forms) are whole at 900 and 1080 with the hash column on.
		/// `shown == total` alone has a +0.5 tolerance gpui's truncation does
		/// not, so the box must also hold the raw text width.
		#[gpui::test]
		fn real_text_dates_with_wide_digits_are_whole(cx: &mut TestAppContext) {
			use chrono::{Duration, Local};
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "solo", &[]);
			let today = Local::now().date_naive();
			let days = [
				"2024-10-24T04:44:00".to_string(),
				"2020-10-20T20:00:00".to_string(),
				format!("{}T04:44:00", today - Duration::days(1)),
				format!("{}T04:44:00", today),
			];
			for (i, date) in days.iter().enumerate() {
				fs::write(r.join(format!("d{i}.txt")), "d").unwrap();
				git(&r, &["add", "."]);
				git_dated(
					&r,
					&["commit", "-q", "-m", &format!("subject number {i}")],
					date,
				);
			}
			let mut bad = Vec::new();
			// (window width, English).
			for (w, english) in
				[(900., false), (900., true), (1080., false), (1080., true)]
			{
				let Some((model, cx)) =
					open_real_text(cx, ws.path().to_path_buf(), w, 720.)
				else {
					return;
				};
				settle_until(cx, "the log rows", |cx| {
					model.read_with(cx, |m, _| m.display_commits().len() >= 4)
				});
				settle(cx);
				if english {
					cx.simulate_keystrokes("alt-l");
					settle(cx);
				}
				click_probed(&model, cx, "btn-log-more");
				click_probed(&model, cx, "log-more:hash");
				settle(cx);
				for ix in 0..4 {
					let Some(d) = seen_cell(&model, cx, ix, RowCell::Date)
					else {
						bad.push(format!("{w} en={english} row {ix}: no date"));
						continue;
					};
					if d.shown != d.total || d.visible < d.natural {
						bad.push(format!(
							"{w} en={english} row {ix} {:?}: {}/{} in {:.1}px of {:.1}",
							d.text, d.shown, d.total, d.visible, d.natural
						));
					}
				}
			}
			assert!(
				bad.is_empty(),
				"{} violations:\n{}",
				bad.len(),
				bad.join("\n")
			);
		}

		/// The test platform measures with `NoopTextSystem` (every char 0.6em),
		/// so CJK is undercounted and truncation never shows (#224).
		/// `use_real_text_system` swaps in the OS text system (CoreText on
		/// macOS, cosmic-text on Linux; patch listed in vendor/gpui/SNIP_PATCH.md),
		/// and `open_real_text` registers the embedded fonts as the app does.
		/// Windows is skipped: building DirectWrite without a window was not
		/// attempted. Without an installed CJK font the CJK check fails.
		#[gpui::test]
		fn real_text_system_measures_cjk_and_the_log_labels_stay_readable(
			cx: &mut TestAppContext,
		) {
			const TAG: &str = "\u{767c}\u{884c}\u{7248}\u{672c}-\u{4e2d}\u{6587}\u{6a19}\u{7c64}";
			let ws = tempfile::tempdir().unwrap();
			head_with_labels(ws.path());
			let r = ws.path().join("solo");
			git(&r, &["tag", "-d", "v0.3.0"]);
			git(&r, &["tag", TAG]);
			let Some((model, cx)) =
				open_real_text(cx, ws.path().to_path_buf(), 1080., 720.)
			else {
				return;
			};
			settle_until(cx, "the log rows", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 2)
			});
			settle(cx);
			assert_eq!(
				head_label_texts(&model, cx),
				["main, origin/main", TAG]
			);
			// Whatever width the list leaves the CJK tag, the rule holds:
			// six of its characters at least, or all of it.
			let tag = seen_cell(&model, cx, 0, RowCell::Label(1)).expect("tag");
			assert!(tag.rule_holds(), "{tag:?}");
			assert!(tag.visible >= tag.floor - 1., "{tag:?}");
			let bad = row_policy_violations(&model, cx, 0, 1080.);
			assert!(bad.is_empty(), "{bad:?}");
		}

		/// #229: the width x locale x hash x panes matrix. Every cell opens
		/// a window with the OS text system and the embedded fonts, draws
		/// the fixture's Log and reads the whole window's `TEXT_FIT` (#228):
		/// what each text shows is checked against the rule of its role
		/// (graphemes, never pixels). It is inline so that the redraw ban of
		/// `check_test_refresh.sh`, which only reads what follows the
		/// tests module's opening line, still covers it.
		mod fit_matrix {
			use super::*;
			use crate::ui::{rule_holds, FitRole};
			use std::collections::{BTreeMap, BTreeSet};

			/// Which panes the Log shows beside the list.
			#[derive(Clone, Copy, Debug, PartialEq, Eq)]
			enum Panes {
				/// The details and branches panes, as the app opens.
				Default,
				/// Both switched off in the "more" menu: the widest list.
				ListOnly,
			}

			/// One text of the latest frame, as the probe saw it.
			#[derive(Clone, Debug)]
			struct Seen {
				id: String,
				role: FitRole,
				/// The display row of a commit row's text.
				row: Option<usize>,
				/// Drawn at all: a dropped text only has a `fit` of 0.
				present: bool,
				x: f32,
				visible: f32,
				natural: f32,
				shown: usize,
				total: usize,
				key_graphemes: usize,
				clipped: bool,
				drifts: bool,
				text: String,
			}

			impl Seen {
				/// What is in sight, from left to right.
				fn span(&self) -> (f32, f32) {
					(self.x, self.x + self.visible)
				}

				fn describe(&self) -> String {
					format!(
						"{} {:?} shown {}/{} rule {} visible {:.1} natural {:.1}",
						self.id,
						self.text,
						self.shown,
						self.total,
						self.role.rule().name(),
						self.visible,
						self.natural
					)
				}
			}

			/// Every text of the latest frame. A commit row's is tied to its
			/// display row by the row key (`log_row_key`).
			fn snapshot(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> Vec<Seen> {
				model.read_with(cx, |m, _| {
					let keys: BTreeMap<String, usize> = m
						.display_commits()
						.iter()
						.enumerate()
						.map(|(ix, c)| (m.log_row_key(c), ix))
						.collect();
					m.probes
						.as_ref()
						.expect("probes are on")
						.text_fits()
						.into_iter()
						.map(|(id, f)| {
							let rest =
								id.strip_prefix("text-fit:").unwrap_or(&id);
							let scope =
								rest.rsplit_once(':').map_or(rest, |(s, _)| s);
							let row = scope
								.strip_prefix("row:")
								.and_then(|k| keys.get(k).copied());
							Seen {
								row,
								present: f.fit != Some(0.),
								role: f.role,
								x: f.x,
								visible: f.visible,
								natural: f.natural,
								shown: f.shown,
								total: f.total,
								key_graphemes: f.key_graphemes,
								clipped: f.clipped,
								drifts: f.drifts() || f.style_drifts(),
								text: f.text.to_string(),
								id,
							}
						})
						.collect()
				})
			}

			/// What a cell's checks need to know besides the texts.
			struct CellCtx {
				hash: bool,
				width: f32,
				/// The first rows that must have been drawn.
				drawn_rows: usize,
				/// Rows that have refs, and collapsed merges.
				ref_rows: Vec<usize>,
				collapsed_rows: Vec<usize>,
				/// The list's left and right edge, and each row's gutter.
				list: (f32, f32),
				gutters: BTreeMap<usize, (f32, f32)>,
			}

			fn cell_ctx(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				width: f32,
				hash: bool,
				drawn_rows: usize,
			) -> CellCtx {
				let (ref_rows, collapsed_rows, n) =
					model.read_with(cx, |m, _| {
						let rows = m.display_commits();
						let refs = (0..rows.len())
							.filter(|&ix| {
								m.graph_layout.as_ref().is_some_and(|l| {
									!l.rows[ix].refs.is_empty()
								})
							})
							.collect();
						let collapsed = (0..rows.len())
							.filter(|&ix| {
								m.collapsed_merges.contains(&rows[ix].sha)
							})
							.collect();
						(refs, collapsed, rows.len())
					});
				let list = bounds_of(cx, "log-list".into())
					.map(|b| (f32::from(b.left()), f32::from(b.right())))
					.expect("the Log's list is drawn");
				let mut gutters = BTreeMap::new();
				for ix in 0..n {
					if let Some(b) = bounds_of(cx, format!("log-gutter:{ix}")) {
						gutters.insert(
							ix,
							(f32::from(b.left()), f32::from(b.right())),
						);
					}
				}
				CellCtx {
					hash,
					width,
					drawn_rows,
					ref_rows,
					collapsed_rows,
					list,
					gutters,
				}
			}

			/// I1 to I4 over one snapshot; every violation, each naming the
			/// cell, the text, how much of it shows and the rule.
			fn check(cell: &str, s: &[Seen], ctx: &CellCtx) -> Vec<String> {
				let mut bad = Vec::new();
				let mut say = |i: &str, what: String| {
					bad.push(format!("{cell}: {i} {what}"));
				};
				for t in s {
					if t.id.starts_with("text-fit:row:") && t.row.is_none() {
						say(
							"stale",
							format!("{} is of no displayed row", t.id),
						);
					}
					if !t.present {
						continue;
					}
					// I1: what shows satisfies the role's rule.
					if !rule_holds(
						t.role.rule(),
						t.shown,
						t.total,
						t.key_graphemes,
					) {
						say("I1", format!("breaks its rule: {}", t.describe()));
					}
					// I2: nothing is clipped by an ancestor or drawn in a
					// width the solver did not give.
					if t.clipped || t.drifts {
						say(
							"I2",
							format!(
								"clipped={} drift={}: {}",
								t.clipped,
								t.drifts,
								t.describe()
							),
						);
					}
				}
				// Per row.
				let mut rows: BTreeMap<usize, Vec<&Seen>> = BTreeMap::new();
				for t in s {
					if let Some(ix) = t.row {
						rows.entry(ix).or_default().push(t);
					}
				}
				for ix in 0..ctx.drawn_rows {
					let subject = rows.get(&ix).and_then(|r| {
						r.iter().find(|t| t.role == FitRole::Subject)
					});
					assert!(
						subject.is_some(),
						"{cell}: row {ix} was not drawn: fixture or window height is wrong"
					);
				}
				for (ix, texts) in &rows {
					let has = |r: FitRole| {
						texts.iter().find(|t| t.role == r).map(|t| t.present)
					};
					// I3: what is in sight never overlaps and stays in the
					// list.
					let mut spans: Vec<(String, (f32, f32))> = texts
						.iter()
						.filter(|t| t.present && t.visible > 0.5)
						.map(|t| (format!("{:?}", t.role), t.span()))
						.collect();
					if let Some(g) = ctx.gutters.get(ix) {
						spans.push(("Gutter".into(), *g));
					}
					for (i, (an, a)) in spans.iter().enumerate() {
						if a.0 < ctx.list.0 - 0.5 || a.1 > ctx.list.1 + 0.5 {
							say(
								"I3",
								format!(
									"row {ix} {an} [{:.1},{:.1}] leaves the list [{:.1},{:.1}]",
									a.0, a.1, ctx.list.0, ctx.list.1
								),
							);
						}
						for (bn, b) in &spans[i + 1..] {
							if a.0 < b.1 - 0.5 && b.0 < a.1 - 0.5 {
								say(
									"I3",
									format!(
										"row {ix} {an} [{:.1},{:.1}] overlaps {bn} [{:.1},{:.1}]",
										a.0, a.1, b.0, b.1
									),
								);
							}
						}
					}
					// I4: what a row has.
					if ctx.hash != (has(FitRole::Hash) == Some(true)) {
						say(
							"I4",
							format!(
								"row {ix}: the hash column is {:?} with the hash setting {}",
								has(FitRole::Hash),
								ctx.hash
							),
						);
					}
					let group: Vec<(FitRole, bool)> = [
						FitRole::BranchLabel,
						FitRole::TagLabel,
						FitRole::RefMore,
					]
					.into_iter()
					.filter_map(|r| has(r).map(|p| (r, p)))
					.collect();
					if group.iter().any(|(_, p)| *p != group[0].1) {
						say(
							"I4",
							format!(
								"row {ix}: the labels do not come and go together: {group:?}"
							),
						);
					}
					if ctx.width >= 1080.
						&& ctx.ref_rows.contains(ix)
						&& !group.iter().any(|(_, p)| *p)
					{
						say(
							"I4",
							format!(
								"row {ix} has refs but no labels at {}",
								ctx.width
							),
						);
					}
					if ctx.collapsed_rows.contains(ix)
						&& has(FitRole::Collapsed).is_none()
					{
						say(
							"I4",
							format!("collapsed row {ix} has no chip text"),
						);
					}
					for r in [FitRole::Date, FitRole::Subject] {
						if has(r) != Some(true) {
							say("I4", format!("row {ix} has no {r:?}"));
						}
					}
				}
				bad
			}

			/// I3 for what is not a row: the filter chips stay clear of the
			/// buttons beside them and inside the list's width; the header's
			/// buttons stay inside the panel and clear of the hint.
			fn check_bars(
				cell: &str,
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				ctx: &CellCtx,
				s: &[Seen],
			) -> Vec<String> {
				let scale = cx.update(|w, _| w.scale_factor());
				let window_w =
					cx.update(|w, _| f32::from(w.viewport_size().width));
				let (ids, bounds): (Vec<String>, BTreeMap<String, [i32; 4]>) =
					model.read_with(cx, |m, _| {
						let p = m.probes.as_ref().expect("probes are on");
						let ids = p.drawn();
						let b = ids
							.iter()
							.filter_map(|id| Some((id.clone(), p.bounds(id)?)))
							.collect();
						(ids, b)
					});
				let span = |id: &str| -> Option<(f32, f32)> {
					let b = bounds.get(id)?;
					Some((b[0] as f32 / scale, (b[0] + b[2]) as f32 / scale))
				};
				let mut bad = Vec::new();
				let chips: Vec<&String> = ids
					.iter()
					.filter(|id| {
						id.starts_with("log-filter-")
							&& !id.ends_with("-clear")
							&& !id.contains(':')
					})
					.collect();
				let blockers = [
					"btn-log-refresh",
					"btn-head",
					"btn-compare",
					"btn-copy-commits",
				];
				let overlaps = |a: (f32, f32), b: (f32, f32)| {
					a.0 < b.1 - 0.5 && b.0 < a.1 - 0.5
				};
				for chip in &chips {
					let Some(c) = span(chip) else { continue };
					if c.1 > ctx.list.1 + 0.5 {
						bad.push(format!(
							"{cell}: I3 chip {chip} ends at {:.1} past the list's {:.1}",
							c.1, ctx.list.1
						));
					}
					for other in blockers {
						if span(other).is_some_and(|o| overlaps(c, o)) {
							bad.push(format!(
								"{cell}: I3 chip {chip} overlaps {other}"
							));
						}
					}
					for other in &chips {
						if *other != *chip
							&& span(other).is_some_and(|o| overlaps(c, o))
						{
							bad.push(format!(
								"{cell}: I3 chip {chip} overlaps chip {other}"
							));
						}
					}
				}
				let hint = s
					.iter()
					.find(|t| t.role == FitRole::RefHint && t.present)
					.map(Seen::span);
				for button in ["btn-log-more", "btn-log-hide"] {
					let Some(b) = span(button) else { continue };
					// The panel runs from the list's left edge to the
					// window's right one (the details pane is inside it).
					if b.0 < ctx.list.0 - 0.5 || b.1 > window_w + 0.5 {
						bad.push(format!(
							"{cell}: I3 {button} [{:.1},{:.1}] is not whole in the panel [{:.1},{:.1}]",
							b.0, b.1, ctx.list.0, window_w
						));
					}
					if hint.is_some_and(|h| overlaps(b, h)) {
						bad.push(format!(
							"{cell}: I3 {button} overlaps the ref hint {hint:?}"
						));
					}
				}
				bad
			}

			fn assert_clean(bad: Vec<String>) {
				assert!(
					bad.is_empty(),
					"{} violations:\n{}",
					bad.len(),
					bad.join("\n")
				);
			}

			/// The Log of the row matrix: the main history with a merge of a
			/// side branch, a CJK subject and author, a CJK tag, five tags
			/// (their `+N`), a long branch name, dates with wide digits
			/// ("10/24", "04:44"). Collapsed, rows 0 to 5 are HEAD, bump,
			/// the CJK one, fix, the merge and init.
			struct FitFixture {
				merge7: String,
			}

			/// The viewer's own commit on HEAD: 56 of the widest capital,
			/// whose three-letter floor is wider in the weight it is drawn
			/// in than in the regular one (#229 review).
			const OWN_AUTHOR: &str = "WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW <me@x>";

			/// A name whose letters kern (T-a, a-y): gpui cuts it by the sum of
			/// its characters, which the shaped line is narrower than.
			const KERNED_AUTHOR: &str = "Taylor Smith <ts@x>";

			fn fit_fixture(dir: &Path) -> FitFixture {
				let r = dir.join("solo");
				fs::create_dir(&r).unwrap();
				git(&r, &["init", "-q", "-b", "main"]);
				// The viewer is `me@x`: the HEAD commit below is theirs, so
				// its author is drawn in the viewer's own weight.
				git(&r, &["config", "user.email", "me@x"]);
				let commit = |file: &str,
				              subject: &str,
				              author: Option<&str>,
				              date: &str| {
					fs::write(r.join(file), file).unwrap();
					git(&r, &["add", "."]);
					let mut args = vec!["commit", "-q", "-m", subject];
					if let Some(a) = author {
						args.extend(["--author", a]);
					}
					git_dated(&r, &args, date);
				};
				commit(
					"init.txt",
					"init",
					Some("AVAVAV Tomas <av@x>"),
					"2025-10-01T10:00:00Z",
				);
				git(&r, &["checkout", "-q", "-b", "side"]);
				commit("side.txt", "side work", None, "2025-10-02T10:00:00Z");
				git(&r, &["checkout", "-q", "main"]);
				git_dated(
					&r,
					&[
						"merge",
						"-q",
						"--no-ff",
						"side",
						"-m",
						"merge side work into main",
					],
					"2025-10-03T10:00:00Z",
				);
				let merge7 = {
					let out = Command::new("git")
						.current_dir(&r)
						.args(["rev-parse", "--short=7", "HEAD"])
						.output()
						.expect("git rev-parse");
					String::from_utf8(out.stdout).unwrap().trim().to_string()
				};
				commit(
					"fix.txt",
					"fix",
					Some("\u{9673}\u{5927}\u{6587}\u{6e2c}\u{8a66}\u{4f5c}\u{8005}\u{540d}\u{7a31} <c@x>"),
					"2025-10-10T15:59:00Z",
				);
				git(
					&r,
					&["branch", "feature/a-rather-long-branch-name-for-labels"],
				);
				commit(
					"zh.txt",
					"\u{4fee}\u{6b63}\u{9060}\u{7aef}\u{5de5}\u{4f5c}\u{5340}\u{5728}\u{6162}\u{901f}\u{9023}\u{7dda}\u{4e0b}\u{8b80}\u{53d6}\u{5927}\u{578b}\u{6a94}\u{6848}\u{903e}\u{6642}\u{5f8c}\u{7121}\u{6cd5}\u{91cd}\u{8a66}\u{7684}\u{554f}\u{984c}",
					Some("Alexandra Konstantinopoulou <a@x>"),
					"2025-10-24T04:44:00Z",
				);
				git(
					&r,
					&["tag", "\u{767c}\u{884c}\u{7248}\u{672c}-\u{4e2d}\u{6587}\u{6a19}\u{7c64}"],
				);
				commit(
					"bump.txt",
					"bump dependencies and regenerate the lockfile for the release",
					Some(KERNED_AUTHOR),
					"2025-12-20T15:30:00Z",
				);
				for t in ["v1.0.0", "v1.0.1", "v1.0.2", "v1.0.3", "v1.0.4"] {
					git(&r, &["tag", t]);
				}
				commit(
					"head.txt",
					"a subject long enough to want the whole row for itself and then some more words",
					Some(OWN_AUTHOR),
					"2025-12-21T15:30:00Z",
				);
				git(&r, &["tag", "release-0.3.0-candidate-2"]);
				git(&r, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
				git(
					&r,
					&[
						"symbolic-ref",
						"refs/remotes/origin/HEAD",
						"refs/remotes/origin/main",
					],
				);
				FitFixture { merge7 }
			}

			/// Two repositories with a `topic-03` each, and a branch of each
			/// that only it has: `only-in-core` (its repo's name is too wide
			/// for any pane or menu row, so it is dropped) and `ci` (a short
			/// name with a repo name that fits whole).
			fn chip_fixture(dir: &Path) {
				for (name, own) in [
					("repo-01-core-with-a-long-name", "only-in-core"),
					("repo-02-api", "ci"),
				] {
					let r = repo(dir, name, &[]);
					for branch in ["topic-03", own] {
						git(&r, &["checkout", "-q", "-b", branch]);
						fs::write(r.join(format!("{branch}.txt")), "b")
							.unwrap();
						git(&r, &["add", "."]);
						git(
							&r,
							&["commit", "-q", "-m", &format!("{branch} work")],
						);
						git(&r, &["checkout", "-q", "main"]);
					}
				}
			}

			/// The locales a cell goes through, with `check` run for each:
			/// the first is what the app opens in, `alt-l` switches.
			fn each_locale(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				mut run: impl FnMut(&Entity<WorkbenchModel>, &mut VisualTestContext),
			) {
				let mut seen = BTreeSet::new();
				for _ in 0..2 {
					let loc = model.read_with(cx, |m, _| m.locale);
					assert!(
						seen.insert(format!("{loc:?}")),
						"alt-l did not switch"
					);
					run(model, cx);
					cx.simulate_keystrokes("alt-l");
					settle(cx);
				}
				assert_eq!(seen.len(), 2);
			}

			/// Opens the row matrix's window: the fixture, merge collapsed,
			/// hash and panes as the cell says. `None` without a real text
			/// system.
			fn open_rows<'a>(
				cx: &'a mut TestAppContext,
				ws: &Path,
				fx: &FitFixture,
				w: f32,
				h: f32,
				hash: bool,
				panes: Panes,
			) -> Option<(Entity<WorkbenchModel>, &'a mut VisualTestContext)>
			{
				let (model, cx) = open_real_text(cx, ws.to_path_buf(), w, h)?;
				settle_until(cx, "the log rows", |cx| {
					model.read_with(cx, |m, _| m.display_commits().len() >= 7)
				});
				settle(cx);
				click_probed(&model, cx, &format!("collapse:{}", fx.merge7));
				assert_eq!(
					model.read_with(cx, |m, _| m.collapsed_merges.len()),
					1,
					"the merge did not collapse"
				);
				if hash {
					click_probed(&model, cx, "btn-log-more");
					click_probed(&model, cx, "log-more:hash");
				}
				if panes == Panes::ListOnly {
					for pane in ["log-more:details", "log-more:branches"] {
						click_probed(&model, cx, "btn-log-more");
						click_probed(&model, cx, pane);
					}
				}
				settle(cx);
				assert_eq!(model.read_with(cx, |m, _| m.log_show_hash), hash);
				Some((model, cx))
			}

			fn rows_cell(
				cx: &mut TestAppContext,
				name: &str,
				w: f32,
				h: f32,
				hash: bool,
				panes: Panes,
			) {
				let ws = tempfile::tempdir().unwrap();
				let fx = fit_fixture(ws.path());
				let Some((model, cx)) =
					open_rows(cx, ws.path(), &fx, w, h, hash, panes)
				else {
					return;
				};
				let mut bad = Vec::new();
				let mut roles: BTreeSet<String> = BTreeSet::new();
				each_locale(&model, cx, |model, cx| {
					let loc =
						model.read_with(cx, |m, _| format!("{:?}", m.locale));
					let name = format!("{name} {loc}");
					let ctx = cell_ctx(model, cx, w, hash, 5);
					let s = snapshot(model, cx);
					roles.extend(
						s.iter()
							.filter(|t| t.present)
							.map(|t| format!("{:?}", t.role)),
					);
					bad.extend(check(&name, &s, &ctx));
					bad.extend(check_bars(&name, model, cx, &ctx, &s));
				});
				// The branches pane, when drawn: a ref's click puts it in the
				// header's hint.
				if click_if_probed(
					&model,
					cx,
					"ref:refs/heads/feature/a-rather-long-branch-name-for-labels",
				) {
					settle(cx);
					let name = format!("{name} ref-hint");
					let ctx = cell_ctx(&model, cx, w, hash, 0);
					let s = snapshot(&model, cx);
					roles.extend(
						s.iter()
							.filter(|t| t.present)
							.map(|t| format!("{:?}", t.role)),
					);
					bad.extend(check(&name, &s, &ctx));
					bad.extend(check_bars(&name, &model, cx, &ctx, &s));
				}
				for role in FitRole::ALL {
					if matrix_cell(role) == name
						&& !roles.contains(&format!("{role:?}"))
					{
						bad.push(format!(
							"{name}: {role:?} is named to this cell but was never drawn in it"
						));
					}
				}
				assert_clean(bad);
			}

			/// How many whole graphemes of `text` a box `visible` wide shows
			/// when drawn in `weight`, from gpui's own cut
			/// (`LineWrapper::truncate_line`, the call `text_ellipsis` makes)
			/// and not through the code under test: the oracle is the
			/// function that draws.
			fn shown_in_weight(
				window: &gpui::Window,
				text: &str,
				visible: f32,
				weight: gpui::FontWeight,
				chrome: f32,
			) -> usize {
				use unicode_segmentation::UnicodeSegmentation;
				let mut font = gpui::font(crate::theme::UI_FONT);
				font.weight = weight;
				let mut runs = vec![gpui::TextRun {
					len: text.len(),
					font: font.clone(),
					color: gpui::black(),
					background_color: None,
					underline: None,
					strikethrough: None,
				}];
				let mut wrapper = window
					.text_system()
					.line_wrapper(font, gpui::px(crate::theme::UI_TEXT));
				let cut = wrapper.truncate_line(
					text.to_string().into(),
					gpui::px(visible - chrome),
					"\u{2026}",
					&mut runs,
				);
				match cut
					.strip_suffix('\u{2026}')
					.filter(|_| cut.as_ref() != text)
				{
					Some(kept) => kept.graphemes(true).count(),
					None => text.graphemes(true).count(),
				}
			}

			/// The viewer's own name is drawn in semibold, and the column,
			/// the rule's floor and TEXT_FIT must all count that weight: at
			/// 1080 wide with the default panes and long labels the author
			/// column is at its floor, and three letters must be in sight
			/// as drawn, not as a regular face would draw them.
			#[gpui::test]
			fn the_viewers_own_author_keeps_three_graphemes_in_the_weight_drawn(
				cx: &mut TestAppContext,
			) {
				let ws = tempfile::tempdir().unwrap();
				let fx = fit_fixture(ws.path());
				let Some((model, cx)) = open_rows(
					cx,
					ws.path(),
					&fx,
					1080.,
					720.,
					false,
					Panes::Default,
				) else {
					return;
				};
				let s = snapshot(&model, cx);
				let author = s
					.iter()
					.find(|t| {
						t.row == Some(0)
							&& t.role == FitRole::Author
							&& t.present
					})
					.expect("the HEAD row draws its author");
				assert!(
					author.text.starts_with("WWW"),
					"{}",
					author.describe()
				);
				let chrome = FitRole::Author.chrome();
				let own = cx.update(|window, _| {
					shown_in_weight(
						window,
						&author.text,
						author.visible,
						gpui::FontWeight::SEMIBOLD,
						chrome,
					)
				});
				assert!(
					own >= 3,
					"semibold shows {own} graphemes, below the rule: {}",
					author.describe()
				);
				assert_eq!(
					author.shown,
					own,
					"TEXT_FIT counts what is drawn: {}",
					author.describe()
				);
				// Every other author, kerned ones among them ("Taylor Smith",
				// "AVAVAV Tomas"), is counted as gpui cuts it, in regular.
				let mut others = 0;
				for t in s.iter().filter(|t| {
					t.role == FitRole::Author && t.present && t.row != Some(0)
				}) {
					others += 1;
					let kept = cx.update(|window, _| {
						shown_in_weight(
							window,
							&t.text,
							t.visible,
							gpui::FontWeight::NORMAL,
							chrome,
						)
					});
					assert_eq!(
						t.shown,
						kept,
						"TEXT_FIT counts what is drawn: {}",
						t.describe()
					);
					assert!(
						kept >= 3.min(t.total),
						"{kept} graphemes, below the rule: {}",
						t.describe()
					);
				}
				assert!(others >= 3, "{others} other authors drawn");
				let names: Vec<&str> = s
					.iter()
					.filter(|t| t.role == FitRole::Author && t.present)
					.map(|t| t.text.as_str())
					.collect();
				for kerned in ["Taylor Smith", "AVAVAV Tomas"] {
					assert!(
						names.contains(&kerned),
						"{kerned} not drawn: {names:?}"
					);
				}
			}

			macro_rules! fit_rows_cells {
				($($name:ident: ($w:expr, $h:expr, $hash:expr, $panes:expr)),* $(,)?) => {
					const FIT_ROWS_CELLS: &[&str] = &[$(stringify!($name)),*];
					$(
						#[gpui::test]
						fn $name(cx: &mut TestAppContext) {
							rows_cell(cx, stringify!($name), $w, $h, $hash, $panes);
						}
					)*
				};
			}

			fit_rows_cells! {
				fit_rows_w0900_hash_off_panes_default: (900., 600., false, Panes::Default),
				fit_rows_w0900_hash_off_panes_listonly: (900., 600., false, Panes::ListOnly),
				fit_rows_w0900_hash_on_panes_default: (900., 600., true, Panes::Default),
				fit_rows_w0900_hash_on_panes_listonly: (900., 600., true, Panes::ListOnly),
				fit_rows_w1080_hash_off_panes_default: (1080., 720., false, Panes::Default),
				fit_rows_w1080_hash_off_panes_listonly: (1080., 720., false, Panes::ListOnly),
				fit_rows_w1080_hash_on_panes_default: (1080., 720., true, Panes::Default),
				fit_rows_w1080_hash_on_panes_listonly: (1080., 720., true, Panes::ListOnly),
				fit_rows_w1440_hash_off_panes_default: (1440., 900., false, Panes::Default),
				fit_rows_w1440_hash_off_panes_listonly: (1440., 900., false, Panes::ListOnly),
				fit_rows_w1440_hash_on_panes_default: (1440., 900., true, Panes::Default),
				fit_rows_w1440_hash_on_panes_listonly: (1440., 900., true, Panes::ListOnly),
			}

			/// The matrix cell that draws `role`: the structural test checks
			/// each is a cell that exists, and each cell checks that what it
			/// is named for was drawn in it.
			fn matrix_cell(role: FitRole) -> &'static str {
				match role {
					FitRole::Hash => "fit_rows_w1080_hash_on_panes_default",
					FitRole::Author => "fit_rows_w1440_hash_off_panes_default",
					FitRole::RefHint
					| FitRole::PaneGroup
					| FitRole::PaneRefName => "fit_rows_w1440_hash_off_panes_default",
					FitRole::PaneRepo
					| FitRole::MenuRefName
					| FitRole::MenuRepo
					| FitRole::ChipRepo
					| FitRole::ChipBranch
					| FitRole::ChipUser
					| FitRole::ChipMore => "fit_chips_w1080",
					FitRole::ChipDate => "fit_chips_w1440",
					FitRole::Subject
					| FitRole::BranchLabel
					| FitRole::TagLabel
					| FitRole::RefMore
					| FitRole::Collapsed
					| FitRole::Date => "fit_rows_w1080_hash_off_panes_default",
				}
			}

			/// The chip matrix: two active filters (a long repo name and a
			/// short branch), the branches pane and the branch menu, in both
			/// locales.
			fn chips_cell(cx: &mut TestAppContext, name: &str, w: f32, h: f32) {
				let ws = tempfile::tempdir().unwrap();
				chip_fixture(ws.path());
				let Some((model, cx)) =
					open_real_text(cx, ws.path().to_path_buf(), w, h)
				else {
					return;
				};
				settle_until(cx, "the two feeds", |cx| {
					model.read_with(cx, |m, _| {
						m.log_feeds.len() == 2 && m.display_commits().len() >= 4
					})
				});
				settle(cx);
				let mut bad = Vec::new();
				let mut roles: BTreeSet<String> = BTreeSet::new();
				// Before any filter every repo is in scope: the pane and the
				// menu name the repo of a branch that only one has.
				each_locale(&model, cx, |model, cx| {
					let loc =
						model.read_with(cx, |m, _| format!("{:?}", m.locale));
					let name = format!("{name} {loc} unfiltered");
					let ctx = cell_ctx(model, cx, w, false, 0);
					let s = snapshot(model, cx);
					roles.extend(
						s.iter()
							.filter(|t| t.present)
							.map(|t| format!("{:?}", t.role)),
					);
					bad.extend(check(&name, &s, &ctx));
					bad.extend(check_bars(&name, model, cx, &ctx, &s));
					for own in ["only-in-core", "ci"] {
						let id =
							format!("text-fit:ref:refs/heads/{own}:pane-name");
						if !s.iter().any(|t| t.id == id) {
							bad.push(format!(
								"{name}: the branches pane drew no row of {own}"
							));
						}
					}
					open_menu(model, cx, "branch");
					click_probed(model, cx, "log-branch-group:refs_local");
					let s = snapshot(model, cx);
					roles.extend(
						s.iter()
							.filter(|t| t.present)
							.map(|t| format!("{:?}", t.role)),
					);
					bad.extend(check(&format!("{name} menu"), &s, &ctx));
					for own in ["only-in-core", "ci"] {
						let id = format!(
							"text-fit:log-branch:refs/heads/{own}:menu-name"
						);
						if !s.iter().any(|t| t.id == id) {
							bad.push(format!(
								"{name}: the branch menu drew no row of {own}"
							));
						}
					}
					cx.simulate_keystrokes("escape");
					settle(cx);
				});
				click_probed(&model, cx, "ref:refs/heads/topic-03");
				open_menu(&model, cx, "repo");
				click_probed(
					&model,
					cx,
					"log-repo:repo-01-core-with-a-long-name",
				);
				settle(cx);
				settle(cx);
				each_locale(&model, cx, |model, cx| {
					let loc =
						model.read_with(cx, |m, _| format!("{:?}", m.locale));
					let name = format!("{name} {loc}");
					let ctx = cell_ctx(model, cx, w, false, 0);
					let s = snapshot(model, cx);
					roles.extend(
						s.iter()
							.filter(|t| t.present)
							.map(|t| format!("{:?}", t.role)),
					);
					bad.extend(check(&name, &s, &ctx));
					bad.extend(check_bars(&name, model, cx, &ctx, &s));
					// Of the chips on the bar with a value, a shorter one is
					// whole when a longer one is cut.
					let valued: Vec<&Seen> = s
						.iter()
						.filter(|t| {
							t.present
								&& t.id.contains(":chip-")
								&& t.total > t.key_graphemes
						})
						.collect();
					assert!(!valued.is_empty(), "{name}: no chip has a value");
					for cut in valued.iter().filter(|t| t.shown < t.total) {
						for short in
							valued.iter().filter(|t| t.natural < cut.natural)
						{
							if short.shown < short.total {
								bad.push(format!(
									"{name}: the shorter chip is cut while a longer one is: {} / {}",
									short.describe(),
									cut.describe()
								));
							}
						}
					}
					// An active chip the bar folded into "more" has its clear
					// item there.
					for (key, chip) in
						[("repo", "chip-repo"), ("branch", "chip-branch")]
					{
						let on_bar = s.iter().any(|t| {
							t.present
								&& t.id
									== format!(
										"text-fit:log-filter-{key}:{chip}"
									)
						});
						if on_bar {
							continue;
						}
						click_probed(model, cx, "log-filter-more");
						let clear = format!("log-filter-{key}-clear");
						let drawn = model.read_with(cx, |m, _| {
							m.probes.as_ref().unwrap().bounds(&clear).is_some()
						});
						if !drawn {
							bad.push(format!(
								"{name}: the {key} filter is on neither the bar nor the more menu"
							));
						}
						click_probed(model, cx, "log-filter-more");
					}
				});
				for role in FitRole::ALL {
					if matrix_cell(role) == name
						&& !roles.contains(&format!("{role:?}"))
					{
						bad.push(format!(
							"{name}: {role:?} is named to this cell but was never drawn in it"
						));
					}
				}
				assert_clean(bad);
			}

			/// Opens the filter menu `key` (repo, branch, user, date): from
			/// its chip on the bar, else from the "more" chip's menu.
			fn open_menu(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				key: &str,
			) {
				if !click_if_probed(model, cx, &format!("log-filter-{key}")) {
					click_probed(model, cx, "log-filter-more");
					click_probed(model, cx, &format!("log-filter-more:{key}"));
				}
			}

			macro_rules! fit_chips_cells {
				($($name:ident: ($w:expr, $h:expr)),* $(,)?) => {
					const FIT_CHIPS_CELLS: &[&str] = &[$(stringify!($name)),*];
					$(
						#[gpui::test]
						fn $name(cx: &mut TestAppContext) {
							chips_cell(cx, stringify!($name), $w, $h);
						}
					)*
				};
			}

			fit_chips_cells! {
				fit_chips_w0900: (900., 600.),
				fit_chips_w1080: (1080., 720.),
				fit_chips_w1440: (1440., 900.),
			}

			/// The axes the row matrix crosses, as cell names.
			#[test]
			fn fit_matrix_cells_cover_every_axis_combination() {
				let mut want = BTreeSet::new();
				for w in ["0900", "1080", "1440"] {
					for hash in ["off", "on"] {
						for panes in ["default", "listonly"] {
							want.insert(format!(
								"fit_rows_w{w}_hash_{hash}_panes_{panes}"
							));
						}
					}
				}
				let have: BTreeSet<String> =
					FIT_ROWS_CELLS.iter().map(|c| c.to_string()).collect();
				assert_eq!(have, want);
				assert_eq!(
					FIT_ROWS_CELLS.len(),
					want.len(),
					"a cell is listed twice"
				);
			}

			/// Every role is named to a cell that exists; the cell itself
			/// checks that it drew it.
			#[test]
			fn every_fit_role_names_the_cell_that_draws_it() {
				let cells: BTreeSet<&str> = FIT_ROWS_CELLS
					.iter()
					.chain(FIT_CHIPS_CELLS)
					.copied()
					.collect();
				for role in FitRole::ALL {
					assert!(
						cells.contains(matrix_cell(role)),
						"{role:?} is named to {}, which is no cell",
						matrix_cell(role)
					);
				}
			}

			// ───────── the resize paths (M3) ─────────

			/// (id, present, shown, clipped) of every text: what two ways to
			/// a width must agree on.
			fn shape(s: &[Seen]) -> BTreeMap<String, (bool, usize, bool)> {
				s.iter()
					.map(|t| (t.id.clone(), (t.present, t.shown, t.clipped)))
					.collect()
			}

			fn assert_same(
				tag: &str,
				resized: &BTreeMap<String, (bool, usize, bool)>,
				fresh: &BTreeMap<String, (bool, usize, bool)>,
			) {
				let mut bad = Vec::new();
				for id in
					resized.keys().chain(fresh.keys()).collect::<BTreeSet<_>>()
				{
					let (a, b) = (resized.get(id), fresh.get(id));
					if a != b {
						bad.push(format!(
							"{tag}: {id}: resized {a:?} vs fresh {b:?} (present, shown, clipped)"
						));
					}
				}
				assert_clean(bad);
			}

			/// A fresh window at `w`x`h`, as the row matrix opens one.
			fn fresh_rows(
				cx: &mut TestAppContext,
				ws: &Path,
				fx: &FitFixture,
				w: f32,
				h: f32,
				hash: bool,
			) -> Option<Vec<Seen>> {
				let (model, cx) =
					open_rows(cx, ws, fx, w, h, hash, Panes::Default)?;
				Some(snapshot(&model, cx))
			}

			/// I5: what is drawn at a width is drawn at every wider one.
			/// The author column is left out: the labels come before it, so
			/// at 900 it may be there while at 950 the labels took its room
			/// (ADR 0005). So is the collapsed chip, which gives way to the
			/// labels that come back as the list widens.
			fn assert_monotone(
				narrow: (f32, &[Seen]),
				wide: (f32, &[Seen]),
				bad: &mut Vec<String>,
			) {
				for t in narrow.1.iter().filter(|t| {
					t.present
						&& t.row.is_some() && !matches!(
						t.role,
						FitRole::Author | FitRole::Collapsed
					)
				}) {
					let there = wide.1.iter().find(|u| u.id == t.id);
					if !there.is_some_and(|u| u.present) {
						bad.push(format!(
							"I5: {} is drawn at {} but not at {}",
							t.describe(),
							narrow.0,
							wide.0
						));
					}
				}
			}

			fn resize_walk(cx: &mut TestAppContext, hash: bool) {
				let ws = tempfile::tempdir().unwrap();
				let fx = fit_fixture(ws.path());
				let sizes = [(1440., 900.), (1080., 720.), (900., 600.)];
				let mut reference = Vec::new();
				for (w, h) in sizes {
					let Some(s) = fresh_rows(cx, ws.path(), &fx, w, h, hash)
					else {
						return;
					};
					reference.push((w, s));
				}
				let mut bad = Vec::new();
				for pair in reference.windows(2) {
					assert_monotone(
						(pair[1].0, &pair[1].1),
						(pair[0].0, &pair[0].1),
						&mut bad,
					);
				}
				assert_monotone(
					(reference[2].0, &reference[2].1),
					(reference[0].0, &reference[0].1),
					&mut bad,
				);
				assert_clean(bad);
				let reference: BTreeMap<u32, _> = reference
					.iter()
					.map(|(w, s)| (*w as u32, shape(s)))
					.collect();
				let Some((model, cx)) = open_rows(
					cx,
					ws.path(),
					&fx,
					1440.,
					900.,
					hash,
					Panes::Default,
				) else {
					return;
				};
				for (w, h) in
					[(1080., 720.), (900., 600.), (1080., 720.), (1440., 900.)]
				{
					cx.simulate_resize(gpui::size(gpui::px(w), gpui::px(h)));
					settle(cx);
					assert_same(
						&format!("walk to {w} hash={hash}"),
						&shape(&snapshot(&model, cx)),
						&reference[&(w as u32)],
					);
				}
			}

			#[gpui::test]
			fn fit_rows_follow_a_resize_walk_hash_off(cx: &mut TestAppContext) {
				resize_walk(cx, false);
			}

			#[gpui::test]
			fn fit_rows_follow_a_resize_walk_hash_on(cx: &mut TestAppContext) {
				resize_walk(cx, true);
			}

			/// 1080 to 1600 crosses the 1200 the panes' widths change at.
			fn widening(cx: &mut TestAppContext, hash: bool) {
				let ws = tempfile::tempdir().unwrap();
				let fx = fit_fixture(ws.path());
				let Some(fresh) =
					fresh_rows(cx, ws.path(), &fx, 1600., 900., hash)
				else {
					return;
				};
				let Some((model, cx)) = open_rows(
					cx,
					ws.path(),
					&fx,
					1080.,
					720.,
					hash,
					Panes::Default,
				) else {
					return;
				};
				cx.simulate_resize(gpui::size(gpui::px(1600.), gpui::px(900.)));
				settle(cx);
				assert_same(
					&format!("widen 1080 to 1600 hash={hash}"),
					&shape(&snapshot(&model, cx)),
					&shape(&fresh),
				);
			}

			#[gpui::test]
			fn fit_rows_follow_a_widening_1080_to_1600_hash_off(
				cx: &mut TestAppContext,
			) {
				widening(cx, false);
			}

			#[gpui::test]
			fn fit_rows_follow_a_widening_1080_to_1600_hash_on(
				cx: &mut TestAppContext,
			) {
				widening(cx, true);
			}
		}

		/// A merge lists the files changed against any parent. Those the
		/// first parent has as well are marked, and the marked set is keyed
		/// by the merge (#188).
		#[gpui::test]
		fn a_merge_commit_marks_files_unchanged_against_the_first_parent(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "m", &[]);
			git(&r, &["checkout", "-q", "-b", "side"]);
			fs::write(r.join("side.txt"), "s").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "side"]);
			git(&r, &["checkout", "-q", "main"]);
			fs::write(r.join("main.txt"), "m").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "main"]);
			git(&r, &["merge", "-q", "--no-ff", "side", "-m", "merge side"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1600., 800.);
			settle_until(cx, "the log rows", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 4)
			});
			let merge = cx.debug_bounds("log-subject:0").expect("merge row");
			cx.simulate_click(merge.center(), gpui::Modifiers::none());
			settle_until(cx, "the merge's files", |cx| {
				model.read_with(cx, |m, _| {
					m.commit_view.commit_files.len() == 2
						&& m.commit_view
							.commit_files_first_parent_same
							.is_some()
				})
			});
			model.read_with(cx, |m, _| {
				let (_, same) = m
					.commit_view
					.commit_files_first_parent_same
					.as_ref()
					.expect("marker");
				assert!(same.contains("main.txt"), "{same:?}");
				assert!(!same.contains("side.txt"), "{same:?}");
			});
		}

		/// Two repositories in one log: the second one (here `branchy`, all
		/// of whose commits are older) starts on the first lane once the
		/// first one's history has ended (#188).
		#[gpui::test]
		fn a_merged_log_starts_the_second_repo_on_the_first_lane(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let branchy = ws.path().join("branchy");
			fs::create_dir(&branchy).unwrap();
			git(&branchy, &["init", "-q", "-b", "main"]);
			fs::write(branchy.join("base.txt"), "b").unwrap();
			git(&branchy, &["add", "."]);
			git_dated(
				&branchy,
				&["commit", "-q", "-m", "branchy root"],
				"2020-01-01T00:00:00Z",
			);
			fs::write(branchy.join("two.txt"), "b").unwrap();
			git(&branchy, &["add", "."]);
			git_dated(
				&branchy,
				&["commit", "-q", "-m", "branchy two"],
				"2020-01-02T00:00:00Z",
			);
			let alpha = repo(ws.path(), "alpha", &[]);
			fs::write(alpha.join("two.txt"), "a").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha two"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1600., 800.);
			settle_until(cx, "both repositories", |cx| {
				model.read_with(cx, |m, _| {
					m.log_feeds.len() == 2
						&& m.graph_layout
							.as_ref()
							.is_some_and(|l| l.rows.len() >= 4)
				})
			});
			model.read_with(cx, |m, _| {
				let feed = m
					.log_feeds
					.iter()
					.position(|f| f.name == "branchy")
					.expect("branchy feed");
				let layout = m.graph_layout.as_ref().expect("layout");
				let row = layout
					.rows
					.iter()
					.find(|r| {
						crate::multi_log::split_id(&r.sha).1 == Some(feed)
					})
					.expect("a branchy row");
				assert_eq!(
					row.node.lane, 0,
					"branchy starts at lane {}",
					row.node.lane
				);
			});
		}

		#[cfg(unix)]
		#[gpui::test]
		fn merged_log_same_name_repos_get_distinct_ids_and_filters(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let real = tmp.path().join("real");
			fs::create_dir_all(real.join("x/svc")).unwrap();
			let x = repo(&real.join("x/svc"), "app", &[]);
			fs::create_dir_all(real.join("y/svc")).unwrap();
			git(
				&real.join("y/svc"),
				&["clone", "-q", x.to_str().unwrap(), "app"],
			);
			std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();

			let out = Command::new("git")
				.current_dir(&x)
				.args(["rev-parse", "HEAD"])
				.output()
				.expect("git rev-parse HEAD");
			assert!(out.status.success(), "git rev-parse HEAD: {out:?}");
			let head = String::from_utf8(out.stdout).expect("utf8");
			let sha7 = head.trim()[..7].to_string();

			let (model, cx) = open(cx, tmp.path().join("link"), None);
			model.update(cx, |m, _| {
				m.probes = Some(crate::ui::Probes::for_test());
			});
			for _ in 0..2 {
				// ALLOWED-TEST-REFRESH: test harness needs a redraw to populate Probes::for_test() after opening workspace
				cx.update(|w, _| w.refresh());
				settle(cx);
			}

			let mut fails: Vec<String> = Vec::new();

			let (repo_names, feed_names, is_merged, feeds_len, ids) = model
				.read_with(cx, |m, _| {
					let repo_names: Vec<String> =
						m.repos.iter().map(|r| r.name.clone()).collect();
					let feed_names: Vec<String> =
						m.log_feeds.iter().map(|f| f.name.clone()).collect();
					let is_merged = m.log_is_merged();
					let feeds_len = m.log_feeds.len();
					let ids = m.probes.as_ref().unwrap().drawn();
					(repo_names, feed_names, is_merged, feeds_len, ids)
				});

			let rel_drawn_ids: Vec<String> = ids
				.iter()
				.filter(|id| {
					id.starts_with("root-stripe:")
						|| id.starts_with("commit-row:")
						|| id.starts_with("log-repo:")
				})
				.cloned()
				.collect();

			// (a) m.log_is_merged(), m.log_feeds.len() == 2, m.log_feeds[0].name != m.log_feeds[1].name,
			// 而且 m.repos 的名稱兩兩不同。
			if !is_merged {
				fails.push(format!(
					"(a) expected log_is_merged() == true; repos={repo_names:?}, feeds={feed_names:?}, drawn={rel_drawn_ids:?}"
				));
			}
			if feeds_len != 2 {
				fails.push(format!(
					"(a) expected 2 log_feeds, found {feeds_len}; repos={repo_names:?}, feeds={feed_names:?}, drawn={rel_drawn_ids:?}"
				));
			}
			if feeds_len >= 2 && feed_names[0] == feed_names[1] {
				fails.push(format!(
					"(a) expected distinct log_feed names, but both are '{}'; repos={repo_names:?}, feeds={feed_names:?}, drawn={rel_drawn_ids:?}",
					feed_names[0]
				));
			}
			let mut unique_repos = repo_names.clone();
			unique_repos.sort();
			unique_repos.dedup();
			if unique_repos.len() != repo_names.len() {
				fails.push(format!(
					"(a) expected pairwise distinct repo names, but found duplicates: {repo_names:?}; feeds={feed_names:?}, drawn={rel_drawn_ids:?}"
				));
			}

			// (b) probe ids
			let sha_suffix = format!(":{sha7}");
			let root_stripes: Vec<_> = ids
				.iter()
				.filter(|id| {
					id.starts_with("root-stripe:") && id.ends_with(&sha_suffix)
				})
				.cloned()
				.collect();
			if root_stripes.len() != 2 {
				fails.push(format!(
					"(b) expected 2 root-stripe IDs ending with '{sha_suffix}', found {}: {root_stripes:?}; repos={repo_names:?}, feeds={feed_names:?}, drawn={rel_drawn_ids:?}",
					root_stripes.len()
				));
			}

			let commit_rows: Vec<_> = ids
				.iter()
				.filter(|id| {
					if let Some(rest) = id.strip_prefix("commit-row:") {
						rest.contains(':') && id.ends_with(&sha_suffix)
					} else {
						false
					}
				})
				.cloned()
				.collect();
			if commit_rows.len() != 2 {
				fails.push(format!(
					"(b) expected 2 commit-row IDs with repo prefix ending with '{sha_suffix}', found {}: {commit_rows:?}; repos={repo_names:?}, feeds={feed_names:?}, drawn={rel_drawn_ids:?}",
					commit_rows.len()
				));
			}

			// (c) 從 m.repos 找出 root.ends_with(x/svc/app)，toggle_log_path，settle，檢查 feeds
			let repo_x = model.read_with(cx, |m, _| {
				m.repos
					.iter()
					.find(|r| {
						r.root.ends_with(std::path::Path::new("x/svc/app"))
					})
					.map(|r| (r.name.clone(), r.root.clone()))
			});

			if let Some((name_x, root_x)) = repo_x {
				model.update(cx, |m, cx| {
					m.toggle_log_path(format!("{name_x}/base.txt"), cx);
				});
				settle(cx);

				model.read_with(cx, |m, _| {
					let cur_feed_names: Vec<String> =
						m.log_feeds.iter().map(|f| f.name.clone()).collect();
					let cur_repo_names: Vec<String> =
						m.repos.iter().map(|r| r.name.clone()).collect();
					let cur_drawn = m
						.probes
						.as_ref()
						.map(|p| p.drawn())
						.unwrap_or_default();
					let cur_rel_ids: Vec<String> = cur_drawn
						.iter()
						.filter(|id| {
							id.starts_with("root-stripe:")
								|| id.starts_with("commit-row:")
								|| id.starts_with("log-repo:")
						})
						.cloned()
						.collect();

					if m.log_feeds.len() != 1 {
						fails.push(format!(
							"(c) expected log_feeds.len() == 1, found {}; repos={cur_repo_names:?}, feeds={cur_feed_names:?}, paths={:?}, drawn={cur_rel_ids:?}",
							m.log_feeds.len(),
							m.log_feeds.iter().map(|f| &f.paths).collect::<Vec<_>>(),
						));
					}
					if m.log_feeds.first().map(|f| &f.root) != Some(&root_x) {
						fails.push(format!(
							"(c) expected log_feeds[0].root == {root_x:?}, found {:?}; repos={cur_repo_names:?}, feeds={cur_feed_names:?}, drawn={cur_rel_ids:?}",
							m.log_feeds.first().map(|f| &f.root),
						));
					}
					let expected_paths = vec!["base.txt".to_string()];
					if m.log_feeds.first().map(|f| &f.paths) != Some(&expected_paths) {
						fails.push(format!(
							"(c) expected log_feeds[0].paths == [\"base.txt\"], found {:?}; repos={cur_repo_names:?}, feeds={cur_feed_names:?}, drawn={cur_rel_ids:?}",
							m.log_feeds.first().map(|f| &f.paths),
						));
					}
				});
			} else {
				fails.push(format!(
					"(c) could not find repo ending with 'x/svc/app'; repos={repo_names:?}, feeds={feed_names:?}, drawn={rel_drawn_ids:?}"
				));
			}

			assert!(fails.is_empty(), "{fails:#?}");
		}

		#[gpui::test]
		fn alt_1_and_alt_0_switch_between_project_and_changes(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let tab = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| (m.active_tab, m.left_visible))
			};
			assert_eq!(tab(cx), (WorkbenchTab::GitChanges, true));
			cx.simulate_keystrokes("alt-1");
			assert_eq!(tab(cx), (WorkbenchTab::FileExplorer, true));
			cx.simulate_keystrokes("alt-0");
			assert_eq!(tab(cx), (WorkbenchTab::GitChanges, true));
		}

		#[gpui::test]
		fn ctrl_r_lists_a_file_created_after_the_load(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			let repo = repo(ws.path(), "alpha", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			assert!(model.read_with(cx, |m, _| m.files.is_empty()));
			fs::write(repo.join("later.txt"), "late").unwrap();
			cx.simulate_keystrokes("ctrl-r");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert!(m.changes_loaded);
				let paths: Vec<&str> =
					m.files.iter().map(|f| f.path.as_str()).collect();
				assert_eq!(paths, ["later.txt"]);
				let summary = m.repos[0].summary.as_ref().unwrap();
				assert_eq!(summary.changes.untracked, 1);
			});
		}

		#[gpui::test]
		fn ctrl_r_releases_the_open_repository_once_its_directory_is_gone(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			repo(ws.path(), "beta", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let gone = model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 2);
				m.repos[m.selected_repo_idx.unwrap()].root.clone()
			});
			fs::remove_dir_all(&gone).unwrap();
			cx.simulate_keystrokes("ctrl-r");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert_eq!(m.repos.len(), 1);
				assert_ne!(m.repos[0].root, gone);
				assert!(m.selected_repo_idx.is_none());
				assert!(m.file_tree.is_none());
			});
		}

		/// The Copy item of a context menu: its targets, or None when the
		/// item is shown disabled.
		fn copy_of(
			menu: Vec<crate::menu::MenuEntry>,
		) -> Option<Vec<crate::menu::CopyTarget>> {
			let act = menu
				.into_iter()
				.find_map(|e| match e {
					crate::menu::MenuEntry::Item {
						id: "copy-files",
						act,
						..
					} => Some(act),
					_ => None,
				})
				.expect("the menu offers copy-files");
			act.map(|act| match act {
				crate::menu::MenuAct::CopyNode(targets) => targets,
				other => panic!("copy-files runs {other:?}"),
			})
		}

		/// The clipboard's snip-sync entries as (path, content), in payload
		/// order.
		fn clipboard_entries() -> Vec<(String, String)> {
			parse_clipboard(&clip::read_text().unwrap(), "")
				.into_iter()
				.map(|e| (e.path, e.content))
				.collect()
		}

		/// Clicks a node's Copy and returns what landed on the clipboard.
		fn run_copy(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			targets: Vec<crate::menu::CopyTarget>,
		) -> Vec<(String, String)> {
			cx.update(|window, cx| {
				model.update(cx, |m, cx| {
					m.run_menu_act(
						crate::menu::MenuAct::CopyNode(targets),
						window,
						cx,
					)
				})
			});
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "status_copied", "{}", m.status)
			});
			clipboard_entries()
		}

		fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
			pairs
				.iter()
				.map(|(p, c)| (p.to_string(), c.to_string()))
				.collect()
		}

		/// A repo whose `tracked` files are committed and then rewritten, so
		/// each is an Unstaged change with the new body.
		fn modified_repo(
			ws: &Path,
			name: &str,
			tracked: &[(&str, &str)],
		) -> PathBuf {
			let root = repo(ws, name, &[]);
			for (rel, _) in tracked {
				let path = root.join(rel);
				fs::create_dir_all(path.parent().unwrap()).unwrap();
				fs::write(path, "old").unwrap();
			}
			git(&root, &["add", "."]);
			git(&root, &["commit", "-q", "-m", "tracked"]);
			for (rel, body) in tracked {
				fs::write(root.join(rel), body).unwrap();
			}
			root
		}

		fn change_idx(
			m: &WorkbenchModel,
			path: &str,
			source: &snip_core::transfer::SourceKind,
		) -> usize {
			m.files
				.iter()
				.position(|f| f.path == path && &f.source == source)
				.unwrap_or_else(|| panic!("no {source:?} row {path}"))
		}

		/// A file row copies that row only, with its own source: the
		/// staged row the index bytes, the unstaged row the working bytes.
		#[gpui::test]
		fn copy_on_a_change_row_reads_that_rows_source(
			cx: &mut TestAppContext,
		) {
			use snip_core::transfer::SourceKind;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "alpha", &[]);
			fs::write(root.join("base.txt"), "staged").unwrap();
			git(&root, &["add", "base.txt"]);
			fs::write(root.join("base.txt"), "working").unwrap();
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let (staged, unstaged) = model.read_with(cx, |m, _| {
				let staged = change_idx(m, "base.txt", &SourceKind::Staged);
				let unstaged = change_idx(m, "base.txt", &SourceKind::Unstaged);
				(
					copy_of(m.change_row_menu(staged)).unwrap(),
					copy_of(m.change_row_menu(unstaged)).unwrap(),
				)
			});
			assert_eq!(staged.len(), 1);
			assert_eq!(staged[0].source, SourceKind::Staged);
			assert_eq!(
				run_copy(&model, cx, staged),
				entries(&[("base.txt", "staged")])
			);
			assert_eq!(
				run_copy(&model, cx, unstaged),
				entries(&[("base.txt", "working")])
			);
		}

		/// A directory, repo or group row copies every file beneath it,
		/// in list order, and nothing else.
		#[gpui::test]
		fn copy_on_a_dir_repo_or_group_covers_the_files_beneath(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			modified_repo(
				ws.path(),
				"alpha",
				&[
					("src/a.txt", "A"),
					("src/sub/b.txt", "B"),
					("srcx.txt", "X"),
					("top.txt", "T"),
				],
			);
			modified_repo(ws.path(), "beta", &[("other.txt", "O")]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			cx.run_until_parked();
			let (dir, repo_row, group) = model.read_with(cx, |m, _| {
				let alpha = m
					.change_repos
					.iter()
					.position(|s| s.name == "alpha")
					.unwrap();
				(
					copy_of(m.change_dir_menu(alpha, "unstaged", "src"))
						.unwrap(),
					copy_of(m.change_repo_menu(alpha, "unstaged")).unwrap(),
					copy_of(m.change_group_menu("unstaged")).unwrap(),
				)
			});
			assert_eq!(
				run_copy(&model, cx, dir),
				entries(&[("src/a.txt", "A"), ("src/sub/b.txt", "B")])
			);
			assert_eq!(
				run_copy(&model, cx, repo_row),
				entries(&[
					("src/a.txt", "A"),
					("src/sub/b.txt", "B"),
					("srcx.txt", "X"),
					("top.txt", "T"),
				])
			);
			// The exporter names other repos' files by repo; the open
			// repo's stay bare.
			assert_eq!(
				model.read_with(cx, |m, _| m.repo().unwrap().name.clone()),
				"alpha"
			);
			assert_eq!(
				run_copy(&model, cx, group),
				entries(&[
					("src/a.txt", "A"),
					("src/sub/b.txt", "B"),
					("srcx.txt", "X"),
					("top.txt", "T"),
					("beta/other.txt", "O"),
				])
			);
			// Nothing is staged: that group has nothing to copy.
			model.read_with(cx, |m, _| {
				assert!(copy_of(m.change_group_menu("staged")).is_none());
			});
		}

		/// Ctrl+C copies the node under the Changes cursor: a file row, or
		/// a whole group from its header.
		#[gpui::test]
		fn ctrl_c_copies_the_change_under_the_cursor(cx: &mut TestAppContext) {
			use crate::ui::ChangeItemRow;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(
				ws.path(),
				"alpha",
				&[("new0.txt", "hello"), ("new1.txt", "x")],
			);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let row_of = |m: &WorkbenchModel, want: &str| {
				m.change_item_rows()
					.iter()
					.position(|r| match r {
						ChangeItemRow::File { file_idx, .. } => {
							m.files[*file_idx].path == want
						}
						ChangeItemRow::Header { group_id, .. } => {
							*group_id == want
						}
						_ => false,
					})
					.unwrap()
			};
			model
				.update(cx, |m, _| m.selected_list_row = row_of(m, "new0.txt"));
			cx.simulate_keystrokes("ctrl-c");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "status_copied", "{}", m.status)
			});
			assert_eq!(clipboard_entries(), entries(&[("new0.txt", "hello")]));
			model
				.update(cx, |m, _| m.selected_list_row = row_of(m, "unstaged"));
			cx.simulate_keystrokes("ctrl-c");
			cx.run_until_parked();
			assert_eq!(
				clipboard_entries(),
				entries(&[("new0.txt", "hello"), ("new1.txt", "x")])
			);
			// The Reader's Ctrl+C with no text selected copies that node too.
			let idx = model.read_with(cx, |m, _| {
				m.files.iter().position(|f| f.path == "new1.txt").unwrap()
			});
			model.update(cx, |m, cx| m.select_change(idx, cx));
			settle(cx);
			cx.update(|window, cx| {
				model.update(cx, |m, _| {
					assert!(m.preview.is_some());
					m.selected_list_row = row_of(m, "new0.txt");
					window.focus(&m.reader_focus);
				})
			});
			clip::write_text("other").unwrap();
			cx.simulate_keystrokes("ctrl-c");
			cx.run_until_parked();
			assert_eq!(clipboard_entries(), entries(&[("new0.txt", "hello")]));
		}

		/// Loads `idx`'s Project tree.
		fn open_repo_tree(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			idx: usize,
		) {
			model.update(cx, |m, cx| m.select_repo(idx, cx));
			for _ in 0..50 {
				settle(cx);
				if model.read_with(cx, |m, _| {
					m.file_tree.as_ref().is_some_and(|t| t.is_loaded)
				}) {
					return;
				}
			}
			panic!("repo {idx} tree never loaded");
		}

		/// Project rows picked with click and Cmd-click copy together from
		/// any selected row's menu and from Ctrl+C; a folder copies the
		/// files under it.
		#[gpui::test]
		fn copy_on_project_rows_copies_the_selection(cx: &mut TestAppContext) {
			use crate::tree::{NodeKey, TreeCommand};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "alpha", &[]);
			fs::create_dir(root.join("dir")).unwrap();
			fs::write(root.join("dir/b.txt"), "B").unwrap();
			fs::write(root.join("dir/c.txt"), "C").unwrap();
			fs::write(root.join("z.txt"), "Z").unwrap();
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			open_repo_tree(&model, cx, 0);
			model.update(cx, |m, cx| {
				m.select_tree_rows_alone(false, &["base.txt".into()], cx);
				m.dispatch_tree(
					Some(TreeCommand::ToggleSelect(NodeKey::from_utf8_rel(
						"dir",
					))),
					cx,
				);
			});
			let targets = model.read_with(cx, |m, _| {
				assert_eq!(m.selected_count(), 2);
				let tree = m.file_tree.as_ref().unwrap();
				let rows = tree.flatten_visible(tree.visible_limit());
				let row = |rel: &str| {
					rows.iter().find(|r| r.rel_path == rel).unwrap().clone()
				};
				assert!(
					copy_of(m.work_row_menu(&row("z.txt"), false)).is_none()
				);
				copy_of(m.work_row_menu(&row("base.txt"), false)).unwrap()
			});
			let want = entries(&[
				("base.txt", "base"),
				("dir/b.txt", "B"),
				("dir/c.txt", "C"),
			]);
			assert_eq!(run_copy(&model, cx, targets), want);
			clip::write_text("other").unwrap();
			cx.simulate_keystrokes("alt-1");
			cx.simulate_keystrokes("ctrl-c");
			cx.run_until_parked();
			assert_eq!(clipboard_entries(), want);
		}

		/// The Project selection is tree state: leaving the repo and coming
		/// back shows the same rows selected; a plain click elsewhere drops
		/// it.
		#[gpui::test]
		fn project_selection_survives_switching_repos(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[("a.txt", "a")]);
			repo(ws.path(), "beta", &[("b.txt", "b")]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let idx = |m: &WorkbenchModel, name: &str| {
				m.repos.iter().position(|r| r.name == name).unwrap()
			};
			let alpha = model.read_with(cx, |m, _| idx(m, "alpha"));
			let beta = model.read_with(cx, |m, _| idx(m, "beta"));
			let selected = |model: &Entity<WorkbenchModel>,
			                cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					m.file_tree.as_ref().unwrap().selected_paths().to_vec()
				})
			};
			open_repo_tree(&model, cx, alpha);
			model.update(cx, |m, cx| {
				m.select_tree_rows_alone(false, &["a.txt".into()], cx)
			});
			open_repo_tree(&model, cx, beta);
			assert!(selected(&model, cx).is_empty());
			open_repo_tree(&model, cx, alpha);
			assert_eq!(selected(&model, cx), ["a.txt"]);
			model.update(cx, |m, cx| {
				m.select_tree_rows_alone(false, &["base.txt".into()], cx)
			});
			assert_eq!(selected(&model, cx), ["base.txt"]);
		}

		/// Rows Cmd-clicked in one repo and then in another copy together,
		/// the way one payload spans repos; a plain click drops the first
		/// repo's rows.
		#[gpui::test]
		fn project_copy_spans_repos_picked_with_cmd_click(
			cx: &mut TestAppContext,
		) {
			use crate::tree::{NodeKey, TreeCommand};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[("a.txt", "A")]);
			repo(ws.path(), "beta", &[("b.txt", "B")]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let idx = |m: &WorkbenchModel, name: &str| {
				m.repos.iter().position(|r| r.name == name).unwrap()
			};
			let alpha = model.read_with(cx, |m, _| idx(m, "alpha"));
			let beta = model.read_with(cx, |m, _| idx(m, "beta"));
			let toggle = |model: &Entity<WorkbenchModel>,
			              cx: &mut VisualTestContext,
			              rel: &str| {
				model.update(cx, |m, cx| {
					m.dispatch_tree(
						Some(TreeCommand::ToggleSelect(
							NodeKey::from_utf8_rel(rel),
						)),
						cx,
					)
				})
			};
			open_repo_tree(&model, cx, alpha);
			toggle(&model, cx, "a.txt");
			open_repo_tree(&model, cx, beta);
			toggle(&model, cx, "b.txt");
			let targets = model.read_with(cx, |m, _| m.project_targets());
			assert_eq!(
				run_copy(&model, cx, targets),
				entries(&[("alpha/a.txt", "A"), ("b.txt", "B")])
			);
			model.update(cx, |m, cx| {
				m.select_tree_rows_alone(false, &["b.txt".into()], cx)
			});
			let targets = model.read_with(cx, |m, _| m.project_targets());
			assert_eq!(
				run_copy(&model, cx, targets),
				entries(&[("b.txt", "B")])
			);
		}

		/// A file of a browsed commit tree copies its bytes at that commit,
		/// not the working copy; a folder row offers no Copy.
		#[gpui::test]
		fn copy_on_a_commit_tree_file_reads_it_at_that_commit(
			cx: &mut TestAppContext,
		) {
			use snip_core::transfer::SourceKind;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "alpha", &[]);
			fs::write(root.join("base.txt"), "changed").unwrap();
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let sha = model.read_with(cx, |m, _| m.commits[0].sha.clone());
			model.update(cx, |m, cx| {
				m.select_commit(&sha, cx);
				m.browse_commit_tree(cx);
			});
			settle(cx);
			let targets = model.read_with(cx, |m, _| {
				assert!(m.rev_tree.is_some());
				assert!(copy_of(m.rev_row_menu("dir", false)).is_none());
				copy_of(m.rev_row_menu("base.txt", true)).unwrap()
			});
			assert!(matches!(
				&targets[0].source,
				SourceKind::Commit { rev } if *rev == sha
			));
			assert_eq!(
				run_copy(&model, cx, targets),
				entries(&[("base.txt", "base")])
			);
		}

		#[gpui::test]
		fn ctrl_v_previews_the_payload_with_overwrite_off(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			fs::write(dest.join("existing.txt"), "keep").unwrap();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(
				&model,
				cx,
				"// FILE: existing.txt\nnew body\n// FILE: fresh.txt\nfresh body\n",
			);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.mapping_ready());
				assert_eq!(plan.destination, dest);
				let mut rows: Vec<(&str, crate::paste::PlannedOp, bool, bool)> =
					plan.items
						.iter()
						.map(|i| {
							(
								i.path.as_str(),
								i.op,
								i.dest_exists,
								i.overwrite_allowed,
							)
						})
						.collect();
				rows.sort_by_key(|r| r.0);
				assert_eq!(
					rows,
					[
						(
							"existing.txt",
							crate::paste::PlannedOp::Overwrite,
							true,
							false
						),
						(
							"fresh.txt",
							crate::paste::PlannedOp::Create,
							false,
							false
						),
					]
				);
				assert_eq!(m.status.key, "status_paste_preview");
			});
			// A preview never writes.
			assert_eq!(
				fs::read_to_string(dest.join("existing.txt")).unwrap(),
				"keep"
			);
			assert!(!dest.join("fresh.txt").exists());
		}

		/// Commit payload on the clipboard format: c1 has a binary (not
		/// copied) file, a new file and a rename; c2 has no files; c3
		/// modifies `base.txt`.
		fn mixed_commit_payload() -> String {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
				NotCopiedReason,
			};
			let file = |path: &str, change, body: Option<&str>| CommitFile {
				path: path.into(),
				old_path: None,
				change,
				content: body.map(Into::into),
				not_copied: None,
			};
			let record = |message: &str, author: &str, files| CommitRecord {
				message: message.into(),
				author_name: author.into(),
				author_email: format!("{author}@example.invalid"),
				author_date: "2026-09-25T12:34:56+00:00".into(),
				files,
			};
			let mut binary = file("img.bin", FileChange::Added, None);
			binary.not_copied = Some(NotCopiedReason::Binary);
			let mut renamed =
				file("dir/new.txt", FileChange::Renamed, Some("moved"));
			renamed.old_path = Some("old.txt".into());
			snip_core::commits::to_clipboard_text(&CommitsPayload {
				commits: vec![
					record(
						"first\n\nbody",
						"ann",
						vec![
							binary,
							file("fresh.txt", FileChange::Added, Some("x")),
							renamed,
						],
					),
					record("empty one", "bob", Vec::new()),
					record(
						"third",
						"cy",
						vec![file("base.txt", FileChange::Modified, Some("y"))],
					),
				],
			})
		}

		#[gpui::test]
		fn commit_preview_renders_every_commit_header_and_folds(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[("old.txt", "old")]);
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			model.update(cx, |m, _| {
				m.probes = Some(crate::ui::Probes::for_test())
			});
			paste(&model, cx, &mixed_commit_payload());
			settle(cx);
			let drawn = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| m.probes.as_ref().unwrap().drawn())
			};
			let ids = drawn(cx);
			// The empty commit still gets its header, and the summary
			// counts commits apart from file actions.
			for id in [
				"paste-commit:0",
				"paste-commit:1",
				"paste-commit:2",
				"paste-commit-count",
				"paste-row:3:img.bin",
				"paste-row:4:base.txt",
			] {
				assert!(ids.contains(&id.to_string()), "{id} in {ids:?}");
			}
			// The header strings come from `commit_header_labels`, the
			// helper the header draws them with (the probes only prove the
			// header exists): subject, author and date of the empty commit.
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				let commits =
					&plan.commit_preview.as_ref().unwrap().plan().commits;
				assert_eq!(
					crate::ui::commit_header_labels(
						&commits[1],
						crate::i18n::Locale::En
					),
					(
						"empty one".to_string(),
						"bob <bob@example.invalid>".to_string(),
						"2026-09-25 12:34".to_string()
					)
				);
				assert!(plan.items.iter().all(|i| i.commit != Some(1)));
				// Header counts agree with the rows under them.
				assert_eq!(plan.commit_counts(0), (4, 1));
				assert_eq!(plan.commit_counts(1), (0, 0));
				assert_eq!(plan.commit_counts(2), (1, 0));
			});
			// Folding commit #1 removes its rows but keeps every header.
			model.update(cx, |m, cx| m.toggle_paste_commit(0, cx));
			settle(cx);
			let ids = drawn(cx);
			assert!(ids.contains(&"paste-commit:0".to_string()));
			assert!(
				!ids.contains(&"paste-row:3:img.bin".to_string()),
				"{ids:?}"
			);
			assert!(ids.contains(&"paste-row:4:base.txt".to_string()));
		}

		#[gpui::test]
		fn delete_notice_renders_only_for_a_delete_that_will_happen(
			cx: &mut TestAppContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			// `present.txt` exists at the destination, `missing.txt` does not.
			let dest = repo(&root, "dest", &[("present.txt", "x")]);
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			model.update(cx, |m, _| {
				m.probes = Some(crate::ui::Probes::for_test())
			});
			let gone = |path: &str| CommitFile {
				path: path.into(),
				old_path: None,
				change: FileChange::Deleted,
				content: None,
				not_copied: None,
			};
			paste(
				&model,
				cx,
				&snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "drop".into(),
						author_name: "ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-09-25T12:34:56+00:00".into(),
						files: vec![gone("missing.txt"), gone("present.txt")],
					}],
				}),
			);
			settle(cx);
			let idx = |cx: &mut VisualTestContext, path: &str| {
				model.read_with(cx, |m, _| {
					m.paste
						.plan()
						.unwrap()
						.items
						.iter()
						.position(|i| i.path == path)
						.unwrap_or_else(|| panic!("no row for {path}"))
				})
			};
			let notice = |cx: &mut VisualTestContext| {
				model
					.read_with(cx, |m, _| m.probes.as_ref().unwrap().drawn())
					.contains(&"paste-delete-notice".to_string())
			};
			let missing = idx(cx, "missing.txt");
			let present = idx(cx, "present.txt");

			// Destination already lacks the file: nothing will be deleted,
			// so no red notice.
			model.update(cx, |m, cx| m.select_paste_item(missing, cx));
			settle(cx);
			assert!(!notice(cx), "notice on a delete of a missing file");

			model.update(cx, |m, cx| m.select_paste_item(present, cx));
			settle(cx);
			assert!(notice(cx), "no notice on a real delete");

			// Excluding the row means it will not be deleted either.
			model.update(cx, |m, cx| m.toggle_paste_selected(present, cx));
			settle(cx);
			assert!(!notice(cx), "notice on an excluded delete");
		}

		#[gpui::test]
		fn folding_the_selected_commit_moves_selection_off_hidden_rows(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[("old.txt", "old")]);
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, &mixed_commit_payload());
			model.update(cx, |m, cx| {
				m.select_paste_item(0, cx);
				m.toggle_paste_commit(0, cx);
			});
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.items[plan.selected_item_idx].path, "base.txt");
				assert!(plan.display_order().contains(&plan.selected_item_idx));
				// Space acted on the visible row only: the folded commit's
				// files stay included and no subset error is raised.
				assert!(plan
					.items
					.iter()
					.filter(|i| i.commit == Some(0))
					.all(|i| i.selected));
				assert!(plan.error.is_none());
			});
		}

		#[gpui::test]
		fn nav_up_and_down_move_paste_selection_and_detail(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[("old.txt", "old")]);
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, &mixed_commit_payload());

			let (initial_idx, initial_path, order) =
				model.read_with(cx, |m, _| {
					let plan = m.paste.plan().unwrap();
					let order = plan.display_order();
					(
						plan.selected_item_idx,
						m.paste.detail().and_then(|d| d.path.clone()),
						order,
					)
				});
			assert_eq!(initial_idx, order[0]);
			assert_eq!(initial_path.as_deref(), Some("old.txt"));

			cx.simulate_keystrokes("down");
			cx.run_until_parked();

			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, order[1]);
				assert_eq!(
					m.paste.detail().and_then(|d| d.path.as_deref()),
					Some("dir/new.txt")
				);
			});

			cx.simulate_keystrokes("up");
			cx.run_until_parked();

			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, order[0]);
				assert_eq!(
					m.paste.detail().and_then(|d| d.path.as_deref()),
					Some("old.txt")
				);
			});
		}

		#[gpui::test]
		fn space_on_a_skip_row_never_flips_the_hidden_overwrite(
			cx: &mut TestAppContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[("old.txt", "old")]);
			// A non-UTF-8 file is never overwritten: the replay plans a
			// SKIP for it although it exists at the destination.
			fs::write(dest.join("img.bin"), [0xff, 0xfe, 0x00]).unwrap();
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "bin".into(),
						author_name: "ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-09-25T12:34:56+00:00".into(),
						files: vec![CommitFile {
							path: "img.bin".into(),
							old_path: None,
							change: FileChange::Modified,
							content: Some("text".into()),
							not_copied: None,
						}],
					}],
				});
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, &payload);
			let before = model.update(cx, |m, cx| {
				m.select_paste_item(0, cx);
				let img = &m.paste.plan().unwrap().items[0];
				assert_eq!(img.path, "img.bin");
				assert!(
					img.dest_exists
						&& matches!(img.op, crate::paste::PlannedOp::Skip(_))
				);
				assert!(!img.overwritable());
				img.overwrite_allowed
			});
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				let img = &m.paste.plan().unwrap().items[0];
				assert_eq!(img.overwrite_allowed, before);
			});
		}

		#[gpui::test]
		fn space_reinclude_clears_the_commit_subset_banner(
			cx: &mut TestAppContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "msg\n".into(),
						author_name: "ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-09-25T12:34:56+00:00".into(),
						files: vec![CommitFile {
							path: "fresh.txt".into(),
							old_path: None,
							change: FileChange::Added,
							content: Some("body\n".into()),
							not_copied: None,
						}],
					}],
				});
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, &payload);
			model.update(cx, |m, cx| {
				m.select_paste_item(0, cx);
			});

			// Space excludes the row -> plan.error key commit_subset_rejected
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(!plan.items[0].selected);
				assert_eq!(
					plan.error.as_ref().map(|e| e.key),
					Some("commit_subset_rejected")
				);
			});

			// Enter triggers Apply refusal -> sets model.status to commit_subset_rejected
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "commit_subset_rejected");
				assert!(m.paste.plan().is_some());
			});

			// Space again re-includes the row -> plan.error is None,
			// and status key is reset to status_paste_preview.
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan stays open");
				assert!(plan.items[0].selected);
				assert!(plan.error.is_none());
				assert_ne!(m.status.key, "commit_subset_rejected");
				assert_eq!(m.status.key, "status_paste_preview");
			});

			// Enter now succeeds
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			assert_eq!(
				fs::read_to_string(dest.join("fresh.txt")).unwrap(),
				"body\n"
			);
		}

		/// A real Apply error must outlive an exclude / re-include cycle, and
		/// a subset Apply is refused whichever banner is showing.
		#[gpui::test]
		fn space_cycle_keeps_a_real_apply_error_and_subset_apply_is_refused(
			cx: &mut TestAppContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "msg\n".into(),
						author_name: "ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-09-25T12:34:56+00:00".into(),
						files: vec![CommitFile {
							path: "fresh.txt".into(),
							old_path: None,
							change: FileChange::Added,
							content: Some("body\n".into()),
							not_copied: None,
						}],
					}],
				});
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, &payload);
			model.update(cx, |m, cx| {
				m.select_paste_item(0, cx);
			});
			// The target appears behind the preview's back: Apply goes stale.
			fs::write(dest.join("fresh.txt"), "external").unwrap();
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			let keys = |cx: &mut gpui::VisualTestContext| {
				model.read_with(cx, |m, _| {
					(
						m.paste.plan().unwrap().error.as_ref().map(|e| e.key),
						m.status.key,
					)
				})
			};
			assert_eq!(keys(cx), (Some("stale_created"), "stale_created"));

			// Space twice, no Apply between: the stale error is never touched.
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			assert_eq!(keys(cx), (Some("stale_created"), "stale_created"));
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			assert_eq!(keys(cx), (Some("stale_created"), "stale_created"));

			// Exclude, then Enter: the subset Apply is refused, nothing written.
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().is_some());
				assert_eq!(m.status.key, "commit_subset_rejected");
			});
			assert_eq!(
				fs::read_to_string(dest.join("fresh.txt")).unwrap(),
				"external"
			);
		}

		#[gpui::test]
		fn escape_cancels_the_preview_and_writes_nothing(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, "// FILE: fresh.txt\nfresh body\n");
			cx.simulate_keystrokes("escape");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().is_none());
				assert_eq!(m.status.key, "paste_cancelled");
			});
			assert!(!dest.join("fresh.txt").exists());
		}

		#[gpui::test]
		fn enter_applies_the_preview_byte_for_byte(cx: &mut TestAppContext) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(
				&model,
				cx,
				"// FILE: a.txt\nalpha\n// FILE: b.txt\nbeta\n  indented\n",
			);
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			assert_eq!(
				fs::read_to_string(dest.join("a.txt")).unwrap(),
				"alpha"
			);
			assert_eq!(
				fs::read_to_string(dest.join("b.txt")).unwrap(),
				"beta\n  indented"
			);
		}

		#[gpui::test]
		fn enter_refuses_a_stale_destination_and_writes_nothing(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, "// FILE: a.txt\nfrom clipboard\n");
			// The target appears behind the preview's back.
			fs::write(dest.join("a.txt"), "external").unwrap();
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "stale_created", "{}", m.status);
				let plan = m.paste.plan().expect("plan stays open");
				assert!(!plan.is_applying);
				assert_eq!(
					plan.error.as_ref().map(|e| e.key),
					Some("stale_created")
				);
			});
			assert_eq!(
				fs::read_to_string(dest.join("a.txt")).unwrap(),
				"external"
			);
		}

		/// A read-only directory makes one write fail; root ignores the mode,
		/// so the test skips there.
		#[cfg(unix)]
		#[gpui::test]
		fn a_refused_write_does_not_stop_the_others_and_the_card_names_it(
			cx: &mut TestAppContext,
		) {
			use std::os::unix::fs::PermissionsExt;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			let ro = dest.join("ro");
			fs::create_dir(&ro).unwrap();
			fs::set_permissions(&ro, fs::Permissions::from_mode(0o555))
				.unwrap();
			if fs::write(ro.join("probe"), "").is_ok() {
				assert!(
					std::env::var_os("SNIP_REQUIRE_ALL_TESTS").is_none(),
					"read-only directories need a non-root user"
				);
				return;
			}
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			// The root marker keeps `ro/` under the destination without a
			// mapping choice.
			paste(
				&model,
				cx,
				"// clipcode-root: src\n// FILE: ro/x.txt\nblocked\n// FILE: ok.txt\nfine\n",
			);
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			fs::set_permissions(&ro, fs::Permissions::from_mode(0o755))
				.unwrap();
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			assert_eq!(
				fs::read_to_string(dest.join("ok.txt")).unwrap(),
				"fine"
			);
			assert!(!ro.join("x.txt").exists());
			// What the user sees after the rescan: a failure card naming
			// the path, not a status line the rescan has already replaced.
			model.read_with(cx, |m, _| {
				let (_, ok, msg) = m.toast.as_ref().expect("result card");
				assert!(!ok);
				assert_eq!(msg.key, "status_paste_partial");
				assert!(msg.render(m.locale).contains("ro/x.txt"), "{msg:?}");
			});
		}

		/// APFS refuses such names, so Linux only.
		#[cfg(target_os = "linux")]
		#[gpui::test]
		fn a_non_utf8_change_is_left_out_of_the_copy(cx: &mut TestAppContext) {
			use std::os::unix::ffi::OsStrExt;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let repo = repo(ws.path(), "alpha", &[("good.txt", "g")]);
			let bad = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
			fs::write(repo.join(bad), "b").unwrap();
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let group = model.read_with(cx, |m, _| {
				let bad_idx =
					m.files.iter().position(|f| !f.is_valid_utf8()).unwrap();
				assert!(copy_of(m.change_row_menu(bad_idx)).is_none());
				copy_of(m.change_group_menu("unstaged")).unwrap()
			});
			assert_eq!(
				run_copy(&model, cx, group),
				entries(&[("good.txt", "g")])
			);
		}

		/// A paste exported with `clipcode-root` from the folder above the
		/// destination: the first path segment repeats the destination's name.
		const RESTORE_BASE_PAYLOAD: &str =
			"// clipcode-root: mono\n// FILE: sub/a.ts\nx\n";

		/// The restore-base suggestion reaches the panel as a hint row with
		/// [Adjust paths] and [Use as-is]; neither is applied unasked, and the
		/// first Enter after a click still applies the plan (#240).
		#[gpui::test]
		fn restore_base_hint_adjusts_paths_and_the_next_enter_applies(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let sub = repo(ws.path(), "sub", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);
			paste(&model, cx, RESTORE_BASE_PAYLOAD);
			let paths = |model: &Entity<WorkbenchModel>,
			             cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| {
					let plan = m.paste.plan().unwrap();
					(
						plan.items
							.iter()
							.map(|i| i.path.clone())
							.collect::<Vec<_>>(),
						plan.adjust_paths,
					)
				})
			};
			assert!(
				cx.debug_bounds("paste-restore-base").is_some(),
				"no restore-base hint row"
			);
			assert_eq!(
				paths(&model, cx),
				(vec!["sub/a.ts".to_string()], false)
			);

			click(cx, "paste-adjust");
			settle(cx);
			assert_eq!(paths(&model, cx), (vec!["a.ts".to_string()], true));

			click(cx, "paste-as-is");
			settle(cx);
			assert_eq!(
				paths(&model, cx),
				(vec!["sub/a.ts".to_string()], false)
			);

			click(cx, "paste-adjust");
			settle(cx);
			cx.simulate_keystrokes("enter");
			settle(cx);
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			assert_eq!(fs::read_to_string(sub.join("a.ts")).unwrap(), "x");
			assert!(!sub.join("sub").exists());
		}

		/// The hint is worded from the suggestion's data in the interface
		/// language: core's English `label` never reaches a zh-TW screen.
		#[gpui::test]
		fn restore_base_hint_is_worded_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "sub", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);
			paste(&model, cx, RESTORE_BASE_PAYLOAD);
			assert!(cx.debug_bounds("paste-restore-base").is_some());
			let zh = model.read_with(cx, |m, _| {
				assert_eq!(m.locale, crate::i18n::Locale::ZhTw);
				m.paste_hint_text().expect("a hint row")
			});
			assert!(zh.contains("sub/"), "{zh}");
			assert!(zh.contains('路'), "{zh}");
			assert!(!zh.contains("remove the leading"), "{zh}");

			model.update(cx, |m, cx| m.toggle_locale(cx));
			settle(cx);
			let en = model.read_with(cx, |m, _| {
				assert_eq!(m.locale, crate::i18n::Locale::En);
				m.paste_hint_text().expect("a hint row")
			});
			assert!(en.contains("sub/"), "{en}");
			assert!(en.contains("extra"), "{en}");
			assert!(!en.contains('路'), "{en}");
			assert!(cx.debug_bounds("paste-restore-base").is_some());
		}

		/// The worker's suggestion is shown too, adjusting re-plans on the
		/// worker, and its Apply writes the adjusted path (#240).
		#[gpui::test]
		fn remote_paste_shows_the_worker_restore_base_hint_and_adjusts(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let sub = shared.join("sub");
			fs::create_dir_all(&sub).unwrap();
			crate::paste::tests::git_init(&sub);
			fs::write(sub.join("init.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&sub, &["add", "init.txt"]);
			crate::paste::tests::git_run(&sub, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			wait_remote_repo_loaded(&model, cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);

			remote_paste_preview(&model, cx, RESTORE_BASE_PAYLOAD);
			assert!(
				cx.debug_bounds("paste-restore-base").is_some(),
				"no restore-base hint row for the worker's suggestion"
			);
			click(cx, "paste-adjust");
			for _ in 0..300 {
				settle(cx);
				if !model.read_with(cx, |m, _| m.paste.is_loading()) {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}
			assert!(
				!model.read_with(cx, |m, _| m.paste.is_loading()),
				"the adjusted re-plan never landed"
			);
			let paths: Vec<String> = model.read_with(cx, |m, _| {
				m.paste
					.plan()
					.unwrap()
					.items
					.iter()
					.map(|i| i.path.clone())
					.collect()
			});
			assert_eq!(paths, ["a.ts"]);

			remote_paste_apply(&model, cx);
			assert_eq!(fs::read_to_string(sub.join("a.ts")).unwrap(), "x");
			assert!(!sub.join("sub").exists());
		}

		/// Stripping `sub/` makes both items `a.ts`, which the engine
		/// refuses.
		const RESTORE_BASE_COLLIDING_PAYLOAD: &str =
			"// clipcode-root: mono\n// FILE: a.ts\ny\n// FILE: sub/a.ts\nx\n";

		/// An adjusted plan the engine refuses keeps the hint row, so
		/// [Use as-is] can undo the choice (#240).
		#[gpui::test]
		fn a_refused_adjusted_plan_keeps_the_hint_and_as_is_recovers(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "sub", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);
			paste(&model, cx, RESTORE_BASE_COLLIDING_PAYLOAD);
			assert!(cx.debug_bounds("paste-restore-base").is_some());
			click(cx, "paste-adjust");
			settle(cx);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(
					plan.error.as_ref().map(|e| e.key),
					Some("paste_err_plan")
				);
				assert!(plan.adjust_paths);
			});
			assert!(
				cx.debug_bounds("paste-restore-base").is_some(),
				"the hint row vanished after the engine refused the plan"
			);
			click(cx, "paste-as-is");
			settle(cx);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.error.is_none(), "{:?}", plan.error);
				assert!(!plan.adjust_paths);
				let paths: Vec<&str> =
					plan.items.iter().map(|i| i.path.as_str()).collect();
				assert_eq!(paths, ["a.ts", "sub/a.ts"]);
			});
		}

		/// The same on a worker, whose refusal carries its suggestion.
		#[gpui::test]
		fn a_remote_refused_adjusted_plan_keeps_the_hint_and_as_is_recovers(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let sub = shared.join("sub");
			fs::create_dir_all(&sub).unwrap();
			crate::paste::tests::git_init(&sub);
			fs::write(sub.join("init.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&sub, &["add", "init.txt"]);
			crate::paste::tests::git_run(&sub, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			wait_remote_repo_loaded(&model, cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);

			remote_paste_preview(&model, cx, RESTORE_BASE_COLLIDING_PAYLOAD);
			assert!(cx.debug_bounds("paste-restore-base").is_some());
			let wait_landed =
				|model: &Entity<WorkbenchModel>, cx: &mut VisualTestContext| {
					for _ in 0..300 {
						settle(cx);
						if !model.read_with(cx, |m, _| m.paste.is_loading()) {
							break;
						}
						std::thread::sleep(Duration::from_millis(10));
					}
					assert!(
						!model.read_with(cx, |m, _| m.paste.is_loading()),
						"the re-plan never landed"
					);
				};
			click(cx, "paste-adjust");
			wait_landed(&model, cx);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(
					plan.error.as_ref().map(|e| e.key),
					Some("paste_err_plan")
				);
			});
			assert!(
				cx.debug_bounds("paste-restore-base").is_some(),
				"the hint row vanished after the worker refused the plan"
			);
			click(cx, "paste-as-is");
			wait_landed(&model, cx);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.error.is_none(), "{:?}", plan.error);
				assert!(!plan.adjust_paths);
				let paths: Vec<&str> =
					plan.items.iter().map(|i| i.path.as_str()).collect();
				assert_eq!(paths, ["a.ts", "sub/a.ts"]);
			});
		}

		/// The same path twice at the root: the engine refuses the plan as
		/// it stands, before any adjustment is chosen, and has nothing to
		/// suggest (no folder to strip).
		const RESTORE_BASE_REFUSED_PAYLOAD: &str =
			"// clipcode-root: mono\n// FILE: a.ts\nx\n// FILE: a.ts\ny\n";

		/// A first plan the engine refuses without a suggestion opens no
		/// preview, so there is no hint row to leave behind (#240): it says
		/// why on the status line. One that carries a suggestion lands as a
		/// panel instead (`a_new_paste_into_a_refused_folder_...`).
		fn assert_first_plan_refused_without_a_panel(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			let (open, loading, status) = model.read_with(cx, |m, _| {
				(
					m.paste.plan().is_some(),
					m.paste.is_loading(),
					m.status.clone(),
				)
			});
			assert!(!open, "a refused first plan opened a preview");
			assert!(!loading, "the refused first plan is still loading");
			assert_eq!(
				status.key, "paste_err_plan",
				"the status does not say the plan was refused: {status:?}"
			);
			assert!(
				cx.debug_bounds("paste-restore-base").is_none(),
				"a hint row without a preview"
			);
		}

		/// A first plan the engine refuses says the same on a worker as in
		/// the folder itself: neither opens a preview or a hint row.
		#[gpui::test]
		fn a_first_plan_the_engine_refuses_opens_no_preview(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "sub", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);
			clip::write_text(RESTORE_BASE_REFUSED_PAYLOAD).unwrap();
			cx.simulate_keystrokes("ctrl-v");
			settle(cx);
			assert_first_plan_refused_without_a_panel(&model, cx);
		}

		#[gpui::test]
		fn a_remote_first_plan_the_worker_refuses_opens_no_preview(
			cx: &mut TestAppContext,
		) {
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let tmp = tempfile::tempdir().unwrap();
			let shared = tmp.path().join("shared");
			let sub = shared.join("sub");
			fs::create_dir_all(&sub).unwrap();
			crate::paste::tests::git_init(&sub);
			fs::write(sub.join("init.txt"), "hello\n").unwrap();
			crate::paste::tests::git_run(&sub, &["add", "init.txt"]);
			crate::paste::tests::git_run(&sub, &["commit", "-m", "init"]);

			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			wait_remote_repo_loaded(&model, cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);

			clip::write_text(RESTORE_BASE_REFUSED_PAYLOAD).unwrap();
			model.update(cx, |m, cx| m.trigger_paste_preview(cx));
			for _ in 0..300 {
				settle(cx);
				if !model.read_with(cx, |m, _| m.paste.is_loading()) {
					break;
				}
				std::thread::sleep(Duration::from_millis(10));
			}
			assert_first_plan_refused_without_a_panel(&model, cx);
		}

		/// Click the centre of the rendered control `id`, the way a user
		/// does. Fails when the UI renders no such element, so it also pins
		/// the id shape the drivers rely on.
		fn click(cx: &mut VisualTestContext, id: &'static str) {
			cx.run_until_parked();
			let bounds = cx
				.debug_bounds(id)
				.unwrap_or_else(|| panic!("no rendered control {id}"));
			cx.simulate_click(bounds.center(), gpui::Modifiers::none());
			cx.run_until_parked();
		}

		const SRC_TREE: &str = "// clipcode-root: src\n// FILE: src/app/a.ts\nA\n// FILE: src/app/b.ts\nB\n// FILE: src/lib/c.ts\nC\n";

		/// `click` for an id built at run time.
		fn click_dyn(cx: &mut VisualTestContext, id: String) {
			click(cx, Box::leak(id.into_boxed_str()));
		}

		fn paste_ix(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			path: &str,
		) -> usize {
			model.read_with(cx, |m, _| {
				m.paste
					.plan()
					.unwrap()
					.items
					.iter()
					.position(|i| i.path == path)
					.unwrap()
			})
		}

		fn paste_dir(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			path: &str,
		) -> crate::paste::DirKey {
			model.read_with(cx, |m, _| crate::paste::DirKey {
				commit: None,
				root: m.paste.plan().unwrap().items[0].dest_root.clone(),
				path: path.into(),
			})
		}

		fn paste_cursor(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> crate::paste::PasteCursor {
			model.read_with(cx, |m, _| m.paste.plan().unwrap().cursor())
		}

		fn paste_tick(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			path: &str,
		) -> crate::paste::Tick {
			use crate::paste::PasteNode;
			model.read_with(cx, |m, _| {
				m.paste
					.plan()
					.unwrap()
					.tree_rows()
					.into_iter()
					.find_map(|n| match n {
						PasteNode::Root { key, sum, .. }
						| PasteNode::Dir { key, sum, .. }
							if key.path == path =>
						{
							Some(sum.tick())
						}
						_ => None,
					})
					.unwrap_or_else(|| panic!("no folder row {path:?}"))
			})
		}

		fn paste_selected(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) -> Vec<bool> {
			model.read_with(cx, |m, _| {
				m.paste
					.plan()
					.unwrap()
					.items
					.iter()
					.map(|i| i.selected)
					.collect()
			})
		}

		/// #239: folders of the paste preview are rows of their own, driven
		/// by the keyboard and the mouse like the Changes tree.
		#[gpui::test]
		fn paste_folders_fold_with_arrows_and_toggle_with_space_and_clicks(
			cx: &mut TestAppContext,
		) {
			use crate::paste::{PasteCursor, Tick};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, SRC_TREE);
			let a = paste_ix(&model, cx, "src/app/a.ts");
			let row_a = crate::paste::RowControl::Row.id(a, "src/app/a.ts");
			let app = paste_dir(&model, cx, "src/app");

			// A click on the folder row selects it: no file detail.
			click(cx, "paste-dir:0:src/app");
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Dir(app.clone()));
			model.read_with(cx, |m, _| assert!(m.paste.detail().is_none()));

			cx.simulate_keystrokes("left");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().unwrap().collapsed_dirs.contains(&app))
			});
			assert!(cx
				.debug_bounds(Box::leak(row_a.clone().into_boxed_str()))
				.is_none());
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Dir(app.clone()));

			cx.simulate_keystrokes("right");
			cx.run_until_parked();
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().unwrap().collapsed_dirs.is_empty())
			});
			assert!(cx
				.debug_bounds(Box::leak(row_a.clone().into_boxed_str()))
				.is_some());
			cx.simulate_keystrokes("right");
			cx.run_until_parked();
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Item(a));
			cx.simulate_keystrokes("left");
			cx.run_until_parked();
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Dir(app.clone()));

			// Space on a folder takes its files out, then puts them back.
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			assert_eq!(paste_selected(&model, cx), [false, false, true]);
			model.read_with(cx, |m, _| {
				assert_eq!(m.paste.plan().unwrap().counts().creates, 1)
			});
			assert_eq!(paste_tick(&model, cx, "src/app"), Tick::Empty);
			assert_eq!(paste_tick(&model, cx, "src"), Tick::Partial);
			cx.simulate_keystrokes("space");
			cx.run_until_parked();
			assert_eq!(paste_selected(&model, cx), [true, true, true]);

			// The folder checkbox: all out, one file back (partial), then all.
			click(cx, "paste-dir-include:0:src");
			assert_eq!(paste_selected(&model, cx), [false, false, false]);
			click_dyn(
				cx,
				crate::paste::RowControl::Include.id(a, "src/app/a.ts"),
			);
			assert_eq!(paste_tick(&model, cx, "src"), Tick::Partial);
			click(cx, "paste-dir-include:0:src");
			assert_eq!(paste_selected(&model, cx), [true, true, true]);

			// A folder under the cursor does not stop Apply.
			cx.simulate_keystrokes("enter");
			settle(cx);
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			for (path, body) in [
				("src/app/a.ts", "A"),
				("src/app/b.ts", "B"),
				("src/lib/c.ts", "C"),
			] {
				assert_eq!(fs::read_to_string(dest.join(path)).unwrap(), body);
			}
		}

		#[gpui::test]
		fn folder_chevron_folds_and_moves_a_hidden_cursor_onto_the_folder(
			cx: &mut TestAppContext,
		) {
			use crate::paste::PasteCursor;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(dest));
			paste(&model, cx, SRC_TREE);
			let b = paste_ix(&model, cx, "src/app/b.ts");
			let (app, lib) = (
				paste_dir(&model, cx, "src/app"),
				paste_dir(&model, cx, "src/lib"),
			);
			click_dyn(cx, crate::paste::RowControl::Row.id(b, "src/app/b.ts"));
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Item(b));

			click(cx, "paste-dir-toggle:0:src/app");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.collapsed_dirs.contains(&app));
				assert!(plan.items[b].selected, "folding is not a selection");
			});
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Dir(app.clone()));
			model.read_with(cx, |m, _| assert!(m.paste.detail().is_none()));

			click(cx, "paste-dir-toggle:0:src/lib");
			model.read_with(cx, |m, _| {
				assert!(m.paste.plan().unwrap().collapsed_dirs.contains(&lib))
			});
			assert_eq!(paste_cursor(&model, cx), PasteCursor::Dir(app));
		}

		/// While a remap plans again the rows are gone; a folder cursor the
		/// plan keeps for the next landing must not draw the old folder's
		/// detail (title and all-zero counts) over an empty list.
		#[gpui::test]
		fn a_cleared_preview_draws_no_folder_detail(cx: &mut TestAppContext) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(dest));
			paste(&model, cx, SRC_TREE);
			click(cx, "paste-dir:0:src/app");
			assert!(cx.debug_bounds("paste-dir-detail").is_some());

			// A prefix this paste does not have: the remap is refused after
			// the plan was cleared, and the empty plan stays on screen.
			model.update(cx, |m, cx| {
				m.rebuild_paste_plan("nope".into(), None, cx)
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.items.is_empty());
				assert!(plan.cursor_dir.is_some(), "kept for the landing");
			});
			assert!(cx.debug_bounds("paste-dir-detail").is_none());
		}

		/// Two commits that both touch `src/`: a fold of the folder under one
		/// commit must not touch the other's.
		fn shared_folder_commits_payload() -> String {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let added = |path: &str| CommitFile {
				path: path.into(),
				old_path: None,
				change: FileChange::Added,
				content: Some(format!("{path}\n")),
				not_copied: None,
			};
			let record = |message: &str, author: &str, files| CommitRecord {
				message: message.into(),
				author_name: author.into(),
				author_email: format!("{author}@example.invalid"),
				author_date: "2026-09-25T12:34:56+00:00".into(),
				files,
			};
			snip_core::commits::to_clipboard_text(&CommitsPayload {
				commits: vec![
					record(
						"first",
						"ann",
						vec![
							added("src/a.txt"),
							added("src/b.txt"),
							added("top.txt"),
						],
					),
					record("second", "bob", vec![added("src/c.txt")]),
				],
			})
		}

		/// #239: a commit replay shows the same nested folders as a file
		/// paste, and each commit folds its own folders.
		#[gpui::test]
		fn commit_folders_fold_per_commit_and_header_folds_still_work(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(dest));
			paste(&model, cx, &shared_folder_commits_payload());
			let (a, b, top, c) = (
				paste_ix(&model, cx, "src/a.txt"),
				paste_ix(&model, cx, "src/b.txt"),
				paste_ix(&model, cx, "top.txt"),
				paste_ix(&model, cx, "src/c.txt"),
			);
			let row = |ix: usize, path: &str| {
				crate::paste::RowControl::Row.id(ix, path)
			};
			let drawn = |cx: &mut VisualTestContext, id: &str| {
				cx.debug_bounds(Box::leak(id.to_string().into_boxed_str()))
					.is_some()
			};

			// Both commits draw their own `src` folder, rows nested in it.
			cx.run_until_parked();
			for id in ["paste-dir:0:src", "paste-dir:1:src"] {
				assert!(drawn(cx, id), "{id} is drawn");
			}
			assert!(drawn(cx, &row(a, "src/a.txt")));

			// Folding commit #1's `src` leaves commit #2's `src` open.
			click(cx, "paste-dir-toggle:0:src");
			assert!(!drawn(cx, &row(a, "src/a.txt")));
			assert!(!drawn(cx, &row(b, "src/b.txt")));
			assert!(drawn(cx, &row(top, "top.txt")), "outside the folder");
			assert!(drawn(cx, &row(c, "src/c.txt")), "the other commit");
			click(cx, "paste-dir-toggle:1:src");
			assert!(!drawn(cx, &row(c, "src/c.txt")));
			click(cx, "paste-dir-toggle:0:src");
			assert!(drawn(cx, &row(a, "src/a.txt")));
			assert!(!drawn(cx, &row(c, "src/c.txt")), "#2 is still folded");
			click(cx, "paste-dir-toggle:1:src");

			// A folder's include box is locked: a replay is all or nothing.
			click(cx, "paste-dir-include:0:src");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.items.iter().all(|i| i.selected));
				assert!(plan.error.is_none());
			});

			// Folding the folder that holds the selected row moves the
			// selection to a row that is still shown.
			click_dyn(cx, row(a, "src/a.txt"));
			click(cx, "paste-dir-toggle:0:src");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, top);
				assert_eq!(
					m.paste.detail().and_then(|d| d.path.clone()).as_deref(),
					Some("top.txt")
				);
			});
			click(cx, "paste-dir-toggle:0:src");

			// Folding the header hides its folders too, and its own state
			// survives the round trip.
			model.update(cx, |m, cx| m.toggle_paste_commit(0, cx));
			cx.run_until_parked();
			assert!(!drawn(cx, "paste-dir:0:src"));
			assert!(drawn(cx, "paste-dir:1:src"));
			model.update(cx, |m, cx| m.toggle_paste_commit(0, cx));
			cx.run_until_parked();
			assert!(drawn(cx, "paste-dir:0:src"));
			assert!(drawn(cx, &row(a, "src/a.txt")));
		}

		/// The branch name keeps its width before the repo count gives way
		/// (#194): the `×N` suffix is dropped first, whole or not at all.
		#[gpui::test]
		fn branch_name_keeps_its_width_before_the_repo_count(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			for i in 0..3 {
				let r = repo(ws.path(), &format!("r{i}"), &[]);
				git(
					&r,
					&[
						"branch",
						"feature/a-very-long-branch-name-every-repo-has",
					],
				);
			}
			let (_model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1080., 720.);
			settle(cx);
			// The merged log: the user's gestures, not a model method.
			click(cx, "log-filter-repo");
			click(cx, "log-repo:all");
			settle(cx);
			if cx.debug_bounds("ref:refs/heads/main").is_none() {
				click(cx, "branch-group:refs_local");
				settle(cx);
			}
			let natural = cx.update(|window, _| {
				crate::ui::text_width(window, "main", crate::theme::UI_TEXT)
			});

			// A short name fits: the whole name, and the count is drawn.
			let name_w = cx
				.debug_bounds("ref-name:refs/heads/main")
				.expect("main's name is rendered")
				.size
				.width;
			assert!(
				f32::from(name_w) >= natural - 1.0,
				"main keeps its width: {name_w:?} vs natural {natural}"
			);
			assert!(
				cx.debug_bounds("ref-repo:refs/heads/main").is_some(),
				"the ×3 suffix is drawn beside main"
			);

			// A long name overflows the pane: the suffix gives way first.
			let long =
				"refs/heads/feature/a-very-long-branch-name-every-repo-has";
			let name_sel: &'static str =
				Box::leak(format!("ref-name:{long}").into_boxed_str());
			let repo_sel: &'static str =
				Box::leak(format!("ref-repo:{long}").into_boxed_str());
			let long_name = cx
				.debug_bounds(name_sel)
				.expect("the long name is rendered")
				.size
				.width;
			// The suffix is whole or gone: it is not drawn at all.
			let long_suffix = cx
				.debug_bounds(repo_sel)
				.map_or(0., |b| f32::from(b.size.width));
			assert!(
				long_suffix <= 1.0,
				"the suffix gives way first: {long_suffix:?}"
			);
			assert!(
				f32::from(long_name) > natural,
				"the name keeps the room the suffix gave up: {long_name:?}"
			);
		}

		#[gpui::test]
		fn one_path_under_two_roots_toggles_and_applies_per_row(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let alpha =
				repo(ws.path(), "alpha", &[("unrelated.txt", "alpha old")]);
			let beta =
				repo(ws.path(), "beta", &[("unrelated.txt", "beta old")]);
			let (_dest, dest) = canonical_tmp();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(
				&model,
				cx,
				"// FILE: alpha/unrelated.txt\nalpha new\n// FILE: beta/unrelated.txt\nbeta new\n",
			);
			for (prefix, root) in [("alpha", &alpha), ("beta", &beta)] {
				let root = dunce::canonicalize(root).unwrap();
				let idx = model.read_with(cx, |m, _| {
					let plan = m.paste.plan().unwrap();
					let choice = plan
						.prefix_choices
						.iter()
						.find(|c| c.prefix == prefix)
						.unwrap();
					choice.candidates.iter().position(|c| *c == root).unwrap()
				});
				model
					.update(cx, |m, cx| m.choose_paste_prefix(prefix, idx, cx));
				settle(cx);
			}
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.items.len(), 2);
				assert!(plan.items.iter().all(|i| i.path == "unrelated.txt"));
				assert_ne!(
					plan.items[0].dest_root_name,
					plan.items[1].dest_root_name
				);
			});
			// Clicks land on the rendered controls of the second row.
			click(cx, "paste-row:1:unrelated.txt");
			click(cx, "paste-overwrite:1:unrelated.txt");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, 1);
				assert!(!plan.items[0].overwrite_allowed);
				assert!(plan.items[1].overwrite_allowed);
			});
			click(cx, "paste-include:1:unrelated.txt");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.items[0].selected);
				assert!(!plan.items[1].selected);
			});
			click(cx, "paste-include:1:unrelated.txt");
			click(cx, "paste-row:0:unrelated.txt");
			click(cx, "paste-overwrite:0:unrelated.txt");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, 0);
				assert!(plan.items.iter().all(|i| i.selected));
				assert!(plan.items.iter().all(|i| i.overwrite_allowed));
			});
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			assert_eq!(
				fs::read_to_string(alpha.join("unrelated.txt")).unwrap(),
				"alpha new"
			);
			assert_eq!(
				fs::read_to_string(beta.join("unrelated.txt")).unwrap(),
				"beta new"
			);
		}

		#[gpui::test]
		fn commit_paste_clicks_show_sequential_destination_diffs(
			cx: &mut TestAppContext,
		) {
			assert_commit_paste_diffs(cx, false);
		}

		#[gpui::test]
		fn remote_commit_paste_clicks_show_sequential_destination_diffs(
			cx: &mut TestAppContext,
		) {
			let _remote = remote_lock();
			assert_commit_paste_diffs(cx, true);
		}

		fn assert_commit_paste_diffs(cx: &mut TestAppContext, remote: bool) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
				NotCopiedReason,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let dest = repo(ws.path(), "dest", &[]);
			for (path, text) in [
				("common.txt", "already\n"),
				("delete.txt", "delete me\n"),
				("old.txt", "old name\n"),
			] {
				fs::write(dest.join(path), text).unwrap();
			}
			git(&dest, &["add", "."]);
			git(&dest, &["commit", "-qm", "destination"]);
			let file = |path: &str, change, content: Option<&str>| CommitFile {
				path: path.into(),
				old_path: None,
				change,
				content: content.map(Into::into),
				not_copied: None,
			};
			let record = |message: &str, files| CommitRecord {
				message: message.into(),
				author_name: "Ann".into(),
				author_email: "ann@example.invalid".into(),
				author_date: "2026-10-13T12:00:00+00:00".into(),
				files,
			};
			let mut renamed =
				file("new.txt", FileChange::Renamed, Some("renamed\n"));
			renamed.old_path = Some("old.txt".into());
			let mut binary = file("img.bin", FileChange::Added, None);
			binary.not_copied = Some(NotCopiedReason::Binary);
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![
						record(
							"C1",
							vec![
								file(
									"common.txt",
									FileChange::Modified,
									Some("c1\n"),
								),
								file("delete.txt", FileChange::Deleted, None),
								renamed,
								binary,
							],
						),
						record(
							"C2",
							vec![file(
								"common.txt",
								FileChange::Modified,
								Some("c2\n"),
							)],
						),
					],
				});
			let before_index = fs::read(dest.join(".git/index")).unwrap();
			let before_head =
				fs::read(dest.join(".git/refs/heads/main")).unwrap();
			let (model, cx) = if remote {
				let (model, cx, _worker) = open_remote(
					cx,
					ws.path(),
					snip_remote::WorkerOptions::default(),
				);
				cx.simulate_resize(gpui::size(
					gpui::px(1440.),
					gpui::px(1100.),
				));
				remote_paste_preview(&model, cx, &payload);
				(model, cx)
			} else {
				let (model, cx) = open_sized(
					cx,
					ws.path().to_path_buf(),
					Some(dest.clone()),
					1440.,
					1100.,
				);
				paste(&model, cx, &payload);
				(model, cx)
			};
			for (row, removed, added) in [
				("paste-row:0:common.txt", "-already\n", "+c1\n"),
				("paste-row:5:common.txt", "-c1\n", "+c2\n"),
				("paste-row:1:delete.txt", "-delete me\n", ""),
				("paste-row:3:old.txt", "-old name\n", ""),
				("paste-row:4:new.txt", "", "+renamed\n"),
			] {
				click(cx, row);
				wait_paste_detail(&model, cx);
				assert!(cx.debug_bounds("paste-commit-metadata").is_some());
				model.read_with(cx, |m, _| {
					let detail = m.paste.detail().expect("selected preview");
					assert!(detail.is_diff, "{row}: {}", detail.text);
					assert!(
						detail.text.contains(removed),
						"{row}: {}",
						detail.text
					);
					assert!(
						detail.text.contains(added),
						"{row}: {}",
						detail.text
					);
					assert!(
						!detail.text.contains("author:"),
						"metadata is not file content"
					);
				});
				click(cx, "btn-diff-mode");
				assert_eq!(
					model.read_with(cx, |m, _| m.reader.diff_mode),
					crate::reader::DiffMode::SideBySide
				);
				click(cx, "btn-diff-mode");
			}
			click(cx, "paste-row:2:img.bin");
			wait_paste_detail(&model, cx);
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.paste
						.detail()
						.unwrap()
						.notice
						.as_ref()
						.map(|n| n.render(m.locale))
						.as_deref(),
					Some(crate::i18n::t("reason_nc_binary", m.locale))
				);
				assert!(m.paste.plan().unwrap().overwrite_missing());
			});
			// A narrow window must still expose both metadata and the diff
			// switch; debug ids alone could exist outside the clipped panel.
			cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(600.)));
			click(cx, "paste-row:0:common.txt");
			wait_paste_detail(&model, cx);
			assert_paste_control_in_view(cx, "paste-commit-metadata");
			assert_paste_control_in_view(cx, "btn-diff-mode");
			click(cx, "btn-diff-mode");
			assert_eq!(
				model.read_with(cx, |m, _| m.reader.diff_mode),
				crate::reader::DiffMode::SideBySide
			);
			for (path, text) in [
				("common.txt", "already\n"),
				("delete.txt", "delete me\n"),
				("old.txt", "old name\n"),
			] {
				assert_eq!(fs::read_to_string(dest.join(path)).unwrap(), text);
			}
			assert!(!dest.join("new.txt").exists());
			assert!(!dest.join("img.bin").exists());
			assert_eq!(
				fs::read(dest.join(".git/index")).unwrap(),
				before_index
			);
			assert_eq!(
				fs::read(dest.join(".git/refs/heads/main")).unwrap(),
				before_head
			);
		}

		/// A commit pasted into a remote workspace (Cmd+V), then a click on a
		/// file row when the worker cannot restart: the preview's notice
		/// says why in the interface language (#206).
		#[cfg(unix)]
		#[gpui::test]
		fn remote_commit_paste_diff_with_a_lost_worker_says_why_in_the_interface_language(
			cx: &mut TestAppContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let _serial = remote_lock();
			let Some(_clip) = clipboard() else { return };
			let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
			model.update(cx, |m, cx| m.select_repo(0, cx));
			settle(cx);
			assert_eq!(
				model.read_with(cx, |m, _| m.locale),
				crate::i18n::Locale::ZhTw
			);
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "C1".into(),
						author_name: "Ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-10-13T12:00:00+00:00".into(),
						files: vec![CommitFile {
							path: "init.txt".into(),
							old_path: None,
							change: FileChange::Modified,
							content: Some("c1\n".into()),
							not_copied: None,
						}],
					}],
				});
			clip::write_text(&payload).unwrap();
			cx.simulate_keystrokes("cmd-v");
			settle_until(cx, "the commit paste to plan", |cx| {
				model.read_with(cx, |m, _| {
					m.paste.plan().is_some() && !m.paste.is_loading()
				})
			});
			lose_the_worker(&model, cx);
			click(cx, "paste-row:0:init.txt");
			wait_paste_detail(&model, cx);
			let notice = model.read_with(cx, |m, _| {
				m.paste
					.detail()
					.and_then(|d| d.notice.as_ref().map(|n| n.render(m.locale)))
					.expect("a notice")
			});
			assert!(notice.contains("無法顯示差異"), "{notice}");
			assert_no_snip_in_zh(&notice);
		}

		/// A ~9 MiB commit opens, and a small row's detail read must fit: the
		/// UI already counts the commit preview the read shares, so the read
		/// is not charged for it again. Each big file stays under the 1 MiB
		/// per-file diff limit.
		#[gpui::test]
		fn large_commit_paste_row_click_shows_diff(cx: &mut TestAppContext) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let dest = repo(ws.path(), "dest", &[]);
			let line = "0123456789abcdef\n";
			let text = line.repeat(1000 * 1024 / line.len());
			let mut files: Vec<CommitFile> = (0..9)
				.map(|i| CommitFile {
					path: format!("big{i}.txt"),
					old_path: None,
					change: FileChange::Added,
					content: Some(text.clone()),
					not_copied: None,
				})
				.collect();
			files.push(CommitFile {
				path: "small.txt".into(),
				old_path: None,
				change: FileChange::Added,
				content: Some("hello\n".into()),
				not_copied: None,
			});
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![CommitRecord {
						message: "large".into(),
						author_name: "Ann".into(),
						author_email: "ann@example.invalid".into(),
						author_date: "2026-10-13T12:00:00+00:00".into(),
						files,
					}],
				});
			let (model, cx) = open_sized(
				cx,
				ws.path().to_path_buf(),
				Some(dest.clone()),
				1440.,
				1100.,
			);
			paste(&model, cx, &payload);
			assert!(
				model.read_with(cx, |m, _| m
					.paste
					.plan()
					.is_some_and(|p| { p.commit_preview.is_some() })),
				"commit preview did not open"
			);
			click(cx, "paste-row:9:small.txt");
			wait_paste_detail(&model, cx);
			model.read_with(cx, |m, _| {
				let detail = m.paste.detail().expect("selected preview");
				assert!(
					detail.is_diff,
					"row click shows no diff: {:?} status={}",
					detail.notice, m.status
				);
				assert!(
					detail.text.contains("+hello"),
					"diff lacks the added line"
				);
			});
		}

		fn wait_paste_detail(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			for _ in 0..100 {
				settle(cx);
				if model.read_with(cx, |m, _| {
					m.paste
						.detail()
						.is_some_and(|p| p.is_diff || p.notice.is_some())
				}) {
					return;
				}
				std::thread::sleep(snip_native_e2e::scaled(
					Duration::from_millis(10),
				));
			}
			panic!("selected paste diff did not finish");
		}

		fn assert_paste_control_in_view(
			cx: &mut VisualTestContext,
			id: &'static str,
		) {
			let bounds = cx
				.debug_bounds(id)
				.unwrap_or_else(|| panic!("missing {id}"));
			assert!(
				f32::from(bounds.origin.x) >= 0.
					&& f32::from(bounds.origin.y) >= 0.
					&& f32::from(bounds.origin.x + bounds.size.width) <= 900.
					&& f32::from(bounds.origin.y + bounds.size.height) <= 600.,
				"{id} is outside the visible 900x600 window: {bounds:?}"
			);
		}

		#[gpui::test]
		fn protected_paste_mapping_shows_rejection_and_counts_without_writing(
			cx: &mut TestAppContext,
		) {
			assert_protected_paste_mapping(cx, false);
		}

		#[gpui::test]
		fn remote_protected_paste_mapping_shows_rejection_and_counts_without_writing(
			cx: &mut TestAppContext,
		) {
			let _remote = remote_lock();
			assert_protected_paste_mapping(cx, true);
		}

		/// A preview opened before the repo scan finished gains the repos the
		/// scan found afterwards, keeping the existing candidates' indexes.
		#[gpui::test]
		fn paste_preview_candidates_follow_the_scan(cx: &mut TestAppContext) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 1);
			paste(&model, cx, "// FILE: x/one.txt\n1\n// FILE: y/two.txt\n2\n");
			// Found by the scan only now.
			let late = scanned_entry(&repo(ws.path(), "b", &[]));
			let b = late.root.clone();
			let before = model.read_with(cx, |m, _| {
				let choices = &m.paste.plan().unwrap().prefix_choices;
				assert!(!choices.is_empty());
				assert!(choices.iter().all(|c| !c.candidates.contains(&b)));
				choices[0].candidates.clone()
			});
			model.update_in(cx, |m, _, cx| {
				m.apply_scan_page(false, vec![late], cx);
				m.finish_discovery(cx);
			});
			model.read_with(cx, |m, _| {
				for choice in &m.paste.plan().unwrap().prefix_choices {
					assert!(choice.candidates.contains(&b));
					assert_eq!(
						choice.candidates[..before.len()],
						before[..],
						"existing candidates keep their index"
					);
				}
			});
		}

		/// The scan finishing while a remap job is in flight: the plan that
		/// lands carries the old candidates, and still gains the new repo.
		#[gpui::test]
		fn paste_candidates_follow_the_scan_during_a_remap(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 1);
			paste(&model, cx, "// FILE: x/one.txt\n1\n");
			let late = scanned_entry(&repo(ws.path(), "b", &[]));
			let b = late.root.clone();
			model.update_in(cx, |m, _, cx| {
				m.choose_paste_prefix("x", 0, cx);
				assert!(m.paste.is_loading(), "remap job in flight");
				m.apply_scan_page(false, vec![late], cx);
				m.finish_discovery(cx);
				assert!(m.paste.is_loading(), "refresh skipped while loading");
			});
			for _ in 0..100 {
				settle(cx);
				if !model.read_with(cx, |m, _| m.paste.is_loading()) {
					break;
				}
				std::thread::sleep(snip_native_e2e::scaled(
					Duration::from_millis(10),
				));
			}
			model.read_with(cx, |m, _| {
				assert!(!m.paste.is_loading(), "remap did not land");
				for choice in &m.paste.plan().unwrap().prefix_choices {
					assert!(choice.candidates.contains(&b));
				}
			});
		}

		/// A repo outside the workspace, in a directory of its own: the
		/// workspace scan cannot find it, so only the user adding it brings
		/// it into the set. Its root comes back canonical.
		fn external_repo(name: &str) -> (tempfile::TempDir, PathBuf) {
			let tmp = tempfile::tempdir().unwrap();
			let root = repo(tmp.path(), name, &[]);
			let root = dunce::canonicalize(root).unwrap();
			(tmp, root)
		}

		/// Adds `path` the way a user does: the Project window's plus button,
		/// the path field, Enter.
		fn add_repo_by_gesture(cx: &mut VisualTestContext, path: &Path) {
			cx.simulate_keystrokes("alt-1");
			settle(cx);
			click(cx, "btn-add-repo");
			click(cx, "input-add-repo");
			cx.simulate_input(&path.display().to_string());
			cx.simulate_keystrokes("enter");
		}

		/// The workspace's repos are all found and the scan is over.
		fn repos_settled(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
			n: usize,
		) {
			settle_until(cx, "the workspace's repos", |cx| {
				model.read_with(cx, |m, _| !m.is_loading && m.repos.len() == n)
			});
		}

		/// The entry the scan builds for a repo it found.
		fn scanned_entry(root: &Path) -> crate::RepoEntry {
			use snip_core::gitrun::CancelToken;
			crate::resolve_added_repo(root.to_path_buf(), &CancelToken::new())
				.unwrap()
		}

		/// A repo added by hand while a paste preview is open joins every
		/// prefix choice's candidates (#227): adding is a change of the
		/// repo set like a scan page, not a scan's end.
		#[gpui::test]
		fn manually_added_repo_joins_an_open_paste_previews_candidates(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			let (_ext_tmp, ext) = external_repo("ext");
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 1);
			paste(&model, cx, "// FILE: x/one.txt\n1\n// FILE: y/two.txt\n2\n");
			let before = model.read_with(cx, |m, _| {
				let choices = &m.paste.plan().unwrap().prefix_choices;
				assert!(!choices.is_empty());
				assert!(choices.iter().all(|c| !c.candidates.contains(&ext)));
				choices
					.iter()
					.map(|c| c.candidates.clone())
					.collect::<Vec<_>>()
			});
			add_repo_by_gesture(cx, &ext);
			settle_until(cx, "the added repo", |cx| {
				model.read_with(cx, |m, _| {
					m.repos.len() == 2 && !m.paste.is_loading()
				})
			});
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("the preview stays open");
				assert_eq!(plan.prefix_choices.len(), before.len());
				for (choice, old) in plan.prefix_choices.iter().zip(&before) {
					assert!(
						choice.candidates.contains(&ext),
						"{:?} lacks {ext:?}",
						choice.candidates
					);
					assert_eq!(
						choice.candidates[..old.len()],
						old[..],
						"existing candidates keep their index"
					);
				}
			});
		}

		/// A scan page that lands while the scan is still running already
		/// reaches the paste preview, and leaves Changes and Log to the
		/// scan's end (#227): the first page of a Refresh must not drop the
		/// rows of the repos it has not found again yet.
		#[gpui::test]
		fn scan_page_updates_paste_candidates_before_the_scan_ends(
			cx: &mut TestAppContext,
		) {
			use snip_core::gitrun::CancelToken;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 1);
			let b = repo(ws.path(), "b", &[]);
			paste(&model, cx, "// FILE: x/one.txt\n1\n");
			let scope_before =
				model.read_with(cx, |m, _| m.log_scope_key.clone());
			let entry =
				crate::resolve_added_repo(b, &CancelToken::new()).unwrap();
			let b_root = entry.root.clone();
			model.update_in(cx, |m, _, cx| {
				m.is_loading = true;
				m.apply_scan_page(false, vec![entry], cx);
			});
			model.read_with(cx, |m, _| {
				assert!(m.repos.iter().any(|r| r.root == b_root));
				for choice in &m.paste.plan().unwrap().prefix_choices {
					assert!(
						choice.candidates.contains(&b_root),
						"a page of the running scan reaches the preview"
					);
				}
				assert!(
					!m.change_repos.iter().any(|s| s.root == b_root),
					"Changes waits for the scan's end"
				);
				assert_eq!(m.log_scope_key, scope_before);
			});
			model.update_in(cx, |m, _, cx| {
				m.is_loading = false;
				m.finish_discovery(cx);
			});
			model.read_with(cx, |m, _| {
				assert!(m.change_repos.iter().any(|s| s.root == b_root));
			});
		}

		/// An Apply that fails stale returns the preview to idle: a repo
		/// added while it was applying (the one moment candidates cannot be
		/// extended) is in its candidates by then (#227).
		#[gpui::test]
		fn failed_apply_rebinds_candidates_to_repos_added_while_applying(
			cx: &mut TestAppContext,
		) {
			use snip_core::gitrun::CancelToken;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_tmp, root) = canonical_tmp();
			let dest = repo(&root, "dest", &[]);
			let (_ext_tmp, ext) = external_repo("ext");
			let entry =
				crate::resolve_added_repo(ext.clone(), &CancelToken::new())
					.unwrap();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			repos_settled(&model, cx, 1);
			paste(&model, cx, "// FILE: x/one.txt\n1\n");
			model.update_in(cx, |m, _, cx| m.choose_paste_keep("x", cx));
			settle_until(cx, "the mapping", |cx| {
				model.read_with(cx, |m, _| !m.paste.is_loading())
			});
			// The target appears behind the preview's back: Apply goes stale.
			fs::create_dir_all(dest.join("x")).unwrap();
			fs::write(dest.join("x/one.txt"), "external").unwrap();
			model.update_in(cx, |m, _, cx| {
				m.apply_paste_restore(cx);
				assert!(m.paste.is_applying());
				m.install_added_repo(Ok(entry), cx);
			});
			settle_until(cx, "the apply to fail", |cx| {
				model.read_with(cx, |m, _| !m.paste.is_applying())
			});
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("the preview stays open");
				assert_eq!(
					plan.error.as_ref().map(|e| e.key),
					Some("stale_created")
				);
				assert!(!plan.prefix_choices.is_empty());
				for choice in &plan.prefix_choices {
					assert!(
						choice.candidates.contains(&ext),
						"{:?} lacks {ext:?}",
						choice.candidates
					);
				}
			});
		}

		/// A manually added namesake renames the repo it shares a name with:
		/// the Changes nodes follow the rename and keep their order (#227).
		#[gpui::test]
		fn changes_nodes_follow_a_rename_from_a_manually_added_namesake(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			fs::create_dir(ws.path().join("nested")).unwrap();
			let app = repo(&ws.path().join("nested"), "app", &[]);
			repo(ws.path(), "zeta", &[]);
			let (_ext_tmp, ext) = external_repo("app");
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(app));
			repos_settled(&model, cx, 2);
			add_repo_by_gesture(cx, &ext);
			settle_until(cx, "the added namesake", |cx| {
				model.read_with(cx, |m, _| {
					m.repos.len() == 3 && m.change_repos.len() == 3
				})
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				for slot in &m.change_repos {
					let name = m
						.repos
						.iter()
						.find(|r| r.root == slot.root)
						.map(|r| r.name.as_str());
					assert_eq!(
						Some(slot.name.as_str()),
						name,
						"the node of {:?} keeps its old name",
						slot.root
					);
				}
				let order: Vec<_> = m
					.change_repos
					.iter()
					.map(|s| (s.name.clone(), s.root.clone()))
					.collect();
				let mut sorted = order.clone();
				sorted.sort();
				assert_eq!(order, sorted, "nodes are in (name, root) order");
			});
		}

		/// A Refresh that finds a repo ahead of the one the repo selector's
		/// cursor is on shifts the list: the cursor stays on its repo, so
		/// Enter opens the repo it pointed at (#227).
		#[gpui::test]
		fn repo_selector_cursor_stays_on_its_repo_when_a_rescan_reorders_it(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			repo(ws.path(), "c", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 2);
			cx.simulate_keystrokes("alt-shift-r");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.overlays.selector(), Some(crate::Popover::Repo));
			});
			cx.simulate_keystrokes("down");
			model.read_with(cx, |m, _| {
				assert_eq!(m.repos[m.overlays.selector_cursor()].name, "c");
			});
			repo(ws.path(), "b", &[]);
			model.update_in(cx, |m, _, cx| m.reload_repos(cx));
			repos_settled(&model, cx, 3);
			model.read_with(cx, |m, _| {
				assert_eq!(m.overlays.selector(), Some(crate::Popover::Repo));
			});
			cx.simulate_keystrokes("enter");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.repo().map(|r| r.name.as_str()), Some("c"));
			});
		}

		/// One Refresh that changes the repo set reloads the Log once: the
		/// open repo's reload and the scope change are one reload (#227).
		#[gpui::test]
		fn refresh_that_changes_the_repo_set_reloads_the_log_once(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			repo(ws.path(), "b", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 2);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| {
					m.log_scope_key.len() == 2 && m.history_loaded
				})
			});
			repo(ws.path(), "c", &[]);
			let before = model.read_with(cx, |m, _| m.history_replace_loads);
			model.update_in(cx, |m, _, cx| m.reload_repos(cx));
			repos_settled(&model, cx, 3);
			settle_until(cx, "the log of three repos", |cx| {
				model.read_with(cx, |m, _| {
					m.log_scope_key.len() == 3 && m.history_loaded
				})
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.history_replace_loads - before, 1);
			});
		}

		/// A paste job already reading when a repo is added lands with it.
		/// Guards the landing refresh the fan-out must not break (#227).
		#[gpui::test]
		fn paste_job_in_flight_lands_with_a_repo_added_meanwhile(
			cx: &mut TestAppContext,
		) {
			use snip_core::gitrun::CancelToken;
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			let (_ext_tmp, ext) = external_repo("ext");
			let entry =
				crate::resolve_added_repo(ext.clone(), &CancelToken::new())
					.unwrap();
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 1);
			clip::write_text("// FILE: x/one.txt\n1\n").unwrap();
			model.update_in(cx, |m, _, cx| {
				m.trigger_paste_preview(cx);
				assert!(m.paste.is_loading());
				m.install_added_repo(Ok(entry), cx);
			});
			settle_until(cx, "the preview", |cx| {
				model.read_with(cx, |m, _| {
					m.paste.plan().is_some() && !m.paste.is_loading()
				})
			});
			model.read_with(cx, |m, _| {
				for choice in &m.paste.plan().unwrap().prefix_choices {
					assert!(choice.candidates.contains(&ext));
				}
			});
		}

		/// Every source of a change to the repo set advances its revision
		/// and leaves the consumers settled: a scan page and a rescan, a repo
		/// added by hand, a vanished repo dropped, the workspace released.
		/// A remote scan's failure (`Clear`) needs a fault injected into the
		/// worker, and is covered by the type: a change that is not handed
		/// to `repos_changed` does not build, and panics when dropped.
		#[gpui::test]
		fn every_repo_set_source_fans_out(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			let (_ext_tmp, ext) = external_repo("ext");
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 1);
			let b = scanned_entry(&repo(ws.path(), "b", &[]));
			let c = scanned_entry(&repo(ws.path(), "c", &[]));
			let c_key = crate::repo_key(&c);
			let ext_entry = scanned_entry(&ext);
			let rev = |m: &WorkbenchModel| m.repos.rev();
			let mut last = model.read_with(cx, |m, _| rev(m));
			let mut advanced = |cx: &mut VisualTestContext, what: &str| {
				settle(cx);
				model.read_with(cx, |m, _| {
					assert!(rev(m) > last, "{what} advances the revision");
					last = rev(m);
					assert!(!m.is_loading);
					m.assert_repo_consumers_settled();
				});
			};

			model.update_in(cx, |m, _, cx| {
				m.apply_scan_page(false, vec![b], cx);
				m.assert_repo_consumers_settled();
			});
			advanced(cx, "a scan page");
			model.update_in(cx, |m, _, cx| {
				m.apply_scan_page(true, vec![c], cx);
				m.assert_repo_consumers_settled();
			});
			advanced(cx, "a rescan");
			model.update_in(cx, |m, _, cx| {
				m.install_added_repo(Ok(ext_entry), cx);
				m.assert_repo_consumers_settled();
			});
			advanced(cx, "a repo added by hand");
			model.update_in(cx, |m, _, cx| {
				assert!(m.drop_vanished_repo(&c_key, cx).is_none());
				m.assert_repo_consumers_settled();
			});
			advanced(cx, "a vanished repo");
			model.update_in(cx, |m, _, cx| m.release_workspace_state(cx));
			model.read_with(cx, |m, _| {
				assert!(rev(m) > last, "the release advances the revision");
				assert!(m.repos.is_empty());
				m.assert_repo_consumers_settled();
			});
		}

		/// A repo added by hand reloads the Log once, with the new repo in
		/// its scope (#227).
		#[gpui::test]
		fn manual_add_reloads_the_log_once_with_the_new_repo(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let a = repo(ws.path(), "a", &[]);
			repo(ws.path(), "b", &[]);
			let (_ext_tmp, ext) = external_repo("ext");
			let (model, cx) = open(cx, ws.path().to_path_buf(), Some(a));
			repos_settled(&model, cx, 2);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| {
					m.log_scope_key.len() == 2 && m.history_loaded
				})
			});
			let before = model.read_with(cx, |m, _| m.history_replace_loads);
			add_repo_by_gesture(cx, &ext);
			settle_until(cx, "the log with the added repo", |cx| {
				model.read_with(cx, |m, _| {
					m.log_scope_key.contains(&ext) && m.history_loaded
				})
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.history_replace_loads - before, 1);
			});
		}

		fn assert_protected_paste_mapping(
			cx: &mut TestAppContext,
			remote: bool,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let dest =
				repo(ws.path(), "target", &[("keep.txt", "keep original")]);
			let (model, cx) = if remote {
				let (model, cx, _worker) = open_remote(
					cx,
					ws.path(),
					snip_remote::WorkerOptions::default(),
				);
				cx.simulate_resize(gpui::size(gpui::px(1440.), gpui::px(900.)));
				(model, cx)
			} else {
				open_sized(
					cx,
					ws.path().to_path_buf(),
					Some(dest.clone()),
					1440.,
					900.,
				)
			};
			for mixed in [false, true] {
				cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(600.)));
				let mut payload = "// FILE: target/.git/hooks/pre-commit\n#!/bin/sh\necho owned\n".to_string();
				if mixed {
					payload.push_str("// FILE: target/good.txt\ngood\n// FILE: target/keep.txt\nincoming\n");
				}
				if remote {
					remote_paste_preview(&model, cx, &payload);
				} else {
					paste(&model, cx, &payload);
				}
				let pick = model.read_with(cx, |m, _| {
					let choice = &m.paste.plan().unwrap().prefix_choices[0];
					let wanted = if remote {
						m.repo_root().unwrap()
					} else {
						dunce::canonicalize(&dest).unwrap()
					};
					choice.candidates.iter().position(|p| p == &wanted).unwrap()
				});
				click(
					cx,
					Box::leak(
						format!("paste-map-pick:target:{pick}")
							.into_boxed_str(),
					),
				);
				for _ in 0..100 {
					settle(cx);
					if !model.read_with(cx, |m, _| m.paste.is_loading()) {
						break;
					}
					std::thread::sleep(snip_native_e2e::scaled(
						Duration::from_millis(10),
					));
				}
				let ix = model.read_with(cx, |m, _| {
					let plan = m.paste.plan().unwrap();
					assert_eq!(
						plan.items.len(),
						if mixed { 3 } else { 1 },
						"protected paths remain visible"
					);
					let ix = plan
						.items
						.iter()
						.position(|i| i.path == "target/.git/hooks/pre-commit")
						.unwrap();
					assert!(matches!(
						plan.items[ix].op,
						crate::paste::PlannedOp::Skip(_)
					));
					assert_eq!(
						crate::ui::paste_op(&plan.items[ix]).2,
						"reason_skip_git_metadata"
					);
					assert_eq!(plan.counts().skips, if mixed { 2 } else { 1 });
					ix
				});
				assert_paste_control_in_view(cx, Box::leak(format!("paste-skip-reason:{ix}:target/.git/hooks/pre-commit").into_boxed_str()));
				click(
					cx,
					Box::leak(
						format!("paste-row:{ix}:target/.git/hooks/pre-commit")
							.into_boxed_str(),
					),
				);
				click(
					cx,
					Box::leak(
						format!(
							"paste-include:{ix}:target/.git/hooks/pre-commit"
						)
						.into_boxed_str(),
					),
				);
				model.read_with(cx, |m, _| {
					assert!(
						!m.paste.plan().unwrap().items[ix].selected,
						"protected row cannot be enabled"
					)
				});
				assert!(
					!dest.join(".git/hooks/pre-commit").exists(),
					"preview must not write hook"
				);
				cx.simulate_keystrokes("enter");
				for _ in 0..100 {
					settle(cx);
					if model.read_with(cx, |m, _| m.paste.plan().is_none()) {
						break;
					}
					std::thread::sleep(snip_native_e2e::scaled(
						Duration::from_millis(10),
					));
				}
				model.read_with(cx, |m, _| {
					assert!(
						m.paste.plan().is_none(),
						"apply finished: {}",
						m.status
					);
					let result =
						&m.toast.as_ref().expect("completion notice").2;
					if mixed {
						assert_eq!(result.key, "status_paste_done");
						assert_eq!(result.args, ["1", "0", "2", "0", "0"]);
					} else {
						assert_eq!(result.key, "status_paste_skipped");
						assert_eq!(result.args, ["1"]);
					}
				});
				assert!(
					!dest.join(".git/hooks/pre-commit").exists(),
					"apply must not write hook"
				);
				assert_eq!(
					fs::read_to_string(dest.join("keep.txt")).unwrap(),
					"keep original"
				);
				if mixed {
					assert_eq!(
						fs::read_to_string(dest.join("good.txt")).unwrap(),
						"good"
					);
				}
			}
		}

		#[gpui::test]
		fn one_path_in_two_commits_confirms_each_overwrite(
			cx: &mut TestAppContext,
		) {
			use snip_core::commits::{
				CommitFile, CommitRecord, CommitsPayload, FileChange,
			};
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			git(&dest, &["init", "-q", "-b", "main"]);
			fs::write(dest.join("common.txt"), "base\n").unwrap();
			git(&dest, &["add", "."]);
			git(&dest, &["commit", "-q", "-m", "base"]);
			let commit = |n: u8| CommitRecord {
				message: format!("edit {n}\n"),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: format!("2026-09-2{n}T12:00:00+00:00"),
				files: vec![CommitFile {
					path: "common.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some(format!("edit {n}\n")),
					not_copied: None,
				}],
			};
			let payload =
				snip_core::commits::to_clipboard_text(&CommitsPayload {
					commits: vec![commit(1), commit(2)],
				});
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, &payload);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.items.len(), 2);
				assert!(plan.items.iter().all(|i| {
					i.path == "common.txt"
						&& i.op == crate::paste::PlannedOp::Overwrite
				}));
			});
			// The second overwrite is what used to be unreachable.
			click(cx, "paste-overwrite:1:common.txt");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(!plan.items[0].overwrite_allowed);
				assert!(plan.items[1].overwrite_allowed);
			});
			click(cx, "paste-overwrite:0:common.txt");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.items.iter().all(|i| i.overwrite_allowed));
			});
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			assert!(model.read_with(cx, |m, _| m.paste.plan().is_none()));
			assert_eq!(
				fs::read_to_string(dest.join("common.txt")).unwrap(),
				"edit 2\n"
			);
			let log = Command::new("git")
				.current_dir(&dest)
				.args(["log", "--format=%s"])
				.output()
				.unwrap();
			assert_eq!(
				String::from_utf8_lossy(&log.stdout),
				"edit 2\nedit 1\nbase\n"
			);
		}

		#[gpui::test]
		fn keeping_an_ambiguous_prefix_replans_under_the_destination(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, "// FILE: sub/x.txt\nnested\n");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				let prefixes: Vec<&str> = plan
					.prefix_choices
					.iter()
					.map(|c| c.prefix.as_str())
					.collect();
				assert_eq!(prefixes, ["sub"]);
				assert!(!plan.mapping_ready());
				assert!(plan.items.is_empty());
			});
			model.update(cx, |m, cx| m.choose_paste_keep("sub", cx));
			settle(cx);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert!(plan.mapping_ready());
				let targets: Vec<&Path> =
					plan.items.iter().map(|i| i.dest_path.as_path()).collect();
				assert_eq!(targets, [dest.join("sub").join("x.txt").as_path()]);
			});
			cx.simulate_keystrokes("enter");
			cx.run_until_parked();
			assert_eq!(
				fs::read_to_string(dest.join("sub").join("x.txt")).unwrap(),
				"nested"
			);
		}

		#[gpui::test]
		fn paste_preview_shows_diff_for_overwritten_file(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			fs::write(dest.join("target.txt"), "line1\nold line\nline3\n")
				.unwrap();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, "// FILE: target.txt\nline1\nnew line\nline3\n");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert_eq!(plan.items.len(), 1);
				let item = &plan.items[0];
				assert!(item.overwritable());
				let detail = m.paste.detail().expect("detail present");
				assert!(detail.is_diff, "expected diff preview");
				assert!(detail.notice.is_none());
				assert!(
					detail.text.contains("-old line"),
					"expected old line in diff: {}",
					detail.text
				);
				assert!(
					detail.text.contains("+new line"),
					"expected new line in diff: {}",
					detail.text
				);
				assert_eq!(m.reader.diff_mode, crate::reader::DiffMode::Inline);
			});
			// Toggle to side-by-side and back via btn-diff-mode
			click(cx, "btn-diff-mode");
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.reader.diff_mode,
					crate::reader::DiffMode::SideBySide
				);
			});
			click(cx, "btn-diff-mode");
			model.read_with(cx, |m, _| {
				assert_eq!(m.reader.diff_mode, crate::reader::DiffMode::Inline);
			});
		}

		#[gpui::test]
		fn paste_preview_identical_destination_shows_source_with_same_notice(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			fs::write(dest.join("same.txt"), "exact content").unwrap();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, "// FILE: same.txt\nexact content\n");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert_eq!(plan.items.len(), 1);
				assert!(plan.items[0].overwritable());
				let detail = m.paste.detail().expect("detail present");
				assert!(
					!detail.is_diff,
					"identical file must show source view"
				);
				assert_eq!(detail.text.as_ref(), "exact content");
				let notice = detail.notice.as_ref().expect("notice present");
				assert_eq!(
					notice.render(m.locale),
					crate::i18n::t("paste_diff_same", m.locale)
				);
			});
		}

		/// A destination the reader cannot show as text keeps the source view
		/// and says why the diff is missing: over the 1 MiB preview cap, or
		/// not text.
		#[gpui::test]
		fn paste_preview_without_a_readable_destination_explains_the_missing_diff(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			fs::write(dest.join("big.txt"), "x".repeat(2 * 1024 * 1024))
				.unwrap();
			fs::write(dest.join("bin.txt"), b"old\0bytes\xff\n").unwrap();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			paste(&model, cx, "// FILE: big.txt\nsmall now\n");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert!(plan.items[0].overwritable());
				let detail = m.paste.detail().expect("detail present");
				assert!(!detail.is_diff, "an oversized target has no diff");
				assert_eq!(detail.text.as_ref(), "small now");
				assert_eq!(
					detail
						.notice
						.as_ref()
						.map(|n| n.render(m.locale))
						.as_deref(),
					Some(crate::i18n::t("paste_diff_too_large", m.locale))
				);
			});
			model.update(cx, |m, cx| m.cancel_paste_preview(cx));
			paste(&model, cx, "// FILE: bin.txt\ntext now\n");
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				// The plan leaves a target that is not text alone; then there
				// is nothing to compare. If it ever plans one, say why.
				if !plan.items.first().is_some_and(|i| i.overwritable()) {
					return;
				}
				let detail = m.paste.detail().expect("detail present");
				assert!(!detail.is_diff, "a binary target has no diff");
				assert_eq!(
					detail
						.notice
						.as_ref()
						.map(|n| n.render(m.locale))
						.as_deref(),
					Some(crate::i18n::t("paste_diff_binary", m.locale))
				);
			});
		}

		#[gpui::test]
		fn paste_preview_selecting_another_row_before_read_finishes_drops_first_diff(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "alpha", &[]);
			let (_dest, dest) = canonical_tmp();
			// Only b.txt exists in dest
			fs::write(dest.join("b.txt"), "old b\n").unwrap();
			let (model, cx) =
				open(cx, ws.path().to_path_buf(), Some(dest.clone()));
			// Three files: a.txt (fresh), b.txt (overwrites), c.txt (fresh)
			paste(
				&model,
				cx,
				"// FILE: a.txt\nfresh a\n// FILE: b.txt\nnew b\n// FILE: c.txt\nfresh c\n",
			);
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert_eq!(plan.selected_item_idx, 0);
				assert!(!m.paste.detail().unwrap().is_diff);
			});

			// Delay background read by 300ms
			model.update(cx, |m, _| {
				m.e2e_read_delay = Some(Duration::from_millis(300));
			});
			let b1 = cx.debug_bounds("paste-row:1:b.txt").unwrap();
			let b2 = cx.debug_bounds("paste-row:2:c.txt").unwrap();
			// Click row 1 (starts slow read) then immediately row 2 before row 1 finishes
			cx.simulate_click(b1.center(), gpui::Modifiers::none());
			cx.simulate_click(b2.center(), gpui::Modifiers::none());
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert_eq!(plan.selected_item_idx, 2);
				let detail = m.paste.detail().expect("detail present");
				assert!(!detail.is_diff);
				assert_eq!(detail.text.as_ref(), "fresh c");
			});

			// Drain all background work including row 1's delayed read
			settle(cx);

			// Row 2 is still showing fresh c, row 1's diff was discarded
			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().expect("plan open");
				assert_eq!(plan.selected_item_idx, 2);
				let detail = m.paste.detail().expect("detail present");
				assert_eq!(detail.path.as_deref(), Some("c.txt"));
				assert!(
					!detail.is_diff,
					"row 2 must not be replaced by row 1 diff"
				);
				assert_eq!(detail.text.as_ref(), "fresh c");
			});
		}

		#[gpui::test]
		fn shift_range_selects_first_parent_chain_excluding_side(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = ws.path().join("repo");
			fs::create_dir(&r).unwrap();
			git(&r, &["init", "-q", "-b", "main"]);
			let head = || -> String {
				let out = Command::new("git")
					.current_dir(&r)
					.args(["rev-parse", "HEAD"])
					.output()
					.unwrap();
				String::from_utf8(out.stdout).unwrap().trim().to_string()
			};

			fs::write(r.join("base.txt"), "base").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "base"]);
			let base_sha = head();

			fs::write(r.join("c1.txt"), "c1").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "c1"]);
			let c1_sha = head();

			fs::write(r.join("c2.txt"), "c2").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "c2"]);
			let c2_sha = head();

			git(&r, &["checkout", "-q", "-b", "side"]);
			fs::write(r.join("side.txt"), "side").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "side"]);
			let side_sha = head();

			git(&r, &["checkout", "-q", "main"]);
			git(&r, &["merge", "-q", "--no-ff", "-m", "c3", "side"]);
			let c3_sha = head();

			fs::write(r.join("c4.txt"), "c4").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "c4"]);
			let c4_sha = head();

			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			settle(cx);

			// Verify display rows are in top-down topological order:
			// C4 (0), C3 (1), SIDE (2), C2 (3), C1 (4), base (5).
			model.read_with(cx, |m, _| {
				let shas: Vec<&str> = m
					.display_commits()
					.iter()
					.map(|c| c.sha.as_str())
					.collect();
				assert_eq!(
					shas,
					[
						c4_sha.as_str(),
						c3_sha.as_str(),
						side_sha.as_str(),
						c2_sha.as_str(),
						c1_sha.as_str(),
						base_sha.as_str()
					]
				);
			});

			// Select C1 (index 4)
			model.update(cx, |m, cx| m.select_commit(&c1_sha, cx));
			// Focus log element
			cx.update(|window, cx| {
				window.focus(&model.read(cx).log_focus.clone())
			});

			// Drive Shift+Up 3 times from C1 to C3:
			// Step 1: to C2 (index 3)
			cx.simulate_keystrokes("shift-up");
			cx.run_until_parked();
			// Step 2: to SIDE (index 2)
			cx.simulate_keystrokes("shift-up");
			cx.run_until_parked();
			// Step 3: to C3 (index 1)
			cx.simulate_keystrokes("shift-up");
			cx.run_until_parked();

			// Assert log_selected has [C3, C2, C1] (excluding SIDE!)
			model.read_with(cx, |m, _| {
				assert_eq!(m.selected_commit.as_deref(), Some(c1_sha.as_str()));
				assert_eq!(m.range_head.as_deref(), Some(c3_sha.as_str()));
				assert_eq!(
					m.log_selected,
					[c3_sha.clone(), c2_sha.clone(), c1_sha.clone()]
				);
				assert!(!m.log_is_selected(&side_sha));
				assert!(m.log_is_selected(&c1_sha));
				assert!(m.log_is_selected(&c2_sha));
				assert!(m.log_is_selected(&c3_sha));
			});

			// Assert commit_copy_target returns tip C3 and the 3 SHAs
			let (_, _, tip, selected) =
				model.read_with(cx, |m, _| m.commit_copy_target().unwrap());
			assert_eq!(tip, c3_sha);
			assert_eq!(
				selected,
				[c3_sha.clone(), c2_sha.clone(), c1_sha.clone()]
			);

			// Assert open_log_menu on C2 (inside selection) preserves the multi-selection:
			cx.update(|window, app| {
				model.update(app, |m, cx| {
					m.open_log_menu(
						&c2_sha,
						gpui::Point::default(),
						window,
						cx,
					);
					assert_eq!(
						m.selected_commit.as_deref(),
						Some(c1_sha.as_str())
					);
					assert_eq!(
						m.log_selected,
						[c3_sha.clone(), c2_sha.clone(), c1_sha.clone()]
					);
				});
			});

			// Assert open_log_menu on SIDE (not in selection) re-selects SIDE alone:
			cx.update(|window, app| {
				model.update(app, |m, cx| {
					m.open_log_menu(
						&side_sha,
						gpui::Point::default(),
						window,
						cx,
					);
					assert_eq!(
						m.selected_commit.as_deref(),
						Some(side_sha.as_str())
					);
					assert!(m.log_selected.is_empty());
				});
			});
		}

		fn make_test_repo(root: PathBuf, name: &str) -> crate::RepoEntry {
			crate::RepoEntry {
				root,
				name: name.into(),
				kind: crate::RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("offline")),
			}
		}

		fn make_test_commit(sha: &str) -> snip_core::browser::CommitSummary {
			snip_core::browser::CommitSummary {
				sha: sha.into(),
				parents: Vec::new(),
				author_name: "a".into(),
				author_email: "a@a".into(),
				author_date: "2026-01-01".into(),
				subject: sha.into(),
			}
		}

		fn make_test_file(path: &str, repo: u32) -> crate::FileChangeItem {
			crate::FileChangeItem {
				path: path.into(),
				change_type: Some(crate::ChangeType::Modified),
				source: snip_core::transfer::SourceKind::Working,
				is_conflict: false,
				repo,
			}
		}

		#[gpui::test]
		fn local_non_repo_folder_says_no_repository_empty(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.probes = Some(crate::ui::Probes::for_test());
			});
			settle(cx);
			for _ in 0..50 {
				let done = model.read_with(cx, |m, _| {
					!m.is_loading && m.discovery_status.is_some()
				});
				if done {
					break;
				}
				cx.run_until_parked();
				cx.executor().advance_clock(Duration::from_millis(50));
			}
			for _ in 0..2 {
				// ALLOWED-TEST-REFRESH: test harness needs a redraw after workspace switch to record probe bounds
				cx.update(|w, _| w.refresh());
				settle(cx);
			}
			let (changes_empty, log_empty) = model.read_with(cx, |m, _| {
				(m.changes_empty_state(), m.log_empty_state())
			});
			assert_eq!(changes_empty, Some(ChangesEmpty::NoRepository));
			assert_eq!(log_empty, Some(LogEmpty::NoRepository));
			assert_ne!(changes_empty, Some(ChangesEmpty::Clean));
			assert_ne!(log_empty, Some(LogEmpty::Empty));
			let drawn =
				model.read_with(cx, |m, _| m.probes.as_ref().unwrap().drawn());
			assert!(
				drawn.contains(&"changes-empty".to_string()),
				"drawn: {drawn:?}"
			);
			assert_eq!(
				model.read_with(cx, |m, _| m.last_changes_empty.get()),
				Some("no_repository")
			);
		}

		#[gpui::test]
		fn a_loading_slot_is_not_shown_as_clean_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().to_path_buf(),
					"repo",
				)]);
				m.change_repos = vec![crate::ChangeRepo {
					root: tmp.path().to_path_buf(),
					name: "repo".into(),
					state: crate::ChangeRepoState::Loading,
					total: 0,
				}];
				m.files.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			let empty = model.read_with(cx, |m, _| m.changes_empty_state());
			assert_eq!(empty, Some(ChangesEmpty::Loading));
			assert_ne!(empty, Some(ChangesEmpty::Clean));
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_title_text()),
				"變更 (…)"
			);
		}

		#[gpui::test]
		fn speed_search_with_no_match_is_not_clean_empty(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![
					make_test_repo(tmp.path().join("r1"), "r1"),
					make_test_repo(tmp.path().join("r2"), "r2"),
				]);
				m.change_repos = vec![
					crate::ChangeRepo {
						root: tmp.path().join("r1"),
						name: "r1".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 1,
					},
					crate::ChangeRepo {
						root: tmp.path().join("r2"),
						name: "r2".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 0,
					},
				];
				m.files = vec![make_test_file("foo.txt", 0)];
				m.chrome.speed = "nomatch_filter_query".into();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			let empty = model.read_with(cx, |m, _| m.changes_empty_state());
			assert_eq!(empty, Some(ChangesEmpty::NoMatch));
			assert_ne!(empty, Some(ChangesEmpty::Clean));
		}

		#[gpui::test]
		fn change_list_top_is_drawn_only_at_the_start_of_the_list(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) =
				open_sized(cx, tmp.path().to_path_buf(), None, 900., 400.);
			for _ in 0..50 {
				if model.read_with(cx, |m, _| m.discovery_status.is_some()) {
					break;
				}
				settle(cx);
			}
			let n = 200;
			model.update(cx, |m, cx| {
				m.probes = Some(crate::ui::Probes::for_test());
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().to_path_buf(),
					"repo",
				)]);
				m.change_repos = vec![crate::ChangeRepo {
					root: tmp.path().to_path_buf(),
					name: "repo".into(),
					state: crate::ChangeRepoState::Loaded,
					total: n,
				}];
				m.files = (0..n)
					.map(|i| make_test_file(&format!("f{i:03}.txt"), 0))
					.collect();
				m.is_loading = false;
				cx.notify();
			});
			settle(cx);
			let drawn = |cx: &mut VisualTestContext| {
				model.read_with(cx, |m, _| m.probes.as_ref().unwrap().drawn())
			};
			let top = "change-list-top".to_string();
			let ids = drawn(cx);
			assert!(
				ids.contains(&"change-header:unstaged".to_string())
					&& ids.contains(&top),
				"the first row carries the top marker: {ids:?}"
			);
			model.update(cx, |m, cx| {
				m.chrome
					.left_scroll
					.scroll_to_item(n, gpui::ScrollStrategy::Top);
				cx.notify();
			});
			settle(cx);
			let ids = drawn(cx);
			assert!(
				!ids.contains(&top)
					&& !ids.contains(&"change-header:unstaged".to_string()),
				"a list scrolled to its end has no top marker: {ids:?}"
			);
		}

		#[gpui::test]
		fn log_while_loading_is_not_empty_log_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().to_path_buf(),
					"repo",
				)]);
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				m.history_loaded = false;
				m.commits.clear();
			});
			let empty = model.read_with(cx, |m, _| m.log_empty_state());
			assert_eq!(empty, Some(LogEmpty::Loading));
			assert_ne!(empty, Some(LogEmpty::Empty));

			model.update(cx, |m, _| {
				m.history_loaded = true;
			});
			let empty_after = model.read_with(cx, |m, _| m.log_empty_state());
			assert_eq!(empty_after, Some(LogEmpty::Empty));

			model.update(cx, |m, _| {
				m.commits = vec![make_test_commit("abcdef123456")];
			});
			let empty_commits = model.read_with(cx, |m, _| m.log_empty_state());
			assert_eq!(empty_commits, None);
		}

		#[gpui::test]
		fn failed_feed_is_reported_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![
					make_test_repo(tmp.path().join("r1"), "r1"),
					make_test_repo(tmp.path().join("r2"), "r2"),
				]);
				m.log_feeds = vec![
					crate::multi_log::Feed {
						name: "r1".into(),
						failed: false,
						loaded: true,
						..Default::default()
					},
					crate::multi_log::Feed {
						name: "r2".into(),
						failed: true,
						loaded: false,
						..Default::default()
					},
				];
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				m.history_loaded = true;
			});
			let msg = model.read_with(cx, |m, _| m.failed_feeds_msg());
			let expected = crate::i18n::Msg::new(
				"log_failed_feeds",
				["1".to_string(), "r2".to_string()],
			);
			assert_eq!(msg, Some(expected.clone()));
			assert_eq!(
				expected.render(crate::i18n::Locale::ZhTw),
				"1 個儲存庫無法讀取：r2"
			);
			assert_eq!(
				expected.render(crate::i18n::Locale::En),
				"1 repository(ies) could not be read: r2"
			);

			// Many failed repos: the banner names two, the tooltip all.
			model.update(cx, |m, _| {
				for name in ["r3", "r4"] {
					m.log_feeds.push(crate::multi_log::Feed {
						name: name.into(),
						failed: true,
						..Default::default()
					});
				}
			});
			let (msg, names) = model.read_with(cx, |m, _| {
				(m.failed_feeds_msg(), m.failed_feed_names())
			});
			assert_eq!(
				msg.map(|m| m.render(crate::i18n::Locale::ZhTw)),
				Some("3 個儲存庫無法讀取：r2, r3, …".to_string())
			);
			assert_eq!(names, ["r2", "r3", "r4"]);
			model.update(cx, |m, _| m.log_feeds.truncate(2));

			// All feeds failed -> Failed
			model.update(cx, |m, _| {
				m.log_feeds[0].failed = true;
				m.commits.clear();
			});
			let (failed_msg, log_empty) = model.read_with(cx, |m, _| {
				(m.failed_feeds_msg(), m.log_empty_state())
			});
			assert!(failed_msg.is_none());
			assert!(matches!(log_empty, Some(LogEmpty::Failed(_))));
		}

		#[gpui::test]
		fn changes_empty_scanning_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.is_loading = true;
				m.discovery_status = None;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::Scanning)
			);
		}

		#[gpui::test]
		fn changes_empty_scan_failed_scan_error_branch_empty(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			let err_msg = crate::i18n::Msg::new(
				"remote_scan_failed",
				["network error".to_string()],
			);
			let worker = snip_remote::RemoteHost::ssh("w");
			let ws = snip_remote::RemoteWorkspace {
				id: "ws1".into(),
				name: "ws1".into(),
				path: "/tmp/ws".into(),
			};
			model.update(cx, |m, _| {
				m.remote.session =
					Some(crate::remote::RemoteSession::new(worker, ws));
				m.remote.scan_error = Some(err_msg.clone());
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::ScanFailed(err_msg))
			);
		}

		#[gpui::test]
		fn changes_empty_scan_failed_discovery_error_branch_empty(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(Vec::new());
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Incomplete);
				m.discovery_errors = vec![(
					tmp.path().join("sub"),
					crate::test_failure("read failed"),
				)];
				m.is_loading = false;
			});
			let state = model.read_with(cx, |m, _| m.changes_empty_state());
			assert_eq!(
				state,
				Some(ChangesEmpty::ScanFailed(crate::i18n::Msg::with_msg_arg(
					"error_repo_status",
					[String::new()],
					0,
					crate::reason::failure_msg(&crate::test_failure(
						"read failed"
					))
				)))
			);
		}

		/// Scan notes past the kept ones are one typed row each, shown as
		/// before: a workspace with only depth-limited directories past the
		/// cap still counts a scan error, offers the retry and says so in
		/// the Changes empty state.
		#[gpui::test]
		fn finish_discovery_turns_overflow_counts_into_typed_rows(
			cx: &mut TestAppContext,
		) {
			use snip_core::reason::Reason;
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, cx| {
				m.repos.replace_for_test(Vec::new());
				m.discovery_errors.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.discovery_depth_overflow = 3;
				m.finish_discovery(cx);
			});
			let (rows, error_overflow, depth_overflow) =
				model.read_with(cx, |m, _| {
					(
						m.discovery_errors
							.iter()
							.map(|(_, f)| (f.message.clone(), f.reason.clone()))
							.collect::<Vec<_>>(),
						m.discovery_error_overflow,
						m.discovery_depth_overflow,
					)
				});
			assert_eq!(
				rows,
				vec![(
					"3 more depth-limited directories omitted".to_string(),
					Reason::DepthLimitedOmitted { count: 3 }
				)]
			);
			assert_eq!((error_overflow, depth_overflow), (0, 0));
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::ScanFailed(crate::i18n::Msg::with_msg_arg(
					"error_repo_status",
					[String::new()],
					0,
					crate::reason::reason_msg(&Reason::DepthLimitedOmitted {
						count: 3
					})
				)))
			);

			model.update(cx, |m, cx| {
				m.discovery_errors.clear();
				m.discovery_error_overflow = 2;
				m.discovery_depth_overflow = 1;
				m.finish_discovery(cx);
			});
			let reasons = model.read_with(cx, |m, _| {
				m.discovery_errors
					.iter()
					.map(|(_, f)| f.reason.clone())
					.collect::<Vec<_>>()
			});
			assert_eq!(
				reasons,
				vec![
					Reason::ScanErrorsOmitted { count: 2 },
					Reason::DepthLimitedOmitted { count: 1 }
				]
			);
		}

		#[gpui::test]
		fn discovery_status_empty_message_localization(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			for status in [
				snip_core::workspace::ScanStatus::LimitReached,
				snip_core::workspace::ScanStatus::TimedOut,
				snip_core::workspace::ScanStatus::Cancelled,
				snip_core::workspace::ScanStatus::Incomplete,
			] {
				model.update(cx, |m, _| {
					m.repos.replace_for_test(Vec::new());
					m.discovery_status = Some(status);
					m.discovery_errors.clear();
					m.is_loading = false;
				});
				let changes_empty = model
					.read_with(cx, |m, _| m.changes_empty_state())
					.expect("changes empty");
				let log_empty = model
					.read_with(cx, |m, _| m.log_empty_state())
					.expect("log empty");

				let crate::ChangesEmpty::ScanFailed(changes_msg) =
					changes_empty
				else {
					panic!("expected ChangesEmpty::ScanFailed for {status:?}");
				};
				let crate::LogEmpty::Failed(log_msg) = log_empty else {
					panic!("expected LogEmpty::Failed for {status:?}");
				};

				for (label, msg) in [("changes", changes_msg), ("log", log_msg)]
				{
					let zh = msg.render(crate::i18n::Locale::ZhTw);
					assert!(
						!zh.to_lowercase().contains("discovery"),
						"{label} {status:?} ZhTw contains discovery: {zh}"
					);
					let en = msg.render(crate::i18n::Locale::En);
					assert!(
						en.to_lowercase().contains("discovery"),
						"{label} {status:?} En should contain discovery: {en}"
					);
				}
			}
		}

		#[gpui::test]
		fn changes_empty_no_repository_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(Vec::new());
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.discovery_errors.clear();
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::NoRepository)
			);
		}

		#[gpui::test]
		fn changes_empty_loading_unsynced_slots_branch_empty(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.change_repos.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::Loading)
			);
		}

		#[gpui::test]
		fn changes_empty_loading_slot_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.change_repos = vec![crate::ChangeRepo {
					root: tmp.path().join("r1"),
					name: "r1".into(),
					state: crate::ChangeRepoState::Loading,
					total: 0,
				}];
				m.files.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::Loading)
			);
		}

		#[gpui::test]
		fn changes_empty_no_match_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![
					make_test_repo(tmp.path().join("r1"), "r1"),
					make_test_repo(tmp.path().join("r2"), "r2"),
				]);
				m.change_repos = vec![
					crate::ChangeRepo {
						root: tmp.path().join("r1"),
						name: "r1".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 1,
					},
					crate::ChangeRepo {
						root: tmp.path().join("r2"),
						name: "r2".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 0,
					},
				];
				m.files = vec![make_test_file("a.txt", 0)];
				m.chrome.speed = "zzz_nomatch".into();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::NoMatch)
			);
		}

		#[gpui::test]
		fn changes_empty_clean_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.change_repos = vec![crate::ChangeRepo {
					root: tmp.path().join("r1"),
					name: "r1".into(),
					state: crate::ChangeRepoState::Loaded,
					total: 0,
				}];
				m.files.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::Clean)
			);
		}

		#[gpui::test]
		fn changes_empty_non_complete_discovery_with_clean_repos_is_scan_failed(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			for status in [
				snip_core::workspace::ScanStatus::TimedOut,
				snip_core::workspace::ScanStatus::LimitReached,
				snip_core::workspace::ScanStatus::Cancelled,
			] {
				model.update(cx, |m, _| {
					m.repos.replace_for_test(vec![make_test_repo(
						tmp.path().join("r1"),
						"r1",
					)]);
					m.change_repos = vec![crate::ChangeRepo {
						root: tmp.path().join("r1"),
						name: "r1".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 0,
					}];
					m.files.clear();
					m.discovery_errors.clear();
					m.discovery_status = Some(status);
					m.is_loading = false;
				});
				let state = model.read_with(cx, |m, _| m.changes_empty_state());
				assert_ne!(
					state,
					Some(ChangesEmpty::Clean),
					"expected non-Clean for {status:?}"
				);
				assert_ne!(
					state,
					Some(ChangesEmpty::CleanPartial),
					"expected non-CleanPartial for {status:?}"
				);
				let expected_msg =
					model.read_with(cx, |m, _| m.discovery_error_msg());
				assert_eq!(
					state,
					Some(ChangesEmpty::ScanFailed(expected_msg.clone())),
					"expected ScanFailed for {status:?}"
				);
				let zh = expected_msg.render(crate::i18n::Locale::ZhTw);
				assert!(
					!zh.to_lowercase().contains("discovery"),
					"{status:?} ZhTw contains discovery: {zh}"
				);
				let en = expected_msg.render(crate::i18n::Locale::En);
				assert!(
					en.to_lowercase().contains("discovery"),
					"{status:?} En should contain discovery: {en}"
				);
			}

			// Incomplete discovery with clean loaded repos -> CleanPartial.
			model.update(cx, |m, _| {
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Incomplete);
			});
			let state = model.read_with(cx, |m, _| m.changes_empty_state());
			assert_ne!(
				state,
				Some(ChangesEmpty::Clean),
				"expected non-Clean for Incomplete"
			);
			assert_eq!(
				state,
				Some(ChangesEmpty::CleanPartial),
				"expected CleanPartial for Incomplete"
			);

			model.update(cx, |m, _| {
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::Clean)
			);
		}

		#[gpui::test]
		fn changes_empty_failed_slot_with_speed_filter_is_no_match(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![
					make_test_repo(tmp.path().join("alpha"), "alpha"),
					make_test_repo(tmp.path().join("beta"), "beta"),
				]);
				m.change_repos = vec![
					crate::ChangeRepo {
						root: tmp.path().join("alpha"),
						name: "alpha".into(),
						state: crate::ChangeRepoState::Failed(
							crate::i18n::Msg::new(
								"error_repo_status",
								["git status failed".to_string()],
							),
						),
						total: 0,
					},
					crate::ChangeRepo {
						root: tmp.path().join("beta"),
						name: "beta".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 0,
					},
				];
				m.files.clear();
				m.chrome.speed = "beta".into();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			// (a) failed alpha + clean beta + speed text hiding alpha -> state is NoMatch (not Clean)
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				Some(ChangesEmpty::NoMatch)
			);

			// (b) same with speed text empty -> failure Note rows non-empty (existing behaviour)
			model.update(cx, |m, _| {
				m.chrome.speed = String::new();
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				None
			);
			assert!(model.read_with(cx, |m, _| {
				m.change_item_rows().iter().any(|r| {
					matches!(r, crate::ui::ChangeItemRow::Note { slot: 0 })
				})
			}));
		}

		#[gpui::test]
		fn changes_empty_truncated_slot_with_hiding_query_is_not_clean(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![
					make_test_repo(tmp.path().join("alpha"), "alpha"),
					make_test_repo(tmp.path().join("beta"), "beta"),
				]);
				m.change_repos = vec![
					crate::ChangeRepo {
						root: tmp.path().join("alpha"),
						name: "alpha".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 10,
					},
					crate::ChangeRepo {
						root: tmp.path().join("beta"),
						name: "beta".into(),
						state: crate::ChangeRepoState::Loaded,
						total: 0,
					},
				];
				m.files.clear(); // 0 rows in range for alpha
				m.chrome.speed = "beta".into(); // query hides alpha
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			// (c) a truncated slot with 0 rows in range + hiding query -> not Clean
			let state = model.read_with(cx, |m, _| m.changes_empty_state());
			assert_ne!(state, Some(ChangesEmpty::Clean));
			assert_eq!(state, Some(ChangesEmpty::NoMatch));
		}

		#[gpui::test]
		fn changes_empty_none_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.change_repos = vec![crate::ChangeRepo {
					root: tmp.path().join("r1"),
					name: "r1".into(),
					state: crate::ChangeRepoState::Loaded,
					total: 1,
				}];
				m.files = vec![make_test_file("a.txt", 0)];
				m.chrome.speed.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.changes_empty_state()),
				None
			);
		}

		#[gpui::test]
		fn log_empty_none_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits = vec![make_test_commit("abc1234")];
			});
			assert_eq!(model.read_with(cx, |m, _| m.log_empty_state()), None);
		}

		#[gpui::test]
		fn log_empty_failed_scan_error_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			let err = crate::i18n::Msg::new(
				"remote_scan_failed",
				["fail".to_string()],
			);
			let worker = snip_remote::RemoteHost::ssh("w");
			let ws = snip_remote::RemoteWorkspace {
				id: "ws1".into(),
				name: "ws1".into(),
				path: "/tmp/ws".into(),
			};
			model.update(cx, |m, _| {
				m.remote.session =
					Some(crate::remote::RemoteSession::new(worker, ws));
				m.commits.clear();
				m.remote.scan_error = Some(err.clone());
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::Failed(err))
			);
		}

		#[gpui::test]
		fn log_empty_scanning_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits.clear();
				m.is_loading = true;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::Scanning)
			);
		}

		#[gpui::test]
		fn log_empty_no_repository_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits.clear();
				m.repos.replace_for_test(Vec::new());
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.discovery_errors.clear();
				m.is_loading = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::NoRepository)
			);
		}

		#[gpui::test]
		fn log_empty_failed_history_error_branch_empty(
			cx: &mut TestAppContext,
		) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits.clear();
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				m.history_error = Some(crate::i18n::Msg::new(
					"error_repo_status",
					["git read failure".to_string()],
				));
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::Failed(crate::i18n::Msg::with_msg_arg(
					"error_history",
					[String::new()],
					0,
					crate::i18n::Msg::new(
						"error_repo_status",
						["git read failure".to_string()]
					)
				)))
			);
		}

		#[gpui::test]
		fn log_empty_loading_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits.clear();
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				m.history_loaded = false;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::Loading)
			);
		}

		#[gpui::test]
		fn log_empty_empty_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits.clear();
				m.repos.replace_for_test(vec![make_test_repo(
					tmp.path().join("r1"),
					"r1",
				)]);
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				m.history_loaded = true;
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::Empty)
			);
		}

		#[gpui::test]
		fn git_log_cmd_c_copies_selected_commit(cx: &mut TestAppContext) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);
			fs::write(r.join("dirty.txt"), "dirty content\n").unwrap();

			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.commits.is_empty()
						&& !m.files.is_empty()
						&& m.display_commits().len() >= 2
				});
				if ready {
					break;
				}
			}

			// Control group: when focus is outside Git Log (default focus on workbench / left tool window),
			// pressing Cmd+C triggers CopySelection, copying the tool window's node (dirty.txt),
			// not a commit payload.
			clip::write_text("before").unwrap();
			cx.simulate_keystrokes("cmd-c");
			for _ in 0..100 {
				settle(cx);
				if model.read_with(cx, |m, _| !m.is_copying) {
					break;
				}
			}
			let text_ctrl = clip::read_text().unwrap();
			assert!(
				!text_ctrl.starts_with("// snip-sync commits v1"),
				"control group should not copy commits: {text_ctrl}"
			);
			assert!(
				text_ctrl.contains("dirty.txt"),
				"control group should copy tool window node: {text_ctrl}"
			);

			// User action: click the commit row via debug_bounds to focus Git Log and select the commit.
			let target_sha =
				model.read_with(cx, |m, _| m.display_commits()[0].sha.clone());
			let row_bounds = cx
				.debug_bounds("log-subject:0")
				.expect("commit row at index 0 drawn");
			cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
			settle(cx);

			// Verify focus entered Git Log and target commit is selected.
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.selected_commit.as_deref(),
					Some(target_sha.as_str())
				);
			});

			// Now press Cmd+C while Git Log has focus.
			clip::write_text("before_commit_copy").unwrap();
			cx.simulate_keystrokes("cmd-c");
			for _ in 0..100 {
				settle(cx);
				if model.read_with(cx, |m, _| !m.is_copying) {
					break;
				}
			}

			let text = clip::read_text().unwrap();
			assert!(
				text.starts_with("// snip-sync commits v1"),
				"expected commit payload starting with '// snip-sync commits v1', got: {text}"
			);
			assert!(
				text.contains("commit c1"),
				"expected payload to contain clicked commit 'commit c1', got: {text}"
			);
			assert!(
				!text.contains("base.txt"),
				"expected payload to be only the selected commit, got: {text}"
			);

			// Verify that when a TextInput in the Log toolbar (e.g. log_search_input) has focus,
			// Cmd+C copies the selected text, not commits.
			cx.update(|window, app| {
				model.update(app, |m, cx| {
					m.log_search_input.update(cx, |i, cx| {
						i.set_text("find_something", cx);
					});
					window.focus(&m.log_search_input.read(cx).handle());
				});
			});
			settle(cx);
			cx.simulate_keystrokes("cmd-a");
			settle(cx);
			cx.simulate_keystrokes("cmd-c");
			settle(cx);
			assert_eq!(
				cx.read_from_clipboard().and_then(|i| i.text()),
				Some("find_something".to_string())
			);
		}

		/// A long error toast wraps inside the card and never overflows the window.
		#[gpui::test]
		fn long_error_toast_wraps_inside_card_in_narrow_window(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let win_w = 480.;
			let win_h = 400.;
			cx.simulate_resize(gpui::size(gpui::px(win_w), gpui::px(win_h)));
			let long_error = "commits are not contiguous: following first parents back from 0123456789abcdef0123456789abcdef01234567, 89abcdef0123456789abcdef0123456789abcdef01";
			model.update(cx, |m, cx| {
				m.show_toast(
					false,
					crate::i18n::Msg::new(
						"paste_err_plan",
						[long_error.to_string()],
					),
					cx,
				);
			});
			for _ in 0..2 {
				// ALLOWED-TEST-REFRESH: test harness needs a redraw after showing toast to measure multiline bounds
				cx.update(|window, _| window.refresh());
				settle(cx);
			}
			let toast =
				cx.debug_bounds("copy-toast").expect("copy-toast drawn");
			let left = f32::from(toast.origin.x);
			let right = left + f32::from(toast.size.width);
			let height = f32::from(toast.size.height);
			assert!(left >= 0., "toast left off-screen: {left}");
			assert!(
				right <= win_w,
				"toast right exceeds window width: {right} > {win_w}"
			);
			assert!(
				height > 40.,
				"toast should wrap to more than one line: height={height}"
			);
		}

		fn bounds_intersect(
			a: gpui::Bounds<gpui::Pixels>,
			b: gpui::Bounds<gpui::Pixels>,
		) -> bool {
			let a_left = f32::from(a.origin.x);
			let a_right = a_left + f32::from(a.size.width);
			let a_top = f32::from(a.origin.y);
			let a_bottom = a_top + f32::from(a.size.height);

			let b_left = f32::from(b.origin.x);
			let b_right = b_left + f32::from(b.size.width);
			let b_top = f32::from(b.origin.y);
			let b_bottom = b_top + f32::from(b.size.height);

			a_left < b_right
				&& a_right > b_left
				&& a_top < b_bottom
				&& a_bottom > b_top
		}

		fn assert_no_filter_chips_intersect_actions(
			cx: &mut VisualTestContext,
		) {
			let visible_chips = [
				"log-filter-repo",
				"log-filter-paths",
				"log-filter-branch",
				"log-filter-user",
				"log-filter-date",
				"log-filter-more",
			];
			let action_buttons = [
				"btn-log-refresh",
				"btn-head",
				"btn-compare",
				"btn-copy-commits",
			];
			for chip_id in visible_chips {
				if let Some(chip_b) = cx.debug_bounds(chip_id) {
					for btn_id in action_buttons {
						if let Some(btn_b) = cx.debug_bounds(btn_id) {
							assert!(
								!bounds_intersect(chip_b, btn_b),
								"chip {chip_id} intersects button {btn_id}: chip={chip_b:?}, btn={btn_b:?}"
							);
						}
					}
				}
			}
		}

		/// The same rule when only the user's gestures set the focus: click a
		/// log row, click a popover opener (the click moves the focus to the
		/// root), press Alt+1 (the tree takes the focus, the menu stays
		/// open), then Escape (#212). The selector shortcuts move the focus
		/// into the filter field, whose own Dismiss would close the selector
		/// without the root rule, so they are not used here.
		#[gpui::test]
		fn escape_closes_a_menu_opened_by_click_after_a_click(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);
			repo(ws.path(), "other-repo", &[]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 900., 600.);
			for _ in 0..50 {
				settle(cx);
				if model.read_with(cx, |m, _| {
					!m.commits.is_empty() && m.log_width.get() > 0.
				}) {
					break;
				}
			}
			settle(cx);
			let mut cases = 0;
			for opener in [
				"btn-workspace-menu",
				"btn-repo-selector",
				"btn-ref-selector",
				"log-filter-more",
				"log-filter-repo",
			] {
				let row =
					cx.debug_bounds("log-subject:0").expect("a commit row");
				cx.simulate_click(row.center(), gpui::Modifiers::none());
				settle(cx);
				click(cx, opener);
				settle(cx);
				cx.simulate_keystrokes("alt-1");
				settle(cx);
				let (open, focused) = cx.update(|window, cx| {
					let m = model.read(cx);
					(
						m.overlays.is_open(
							crate::overlay::OverlayKind::WorkspaceMenu,
						) || m.overlays.log_menu().is_some()
							|| m.overlays.selector().is_some(),
						m.tree_focus.is_focused(window),
					)
				});
				assert!(open, "{opener} opened");
				assert!(
					focused,
					"Alt+1 moves the focus to the tree with {opener} open"
				);
				cx.simulate_keystrokes("escape");
				settle(cx);
				model.read_with(cx, |m, _| {
					assert!(
						!(m.overlays.is_open(
							crate::overlay::OverlayKind::WorkspaceMenu
						) || m.overlays.log_menu().is_some()
							|| m.overlays.selector().is_some()),
						"Escape closes {opener} opened after a log click"
					);
				});
				cases += 1;
			}
			assert_eq!(cases, 5);
		}

		/// At narrow window width (900x600), overflowing log filter chips do
		/// not overlap action buttons; the overflow chip lists hidden filters,
		/// and clicking the User item opens the User menu without triggering refresh.
		#[gpui::test]
		fn log_filter_narrow_window_overflow_and_user_menu(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 900., 600.);
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.commits.is_empty() && m.log_width.get() > 0.
				});
				if ready {
					break;
				}
			}
			settle(cx);

			let more_b = cx
				.debug_bounds("log-filter-more")
				.expect("log-filter-more drawn at 900w");

			// User and Date chips must not be directly visible at narrow width
			assert!(
				cx.debug_bounds("log-filter-user").is_none(),
				"user chip should be hidden into overflow at narrow width"
			);
			assert!(
				cx.debug_bounds("log-filter-date").is_none(),
				"date chip should be hidden into overflow at narrow width"
			);

			// No visible chip intersects refresh or any other action button
			assert_no_filter_chips_intersect_actions(cx);

			// Click overflow chip to reveal hidden filters
			cx.simulate_click(more_b.center(), gpui::Modifiers::none());
			settle(cx);

			// User item should now be present in the overflow menu
			let user_item_b = cx
				.debug_bounds("log-filter-more:user")
				.expect("User item in overflow menu");
			cx.simulate_click(user_item_b.center(), gpui::Modifiers::none());
			settle(cx);

			// Verify User menu is now open, and refresh was not triggered
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.overlays.log_menu(),
					Some(crate::ui::LogMenu::User),
					"User menu should be open"
				);
			});
		}

		/// At a wide width all four filter chips render and no overflow chip exists.
		#[gpui::test]
		fn log_filter_wide_window_shows_all_chips_and_no_overflow(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);

			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(752.)));
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.commits.is_empty() && m.log_width.get() > 1000.
				});
				if ready {
					break;
				}
			}
			settle(cx);

			assert!(
				cx.debug_bounds("log-filter-more").is_none(),
				"overflow chip should not exist at wide width"
			);
			assert!(
				cx.debug_bounds("log-filter-paths").is_some()
					|| cx.debug_bounds("log-filter-repo").is_some(),
				"repo/paths chip should exist at wide width"
			);
			assert!(
				cx.debug_bounds("log-filter-branch").is_some(),
				"branch chip should exist at wide width"
			);
			assert!(
				cx.debug_bounds("log-filter-user").is_some(),
				"user chip should exist at wide width"
			);
			assert!(
				cx.debug_bounds("log-filter-date").is_some(),
				"date chip should exist at wide width"
			);
		}

		/// The default 1080×720 window keeps every chip, also after the
		/// window was narrow first: a chip that was hidden still has its
		/// real width (native smoke clicks `log-filter-user` at this size).
		#[gpui::test]
		fn log_filter_chips_return_at_the_default_window_size(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);

			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			for (w, h) in [(700., 632.), (1080., 752.)] {
				cx.simulate_resize(gpui::size(gpui::px(w), gpui::px(h)));
				for _ in 0..50 {
					settle(cx);
					let ready = model.read_with(cx, |m, _| {
						!m.commits.is_empty()
							&& (m.log_width.get() - (w - 28.)).abs() < 40.
					});
					if ready {
						break;
					}
				}
				settle(cx);
			}
			for id in [
				"log-filter-paths",
				"log-filter-branch",
				"log-filter-user",
				"log-filter-date",
			] {
				assert!(cx.debug_bounds(id).is_some(), "{id} hidden at 1080");
			}
			// The test frame keeps stale ids across resizes; ask the split.
			let hidden =
				model.read_with(cx, |m, _| m.log_filter_split().1.len());
			assert_eq!(hidden, 0, "no chip goes to the overflow at 1080");
		}

		/// One resize to 900×600 settles the chips by itself: the frame that
		/// meets the new width schedules the next one, with no other input.
		/// (A real round saw the old layout until the next click.)
		#[gpui::test]
		fn log_filter_chips_follow_a_resize_without_input(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			for name in ["repo-a", "repo-b"] {
				let r = repo(ws.path(), name, &[]);
				fs::write(r.join("c1.txt"), "c1\n").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", "c1"]);
			}
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(752.)));
			for _ in 0..50 {
				settle(cx);
				if model.read_with(cx, |m, _| m.log_width.get() > 1000.) {
					break;
				}
			}
			let wide =
				model.read_with(cx, |m, _| m.log_filter_layout.last_mask.get());
			assert_eq!(wide, 0b1111, "all chips fit at 1080");

			cx.simulate_resize(gpui::size(gpui::px(700.), gpui::px(632.)));
			settle(cx);
			let (drawn, split_for, measured) = model.read_with(cx, |m, _| {
				let layout = &m.log_filter_layout;
				(
					layout.last_mask.get(),
					layout.used_w.get(),
					layout.container_w.get(),
				)
			});
			assert_ne!(drawn, 0b1111, "700 wide must not fit every chip");
			assert!(
				(split_for - measured).abs() < 0.5,
				"the drawn split is for the bar's new width: {split_for} vs {measured}"
			);
		}

		/// At the default 1080x720 window with the details pane open, an
		/// active repository and an active branch both stay on the bar:
		/// their values are truncated before either goes into the overflow
		/// chip (#205).
		#[gpui::test]
		fn two_active_log_chips_stay_on_the_bar_at_the_default_window(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			for name in ["repo-01-core", "repo-02-api"] {
				let r = repo(ws.path(), name, &[]);
				fs::write(r.join("c1.txt"), "c1\n").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", "c1"]);
				git(&r, &["branch", "topic-03"]);
			}
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1080., 752.);
			model.update(cx, |m, cx| {
				m.log_details_visible = true;
				cx.notify();
			});
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.refs.is_empty() && m.log_width.get() > 1000.
				});
				if ready {
					break;
				}
			}
			click(cx, "log-filter-branch");
			click(cx, "log-branch-group:refs_local");
			click(cx, "log-branch:refs/heads/topic-03");
			click(cx, "log-filter-repo");
			click(cx, "log-repo:repo-01-core");
			for _ in 0..5 {
				settle(cx);
			}
			let (visible, hidden, repo_value) = model.read_with(cx, |m, _| {
				let (on, off) = m.log_filter_split();
				(on, off, m.log_scope_value())
			});
			assert_eq!(
				repo_value.as_deref(),
				Some("repo-01-core"),
				"the repo chip shows the repo name"
			);
			assert!(
				visible.contains(&crate::ui::LogMenu::Repo)
					&& visible.contains(&crate::ui::LogMenu::Branch),
				"both active chips stay on the bar: shown {visible:?}, \
				 overflow {hidden:?}"
			);
			for id in ["log-filter-repo", "log-filter-branch"] {
				assert!(cx.debug_bounds(id).is_some(), "{id} drawn");
			}
		}

		/// The chips follow a resize that keeps the split mask: the bar
		/// widened, so the cut values get their room back with no input
		/// (#205).
		#[gpui::test]
		fn log_chips_follow_a_resize_that_keeps_the_mask(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			for name in ["repo-01-core", "repo-02-api"] {
				let r = repo(ws.path(), name, &[]);
				fs::write(r.join("c1.txt"), "c1\n").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", "c1"]);
				git(&r, &["branch", "topic-03"]);
			}
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1080., 752.);
			model.update(cx, |m, cx| {
				m.log_details_visible = true;
				cx.notify();
			});
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.refs.is_empty() && m.log_width.get() > 1000.
				});
				if ready {
					break;
				}
			}
			click(cx, "log-filter-branch");
			click(cx, "log-branch-group:refs_local");
			click(cx, "log-branch:refs/heads/topic-03");
			click(cx, "log-filter-repo");
			click(cx, "log-repo:repo-01-core");
			for _ in 0..5 {
				settle(cx);
			}
			let chip_w = |cx: &mut VisualTestContext, id: &'static str| {
				cx.debug_bounds(id)
					.map(|b| f32::from(b.size.width))
					.unwrap_or(0.)
			};
			let mut before = [
				chip_w(cx, "log-filter-repo"),
				chip_w(cx, "log-filter-branch"),
			];
			for w in [1110., 1140.] {
				cx.simulate_resize(gpui::size(gpui::px(w), gpui::px(752.)));
				for _ in 0..5 {
					settle(cx);
				}
				let (split_for, measured, mask) =
					model.read_with(cx, |m, _| {
						let l = &m.log_filter_layout;
						(l.used_w.get(), l.container_w.get(), l.last_mask.get())
					});
				assert_eq!(mask & 0b11, 0b11, "both chips stay on the bar");
				assert!(
					(split_for - measured).abs() < 0.5,
					"at {w}: the chips were split for {split_for}, the bar is {measured}"
				);
				let now = [
					chip_w(cx, "log-filter-repo"),
					chip_w(cx, "log-filter-branch"),
				];
				assert!(
					now[0] >= before[0] - 0.5 && now[1] >= before[1] - 0.5,
					"a wider bar never shrinks a chip: {before:?} -> {now:?}"
				);
				before = now;
			}
		}

		/// The same as above in a remote workspace (#205): repositories nest
		/// under the shared folder, and the chip names the repo without the
		/// `host ▸ path ›` prefix, so both active chips stay on the bar.
		#[gpui::test]
		fn two_active_log_chips_stay_on_the_bar_in_a_remote_workspace(
			cx: &mut TestAppContext,
		) {
			let _guard = remote_lock();
			let ws = tempfile::tempdir().unwrap();
			let shared = ws.path().join("shared");
			fs::create_dir(&shared).unwrap();
			for name in ["repo-01-core", "repo-02-api"] {
				let r = repo(&shared, name, &[]);
				fs::write(r.join("c1.txt"), "c1\n").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", "c1"]);
				git(&r, &["branch", "topic-03"]);
			}
			let (model, cx, _worker) = open_remote(
				cx,
				&shared,
				snip_remote::WorkerOptions {
					name: "test-worker".into(),
					..Default::default()
				},
			);
			cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(752.)));
			model.update(cx, |m, cx| {
				m.log_details_visible = true;
				cx.notify();
			});
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					m.repos.len() >= 2
						&& !m.refs.is_empty()
						&& m.log_width.get() > 1000.
				});
				if ready {
					break;
				}
			}
			click(cx, "log-filter-branch");
			click(cx, "log-branch-group:refs_local");
			click(cx, "log-branch:refs/heads/topic-03");
			click(cx, "log-filter-repo");
			click(cx, "log-repo:repo-01-core");
			for _ in 0..5 {
				settle(cx);
			}
			let (visible, hidden, repo_value) = model.read_with(cx, |m, _| {
				let (on, off) = m.log_filter_split();
				(on, off, m.log_scope_value())
			});
			assert_eq!(repo_value.as_deref(), Some("repo-01-core"));
			assert!(
				visible.contains(&crate::ui::LogMenu::Repo)
					&& visible.contains(&crate::ui::LogMenu::Branch),
				"both active chips stay on the bar: shown {visible:?}, \
				 overflow {hidden:?}"
			);
		}

		/// At a narrow window width with widened details pane and an active long user filter,
		/// no filter chip intersects action buttons and user filter is reachable.
		#[gpui::test]
		fn log_filter_narrow_window_widened_details_and_long_user_filter(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 900., 600.);
			model.update(cx, |m, cx| {
				m.log_details_visible = true;
				m.log_details_w = 400.;
				m.log_filter.author =
					Some("very.long.contributor.name@example.com".to_string());
				cx.notify();
			});
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.commits.is_empty() && m.log_width.get() > 0.
				});
				if ready {
					break;
				}
			}
			settle(cx);

			assert_no_filter_chips_intersect_actions(cx);

			if let Some(user_b) = cx.debug_bounds("log-filter-user") {
				cx.simulate_click(user_b.center(), gpui::Modifiers::none());
			} else {
				let more_b = cx
					.debug_bounds("log-filter-more")
					.expect("more chip must exist when user chip is hidden");
				cx.simulate_click(more_b.center(), gpui::Modifiers::none());
				settle(cx);
				let user_item_b = cx
					.debug_bounds("log-filter-more:user")
					.expect("User item in overflow menu");
				cx.simulate_click(
					user_item_b.center(),
					gpui::Modifiers::none(),
				);
			}
			settle(cx);

			model.read_with(cx, |m, _| {
				assert_eq!(
					m.overlays.log_menu(),
					Some(crate::ui::LogMenu::User),
					"User menu should be open"
				);
			});
		}

		/// At a narrow window width in English locale, no filter chip intersects action buttons.
		#[gpui::test]
		fn log_filter_narrow_window_english_locale(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 900., 600.);
			model.update(cx, |m, cx| {
				m.locale = crate::i18n::Locale::En;
				cx.notify();
			});
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.commits.is_empty() && m.log_width.get() > 0.
				});
				if ready {
					break;
				}
			}
			settle(cx);

			assert_no_filter_chips_intersect_actions(cx);

			let more_b = cx
				.debug_bounds("log-filter-more")
				.expect("more chip must exist at 900w English");
			cx.simulate_click(more_b.center(), gpui::Modifiers::none());
			settle(cx);
			let user_item_b = cx
				.debug_bounds("log-filter-more:user")
				.expect("User item in overflow menu");
			cx.simulate_click(user_item_b.center(), gpui::Modifiers::none());
			settle(cx);

			model.read_with(cx, |m, _| {
				assert_eq!(
					m.overlays.log_menu(),
					Some(crate::ui::LogMenu::User),
					"User menu should be open"
				);
			});
		}

		/// At a wide window width with branches and details panes hidden, all four chips
		/// are visible and no overflow chip exists.
		#[gpui::test]
		fn log_filter_wide_window_hidden_panes_shows_all_chips(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "commit c1"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1200., 752.);
			model.update(cx, |m, cx| {
				m.log_branches_visible = false;
				m.log_details_visible = false;
				cx.notify();
			});
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					!m.commits.is_empty() && m.log_width.get() > 1000.
				});
				if ready {
					break;
				}
			}
			settle(cx);

			assert!(
				cx.debug_bounds("log-filter-more").is_none(),
				"overflow chip should not exist when all fit"
			);
			assert!(
				cx.debug_bounds("log-filter-paths").is_some()
					|| cx.debug_bounds("log-filter-repo").is_some(),
				"repo/paths chip should exist"
			);
			assert!(
				cx.debug_bounds("log-filter-branch").is_some(),
				"branch chip should exist"
			);
			assert!(
				cx.debug_bounds("log-filter-user").is_some(),
				"user chip should exist"
			);
			assert!(
				cx.debug_bounds("log-filter-date").is_some(),
				"date chip should exist"
			);
		}

		/// The branch chip shows the selected repo's HEAD while the Log is
		/// filtered to another branch (#188).
		#[gpui::test]
		fn header_branch_chip_keeps_head_while_a_branch_filter_is_active(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			git(&r, &["checkout", "-b", "side"]);
			fs::write(r.join("side.txt"), "side content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "side commit"]);
			git(&r, &["checkout", "main"]);
			fs::write(r.join("main.txt"), "main content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "main head commit"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1200., 752.);
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					m.display_commits().len() >= 3 && !m.refs.is_empty()
				});
				if ready {
					break;
				}
			}
			model.update(cx, |m, cx| {
				m.filter_by_ref(Some("side".into()), cx);
			});
			for _ in 0..50 {
				settle(cx);
				let filtered = model.read_with(cx, |m, _| {
					m.active_ref_filter.as_deref() == Some("side")
						&& !m.display_commits().is_empty()
				});
				if filtered {
					break;
				}
			}
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.active_ref_filter.as_deref(), Some("side"));
				assert_eq!(m.header_ref_label(), "main");
			});
		}

		/// A filter that hides the selected commit drops that commit's files
		/// and details with the selection (#188).
		#[gpui::test]
		fn a_filter_that_hides_the_selected_commit_clears_its_files(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			git(&r, &["checkout", "-b", "side"]);
			fs::write(r.join("side.txt"), "side content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "side commit"]);
			git(&r, &["checkout", "main"]);
			fs::write(r.join("main.txt"), "main content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "main head commit"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1200., 752.);
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					m.display_commits().len() >= 3 && !m.refs.is_empty()
				});
				if ready {
					break;
				}
			}
			settle(cx);

			// The HEAD commit is main-only, so the side filter hides it. Same-
			// second commits can list either first, so find its row by subject.
			let ix = model
				.read_with(cx, |m, _| {
					m.display_commits()
						.iter()
						.position(|c| c.subject == "main head commit")
				})
				.expect("main head commit listed");
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			let row = cx.debug_bounds(id).expect("commit row");
			cx.simulate_click(row.center(), gpui::Modifiers::none());
			for _ in 0..50 {
				settle(cx);
				let loaded = model.read_with(cx, |m, _| {
					!m.commit_view.commit_files.is_empty()
						&& m.preview.is_some()
				});
				if loaded {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				assert!(
					!m.commit_view.commit_files.is_empty(),
					"files must load first"
				);
				assert!(m.preview.is_some(), "the diff must load first");
			});

			model.update(cx, |m, cx| {
				m.filter_by_ref(Some("side".into()), cx);
			});
			// The reload runs on a worker thread: poll in real time, bounded
			// (a loaded test run can take several seconds to reread the log).
			for _ in 0..1200 {
				settle(cx);
				let cleared =
					model.read_with(cx, |m, _| m.selected_commit.is_none());
				if cleared {
					break;
				}
				std::thread::sleep(std::time::Duration::from_millis(25));
			}
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.selected_commit.is_none(), "selection kept");
				assert!(
					m.commit_view.commit_files.is_empty(),
					"stale files kept"
				);
				assert!(
					m.commit_view.commit_details.is_none(),
					"stale details kept"
				);
				assert!(
					!commit_preview_shown(m),
					"stale commit diff kept after the branch filter",
				);
			});
		}

		/// True while the preview is a diff, file or compare from the Log.
		fn commit_preview_shown(m: &WorkbenchModel) -> bool {
			use crate::reader::PreviewSource;
			matches!(
				m.preview.as_ref().map(|p| &p.source),
				Some(PreviewSource::CommitDiff { .. })
					| Some(PreviewSource::CommitFile { .. })
					| Some(PreviewSource::Compare { .. })
			)
		}

		/// Changing the Log's repository scope drops the selected commit with
		/// its files and diff, as a branch filter does (#202).
		#[gpui::test]
		fn a_repo_filter_change_clears_the_selected_commit_and_its_diff(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("a.txt"), "alpha content\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle(cx);
			let ix = model
				.read_with(cx, |m, _| {
					m.display_commits()
						.iter()
						.position(|c| c.subject == "alpha commit")
				})
				.expect("alpha commit listed");
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			click(cx, id);
			for _ in 0..50 {
				settle(cx);
				let loaded = model.read_with(cx, |m, _| {
					!m.commit_view.commit_files.is_empty()
						&& commit_preview_shown(m)
				});
				if loaded {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				assert!(
					!m.commit_view.commit_files.is_empty(),
					"files must load first"
				);
				assert!(commit_preview_shown(m), "the diff must load first");
			});

			// The user's gestures: the repository chip, then beta.
			click(cx, "log-filter-repo");
			click(cx, "log-repo:beta");
			for _ in 0..1200 {
				settle(cx);
				let cleared =
					model.read_with(cx, |m, _| m.selected_commit.is_none());
				if cleared {
					break;
				}
				std::thread::sleep(std::time::Duration::from_millis(25));
			}
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.selected_commit.is_none(), "selection kept");
				assert!(
					m.commit_view.commit_files.is_empty(),
					"stale files kept"
				);
				assert!(
					m.commit_view.commit_details.is_none(),
					"stale details kept"
				);
				assert!(
					!commit_preview_shown(m),
					"stale commit diff kept after the repository filter",
				);
			});
		}

		/// Every member of the commit view goes with the selection, whichever
		/// path dropped it (#230). A multi-selection fills `commit_file_origin`;
		/// a repository filter that hides it used to leave that behind.
		#[gpui::test]
		fn commit_view_resets_with_the_selection(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			repo(ws.path(), "beta", &[]);
			for i in 0..3 {
				fs::write(alpha.join(format!("a{i}.txt")), "x\n").unwrap();
				git(&alpha, &["add", "."]);
				git(&alpha, &["commit", "-q", "-m", &format!("alpha {i}")]);
			}
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 5)
			});
			// Two rows of one repository: the merged log sorts by date, so
			// rows 0 and 1 may belong to different repositories, and a
			// toggle across them is refused.
			let (i, j) = model.read_with(cx, |m, _| {
				let rows = m.display_commits();
				let feeds: Vec<_> = rows
					.iter()
					.map(|c| crate::multi_log::split_id(&c.sha).1)
					.collect();
				(0..feeds.len())
					.flat_map(|i| (i + 1..feeds.len()).map(move |j| (i, j)))
					.find(|&(i, j)| feeds[i] == feeds[j])
					.expect("two rows of one repository")
			});
			let at = |cx: &mut VisualTestContext,
			          ix: usize,
			          mods: gpui::Modifiers| {
				cx.run_until_parked();
				let id: &'static str =
					Box::leak(format!("log-subject:{ix}").into_boxed_str());
				let row = cx.debug_bounds(id).expect("a commit row");
				cx.simulate_click(row.center(), mods);
			};
			at(cx, i, gpui::Modifiers::none());
			settle_until(cx, "the first commit's files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading && !m.commit_view.commit_files.is_empty()
				})
			});
			at(cx, j, gpui::Modifiers::secondary_key());
			settle_until(cx, "the multi-selection's files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& !m.commit_view.commit_file_origin.is_empty()
				})
			});
			model.read_with(cx, |m, _| {
				assert_eq!(m.log_selected.len(), 2, "a multi-selection");
			});

			click(cx, "log-filter-repo");
			click(cx, "log-repo:beta");
			settle_until(cx, "the selection to leave", |cx| {
				model.read_with(cx, |m, _| {
					m.selected_commit.is_none() && m.history_loaded
				})
			});
			model.read_with(cx, |m, _| {
				assert!(m.selected_commit.is_none(), "selection kept");
				assert!(m.log_selected.is_empty(), "multi-selection kept");
				assert!(
					m.commit_view.is_reset(),
					"a commit view member stayed"
				);
			});
		}

		/// A file the retained budget refuses leaves the previous text shown
		/// and the selection naming it again, so the commit view must come
		/// back with it (#230): the shown commit diff still belongs to its
		/// own repository, not to the repository the refused file was in.
		#[gpui::test]
		fn a_file_the_budget_refuses_keeps_the_commit_view_of_the_shown_diff(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			let beta = repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("big.txt"), "x".repeat(128 * 1024)).unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);
			fs::write(beta.join("b.txt"), "beta content\n").unwrap();
			git(&beta, &["add", "."]);
			git(&beta, &["commit", "-q", "-m", "beta commit"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| {
					["alpha commit", "beta commit"].iter().all(|s| {
						m.display_commits().iter().any(|c| c.subject == *s)
					})
				})
			});
			let ix = model.read_with(cx, |m, _| {
				m.display_commits()
					.iter()
					.position(|c| c.subject == "beta commit")
					.expect("beta commit listed")
			});
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			click(cx, id);
			settle_until(cx, "the beta commit's diff and files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& !m.commit_view.commit_files.is_empty()
						&& commit_preview_shown(m)
				})
			});
			let shown_root = model.read_with(cx, |m, _| {
				let root = m.preview_root().expect("a preview root");
				assert!(root.ends_with("beta"), "the commit's repo: {root:?}");
				root
			});

			// Nearly all of the retained budget is held by a queued paste,
			// so the 128 KiB file cannot replace the commit diff.
			model.update(cx, |m, _| {
				m.paste
					.enqueue(crate::paste::PasteRequest::Clipboard {
						text: "n".repeat(
							crate::paste::MAX_RETAINED_PREVIEW_BYTES
								- 32 * 1024,
						),
						dest: PathBuf::new(),
						roots: Vec::new(),
						generation: 0,
						remote: None,
					})
					.expect("the queued paste fits");
			});
			model.update(cx, |m, cx| {
				m.select_file_in(
					Some(alpha),
					"big.txt",
					snip_core::transfer::SourceKind::File,
					cx,
				);
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(commit_preview_shown(m), "the commit diff stays");
				assert!(m.selected_commit.is_some(), "selection restored");
				assert!(
					m.commit_view.commit_details.is_some(),
					"details cleared while the selection came back",
				);
				assert!(
					!m.commit_view.commit_files.is_empty(),
					"files cleared while the selection came back",
				);
				assert_eq!(
					m.preview_root().as_deref(),
					Some(shown_root.as_path()),
					"the shown diff must keep its own repository",
				);
			});
		}

		/// Two repos: alpha (open) with a small file, beta with a 40-line
		/// file whose second commit changes line 20, so its diff has folds.
		/// The beta commit's diff is shown from the merged Log.
		fn open_with_beta_commit_shown(
			cx: &mut TestAppContext,
		) -> (
			Entity<WorkbenchModel>,
			&mut VisualTestContext,
			tempfile::TempDir,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			let beta = repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("a.txt"), "alpha\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);
			let body: String =
				(1..=40).map(|n| format!("line {n}\n")).collect();
			fs::write(beta.join("f.txt"), &body).unwrap();
			git(&beta, &["add", "."]);
			git(&beta, &["commit", "-q", "-m", "beta add"]);
			fs::write(beta.join("f.txt"), body.replace("line 20", "LINE 20"))
				.unwrap();
			git(&beta, &["add", "."]);
			git(&beta, &["commit", "-q", "-m", "beta edit"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| {
					["alpha commit", "beta edit"].iter().all(|s| {
						m.display_commits().iter().any(|c| c.subject == *s)
					})
				})
			});
			let ix = model.read_with(cx, |m, _| {
				m.display_commits()
					.iter()
					.position(|c| c.subject == "beta edit")
					.expect("beta commit listed")
			});
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			click(cx, id);
			settle_until(cx, "the beta commit's diff and files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& !m.commit_view.commit_files.is_empty()
						&& commit_preview_shown(m)
				})
			});
			model.read_with(cx, |m, _| {
				assert!(
					m.repo_root().is_some_and(|r| r.ends_with("alpha")),
					"alpha is the open repo: {:?}",
					m.repo_root()
				);
				assert!(
					m.preview_root().is_some_and(|r| r.ends_with("beta")),
					"the commit's repo: {:?}",
					m.preview_root()
				);
			});
			(model, cx, ws)
		}

		/// A Project file of the open repo is clicked while the beta commit's
		/// diff is still on screen: the diff keeps naming its own repository
		/// in the breadcrumb, and Expand All reads from it, until the new
		/// read lands (#228 #229).
		#[gpui::test]
		fn a_diff_still_shown_while_a_file_loads_keeps_its_repository(
			cx: &mut TestAppContext,
		) {
			let (model, cx, _ws) = open_with_beta_commit_shown(cx);
			open_project_tool(&model, cx);
			let delay = std::time::Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(delay));
			click_probed(&model, cx, "tree-row:a.txt");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.preview_loading, "the read is held back");
				assert!(commit_preview_shown(m), "the beta diff stays");
				let root = m.preview_root().expect("a preview root");
				assert!(
					root.ends_with("beta"),
					"the shown diff's repo: {root:?}"
				);
				let crumbs = m.source_labels().1;
				assert!(
					crumbs.contains("beta") && !crumbs.contains("alpha"),
					"the breadcrumb names the open repo: {crumbs}"
				);
			});
			let _ = crate::test_log::take();
			click_probed(&model, cx, "btn-expand-folds");
			settle(cx);
			let lines = crate::test_log::take();
			assert!(
				lines.iter().any(|l| l.starts_with("[APP:FOLD_EXPANDED: ")),
				"Expand All read from the wrong repository: {lines:#?}"
			);
			assert!(
				!lines.iter().any(|l| l.starts_with("[APP:FOLD_REFUSED: ")),
				"{lines:#?}"
			);
		}

		/// A diff keeps its repository through Expand All, which rebuilds
		/// the preview: a Changes row of a repo that is not the open one
		/// (#228 #229).
		#[gpui::test]
		fn a_diff_keeps_its_repository_after_expand_all(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			let beta = repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("a.txt"), "alpha\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);
			let body: String =
				(1..=40).map(|n| format!("line {n}\n")).collect();
			fs::write(beta.join("f.txt"), &body).unwrap();
			git(&beta, &["add", "."]);
			git(&beta, &["commit", "-q", "-m", "beta add"]);
			fs::write(beta.join("f.txt"), body.replace("line 20", "LINE 20"))
				.unwrap();
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "beta's change", |cx| {
				model.read_with(cx, |m, _| {
					m.files.iter().any(|f| f.path == "f.txt")
				})
			});
			click_probed(&model, cx, "change-repo-toggle:unstaged:beta");
			settle(cx);
			click_probed(&model, cx, "change-row@beta:unstaged:f.txt");
			settle_until(cx, "beta's diff", |cx| {
				model.read_with(cx, |m, _| {
					m.preview.is_some() && !m.preview_loading
				})
			});
			let beta_root = model.read_with(cx, |m, _| {
				let root = m.preview_root().expect("a preview root");
				assert!(root.ends_with("beta"), "{root:?}");
				assert!(
					m.repo_root().is_some_and(|r| r.ends_with("alpha")),
					"alpha is the open repo"
				);
				root
			});
			click_probed(&model, cx, "btn-expand-folds");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(
					m.preview.as_ref().is_some_and(|p| p.lines.len() > 40),
					"the folds were expanded"
				);
				assert_eq!(
					m.preview_root().as_deref(),
					Some(beta_root.as_path()),
					"the rebuilt preview forgot its repository"
				);
			});
		}

		/// Opens the Project tool window with the rail button, as a user does.
		fn open_project_tool(
			model: &Entity<WorkbenchModel>,
			cx: &mut VisualTestContext,
		) {
			let open = model.read_with(cx, |m, _| {
				m.active_tab == WorkbenchTab::FileExplorer
			});
			if !open {
				click_probed(model, cx, "rail-project");
			}
			settle_until(cx, "the Project tree", |cx| {
				model.read_with(cx, |m, _| m.file_tree.is_some())
			});
		}

		/// Two file reads in flight, the last one refused by the budget: the
		/// second read must not capture the emptied selection the first one
		/// left behind. The snapshot belongs to the diff still shown, not to
		/// one request, so the refusal puts the real selection back.
		#[gpui::test]
		fn a_superseded_file_read_keeps_the_snapshot_of_the_shown_diff(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			let beta = repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("big.txt"), "x".repeat(128 * 1024)).unwrap();
			fs::write(alpha.join("small.txt"), "small\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);
			fs::write(beta.join("b.txt"), "beta content\n").unwrap();
			git(&beta, &["add", "."]);
			git(&beta, &["commit", "-q", "-m", "beta commit"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| {
					["alpha commit", "beta commit"].iter().all(|s| {
						m.display_commits().iter().any(|c| c.subject == *s)
					})
				})
			});
			let ix = model.read_with(cx, |m, _| {
				m.display_commits()
					.iter()
					.position(|c| c.subject == "beta commit")
					.expect("beta commit listed")
			});
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			click(cx, id);
			settle_until(cx, "the beta commit's diff and files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& !m.commit_view.commit_files.is_empty()
						&& commit_preview_shown(m)
				})
			});
			let shown_root = model.read_with(cx, |m, _| {
				m.preview_root().expect("a preview root")
			});
			let delay = std::time::Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(delay));
			open_project_tool(&model, cx);
			// Two reads by the user's gesture (Project tree rows), the second
			// superseding the first before it lands.
			for name in ["small.txt", "big.txt"] {
				click_probed(&model, cx, &format!("tree-row:{name}"));
			}
			settle(cx);
			// Nearly all of the retained budget is held, so the 128 KiB file
			// cannot replace the commit diff.
			model.update(cx, |m, _| {
				m.paste.hold_retained_for_test(
					crate::paste::MAX_RETAINED_PREVIEW_BYTES - 32 * 1024,
				);
			});
			cx.executor().advance_clock(delay);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(commit_preview_shown(m), "the commit diff stays");
				assert!(m.selected_commit.is_some(), "selection restored");
				assert!(
					m.commit_view.commit_details.is_some(),
					"details lost to the superseded read",
				);
				assert!(
					!m.commit_view.commit_files.is_empty(),
					"files lost to the superseded read",
				);
				assert_eq!(
					m.preview_root().as_deref(),
					Some(shown_root.as_path()),
					"the shown diff must keep its own repository",
				);
			});
		}

		/// The snapshot ends with the diff it belongs to: after a superseding
		/// read lands, a later refusal restores that file's selection, never
		/// the commit that an earlier snapshot still named.
		#[gpui::test]
		fn a_landed_file_read_releases_the_snapshot_of_the_replaced_diff(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			fs::write(alpha.join("big.txt"), "x".repeat(128 * 1024)).unwrap();
			fs::write(alpha.join("small.txt"), "small\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the log", |cx| {
				model.read_with(cx, |m, _| {
					m.display_commits()
						.iter()
						.any(|c| c.subject == "alpha commit")
				})
			});
			let ix = model.read_with(cx, |m, _| {
				m.display_commits()
					.iter()
					.position(|c| c.subject == "alpha commit")
					.expect("commit listed")
			});
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			click(cx, id);
			settle_until(cx, "the commit's diff and files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& !m.commit_view.commit_files.is_empty()
						&& commit_preview_shown(m)
				})
			});
			open_project_tool(&model, cx);
			let read = |model: &Entity<WorkbenchModel>,
			            cx: &mut VisualTestContext,
			            name: &str| {
				click_probed(model, cx, &format!("tree-row:{name}"));
			};
			let delay = std::time::Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(delay));
			read(&model, cx, "big.txt");
			read(&model, cx, "small.txt");
			settle(cx);
			cx.executor().advance_clock(delay);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.selected_file.as_deref(), Some("small.txt"));
				assert!(m.selected_commit.is_none(), "the file replaced it");
			});
			model.update(cx, |m, _| {
				m.e2e_read_delay = None;
				m.paste.hold_retained_for_test(
					crate::paste::MAX_RETAINED_PREVIEW_BYTES - 32 * 1024,
				);
			});
			read(&model, cx, "big.txt");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.selected_file.as_deref(),
					Some("small.txt"),
					"the refusal restored a selection older than the shown file",
				);
				assert!(
					m.selected_commit.is_none(),
					"a released snapshot came back",
				);
				assert!(m.commit_view.is_reset());
			});
		}

		/// One commit of two files: a small one (listed, so shown, first) and
		/// a big one the retained budget will refuse once it is held.
		fn open_commit_of_small_and_big(
			cx: &mut TestAppContext,
		) -> (
			Entity<WorkbenchModel>,
			&mut VisualTestContext,
			tempfile::TempDir,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			fs::write(alpha.join("a_small.txt"), "small\n").unwrap();
			fs::write(alpha.join("z_big.txt"), "x".repeat(128 * 1024)).unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "two files"]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the log", |cx| {
				model.read_with(cx, |m, _| {
					m.display_commits().iter().any(|c| c.subject == "two files")
				})
			});
			let ix = model.read_with(cx, |m, _| {
				m.display_commits()
					.iter()
					.position(|c| c.subject == "two files")
					.expect("commit listed")
			});
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			click(cx, id);
			settle_until(cx, "the commit's first file diff", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& commit_preview_shown(m)
						&& m.commit_view.commit_files.len() == 2
				})
			});
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.commit_view.selected_commit_file.as_deref(),
					Some("a_small.txt"),
					"the first listed file is shown",
				);
			});
			(model, cx, ws)
		}

		/// A commit file the retained budget refuses leaves the previous diff
		/// shown, so the selection must keep naming that diff's file and no
		/// error root may be set: a selection that points at a file nothing
		/// shows is the state a refused Project read already avoids.
		#[gpui::test]
		fn a_commit_file_the_budget_refuses_keeps_the_shown_diffs_selection(
			cx: &mut TestAppContext,
		) {
			let (model, cx, _ws) = open_commit_of_small_and_big(cx);
			model.update(cx, |m, _| {
				m.paste.hold_retained_for_test(
					crate::paste::MAX_RETAINED_PREVIEW_BYTES - 32 * 1024,
				);
			});
			click_probed(&model, cx, "commit-file:z_big.txt");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(commit_preview_shown(m), "the shown diff stays");
				assert_eq!(
					m.preview.as_ref().and_then(|p| p.path.as_deref()),
					Some("a_small.txt"),
				);
				assert_eq!(
					m.commit_view.selected_commit_file.as_deref(),
					Some("a_small.txt"),
					"the selection names a file nothing shows",
				);
				assert!(
					m.preview_error_root.is_none(),
					"an error root was set for a diff that shows no error",
				);
			});
		}

		/// The same refusal on a file of a browsed commit tree: the shown
		/// diff keeps its own file selected, and the Project tree's row does
		/// not take over the commit view's selection.
		#[gpui::test]
		fn a_tree_file_the_budget_refuses_keeps_the_shown_diffs_selection(
			cx: &mut TestAppContext,
		) {
			let (model, cx, _ws) = open_commit_of_small_and_big(cx);
			let sha = model.read_with(cx, |m, _| {
				let id = m.selected_commit.clone().expect("a commit");
				crate::multi_log::split_id(&id).0.to_string()
			});
			click_probed(&model, cx, &format!("btn-browse-tree:{sha}"));
			settle_until(cx, "the commit tree", |cx| {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| {
						p.bounds("rev-row:z_big.txt").is_some()
					})
				})
			});
			model.update(cx, |m, _| {
				m.paste.hold_retained_for_test(
					crate::paste::MAX_RETAINED_PREVIEW_BYTES - 32 * 1024,
				);
			});
			click_probed(&model, cx, "rev-row:z_big.txt");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(commit_preview_shown(m), "the shown diff stays");
				assert_eq!(
					m.commit_view.selected_commit_file.as_deref(),
					Some("a_small.txt"),
					"the selection names a file nothing shows",
				);
				assert!(
					m.preview_error_root.is_none() && m.preview_error.is_none()
				);
			});
		}

		/// A refresh while a file read is held back drops that read; the
		/// diff still shown keeps its own selection (#230): the reload used
		/// to forget the snapshot and leave the discarded read's path as the
		/// selection, with no commit selected over a commit's diff.
		#[gpui::test]
		fn a_reload_that_keeps_the_preview_keeps_its_selection(
			cx: &mut TestAppContext,
		) {
			let (model, cx, _ws) = open_commit_of_small_and_big(cx);
			open_project_tool(&model, cx);
			let delay = std::time::Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(delay));
			click_probed(&model, cx, "tree-row:a_small.txt");
			settle(cx);
			model.update(cx, |m, _| m.e2e_read_delay = None);
			click_probed(&model, cx, "btn-refresh");
			settle_until(cx, "the reload to settle", |cx| {
				model.read_with(cx, |m, _| {
					m.changes_loaded && !m.preview_loading && m.history_loaded
				})
			});
			model.read_with(cx, |m, _| {
				assert!(commit_preview_shown(m), "the commit diff is shown");
				assert!(
					m.selected_file.is_none(),
					"the discarded read's path is selected: {:?}",
					m.selected_file,
				);
				assert!(
					m.selected_commit.is_some(),
					"a commit's diff is shown with no commit selected",
				);
				assert!(!m.commit_view.commit_files.is_empty());
			});
		}

		/// The budget's refusal puts the whole selection back, the
		/// multi-selection with it (#230): a file list that outlived its
		/// `log_selected` named origins past its end, so a copy took the
		/// file from the wrong commit.
		#[gpui::test]
		fn a_file_the_budget_refuses_keeps_the_multi_selection(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			let beta = repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("big.txt"), "x".repeat(128 * 1024)).unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);
			for name in ["b1", "b2"] {
				fs::write(beta.join(format!("{name}.txt")), "beta\n").unwrap();
				git(&beta, &["add", "."]);
				git(&beta, &["commit", "-q", "-m", name]);
			}
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the merged log", |cx| {
				model.read_with(cx, |m, _| {
					["alpha commit", "b1", "b2"].iter().all(|s| {
						m.display_commits().iter().any(|c| c.subject == *s)
					})
				})
			});
			let row_of = |cx: &mut VisualTestContext, subject: &str| {
				model.read_with(cx, |m, _| {
					m.display_commits()
						.iter()
						.position(|c| c.subject == subject)
						.expect("commit listed")
				})
			};
			let (i2, i1) = (row_of(cx, "b2"), row_of(cx, "b1"));
			let b2_id =
				model.read_with(cx, |m, _| m.display_commits()[i2].sha.clone());
			let at = |cx: &mut VisualTestContext,
			          ix: usize,
			          mods: gpui::Modifiers| {
				cx.run_until_parked();
				let id: &'static str =
					Box::leak(format!("log-subject:{ix}").into_boxed_str());
				let row = cx.debug_bounds(id).expect("a commit row");
				cx.simulate_click(row.center(), mods);
			};
			at(cx, i2, gpui::Modifiers::none());
			settle_until(cx, "the first commit's files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading && !m.commit_view.commit_files.is_empty()
				})
			});
			at(cx, i1, gpui::Modifiers::secondary_key());
			settle_until(cx, "the multi-selection's files and diff", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading
						&& !m.commit_view.commit_file_origin.is_empty()
						&& commit_preview_shown(m)
				})
			});
			model.read_with(cx, |m, _| {
				assert_eq!(m.log_selected.len(), 2, "a multi-selection");
			});

			model.update(cx, |m, _| {
				m.paste
					.enqueue(crate::paste::PasteRequest::Clipboard {
						text: "n".repeat(
							crate::paste::MAX_RETAINED_PREVIEW_BYTES
								- 32 * 1024,
						),
						dest: PathBuf::new(),
						roots: Vec::new(),
						generation: 0,
						remote: None,
					})
					.expect("the queued paste fits");
			});
			model.update(cx, |m, cx| {
				m.select_file_in(
					Some(alpha),
					"big.txt",
					snip_core::transfer::SourceKind::File,
					cx,
				);
			});
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(commit_preview_shown(m), "the shown diff stays");
				assert_eq!(
					m.log_selected.len(),
					2,
					"the multi-selection must come back with its file list",
				);
				assert!(
					m.commit_view
						.commit_file_origin
						.iter()
						.all(|&o| (o as usize) < m.log_selected.len()),
					"an origin past the end of the selection",
				);
				let (_, want) =
					m.log_root_for(&b2_id).expect("b2's repository");
				let (_, got) = m
					.commit_file_rev("b2.txt")
					.expect("b2.txt names a revision");
				assert_eq!(got, want, "b2.txt belongs to the newer commit");
			});
		}

		/// A search the Log cannot run hides the commits it replaced, so it
		/// drops the selection too (#230): the right side must not show a
		/// commit that is no longer listed.
		#[gpui::test]
		fn a_failed_log_reload_drops_the_selection_it_hid(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			for i in 0..3 {
				fs::write(alpha.join(format!("a{i}.txt")), "x\n").unwrap();
				git(&alpha, &["add", "."]);
				git(&alpha, &["commit", "-q", "-m", &format!("alpha {i}")]);
			}
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the log", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 4)
			});
			click(cx, "log-subject:0");
			settle_until(cx, "the commit's files", |cx| {
				model.read_with(cx, |m, _| {
					!m.preview_loading && !m.commit_view.commit_files.is_empty()
				})
			});
			model.read_with(cx, |m, _| {
				assert!(m.selected_commit.is_some(), "a commit is selected");
			});

			// An empty text does not reload; the unbalanced parenthesis then
			// fails `git log --extended-regexp --grep=(` with status 128.
			// (If a platform's regex accepts it, search `[` instead.)
			click(cx, "btn-log-regex");
			click(cx, "log-search-input");
			cx.simulate_input("(");
			cx.simulate_keystrokes("enter");
			settle_until(
				cx,
				"the failed reload (if the platform's regex accepts `(`, \
				 search `[` instead)",
				|cx| model.read_with(cx, |m, _| m.history_error.is_some()),
			);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.history_error.is_some(), "the search failed");
				assert!(m.selected_commit.is_none(), "selection kept");
				assert!(m.log_selected.is_empty(), "multi-selection kept");
				assert!(
					m.commit_view.is_reset(),
					"a commit view member stayed"
				);
				assert!(!commit_preview_shown(m), "stale commit diff kept");
			});
		}

		/// A file preview still loading (a slow read) is not the Log's: the
		/// repository chip must not discard it (#202).
		#[gpui::test]
		fn a_repo_filter_change_keeps_a_file_preview_still_loading(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("a.txt"), "alpha content\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle(cx);
			let delay = std::time::Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_read_delay = Some(delay));
			// Project-tree and Changes rows have no debug selector, so the
			// read starts through the model; the chip is a real click.
			let root = alpha.clone();
			model.update(cx, |m, cx| {
				m.select_file_in(
					Some(root),
					"a.txt",
					crate::SourceKind::File,
					cx,
				);
			});
			settle(cx);
			assert!(
				model.read_with(cx, |m, _| m.preview_loading),
				"the read must be held in flight",
			);
			click(cx, "log-filter-repo");
			click(cx, "log-repo:beta");
			model.read_with(cx, |m, _| {
				assert_eq!(m.log_repo_filter.len(), 1, "the chip applied");
				assert!(
					m.preview_loading,
					"the repo filter dropped the file read in flight",
				);
			});
			cx.executor().advance_clock(delay);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.selected_file.as_deref(), Some("a.txt"));
				assert!(
					m.preview.is_some() && !m.preview_loading,
					"the requested preview was dropped by the repo filter",
				);
			});
		}

		/// A Log commit read still in flight when an error clears
		/// `preview_loading` must still be dropped by a repository filter
		/// change: the Log's read must not reinstall files or a diff for a
		/// selection that is gone (#202).
		#[gpui::test]
		fn a_repo_filter_change_drops_a_log_read_after_an_error_cleared_loading(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			repo(ws.path(), "beta", &[]);
			fs::write(alpha.join("a.txt"), "alpha content\n").unwrap();
			git(&alpha, &["add", "."]);
			git(&alpha, &["commit", "-q", "-m", "alpha commit"]);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle(cx);
			let ix = model
				.read_with(cx, |m, _| {
					m.display_commits()
						.iter()
						.position(|c| c.subject == "alpha commit")
				})
				.expect("alpha commit listed");
			let id: &'static str =
				Box::leak(format!("log-subject:{ix}").into_boxed_str());
			let delay = std::time::Duration::from_secs(60);
			model.update(cx, |m, _| m.e2e_log_read_delay = Some(delay));
			click(cx, id);
			settle(cx);
			assert!(
				model.read_with(cx, |m, _| {
					m.preview_loading && m.commit_view.commit_files.is_empty()
				}),
				"the Log read must be held in flight",
			);
			// Another read fails first and clears the loading flag without
			// touching the generation.
			model.update(cx, |m, _| {
				m.show_preview_error(crate::Msg::new(
					"error_repo_changes",
					["beta".to_string(), "boom".to_string()],
				));
			});
			assert!(!model.read_with(cx, |m, _| m.preview_loading));
			click(cx, "log-filter-repo");
			click(cx, "log-repo:beta");
			for _ in 0..1200 {
				settle(cx);
				if model.read_with(cx, |m, _| m.selected_commit.is_none()) {
					break;
				}
				std::thread::sleep(std::time::Duration::from_millis(25));
			}
			cx.executor().advance_clock(delay);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.selected_commit.is_none(), "selection kept");
				assert!(
					m.commit_view.commit_files.is_empty(),
					"the late Log read reinstalled files",
				);
				assert!(
					!commit_preview_shown(m),
					"the late Log read reinstalled a commit diff",
				);
			});
		}

		/// Copying commits that are not one chain, by the user's gestures
		/// (click, Cmd-click, Cmd+C): the e2e drivers read the engine's own
		/// text on `COPY_COMMITS_ERR`, while the status words it in the
		/// interface language. A regression this stops: the call site printing
		/// the translated text, dropping the line, or a focus path that never
		/// starts the copy.
		#[gpui::test]
		fn copying_non_contiguous_commits_logs_the_engines_text(
			cx: &mut TestAppContext,
		) {
			let Some(_clip) = clipboard() else { return };
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			for n in ["one", "two"] {
				fs::write(alpha.join(format!("{n}.txt")), "x\n").unwrap();
				git(&alpha, &["add", "."]);
				git(&alpha, &["commit", "-q", "-m", &format!("alpha {n}")]);
			}
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle_until(cx, "the log rows", |cx| {
				model.read_with(cx, |m, _| m.display_commits().len() >= 3)
			});
			let ix_of = |cx: &mut VisualTestContext, subject: &str| {
				model
					.read_with(cx, |m, _| {
						m.display_commits()
							.iter()
							.position(|c| c.subject == subject)
					})
					.expect("commit listed")
			};
			let newest = ix_of(cx, "alpha two");
			let oldest = ix_of(cx, "base");
			let id = |ix: usize| -> &'static str {
				Box::leak(format!("log-subject:{ix}").into_boxed_str())
			};
			click(cx, id(newest));
			let bounds = cx.debug_bounds(id(oldest)).expect("the oldest row");
			cx.simulate_click(
				bounds.center(),
				gpui::Modifiers::secondary_key(),
			);
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(m.log_selected.len(), 2, "two commits are selected");
			});
			let _ = crate::test_log::take();
			cx.simulate_keystrokes("cmd-c");
			settle_until(cx, "the copy to end", |cx| {
				model.read_with(cx, |m, _| !m.is_copying)
			});
			let lines = crate::test_log::take();
			assert!(
				lines.iter().any(|l| l.starts_with(
					"[APP:COPY_COMMITS_ERR: commits are not contiguous:"
				)),
				"{lines:#?}"
			);
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "error_payload");
				let zh = m.status.render(crate::i18n::Locale::ZhTw);
				assert!(zh.contains("產生 payload 失敗"), "{zh}");
				assert!(!zh.contains("not contiguous"), "{zh}");
			});
		}

		/// A Shift-range selection previews a Compare; a repository filter
		/// change drops it with the selection and the compare state (#202).
		#[gpui::test]
		fn a_repo_filter_change_clears_a_shift_range_compare(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let alpha = repo(ws.path(), "alpha", &[]);
			repo(ws.path(), "beta", &[]);
			for n in ["one", "two"] {
				fs::write(alpha.join(format!("{n}.txt")), "x\n").unwrap();
				git(&alpha, &["add", "."]);
				git(&alpha, &["commit", "-q", "-m", &format!("alpha {n}")]);
			}

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1440., 900.);
			settle(cx);
			let ix_of = |subject: &str| {
				model
					.read_with(cx, |m, _| {
						m.display_commits()
							.iter()
							.position(|c| c.subject == subject)
					})
					.expect("commit listed")
			};
			let newer = ix_of("alpha two");
			let older = ix_of("alpha one");
			let newer_id: &'static str =
				Box::leak(format!("log-subject:{newer}").into_boxed_str());
			let older_id: &'static str =
				Box::leak(format!("log-subject:{older}").into_boxed_str());
			click(cx, newer_id);
			let bounds = cx.debug_bounds(older_id).expect("older row");
			cx.simulate_click(
				bounds.center(),
				gpui::Modifiers {
					shift: true,
					..Default::default()
				},
			);
			cx.run_until_parked();
			for _ in 0..50 {
				settle(cx);
				if model.read_with(cx, |m, _| commit_preview_shown(m)) {
					break;
				}
			}
			model.read_with(cx, |m, _| {
				assert_eq!(m.log_selected.len(), 2, "range must be selected");
				assert!(
					matches!(
						m.preview.as_ref().map(|p| &p.source),
						Some(crate::reader::PreviewSource::Compare { .. })
					),
					"the range must preview a compare first",
				);
			});

			click(cx, "log-filter-repo");
			click(cx, "log-repo:beta");
			for _ in 0..1200 {
				settle(cx);
				if model.read_with(cx, |m, _| m.selected_commit.is_none()) {
					break;
				}
				std::thread::sleep(std::time::Duration::from_millis(25));
			}
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(m.log_selected.is_empty(), "range kept");
				assert!(m.compare.is_none(), "compare state kept");
				assert!(
					!commit_preview_shown(m),
					"stale compare kept after the repository filter",
				);
			});
		}

		/// In the Git log with a branch filter active and a non-HEAD commit
		/// selected, clicking `btn-head` clears the filter, reloads the full
		/// graph, scrolls to and selects HEAD.
		#[gpui::test]
		fn head_button_after_ref_filter_selects_head(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			let r = repo(ws.path(), "my-repo", &[]);
			git(&r, &["checkout", "-b", "side"]);
			fs::write(r.join("side.txt"), "side content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "side commit"]);
			let side_sha = {
				let out = Command::new("git")
					.current_dir(&r)
					.args(["rev-parse", "HEAD"])
					.output()
					.unwrap();
				String::from_utf8(out.stdout).unwrap().trim().to_string()
			};
			git(&r, &["checkout", "main"]);
			fs::write(r.join("main.txt"), "main content\n").unwrap();
			git(&r, &["add", "."]);
			git(&r, &["commit", "-q", "-m", "main head commit"]);
			let head_sha = {
				let out = Command::new("git")
					.current_dir(&r)
					.args(["rev-parse", "HEAD"])
					.output()
					.unwrap();
				String::from_utf8(out.stdout).unwrap().trim().to_string()
			};
			assert_ne!(side_sha, head_sha);

			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1200., 752.);
			for _ in 0..50 {
				settle(cx);
				let ready = model.read_with(cx, |m, _| {
					m.display_commits().len() >= 3 && !m.refs.is_empty()
				});
				if ready {
					break;
				}
			}
			settle(cx);

			// Activate branch filter for "side"
			model.update(cx, |m, cx| {
				m.filter_by_ref(Some("side".into()), cx);
			});
			for _ in 0..50 {
				settle(cx);
				let filtered = model.read_with(cx, |m, _| {
					m.active_ref_filter.as_deref() == Some("side")
						&& !m.display_commits().is_empty()
				});
				if filtered {
					break;
				}
			}
			settle(cx);

			// Select the side commit (non-HEAD)
			if let Some(row_bounds) = cx.debug_bounds("log-subject:0") {
				cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
			} else {
				model.update(cx, |m, cx| {
					m.select_commit(&side_sha, cx);
				});
			}
			settle(cx);
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.selected_commit.as_deref(),
					Some(side_sha.as_str())
				);
			});

			// User clicks `btn-head`
			let btn =
				cx.debug_bounds("btn-head").expect("btn-head must be drawn");
			cx.simulate_click(btn.center(), gpui::Modifiers::none());

			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.active_ref_filter.is_none()
						&& m.selected_commit.as_deref()
							== Some(head_sha.as_str())
				});
				if done {
					break;
				}
			}
			settle(cx);

			model.read_with(cx, |m, _| {
				assert!(
					m.active_ref_filter.is_none(),
					"active ref filter must be cleared"
				);
				assert_eq!(
					m.selected_commit.as_deref(),
					Some(head_sha.as_str()),
					"HEAD commit must be selected after clicking btn-head"
				);
			});
		}

		/// An error in the status bar is never overwritten by background
		/// progress/ready messages until the user's next action.
		#[gpui::test]
		fn status_bar_error_not_overwritten_by_background_loading(
			cx: &mut TestAppContext,
		) {
			let ws = tempfile::tempdir().unwrap();
			let _r = repo(ws.path(), "my-repo", &[]);
			let (model, cx) =
				open_sized(cx, ws.path().to_path_buf(), None, 1200., 752.);
			settle(cx);

			// User causes a paste error
			model.update(cx, |m, _| {
				m.user_action();
				m.set_status_msg(crate::i18n::Msg::new(
					"paste_err_plan",
					["Not a directory (os error 20)".into()],
				));
			});
			model.read_with(cx, |m, _| {
				assert_eq!(m.status.key, "paste_err_plan");
			});

			// Background progress/ready messages arrive: must not overwrite the error
			for bg in [
				"status_scanning",
				"status_repo_loading",
				"status_repo_loaded",
				"status_repos_loaded",
				"status_history_loaded",
			] {
				model.update(cx, |m, _| {
					m.set_status(bg, []);
				});
				model.read_with(cx, |m, _| {
					assert_eq!(
						m.status.key, "paste_err_plan",
						"background status '{bg}' should not overwrite user error"
					);
				});
			}

			// User initiates next action: error can be superseded
			model.update(cx, |m, _| {
				m.user_action();
				m.set_status("status_scanning", []);
			});
			model.read_with(cx, |m, _| {
				assert_eq!(
					m.status.key, "status_scanning",
					"user's next action allows status update"
				);
			});
		}

		/// Workspace tabs (#136): every gesture is a real click or key.
		mod workspace_tabs {
			use super::*;
			use crate::lifecycle::JobKind;
			use crate::tabs::{
				FirstTab, Launch, TabEvent, TabsRoot, WsIdentity,
			};
			use std::cell::Cell;
			use std::rc::Rc;

			fn open_tabs(
				cx: &mut TestAppContext,
				first: FirstTab,
				restore_dir: Option<PathBuf>,
			) -> (Entity<TabsRoot>, &mut VisualTestContext) {
				launch_tabs(cx, Launch::single(first, restore_dir))
			}

			fn launch_tabs(
				cx: &mut TestAppContext,
				launch: Launch,
			) -> (Entity<TabsRoot>, &mut VisualTestContext) {
				cx.update(|cx| cx.bind_keys(crate::key_bindings()));
				let (root, cx) = cx.add_window_view(|window, cx| {
					TabsRoot::new(launch, "normal".into(), None, window, cx)
				});
				settle(cx);
				(root, cx)
			}

			pub(super) fn tab(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
				ix: usize,
			) -> Entity<WorkbenchModel> {
				root.read_with(cx, |r, _| r.tabs[ix].model.clone())
			}

			fn count(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
			) -> usize {
				root.read_with(cx, |r, _| r.tabs.len())
			}

			pub(super) fn active(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
			) -> Option<usize> {
				root.read_with(cx, |r, _| r.active)
			}

			fn labels(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
			) -> Vec<String> {
				root.read_with(cx, |r, cx| {
					r.tab_views(cx).into_iter().map(|(l, _)| l).collect()
				})
			}

			/// Settles until `ready`, failing with `what` after a bound.
			fn wait(
				cx: &mut VisualTestContext,
				what: &str,
				mut ready: impl FnMut(&mut VisualTestContext) -> bool,
			) {
				for _ in 0..80 {
					settle(cx);
					if ready(cx) {
						return;
					}
				}
				panic!("timed out waiting for {what}");
			}

			pub(super) fn sel(id: String) -> &'static str {
				Box::leak(id.into_boxed_str())
			}

			pub(super) fn click_id(cx: &mut VisualTestContext, id: String) {
				cx.run_until_parked();
				let id = sel(id);
				let bounds = cx
					.debug_bounds(id)
					.unwrap_or_else(|| panic!("no rendered control {id}"));
				cx.simulate_click(bounds.center(), gpui::Modifiers::none());
				settle(cx);
			}

			fn drawn(cx: &mut VisualTestContext, id: &str) -> bool {
				cx.run_until_parked();
				cx.debug_bounds(sel(id.to_string())).is_some()
			}

			/// The typed-path field of the active tab's workspace menu,
			/// filled and submitted with the keyboard.
			fn open_typed(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
				path: &Path,
			) {
				let ix = active(root, cx).expect("a tab is shown");
				let m = tab(root, cx, ix);
				m.update(cx, |m, cx| m.show_workspace_picker(cx));
				settle(cx);
				cx.simulate_input(&path.display().to_string());
				cx.simulate_keystrokes("enter");
				settle(cx);
			}

			fn workspace_ready(
				m: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> bool {
				m.read_with(cx, |m, _| {
					m.workspace_open
						&& !m.lifecycle.is_draining()
						&& !m.is_loading
				})
			}

			fn focused_in(
				m: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> bool {
				cx.update(|window, cx| {
					m.read(cx).focus_handle.contains_focused(window, cx)
				})
			}

			/// An owned job of `kind` that stays live until the returned
			/// flag is set: a load, or a confirmed paste, the test controls.
			fn hold_job(
				m: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				kind: JobKind,
			) -> Rc<Cell<bool>> {
				let release = Rc::new(Cell::new(false));
				let flag = release.clone();
				m.update(cx, |m, cx| {
					let bg = cx.background_executor().clone();
					let hold = async move {
						while !flag.get() {
							bg.timer(Duration::from_millis(20)).await;
						}
					};
					match kind {
						JobKind::Mutating => {
							m.spawn_mutating(cx, hold);
						}
						JobKind::UncancellableRead => {
							m.spawn_owned(cx, None, hold);
						}
						JobKind::CancellableRead => {
							m.spawn_owned(
								cx,
								Some(snip_core::gitrun::CancelToken::new()),
								hold,
							);
						}
					}
					// What a confirmed paste does: the tab bar shows it.
					cx.notify();
				});
				release
			}

			/// Two local workspaces, one repo each with an untracked file.
			fn two_workspaces() -> (tempfile::TempDir, PathBuf, PathBuf) {
				let tmp = tempfile::tempdir().unwrap();
				let root = dunce::canonicalize(tmp.path()).unwrap();
				let a = root.join("ws-a");
				let b = root.join("ws-b");
				fs::create_dir(&a).unwrap();
				fs::create_dir(&b).unwrap();
				repo(&a, "alpha", &[("only-in-a.txt", "from a\n")]);
				repo(&b, "beta", &[("only-in-b.txt", "from b\n")]);
				(tmp, a, b)
			}

			pub(super) fn two_tabs(
				cx: &mut TestAppContext,
			) -> (
				tempfile::TempDir,
				PathBuf,
				PathBuf,
				Entity<TabsRoot>,
				&mut VisualTestContext,
			) {
				let (tmp, a, b) = two_workspaces();
				let (root, cx) =
					open_tabs(cx, FirstTab::Local(a.clone()), None);
				let ta = tab(&root, cx, 0);
				wait(cx, "tab A loaded", |cx| workspace_ready(&ta, cx));
				open_typed(&root, cx, &b);
				wait(cx, "tab B opened", |cx| {
					count(&root, cx) == 2 && {
						let tb = tab(&root, cx, 1);
						workspace_ready(&tb, cx)
					}
				});
				(tmp, a, b, root, cx)
			}

			pub(super) fn wait_scaled(
				cx: &mut VisualTestContext,
				what: &str,
				mut ready: impl FnMut(&mut VisualTestContext) -> bool,
			) {
				let deadline = std::time::Instant::now()
					+ snip_native_e2e::scaled(Duration::from_secs(10));
				loop {
					cx.run_until_parked();
					cx.executor().advance_clock(snip_native_e2e::scaled(
						Duration::from_millis(50),
					));
					cx.run_until_parked();
					if ready(cx) {
						return;
					}
					assert!(
						std::time::Instant::now() < deadline,
						"timed out waiting for {what}"
					);
					std::thread::sleep(snip_native_e2e::scaled(
						Duration::from_millis(10),
					));
				}
			}

			/// Two local tabs and a third remote tab opened with the menu's
			/// actual controls. Only the transport is replaced by a worker
			/// on this machine; opening, focus and tab routing are unchanged.
			fn three_tabs_with_remote(
				cx: &mut TestAppContext,
			) -> (
				tempfile::TempDir,
				PathBuf,
				Entity<TabsRoot>,
				&mut VisualTestContext,
			) {
				let (tmp, _a, _b, root, cx) = two_tabs(cx);
				let shared =
					dunce::canonicalize(tmp.path()).unwrap().join("remote");
				fs::create_dir(&shared).unwrap();
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				click_id(cx, "ws-tab-new".into());
				let remote = tab(&root, cx, 2);
				remote.update(cx, |m, cx| {
					m.remote.hosts =
						vec![snip_remote::RemoteHost::in_process(worker)];
					m.remote.recent.clear();
					cx.notify();
				});
				click_id(cx, "remote-host:0".into());
				wait_scaled(cx, "remote host listed", |cx| {
					remote.read_with(cx, |m, _| !m.remote.busy)
				});
				click_id(cx, "remote-path-input".into());
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input(&shared.display().to_string());
				click_id(cx, "btn-remote-open".into());
				wait_scaled(cx, "third remote tab loaded", |cx| {
					remote.read_with(cx, |m, _| {
						m.remote.session.is_some() && !m.is_loading
					})
				});
				assert_eq!(active(&root, cx), Some(2));
				(tmp, shared, root, cx)
			}

			#[gpui::test]
			fn closing_a_remote_tab_drops_its_connection(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let (_tmp, _shared, root, cx) = three_tabs_with_remote(cx);
				let remote = tab(&root, cx, 2);
				let client = remote.read_with(cx, |m, _| {
					std::sync::Arc::downgrade(
						&m.remote.session.as_ref().unwrap().client,
					)
				});
				assert!(
					client.upgrade().is_some(),
					"the tab owns a live connection"
				);
				click_id(cx, "ws-tab-close:2".into());
				wait_scaled(
					cx,
					"remote tab closed and its connection dropped",
					|cx| count(&root, cx) == 2 && client.upgrade().is_none(),
				);
				assert!(remote.read_with(cx, |m, _| m.remote.session.is_none()));
				assert_eq!(active(&root, cx), Some(1));
			}

			/// Enter confirms the real remote paste, then Cmd+1 switches
			/// away before GPUI runs its background task. Completion clears
			/// the PastePanel while the tab is hidden, as in WT61.
			fn complete_remote_paste_in_background(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
				shared: &Path,
			) -> Entity<WorkbenchModel> {
				let remote = tab(root, cx, 2);
				clip::write_text("// FILE: new.txt\nbackground paste\n")
					.unwrap();
				cx.simulate_keystrokes("cmd-v");
				wait_scaled(cx, "remote paste preview", |cx| {
					remote.read_with(cx, |m, _| {
						m.paste.plan().is_some() && !m.paste.is_loading()
					})
				});
				assert!(
					cx.update(|w, cx| remote
						.read(cx)
						.paste_focus
						.is_focused(w)),
					"Enter confirms the focused paste panel"
				);
				cx.simulate_keystrokes("enter cmd-1");
				assert_eq!(active(root, cx), Some(0));
				wait_scaled(cx, "remote paste completed in background", |cx| {
					remote.read_with(cx, |m, _| {
						m.paste.plan().is_none()
							&& !m.paste_busy() && !m.is_loading
					}) && shared.join("new.txt").is_file()
				});
				assert_eq!(
					fs::read_to_string(shared.join("new.txt")).unwrap(),
					"background paste"
				);
				assert_eq!(
					active(root, cx),
					Some(0),
					"completion does not switch tabs"
				);
				remote
			}

			#[gpui::test]
			fn background_remote_paste_then_cmd_3_refreshes_and_focuses_the_tab(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let Some(_clip) = clipboard() else { return };
				let (_tmp, shared, root, cx) = three_tabs_with_remote(cx);
				let remote =
					complete_remote_paste_in_background(&root, cx, &shared);
				let before =
					remote.read_with(cx, |m, _| m.discovery_generation);
				// Consecutive real key events, with GPUI's normal event-loop
				// turn between them; no click or test-only redraw.
				cx.simulate_keystrokes("cmd-3");
				assert!(cx.update(|w, cx| remote.read(cx).focus_handle.is_focused(w)), "Cmd+3 must restore the workbench focus after its PastePanel closed in background");
				cx.simulate_keystrokes("cmd-r");
				assert_eq!(
					active(&root, cx),
					Some(2),
					"Cmd+3 reached TabsRoot"
				);
				wait_scaled(cx, "Cmd+R refresh on the remote tab", |cx| {
					remote.read_with(cx, |m, _| {
						m.discovery_generation > before && !m.is_loading
					})
				});
				assert_eq!(active(&root, cx), Some(2));
				assert!(
					cx.update(|w, cx| remote
						.read(cx)
						.focus_handle
						.is_focused(w)),
					"the completed tab holds its workbench focus"
				);
			}

			#[gpui::test]
			fn background_remote_paste_then_cmd_3_opens_the_next_paste_preview(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let Some(_clip) = clipboard() else { return };
				let (_tmp, shared, root, cx) = three_tabs_with_remote(cx);
				let remote =
					complete_remote_paste_in_background(&root, cx, &shared);
				clip::write_text("// FILE: next.txt\nnext paste\n").unwrap();
				cx.simulate_keystrokes("cmd-3");
				assert!(cx.update(|w, cx| remote.read(cx).focus_handle.is_focused(w)), "Cmd+3 must restore the workbench focus after its PastePanel closed in background");
				cx.simulate_keystrokes("cmd-v");
				assert_eq!(
					active(&root, cx),
					Some(2),
					"Cmd+3 reached TabsRoot"
				);
				wait_scaled(cx, "Cmd+V preview on the remote tab", |cx| {
					remote.read_with(cx, |m, _| {
						m.paste.plan().is_some() && !m.paste.is_loading()
					})
				});
				assert_eq!(active(&root, cx), Some(2));
				assert!(
					focused_in(&remote, cx),
					"the remote tab contains the preview focus"
				);
				assert!(
					cx.update(|w, cx| remote
						.read(cx)
						.paste_focus
						.is_focused(w)),
					"the new paste panel holds the keyboard"
				);
				assert!(
					!shared.join("next.txt").exists(),
					"preview does not apply"
				);
			}

			#[gpui::test]
			fn opening_another_workspace_adds_a_tab_and_an_open_one_is_switched_to(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, b, root, cx) = two_tabs(cx);
				assert_eq!(active(&root, cx), Some(1));
				assert_eq!(labels(&root, cx), ["ws-a", "ws-b"]);
				let tb = tab(&root, cx, 1);
				assert_eq!(
					tb.read_with(cx, |m, _| m.ws_identity()),
					Some(WsIdentity::Local(b.clone()))
				);

				// The same folder again, from tab B: tab A is shown.
				open_typed(&root, cx, &a);
				assert_eq!(count(&root, cx), 2);
				assert_eq!(active(&root, cx), Some(0));

				// Spelled through a symlink it is still the same workspace.
				#[cfg(unix)]
				{
					let link = _tmp.path().join("link-to-b");
					std::os::unix::fs::symlink(&b, &link).unwrap();
					open_typed(&root, cx, &link);
					assert_eq!(count(&root, cx), 2);
					assert_eq!(active(&root, cx), Some(1));
				}

				// A nested folder is a workspace of its own.
				let nested = a.join("alpha");
				open_typed(&root, cx, &nested);
				wait(cx, "nested tab", |cx| count(&root, cx) == 3);
				assert_eq!(active(&root, cx), Some(2));
				assert_eq!(labels(&root, cx), ["ws-a", "ws-b", "alpha"]);
			}

			#[gpui::test]
			fn an_empty_tab_is_filled_and_an_empty_tab_for_an_open_workspace_goes_away(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, b) = two_workspaces();
				let (root, cx) = open_tabs(cx, FirstTab::Empty, None);
				assert_eq!(count(&root, cx), 1);
				assert_eq!(labels(&root, cx), ["新分頁"]);
				open_typed(&root, cx, &a);
				let ta = tab(&root, cx, 0);
				wait(cx, "A in the first tab", |cx| workspace_ready(&ta, cx));
				assert_eq!(count(&root, cx), 1, "the empty tab was filled");

				// "+" opens an empty tab with the workspace menu open.
				click_id(cx, "ws-tab-new".into());
				assert_eq!(count(&root, cx), 2);
				assert_eq!(active(&root, cx), Some(1));
				let empty = tab(&root, cx, 1);
				assert!(empty.read_with(cx, |m, _| m
					.overlays
					.is_open(crate::overlay::OverlayKind::WorkspaceMenu)));
				assert!(focused_in(&empty, cx));

				// Asking it for A shows tab A and drops the empty tab.
				open_typed(&root, cx, &a);
				wait(cx, "empty tab dropped", |cx| count(&root, cx) == 1);
				assert_eq!(active(&root, cx), Some(0));

				// Asking a fresh empty tab for B fills it.
				click_id(cx, "ws-tab-new".into());
				open_typed(&root, cx, &b);
				let tb = tab(&root, cx, 1);
				wait(cx, "B in the second tab", |cx| workspace_ready(&tb, cx));
				assert_eq!(count(&root, cx), 2);
			}

			/// The first Cmd+C and Cmd+V after a tab is clicked reach that
			/// tab only.
			#[gpui::test]
			fn the_first_copy_and_paste_after_clicking_a_tab_reach_only_that_tab(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let (ta, tb) = (tab(&root, cx, 0), tab(&root, cx, 1));
				wait(cx, "both trees", |cx| {
					ta.read_with(cx, |m, _| !m.files.is_empty())
						&& tb.read_with(cx, |m, _| !m.files.is_empty())
				});

				click_id(cx, "ws-tab:0".into());
				assert_eq!(active(&root, cx), Some(0));
				assert!(focused_in(&ta, cx), "tab A holds the keyboard");
				assert!(!focused_in(&tb, cx));
				clip::write_text("before").unwrap();
				cx.simulate_keystrokes("cmd-c");
				wait(cx, "A's copy", |cx| {
					ta.read_with(cx, |m, _| !m.is_copying)
						&& clip::read_text().unwrap() != "before"
				});
				let copied = clip::read_text().unwrap();
				assert!(copied.contains("only-in-a.txt"), "{copied}");
				assert!(!copied.contains("only-in-b.txt"), "{copied}");
				assert!(tb.read_with(cx, |m, _| m.toast.is_none()));

				click_id(cx, "ws-tab:1".into());
				assert!(focused_in(&tb, cx), "tab B holds the keyboard");
				clip::write_text("// FILE: beta/pasted.txt\nhello\n").unwrap();
				cx.simulate_keystrokes("cmd-v");
				settle(cx);
				assert!(
					tb.read_with(cx, |m, _| m.paste.is_open()),
					"B previews the paste: {}",
					tb.read_with(cx, |m, _| m.status.clone())
				);
				assert!(!ta.read_with(cx, |m, _| m.paste.is_open()));

				// Back to A by keyboard: its Cmd+V is its own.
				cx.simulate_keystrokes("cmd-1");
				assert!(focused_in(&ta, cx));
				cx.simulate_keystrokes("cmd-v");
				settle(cx);
				assert!(ta.read_with(cx, |m, _| m.paste.is_open()));
				assert!(
					tb.read_with(cx, |m, _| m.paste.is_open()),
					"B keeps its preview"
				);
			}

			#[gpui::test]
			fn tab_shortcuts_switch_wrap_and_jump_and_ctrl_tab_still_moves_focus(
				cx: &mut TestAppContext,
			) {
				let tmp = tempfile::tempdir().unwrap();
				let root_dir = dunce::canonicalize(tmp.path()).unwrap();
				let dirs: Vec<PathBuf> = ["one", "two", "three"]
					.iter()
					.map(|n| {
						let d = root_dir.join(n);
						fs::create_dir(&d).unwrap();
						d
					})
					.collect();
				let (root, cx) =
					open_tabs(cx, FirstTab::Local(dirs[0].clone()), None);
				open_typed(&root, cx, &dirs[1]);
				open_typed(&root, cx, &dirs[2]);
				wait(cx, "three tabs", |cx| count(&root, cx) == 3);
				assert_eq!(active(&root, cx), Some(2));
				let steps = [
					("cmd-shift-]", 0),
					("cmd-shift-[", 2),
					("cmd-{", 1),
					("cmd-}", 2),
					("ctrl-shift-[", 1),
					("cmd-1", 0),
					("cmd-3", 2),
					("cmd-9", 2),
					("ctrl-2", 1),
				];
				for (keys, want) in steps {
					cx.simulate_keystrokes(keys);
					settle(cx);
					assert_eq!(active(&root, cx), Some(want), "after {keys}");
					let m = tab(&root, cx, want);
					assert!(focused_in(&m, cx), "focus follows {keys}");
				}
				cx.simulate_keystrokes("ctrl-tab");
				settle(cx);
				assert_eq!(
					active(&root, cx),
					Some(1),
					"Ctrl+Tab is not a tab switch"
				);
			}

			#[gpui::test]
			fn close_keys_and_the_close_button_drop_tabs_down_to_the_plus_alone(
				cx: &mut TestAppContext,
			) {
				let tmp = tempfile::tempdir().unwrap();
				let root_dir = dunce::canonicalize(tmp.path()).unwrap();
				let dirs: Vec<PathBuf> = ["one", "two", "three", "four"]
					.iter()
					.map(|n| {
						let d = root_dir.join(n);
						fs::create_dir(&d).unwrap();
						d
					})
					.collect();
				let (root, cx) =
					open_tabs(cx, FirstTab::Local(dirs[0].clone()), None);
				for d in &dirs[1..] {
					open_typed(&root, cx, d);
				}
				wait(cx, "four tabs", |cx| count(&root, cx) == 4);
				cx.simulate_keystrokes("cmd-2");
				settle(cx);
				assert_eq!(labels(&root, cx), ["one", "two", "three", "four"]);

				cx.simulate_keystrokes("cmd-w");
				wait(cx, "cmd-w", |cx| count(&root, cx) == 3);
				assert_eq!(labels(&root, cx), ["one", "three", "four"]);
				assert_eq!(active(&root, cx), Some(1), "the right neighbour");
				let shown = tab(&root, cx, 1);
				assert!(focused_in(&shown, cx));

				// The × of a background tab closes it; the shown tab stays.
				click_id(cx, "ws-tab-close:0".into());
				wait(cx, "close button", |cx| count(&root, cx) == 2);
				assert_eq!(labels(&root, cx), ["three", "four"]);
				assert_eq!(active(&root, cx), Some(0));
				assert!(focused_in(&shown, cx));

				cx.simulate_keystrokes("cmd-shift-w");
				wait(cx, "cmd-shift-w", |cx| count(&root, cx) == 1);
				cx.simulate_keystrokes("ctrl-w");
				wait(cx, "last tab", |cx| count(&root, cx) == 0);
				assert_eq!(active(&root, cx), None);
				// (debug_bounds keeps ids of earlier frames, so only what is
				// drawn is checked.)
				assert!(drawn(cx, "ws-tab-new"), "only + is left");
				assert!(
					!root.read_with(cx, |r, _| r.quit_sent),
					"the app stays"
				);
				let title = root.read_with(cx, |r, _| r.title.clone());
				assert_eq!(title, "snip-sync");

				// Keys still work on the empty window.
				cx.simulate_keystrokes("cmd-w");
				settle(cx);
				assert_eq!(count(&root, cx), 0);
				click_id(cx, "ws-tab-new".into());
				assert_eq!(count(&root, cx), 1);
				cx.simulate_keystrokes("cmd-w");
				wait(cx, "empty again", |cx| count(&root, cx) == 0);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert!(
					root.read_with(cx, |r, _| r.quit_sent),
					"Cmd+Q with no tab quits"
				);
			}

			#[gpui::test]
			fn a_closing_tab_waits_only_for_its_own_jobs(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let tb = tab(&root, cx, 1);
				let b_load = hold_job(&tb, cx, JobKind::UncancellableRead);

				// Tab A closes while tab B is still loading.
				click_id(cx, "ws-tab:0".into());
				cx.simulate_keystrokes("cmd-w");
				wait(cx, "A closed", |cx| count(&root, cx) == 1);
				assert_eq!(tab(&root, cx, 0), tb);
				assert!(tb.read_with(cx, |m, _| m.lifecycle.live_jobs() > 0));

				// Tab B's own close waits for its load.
				cx.simulate_keystrokes("cmd-w");
				for _ in 0..5 {
					settle(cx);
				}
				assert_eq!(count(&root, cx), 1, "B drains its own load first");
				assert!(tb.read_with(cx, |m, _| m.lifecycle.is_draining()));
				b_load.set(true);
				wait(cx, "B closed", |cx| count(&root, cx) == 0);
			}

			/// A tab that waits for a held job before it closes still shows its
			/// Log and still takes clicks. A click on a row starts nothing and
			/// changes nothing: it must not leave a load flag no job will
			/// ever clear (#230).
			#[gpui::test]
			fn a_log_click_during_a_drain_starts_nothing(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, b) = two_workspaces();
				let beta = b.join("beta");
				for i in 0..2 {
					fs::write(beta.join(format!("b{i}.txt")), "x\n").unwrap();
					git(&beta, &["add", "."]);
					git(&beta, &["commit", "-q", "-m", &format!("beta {i}")]);
				}
				let (root, cx) = open_tabs(cx, FirstTab::Local(a), None);
				let ta = tab(&root, cx, 0);
				wait(cx, "tab A loaded", |cx| workspace_ready(&ta, cx));
				open_typed(&root, cx, &b);
				wait(cx, "tab B opened", |cx| {
					count(&root, cx) == 2 && {
						let tb = tab(&root, cx, 1);
						workspace_ready(&tb, cx)
					}
				});
				let tb = tab(&root, cx, 1);

				// A selection that already landed.
				click_id(cx, "ws-tab:1".into());
				wait(cx, "B's log", |cx| {
					tb.read_with(cx, |m, _| m.display_commits().len() >= 3)
				});
				click_id(cx, "log-subject:0".into());
				wait(cx, "B's selection", |cx| {
					tb.read_with(cx, |m, _| {
						m.selected_commit.is_some() && !m.preview_loading
					})
				});
				let b_load = hold_job(&tb, cx, JobKind::UncancellableRead);

				// A closes; B's own close waits for the held job.
				click_id(cx, "ws-tab:0".into());
				cx.simulate_keystrokes("cmd-w");
				wait(cx, "A closed", |cx| count(&root, cx) == 1);
				cx.simulate_keystrokes("cmd-w");
				for _ in 0..5 {
					settle(cx);
				}
				assert_eq!(count(&root, cx), 1, "B drains its own load first");
				assert!(tb.read_with(cx, |m, _| m.lifecycle.is_draining()));

				let anchor = tb.read_with(cx, |m, _| m.selected_commit.clone());
				let selected = tb.read_with(cx, |m, _| m.log_selected.clone());
				let secondary = gpui::Modifiers::secondary_key();
				let shift = gpui::Modifiers {
					shift: true,
					..Default::default()
				};
				for (row, mods) in
					[(1, secondary), (2, shift), (1, gpui::Modifiers::none())]
				{
					cx.run_until_parked();
					let id = sel(format!("log-subject:{row}"));
					let bounds = cx.debug_bounds(id).expect("a commit row");
					cx.simulate_click(bounds.center(), mods);
					settle(cx);
					tb.read_with(cx, |m, _| {
						assert!(
							!m.preview_loading,
							"a click on {id} ({mods:?}) during a drain left a load flag"
						);
						assert_eq!(m.selected_commit, anchor, "{id} {mods:?}");
						assert_eq!(m.log_selected, selected, "{id} {mods:?}");
					});
				}
				b_load.set(true);
				wait(cx, "B closed", |cx| count(&root, cx) == 0);
			}

			/// A real Local-pool Git child holds a slot of the shared
			/// budget: a tab closing elsewhere does not wait for it, and
			/// Quit does.
			#[cfg(unix)]
			#[gpui::test]
			fn a_closing_tab_does_not_wait_for_git_another_tab_runs(
				cx: &mut TestAppContext,
			) {
				if !crate::run_isolated(
					"tests::in_process::workspace_tabs::a_closing_tab_does_not_wait_for_git_another_tab_runs",
				) {
					return;
				}
				use snip_core::gitrun::{in_flight, RunOptions};
				use snip_core::gitsrc::Git;

				let (_tmp, _a, b, root, cx) = two_tabs(cx);
				// Both tabs' own loads finish first (bounded): the counter is
				// process-wide.
				wait(cx, "both tabs idle", |_| in_flight() == 0);
				let base = in_flight();
				let git = Git::open(&b.join("beta")).unwrap();
				let release = b.join("release");
				let wait_for = release.display().to_string();
				let handle = std::thread::spawn(move || {
					let opts = RunOptions {
						timeout: Duration::from_secs(30),
						..Default::default()
					};
					let alias = format!(
						"alias.hold=!f() {{ while [ ! -e '{wait_for}' ]; do sleep 0.05; done; }}; f"
					);
					git.run_with(&["-c", &alias, "hold"], &opts)
				});
				let deadline =
					std::time::Instant::now() + Duration::from_secs(10);
				while in_flight() < base + 1 {
					assert!(
						std::time::Instant::now() < deadline,
						"git never started"
					);
					std::thread::sleep(Duration::from_millis(10));
				}

				// Tab A closes while the shared budget has a Git child.
				click_id(cx, "ws-tab:0".into());
				cx.simulate_keystrokes("cmd-w");
				wait(cx, "A closed", |cx| count(&root, cx) == 1);
				assert!(!handle.is_finished(), "the Git child still runs");

				// Quit waits for every Git child.
				cx.simulate_keystrokes("cmd-q");
				for _ in 0..5 {
					settle(cx);
				}
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
				fs::write(&release, "").unwrap();
				handle.join().unwrap().unwrap();
				wait(cx, "quit", |cx| root.read_with(cx, |r, _| r.quit_sent));
			}

			#[gpui::test]
			fn quit_drains_every_tab_and_a_paste_anywhere_refuses_it(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let (ta, tb) = (tab(&root, cx, 0), tab(&root, cx, 1));

				// A confirmed paste in background tab A: quit is refused, tab
				// A is shown, and tab B never started draining.
				let paste = hold_job(&ta, cx, JobKind::Mutating);
				settle(cx);
				let info = root.read_with(cx, |r, cx| r.tab_views(cx));
				assert!(info[0].1.pasting && !info[1].1.pasting);
				assert!(drawn(cx, "ws-tab-pasting:0"));
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert_eq!(active(&root, cx), Some(0));
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
				assert!(!tb.read_with(cx, |m, _| m.lifecycle.is_draining()));
				assert_eq!(
					ta.read_with(cx, |m, _| m.status.key),
					"workspace_busy_applying"
				);
				// Closing that tab is refused the same way.
				cx.simulate_keystrokes("cmd-w");
				settle(cx);
				assert_eq!(count(&root, cx), 2);
				paste.set(true);
				settle(cx);

				// Quit waits for every tab's work, then quits.
				let b_load = hold_job(&tb, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-q");
				for _ in 0..5 {
					settle(cx);
				}
				assert!(
					!root.read_with(cx, |r, _| r.quit_sent),
					"B still loads"
				);
				b_load.set(true);
				wait(cx, "quit", |cx| root.read_with(cx, |r, _| r.quit_sent));
			}

			/// A tab that drained early and then starts a confirmed paste
			/// while another tab still drains stops the quit: the write is
			/// not cut off, and that tab is shown.
			#[gpui::test]
			fn a_paste_started_during_the_quit_stops_it(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let (ta, tb) = (tab(&root, cx, 0), tab(&root, cx, 1));
				let b_load = hold_job(&tb, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert!(
					!ta.read_with(cx, |m, _| m.lifecycle.is_draining()),
					"A drained"
				);
				assert!(tb.read_with(cx, |m, _| m.lifecycle.is_draining()));
				let paste = hold_job(&ta, cx, JobKind::Mutating);
				b_load.set(true);
				for _ in 0..5 {
					settle(cx);
				}
				assert!(
					!root.read_with(cx, |r, _| r.quit_sent),
					"the write is not cut off"
				);
				assert_eq!(active(&root, cx), Some(0));
				assert_eq!(
					ta.read_with(cx, |m, _| m.status.key),
					"workspace_busy_applying"
				);
				assert_eq!(count(&root, cx), 2);
				// Once written, Quit quits.
				paste.set(true);
				settle(cx);
				cx.simulate_keystrokes("cmd-q");
				wait(cx, "quit", |cx| root.read_with(cx, |r, _| r.quit_sent));
			}

			/// Reopening a folder whose tab is still draining its close
			/// opens it again in a new tab.
			#[gpui::test]
			fn reopening_a_closing_workspace_opens_a_new_tab(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, _b, root, cx) = two_tabs(cx);
				click_id(cx, "ws-tab:0".into());
				let ta = tab(&root, cx, 0);
				let a_load = hold_job(&ta, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-w");
				settle(cx);
				assert!(ta.read_with(cx, |m, _| m.is_closing()));
				open_typed(&root, cx, &a);
				wait(cx, "a new tab for A", |cx| count(&root, cx) == 3);
				a_load.set(true);
				wait(cx, "the old A closed", |cx| count(&root, cx) == 2);
				let reopened = tab(&root, cx, 1);
				assert_ne!(reopened, ta);
				wait(cx, "A open again", |cx| workspace_ready(&reopened, cx));
				assert_eq!(
					reopened.read_with(cx, |m, _| m.ws_identity()),
					Some(WsIdentity::Local(a.clone()))
				);
			}

			/// An open owns its transport until the handshake and connection
			/// cleanup return, even when this tab has no workspace yet. The
			/// close cancels it: a host that does not answer (ssh's connect
			/// timeout and the hello's outlast the drain deadline) must not
			/// keep the tab open.
			#[gpui::test]
			fn closing_a_connecting_empty_tab_cancels_the_remote_open(
				cx: &mut TestAppContext,
			) {
				let tmp = tempfile::tempdir().unwrap();
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				let (root, cx) = open_tabs(cx, FirstTab::Empty, None);
				click_id(cx, "ws-tab-new".into());
				let empty = tab(&root, cx, 1);
				let hold = crate::lifecycle::DRAIN_DEADLINE * 4;
				empty.update(cx, |m, cx| {
					m.remote.hosts = vec![snip_remote::RemoteHost::in_process(
						worker.clone(),
					)];
					m.e2e_remote_open_delay = Some(hold);
					m.open_remote_path(0, tmp.path().display().to_string(), cx);
				});
				cx.run_until_parked();
				assert!(empty.read_with(cx, |m, _| m.remote.busy));
				assert_eq!(
					empty.update(cx, |m, _| m.lifecycle.unfinished()),
					1
				);
				cx.simulate_keystrokes("cmd-w");
				// One hold slice: the cancel lands well inside the deadline.
				cx.executor().advance_clock(Duration::from_millis(100));
				wait(
					cx,
					"open cancelled, connection cleaned, tab closed",
					|cx| count(&root, cx) == 1,
				);
				assert!(!empty.read_with(cx, |m, _| m.remote.busy));
				assert_eq!(
					empty.update(cx, |m, _| m.lifecycle.unfinished()),
					0
				);
			}

			/// Browsing a host's folders in the menu also starts ssh: the
			/// listing belongs to the tab, and a close cancels it rather than
			/// leaving it to run on after the tab is gone.
			#[gpui::test]
			fn closing_a_tab_while_browsing_a_host_cancels_the_listing(
				cx: &mut TestAppContext,
			) {
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				let (root, cx) = open_tabs(cx, FirstTab::Empty, None);
				click_id(cx, "ws-tab-new".into());
				let empty = tab(&root, cx, 1);
				empty.update(cx, |m, cx| {
					m.remote.hosts = vec![snip_remote::RemoteHost::in_process(
						worker.clone(),
					)];
					m.e2e_remote_open_delay =
						Some(crate::lifecycle::DRAIN_DEADLINE * 4);
					m.browse_remote_host(0, cx);
				});
				cx.run_until_parked();
				assert_eq!(
					empty.update(cx, |m, _| m.lifecycle.unfinished()),
					1
				);
				cx.simulate_keystrokes("cmd-w");
				cx.executor().advance_clock(Duration::from_millis(100));
				wait(cx, "listing cancelled and tab closed", |cx| {
					count(&root, cx) == 1
				});
				assert_eq!(
					empty.update(cx, |m, _| m.lifecycle.unfinished()),
					0
				);
			}

			/// An empty tab with a job still running drains it before it goes.
			#[gpui::test]
			fn an_empty_tab_with_a_job_drains_before_closing(
				cx: &mut TestAppContext,
			) {
				let (root, cx) = open_tabs(cx, FirstTab::Empty, None);
				click_id(cx, "ws-tab-new".into());
				let empty = tab(&root, cx, 1);
				let job = hold_job(&empty, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-w");
				for _ in 0..5 {
					settle(cx);
				}
				assert_eq!(count(&root, cx), 2, "its job drains first");
				job.set(true);
				wait(cx, "closed", |cx| count(&root, cx) == 1);
			}

			/// A drain that gives up stops the quit; the tabs that already
			/// drained reload what the quit cancelled.
			#[gpui::test]
			fn a_failed_drain_stops_the_quit_and_shows_that_tab(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let (ta, tb) = (tab(&root, cx, 0), tab(&root, cx, 1));
				let b_load = hold_job(&tb, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert!(!ta.read_with(cx, |m, _| m.lifecycle.is_draining()));
				// Tab B gives up (as on its 8 s deadline).
				click_id(cx, "ws-tab:0".into());
				tb.update(cx, |_, cx| cx.emit(TabEvent::DrainFailed));
				settle(cx);
				assert_eq!(active(&root, cx), Some(1));
				b_load.set(true);
				settle(cx);
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
				assert_eq!(count(&root, cx), 2);
				wait(cx, "A reloaded", |cx| {
					ta.read_with(cx, |m, _| {
						!m.files.is_empty() && !m.is_loading
					})
				});
			}

			#[gpui::test]
			fn a_background_tab_keeps_its_state_and_is_not_reloaded(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let ta = tab(&root, cx, 0);
				wait(cx, "A's files", |cx| {
					ta.read_with(cx, |m, _| !m.files.is_empty())
				});
				ta.update(cx, |m, _| m.left_w = 333.);
				let before = ta.read_with(cx, |m, _| {
					(
						m.lifecycle.generation(),
						m.history_generation,
						m.tree_generation,
						m.files.len(),
						m.selected_repo_idx,
					)
				});
				click_id(cx, "ws-tab:1".into());
				click_id(cx, "ws-tab:0".into());
				let after = ta.read_with(cx, |m, _| {
					(
						m.lifecycle.generation(),
						m.history_generation,
						m.tree_generation,
						m.files.len(),
						m.selected_repo_idx,
					)
				});
				assert_eq!(before, after);
				assert_eq!(ta.read_with(cx, |m, _| m.left_w), 333.);
				let tb = tab(&root, cx, 1);
				assert_ne!(tb.read_with(cx, |m, _| m.left_w), 333.);
			}

			#[gpui::test]
			fn labels_add_parent_folders_on_a_clash_and_hover_shows_the_full_path(
				cx: &mut TestAppContext,
			) {
				let tmp = tempfile::tempdir().unwrap();
				let base = dunce::canonicalize(tmp.path()).unwrap();
				let one = base.join("one").join("app");
				let two = base.join("two").join("app");
				fs::create_dir_all(&one).unwrap();
				fs::create_dir_all(&two).unwrap();
				let (root, cx) =
					open_tabs(cx, FirstTab::Local(one.clone()), None);
				assert_eq!(labels(&root, cx), ["app"]);
				assert_eq!(
					root.read_with(cx, |r, _| r.title.clone()),
					"app — snip-sync"
				);
				open_typed(&root, cx, &two);
				wait(cx, "second app", |cx| count(&root, cx) == 2);
				settle(cx);
				assert_eq!(labels(&root, cx), ["one/app", "two/app"]);
				let views = root.read_with(cx, |r, cx| r.tab_views(cx));
				assert_eq!(views[0].1.full, one.display().to_string());
				assert_eq!(views[1].1.full, two.display().to_string());
				assert_eq!(
					root.read_with(cx, |r, _| r.title.clone()),
					"two/app — snip-sync"
				);
				click_id(cx, "ws-tab-close:1".into());
				wait(cx, "one tab", |cx| count(&root, cx) == 1);
				assert_eq!(labels(&root, cx), ["app"]);
			}

			#[gpui::test]
			fn remote_tabs_are_one_per_host_and_real_path_beside_local_ones(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let (_tmp, a, _b) = two_workspaces();
				let shared_tmp = tempfile::tempdir().unwrap();
				let shared = dunce::canonicalize(shared_tmp.path()).unwrap();
				repo(&shared, "proj", &[("remote.txt", "r\n")]);
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				let host = snip_remote::RemoteHost::in_process(worker.clone());
				let host_name = host.name.clone();
				let (root, cx) =
					open_tabs(cx, FirstTab::Local(a.clone()), None);
				let open_remote_from_active =
					|root: &Entity<TabsRoot>,
					 cx: &mut VisualTestContext,
					 path: String| {
						let ix = active(root, cx).unwrap();
						let m = tab(root, cx, ix);
						let host = host.clone();
						m.update(cx, |m, cx| {
							m.open_overlay(
								crate::overlay::OverlayKind::WorkspaceMenu,
								crate::overlay::ReturnFocus::Root,
								Some(crate::overlay::TriggerId::Workspace),
								None,
								cx,
							);
							m.remote.hosts = vec![host];
							m.browse_remote_host(0, cx);
							m.open_remote_path(0, path, cx);
						});
						settle(cx);
					};
				open_remote_from_active(
					&root,
					cx,
					shared.display().to_string(),
				);
				wait(cx, "remote tab", |cx| {
					count(&root, cx) == 2 && {
						let m = tab(&root, cx, 1);
						m.read_with(cx, |m, _| m.remote.session.is_some())
					}
				});
				assert_eq!(active(&root, cx), Some(1));
				let name =
					shared.file_name().unwrap().to_string_lossy().into_owned();
				assert_eq!(
					labels(&root, cx),
					["ws-a".to_string(), format!("{host_name} ▸ {name}")]
				);
				let views = root.read_with(cx, |r, cx| r.tab_views(cx));
				assert_eq!(views[1].1.conn, Some(crate::tabs::Conn::Connected));
				assert_eq!(views[0].1.conn, None);

				// The same folder from the local tab, spelled with a trailing
				// slash: the worker resolves it, and the remote tab is shown.
				click_id(cx, "ws-tab:0".into());
				open_remote_from_active(
					&root,
					cx,
					format!("{}/", shared.display()),
				);
				wait(cx, "switched to the remote tab", |cx| {
					active(&root, cx) == Some(1)
				});
				assert_eq!(count(&root, cx), 2);
				drop(worker);
			}

			/// Back on a tab, the keyboard is in the pane it was in: Cmd+C
			/// in a Git Log that had focus copies the commit.
			#[gpui::test]
			fn a_tab_shown_again_gives_the_keyboard_back_to_its_pane(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, a, _b) = two_workspaces();
				let r = a.join("alpha");
				fs::write(r.join("c1.txt"), "c1\n").unwrap();
				git(&r, &["add", "c1.txt"]);
				git(&r, &["commit", "-q", "-m", "commit c1"]);
				let (_tmp2, _a2, b) = two_workspaces();
				let (root, cx) = open_tabs(cx, FirstTab::Local(a), None);
				let ta = tab(&root, cx, 0);
				wait(cx, "A loaded", |cx| workspace_ready(&ta, cx));
				open_typed(&root, cx, &b);
				wait(cx, "tab B", |cx| count(&root, cx) == 2);
				click_id(cx, "ws-tab:0".into());
				wait(cx, "A's log", |cx| {
					ta.read_with(cx, |m, _| m.display_commits().len() >= 2)
				});
				let row = cx.debug_bounds("log-subject:0").expect("commit row");
				cx.simulate_click(row.center(), gpui::Modifiers::none());
				settle(cx);
				let log_focused = |cx: &mut VisualTestContext| {
					cx.update(|w, cx| {
						ta.read(cx).log_focus.contains_focused(w, cx)
					})
				};
				assert!(log_focused(cx));
				click_id(cx, "ws-tab:1".into());
				assert!(!log_focused(cx));
				click_id(cx, "ws-tab:0".into());
				assert!(log_focused(cx), "the Git Log has the keyboard again");
				clip::write_text("before").unwrap();
				cx.simulate_keystrokes("cmd-c");
				wait(cx, "commit copy", |_| {
					clip::read_text().unwrap() != "before"
				});
				let text = clip::read_text().unwrap();
				assert!(text.starts_with("// snip-sync commits v1"), "{text}");
			}

			#[gpui::test]
			fn a_remote_tab_shows_connecting_connected_and_failed(
				cx: &mut TestAppContext,
			) {
				let (root, cx) = open_tabs(cx, FirstTab::Empty, None);
				let m = tab(&root, cx, 0);
				let conn = |cx: &mut VisualTestContext| {
					root.read_with(cx, |r, cx| r.tab_views(cx)[0].1.conn)
				};
				assert_eq!(conn(cx), None);
				m.update(cx, |m, cx| {
					m.remote.busy = true;
					cx.notify();
				});
				settle(cx);
				assert_eq!(conn(cx), Some(crate::tabs::Conn::Connecting));
				assert!(drawn(cx, "ws-tab-state:0:connecting"));
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				let host = snip_remote::RemoteHost::in_process(worker);
				let ws = snip_remote::RemoteWorkspace {
					id: "/srv/proj".into(),
					name: "proj".into(),
					path: "/srv/proj".into(),
				};
				m.update(cx, |m, cx| {
					m.remote.busy = false;
					m.remote.session =
						Some(crate::remote::RemoteSession::new(host, ws));
					m.workspace_open = true;
					cx.notify();
				});
				settle(cx);
				assert_eq!(conn(cx), Some(crate::tabs::Conn::Connected));
				m.update(cx, |m, cx| {
					m.remote.scan_error = Some(crate::i18n::Msg::new(
						"remote_scan_failed",
						["x".to_string()],
					));
					cx.notify();
				});
				settle(cx);
				assert_eq!(conn(cx), Some(crate::tabs::Conn::Failed));
				assert!(drawn(cx, "ws-tab-state:0:failed"));
				let full = root
					.read_with(cx, |r, cx| r.tab_views(cx)[0].1.full.clone());
				assert!(full.ends_with(":/srv/proj"), "{full}");
			}

			#[gpui::test]
			fn the_tab_bar_sits_above_the_workbench_header(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, _b) = two_workspaces();
				let (root, cx) = open_tabs(cx, FirstTab::Local(a), None);
				let m = tab(&root, cx, 0);
				wait(cx, "loaded", |cx| workspace_ready(&m, cx));
				let bar = cx.debug_bounds("ws-tab:0").expect("tab drawn");
				let menu = cx
					.debug_bounds("btn-workspace-menu")
					.expect("workbench header drawn");
				assert!(
					f32::from(bar.bottom()) <= f32::from(menu.top()),
					"tab bar {bar:?} above the header {menu:?}"
				);
				assert_eq!(
					m.read_with(cx, |m, _| m.top_inset),
					crate::tabs::TAB_BAR_H
				);
			}

			/// Rests the mouse on control `id` until its tooltip is up.
			pub(super) fn hover_id(
				cx: &mut VisualTestContext,
				id: &str,
			) -> gpui::Point<gpui::Pixels> {
				cx.run_until_parked();
				let at = cx
					.debug_bounds(sel(id.to_string()))
					.unwrap_or_else(|| panic!("no rendered control {id}"))
					.center();
				cx.simulate_mouse_move(at, None, gpui::Modifiers::none());
				cx.executor().advance_clock(Duration::from_millis(700));
				settle(cx);
				at
			}

			pub(super) fn tooltip_shown(
				cx: &mut VisualTestContext,
				text: &str,
			) -> bool {
				drawn(cx, &format!("tooltip:{text}"))
			}

			/// `n` local workspaces named `workspace-number-NN`, all open
			/// in tabs, the last one shown.
			fn many_tabs(
				cx: &mut TestAppContext,
				n: usize,
			) -> (
				tempfile::TempDir,
				Vec<PathBuf>,
				Entity<TabsRoot>,
				&mut VisualTestContext,
			) {
				let tmp = tempfile::tempdir().unwrap();
				let base = dunce::canonicalize(tmp.path()).unwrap();
				let dirs: Vec<PathBuf> = (0..n)
					.map(|i| {
						let d = base.join(format!("workspace-number-{i:02}"));
						fs::create_dir(&d).unwrap();
						d
					})
					.collect();
				let (root, cx) =
					open_tabs(cx, FirstTab::Local(dirs[0].clone()), None);
				for d in &dirs[1..] {
					open_typed(&root, cx, d);
				}
				wait(cx, "all tabs open", |cx| count(&root, cx) == n);
				assert_eq!(active(&root, cx), Some(n - 1));
				(tmp, dirs, root, cx)
			}

			/// The Git Log may take more than half the window, and a new tab
			/// opens with the layout the active tab has (#188).
			#[gpui::test]
			fn the_git_panel_drags_past_half_the_window_and_a_new_tab_keeps_it(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _dirs, root, cx) = many_tabs(cx, 1);
				cx.simulate_resize(gpui::size(
					gpui::px(1200.),
					gpui::px(1000.),
				));
				settle(cx);
				let vh = cx.update(|window, _| {
					f32::from(window.viewport_size().height)
				});
				// The Git panel shows once discovery finds the repo.
				for _ in 0..50 {
					settle(cx);
					if cx
						.debug_bounds(sel("splitter-bottom".to_string()))
						.is_some()
					{
						break;
					}
				}
				let grip = cx
					.debug_bounds(sel("splitter-bottom".to_string()))
					.expect("splitter-bottom drawn")
					.center();
				let top = gpui::point(grip.x, gpui::px(150.));
				let none = gpui::Modifiers::none();
				cx.simulate_mouse_down(grip, gpui::MouseButton::Left, none);
				cx.simulate_mouse_move(
					top,
					Some(gpui::MouseButton::Left),
					none,
				);
				cx.simulate_mouse_up(top, gpui::MouseButton::Left, none);
				settle(cx);
				let first = tab(&root, cx, 0);
				let before = first.read_with(cx, |m, _| m.bottom_h);
				assert!(
					before > 0.55 * vh,
					"the Git Log stopped at {before} of a {vh} px window"
				);
				click_id(cx, "ws-tab-new".into());
				assert_eq!(active(&root, cx), Some(1), "the new tab is active");
				let second = tab(&root, cx, 1);
				assert_eq!(second.read_with(cx, |m, _| m.bottom_h), before);
			}

			/// A tooltip that is up follows the tab switch under it (WT33,
			/// #148): the "+" of a tab in another language shows that
			/// language's text, without hovering again.
			#[gpui::test]
			fn a_shown_tooltip_follows_the_tab_switch(cx: &mut TestAppContext) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let first = tab(&root, cx, 0).read_with(cx, |m, _| m.locale);
				cx.simulate_keystrokes("alt-l");
				settle(cx);
				let second = tab(&root, cx, 1).read_with(cx, |m, _| m.locale);
				assert_ne!(first, second, "alt-l switched tab B's language");
				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				assert_eq!(active(&root, cx), Some(0));
				let old = crate::i18n::t("tip_ws_tab_new", first).to_string();
				let new = crate::i18n::t("tip_ws_tab_new", second).to_string();
				hover_id(cx, "ws-tab-new");
				assert!(tooltip_shown(cx, &old), "the + tooltip is up");

				cx.simulate_keystrokes("cmd-2");
				settle(cx);
				assert_eq!(active(&root, cx), Some(1));
				assert!(
					tooltip_shown(cx, &new),
					"the + tooltip is in tab B's language"
				);
				assert!(!tooltip_shown(cx, &old), "no stale + tooltip");
			}

			/// A tab's tooltip goes when the strip scrolls it away from the
			/// still mouse (WT31, #148): after cmd-1 scrolls to the first
			/// tab, the tab now under the mouse does not show the path of
			/// the one that was there.
			#[gpui::test]
			fn a_tab_tooltip_goes_when_the_strip_scrolls_it_away(
				cx: &mut TestAppContext,
			) {
				let (_tmp, dirs, root, cx) = many_tabs(cx, 8);
				cx.simulate_resize(gpui::size(gpui::px(700.), gpui::px(500.)));
				settle(cx);
				let plus = cx.debug_bounds("ws-tab-new").expect("+ drawn");
				// The leftmost tab wholly inside the strip, which the
				// shown last tab scrolled to its end.
				let k = (0..8)
					.find(|ix| {
						cx.debug_bounds(sel(format!("ws-tab:{ix}")))
							.is_some_and(|b| {
								f32::from(b.left()) >= 0.
									&& f32::from(b.right())
										<= f32::from(plus.left())
							})
					})
					.expect("a tab wholly visible");
				assert!(k > 0, "the strip is scrolled to its end");
				let at = hover_id(cx, &format!("ws-tab:{k}"));
				let path = dirs[k].display().to_string();
				assert!(tooltip_shown(cx, &path), "tab {k}'s tooltip is up");

				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				assert_eq!(active(&root, cx), Some(0));
				let moved = cx
					.debug_bounds(sel(format!("ws-tab:{k}")))
					.expect("tab drawn");
				assert!(
					!moved.contains(&at),
					"tab {k} scrolled away: {moved:?}"
				);
				assert!(
					!tooltip_shown(cx, &path),
					"tab {k}'s tooltip stays over another tab"
				);
			}

			/// Opening the workspace menu takes its button's tooltip away
			/// (#203): left up, it covers the menu's first item.
			#[gpui::test]
			fn opening_the_workspace_menu_hides_its_buttons_tooltip(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, _b, root, cx) = two_tabs(cx);
				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				let loc = tab(&root, cx, 0).read_with(cx, |m, _| m.locale);
				let text = format!(
					"{} · {}",
					a.display(),
					crate::ui::mac_keys(crate::i18n::t(
						"tip_workspace_menu",
						loc
					))
				);
				// Clicked while the tooltip still waits to show: GPUI keeps
				// that timer running past the mouse-down.
				cx.run_until_parked();
				let at = cx
					.debug_bounds("btn-workspace-menu")
					.expect("button drawn")
					.center();
				cx.simulate_mouse_move(at, None, gpui::Modifiers::none());

				click_id(cx, "btn-workspace-menu".into());
				cx.executor().advance_clock(Duration::from_millis(700));
				settle(cx);
				assert!(
					tab(&root, cx, 0).read_with(cx, |m, _| m
						.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu)),
					"the menu is open"
				);
				assert!(
					!tooltip_shown(cx, &text),
					"the tooltip stays over the opened menu"
				);
				let row = workspace_menu_first_row(cx);
				assert!(!covered_by_tooltip(cx, row, &text));
			}

			/// The tooltip `text` is drawn over the point `at`.
			pub(super) fn covered_by_tooltip(
				cx: &mut VisualTestContext,
				at: gpui::Point<gpui::Pixels>,
				text: &str,
			) -> bool {
				cx.run_until_parked();
				cx.debug_bounds(sel(format!("tooltip:{text}")))
					.is_some_and(|b| b.contains(&at))
			}

			/// The first row of the open workspace menu.
			fn workspace_menu_first_row(
				cx: &mut VisualTestContext,
			) -> gpui::Point<gpui::Pixels> {
				let panel = cx
					.debug_bounds("workspace-menu")
					.expect("the workspace menu is drawn");
				gpui::point(panel.center().x, panel.origin.y + gpui::px(20.))
			}

			/// Hovered until its tooltip is up, then clicked: GPUI's own
			/// MouseDown already drops a shown tooltip, so this guards that
			/// existing behaviour for the workspace button (not the #203 fix).
			#[gpui::test]
			fn clicking_the_workspace_button_drops_its_shown_tooltip(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, _b, root, cx) = two_tabs(cx);
				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				let loc = tab(&root, cx, 0).read_with(cx, |m, _| m.locale);
				let text = format!(
					"{} · {}",
					a.display(),
					crate::ui::mac_keys(crate::i18n::t(
						"tip_workspace_menu",
						loc
					))
				);
				hover_id(cx, "btn-workspace-menu");
				assert!(tooltip_shown(cx, &text), "the tooltip is up");
				click_id(cx, "btn-workspace-menu".into());
				assert!(
					tab(&root, cx, 0).read_with(cx, |m, _| m
						.overlays
						.is_open(crate::overlay::OverlayKind::WorkspaceMenu)),
					"the menu is open"
				);
				assert!(!tooltip_shown(cx, &text), "the tooltip stays");
				let row = workspace_menu_first_row(cx);
				assert!(!covered_by_tooltip(cx, row, &text));
			}

			/// A tooltip left up while a native dialog took the pointer
			/// goes when the window gets its focus back (#203), without
			/// waiting for the next mouse input.
			#[gpui::test]
			fn a_tooltip_goes_when_the_window_regains_focus(
				cx: &mut TestAppContext,
			) {
				let (_tmp, dirs, root, cx) = many_tabs(cx, 2);
				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				assert_eq!(active(&root, cx), Some(0));
				hover_id(cx, "ws-tab:0");
				let path = dirs[0].display().to_string();
				assert!(tooltip_shown(cx, &path), "the tab tooltip is up");

				cx.update(|window, _| window.activate_window());
				settle(cx);
				assert!(
					!tooltip_shown(cx, &path),
					"the pointer's place is unknown until it moves"
				);
			}

			/// Narrowing the window keeps the shown tab in view (WT91,
			/// #148): it is scrolled back inside the strip at once, not
			/// only at the next switch.
			#[gpui::test]
			fn narrowing_the_window_keeps_the_shown_tab_in_view(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _dirs, root, cx) = many_tabs(cx, 7);
				let in_view = |cx: &mut VisualTestContext, w: f32| {
					cx.run_until_parked();
					let tab = cx.debug_bounds("ws-tab:6").expect("tab drawn");
					let plus = cx.debug_bounds("ws-tab-new").expect("+ drawn");
					let strip =
						cx.debug_bounds("ws-tabs").expect("strip drawn");
					assert!(
						f32::from(tab.left()) >= f32::from(strip.left()) - 0.5
							&& f32::from(tab.right())
								<= f32::from(plus.left()) + 0.5,
						"at {w}: the shown tab {tab:?} is outside the strip \
						 {strip:?} (+ at {plus:?})"
					);
				};
				cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(600.)));
				settle(cx);
				// Shown again by key at 900, which scrolls it into view.
				cx.simulate_keystrokes("cmd-{");
				settle(cx);
				cx.simulate_keystrokes("cmd-}");
				settle(cx);
				assert_eq!(active(&root, cx), Some(6));
				in_view(cx, 900.);
				cx.simulate_resize(gpui::size(gpui::px(700.), gpui::px(600.)));
				settle(cx);
				assert_eq!(active(&root, cx), Some(6));
				in_view(cx, 700.);
			}
			fn saved(store: &Path) -> crate::open_tabs::OpenTabs {
				crate::open_tabs::load_from(store).expect("tabs saved")
			}

			fn local_tab(p: &Path) -> crate::open_tabs::SavedTab {
				crate::open_tabs::SavedTab::Local {
					path: p.to_path_buf(),
				}
			}

			/// Let real worker/Git threads finish without advancing the test
			/// clock: assertions about retry deadlines must not consume time.
			fn pump_scan_until(
				cx: &mut VisualTestContext,
				what: &str,
				mut ready: impl FnMut(&mut VisualTestContext) -> bool,
			) {
				let deadline = std::time::Instant::now()
					+ snip_native_e2e::scaled(Duration::from_secs(10));
				loop {
					cx.run_until_parked();
					if ready(cx) {
						return;
					}
					assert!(
						std::time::Instant::now() < deadline,
						"timed out waiting for {what}"
					);
					std::thread::sleep(Duration::from_millis(2));
				}
			}

			struct ScanRestoreFixture {
				_tmp: tempfile::TempDir,
				store: tempfile::TempDir,
				worker: std::sync::Arc<snip_remote::Worker>,
				host: snip_remote::RemoteHost,
				paths: [String; 2],
				saved: crate::open_tabs::OpenTabs,
			}

			impl ScanRestoreFixture {
				fn new(
					eofs: usize,
					refusal: Option<snip_remote::proto::ErrorCode>,
				) -> Self {
					Self::with_options(
						eofs,
						refusal,
						snip_remote::WorkerOptions::default(),
					)
				}

				fn with_options(
					eofs: usize,
					refusal: Option<snip_remote::proto::ErrorCode>,
					options: snip_remote::WorkerOptions,
				) -> Self {
					use crate::open_tabs::{OpenTabs, SavedTab};
					let tmp = tempfile::tempdir().unwrap();
					let base = dunce::canonicalize(tmp.path()).unwrap();
					let local = base.join("local");
					fs::create_dir(&local).unwrap();
					let paths = ["background", "active"].map(|name| {
						let path = base.join(name);
						fs::create_dir(&path).unwrap();
						repo(&path, name, &[("kept.txt", "unchanged\n")]);
						path.display().to_string()
					});
					let worker =
						std::sync::Arc::new(snip_remote::Worker::new(options));
					for path in &paths {
						worker.set_scan_failures_for_tests(path, eofs, refusal);
					}
					let host =
						snip_remote::RemoteHost::in_process(worker.clone());
					let saved = OpenTabs {
						tabs: vec![
							local_tab(&local),
							SavedTab::Remote {
								host: host.name.clone(),
								path: paths[0].clone(),
							},
							SavedTab::Remote {
								host: host.name.clone(),
								path: paths[1].clone(),
							},
						],
						active: Some(2),
					};
					Self {
						_tmp: tmp,
						store: tempfile::tempdir().unwrap(),
						worker,
						host,
						paths,
						saved,
					}
				}

				fn launch<'a>(
					&self,
					cx: &'a mut TestAppContext,
				) -> (Entity<TabsRoot>, &'a mut VisualTestContext) {
					self.launch_with_hosts(cx, vec![self.host.clone()])
				}

				fn launch_with_hosts<'a>(
					&self,
					cx: &'a mut TestAppContext,
					hosts: Vec<snip_remote::RemoteHost>,
				) -> (Entity<TabsRoot>, &'a mut VisualTestContext) {
					cx.update(|cx| cx.bind_keys(crate::key_bindings()));
					let launch = Launch {
						restore: Some(self.saved.clone()),
						first: FirstTab::Empty,
						restore_dir: None,
						store: Some(self.store.path().to_path_buf()),
					};
					crate::remote::with_test_hosts(hosts, || {
						cx.add_window_view(|window, cx| {
							TabsRoot::new(
								launch,
								"normal".into(),
								None,
								window,
								cx,
							)
						})
					})
				}
			}

			/// A saved remote tab whose host is an ssh that is down: its stderr
			/// is a network message no classifier knows. Returns the down host,
			/// the same host once up, the saved tabs, the store and the folder.
			#[cfg(unix)]
			fn unreachable_host_restore() -> (
				snip_remote::RemoteHost,
				snip_remote::RemoteHost,
				crate::open_tabs::OpenTabs,
				tempfile::TempDir,
				tempfile::TempDir,
			) {
				use crate::open_tabs::{OpenTabs, SavedTab};
				let shared_tmp = tempfile::tempdir().unwrap();
				let shared = dunce::canonicalize(shared_tmp.path()).unwrap();
				repo(&shared, "proj", &[("remote.txt", "r\n")]);
				let name = "flaky-host";
				let down = snip_remote::RemoteHost {
					name: name.into(),
					transport: snip_remote::Transport::Command(vec![
						"sh".into(),
						"-c".into(),
						"echo 'ssh: connect to host flaky-host port 22: Network is unreachable' >&2; exit 255".into(),
					]),
				};
				let mut up = snip_remote::RemoteHost::in_process(
					std::sync::Arc::new(snip_remote::Worker::new(
						snip_remote::WorkerOptions::default(),
					)),
				);
				up.name = name.into();
				let tabs = OpenTabs {
					tabs: vec![SavedTab::Remote {
						host: name.into(),
						path: shared.display().to_string(),
					}],
					active: Some(0),
				};
				(down, up, tabs, tempfile::tempdir().unwrap(), shared_tmp)
			}

			/// Launches `tabs` with `hosts` as the ssh hosts on offer.
			#[cfg(unix)]
			fn launch_with_test_hosts<'a>(
				cx: &'a mut TestAppContext,
				tabs: &crate::open_tabs::OpenTabs,
				store: &tempfile::TempDir,
				hosts: Vec<snip_remote::RemoteHost>,
			) -> (Entity<TabsRoot>, &'a mut VisualTestContext) {
				cx.update(|cx| cx.bind_keys(crate::key_bindings()));
				let launch = Launch {
					restore: Some(tabs.clone()),
					first: FirstTab::Empty,
					restore_dir: None,
					store: Some(store.path().to_path_buf()),
				};
				let (root, cx) = crate::remote::with_test_hosts(hosts, || {
					cx.add_window_view(|window, cx| {
						TabsRoot::new(launch, "normal".into(), None, window, cx)
					})
				});
				settle(cx);
				(root, cx)
			}

			fn initial_scan_failures(
				root: &Entity<TabsRoot>,
				cx: &mut VisualTestContext,
			) -> [Entity<WorkbenchModel>; 2] {
				let models = [tab(root, cx, 1), tab(root, cx, 2)];
				pump_scan_until(
					cx,
					"successful opens followed by initial scan failures",
					|cx| {
						// Tick the ordinary open/close-drain watcher only. Once
						// open has landed, leave the retry clock under test control.
						if models.iter().any(|model| {
							model.read_with(cx, |m, _| {
								m.lifecycle.is_draining()
									|| (m.remote.busy
										&& m.remote.session.is_none())
							})
						}) {
							cx.executor()
								.advance_clock(Duration::from_millis(50));
						}
						models.iter().all(|model| {
							model.read_with(cx, |m, _| {
								m.remote.session.is_some()
									&& m.remote.scan_error.is_some()
									&& !m.is_loading
							})
						})
					},
				);
				models
			}

			#[gpui::test]
			fn restored_remote_scans_recover_in_active_and_background_tabs(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(2, None);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				let identities = root.read_with(cx, |r, _| {
					r.tabs.iter().map(|t| t.id).collect::<Vec<_>>()
				});
				assert_eq!(active(&root, cx), Some(2));
				assert_eq!(saved(fixture.store.path()), fixture.saved);
				for seconds in [1, 2, 4] {
					cx.executor().advance_clock(Duration::from_secs(seconds));
					cx.run_until_parked();
					pump_scan_until(
						cx,
						"the scheduled scan attempt finishes",
						|cx| {
							models.iter().all(|model| {
								model.read_with(cx, |m, _| !m.is_loading)
							})
						},
					);
				}
				pump_scan_until(
					cx,
					"scan recovery without Refresh in both restored tabs",
					|cx| {
						models.iter().all(|model| {
							model.read_with(cx, |m, _| {
								!m.is_loading
									&& m.remote.scan_error.is_none()
									&& m.repos.len() == 1
							})
						})
					},
				);
				assert_eq!(count(&root, cx), 3);
				assert_eq!(
					active(&root, cx),
					Some(2),
					"background recovery must not activate its tab"
				);
				assert_eq!(
					root.read_with(cx, |r, _| r
						.tabs
						.iter()
						.map(|t| t.id)
						.collect::<Vec<_>>()),
					identities
				);
				assert_eq!(saved(fixture.store.path()), fixture.saved);
				for path in &fixture.paths {
					assert_eq!(fixture.worker.scan_requests_for_tests(path), 3);
				}
				click_id(cx, "ws-tab:1".into());
				assert!(drawn(cx, "ws-tab-state:1:connected"));
				assert!(models[0].read_with(cx, |m, _| m
					.remote
					.scan_error
					.is_none() && m
					.repos
					.len()
					== 1));
			}

			/// A worker that closes the stream mid-scan is worded by the
			/// interface language, not by the layer's English sentence (#233).
			#[gpui::test]
			fn a_worker_that_closes_the_scan_says_so_in_the_interface_language(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(usize::MAX, None);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				for model in &models {
					model.read_with(cx, |m, _| {
						let msg =
							m.remote.scan_error.as_ref().expect("scan error");
						let zh = msg.render(crate::i18n::Locale::ZhTw);
						let en = msg.render(crate::i18n::Locale::En);
						assert!(zh.contains("結束了連線"), "{zh}");
						assert!(!zh.contains("closed the connection"), "{zh}");
						// The layer's own sentence, not the dictionary's.
						assert!(!en.contains("the worker closed"), "{en}");
					});
				}
			}

			#[gpui::test]
			fn restored_remote_scan_retry_is_bounded_and_uses_backoff(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(usize::MAX, None);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				let counts = || {
					fixture
						.paths
						.each_ref()
						.map(|p| fixture.worker.scan_requests_for_tests(p))
				};
				for seconds in [1, 2, 4] {
					let before = counts();
					cx.executor()
						.advance_clock(Duration::from_millis(seconds * 500));
					cx.run_until_parked();
					assert_eq!(
						counts(),
						before,
						"must wait the {seconds}s backoff"
					);
					cx.executor().advance_clock(Duration::from_millis(
						seconds * 500 + 100,
					));
					pump_scan_until(cx, "one failed retry finishes", |cx| {
						counts().iter().zip(before).all(|(now, old)| *now > old)
							&& models.iter().all(|m| {
								m.read_with(cx, |m, _| {
									!m.is_loading
										&& m.remote.scan_error.is_some()
								})
							})
					});
					for (now, old) in counts().into_iter().zip(before) {
						assert!(
							(1..=2).contains(&(now - old)),
							"one job, at most one connection retry"
						);
					}
				}
				let exhausted = counts();
				assert!(exhausted.iter().all(|n| (4..=8).contains(n)));
				cx.executor().advance_clock(Duration::from_secs(60));
				cx.run_until_parked();
				assert_eq!(counts(), exhausted, "no unbounded retry loop");
				for model in &models {
					model.read_with(cx, |m, _| {
						assert!(
							m.remote.scan_error.is_some()
								&& m.remote.scan_recovery.is_none()
						)
					});
				}
				assert_eq!(saved(fixture.store.path()), fixture.saved);
			}

			#[gpui::test]
			fn restored_remote_scan_recovers_after_a_single_closed_read(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(1, None);
				let (root, cx) = fixture.launch(cx);
				let models = [tab(&root, cx, 1), tab(&root, cx, 2)];
				wait_scaled(
					cx,
					"one closed read recovers without Refresh",
					|cx| {
						models.iter().all(|model| {
							model.read_with(cx, |m, _| {
								m.remote.session.is_some()
									&& !m.is_loading && m
									.remote
									.scan_error
									.is_none() && m.repos.len() == 1
							})
						})
					},
				);
				for path in &fixture.paths {
					assert_eq!(fixture.worker.scan_requests_for_tests(path), 2);
				}
				assert_eq!(saved(fixture.store.path()), fixture.saved);
			}

			#[gpui::test]
			fn restored_remote_scan_and_tree_reuse_the_successful_open_client(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::{OpenTabs, SavedTab};
				let _remote = remote_lock();
				let tmp = tempfile::tempdir().unwrap();
				let path = dunce::canonicalize(tmp.path())
					.unwrap()
					.display()
					.to_string();
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				worker.set_scan_failures_for_tests(&path, 0, None);
				let host = snip_remote::RemoteHost::in_process(worker.clone());
				let saved = OpenTabs {
					tabs: vec![SavedTab::Remote {
						host: host.name.clone(),
						path: path.clone(),
					}],
					active: Some(0),
				};
				let (root, cx) =
					crate::remote::with_test_hosts(vec![host], || {
						launch_tabs(
							cx,
							Launch {
								restore: Some(saved),
								first: FirstTab::Empty,
								restore_dir: None,
								store: None,
							},
						)
					});
				let model = tab(&root, cx, 0);
				wait_scaled(cx, "empty remote scan and tree complete", |cx| {
					model.read_with(cx, |m, _| {
						m.remote.session.is_some()
							&& !m.is_loading && m.remote.scan_error.is_none()
							&& m.ws_tree.as_ref().is_some_and(|t| t.is_loaded)
					})
				});
				assert_eq!(worker.scan_requests_for_tests(&path), 1);
				assert_eq!(worker.started_connections_for_tests(), 1,
					"the test executor's sequential scan/tree must reuse the probed connection");
			}

			#[gpui::test]
			fn restored_remote_scan_forbidden_is_not_retried(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(
					0,
					Some(snip_remote::proto::ErrorCode::Forbidden),
				);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				let generations = models
					.each_ref()
					.map(|m| m.read_with(cx, |m, _| m.discovery_generation));
				cx.executor().advance_clock(Duration::from_secs(60));
				cx.run_until_parked();
				for (ix, model) in models.iter().enumerate() {
					assert_eq!(
						fixture
							.worker
							.scan_requests_for_tests(&fixture.paths[ix]),
						1
					);
					model.read_with(cx, |m, _| {
						assert_eq!(m.discovery_generation, generations[ix]);
						assert!(
							m.remote.scan_error.is_some()
								&& m.remote.scan_recovery.is_none()
						);
					});
				}
			}

			#[gpui::test]
			fn restored_remote_scan_old_worker_is_not_retried(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::with_options(
					0,
					None,
					snip_remote::WorkerOptions {
						name: "ancient".into(),
						max_protocol: Some(1),
					},
				);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				let generations = models
					.each_ref()
					.map(|m| m.read_with(cx, |m, _| m.discovery_generation));
				let connections =
					fixture.worker.started_connections_for_tests();
				cx.executor().advance_clock(Duration::from_secs(60));
				cx.run_until_parked();
				assert_eq!(
					fixture.worker.started_connections_for_tests(),
					connections
				);
				for (ix, model) in models.iter().enumerate() {
					model.read_with(cx, |m, _| {
						assert_eq!(m.discovery_generation, generations[ix]);
						assert_eq!(
							m.remote.scan_error.as_ref().unwrap().key,
							"remote_worker_too_old"
						);
						assert!(m.remote.scan_recovery.is_none());
					});
				}
			}

			#[gpui::test]
			fn restored_remote_scan_retry_is_cancelled_by_close_and_quit_gestures(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(usize::MAX, None);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				let background_client = models[0].read_with(cx, |m, _| {
					std::sync::Arc::downgrade(
						&m.remote.session.as_ref().unwrap().client,
					)
				});
				let before =
					fixture.worker.scan_requests_for_tests(&fixture.paths[0]);
				click_id(cx, "ws-tab-close:1".into());
				wait(cx, "background retry cancelled and tab closed", |cx| {
					count(&root, cx) == 2
						&& background_client.upgrade().is_none()
				});
				assert_eq!(
					fixture.worker.scan_requests_for_tests(&fixture.paths[0]),
					before
				);
				assert_eq!(tab(&root, cx, 1), models[1]);
				let active_before =
					fixture.worker.scan_requests_for_tests(&fixture.paths[1]);
				cx.simulate_keystrokes("cmd-q");
				wait(cx, "quit cancels pending retry", |cx| {
					root.read_with(cx, |r, _| r.quit_sent)
				});
				cx.executor().advance_clock(Duration::from_secs(60));
				cx.run_until_parked();
				assert_eq!(
					fixture.worker.scan_requests_for_tests(&fixture.paths[0]),
					before
				);
				assert_eq!(
					fixture.worker.scan_requests_for_tests(&fixture.paths[1]),
					active_before
				);
			}

			#[gpui::test]
			fn restored_remote_scan_refresh_replaces_the_pending_retry(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(usize::MAX, None);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				for path in &fixture.paths {
					fixture.worker.set_scan_failures_for_tests(path, 0, None);
				}
				let before =
					fixture.worker.scan_requests_for_tests(&fixture.paths[1]);
				click_id(cx, "btn-refresh".into());
				pump_scan_until(cx, "Refresh recovers the active tab", |cx| {
					models[1].read_with(cx, |m, _| {
						!m.is_loading
							&& m.remote.scan_error.is_none()
							&& m.repos.len() == 1
					})
				});
				let refreshed =
					fixture.worker.scan_requests_for_tests(&fixture.paths[1]);
				assert_eq!(refreshed, before + 1);
				cx.executor().advance_clock(Duration::from_secs(30));
				cx.run_until_parked();
				assert_eq!(
					fixture.worker.scan_requests_for_tests(&fixture.paths[1]),
					refreshed,
					"old timer must not launch a second scan"
				);
				assert_eq!(active(&root, cx), Some(2));
			}

			#[gpui::test]
			fn restored_initial_remote_scan_resumes_after_quit_is_aborted(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(usize::MAX, None);
				let (root, cx) = fixture.launch(cx);
				let models = initial_scan_failures(&root, cx);
				let local = tab(&root, cx, 0);
				let held = hold_job(&local, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
				for path in &fixture.paths {
					fixture.worker.set_scan_failures_for_tests(path, 0, None);
				}
				local.update(cx, |m, cx| {
					let step = m.lifecycle.poll_at(
						std::time::Instant::now()
							+ crate::lifecycle::DRAIN_DEADLINE,
						crate::lifecycle::GitLoad::idle(),
					);
					assert!(matches!(
						step,
						crate::lifecycle::Step::Failed { .. }
					));
					m.resume_after_drain(cx);
					cx.emit(TabEvent::DrainFailed);
				});
				held.set(true);
				wait(cx, "initial scans resume after stopped quit", |cx| {
					models.iter().all(|model| {
						model.read_with(cx, |m, _| {
							m.remote.session.is_some()
								&& !m.is_loading && m.remote.scan_error.is_none()
								&& m.repos.len() == 1
						})
					})
				});
				assert_eq!(count(&root, cx), 3);
				assert_eq!(tab(&root, cx, 1), models[0]);
				assert_eq!(tab(&root, cx, 2), models[1]);
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
			}

			#[gpui::test]
			fn restored_remote_open_missing_workspace_is_retried_until_it_exists(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let mut fixture = ScanRestoreFixture::new(0, None);
				let missing = Path::new(&fixture.paths[1])
					.join("missing")
					.to_string_lossy()
					.into_owned();
				fixture.saved.tabs[2] = crate::open_tabs::SavedTab::Remote {
					host: fixture.host.name.clone(),
					path: missing,
				};
				let (root, cx) = fixture.launch(cx);
				let model = tab(&root, cx, 2);
				pump_scan_until(cx, "missing workspace open fails", |cx| {
					model.read_with(cx, |m, _| {
						!m.remote.busy
							&& m.remote
								.restoring
								.as_ref()
								.is_some_and(|r| r.failed.is_some())
					})
				});
				wait_scaled(
					cx,
					"background opens after the active permanent failure",
					|cx| {
						tab(&root, cx, 1).read_with(cx, |m, _| {
							m.remote.session.is_some()
								&& !m.is_loading && m.repos.len() == 1
						})
					},
				);
				model.read_with(cx, |m, _| {
					assert_eq!(
						m.remote.restoring.as_ref().unwrap().attempts,
						1
					);
					assert!(m.remote.session.is_none());
				});
				// A folder that is not there yet is a start that may clear:
				// it is tried again on the backoff, and connects once it exists.
				cx.executor().advance_clock(crate::remote::retry_delay(1));
				pump_scan_until(cx, "the second try fails too", |cx| {
					model.read_with(cx, |m, _| {
						!m.remote.busy
							&& m.remote.restoring.as_ref().is_some_and(|r| {
								r.attempts == 2 && r.failed.is_some()
							})
					})
				});
				repo(
					Path::new(&fixture.paths[1]),
					"missing",
					&[("kept.txt", "unchanged\n")],
				);
				cx.executor().advance_clock(crate::remote::retry_delay(2));
				pump_scan_until(
					cx,
					"the folder appears and the tab connects",
					|cx| model.read_with(cx, |m, _| m.remote.session.is_some()),
				);
				model.read_with(cx, |m, _| {
					assert!(m.remote.restoring.is_none())
				});
				assert_eq!(saved(fixture.store.path()), fixture.saved);
			}

			/// A start that ssh failed for a reason nobody classified (the
			/// network not up yet after login) keeps the tab and retries on
			/// the backoff; the tab connects once the host answers.
			#[cfg(unix)]
			#[gpui::test]
			fn restored_remote_unclassified_start_failure_is_retried(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let (down, up, tabs, store, _shared) =
					unreachable_host_restore();
				let (root, cx) =
					launch_with_test_hosts(cx, &tabs, &store, vec![down]);
				let remote = tab(&root, cx, 0);
				pump_scan_until(cx, "the first start fails", |cx| {
					remote.read_with(cx, |m, _| {
						!m.remote.busy
							&& m.remote
								.restoring
								.as_ref()
								.is_some_and(|r| r.failed.is_some())
					})
				});
				assert!(drawn(cx, "ws-tab-state:0:failed"));
				remote.read_with(cx, |m, _| {
					assert_eq!(
						m.remote.restoring.as_ref().unwrap().attempts,
						1
					);
				});

				// The network is up: the next scheduled try connects the same tab.
				remote.update(cx, |m, _| m.remote.hosts = vec![up]);
				cx.executor().advance_clock(crate::remote::retry_delay(1));
				pump_scan_until(cx, "the retry connects", |cx| {
					remote.read_with(cx, |m, _| m.remote.session.is_some())
				});
				assert_eq!(count(&root, cx), 1, "no new tab");
				assert_eq!(saved(store.path()).tabs, tabs.tabs);
			}

			/// Refresh on a failed restored tab tries the reconnect at once,
			/// without waiting for its backoff timer.
			#[cfg(unix)]
			#[gpui::test]
			fn refresh_retries_a_failed_restore_at_once(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let (down, up, tabs, store, _shared) =
					unreachable_host_restore();
				let (root, cx) =
					launch_with_test_hosts(cx, &tabs, &store, vec![down]);
				let remote = tab(&root, cx, 0);
				pump_scan_until(cx, "the first start fails", |cx| {
					remote.read_with(cx, |m, _| {
						!m.remote.busy
							&& m.remote
								.restoring
								.as_ref()
								.is_some_and(|r| r.failed.is_some())
					})
				});
				// The host is back, but the clock stays put: only Refresh can
				// try now, the backoff timer is 5 s away.
				remote.update(cx, |m, _| m.remote.hosts = vec![up]);
				click_id(cx, "btn-refresh".into());
				pump_scan_until(cx, "Refresh connects the tab", |cx| {
					remote.read_with(cx, |m, _| m.remote.session.is_some())
				});
				assert_eq!(count(&root, cx), 1, "no new tab");
			}

			#[gpui::test]
			fn restored_active_remote_workspace_is_opened_before_background(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(0, None);
				let (root, cx) = fixture.launch(cx);
				wait_scaled(
					cx,
					"both remote workspaces finish opening",
					|cx| {
						[1, 2].into_iter().all(|ix| {
							tab(&root, cx, ix).read_with(cx, |m, _| {
								m.remote.session.is_some() && !m.is_loading
							})
						})
					},
				);
				assert_eq!(
					fixture.worker.opened_workspaces_for_tests(),
					[fixture.paths[1].clone(), fixture.paths[0].clone()]
				);
				assert_eq!(saved(fixture.store.path()), fixture.saved);
			}

			#[gpui::test]
			fn restored_remote_scan_continuation_retry_keeps_its_scope_and_existing_repos(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(0, None);
				let deep_parent = Path::new(&fixture.paths[1])
					.join("d1/d2/d3/d4/d5/d6/d7/d8/d9");
				fs::create_dir_all(&deep_parent).unwrap();
				repo(&deep_parent, "deep", &[]);
				let (root, cx) = fixture.launch(cx);
				let model = tab(&root, cx, 2);
				wait_scaled(
					cx,
					"initial scan finds one repo and the depth boundary",
					|cx| {
						model.read_with(cx, |m, _| {
							m.remote.session.is_some()
								&& !m.is_loading && m.repos.len() == 1
								&& !m.discovery_depth_limited.is_empty()
						})
					},
				);
				let continued = model
					.read_with(cx, |m, _| m.discovery_depth_limited[0].clone());
				cx.simulate_keystrokes("alt-1");
				settle(cx);
				fixture.worker.set_scan_failures_for_tests(
					&fixture.paths[1],
					2,
					None,
				);
				click_id(cx, "btn-discovery-continue".into());
				pump_scan_until(
					cx,
					"continuation hits the injected scan failure",
					|cx| {
						model.read_with(cx, |m, _| {
							!m.is_loading && m.remote.scan_error.is_some()
						})
					},
				);
				model.read_with(cx, |m, _| {
					let retry = m
						.remote
						.scan_recovery
						.as_ref()
						.expect("continuation retry retained");
					assert_eq!(retry.under.as_ref(), Some(&continued));
					assert!(!retry.wipe);
					assert_eq!(
						m.repos.len(),
						1,
						"partial scan failure must not erase the earlier repo"
					);
				});
				for seconds in [1, 2, 4] {
					cx.executor().advance_clock(Duration::from_secs(seconds));
					cx.run_until_parked();
					pump_scan_until(
						cx,
						"continuation attempt finishes",
						|cx| model.read_with(cx, |m, _| !m.is_loading),
					);
				}
				model.read_with(cx, |m, _| {
					assert!(m.remote.scan_error.is_none());
					let mut names: Vec<_> =
						m.repos.iter().map(|r| r.name.as_str()).collect();
					names.sort_unstable();
					assert_eq!(names, ["active", "deep"]);
				});
			}

			#[gpui::test]
			fn closing_the_prioritized_restore_releases_background_workspaces(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(0, None);
				let (root, cx) = crate::remote::with_test_restore_delay(
					&fixture.paths[1],
					crate::remote::RESTORE_PRIORITY_LIMIT * 4,
					|| fixture.launch(cx),
				);
				cx.run_until_parked();
				assert!(tab(&root, cx, 2).read_with(cx, |m, _| m.remote.busy));
				assert_eq!(fixture.worker.started_connections_for_tests(), 0);
				let bounds = cx
					.debug_bounds("ws-tab-close:2")
					.expect("active restore close button drawn");
				cx.simulate_click(bounds.center(), gpui::Modifiers::none());
				wait_scaled(
					cx,
					"active restore closes and background finishes",
					|cx| {
						count(&root, cx) == 2
							&& tab(&root, cx, 1).read_with(cx, |m, _| {
								m.remote.session.is_some()
									&& !m.is_loading && m.repos.len() == 1
							})
					},
				);
				assert_eq!(
					saved(fixture.store.path()).tabs,
					fixture.saved.tabs[..2]
				);
				assert_eq!(
					fixture.worker.opened_workspaces_for_tests(),
					[fixture.paths[0].clone()]
				);
			}

			#[gpui::test]
			fn prioritized_remote_restore_wait_has_a_deadline(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let fixture = ScanRestoreFixture::new(0, None);
				let limit = crate::remote::RESTORE_PRIORITY_LIMIT;
				let (root, cx) = crate::remote::with_test_restore_delay(
					&fixture.paths[1],
					limit * 4,
					|| fixture.launch(cx),
				);
				cx.run_until_parked();
				cx.executor()
					.advance_clock(limit - Duration::from_millis(100));
				cx.run_until_parked();
				assert_eq!(
					fixture.worker.started_connections_for_tests(),
					0,
					"same-host background waits before priority deadline"
				);
				cx.executor().advance_clock(Duration::from_millis(150));
				wait_scaled(
					cx,
					"background opens after bounded priority wait",
					|cx| {
						tab(&root, cx, 1).read_with(cx, |m, _| {
							m.remote.session.is_some()
								&& !m.is_loading && m.repos.len() == 1
						})
					},
				);
				assert_eq!(
					fixture.worker.opened_workspaces_for_tests(),
					[fixture.paths[0].clone()]
				);
				assert!(tab(&root, cx, 2).read_with(cx, |m, _| m.remote.busy
					&& m.remote.session.is_none()));
				assert_eq!(active(&root, cx), Some(2));
				cx.simulate_keystrokes("cmd-q");
				wait(cx, "quit cancels delayed priority leader", |cx| {
					root.read_with(cx, |r, _| r.quit_sent)
				});
			}

			#[gpui::test]
			fn prioritized_remote_restore_does_not_block_another_host(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let mut fixture = ScanRestoreFixture::new(0, None);
				let other_worker =
					std::sync::Arc::new(snip_remote::Worker::new(
						snip_remote::WorkerOptions::default(),
					));
				other_worker.set_scan_failures_for_tests(
					&fixture.paths[0],
					0,
					None,
				);
				let mut other_host =
					snip_remote::RemoteHost::in_process(other_worker.clone());
				other_host.name = "other-worker".into();
				fixture.saved.tabs[1] = crate::open_tabs::SavedTab::Remote {
					host: other_host.name.clone(),
					path: fixture.paths[0].clone(),
				};
				let (root, cx) = crate::remote::with_test_restore_delay(
					&fixture.paths[1],
					crate::remote::RESTORE_PRIORITY_LIMIT * 4,
					|| {
						fixture.launch_with_hosts(
							cx,
							vec![fixture.host.clone(), other_host],
						)
					},
				);
				let other = tab(&root, cx, 1);
				pump_scan_until(
					cx,
					"another host opens without waiting for the leader",
					|cx| {
						if other.read_with(cx, |m, _| m.lifecycle.is_draining())
						{
							cx.executor()
								.advance_clock(Duration::from_millis(50));
						}
						other.read_with(cx, |m, _| {
							m.remote.session.is_some()
								&& !m.is_loading && m.repos.len() == 1
						})
					},
				);
				assert_eq!(fixture.worker.started_connections_for_tests(), 0);
				assert_eq!(
					other_worker.opened_workspaces_for_tests(),
					[fixture.paths[0].clone()]
				);
				cx.simulate_keystrokes("cmd-q");
				wait(cx, "quit cancels delayed other host", |cx| {
					root.read_with(cx, |r, _| r.quit_sent)
				});
			}

			/// A launch reopens the saved tabs in order and shows the one
			/// that was shown. A remote tab whose host cannot be reached
			/// keeps its place and says why, and connects on a later try.
			#[gpui::test]
			fn a_launch_restores_the_saved_tabs_and_retries_a_remote_one(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::{OpenTabs, SavedTab};
				use crate::tabs::Conn;
				let _remote = remote_lock();
				let (_tmp, a, b) = two_workspaces();
				let shared_tmp = tempfile::tempdir().unwrap();
				let shared = dunce::canonicalize(shared_tmp.path()).unwrap();
				repo(&shared, "proj", &[("remote.txt", "r\n")]);
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				let host = snip_remote::RemoteHost::in_process(worker);
				let store = tempfile::tempdir().unwrap();
				let remote_tab = SavedTab::Remote {
					host: host.name.clone(),
					path: shared.display().to_string(),
				};
				let tabs = OpenTabs {
					tabs: vec![local_tab(&a), remote_tab, local_tab(&b)],
					active: Some(2),
				};
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: Some(tabs.clone()),
						first: FirstTab::Empty,
						restore_dir: None,
						store: Some(store.path().to_path_buf()),
					},
				);
				let name =
					shared.file_name().unwrap().to_string_lossy().into_owned();
				assert_eq!(
					labels(&root, cx),
					[
						"ws-a".to_string(),
						format!("{} ▸ {name}", host.name),
						"ws-b".to_string()
					]
				);
				assert_eq!(active(&root, cx), Some(2));
				assert!(focused_in(&tab(&root, cx, 2), cx));
				// No ssh host has that name: the tab stays, failed, and its
				// hover text says why.
				let views = root.read_with(cx, |r, cx| r.tab_views(cx));
				assert_eq!(views[1].1.conn, Some(Conn::Failed));
				let why = views[1].1.failed.clone().expect("a reason");
				let why = why.render(crate::i18n::Locale::En);
				assert!(why.contains(&host.name), "{why}");
				click_id(cx, "ws-tab:1".into());
				assert!(drawn(cx, "ws-tab-state:1:failed"));
				let remote = tab(&root, cx, 1);
				remote.read_with(cx, |m, _| {
					assert_eq!(m.status.key, "remote_open_failed");
				});
				assert_eq!(
					saved(store.path()).tabs,
					tabs.tabs,
					"kept while failed"
				);

				// The host appears; the next try connects the same tab.
				remote.update(cx, |m, _| m.remote.hosts = vec![host.clone()]);
				cx.executor().advance_clock(crate::remote::retry_delay(1));
				settle(cx);
				wait(cx, "the retry connects", |cx| {
					remote.read_with(cx, |m, _| m.remote.session.is_some())
				});
				assert_eq!(count(&root, cx), 3, "no new tab");
				let views = root.read_with(cx, |r, cx| r.tab_views(cx));
				assert_eq!(views[1].1.conn, Some(Conn::Connected));
				assert_eq!(views[1].1.failed, None);
				assert_eq!(
					saved(store.path()),
					OpenTabs {
						tabs: tabs.tabs.clone(),
						active: Some(1),
					}
				);
			}

			/// A saved folder that is gone is skipped, and the status bar
			/// of the tab shown says how many were.
			#[gpui::test]
			fn a_missing_folder_is_skipped_and_counted(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::OpenTabs;
				let (tmp, a, _b) = two_workspaces();
				let gone = tmp.path().join("gone");
				let store = tempfile::tempdir().unwrap();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: Some(OpenTabs {
							tabs: vec![local_tab(&gone), local_tab(&a)],
							active: Some(0),
						}),
						first: FirstTab::Empty,
						restore_dir: None,
						store: Some(store.path().to_path_buf()),
					},
				);
				let m = tab(&root, cx, 0);
				// On the status bar, and in a card that outlives the
				// statuses of the tab's first load.
				m.read_with(cx, |m, _| {
					let (_, ok, msg) = m.toast.clone().expect("a card");
					assert!(!ok);
					assert_eq!(msg.key, "workspace_restore_skipped");
					assert_eq!(msg.args, ["1"]);
				});
				wait(cx, "workspace a", |cx| workspace_ready(&m, cx));
				assert_eq!(labels(&root, cx), ["ws-a"]);
				assert_eq!(
					saved(store.path()),
					OpenTabs {
						tabs: vec![local_tab(&a)],
						active: Some(0),
					}
				);
			}

			/// `--workspace` after a restore: a folder already restored,
			/// even spelled through a symlink, is switched to; a new one is
			/// added. Either way it is shown, and only it pastes into
			/// `--restore-dir`.
			#[gpui::test]
			fn the_workspace_argument_is_shown_and_alone_gets_the_restore_dir(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::OpenTabs;
				let (tmp, a, b) = two_workspaces();
				let dest = tmp.path().join("dest");
				fs::create_dir(&dest).unwrap();
				#[cfg(unix)]
				let arg = {
					let link = tmp.path().join("link-to-b");
					std::os::unix::fs::symlink(&b, &link).unwrap();
					link
				};
				#[cfg(not(unix))]
				let arg = b.clone();
				let tabs = OpenTabs {
					tabs: vec![local_tab(&a), local_tab(&b)],
					active: Some(0),
				};
				let launch = |first| Launch {
					restore: Some(tabs.clone()),
					first,
					restore_dir: Some(dest.clone()),
					store: None,
				};
				let (root, cx) = launch_tabs(cx, launch(FirstTab::Local(arg)));
				assert_eq!(labels(&root, cx), ["ws-a", "ws-b"]);
				assert_eq!(active(&root, cx), Some(1));
				let dest_of = |ix, cx: &mut VisualTestContext| {
					tab(&root, cx, ix)
						.read_with(cx, |m, _| m.current_restore_destination())
				};
				assert_eq!(dest_of(1, cx), dest);
				assert_ne!(dest_of(0, cx), dest);
				// A tab opened later pastes into its own repo.
				click_id(cx, "ws-tab-new".into());
				let c = tmp.path().join("ws-c");
				fs::create_dir(&c).unwrap();
				repo(&c, "gamma", &[]);
				open_typed(&root, cx, &c);
				wait(cx, "tab c", |cx| {
					count(&root, cx) == 3
						&& workspace_ready(&tab(&root, cx, 2), cx)
				});
				assert_ne!(dest_of(2, cx), dest);
				let _ = c;
			}

			#[gpui::test]
			fn a_new_workspace_argument_is_added_after_the_restored_tabs(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::OpenTabs;
				let (tmp, a, b) = two_workspaces();
				let c = tmp.path().join("ws-c");
				fs::create_dir(&c).unwrap();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: Some(OpenTabs {
							tabs: vec![local_tab(&a), local_tab(&b)],
							active: Some(0),
						}),
						first: FirstTab::Local(c.clone()),
						restore_dir: None,
						store: None,
					},
				);
				assert_eq!(labels(&root, cx), ["ws-a", "ws-b", "ws-c"]);
				assert_eq!(active(&root, cx), Some(2));
			}

			/// Nothing left open last time: one empty tab, and the empty
			/// list stays saved.
			#[gpui::test]
			fn an_empty_saved_list_opens_one_empty_tab(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::OpenTabs;
				let store = tempfile::tempdir().unwrap();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: Some(OpenTabs::default()),
						first: FirstTab::Empty,
						restore_dir: None,
						store: Some(store.path().to_path_buf()),
					},
				);
				assert_eq!(count(&root, cx), 1);
				assert!(
					tab(&root, cx, 0).read_with(cx, |m, _| m.is_empty_tab())
				);
				assert_eq!(saved(store.path()), OpenTabs::default());
			}

			/// Opening, switching and closing tabs by mouse and keys each
			/// save the tabs; quitting leaves them as they were.
			#[gpui::test]
			fn tabs_are_saved_on_open_switch_and_close_but_not_on_quit(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::OpenTabs;
				let (tmp, a, b) = two_workspaces();
				let store = tempfile::tempdir().unwrap();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: None,
						first: FirstTab::Local(a.clone()),
						restore_dir: None,
						store: Some(store.path().to_path_buf()),
					},
				);
				let both = |active| OpenTabs {
					tabs: vec![local_tab(&a), local_tab(&b)],
					active: Some(active),
				};
				assert_eq!(
					saved(store.path()),
					OpenTabs {
						tabs: vec![local_tab(&a)],
						active: Some(0)
					}
				);
				click_id(cx, "ws-tab-new".into());
				open_typed(&root, cx, &b);
				wait(cx, "tab b", |cx| {
					count(&root, cx) == 2
						&& workspace_ready(&tab(&root, cx, 1), cx)
				});
				assert_eq!(saved(store.path()), both(1));
				click_id(cx, "ws-tab:0".into());
				assert_eq!(saved(store.path()), both(0));
				cx.simulate_keystrokes("cmd-2");
				settle(cx);
				assert_eq!(saved(store.path()), both(1));
				// An empty tab is not saved, but the shown one is unknown.
				click_id(cx, "ws-tab-new".into());
				assert_eq!(
					saved(store.path()),
					OpenTabs {
						tabs: both(0).tabs,
						active: None
					}
				);
				cx.simulate_keystrokes("cmd-w");
				settle(cx);
				wait(cx, "empty tab closed", |cx| count(&root, cx) == 2);
				cx.simulate_keystrokes("cmd-1 cmd-w");
				wait(cx, "tab a closed", |cx| count(&root, cx) == 1);
				let only_b = OpenTabs {
					tabs: vec![local_tab(&b)],
					active: Some(0),
				};
				assert_eq!(saved(store.path()), only_b);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				wait(cx, "quit", |cx| root.read_with(cx, |r, _| r.quit_sent));
				assert_eq!(
					saved(store.path()),
					only_b,
					"a quit keeps the tabs"
				);
				let _ = tmp;
			}
			/// A quit that stops (a tab's drain gave up) resumes its tabs one
			/// by one; the saved tabs stay as they were before the quit.
			#[gpui::test]
			fn a_stopped_quit_leaves_the_saved_tabs_alone(
				cx: &mut TestAppContext,
			) {
				let (_tmp, a, b) = two_workspaces();
				let store = tempfile::tempdir().unwrap();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: None,
						first: FirstTab::Local(a.clone()),
						restore_dir: None,
						store: Some(store.path().to_path_buf()),
					},
				);
				click_id(cx, "ws-tab-new".into());
				open_typed(&root, cx, &b);
				wait(cx, "tab b", |cx| {
					count(&root, cx) == 2
						&& workspace_ready(&tab(&root, cx, 1), cx)
				});
				let file = store.path().join("open-tabs.json");
				let before = fs::read(&file).unwrap();
				let (ta, tb) = (tab(&root, cx, 0), tab(&root, cx, 1));
				let b_load = hold_job(&tb, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert_eq!(fs::read(&file).unwrap(), before);
				tb.update(cx, |_, cx| cx.emit(TabEvent::DrainFailed));
				settle(cx);
				assert_eq!(fs::read(&file).unwrap(), before, "while resuming");
				b_load.set(true);
				wait(cx, "A reloaded", |cx| {
					ta.read_with(cx, |m, _| {
						!m.files.is_empty() && !m.is_loading
					})
				});
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
				assert_eq!(fs::read(&file).unwrap(), before, "after the stop");
			}

			/// A restored remote tab that never connected closes like any
			/// other, and leaves the saved tabs.
			#[gpui::test]
			fn a_failed_restored_tab_closes_and_is_no_longer_saved(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::{OpenTabs, SavedTab};
				let (_tmp, a, _b) = two_workspaces();
				let store = tempfile::tempdir().unwrap();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: Some(OpenTabs {
							tabs: vec![
								local_tab(&a),
								SavedTab::Remote {
									host: "snip-test-gone-host".into(),
									path: "/srv/x".into(),
								},
							],
							active: Some(1),
						}),
						first: FirstTab::Empty,
						restore_dir: None,
						store: Some(store.path().to_path_buf()),
					},
				);
				assert!(drawn(cx, "ws-tab-state:1:failed"));
				cx.simulate_keystrokes("cmd-w");
				wait(cx, "failed tab closed", |cx| count(&root, cx) == 1);
				assert_eq!(
					saved(store.path()),
					OpenTabs {
						tabs: vec![local_tab(&a)],
						active: Some(0),
					}
				);
			}
			fn interrupted_restore(
				cx: &mut TestAppContext,
				answer_before_abort: bool,
				first_fails: bool,
			) {
				let tmp = tempfile::tempdir().unwrap();
				let path = tmp.path().join("restored");
				if !first_fails {
					fs::create_dir(&path).unwrap();
				}
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				let (root, cx) = open_tabs(cx, FirstTab::Empty, None);
				let blocker = tab(&root, cx, 0);
				click_id(cx, "ws-tab-new".into());
				let restoring = tab(&root, cx, 1);
				let held = hold_job(&blocker, cx, JobKind::UncancellableRead);
				restoring.update(cx, |m, cx| {
					let host =
						snip_remote::RemoteHost::in_process(worker.clone());
					let name = host.name.clone();
					m.remote.hosts = vec![host];
					m.e2e_remote_open_delay = Some(Duration::from_secs(2));
					m.restore_remote(
						crate::remote::RecentRemote {
							host: name,
							path: path.display().to_string(),
						},
						cx,
					);
				});
				cx.run_until_parked();
				assert!(restoring.read_with(cx, |m, _| m.remote.busy));
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				if answer_before_abort {
					cx.executor().advance_clock(Duration::from_secs(2));
					wait(cx, "first restore finished", |cx| {
						!restoring.read_with(cx, |m, _| m.remote.busy)
					});
				}
				if first_fails {
					// The first cancelled open may still answer late. Make
					// the folder available before the genuine resumed open;
					// a new NotFound refusal is permanent, not a retry signal.
					fs::create_dir(&path).unwrap();
				}
				// The other tab's real Lifecycle reports the same deadline
				// failure as poll_lifecycle, without sleeping eight seconds.
				blocker.update(cx, |m, cx| {
					let step = m.lifecycle.poll_at(
						std::time::Instant::now()
							+ crate::lifecycle::DRAIN_DEADLINE,
						crate::lifecycle::GitLoad::idle(),
					);
					assert!(matches!(
						step,
						crate::lifecycle::Step::Failed { .. }
					));
					m.resume_after_drain(cx);
					cx.emit(TabEvent::DrainFailed);
				});
				held.set(true);
				settle(cx);
				if !answer_before_abort {
					cx.executor().advance_clock(Duration::from_secs(2));
					wait(cx, "first restore finished", |cx| {
						!restoring.read_with(cx, |m, _| m.remote.busy)
							|| restoring.read_with(cx, |m, _| {
								m.remote
									.restoring
									.as_ref()
									.is_some_and(|r| r.attempts > 1)
							})
					});
				}
				cx.executor().advance_clock(Duration::from_secs(2));
				wait(
					cx,
					"restore resumed in the same tab after stopped quit",
					|cx| workspace_ready(&restoring, cx),
				);
				assert_eq!(count(&root, cx), 2);
				assert_eq!(tab(&root, cx, 1), restoring);
				assert!(restoring.read_with(cx, |m, _| m
					.remote
					.restoring
					.is_none() && m
					.remote
					.session
					.is_some()));
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
			}

			#[gpui::test]
			fn completed_restore_resumes_after_quit_is_aborted(
				cx: &mut TestAppContext,
			) {
				interrupted_restore(cx, true, false);
			}

			#[gpui::test]
			fn late_failed_restore_resumes_after_quit_is_aborted(
				cx: &mut TestAppContext,
			) {
				interrupted_restore(cx, false, true);
			}

			/// A retry that falls while the tab drains for a quit that is
			/// then stopped is put off, not dropped: the tab keeps trying.
			#[gpui::test]
			fn a_retry_during_a_stopped_quit_still_comes(
				cx: &mut TestAppContext,
			) {
				use crate::open_tabs::{OpenTabs, SavedTab};
				let (_tmp, a, _b) = two_workspaces();
				let (root, cx) = launch_tabs(
					cx,
					Launch {
						restore: Some(OpenTabs {
							tabs: vec![
								local_tab(&a),
								SavedTab::Remote {
									host: "snip-test-gone-host".into(),
									path: "/srv/x".into(),
								},
							],
							active: Some(0),
						}),
						first: FirstTab::Empty,
						restore_dir: None,
						store: None,
					},
				);
				let tb = tab(&root, cx, 1);
				let attempts = |cx: &mut VisualTestContext| {
					tb.read_with(cx, |m, _| {
						m.remote.restoring.as_ref().map(|r| r.attempts)
					})
				};
				assert_eq!(attempts(cx), Some(1));
				let held = hold_job(&tb, cx, JobKind::UncancellableRead);
				cx.simulate_keystrokes("cmd-q");
				settle(cx);
				assert!(tb.read_with(cx, |m, _| m.lifecycle.is_draining()));
				// The first retry is due while the tab drains.
				cx.executor().advance_clock(crate::remote::retry_delay(1));
				settle(cx);
				assert_eq!(attempts(cx), Some(1), "no try during the drain");
				tb.update(cx, |_, cx| cx.emit(TabEvent::DrainFailed));
				settle(cx);
				held.set(true);
				wait(cx, "the drain ends", |cx| {
					!tb.read_with(cx, |m, _| m.lifecycle.is_draining())
				});
				assert!(!root.read_with(cx, |r, _| r.quit_sent));
				cx.executor().advance_clock(crate::remote::retry_delay(1));
				settle(cx);
				assert_eq!(
					attempts(cx),
					Some(2),
					"the retry came after the stop"
				);
			}
		}

		/// Log toolbar tooltips.
		mod log_tooltips {
			use super::workspace_tabs::{
				click_id, covered_by_tooltip, tooltip_shown,
			};
			use super::*;

			/// The Log toolbar's More button opens a menu below itself:
			/// its tooltip must not cover the first item (#203).
			#[gpui::test]
			fn opening_the_log_more_menu_hides_its_buttons_tooltip(
				cx: &mut TestAppContext,
			) {
				let ws = tempfile::tempdir().unwrap();
				let r = repo(ws.path(), "my-repo", &[]);
				fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
				git(&r, &["add", "."]);
				git(&r, &["commit", "-q", "-m", "commit c1"]);
				let (model, cx) =
					open_sized(cx, ws.path().to_path_buf(), None, 1080., 720.);
				for _ in 0..50 {
					settle(cx);
					if model.read_with(cx, |m, _| !m.commits.is_empty()) {
						break;
					}
				}
				settle(cx);
				let loc = model.read_with(cx, |m, _| m.locale);
				let text = crate::i18n::t("tip_log_more", loc).to_string();
				// Clicked while the tooltip still waits to show.
				let at = cx
					.debug_bounds("btn-log-more")
					.expect("More drawn")
					.center();
				cx.simulate_mouse_move(at, None, gpui::Modifiers::none());
				click_id(cx, "btn-log-more".into());
				cx.executor().advance_clock(Duration::from_millis(700));
				settle(cx);
				assert_eq!(
					model.read_with(cx, |m, _| m.overlays.log_menu()),
					Some(crate::ui::LogMenu::More),
					"the menu is open"
				);
				assert!(
					!tooltip_shown(cx, &text),
					"the tooltip stays over the opened menu"
				);
				let item = cx
					.debug_bounds("log-more:details")
					.expect("first item drawn")
					.center();
				assert!(!covered_by_tooltip(cx, item, &text));
			}
		}

		/// Floating and inline layers have one owner (#226): the shared
		/// rules (Escape, exclusion, return of the focus, a tab switch) are
		/// pinned over every layer, not over the one that last broke.
		mod overlays {
			use super::workspace_tabs::{
				active, click_id, hover_id, sel, tab, tooltip_shown, two_tabs,
				wait_scaled,
			};
			use super::*;
			use gpui::{Modifiers, MouseButton};

			/// Every open layer by name: the one place a test reads them.
			fn open_kinds(m: &WorkbenchModel) -> Vec<String> {
				use crate::overlay::OverlayKind as K;
				let mut v = Vec::new();
				match m.overlays.floating() {
					Some(K::WorkspaceMenu) => v.push("workspace-menu".into()),
					Some(K::LogMenu(k)) => v.push(format!("log-menu:{k:?}")),
					Some(K::Selector(k)) => v.push(format!("selector:{k:?}")),
					Some(K::ContextMenu) => v.push("context-menu".into()),
					Some(K::FindBar | K::AddRepoField | K::PasteDestField)
					| None => {}
				}
				if m.overlays.is_open(K::FindBar) {
					v.push("find-bar".to_string());
				}
				if m.overlays.is_open(K::AddRepoField) {
					v.push("add-repo".to_string());
				}
				if m.overlays.is_open(K::PasteDestField) {
					v.push("paste-dest".to_string());
				}
				v
			}

			fn is_inline(name: &str) -> bool {
				matches!(name, "find-bar" | "add-repo" | "paste-dest")
			}

			fn floating(m: &WorkbenchModel) -> Vec<String> {
				open_kinds(m)
					.into_iter()
					.filter(|k| !is_inline(k))
					.collect()
			}

			fn inline(m: &WorkbenchModel) -> Vec<String> {
				open_kinds(m).into_iter().filter(|k| is_inline(k)).collect()
			}

			fn floating_now(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> Vec<String> {
				model.read_with(cx, |m, _| floating(m))
			}

			/// The pane or field that holds the keyboard, by name.
			fn focus_name(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> &'static str {
				cx.update(|window, cx| {
					let m = model.read(cx);
					let on = |h: &gpui::FocusHandle| h.is_focused(window);
					if on(&m.tree_focus) {
						"tree"
					} else if on(&m.log_focus) {
						"log"
					} else if on(&m.reader_focus) {
						"reader"
					} else if on(&m.paste_focus) {
						"paste"
					} else if on(&m.chrome.menu_focus) {
						"menu"
					} else if on(&m.focus_handle) {
						"root"
					} else if on(&m.find_input.read(cx).handle())
						|| on(&m.goto_input.read(cx).handle())
					{
						"find-field"
					} else if on(&m.selector_input.read(cx).handle()) {
						"selector-field"
					} else if on(&m.log_path_input.read(cx).handle())
						|| on(&m.log_branch_menu_input.read(cx).handle())
					{
						"log-menu-field"
					} else if on(&m.workspace_path_input.read(cx).handle()) {
						"workspace-field"
					} else {
						"other"
					}
				})
			}

			/// The centre of a probed control (tree rows and the editor tab
			/// carry probe ids, not debug selectors), in window pixels.
			fn probed_center(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				id: &str,
			) -> gpui::Point<gpui::Pixels> {
				if model.read_with(cx, |m, _| m.probes.is_none()) {
					model.update(cx, |m, cx| {
						m.probes = Some(crate::ui::Probes::for_test());
						cx.notify();
					});
				}
				settle(cx);
				let b = model
					.read_with(cx, |m, _| m.probes.as_ref().unwrap().bounds(id))
					.unwrap_or_else(|| panic!("no probed control {id}"));
				let scale = cx.update(|window, _| window.scale_factor());
				gpui::point(
					gpui::px((b[0] + b[2] / 2) as f32 / scale),
					gpui::px((b[1] + b[3] / 2) as f32 / scale),
				)
			}

			/// A right press, the way the row's menu opens (on mouse down).
			fn right_press(
				cx: &mut VisualTestContext,
				at: gpui::Point<gpui::Pixels>,
			) {
				cx.simulate_mouse_down(
					at,
					MouseButton::Right,
					Modifiers::none(),
				);
				cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
				settle(cx);
			}

			/// Two repositories with a commit each and a dirty file in the
			/// first, opened with its Changes row previewed in the reader:
			/// every pane has something, and the Log has a repo chip.
			fn fixture(
				cx: &mut TestAppContext,
				w: f32,
				h: f32,
			) -> (
				tempfile::TempDir,
				Entity<WorkbenchModel>,
				&mut VisualTestContext,
			) {
				let ws = tempfile::tempdir().unwrap();
				for name in ["alpha", "beta"] {
					let r = repo(ws.path(), name, &[]);
					fs::write(r.join("c1.txt"), "c1 content\n").unwrap();
					git(&r, &["add", "."]);
					git(&r, &["commit", "-q", "-m", "commit c1"]);
				}
				fs::write(ws.path().join("alpha").join("base.txt"), "dirty")
					.unwrap();
				let (model, cx) =
					open_sized(cx, ws.path().to_path_buf(), None, w, h);
				for _ in 0..50 {
					settle(cx);
					if model.read_with(cx, |m, _| {
						!m.commits.is_empty() && m.log_width.get() > 0.
					}) {
						break;
					}
				}
				settle(cx);
				cx.simulate_keystrokes("alt-0");
				settle(cx);
				click_probed(&model, cx, "change-repo-toggle:unstaged:alpha");
				click_probed(&model, cx, "change-row@alpha:unstaged:base.txt");
				settle(cx);
				assert!(
					model.read_with(cx, |m, _| m.preview.is_some()),
					"the dirty file is previewed in the reader"
				);
				(ws, model, cx)
			}

			/// Every way a floating layer opens by a gesture.
			#[derive(Clone, Copy, Debug)]
			enum Opener {
				Click(&'static str, &'static str),
				RightLogRow,
				RightRepoRow,
				RightEditorTab,
			}

			impl Opener {
				/// The layer it opens.
				fn opens(self) -> &'static str {
					match self {
						Opener::Click(_, name) => name,
						_ => "context-menu",
					}
				}

				fn label(self) -> String {
					match self {
						Opener::Click(id, _) => id.to_string(),
						other => format!("{other:?}"),
					}
				}

				fn fire(
					self,
					model: &Entity<WorkbenchModel>,
					cx: &mut VisualTestContext,
				) {
					match self {
						Opener::Click(id, _) => {
							let b = cx.debug_bounds(id).unwrap_or_else(|| {
								panic!("opener {id} is not drawn")
							});
							cx.simulate_click(b.center(), Modifiers::none());
							settle_checked(cx, model);
						}
						Opener::RightLogRow => {
							let b = cx
								.debug_bounds("log-subject:0")
								.expect("a Log row is drawn");
							right_press(cx, b.center());
						}
						Opener::RightRepoRow => {
							cx.simulate_keystrokes("alt-1");
							settle_checked(cx, model);
							let at = probed_center(model, cx, "repo-row:alpha");
							right_press(cx, at);
						}
						Opener::RightEditorTab => {
							let at = probed_center(model, cx, "editor-tab");
							right_press(cx, at);
						}
					}
				}
			}

			const WIDE_OPENERS: [Opener; 11] = [
				Opener::Click("btn-workspace-menu", "workspace-menu"),
				Opener::Click("btn-repo-selector", "selector:Repo"),
				Opener::Click("btn-ref-selector", "selector:Ref"),
				Opener::Click("btn-log-more", "log-menu:More"),
				Opener::Click("log-filter-repo", "log-menu:Repo"),
				Opener::Click("log-filter-branch", "log-menu:Branch"),
				Opener::Click("log-filter-user", "log-menu:User"),
				Opener::Click("log-filter-date", "log-menu:Date"),
				Opener::RightLogRow,
				Opener::RightRepoRow,
				Opener::RightEditorTab,
			];

			/// The focus origins: every key context the keymap names, and
			/// the text fields that bind their own Escape.
			const ORIGINS: [&str; 9] = [
				"root",
				"tree",
				"log-row",
				"reader",
				"find-bar",
				"goto-bar",
				"log-search",
				"branch-filter",
				"own",
			];

			/// Settles, then holds every overlay invariant and the checks only
			/// a driver can make (what is drawn, where the keyboard is).
			pub(crate) fn settle_checked(
				cx: &mut VisualTestContext,
				model: &Entity<WorkbenchModel>,
			) {
				settle(cx);
				model.read_with(cx, |m, cx| {
					let broken =
						crate::overlay::OverlayStack::check_invariants(m, cx);
					assert!(
						broken.is_empty(),
						"overlay invariants broken: {broken:?}"
					);
				});
				assert_overlay_panels_match(cx, model);
				assert_focus_not_in_closed_layer(cx, model);
			}

			/// A layer is open exactly when its panel is drawn. The add-repo
			/// field belongs to the Project pane: drawn only while it shows.
			pub(crate) fn assert_overlay_panels_match(
				cx: &mut VisualTestContext,
				model: &Entity<WorkbenchModel>,
			) {
				use crate::overlay::OverlayKind;
				let (open, project) = model.read_with(cx, |m, _| {
					(
						OverlayKind::ALL
							.into_iter()
							.filter(|k| m.overlays.is_open(*k))
							.collect::<Vec<_>>(),
						m.active_tab == crate::WorkbenchTab::FileExplorer,
					)
				});
				let mut selectors: Vec<&'static str> = OverlayKind::ALL
					.iter()
					.map(|k| k.panel_selector())
					.collect();
				selectors.dedup();
				selectors.sort_unstable();
				selectors.dedup();
				for sel in selectors {
					if sel == OverlayKind::AddRepoField.panel_selector()
						&& !project
					{
						continue;
					}
					let want = open.iter().any(|k| k.panel_selector() == sel);
					let drawn = cx.debug_bounds(sel).is_some();
					assert_eq!(
						drawn, want,
						"panel {sel}: drawn {drawn}, open layers {open:?}"
					);
				}
			}

			/// The keyboard is on no handle of a layer that is not open.
			pub(crate) fn assert_focus_not_in_closed_layer(
				cx: &mut VisualTestContext,
				model: &Entity<WorkbenchModel>,
			) {
				use crate::overlay::OverlayKind;
				cx.update(|window, cx| {
					let m = model.read(cx);
					for kind in OverlayKind::ALL {
						if m.overlays.is_open(kind) {
							continue;
						}
						// A handle two kinds share is open while either is.
						for h in m.owned_handles(kind, cx) {
							let shared =
								OverlayKind::ALL.into_iter().any(|k| {
									m.overlays.is_open(k)
										&& m.owned_handles(k, cx).contains(&h)
								});
							assert!(
								shared || !h.is_focused(window),
								"the keyboard is on a handle of {kind:?}, which is closed"
							);
						}
					}
				});
			}

			/// Closes whatever a failed cell left open, so one failure does not
			/// become every later cell's.
			fn reset(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) {
				model.update(cx, |m, cx| {
					m.drop_overlays(crate::overlay::DropScope::All);
					cx.notify();
				});
				focus_origin(model, cx, "root");
			}

			fn drawn_among(
				cx: &mut VisualTestContext,
				ids: &[&'static str],
			) -> Vec<&'static str> {
				ids.iter()
					.copied()
					.filter(|id| cx.debug_bounds(id).is_some())
					.collect()
			}

			/// Puts the keyboard in `origin` the way a user leaves it there:
			/// Alt+N or a click moves a pane, a field takes its own focus.
			fn focus_origin(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				origin: &str,
			) {
				if origin == "own" {
					return;
				}
				let h = cx.update(|_, cx| {
					let m = model.read(cx);
					match origin {
						"find-bar" => m.find_input.read(cx).handle(),
						"goto-bar" => m.goto_input.read(cx).handle(),
						"log-search" => m.log_search_input.read(cx).handle(),
						"branch-filter" => {
							m.branch_filter_input.read(cx).handle()
						}
						"log-row" => m.log_focus.clone(),
						"tree" => m.tree_focus.clone(),
						"reader" => m.reader_focus.clone(),
						_ => m.focus_handle.clone(),
					}
				});
				cx.update(|window, _| window.focus(&h));
				settle_checked(cx, model);
				assert!(
					cx.update(|window, _| h.is_focused(window)),
					"{origin} holds the keyboard (it is drawn)"
				);
			}

			/// The whole matrix: each opener against each origin. A failing
			/// cell is named, and the cells are counted so a control that
			/// stops being drawn cannot thin the matrix out.
			fn escape_matrix(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				openers: &[Opener],
			) {
				let mut failures = Vec::new();
				let mut cases = 0;
				for origin in ORIGINS {
					for opener in openers {
						// A find bar is open before the layer is: opening a
						// layer leaves an inline layer alone.
						match origin {
							"find-bar" => cx.simulate_keystrokes("ctrl-f"),
							"goto-bar" => cx.simulate_keystrokes("ctrl-g"),
							_ => {}
						}
						settle_checked(cx, model);
						opener.fire(model, cx);
						let open = floating_now(model, cx);
						assert!(
							open.iter().any(|k| k == opener.opens()),
							"{} opened {} (open: {open:?})",
							opener.label(),
							opener.opens()
						);
						focus_origin(model, cx, origin);
						let before = model.read_with(cx, |m, _| inline(m));
						cx.simulate_keystrokes("escape");
						settle_checked(cx, model);
						let (after, inl) = model
							.read_with(cx, |m, _| (floating(m), inline(m)));
						if !after.is_empty() {
							failures.push(format!(
								"{} from {origin}: still open {after:?}",
								opener.label()
							));
						}
						if inl != before {
							failures.push(format!(
								"{} from {origin}: inline layers {before:?} -> {inl:?}",
								opener.label()
							));
						}
						cases += 1;
						// Back to a bare state for the next cell.
						reset(model, cx);
					}
				}
				assert_eq!(cases, openers.len() * ORIGINS.len());
				assert!(
					failures.is_empty(),
					"Escape left layers open in {} of {cases} cases:\n{}",
					failures.len(),
					failures.join("\n")
				);
			}

			/// The key contexts the keymap names are all covered by an origin
			/// or are explained: a new context needs a decision here.
			#[test]
			fn no_plain_escape_binding_outside_the_root_rule() {
				use gpui::AsKeystroke;
				let offenders: Vec<String> = crate::key_bindings()
					.iter()
					.filter(|b| {
						b.keystrokes().first().is_some_and(|k| {
							let k = k.as_keystroke();
							k.key == "escape" && !k.modifiers.modified()
						})
					})
					.map(|b| {
						format!(
							"escape -> {:?} in {:?}",
							b.action().name(),
							b.predicate().map(|p| p.to_string())
						)
					})
					.collect();
				assert!(
					offenders.is_empty(),
					"a bare Escape belongs to the root rule \
					 (`WorkbenchModel::on_escape`), not to a pane or a field:\n{}",
					offenders.join("\n")
				);
			}

			/// Every key context the keymap names has a focus target, so the
			/// one Escape rule decides for it.
			#[test]
			fn every_keymap_context_has_a_focus_target() {
				use crate::overlay::FocusTarget;
				use gpui::KeyBindingContextPredicate as P;
				let mut named = std::collections::BTreeSet::new();
				for b in crate::key_bindings() {
					match b.predicate().as_deref() {
						None => {}
						Some(P::Identifier(name)) => {
							named.insert(name.to_string());
						}
						Some(other) => {
							panic!("a context this test cannot read: {other}")
						}
					}
				}
				let targets: std::collections::BTreeSet<String> =
					FocusTarget::KINDS
						.iter()
						.filter_map(|t| t.context())
						.map(String::from)
						.collect();
				assert_eq!(
					named, targets,
					"a key context needs a FocusTarget (and a decision in \
					 `WorkbenchModel::on_escape`)"
				);
			}

			/// Escape closes the floating layer whatever holds the keyboard
			/// (#212): the matrix over every opener and every origin.
			#[gpui::test]
			fn escape_closes_the_floating_layer_from_every_focus_target(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				let drawn = drawn_among(
					cx,
					&[
						"btn-workspace-menu",
						"btn-repo-selector",
						"btn-ref-selector",
						"btn-log-more",
						"log-filter-repo",
						"log-filter-branch",
						"log-filter-user",
						"log-filter-date",
						"log-filter-more",
					],
				);
				assert_eq!(
					drawn,
					[
						"btn-workspace-menu",
						"btn-repo-selector",
						"btn-ref-selector",
						"btn-log-more",
						"log-filter-repo",
						"log-filter-branch",
						"log-filter-user",
						"log-filter-date",
					],
					"the openers drawn at 1080w"
				);
				escape_matrix(&model, cx, &WIDE_OPENERS);
			}

			/// The same at the width where the filter chips fold into the
			/// overflow chip.
			#[gpui::test]
			fn escape_closes_the_overflow_layout_layers_from_every_focus_target(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 900., 600.);
				let drawn = drawn_among(
					cx,
					&[
						"log-filter-repo",
						"log-filter-branch",
						"log-filter-user",
						"log-filter-date",
						"log-filter-more",
					],
				);
				assert_eq!(
					drawn,
					["log-filter-repo", "log-filter-more"],
					"the chips drawn at 900w"
				);
				escape_matrix(
					&model,
					cx,
					&[
						Opener::Click("log-filter-repo", "log-menu:Repo"),
						Opener::Click("log-filter-more", "log-menu:FilterMore"),
						Opener::RightLogRow,
					],
				);
			}

			/// A context menu does not outlive a keyboard move to a pane
			/// (#212): Alt+1 leaves the menu drawn, Escape still closes it.
			#[gpui::test]
			fn escape_closes_a_menu_after_keyboard_focus_moves(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				let at = probed_center(&model, cx, "repo-row:alpha");
				right_press(cx, at);
				assert_eq!(
					floating_now(&model, cx),
					["context-menu"],
					"the row's menu is open"
				);
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert_eq!(
					floating_now(&model, cx),
					Vec::<String>::new(),
					"Escape after Alt+1 closes the menu (focus: {})",
					focus_name(&model, cx)
				);
			}

			/// Escape in another pane leaves the find bar open: it belongs to
			/// the reader, not to whatever pane the key landed in.
			#[gpui::test]
			fn escape_in_another_pane_keeps_the_find_bar(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				cx.simulate_keystrokes("ctrl-f");
				settle_checked(cx, &model);
				assert_eq!(model.read_with(cx, |m, _| inline(m)), ["find-bar"]);
				// Alt+9 hides a visible Log; the keyboard moves there the way a
				// click on a row leaves it.
				focus_origin(&model, cx, "log-row");
				assert_eq!(focus_name(&model, cx), "log");
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert_eq!(
					model.read_with(cx, |m, _| inline(m)),
					["find-bar"],
					"Escape in the Log leaves the find bar"
				);
				assert_eq!(focus_name(&model, cx), "reader");
				// With a selection, the first Escape only clears it.
				model.update(cx, |m, cx| m.select_all_text(cx));
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert_eq!(
					model.read_with(cx, |m, _| inline(m)),
					["find-bar"],
					"the first Escape clears the selection"
				);
				assert!(
					model.read_with(cx, |m, _| m.reader.selection().is_none())
				);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert!(
					model.read_with(cx, |m, _| inline(m).is_empty()),
					"the second Escape closes the find bar"
				);
			}

			/// A second press on the button that opened a layer closes it,
			/// fast or slow (#212): the chip's old 400 ms clock reopened it.
			#[gpui::test]
			fn a_second_press_on_a_trigger_closes_its_layer(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				let mut failures = Vec::new();
				let buttons: Vec<Opener> = WIDE_OPENERS
					.into_iter()
					.filter(|o| matches!(o, Opener::Click(..)))
					.collect();
				for o in &buttons {
					o.fire(&model, cx);
					assert_eq!(
						floating_now(&model, cx),
						[o.opens()],
						"{} opens its layer",
						o.label()
					);
					o.fire(&model, cx);
					let left = floating_now(&model, cx);
					if !left.is_empty() {
						failures.push(format!(
							"{}: a second press left {left:?}",
							o.label()
						));
						reset(&model, cx);
					}
				}
				// The slow press: down, hold, up. The mouse down closes the
				// layer; the click that follows must not reopen it.
				for o in buttons.iter().filter(
					|o| matches!(o, Opener::Click(id, _) if id.starts_with("log-filter-")),
				) {
					let Opener::Click(id, _) = o else {
						unreachable!()
					};
					o.fire(&model, cx);
					let at = cx.debug_bounds(id).unwrap().center();
					cx.simulate_mouse_down(
						at,
						MouseButton::Left,
						Modifiers::none(),
					);
					// Holding the button down is the point: not a wait.
					std::thread::sleep(Duration::from_millis(450));
					cx.simulate_mouse_up(
						at,
						MouseButton::Left,
						Modifiers::none(),
					);
					settle_checked(cx, &model);
					let left = floating_now(&model, cx);
					if !left.is_empty() {
						failures.push(format!(
							"{id}: a slow press (450 ms) left {left:?}"
						));
						reset(&model, cx);
					}
				}
				assert!(
					failures.is_empty(),
					"a second press must close the layer it opened:\n{}",
					failures.join("\n")
				);
			}

			/// The overflow chip's menu holds sub-menus: the chip that opened
			/// the first closes whatever it opened last.
			#[gpui::test]
			fn the_overflow_chip_closes_the_sub_menu_it_opened(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 900., 600.);
				let more =
					Opener::Click("log-filter-more", "log-menu:FilterMore");
				more.fire(&model, cx);
				let b = cx
					.debug_bounds("log-filter-more:user")
					.expect("the User filter is listed in the overflow menu");
				cx.simulate_click(b.center(), Modifiers::none());
				settle_checked(cx, &model);
				assert_eq!(floating_now(&model, cx), ["log-menu:User"]);
				more.fire(&model, cx);
				assert_eq!(
					floating_now(&model, cx),
					Vec::<String>::new(),
					"the overflow chip closes the sub-menu it opened"
				);
			}

			/// A layer opened by the keyboard closes the floating layer that
			/// was open; opening an inline layer does too, and an inline layer
			/// that was open stays.
			#[gpui::test]
			fn opening_a_layer_by_keyboard_closes_the_floating_one(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				let mut failures = Vec::new();
				let mut cases = 0;
				for opener in WIDE_OPENERS {
					for key in
						["alt-shift-r", "secondary-shift-`", "ctrl-f", "ctrl-g"]
					{
						opener.fire(&model, cx);
						cx.simulate_keystrokes(key);
						settle_checked(cx, &model);
						let (open, inl) = model
							.read_with(cx, |m, _| (floating(m), inline(m)));
						let want: Vec<String> = match (key, opener) {
							(
								"alt-shift-r",
								Opener::Click(_, "selector:Repo"),
							)
							| (
								"secondary-shift-`",
								Opener::Click(_, "selector:Ref"),
							) => Vec::new(),
							("alt-shift-r", _) => vec!["selector:Repo".into()],
							("secondary-shift-`", _) => {
								vec!["selector:Ref".into()]
							}
							_ => Vec::new(),
						};
						if open != want {
							failures.push(format!(
								"{} then {key}: floating {open:?}, want {want:?}",
								opener.label()
							));
						}
						if key.starts_with("ctrl") && inl != ["find-bar"] {
							failures.push(format!(
								"{} then {key}: inline {inl:?}",
								opener.label()
							));
						}
						cases += 1;
						reset(&model, cx);
					}
				}
				assert_eq!(cases, WIDE_OPENERS.len() * 4);
				assert!(
					failures.is_empty(),
					"{} of {cases} cases left two layers or the wrong one:\n{}",
					failures.len(),
					failures.join("\n")
				);
			}

			/// A background workspace tab shows no layer, and the host
			/// browse a layer owned ends with it; the focus the layer would
			/// have given back is where the tab resumes.
			#[gpui::test]
			fn a_tab_switch_closes_floating_layers_and_their_browse(
				cx: &mut TestAppContext,
			) {
				let _remote = remote_lock();
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(720.)));
				settle(cx);
				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				assert_eq!(active(&root, cx), Some(0));
				let tab0 = tab(&root, cx, 0);
				let worker = std::sync::Arc::new(snip_remote::Worker::new(
					snip_remote::WorkerOptions::default(),
				));
				tab0.update(cx, |m, cx| {
					m.remote.hosts =
						vec![snip_remote::RemoteHost::in_process(worker)];
					cx.notify();
				});
				for _ in 0..50 {
					settle(cx);
					if tab0.read_with(cx, |m, _| !m.commits.is_empty())
						&& cx.debug_bounds("log-filter-branch").is_some()
					{
						break;
					}
				}
				let mut failures = Vec::new();
				for case in
					["workspace-menu", "log-menu:Branch", "selector:Repo"]
				{
					match case {
						"workspace-menu" => {
							Opener::Click("btn-workspace-menu", case)
								.fire(&tab0, cx);
							Opener::Click("remote-host:0", case)
								.fire(&tab0, cx);
							wait_scaled(
								cx,
								"the host's folders are listed",
								|cx| {
									tab0.read_with(cx, |m, _| {
										m.remote.browse.as_ref().is_some_and(
											|b| b.listing.is_some(),
										)
									})
								},
							);
						}
						"log-menu:Branch" => {
							Opener::Click("log-filter-branch", case)
								.fire(&tab0, cx)
						}
						_ => {
							cx.simulate_keystrokes("alt-1");
							settle(cx);
							cx.simulate_keystrokes("alt-shift-r");
							settle(cx);
							assert_eq!(focus_name(&tab0, cx), "selector-field");
						}
					}
					assert_eq!(
						floating_now(&tab0, cx),
						[case],
						"{case} is open in the shown tab"
					);
					for (step, key, shown) in
						[("away", "cmd-2", 1), ("back", "cmd-1", 0)]
					{
						cx.simulate_keystrokes(key);
						settle(cx);
						assert_eq!(active(&root, cx), Some(shown));
						let (open, browse, hosts_ok) =
							tab0.read_with(cx, |m, _| {
								(
									floating(m),
									m.remote.browse.is_some(),
									// The menu open implies its browse is the host's.
									!m.overlays.is_open(crate::overlay::OverlayKind::WorkspaceMenu)
										|| m.remote
											.browse
											.as_ref()
											.is_none_or(|b| b.host == 0),
								)
							});
						if !open.is_empty() {
							failures.push(format!(
								"{case} {step}: still open {open:?}"
							));
						}
						if browse && !open.iter().any(|k| k == "workspace-menu")
						{
							failures.push(format!(
								"{case} {step}: a browse without its menu"
							));
						}
						if !hosts_ok {
							failures.push(format!(
								"{case} {step}: a menu over another host's browse"
							));
						}
					}
					if case == "workspace-menu" {
						// The menu is gone, so its browse must be too.
						let browse = tab0
							.read_with(cx, |m, _| m.remote.browse.is_some());
						if browse {
							failures
								.push("workspace-menu: a browse kept".into());
						}
					}
					if case == "selector:Repo" {
						let at = focus_name(&tab0, cx);
						if at != "tree" {
							failures.push(format!(
								"selector:Repo: focus resumes in {at}, not the tree it was opened from"
							));
						}
					}
					// Leave nothing open for the next case.
					reset(&tab0, cx);
				}
				assert!(
					failures.is_empty(),
					"a tab switch must close the floating layers:\n{}",
					failures.join("\n")
				);
			}

			/// The pointer rests in a corner no control is in.
			fn park_mouse(cx: &mut VisualTestContext) {
				cx.simulate_mouse_move(
					gpui::point(gpui::px(2.), gpui::px(2.)),
					None,
					Modifiers::none(),
				);
				settle(cx);
			}

			/// Tab 0 of two workspaces at 1080 wide with its Log loaded and a
			/// status line: the places a tooltip can rest while a layer is
			/// open. Both tooltips are up when no layer is, so what follows is
			/// the rule, not an accident of the fixture.
			fn tooltip_fixture(
				cx: &mut TestAppContext,
			) -> (
				tempfile::TempDir,
				Entity<WorkbenchModel>,
				String,
				String,
				gpui::Bounds<gpui::Pixels>,
				&mut VisualTestContext,
			) {
				let (tmp, a, _b, root, cx) = two_tabs(cx);
				cx.simulate_resize(gpui::size(gpui::px(1080.), gpui::px(720.)));
				cx.simulate_keystrokes("cmd-1");
				settle(cx);
				let tab0 = tab(&root, cx, 0);
				for _ in 0..50 {
					settle(cx);
					if tab0.read_with(cx, |m, _| !m.commits.is_empty())
						&& cx.debug_bounds("log-filter-branch").is_some()
					{
						break;
					}
				}
				// A status line gives `status-text` something to say.
				tab0.update(cx, |m, cx| {
					m.set_status("workspace_opening", [String::from("here")]);
					cx.notify();
				});
				settle(cx);
				let status =
					tab0.read_with(cx, |m, _| m.status.render(m.locale));
				assert!(!status.is_empty());
				let tab_tip = a.display().to_string();
				let status_b =
					cx.debug_bounds("status-text").expect("status drawn");
				hover_id(cx, "status-text");
				assert!(tooltip_shown(cx, &status), "the status tooltip is up");
				hover_id(cx, "ws-tab:0");
				assert!(tooltip_shown(cx, &tab_tip), "the tab tooltip is up");
				park_mouse(cx);
				(tmp, tab0, status, tab_tip, status_b, cx)
			}

			/// While a floating layer is open no tooltip shows, wherever the
			/// pointer rests: a tooltip would land on the layer or on the
			/// controls under it (#203 as a class, not one guard per button).
			#[gpui::test]
			fn no_tooltip_while_a_floating_layer_is_open(
				cx: &mut TestAppContext,
			) {
				use crate::overlay::OverlayKind as K;
				let (_tmp, tab0, status, tab_tip, status_b, cx) =
					tooltip_fixture(cx);
				let mut failures = Vec::new();
				for opener in [
					Opener::Click("btn-workspace-menu", "workspace-menu"),
					Opener::Click("btn-repo-selector", "selector:Repo"),
					Opener::Click("log-filter-branch", "log-menu:Branch"),
					Opener::RightLogRow,
				] {
					opener.fire(&tab0, cx);
					assert_eq!(floating_now(&tab0, cx), [opener.opens()]);
					let panel = cx
						.debug_bounds(match opener.opens() {
							"workspace-menu" => {
								K::WorkspaceMenu.panel_selector()
							}
							"selector:Repo" => {
								K::Selector(crate::Popover::Repo)
									.panel_selector()
							}
							"log-menu:Branch" => {
								K::LogMenu(crate::ui::LogMenu::Branch)
									.panel_selector()
							}
							_ => K::ContextMenu.panel_selector(),
						})
						.expect("the panel is drawn");
					// A panel over the status line would hide it from the
					// pointer by itself: that line is only tried where the
					// panel leaves it free.
					let status_free = !bounds_intersect(panel, status_b);
					for (id, text) in
						[("status-text", &status), ("ws-tab:0", &tab_tip)]
					{
						if id == "status-text" && !status_free {
							continue;
						}
						hover_id(cx, id);
						if tooltip_shown(cx, text) {
							failures.push(format!(
								"{}: the tooltip of {id} shows over the layer",
								opener.label()
							));
						}
					}
					reset(&tab0, cx);
					park_mouse(cx);
				}
				assert!(failures.is_empty(), "{}", failures.join("\n"));
			}

			/// A tooltip already up goes when a layer opens, and an inline
			/// layer takes none away.
			#[gpui::test]
			fn a_shown_tooltip_goes_when_a_floating_layer_opens(
				cx: &mut TestAppContext,
			) {
				let (_tmp, tab0, status, tab_tip, _status_b, cx) =
					tooltip_fixture(cx);
				hover_id(cx, "status-text");
				assert!(tooltip_shown(cx, &status), "the status tooltip is up");
				cx.simulate_keystrokes("alt-shift-r");
				settle_checked(cx, &tab0);
				assert!(
					!tooltip_shown(cx, &status),
					"opening a layer takes the tooltip that was up"
				);
				reset(&tab0, cx);
				park_mouse(cx);
				cx.simulate_keystrokes("ctrl-f");
				settle_checked(cx, &tab0);
				hover_id(cx, "ws-tab:0");
				assert!(
					tooltip_shown(cx, &tab_tip),
					"the find bar leaves the tooltips alone"
				);
			}

			/// A press on each opener, rested on first, leaves no tooltip of
			/// its own over the layer it opened (#203).
			#[gpui::test]
			fn opening_a_layer_drops_its_triggers_tooltip(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				let loc = model.read_with(cx, |m, _| m.locale);
				let branch = model.read_with(cx, |m, _| m.header_ref_label());
				let mut failures = Vec::new();
				for (id, text) in [
					(
						"btn-repo-selector",
						crate::i18n::t("tip_repo_selector", loc).to_string(),
					),
					(
						"btn-ref-selector",
						crate::i18n::tf("tip_ref_selector", loc, &[&branch]),
					),
					(
						"btn-log-more",
						crate::i18n::t("tip_log_more", loc).to_string(),
					),
				] {
					let at =
						cx.debug_bounds(id).expect("opener drawn").center();
					cx.simulate_mouse_move(at, None, Modifiers::none());
					cx.simulate_click(at, Modifiers::none());
					cx.executor().advance_clock(Duration::from_millis(700));
					settle_checked(cx, &model);
					if cx.debug_bounds(sel(format!("tooltip:{text}"))).is_some()
					{
						failures.push(format!("{id}: its tooltip stays up"));
					}
					reset(&model, cx);
					park_mouse(cx);
				}
				assert!(failures.is_empty(), "{}", failures.join("\n"));
			}

			/// The add-repo field is an inline layer of the Project pane: its
			/// button toggles it, a floating layer opens over it without
			/// closing it, and Escape in its field closes it and gives the
			/// keyboard back to where the button left it.
			#[gpui::test]
			fn the_add_repo_field_is_an_inline_layer(cx: &mut TestAppContext) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				let button = Opener::Click("btn-add-repo", "add-repo");
				button.fire(&model, cx);
				assert_eq!(model.read_with(cx, |m, _| inline(m)), ["add-repo"]);
				assert!(cx.debug_bounds("input-add-repo").is_some());
				let at_open = focus_name(&model, cx);
				// A second press on the button closes it.
				button.fire(&model, cx);
				assert!(model.read_with(cx, |m, _| inline(m).is_empty()));
				button.fire(&model, cx);
				// A floating layer opens over it and closes first.
				Opener::Click("btn-ref-selector", "selector:Ref")
					.fire(&model, cx);
				assert_eq!(floating_now(&model, cx), ["selector:Ref"]);
				assert_eq!(model.read_with(cx, |m, _| inline(m)), ["add-repo"]);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert_eq!(model.read_with(cx, |m, _| inline(m)), ["add-repo"]);
				// Escape in its field closes it.
				let field = model
					.read_with(cx, |m, cx| m.add_repo_input.read(cx).handle());
				cx.update(|window, _| window.focus(&field));
				settle_checked(cx, &model);
				assert_eq!(focus_name(&model, cx), "other");
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert!(model.read_with(cx, |m, _| inline(m).is_empty()));
				assert_eq!(
					focus_name(&model, cx),
					at_open,
					"the keyboard is back where the button left it"
				);
			}

			/// One rule for the keyboard after a layer closes (the user's
			/// decision): back to where it was opened from, or the root.
			#[gpui::test]
			fn closing_a_layer_returns_focus_by_one_rule(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				let mut failures = Vec::new();
				let mut check = |row: &str,
				                 got: &'static str,
				                 want: &'static str| {
					if got != want {
						failures
							.push(format!("{row}: focus {got}, want {want}"));
					}
				};
				// 1: a selector opened from the project tree.
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				assert_eq!(focus_name(&model, cx), "tree");
				cx.simulate_keystrokes("alt-shift-r");
				settle_checked(cx, &model);
				assert_eq!(floating_now(&model, cx), ["selector:Repo"]);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				check("1 selector, Escape", focus_name(&model, cx), "tree");
				assert!(floating_now(&model, cx).is_empty());

				// 2: a Log chip, closed by a press on bare header space.
				Opener::Click("log-filter-repo", "log-menu:Repo")
					.fire(&model, cx);
				assert_eq!(floating_now(&model, cx), ["log-menu:Repo"]);
				let ref_b = cx.debug_bounds("btn-ref-selector").unwrap();
				let paste = probed_center(&model, cx, "btn-paste");
				let left = f32::from(ref_b.origin.x + ref_b.size.width);
				let blank = gpui::point(gpui::px(left + 40.), ref_b.center().y);
				assert!(
					f32::from(paste.x) - (left + 40.) > 40.,
					"bare header space: {left}..{:?}",
					paste.x
				);
				cx.simulate_click(blank, Modifiers::none());
				settle_checked(cx, &model);
				assert!(floating_now(&model, cx).is_empty());
				check(
					"2 Log chip, outside press",
					focus_name(&model, cx),
					"log",
				);

				// 3: a context menu opened on a tree row.
				Opener::RightRepoRow.fire(&model, cx);
				assert_eq!(floating_now(&model, cx), ["context-menu"]);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				check("3 context menu, Escape", focus_name(&model, cx), "tree");

				// 4: the find bar.
				cx.simulate_keystrokes("ctrl-f");
				settle_checked(cx, &model);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				check("4 find bar, Escape", focus_name(&model, cx), "reader");

				// 5: the workspace menu's path field.
				Opener::Click("btn-workspace-menu", "workspace-menu")
					.fire(&model, cx);
				let at = probed_center(&model, cx, "btn-open-workspace");
				cx.simulate_click(at, Modifiers::none());
				settle_checked(cx, &model);
				assert_eq!(focus_name(&model, cx), "workspace-field");
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert!(floating_now(&model, cx).is_empty());
				check(
					"5 workspace field, Escape",
					focus_name(&model, cx),
					"root",
				);

				// 6: a press on another pane keeps what that pane took.
				Opener::Click("btn-ref-selector", "selector:Ref")
					.fire(&model, cx);
				let at = probed_center(&model, cx, "repo-row:alpha");
				cx.simulate_click(at, Modifiers::none());
				settle_checked(cx, &model);
				assert!(floating_now(&model, cx).is_empty());
				check(
					"6 outside press on the tree",
					focus_name(&model, cx),
					"tree",
				);

				// 7: a layer opened over another's keyboard keeps its own
				// place to give it back: the find bar's is the reader, not
				// the pane the selector under it was opened from.
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				cx.simulate_keystrokes("alt-shift-r");
				settle_checked(cx, &model);
				assert_eq!(floating_now(&model, cx), ["selector:Repo"]);
				cx.simulate_keystrokes("ctrl-f");
				settle_checked(cx, &model);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				check(
					"7 find bar over a selector, Escape",
					focus_name(&model, cx),
					"reader",
				);
				// 8: a Log dropdown that takes the keyboard (its filter field)
				// opened after a selector gives it back to the Log.
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				cx.simulate_keystrokes("alt-shift-r");
				settle_checked(cx, &model);
				Opener::Click("log-filter-branch", "log-menu:Branch")
					.fire(&model, cx);
				assert_eq!(floating_now(&model, cx), ["log-menu:Branch"]);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				check(
					"8 Log dropdown over a selector, Escape",
					focus_name(&model, cx),
					"log",
				);
				assert!(
					failures.is_empty(),
					"the focus after a layer closes:\n{}",
					failures.join("\n")
				);
			}

			/// Ctrl+F / Ctrl+G with the find bar already open still close the
			/// floating layer that was opened over it: the keyboard goes to
			/// the bar, so no layer may stay drawn (and mute the tooltips)
			/// behind it.
			#[gpui::test]
			fn reopening_an_open_inline_layer_closes_the_floating_one(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				let mut failures = Vec::new();
				let mut cases = 0;
				for opener in WIDE_OPENERS {
					for (first, key) in [
						("ctrl-f", "ctrl-f"),
						("ctrl-f", "ctrl-g"),
						("ctrl-g", "ctrl-f"),
						("ctrl-g", "ctrl-g"),
					] {
						cx.simulate_keystrokes(first);
						settle_checked(cx, &model);
						opener.fire(&model, cx);
						assert_eq!(
							floating_now(&model, cx),
							[opener.opens()],
							"{} opened over the find bar",
							opener.label()
						);
						cx.simulate_keystrokes(key);
						settle_checked(cx, &model);
						let (open, inl) = model
							.read_with(cx, |m, _| (floating(m), inline(m)));
						let at = focus_name(&model, cx);
						let label = opener.label();
						if !open.is_empty() {
							failures.push(format!(
								"{label}, {first} then {key}: floating {open:?}"
							));
						}
						if inl != ["find-bar"] || at != "find-field" {
							failures.push(format!(
								"{label}, {first} then {key}: inline {inl:?}, focus {at}"
							));
						}
						cases += 1;
						reset(&model, cx);
					}
				}
				assert_eq!(cases, WIDE_OPENERS.len() * 4);
				assert!(
					failures.is_empty(),
					"{} of {cases} cases left a floating layer over the find bar:\n{}",
					failures.len(),
					failures.join("\n")
				);
			}

			/// Where a trigger was drawn is forgotten when a frame does not
			/// draw it: the chip a narrower window folds into "more" must not
			/// keep its old place, or a press there would pass for a press on
			/// it and the layer would survive an outside press.
			#[gpui::test]
			fn a_trigger_that_is_not_drawn_has_no_place(
				cx: &mut TestAppContext,
			) {
				use crate::{overlay::TriggerId, ui::LogMenu};
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				Opener::Click("log-filter-branch", "log-menu:Branch")
					.fire(&model, cx);
				let slot = model.read_with(cx, |m, _| {
					m.overlays.trigger_slot(TriggerId::LogChip(LogMenu::Branch))
				});
				let at =
					slot.get().expect("the chip is drawn at 1080w").center();
				cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(600.)));
				settle(cx);
				assert!(
					cx.debug_bounds("log-filter-branch").is_none(),
					"the chip is folded away at 900w"
				);
				assert_eq!(
					slot.get(),
					None,
					"the folded chip keeps no place from the wide frame"
				);
				assert_eq!(
					floating_now(&model, cx),
					["log-menu:Branch"],
					"the menu survives the resize, so the press below tests it"
				);
				cx.simulate_click(at, Modifiers::none());
				settle_checked(cx, &model);
				assert!(
					floating_now(&model, cx).is_empty(),
					"a press where the chip used to be is an outside press"
				);
			}

			/// A layer closed by a focusable control inside its own panel
			/// still gives the keyboard back: the press put the focus on
			/// the control, which is the layer's, not "somewhere else".
			#[gpui::test]
			fn the_find_bar_close_button_returns_to_the_reader(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				cx.simulate_keystrokes("ctrl-f");
				settle_checked(cx, &model);
				assert_eq!(model.read_with(cx, |m, _| inline(m)), ["find-bar"]);
				let at = probed_center(&model, cx, "btn-find-close");
				cx.simulate_click(at, Modifiers::none());
				settle_checked(cx, &model);
				assert!(model.read_with(cx, |m, _| inline(m).is_empty()));
				assert_eq!(
					focus_name(&model, cx),
					"reader",
					"the find bar's close button hands the keyboard to the reader"
				);
			}

			#[gpui::test]
			fn the_log_date_apply_button_returns_to_the_log(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				Opener::Click("log-filter-date", "log-menu:Date")
					.fire(&model, cx);
				model.update(cx, |m, cx| {
					m.log_since_input
						.update(cx, |i, cx| i.set_text("2020-01-01", cx));
					m.log_until_input
						.update(cx, |i, cx| i.set_text("2030-01-01", cx));
				});
				settle_checked(cx, &model);
				let at = probed_center(&model, cx, "log-date-apply");
				cx.simulate_click(at, Modifiers::none());
				settle_checked(cx, &model);
				assert!(
					floating_now(&model, cx).is_empty(),
					"a valid range closes the menu"
				);
				assert_eq!(
					focus_name(&model, cx),
					"log",
					"the Log keeps the keyboard after Apply"
				);
			}

			/// The panel tracks the keyboard for its controls, but a press on
			/// its blank part does not take it from the field typed in.
			#[gpui::test]
			fn a_press_on_a_panels_blank_part_keeps_the_field_focused(
				cx: &mut TestAppContext,
			) {
				use crate::{overlay::OverlayKind, ui::LogMenu};
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				Opener::Click("log-filter-date", "log-menu:Date")
					.fire(&model, cx);
				let since = model
					.read_with(cx, |m, cx| m.log_since_input.read(cx).handle());
				cx.update(|window, _| window.focus(&since));
				settle_checked(cx, &model);
				let panel = cx
					.debug_bounds(
						OverlayKind::LogMenu(LogMenu::Date).panel_selector(),
					)
					.expect("the date menu is drawn");
				let at = panel.origin + gpui::point(gpui::px(2.), gpui::px(2.));
				cx.simulate_click(at, Modifiers::none());
				settle_checked(cx, &model);
				assert_eq!(floating_now(&model, cx), ["log-menu:Date"]);
				assert!(
					cx.update(|window, _| since.is_focused(window)),
					"the field keeps the keyboard"
				);
			}

			/// A press on a find-bar control (not a field) leaves the bar
			/// under the one Escape rule: Escape closes it.
			#[gpui::test]
			fn escape_after_a_find_bar_control_closes_the_find_bar(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				for id in ["btn-find-regex", "btn-find-case"] {
					cx.simulate_keystrokes("ctrl-f");
					settle_checked(cx, &model);
					let at = probed_center(&model, cx, id);
					cx.simulate_click(at, Modifiers::none());
					settle_checked(cx, &model);
					cx.simulate_keystrokes("escape");
					settle_checked(cx, &model);
					assert!(
						model.read_with(cx, |m, _| inline(m).is_empty()),
						"Escape after a press on {id} closes the find bar"
					);
					reset(&model, cx);
				}
			}

			/// The tooltip gate invariant (O6) is the shown tab's: a tab that
			/// went to the background with a layer open drew its last frame
			/// with the gate closed, and that is not a mismatch.
			#[gpui::test]
			fn a_background_tab_holds_every_overlay_invariant(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let shown = tab(&root, cx, 1);
				let at = cx
					.debug_bounds("btn-repo-selector")
					.expect("the shown tab draws its selector button")
					.center();
				cx.simulate_click(at, Modifiers::none());
				settle(cx);
				assert_eq!(floating_now(&shown, cx), ["selector:Repo"]);
				click_id(cx, "ws-tab:0".into());
				assert_eq!(active(&root, cx), Some(0));
				shown.read_with(cx, |m, cx| {
					assert!(m.overlays.floating().is_none());
					let broken =
						crate::overlay::OverlayStack::check_invariants(m, cx);
					assert!(
						broken.is_empty(),
						"a background tab breaks {broken:?}"
					);
				});
			}

			/// A selector opened by a press on its header button gives the
			/// keyboard back to that button: the button took the focus the
			/// press, so it is where the keyboard was when the layer opened.
			/// (The shortcut gives it back to the pane it was pressed in.)
			#[gpui::test]
			fn a_selector_opened_by_a_click_returns_to_its_button(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				cx.simulate_keystrokes("alt-1");
				settle_checked(cx, &model);
				assert_eq!(focus_name(&model, cx), "tree");
				Opener::Click("btn-repo-selector", "selector:Repo")
					.fire(&model, cx);
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert!(floating_now(&model, cx).is_empty());
				assert_eq!(
					focus_name(&model, cx),
					"other",
					"back on the button that opened it, not the tree"
				);
			}

			/// The folder dialog's failure lands after the tab went to the
			/// background: the typed-path menu opens nothing there, and no
			/// field is asked to take the keyboard when the tab is shown.
			#[gpui::test]
			fn the_workspace_picker_asks_for_no_focus_in_a_background_tab(
				cx: &mut TestAppContext,
			) {
				let (_tmp, _a, _b, root, cx) = two_tabs(cx);
				let background = tab(&root, cx, 0);
				assert_eq!(active(&root, cx), Some(1));
				background.update(cx, |m, cx| {
					m.pending_focus = None;
					m.show_workspace_picker(cx);
				});
				background.read_with(cx, |m, _| {
					assert!(
						!m.overlays.is_open(
							crate::overlay::OverlayKind::WorkspaceMenu
						),
						"a background tab opens no menu"
					);
					assert!(
						m.pending_focus.is_none(),
						"and queues no focus for a field that is not drawn"
					);
				});
			}

			/// An IME composition owns Escape: the first one ends the
			/// composition, the next closes the layer.
			#[gpui::test]
			fn escape_during_ime_composition_goes_to_the_ime(
				cx: &mut TestAppContext,
			) {
				let (_ws, model, cx) = fixture(cx, 1080., 720.);
				cx.simulate_keystrokes("alt-shift-r");
				settle_checked(cx, &model);
				assert_eq!(floating_now(&model, cx), ["selector:Repo"]);
				let input =
					model.read_with(cx, |m, _| m.selector_input.clone());
				input.update(cx, |i, _| {
					i.handle_replace_and_mark(None, "\u{3110}\u{3127}", None)
				});
				assert!(input.read_with(cx, |i, _| i.marked_range().is_some()));
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert_eq!(
					floating_now(&model, cx),
					["selector:Repo"],
					"Escape during a composition is the IME's"
				);
				assert!(
					input.read_with(cx, |i, _| i.marked_range().is_some()),
					"the composition is untouched"
				);
				input.update(cx, |i, _| {
					i.handle_unmark();
				});
				cx.simulate_keystrokes("escape");
				settle_checked(cx, &model);
				assert_eq!(
					floating_now(&model, cx),
					Vec::<String>::new(),
					"Escape with no composition closes the selector"
				);
			}
		}

		/// The paste panel's destination folder (#238): picked with the
		/// pointer or typed, then applied from the keyboard.
		mod paste_destination {
			use super::*;

			/// `ws/` is a plain folder holding two repos, `docs/` with a
			/// file, and a folder whose name has a space. Also returns the
			/// temp dir's owner so a test can add siblings.
			fn multi_repo_ws() -> (tempfile::TempDir, PathBuf) {
				let tmp = tempfile::tempdir().unwrap();
				let ws = tmp.path().join("ws");
				fs::create_dir(&ws).unwrap();
				repo(&ws, "repoA", &[]);
				repo(&ws, "repoB", &[]);
				fs::create_dir(ws.join("docs")).unwrap();
				fs::write(ws.join("docs/readme.txt"), "readme").unwrap();
				fs::create_dir(ws.join("my dir")).unwrap();
				(tmp, ws)
			}

			/// Opens `ws` and waits (bounded) for the scan and its trees.
			fn open_ready<'a>(
				cx: &'a mut TestAppContext,
				ws: &Path,
			) -> (Entity<WorkbenchModel>, &'a mut VisualTestContext) {
				let (model, cx) = open(cx, ws.to_path_buf(), None);
				settle_until(cx, "the workspace scan", |cx| {
					model.read_with(cx, |m, _| {
						!m.is_loading
							&& m.repos.len() == 2 && m
							.ws_tree
							.as_ref()
							.is_some_and(|t| t.is_loaded)
					})
				});
				// The Project tool window, where the folders are.
				cx.simulate_keystrokes("alt-1");
				settle(cx);
				(model, cx)
			}

			/// Puts `text` on the clipboard and opens its paste preview
			/// with Cmd+V, waiting (bounded) for the plan.
			fn preview(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				text: &str,
			) {
				clip::write_text(text).unwrap();
				cx.simulate_keystrokes("cmd-v");
				landed(model, cx);
			}

			fn landed(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) {
				settle_until(cx, "the paste plan", |cx| {
					model.read_with(cx, |m, _| {
						m.paste.plan().is_some() && !m.paste.is_loading()
					})
				});
			}

			/// The Enter that applies, then the wait for the write.
			fn apply_with_enter(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) {
				cx.simulate_keystrokes("enter");
				settle_until(cx, "the write to finish", |cx| {
					model.read_with(cx, |m, _| {
						m.paste.plan().is_none() && !m.paste_busy()
					})
				});
			}

			/// Where the probe `id` is drawn, centred, in window points.
			fn probed_point(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				id: &str,
			) -> gpui::Point<gpui::Pixels> {
				if model.read_with(cx, |m, _| m.probes.is_none()) {
					model.update(cx, |m, cx| {
						m.probes = Some(crate::ui::Probes::for_test());
						cx.notify();
					});
					settle(cx);
				}
				let b = model
					.read_with(cx, |m, _| m.probes.as_ref().unwrap().bounds(id))
					.unwrap_or_else(|| panic!("no probed control {id}"));
				let scale = cx.update(|window, _| window.scale_factor());
				gpui::point(
					gpui::px((b[0] + b[2] / 2) as f32 / scale),
					gpui::px((b[1] + b[3] / 2) as f32 / scale),
				)
			}

			fn right_click_probed(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				id: &str,
			) {
				let pos = probed_point(model, cx, id);
				cx.simulate_mouse_down(
					pos,
					gpui::MouseButton::Right,
					gpui::Modifiers::none(),
				);
				cx.simulate_mouse_up(
					pos,
					gpui::MouseButton::Right,
					gpui::Modifiers::none(),
				);
				settle(cx);
			}

			fn plan_destination(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> PathBuf {
				model.read_with(cx, |m, _| {
					m.paste.plan().expect("a plan").destination.clone()
				})
			}

			fn executable(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> bool {
				model.read_with(cx, |m, _| {
					m.paste.plan().is_some_and(|p| p.executable())
				})
			}

			fn paste_focused(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> bool {
				cx.update(|w, cx| model.read(cx).paste_focus.is_focused(w))
			}

			fn probed(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
				id: &str,
			) -> bool {
				model.read_with(cx, |m, _| {
					m.probes.as_ref().is_some_and(|p| p.bounds(id).is_some())
				})
			}

			fn file(path: &Path) -> Option<String> {
				// The payload's closing line break is not part of the file.
				fs::read_to_string(path)
					.ok()
					.map(|text| text.trim_end().to_string())
			}

			/// The report of #236: in a workspace of several repos there was
			/// no way to paste into the workspace folder itself.
			#[gpui::test]
			fn multi_repo_paste_targets_the_workspace_root_from_the_breadcrumb(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				// The default stays the pinned repo, whichever scans first.
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				assert_ne!(first, ws);
				assert_eq!(plan_destination(&model, cx), first);
				assert_eq!(folder(&model, cx).0, first);

				click_probed(&model, cx, "paste-dest-crumb:0");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws);
				assert_eq!(
					folder(&model, cx),
					(ws.clone(), Some(String::new()))
				);
				assert!(executable(&model, cx));
				assert!(
					paste_focused(&model, cx),
					"focus returns to the panel"
				);

				apply_with_enter(&model, cx);
				assert!(
					file(&ws.join("x.txt")).is_some_and(|s| s.contains('X'))
				);
				assert!(!ws.join("repoA/x.txt").exists());
				assert!(!ws.join("repoB/x.txt").exists());
			}

			/// Keeping a prefix follows the destination only until the
			/// destination moves: the choice is asked for again (#238).
			#[gpui::test]
			fn moving_the_destination_asks_for_the_prefix_again(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: notes/x.txt\nN\n");
				click_probed(&model, cx, "paste-map-keep:notes");
				landed(&model, cx);
				assert!(model.read_with(cx, |m, _| {
					m.paste.plan().unwrap().mapping_ready()
				}));
				click_probed(&model, cx, "paste-dest-root");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws);
				assert!(!model.read_with(cx, |m, _| {
					m.paste.plan().unwrap().mapping_ready()
				}));
				click_probed(&model, cx, "paste-map-keep:notes");
				landed(&model, cx);
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("notes/x.txt")).as_deref(), Some("N"));
			}

			/// Right-click a folder, pick "paste into this folder": the next
			/// Enter applies (the menu's own Enter has been spent).
			#[gpui::test]
			fn paste_into_from_the_project_menu_applies_on_the_first_enter(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();

				// (a) The pointer picks it, with no preview open.
				{
					let (model, cx) = open_ready(cx, &ws);
					clip::write_text("// FILE: x.txt\nX\n").unwrap();
					right_click_probed(&model, cx, "ws-tree-row:docs");
					assert!(model.read_with(cx, |m, _| menu_has_enabled(
						m,
						"paste-into"
					)));
					click_probed(&model, cx, "menu-item:paste-into");
					landed(&model, cx);
					assert_eq!(plan_destination(&model, cx), ws.join("docs"));
					assert!(paste_focused(&model, cx));
					assert!(model.read_with(cx, |m, _| m
						.overlays
						.context_menu()
						.is_none()));
					apply_with_enter(&model, cx);
					assert_eq!(
						file(&ws.join("docs/x.txt")).as_deref(),
						Some("X")
					);
					fs::remove_file(ws.join("docs/x.txt")).unwrap();
				}
			}

			/// Whether the open menu has an enabled item `id`.
			fn menu_has_enabled(m: &WorkbenchModel, id: &str) -> bool {
				m.overlays.context_menu().is_some_and(|menu| {
					menu.entries.iter().any(|e| {
						matches!(
							e,
							crate::menu::MenuEntry::Item {
								id: i,
								act: Some(_),
								..
							} if *i == id
						)
					})
				})
			}

			/// The same, with the menu driven from the keyboard.
			#[gpui::test]
			fn paste_into_from_the_menu_by_keyboard_applies_on_the_next_enter(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				clip::write_text("// FILE: x.txt\nX\n").unwrap();
				right_click_probed(&model, cx, "ws-tree-row:docs");
				for _ in 0..8 {
					let on_it = model.read_with(cx, |m, _| {
						m.overlays.context_menu().is_some_and(|menu| {
							matches!(
								menu.cursor.and_then(|c| menu.entries.get(c)),
								Some(crate::menu::MenuEntry::Item {
									id: "paste-into",
									..
								})
							)
						})
					});
					if on_it {
						break;
					}
					cx.simulate_keystrokes("down");
				}
				// The menu's Enter picks the item and applies nothing.
				cx.simulate_keystrokes("enter");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("docs"));
				assert!(!ws.join("docs/x.txt").exists());
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("docs/x.txt")).as_deref(), Some("X"));
			}

			/// With a preview already open the menu moves it, and the
			/// payload is the one previewed, not the clipboard now.
			#[gpui::test]
			fn paste_into_with_a_preview_open_keeps_the_previewed_payload(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				clip::write_text("other").unwrap();
				right_click_probed(&model, cx, "ws-tree-row:docs");
				click_probed(&model, cx, "menu-item:paste-into");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("docs"));
				assert!(model.read_with(cx, |m, _| {
					m.paste
						.plan()
						.unwrap()
						.items
						.iter()
						.any(|i| i.path == "x.txt")
				}));
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("docs/x.txt")).as_deref(), Some("X"));
			}

			/// The target of the item `paste-into` in `entries`, if it is
			/// there and enabled.
			fn paste_into_act(
				entries: Vec<crate::menu::MenuEntry>,
			) -> Option<PathBuf> {
				entries.into_iter().find_map(|e| match e {
					crate::menu::MenuEntry::Item {
						id: "paste-into",
						act: Some(crate::menu::MenuAct::PasteInto(path)),
						..
					} => Some(path),
					_ => None,
				})
			}

			fn has_paste_into(entries: &[crate::menu::MenuEntry]) -> bool {
				entries.iter().any(|e| {
					matches!(
						e,
						crate::menu::MenuEntry::Item {
							id: "paste-into",
							..
						}
					)
				})
			}

			/// Folder rows and repo rows offer "paste into this folder", with
			/// the folder as the target; file rows do not.
			#[gpui::test]
			fn project_menus_offer_paste_into_for_folders_only(
				cx: &mut TestAppContext,
			) {
				let (_tmp, ws) = multi_repo_ws();
				fs::write(ws.join("top.txt"), "T").unwrap();
				for repo in ["repoA", "repoB"] {
					fs::create_dir(ws.join(repo).join("dir")).unwrap();
					fs::write(ws.join(repo).join("dir/b.txt"), "B").unwrap();
				}
				let (model, cx) = open_ready(cx, &ws);
				open_repo_tree(&model, cx, 0);
				model.read_with(cx, |m, _| {
					// A workspace folder row and its file.
					let tree = m.ws_tree.as_ref().unwrap();
					let rows = tree.flatten_visible(tree.visible_limit());
					let row = |rel: &str| {
						rows.iter().find(|r| r.rel_path == rel).cloned()
					};
					let docs = row("docs").expect("docs row");
					assert_eq!(
						paste_into_act(m.work_row_menu(&docs, true)),
						Some(m.ws_root().unwrap().join("docs"))
					);
					let top = row("top.txt").expect("top.txt row");
					assert!(!top.is_dir);
					assert!(!has_paste_into(&m.work_row_menu(&top, true)));
					// A folder inside the open repo.
					let tree = m.file_tree.as_ref().unwrap();
					let rows = tree.flatten_visible(tree.visible_limit());
					let find = |rel: &str| {
						rows.iter().find(|r| r.rel_path == rel).unwrap().clone()
					};
					let dir = find("dir");
					assert!(dir.is_dir);
					assert_eq!(
						paste_into_act(m.work_row_menu(&dir, false)),
						Some(m.repo_root().unwrap().join("dir"))
					);
					let base = find("base.txt");
					assert!(!has_paste_into(&m.work_row_menu(&base, false)));
					// Every repo row, first in its menu.
					for i in 0..m.repos.len() {
						let entries = m.repo_row_menu(i);
						assert!(matches!(
							entries.first(),
							Some(crate::menu::MenuEntry::Item {
								id: "paste-into",
								..
							})
						));
						assert_eq!(
							paste_into_act(entries),
							Some(m.repos[i].root.clone())
						);
					}
				});
			}

			/// In a remote workspace the target is a tree path under the
			/// session root, which is what the worker is asked about.
			#[cfg(unix)]
			#[gpui::test]
			fn remote_project_menus_paste_into_a_tree_path_under_the_session_root(
				cx: &mut TestAppContext,
			) {
				let _serial = remote_lock();
				let (_tmp, model, cx, _worker) = open_remote_to_lose(cx);
				model.read_with(cx, |m, _| {
					let root = m.remote.session.as_ref().unwrap().root.clone();
					let tree = m.ws_tree.as_ref().unwrap();
					let rows = tree.flatten_visible(tree.visible_limit());
					let notes = rows
						.iter()
						.find(|r| r.rel_path == "notes")
						.expect("notes row");
					assert_eq!(
						paste_into_act(m.work_row_menu(notes, true)),
						Some(root.join("notes"))
					);
					let repo = paste_into_act(m.repo_row_menu(0))
						.expect("the repo row offers it");
					assert!(
						repo.starts_with(&root) && repo != root,
						"{repo:?}"
					);
				});
			}

			/// At the narrowest window the bar wraps instead of overlapping:
			/// its controls and Apply and Cancel stay apart and inside the
			/// panel.
			#[gpui::test]
			fn paste_dest_bar_fits_at_900w(cx: &mut TestAppContext) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_sized(cx, ws.clone(), None, 900., 700.);
				settle_until(cx, "the workspace scan", |cx| {
					model.read_with(cx, |m, _| {
						!m.is_loading && m.repos.len() == 2
					})
				});
				preview(&model, cx, "// FILE: x.txt\nX\n");
				// Probes switch on with the first read and draw next frame.
				let _ = probed_point(&model, cx, "btn-apply");
				settle(cx);
				let ids = [
					"paste-dest-crumb:0",
					"paste-dest-crumb:1",
					"paste-dest-root",
					"paste-dest-edit",
					"paste-dest-browse",
					"btn-apply",
					"btn-cancel",
				];
				let bounds = |cx: &mut VisualTestContext, id: &str| {
					model
						.read_with(cx, |m, _| {
							m.probes.as_ref().unwrap().bounds(id)
						})
						.unwrap_or_else(|| panic!("{id} is not drawn"))
				};
				let panel = bounds(cx, "paste-panel");
				let inside = |b: [i32; 4]| {
					b[0] >= panel[0]
						&& b[1] >= panel[1]
						&& b[0] + b[2] <= panel[0] + panel[2]
						&& b[1] + b[3] <= panel[1] + panel[3]
				};
				let all: Vec<(&str, [i32; 4])> =
					ids.iter().map(|id| (*id, bounds(cx, id))).collect();
				for (id, b) in &all {
					assert!(
						inside(*b),
						"{id} {b:?} leaves the panel {panel:?}"
					);
				}
				for (i, (a_id, a)) in all.iter().enumerate() {
					for (b_id, b) in &all[i + 1..] {
						let apart = a[0] + a[2] <= b[0]
							|| b[0] + b[2] <= a[0]
							|| a[1] + a[3] <= b[1]
							|| b[1] + b[3] <= a[1];
						assert!(apart, "{a_id} {a:?} overlaps {b_id} {b:?}");
					}
				}
			}

			/// The Project selection offers its folder as a place to paste:
			/// a file names its folder, and nothing is offered for several
			/// rows (one cannot be guessed).
			#[gpui::test]
			fn the_left_chip_names_the_folder_of_the_single_selected_row(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				click_probed(&model, cx, "ws-tree-row:docs");
				settle_until(cx, "docs to open", |cx| {
					model.read_with(cx, |m, _| {
						m.probes.as_ref().is_some_and(|p| {
							p.bounds("ws-tree-row:docs/readme.txt").is_some()
						})
					})
				});
				click_probed(&model, cx, "ws-tree-row:docs/readme.txt");
				preview(&model, cx, "// FILE: x.txt\nX\n");
				assert!(probed(&model, cx, "paste-dest-left"));
				click_probed(&model, cx, "paste-dest-left");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("docs"));
				assert_eq!(folder(&model, cx).1.as_deref(), Some("docs"));
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("docs/x.txt")).as_deref(), Some("X"));

				// Two rows selected: no chip.
				model.update(cx, |m, cx| {
					m.select_tree_rows_alone(
						true,
						&["docs".into(), "my dir".into()],
						cx,
					)
				});
				settle(cx);
				preview(&model, cx, "// FILE: y.txt\nY\n");
				assert!(!probed(&model, cx, "paste-dest-left"));
			}

			/// The engine suggests dropping `sub/` for a folder named `sub`,
			/// then refuses the plan as it stands: a regular file `sub` is
			/// where the directory `sub/` would go. The refusal keeps the
			/// suggestion, so [Adjust paths] on the panel recovers the paste.
			#[gpui::test]
			fn a_refused_new_destination_still_offers_the_adjustment(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				fs::create_dir(ws.join("sub")).unwrap();
				fs::write(ws.join("sub/sub"), "a file named like its folder")
					.unwrap();
				let (model, cx) = open_ready(cx, &ws);
				preview(
					&model,
					cx,
					"// clipcode-root: mono\n// FILE: sub/a.ts\nx\n",
				);
				assert!(executable(&model, cx), "valid where it was opened");

				click_probed(&model, cx, "ws-tree-row:sub");
				click_probed(&model, cx, "paste-dest-left");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("sub"));
				model.read_with(cx, |m, _| {
					let plan = m.paste.plan().expect("a plan");
					assert!(!plan.executable());
					assert!(plan.error.is_some(), "the engine refused it");
				});
				assert!(
					cx.debug_bounds("paste-restore-base").is_some(),
					"the refusal lost the way back: no [Adjust paths]"
				);

				click_probed(&model, cx, "paste-adjust");
				landed(&model, cx);
				model.read_with(cx, |m, _| {
					let plan = m.paste.plan().expect("a plan");
					assert!(plan.executable(), "{:?}", plan.error);
					assert_eq!(
						plan.items
							.iter()
							.map(|i| i.path.clone())
							.collect::<Vec<_>>(),
						vec!["a.ts".to_string()]
					);
				});
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("sub/a.ts")).as_deref(), Some("x"));
			}

			/// A new paste opened on a folder the engine refuses as it
			/// stands (a regular file `sub` is where the directory `sub/`
			/// would go) lands like a retarget does: the panel stays open,
			/// not executable, with the restore-base suggestion, and
			/// [Adjust paths] recovers it (#240).
			#[gpui::test]
			fn a_new_paste_into_a_refused_folder_still_offers_the_adjustment(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				fs::create_dir(ws.join("sub")).unwrap();
				fs::write(ws.join("sub/sub"), "a file named like its folder")
					.unwrap();
				let (model, cx) = open_ready(cx, &ws);
				clip::write_text(
					"// clipcode-root: mono\n// FILE: sub/a.ts\nx\n",
				)
				.unwrap();
				right_click_probed(&model, cx, "ws-tree-row:sub");
				click_probed(&model, cx, "menu-item:paste-into");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("sub"));
				model.read_with(cx, |m, _| {
					let plan = m.paste.plan().expect("a plan");
					assert!(!plan.executable());
					assert!(plan.error.is_some(), "the engine refused it");
				});
				assert!(
					cx.debug_bounds("paste-restore-base").is_some(),
					"the refusal closed the panel and lost the way back"
				);

				click_probed(&model, cx, "paste-adjust");
				landed(&model, cx);
				model.read_with(cx, |m, _| {
					let plan = m.paste.plan().expect("a plan");
					assert!(plan.executable(), "{:?}", plan.error);
				});
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("sub/a.ts")).as_deref(), Some("x"));
			}

			/// `multi_repo_ws` plus folders whose names begin or end with a
			/// space, or are nothing else (a real name on a real disk), next
			/// to the plain ones they would be mistaken for. Unix only: Win32
			/// strips a trailing space, so `docs ` would be `docs` there.
			#[cfg(unix)]
			fn spaced_ws() -> (tempfile::TempDir, PathBuf) {
				let (tmp, ws) = multi_repo_ws();
				for name in ["docs ", " docs", "   ", " lead", "lead"] {
					fs::create_dir(ws.join(name)).unwrap();
				}
				fs::create_dir(ws.join(" lead").join("inner")).unwrap();
				(tmp, ws)
			}

			/// Every folder a file system names is data: what the Project
			/// menu hands over is the folder it was opened on, never its
			/// trimmed spelling. ` docs` is not `docs`, and the paste is
			/// written there.
			#[cfg(unix)]
			#[gpui::test]
			fn paste_into_a_folder_named_with_a_leading_space_lands_in_it(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				clip::write_text("// FILE: x.txt\nX\n").unwrap();
				right_click_probed(&model, cx, "ws-tree-row: docs");
				click_probed(&model, cx, "menu-item:paste-into");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join(" docs"));
				assert_eq!(folder(&model, cx).1.as_deref(), Some(" docs"));
				apply_with_enter(&model, cx);
				assert_eq!(
					file(&ws.join(" docs").join("x.txt")).as_deref(),
					Some("X")
				);
				for elsewhere in ["docs", "docs ", "", "repoA"] {
					assert!(
						!ws.join(elsewhere).join("x.txt").exists(),
						"written into {elsewhere:?}"
					);
				}
			}

			/// The engine trims the end of a destination root, so a folder
			/// whose name ends with a space cannot be written into (the CLI
			/// fails "unsafe path" there too). The bar says so at once and
			/// the plan stays where it was: the paste is never redirected to
			/// `docs`, and a name of spaces is not the workspace folder.
			#[cfg(unix)]
			#[gpui::test]
			fn a_folder_ending_with_a_space_is_refused_and_nothing_is_redirected(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				for name in ["docs ", "   "] {
					right_click_probed(
						&model,
						cx,
						&format!("ws-tree-row:{name}"),
					);
					click_probed(&model, cx, "menu-item:paste-into");
					settle(cx);
					let said = model
						.read_with(cx, |m, _| m.paste.dest_refusal().cloned())
						.unwrap_or_else(|| panic!("{name:?}: no refusal"));
					assert_eq!(said.key, "paste_err_destination", "{name:?}");
					assert!(
						said.render(crate::i18n::Locale::En)
							.contains("ends with a space"),
						"{name:?}"
					);
					assert!(probed(&model, cx, "paste-dest-status"));
					assert!(paste_focused(&model, cx));
					assert_eq!(plan_destination(&model, cx), first, "{name:?}");
					assert!(executable(&model, cx), "{name:?}");
				}
				apply_with_enter(&model, cx);
				assert_eq!(file(&first.join("x.txt")).as_deref(), Some("X"));
				for elsewhere in ["docs", "docs ", "   ", ""] {
					assert!(
						!ws.join(elsewhere).join("x.txt").exists(),
						"written into {elsewhere:?}"
					);
				}
			}

			/// With no preview open, the menu of such a folder opens none.
			#[cfg(unix)]
			#[gpui::test]
			fn paste_into_a_folder_ending_with_a_space_opens_no_preview(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				clip::write_text("// FILE: x.txt\nX\n").unwrap();
				right_click_probed(&model, cx, "ws-tree-row:docs ");
				click_probed(&model, cx, "menu-item:paste-into");
				settle(cx);
				assert!(!model.read_with(cx, |m, _| m.paste.is_open()));
				assert!(!ws.join("docs").join("x.txt").exists());
				assert!(!ws.join("docs ").join("x.txt").exists());
			}

			/// The breadcrumb names the folders it came from: a segment is
			/// a folder to go back to, and ` lead` is not `lead`.
			#[cfg(unix)]
			#[gpui::test]
			fn a_breadcrumb_segment_with_a_leading_space_goes_to_that_folder(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				let inner = ws.join(" lead").join("inner");
				model
					.update(cx, |m, cx| m.paste_dest_picked(inner.clone(), cx));
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), inner);
				assert_eq!(
					folder(&model, cx).1.as_deref(),
					Some(" lead/inner")
				);
				click_probed(&model, cx, "paste-dest-crumb:1");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join(" lead"));
				assert_eq!(folder(&model, cx).1.as_deref(), Some(" lead"));
				apply_with_enter(&model, cx);
				assert_eq!(
					file(&ws.join(" lead").join("x.txt")).as_deref(),
					Some("X")
				);
				assert!(!ws.join("lead").join("x.txt").exists());
			}

			/// The Project selection is a folder of the file system too.
			#[cfg(unix)]
			#[gpui::test]
			fn the_left_chip_keeps_the_leading_space_of_the_selected_folder(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				click_probed(&model, cx, "ws-tree-row: docs");
				preview(&model, cx, "// FILE: x.txt\nX\n");
				assert!(probed(&model, cx, "paste-dest-left"));
				click_probed(&model, cx, "paste-dest-left");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join(" docs"));
				apply_with_enter(&model, cx);
				assert_eq!(
					file(&ws.join(" docs").join("x.txt")).as_deref(),
					Some("X")
				);
				assert!(!ws.join("docs").join("x.txt").exists());
			}

			/// The folder the OS dialog returns is a name, not typed text:
			/// ` docs` is that folder; `docs ` and `   ` are refused, not
			/// read as `docs` and the workspace.
			#[cfg(unix)]
			#[gpui::test]
			fn a_folder_picked_in_the_dialog_keeps_its_spaces(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				model.update(cx, |m, cx| {
					m.paste_dest_picked(ws.join(" docs"), cx)
				});
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join(" docs"));
				for name in ["docs ", "   "] {
					model.update(cx, |m, cx| {
						m.paste_dest_picked(ws.join(name), cx)
					});
					settle(cx);
					assert!(
						model.read_with(cx, |m, _| m
							.paste
							.dest_refusal()
							.is_some_and(|e| e.key == "paste_err_destination")),
						"{name:?}"
					);
					assert_eq!(
						plan_destination(&model, cx),
						ws.join(" docs"),
						"{name:?}"
					);
				}
				assert_ne!(first, ws.join(" docs"));
			}

			/// What is typed is text: the spaces around it are not part of
			/// the path. The field opened on a folder with a space in its
			/// name, left as it is, still names that folder.
			#[cfg(unix)]
			#[gpui::test]
			fn typed_text_is_trimmed_but_an_untouched_field_names_its_folder(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = spaced_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				model.update(cx, |m, cx| {
					m.paste_dest_picked(ws.join(" docs"), cx)
				});
				landed(&model, cx);
				click_probed(&model, cx, "paste-dest-edit");
				assert_eq!(editor_text(&model, cx), " docs");
				cx.simulate_keystrokes("enter");
				settle(cx);
				assert!(!editor_open(&model, cx));
				assert_eq!(plan_destination(&model, cx), ws.join(" docs"));

				click_probed(&model, cx, "paste-dest-edit");
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input("  docs  ");
				cx.simulate_keystrokes("enter");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("docs"));
				assert_eq!(folder(&model, cx).1.as_deref(), Some("docs"));

				// A typed name that only ends with a space once its slash is
				// gone is the same refusal as the gesture's.
				click_probed(&model, cx, "paste-dest-edit");
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input("docs /");
				cx.simulate_keystrokes("enter");
				settle(cx);
				assert_eq!(
					editor_error(&model, cx).map(|e| e.key),
					Some("paste_err_destination")
				);
				assert_eq!(plan_destination(&model, cx), ws.join("docs"));
			}

			/// Opens the destination editor over a fresh preview.
			fn open_editor(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) {
				preview(model, cx, "// FILE: x.txt\nX\n");
				click_probed(model, cx, "paste-dest-edit");
				assert!(probed(model, cx, "paste-dest-input"));
			}

			#[gpui::test]
			fn typed_destination_applies_on_enter_and_escape_restores(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				open_editor(&model, cx);
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				assert!(editor_focused(&model, cx));
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input("docs");
				cx.simulate_keystrokes("enter");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("docs"));
				assert!(!editor_open(&model, cx));
				assert!(paste_focused(&model, cx));
				// The first Enter chose the folder; the second applies.
				assert!(!ws.join("docs/x.txt").exists());
				apply_with_enter(&model, cx);
				assert_eq!(file(&ws.join("docs/x.txt")).as_deref(), Some("X"));

				// Escape drops what was typed and keeps the folder.
				open_editor(&model, cx);
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input("repoA");
				cx.simulate_keystrokes("escape");
				settle(cx);
				assert!(!editor_open(&model, cx));
				assert_eq!(plan_destination(&model, cx), first);
				assert!(model.read_with(cx, |m, _| m.paste.is_open()));
				assert!(paste_focused(&model, cx));
				// The second Escape closes the panel.
				cx.simulate_keystrokes("escape");
				settle(cx);
				assert!(!model.read_with(cx, |m, _| m.paste.is_open()));
			}

			/// The typed field is an inline layer of the overlay stack
			/// (#226): its field is drawn exactly while the layer is open,
			/// it outlives no preview, and a new preview opens without it.
			#[gpui::test]
			fn the_typed_destination_is_an_inline_layer_that_ends_with_its_preview(
				cx: &mut TestAppContext,
			) {
				use super::overlays::{
					assert_overlay_panels_match, settle_checked,
				};
				use crate::overlay::OverlayKind;
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				open_editor(&model, cx);
				settle_checked(cx, &model);
				assert!(model.read_with(cx, |m, _| m
					.overlays
					.is_open(OverlayKind::PasteDestField)));
				assert_overlay_panels_match(cx, &model);
				assert!(editor_focused(&model, cx));

				// Cancel with the field open: the layer goes with the panel.
				click_probed(&model, cx, "btn-cancel");
				settle_checked(cx, &model);
				assert!(!model.read_with(cx, |m, _| m.paste.is_open()));
				assert!(!editor_open(&model, cx));
				assert_overlay_panels_match(cx, &model);

				// The next preview starts on the folder, not in the field.
				preview(&model, cx, "// FILE: y.txt\nY\n");
				landed(&model, cx);
				settle_checked(cx, &model);
				assert!(!editor_open(&model, cx));
				assert!(!probed(&model, cx, "paste-dest-input"));
				assert!(paste_focused(&model, cx));
				assert_overlay_panels_match(cx, &model);
			}

			/// Refused folders stay in the editor, and the open plan stays
			/// ready for the folder it already had.
			#[gpui::test]
			fn typed_destination_errors_stay_in_the_editor_and_keep_the_plan(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (tmp, ws) = multi_repo_ws();
				#[cfg(unix)]
				{
					let outside = tmp.path().join("outside");
					fs::create_dir(&outside).unwrap();
					std::os::unix::fs::symlink(&outside, ws.join("link"))
						.unwrap();
				}
				#[cfg(not(unix))]
				let _ = &tmp;
				let (model, cx) = open_ready(cx, &ws);
				open_editor(&model, cx);
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				let mut cases = vec![
					("missing/dir", "paste_err_destination", None),
					(
						"../outside",
						"paste_err_destination",
						Some("not under the workspace"),
					),
					("docs/readme.txt", "paste_err_destination_not_dir", None),
				];
				// A runtime guard, not a `cfg`: `mut` is used on every platform.
				if cfg!(unix) {
					cases.push((
						"link",
						"paste_err_destination",
						Some("not under the workspace"),
					));
				}
				for (typed, key, words) in cases {
					cx.simulate_keystrokes("cmd-a");
					cx.simulate_input(typed);
					cx.simulate_keystrokes("enter");
					settle(cx);
					let err = editor_error(&model, cx)
						.unwrap_or_else(|| panic!("{typed}: no error"));
					assert_eq!(err.key, key, "{typed}");
					if let Some(words) = words {
						let en = err.render(crate::i18n::Locale::En);
						assert!(en.contains(words), "{typed}: {en}");
					}
					assert!(editor_open(&model, cx), "{typed}: editor closed");
					assert!(probed(&model, cx, "paste-dest-status"), "{typed}");
					assert_eq!(plan_destination(&model, cx), first, "{typed}");
					assert!(executable(&model, cx), "{typed}: plan disarmed");
				}
				cx.simulate_keystrokes("escape");
				settle(cx);
				assert!(!editor_open(&model, cx));
				apply_with_enter(&model, cx);
				assert_eq!(file(&first.join("x.txt")).as_deref(), Some("X"));
			}

			/// A space typed in the field is text, not the preview's row
			/// toggle.
			#[gpui::test]
			fn a_space_typed_in_the_destination_reaches_the_input(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				open_editor(&model, cx);
				cx.simulate_keystrokes("cmd-a");
				// The key itself, through the keymap: `simulate_input` skips
				// it, and without the field's `NoAction` binding the preview
				// takes the space as its row toggle.
				cx.simulate_input("my");
				cx.simulate_keystrokes("space");
				cx.simulate_input("dir");
				assert_eq!(editor_text(&model, cx), "my dir");
				assert!(model.read_with(cx, |m, _| {
					m.paste.plan().unwrap().items.iter().all(|i| i.selected)
				}));
				cx.simulate_keystrokes("enter");
				landed(&model, cx);
				assert_eq!(plan_destination(&model, cx), ws.join("my dir"));
			}

			/// A path typed but not yet confirmed must not be pasted into
			/// the old folder, by any route to Apply: a plan that lands
			/// while the field is open leaves the focus in the field, and a
			/// panel that does hold the focus refuses Enter.
			#[gpui::test]
			fn enter_never_applies_while_the_destination_field_is_open(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: notes/x.txt\nN\n");
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				click_probed(&model, cx, "paste-dest-edit");
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input("docs");
				// A plan landing under the open field (the report: a prefix
				// choice made with the pointer while typing).
				click_probed(&model, cx, "paste-map-keep:notes");
				landed(&model, cx);
				assert!(editor_open(&model, cx));
				assert!(
					editor_focused(&model, cx),
					"the field keeps the focus a landing plan would take"
				);
				assert!(!paste_focused(&model, cx));

				// The panel holds the focus all the same (a click on a row):
				// Enter is refused, the field stays.
				click_probed(&model, cx, "paste-row:0:notes/x.txt");
				settle(cx);
				assert!(paste_focused(&model, cx));
				cx.simulate_keystrokes("enter");
				settle(cx);
				assert!(editor_open(&model, cx));
				assert!(model.read_with(cx, |m, _| m.paste.is_open()
					&& !m.paste_busy()));
				assert!(!first.join("notes/x.txt").exists());
				assert!(!ws.join("docs/notes/x.txt").exists());
				assert!(!ws.join("notes/x.txt").exists());
			}

			/// A folder the bar's gestures cannot take is said on the bar,
			/// beside the plan that stays as it was, until the destination
			/// moves or the user types; the focus stays with the panel.
			#[gpui::test]
			fn a_refused_folder_is_said_on_the_bar_and_the_plan_stays(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				let first = model.read_with(cx, |m, _| m.repo_root().unwrap());
				// Turns the probes on.
				probed_point(&model, cx, "paste-dest-root");
				model.update(cx, |m, cx| {
					m.paste_into_rel(
						&crate::paste::dest::WsRel::parse_typed("missing")
							.unwrap(),
						"crumb",
						cx,
					)
				});
				settle(cx);
				let said = model.read_with(cx, |m, _| {
					m.paste.dest_refusal().map(|e| e.key)
				});
				assert_eq!(said, Some("paste_err_destination"));
				assert!(probed(&model, cx, "paste-dest-status"));
				assert!(paste_focused(&model, cx));
				assert_eq!(plan_destination(&model, cx), first);
				assert!(executable(&model, cx));

				// Moving the destination clears it.
				click_probed(&model, cx, "paste-dest-root");
				landed(&model, cx);
				assert!(model
					.read_with(cx, |m, _| m.paste.dest_refusal().is_none()));
			}

			/// A folder that comes back from the OS dialog after the preview
			/// was cancelled leaves nothing behind.
			#[gpui::test]
			fn a_destination_after_the_preview_closed_changes_nothing(
				cx: &mut TestAppContext,
			) {
				let Some(_clip) = clipboard() else { return };
				let (_tmp, ws) = multi_repo_ws();
				let (model, cx) = open_ready(cx, &ws);
				preview(&model, cx, "// FILE: x.txt\nX\n");
				cx.simulate_keystrokes("escape");
				settle(cx);
				assert!(!model.read_with(cx, |m, _| m.paste.is_open()));
				model.update(cx, |m, cx| {
					m.paste_dest_picked(ws.join("docs"), cx)
				});
				settle(cx);
				model.read_with(cx, |m, _| {
					assert!(m.paste.folder().is_none());
					assert!(m.paste.dest_refusal().is_none());
					assert!(!m.paste.is_open());
				});
			}

			/// The workspace root of a remote workspace is a destination of
			/// its own, not the repo the paste would default to.
			#[cfg(unix)]
			#[gpui::test]
			fn remote_root_chip_plans_and_writes_the_workspace_itself(
				cx: &mut TestAppContext,
			) {
				let _serial = remote_lock();
				let Some(_clip) = clipboard() else { return };
				let tmp = tempfile::tempdir().unwrap();
				let shared = tmp.path().join("shared");
				fs::create_dir(&shared).unwrap();
				for name in ["repoA", "repoB"] {
					let r = shared.join(name);
					fs::create_dir(&r).unwrap();
					crate::paste::tests::git_init(&r);
					fs::write(r.join("init.txt"), "hello\n").unwrap();
					crate::paste::tests::git_run(&r, &["add", "init.txt"]);
					crate::paste::tests::git_run(&r, &["commit", "-m", "init"]);
				}
				let (model, cx, _worker) = open_remote(
					cx,
					&shared,
					snip_remote::WorkerOptions {
						name: "test-worker".into(),
						..Default::default()
					},
				);
				model.update(cx, |m, cx| m.select_repo(0, cx));
				settle(cx);
				remote_paste_preview(&model, cx, "// FILE: x.txt\nX\n");
				let (root, first) = model.read_with(cx, |m, _| {
					(
						m.remote.session.as_ref().unwrap().root.clone(),
						m.repo_root().unwrap(),
					)
				});
				assert_ne!(first, root);
				assert_eq!(plan_destination(&model, cx), first);
				assert_eq!(folder(&model, cx).0, first);

				click_probed(&model, cx, "paste-dest-root");
				for _ in 0..300 {
					settle(cx);
					if model.read_with(cx, |m, _| !m.paste.is_loading()) {
						break;
					}
					std::thread::sleep(Duration::from_millis(10));
				}
				assert_eq!(plan_destination(&model, cx), root);
				assert_eq!(
					crate::remote::remote_rel(&root, &root).as_deref(),
					Some("")
				);
				assert!(executable(&model, cx));
				remote_paste_apply(&model, cx);
				assert_eq!(file(&shared.join("x.txt")).as_deref(), Some("X"));
				assert!(!shared.join("repoA/x.txt").exists());
				assert!(!shared.join("repoB/x.txt").exists());

				// A folder the worker has none of: its refusal, in the
				// interface language, and nothing to apply.
				remote_paste_preview(&model, cx, "// FILE: y.txt\nY\n");
				click_probed(&model, cx, "paste-dest-edit");
				cx.simulate_keystrokes("cmd-a");
				cx.simulate_input("nope");
				cx.simulate_keystrokes("enter");
				for _ in 0..300 {
					settle(cx);
					if model.read_with(cx, |m, _| !m.paste.is_loading()) {
						break;
					}
					std::thread::sleep(Duration::from_millis(10));
				}
				assert_eq!(plan_destination(&model, cx), root.join("nope"));
				assert!(!executable(&model, cx));
				let said = model.read_with(cx, |m, _| {
					m.paste
						.plan()
						.and_then(|p| p.error.as_ref())
						.map(|e| e.render(crate::i18n::Locale::ZhTw))
				});
				let said = said.expect("the worker's refusal is shown");
				assert!(said.contains("找不到"), "{said}");
				assert!(!said.contains("No such file"), "{said}");
				assert!(model.read_with(cx, |m, _| m.paste.is_open()));
				cx.simulate_keystrokes("enter");
				settle(cx);
				assert!(!shared.join("nope").exists());
				assert!(!shared.join("y.txt").exists());
			}

			fn editor_open(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> bool {
				model.read_with(cx, |m, _| m.paste_dest_edit().is_some())
			}

			fn editor_focused(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> bool {
				cx.update(|w, cx| {
					model
						.read(cx)
						.paste_dest_input
						.read(cx)
						.handle()
						.is_focused(w)
				})
			}

			fn editor_text(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> String {
				model.read_with(cx, |m, cx| {
					m.paste_dest_input.read(cx).text().to_string()
				})
			}

			fn editor_error(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> Option<crate::i18n::Msg> {
				model.read_with(cx, |m, _| {
					m.paste_dest_edit().and_then(|e| e.error.clone())
				})
			}

			/// The folder the session holds, and where it lies in the
			/// workspace.
			fn folder(
				model: &Entity<WorkbenchModel>,
				cx: &mut VisualTestContext,
			) -> (PathBuf, Option<String>) {
				model.read_with(cx, |m, _| {
					let f = m.paste.folder().expect("a destination folder");
					(
						f.path.clone(),
						f.rel.as_ref().map(|r| r.as_str().to_string()),
					)
				})
			}
		}
	}

	mod folder_copy {
		use crate::{native_export_settings, NATIVE_FILE_COUNT_LIMIT};
		use snip_core::gitrun::{CancelToken, RunOptions};
		use snip_core::transfer::expand_folder_items;
		use snip_core::transfer::{
			copy_selection_detailed, plan_export, CanonicalRootId, ExportItem,
			ExportSelection, SourceKind,
		};
		use std::fs;
		use std::path::{Path, PathBuf};

		fn canonical_tmp() -> (tempfile::TempDir, PathBuf) {
			let tmp = tempfile::tempdir().unwrap();
			let root = CanonicalRootId::new(tmp.path())
				.unwrap()
				.path()
				.to_path_buf();
			(tmp, root)
		}

		fn selection(root: &Path, rels: &[&str]) -> ExportSelection {
			let id = CanonicalRootId::new(root).unwrap();
			let items = rels
				.iter()
				.map(|rel| ExportItem {
					root: id.clone(),
					relative_path: rel.to_string(),
					source: SourceKind::File,
					change_type: None,
					gitlink: false,
				})
				.collect();
			ExportSelection::new(
				vec![root.to_path_buf()],
				Some(root.to_path_buf()),
				items,
			)
			.unwrap()
		}

		fn rels(sel: &ExportSelection) -> Vec<&str> {
			sel.items
				.iter()
				.map(|item| item.relative_path.as_str())
				.collect()
		}

		/// One file the payload cannot carry costs that file only.
		#[test]
		#[cfg(unix)]
		fn folder_skips_files_the_payload_cannot_carry() {
			let (_tmp, root) = canonical_tmp();
			let outside = tempfile::tempdir().unwrap();
			fs::write(outside.path().join("secret.txt"), "SECRET").unwrap();
			let d = root.join("d");
			fs::create_dir_all(&d).unwrap();
			fs::write(d.join("good.txt"), "GOOD").unwrap();
			fs::write(d.join("a\\b.txt"), "BACKSLASH").unwrap();
			fs::write(d.join("bad>.txt"), "BAD").unwrap();
			fs::write(d.join("trail.txt "), "TRAIL").unwrap();
			std::os::unix::fs::symlink(d.join("gone.txt"), d.join("dangling"))
				.unwrap();
			std::os::unix::fs::symlink(
				outside.path().join("secret.txt"),
				d.join("escape.txt"),
			)
			.unwrap();
			let fifo = std::process::Command::new("mkfifo")
				.arg(d.join("pipe"))
				.status()
				.is_ok_and(|s| s.success());
			let expanded = expand_folder_items(
				selection(&root, &["d"]),
				100,
				&CancelToken::new(),
			)
			.unwrap();
			assert_eq!(rels(&expanded.sel), ["d/good.txt"]);
			assert_eq!(expanded.skipped, 5 + usize::from(fifo));
			assert!(!expanded.truncated);
			let plan =
				plan_export(&expanded.sel, &native_export_settings(), None)
					.unwrap();
			assert!(plan.payload.contains("GOOD"));
			assert!(!plan.payload.contains("SECRET"));
		}

		#[test]
		#[cfg(unix)]
		fn folder_skips_an_unreadable_file() {
			use std::os::unix::fs::PermissionsExt;
			let (_tmp, root) = canonical_tmp();
			fs::create_dir_all(root.join("d")).unwrap();
			fs::write(root.join("d/ok.txt"), "OK").unwrap();
			let locked = root.join("d/locked.txt");
			fs::write(&locked, "LOCKED").unwrap();
			fs::set_permissions(&locked, fs::Permissions::from_mode(0o0))
				.unwrap();
			if fs::File::open(&locked).is_ok() {
				// Running as root: nothing is unreadable.
				return;
			}
			let expanded = expand_folder_items(
				selection(&root, &["d"]),
				100,
				&CancelToken::new(),
			)
			.unwrap();
			assert_eq!(rels(&expanded.sel), ["d/ok.txt"]);
			assert_eq!(expanded.skipped, 1);
			plan_export(&expanded.sel, &native_export_settings(), None)
				.unwrap();
		}

		/// A selected folder that is itself a repo is never walked, and
		/// neither is a repo below a selected folder.
		#[test]
		fn folder_walk_never_enters_a_repo() {
			let (_tmp, root) = canonical_tmp();
			fs::create_dir_all(root.join("sub/.git")).unwrap();
			fs::write(root.join("sub/in_repo.txt"), "IN").unwrap();
			fs::create_dir_all(root.join("app/inner/.git")).unwrap();
			fs::write(root.join("app/inner/deep.txt"), "DEEP").unwrap();
			fs::write(root.join("app/top.txt"), "TOP").unwrap();
			fs::write(root.join("x.txt"), "X").unwrap();
			let expanded = expand_folder_items(
				selection(&root, &["sub", "app", "x.txt"]),
				100,
				&CancelToken::new(),
			)
			.unwrap();
			assert_eq!(rels(&expanded.sel), ["app/top.txt", "x.txt"]);
		}

		/// The walk stops at the limit, says so, and never starves a file
		/// picked on its own after the folder.
		#[test]
		fn folder_walk_stops_at_the_limit_and_keeps_explicit_files() {
			let (_tmp, root) = canonical_tmp();
			fs::create_dir_all(root.join("big")).unwrap();
			for i in 0..12 {
				fs::write(root.join(format!("big/f{i:02}.txt")), "B").unwrap();
			}
			fs::write(root.join("z.txt"), "Z").unwrap();
			let expanded = expand_folder_items(
				selection(&root, &["big", "z.txt"]),
				5,
				&CancelToken::new(),
			)
			.unwrap();
			assert_eq!(
				rels(&expanded.sel),
				[
					"big/f00.txt",
					"big/f01.txt",
					"big/f02.txt",
					"big/f03.txt",
					"z.txt"
				]
			);
			assert!(expanded.truncated);
			let exact = expand_folder_items(
				selection(&root, &["big"]),
				12,
				&CancelToken::new(),
			)
			.unwrap();
			assert_eq!(exact.sel.items.len(), 12);
			assert!(!exact.truncated);
		}

		/// A copy the file limit cuts reaches the toast as a partial copy;
		/// the shared engine stops at the limit, and copies the whole
		/// folder without one.
		#[test]
		fn truncated_copy_says_so_in_the_status() {
			let (_tmp, root) = canonical_tmp();
			fs::create_dir_all(root.join("big")).unwrap();
			for i in 0..8 {
				fs::write(root.join(format!("big/{i}.txt")), "B").unwrap();
			}
			let mut cut = native_export_settings();
			cut.set_max_file_count = true;
			cut.file_count_limit = 3.0;
			let report = copy_selection_detailed(
				selection(&root, &["big"]),
				&cut,
				NATIVE_FILE_COUNT_LIMIT,
				&RunOptions::default(),
				|_| {},
			)
			.unwrap();
			assert_eq!(report.outcome.copied, 3);
			assert!(report.outcome.truncated);
			let msg = crate::copied_status("r".into(), &report.outcome);
			assert_eq!(msg.key, "status_copied_limit");
			assert_eq!(msg.args[1], "3");
			assert_eq!(msg.args[4], report.outcome.words.to_string());
			assert_eq!(msg.args[5], report.outcome.tokens.to_string());
			assert_eq!(msg.args[7], NATIVE_FILE_COUNT_LIMIT.to_string());

			let mut whole = native_export_settings();
			whole.set_max_file_count = false;
			let report = copy_selection_detailed(
				selection(&root, &["big"]),
				&whole,
				NATIVE_FILE_COUNT_LIMIT,
				&RunOptions::default(),
				|_| {},
			)
			.unwrap();
			assert_eq!(report.outcome.copied, 8);
			assert!(!report.outcome.truncated);
			assert_eq!(
				crate::copied_status("r".into(), &report.outcome).key,
				"status_copied"
			);
		}

		/// ClipCode's 30-file default would silently cut a folder copy.
		#[test]
		fn native_limit_copies_past_thirty_files() {
			let (_tmp, root) = canonical_tmp();
			fs::create_dir_all(root.join("many")).unwrap();
			for i in 0..40 {
				fs::write(root.join(format!("many/{i:02}.txt")), "M").unwrap();
			}
			let expanded = expand_folder_items(
				selection(&root, &["many"]),
				NATIVE_FILE_COUNT_LIMIT,
				&CancelToken::new(),
			)
			.unwrap();
			let plan =
				plan_export(&expanded.sel, &native_export_settings(), None)
					.unwrap();
			assert_eq!(plan.copied_file_count, 40);
			assert!(!plan.file_limit_reached);
		}
	}

	#[test]
	fn lossy_change_names_are_not_copyable() {
		let item = |path: String| FileChangeItem {
			path,
			change_type: None,
			source: SourceKind::Working,
			is_conflict: false,
			repo: 0,
		};
		let lossy = String::from_utf8_lossy(b"bad\xff.txt").into_owned();
		assert!(!item(lossy).is_valid_utf8());
		assert!(item("長路徑/good.txt".into()).is_valid_utf8());
	}

	#[test]
	fn read_preview_honors_explicit_source_and_missing_revision() {
		let dir = tempfile::tempdir().unwrap();
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(dir.path())
				.args(args)
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
		};
		git(&["init", "-q", "-b", "main"]);
		git(&["config", "user.name", "Preview test"]);
		git(&["config", "user.email", "preview@example.invalid"]);
		std::fs::write(dir.path().join("a.txt"), "committed A\n").unwrap();
		git(&["add", "a.txt"]);
		git(&["commit", "-qm", "A"]);
		std::fs::write(dir.path().join("a.txt"), "staged A\n").unwrap();
		git(&["add", "a.txt"]);
		std::fs::write(dir.path().join("a.txt"), "working B\n").unwrap();
		let result = super::read_preview(
			&super::githost::GitHost::Local,
			dir.path(),
			"a.txt",
			&super::SourceKind::Commit {
				rev: "refs/heads/missing-preview-revision".into(),
			},
			super::CancelToken::new(),
		);
		assert!(
			result.is_err(),
			"missing commit must not display working B as a successful preview"
		);
		let (staged, source) = super::read_preview(
			&super::githost::GitHost::Local,
			dir.path(),
			"a.txt",
			&super::SourceKind::Staged,
			super::CancelToken::new(),
		)
		.unwrap();
		assert_eq!(source, super::PreviewSource::StagedChanges);
		assert_eq!(staged.content.as_deref(), Some("staged A\n"));
		assert!(!staged.patch.contains("working B"));
		let (working, source) = super::read_preview(
			&super::githost::GitHost::Local,
			dir.path(),
			"a.txt",
			&super::SourceKind::File,
			super::CancelToken::new(),
		)
		.unwrap();
		assert_eq!(source, super::PreviewSource::WorkingFile);
		assert_eq!(working.content.as_deref(), Some("working B\n"));
		git(&["rm", "-q", "-f", "a.txt"]);
		let (deleted, source) = super::read_preview(
			&super::githost::GitHost::Local,
			dir.path(),
			"a.txt",
			&super::SourceKind::Staged,
			super::CancelToken::new(),
		)
		.unwrap();
		assert_eq!(source, super::PreviewSource::StagedChanges);
		assert!(deleted.patch.contains("-committed A"));
		assert!(deleted
			.content
			.as_deref()
			.is_some_and(|body| body.contains("committed A")
				&& !body.contains("working B")));
	}

	/// A staged rename yields exactly one Moved row in read_change_list,
	/// and exporting it yields [MOVED] new-name with the index bytes and no [DELETED] old.
	#[test]
	fn staged_rename_read_change_list_and_export_moved() {
		use snip_core::format::ChangeType;
		let dir = tempfile::tempdir().unwrap();
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(dir.path())
				.args(args)
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
		};
		git(&["init", "-q", "-b", "main"]);
		git(&["config", "user.name", "Test"]);
		git(&["config", "user.email", "test@example.invalid"]);
		std::fs::write(dir.path().join("old-name.txt"), "index bytes\n")
			.unwrap();
		git(&["add", "old-name.txt"]);
		git(&["commit", "-qm", "initial"]);
		git(&["mv", "old-name.txt", "new-name.txt"]);

		let (_summary, items, _total) = super::read_change_list(
			&super::githost::GitHost::Local,
			dir.path(),
			None,
			super::CancelToken::new(),
		)
		.unwrap();
		let staged_items: Vec<_> = items
			.iter()
			.filter(|(_, _, s, _)| *s == super::SourceKind::Staged)
			.collect();
		assert_eq!(staged_items.len(), 1);
		assert_eq!(staged_items[0].0, "new-name.txt");
		assert_eq!(staged_items[0].1, Some(ChangeType::Moved));

		// Verify no old-name.txt row in any group
		assert!(!items.iter().any(|(p, _, _, _)| p == "old-name.txt"));

		let root = super::CanonicalRootId::new(dir.path()).unwrap();
		let export_item = super::ExportItem {
			root: root.clone(),
			relative_path: staged_items[0].0.clone(),
			source: super::SourceKind::Staged,
			change_type: staged_items[0].1,
			gitlink: false,
		};
		let sel = super::ExportSelection::new(
			vec![dir.path().to_path_buf()],
			Some(dir.path().to_path_buf()),
			vec![export_item],
		)
		.unwrap();
		let plan = snip_core::transfer::plan_export_with(
			&sel,
			&super::native_export_settings(),
			None,
			&snip_core::gitrun::RunOptions::default(),
		)
		.unwrap();
		let payload = &plan.payload;
		assert!(
			payload.contains("[MOVED] new-name.txt"),
			"payload: {payload}"
		);
		assert!(payload.contains("index bytes\n"), "payload: {payload}");
		assert!(!payload.contains("[DELETED]"), "payload: {payload}");
		assert!(!payload.contains("old-name.txt"), "payload: {payload}");
	}

	#[cfg(unix)]
	#[test]
	fn read_preview_refuses_symlink_leaving_workspace() {
		use std::os::unix::fs::symlink;

		let outside_dir = tempfile::tempdir().unwrap();
		let secret_file = outside_dir.path().join("secret.txt");
		std::fs::write(&secret_file, "SECRET_OUTSIDE_REPO\n").unwrap();

		let repo_dir = tempfile::tempdir().unwrap();
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(repo_dir.path())
				.args(args)
				.output()
				.unwrap();
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
		};
		git(&["init", "-q", "-b", "main"]);
		git(&["config", "user.name", "Test"]);
		git(&["config", "user.email", "test@example.invalid"]);
		let notes_path = repo_dir.path().join("notes.txt");
		std::fs::write(&notes_path, "committed notes\n").unwrap();
		git(&["add", "notes.txt"]);
		git(&["commit", "-qm", "initial"]);

		// Replace notes.txt in worktree with a symlink to secret.txt outside the repo
		std::fs::remove_file(&notes_path).unwrap();
		symlink(&secret_file, &notes_path).unwrap();

		let result = super::read_preview(
			&super::githost::GitHost::Local,
			repo_dir.path(),
			"notes.txt",
			&super::SourceKind::Unstaged,
			super::CancelToken::new(),
		);
		match result {
			Err(err) => {
				// A refusal by its reason, not by the English text.
				let PreviewErr::Failed(msg) = &err else {
					panic!("expected a refusal, got: {err:?}");
				};
				assert_eq!(
					*msg,
					crate::reason::reason_msg(
						&snip_core::reason::Reason::PermissionDenied {
							path: String::new()
						}
					)
				);
			}
			Ok((preview, _)) => {
				let content = preview.content.unwrap_or_default();
				assert!(
					!content.contains("SECRET_OUTSIDE_REPO"),
					"secret text leaked in preview content"
				);
				assert!(
					!preview.patch.contains("SECRET_OUTSIDE_REPO"),
					"secret text leaked in preview patch"
				);
				panic!("read_preview must return Err for symlink leaving workspace, got Ok");
			}
		}
	}

	#[test]
	fn read_change_list_reports_total_and_respects_cap() {
		let dir = tempfile::tempdir().unwrap();
		let git = |args: &[&str]| {
			let out = std::process::Command::new("git")
				.current_dir(dir.path())
				.args(args)
				.output()
				.unwrap();
			assert!(out.status.success(), "{args:?}");
		};
		git(&["init", "-q", "-b", "main"]);
		for i in 0..MAX_CHANGES_PER_REPO + 5 {
			std::fs::File::create(dir.path().join(format!("file_{i:04}.txt")))
				.unwrap();
		}
		let (_summary, items, total) = super::read_change_list(
			&super::githost::GitHost::Local,
			dir.path(),
			None,
			super::CancelToken::new(),
		)
		.unwrap();
		assert_eq!(items.len(), MAX_CHANGES_PER_REPO);
		assert_eq!(total, MAX_CHANGES_PER_REPO + 5);
	}

	use super::*;
	use snip_core::workspace::GitMarker;
	use std::fs;
	use std::process::Command;

	fn run_git(cwd: &std::path::Path, args: &[&str]) {
		let st = Command::new("git")
			.args(args)
			.current_dir(cwd)
			.status()
			.expect("git must run");
		assert!(st.success(), "git failed: {args:?}");
	}

	/// The `COPY_COMMITS_ERR` line is a machine contract (the e2e drivers
	/// match it): for a selection that is not one first-parent chain it
	/// starts with the engine's text, whatever the UI says in any language.
	#[test]
	fn copy_commits_err_line_is_the_engines_text_not_the_uis() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path();
		run_git(root, &["init", "-q"]);
		let mut shas = Vec::new();
		for n in 1..=3 {
			fs::write(root.join("f.txt"), format!("{n}\n")).unwrap();
			run_git(root, &["add", "f.txt"]);
			run_git(
				root,
				&[
					"-c",
					"user.name=t",
					"-c",
					"user.email=t@example.com",
					"commit",
					"-q",
					"-m",
					&format!("c{n}"),
				],
			);
			let out = Command::new("git")
				.args(["rev-parse", "HEAD"])
				.current_dir(root)
				.output()
				.unwrap();
			shas.push(
				String::from_utf8(out.stdout).unwrap().trim().to_string(),
			);
		}
		// The first and third commit: the second is skipped.
		let selected = vec![shas[0].clone(), shas[2].clone()];
		let (log, msg) =
			export_commits_local(root, &shas[2], &selected, CancelToken::new())
				.unwrap_err();
		let line = copy_commits_err_line(&log);
		assert!(
			line.starts_with(
				"[APP:COPY_COMMITS_ERR: commits are not contiguous:"
			),
			"{line}"
		);
		// The UI words it its own way, in either language.
		let ui = msg.render(Locale::En);
		assert!(ui.contains("not contiguous"), "{ui}");
		assert!(!ui.starts_with("commits are not contiguous"), "{ui}");
	}

	#[test]
	fn test_disambiguate_repo_names() {
		let mut repos = vec![
			RepoEntry {
				root: PathBuf::from("/workspace/a/core"),
				name: "core".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
			RepoEntry {
				root: PathBuf::from("/workspace/b/core"),
				name: "core".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
			RepoEntry {
				root: PathBuf::from("/workspace/other"),
				name: "other".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
		];
		let ws = PathBuf::from("/workspace");
		WorkbenchModel::disambiguate_repo_names(&mut repos, &ws);
		assert_eq!(repos[0].name, "a/core");
		assert_eq!(repos[1].name, "b/core");
		assert_eq!(repos[2].name, "other");
	}

	#[test]
	fn test_disambiguate_repo_names_when_root_does_not_strip() {
		let mut repos1 = vec![
			RepoEntry {
				root: PathBuf::from("/real/x/svc/app"),
				name: "app".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
			RepoEntry {
				root: PathBuf::from("/real/y/svc/app"),
				name: "app".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
			RepoEntry {
				root: PathBuf::from("/real/z/other"),
				name: "other".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
		];
		let ws = PathBuf::from("/link");
		WorkbenchModel::disambiguate_repo_names(&mut repos1, &ws);
		assert_eq!(repos1[0].name, "x/svc/app");
		assert_eq!(repos1[1].name, "y/svc/app");
		assert_eq!(repos1[2].name, "other");

		let mut repos2 = vec![
			RepoEntry {
				root: PathBuf::from("/real/p/x/svc/app"),
				name: "app".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
			RepoEntry {
				root: PathBuf::from("/real/q/x/svc/app"),
				name: "app".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err(crate::test_failure("mock")),
			},
		];
		WorkbenchModel::disambiguate_repo_names(&mut repos2, &ws);
		assert_eq!(repos2[0].name, "p/x/svc/app");
		assert_eq!(repos2[1].name, "q/x/svc/app");
	}

	/// Adding a repository that cannot be added says why as data, not as
	/// text to read.
	#[test]
	fn resolve_added_repo_failures_carry_a_reason() {
		let temp = tempfile::tempdir().unwrap();
		let cancel = CancelToken::new();
		cancel.cancel();
		let err =
			resolve_added_repo(temp.path().to_path_buf(), &cancel).unwrap_err();
		assert_eq!(err.reason, Reason::Cancelled);

		let missing = temp.path().join("nope");
		let err = resolve_added_repo(missing, &CancelToken::new()).unwrap_err();
		assert!(
			matches!(err.reason, Reason::NotFound { .. }),
			"{:?}",
			err.reason
		);
		assert!(!err.message.is_empty());
	}

	#[test]
	fn test_discovery_processing_symlink_dedup_and_submodules() {
		let temp = tempfile::tempdir().unwrap();
		let root = temp.path();

		let main = root.join("main");
		fs::create_dir_all(&main).unwrap();
		run_git(&main, &["init"]);
		run_git(&main, &["config", "user.name", "Test"]);
		run_git(&main, &["config", "user.email", "test@test.local"]);
		fs::write(main.join("file.txt"), "hello").unwrap();
		run_git(&main, &["add", "."]);
		run_git(&main, &["commit", "-m", "init"]);

		#[cfg(unix)]
		{
			std::os::unix::fs::symlink(&main, root.join("main-alias")).unwrap();
		}

		let gitmodules_content = "[submodule \"vendor/sub\"]\n\tpath = vendor/sub\n\turl = https://example.invalid/sub.git\n";
		fs::write(main.join(".gitmodules"), gitmodules_content).unwrap();

		let opts = RunOptions::default();
		#[allow(unused_mut)]
		let mut disc = vec![DiscoveredRepo {
			path: main.clone(),
			marker: GitMarker::Directory,
		}];
		#[cfg(unix)]
		{
			disc.push(DiscoveredRepo {
				path: root.join("main-alias"),
				marker: GitMarker::Directory,
			});
		}

		let (list, _errors) =
			WorkbenchModel::process_discovery_repos(disc, &opts);

		let main_entries: Vec<_> = list
			.iter()
			.filter(|r| r.name == "main" || r.name == "main-alias")
			.collect();
		assert_eq!(
			main_entries.len(),
			1,
			"symlink alias must be deduplicated: {main_entries:?}"
		);
		assert_eq!(main_entries[0].kind, RepoEntryKind::Main);

		let sub_entries: Vec<_> =
			list.iter().filter(|r| r.name == "vendor/sub").collect();
		assert_eq!(
			sub_entries.len(),
			1,
			"uninitialized submodule must be listed: {list:?}"
		);
		assert_eq!(sub_entries[0].kind, RepoEntryKind::UninitializedSubmodule);
		assert!(sub_entries[0].identity.is_none());
		assert!(sub_entries[0].summary.is_err());
	}

	#[test]
	fn release_helpers_drop_backing_capacity() {
		let mut values = Vec::<u8>::with_capacity(64);
		values.extend_from_slice(&[1, 2, 3]);
		release_vec(&mut values);
		assert!(values.is_empty());
		assert_eq!(values.capacity(), 0);

		let mut names = HashMap::<String, String>::with_capacity(16);
		names.insert("root".into(), "x".repeat(128));
		release_map(&mut names);
		assert!(names.is_empty());
		assert_eq!(names.capacity(), 0);

		let mut path = PathBuf::with_capacity(128);
		path.push("/tmp/workspace");
		release_path(&mut path);
		assert!(path.as_os_str().is_empty());
		assert_eq!(path.capacity(), 0);
	}

	fn change(path: &str, source: SourceKind, repo: u32) -> FileChangeItem {
		FileChangeItem {
			path: path.into(),
			change_type: None,
			source,
			is_conflict: false,
			repo,
		}
	}

	#[test]
	fn change_queue_runs_at_most_two_reads() {
		let mut q = ReadQueue::new(MAX_CHANGES_READS);
		for n in 0..15 {
			q.push(n);
		}
		q.push(3);
		assert_eq!(q.take(), Some(0));
		assert_eq!(q.take(), Some(1));
		assert_eq!(q.take(), None, "a third read waits");
		assert_eq!(q.in_flight(), 2);
		q.finish();
		assert_eq!(q.take(), Some(2));
		assert_eq!(q.take(), None);
		let mut started = 3;
		while q.in_flight() > 0 {
			q.finish();
			while let Some(n) = q.take() {
				assert_eq!(n, started, "reads start in order, once each");
				started += 1;
				assert!(q.in_flight() <= MAX_CHANGES_READS);
			}
		}
		assert_eq!(started, 15);
		q.push(1);
		q.clear_pending();
		assert_eq!(q.take(), None);
	}

	/// A node renamed in place moves to where its new name sorts, and its
	/// rows with it, in the order they had (#227).
	#[test]
	fn a_renamed_change_slot_moves_with_its_rows() {
		use std::path::Path;
		let mut slots = Vec::new();
		let mut files = Vec::new();
		for n in ["a", "b", "c"] {
			let root = format!("/w/{n}");
			let slot = slot_insert(&mut slots, &mut files, Path::new(&root), n);
			let rows = ["1", "2"]
				.map(|i| {
					change(
						&format!("{n}{i}.txt"),
						SourceKind::Working,
						slot as u32,
					)
				})
				.to_vec();
			slot_replace_rows(&mut files, slot, rows);
		}
		slots[0].name = "z".into();
		assert!(!slots_sorted(&slots));
		slots_resort(&mut slots, &mut files);
		assert!(slots_sorted(&slots));
		let names: Vec<_> = slots.iter().map(|s| s.name.as_str()).collect();
		assert_eq!(names, ["b", "c", "z"]);
		let paths: Vec<_> =
			files.iter().map(|f| (f.repo, f.path.as_str())).collect();
		assert_eq!(
			paths,
			[
				(0, "b1.txt"),
				(0, "b2.txt"),
				(1, "c1.txt"),
				(1, "c2.txt"),
				(2, "a1.txt"),
				(2, "a2.txt"),
			]
		);
	}

	#[test]
	fn change_slots_keep_rows_grouped_by_repo() {
		use std::path::Path;
		let mut slots = Vec::new();
		let mut files = Vec::new();
		let b = slot_insert(&mut slots, &mut files, Path::new("/w/b"), "b");
		slot_replace_rows(
			&mut files,
			b,
			vec![
				change("x.txt", SourceKind::Unstaged, 0),
				change("y.txt", SourceKind::Working, 0),
			],
		);
		// A repo sorting first renumbers the rows after it.
		let a = slot_insert(&mut slots, &mut files, Path::new("/w/a"), "a");
		assert_eq!((a, slots[1].name.as_str()), (0, "b"));
		assert!(files.iter().all(|f| f.repo == 1));
		slot_replace_rows(
			&mut files,
			a,
			vec![change("x.txt", SourceKind::Staged, 0)],
		);
		let c = slot_insert(&mut slots, &mut files, Path::new("/w/c"), "c");
		assert_eq!(c, 2);
		assert_eq!(slot_range(&files, 0), 0..1);
		assert_eq!(slot_range(&files, 1), 1..3);
		assert_eq!(slot_range(&files, 2), 3..3);
		assert_eq!(
			slot_insert(&mut slots, &mut files, Path::new("/w/b"), "b"),
			1
		);

		// Replacing one repo's rows leaves the others alone.
		slot_replace_rows(
			&mut files,
			1,
			vec![change("z", SourceKind::Working, 1)],
		);
		assert_eq!(files.len(), 2);
		assert_eq!(files[1].path, "z");

		slot_remove(&mut slots, &mut files, 0);
		assert_eq!(files.len(), 1);
		assert_eq!(files[0].repo, 0);
	}

	/// Three repos: `a` staged + unstaged, `b` staged + untracked, `c`
	/// clean.
	fn three_repos() -> (Vec<ChangeRepo>, Vec<FileChangeItem>) {
		use std::path::Path;
		let mut slots = Vec::new();
		let mut files = Vec::new();
		for name in ["a", "b", "c"] {
			let root = Path::new("/w").join(name);
			slot_insert(&mut slots, &mut files, &root, name);
		}
		slot_replace_rows(
			&mut files,
			0,
			vec![
				change("s.txt", SourceKind::Staged, 0),
				change("u.txt", SourceKind::Unstaged, 0),
			],
		);
		slot_replace_rows(
			&mut files,
			1,
			vec![
				change("s.txt", SourceKind::Staged, 1),
				change("w.txt", SourceKind::Working, 1),
			],
		);
		(slots, files)
	}

	/// Compact form of Changes rows: `G:<group>:<count>`,
	/// `R:<group>:<repo>:<count>`, `N:<repo>`, `D<depth>:<repo>:<name>:<count>`
	/// and `F<depth>:<repo>:<path>`.
	fn shape(
		slots: &[ChangeRepo],
		files: &[FileChangeItem],
		rows: &[ui::ChangeItemRow],
	) -> Vec<String> {
		rows.iter()
			.map(|r| match r {
				ui::ChangeItemRow::Header {
					group_id, count, ..
				} => format!("G:{group_id}:{count}"),
				ui::ChangeItemRow::Repo {
					slot,
					group_id,
					count,
				} => format!("R:{group_id}:{}:{count}", slots[*slot].name),
				ui::ChangeItemRow::Note { slot } => {
					format!("N:{}", slots[*slot].name)
				}
				ui::ChangeItemRow::Unreadable { count } => format!("U:{count}"),
				ui::ChangeItemRow::Dir {
					slot,
					name,
					count,
					depth,
					..
				} => format!("D{depth}:{}:{name}:{count}", slots[*slot].name),
				ui::ChangeItemRow::File { file_idx, depth } => {
					let f = &files[*file_idx];
					format!(
						"F{depth}:{}:{}",
						slots[f.repo as usize].name, f.path
					)
				}
			})
			.collect()
	}

	type Expanded = fn(usize, &str, &str) -> bool;

	/// Flat file lists under each repo.
	const FLAT: ui::ChangeLayout<Expanded> = ui::ChangeLayout {
		by_dir: false,
		expanded: |_, _, _| false,
	};

	#[test]
	fn change_rows_put_groups_over_repos() {
		let (mut slots, files) = three_repos();
		// Groups first (staged, unstaged), each over its repos in name
		// order; repo rows start collapsed; clean `c` is absent. Untracked
		// `w.txt` counts under Unstaged.
		let rows =
			ui::change_rows(&slots, &files, |_| false, |_, _| true, "", &FLAT);
		assert_eq!(
			shape(&slots, &files, &rows),
			[
				"G:staged:2",
				"R:staged:a:1",
				"R:staged:b:1",
				"G:unstaged:2",
				"R:unstaged:a:1",
				"R:unstaged:b:1",
			]
		);

		// Opening `b` expands it in every group, and only `b`.
		let mut open = Vec::new();
		expand_repo_everywhere(&mut open, &slots[1].root);
		let roots: Vec<PathBuf> =
			slots.iter().map(|s| s.root.clone()).collect();
		let collapsed = |slot: usize, g: &str| {
			!open.iter().any(|(og, r)| *og == g && *r == roots[slot])
		};
		let rows =
			ui::change_rows(&slots, &files, |_| false, collapsed, "", &FLAT);
		assert_eq!(
			shape(&slots, &files, &rows),
			[
				"G:staged:2",
				"R:staged:a:1",
				"R:staged:b:1",
				"F2:b:s.txt",
				"G:unstaged:2",
				"R:unstaged:a:1",
				"R:unstaged:b:1",
				"F2:b:w.txt",
			]
		);

		// A collapsed group hides its repo rows; the others stay.
		let rows = ui::change_rows(
			&slots,
			&files,
			|g| g == "staged",
			|_, _| true,
			"",
			&FLAT,
		);
		assert_eq!(
			shape(&slots, &files, &rows)[..2],
			["G:staged:2", "G:unstaged:2"]
		);

		// Speed search keeps repos whose name matches, ignoring case; a
		// group keeps its full count.
		let rows =
			ui::change_rows(&slots, &files, |_| false, |_, _| true, "B", &FLAT);
		assert_eq!(
			shape(&slots, &files, &rows),
			[
				"G:staged:2",
				"R:staged:b:1",
				"G:unstaged:2",
				"R:unstaged:b:1"
			]
		);

		// A failed repo with no rows gets a top-level note; a truncated
		// list says so under each of its expanded repo rows.
		slots[2].state = ChangeRepoState::Failed(Msg::new(
			"error_repo_status",
			["boom".to_string()],
		));
		slots[1].total = MAX_CHANGES_PER_REPO + 1;
		let rows =
			ui::change_rows(&slots, &files, |_| false, collapsed, "", &FLAT);
		let got = shape(&slots, &files, &rows);
		assert_eq!(got[0], "N:c");
		assert_eq!(got[3..6], ["R:staged:b:1", "N:b", "F2:b:s.txt"]);
		assert_eq!(got.iter().filter(|r| *r == "N:b").count(), 2);
	}

	#[test]
	fn change_rows_fold_many_unreadable_repos_into_one_node() {
		use std::path::Path;
		let (mut slots, mut files) = three_repos();
		for name in ["d", "e"] {
			slot_insert(
				&mut slots,
				&mut files,
				&Path::new("/w").join(name),
				name,
			);
		}
		for slot in [2, 3, 4] {
			slots[slot].state = ChangeRepoState::Failed(Msg::new(
				"error_repo_status",
				["boom".to_string()],
			));
		}
		// Three unreadable repos: one node after the changes, collapsed
		// until opened, then one note per repo under it.
		let folded = |g: &str| g == ui::UNREADABLE;
		let rows =
			ui::change_rows(&slots, &files, folded, |_, _| true, "", &FLAT);
		let got = shape(&slots, &files, &rows);
		assert_eq!(got[0], "G:staged:2");
		assert_eq!(got.last().map(String::as_str), Some("U:3"));
		assert!(!got.iter().any(|r| r.starts_with("N:")), "{got:?}");
		let rows =
			ui::change_rows(&slots, &files, |_| false, |_, _| true, "", &FLAT);
		let got = shape(&slots, &files, &rows);
		assert_eq!(got[got.len() - 4..], ["U:3", "N:c", "N:d", "N:e"]);

		// One unreadable repo keeps its note at the top, with no node.
		for slot in [3, 4] {
			slots[slot].state = ChangeRepoState::Loaded;
		}
		let rows =
			ui::change_rows(&slots, &files, folded, |_, _| true, "", &FLAT);
		let got = shape(&slots, &files, &rows);
		assert_eq!(got[0], "N:c");
		assert!(!got.iter().any(|r| r.starts_with("U:")), "{got:?}");
	}

	#[test]
	fn change_rows_keep_a_single_repo_flat() {
		let (mut slots, mut files) = three_repos();
		slot_remove(&mut slots, &mut files, 2);
		slot_remove(&mut slots, &mut files, 1);
		let rows =
			ui::change_rows(&slots, &files, |_| false, |_, _| true, "x", &FLAT);
		assert_eq!(
			shape(&slots, &files, &rows),
			["G:staged:1", "F1:a:s.txt", "G:unstaged:1", "F1:a:u.txt"]
		);
	}

	/// One repo `r` with a nested tree: `src/main/java/pkg` is a chain of
	/// single-child directories, `src` has two children, and an untracked
	/// folder `Notes/` arrives as a leaf.
	fn nested_repo() -> (Vec<ChangeRepo>, Vec<FileChangeItem>) {
		let mut slots = Vec::new();
		let mut files = Vec::new();
		slot_insert(&mut slots, &mut files, std::path::Path::new("/w/r"), "r");
		slot_replace_rows(
			&mut files,
			0,
			vec![
				change("src/main/java/pkg/B.java", SourceKind::Unstaged, 0),
				change("pom.xml", SourceKind::Unstaged, 0),
				change("src/test/T.java", SourceKind::Working, 0),
				change("src/main/java/pkg/a.java", SourceKind::Unstaged, 0),
				change("AGENTS.md", SourceKind::Working, 0),
				change("docs/x/y.md", SourceKind::Staged, 0),
				change("Notes/", SourceKind::Working, 0),
			],
		);
		(slots, files)
	}

	#[test]
	fn change_rows_group_by_directory() {
		let (slots, files) = nested_repo();
		let open = |dirs: &'static [&'static str]| ui::ChangeLayout {
			by_dir: true,
			expanded: move |_: usize, g: &str, d: &str| {
				g == "unstaged" && dirs.contains(&d)
			},
		};
		// Directories start collapsed: the group shows its top-level
		// directories (first, with the count of every file beneath) and
		// then its root files, each by name ignoring case. Untracked files
		// sit in Unstaged; the untracked folder `Notes/` is a leaf.
		let rows = ui::change_rows(
			&slots,
			&files,
			|_| false,
			|_, _| false,
			"",
			&open(&[]),
		);
		assert_eq!(
			shape(&slots, &files, &rows),
			[
				"G:staged:1",
				"D1:r:docs/x:1",
				"G:unstaged:6",
				"D1:r:src:3",
				"F1:r:AGENTS.md",
				"F1:r:Notes/",
				"F1:r:pom.xml",
			]
		);

		// Expanding by (group, dir path): `src` shows its children, and the
		// single-child chain under `main` is one row keyed by its full path.
		let rows = ui::change_rows(
			&slots,
			&files,
			|_| false,
			|_, _| false,
			"",
			&open(&["src", "src/main/java/pkg"]),
		);
		assert_eq!(
			shape(&slots, &files, &rows)[2..],
			[
				"G:unstaged:6",
				"D1:r:src:3",
				"D2:r:main/java/pkg:2",
				"F3:r:src/main/java/pkg/a.java",
				"F3:r:src/main/java/pkg/B.java",
				"D2:r:test:1",
				"F1:r:AGENTS.md",
				"F1:r:Notes/",
				"F1:r:pom.xml",
			]
		);
		let dir_path = rows.iter().find_map(|r| match r {
			ui::ChangeItemRow::Dir { name, path, .. }
				if name == "main/java/pkg" =>
			{
				Some(path.clone())
			}
			_ => None,
		});
		assert_eq!(dir_path.as_deref(), Some("src/main/java/pkg"));
		// Expansion is per group: `docs/x` stays closed in Staged.
		assert_eq!(
			shape(&slots, &files, &rows)[..2],
			["G:staged:1", "D1:r:docs/x:1"]
		);
		// An open child under a closed parent stays hidden.
		let rows = ui::change_rows(
			&slots,
			&files,
			|_| false,
			|_, _| false,
			"",
			&open(&["src/main/java/pkg"]),
		);
		assert!(!shape(&slots, &files, &rows)
			.iter()
			.any(|r| r.contains("pkg")));

		// Flat mode lists the files in Git's order under the group.
		let rows =
			ui::change_rows(&slots, &files, |_| false, |_, _| false, "", &FLAT);
		assert_eq!(
			shape(&slots, &files, &rows)[2..],
			[
				"G:unstaged:6",
				"F1:r:src/main/java/pkg/B.java",
				"F1:r:pom.xml",
				"F1:r:src/test/T.java",
				"F1:r:src/main/java/pkg/a.java",
				"F1:r:AGENTS.md",
				"F1:r:Notes/",
			]
		);
	}

	#[test]
	fn change_rows_nest_directories_under_repo_rows() {
		let (mut slots, mut files) = nested_repo();
		slot_insert(&mut slots, &mut files, std::path::Path::new("/w/s"), "s");
		slot_replace_rows(
			&mut files,
			1,
			vec![change("lib/z.rs", SourceKind::Unstaged, 1)],
		);
		let layout = ui::ChangeLayout {
			by_dir: true,
			expanded: |slot: usize, _: &str, d: &str| slot == 0 && d == "src",
		};
		let rows = ui::change_rows(
			&slots,
			&files,
			|_| false,
			|slot, g| slot == 1 || g == "staged",
			"",
			&layout,
		);
		// Under a repo row the tree starts one level deeper, and a
		// directory's expansion is per repo.
		assert_eq!(
			shape(&slots, &files, &rows),
			[
				"G:staged:1",
				"R:staged:r:1",
				"G:unstaged:7",
				"R:unstaged:r:6",
				"D2:r:src:3",
				"D3:r:main/java/pkg:2",
				"D3:r:test:1",
				"F2:r:AGENTS.md",
				"F2:r:Notes/",
				"F2:r:pom.xml",
				"R:unstaged:s:1",
			]
		);
	}

	#[test]
	fn commit_copy_toast_reports_counts_and_where_files_were_left_out() {
		use snip_core::commits::{
			CommitExport, CommitFile, CommitRecord, CommitsPayload, FileChange,
			NotCopiedReason,
		};
		let file =
			|path: &str, not_copied: Option<NotCopiedReason>| CommitFile {
				path: path.into(),
				old_path: None,
				change: FileChange::Added,
				content: not_copied.is_none().then(|| "body".into()),
				not_copied,
			};
		let record = |files| CommitRecord {
			message: "m".into(),
			author_name: "a".into(),
			author_email: "a@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files,
		};
		let export = |payload: CommitsPayload| CommitExport {
			text: snip_core::commits::to_clipboard_text(&payload),
			payload,
		};
		let bin = Some(NotCopiedReason::Binary);
		let clean = export(CommitsPayload {
			commits: vec![record(vec![file("a.txt", None)]), record(vec![])],
		});
		let chars = clean.text.encode_utf16().count().to_string();
		let msg = commit_copied_status(&clean.outcome());
		assert_eq!(msg.key, "status_commits_copied");
		assert_eq!(msg.args, ["2", "1", chars.as_str()]);

		let lossy = export(CommitsPayload {
			commits: vec![
				record(vec![file("a.txt", None), file("x.bin", bin)]),
				record(vec![file("y.bin", bin), file("z.bin", bin)]),
				record(vec![file("w.bin", bin)]),
			],
		});
		let msg = commit_copied_status(&lossy.outcome());
		assert_eq!(msg.key, "status_commits_copied_skipped");
		assert_eq!(&msg.args[..2], ["3", "5"]);
		assert_eq!(&msg.args[3..], ["4", "#1 x.bin; #2 y.bin, z.bin …"]);
		let text = msg.render(crate::i18n::Locale::ZhTw);
		assert!(
			text.contains("3 個 commit") && text.contains("4 個檔案未複製")
		);
	}

	/// The Git Log's changed-files pane uses the Changes tree: dirs first,
	/// single-child chains compacted, every directory open unless
	/// collapsed; flat mode keeps Git's order at depth 0.
	#[test]
	fn commit_file_rows_tree_and_flat() {
		let files: Vec<(String, Option<ChangeType>)> = [
			"src/main/java/pkg/B.java",
			"README.md",
			"src/main/java/pkg/a.java",
			"src/test/T.java",
			"docs/guide.md",
		]
		.iter()
		.map(|p| (p.to_string(), Some(ChangeType::Modified)))
		.collect();
		let shape = |rows: &[ui::ChangeItemRow]| -> Vec<String> {
			rows.iter()
				.map(|r| match r {
					ui::ChangeItemRow::Dir {
						name, count, depth, ..
					} => format!("D{depth}:{name}:{count}"),
					ui::ChangeItemRow::File { file_idx, depth } => {
						format!("F{depth}:{}", files[*file_idx].0)
					}
					_ => "?".into(),
				})
				.collect()
		};
		assert_eq!(
			shape(&ui::commit_file_rows(&files, true, &[])),
			[
				"D0:docs:1",
				"F1:docs/guide.md",
				"D0:src:3",
				"D1:main/java/pkg:2",
				"F2:src/main/java/pkg/a.java",
				"F2:src/main/java/pkg/B.java",
				"D1:test:1",
				"F2:src/test/T.java",
				"F0:README.md",
			]
		);
		// A collapsed directory hides what is under it, keyed by full path.
		let closed = ["src/main/java/pkg".to_string(), "docs".to_string()];
		assert_eq!(
			shape(&ui::commit_file_rows(&files, true, &closed)),
			[
				"D0:docs:1",
				"D0:src:3",
				"D1:main/java/pkg:2",
				"D1:test:1",
				"F2:src/test/T.java",
				"F0:README.md",
			]
		);
		// Flat rows are in natural path order, ignoring case.
		assert_eq!(
			shape(&ui::commit_file_rows(&files, false, &closed)),
			[
				"F0:docs/guide.md",
				"F0:README.md",
				"F0:src/main/java/pkg/a.java",
				"F0:src/main/java/pkg/B.java",
				"F0:src/test/T.java",
			]
		);
	}
}
