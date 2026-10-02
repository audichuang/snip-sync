//! Expandable working tree for the File Explorer.
//!
//! Pages come from one [`DirectoryScan`] per open directory. The UI thread
//! never reads the filesystem: it starts a [`TreeIo`] job, a background
//! worker runs [`execute_tree_io`], and [`FileTreeNode::apply_io_result`]
//! admits only what still fits. Selection is a path set on the root, not a
//! flag that disappears when a page is evicted.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use snip_core::gitrun::CancelToken;
use snip_core::workspace::{
	DirectoryScan, ScanBudget, ScanEntry, ScanError, ScanStatus,
};

pub struct FileTreeNode {
	pub name: String,
	pub rel_path: String,
	pub full_path: PathBuf,
	pub is_dir: bool,
	pub is_nested_repo: bool,
	pub is_expanded: bool,
	pub is_loaded: bool,
	pub is_truncated: bool,
	pub has_more: bool,
	pub is_valid_utf8: bool,
	pub read_error: Option<String>,
	pub selected: bool,
	pub children: Vec<FileTreeNode>,
	pub depth: usize,
	key: NodeKey,
	scan: Option<DirectoryScan>,
	held: Option<ScanEntry>,
	loading: bool,
	load_epoch: u64,
	selected_paths: Vec<String>,
	budget_blocked: bool,
	extra_rows: usize,
	/// How many direct children are shown before a continuation row.
	row_window: usize,
}

impl std::fmt::Debug for FileTreeNode {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("FileTreeNode")
			.field("name", &self.name)
			.field("rel_path", &self.rel_path)
			.field("is_dir", &self.is_dir)
			.field("is_expanded", &self.is_expanded)
			.field("has_more", &self.has_more)
			.field("children", &self.children.len())
			.field("has_scan", &self.scan.is_some())
			.finish()
	}
}

/// Identity of a tree node. Empty is the working-tree root. Components keep
/// their original bytes, so a non-UTF-8 name cannot alias a real path or a
/// marker string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeKey {
	relative: PathBuf,
}

impl NodeKey {
	pub fn root() -> Self {
		Self {
			relative: PathBuf::new(),
		}
	}

	pub fn is_root(&self) -> bool {
		self.relative.as_os_str().is_empty()
	}

	pub fn from_utf8_rel(rel: &str) -> Self {
		if rel.is_empty() {
			Self::root()
		} else {
			Self {
				relative: PathBuf::from(rel),
			}
		}
	}

	pub fn child(&self, name: &OsStr) -> Self {
		Self {
			relative: self.relative.join(name),
		}
	}

	/// `None` when any component is not Unicode. The root is `Some("")`.
	pub fn utf8_rel(&self) -> Option<String> {
		self.relative.to_str().map(|s| s.replace('\\', "/"))
	}

