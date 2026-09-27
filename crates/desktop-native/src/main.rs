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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::OnceLock;

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

fn release_map<K, V, S: Default>(slot: &mut HashMap<K, V, S>) {
	*slot = HashMap::default();
}

fn release_set<T, S: Default>(slot: &mut HashSet<T, S>) {
	*slot = HashSet::default();
}

fn release_path(slot: &mut PathBuf) {
	*slot = PathBuf::new();
}

/// Test-harness event line.
macro_rules! app_log {
	($($arg:tt)*) => {{
		println!($($arg)*);
		let _ = std::io::Write::flush(&mut std::io::stdout());
	}};
}

/// Measurement harness readiness marker (`--mode idle|overview|preview`).
fn ready_marker(name: &str) {
	println!("[READY:{name}]");
	let _ = std::io::Write::flush(&mut std::io::stdout());
}

pub mod graph_view;
mod history;
pub mod i18n;
mod icons;
pub mod lifecycle;
pub mod paste;
mod reader;
mod selector;
pub mod syntax;
mod text_input;
pub mod theme;
pub mod tree;
mod ui;

use gpui::{
	actions, prelude::*, px, size, App, Application, Bounds, Context, Entity,
	FocusHandle, KeyBinding, WindowBounds, WindowOptions,
};
use snip_core::browser::{self, CommitSummary, GitReference};
use snip_core::clip;
use snip_core::format::ChangeType;
use snip_core::gitrun::{CancelToken, Overflow, RunOptions};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::graph::GraphLayout;
use snip_core::settings::Settings;
use snip_core::transfer::{
	plan_commit_export_exact_with, plan_export_with, CanonicalRootId,
	ExportItem, ExportSelection, SourceKind,
};
use snip_core::workspace::{
	declared_submodules, status_details, summarize, DiscoveredRepo, Discovery,
	RepoIdentity, RepoKind, RepoSummary, ScanBudget, ScanStatus,
	SubmoduleState,
};

use crate::history::{LogSearch, RevTree};
use crate::i18n::{Locale, Msg};
use crate::paste::PastePreviewPlan;
use crate::reader::{Preview, PreviewSource, Reader};
use crate::syntax::Language;
use crate::text_input::{InputEvent, TextInput};
use crate::tree::FileTreeNode;
use crate::tree::{NodeKey, TreeCommand, TreeEffect, TreeIo};

actions!(
	workbench,
	[
		Quit,
		CopySelection,
		PastePreview,
		ApplyPaste,
		CancelPaste,
		Refresh,
		DeselectAllFiles,
		SelectAllFiles,
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
	]
);

#[derive(Clone, Debug)]
pub struct FileChangeItem {
	pub path: String,
	pub change_type: Option<ChangeType>,
	pub source: SourceKind,
	pub is_conflict: bool,
	pub selected: bool,
}

