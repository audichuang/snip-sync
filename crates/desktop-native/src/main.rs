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

/// Test-harness event line.
macro_rules! app_log {
	($($arg:tt)*) => {{
		println!($($arg)*);
		let _ = std::io::Write::flush(&mut std::io::stdout());
	}};
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

/// The copy toast: a partial copy (file limit hit in the folder walk or
/// in the plan) always says so, with the limit. Local and remote copies
/// report the same [`snip_core::transfer::CopyOutcome`] shape.
fn copied_status(
	repo_name: String,
	out: &snip_core::transfer::CopyOutcome,
) -> Msg {
	let mut args = vec![
		repo_name,
		out.copied.to_string(),
		out.chars.to_string(),
		out.lines.to_string(),
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

mod githost;
pub mod graph_view;
mod history;
pub mod i18n;
mod icons;
pub mod lifecycle;
mod menu;
mod multi_log;
pub mod paste;
mod reader;
mod recent;
pub mod remote;
mod selector;
pub mod syntax;
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
		CancelPaste,
		Refresh,
		NavUp,
		NavDown,
		NavToggle,
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
		ReaderClear,
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
		// IntelliJ chrome (IJ-2c).
		NextDiff,
		PrevDiff,
		MenuUp,
		MenuDown,
		MenuConfirm,
		MenuCancel,
		LogParent,
		LogChild,
		LogPageUp,
		LogPageDown,
		ToolPageUp,
		ToolPageDown,
		HideToolWindow,
		FocusEditor,
		OpenTabMenu,
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
	Failed(String),
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
) -> Result<(Option<RepoSummary>, Vec<WorkingChangeTuple>, usize), String> {
	let read = snip_core::gitview::Read {
		profile: snip_core::gitview::ReadProfile::Interactive,
		cancel: Some(cancel),
	};
	let repo = host.open(root, known, &read)?;
	let list = repo
		.change_list(MAX_CHANGES_PER_REPO, &read)
		.map_err(|e| e.to_string())?;
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
	pub summary: Result<RepoSummary, String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Popover {
	Repo,
	Ref,
}

pub struct WorkbenchModel {
	pub workspace_root: PathBuf,
	pub restore_dir: Option<PathBuf>,
	pub repos: Vec<RepoEntry>,
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
	pub history_error: Option<String>,
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
	pub log_menu: Option<ui::LogMenu>,
	/// The menu a mouse-down outside it just closed, and when.
	pub log_menu_dismissed: Option<(ui::LogMenu, std::time::Instant)>,
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
	/// Collapsed directories of the changed-files tree.
	pub changed_dirs_collapsed: Vec<String>,
	/// The changed-files pane groups by directory (else a flat list).
	pub log_details_by_dir: bool,
	/// The changed-files rows, keyed on a hash of what builds them: the
	/// tree sorts up to `MAX_COMMIT_FILES` paths, too slow for every frame.
	pub commit_rows_cache: std::cell::RefCell<
		Option<(u64, std::rc::Rc<Vec<crate::ui::ChangeItemRow>>)>,
	>,
	pub commit_details: Option<crate::history::CommitDetails>,
	/// Details pane height under the changed files once dragged; `None`
	/// keeps the default share (the files get most of the pane).
	pub log_details_h: Option<f32>,
	/// The multi-selection's commit list is open (collapsed by default).
	pub log_selection_expanded: bool,
	/// Details of an open multi-selection's commits, newest first.
	pub selection_details: Vec<crate::history::CommitDetails>,
	/// Commits whose details list every containing branch read.
	pub log_branches_all: Vec<String>,
	pub details_generation: u64,
	pub details_cancel: Option<CancelToken>,
	pub git_user_email: Option<String>,

	// Files of the selected commit or compare.
	pub commit_files: Vec<(String, Option<ChangeType>)>,
	/// Per file of a multi-selection: index in `log_selected` of the newest
	/// selected commit that touched it.
	pub commit_file_origin: Vec<u32>,
	/// Listed files that are submodule commits (no content to copy).
	pub commit_file_gitlinks: Vec<String>,
	/// The listing was cut to `MAX_COMMIT_FILES`.
	pub commit_files_truncated: bool,
	pub selected_commit_file: Option<String>,
	/// Cmd/Shift multi-selection in the changed files; empty means just
	/// `selected_commit_file`. Cleared with every change of that one.
	pub commit_file_sel: Vec<String>,
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
	pub preview_error: Option<Msg>,
	pub reader: Reader,
	pub find_input: Entity<TextInput>,
	pub goto_input: Entity<TextInput>,
	pub log_search_input: Entity<TextInput>,
	pub selector_input: Entity<TextInput>,
	pub popover: Option<Popover>,
	pub popover_cursor: usize,

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
	pub discovery: Option<Discovery>,
	pub discovery_status: Option<ScanStatus>,
	pub discovery_errors: Vec<(PathBuf, String)>,
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
	pub is_adding_repo: bool,
	pub add_repo_input: Entity<TextInput>,
	pub paste: paste::preview::PastePreview,
	pub status: Msg,
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
	pub e2e_read_delay: Option<std::time::Duration>,
	/// Test-only hold file for project-tree reads, honoured only in E2E mode.
	pub e2e_tree_hold: Option<PathBuf>,
	/// Test-only hold file for copy export revalidation, honoured only in E2E mode.
	pub e2e_export_hold: Option<PathBuf>,
	pub workspace_open: bool,
	pub workspace_menu: bool,
	/// Where the workspace menu button was drawn: a press there toggles the
	/// menu, so the menu's outside-press close must leave it alone.
	pub workspace_menu_button:
		std::rc::Rc<std::cell::Cell<Option<Bounds<gpui::Pixels>>>>,
	pub workspace_picker: bool,
	pub workspace_path_input: Entity<TextInput>,
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
	/// Repo the shown preview was read from, with `preview_identity`.
	pub preview_root: Option<(PathBuf, usize)>,
	/// Repo of the failed preview load when `preview_error` is Some.
	pub preview_error_root: Option<PathBuf>,
	/// Repositories the log's Repository chip picked; empty is all of them.
	pub log_repo_filter: Vec<PathBuf>,
	/// Repositories the loaded log covers (two or more: the merged log).
	pub log_scope_key: Vec<PathBuf>,
	/// The merged log's per-repository feeds.
	pub log_feeds: Vec<multi_log::Feed>,
	/// Repository of the commit or compare the reader shows from the log.
	pub log_commit_root: Option<PathBuf>,
	/// A Replace load waited for discovery to reach the picked repositories.
	pub log_deferred: bool,
	/// Remote workspaces: ssh hosts and an open remote workspace.
	pub remote: remote::MasterState,
	pub remote_path_input: Entity<TextInput>,
}

/// Identity of a shown preview's text, as `reader.rs` compares it.
pub(crate) fn preview_identity(p: &Preview) -> usize {
	Arc::as_ptr(&p.text) as *const u8 as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Splitter {
	Left,
	Bottom,
	LogDetails,
	/// Between the log's changed files and the commit details.
	LogFiles,
}

impl WorkbenchModel {
	pub fn new(
		workspace: Option<PathBuf>,
		restore_dir: Option<PathBuf>,
		mode: String,
		cx: &mut Context<Self>,
	) -> Self {
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
				InputEvent::Dismiss => this.dismiss_log_menu(cx),
				// The picker filters by the text as it is typed.
				InputEvent::Changed => cx.notify(),
				_ => {}
			}
		})
		.detach();
		let log_branch_menu_input = cx.new(|cx| {
			TextInput::new(i18n::t("log_branch_placeholder", loc), 0, cx)
		});
		cx.subscribe(&log_branch_menu_input, |this, _, ev: &InputEvent, cx| {
			match ev {
				InputEvent::Dismiss => this.dismiss_log_menu(cx),
				InputEvent::Changed => cx.notify(),
				_ => {}
			}
		})
		.detach();
		let date_input = |key: &'static str, cx: &mut Context<Self>| {
			let input = cx.new(|cx| TextInput::new(i18n::t(key, loc), 0, cx));
			cx.subscribe(&input, |this, _, ev: &InputEvent, cx| match ev {
				InputEvent::Submit => this.apply_log_date_range(cx),
				InputEvent::Dismiss => this.close_log_menu(cx),
				_ => {}
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
				InputEvent::Dismiss => this.close_find(cx),
			}
		})
		.detach();
		cx.subscribe(
			&goto_input,
			|this, input, ev: &InputEvent, cx| match ev {
				InputEvent::Submit => {
					let q = input.read(cx).text().to_string();
					this.goto_line(&q, cx);
					this.pending_focus = Some(this.reader_focus.clone());
				}
				InputEvent::Dismiss => this.close_find(cx),
				_ => {}
			},
		)
		.detach();
		cx.subscribe(&log_search_input, |this, input, ev: &InputEvent, cx| {
			match ev {
				InputEvent::Submit => {
					let q = input.read(cx).text().trim().to_string();
					this.start_log_search(q, cx);
				}
				InputEvent::Dismiss => {
					input.update(cx, |i, cx| i.set_text("", cx));
					this.start_log_search(String::new(), cx);
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
			match ev {
				InputEvent::Submit => {
					let text = input.read(cx).text().trim().to_string();
					if !text.is_empty() {
						this.add_repo_path(PathBuf::from(text), cx);
						input.update(cx, |i, cx| i.set_text("", cx));
						this.is_adding_repo = false;
					}
				}
				InputEvent::Dismiss => {
					this.is_adding_repo = false;
					cx.notify();
				}
				_ => {}
			}
		})
		.detach();

		let workspace_path_input = cx.new(|cx| {
			TextInput::new(i18n::t("workspace_path_placeholder", loc), 70, cx)
		});
		cx.subscribe(
			&workspace_path_input,
			|this, input, ev: &InputEvent, cx| match ev {
				InputEvent::Submit => {
					let text = input.read(cx).text().trim().to_string();
					this.confirm_open_workspace(&text, cx);
				}
				InputEvent::Dismiss => this.close_workspace_menu(cx),
				_ => {}
			},
		)
		.detach();
		let remote_path_input = cx.new(|cx| {
			TextInput::new(i18n::t("remote_path_placeholder", loc), 71, cx)
		});
		cx.subscribe(
			&remote_path_input,
			|this, _, ev: &InputEvent, cx| match ev {
				InputEvent::Submit => this.open_remote_typed(cx),
				InputEvent::Dismiss => this.close_workspace_menu(cx),
				_ => {}
			},
		)
		.detach();
		cx.on_release(|this, _| {
			this.lifecycle.cancel_cancellable();
			if e2e_on() {
				app_log!(
					"[APP:LIFECYCLE: phase=released reason=on_release jobs={}]",
					this.lifecycle.unfinished()
				);
			}
		})
		.detach();

		let mut model = Self {
			workspace_root: workspace.clone().unwrap_or_default(),
			restore_dir,
			repos: Vec::new(),
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
			log_menu: None,
			log_menu_dismissed: None,
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
			changed_dirs_collapsed: Vec::new(),
			log_details_by_dir: true,
			commit_rows_cache: Default::default(),
			commit_details: None,
			log_details_h: None,
			log_selection_expanded: false,
			selection_details: Vec::new(),
			log_branches_all: Vec::new(),
			details_generation: 0,
			details_cancel: None,
			git_user_email: None,
			commit_files: Vec::new(),
			commit_file_origin: Vec::new(),
			commit_file_gitlinks: Vec::new(),
			commit_files_truncated: false,
			selected_commit_file: None,
			commit_file_sel: Vec::new(),
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
			preview_error: None,
			reader: Reader::default(),
			find_input,
			goto_input,
			log_search_input,
			selector_input,
			add_repo_input,
			popover: None,
			popover_cursor: 0,
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
			is_adding_repo: false,
			paste: paste::preview::PastePreview::new(ui::e2e_apply_delay()),
			status: if workspace.is_some() {
				Msg::new("status_scanning", [])
			} else {
				Msg::new("workspace_closed", [])
			},
			toast: None,
			toast_seq: 0,
			is_loading: workspace.is_some(),
			is_copying: false,
			locale: loc,
			generation: 0,
			preview_generation: 0,
			history_generation: 0,
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
			left_w: theme::LEFT_W_DEFAULT,
			bottom_h: theme::BOTTOM_H_DEFAULT,
			left_visible: true,
			bottom_visible: true,
			dragging: None,
			last_viewport: (0, 0),
			viewport_h: 0.,
			probes: ui::Probes::from_env(),
			pending_focus: None,
			e2e_read_delay: ui::e2e_read_delay(),
			e2e_tree_hold: ui::e2e_tree_hold(),
			e2e_export_hold: ui::e2e_export_hold(),
			workspace_open: workspace.is_some(),
			workspace_menu: false,
			workspace_menu_button: Default::default(),
			workspace_picker: false,
			workspace_path_input,
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
			preview_root: None,
			preview_error_root: None,
			log_repo_filter: Vec::new(),
			log_scope_key: Vec::new(),
			log_feeds: Vec::new(),
			log_commit_root: None,
			log_deferred: false,
			remote: remote::MasterState {
				hosts: remote::load_hosts(),
				recent: remote::load_recent(),
				..Default::default()
			},
			remote_path_input,
		};
		if let Some(path) = workspace {
			recent::remember(&mut model.recent_workspaces, &path);
			remote::remember_last(None);
			model.reload_repos(cx);
		}
		model
	}

	/// The repo the shown preview came from: a log commit's own repo, the
	/// Changes row's repo, else the open repo.
	pub fn preview_root(&self) -> Option<PathBuf> {
		if self.preview.is_none() && self.preview_error.is_some() {
			if let Some(root) = &self.preview_error_root {
				return Some(root.clone());
			}
		}
		match (&self.preview_root, &self.preview) {
			(_, Some(p))
				if matches!(
					p.source,
					PreviewSource::CommitDiff { .. }
						| PreviewSource::Compare { .. }
				) =>
			{
				self.log_commit_root.clone().or_else(|| self.repo_root())
			}
			(Some((root, id)), Some(p)) if *id == preview_identity(p) => {
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
			(Some(p), _) => self
				.preview_root
				.as_ref()
				.filter(|(_, id)| *id == preview_identity(p))
				.map(|(root, _)| root.clone()),
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
			Err(e) => format!("{}\n{}", path, e),
		}
	}

	/// The open repo's Changes slot.
	pub fn selected_change_slot(&self) -> Option<usize> {
		let root = self.repo_root()?;
		self.change_repos.iter().position(|s| s.root == root)
	}

	pub(crate) fn discovery_error_msg(&self) -> Msg {
		if let Some((_, err)) = self.discovery_errors.first() {
			Msg::new("error_repo_status", [err.clone()])
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
				return Some(LogEmpty::Failed(Msg::new(
					"error_history",
					[err.clone()],
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

	fn ensure_change_slot(
		&mut self,
		root: &std::path::Path,
		name: &str,
	) -> usize {
		slot_insert(&mut self.change_repos, &mut self.files, root, name)
	}

	/// Installs one repo's status read into its slot: rows past
	/// `MAX_CHANGES_PER_REPO` are dropped (the node says so), and a failure
	/// leaves an error row instead of rows.
	fn install_slot_changes(
		&mut self,
		slot: usize,
		result: Result<(Vec<WorkingChangeTuple>, usize), String>,
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
			self.spawn_owned(
				cx,
				lifecycle::JobKind::CancellableRead,
				Some(cancel.clone()),
				async move {
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
				},
			);
		}
	}

	fn land_queued_changes(
		&mut self,
		root: &std::path::Path,
		result: Result<
			(Option<RepoSummary>, Vec<WorkingChangeTuple>, usize),
			String,
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
				if let Some(entry) =
					self.repos.iter_mut().find(|r| r.root == root)
				{
					entry.summary = Ok(summary);
				}
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

	pub fn set_status(
		&mut self,
		key: &'static str,
		args: impl crate::i18n::IntoMsgArgs,
	) {
		self.status = Msg::new(key, args);
	}

	/// Shows `msg` in a card over the window for a few seconds, longer for a
	/// failure, which may list paths to read (a later toast replaces it and
	/// restarts the clock).
	pub fn show_toast(&mut self, ok: bool, msg: Msg, cx: &mut Context<Self>) {
		self.toast_seq = self.toast_seq.wrapping_add(1);
		let id = self.toast_seq;
		app_log!("[APP:TOAST: ok={ok}]");
		self.toast = Some((id, ok, msg));
		cx.spawn(async move |this, cx| {
			cx.background_executor()
				.timer(std::time::Duration::from_secs(if ok { 4 } else { 12 }))
				.await;
			let _ = this.update(cx, |model, cx| {
				if model.toast.as_ref().is_some_and(|t| t.0 == id) {
					model.toast = None;
					cx.notify();
				}
			});
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
			self.status = err;
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
		// The caller that read it names its repo again after installing.
		self.preview_root = None;
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
		self.preview = None;
		self.preview_root = None;
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
		] {
			input.update(cx, |i, _| i.set_placeholder(i18n::t(key, loc)));
		}
		app_log!("[APP:LOCALE: {:?}]", self.locale);
		cx.notify();
	}

	pub fn process_discovery_repos(
		discovered: Vec<DiscoveredRepo>,
		opts: &RunOptions,
	) -> (Vec<RepoEntry>, Vec<(PathBuf, String)>) {
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
		cx.spawn(async move |this, cx| loop {
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
		})
		.detach();
	}

	pub(crate) fn spawn_owned(
		&mut self,
		cx: &mut Context<Self>,
		kind: lifecycle::JobKind,
		cancel: Option<CancelToken>,
		fut: impl std::future::Future<Output = ()> + 'static,
	) -> u64 {
		let (id, flag) = self.lifecycle.register(kind, cancel);
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

	/// Ctrl/Cmd-Q and the OS close button. Does not call `cx.quit`.
	pub fn begin_quit(&mut self, cx: &mut Context<Self>) {
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
			&& matches!(intent, lifecycle::Intent::CloseWorkspace)
		{
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
				self.generation = self.generation.wrapping_add(1);
				self.preview_generation =
					self.preview_generation.wrapping_add(1);
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

	fn poll_lifecycle(&mut self, cx: &mut Context<Self>) {
		let now = std::time::Instant::now();
		let git = lifecycle::GitLoad::current();
		match self.lifecycle.poll_at(now, git) {
			lifecycle::Step::Idle | lifecycle::Step::Draining => {
				if self.lifecycle.is_draining() {
					let intent = self.lifecycle.intent_name();
					self.emit_life("draining", intent, None);
				}
			}
			lifecycle::Step::Ready(intent) => {
				let git = lifecycle::GitLoad::current();
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
				self.set_status(key, []);
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
				if let (Some(idx), Some(_)) =
					(self.selected_repo_idx, self.repo())
				{
					if !self.changes_loaded {
						self.select_repo_internal(idx, true, cx);
						self.set_status(key, []);
					} else if self.commits.is_empty()
						&& self.history_error.is_none()
					{
						self.load_history(cx);
					}
				}
				self.start_changes_queue(true, cx);
				cx.notify();
			}
		}
	}

	fn finish_intent(
		&mut self,
		intent: lifecycle::Intent,
		cx: &mut Context<Self>,
	) {
		let git = lifecycle::GitLoad::current();
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
			lifecycle::Intent::Quit => cx.quit(),
			lifecycle::Intent::CloseWorkspace => self.finish_close(cx),
			lifecycle::Intent::OpenWorkspace(path) => {
				self.finish_open(path, cx)
			}
			lifecycle::Intent::OpenRemoteWorkspace(target) => {
				let (host, workspace) = *target;
				self.finish_open_remote(host, workspace, cx)
			}
		}
	}

	fn release_workspace_state(&mut self, cx: &mut Context<Self>) {
		release_vec(&mut self.repos);
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
		self.preview_root = None;
		self.preview_error_root = None;
		self.paste.invalidate_job();
		if !self.paste_busy() {
			self.paste.clear(self.preview.as_ref());
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
		release_vec(&mut self.manual_repos);
		self.add_cancel = None;
		self.is_loading = false;
		self.is_copying = false;
		self.is_adding_repo = false;
		self.popover = None;
		self.popover_cursor = 0;
		self.scan_cancel = None;
		self.copy_cancel = None;
		release_path(&mut self.workspace_root);
		self.remote.session = None;
		self.remote.scan_error = None;
		self.selected_file_root = None;
		self.clear_workspace_inputs(cx);
	}

	/// Drops everything that belongs to the open repo and stops its reads, so
	/// a late result cannot repopulate it.
	fn release_repo_state(&mut self) {
		self.generation = self.generation.wrapping_add(1);
		self.preview_generation = self.preview_generation.wrapping_add(1);
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
		release_vec(&mut self.commit_files);
		release_vec(&mut self.commit_file_origin);
		release_vec(&mut self.commit_file_gitlinks);
		release_vec(&mut self.selection_details);
		release_vec(&mut self.log_branches_all);
		self.commit_rows_cache.take();
		self.log_first_page = 0;
		self.history_extending = false;
		self.history_loaded = false;
		self.last_log_empty.set(None);
		self.history_autoload = true;
		release_vec(&mut self.log_on_head);
		self.log_filter = LogQuery::default();
		self.log_menu = None;
		release_vec(&mut self.changed_dirs_collapsed);
		self.log_date_error = false;
		release_vec(&mut self.log_paths_expanded);
		release_vec(&mut self.log_branch_menu_open);
		release_vec(&mut self.log_feeds);
		release_vec(&mut self.log_scope_key);
		self.log_commit_root = None;
		self.commit_details = None;
		self.details_generation = self.details_generation.wrapping_add(1);
		self.git_user_email = None;
		self.selected_commit_file = None;
		self.commit_file_sel.clear();
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
		for input in [
			self.find_input.clone(),
			self.goto_input.clone(),
			self.log_search_input.clone(),
			self.selector_input.clone(),
			self.add_repo_input.clone(),
			self.workspace_path_input.clone(),
			self.log_path_input.clone(),
			self.log_branch_menu_input.clone(),
			self.log_since_input.clone(),
			self.log_until_input.clone(),
			self.branch_filter_input.clone(),
		] {
			input.update(cx, |field, _| field.clear_retained());
		}
	}

	fn finish_close(&mut self, cx: &mut Context<Self>) {
		self.release_workspace_state(cx);
		self.workspace_open = false;
		self.workspace_menu = true;
		self.workspace_picker = false;
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
		remote::remember_last(None);
		self.workspace_open = true;
		self.workspace_menu = false;
		self.workspace_picker = false;
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

	/// Escape in one of its fields, or a press outside it.
	pub fn close_workspace_menu(&mut self, cx: &mut Context<Self>) {
		self.workspace_menu = false;
		self.workspace_picker = false;
		cx.notify();
	}

	pub fn toggle_workspace_menu(&mut self, cx: &mut Context<Self>) {
		self.workspace_menu = !self.workspace_menu;
		if !self.workspace_menu {
			self.workspace_picker = false;
		}
		cx.notify();
	}

	/// The OS folder dialog. Without one (Linux with no portal) the typed
	/// path field opens instead.
	pub fn open_folder_dialog(&mut self, cx: &mut Context<Self>) {
		self.workspace_menu = false;
		self.workspace_picker = false;
		cx.notify();
		let picked = cx.prompt_for_paths(PathPromptOptions {
			files: false,
			directories: true,
			multiple: false,
			prompt: Some(i18n::t("workspace_open_confirm", self.locale).into()),
		});
		cx.spawn(async move |this, cx| {
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
		.detach();
	}

	pub fn show_workspace_picker(&mut self, cx: &mut Context<Self>) {
		self.workspace_menu = true;
		self.workspace_picker = true;
		self.pending_focus = Some(self.workspace_path_input.read(cx).handle());
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
		self.request_user_close(lifecycle::Intent::OpenWorkspace(path), cx);
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
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel_job),
			async move {
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
			},
		);
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
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel_job),
			async move {
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
							model.discovery_status =
								Some(ScanStatus::Incomplete);
							model.push_discovery_error((root, err.to_string()));
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
			},
		);
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
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel_job),
			async move {
				drive_discovery(
					this,
					&mut async_app,
					disc,
					generation,
					cancel,
					wipe,
				)
				.await;
			},
		);
	}

	/// Replaces the list with the manual repos plus the open one, so the
	/// selection keeps naming the same repo while later pages arrive.
	fn begin_rescan(&mut self) {
		let open = self.pinned_repo.as_ref().and_then(|key| {
			let pos =
				self.repos.iter().position(|e| repo_key_matches(e, key))?;
			Some(self.repos.swap_remove(pos))
		});
		self.repos.clear();
		let mut keep = self.manual_repos.clone();
		keep.extend(open);
		self.merge_repo_entries(keep);
		// A manual repo is kept regardless of the walk.
		self.carried_repo = self.pinned_repo.clone().filter(|key| {
			!self.manual_repos.iter().any(|m| repo_key_matches(m, key))
		});
	}

	/// Adds or refreshes entries; a newer read of a known repo replaces it.
	fn merge_repo_entries(&mut self, extra: Vec<RepoEntry>) {
		for entry in extra {
			let key = repo_key(&entry);
			if self.carried_repo.as_ref() == Some(&key) {
				self.carried_repo = None;
			}
			match self
				.repos
				.iter()
				.position(|have| repo_key_matches(have, &key))
			{
				Some(pos) => self.repos[pos] = entry,
				None => self.repos.push(entry),
			}
		}
		Self::disambiguate_repo_names(&mut self.repos, &self.workspace_root);
		self.repos
			.sort_by(|a, b| a.name.cmp(&b.name).then(a.root.cmp(&b.root)));
	}

	fn push_discovery_error(&mut self, item: (PathBuf, String)) {
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
			if let Some(idx) = self.selected_repo_idx {
				self.repos[idx].summary = Ok(summary);
			}
		}
	}

	/// Removes a kept repo the rescan did not find. Returns its name when it
	/// was the open one, whose view is then released.
	fn drop_vanished_repo(
		&mut self,
		key: &(PathBuf, PathBuf),
	) -> Option<String> {
		let pos = self.repos.iter().position(|e| repo_key_matches(e, key))?;
		let gone = self.repos.remove(pos);
		// Its kept selection names files that can no longer be read.
		self.tree_selections.retain(|(root, _)| root != &gone.root);
		if self.pinned_repo.as_ref() == Some(key) {
			self.pinned_repo = None;
			self.selected_repo_idx = None;
			self.release_repo_state();
			return Some(gone.name);
		}
		self.selected_repo_idx = self.pinned_repo.as_ref().and_then(|pinned| {
			self.repos.iter().position(|e| repo_key_matches(e, pinned))
		});
		None
	}

	fn finish_discovery(&mut self, cx: &mut Context<Self>) {
		self.is_loading = false;
		if self.discovery_error_overflow > 0 {
			self.discovery_errors.push((
				self.workspace_root.clone(),
				format!(
					"{} more discovery errors omitted",
					self.discovery_error_overflow
				),
			));
			self.discovery_error_overflow = 0;
		}
		if self.discovery_depth_overflow > 0 {
			self.discovery_errors.push((
				self.workspace_root.clone(),
				format!(
					"{} more depth-limited directories omitted",
					self.discovery_depth_overflow
				),
			));
			self.discovery_depth_overflow = 0;
		}
		self.place_selection(cx);
		self.ensure_ws_tree(cx);
		// Only a complete walk proves the kept repo is gone; a capped or
		// partial one may simply not have reached it yet.
		let vanished =
			matches!(self.discovery_status, Some(ScanStatus::Complete))
				.then(|| self.carried_repo.take())
				.flatten()
				.and_then(|key| self.drop_vanished_repo(&key));
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
		self.sync_change_slots();
		self.start_changes_queue(false, cx);
		self.sync_log_scope(cx);
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
		outcome: Result<RepoEntry, String>,
		cx: &mut Context<Self>,
	) {
		let entry = match outcome {
			Ok(entry) => entry,
			Err(err) => {
				self.set_status("error_repo_status", [err]);
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
		self.repos.push(entry);
		Self::disambiguate_repo_names(&mut self.repos, &self.workspace_root);
		self.repos
			.sort_by(|a, b| a.name.cmp(&b.name).then(a.root.cmp(&b.root)));
		if let Some(pos) = self
			.repos
			.iter()
			.position(|have| repo_key_matches(have, &key))
		{
			self.pinned_repo = Some(key);
			self.select_repo_internal(pos, false, cx);
		}
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
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel.clone()),
			async move {
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
										result
											.key
											.utf8_rel()
											.unwrap_or_default()
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
			},
		);
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
		if reload_log {
			self.history_generation += 1;
			let _ = arm_cancel(&mut self.history_cancel);
		}
		self.tree_generation += 1;
		let task_generation = self.generation;
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
		self.commit_files.clear();
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
		if !preserve_anchors {
			self.commit_details = None;
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
				self.show_preview_error(Msg::new(
					"error_repo_status",
					[e.clone()],
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
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
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
							model.install_slot_changes(
								slot,
								Ok((changes, total)),
							);
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
												&f.path == anchor
													&& &f.source == *s
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
							model.set_status(
								"error_repo_changes",
								[repo_name.clone(), err.clone()],
							);
							if let (true, Some(anchor)) = (
								anchor_source.as_ref()
									== Some(&SourceKind::File),
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
								model.show_preview_error(Msg::new(
									"error_repo_changes",
									[repo_name.clone(), err],
								));
							}
						}
					}
					if reload_log {
						model.load_history(cx);
					} else {
						model.log_kept_on_repo_switch();
					}
					cx.notify();
				});
			},
		);
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
		if !self.accepting_work() {
			return;
		}
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		// Restored if the budget refuses the new text and the old one stays.
		let shown_selection = (
			self.selected_file.replace(path.to_string()),
			self.selected_file_source.replace(source.clone()),
			self.selected_file_root.clone(),
			self.selected_commit.take(),
		);
		self.range_head = None;
		self.log_selected.clear();
		self.compare = None;
		self.commit_files.clear();
		let Some(repo_root) = root.or_else(|| self.repo_root()) else {
			return;
		};
		let fs_only = matches!(source, SourceKind::File);
		if fs_only {
			self.selected_file_root = Some(repo_root.clone());
		} else {
			self.selected_file_root = None;
		}
		let shown_root = repo_root.clone();
		let file_path = path.to_string();
		self.preview_loading = true;
		self.preview_error = None;
		self.preview_error_root = None;

		let cancel = arm_cancel(&mut self.preview_cancel);
		let kind = if fs_only {
			lifecycle::JobKind::UncancellableRead
		} else {
			lifecycle::JobKind::CancellableRead
		};
		let job_cancel = (!fs_only).then(|| cancel.clone());
		let delay = self.e2e_read_delay;
		let remote = self.remote_target();
		let host = self.git_host();
		if e2e_on() {
			app_log!("[APP:PREVIEW_LOADING: {file_path}]");
		}

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();

		self.spawn_owned(cx, kind, job_cancel, async move {
			let for_bg = file_path.clone();
			let result = bg
				.spawn(async move {
					let result = match remote {
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
								None => {
									Err("not under the workspace".to_string())
								}
							}
						}
						_ => read_preview(
							&host, &repo_root, &for_bg, &source, cancel,
						),
					};
					if let Some(delay) = delay {
						std::thread::sleep(delay);
					}
					result
				})
				.await;

			let _ = this.update(&mut async_app, |model, cx| {
				if model.preview_generation != task_generation {
					if e2e_on() {
						app_log!(
							"[APP:PREVIEW_DISCARDED: stale path={file_path}]"
						);
					}
					return;
				}
				if model.apply_source_preview(file_path.clone(), result) {
					model.preview_root = model
						.preview
						.as_ref()
						.map(|p| (shown_root, preview_identity(p)));
					app_log!("[APP:PREVIEW_LOADED: {}]", file_path);
					if model.mode == "preview" {
						ready_marker("PREVIEW");
					}
				} else if model.preview.is_some()
					&& model.preview_error.is_none()
				{
					// Refused by the retained budget: the previous text is
					// still shown, so the selection must name it again.
					(
						model.selected_file,
						model.selected_file_source,
						model.selected_file_root,
						model.selected_commit,
					) = shown_selection;
				} else {
					model.preview_error_root = Some(shown_root);
				}
				cx.notify();
			});
		});
	}

	/// Turns a core `SourcePreview` into the single retained preview.
	pub fn apply_source_preview(
		&mut self,
		path: String,
		result: Result<(browser::SourcePreview, PreviewSource), String>,
	) -> bool {
		match result {
			Ok((p, source)) => {
				if !p.patch.is_empty() {
					self.set_preview(Preview::new(
						source,
						Some(path),
						p.patch,
						true,
						Language::Diff,
					))
				} else if let Some(content) = p.content {
					let lang = Language::from_path_or_ext(&path, false);
					self.set_preview(Preview::new(
						source,
						Some(path),
						content,
						false,
						lang,
					))
				} else {
					self.show_preview_error(Msg::new("error_binary", [path]));
					false
				}
			}
			Err(e) => {
				self.show_preview_error(Msg::new("error_preview", [path, e]));
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
					self.set_status("error_payload", [e.to_string()]);
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
			lifecycle::JobKind::CancellableRead,
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
							Msg::new("error_payload", [e.to_string()])
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
					self.set_status("status_clipboard_failed", [e.to_string()]);
				} else {
					app_log!("[APP:COPY_DONE: copied={copied_count}]");
					self.status = msg;
				}
			}
			Err(err) => self.status = err,
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
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(job_token),
			async move {
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
								Msg::new(
									"error_payload",
									[crate::remote::describe(e)],
								)
							})?;
						if out.copied == 0 {
							return Err(Msg::new("status_copy_nothing", []));
						}
						let msg = copied_status(repo_name, &out);
						Ok((out.payload, out.copied, msg))
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

		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(job_token),
			async move {
				let result: Result<(String, usize, Msg), String> = bg
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
								.map_err(crate::remote::describe)?;
							let status = commit_copied_status(&out);
							return Ok((out.text, out.commit_count, status));
						}
						let opts = interactive_read_opts(run_token);
						let git = Git::open_with(&repo_root, &opts)
							.map_err(|e| e.to_string())?;
						let exported = plan_commit_export_exact_with(
							&git,
							&tip_sha,
							&selected,
							&opts,
							snip_core::transfer::CLIPBOARD_PAYLOAD_MAX,
						)
						.map_err(|e| e.to_string())?;
						let out = exported.outcome();
						let status = commit_copied_status(&out);
						Ok((out.text, out.commit_count, status))
					})
					.await;

				let _ = this.update(&mut async_app, |model, cx| {
					if !model.accept_copy_result(ws_gen, &cancel) {
						cx.notify();
						return;
					}
					match result {
						Ok((text, n_commits, status)) => {
							if let Err(e) = clip::write_text(&text) {
								model.set_status(
									"status_clipboard_failed",
									[e.to_string()],
								);
							} else {
								app_log!(
									"[APP:COPY_COMMITS_DONE: commits={n_commits}]"
								);
								model.status = status;
							}
						}
						Err(err) => {
							app_log!("[APP:COPY_COMMITS_ERR: {err}]");
							model.set_status("error_payload", [err]);
						}
					}
					let ok = matches!(
						model.status.key,
						"status_commits_copied"
							| "status_commits_copied_skipped"
					);
					model.show_toast(ok, model.status.clone(), cx);
					cx.notify();
				});
			},
		);
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
		if let Err(err) = self.paste.enqueue(request) {
			self.status = err;
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
		match landed {
			Some(paste::preview::Landed::Shown { remap }) => {
				self.show_landed_plan(remap);
				cx.notify();
			}
			Some(paste::preview::Landed::Refused { err, closed }) => {
				app_log!("[APP:PASTE_ERR: {}]", err.key);
				self.status = err;
				if closed {
					self.restore_log_after_paste();
					self.pending_focus = Some(self.focus_handle.clone());
				}
				cx.notify();
			}
			Some(paste::preview::Landed::WorkerLost) => {
				self.status = Msg::new(
					"paste_err_plan",
					["Preview worker ended before producing a result".into()],
				);
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
				let id = self.spawn_owned(
					cx,
					lifecycle::JobKind::CancellableRead,
					Some(cancel.clone()),
					async move {
						bg.spawn(async move {
							work.run(&interactive_read_opts(token));
						})
						.await;
					},
				);
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

	fn show_landed_plan(&mut self, remap: Option<(String, bool)>) {
		// `Landed::Shown` guarantees an installed plan (see its doc).
		let plan = self.paste.plan().expect("Shown installs a plan");
		let items_count = plan.items.len();
		if let Some((prefix, keep)) = remap {
			let dest = prefix_target(plan, &prefix, keep);
			let keep_note = if keep { " keep=primary" } else { "" };
			app_log!(
				"[APP:PASTE_MAPPED: prefix={}{} dest={} items={}]",
				prefix,
				keep_note,
				dest,
				items_count
			);
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
			app_log!(
				"[APP:PASTE_PREVIEW: items={} dest={} mapping={}]",
				items_count,
				plan.shown(&plan.destination),
				plan.mapping_ready()
			);
		}
		self.set_status("status_paste_preview", [items_count.to_string()]);
		if self.paste.collapse_log(&mut self.bottom_visible) {
			app_log!("[APP:LOG_PANEL: visible=false reason=paste_open]");
		}
		self.pending_focus = Some(self.paste_focus.clone());
	}

	/// Keep full write/read diagnostics in status, but do not let a newly
	/// allocated diagnostic grow an already admitted plan past its tier.
	fn set_paste_error(&mut self, err: Msg) {
		self.status = err.clone();
		self.paste.record_error(err, self.preview.as_ref());
	}

	pub fn trigger_paste_preview(&mut self, cx: &mut Context<Self>) {
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
		let text = match clip::read_text() {
			Ok(t) => t,
			Err(e) => {
				app_log!("[APP:PASTE_ERR: clipboard]");
				self.restore_log_after_paste();
				self.set_status(
					"status_clipboard_read_failed",
					[e.to_string()],
				);
				self.pending_focus = Some(self.focus_handle.clone());
				cx.notify();
				return;
			}
		};
		if text.len() > paste::MAX_RETAINED_PREVIEW_BYTES {
			self.status = paste::preview_budget_error();
			self.restore_log_after_paste();
			self.pending_focus = Some(self.focus_handle.clone());
			app_log!("[APP:PASTE_ERR: preview_memory_limit]");
			cx.notify();
			return;
		}

		let target_dest = self.current_restore_destination();
		let known_roots: Vec<std::path::PathBuf> =
			self.repos.iter().map(|r| r.root.clone()).collect();
		self.pending_focus = Some(self.paste_focus.clone());
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
		match self.paste.remap(prefix, destination, self.preview.as_ref()) {
			paste::preview::Remapped::NoPlan => {}
			paste::preview::Remapped::Invalid(err) => {
				self.status = err;
				cx.notify();
			}
			paste::preview::Remapped::Dropped(err) => {
				self.status = err;
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
			paste::preview::Nav::Refused(err) => {
				self.status = err;
				app_log!("[APP:PASTE_DETAIL_REFUSED: reason=retained_budget]");
				cx.notify();
			}
		}
	}

	pub fn select_paste_item(&mut self, idx: usize, cx: &mut Context<Self>) {
		let nav = self.paste.select(idx, self.preview.as_ref());
		self.report_paste_nav(nav, cx);
	}

	pub fn step_paste_selection(
		&mut self,
		forward: bool,
		cx: &mut Context<Self>,
	) {
		let nav = self.paste.step(forward, self.preview.as_ref());
		self.report_paste_nav(nav, cx);
	}

	pub fn toggle_paste_commit(&mut self, c: usize, cx: &mut Context<Self>) {
		match self.paste.toggle_fold(c, self.preview.as_ref()) {
			paste::preview::Folded::NoPlan => {}
			paste::preview::Folded::Kept => {
				app_log!("[APP:PASTE_COMMIT_TOGGLED: idx={}]", c);
				cx.notify();
			}
			paste::preview::Folded::Reselected(nav) => {
				app_log!("[APP:PASTE_COMMIT_TOGGLED: idx={}]", c);
				self.report_paste_nav(nav, cx);
				cx.notify();
			}
		}
	}

	pub fn apply_paste_restore(&mut self, cx: &mut Context<Self>) {
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

		self.spawn_owned(
			cx,
			lifecycle::JobKind::Mutating,
			None,
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
								result.files.skipped_existing_count,
								result.files.deleted_count,
								result.files.errors.len(),
								result.created_commits.len()
							);
							for error in &result.files.errors {
								app_log!("[APP:PASTE_FILE_ERROR: {error}]");
							}
							model.paste.clear(model.preview.as_ref());
							model.restore_log_after_paste();
							model.pending_focus =
								Some(model.focus_handle.clone());
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
							app_log!("[APP:PASTE_STALE_DETECTED: {}]", err.key);
							model.status = err.clone();
							model.paste.apply_failed(err, model.preview.as_ref());
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// Called on every path that closes the paste preview.
	fn restore_log_after_paste(&mut self) {
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
		self.selected_commit_file = None;
		self.commit_file_sel.clear();
		self.commit_files.clear();
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
		if self.refuse_while_applying("cancel", cx) {
			return;
		}
		let was_loading = self.paste.is_loading();
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
					self.set_status("status_clipboard_failed", [e.to_string()])
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
			if wipe {
				model.begin_rescan();
				wipe = false;
			}
			model.merge_repo_entries(processed.0);
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

fn resolve_added_repo(
	path: PathBuf,
	cancel: &CancelToken,
) -> Result<RepoEntry, String> {
	if cancel.is_cancelled() {
		return Err("cancelled".to_string());
	}
	let path = dunce::canonicalize(&path).map_err(|err| err.to_string())?;
	let opts = RunOptions {
		cancel: Some(cancel.clone()),
		..RunOptions::interactive(None)
	};
	Ok(RepoEntry::from(snip_core::gitview::identify_repo(
		&path, None, &opts,
	)))
}

fn read_preview(
	host: &crate::githost::GitHost,
	repo_root: &std::path::Path,
	path: &str,
	source: &SourceKind,
	cancel: CancelToken,
) -> Result<(browser::SourcePreview, PreviewSource), String> {
	if matches!(source, SourceKind::File) {
		return browser::file_preview(repo_root, path)
			.map(|p| (p, PreviewSource::WorkingFile))
			.map_err(|e| e.to_string());
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
		Err(e) => Err(e.to_string()),
	}
}

/// What the app opens when it starts.
#[derive(Debug, PartialEq, Eq)]
enum Startup {
	Local(PathBuf),
	/// Reconnected in the background once the window is up.
	Remote(remote::RecentRemote),
	Nothing,
}

impl Startup {
	fn local(self) -> Option<PathBuf> {
		match self {
			Startup::Local(path) => Some(path),
			_ => None,
		}
	}
}

/// `--workspace`, else the last workspace open when it was remote, else the
/// last remembered local one (IntelliJ reopens the last project), else the
/// launch folder. A Finder or Explorer launch starts in `/` or the home
/// folder: that opens nothing rather than scanning it.
fn startup_choice(
	arg: Option<PathBuf>,
	last_remote: Option<remote::RecentRemote>,
	last_local: Option<PathBuf>,
	cwd: Option<PathBuf>,
	home: Option<PathBuf>,
) -> Startup {
	if let Some(arg) = arg {
		return Startup::Local(arg);
	}
	if let Some(last) = last_remote {
		return Startup::Remote(last);
	}
	if let Some(last) = last_local {
		return Startup::Local(last);
	}
	match cwd {
		Some(cwd) if cwd.parent().is_some() && Some(&cwd) != home.as_ref() => {
			Startup::Local(cwd)
		}
		_ => Startup::Nothing,
	}
}

/// [`startup_choice`] on the remembered workspaces; nothing is read under
/// `cfg(test)` or in an e2e run without `SNIP_CONFIG_DIR`. Only a normal
/// launch (`allow_remote`) reconnects to a remote workspace.
fn startup_workspace(arg: Option<PathBuf>, allow_remote: bool) -> Startup {
	if arg.is_some() {
		return startup_choice(arg, None, None, None, None);
	}
	startup_choice(
		None,
		remote::load_last().filter(|_| allow_remote),
		recent::load().into_iter().next(),
		std::env::current_dir().ok(),
		recent::home(),
	)
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
		KeyBinding::new("ctrl-shift-w", CloseWorkspace, None),
		KeyBinding::new("cmd-shift-w", CloseWorkspace, None),
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
		KeyBinding::new("escape", CancelPaste, Some("PastePanel")),
		KeyBinding::new("up", NavUp, Some("PastePanel")),
		KeyBinding::new("down", NavDown, Some("PastePanel")),
		KeyBinding::new("space", NavToggle, Some("PastePanel")),
		// Reader.
		KeyBinding::new("ctrl-c", ReaderCopy, Some("Reader")),
		KeyBinding::new("cmd-c", ReaderCopy, Some("Reader")),
		KeyBinding::new("ctrl-a", ReaderSelectAll, Some("Reader")),
		KeyBinding::new("up", ReaderUp, Some("Reader")),
		KeyBinding::new("down", ReaderDown, Some("Reader")),
		KeyBinding::new("pageup", ReaderPageUp, Some("Reader")),
		KeyBinding::new("pagedown", ReaderPageDown, Some("Reader")),
		KeyBinding::new("escape", ReaderClear, Some("Reader")),
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
		KeyBinding::new("escape", FocusEditor, Some("ToolList")),
		// Context menu.
		KeyBinding::new("up", MenuUp, Some("ContextMenu")),
		KeyBinding::new("down", MenuDown, Some("ContextMenu")),
		KeyBinding::new("enter", MenuConfirm, Some("ContextMenu")),
		KeyBinding::new("escape", MenuCancel, Some("ContextMenu")),
		// Git log.
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
		KeyBinding::new("escape", FocusEditor, Some("GitLog")),
		// No IntelliJ key for "go to HEAD" in the Log; kept.
		KeyBinding::new("h", LogHead, Some("GitLog")),
	];
	b.extend(text_input::bindings());
	b
}

fn main() {
	let (workspace, mode, restore_dir) = parse_cli_args();
	let (workspace, reconnect) =
		match startup_workspace(workspace, mode == "normal") {
			Startup::Remote(last) => (None, Some(last)),
			other => (other.local(), None),
		};
	let app = Application::new().with_assets(icons::Assets);

	app.run(move |cx: &mut App| {
		cx.bind_keys(key_bindings());
		theme::register_fonts(cx);

		let bounds = Bounds::centered(None, size(px(1080.0), px(720.0)), cx);
		let ws = workspace.clone();
		let last_remote = reconnect.clone();
		let app_mode = mode.clone();
		let paste_dir = restore_dir.clone();

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
				let model = cx
					.new(|cx| WorkbenchModel::new(ws, paste_dir, app_mode, cx));
				if let Some(last) = last_remote {
					model.update(cx, |m, cx| m.reopen_last_remote(last, cx));
				}
				let fh = model.read(cx).focus_handle.clone();
				window.focus(&fh);
				let close_target = model.clone();
				window.on_window_should_close(cx, move |_window, cx| {
					close_target.update(cx, |model, cx| model.begin_quit(cx));
					false
				});
				app_log!("[APP:WINDOW_READY]");
				model
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
	/// `--workspace` wins; then the last workspace open when it was remote;
	/// then the last local one; then a launch folder other than `/` or home.
	#[test]
	fn startup_reconnects_the_last_remote_workspace_unless_told_otherwise() {
		use super::{startup_choice, startup_workspace, Startup};
		use crate::remote::RecentRemote;
		use std::path::PathBuf;
		let last = || {
			Some(RecentRemote {
				host: "macmini".into(),
				path: "/Users/x/ck/cat".into(),
			})
		};
		let p = |s: &str| Some(PathBuf::from(s));
		assert_eq!(
			startup_choice(p("/arg"), last(), p("/local"), p("/cwd"), None),
			Startup::Local(PathBuf::from("/arg"))
		);
		assert_eq!(
			startup_choice(None, last(), p("/local"), p("/cwd"), None),
			Startup::Remote(last().unwrap())
		);
		assert_eq!(
			startup_choice(None, None, p("/local"), p("/cwd"), None),
			Startup::Local(PathBuf::from("/local"))
		);
		assert_eq!(
			startup_choice(None, None, None, p("/home/u/w"), p("/home/u")),
			Startup::Local(PathBuf::from("/home/u/w"))
		);
		assert_eq!(
			startup_choice(None, None, None, p("/home/u"), p("/home/u")),
			Startup::Nothing
		);
		assert_eq!(
			startup_choice(None, None, None, p("/"), None),
			Startup::Nothing
		);
		// Nothing remembered is read under cfg(test).
		assert!(!matches!(startup_workspace(None, true), Startup::Remote(_)));
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
				assert!(!m.workspace_menu, "{:?}", m.remote.message);
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
					let (host, ws) = *target;
					m.finish_open_remote(host, ws, cx);
				}
			});
		}

		/// A window with no workspace and an in-process worker as the only
		/// ssh host, its menu open.
		fn remote_menu(
			cx: &mut TestAppContext,
		) -> (Entity<WorkbenchModel>, &mut VisualTestContext) {
			let worker = std::sync::Arc::new(snip_remote::Worker::new(
				snip_remote::WorkerOptions::default(),
			));
			let host = snip_remote::RemoteHost::in_process(worker);
			cx.update(|cx| cx.bind_keys(crate::key_bindings()));
			let (model, cx) = cx.add_window_view(|_, cx| {
				WorkbenchModel::new(None, None, "normal".into(), cx)
			});
			cx.run_until_parked();
			model.update(cx, |m, _| {
				m.workspace_menu = true;
				m.remote.hosts = vec![host];
				m.remote.recent.clear();
			});
			(model, cx)
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
				m.workspace_menu = true;
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
				let Some(Some(Ok(listing))) =
					m.remote.browse.as_ref().map(|b| &b.listing)
				else {
					panic!("home not listed: {:?}", m.remote.browse);
				};
				assert!(!listing.path.is_empty());
				let session = m.remote.session.as_ref().expect("open");
				assert_eq!(session.workspace.name, "shared");
				assert_eq!(m.remote.recent[0].path, session.workspace.id);

				assert!(m.workspace_open && !m.workspace_menu);
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
				assert!(!m.workspace_open && m.workspace_menu);
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
				m.remote_listing_landed(stale, Err("late failure".into()), cx);
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

		/// The launch reconnect: the last remote workspace opens in the
		/// background; a folder or host that is gone leaves the app with no
		/// workspace and says why, on the status bar and the empty screen.
		#[gpui::test]
		fn launch_reconnects_the_last_remote_workspace_or_says_why_not(
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
				m.workspace_menu = false;
				m.remote.hosts[0].name.clone()
			});

			model.update(cx, |m, cx| {
				m.reopen_last_remote(
					RecentRemote {
						host: "gone-host".into(),
						path: id.clone(),
					},
					cx,
				);
				assert_eq!(m.status.key, "remote_open_failed");
				assert!(!m.workspace_open && !m.remote.busy);
			});

			model.update(cx, |m, cx| {
				m.reopen_last_remote(
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
			});

			model.update(cx, |m, cx| {
				m.reopen_last_remote(
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
				let done =
					model.read_with(cx, |m, _| !m.commit_files.is_empty());
				if done {
					break;
				}
			}
			let commit_file = model.read_with(cx, |m, _| {
				assert!(!m.commit_files.is_empty());
				m.commit_files[0].0.clone()
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
			for _ in 0..50 {
				settle(cx);
				let done = model.read_with(cx, |m, _| {
					m.repos.len() == 1
						&& m.change_repos.first().is_some_and(|r| {
							r.state == crate::ChangeRepoState::Loaded
						})
				});
				if done {
					break;
				}
			}
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
				assert_eq!(paths, ["d.txt"]);
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
			while model.read_with(cx, |m, _| m.workspace_menu) {
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
			model.read_with(cx, |m, _| assert!(!m.workspace_menu));
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
				assert!(m.workspace_menu);
				m.remote_path_input.read(cx).handle()
			});
			assert!(
				cx.update(|window, _| field.is_focused(window)),
				"the path field has the focus"
			);
			cx.simulate_keystrokes("escape");
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(!m.workspace_menu, "Escape in the path field");
			});

			model.update(cx, |m, cx| m.toggle_workspace_menu(cx));
			settle(cx);
			let button = cx
				.debug_bounds("btn-workspace-menu")
				.expect("menu button drawn");
			cx.simulate_click(button.center(), gpui::Modifiers::none());
			settle(cx);
			model.read_with(cx, |m, _| {
				assert!(!m.workspace_menu, "second press of the button");
			});
		}

		#[gpui::test]
		fn commit_files_cmd_and_shift_select_several(cx: &mut TestAppContext) {
			let ws = tempfile::tempdir().unwrap();
			repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let files = ["a.txt", "b.txt", "c.txt", "d.txt"];
			model.update(cx, |m, cx| {
				m.commit_files =
					files.iter().map(|f| (f.to_string(), None)).collect();
				m.selected_commit_file = Some("b.txt".into());
				m.toggle_commit_file("d.txt", cx);
				assert_eq!(m.commit_file_sel, ["b.txt", "d.txt"]);
				m.toggle_commit_file("b.txt", cx);
				assert_eq!(m.commit_file_sel, ["d.txt"]);
				m.extend_commit_files(&files, "d.txt", cx);
				assert_eq!(m.commit_file_sel, ["b.txt", "c.txt", "d.txt"]);
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
				m.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.commit_files = [
					("src/a.rs", Modified),
					("src/gone.rs", Deleted),
					("pom.xml", Modified),
					("README.md", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				m.selected_commit_file = Some("src/a.rs".into());
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
				m.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.commit_files = [
					("common.txt", Modified),
					("emoji.txt", Modified),
					("dir/gone.txt", Deleted),
					("dir/keep.txt", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				m.selected_commit_file = Some("emoji.txt".into());
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

		/// Flat layout (`log_details_by_dir = false`) copies in `commit_files`
		/// order, not click order: b.txt is clicked before z.txt, but the flat
		/// screen shows z.txt first. (In-process tests default to the by-dir
		/// layout, which sorts dirs and names and hides a click-order bug.)
		#[gpui::test]
		fn commit_files_copy_flat_layout_screen_order(cx: &mut TestAppContext) {
			use crate::menu::{MenuAct, MenuEntry};
			use snip_core::format::ChangeType::Modified;
			let ws = tempfile::tempdir().unwrap();
			let root = repo(ws.path(), "a", &[]);
			let (model, cx) = open(cx, ws.path().to_path_buf(), None);
			let copied = model.update(cx, |m, cx| {
				m.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.log_details_by_dir = false;
				m.commit_files = [
					("z.txt", Modified),
					("a/x.txt", Modified),
					("b.txt", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				// Click order: b.txt, a/x.txt, z.txt (reverse of flat order).
				m.selected_commit_file = Some("b.txt".into());
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
			assert_eq!(
				got,
				[("z.txt", false), ("a/x.txt", false), ("b.txt", false)]
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
				m.log_commit_root = Some(root.clone());
				m.compare = Some(("old".into(), "new".into()));
				m.commit_files = [
					("common.txt", Modified),
					("emoji.txt", Modified),
					("dir/gone.txt", Deleted),
					("dir/keep.txt", Modified),
				]
				.map(|(p, c)| (p.to_string(), Some(c)))
				.into();
				m.selected_commit_file = Some("dir/keep.txt".into());
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

		/// The log list width and, per laid-out row, the widths of its graph
		/// gutter and subject cell.
		struct LogMeasure {
			list_w: f32,
			rows: Vec<(f32, f32)>,
		}

		/// Six repositories with three feature branches of long names each,
		/// so the graph is wide and rows carry ref labels; shown in a
		/// `w` x 752 window.
		fn measure_wide_log(cx: &mut TestAppContext, w: f32) -> LogMeasure {
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
			cx.simulate_resize(gpui::size(gpui::px(w), gpui::px(752.)));
			settle(cx);
			// The list reads its own width from the previous frame.
			for _ in 0..2 {
				cx.update(|window, _| window.refresh());
				settle(cx);
			}
			let (n, list_w) = model.read_with(cx, |m, _| {
				let bounds = m.log_scroll.0.borrow().base_handle.bounds();
				(m.display_commits().len(), f32::from(bounds.size.width))
			});
			assert!(n > 10, "log did not load: {n} commits");
			let mut rows = Vec::new();
			for ix in 0..n {
				// `debug_bounds` wants a 'static selector; a test may leak.
				let key = |what: &str| -> &'static str {
					Box::leak(format!("log-{what}:{ix}").into_boxed_str())
				};
				if let (Some(g), Some(s)) = (
					cx.debug_bounds(key("gutter")),
					cx.debug_bounds(key("subject")),
				) {
					rows.push((
						f32::from(g.size.width),
						f32::from(s.size.width),
					));
				}
			}
			LogMeasure { list_w, rows }
		}

		/// Every row shares one gutter, and the subject of each row keeps at
		/// least `min` wide.
		fn assert_log_layout(m: &LogMeasure, min: f32) {
			assert!(!m.rows.is_empty(), "no rows were laid out");
			for (gutter, subject) in &m.rows {
				assert_eq!(*gutter, m.rows[0].0, "{:?}", m.rows);
				assert!(
					*subject >= min,
					"list {}: subject {subject} < {min}: {:?}",
					m.list_w,
					m.rows
				);
			}
		}

		#[gpui::test]
		fn wide_multi_repo_log_keeps_a_subject_at_1080(
			cx: &mut TestAppContext,
		) {
			let m = measure_wide_log(cx, 1080.);
			// ui::log::MIN_SUBJECT_W
			assert_log_layout(&m, 160.);
		}

		/// At 900 the list is ~430px: the gutter sits at its floor and the
		/// date and author cells shrink, so the subject still gets its
		/// minimum with the labels dropped.
		#[gpui::test]
		fn wide_multi_repo_log_keeps_a_subject_at_900(cx: &mut TestAppContext) {
			let m = measure_wide_log(cx, 900.);
			assert_log_layout(&m, 160.);
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
				"paste-row:0:img.bin",
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
				!ids.contains(&"paste-row:0:img.bin".to_string()),
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
			assert_eq!(initial_path.as_deref(), Some("img.bin"));

			cx.simulate_keystrokes("down");
			cx.run_until_parked();

			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, order[1]);
				assert_eq!(
					m.paste.detail().and_then(|d| d.path.as_deref()),
					Some("fresh.txt")
				);
			});

			cx.simulate_keystrokes("up");
			cx.run_until_parked();

			model.read_with(cx, |m, _| {
				let plan = m.paste.plan().unwrap();
				assert_eq!(plan.selected_item_idx, order[0]);
				assert_eq!(
					m.paste.detail().and_then(|d| d.path.as_deref()),
					Some("img.bin")
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
				summary: Err("offline".into()),
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
				m.repos =
					vec![make_test_repo(tmp.path().to_path_buf(), "repo")];
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
				m.repos = vec![
					make_test_repo(tmp.path().join("r1"), "r1"),
					make_test_repo(tmp.path().join("r2"), "r2"),
				];
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
		fn log_while_loading_is_not_empty_log_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.repos =
					vec![make_test_repo(tmp.path().to_path_buf(), "repo")];
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
				m.repos = vec![
					make_test_repo(tmp.path().join("r1"), "r1"),
					make_test_repo(tmp.path().join("r2"), "r2"),
				];
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
				m.repos.clear();
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Incomplete);
				m.discovery_errors =
					vec![(tmp.path().join("sub"), "read failed".into())];
				m.is_loading = false;
			});
			let state = model.read_with(cx, |m, _| m.changes_empty_state());
			assert_eq!(
				state,
				Some(ChangesEmpty::ScanFailed(crate::i18n::Msg::new(
					"error_repo_status",
					["read failed".to_string()]
				)))
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
					m.repos.clear();
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
				m.repos.clear();
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
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
				m.repos = vec![
					make_test_repo(tmp.path().join("r1"), "r1"),
					make_test_repo(tmp.path().join("r2"), "r2"),
				];
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
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
					m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
				m.repos = vec![
					make_test_repo(tmp.path().join("alpha"), "alpha"),
					make_test_repo(tmp.path().join("beta"), "beta"),
				];
				m.change_repos = vec![
					crate::ChangeRepo {
						root: tmp.path().join("alpha"),
						name: "alpha".into(),
						state: crate::ChangeRepoState::Failed(
							"git status failed".into(),
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
				m.repos = vec![
					make_test_repo(tmp.path().join("alpha"), "alpha"),
					make_test_repo(tmp.path().join("beta"), "beta"),
				];
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
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
				m.repos.clear();
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
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
				m.discovery_status =
					Some(snip_core::workspace::ScanStatus::Complete);
				m.is_loading = false;
				m.history_error = Some("git read failure".into());
			});
			assert_eq!(
				model.read_with(cx, |m, _| m.log_empty_state()),
				Some(LogEmpty::Failed(crate::i18n::Msg::new(
					"error_history",
					["git read failure".to_string()]
				)))
			);
		}

		#[gpui::test]
		fn log_empty_loading_branch_empty(cx: &mut TestAppContext) {
			let tmp = tempfile::tempdir().unwrap();
			let (model, cx) = open(cx, tmp.path().to_path_buf(), None);
			model.update(cx, |m, _| {
				m.commits.clear();
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
				m.repos = vec![make_test_repo(tmp.path().join("r1"), "r1")];
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
			assert_eq!(msg.args[5], NATIVE_FILE_COUNT_LIMIT.to_string());

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
			Err(err_msg) => {
				assert!(
					err_msg.contains("Path leaves the workspace"),
					"expected 'Path leaves the workspace', got: {err_msg}"
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

	#[test]
	fn test_disambiguate_repo_names() {
		let mut repos = vec![
			RepoEntry {
				root: PathBuf::from("/workspace/a/core"),
				name: "core".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err("mock".into()),
			},
			RepoEntry {
				root: PathBuf::from("/workspace/b/core"),
				name: "core".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err("mock".into()),
			},
			RepoEntry {
				root: PathBuf::from("/workspace/other"),
				name: "other".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err("mock".into()),
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
				summary: Err("mock".into()),
			},
			RepoEntry {
				root: PathBuf::from("/real/y/svc/app"),
				name: "app".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err("mock".into()),
			},
			RepoEntry {
				root: PathBuf::from("/real/z/other"),
				name: "other".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err("mock".into()),
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
				summary: Err("mock".into()),
			},
			RepoEntry {
				root: PathBuf::from("/real/q/x/svc/app"),
				name: "app".to_string(),
				kind: RepoEntryKind::Main,
				identity: None,
				summary: Err("mock".into()),
			},
		];
		WorkbenchModel::disambiguate_repo_names(&mut repos2, &ws);
		assert_eq!(repos2[0].name, "p/x/svc/app");
		assert_eq!(repos2[1].name, "q/x/svc/app");
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
		slots[2].state = ChangeRepoState::Failed("boom".into());
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
			slots[slot].state = ChangeRepoState::Failed("boom".into());
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
		assert_eq!(
			shape(&ui::commit_file_rows(&files, false, &closed)),
			[
				"F0:src/main/java/pkg/B.java",
				"F0:README.md",
				"F0:src/main/java/pkg/a.java",
				"F0:src/test/T.java",
				"F0:docs/guide.md",
			]
		);
	}
}