	pub fn id_hex(&self) -> String {
		let bytes = self.relative.as_os_str().as_encoded_bytes();
		let mut out = String::with_capacity(bytes.len() * 2);
		for b in bytes {
			out.push_str(&format!("{b:02x}"));
		}
		out
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowGesture {
	Primary,
	Toggle,
	Expand,
	Collapse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeCommand {
	Expand(NodeKey),
	Collapse(NodeKey),
	LoadMore(NodeKey),
	Retry(NodeKey),
	ToggleSelect(NodeKey),
	RevealMoreRows(NodeKey),
	OpenFile(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeIoKind {
	Expand,
	LoadMore,
	Retry,
}

// A transient command result, immediately unpacked by callers; only TreeIo
// is queued. Its Windows directory cursor is large, but boxing this enum's
// payload would add an allocation to every directory request.
#[allow(clippy::large_enum_variant)]
pub enum TreeEffect {
	Idle,
	OpenFile(String),
	Io(TreeIo),
}

pub struct TreeIo {
	pub key: NodeKey,
	pub epoch: u64,
	pub dir: PathBuf,
	pub base: PathBuf,
	pub depth: usize,
	pub byte_budget: usize,
	pub scan: Option<DirectoryScan>,
	pub held: Option<ScanEntry>,
	pub replace_children: bool,
	pub kind: TreeIoKind,
}

impl TreeIo {
	/// Complete queued-input storage. Cursor/entry headers are inline in Self.
	pub fn retained_bytes(&self) -> usize {
		std::mem::size_of::<Self>()
			.saturating_add(self.key.relative.capacity())
			.saturating_add(self.dir.capacity())
			.saturating_add(self.base.capacity())
			.saturating_add(self.scan.as_ref().map_or(0, |scan| {
				scan.retained_bytes() - std::mem::size_of::<DirectoryScan>()
			}))
			.saturating_add(
				self.held.as_ref().map_or(0, |entry| entry.name.capacity()),
			)
	}
}

pub struct TreeIoResult {
	pub key: NodeKey,
	pub epoch: u64,
	pub base: PathBuf,
	pub kind: TreeIoKind,
	pub children: Vec<FileTreeNode>,
	pub scan: Option<DirectoryScan>,
	pub held: Option<ScanEntry>,
	pub has_more: bool,
	pub error: Option<String>,
	pub truncated: bool,
	pub skipped_oversize: usize,
	pub budget_blocked: bool,
	pub cancelled: bool,
	pub replace_children: bool,
}

pub struct TreeApply {
	pub kind: TreeIoKind,
	pub rel: String,
	pub child_count: usize,
	pub has_more: bool,
	pub selected_count: usize,
}

#[derive(Debug, Clone)]
pub struct FlattenedTreeRow {
	pub name: String,
	pub rel_path: String,
	pub id_suffix: String,
	pub key: NodeKey,
	pub is_dir: bool,
	pub is_nested_repo: bool,
	pub is_expanded: bool,
	pub is_truncation_marker: bool,
	pub is_more_marker: bool,
	pub is_view_limit: bool,
	pub is_error: bool,
	pub is_loading: bool,
	pub is_valid_utf8: bool,
	pub selected: bool,
	pub depth: usize,
}

pub const MAX_DIR_ENTRIES: usize = 100;
pub const MAX_VISIBLE_ROWS: usize = 300;
/// Direct children kept in the list before a reveal row. Large enough that a
/// normal repository stays fully listed; a continuation row is placed first.
pub const DIR_PAGE_ROWS: usize = MAX_VISIBLE_ROWS;
pub const MAX_RETAINED_WORKING_TREE_BYTES: usize = 256 * 1024;

/// `target` equals `parent` or is a path-component child of `parent`.
pub fn is_component_child_or_exact(target: &str, parent: &str) -> bool {
	if parent.is_empty() || target.is_empty() {
		return false;
	}
	if target == parent {
		return true;
	}
	if let Some(rest) = target.strip_prefix(parent) {
		return rest.starts_with('/') || rest.starts_with('\\');
	}
	false
}

fn key_within(key: &NodeKey, parent: &NodeKey) -> bool {
	if parent.is_root() {
		return !key.is_root();
	}
	let p = parent.relative.as_os_str().as_encoded_bytes();
	let c = key.relative.as_os_str().as_encoded_bytes();
	c.len() > p.len()
		&& c.starts_with(p)
		&& (c[p.len()] == b'/' || c[p.len()] == b'\\')
}

fn shrink_path(path: PathBuf) -> PathBuf {
	let mut os = path.into_os_string();
	os.shrink_to_fit();
	PathBuf::from(os)
}

/// Heap bytes of a sorted path selection, including unused vector slots.
pub(crate) fn selection_bytes(paths: &Vec<String>) -> usize {
	paths.iter().fold(
		paths
			.capacity()
			.saturating_mul(std::mem::size_of::<String>()),
		|bytes, path| bytes.saturating_add(path.capacity()),
	)
}

fn normalize_selection(paths: &mut Vec<String>) {
	paths.retain(|path| !path.is_empty());
	paths.sort();
	paths.dedup();
}

fn nested_git(dir: &Path) -> bool {
	match std::fs::symlink_metadata(dir.join(".git")) {
		Ok(meta) => meta.is_dir() || meta.is_file(),
		Err(_) => false,
	}
}

/// Mouse and keyboard share this mapping. Marker rows carry the directory
/// key; they never encode the action as a path suffix.
pub fn command_for_row(
	row: &FlattenedTreeRow,
	gesture: RowGesture,
) -> Option<TreeCommand> {
	if row.is_loading {
		return None;
	}
	if row.is_view_limit {
		return match gesture {
			RowGesture::Collapse => None,
			_ => Some(TreeCommand::RevealMoreRows(row.key.clone())),
		};
	}
	if row.is_truncation_marker {
		return None;
	}
	if row.is_error {
		return match gesture {
			RowGesture::Collapse => None,
			_ => Some(TreeCommand::Retry(row.key.clone())),
		};
	}
	if row.is_more_marker {
		return match gesture {
			RowGesture::Collapse => None,
			_ => Some(TreeCommand::LoadMore(row.key.clone())),
		};
	}
	if !row.is_valid_utf8 {
		return None;
	}
	match gesture {
		RowGesture::Toggle => Some(TreeCommand::ToggleSelect(row.key.clone())),
		RowGesture::Expand if row.is_dir && !row.is_expanded => {
			Some(TreeCommand::Expand(row.key.clone()))
		}
		RowGesture::Collapse if row.is_dir && row.is_expanded => {
			Some(TreeCommand::Collapse(row.key.clone()))
		}
		RowGesture::Expand | RowGesture::Collapse => None,
		RowGesture::Primary if row.is_dir => {
			if row.is_expanded {
				Some(TreeCommand::Collapse(row.key.clone()))
			} else {
				Some(TreeCommand::Expand(row.key.clone()))
			}
		}
		RowGesture::Primary if row.rel_path.is_empty() => None,
		RowGesture::Primary => {
			Some(TreeCommand::OpenFile(row.rel_path.clone()))
		}
	}
}

/// Reads at most one admitted page. Checks `cancel` between entries.
/// A name that cannot fit in [`MAX_RETAINED_WORKING_TREE_BYTES`] is skipped
/// so a single path cannot pin the cursor.
pub fn execute_tree_io(io: TreeIo, cancel: &CancelToken) -> TreeIoResult {
	let mut result = TreeIoResult {
		key: io.key.clone(),
		epoch: io.epoch,
		base: io.base.clone(),
		kind: io.kind,
		children: Vec::new(),
		scan: None,
		held: None,
		has_more: false,
		error: None,
		truncated: false,
		skipped_oversize: 0,
		budget_blocked: false,
		cancelled: false,
		replace_children: io.replace_children,
	};
	if cancel.is_cancelled() {
		result.cancelled = true;
		result.scan = io.scan;
		result.held = io.held;
		result.has_more = result.scan.is_some() || result.held.is_some();
		return result;
	}
	let mut scan = match io.scan {
		Some(scan) => scan,
		None => match DirectoryScan::open(&io.dir) {
			Ok(scan) => scan,
			Err(err) => {
				result.error = Some(format!("無法讀取目錄: {err}"));
				return result;
			}
		},
	};
	let mut held = io.held;
	let mut room = io.byte_budget.saturating_sub(scan.retained_bytes());
	if room == 0 && held.is_none() {
		result.budget_blocked = true;
		result.has_more = true;
		result.scan = Some(scan);
		return result;
	}
	let mut stop = false;
	while !stop {
		if cancel.is_cancelled() {
			result.cancelled = true;
			result.has_more = true;
			break;
		}
		let entry = if let Some(entry) = held.take() {
			entry
		} else if result.children.len() >= MAX_DIR_ENTRIES {
			result.has_more = true;
			break;
		} else {
			let mut budget = ScanBudget::visits(1);
			budget.cancel = Some(cancel.clone());
			let page = match scan.next_page(&budget) {
				Ok(page) => page,
				Err(ScanError::Changed) => {
					result.error =
						Some("目錄在讀取期間改變了，請重試".to_string());
					return result;
				}
				Err(ScanError::Io(err)) => {
					result.error = Some(format!("無法讀取目錄: {err}"));
					return result;
				}
			};
			if page.entries.is_empty() {
				match page.status {
					ScanStatus::Complete => {
						result.has_more = false;
						break;
					}
					ScanStatus::Cancelled | ScanStatus::TimedOut => {
						result.cancelled = page.status == ScanStatus::Cancelled;
						result.has_more = true;
						break;
					}
					ScanStatus::More | ScanStatus::LimitReached => continue,
					ScanStatus::Incomplete => {
						result.truncated = true;
						result.has_more = false;
						break;
					}
				}
			}
			page.entries.into_iter().next().unwrap()
		};
		if room == 0 {
			held = Some(entry);
			result.budget_blocked = true;
			result.has_more = true;
			break;
		}
		let node = build_node(&entry, &io.dir, &io.key, io.depth, None);
		let cost = node.retained_bytes();
		if cost > MAX_RETAINED_WORKING_TREE_BYTES {
			result.skipped_oversize += 1;
			result.truncated = true;
			if result.skipped_oversize >= MAX_DIR_ENTRIES {
				result.has_more = true;
				break;
			}
			continue;
		}
		if cost > room {
			held = Some(entry);
			result.budget_blocked = true;
			result.has_more = true;
			stop = true;
		} else {
			room = room.saturating_sub(cost);
			result.children.push(node);
		}
	}
	if held.is_some() {
		result.has_more = true;
	}
	result.children.sort_by(|a, b| {
		b.is_dir.cmp(&a.is_dir).then_with(|| {
			a.key.relative.as_os_str().cmp(b.key.relative.as_os_str())
		})
	});
	result.scan = Some(scan);
	result.held = held;
	if result.error.is_some() {
		result.scan = None;
		result.held = None;
		result.has_more = false;
	}
	result
}

/// One child of a directory listed somewhere else (a remote worker).
pub struct ListedChild {
	pub name: String,
	/// False when the worker's name was not UTF-8 and `name` is lossy: the
	/// row is shown but cannot be addressed.
	pub utf8: bool,
	pub directory: bool,
	pub nested_repo: bool,
}

/// [`execute_tree_io`] for a directory listed by someone else: the whole
/// listing arrives at once, so there is no cursor to keep and no page to
/// continue. Children are admitted under the same byte budget.
pub fn listed_tree_result(
	io: TreeIo,
	listed: Result<(Vec<ListedChild>, bool), String>,
) -> TreeIoResult {
	let mut result = TreeIoResult {
		key: io.key.clone(),
		epoch: io.epoch,
		base: io.base.clone(),
		kind: io.kind,
		children: Vec::new(),
		scan: None,
		held: None,
		has_more: false,
		error: None,
		truncated: false,
		skipped_oversize: 0,
		budget_blocked: false,
		cancelled: false,
		replace_children: io.replace_children,
	};
	let (children, truncated) = match listed {
		Ok(listed) => listed,
		Err(err) => {
			result.error = Some(err);
			return result;
		}
	};
	result.truncated = truncated;
	let mut room = io.byte_budget;
	for child in children {
		let entry = ScanEntry {
			name: child.name.into(),
			directory: child.directory,
			symlink: false,
		};
		let mut node = build_node(
			&entry,
			&io.dir,
			&io.key,
			io.depth,
			Some(child.nested_repo),
		);
		if !child.utf8 {
			node.is_valid_utf8 = false;
			node.rel_path = String::new();
			node.read_error = Some("non-UTF-8 filename: unselectable".into());
		}
		let cost = node.retained_bytes();
		if cost > room {
			result.truncated = true;
			break;
		}
		room -= cost;
		result.children.push(node);
	}
	result
}

/// `nested`: whether the entry is a repo of its own, when the caller knows
/// (a remote listing); `None` looks on this machine's disk.
fn build_node(
	entry: &ScanEntry,
	dir: &Path,
	parent: &NodeKey,
	depth: usize,
	nested: Option<bool>,
) -> FileTreeNode {
	let key = parent.child(&entry.name);
	let utf8_name = entry.utf8_name().map(str::to_string);
	let rel = match &utf8_name {
		Some(_) => key.utf8_rel(),
		None => None,
	};
	let is_utf8 = rel.is_some();
	let mut name =
		utf8_name.unwrap_or_else(|| entry.name.to_string_lossy().into_owned());
	name.shrink_to_fit();
	let mut rel_path = if is_utf8 {
		rel.unwrap_or_default()
	} else {
		String::new()
	};
	rel_path.shrink_to_fit();
	let full = dir.join(&entry.name);
	let is_dir = entry.directory;
	let mut read_error = if is_utf8 {
		None
	} else {
		Some("non-UTF-8 filename: unselectable".to_string())
	};
	if let Some(err) = read_error.as_mut() {
		err.shrink_to_fit();
	}
	FileTreeNode {
		name,
		rel_path,
		full_path: PathBuf::new(),
		is_dir,
		is_nested_repo: is_dir && nested.unwrap_or_else(|| nested_git(&full)),
		is_expanded: false,
		is_loaded: false,
		is_truncated: false,
		has_more: false,
		is_valid_utf8: is_utf8,
		read_error,
		selected: false,
		children: Vec::new(),
		depth,
		key: NodeKey {
			relative: shrink_path(key.relative),
		},
		scan: None,
		held: None,
		loading: false,
		load_epoch: 0,
		selected_paths: Vec::new(),
		budget_blocked: false,
		extra_rows: 0,
		row_window: DIR_PAGE_ROWS,
	}
}

impl FileTreeNode {
	pub fn unloaded_root(root: &Path) -> Self {
		let mut full_path = root.to_path_buf();
		full_path = shrink_path(full_path);
		Self {
			name: root
				.file_name()
				.map(|n| n.to_string_lossy().into_owned())
				.unwrap_or_else(|| "root".to_string()),
			rel_path: String::new(),
			full_path,
			is_dir: true,
			is_nested_repo: false,
			is_expanded: true,
			is_loaded: false,
			is_truncated: false,
			has_more: false,
			is_valid_utf8: true,
			read_error: None,
			selected: false,
			children: Vec::new(),
			depth: 0,
			key: NodeKey::root(),
			scan: None,
			held: None,
			loading: true,
			load_epoch: 0,
			selected_paths: Vec::new(),
			budget_blocked: false,
			extra_rows: 0,
			row_window: DIR_PAGE_ROWS,
		}
	}

	/// Blocking load for tests and other non-UI callers. The window calls
	/// [`Self::start`] and [`execute_tree_io`] on a background worker.
	pub fn new_root(root: &Path) -> Self {
		let mut tree = Self::unloaded_root(root);
		tree.drive(TreeCommand::Expand(NodeKey::root()));
		tree
	}

	pub fn visible_limit(&self) -> usize {
		MAX_VISIBLE_ROWS.saturating_add(self.extra_rows)
	}

	pub fn contains_dir(&self, key: &NodeKey) -> bool {
		self.find(key).is_some_and(|node| node.is_dir)
	}

	pub fn clear_loading(&mut self) {
		if self.loading {
			self.loading = false;
			self.load_epoch = self.load_epoch.wrapping_add(1);
		}
		if !self.is_loaded {
			self.is_expanded = false;
		}
		for child in &mut self.children {
			child.clear_loading();
		}
	}

	/// Node/cursor cache only. Persistent paths survive cache eviction and
	/// belong to the aggregate tree/selection budget instead of the256KiB cache.
	pub fn cache_bytes(&self) -> usize {
		let mut total = std::mem::size_of::<Self>();
		total = total.saturating_add(self.name.capacity());
		total = total.saturating_add(self.rel_path.capacity());
		total = total.saturating_add(self.full_path.capacity());
		total = total.saturating_add(self.key.relative.capacity());
		if let Some(err) = &self.read_error {
			total = total.saturating_add(err.capacity());
		}
		if let Some(scan) = &self.scan {
			// The Option's inline cursor storage is already in Self.
			total = total.saturating_add(
				scan.retained_bytes() - std::mem::size_of::<DirectoryScan>(),
			);
		}
		if let Some(held) = &self.held {
			total = total.saturating_add(held.name.capacity());
		}
		let spare = self.children.capacity() - self.children.len();
		total = total
			.saturating_add(spare.saturating_mul(std::mem::size_of::<Self>()));
		for child in &self.children {
			total = total.saturating_add(child.cache_bytes());
		}
		total
	}

	pub fn selection_bytes(&self) -> usize {
		self.children
			.iter()
			.fold(selection_bytes(&self.selected_paths), |bytes, child| {
				bytes.saturating_add(child.selection_bytes())
			})
	}

	pub fn retained_bytes(&self) -> usize {
		self.cache_bytes().saturating_add(self.selection_bytes())
	}

	pub fn start(&mut self, cmd: TreeCommand) -> TreeEffect {
		match cmd {
			TreeCommand::OpenFile(path) => TreeEffect::OpenFile(path),
			TreeCommand::RevealMoreRows(key) => {
				if let Some(node) = self.find_mut(&key) {
					node.row_window =
						node.row_window.saturating_add(DIR_PAGE_ROWS);
				}
				TreeEffect::Idle
			}
			TreeCommand::LoadMore(key) => {
				if let Some(node) = self.find_mut(&key) {
					node.row_window =
						node.row_window.saturating_add(DIR_PAGE_ROWS);
				}
				self.begin_more(&key)
			}
			TreeCommand::ToggleSelect(key) => {
				self.toggle_key(&key);
				TreeEffect::Idle
			}
			TreeCommand::Collapse(key) => {
				if !key.is_root() {
					if let Some(node) = self.find_mut(&key) {
						node.collapse();
					}
				}
				TreeEffect::Idle
			}
			TreeCommand::Expand(key) => self.begin_expand(&key),

			TreeCommand::Retry(key) => self.begin_retry(&key),
		}
	}

	pub fn apply_io_result(
		&mut self,
		mut result: TreeIoResult,
	) -> Option<TreeApply> {
		let rel = result.key.utf8_rel().unwrap_or_default();
		let kind = result.kind;
		let key = result.key.clone();
		{
			let node = self.find_mut(&key)?;
			if node.load_epoch != result.epoch {
				return None;
			}
			node.loading = false;
			if result.cancelled {
				if !node.is_loaded {
					node.is_expanded = false;
				}
				node.scan = result.scan.take();
				node.held = result.held.take();
				node.has_more = node.scan.is_some() || node.held.is_some();
				return None;
			}
			node.is_expanded = true;
			node.is_loaded = true;
			node.read_error = result.error.take();
			node.is_truncated = result.truncated || result.skipped_oversize > 0;
			node.budget_blocked = result.budget_blocked;
			node.held = result.held.take();
			node.has_more = result.has_more || node.held.is_some();
			node.scan = if node.has_more {
				result.scan.take()
			} else {
				None
			};
			if result.replace_children {
				node.children = Vec::new();
			}
			node.children.append(&mut result.children);
			node.children.sort_by(|a, b| {
				b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
			});
			node.children.shrink_to_fit();
		}
		self.sync_flags();
		self.reclaim_until_fit();
		let (child_count, has_more) = self
			.find(&key)
			.map(|node| (node.children.len(), node.has_more))
			.unwrap_or((0, false));
		Some(TreeApply {
			kind,
			rel,
			child_count,
			has_more,
			selected_count: self.selected_paths.len(),
		})
	}

	pub fn toggle_expand(&mut self, rel: &str, _base: &Path) {
		let key = NodeKey::from_utf8_rel(rel);
		let open = self.find(&key).is_some_and(|node| {
			node.is_expanded && node.is_loaded && !node.loading
		});
		if open {
			self.drive(TreeCommand::Collapse(key));
		} else {
			self.drive(TreeCommand::Expand(key));
		}
	}

	pub fn load_more(&mut self, rel: &str, _base: &Path) {
		self.drive(TreeCommand::LoadMore(NodeKey::from_utf8_rel(rel)));
	}

	pub fn retry(&mut self, rel: &str, _base: &Path) {
		self.drive(TreeCommand::Retry(NodeKey::from_utf8_rel(rel)));
	}

	pub fn toggle_select(&mut self, rel: &str) {
		if rel.is_empty() {
			return;
		}
		self.drive(TreeCommand::ToggleSelect(NodeKey::from_utf8_rel(rel)));
	}

	pub fn selected_paths(&self) -> &[String] {
		&self.selected_paths
	}

	/// Prepares the whole path intent without changing visible checkboxes.
	pub fn selection_for_all(&self, selected: bool) -> Vec<String> {
		if !selected {
			return Vec::new();
		}
		let mut paths = self.selected_paths.clone();
		for child in &self.children {
			child.collect_selectable(&mut paths);
		}
		paths
	}

	pub fn selection_for_toggle(&self, key: &NodeKey) -> Option<Vec<String>> {
		if key.is_root() {
			return None;
		}
		let rel = key.utf8_rel().filter(|rel| !rel.is_empty())?;
		let node = self.find(key)?;
		// A nested repo's files belong to its own repo row.
		if !node.is_valid_utf8 || node.is_nested_repo {
			return None;
		}
		let mut paths = self.selected_paths.clone();
		match paths.binary_search(&rel) {
			Ok(_) => {
				paths.retain(|path| !is_component_child_or_exact(path, &rel));
			}
			// A folder is selected as itself; Copy walks it later.
			Err(index) => paths.insert(index, rel),
		}
		Some(paths)
	}

	/// A fresh selection of exactly `rels` (a folder as itself, never a
	/// nested repo), as a click or range selects rows.
	pub fn selection_for_rels(&self, rels: &[String]) -> Vec<String> {
		let mut paths = Vec::new();
		for rel in rels.iter().filter(|rel| !rel.is_empty()) {
			match self.find(&NodeKey::from_utf8_rel(rel)) {
				Some(node) if !node.is_valid_utf8 || node.is_nested_repo => {}
				Some(_) => {
					if let Err(index) = paths.binary_search(rel) {
						paths.insert(index, rel.clone());
					}
				}
				None => {}
			}
		}
		paths
	}

	/// The caller admits this complete allocation before transferring it here.
	pub fn install_selection(&mut self, mut selected: Vec<String>) {
		normalize_selection(&mut selected);
		self.selected_paths = selected;
		self.sync_flags();
	}

	pub fn apply_selection(&mut self, selected: &[String]) {
		self.install_selection(selected.to_vec());
	}

	pub fn set_all_selected(&mut self, selected: bool) {
		self.install_selection(self.selection_for_all(selected));
	}

	pub fn collect_selected_paths(&self, out: &mut Vec<String>) {
		out.extend(self.selected_paths.iter().cloned());
	}

	pub fn collect_expanded_paths(&self, out: &mut Vec<String>) {
		for child in &self.children {
			if child.is_dir && child.is_expanded && !child.rel_path.is_empty() {
				out.push(child.rel_path.clone());
				child.collect_expanded_paths(out);
			}
		}
	}

	pub fn flatten_visible(&self, max_rows: usize) -> Vec<FlattenedTreeRow> {
		let mut rows = Vec::new();
		self.collect_root(&mut rows);
		if max_rows == 0 {
			return Vec::new();
		}
		if rows.len() > max_rows {
			rows.truncate(max_rows.saturating_sub(1));
			rows.push(self.view_row());
		}
		rows
	}

	fn drive(&mut self, cmd: TreeCommand) {
		if let TreeEffect::Io(io) = self.start(cmd) {
			let result = execute_tree_io(io, &CancelToken::new());
			self.apply_io_result(result);
		}
	}

	fn begin_expand(&mut self, key: &NodeKey) -> TreeEffect {
		let Some(node) = self.find(key) else {
			return TreeEffect::Idle;
		};
		if !node.is_dir || (!key.is_root() && !node.is_valid_utf8) {
			return TreeEffect::Idle;
		}
		if node.loading && node.load_epoch > 0 {
			return TreeEffect::Idle;
		}
		if node.is_expanded && node.is_loaded && !node.loading {
			return TreeEffect::Idle;
		}
		self.begin_io(key, TreeIoKind::Expand)
	}

	fn begin_more(&mut self, key: &NodeKey) -> TreeEffect {
		let Some(node) = self.find(key) else {
			return TreeEffect::Idle;
		};
		let resumable = node.scan.is_some() || node.held.is_some();
		let has_more = node.has_more;
		if !resumable {
			if has_more {
				if let Some(node) = self.find_mut(key) {
					node.budget_blocked = true;
					node.read_error =
						Some("無法繼續：目錄游標已不在".to_string());
				}
			}
			return TreeEffect::Idle;
		}
		self.begin_io(key, TreeIoKind::LoadMore)
	}

	fn begin_retry(&mut self, key: &NodeKey) -> TreeEffect {
		let Some(node) = self.find(key) else {
			return TreeEffect::Idle;
		};
		if !node.is_dir || (!key.is_root() && !node.is_valid_utf8) {
			return TreeEffect::Idle;
		}
		self.begin_io(key, TreeIoKind::Retry)
	}

	fn begin_io(&mut self, key: &NodeKey, kind: TreeIoKind) -> TreeEffect {
		let base = self.full_path.clone();
		let prepared = {
			let Some(node) = self.find_mut(key) else {
				return TreeEffect::Idle;
			};
			node.load_epoch = node.load_epoch.wrapping_add(1);
			let epoch = node.load_epoch;
			let depth = node.depth.saturating_add(1);
			node.loading = true;
			node.is_expanded = true;
			node.read_error = None;
			node.budget_blocked = false;
			let mut scan = node.scan.take();
			let mut held = node.held.take();
			if matches!(kind, TreeIoKind::Retry | TreeIoKind::Expand) {
				node.children = Vec::new();
				node.is_loaded = false;
				node.has_more = false;
				if matches!(kind, TreeIoKind::Retry) {
					scan = None;
					held = None;
				}
			}
			(epoch, depth, scan, held)
		};
		let (epoch, depth, scan, held) = prepared;
		let byte_budget =
			MAX_RETAINED_WORKING_TREE_BYTES.saturating_sub(self.cache_bytes());
		TreeEffect::Io(TreeIo {
			key: key.clone(),
			epoch,
			dir: base.join(&key.relative),
			base,
			depth,
			byte_budget,
			scan,
			held,
			replace_children: matches!(
				kind,
				TreeIoKind::Retry | TreeIoKind::Expand
			),
			kind,
		})
	}

	fn find(&self, key: &NodeKey) -> Option<&Self> {
		if &self.key == key {
			return Some(self);
		}
		for child in &self.children {
			if &child.key == key || key_within(key, &child.key) {
				return child.find(key);
			}
		}
		None
	}

	fn find_mut(&mut self, key: &NodeKey) -> Option<&mut Self> {
		if &self.key == key {
			return Some(self);
		}
		for child in &mut self.children {
			if &child.key == key || key_within(key, &child.key) {
				return child.find_mut(key);
			}
		}
		None
	}

	fn collapse(&mut self) {
		self.children = Vec::new();
		self.is_expanded = false;
		self.is_loaded = false;
		self.is_truncated = false;
		self.has_more = false;
		self.read_error = None;
		self.scan = None;
		self.held = None;
		self.loading = false;
		self.budget_blocked = false;
		self.row_window = DIR_PAGE_ROWS;
		self.load_epoch = self.load_epoch.wrapping_add(1);
	}

	fn toggle_key(&mut self, key: &NodeKey) {
		if let Some(paths) = self.selection_for_toggle(key) {
			self.install_selection(paths);
		}
	}

	fn collect_selectable(&self, out: &mut Vec<String>) {
		if self.is_nested_repo {
			return;
		}
		if self.is_valid_utf8 && !self.rel_path.is_empty() {
			if let Err(index) = out.binary_search(&self.rel_path) {
				out.insert(index, self.rel_path.clone());
			}
		}
		for child in &self.children {
			child.collect_selectable(out);
		}
	}

	fn sync_flags(&mut self) {
		normalize_selection(&mut self.selected_paths);
		self.selected = self.is_valid_utf8
			&& !self.rel_path.is_empty()
			&& self.selected_paths.binary_search(&self.rel_path).is_ok();
		for child in &mut self.children {
			child.sync_flags_with(&self.selected_paths);
		}
	}

	fn sync_flags_with(&mut self, paths: &[String]) {
		self.selected = self.is_valid_utf8
			&& !self.rel_path.is_empty()
			&& paths.binary_search(&self.rel_path).is_ok();
		for child in &mut self.children {
			child.sync_flags_with(paths);
		}
	}

	fn reclaim_until_fit(&mut self) {
		let mut guard = 0;
		while self.cache_bytes() > MAX_RETAINED_WORKING_TREE_BYTES && guard < 64
		{
			guard += 1;
			if !self.collapse_deepest() {
				break;
			}
		}
	}

	fn collapse_deepest(&mut self) -> bool {
		let mut best: Option<(usize, NodeKey)> = None;
		self.find_deepest(0, &mut best);
		let Some((_, key)) = best else {
			return false;
		};
		let Some(node) = self.find_mut(&key) else {
			return false;
		};
		node.collapse();
		true
	}

	fn find_deepest(&self, depth: usize, best: &mut Option<(usize, NodeKey)>) {
		for child in &self.children {
			if child.is_dir && child.is_expanded {
				let here = depth + 1;
				let replace =
					best.as_ref().is_none_or(|(found, _)| here >= *found);
				if replace {
					*best = Some((here, child.key.clone()));
				}
				child.find_deepest(here, best);
			}
		}
	}

	fn collect_root(&self, rows: &mut Vec<FlattenedTreeRow>) {
		if self.loading {
			rows.push(self.loading_row());
		}
		if let Some(err) = &self.read_error {
			rows.push(self.error_row(err.clone()));
		}
		self.collect_window(rows);
	}

	fn collect_node(&self, rows: &mut Vec<FlattenedTreeRow>) {
		rows.push(self.node_row());
		if !(self.is_dir && self.is_expanded) {
			return;
		}
		if self.loading {
			rows.push(self.loading_row());
		}
		if let Some(err) = &self.read_error {
			rows.push(self.error_row(err.clone()));
		}
		self.collect_window(rows);
	}

	fn collect_window(&self, rows: &mut Vec<FlattenedTreeRow>) {
		let shown = self.row_window.max(1);
		// The continuation row is first so a short window can reach it
		// without scrolling past every loaded child.
		if self.has_more {
			rows.push(self.more_row());
		} else if self.children.len() > shown {
			rows.push(self.view_row());
		}
		for child in self.children.iter().take(shown) {
			child.collect_node(rows);
		}
		if self.is_truncated || self.budget_blocked {
			rows.push(self.trunc_row());
		}
	}

	fn node_row(&self) -> FlattenedTreeRow {
		FlattenedTreeRow {
			name: self.name.clone(),
			rel_path: self.rel_path.clone(),
			id_suffix: self.id_suffix(),
			key: self.key.clone(),
			is_dir: self.is_dir,
			is_nested_repo: self.is_nested_repo,
			is_expanded: self.is_expanded,
			is_truncation_marker: false,
			is_more_marker: false,
			is_view_limit: false,
			is_error: false,
			is_loading: false,
			is_valid_utf8: self.is_valid_utf8,
			selected: self.selected,
			depth: self.depth,
		}
	}

	fn id_suffix(&self) -> String {
		if self.is_valid_utf8 {
			self.rel_path.clone()
		} else {
			self.key.id_hex()
		}
	}

	fn marker_depth(&self) -> usize {
		if self.depth == 0 {
			0
		} else {
			self.depth + 1
		}
	}

	fn marker(&self, name: String, kind: MarkerKind) -> FlattenedTreeRow {
		FlattenedTreeRow {
			name,
			rel_path: self.rel_path.clone(),
			id_suffix: self.rel_path.clone(),
			key: self.key.clone(),
			is_dir: false,
			is_nested_repo: false,
			is_expanded: false,
			is_truncation_marker: kind == MarkerKind::Trunc,
			is_more_marker: kind == MarkerKind::More,
			is_view_limit: kind == MarkerKind::View,
			is_error: kind == MarkerKind::Error,
			is_loading: kind == MarkerKind::Loading,
			is_valid_utf8: true,
			selected: false,
			depth: self.marker_depth(),
		}
	}

	fn loading_row(&self) -> FlattenedTreeRow {
		self.marker("[載入中…]".to_string(), MarkerKind::Loading)
	}

	fn error_row(&self, err: String) -> FlattenedTreeRow {
		self.marker(err, MarkerKind::Error)
	}

	fn more_row(&self) -> FlattenedTreeRow {
		self.marker("[繼續載入更多檔案...]".to_string(), MarkerKind::More)
	}

	fn trunc_row(&self) -> FlattenedTreeRow {
		let name = if self.budget_blocked {
			"[已達工作樹保留上限，請先摺疊其他目錄]"
		} else {
			"[目錄未完整列出: 已截斷]"
		};
		self.marker(name.to_string(), MarkerKind::Trunc)
	}

	fn view_row(&self) -> FlattenedTreeRow {
		self.marker("[還有更多列，繼續顯示]".to_string(), MarkerKind::View)
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
	More,
	Error,
	Trunc,
	Loading,
	View,
}

#[cfg(test)]
mod tests {
	use std::collections::HashSet;
	#[cfg(unix)]
	use std::ffi::OsString;
	use std::fs;
	#[cfg(unix)]
	use std::os::unix::ffi::OsStringExt;

	use super::*;

	fn drive(tree: &mut FileTreeNode, cmd: TreeCommand) {
		if let TreeEffect::Io(io) = tree.start(cmd) {
			let result = execute_tree_io(io, &CancelToken::new());
			tree.apply_io_result(result);
		}
	}

	fn selected(tree: &FileTreeNode) -> Vec<String> {
		let mut paths = Vec::new();
		tree.collect_selected_paths(&mut paths);
		paths.sort();
		paths
	}

	#[cfg(unix)]
	fn tree_with_raw_entry(root: &Path, name: &[u8]) -> FileTreeNode {
		let mut tree = FileTreeNode::new_root(root);
		// APFS rejects these bytes on disk. Exercise the production entry-to-node
		// boundary here; Linux also checks real directory scanning below.
		let entry = ScanEntry {
			name: OsString::from_vec(name.to_vec()),
			directory: false,
			symlink: false,
		};
		tree.children
			.push(build_node(&entry, root, &NodeKey::root(), 1, None));
		tree
	}

	#[test]
	fn persistent_selection_capacity_is_separate_from_the_node_cache() {
		let dir = tempfile::tempdir().unwrap();
		let mut tree = FileTreeNode::unloaded_root(dir.path());
		let cache = tree.cache_bytes();
		let mut paths: Vec<_> = (0..2000)
			.map(|i| format!("{i:04}-{}", "p".repeat(200)))
			.collect();
		paths.reserve_exact(23);
		paths[0].reserve_exact(8192);
		let selected_bytes = selection_bytes(&paths);
		assert!(selected_bytes > MAX_RETAINED_WORKING_TREE_BYTES);
		tree.install_selection(paths);
		assert_eq!(tree.selection_bytes(), selected_bytes);
		assert_eq!(tree.cache_bytes(), cache);
		assert_eq!(tree.retained_bytes(), cache + selected_bytes);
		tree.reclaim_until_fit();
		assert_eq!(
			tree.selected_paths().len(),
			2000,
			"cache pressure never drops checked paths"
		);
		tree.set_all_selected(false);
		assert_eq!(tree.selection_bytes(), 0);
		assert_eq!(tree.retained_bytes(), cache);
	}

	#[test]
	fn whole_folder_selection_prepares_without_changing_visible_checks() {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("folder")).unwrap();
		fs::write(dir.path().join("folder/a.txt"), "a").unwrap();
		fs::write(dir.path().join("folder/b.txt"), "b").unwrap();
		let mut tree = FileTreeNode::new_root(dir.path());
		let key = NodeKey::from_utf8_rel("folder");
		drive(&mut tree, TreeCommand::Expand(key.clone()));
		let proposed = tree.selection_for_all(true);
		assert_eq!(proposed, ["folder", "folder/a.txt", "folder/b.txt"]);
		assert!(tree.selected_paths().is_empty());
		assert!(tree
			.flatten_visible(MAX_VISIBLE_ROWS)
			.iter()
			.all(|row| !row.selected));
		tree.install_selection(proposed);
		assert_eq!(tree.selected_paths().len(), 3);
		let old_bytes = tree.selection_bytes();
		let old_capacity = tree.selected_paths.capacity();
		let all_again = tree.selection_for_all(true);
		assert_eq!(all_again, tree.selected_paths());
		assert!(
			all_again.capacity() <= old_capacity,
			"no-op Select All must not grow storage for duplicate paths"
		);
		assert!(selection_bytes(&all_again) <= old_bytes);
		assert_eq!(tree.selection_bytes(), old_bytes);
		let removed = tree.selection_for_toggle(&key).unwrap();
		assert!(removed.is_empty());
		assert_eq!(
			tree.selected_paths().len(),
			3,
			"preparing removal is also read-only"
		);
	}

	#[test]
	fn test_path_component_boundary_matching() {
		assert!(is_component_child_or_exact("foo", "foo"));
		assert!(is_component_child_or_exact("foo/bar", "foo"));
		assert!(is_component_child_or_exact("foo/bar/baz.txt", "foo"));
		assert!(is_component_child_or_exact("foo/bar/baz.txt", "foo/bar"));
		assert!(!is_component_child_or_exact("foo-bar", "foo"));
		assert!(!is_component_child_or_exact("foo-bar/file.txt", "foo"));
		assert!(!is_component_child_or_exact("foo_extra", "foo"));
	}

	#[test]
	fn test_tree_nested_repo_and_lazy_expansion() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir(root.join("subfolder")).unwrap();
		fs::write(root.join("subfolder").join("file.txt"), "hello").unwrap();
		fs::create_dir(root.join("nested_repo")).unwrap();
		fs::create_dir(root.join("nested_repo").join(".git")).unwrap();

		let mut tree = FileTreeNode::new_root(root);
		assert_eq!(tree.children.len(), 2);
		let nested = tree
			.children
			.iter()
			.find(|c| c.name == "nested_repo")
			.unwrap();
		assert!(nested.is_nested_repo, "should detect nested git repository");
		let sub = tree
			.children
			.iter()
			.find(|c| c.name == "subfolder")
			.unwrap();
		assert!(!sub.is_nested_repo);
		assert!(!sub.is_expanded);

		tree.toggle_expand("subfolder", root);
		assert!(
			tree.children
				.iter()
				.find(|c| c.name == "subfolder")
				.unwrap()
				.is_expanded
		);
		let flattened = tree.flatten_visible(50);
		assert!(flattened.iter().any(|r| r.name == "file.txt"));

		tree.toggle_expand("subfolder", root);
		let sub_collapsed = tree
			.children
			.iter()
			.find(|c| c.name == "subfolder")
			.unwrap();
		assert!(!sub_collapsed.is_expanded);
		assert!(
			sub_collapsed.children.is_empty(),
			"collapse must release children to free memory"
		);
		assert_eq!(
			sub_collapsed.children.capacity(),
			0,
			"collapse must drop backing capacity"
		);
	}

	#[test]
	fn test_tree_folder_selection_and_collect() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir(root.join("folder")).unwrap();
		fs::write(root.join("folder").join("a.txt"), "a").unwrap();
		fs::write(root.join("folder").join("b.txt"), "b").unwrap();
		fs::write(root.join("outside.txt"), "out").unwrap();

		let mut tree = FileTreeNode::new_root(root);
		tree.toggle_expand("folder", root);
		tree.toggle_select("folder");
		// The folder alone: its loaded files are not highlighted with it.
		assert_eq!(selected(&tree), ["folder"]);
		// Expanding it again inherits nothing either.
		tree.toggle_expand("folder", root);
		tree.toggle_expand("folder", root);
		assert_eq!(selected(&tree), ["folder"]);
		tree.toggle_select("folder/a.txt");
		assert_eq!(selected(&tree), ["folder", "folder/a.txt"]);
	}

	#[test]
	fn test_tree_nested_repo_selection_excluded() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir_all(root.join("parent/sub")).unwrap();
		fs::write(root.join("parent/sub/normal.txt"), "data").unwrap();
		fs::create_dir_all(root.join("parent/nested_repo/.git")).unwrap();
		fs::write(root.join("parent/nested_repo/file.txt"), "repo data")
			.unwrap();

		let mut tree = FileTreeNode::new_root(root);
		tree.toggle_expand("parent", root);
		tree.toggle_select("parent");
		assert_eq!(selected(&tree), ["parent"]);
		tree.toggle_select("parent/nested_repo");
		assert_eq!(selected(&tree), ["parent"]);
	}

	/// A nested repo folder is not selectable on its own either: its files
	/// belong to its own repo row.
	#[test]
	fn nested_repo_folder_toggle_selects_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir_all(root.join("sub/.git")).unwrap();
		fs::write(root.join("sub/file.txt"), "repo data").unwrap();
		let mut tree = FileTreeNode::new_root(root);
		tree.toggle_select("sub");
		assert!(selected(&tree).is_empty());
		assert!(tree.selection_for_rels(&["sub".into()]).is_empty());
	}

	#[test]
	#[cfg(unix)]
	fn test_tree_non_utf8_path_safety_and_no_alias() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::write(root.join("invalid-\u{fffd}.txt"), b"valid U+FFFD").unwrap();

		let tree = tree_with_raw_entry(root, b"invalid-\xff.txt");
		let matches: Vec<_> = tree
			.children
			.iter()
			.filter(|n| n.rel_path == "invalid-\u{fffd}.txt")
			.collect();
		assert_eq!(
			matches.len(),
			1,
			"non-UTF8 file must never alias a real valid UTF8 path"
		);
		let invalid = tree
			.children
			.iter()
			.find(|n| !n.is_valid_utf8)
			.expect("invalid node should exist");
		assert!(
			invalid.rel_path.is_empty(),
			"invalid UTF8 must have empty rel_path"
		);
		assert!(
			invalid.read_error.is_some(),
			"invalid UTF8 must have error note"
		);
		assert!(!invalid.selected, "invalid UTF8 cannot be selected");
	}