type WorkingChangeTuple = (String, Option<ChangeType>, SourceKind, bool);

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
	pub log_search: Option<LogSearch>,
	pub search_by_author: bool,
	pub collapsed_merges: HashSet<String>,
	/// Commits hidden on this page by collapsed merges.
	pub hidden_commits: HashSet<String>,
	pub selected_commit: Option<String>,
	/// Range endpoint picked with shift (anchor is `selected_commit`).
	pub range_head: Option<String>,
	pub log_scroll: gpui::UniformListScrollHandle,
	pub select_head_after_load: bool,

	// Files of the selected commit or compare.
	pub commit_files: Vec<(String, Option<ChangeType>)>,
	pub selected_commit_file: Option<String>,
	pub compare: Option<(String, String)>,

	// Tool windows.
	pub files: Vec<FileChangeItem>,
	pub file_tree: Option<FileTreeNode>,
	pub rev_tree: Option<RevTree>,
	pub active_tab: WorkbenchTab,
	pub selected_file: Option<String>,
	pub selected_file_source: Option<SourceKind>,
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

	pub basket: HashMap<CanonicalRootId, Vec<ExportItem>>,
	pub history_cancel: Option<CancelToken>,
	pub preview_cancel: Option<CancelToken>,
	pub tree_cancel: Option<CancelToken>,
	pub rev_tree_cancel: Option<CancelToken>,
	pub repo_cancel: Option<CancelToken>,
	pub scan_cancel: Option<CancelToken>,
	pub copy_cancel: Option<CancelToken>,
	pub paste_cancel: Option<CancelToken>,
	pub discovery: Option<Discovery>,
	pub discovery_status: Option<ScanStatus>,
	pub discovery_errors: Vec<(PathBuf, String)>,
	pub discovery_depth_limited: Vec<PathBuf>,
	pub discovery_error_overflow: usize,
	pub discovery_depth_overflow: usize,
	discovery_generation: u64,
	pinned_repo: Option<(PathBuf, PathBuf)>,
	manual_repos: Vec<RepoEntry>,
	tree_queue: VecDeque<TreeIo>,
	tree_worker: u64,
	tree_worker_alive: bool,
	restore_expanded: Vec<String>,
	add_cancel: Option<CancelToken>,
	pub is_adding_repo: bool,
	pub add_repo_input: Entity<TextInput>,
	pub paste_preview: Option<PastePreviewPlan>,
	/// A read-only preview or mapping rebuild is running in the background.
	/// Nothing can be applied until it lands.
	pub paste_loading: bool,
	/// Bumped for every paste request, remap and cancel; a background
	/// result from an older value is dropped.
	pub paste_generation: u64,
	/// Text of the selected paste item (one at a time).
	pub paste_detail: Option<Preview>,
	pub paste_scroll: gpui::UniformListScrollHandle,
	pub status: Msg,
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
	/// Log visibility saved when the paste preview auto-collapsed it;
	/// cleared once restored or when the user toggles the log themselves.
	pub log_before_paste: Option<bool>,
	pub dragging: Option<Splitter>,
	pub last_viewport: (i32, i32),
	/// E2E control-bounds reporting; `None` unless `SNIP_NATIVE_E2E=1`.
	pub probes: Option<ui::Probes>,
	/// Test-only delay before a confirmed write, honoured only in E2E mode.
	pub e2e_apply_delay: Option<std::time::Duration>,
	/// Focus requested from a context without a `Window`; applied on render.
	pub pending_focus: Option<FocusHandle>,
	pub e2e_read_delay: Option<std::time::Duration>,
	/// Test-only hold file for project-tree reads, honoured only in E2E mode.
	pub e2e_tree_hold: Option<PathBuf>,
	/// Test-only hold file for copy export revalidation, honoured only in E2E mode.
	pub e2e_export_hold: Option<PathBuf>,
	pub workspace_open: bool,
	pub workspace_menu: bool,
	pub workspace_picker: bool,
	pub workspace_path_input: Entity<TextInput>,
	pub lifecycle: lifecycle::Lifecycle,
	pub watch_running: bool,
	pub last_life_log: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Splitter {
	Left,
	Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasketGroup {
	/// Workspace working file mode (SourceKind::File).
	File,
	/// Working changes from Git changes view (Working, Unstaged, Staged).
	GitChanges,
}

impl WorkbenchModel {
	pub fn new(
		workspace_root: PathBuf,
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
		});
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
				InputEvent::Dismiss => {
					this.pending_focus = Some(this.reader_focus.clone());
					cx.notify();
				}
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
				InputEvent::Dismiss => {
					this.pending_focus = Some(this.reader_focus.clone());
					cx.notify();
				}
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
			|this, input, ev: &InputEvent, cx| {
				if matches!(ev, InputEvent::Submit) {
					let text = input.read(cx).text().trim().to_string();
					this.confirm_open_workspace(&text, cx);
				}
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
			workspace_root,
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
			search_by_author: false,
			collapsed_merges: HashSet::new(),
			hidden_commits: HashSet::new(),
			selected_commit: None,
			range_head: None,
			log_scroll: gpui::UniformListScrollHandle::new(),
			select_head_after_load: false,
			commit_files: Vec::new(),
			selected_commit_file: None,
			compare: None,
			files: Vec::new(),
			file_tree: None,
			rev_tree: None,
			active_tab: WorkbenchTab::GitChanges,
			selected_file: None,
			selected_file_source: None,
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
			basket: HashMap::new(),
			history_cancel: None,
			preview_cancel: None,
			tree_cancel: None,
			rev_tree_cancel: None,
			repo_cancel: None,
			scan_cancel: None,
			copy_cancel: None,
			paste_cancel: None,
			discovery: None,
			discovery_status: None,
			discovery_errors: Vec::new(),
			discovery_depth_limited: Vec::new(),
			discovery_error_overflow: 0,
			discovery_depth_overflow: 0,
			discovery_generation: 0,
			pinned_repo: None,
			manual_repos: Vec::new(),
			tree_queue: VecDeque::new(),
			tree_worker: 0,
			tree_worker_alive: false,
			restore_expanded: Vec::new(),
			add_cancel: None,
			is_adding_repo: false,
			paste_preview: None,
			paste_loading: false,
			paste_generation: 0,
			paste_detail: None,
			paste_scroll: gpui::UniformListScrollHandle::new(),
			status: Msg::new("status_scanning", []),
			is_loading: true,
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
			log_before_paste: None,
			dragging: None,
			last_viewport: (0, 0),
			probes: ui::Probes::from_env(),
			e2e_apply_delay: ui::e2e_apply_delay(),
			pending_focus: None,
			e2e_read_delay: ui::e2e_read_delay(),
			e2e_tree_hold: ui::e2e_tree_hold(),
			e2e_export_hold: ui::e2e_export_hold(),
			workspace_open: true,
			workspace_menu: false,
			workspace_picker: false,
			workspace_path_input,
			lifecycle: lifecycle::Lifecycle::new(1),
			watch_running: false,
			last_life_log: String::new(),
		};
		model.reload_repos(cx);
		model
	}

	pub fn set_status(
		&mut self,
		key: &'static str,
		args: impl crate::i18n::IntoMsgArgs,
	) {
		self.status = Msg::new(key, args);
	}

	pub fn repo(&self) -> Option<&RepoEntry> {
		self.selected_repo_idx.and_then(|i| self.repos.get(i))
	}

	pub fn repo_root(&self) -> Option<PathBuf> {
		self.repo().map(|r| r.root.clone())
	}

	pub fn current_restore_destination(&self) -> PathBuf {
		if let Some(ref d) = self.restore_dir {
			d.clone()
		} else if let Some(root) = self.repo_root() {
			root
		} else {
			self.workspace_root.clone()
		}
	}

	pub fn set_preview(&mut self, p: Preview) {
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
		self.preview = Some(p);
		self.preview_loading = false;
		self.preview_error = None;
		self.reader.reset_for_new_preview();
		// Re-run an active find against the new text.
		self.reader.matches.clear();
	}

	pub fn deselect_all_files(&mut self, cx: &mut Context<Self>) {
		for f in &mut self.files {
			f.selected = false;
		}
		if let Some(ref mut tree) = self.file_tree {
			tree.set_all_selected(false);
		}
		if let Some(repo) = self.repo() {
			if let Ok(c) = CanonicalRootId::new(&repo.root) {
				self.set_basket_group(c, BasketGroup::GitChanges, Vec::new());
			}
		}
		self.remember_tree_selection();
		app_log!("[APP:FILES_DESELECTED]");
		self.set_status("status_deselected_all", []);
		cx.notify();
	}

	pub fn select_all_files(&mut self, cx: &mut Context<Self>) {
		for f in &mut self.files {
			f.selected = true;
		}
		if let Some(ref mut tree) = self.file_tree {
			tree.set_all_selected(true);
		}
		self.sync_git_selection_to_basket();
		self.remember_tree_selection();
		app_log!("[APP:FILES_SELECTED_ALL]");
		self.set_status("status_selected_all", []);
		cx.notify();
	}

	pub fn toggle_file(&mut self, idx: usize, cx: &mut Context<Self>) {
		if let Some(f) = self.files.get_mut(idx) {
			f.selected = !f.selected;
			let path = f.path.clone();
			let selected = f.selected;
			self.sync_git_selection_to_basket();
			app_log!(
				"[APP:FILE_TOGGLED: {}: {}: selected={}]",
				idx,
				path,
				selected
			);
			self.set_status("status_toggled_file", [path]);
			cx.notify();
		}
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
		let mut seen_identities: HashSet<(PathBuf, PathBuf)> = HashSet::new();
		let mut seen_roots: HashSet<PathBuf> = HashSet::new();
		let mut list = Vec::new();
		let mut errors = Vec::new();

		for r in discovered {
			let canonical_path =
				dunce::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone());
			let name = r
				.path
				.file_name()
				.map(|n| n.to_string_lossy().into_owned())
				.unwrap_or_else(|| r.path.display().to_string());

			match Git::open_with(&r.path, opts) {
				Ok(git) => {
					let id_res = RepoIdentity::resolve(&git, opts);
					let (kind, identity) = match &id_res {
						Ok(id) => {
							// Symlink alias dedup:
							if !seen_identities.insert((
								id.toplevel.clone(),
								id.git_dir.clone(),
							)) {
								continue;
							}
							seen_roots.insert(canonical_path.clone());
							let k = match id.kind {
								RepoKind::LinkedWorktree => {
									RepoEntryKind::LinkedWorktree
								}
								RepoKind::Submodule => RepoEntryKind::Submodule,
								RepoKind::Main => RepoEntryKind::Main,
							};
							(k, Some(id.clone()))
						}
						Err(err) => {
							let root = git.root().to_path_buf();
							if !seen_roots.insert(root.clone()) {
								continue;
							}
							list.push(RepoEntry {
								root,
								name,
								kind: RepoEntryKind::Main,
								identity: None,
								summary: Err(format!(
									"repository identity: {err}"
								)),
							});
							continue;
						}
					};

					let root = identity
						.as_ref()
						.map(|id| id.toplevel.clone())
						.unwrap_or_else(|| git.root().to_path_buf());
					let name = root
						.file_name()
						.map(|n| n.to_string_lossy().into_owned())
						.unwrap_or(name);
					let summary =
						summarize(&git, opts).map_err(|e| e.to_string());

					list.push(RepoEntry {
						root: root.clone(),
						name,
						kind,
						identity,
						summary,
					});

					let submodules = match declared_submodules(&git, opts) {
						Ok(submodules) => submodules,
						Err(err) => {
							errors.push((root, format!("submodules: {err}")));
							Vec::new()
						}
					};
					{
						for subm in submodules {
							let sub_path = git.root().join(&subm.path);
							let canonical_sub = dunce::canonicalize(&sub_path)
								.unwrap_or_else(|_| sub_path.clone());
							match subm.state {
								SubmoduleState::NotCheckedOut
									if seen_roots
										.insert(canonical_sub.clone()) =>
								{
									list.push(RepoEntry {
									root: sub_path,
									name: subm.name,
									kind: RepoEntryKind::UninitializedSubmodule,
									identity: None,
									summary: Err(
										"Submodule not checked out (uninitialized)"
											.into(),
									),
								});
								}
								SubmoduleState::Unreadable(reason)
									if seen_roots.insert(canonical_sub) =>
								{
									list.push(RepoEntry {
									root: sub_path,
									name: subm.name,
									kind: RepoEntryKind::UninitializedSubmodule,
									identity: None,
									summary: Err(format!(
										"Submodule unreadable: {reason}"
									)),
								});
								}
								_ => {}
							}
						}
					}
				}
				Err(e) => {
					if seen_roots.insert(canonical_path) {
						list.push(RepoEntry {
							root: r.path,
							name,
							kind: RepoEntryKind::Main,
							identity: None,
							summary: Err(e.to_string()),
						});
					}
				}
			}
		}

		(list, errors)
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
	}

	pub(crate) fn accepting_work(&self) -> bool {
		self.workspace_open && !self.lifecycle.is_draining()
	}

	fn needs_watch(&mut self) -> bool {
		self.lifecycle.unfinished() > 0 || self.lifecycle.is_draining()
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
	) {
		let (id, flag) = self.lifecycle.register(kind, cancel);
		let task = cx.foreground_executor().spawn(async move {
			fut.await;
			drop(flag);
		});
		self.lifecycle.attach(id, task);
		self.arm_watch(cx);
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
				] {
					if let Some(token) = slot.as_ref() {
						token.cancel();
					}
				}
				self.invalidate_paste_job();
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
		}
	}

	fn release_workspace_state(&mut self, cx: &mut Context<Self>) {
		release_vec(&mut self.repos);
		self.selected_repo_idx = None;
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
		release_set(&mut self.collapsed_merges);
		release_set(&mut self.hidden_commits);
		self.selected_commit = None;
		self.range_head = None;
		self.select_head_after_load = false;
		release_vec(&mut self.commit_files);
		self.selected_commit_file = None;
		self.compare = None;
		release_vec(&mut self.files);
		self.file_tree = None;
		self.rev_tree = None;
		self.selected_file = None;
		self.selected_file_source = None;
		self.tree_cursor = 0;
		self.selected_list_row = 0;
		self.preview = None;
		self.preview_loading = false;
		self.preview_error = None;
		self.reader.release_retained();
		release_map(&mut self.basket);
		self.paste_detail = None;
		self.invalidate_paste_job();
		if !self.paste_busy() {
			self.paste_preview = None;
		}
		self.discovery = None;
		self.discovery_status = None;
		release_vec(&mut self.discovery_errors);
		release_vec(&mut self.discovery_depth_limited);
		self.discovery_error_overflow = 0;
		self.discovery_depth_overflow = 0;
		self.pinned_repo = None;
		release_vec(&mut self.manual_repos);
		self.tree_queue = VecDeque::new();
		self.tree_worker_alive = false;
		release_vec(&mut self.restore_expanded);
		self.add_cancel = None;
		self.is_loading = false;
		self.is_copying = false;
		self.is_adding_repo = false;
		self.popover = None;
		self.popover_cursor = 0;
		self.history_cancel = None;
		self.preview_cancel = None;
		self.tree_cancel = None;
		self.rev_tree_cancel = None;
		self.repo_cancel = None;
		self.scan_cancel = None;
		self.copy_cancel = None;
		release_path(&mut self.workspace_root);
		self.clear_workspace_inputs(cx);
	}

	/// Drops workspace text without `InputEvent::Changed`, which would start
	/// a find or a log search.
	fn clear_workspace_inputs(&mut self, cx: &mut Context<Self>) {
		for input in [
			self.find_input.clone(),
			self.goto_input.clone(),
			self.log_search_input.clone(),
			self.selector_input.clone(),
			self.add_repo_input.clone(),
			self.workspace_path_input.clone(),
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
		self.release_workspace_state(cx);
		self.workspace_root = path.clone();
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

	pub fn toggle_workspace_menu(&mut self, cx: &mut Context<Self>) {
		self.workspace_menu = !self.workspace_menu;
		if !self.workspace_menu {
			self.workspace_picker = false;
		}
		cx.notify();
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
		let path = PathBuf::from(text);
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
		let ws = self.workspace_root.clone();
		self.launch_fresh_discovery(ws, true, cx);
	}

	pub fn continue_discovery(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work() || self.is_loading {
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

	fn merge_repo_entries(&mut self, extra: Vec<RepoEntry>) {
		for entry in extra {
			let key = repo_key(&entry);
			if self.repos.iter().any(|have| repo_key(have) == key) {
				continue;
			}
			self.repos.push(entry);
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
		let Some(pos) =
			self.repos.iter().position(|entry| repo_key(entry) == key)
		else {
			if self
				.selected_repo_idx
				.is_some_and(|idx| idx >= self.repos.len())
			{
				self.selected_repo_idx = None;
			}
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
		let errors = self.repos.iter().filter(|r| r.summary.is_err()).count();
		self.set_status(
			"status_repos_loaded",
			[self.repos.len().to_string(), errors.to_string()],
		);
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
		if let Some(pos) =
			self.repos.iter().position(|have| repo_key(have) == key)
		{
			self.pinned_repo = Some(key);
			self.select_repo_internal(pos, false, cx);
			return;
		}
		if !self.manual_repos.iter().any(|have| repo_key(have) == key) {
			self.manual_repos.push(entry.clone());
		}
		self.repos.push(entry);
		Self::disambiguate_repo_names(&mut self.repos, &self.workspace_root);
		self.repos
			.sort_by(|a, b| a.name.cmp(&b.name).then(a.root.cmp(&b.root)));
		if let Some(pos) =
			self.repos.iter().position(|have| repo_key(have) == key)
		{
			self.pinned_repo = Some(key);
			self.select_repo_internal(pos, false, cx);
		}
		cx.notify();
	}

	pub fn select_repo(&mut self, idx: usize, cx: &mut Context<Self>) {
		if let Some(entry) = self.repos.get(idx) {
			self.pinned_repo = Some(repo_key(entry));
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
				self.select_file(&rel, cx);
				return;
			}
			TreeCommand::ToggleSelect(key) => {
				if let Some(rel) = key.utf8_rel() {
					app_log!("[APP:TREE_TOGGLED: {}]", rel);
				}
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
				self.select_file(&rel, cx);
				app_log!("[APP:TREE_FILE_SELECTED: {}]", rel);
			}
			Some(TreeEffect::Idle) => {
				self.remember_tree_selection();
				cx.notify();
			}
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
					let result = bg
						.spawn(async move {
							let result =
								crate::tree::execute_tree_io(io, &cancel_bg);
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
		self.remember_tree_selection();
	}

	fn next_tree_io(&mut self) -> Option<TreeIo> {
		if let Some(io) = self.tree_queue.pop_front() {
			return Some(io);
		}
		while let Some(rel) = self.restore_expanded.first().cloned() {
			self.restore_expanded.remove(0);
			let key = NodeKey::from_utf8_rel(&rel);
			let tree = self.file_tree.as_mut()?;
			if !tree.contains_dir(&key) {
				continue;
			}
			if let TreeEffect::Io(io) = tree.start(TreeCommand::Expand(key)) {
				return Some(io);
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
		self.remember_tree_selection();
		self.generation += 1;
		self.preview_generation += 1;
		self.history_generation += 1;
		self.tree_generation += 1;
		let task_generation = self.generation;
		self.preview_loading = false;
		let _ = arm_cancel(&mut self.preview_cancel);
		let _ = arm_cancel(&mut self.tree_cancel);
		let _ = arm_cancel(&mut self.rev_tree_cancel);
		let _ = arm_cancel(&mut self.history_cancel);
		let cancel = arm_cancel(&mut self.repo_cancel);

		let anchor_file = if preserve_anchors {
			self.selected_file.clone()
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

		self.selected_repo_idx = Some(idx);
		self.selected_file = anchor_file.clone();
		self.selected_commit = anchor_commit.clone();
		self.range_head = None;
		self.compare = None;
		self.commit_files.clear();
		self.commits.clear();
		self.refs.clear();
		self.head_sha = None;
		self.graph_layout = None;
		self.files.clear();
		self.commit_page = 0;
		self.page_checkpoints = vec![None];
		self.active_ref_filter = None;
		self.log_search = None;
		self.collapsed_merges.clear();
		self.hidden_commits.clear();
		self.history_error = None;
		if !preserve_anchors {
			self.preview = None;
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
		if let Err(e) = &self.repos[idx].summary {
			self.preview_error =
				Some(Msg::new("error_repo_status", [e.clone()]));
		}

		let saved_files = self.file_paths_in_basket(&repo_root);
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

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();

		let repo_root_for_update = repo_root.clone();
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let working_res: Result<Vec<WorkingChangeTuple>, String> = bg
					.spawn(async move {
						let opts = interactive_read_opts(cancel_bg);
						let git = Git::open_with(&repo_root, &opts)
							.map_err(|e| e.to_string())?;
						let details = status_details(&git, &opts)
							.map_err(|e| e.to_string())?;
						let mut items = Vec::new();
						for (p, ct) in details.staged {
							items.push((p, ct, SourceKind::Staged, false));
						}
						for (p, ct) in details.unstaged {
							items.push((p, ct, SourceKind::Unstaged, false));
						}
						for p in details.untracked {
							items.push((
								p,
								Some(ChangeType::New),
								SourceKind::Working,
								false,
							));
						}
						for p in details.conflicted {
							items.push((
								p,
								Some(ChangeType::Modified),
								SourceKind::Working,
								true,
							));
						}
						items.sort_by(|a, b| a.0.cmp(&b.0));
						Ok(items)
					})
					.await;

				let _ = this.update(&mut async_app, |model, cx| {
					if model.generation != task_generation {
						return;
					}
					match working_res {
						Ok(changes) => {
							let canonical =
								CanonicalRootId::new(&repo_root_for_update)
									.ok();
							let basket_items = canonical
								.as_ref()
								.and_then(|c| model.basket.get(c));
							model.files = changes
								.into_iter()
								.map(
									|(
										path,
										change_type,
										source,
										is_conflict,
									)| {
										let selected = basket_items
											.is_some_and(|items| {
												items.iter().any(|i| {
													i.relative_path == path
														&& i.source == source
												})
											});
										FileChangeItem {
											path,
											change_type,
											source,
											is_conflict,
											selected,
										}
									},
								)
								.collect();
							app_log!(
								"[APP:REPO_LOADED: {} files={}]",
								repo_name,
								model.files.len()
							);
							model.set_status(
								"status_repo_loaded",
								[
									repo_name.clone(),
									model.files.len().to_string(),
								],
							);
							if let Some(ref anchor) = anchor_file {
								if model.files.iter().any(|f| &f.path == anchor)
								{
									model.select_file(anchor, cx);
								} else if let Some(first) = model.files.first()
								{
									let first_path = first.path.clone();
									model.select_file(&first_path, cx);
								} else {
									model.selected_file = None;
									model.preview = None;
									if model.mode == "preview" {
										ready_marker("PREVIEW");
									}
								}
							} else if let Some(first) = model.files.first() {
								let first_path = first.path.clone();
								model.select_file(&first_path, cx);
							} else if model.mode == "preview" {
								ready_marker("PREVIEW");
							}
						}
						Err(err) => {
							app_log!("[APP:REPO_ERROR: {}]", repo_name);
							model.set_status(
								"error_repo_changes",
								[repo_name.clone(), err.clone()],
							);
							model.preview_error = Some(Msg::new(
								"error_repo_changes",
								[repo_name.clone(), err],
							));
						}
					}
					model.load_history(cx);
					cx.notify();
				});
			},
		);
	}

	/// Selects a file, using its known source identity if in the changes list.
	pub fn select_file(&mut self, path: &str, cx: &mut Context<Self>) {
		let source = self
			.files
			.iter()
			.find(|f| f.path == path)
			.map(|f| f.source.clone())
			.unwrap_or(SourceKind::Working);
		self.select_file_with_source(path, source, cx);
	}

	/// Selects a file item directly from the changes list.
	pub fn select_file_item(
		&mut self,
		item: &FileChangeItem,
		cx: &mut Context<Self>,
	) {
		self.select_file_with_source(&item.path, item.source.clone(), cx);
	}

	/// Opens a working-tree file (Project) or its staged/unstaged changes (Changes).
	pub fn select_file_with_source(
		&mut self,
		path: &str,
		source: SourceKind,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.selected_file = Some(path.to_string());
		self.selected_file_source = Some(source.clone());
		self.selected_commit = None;
		self.range_head = None;
		self.compare = None;
		self.commit_files.clear();
		let Some(repo_root) = self.repo_root() else {
			return;
		};
		let file_path = path.to_string();
		self.preview_loading = true;
		self.preview_error = None;
		let is_tree = self.active_tab == WorkbenchTab::FileExplorer;

		let cancel = arm_cancel(&mut self.preview_cancel);
		let fs_only = is_tree || matches!(source, SourceKind::File);
		let kind = if fs_only {
			lifecycle::JobKind::UncancellableRead
		} else {
			lifecycle::JobKind::CancellableRead
		};
		let job_cancel = (!fs_only).then(|| cancel.clone());
		let delay = self.e2e_read_delay;
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
					let result = read_preview(
						&repo_root, &for_bg, &source, is_tree, cancel,
					);
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
				model.apply_source_preview(file_path.clone(), result);
				app_log!("[APP:PREVIEW_LOADED: {}]", file_path);
				if model.mode == "preview" {
					ready_marker("PREVIEW");
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
	) {
		match result {
			Ok((p, source)) => {
				if !p.patch.is_empty() {
					self.set_preview(Preview::new(
						source,
						Some(path),
						p.patch,
						true,
						Language::Diff,
					));
				} else if let Some(content) = p.content {
					let lang = Language::from_path_or_ext(&path, false);
					self.set_preview(Preview::new(
						source,
						Some(path),
						content,
						false,
						lang,
					));
				} else {
					self.preview = None;
					self.preview_loading = false;
					self.preview_error = Some(Msg::new("error_binary", [path]));
				}
			}
			Err(e) => {
				self.preview = None;
				self.preview_loading = false;
				self.preview_error = Some(Msg::new("error_preview", [path, e]));
			}
		}
	}

	fn source_summary(source: &SourceKind) -> String {
		match source {
			SourceKind::Staged => "staged".to_string(),
			SourceKind::Unstaged => "unstaged".to_string(),
			SourceKind::Working => "untracked".to_string(),
			SourceKind::File => "file".to_string(),
			SourceKind::Commit { rev } => {
				let short = &rev[..7.min(rev.len())];
				format!("commit@{short}")
			}
		}
	}

	fn set_basket_group(
		&mut self,
		root: CanonicalRootId,
		group: BasketGroup,
		new_items: Vec<ExportItem>,
	) {
		let mut kept = self.basket.remove(&root).unwrap_or_default();
		kept.retain(|item| match group {
			BasketGroup::File => !matches!(item.source, SourceKind::File),
			BasketGroup::GitChanges => !matches!(
				item.source,
				SourceKind::Working | SourceKind::Unstaged | SourceKind::Staged
			),
		});
		kept.extend(new_items);
		if !kept.is_empty() {
			self.basket.insert(root, kept);
		}
		self.log_basket();
	}

	pub fn is_rev_file_selected(&self, sha: &str, path: &str) -> bool {
		let Some(repo) = self.repo() else {
			return false;
		};
		let Ok(root) = CanonicalRootId::new(&repo.root) else {
			return false;
		};
		let Some(items) = self.basket.get(&root) else {
			return false;
		};
		items.iter().any(|item| {
			item.relative_path == path
				&& matches!(&item.source, SourceKind::Commit { rev } if rev == sha)
		})
	}

	pub fn toggle_rev_file_selection(
		&mut self,
		sha: &str,
		path: &str,
		cx: &mut Context<Self>,
	) {
		let Some(repo) = self.repo() else {
			return;
		};
		let Ok(root) = CanonicalRootId::new(&repo.root) else {
			return;
		};
		let target_source = SourceKind::Commit {
			rev: sha.to_string(),
		};
		let items = self.basket.entry(root.clone()).or_default();
		if let Some(pos) = items.iter().position(|item| {
			item.relative_path == path && item.source == target_source
		}) {
			items.remove(pos);
			if items.is_empty() {
				self.basket.remove(&root);
			}
			app_log!(
				"[APP:REV_FILE_TOGGLED: sha={} path={} selected=false]",
				&sha[..7.min(sha.len())],
				path
			);
		} else {
			items.push(ExportItem {
				root: root.clone(),
				relative_path: path.to_string(),
				source: target_source,
				change_type: None,
			});
			app_log!(
				"[APP:REV_FILE_TOGGLED: sha={} path={} selected=true]",
				&sha[..7.min(sha.len())],
				path
			);
		}
		self.log_basket();
		cx.notify();
	}

	fn log_basket(&self) {
		app_log!(
			"[APP:BASKET: n={} summary={}]",
			self.basket_count(),
			self.basket_summary()
		);
		if let Some(collision) = self.basket_collision_text() {
			app_log!("[APP:BASKET_COLLISION: {}]", collision);
		}
	}

	pub fn basket_count(&self) -> usize {
		self.basket.values().map(|items| items.len()).sum()
	}

	fn basket_summary_with<F>(&self, format_source: F) -> String
	where
		F: Fn(&SourceKind) -> String,
	{
		let mut parts = Vec::new();
		let mut rows: Vec<_> = self.basket.iter().collect();
		rows.sort_by_key(|(root, _)| {
			root.path().to_string_lossy().into_owned()
		});
		for (root, items) in rows {
			let name = self
				.repos
				.iter()
				.find(|repo| {
					CanonicalRootId::new(&repo.root).ok().as_ref() == Some(root)
				})
				.map(|repo| repo.name.clone())
				.unwrap_or_else(|| root.path().display().to_string());
			let mut items = items.clone();
			items.sort_by(|a, b| {
				(&a.relative_path, Self::source_summary(&a.source))
					.cmp(&(&b.relative_path, Self::source_summary(&b.source)))
			});
			for item in items {
				parts.push(format!(
					"{name} {} {}",
					format_source(&item.source),
					item.relative_path
				));
			}
		}
		parts.join("; ")
	}

	pub fn basket_summary(&self) -> String {
		self.basket_summary_with(Self::source_summary)
	}

	pub fn basket_summary_localized(&self, loc: Locale) -> String {
		self.basket_summary_with(|source| match source {
			SourceKind::File => i18n::t("src_working_file", loc).to_string(),
			SourceKind::Working => i18n::t("group_untracked", loc).to_string(),
			SourceKind::Unstaged => i18n::t("tag_unstaged", loc).to_string(),
			SourceKind::Staged => i18n::t("tag_staged", loc).to_string(),
			SourceKind::Commit { rev } => {
				let short = &rev[..7.min(rev.len())];
				let tmpl = i18n::t("src_commit_short", loc);
				if tmpl.contains("{}") {
					tmpl.replace("{}", short)
				} else {
					format!("commit@{short}")
				}
			}
		})
	}

	/// Two selections of one path cannot share a wire header. File rows count;
	/// a matching basename is not a reason to drop one of them.
	pub fn basket_collision_text(&self) -> Option<String> {
		let mut seen: HashMap<(String, String), usize> = HashMap::new();
		for (root, items) in &self.basket {
			for item in items {
				*seen
					.entry((
						root.path().display().to_string(),
						item.relative_path.clone(),
					))
					.or_default() += 1;
			}
		}
		let mut hits = Vec::new();
		for ((root, path), count) in seen {
			if count > 1 {
				hits.push(format!("{root}:{path}"));
			}
		}
		hits.sort();
		if hits.is_empty() {
			None
		} else {
			Some(hits.join(", "))
		}
	}

	fn file_paths_in_basket(&self, root: &std::path::Path) -> HashSet<String> {
		let Ok(id) = CanonicalRootId::new(root) else {
			return HashSet::new();
		};
		self.basket
			.get(&id)
			.map(|items| {
				items
					.iter()
					.filter(|item| matches!(item.source, SourceKind::File))
					.map(|item| item.relative_path.clone())
					.collect()
			})
			.unwrap_or_default()
	}

	/// Keeps Project checkboxes without replacing Git change entries.
	pub fn remember_tree_selection(&mut self) {
		let Some(repo) = self.repo() else {
			return;
		};
		let Ok(id) = CanonicalRootId::new(&repo.root) else {
			return;
		};
		let mut paths = Vec::new();
		if let Some(tree) = &self.file_tree {
			tree.collect_selected_paths(&mut paths);
		}
		paths.sort();
		paths.dedup();
		let items = paths
			.into_iter()
			.map(|path| ExportItem {
				root: id.clone(),
				relative_path: path,
				source: SourceKind::File,
				change_type: None,
			})
			.collect();
		self.set_basket_group(id, BasketGroup::File, items);
	}

	pub fn reapply_tree_selection(&mut self) {
		let Some(root) = self.repo_root() else {
			return;
		};
		let saved = self.file_paths_in_basket(&root);
		if let Some(tree) = self.file_tree.as_mut() {
			tree.apply_selection(&saved);
		}
	}

	pub fn sync_git_selection_to_basket(&mut self) {
		let Some(repo) = self.repo() else {
			return;
		};
		let Ok(id) = CanonicalRootId::new(&repo.root) else {
			return;
		};
		let items = self
			.files
			.iter()
			.filter(|file| file.selected)
			.map(|file| ExportItem {
				root: id.clone(),
				relative_path: file.path.clone(),
				source: file.source.clone(),
				change_type: file.change_type,
			})
			.collect();
		self.set_basket_group(id, BasketGroup::GitChanges, items);
	}

	pub fn clear_basket(&mut self, cx: &mut Context<Self>) {
		self.basket.clear();
		for file in &mut self.files {
			file.selected = false;
		}
		if let Some(tree) = self.file_tree.as_mut() {
			tree.set_all_selected(false);
		}
		self.log_basket();
		app_log!("[APP:BASKET_CLEARED]");
		self.set_status("basket_cleared", []);
		cx.notify();
	}

	pub fn sync_current_selection_to_basket(&mut self) {
		match self.active_tab {
			WorkbenchTab::FileExplorer => self.remember_tree_selection(),
			WorkbenchTab::GitChanges => self.sync_git_selection_to_basket(),
		}
	}

	pub fn copy_selection_to_clipboard(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		if self.is_copying {
			app_log!("[APP:COPY_BUSY]");
			return;
		}
		self.remember_tree_selection();
		self.sync_git_selection_to_basket();
		if self.basket_count() == 0
			&& self.selected_commit.is_some()
			&& self.selected_file.is_none()
		{
			app_log!("[APP:COPY_REFUSED: commit_readonly]");
			self.set_status("btn_copy_commit_readonly", []);
			cx.notify();
			return;
		}
		if let Some(collision) = self.basket_collision_text() {
			app_log!("[APP:COPY_REFUSED: collision]");
			self.set_status("basket_collision", [collision]);
			cx.notify();
			return;
		}
		let repo_name = self
			.repo()
			.map(|repo| repo.name.clone())
			.unwrap_or_else(|| "basket".into());
		let mut items = Vec::new();
		let mut roots = Vec::new();
		for repo_items in self.basket.values() {
			for item in repo_items {
				let path = item.root.path().to_path_buf();
				if !roots.iter().any(|root| root == &path) {
					roots.push(path);
				}
				items.push(item.clone());
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
						let settings = Settings::default();
						// Document cap is the retained UI output ceiling.
						// It is not `RunOptions::max_stdout`.
						let plan = plan_export_with(
							&export_sel,
							&settings,
							Some(RunOptions::INTERACTIVE_MAX_STDOUT),
							&opts,
						)
						.map_err(|e| {
							Msg::new("error_payload", [e.to_string()])
						})?;
						if plan.files.is_empty() {
							return Err(Msg::new("status_copy_nothing", []));
						}
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
						plan.revalidate_with(&opts).map_err(|e| {
							let reason = match &e {
								snip_core::transfer::TransferError::StaleSource { .. } => "stale_source",
								_ => "revalidate",
							};
							if e2e_on() {
								app_log!("[APP:COPY_FAILED: {reason}]");
							}
							Msg::new("error_payload", [e.to_string()])
						})?;
						let skipped = plan.skipped_unreadable_count
							+ plan.skipped_file_size_count;
						let msg = Msg::new(
							"status_copied",
							[
								repo_name,
								plan.copied_file_count.to_string(),
								plan.stats.chars.to_string(),
								plan.stats.lines.to_string(),
								skipped.to_string(),
							],
						);
						Ok((plan.payload, plan.copied_file_count, msg))
					})
					.await;

				match this.update(&mut async_app, |model, cx| {
					if !model.accept_copy_result(ws_gen, &cancel) {
						cx.notify();
						return;
					}
					match result {
						Ok((text, copied_count, msg)) => {
							if let Err(e) = clip::write_text(&text) {
								model.set_status(
									"status_clipboard_failed",
									[e.to_string()],
								);
							} else {
								app_log!(
									"[APP:COPY_DONE: copied={copied_count}]"
								);
								model.status = msg;
							}
						}
						Err(err) => {
							model.status = err;
						}
					}
					app_log!("[APP:COPY_IDLE]");
					cx.notify();
				}) {
					Ok(()) => {}
					Err(err) => {
						app_log!("[APP:COPY_IDLE_FAILED: {err}]");
					}
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
		let Some(repo_root) = self.repo_root() else {
			return;
		};
		let repo_name = self.repo().map(|r| r.name.clone()).unwrap_or_default();
		let rows = self.display_commits();
		let (tip_sha, selected) = if let Some((top, bottom)) = self.range_rows()
		{
			let slice = &rows[top..=bottom];
			let tip = slice[0].sha.clone();
			let selected = slice.iter().map(|c| c.sha.clone()).collect();
			(tip, selected)
		} else if let Some(ref sel) = self.selected_commit {
			if rows.iter().any(|c| &c.sha == sel) {
				(sel.clone(), vec![sel.clone()])
			} else {
				app_log!("[APP:COPY_COMMITS_REFUSED: no_selection]");
				self.set_status("status_copy_empty", []);
				cx.notify();
				return;
			}
		} else {
			app_log!("[APP:COPY_COMMITS_REFUSED: no_selection]");
			self.set_status("status_copy_empty", []);
			cx.notify();
			return;
		};
		drop(rows);

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
				let result: Result<(String, usize), String> = bg
					.spawn(async move {
						let opts = interactive_read_opts(run_token);
						let git = Git::open_with(&repo_root, &opts)
							.map_err(|e| e.to_string())?;
						let exported = plan_commit_export_exact_with(
							&git,
							&tip_sha,
							&selected,
							&opts,
							RunOptions::INTERACTIVE_MAX_STDOUT,
						)
						.map_err(|e| e.to_string())?;
						let n_commits = exported.payload.commits.len();
						Ok((exported.text, n_commits))
					})
					.await;

				let _ = this.update(&mut async_app, |model, cx| {
					if !model.accept_copy_result(ws_gen, &cancel) {
						cx.notify();
						return;
					}
					match result {
						Ok((text, n_commits)) => {
							if let Err(e) = clip::write_text(&text) {
								model.set_status(
									"status_clipboard_failed",
									[e.to_string()],
								);
							} else {
								app_log!(
									"[APP:COPY_COMMITS_DONE: commits={n_commits}]"
								);
								model.set_status(
									"status_commits_copied",
									[n_commits.to_string()],
								);
							}
						}
						Err(err) => {
							app_log!("[APP:COPY_COMMITS_ERR: {err}]");
							model.set_status("error_payload", [err]);
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// True while a confirmed plan is being written. The write is not
	/// cancellable, so every control that would change or discard the plan
	/// is refused until it finishes.
	pub fn paste_busy(&self) -> bool {
		self.paste_preview.as_ref().is_some_and(|p| p.is_applying)
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

	/// Cancels the read-only paste job in flight, if any, and makes its
	/// result stale. Does not touch a confirmed write.
	fn invalidate_paste_job(&mut self) {
		if let Some(token) = self.paste_cancel.take() {
			token.cancel();
		}
		self.paste_generation = self.paste_generation.wrapping_add(1);
		self.paste_loading = false;
	}

	/// Arms a new read-only paste job and returns what its result must match.
	fn arm_paste_job(&mut self) -> (CancelToken, u64, u64) {
		self.invalidate_paste_job();
		let cancel = arm_cancel(&mut self.paste_cancel);
		self.paste_loading = true;
		(cancel, self.paste_generation, self.lifecycle.generation())
	}

	/// True when a background paste result may be shown. A cancelled token,
	/// a newer paste job, or a workspace change drops it.
	fn accept_paste_result(
		&mut self,
		seq: u64,
		ws_gen: u64,
		token: &CancelToken,
	) -> bool {
		let cancelled = token.is_cancelled();
		let stale = self.paste_generation != seq
			|| self.lifecycle.generation() != ws_gen;
		if !cancelled && !stale {
			self.paste_loading = false;
			return true;
		}
		let why = if cancelled { "cancelled" } else { "stale" };
		app_log!("[APP:PASTE_DISCARDED: {why}]");
		false
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
		self.invalidate_paste_job();
		if self.paste_preview.take().is_some() {
			app_log!("[APP:PASTE_PLAN_CLEARED]");
		}
		self.paste_detail = None;
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

		let target_dest = self.current_restore_destination();
		let known_roots: Vec<std::path::PathBuf> =
			self.repos.iter().map(|r| r.root.clone()).collect();
		let generation = self.generation;
		let (cancel, seq, ws_gen) = self.arm_paste_job();
		self.set_status("paste_loading", []);
		app_log!("[APP:PASTE_LOADING]");
		// The loading panel owns Escape, so the read can be cancelled.
		self.pending_focus = Some(self.paste_focus.clone());
		cx.notify();

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();
		let dest_bg = target_dest.clone();
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel.clone()),
			async move {
				let built = bg
					.spawn(async move {
						let opts = interactive_read_opts(cancel_bg);
						PastePreviewPlan::build_from_clipboard_text_with(
							&text,
							&dest_bg,
							&known_roots,
							generation,
							&opts,
						)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if !model.accept_paste_result(seq, ws_gen, &cancel) {
						cx.notify();
						return;
					}
					model.show_paste_plan(built, &target_dest);
					cx.notify();
				});
			},
		);
	}

	fn show_paste_plan(
		&mut self,
		built: Result<PastePreviewPlan, Msg>,
		target_dest: &std::path::Path,
	) {
		match built {
			Ok(plan) => {
				for choice in &plan.prefix_choices {
					for (idx, path) in choice.candidates.iter().enumerate() {
						app_log!(
							"[APP:PASTE_MAP_CANDIDATE: prefix={} idx={} path={}]",
							choice.prefix,
							idx,
							path.display()
						);
					}
				}
				app_log!(
					"[APP:PASTE_PREVIEW: items={} dest={} mapping={}]",
					plan.items.len(),
					target_dest.display(),
					plan.mapping_ready()
				);
				self.set_status(
					"status_paste_preview",
					[plan.items.len().to_string()],
				);
				self.paste_preview = Some(plan);
				if collapse_log_for_paste(
					&mut self.log_before_paste,
					&mut self.bottom_visible,
				) {
					app_log!(
						"[APP:LOG_PANEL: visible=false reason=paste_open]"
					);
				}
				self.refresh_paste_detail();
				self.pending_focus = Some(self.paste_focus.clone());
			}
			Err(err) => {
				app_log!("[APP:PASTE_ERR: {}]", err.key);
				self.restore_log_after_paste();
				self.status = err;
				self.pending_focus = Some(self.focus_handle.clone());
			}
		}
	}

	pub fn choose_paste_keep(&mut self, prefix: &str, cx: &mut Context<Self>) {
		if self.refuse_while_applying("mapping", cx) {
			return;
		}
		let Some(plan) = self.paste_preview.as_mut() else {
			return;
		};
		match plan.choose_keep_relative(prefix) {
			Ok(()) => self.rebuild_paste_plan(prefix.to_string(), true, cx),
			Err(err) => {
				app_log!("[APP:PASTE_ERR: {}]", err.key);
				plan.error = Some(err);
				cx.notify();
			}
		}
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
		let Some(dest) = self.paste_preview.as_ref().and_then(|plan| {
			plan.prefix_choices
				.iter()
				.find(|c| c.prefix == prefix)
				.and_then(|choice| {
					choice.candidates.get(candidate_idx).cloned()
				})
		}) else {
			return;
		};
		let Some(plan) = self.paste_preview.as_mut() else {
			return;
		};
		match plan.choose_prefix_destination(prefix, &dest) {
			Ok(()) => self.rebuild_paste_plan(prefix.to_string(), false, cx),
			Err(err) => {
				app_log!("[APP:PASTE_ERR: {}]", err.key);
				plan.error = Some(err);
				cx.notify();
			}
		}
	}

	/// Replans the writes for the mapping just chosen. The choice is already
	/// on the visible plan and its old writes are dropped here, so neither a
	/// second choice nor Apply can act on the previous mapping.
	fn rebuild_paste_plan(
		&mut self,
		prefix: String,
		keep: bool,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let Some(plan) = self.paste_preview.as_mut() else {
			return;
		};
		plan.clear_file_plan();
		plan.error = None;
		let mut work = plan.clone();
		self.paste_detail = None;
		let (cancel, seq, ws_gen) = self.arm_paste_job();
		self.set_status("paste_loading", []);
		app_log!("[APP:PASTE_LOADING]");
		cx.notify();

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel.clone()),
			async move {
				let (work, rebuilt) = bg
					.spawn(async move {
						let opts = interactive_read_opts(cancel_bg);
						let rebuilt = work.rebuild_file_plan_with(&opts);
						(work, rebuilt)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if !model.accept_paste_result(seq, ws_gen, &cancel) {
						cx.notify();
						return;
					}
					match rebuilt {
						Ok(()) => {
							let dest = prefix_target(&work, &prefix, keep);
							let keep_note =
								if keep { " keep=primary" } else { "" };
							app_log!(
								"[APP:PASTE_MAPPED: prefix={}{} dest={} items={}]",
								prefix,
								keep_note,
								dest,
								work.items.len()
							);
							model.set_status(
								"status_paste_preview",
								[work.items.len().to_string()],
							);
							model.paste_preview = Some(work);
							model.refresh_paste_detail();
						}
						Err(err) => {
							app_log!("[APP:PASTE_ERR: {}]", err.key);
							if let Some(plan) = model.paste_preview.as_mut() {
								plan.error = Some(err);
							}
						}
					}
					cx.notify();
				});
			},
		);
	}

	pub fn toggle_paste_overwrite(
		&mut self,
		idx: usize,
		cx: &mut Context<Self>,
	) {
		if self.refuse_while_applying("overwrite", cx) || self.paste_loading {
			return;
		}
		if let Some(ref mut p) = self.paste_preview {
			p.toggle_overwrite(idx);
			let st = p
				.items
				.get(idx)
				.map(|i| i.overwrite_allowed)
				.unwrap_or(false);
			app_log!("[APP:PASTE_TOGGLED: idx={} state={}]", idx, st);
			cx.notify();
		}
	}

	pub fn toggle_paste_selected(
		&mut self,
		idx: usize,
		cx: &mut Context<Self>,
	) {
		if self.refuse_while_applying("include", cx) || self.paste_loading {
			return;
		}
		if let Some(ref mut p) = self.paste_preview {
			p.toggle_selected(idx);
			let st = p.items.get(idx).map(|i| i.selected).unwrap_or(false);
			app_log!("[APP:PASTE_SEL_TOGGLED: idx={} state={}]", idx, st);
			cx.notify();
		}
	}

	pub fn select_paste_item(&mut self, idx: usize, cx: &mut Context<Self>) {
		// Read-only navigation stays available while applying.
		if let Some(ref mut p) = self.paste_preview {
			if idx < p.items.len() {
				p.selected_item_idx = idx;
				app_log!("[APP:PASTE_NAV: idx={}]", idx);
				self.refresh_paste_detail();
				cx.notify();
			}
		}
	}

	/// Builds the one retained detail text for the selected paste item.
	fn refresh_paste_detail(&mut self) {
		self.paste_detail = self
			.paste_preview
			.as_ref()
			.and_then(|p| p.items.get(p.selected_item_idx))
			.filter(|i| !i.is_delete)
			.map(|i| {
				Preview::new(
					PreviewSource::PasteItem,
					Some(i.path.clone()),
					i.content.to_string(),
					false,
					Language::from_path_or_ext(&i.path, false),
				)
			});
		self.paste_scroll
			.scroll_to_item(0, gpui::ScrollStrategy::Top);
	}

	pub fn apply_paste_restore(&mut self, cx: &mut Context<Self>) {
		if self.paste_loading {
			app_log!("[APP:APPLY_IGNORED: loading]");
			self.set_status("paste_loading_refused", []);
			cx.notify();
			return;
		}
		let Some(ref mut plan) = self.paste_preview else {
			app_log!("[APP:APPLY_IGNORED: no_plan]");
			return;
		};
		if plan.is_applying {
			app_log!("[APP:PASTE_BUSY: refused=apply]");
			return;
		}
		if !plan.mapping_ready() {
			app_log!("[APP:PASTE_ERR: mapping_required]");
			plan.error = Some(Msg::new("mapping_required", []));
			self.set_status("mapping_required", []);
			cx.notify();
			return;
		}

		plan.is_applying = true;
		self.pending_focus = Some(self.paste_focus.clone());
		// The worker gets a cheap handle: the plan's contents are shared.
		let plan_clone = plan.clone();
		self.set_status("paste_apply_busy", []);
		app_log!("[APP:PASTE_APPLYING]");
		cx.notify();

		let delay = self.e2e_apply_delay;
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
						plan_clone.execute()
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
							if result.created_commits.is_empty() {
								model.set_status(
									"status_paste_done",
									[
										result.files.created_count.to_string(),
										result
											.files
											.overwritten_count
											.to_string(),
										result
											.files
											.skipped_existing_count
											.to_string(),
										result.files.deleted_count.to_string(),
										result.files.errors.len().to_string(),
									],
								);
							} else {
								model.set_status(
									"commit_replay_done",
									[result.created_commits.join(", ")],
								);
							}
							model.paste_preview = None;
							model.restore_log_after_paste();
							model.pending_focus =
								Some(model.focus_handle.clone());
							if let Some(idx) = model.selected_repo_idx {
								model.select_repo(idx, cx);
							}
						}
						Err(err) => {
							app_log!("[APP:PASTE_STALE_DETECTED: {}]", err.key);
							model.status = err.clone();
							if let Some(ref mut p) = model.paste_preview {
								p.is_applying = false;
								p.error = Some(err);
							}
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// Called on every path that closes the paste preview.
	fn restore_log_after_paste(&mut self) {
		if restore_log_after_paste(
			&mut self.log_before_paste,
			&mut self.bottom_visible,
		) {
			app_log!("[APP:LOG_PANEL: visible=true reason=paste_close]");
		}
	}

	pub fn cancel_paste_preview(&mut self, cx: &mut Context<Self>) {
		if self.refuse_while_applying("cancel", cx) {
			return;
		}
		let was_loading = self.paste_loading;
		self.invalidate_paste_job();
		if self.paste_preview.take().is_none() && !was_loading {
			return;
		}
		self.paste_detail = None;
		self.restore_log_after_paste();
		self.pending_focus = Some(self.focus_handle.clone());
		self.set_status("paste_cancelled", []);
		app_log!("[APP:PASTE_CANCELLED]");
		cx.notify();
	}

	pub fn copy_current_preview_content(&mut self, cx: &mut Context<Self>) {
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
		return plan.destination.display().to_string();
	}
	plan.prefix_choices
		.iter()
		.find(|choice| choice.prefix == prefix)
		.and_then(|choice| choice.destination.as_ref())
		.map(|dest| dest.display().to_string())
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
				model.repos.clear();
				let manual = model.manual_repos.clone();
				model.merge_repo_entries(manual);
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
	let (root, kind, identity, summary) = match Git::open_with(&path, &opts) {
		Ok(git) => {
			let root = git.root().to_path_buf();
			match RepoIdentity::resolve(&git, &opts) {
				Ok(id) => {
					let kind = match id.kind {
						RepoKind::LinkedWorktree => {
							RepoEntryKind::LinkedWorktree
						}
						RepoKind::Submodule => RepoEntryKind::Submodule,
						RepoKind::Main => RepoEntryKind::Main,
					};
					let summary =
						summarize(&git, &opts).map_err(|err| err.to_string());
					(id.toplevel.clone(), kind, Some(id), summary)
				}
				Err(err) => (
					root,
					RepoEntryKind::Main,
					None,
					Err(format!("repository identity: {err}")),
				),
			}
		}
		Err(err) => (path, RepoEntryKind::Main, None, Err(err.to_string())),
	};
	let name = root
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_else(|| root.display().to_string());
	Ok(RepoEntry {
		root,
		name,
		kind,
		identity,
		summary,
	})
}

fn read_preview(
	repo_root: &std::path::Path,
	path: &str,
	source: &SourceKind,
	is_tree: bool,
	cancel: CancelToken,
) -> Result<(browser::SourcePreview, PreviewSource), String> {
	if is_tree || matches!(source, SourceKind::File) {
		return browser::file_preview(repo_root, path)
			.map(|p| (p, PreviewSource::WorkingFile))
			.map_err(|e| e.to_string());
	}
	let opts = RunOptions {
		cancel: Some(cancel),
		max_stdout: browser::PREVIEW_LIMIT,
		overflow: Overflow::Error,
		..RunOptions::interactive(None)
	};
	let git = Git::open_with(repo_root, &opts).map_err(|e| e.to_string())?;
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
		SourceKind::File => (GitSource::Working, PreviewSource::WorkingFile),
	};
	match browser::git_preview_with(&git, &git_source, path, &opts) {
		Ok(p) => Ok((
			browser::SourcePreview {
				content: p.content,
				patch: p.patch,
			},
			preview_source,
		)),
		Err(_) => browser::file_preview(repo_root, path)
			.map(|p| (p, PreviewSource::WorkingFile))
			.map_err(|e| e.to_string()),
	}
}

fn parse_cli_args() -> (PathBuf, String, Option<PathBuf>) {
	let mut workspace =
		std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
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
					workspace = PathBuf::from(&args[i + 1]);
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
		KeyBinding::new("alt-d", DeselectAllFiles, None),
		KeyBinding::new("alt-s", SelectAllFiles, None),
		KeyBinding::new("tab", FocusNext, None),
		KeyBinding::new("shift-tab", FocusPrev, None),
		KeyBinding::new("ctrl-tab", FocusNext, None),
		KeyBinding::new("ctrl-shift-tab", FocusPrev, None),
		// IntelliJ tool window shortcuts.
		KeyBinding::new("alt-1", ShowProject, None),
		KeyBinding::new("alt-0", ShowChanges, None),
		KeyBinding::new("alt-9", ToggleLog, None),
		KeyBinding::new("alt-shift-r", OpenRepoSelector, None),
		KeyBinding::new("alt-shift-b", OpenRefSelector, None),
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
		// Git log.
		KeyBinding::new("up", LogUp, Some("GitLog")),
		KeyBinding::new("down", LogDown, Some("GitLog")),
		KeyBinding::new("shift-up", LogExtendUp, Some("GitLog")),
		KeyBinding::new("shift-down", LogExtendDown, Some("GitLog")),
		KeyBinding::new("enter", LogOpen, Some("GitLog")),
		KeyBinding::new("ctrl-f", LogSearchFocus, Some("GitLog")),
		KeyBinding::new("pagedown", HistoryNextPage, Some("GitLog")),
		KeyBinding::new("pageup", HistoryPrevPage, Some("GitLog")),
		KeyBinding::new("h", LogHead, Some("GitLog")),
	];
	b.extend(text_input::bindings());
	b
}

fn main() {
	let (workspace, mode, restore_dir) = parse_cli_args();
	let app = Application::new();

	app.run(move |cx: &mut App| {
		cx.bind_keys(key_bindings());

		let bounds = Bounds::centered(None, size(px(1080.0), px(720.0)), cx);
		let ws = workspace.clone();
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
				if app_mode == "idle" {
					ready_marker("IDLE");
				}
				let model = cx
					.new(|cx| WorkbenchModel::new(ws, paste_dir, app_mode, cx));
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

/// Hides the Git Log for the paste preview, remembering the prior state once
/// (a re-paste over an open preview keeps the first saved value). Returns
/// whether visibility changed.
fn collapse_log_for_paste(
	saved: &mut Option<bool>,
	visible: &mut bool,
) -> bool {
	saved.get_or_insert(*visible);
	std::mem::replace(visible, false)
}

/// Restores the saved visibility when the preview closes. No-op if nothing
/// was saved (already restored, or the user toggled the log) or if the log
/// is already shown again. Returns whether visibility changed.
fn restore_log_after_paste(
	saved: &mut Option<bool>,
	visible: &mut bool,
) -> bool {
	match saved.take() {
		Some(true) if !*visible => {
			*visible = true;
			true
		}
		_ => false,
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn test_paste_log_collapse_and_restore() {
		// Open then close restores the visible log, exactly once.
		let (mut saved, mut vis) = (None, true);
		assert!(super::collapse_log_for_paste(&mut saved, &mut vis));
		assert!(!vis);
		// A re-paste over the open preview keeps the first saved state.
		assert!(!super::collapse_log_for_paste(&mut saved, &mut vis));
		assert!(super::restore_log_after_paste(&mut saved, &mut vis));
		assert!(vis);
		vis = false;
		assert!(!super::restore_log_after_paste(&mut saved, &mut vis));
		assert!(!vis, "must not restore twice");

		// A log that was already hidden stays hidden.
		let (mut saved, mut vis) = (None, false);
		assert!(!super::collapse_log_for_paste(&mut saved, &mut vis));
		assert!(!super::restore_log_after_paste(&mut saved, &mut vis));
		assert!(!vis);

		// A manual toggle (toggle_log clears the saved state) wins.
		let (mut saved, mut vis) = (None, true);
		super::collapse_log_for_paste(&mut saved, &mut vis);
		saved = None;
		assert!(!super::restore_log_after_paste(&mut saved, &mut vis));
		assert!(!vis);

		// Reopened by a path that does not clear the saved state: no flip.
		let (mut saved, mut vis) = (None, true);
		super::collapse_log_for_paste(&mut saved, &mut vis);
		vis = true;
		assert!(!super::restore_log_after_paste(&mut saved, &mut vis));
		assert!(vis && saved.is_none());
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

		let mut shas = HashSet::<String>::with_capacity(16);
		shas.insert("a".repeat(40));
		release_set(&mut shas);
		assert!(shas.is_empty());
		assert_eq!(shas.capacity(), 0);

		let mut path = PathBuf::with_capacity(128);
		path.push("/tmp/workspace");
		release_path(&mut path);
		assert!(path.as_os_str().is_empty());
		assert_eq!(path.capacity(), 0);
	}
}
