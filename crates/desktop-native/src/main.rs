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
use std::sync::{Arc, Mutex, OnceLock};

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

#[cfg(test)]
fn release_map<K, V, S: Default>(slot: &mut HashMap<K, V, S>) {
	*slot = HashMap::default();
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
mod menu;
mod multi_log;
pub mod paste;
mod reader;
mod recent;
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
use snip_core::gitrun::{CancelToken, Overflow, RunOptions};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::graph::GraphLayout;
use snip_core::settings::Settings;
use snip_core::transfer::{
	plan_commit_export_exact_with, plan_export_with, CanonicalRootId,
	ExportItem, ExportSelection, SourceKind,
};
use snip_core::workspace::{
	declared_submodules, status_details, summarize, summarize_with_details,
	summarize_with_identity, DiscoveredRepo, Discovery, RepoIdentity, RepoKind,
	RepoSummary, ScanBudget, ScanStatus, SubmoduleState,
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
	pub selected: bool,
	/// Index of the owning repo in `WorkbenchModel::change_repos`.
	pub repo: u32,
}

impl FileChangeItem {
	/// False when Git's path bytes were not UTF-8: the lossy name here does
	/// not exist on disk, so selecting it would make the whole Copy fail.
	// ponytail: core's status listing is already lossy, so U+FFFD is the only
	// signal (a real U+FFFD name is refused too); carry raw bytes from core to
	// tell them apart.
	pub fn is_valid_utf8(&self) -> bool {
		!self.path.contains(char::REPLACEMENT_CHARACTER)
	}
}

type WorkingChangeTuple = (String, Option<ChangeType>, SourceKind, bool);

/// Rows kept per repo in the Changes tool window. Every repo's rows count
/// against `MAX_RETAINED_TREE_BYTES` and each checkbox click clones them, so
/// 15 repos at this cap stay well inside that budget.
pub const MAX_CHANGES_PER_REPO: usize = 2_000;

/// Status reads the Changes queue runs at once (the Git runner's own limit).
pub const MAX_CHANGES_READS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeRepoState {
	Loading,
	Loaded,
	Failed(String),
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
	root: &std::path::Path,
	known: Option<&RepoIdentity>,
	cancel: CancelToken,
) -> Result<(Option<RepoSummary>, Vec<WorkingChangeTuple>), String> {
	let opts = interactive_read_opts(cancel);
	// A known identity skips the open and the identity reads, and one
	// status feeds both the list and the repo's summary.
	let (summary, details) = match known {
		Some(id) => {
			let git = Git::at_known_root(id.toplevel.clone());
			let (summary, details) = summarize_with_details(&git, id, &opts)
				.map_err(|e| e.to_string())?;
			(Some(summary), details)
		}
		None => {
			let git = Git::open_with(root, &opts).map_err(|e| e.to_string())?;
			let details =
				status_details(&git, &opts).map_err(|e| e.to_string())?;
			(None, details)
		}
	};
	let mut items = Vec::new();
	for (p, ct) in details.staged {
		items.push((p, ct, SourceKind::Staged, false));
	}
	for (p, ct) in details.unstaged {
		items.push((p, ct, SourceKind::Unstaged, false));
	}
	for p in details.untracked {
		items.push((p, Some(ChangeType::New), SourceKind::Working, false));
	}
	for p in details.conflicted {
		items.push((p, Some(ChangeType::Modified), SourceKind::Working, true));
	}
	items.sort_by(|a, b| a.0.cmp(&b.0));
	Ok((summary, items))
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

const MAX_RETAINED_TREE_BYTES: usize = 8 * 1024 * 1024;

fn source_heap_bytes(source: &SourceKind) -> usize {
	match source {
		SourceKind::Commit { rev } => rev.capacity(),
		_ => 0,
	}
}

fn repo_identity_heap_bytes(identity: &RepoIdentity) -> usize {
	identity
		.toplevel
		.capacity()
		.saturating_add(identity.git_dir.capacity())
		.saturating_add(identity.common_dir.capacity())
}

fn repo_entries_bytes(repos: &Vec<RepoEntry>) -> usize {
	let mut bytes = std::mem::size_of_val(repos).saturating_add(
		repos
			.capacity()
			.saturating_mul(std::mem::size_of::<RepoEntry>()),
	);
	for repo in repos {
		bytes = bytes
			.saturating_add(repo.root.capacity())
			.saturating_add(repo.name.capacity())
			.saturating_add(
				repo.identity.as_ref().map_or(0, repo_identity_heap_bytes),
			);
		bytes = bytes.saturating_add(match &repo.summary {
			Ok(summary) => repo_identity_heap_bytes(&summary.identity)
				.saturating_add(
					summary.head.as_ref().map_or(0, String::capacity),
				)
				.saturating_add(
					summary.branch.as_ref().map_or(0, String::capacity),
				),
			Err(error) => error.capacity(),
		});
	}
	bytes
}

fn basket_bytes(basket: &Vec<(CanonicalRootId, Vec<ExportItem>)>) -> usize {
	let mut bytes = std::mem::size_of_val(basket).saturating_add(
		basket.capacity().saturating_mul(std::mem::size_of::<(
			CanonicalRootId,
			Vec<ExportItem>,
		)>()),
	);
	for (root, items) in basket {
		bytes = bytes
			.saturating_add(root.retained_heap_bytes())
			.saturating_add(
				items
					.capacity()
					.saturating_mul(std::mem::size_of::<ExportItem>()),
			);
		for item in items {
			bytes = bytes.saturating_add(export_item_heap_bytes(item));
		}
	}
	bytes
}

fn admitted_selection_root(
	repo: &RepoEntry,
	basket: &[(CanonicalRootId, Vec<ExportItem>)],
) -> Option<CanonicalRootId> {
	let canonical = repo
		.identity
		.as_ref()
		.or_else(|| repo.summary.as_ref().ok().map(|summary| &summary.identity))
		.map(|identity| identity.toplevel.as_path());
	basket
		.iter()
		.find(|(root, _)| {
			root.path() == repo.root.as_path() || Some(root.path()) == canonical
		})
		.map(|(root, _)| root.clone())
}

fn files_bytes(files: &Vec<FileChangeItem>) -> usize {
	files.iter().fold(
		std::mem::size_of_val(files).saturating_add(
			files
				.capacity()
				.saturating_mul(std::mem::size_of::<FileChangeItem>()),
		),
		|bytes, file| {
			bytes
				.saturating_add(file.path.capacity())
				.saturating_add(source_heap_bytes(&file.source))
		},
	)
}

fn message_bytes(message: &Msg) -> usize {
	message.args.iter().fold(
		std::mem::size_of::<Msg>().saturating_add(
			message
				.args
				.capacity()
				.saturating_mul(std::mem::size_of::<String>()),
		),
		|bytes, arg| bytes.saturating_add(arg.capacity()),
	)
}

fn export_item_heap_bytes(item: &ExportItem) -> usize {
	item.root
		.retained_heap_bytes()
		.saturating_add(item.relative_path.capacity())
		.saturating_add(source_heap_bytes(&item.source))
}

const MAX_BASKET_DISPLAY_BYTES: usize = 1024;

fn append_basket_display(out: &mut String, text: &str) -> bool {
	if out.len().saturating_add(text.len()) <= MAX_BASKET_DISPLAY_BYTES {
		out.push_str(text);
		return true;
	}
	let room = MAX_BASKET_DISPLAY_BYTES
		.saturating_sub(out.len())
		.saturating_sub('…'.len_utf8());
	let mut end = text.len().min(room);
	while !text.is_char_boundary(end) {
		end -= 1;
	}
	// Make space for the marker even when previous pieces exactly filled it.
	while out.len() + '…'.len_utf8() > MAX_BASKET_DISPLAY_BYTES {
		out.pop();
	}
	if end > 0 {
		out.push_str(&text[..end]);
	}
	out.push('…');
	false
}

/// One complete checkbox/basket replacement. Preparation leaves the current
/// view unchanged; refusal cannot apply a prefix of Select All or relabel sources.
/// The owned clones are synchronous construction scratch and overlap the old
/// view until installation or rejection; they are not queued or published.
/// This replacement check bounds the installed candidate, not that transient
/// overlap. Complete source admission and pending-worker charges are later stages.
struct PreparedSelection {
	replace_file_group: bool,
	replace_git_group: bool,
	remove_only: bool,
	status: Option<Msg>,
	files: Vec<FileChangeItem>,
	paths: Vec<String>,
	basket: Vec<(CanonicalRootId, Vec<ExportItem>)>,
}

impl PreparedSelection {
	fn new(
		files: &[FileChangeItem],
		paths: &[String],
		basket: &[(CanonicalRootId, Vec<ExportItem>)],
	) -> Self {
		Self {
			replace_file_group: false,
			replace_git_group: false,
			remove_only: false,
			status: None,
			files: files.to_vec(),
			paths: paths.to_vec(),
			basket: basket.to_vec(),
		}
	}

	fn retained_bytes(&self) -> usize {
		files_bytes(&self.files)
			.saturating_add(crate::tree::selection_bytes(&self.paths))
			.saturating_add(basket_bytes(&self.basket))
			.saturating_add(self.status.as_ref().map_or(0, message_bytes))
	}

	/// Preserve every other root and historical source while replacing
	/// `root`'s File group (when `file_group`) and its working-change group
	/// from the checkboxes of Changes slot `git_slot`.
	fn sync_root(
		&mut self,
		root: CanonicalRootId,
		file_group: bool,
		git_slot: Option<u32>,
	) -> bool {
		let replace_file_group = self.replace_file_group && file_group;
		let replace_git_group = self.replace_git_group && git_slot.is_some();
		let in_slot = |file: &FileChangeItem| Some(file.repo) == git_slot;
		if !replace_file_group && !replace_git_group {
			return true;
		}
		self.paths.retain(|path| !path.is_empty());
		self.paths.sort();
		self.paths.dedup();
		let index = self
			.basket
			.binary_search_by(|(have, _)| have.path().cmp(root.path()));
		if self.remove_only {
			// Deselecting must not need the filesystem, clone roots per item, or
			// rebuild spare vector capacity while the old allocation is near its cap.
			if let Ok(index) = index {
				let paths = &self.paths;
				let (replace_file, replace_git) =
					(replace_file_group, replace_git_group);
				// One pass over the checkboxes, not one per basket item.
				let kept: HashSet<(&str, &SourceKind)> = if replace_git {
					self.files
						.iter()
						.filter(|file| file.selected && in_slot(file))
						.map(|file| (file.path.as_str(), &file.source))
						.collect()
				} else {
					HashSet::new()
				};
				self.basket[index].1.retain(|item| match &item.source {
					SourceKind::File if replace_file => {
						paths.binary_search(&item.relative_path).is_ok()
					}
					SourceKind::Working
					| SourceKind::Unstaged
					| SourceKind::Staged
						if replace_git =>
					{
						kept.contains(&(
							item.relative_path.as_str(),
							&item.source,
						))
					}
					_ => true,
				});
				if self.basket[index].1.is_empty() {
					self.basket.remove(index);
				}
			}
			return true;
		}
		// Source/list admission is a later stage. Until then an already-large
		// input may be reduced, while this builder never amplifies beyond it.
		let allocation_limit =
			MAX_RETAINED_TREE_BYTES.max(self.retained_bytes());
		let mut items = match index {
			Ok(index) => self.basket.remove(index).1,
			Err(_) => Vec::new(),
		};
		items.retain(|item| match item.source {
			SourceKind::File => !replace_file_group,
			SourceKind::Working | SourceKind::Unstaged | SourceKind::Staged => {
				!replace_git_group
			}
			SourceKind::Commit { .. } => true,
		});
		let other = self
			.retained_bytes()
			.saturating_add(root.retained_heap_bytes());
		let mut heap = items.iter().fold(0usize, |bytes, item| {
			bytes.saturating_add(export_item_heap_bytes(item))
		});
		let file_count = if replace_file_group {
			self.paths.len()
		} else {
			0
		};
		let git_count = if replace_git_group {
			self.files
				.iter()
				.filter(|file| file.selected && in_slot(file))
				.count()
		} else {
			0
		};
		let add_count = file_count.saturating_add(git_count);
		let needed = items.len().saturating_add(add_count);
		if other
			.saturating_add(
				needed.saturating_mul(std::mem::size_of::<ExportItem>()),
			)
			.saturating_add(heap)
			> allocation_limit
		{
			return false;
		}
		items.reserve_exact(add_count);
		let slots = items
			.capacity()
			.saturating_mul(std::mem::size_of::<ExportItem>());
		if other.saturating_add(slots).saturating_add(heap) > allocation_limit {
			return false;
		}
		let added = self
			.paths
			.iter()
			.filter(|_| replace_file_group)
			.map(|path| ExportItem {
				root: root.clone(),
				relative_path: path.clone(),
				source: SourceKind::File,
				change_type: None,
			})
			.chain(
				self.files
					.iter()
					.filter(|file| {
						replace_git_group && file.selected && in_slot(file)
					})
					.map(|file| ExportItem {
						root: root.clone(),
						relative_path: file.path.clone(),
						source: file.source.clone(),
						change_type: file.change_type,
					}),
			);
		for item in added {
			let proposed_heap =
				heap.saturating_add(export_item_heap_bytes(&item));
			if other.saturating_add(slots).saturating_add(proposed_heap)
				> allocation_limit
			{
				return false;
			}
			heap = proposed_heap;
			items.push(item);
		}
		if !items.is_empty() {
			let index = self
				.basket
				.binary_search_by(|(have, _)| have.path().cmp(root.path()))
				.unwrap_err();
			self.basket.insert(index, (root, items));
		}
		self.retained_bytes() <= allocation_limit
	}

	fn toggle_revision(
		&mut self,
		root: CanonicalRootId,
		sha: &str,
		path: &str,
	) -> bool {
		let index = match self
			.basket
			.binary_search_by(|(have, _)| have.path().cmp(root.path()))
		{
			Ok(index) => index,
			Err(index) => {
				self.basket.insert(index, (root.clone(), Vec::new()));
				index
			}
		};
		let items = &mut self.basket[index].1;
		if let Some(position) = items.iter().position(|item| {
			item.relative_path == path
				&& matches!(&item.source, SourceKind::Commit { rev } if rev == sha)
		}) {
			items.remove(position);
			if items.is_empty() {
				self.basket.remove(index);
			}
			false
		} else {
			items.push(ExportItem {
				root,
				relative_path: path.to_owned(),
				source: SourceKind::Commit {
					rev: sha.to_owned(),
				},
				change_type: None,
			});
			true
		}
	}

	fn fits_replacing(
		&self,
		current_bytes: usize,
		old_bytes: usize,
		limit: usize,
	) -> bool {
		let candidate = self.retained_bytes();
		current_bytes
			.checked_sub(old_bytes)
			.and_then(|other| other.checked_add(candidate))
			.is_some_and(|total| total <= limit || candidate <= old_bytes)
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
	pub details_generation: u64,
	pub details_cancel: Option<CancelToken>,
	pub git_user_email: Option<String>,

	// Files of the selected commit or compare.
	pub commit_files: Vec<(String, Option<ChangeType>)>,
	/// Per file of a multi-selection: index in `log_selected` of the newest
	/// selected commit that touched it.
	pub commit_file_origin: Vec<u32>,
	pub selected_commit_file: Option<String>,
	pub compare: Option<(String, String)>,

	// Tool windows.
	pub files: Vec<FileChangeItem>,
	/// `files` holds the current repo's finished status listing. Until then
	/// its empty checkboxes say nothing about the basket's Git group.
	pub changes_loaded: bool,
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

	pub basket: Vec<(CanonicalRootId, Vec<ExportItem>)>,
	/// Status-bar basket text (localized summary, collision), rebuilt by
	/// `refresh_basket_view` when the basket, repo names or locale change
	/// instead of on every frame.
	pub basket_view: (String, Option<String>),
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
	paste_pending: Arc<Mutex<paste::PastePending>>,
	paste_worker: Option<(u64, CancelToken)>,
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
	/// Remembered workspaces, newest first.
	pub recent_workspaces: Vec<PathBuf>,
	pub lifecycle: lifecycle::Lifecycle,
	pub watch_running: bool,
	pub last_life_log: String,
	/// The selected repo's Project row is collapsed (its tree stays loaded).
	pub repo_collapsed: bool,
	/// Context menus, speed search and Changes group state.
	pub chrome: menu::Chrome,
	/// Changes tool window: one node per workspace repo, in name order.
	pub change_repos: Vec<ChangeRepo>,
	/// Status reads of the repos other than the open one.
	pub changes_queue: ReadQueue<PathBuf>,
	pub changes_cancel: Option<CancelToken>,
	pub changes_generation: u64,
	/// Repo the shown preview was read from, with `preview_identity`.
	pub preview_root: Option<(PathBuf, usize)>,
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
			details_generation: 0,
			details_cancel: None,
			git_user_email: None,
			commit_files: Vec::new(),
			commit_file_origin: Vec::new(),
			selected_commit_file: None,
			compare: None,
			files: Vec::new(),
			changes_loaded: false,
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
			basket: Vec::new(),
			basket_view: (String::new(), None),
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
			carried_repo: None,
			refresh_reload: false,
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
			paste_pending: Arc::new(Mutex::new(paste::PastePending::default())),
			paste_worker: None,
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
			workspace_open: workspace.is_some(),
			workspace_menu: false,
			workspace_picker: false,
			workspace_path_input,
			recent_workspaces: recent::load(),
			lifecycle: lifecycle::Lifecycle::new(1),
			watch_running: false,
			last_life_log: String::new(),
			chrome: menu::Chrome::new(cx),
			repo_collapsed: false,
			change_repos: Vec::new(),
			changes_queue: ReadQueue::new(MAX_CHANGES_READS),
			changes_cancel: None,
			changes_generation: 0,
			preview_root: None,
			log_repo_filter: Vec::new(),
			log_scope_key: Vec::new(),
			log_feeds: Vec::new(),
			log_commit_root: None,
			log_deferred: false,
		};
		if let Some(path) = workspace {
			recent::remember(&mut model.recent_workspaces, &path);
			model.reload_repos(cx);
		}
		model
	}

	/// The repo the shown preview came from: a log commit's own repo, the
	/// Changes row's repo, else the open repo.
	pub fn preview_root(&self) -> Option<PathBuf> {
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

	/// The open repo's Changes slot.
	pub fn selected_change_slot(&self) -> Option<usize> {
		let root = self.repo_root()?;
		self.change_repos.iter().position(|s| s.root == root)
	}

	fn ensure_change_slot(
		&mut self,
		root: &std::path::Path,
		name: &str,
	) -> usize {
		slot_insert(&mut self.change_repos, &mut self.files, root, name)
	}

	/// Installs one repo's status read into its slot: checkboxes follow the
	/// basket, rows past `MAX_CHANGES_PER_REPO` are dropped (the node says
	/// so), and a failure leaves an error row instead of rows.
	fn install_slot_changes(
		&mut self,
		slot: usize,
		result: Result<Vec<WorkingChangeTuple>, String>,
	) {
		let root = self.change_repos[slot].root.clone();
		let changes = match result {
			Ok(changes) => changes,
			Err(err) => {
				slot_replace_rows(&mut self.files, slot, Vec::new());
				let s = &mut self.change_repos[slot];
				s.state = ChangeRepoState::Failed(err);
				s.total = 0;
				return;
			}
		};
		let total = changes.len();
		let canonical = self
			.repos
			.iter()
			.find(|r| r.root == root)
			.and_then(|r| admitted_selection_root(r, &self.basket))
			.or_else(|| CanonicalRootId::new(&root).ok());
		let saved: HashSet<(&str, &SourceKind)> = canonical
			.as_ref()
			.and_then(|c| self.basket_items(c))
			.into_iter()
			.flatten()
			.map(|i| (i.relative_path.as_str(), &i.source))
			.collect();
		// ponytail: rows past the cap are not listed, so a Git-group sync of
		// this repo drops a basket entry for one of them; preserve unlisted
		// entries if a real repo ever needs more than the cap.
		let rows: Vec<FileChangeItem> = changes
			.into_iter()
			.take(MAX_CHANGES_PER_REPO)
			.map(|(path, change_type, source, is_conflict)| {
				let selected = saved.contains(&(path.as_str(), &source));
				FileChangeItem {
					path,
					change_type,
					source,
					is_conflict,
					selected,
					repo: slot as u32,
				}
			})
			.collect();
		drop(saved);
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
		result: Result<(Option<RepoSummary>, Vec<WorkingChangeTuple>), String>,
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
		let result = result.map(|(summary, changes)| {
			if let Some(summary) = summary {
				if let Some(entry) =
					self.repos.iter_mut().find(|r| r.root == root)
				{
					entry.summary = Ok(summary);
				}
			}
			changes
		});
		match &result {
			Ok(changes) => {
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

	pub fn set_status(
		&mut self,
		key: &'static str,
		args: impl crate::i18n::IntoMsgArgs,
	) {
		self.status = Msg::new(key, args);
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
		if let Some(ref d) = self.restore_dir {
			d.clone()
		} else if let Some(root) = self.repo_root() {
			root
		} else {
			self.workspace_root.clone()
		}
	}

	pub fn set_preview(&mut self, p: Preview) -> bool {
		let pool = self.paste_pending.clone();
		let mut pending = paste::lock_pending(&pool);
		if let Err(err) = pending.admit_ui(
			Some(&p),
			self.paste_preview.as_ref(),
			self.paste_detail.as_ref(),
		) {
			self.preview_loading = false;
			self.preview_error = None;
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
		self.preview = Some(p);
		self.preview_loading = false;
		self.preview_error = None;
		self.reader.reset_for_new_preview();
		// Re-run an active find against the new text.
		self.refind();
		true
	}

	pub(crate) fn clear_preview(&mut self) {
		let pool = self.paste_pending.clone();
		let mut state = paste::lock_pending(&pool);
		self.preview = None;
		self.preview_root = None;
		state
			.admit_ui(
				None,
				self.paste_preview.as_ref(),
				self.paste_detail.as_ref(),
			)
			.expect("dropping ordinary preview cannot grow retained data");
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

	fn clear_paste_state(&mut self) {
		let pool = self.paste_pending.clone();
		let mut state = paste::lock_pending(&pool);
		self.paste_preview = None;
		self.paste_detail = None;
		state.admit_ui(self.preview.as_ref(), None, None).expect(
			"dropping paste transfers any shared raw charge to its worker",
		);
	}

	pub fn deselect_all_files(&mut self, cx: &mut Context<Self>) {
		let mut candidate = self.selection_candidate();
		for file in &mut candidate.files {
			if self.change_slot_loaded(file.repo) {
				file.selected = false;
			}
		}
		candidate.paths = Vec::new();
		candidate.replace_file_group = true;
		candidate.replace_git_group = true;
		candidate.remove_only = true;
		candidate.status = Some(Msg::new("status_deselected_all", []));
		if self.install_selection_candidate(candidate) {
			self.log_basket();
			app_log!("[APP:FILES_DESELECTED]");
		}
		cx.notify();
	}

	pub fn select_all_files(&mut self, cx: &mut Context<Self>) {
		let mut candidate = self.selection_candidate();
		for file in &mut candidate.files {
			if self.change_slot_loaded(file.repo) {
				file.selected = file.is_valid_utf8();
			}
		}
		if let Some(tree) = &self.file_tree {
			candidate.paths = tree.selection_for_all(true);
		}
		candidate.replace_file_group = true;
		candidate.replace_git_group = true;
		candidate.status = Some(Msg::new("status_selected_all", []));
		if self.install_selection_candidate(candidate) {
			self.log_basket();
			app_log!("[APP:FILES_SELECTED_ALL]");
		}
		cx.notify();
	}

	/// A slot whose status read has landed; a loading one's rows are inert.
	pub fn change_slot_loaded(&self, slot: u32) -> bool {
		self.change_repos
			.get(slot as usize)
			.is_some_and(|s| s.state == ChangeRepoState::Loaded)
	}

	pub fn toggle_file(&mut self, idx: usize, cx: &mut Context<Self>) {
		if self
			.files
			.get(idx)
			.is_some_and(|f| !self.change_slot_loaded(f.repo))
		{
			return;
		}
		if self.files.get(idx).is_some_and(|f| !f.is_valid_utf8()) {
			self.set_status("change_not_utf8", []);
			app_log!("[APP:FILE_TOGGLE_REFUSED: not_utf8]");
			cx.notify();
			return;
		}
		let mut candidate = self.selection_candidate();
		let Some(file) = candidate.files.get_mut(idx) else {
			return;
		};
		file.selected = !file.selected;
		let (path, selected) = (file.path.clone(), file.selected);
		candidate.replace_git_group = true;
		candidate.remove_only = !selected;
		candidate.status = Some(if selected {
			Msg::new("status_toggled_file", [path.clone()])
		} else {
			Msg::new("status_selection_removed", [])
		});
		if self.install_selection_candidate(candidate) {
			self.log_basket();
			app_log!(
				"[APP:FILE_TOGGLED: {}: {}: selected={}]",
				idx,
				path,
				selected
			);
		}
		cx.notify();
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
		self.refresh_basket_view();
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
					// The identity was just resolved; reuse it.
					let summary = match &identity {
						Some(id) => summarize_with_identity(&git, id, opts),
						None => summarize(&git, opts),
					}
					.map_err(|e| e.to_string());

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
		self.lifecycle.unfinished() > 0
			|| self.lifecycle.is_draining()
			|| paste::lock_pending(&self.paste_pending).has_pending()
			|| self.paste_worker.is_some()
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
		}
	}

	fn release_workspace_state(&mut self, cx: &mut Context<Self>) {
		release_vec(&mut self.repos);
		self.selected_repo_idx = None;
		self.release_repo_state();
		release_vec(&mut self.basket);
		release_vec(&mut self.files);
		release_vec(&mut self.change_repos);
		if let Some(token) = self.changes_cancel.take() {
			token.cancel();
		}
		self.changes_generation = self.changes_generation.wrapping_add(1);
		self.changes_queue.clear_pending();
		self.preview_root = None;
		self.basket_view = (String::new(), None);
		self.invalidate_paste_job();
		if !self.paste_busy() {
			self.clear_paste_state();
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
		self.commit_rows_cache.take();
		self.log_first_page = 0;
		self.history_extending = false;
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
		self.compare = None;
		self.changes_loaded = false;
		self.file_tree = None;
		self.rev_tree = None;
		self.selected_file = None;
		self.selected_file_source = None;
		self.tree_cursor = 0;
		self.selected_list_row = 0;
		self.clear_preview();
		self.preview_loading = false;
		self.preview_error = None;
		self.reader.release_retained();
		self.tree_queue = VecDeque::new();
		self.tree_worker_alive = false;
		release_vec(&mut self.restore_expanded);
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
		self.refresh_basket_view();
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
		// Its selections can no longer be read, and one unreadable root
		// would make every later Copy fail.
		let dropped = admitted_selection_root(&gone, &self.basket);
		if let Some(root) = &dropped {
			self.basket.retain(|(have, _)| have != root);
		}
		self.refresh_basket_view();
		if dropped.is_some() {
			self.log_basket();
		}
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
		self.refresh_basket_view();
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
				let mut candidate = self.selection_candidate();
				candidate.remove_only = paths
					.iter()
					.all(|path| candidate.paths.binary_search(path).is_ok());
				candidate.paths = paths;
				candidate.replace_file_group = true;
				if candidate.remove_only {
					candidate.status =
						Some(Msg::new("status_selection_removed", []));
				}
				if self.install_selection_candidate(candidate) {
					if let Some(rel) = key.utf8_rel() {
						app_log!("[APP:TREE_TOGGLED: {}]", rel);
					}
					self.log_basket();
				}
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
			Some(TreeEffect::Idle) => {
				if !self.remember_tree_selection() {
					cx.notify();
					return;
				}
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
		let previous_selection = self
			.file_tree
			.as_ref()
			.map(|tree| tree.selected_paths().to_vec());
		let Some(tree) = self.file_tree.as_mut() else {
			return;
		};
		if tree.full_path != result.base {
			return;
		}
		let Some(applied) = tree.apply_io_result(result) else {
			return;
		};
		if !self.remember_tree_selection() {
			// Incoming page/cache admission is stage4; even before that lands,
			// a refused inherited selection cannot change the existing basket or checks.
			if let (Some(tree), Some(paths)) =
				(&mut self.file_tree, previous_selection)
			{
				tree.install_selection(paths);
			}
			return;
		}
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
		if !self.remember_tree_selection() {
			cx.notify();
			return;
		}
		// The log shows the workspace: a plain switch keeps it.
		let reload_log = self.log_reloads_on_repo_switch(preserve_anchors);
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
		self.selected_commit = anchor_commit.clone();
		self.range_head = None;
		self.log_selected.clear();
		self.compare = None;
		self.commit_files.clear();
		self.changes_loaded = false;
		// Its rows stay shown but inert until the re-read lands: their
		// checkboxes must not replace the repo's Git group meanwhile.
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
		if let Err(e) = &self.repos[idx].summary {
			self.show_preview_error(Msg::new("error_repo_status", [e.clone()]));
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
		let known = self.repos[idx].identity.clone();
		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let working_res = bg
					.spawn(async move {
						read_change_list(&repo_root, known.as_ref(), cancel_bg)
					})
					.await;

				let _ = this.update(&mut async_app, |model, cx| {
					if model.generation != task_generation {
						return;
					}
					match working_res {
						Ok((summary, changes)) => {
							model.update_open_summary(summary);
							let slot = model.ensure_change_slot(
								&repo_root_for_update,
								&repo_name,
							);
							model.install_slot_changes(slot, Ok(changes));
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
									model.select_file_with_source(
										anchor,
										source.clone(),
										cx,
									);
								} else if model.files[slot.clone()]
									.iter()
									.any(|f| &f.path == anchor)
								{
									model.select_file(anchor, cx);
								} else if let Some(first) =
									model.files[slot.clone()].first()
								{
									let first_path = first.path.clone();
									model.select_file(&first_path, cx);
								} else {
									model.selected_file = None;
									model.clear_preview();
									if model.mode == "preview" {
										ready_marker("PREVIEW");
									}
								}
							} else if let Some(first) =
								model.files[slot.clone()].first()
							{
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
							model.show_preview_error(Msg::new(
								"error_repo_changes",
								[repo_name.clone(), err],
							));
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
			self.selected_commit.take(),
		);
		self.range_head = None;
		self.log_selected.clear();
		self.compare = None;
		self.commit_files.clear();
		let Some(repo_root) = root.or_else(|| self.repo_root()) else {
			return;
		};
		let shown_root = repo_root.clone();
		let file_path = path.to_string();
		self.preview_loading = true;
		self.preview_error = None;

		let cancel = arm_cancel(&mut self.preview_cancel);
		let fs_only = matches!(source, SourceKind::File);
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
					let result =
						read_preview(&repo_root, &for_bg, &source, cancel);
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
						model.selected_commit,
					) = shown_selection;
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

	/// Current model and queued tree storage. Background input/publication
	/// ownership is added in the worker-admission stage before tree8 acceptance.
	fn tree_retained_bytes(&self) -> usize {
		let mut bytes = message_bytes(&self.status)
			.saturating_add(repo_entries_bytes(&self.repos))
			.saturating_add(repo_entries_bytes(&self.manual_repos))
			.saturating_add(basket_bytes(&self.basket));
		bytes = bytes.saturating_add(files_bytes(&self.files));
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.commit_files))
			.saturating_add(self.commit_files.capacity().saturating_mul(
				std::mem::size_of::<(String, Option<ChangeType>)>(),
			));
		for (path, _) in &self.commit_files {
			bytes = bytes.saturating_add(path.capacity());
		}
		bytes = bytes
			.saturating_add(self.commit_file_origin.capacity() * 4)
			.saturating_add(
				self.log_selected.capacity() * std::mem::size_of::<String>(),
			);
		for id in &self.log_selected {
			bytes = bytes.saturating_add(id.capacity());
		}
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.file_tree))
			.saturating_add(self.file_tree.as_ref().map_or(0, |tree| {
				tree.retained_bytes() - std::mem::size_of::<FileTreeNode>()
			}));
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.rev_tree))
			.saturating_add(self.rev_tree.as_ref().map_or(0, |tree| {
				tree.retained_bytes() - std::mem::size_of::<RevTree>()
			}));
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.workspace_root))
			.saturating_add(self.workspace_root.capacity())
			.saturating_add(std::mem::size_of_val(&self.restore_dir))
			.saturating_add(
				self.restore_dir.as_ref().map_or(0, PathBuf::capacity),
			)
			.saturating_add(std::mem::size_of_val(&self.pinned_repo));
		if let Some((root, git_dir)) = &self.pinned_repo {
			bytes = bytes
				.saturating_add(root.capacity())
				.saturating_add(git_dir.capacity());
		}
		for path in [&self.selected_file, &self.selected_commit_file] {
			bytes = bytes
				.saturating_add(std::mem::size_of_val(path))
				.saturating_add(path.as_ref().map_or(0, String::capacity));
		}
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.selected_file_source))
			.saturating_add(
				self.selected_file_source
					.as_ref()
					.map_or(0, source_heap_bytes),
			);
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.restore_expanded))
			.saturating_add(
				self.restore_expanded
					.capacity()
					.saturating_mul(std::mem::size_of::<String>()),
			);
		for path in &self.restore_expanded {
			bytes = bytes.saturating_add(path.capacity());
		}
		let dirs = &self.chrome.expanded_dirs;
		bytes = bytes
			.saturating_add(std::mem::size_of_val(dirs))
			.saturating_add(dirs.capacity().saturating_mul(
				std::mem::size_of::<(&'static str, PathBuf, String)>(),
			));
		for (_, root, dir) in dirs {
			bytes = bytes
				.saturating_add(root.capacity())
				.saturating_add(dir.capacity());
		}
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.discovery))
			.saturating_add(self.discovery.as_ref().map_or(0, |discovery| {
				discovery.retained_bytes() - std::mem::size_of::<Discovery>()
			}));
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.discovery_errors))
			.saturating_add(
				self.discovery_errors
					.capacity()
					.saturating_mul(std::mem::size_of::<(PathBuf, String)>()),
			);
		for (path, error) in &self.discovery_errors {
			bytes = bytes
				.saturating_add(path.capacity())
				.saturating_add(error.capacity());
		}
		bytes = bytes
			.saturating_add(std::mem::size_of_val(
				&self.discovery_depth_limited,
			))
			.saturating_add(
				self.discovery_depth_limited
					.capacity()
					.saturating_mul(std::mem::size_of::<PathBuf>()),
			);
		for path in &self.discovery_depth_limited {
			bytes = bytes.saturating_add(path.capacity());
		}
		bytes = bytes
			.saturating_add(std::mem::size_of_val(&self.tree_queue))
			.saturating_add(
				self.tree_queue
					.capacity()
					.saturating_mul(std::mem::size_of::<TreeIo>()),
			);
		for io in &self.tree_queue {
			bytes = bytes.saturating_add(
				io.retained_bytes() - std::mem::size_of::<TreeIo>(),
			);
		}
		bytes
	}

	fn basket_items(&self, root: &CanonicalRootId) -> Option<&Vec<ExportItem>> {
		self.basket
			.binary_search_by(|(have, _)| have.path().cmp(root.path()))
			.ok()
			.map(|index| &self.basket[index].1)
	}

	fn selection_candidate(&self) -> PreparedSelection {
		PreparedSelection::new(
			&self.files,
			self.file_tree
				.as_ref()
				.map_or(&[], FileTreeNode::selected_paths),
			&self.basket,
		)
	}

	fn selection_bytes(&self) -> usize {
		files_bytes(&self.files)
			.saturating_add(basket_bytes(&self.basket))
			.saturating_add(
				self.file_tree
					.as_ref()
					.map_or(0, FileTreeNode::selection_bytes),
			)
	}

	fn install_selection_candidate(
		&mut self,
		mut candidate: PreparedSelection,
	) -> bool {
		// Each loaded repo's Changes rows replace that repo's Git group; an
		// unloaded or failed list has no checkboxes to replace it with, and
		// doing so would silently drop the repo's selections. The Project
		// tree's File group belongs to the open repo.
		if candidate.replace_file_group || candidate.replace_git_group {
			let open = self.repo_root();
			let mut jobs: Vec<(PathBuf, bool, Option<u32>)> = Vec::new();
			if candidate.replace_git_group {
				for (i, slot) in self.change_repos.iter().enumerate() {
					if slot.state == ChangeRepoState::Loaded {
						let file_group = candidate.replace_file_group
							&& open.as_ref() == Some(&slot.root);
						jobs.push((
							slot.root.clone(),
							file_group,
							Some(i as u32),
						));
					}
				}
			}
			if candidate.replace_file_group && !jobs.iter().any(|job| job.1) {
				if let Some(root) = open {
					jobs.push((root, true, None));
				}
			}
			for (path, file_group, git_slot) in jobs {
				let admitted =
					self.repos.iter().find(|repo| repo.root == path).and_then(
						|repo| admitted_selection_root(repo, &self.basket),
					);
				let adds = (file_group && !candidate.paths.is_empty())
					|| git_slot.is_some_and(|slot| {
						candidate
							.files
							.iter()
							.any(|f| f.repo == slot && f.selected)
					});
				let root = match admitted {
					Some(root) => root,
					None if candidate.remove_only || !adds => continue,
					None => match CanonicalRootId::new(&path) {
						Ok(root) => root,
						Err(_) => {
							self.set_status("error_selection_root", []);
							return false;
						}
					},
				};
				if !candidate.sync_root(root, file_group, git_slot) {
					self.set_status("error_tree_budget", []);
					app_log!(
						"[APP:TREE_ADMISSION_REFUSED: selection_amplification]"
					);
					return false;
				}
			}
		}
		if !candidate.fits_replacing(
			self.tree_retained_bytes(),
			self.selection_bytes().saturating_add(
				if candidate.status.is_some() {
					message_bytes(&self.status)
				} else {
					0
				},
			),
			MAX_RETAINED_TREE_BYTES,
		) {
			self.set_status("error_tree_budget", []);
			app_log!("[APP:TREE_ADMISSION_REFUSED: selection]");
			return false;
		}
		if let Some(status) = candidate.status {
			self.status = status;
		}
		self.files = candidate.files;
		self.basket = candidate.basket;
		self.refresh_basket_view();
		if let Some(tree) = &mut self.file_tree {
			tree.install_selection(candidate.paths);
		}
		// Callers emit their established action/basket event order only after
		// this complete intent has been admitted and installed.
		true
	}

	pub fn is_rev_file_selected(&self, sha: &str, path: &str) -> bool {
		let Some(repo) = self.repo() else {
			return false;
		};
		let Some(root) = admitted_selection_root(repo, &self.basket) else {
			return false;
		};
		let Some(items) = self.basket_items(&root) else {
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
		let Some(root) = admitted_selection_root(repo, &self.basket)
			.or_else(|| CanonicalRootId::new(&repo.root).ok())
		else {
			self.set_status("error_selection_root", []);
			cx.notify();
			return;
		};
		let mut candidate = self.selection_candidate();
		let selected = candidate.toggle_revision(root, sha, path);
		candidate.remove_only = !selected;
		if !selected {
			candidate.status = Some(Msg::new("status_selection_removed", []));
		}
		if self.install_selection_candidate(candidate) {
			app_log!(
				"[APP:REV_FILE_TOGGLED: sha={} path={} selected={}]",
				&sha[..7.min(sha.len())],
				path,
				selected
			);
			self.log_basket();
		}
		cx.notify();
	}

	fn log_basket(&self) {
		app_log!(
			"[APP:BASKET: n={} summary={}]",
			self.basket_count(),
			self.basket_summary()
		);
		if let Some(collision) = &self.basket_view.1 {
			app_log!("[APP:BASKET_COLLISION: {}]", collision);
		}
	}

	pub fn basket_count(&self) -> usize {
		self.basket.iter().map(|(_, items)| items.len()).sum()
	}

	fn basket_summary_with<F>(&self, format_source: F) -> String
	where
		F: Fn(&SourceKind) -> String,
	{
		let mut out = String::new();
		for (root, items) in &self.basket {
			let name = self
				.repos
				.iter()
				.find(|repo| {
					CanonicalRootId::new(&repo.root).ok().as_ref() == Some(root)
				})
				.map(|repo| repo.name.clone())
				.unwrap_or_else(|| root.path().display().to_string());
			let mut ordered: Vec<_> = items.iter().collect();
			ordered.sort_by(|a, b| {
				(&a.relative_path, source_order(&a.source))
					.cmp(&(&b.relative_path, source_order(&b.source)))
			});
			for item in ordered {
				if !out.is_empty() && !append_basket_display(&mut out, "; ") {
					return out;
				}
				for part in [
					&name,
					" ",
					&format_source(&item.source),
					" ",
					&item.relative_path,
				] {
					if !append_basket_display(&mut out, part) {
						return out;
					}
				}
			}
		}
		out
	}

	pub fn basket_summary(&self) -> String {
		self.basket_summary_with(Self::source_summary)
	}

	pub fn basket_summary_localized(&self, loc: Locale) -> String {
		self.basket_summary_with(|source| match source {
			SourceKind::File => i18n::t("src_working_file", loc).to_string(),
			SourceKind::Working => i18n::t("tag_untracked", loc).to_string(),
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

	pub(crate) fn refresh_basket_view(&mut self) {
		self.basket_view = (
			self.basket_summary_localized(self.locale),
			self.basket_collision_text(),
		);
	}

	/// Two selections of one path cannot share a wire header. File rows count;
	/// a matching basename is not a reason to drop one of them.
	pub fn basket_collision_text(&self) -> Option<String> {
		let mut out = String::new();
		for (root, items) in &self.basket {
			let mut paths: Vec<_> = items
				.iter()
				.map(|item| item.relative_path.as_str())
				.collect();
			paths.sort_unstable();
			let mut previous_hit = None;
			for pair in paths.windows(2) {
				if pair[0] != pair[1] || previous_hit == Some(pair[0]) {
					continue;
				}
				previous_hit = Some(pair[0]);
				if !out.is_empty() && !append_basket_display(&mut out, ", ") {
					return Some(out);
				}
				for part in
					[root.path().to_string_lossy().as_ref(), ":", pair[0]]
				{
					if !append_basket_display(&mut out, part) {
						return Some(out);
					}
				}
			}
		}
		(!out.is_empty()).then_some(out)
	}

	fn file_paths_in_basket(&self, root: &std::path::Path) -> Vec<String> {
		let id = self
			.repos
			.iter()
			.find(|repo| repo.root == root)
			.and_then(|repo| admitted_selection_root(repo, &self.basket))
			.or_else(|| {
				self.basket
					.iter()
					.find(|(id, _)| id.path() == root)
					.map(|(id, _)| id.clone())
			});
		let Some(id) = id else {
			return Vec::new();
		};
		self.basket_items(&id)
			.map(|items| {
				items
					.iter()
					.filter(|item| matches!(item.source, SourceKind::File))
					.map(|item| item.relative_path.clone())
					.collect()
			})
			.unwrap_or_default()
	}

	/// Synchronization also goes through whole-selection admission; callers
	/// must not publish Copy or success after a refusal.
	pub fn remember_tree_selection(&mut self) -> bool {
		let mut candidate = self.selection_candidate();
		candidate.replace_file_group = true;
		let accepted = self.install_selection_candidate(candidate);
		if accepted {
			self.log_basket();
		}
		accepted
	}

	pub fn reapply_tree_selection(&mut self) -> bool {
		let Some(root) = self.repo_root() else {
			return true;
		};
		let mut candidate = self.selection_candidate();
		candidate.paths = self.file_paths_in_basket(&root);
		self.install_selection_candidate(candidate)
	}

	pub fn sync_git_selection_to_basket(&mut self) -> bool {
		let mut candidate = self.selection_candidate();
		candidate.replace_git_group = true;
		let accepted = self.install_selection_candidate(candidate);
		if accepted {
			self.log_basket();
		}
		accepted
	}

	pub fn clear_basket(&mut self, cx: &mut Context<Self>) {
		self.basket = Vec::new();
		self.refresh_basket_view();
		for file in &mut self.files {
			file.selected = false;
		}
		if let Some(tree) = &mut self.file_tree {
			tree.install_selection(Vec::new());
		}
		self.log_basket();
		app_log!("[APP:BASKET_CLEARED]");
		self.set_status("basket_cleared", []);
		cx.notify();
	}

	pub fn sync_current_selection_to_basket(&mut self) -> bool {
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
		// Change-list checkboxes already update the basket as they toggle;
		// only the project tree's selection is synced lazily.
		let mut candidate = self.selection_candidate();
		candidate.replace_file_group = true;
		if !self.install_selection_candidate(candidate) {
			cx.notify();
			return;
		}
		self.log_basket();
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
			let mut candidate = self.selection_candidate();
			candidate.status = Some(Msg::new("basket_collision", [collision]));
			self.install_selection_candidate(candidate);
			cx.notify();
			return;
		}
		let repo_name = self
			.repo()
			.map(|repo| repo.name.clone())
			.unwrap_or_else(|| "basket".into());
		let items: Vec<ExportItem> =
			self.basket.iter().flat_map(|(_, i)| i.clone()).collect();
		self.export_items_to_clipboard(items, repo_name, cx);
	}

	/// Exports `items` (any roots) as one snip-sync payload to the
	/// clipboard: the basket's Copy, and a Log file's Copy.
	pub fn export_items_to_clipboard(
		&mut self,
		items: Vec<ExportItem>,
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
		paste::lock_pending(&self.paste_pending)
			.invalidate(self.paste_generation);
		self.paste_loading = false;
	}

	fn queue_paste(
		&mut self,
		request: paste::PasteRequest,
		cx: &mut Context<Self>,
	) {
		let result = paste::lock_pending(&self.paste_pending).enqueue(request);
		if let Err(err) = result {
			self.paste_loading = false;
			self.status = err;
			app_log!("[APP:PASTE_ERR: preview_memory_limit]");
			self.restore_log_after_paste();
			cx.notify();
			return;
		}
		self.paste_loading = true;
		self.set_status("paste_loading", []);
		app_log!("[APP:PASTE_LOADING]");
		self.poll_paste(cx);
		self.arm_watch(cx);
		cx.notify();
	}

	/// Runs before the existing watch decides it can exit. A cancelled job
	/// still occupies this slot until its actual FinishFlag has dropped.
	fn poll_paste(&mut self, cx: &mut Context<Self>) {
		if self
			.paste_worker
			.as_ref()
			.is_some_and(|(id, _)| self.lifecycle.is_live(*id))
		{
			return;
		}
		let finished = self.paste_worker.take();
		if let Some((_, token)) = &finished {
			if token.is_cancelled() {
				app_log!("[APP:PASTE_DISCARDED: cancelled]");
			}
		}
		let pool = self.paste_pending.clone();
		{
			let mut pending = paste::lock_pending(&pool);
			if let Some(outcome) = pending.ready.take() {
				self.paste_loading = false;
				self.show_paste_plan(outcome, &mut pending);
				cx.notify();
			} else if finished
				.as_ref()
				.is_some_and(|(_, token)| !token.is_cancelled())
				&& !pending.has_pending()
				&& self.paste_loading
			{
				self.paste_loading = false;
				self.status = Msg::new(
					"paste_err_plan",
					["Preview worker ended before producing a result".into()],
				);
				cx.notify();
			}
		}
		if !self.accepting_work() {
			return;
		}
		let cancel = CancelToken::new();
		match paste::PastePending::start(
			&pool,
			self.paste_preview.as_ref(),
			cancel.clone(),
		) {
			Ok(Some(work)) => {
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
				self.paste_cancel = Some(cancel.clone());
				self.paste_worker = Some((id, cancel));
			}
			Ok(None) => {}
			Err(err) => {
				self.paste_loading = false;
				self.set_paste_error(err);
				app_log!("[APP:PASTE_ERR: preview_memory_limit]");
				cx.notify();
			}
		}
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
		if self.paste_preview.is_some() {
			app_log!("[APP:PASTE_PLAN_CLEARED]");
		}
		self.clear_paste_state();
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
			},
			cx,
		);
	}

	fn show_paste_plan(
		&mut self,
		outcome: paste::PasteOutcome,
		pending: &mut paste::PastePending,
	) {
		let remap = outcome.remap;
		let built = outcome.result.and_then(|plan| {
			let detail = plan.detail_preview();
			pending.admit_ui(
				self.preview.as_ref(),
				Some(&plan),
				detail.as_ref(),
			)?;
			Ok((plan, detail))
		});
		match built {
			Ok((plan, detail)) => {
				if let Some((prefix, keep)) = remap {
					let dest = prefix_target(&plan, &prefix, keep);
					let keep_note = if keep { " keep=primary" } else { "" };
					app_log!(
						"[APP:PASTE_MAPPED: prefix={}{} dest={} items={}]",
						prefix,
						keep_note,
						dest,
						plan.items.len()
					);
				} else {
					for choice in &plan.prefix_choices {
						for (idx, path) in choice.candidates.iter().enumerate()
						{
							app_log!("[APP:PASTE_MAP_CANDIDATE: prefix={} idx={} path={}]", choice.prefix, idx, path.display());
						}
					}
					app_log!(
						"[APP:PASTE_PREVIEW: items={} dest={} mapping={}]",
						plan.items.len(),
						plan.destination.display(),
						plan.mapping_ready()
					);
				}
				self.set_status(
					"status_paste_preview",
					[plan.items.len().to_string()],
				);
				self.paste_preview = Some(plan);
				self.paste_detail = detail;
				if collapse_log_for_paste(
					&mut self.log_before_paste,
					&mut self.bottom_visible,
				) {
					app_log!(
						"[APP:LOG_PANEL: visible=false reason=paste_open]"
					);
				}
				self.paste_scroll
					.scroll_to_item(0, gpui::ScrollStrategy::Top);
				self.pending_focus = Some(self.paste_focus.clone());
			}
			Err(err) => {
				app_log!("[APP:PASTE_ERR: {}]", err.key);
				self.status = err;
				// A disarmed remap shell may remain; it cannot authorize writes.
				if pending
					.admit_ui(
						self.preview.as_ref(),
						self.paste_preview.as_ref(),
						self.paste_detail.as_ref(),
					)
					.is_err()
				{
					self.paste_preview = None;
					self.paste_detail = None;
					pending
						.admit_ui(self.preview.as_ref(), None, None)
						.expect("drop failed paste candidate");
				}
				if self.paste_preview.is_none() {
					self.restore_log_after_paste();
					self.pending_focus = Some(self.focus_handle.clone());
				}
			}
		}
	}

	/// Keep full write/read diagnostics in status, but do not let a newly
	/// allocated diagnostic grow an already admitted plan past its tier.
	fn set_paste_error(&mut self, err: Msg) {
		let pool = self.paste_pending.clone();
		let mut pending = paste::lock_pending(&pool);
		self.status = err.clone();
		if let Some(plan) = &mut self.paste_preview {
			plan.error = Some(err);
		}
		if pending
			.admit_ui(
				self.preview.as_ref(),
				self.paste_preview.as_ref(),
				self.paste_detail.as_ref(),
			)
			.is_err()
		{
			if let Some(plan) = &mut self.paste_preview {
				plan.error = Some(paste::preview_budget_error());
			}
			pending
				.admit_ui(
					self.preview.as_ref(),
					self.paste_preview.as_ref(),
					self.paste_detail.as_ref(),
				)
				.expect("fixed error fits admitted plan");
		}
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
		let Some(dest) = self.paste_preview.as_ref().and_then(|plan| {
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
		let keep = destination.is_none();
		self.invalidate_paste_job();
		let pool = self.paste_pending.clone();
		{
			let mut pending = paste::lock_pending(&pool);
			let Some(plan) = self.paste_preview.as_mut() else {
				return;
			};
			plan.clear_file_plan();
			plan.error = None;
			self.paste_detail = None;
			let chosen = match destination {
				Some(dest) => plan.choose_prefix_destination(&prefix, &dest),
				None => plan.choose_keep_relative(&prefix),
			};
			if let Err(err) = chosen {
				pending
					.admit_ui(self.preview.as_ref(), Some(plan), None)
					.expect("invalid choice cannot grow disarmed shell");
				self.status = err;
				cx.notify();
				return;
			}
			if let Err(err) =
				pending.admit_ui(self.preview.as_ref(), Some(plan), None)
			{
				self.paste_preview = None;
				pending
					.admit_ui(self.preview.as_ref(), None, None)
					.expect("drop over-budget mapping shell");
				self.status = err;
				self.restore_log_after_paste();
				cx.notify();
				return;
			}
		}
		self.queue_paste(paste::PasteRequest::Remap { prefix, keep }, cx);
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
		let pool = self.paste_pending.clone();
		let mut pending = paste::lock_pending(&pool);
		if let Some(plan) = &mut self.paste_preview {
			if idx >= plan.items.len() {
				return;
			}
			let detail = plan.detail_at(idx);
			match pending.admit_ui(
				self.preview.as_ref(),
				Some(plan),
				detail.as_ref(),
			) {
				Ok(()) => {
					plan.selected_item_idx = idx;
					self.paste_detail = detail;
					app_log!("[APP:PASTE_NAV: idx={}]", idx);
					self.paste_scroll
						.scroll_to_item(0, gpui::ScrollStrategy::Top);
				}
				Err(err) => {
					self.status = err;
					app_log!(
						"[APP:PASTE_DETAIL_REFUSED: reason=retained_budget]"
					);
				}
			}
			cx.notify();
		}
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
			self.set_paste_error(Msg::new("mapping_required", []));
			cx.notify();
			return;
		}

		if !plan.executable() {
			app_log!("[APP:APPLY_IGNORED: no_plan]");
			return;
		}

		plan.is_applying = true;
		self.pending_focus = Some(self.paste_focus.clone());
		// The worker gets a cheap handle: the plan's contents are shared.
		let plan_clone =
			paste::PasteApplyWorker::new(plan, &self.paste_pending);
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
							model.clear_paste_state();
							model.restore_log_after_paste();
							model.pending_focus =
								Some(model.focus_handle.clone());
							// A rescan refreshes the destination's summary too,
							// then reloads the open repo keeping its anchors.
							model.reload_repos(cx);
						}
						Err(err) => {
							app_log!("[APP:PASTE_STALE_DETECTED: {}]", err.key);
							if let Some(p) = &mut model.paste_preview { p.is_applying = false; }
							model.set_paste_error(err);
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

	/// Editor tabs open: the paste preview, or the one reader tab.
	pub fn open_tab_count(&self) -> usize {
		let paste = self.paste_preview.is_some() || self.paste_loading;
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
		if self.paste_preview.is_some() || self.paste_loading {
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
		self.selected_commit = None;
		self.range_head = None;
		self.log_selected.clear();
		self.compare = None;
		self.selected_commit_file = None;
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
		let was_loading = self.paste_loading;
		self.invalidate_paste_job();
		if self.paste_preview.is_none() && !was_loading {
			return;
		}
		self.clear_paste_state();
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

/// Orders like `WorkbenchModel::source_summary` text without allocating: no
/// tag is a prefix of another, so comparing (tag, rev) equals comparing tag+rev.
fn source_order(source: &SourceKind) -> (&'static str, &str) {
	match source {
		SourceKind::Staged => ("staged", ""),
		SourceKind::Unstaged => ("unstaged", ""),
		SourceKind::Working => ("untracked", ""),
		SourceKind::File => ("file", ""),
		SourceKind::Commit { rev } => {
			("commit@", rev.get(..7).unwrap_or(rev.as_str()))
		}
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
					let summary = summarize_with_identity(&git, &id, &opts)
						.map_err(|err| err.to_string());
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
	cancel: CancelToken,
) -> Result<(browser::SourcePreview, PreviewSource), String> {
	if matches!(source, SourceKind::File) {
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
		// A failed/cancelled Git source must never be replaced by current
		// working-tree bytes. Explicit File/tree sources are handled above.
		Err(e) => Err(e.to_string()),
	}
}

/// `--workspace`, else the last remembered workspace (IntelliJ reopens the
/// last project), else the launch folder. A Finder or Explorer launch starts
/// in `/` or the home folder: that opens nothing rather than scanning it.
fn startup_workspace(arg: Option<PathBuf>) -> Option<PathBuf> {
	if arg.is_some() {
		return arg;
	}
	if let Some(last) = recent::load().into_iter().next() {
		return Some(last);
	}
	let cwd = std::env::current_dir().ok()?;
	(cwd.parent().is_some() && Some(&cwd) != recent::home().as_ref())
		.then_some(cwd)
}

fn parse_cli_args() -> (Option<PathBuf>, String, Option<PathBuf>) {
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
	let workspace = startup_workspace(workspace);
	let app = Application::new().with_assets(icons::Assets);

	app.run(move |cx: &mut App| {
		cx.bind_keys(key_bindings());
		theme::register_fonts(cx);

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
	fn prepared_removal_uses_admitted_identity_after_root_deletion() {
		let temp = tempfile::tempdir().unwrap();
		let path = temp.path().join("selected");
		std::fs::create_dir(&path).unwrap();
		let other_dir = tempfile::tempdir().unwrap();
		let root = CanonicalRootId::new(&path).unwrap();
		#[cfg(unix)]
		let presented = {
			let alias = temp.path().join("presented");
			std::os::unix::fs::symlink(&path, &alias).unwrap();
			alias
		};
		#[cfg(not(unix))]
		let presented = {
			let alias = temp.path().join("parent");
			std::fs::create_dir(&alias).unwrap();
			alias.join("..").join("selected")
		};
		assert_eq!(CanonicalRootId::new(&presented).unwrap(), root);
		let other_root = CanonicalRootId::new(other_dir.path()).unwrap();
		let repo = RepoEntry {
			root: presented,
			name: "selected".into(),
			kind: RepoEntryKind::Main,
			identity: Some(RepoIdentity {
				toplevel: root.path().to_path_buf(),
				git_dir: path.join(".git"),
				common_dir: path.join(".git"),
				kind: RepoKind::Main,
			}),
			summary: Err("offline".into()),
		};
		let files: Vec<_> = ["same.txt", "kept.txt"]
			.into_iter()
			.map(|path| FileChangeItem {
				path: path.into(),
				source: SourceKind::Staged,
				change_type: None,
				is_conflict: false,
				selected: true,
				repo: 0,
			})
			.collect();
		let sha_a = "a".repeat(40);
		let sha_b = "b".repeat(40);
		let mut old = PreparedSelection::new(
			&files,
			&["same.txt".into(), "kept.txt".into()],
			&[],
		);
		old.replace_file_group = true;
		old.replace_git_group = true;
		assert!(old.sync_root(root.clone(), true, Some(0)));
		assert!(old.toggle_revision(root.clone(), &sha_a, "same.txt"));
		assert!(old.toggle_revision(root.clone(), &sha_b, "same.txt"));
		assert!(old.toggle_revision(other_root.clone(), &sha_a, "same.txt"));
		old.basket.reserve_exact(7);
		for (_, items) in &mut old.basket {
			items.reserve_exact(19);
		}
		let reserve = MAX_RETAINED_TREE_BYTES - old.retained_bytes() - 2048;
		old.files[0].path.reserve_exact(reserve);
		let old_bytes = old.retained_bytes();
		assert!(
			old_bytes < MAX_RETAINED_TREE_BYTES
				&& old_bytes > MAX_RETAINED_TREE_BYTES - 4096
		);
		let before = old.basket.clone();
		std::fs::remove_dir(&path).unwrap();
		assert!(
			CanonicalRootId::new(&repo.root).is_err(),
			"the real root has disappeared"
		);
		let admitted = admitted_selection_root(&repo, &old.basket)
			.expect("stored canonical identity survives deletion");
		assert_eq!(admitted, root);
		let mut removal =
			PreparedSelection::new(&old.files, &old.paths, &old.basket);
		removal.replace_file_group = true;
		removal.replace_git_group = true;
		removal.remove_only = true;
		removal.files[0].selected = false;
		removal.paths.retain(|path| path != "same.txt");
		let kept_item = removal
			.basket
			.iter_mut()
			.find(|(id, _)| id == &root)
			.unwrap()
			.1
			.iter_mut()
			.find(|item| {
				item.relative_path == "kept.txt"
					&& item.source == SourceKind::Staged
			})
			.unwrap();
		kept_item.relative_path.reserve_exact(4096);
		let retained_path = (
			kept_item.relative_path.as_ptr(),
			kept_item.relative_path.capacity(),
		);
		removal.status = Some(Msg::new("status_selection_removed", []));
		assert!(removal.sync_root(admitted, true, Some(0)));
		assert!(removal.retained_bytes() < old_bytes);
		assert!(removal.fits_replacing(
			MAX_RETAINED_TREE_BYTES,
			old_bytes,
			MAX_RETAINED_TREE_BYTES
		));
		let kept =
			&removal.basket.iter().find(|(id, _)| id == &root).unwrap().1;
		assert_eq!(kept.len(), 4);
		let still_selected = kept
			.iter()
			.find(|item| {
				item.relative_path == "kept.txt"
					&& item.source == SourceKind::Staged
			})
			.unwrap();
		assert_eq!((still_selected.relative_path.as_ptr(), still_selected.relative_path.capacity()), retained_path,
			"toggle-off filters in place instead of rebuilding the remaining group");
		assert!(kept.iter().all(|item| item.relative_path == "kept.txt" || matches!(&item.source, SourceKind::Commit { rev } if rev == &sha_a || rev == &sha_b)));
		assert_eq!(
			removal
				.basket
				.iter()
				.find(|(id, _)| id == &other_root)
				.unwrap()
				.1,
			before.iter().find(|(id, _)| id == &other_root).unwrap().1
		);
		assert_eq!(old.basket, before);
		for file in &mut removal.files {
			file.selected = false;
		}
		removal.paths = Vec::new();
		assert!(removal.sync_root(root.clone(), true, Some(0)));
		let kept =
			&removal.basket.iter().find(|(id, _)| id == &root).unwrap().1;
		assert_eq!(
			kept.len(),
			2,
			"Deselect All keeps both explicit historical selections"
		);
		assert!(kept
			.iter()
			.all(|item| matches!(item.source, SourceKind::Commit { .. })));
		assert!(!removal.toggle_revision(root.clone(), &sha_a, "same.txt"));
		assert!(removal.fits_replacing(
			MAX_RETAINED_TREE_BYTES,
			old_bytes,
			MAX_RETAINED_TREE_BYTES
		));
	}

	#[test]
	fn lossy_change_names_are_not_checkable() {
		let item = |path: String| FileChangeItem {
			path,
			change_type: None,
			source: SourceKind::Working,
			is_conflict: false,
			selected: false,
			repo: 0,
		};
		let lossy = String::from_utf8_lossy(b"bad\xff.txt").into_owned();
		assert!(!item(lossy).is_valid_utf8());
		assert!(item("長路徑/good.txt".into()).is_valid_utf8());
	}

	#[test]
	fn basket_order_key_matches_the_source_summary_text() {
		let sources = [
			SourceKind::Staged,
			SourceKind::Unstaged,
			SourceKind::Working,
			SourceKind::File,
			SourceKind::Commit {
				rev: "b".repeat(40),
			},
			SourceKind::Commit {
				rev: "a".repeat(40),
			},
			SourceKind::Commit { rev: "abc".into() },
		];
		for a in &sources {
			for b in &sources {
				assert_eq!(
					source_order(a).cmp(&source_order(b)),
					WorkbenchModel::source_summary(a)
						.cmp(&WorkbenchModel::source_summary(b)),
					"{a:?} vs {b:?}"
				);
			}
		}
	}

	#[test]
	fn project_intent_preserves_git_group_while_change_list_is_unloaded() {
		let dir = tempfile::tempdir().unwrap();
		let root = CanonicalRootId::new(dir.path()).unwrap();
		let staged = ExportItem {
			root: root.clone(),
			relative_path: "same.txt".into(),
			source: SourceKind::Staged,
			change_type: None,
		};
		let historical = ExportItem {
			root: root.clone(),
			relative_path: "same.txt".into(),
			source: SourceKind::Commit {
				rev: "a".repeat(40),
			},
			change_type: None,
		};
		let mut candidate = PreparedSelection::new(
			&[],
			&["new.txt".into()],
			&[(root.clone(), vec![staged.clone(), historical.clone()])],
		);
		candidate.replace_file_group = true;
		assert!(candidate.sync_root(root, true, Some(0)));
		assert!(candidate.basket[0].1.contains(&staged));
		assert!(candidate.basket[0].1.contains(&historical));
		assert_eq!(candidate.basket[0].1.len(), 3);
	}

	#[test]
	fn prepared_selection_is_whole_and_keeps_root_source_and_full_oid() {
		let one = tempfile::tempdir().unwrap();
		let two = tempfile::tempdir().unwrap();
		let root = CanonicalRootId::new(one.path()).unwrap();
		let other_root = CanonicalRootId::new(two.path()).unwrap();
		let sha_a = "a".repeat(40);
		let sha_b = "b".repeat(40);
		let files: Vec<_> = [
			SourceKind::Staged,
			SourceKind::Unstaged,
			SourceKind::Working,
		]
		.into_iter()
		.map(|source| FileChangeItem {
			path: "same.txt".into(),
			change_type: None,
			source,
			is_conflict: false,
			selected: false,
			repo: 0,
		})
		.collect();
		let mut old = PreparedSelection::new(&files, &["same.txt".into()], &[]);
		old.replace_file_group = true;
		old.replace_git_group = true;
		assert!(old.sync_root(root.clone(), true, Some(0)));
		assert!(old.toggle_revision(other_root.clone(), &sha_b, "same.txt"));
		let before = old.basket.clone();
		let old_bytes = old.retained_bytes();
		let mut candidate =
			PreparedSelection::new(&old.files, &old.paths, &old.basket);
		for file in &mut candidate.files {
			file.selected = true;
		}
		assert!(candidate.toggle_revision(root.clone(), &sha_a, "same.txt"));
		assert!(candidate.toggle_revision(root.clone(), &sha_b, "same.txt"));
		candidate.replace_file_group = true;
		candidate.replace_git_group = true;
		assert!(candidate.sync_root(root.clone(), true, Some(0)));
		let bytes = candidate.retained_bytes();
		assert!(bytes > old_bytes);
		let other_owners = 173;
		assert!(candidate.fits_replacing(
			other_owners + old_bytes,
			old_bytes,
			other_owners + bytes
		));
		assert!(!candidate.fits_replacing(
			other_owners + old_bytes,
			old_bytes,
			other_owners + bytes - 1
		));
		assert!(!candidate.fits_replacing(
			old_bytes - 1,
			old_bytes,
			usize::MAX
		));
		assert_eq!(
			old.basket, before,
			"refusing the intent cannot install a prefix"
		);
		assert!(old.files.iter().all(|file| !file.selected));
		let items = &candidate
			.basket
			.iter()
			.find(|(id, _)| id == &root)
			.unwrap()
			.1;
		assert_eq!(
			items.len(),
			6,
			"File, index, working changes and both commits stay distinct"
		);
		for source in [
			SourceKind::File,
			SourceKind::Staged,
			SourceKind::Unstaged,
			SourceKind::Working,
			SourceKind::Commit { rev: sha_a.clone() },
			SourceKind::Commit { rev: sha_b.clone() },
		] {
			assert!(items.iter().any(|item| item.source == source
				&& item.relative_path == "same.txt"));
		}
		assert_eq!(
			candidate
				.basket
				.iter()
				.find(|(id, _)| id == &other_root)
				.unwrap()
				.1,
			before.iter().find(|(id, _)| id == &other_root).unwrap().1
		);
	}

	#[test]
	fn prepared_selection_counts_spare_storage_and_new_status_before_install() {
		let temp = tempfile::tempdir().unwrap();
		let root = CanonicalRootId::new(temp.path()).unwrap();
		let mut candidate =
			PreparedSelection::new(&[], &["same.txt".into()], &[]);
		assert!(candidate.toggle_revision(
			root.clone(),
			&"a".repeat(40),
			"same.txt"
		));
		candidate.replace_file_group = true;
		assert!(candidate.sync_root(root, true, Some(0)));
		let before = candidate.retained_bytes();
		let slots = candidate.paths.capacity();
		candidate.paths.reserve_exact(31);
		let path_cap = candidate.paths[0].capacity();
		candidate.paths[0].reserve_exact(4096);
		let (root_slots, item_slots) = (
			candidate.basket.capacity(),
			candidate.basket[0].1.capacity(),
		);
		candidate.basket.reserve_exact(5);
		candidate.basket[0].1.reserve_exact(17);
		let after = candidate.retained_bytes();
		assert_eq!(
			after - before,
			(candidate.paths.capacity() - slots)
				* std::mem::size_of::<String>()
				+ candidate.paths[0].capacity()
				- path_cap + (candidate.basket.capacity() - root_slots)
				* std::mem::size_of::<(CanonicalRootId, Vec<ExportItem>)>()
				+ (candidate.basket[0].1.capacity() - item_slots)
					* std::mem::size_of::<ExportItem>()
		);
		let mut status_path = String::with_capacity(8192);
		status_path.push_str("same.txt");
		candidate.status = Some(Msg::new("status_toggled_file", [status_path]));
		assert_eq!(
			candidate.retained_bytes() - after,
			message_bytes(candidate.status.as_ref().unwrap())
		);
		assert!(!candidate.fits_replacing(before, before, after));
	}

	#[test]
	fn prepared_selection_stops_repeated_root_amplification() {
		let temp = tempfile::tempdir().unwrap();
		let root_path = temp.path().join("r".repeat(200));
		std::fs::create_dir(&root_path).unwrap();
		let root = CanonicalRootId::new(root_path).unwrap();
		let paths: Vec<_> = (0..40_000).map(|i| format!("p{i:06}")).collect();
		let mut candidate = PreparedSelection::new(&[], &paths, &[]);
		candidate.replace_file_group = true;
		assert!(
			candidate.retained_bytes()
				+ root.retained_heap_bytes()
				+ paths.len() * std::mem::size_of::<ExportItem>()
				< MAX_RETAINED_TREE_BYTES,
			"the whole slot allocation fits; repeated root/path heaps must reject"
		);
		assert!(!candidate.sync_root(root, true, Some(0)), "per-item root/path copies must be checked while building the whole basket");
		assert!(
			candidate.basket.is_empty(),
			"a rejected partial group is never installed"
		);
		assert_eq!(paths.len(), 40_000);
	}

	#[test]
	fn basket_display_clips_on_utf8_boundaries_without_growing_past_limit() {
		let mut text = String::new();
		assert!(append_basket_display(&mut text, "prefix "));
		assert!(!append_basket_display(
			&mut text,
			&"漢".repeat(MAX_BASKET_DISPLAY_BYTES)
		));
		assert!(text.len() <= MAX_BASKET_DISPLAY_BYTES && text.ends_with('…'));
		let mut exact = "a".repeat(MAX_BASKET_DISPLAY_BYTES);
		assert!(!append_basket_display(&mut exact, "next"));
		assert_eq!(exact.len(), MAX_BASKET_DISPLAY_BYTES);
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
			selected: false,
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

	#[test]
	fn change_dir_checkbox_is_tri_state() {
		let (_, mut files) = nested_repo();
		let under = |dir: &'static str| {
			move |f: &FileChangeItem| {
				menu::change_group(f) == Some("unstaged")
					&& menu::path_under(&f.path, dir)
			}
		};
		assert_eq!(menu::rows_tri_state(&files, under("src")), Some(false));
		// A prefix that is not a whole directory name does not match.
		assert!(!menu::path_under("src2/a", "src"));
		assert!(!menu::path_under("src", "src"));
		for f in &mut files {
			if f.path.starts_with("src/main/") {
				f.selected = true;
			}
		}
		assert_eq!(menu::rows_tri_state(&files, under("src")), None);
		assert_eq!(
			menu::rows_tri_state(&files, under("src/main/java/pkg")),
			Some(true)
		);
		for f in &mut files {
			if f.path.starts_with("src/") {
				f.selected = true;
			}
		}
		assert_eq!(menu::rows_tri_state(&files, under("src")), Some(true));
		// Only files of that group count: the staged `docs` file is not
		// under an Unstaged directory.
		assert_eq!(menu::rows_tri_state(&files, under("docs")), Some(false));
	}
}