	#[test]
	#[cfg(target_os = "linux")]
	fn directory_scan_preserves_non_utf8_filename() {
		let dir = tempfile::tempdir().unwrap();
		let name = OsString::from_vec(b"invalid-\xff.txt".to_vec());
		fs::write(dir.path().join(&name), b"invalid").unwrap();
		fs::write(dir.path().join("invalid-\u{fffd}.txt"), b"valid").unwrap();
		let tree = FileTreeNode::new_root(dir.path());
		assert_eq!(tree.children.len(), 2);
		let invalid = tree.children.iter().find(|n| !n.is_valid_utf8).unwrap();
		assert_eq!(invalid.key.relative.as_os_str(), name.as_os_str());
		assert!(invalid.rel_path.is_empty());
		assert!(invalid.read_error.is_some());
		assert!(!invalid.selected);
		assert_eq!(
			tree.children
				.iter()
				.filter(|n| n.rel_path == "invalid-\u{fffd}.txt")
				.count(),
			1
		);
	}

	#[test]
	fn test_tree_retained_bytes_and_budget_enforcement() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		for i in 0..10 {
			let folder = root.join(format!("dir_{i}"));
			fs::create_dir(&folder).unwrap();
			for j in 0..10 {
				fs::write(
					folder.join(format!("file_{j}_{}", "x".repeat(100))),
					"data",
				)
				.unwrap();
			}
		}
		let mut tree = FileTreeNode::new_root(root);
		for i in 0..10 {
			tree.toggle_expand(&format!("dir_{i}"), root);
		}
		assert!(tree.retained_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
	}

	#[test]
	fn root_more_keeps_cursor_selection_and_a_visible_marker() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		for n in 0..130 {
			fs::write(root.join(format!("f-{n:03}.txt")), b"x").unwrap();
		}
		let mut tree = FileTreeNode::new_root(root);
		assert!(tree.has_more);
		assert!(tree.children.len() <= MAX_DIR_ENTRIES);
		let rows = tree.flatten_visible(MAX_VISIBLE_ROWS);
		let more = rows.iter().find(|row| row.is_more_marker).unwrap();
		assert!(more.rel_path.is_empty());
		assert!(matches!(
			command_for_row(more, RowGesture::Primary),
			Some(TreeCommand::LoadMore(_))
		));
		assert!(matches!(
			command_for_row(more, RowGesture::Toggle),
			Some(TreeCommand::LoadMore(_))
		));
		let first = tree.children[0].rel_path.clone();
		let first_page: HashSet<String> = tree
			.children
			.iter()
			.map(|child| child.rel_path.clone())
			.collect();
		tree.toggle_select(&first);
		drive(
			&mut tree,
			command_for_row(more, RowGesture::Primary).unwrap(),
		);
		assert!(
			first_page.iter().all(|path| {
				tree.children.iter().any(|child| child.rel_path == *path)
			}),
			"continuing must not reload the first page"
		);
		assert!(
			tree.children
				.iter()
				.any(|child| !first_page.contains(&child.rel_path)),
			"the next page must append names that were not loaded yet"
		);
		assert!(
			selected(&tree).contains(&first),
			"selection survives the next page"
		);
		assert!(tree.retained_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
		let count = tree.children.len();
		let marker = tree
			.flatten_visible(MAX_VISIBLE_ROWS)
			.into_iter()
			.find(|row| row.is_more_marker);
		if let Some(marker) = marker {
			drive(
				&mut tree,
				command_for_row(&marker, RowGesture::Primary).unwrap(),
			);
		}
		assert!(tree.children.len() >= count);
		assert_eq!(tree.children[0].rel_path, first);
	}

	#[test]
	#[cfg(unix)]
	fn invalid_utf8_is_not_actionable_for_mouse_or_keyboard() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::write(root.join("zzz-valid.txt"), b"valid").unwrap();
		fs::write(root.join("__invalid_utf8_name__"), b"real").unwrap();
		let mut tree = tree_with_raw_entry(root, b"aaa-\xff");
		let rows = tree.flatten_visible(20);
		let invalid = rows
			.iter()
			.find(|row| {
				!row.is_valid_utf8 && !row.is_error && !row.is_more_marker
			})
			.unwrap();
		assert!(command_for_row(invalid, RowGesture::Primary).is_none());
		assert!(command_for_row(invalid, RowGesture::Toggle).is_none());
		assert!(command_for_row(invalid, RowGesture::Expand).is_none());
		let valid = rows
			.iter()
			.find(|row| row.rel_path == "zzz-valid.txt")
			.unwrap();
		drive(
			&mut tree,
			command_for_row(valid, RowGesture::Toggle).unwrap(),
		);
		assert!(selected(&tree).contains(&"zzz-valid.txt".to_string()));
		assert!(!selected(&tree).iter().any(|path| path.is_empty()));
		tree.toggle_select("");
		assert!(!tree.selected);
		assert!(selected(&tree).contains(&"zzz-valid.txt".to_string()));
		let literal = rows
			.iter()
			.find(|row| row.rel_path == "__invalid_utf8_name__")
			.unwrap();
		drive(
			&mut tree,
			command_for_row(literal, RowGesture::Toggle).unwrap(),
		);
		assert!(selected(&tree).contains(&"__invalid_utf8_name__".to_string()));
		assert_eq!(
			tree.children
				.iter()
				.filter(|child| !child.is_valid_utf8)
				.count(),
			1
		);
	}

	#[test]
	fn retry_uses_directory_key_and_does_not_strip_real_names() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir(root.join("broken")).unwrap();
		fs::create_dir_all(root.join("keep/__error__")).unwrap();
		fs::write(root.join("keep/__error__/real.txt"), b"ok").unwrap();
		let mut tree = FileTreeNode::new_root(root);
		fs::remove_dir(root.join("broken")).unwrap();
		tree.toggle_expand("broken", root);
		let rows = tree.flatten_visible(20);
		let error = rows.iter().find(|row| row.is_error).unwrap();
		assert_eq!(error.rel_path, "broken");
		assert!(!error.rel_path.ends_with("__error__"));
		fs::create_dir(root.join("broken")).unwrap();
		fs::write(root.join("broken/repaired.txt"), b"ok").unwrap();
		drive(
			&mut tree,
			command_for_row(error, RowGesture::Primary).unwrap(),
		);
		let broken = tree
			.children
			.iter()
			.find(|child| child.rel_path == "broken")
			.unwrap();
		assert!(broken.read_error.is_none(), "{:?}", broken.read_error);
		assert!(broken
			.children
			.iter()
			.any(|child| child.name == "repaired.txt"));
		tree.toggle_expand("keep", root);
		tree.toggle_expand("keep/__error__", root);
		assert!(tree
			.flatten_visible(30)
			.iter()
			.any(|row| { row.rel_path == "keep/__error__/real.txt" }));
	}

	#[test]
	fn budget_blocks_without_restarting_or_exceeding_the_cap() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		for n in 0..8 {
			let folder = root.join(format!("dir-{n:02}"));
			fs::create_dir(&folder).unwrap();
			for file in 0..60 {
				fs::write(
					folder.join(format!("f-{file:03}-{}", "n".repeat(120))),
					b"x",
				)
				.unwrap();
			}
		}
		let mut tree = FileTreeNode::new_root(root);
		for n in 0..8 {
			tree.toggle_expand(&format!("dir-{n:02}"), root);
			assert!(tree.retained_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
		}
		let first = tree
			.children
			.iter()
			.find(|child| child.is_expanded)
			.map(|child| child.children.first().map(|n| n.rel_path.clone()))
			.unwrap();
		for _ in 0..6 {
			let marker = tree
				.flatten_visible(tree.visible_limit())
				.into_iter()
				.find(|row| row.is_more_marker);
			if let Some(marker) = marker {
				drive(
					&mut tree,
					command_for_row(&marker, RowGesture::Primary).unwrap(),
				);
			}
			assert!(tree.retained_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
		}
		if let Some(path) = first {
			assert!(
				tree.flatten_visible(tree.visible_limit())
					.iter()
					.any(|row| row.rel_path == path),
				"paging must not drop the first admitted name"
			);
		}
	}

	#[test]
	fn deep_root_does_not_loop_or_exceed_the_cap() {
		let dir = tempfile::tempdir().unwrap();
		let mut deep = dir.path().to_path_buf();
		for n in 0..12 {
			deep.push(format!("{n:02}{}", "p".repeat(70)));
			fs::create_dir(&deep).unwrap();
		}
		for n in 0..40 {
			fs::write(deep.join(format!("f-{n:03}.txt")), b"x").unwrap();
		}
		let mut tree = FileTreeNode::new_root(&deep);
		assert!(tree.retained_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
		let first = tree.children.first().map(|child| child.rel_path.clone());
		for _ in 0..5 {
			tree.load_more("", &deep);
			assert!(tree.retained_bytes() <= MAX_RETAINED_WORKING_TREE_BYTES);
		}
		if let Some(path) = first.clone() {
			assert!(
				tree.children.iter().any(|child| child.rel_path == path),
				"paging must keep the first admitted name"
			);
		}
	}

	#[test]
	fn cancel_and_stale_epoch_drop_the_cursor() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir(root.join("sub")).unwrap();
		fs::write(root.join("sub/a.txt"), b"a").unwrap();
		for n in 0..20 {
			fs::write(root.join(format!("f-{n}.txt")), b"x").unwrap();
		}
		let mut tree = FileTreeNode::unloaded_root(root);
		let TreeEffect::Io(io) =
			tree.start(TreeCommand::Expand(NodeKey::root()))
		else {
			panic!("root load must be background work");
		};
		let cancel = CancelToken::new();
		cancel.cancel();
		let result = execute_tree_io(io, &cancel);
		assert!(result.cancelled);
		assert!(tree.apply_io_result(result).is_none());
		assert!(tree.children.is_empty());

		let mut tree = FileTreeNode::new_root(root);
		let TreeEffect::Io(io) =
			tree.start(TreeCommand::Expand(NodeKey::from_utf8_rel("sub")))
		else {
			panic!("expand must schedule io");
		};
		assert!(matches!(
			tree.start(TreeCommand::Collapse(NodeKey::from_utf8_rel("sub"))),
			TreeEffect::Idle
		));
		let result = execute_tree_io(io, &CancelToken::new());
		assert!(tree.apply_io_result(result).is_none());
		let sub = tree
			.children
			.iter()
			.find(|child| child.rel_path == "sub")
			.unwrap();
		assert!(!sub.is_expanded);
		assert!(sub.children.is_empty());
		assert!(sub.scan.is_none());
	}

	#[test]
	fn collapsed_selection_survives_and_view_limit_is_not_a_reread() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir(root.join("folder")).unwrap();
		fs::write(root.join("folder/a.txt"), b"a").unwrap();
		for n in 0..20 {
			fs::write(root.join(format!("g-{n:02}.txt")), b"x").unwrap();
		}
		let mut tree = FileTreeNode::new_root(root);
		tree.toggle_expand("folder", root);
		tree.toggle_select("folder/a.txt");
		tree.toggle_expand("folder", root);
		assert!(selected(&tree).contains(&"folder/a.txt".to_string()));
		let narrow = tree.flatten_visible(3);
		assert!(narrow.len() <= 3);
		assert!(narrow.last().unwrap().is_view_limit);
		assert!(matches!(
			command_for_row(narrow.last().unwrap(), RowGesture::Primary),
			Some(TreeCommand::RevealMoreRows(_))
		));
		let before = tree.children.len();
		drive(
			&mut tree,
			command_for_row(narrow.last().unwrap(), RowGesture::Toggle)
				.unwrap(),
		);
		assert_eq!(tree.children.len(), before);
		assert!(tree.flatten_visible(tree.visible_limit()).len() > 3);
	}
}
