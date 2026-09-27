//! Git log behaviour: paged graph, search, merge collapse, commit / range
//! selection, endpoint compare, HEAD, and a read-only tree of any commit.
//! Reads go through `snip-core` (`browser::history`, `gitsrc`, `graph`).

use std::collections::{HashMap, HashSet, VecDeque};

use crate::arm_cancel;

use crate::graph_view;
use crate::i18n::Msg;
use crate::reader::{fnv1a, Preview, PreviewSource};
use crate::syntax::Language;
use crate::{e2e_on, WorkbenchModel, WorkbenchTab};
use gpui::{Context, ScrollStrategy};
use snip_core::browser::{self, BlobText, CommitSummary, TreeEntry, TreeKind};
use snip_core::gitrun::RunOptions;
use snip_core::gitsrc::{self, Git, GitSource};
use snip_core::graph::{GraphCheckpoint, GraphLayout, MAX_SHA_LEN};

/// Files listed for one commit or compare; more are counted, not kept.
pub const MAX_COMMIT_FILES: usize = 5_000;
/// Rows shown in the commit tree at once.
pub const MAX_REV_ROWS: usize = 2_000;
/// Retained directories budget for RevTree browsing memory.
pub const MAX_CACHED_DIRS: usize = 50;
/// Retained entries budget for RevTree browsing memory.
pub const MAX_CACHED_ENTRIES: usize = 10_000;
/// Hard cap for the commit-tree cache. Stricter than the 8MiB tree budget so
/// this browser cannot consume the whole allowance. Nothing stored here may
/// exceed it, including one oversized listing.
pub const MAX_RETAINED_TREE_BYTES: usize = 256 * 1024;
/// Long Git errors are clipped before they are retained.
const MAX_STORED_ERROR_BYTES: usize = 240;
pub const MAX_RETAINED_GRAPH_BYTES: usize = 16 * 1024 * 1024;

// Metadata can change between page admissions. Selection/range/compare OIDs
// are length-checked and copied into compact strings. TextInput separately
// retains the typed query as Arc<str> (no spare capacity), bounded by its
// existing Unicode-character limit; its placeholder is a static translation.
// Both history_error and the status Msg retain the bounded error. Two Msg
// argument slots also cover the success status's count/page strings.
// Framework glyph/layout caches, allocator overhead and temporary candidate
// copies are outside this application-data accounting. Ref-selector copies
// belong to the separate selector/tree tier, not this history model.
const GRAPH_METADATA_RESERVE: usize = 4
	* (MAX_SHA_LEN + std::mem::size_of::<Option<String>>())
	+ 4 * crate::text_input::TextInput::MAX_TOTAL_CHARS
	+ std::mem::size_of::<crate::text_input::TextInput>()
	+ 2 * std::mem::size_of::<usize>() // Arc<str> reference counts
	+ 2 * MAX_STORED_ERROR_BYTES
	+ std::mem::size_of::<Option<String>>()
	+ std::mem::size_of::<Msg>()
	+ 2 * std::mem::size_of::<String>();

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSearch {
	pub query: String,
	pub author: bool,
}

/// Lazily listed tree of one commit (no checkout).
pub struct RevTree {
	pub sha: String,
	pub dirs: HashMap<String, (Vec<TreeEntry>, bool)>,
	/// Oldest directory key at the front. This is the eviction order.
	order: VecDeque<String>,
	pub expanded: HashSet<String>,
	/// Oldest expanded key at the front. Not hash-map iteration order.
	expanded_order: VecDeque<String>,
	pub errors: HashMap<String, String>,
	/// Oldest error key at the front.
	error_order: VecDeque<String>,
}

pub struct RevRow {
	pub path: String,
	pub name: String,
	pub depth: usize,
	pub kind: TreeKind,
	pub expanded: bool,
	/// Marker rows: `Some(msg)` for loading / truncated / error lines.
	pub marker: Option<Msg>,
}

fn string_cost(value: &str) -> usize {
	std::mem::size_of::<String>().saturating_add(value.len())
}

/// Heap capacity, not length. A `String` can own more bytes than it shows.
fn stored_string_cost(value: &String) -> usize {
	std::mem::size_of::<String>().saturating_add(value.capacity())
}

fn deque_slot_cost(queue: &VecDeque<String>) -> usize {
	queue
		.capacity()
		.saturating_mul(std::mem::size_of::<String>())
}

fn listing_cost(dir: &str, entries: &[TreeEntry]) -> usize {
	let mut total = string_cost(dir).saturating_mul(2).saturating_add(128);
	total = total.saturating_add(
		entries
			.len()
			.saturating_mul(std::mem::size_of::<TreeEntry>()),
	);
	for entry in entries {
		total = total.saturating_add(stored_string_cost(&entry.path));
		total = total.saturating_add(stored_string_cost(&entry.name));
	}
	total
}

impl RevTree {
	pub fn new(sha: String) -> Self {
		let mut expanded = HashSet::new();
		expanded.insert(String::new());
		Self {
			sha,
			dirs: HashMap::new(),
			order: VecDeque::new(),
			expanded,
			expanded_order: VecDeque::new(),
			errors: HashMap::new(),
			error_order: VecDeque::new(),
		}
	}

	pub fn retained_bytes(&self) -> usize {
		const SLOT: usize = 64;
		let mut total = std::mem::size_of::<Self>();
		total = total.saturating_add(stored_string_cost(&self.sha));
		total = total.saturating_add(self.dirs.capacity().saturating_mul(
			std::mem::size_of::<String>()
				+ std::mem::size_of::<(Vec<TreeEntry>, bool)>()
				+ SLOT,
		));
		total = total.saturating_add(deque_slot_cost(&self.order));
		total = total.saturating_add(deque_slot_cost(&self.expanded_order));
		total = total.saturating_add(deque_slot_cost(&self.error_order));
		total = total.saturating_add(
			self.expanded
				.capacity()
				.saturating_mul(std::mem::size_of::<String>() + SLOT),
		);
		total = total.saturating_add(
			self.errors
				.capacity()
				.saturating_mul(std::mem::size_of::<String>() * 2 + SLOT),
		);
		for (key, (entries, _)) in &self.dirs {
			total = total.saturating_add(stored_string_cost(key));
			total = total.saturating_add(
				entries
					.capacity()
					.saturating_mul(std::mem::size_of::<TreeEntry>()),
			);
			for entry in entries {
				total = total.saturating_add(stored_string_cost(&entry.path));
				total = total.saturating_add(stored_string_cost(&entry.name));
			}
		}
		for key in self
			.order
			.iter()
			.chain(&self.expanded_order)
			.chain(&self.error_order)
		{
			total = total.saturating_add(stored_string_cost(key));
		}
		for key in &self.expanded {
			total = total.saturating_add(stored_string_cost(key));
		}
		for (key, err) in &self.errors {
			total = total.saturating_add(stored_string_cost(key));
			total = total.saturating_add(stored_string_cost(err));
		}
		total
	}

	fn entry_count(&self) -> usize {
		self.dirs.values().map(|(entries, _)| entries.len()).sum()
	}

	fn touch(&mut self, dir: &str) {
		if let Some(pos) = self.order.iter().position(|key| key == dir) {
			self.order.remove(pos);
		}
		self.order.push_back(dir.to_string());
	}

	fn evict_oldest(
		&mut self,
		protect: Option<&str>,
		allow_root: bool,
	) -> bool {
		let Some(pos) = self.order.iter().position(|key| {
			Some(key.as_str()) != protect && (allow_root || !key.is_empty())
		}) else {
			return false;
		};
		let Some(key) = self.order.remove(pos) else {
			return false;
		};
		self.dirs.remove(&key);
		self.expanded.remove(&key);
		self.expanded_order.retain(|item| item != &key);
		self.errors.remove(&key);
		self.error_order.retain(|item| item != &key);
		true
	}

	fn shrink(&mut self) {
		self.dirs.shrink_to_fit();
		self.order.shrink_to_fit();
		self.expanded.shrink_to_fit();
		self.expanded_order.shrink_to_fit();
		self.errors.shrink_to_fit();
		self.error_order.shrink_to_fit();
		for (entries, _) in self.dirs.values_mut() {
			entries.shrink_to_fit();
		}
	}

	/// Keys inserted without going through the ordered API join the back in
	/// sorted order, so a later eviction still has one stable sequence.
	fn sync_order(order: &mut VecDeque<String>, live: &[String]) {
		let have: HashSet<&str> = live.iter().map(String::as_str).collect();
		order.retain(|key| have.contains(key.as_str()));
		let mut missing: Vec<String> = live
			.iter()
			.filter(|key| !order.iter().any(|have| have == *key))
			.cloned()
			.collect();
		missing.sort();
		order.extend(missing);
	}

	fn evict_error_oldest(&mut self) -> bool {
		let Some(key) = self.error_order.pop_front() else {
			return false;
		};
		self.errors.remove(&key).is_some()
	}

	fn within_budget(&self) -> bool {
		self.retained_bytes() <= MAX_RETAINED_TREE_BYTES
			&& self.dirs.len() <= MAX_CACHED_DIRS
			&& self.entry_count() <= MAX_CACHED_ENTRIES
	}

	/// Drops oldest listings, then oldest errors and expanded keys, until the
	/// hard cap holds. Root listings go only after every non-root listing.
	pub fn enforce_budget(&mut self) {
		loop {
			let dir_keys: Vec<String> = self.dirs.keys().cloned().collect();
			let expanded_keys: Vec<String> =
				self.expanded.iter().cloned().collect();
			let error_keys: Vec<String> = self.errors.keys().cloned().collect();
			Self::sync_order(&mut self.order, &dir_keys);
			Self::sync_order(&mut self.expanded_order, &expanded_keys);
			Self::sync_order(&mut self.error_order, &error_keys);
			self.shrink();
			if self.within_budget() {
				return;
			}
			if self.evict_oldest(None, false) {
				continue;
			}
			if self.evict_error_oldest() {
				continue;
			}
			if let Some(pos) =
				self.expanded_order.iter().position(|key| !key.is_empty())
			{
				let key = self.expanded_order.remove(pos).unwrap();
				self.expanded.remove(&key);
				continue;
			}
			let oldest = self.order.front().cloned();
			if let Some(key) = oldest {
				if let Some((entries, truncated)) = self.dirs.get_mut(&key) {
					if !entries.is_empty() {
						let to_pop = (entries.len() / 20).max(1);
						for _ in 0..to_pop {
							entries.pop();
						}
						*truncated = true;
						entries.shrink_to_fit();
						continue;
					}
				}
			}
			if self.dirs.len() > 1 && self.evict_oldest(None, false) {
				continue;
			}
			if self.dirs.contains_key("") {
				if let Some((entries, _)) = self.dirs.get("") {
					if entries.is_empty() {
						self.dirs.clear();
						self.order.clear();
						self.note_error(
							"".into(),
							"listing exceeds the tree budget".into(),
						);
						return;
					}
				}
			}
			self.dirs.clear();
			self.order.clear();
			self.errors.clear();
			self.error_order.clear();
			self.expanded.retain(|key| key.is_empty());
			self.expanded_order.retain(|key| key.is_empty());
			self.shrink();
			return;
		}
	}

	pub fn note_error(&mut self, dir: String, err: String) {
		if string_cost(&dir) > MAX_RETAINED_TREE_BYTES {
			return;
		}
		let err = if err.len() > MAX_STORED_ERROR_BYTES {
			let mut clipped: String = err.chars().take(80).collect();
			clipped.push('…');
			clipped
		} else {
			err
		};
		self.error_order.retain(|key| key != &dir);
		self.error_order.push_back(dir.clone());
		self.errors.insert(dir, err);
		self.enforce_budget();
	}

	/// Records an expanded directory. The key joins the back of the eviction
	/// order; collapsing or the byte cap drops the oldest keys first.
	pub fn note_expanded(&mut self, path: &str) {
		self.expanded.insert(path.to_string());
		self.expanded_order.retain(|key| key != path);
		self.expanded_order.push_back(path.to_string());
		self.enforce_budget();
	}

	/// Drops a collapsed directory, its children, and the bytes those keys own.
	pub fn release_dir(&mut self, path: &str) {
		let nested = format!("{path}/");
		let drop_dir = |key: &str| {
			key == path || (!path.is_empty() && key.starts_with(&nested))
		};
		self.dirs.retain(|key, _| !drop_dir(key));
		self.order.retain(|key| !drop_dir(key));
		self.errors.retain(|key, _| !drop_dir(key));
		self.error_order.retain(|key| !drop_dir(key));
		if path.is_empty() {
			self.expanded.remove("");
		} else {
			self.expanded
				.retain(|key| key != path && !key.starts_with(&nested));
		}
		self.expanded_order
			.retain(|key| self.expanded.contains(key));
		self.enforce_budget();
	}

	/// Inserts a listing that fits the hard cap. An oversized listing is
	/// truncated or refused before any of its excess bytes are retained.
	pub fn insert_dir(
		&mut self,
		dir: String,
		mut entries: Vec<TreeEntry>,
		mut truncated: bool,
	) {
		for e in &mut entries {
			e.path.shrink_to_fit();
			e.name.shrink_to_fit();
		}
		entries.shrink_to_fit();

		// The directory key itself must fit. Refusing it does not replace the
		// root listing with an error, and the huge key is not stored.
		if listing_cost(&dir, &[]) > MAX_RETAINED_TREE_BYTES {
			return;
		}
		while listing_cost(&dir, &entries) > MAX_RETAINED_TREE_BYTES
			&& !entries.is_empty()
		{
			let last_cost = entries
				.last()
				.map(|e| {
					std::mem::size_of::<TreeEntry>()
						+ stored_string_cost(&e.path)
						+ stored_string_cost(&e.name)
				})
				.unwrap_or(1);
			let excess = listing_cost(&dir, &entries) - MAX_RETAINED_TREE_BYTES;
			let to_pop = (excess / last_cost).max(1);
			for _ in 0..to_pop.min(entries.len()) {
				entries.pop();
			}
			truncated = true;
			entries.shrink_to_fit();
		}
		self.dirs.remove(&dir);
		self.order.retain(|key| key != &dir);
		self.enforce_budget();
		while self
			.retained_bytes()
			.saturating_add(listing_cost(&dir, &entries))
			> MAX_RETAINED_TREE_BYTES
			|| self.dirs.len() >= MAX_CACHED_DIRS
			|| self.entry_count().saturating_add(entries.len())
				> MAX_CACHED_ENTRIES
		{
			if !self.evict_oldest(Some(dir.as_str()), false) {
				while !entries.is_empty() {
					let last_cost = entries
						.last()
						.map(|e| {
							std::mem::size_of::<TreeEntry>()
								+ stored_string_cost(&e.path)
								+ stored_string_cost(&e.name)
						})
						.unwrap_or(1);
					let excess = self
						.retained_bytes()
						.saturating_add(listing_cost(&dir, &entries))
						.saturating_sub(MAX_RETAINED_TREE_BYTES);
					let to_pop = (excess / last_cost).max(1);
					for _ in 0..to_pop.min(entries.len()) {
						entries.pop();
					}
					truncated = true;
					entries.shrink_to_fit();
					if self
						.retained_bytes()
						.saturating_add(listing_cost(&dir, &entries))
						<= MAX_RETAINED_TREE_BYTES
					{
						break;
					}
				}
				break;
			}
		}
		if entries.is_empty()
			&& listing_cost(&dir, &entries) > MAX_RETAINED_TREE_BYTES
		{
			self.note_error(dir, "listing exceeds the tree budget".into());
			return;
		}
		self.dirs.insert(dir.clone(), (entries, truncated));
		self.touch(&dir);
		self.enforce_budget();
	}

	pub fn rows(&self) -> Vec<RevRow> {
		let mut out = Vec::new();
		self.push_dir("", 1, &mut out);
		out
	}

	fn push_dir(&self, dir: &str, depth: usize, out: &mut Vec<RevRow>) {
		if out.len() >= MAX_REV_ROWS {
			return;
		}
		if let Some(err) = self.errors.get(dir) {
			out.push(marker(depth, Msg::new("error_tree", [err.clone()])));
			return;
		}
		let Some((entries, truncated)) = self.dirs.get(dir) else {
			out.push(marker(depth, Msg::new("status_loading", [])));
			return;
		};
		for e in entries {
			if out.len() >= MAX_REV_ROWS {
				out.push(marker(
					depth,
					Msg::new("tree_rows_capped", [MAX_REV_ROWS.to_string()]),
				));
				return;
			}
			let expanded =
				e.kind == TreeKind::Tree && self.expanded.contains(&e.path);
			out.push(RevRow {
				path: e.path.clone(),
				name: e.name.clone(),
				depth,
				kind: e.kind.clone(),
				expanded,
				marker: None,
			});
			if expanded {
				self.push_dir(&e.path, depth + 1, out);
			}
		}
		if *truncated {
			out.push(marker(
				depth,
				Msg::new("tree_dir_truncated", [entries.len().to_string()]),
			));
		}
	}
}

fn marker(depth: usize, msg: Msg) -> RevRow {
	RevRow {
		path: String::new(),
		name: String::new(),
		depth,
		kind: TreeKind::Blob,
		expanded: false,
		marker: Some(msg),
	}
}

/// Commits reachable from the merge's non-first parents but not from its
/// first parent, limited to the loaded page. Hiding exactly these keeps
/// every remaining edge truthful; the merge's side edge becomes a filtered
/// gap instead of pointing at an unrelated commit.
pub fn side_only(commits: &[CommitSummary], merge: &str) -> HashSet<String> {
	let by_sha: HashMap<&str, &CommitSummary> =
		commits.iter().map(|c| (c.sha.as_str(), c)).collect();
	let Some(m) = by_sha.get(merge) else {
		return HashSet::new();
	};
	if m.parents.len() < 2 {
		return HashSet::new();
	}
	let reach = |starts: &[String], stop: &HashSet<String>| {
		let mut seen = HashSet::new();
		let mut q: VecDeque<&str> = starts.iter().map(String::as_str).collect();
		while let Some(s) = q.pop_front() {
			if stop.contains(s) || !seen.insert(s.to_string()) {
				continue;
			}
			if let Some(c) = by_sha.get(s) {
				q.extend(c.parents.iter().map(String::as_str));
			}
		}
		seen
	};
	let main = reach(&m.parents[..1], &HashSet::new());
	reach(&m.parents[1..], &main)
		.into_iter()
		.filter(|s| by_sha.contains_key(s.as_str()))
		.collect()
}

/// A complete replacement page. Construction only borrows the current cache;
/// failure cannot attach old rails/checkpoints to new commits or move the page.
struct PreparedHistory {
	commits: Vec<CommitSummary>,
	refs: Vec<browser::GitReference>,
	head_sha: Option<String>,
	graph_layout: Option<GraphLayout>,
	page_checkpoints: Vec<Option<GraphCheckpoint>>,
	// ponytail: linear membership, bounded by 16 MiB; add a countable index only
	// if profiling shows collapse lookup matters. No opaque hash-table capacity.
	collapsed_merges: Vec<String>,
	hidden_commits: Vec<String>,
	active_ref_filter: Option<String>,
	log_search: Option<LogSearch>,
	commit_page: usize,
	history_has_more: bool,
}

#[derive(Debug)]
enum GraphAdmissionError {
	Budget,
	Layout(String),
}

impl PreparedHistory {
	fn prepare(
		history: browser::RepositoryHistory,
		page: usize,
		checkpoints: &[Option<GraphCheckpoint>],
		collapsed_merges: Vec<String>,
		active_ref_filter: Option<String>,
		log_search: Option<LogSearch>,
	) -> Result<Self, GraphAdmissionError> {
		let mut candidate = Self {
			commits: history.commits,
			refs: history.refs,
			head_sha: history.head,
			graph_layout: None,
			page_checkpoints: checkpoints.to_vec(),
			collapsed_merges,
			hidden_commits: Vec::new(),
			active_ref_filter,
			log_search,
			commit_page: page,
			history_has_more: history.has_more,
		};
		candidate.check_budget()?;
		if candidate.commits.iter().any(|commit| {
			commit.sha.len() > MAX_SHA_LEN
				|| commit.parents.iter().any(|sha| sha.len() > MAX_SHA_LEN)
		}) || candidate
			.head_sha
			.as_ref()
			.is_some_and(|sha| sha.len() > MAX_SHA_LEN)
			|| candidate
				.refs
				.iter()
				.any(|reference| reference.sha.len() > MAX_SHA_LEN)
		{
			return Err(GraphAdmissionError::Layout(
				"Commit ID exceeds graph limit".into(),
			));
		}
		if candidate.log_search.is_none() {
			let checkpoint = checkpoints.get(page).and_then(Option::as_ref);
			if page > 0 && checkpoint.is_none() {
				return Err(GraphAdmissionError::Layout(
					"Missing graph page checkpoint".into(),
				));
			}
			let full = graph_view::layout_commits_paged(
				&candidate.commits,
				&candidate.refs,
				candidate.head_sha.as_deref(),
				checkpoint,
			)
			.map_err(GraphAdmissionError::Layout)?;
			if let Some(next) = &full.checkpoint {
				let slots =
					page.checked_add(2).ok_or(GraphAdmissionError::Budget)?;
				if slots.saturating_mul(std::mem::size_of::<
					Option<GraphCheckpoint>,
				>()) > MAX_RETAINED_GRAPH_BYTES
				{
					return Err(GraphAdmissionError::Budget);
				}
				if candidate.page_checkpoints.len() < slots {
					candidate.page_checkpoints.resize(slots, None);
				}
				candidate.page_checkpoints[page + 1] = Some(next.clone());
			}
			let hidden: HashSet<String> = candidate
				.collapsed_merges
				.iter()
				.flat_map(|merge| side_only(&candidate.commits, merge))
				.collect();
			candidate.graph_layout = Some(if hidden.is_empty() {
				full
			} else {
				let shown: Vec<CommitSummary> = candidate
					.commits
					.iter()
					.filter(|commit| !hidden.contains(&commit.sha))
					.cloned()
					.collect();
				graph_view::layout_commits_filtered(
					&shown,
					&candidate.refs,
					candidate.head_sha.as_deref(),
					checkpoint,
					hidden.clone(),
				)
				.map_err(GraphAdmissionError::Layout)?
			});
			candidate.hidden_commits = hidden.into_iter().collect();
			candidate.hidden_commits.sort();
		}
		candidate.check_budget()?;
		Ok(candidate)
	}

	fn retained_bytes(&self) -> usize {
		use graph_view::vec_bytes;
		let mut bytes = std::mem::size_of::<Self>()
			.saturating_add(GRAPH_METADATA_RESERVE)
			.saturating_add(vec_bytes(&self.commits))
			.saturating_add(vec_bytes(&self.refs))
			.saturating_add(vec_bytes(&self.page_checkpoints))
			.saturating_add(vec_bytes(&self.collapsed_merges))
			.saturating_add(vec_bytes(&self.hidden_commits));
		for commit in &self.commits {
			bytes = bytes
				.saturating_add(commit.sha.capacity())
				.saturating_add(commit.author_name.capacity())
				.saturating_add(commit.author_email.capacity())
				.saturating_add(commit.author_date.capacity())
				.saturating_add(commit.subject.capacity())
				.saturating_add(vec_bytes(&commit.parents));
			for parent in &commit.parents {
				bytes = bytes.saturating_add(parent.capacity());
			}
		}
		for reference in &self.refs {
			bytes = bytes
				.saturating_add(reference.name.capacity())
				.saturating_add(reference.sha.capacity());
		}
		for value in self
			.head_sha
			.iter()
			.chain(self.active_ref_filter.iter())
			.chain(self.log_search.iter().map(|search| &search.query))
			.chain(self.collapsed_merges.iter())
			.chain(self.hidden_commits.iter())
		{
			bytes = bytes.saturating_add(value.capacity());
		}
		for checkpoint in self.page_checkpoints.iter().flatten() {
			bytes = bytes
				.saturating_add(graph_view::checkpoint_heap_bytes(checkpoint));
		}
		if let Some(layout) = &self.graph_layout {
			bytes = bytes.saturating_add(graph_view::layout_heap_bytes(layout));
		}
		bytes
	}

	fn check_budget(&self) -> Result<(), GraphAdmissionError> {
		if self.retained_bytes() > MAX_RETAINED_GRAPH_BYTES {
			Err(GraphAdmissionError::Budget)
		} else {
			Ok(())
		}
	}

	fn install(self, model: &mut WorkbenchModel) {
		model.commits = self.commits;
		model.refs = self.refs;
		model.head_sha = self.head_sha;
		model.graph_layout = self.graph_layout;
		model.page_checkpoints = self.page_checkpoints;
		model.collapsed_merges = self.collapsed_merges;
		model.hidden_commits = self.hidden_commits;
		model.active_ref_filter = self.active_ref_filter;
		model.log_search = self.log_search;
		model.commit_page = self.commit_page;
		model.history_has_more = self.history_has_more;
		model.history_error = None;
	}
}

impl WorkbenchModel {
	/// Commits visible in the log, in display order.
	pub fn display_commits(&self) -> Vec<&CommitSummary> {
		self.commits
			.iter()
			.filter(|c| !self.hidden_commits.contains(&c.sha))
			.collect()
	}

	pub fn load_history(&mut self, cx: &mut Context<Self>) {
		self.load_history_page(self.commit_page, cx);
	}

	fn load_history_page(&mut self, page: usize, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		let Some(repo_root) = self.repo_root() else {
			return;
		};
		let ref_filter = self.active_ref_filter.clone();
		let search = self.log_search.clone();
		let page_size = self.history_page_size;
		let Some(skip) = page.checked_mul(page_size) else {
			self.report_graph_error(GraphAdmissionError::Budget);
			cx.notify();
			return;
		};

		self.history_generation += 1;
		let task_generation = self.history_generation;
		let cancel = arm_cancel(&mut self.history_cancel);
		self.history_error = None;

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();

		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let res = bg
					.spawn(async move {
						let opts = crate::interactive_read_opts(cancel_bg);
						let git = Git::open_with(&repo_root, &opts)
							.map_err(|e| e.to_string())?;
						match &search {
							Some(LogSearch {
								query,
								author: true,
							}) => {
								let refs = browser::history_with(
									&git, None, "", 0, 0, &opts,
								)
								.map_err(|e| e.to_string())?;
								let (commits, more) =
									browser::history_by_author_with(
										&git, query, skip, page_size, &opts,
									)
									.map_err(|e| e.to_string())?;
								Ok(browser::RepositoryHistory {
									root: refs.root,
									commits,
									refs: refs.refs,
									head: refs.head,
									has_more: more,
								})
							}
							_ => browser::history_with(
								&git,
								ref_filter.as_deref(),
								search
									.as_ref()
									.map(|s| s.query.as_str())
									.unwrap_or(""),
								skip,
								page_size,
								&opts,
							)
							.map_err(|e| e.to_string()),
						}
					})
					.await;

				let _ =
					this.update(&mut async_app, |model, cx| {
						if model.history_generation != task_generation {
							return;
						}
						match res {
							Ok(hist) => {
								match PreparedHistory::prepare(hist, page, &model.page_checkpoints,
                                    model.collapsed_merges.clone(), model.active_ref_filter.clone(), model.log_search.clone()) {
                                    Ok(candidate) => {
                                        app_log!("[APP:GRAPH_RETAINED: bytes={} limit={}]", candidate.retained_bytes(), MAX_RETAINED_GRAPH_BYTES);
                                        candidate.install(model);
                                    }
                                    Err(error) => {
                                        model.report_graph_error(error);
                                        cx.notify();
                                        return;
                                    }
                                }
								model.set_status(
									"status_history_loaded",
									[
										model.commits.len().to_string(),
										(page + 1).to_string(),
									],
								);
								app_log!(
									"[APP:GRAPH_LOADED: commits={}]",
									model.commits.len()
								);
								app_log!(
								"[APP:E2E_LOG: mode={} n={} first={} page={}]",
								if model.log_search.is_some() { "search" } else { "graph" },
								model.commits.len(),
								model.commits.first().map(|c| &c.sha[..7]).unwrap_or("-"),
								page + 1
							);
								if let Some(anchor) =
									model.selected_commit.clone()
								{
									if model
										.commits
										.iter()
										.any(|c| c.sha == anchor)
									{
										model.select_commit(&anchor, cx);
									} else if model.select_head_after_load {
										model.select_head_after_load = false;
										model.focus_head(cx);
									} else {
										model.selected_commit = None;
									}
								} else if model.select_head_after_load {
									model.select_head_after_load = false;
									model.focus_head(cx);
								}
							}
							Err(e) => {
								model.report_graph_error(GraphAdmissionError::Layout(e));
							}
						}
						cx.notify();
					});
			},
		);
	}

	fn report_graph_error(&mut self, error: GraphAdmissionError) {
		let mut message = match error {
			GraphAdmissionError::Budget => {
				crate::i18n::t("error_graph_budget", self.locale).to_string()
			}
			GraphAdmissionError::Layout(message) => message,
		};
		let mut end = message.len().min(MAX_STORED_ERROR_BYTES);
		while !message.is_char_boundary(end) {
			end -= 1;
		}
		message.truncate(end);
		// Boxed str has no spare capacity; errors cannot consume page headroom.
		let message = message.into_boxed_str().into_string();
		self.history_error = Some(message.clone());
		self.set_status("error_history", [message]);
		app_log!("[APP:HISTORY_ERROR]");
	}

	pub fn toggle_collapse(&mut self, merge: String, cx: &mut Context<Self>) {
		let mut collapsed_merges = self.collapsed_merges.clone();
		let collapsed = if let Some(index) =
			collapsed_merges.iter().position(|sha| sha == &merge)
		{
			collapsed_merges.remove(index);
			false
		} else {
			collapsed_merges.push(merge.clone());
			true
		};
		let history = browser::RepositoryHistory {
			root: String::new(),
			commits: self.commits.clone(),
			refs: self.refs.clone(),
			head: self.head_sha.clone(),
			has_more: self.history_has_more,
		};
		match PreparedHistory::prepare(
			history,
			self.commit_page,
			&self.page_checkpoints,
			collapsed_merges,
			self.active_ref_filter.clone(),
			self.log_search.clone(),
		) {
			Ok(candidate) => {
				candidate.install(self);
				app_log!("[APP:MERGE_COLLAPSE: sha={} collapsed={} hidden={} shown={}]",
                    &merge[..7.min(merge.len())], collapsed, side_only(&self.commits, &merge).len(), self.display_commits().len());
			}
			Err(error) => self.report_graph_error(error),
		}
		cx.notify();
	}

	pub fn history_next_page(&mut self, cx: &mut Context<Self>) {
		if self.history_has_more {
			self.load_history_page(self.commit_page.saturating_add(1), cx);
		}
	}

	pub fn history_prev_page(&mut self, cx: &mut Context<Self>) {
		if self.commit_page > 0 {
			self.load_history_page(self.commit_page - 1, cx);
		}
	}

	/// Changing query identity clears the old graph before the async read.
	/// Oversized metadata is refused without changing the previous query/page.
	fn reset_history_query(
		&mut self,
		reference: Option<String>,
		search: Option<LogSearch>,
	) -> bool {
		let empty = browser::RepositoryHistory {
			root: String::new(),
			commits: Vec::new(),
			refs: Vec::new(),
			head: None,
			has_more: false,
		};
		match PreparedHistory::prepare(
			empty,
			0,
			&[],
			Vec::new(),
			reference,
			search,
		) {
			Ok(candidate) => {
				candidate.install(self);
				true
			}
			Err(error) => {
				self.report_graph_error(error);
				false
			}
		}
	}

	pub fn filter_by_ref(
		&mut self,
		ref_name: Option<String>,
		cx: &mut Context<Self>,
	) {
		if !self.reset_history_query(ref_name, None) {
			cx.notify();
			return;
		}
		app_log!(
			"[APP:REF_FILTER: {}]",
			self.active_ref_filter.as_deref().unwrap_or("all")
		);
		self.load_history(cx);
	}

	pub fn start_log_search(&mut self, query: String, cx: &mut Context<Self>) {
		let search = (!query.is_empty()).then_some(LogSearch {
			query,
			author: self.search_by_author,
		});
		if !self.reset_history_query(self.active_ref_filter.clone(), search) {
			cx.notify();
			return;
		}
		app_log!(
			"[APP:LOG_SEARCH: active={} author={}]",
			self.log_search.is_some(),
			self.search_by_author
		);
		self.load_history(cx);
	}

	/// Shows HEAD: back to the full graph on page 1, then selects HEAD.
	pub fn locate_head(&mut self, cx: &mut Context<Self>) {
		let on_page = self.log_search.is_none()
			&& self.active_ref_filter.is_none()
			&& self.head_sha.as_ref().is_some_and(|h| {
				self.display_commits().iter().any(|c| &c.sha == h)
			});
		if on_page {
			self.focus_head(cx);
			return;
		}
		if !self.reset_history_query(None, None) {
			cx.notify();
			return;
		}
		self.select_head_after_load = true;
		self.load_history(cx);
	}

	fn focus_head(&mut self, cx: &mut Context<Self>) {
		let Some(head) = self.head_sha.clone() else {
			return;
		};
		match self.display_commits().iter().position(|c| c.sha == head) {
			Some(ix) => {
				self.log_scroll.scroll_to_item(ix, ScrollStrategy::Center);
				app_log!("[APP:HEAD_LOCATED: row={}]", ix);
				self.select_commit(&head, cx);
			}
			None => {
				// HEAD is further down in topo order: show its own history.
				app_log!("[APP:HEAD_VIA_FILTER]");
				self.filter_by_ref(Some("HEAD".into()), cx);
				self.select_head_after_load = true;
			}
		}
	}

	pub fn select_commit(&mut self, sha: &str, cx: &mut Context<Self>) {
		if sha.len() > MAX_SHA_LEN {
			self.report_graph_error(GraphAdmissionError::Budget);
			cx.notify();
			return;
		}
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.selected_commit = Some(Box::<str>::from(sha).into_string());
		self.range_head = None;
		self.compare = None;
		self.selected_file = None;
		self.selected_commit_file = None;
		self.commit_files.clear();
		self.clear_preview();
		self.preview_loading = true;
		self.preview_error = None;
		let Some(root) = self.repo_root() else {
			return;
		};
		let sha = sha.to_string();
		app_log!("[APP:COMMIT_SELECTED: {}]", &sha[..7.min(sha.len())]);
		self.load_change_list(
			root,
			GitSource::Commit(sha.clone()),
			PreviewSource::CommitDiff { sha },
			task_generation,
			cx,
		);
	}

	/// Shift-selection: `selected_commit` stays the anchor.
	pub fn extend_range(&mut self, sha: &str, cx: &mut Context<Self>) {
		if sha.len() > MAX_SHA_LEN {
			self.report_graph_error(GraphAdmissionError::Budget);
			cx.notify();
			return;
		}
		if self.selected_commit.is_none() {
			self.select_commit(sha, cx);
			return;
		}
		self.range_head = (self.selected_commit.as_deref() != Some(sha))
			.then(|| Box::<str>::from(sha).into_string());
		let n = self.range_rows().map(|(a, b)| b - a + 1).unwrap_or(1);
		app_log!("[APP:RANGE: commits={}]", n);
		cx.notify();
	}

	/// Display rows spanned by the selected range, (top, bottom).
	pub fn range_rows(&self) -> Option<(usize, usize)> {
		let rows = self.display_commits();
		let a = rows
			.iter()
			.position(|c| Some(&c.sha) == self.selected_commit.as_ref())?;
		let b = rows
			.iter()
			.position(|c| Some(&c.sha) == self.range_head.as_ref())?;
		Some((a.min(b), a.max(b)))
	}

	/// Endpoint diff between the two ends of the range (older → newer).
	pub fn compare_range(&mut self, cx: &mut Context<Self>) {
		let Some((top, bottom)) = self.range_rows() else {
			return;
		};
		let rows = self.display_commits();
		let (newer, older) = (
			Box::<str>::from(rows[top].sha.as_str()).into_string(),
			Box::<str>::from(rows[bottom].sha.as_str()).into_string(),
		);
		drop(rows);
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.compare = Some((older.clone(), newer.clone()));
		self.selected_commit_file = None;
		self.commit_files.clear();
		self.clear_preview();
		self.preview_loading = true;
		self.preview_error = None;
		let Some(root) = self.repo_root() else {
			return;
		};
		app_log!("[APP:COMPARE: from={} to={}]", &older[..7], &newer[..7]);
		self.load_change_list(
			root,
			GitSource::Range(older.clone(), newer.clone()),
			PreviewSource::Compare {
				from: older,
				to: newer,
			},
			task_generation,
			cx,
		);
	}

	/// Lists changed files of a commit/compare and previews the first.
	fn load_change_list(
		&mut self,
		root: std::path::PathBuf,
		source: GitSource,
		preview_source: PreviewSource,
		task_generation: u64,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.preview_cancel);
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel.clone()),
			async move {
				let res = bg
					.spawn(async move {
						let opts = RunOptions {
							cancel: Some(cancel),
							max_stdout: browser::PREVIEW_LIMIT,
							overflow: snip_core::gitrun::Overflow::Error,
							..RunOptions::preview(None)
						};
						let git = Git::open_with(&root, &opts)
							.map_err(|e| e.to_string())?;
						let files = gitsrc::list_changed_paths_with(
							&git, &source, &opts,
						)
						.map_err(|e| e.to_string())?;
						let first = files.first().map(|(p, _)| {
							(
								p.clone(),
								browser::git_preview_with(
									&git, &source, p, &opts,
								)
								.map(|p| browser::SourcePreview {
									content: p.content,
									patch: p.patch,
								})
								.map_err(|e| e.to_string()),
							)
						});
						Ok::<_, String>((files, first))
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.preview_generation != task_generation {
						return;
					}
					match res {
						Ok((mut files, first)) => {
							let total = files.len();
							files.truncate(MAX_COMMIT_FILES);
							app_log!("[APP:E2E_CHANGES: files={}]", total);
							model.commit_files = files;
							match first {
								Some((path, p)) => {
									model.selected_commit_file =
										Some(path.clone());
									model.apply_source_preview(
										path,
										p.map(|p| (p, preview_source)),
									);
								}
								None => {
									model.preview_loading = false;
									model.preview_error =
										Some(Msg::new("status_no_changes", []));
								}
							}
						}
						Err(e) => {
							model.preview_loading = false;
							model.preview_error =
								Some(Msg::new("error_history", [e]));
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// Opens one file of the selected commit or compare as a diff.
	pub fn select_commit_file(&mut self, path: &str, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		let (source, psource) = match (&self.compare, &self.selected_commit) {
			(Some((a, b)), _) => (
				GitSource::Range(a.clone(), b.clone()),
				PreviewSource::Compare {
					from: a.clone(),
					to: b.clone(),
				},
			),
			(None, Some(sha)) => (
				GitSource::Commit(sha.clone()),
				PreviewSource::CommitDiff { sha: sha.clone() },
			),
			_ => return,
		};
		let Some(root) = self.repo_root() else {
			return;
		};
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.selected_commit_file = Some(path.to_string());
		self.preview_loading = true;
		let path = path.to_string();
		let for_bg = path.clone();
		let cancel = arm_cancel(&mut self.preview_cancel);
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel.clone()),
			async move {
				let res = bg
					.spawn(async move {
						let opts = RunOptions {
							cancel: Some(cancel),
							max_stdout: browser::PREVIEW_LIMIT,
							overflow: snip_core::gitrun::Overflow::Error,
							..RunOptions::preview(None)
						};
						let git = Git::open_with(&root, &opts)
							.map_err(|e| e.to_string())?;
						browser::git_preview_with(&git, &source, &for_bg, &opts)
							.map(|p| browser::SourcePreview {
								content: p.content,
								patch: p.patch,
							})
							.map_err(|e| e.to_string())
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.preview_generation != task_generation {
						return;
					}
					if model.apply_source_preview(
						path.clone(),
						res.map(|p| (p, psource)),
					) {
						app_log!("[APP:PREVIEW_LOADED: {}]", path);
					}
					cx.notify();
				});
			},
		);
	}

	/// Keyboard move in the log; `extend` grows the range instead.
	pub fn log_move(
		&mut self,
		delta: isize,
		extend: bool,
		cx: &mut Context<Self>,
	) {
		let rows: Vec<String> = self
			.display_commits()
			.iter()
			.map(|c| c.sha.clone())
			.collect();
		if rows.is_empty() {
			return;
		}
		let from = if extend {
			self.range_head.as_ref().or(self.selected_commit.as_ref())
		} else {
			self.selected_commit.as_ref()
		};
		let cur = from.and_then(|s| rows.iter().position(|r| r == s));
		let next = match cur {
			Some(c) => {
				(c as isize + delta).clamp(0, rows.len() as isize - 1) as usize
			}
			None => 0,
		};
		self.log_scroll.scroll_to_item(next, ScrollStrategy::Center);
		if extend && self.selected_commit.is_some() {
			self.extend_range(&rows[next], cx);
		} else {
			self.select_commit(&rows[next], cx);
		}
	}

	pub fn browse_commit_tree(&mut self, cx: &mut Context<Self>) {
		let Some(sha) = self.selected_commit.clone() else {
			return;
		};
		self.tree_generation += 1;
		let _ = arm_cancel(&mut self.rev_tree_cancel);
		self.rev_tree = Some(RevTree::new(sha.clone()));
		self.tree_cursor = 0;
		self.active_tab = WorkbenchTab::FileExplorer;
		self.left_visible = true;
		app_log!("[APP:REV_TREE: {}]", &sha[..7]);
		self.load_rev_dir(String::new(), cx);
		cx.notify();
	}

	pub fn leave_rev_tree(&mut self, cx: &mut Context<Self>) {
		self.tree_generation += 1;
		let _ = arm_cancel(&mut self.rev_tree_cancel);
		self.rev_tree = None;
		self.tree_cursor = 0;
		app_log!("[APP:REV_TREE: off]");
		cx.notify();
	}

	fn load_rev_dir(&mut self, dir: String, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		let (Some(root), Some(tree)) =
			(self.repo_root(), self.rev_tree.as_ref())
		else {
			return;
		};
		let sha = tree.sha.clone();
		let gen = self.tree_generation;
		let cancel = arm_cancel(&mut self.rev_tree_cancel);
		let cancel_bg = cancel.clone();

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let (sha_bg, dir_bg) = (sha.clone(), dir.clone());
				let res = bg
					.spawn(async move {
						let opts = crate::interactive_read_opts(cancel_bg);
						let git = Git::open_with(&root, &opts)
							.map_err(|e| e.to_string())?;
						browser::commit_directory_with(
							&git,
							&sha_bg,
							&dir_bg,
							MAX_REV_ROWS,
							&opts,
						)
						.map_err(|e| e.to_string())
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.tree_generation != gen {
						return;
					}
					let Some(tree) = model.rev_tree.as_mut() else {
						return;
					};
					match res {
						Ok((entries, truncated)) => {
							if e2e_on() {
								let names: Vec<&str> = entries
									.iter()
									.map(|e| e.name.as_str())
									.collect();
								app_log!(
									"[APP:E2E_TREE: rev={} dir={} entries={} fnv={:x}]",
									&sha[..7],
									if dir.is_empty() { "/" } else { &dir },
									entries.len(),
									fnv1a(names.join("\n").as_bytes())
								);
							}
							tree.insert_dir(dir, entries, truncated);
						}
						Err(e) => {
							tree.note_error(dir, e);
						}
					}
					cx.notify();
				});
			},
		);
	}

	pub fn rev_tree_click(
		&mut self,
		path: &str,
		is_dir: bool,
		cx: &mut Context<Self>,
	) {
		let Some(tree) = self.rev_tree.as_mut() else {
			return;
		};
		if is_dir {
			if tree.expanded.contains(path) {
				tree.release_dir(path);
			} else {
				tree.note_expanded(path);
				app_log!("[APP:TREE_EXPANDED: {}]", path);
				if !tree.dirs.contains_key(path) {
					self.load_rev_dir(path.to_string(), cx);
				}
			}
			cx.notify();
			return;
		}
		let sha = tree.sha.clone();
		self.open_rev_file(sha, path.to_string(), cx);
	}

	fn open_rev_file(
		&mut self,
		sha: String,
		path: String,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let Some(root) = self.repo_root() else {
			return;
		};
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		let cancel = arm_cancel(&mut self.preview_cancel);
		let cancel_bg = cancel.clone();

		self.selected_file = None;
		self.selected_commit_file = Some(path.clone());
		self.preview_loading = true;
		self.preview_error = None;
		app_log!("[APP:TREE_FILE_SELECTED: {}]", path);
		let (sha_bg, path_bg) = (sha.clone(), path.clone());
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let res = bg
					.spawn(async move {
						let opts = RunOptions {
							cancel: Some(cancel_bg),
							..RunOptions::preview(None)
						};
						let git = Git::open_with(&root, &opts)
							.map_err(|e| e.to_string())?;
						browser::commit_blob_with(
							&git,
							&sha_bg,
							&path_bg,
							browser::MAX_BLOB_BYTES,
							&opts,
						)
						.map_err(|e| e.to_string())
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.preview_generation != task_generation {
						return;
					}
					model.preview_loading = false;
					match res {
						Ok(BlobText::Text(s)) => {
							let lang = Language::from_path_or_ext(&path, false);
							if model.set_preview(Preview::new(
								PreviewSource::CommitFile { sha: sha.clone() },
								Some(path.clone()),
								s,
								false,
								lang,
							)) {
								app_log!("[APP:PREVIEW_LOADED: {}]", path);
							}
						}
						Ok(BlobText::Binary) => {
							model.preview_error =
								Some(Msg::new("error_binary", [path.clone()]))
						}
						Ok(BlobText::NotUtf8) => {
							model.preview_error =
								Some(Msg::new("error_not_utf8", [path.clone()]))
						}
						Ok(BlobText::TooLarge(n)) => {
							model.preview_error = Some(Msg::new(
								"error_too_large",
								[path.clone(), n.to_string()],
							))
						}
						Err(e) => {
							model.preview_error = Some(Msg::new(
								"error_preview",
								[path.clone(), e],
							))
						}
					}
					cx.notify();
				});
			},
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn c(sha: &str, parents: &[&str]) -> CommitSummary {
		CommitSummary {
			sha: sha.into(),
			parents: parents.iter().map(|s| s.to_string()).collect(),
			author_name: String::new(),
			author_email: String::new(),
			author_date: String::new(),
			subject: String::new(),
		}
	}

	fn history(commits: Vec<CommitSummary>) -> browser::RepositoryHistory {
		browser::RepositoryHistory {
			root: String::new(),
			commits,
			refs: Vec::new(),
			head: None,
			has_more: true,
		}
	}

	fn first_page() -> PreparedHistory {
		PreparedHistory::prepare(
			history(vec![c("c2", &["c1"])]),
			0,
			&[],
			Vec::new(),
			None,
			None,
		)
		.unwrap()
	}

	fn oversized_short_string() -> String {
		let mut value = String::with_capacity(MAX_RETAINED_GRAPH_BYTES);
		value.push('x');
		value
	}

	#[test]
	fn graph_admission_refuses_spare_capacity_in_page_and_metadata() {
		let previous = first_page();
		let mut next = c("c1", &["c0"]);
		next.subject = oversized_short_string();
		let rejected = PreparedHistory::prepare(
			history(vec![next]),
			1,
			&previous.page_checkpoints,
			Vec::new(),
			None,
			None,
		);
		assert!(matches!(rejected, Err(GraphAdmissionError::Budget)));

		let rejected = PreparedHistory::prepare(
			history(vec![c("c1", &["c0"])]),
			1,
			&previous.page_checkpoints,
			vec![oversized_short_string()],
			None,
			None,
		);
		assert!(matches!(rejected, Err(GraphAdmissionError::Budget)));
		let rejected = PreparedHistory::prepare(
			history(Vec::new()),
			0,
			&[],
			Vec::new(),
			None,
			Some(LogSearch {
				query: oversized_short_string(),
				author: false,
			}),
		);
		assert!(matches!(rejected, Err(GraphAdmissionError::Budget)));
	}

	#[test]
	fn graph_admission_leaves_room_for_the_separate_search_input() {
		let input = crate::text_input::TextInput::new_for_test(
			&"🦀".repeat(crate::text_input::TextInput::MAX_TOTAL_CHARS + 1),
		);
		assert_eq!(input.text().len(), 16 * 1024);
		// This page itself fits, but would leave only 8 KiB for independently
		// edited metadata. Reserving the existing input bound must reject it.
		let mut commit = c("c1", &[]);
		commit.subject =
			String::with_capacity(MAX_RETAINED_GRAPH_BYTES - 8 * 1024);
		commit.subject.push('x');
		assert!(matches!(
			PreparedHistory::prepare(
				history(vec![commit]),
				0,
				&[],
				Vec::new(),
				None,
				None,
			),
			Err(GraphAdmissionError::Budget)
		));
	}

	#[test]
	fn graph_admission_propagates_layout_failure() {
		let previous = first_page();
		let mut bad = history(vec![c("c1", &["c0"])]);
		bad.refs = (0..1001)
			.map(|n| browser::GitReference {
				name: format!("refs/heads/b{n}"),
				sha: "c1".into(),
			})
			.collect();
		let rejected = PreparedHistory::prepare(
			bad,
			1,
			&previous.page_checkpoints,
			Vec::new(),
			None,
			None,
		);
		assert!(
			matches!(rejected, Err(GraphAdmissionError::Layout(message)) if message.contains("refs limit"))
		);
	}

	#[test]
	fn graph_budget_counts_nested_output_and_all_checkpoint_and_collapse_storage(
	) {
		let mut page = first_page();
		page.graph_layout.as_mut().unwrap().rows[0].parent_edges[0]
			.parent_sha = oversized_short_string();
		assert!(matches!(
			page.check_budget(),
			Err(GraphAdmissionError::Budget)
		));
		page = first_page();
		page.graph_layout.as_mut().unwrap().paths[0].points.reserve(
			MAX_RETAINED_GRAPH_BYTES
				/ std::mem::size_of::<snip_core::graph::Point>(),
		);
		assert!(matches!(
			page.check_budget(),
			Err(GraphAdmissionError::Budget)
		));
		page = first_page();
		page.page_checkpoints[1].as_mut().unwrap().frontier[0].next_sha =
			oversized_short_string();
		assert!(matches!(
			page.check_budget(),
			Err(GraphAdmissionError::Budget)
		));
		page = first_page();
		page.page_checkpoints.reserve(
			MAX_RETAINED_GRAPH_BYTES
				/ std::mem::size_of::<Option<GraphCheckpoint>>(),
		);
		assert!(matches!(
			page.check_budget(),
			Err(GraphAdmissionError::Budget)
		));
		page = first_page();
		page.collapsed_merges =
			(0..150_000).map(|n| format!("{n:0128x}")).collect();
		assert!(matches!(
			page.check_budget(),
			Err(GraphAdmissionError::Budget)
		));
		page = first_page();
		let mut checkpoint = page.page_checkpoints[1].clone().unwrap();
		checkpoint
			.frontier
			.resize(64, checkpoint.frontier[0].clone());
		for rail in &mut checkpoint.frontier {
			rail.next_sha = "a".repeat(MAX_SHA_LEN);
		}
		page.page_checkpoints = vec![Some(checkpoint); 2000];
		assert!(matches!(
			page.check_budget(),
			Err(GraphAdmissionError::Budget)
		));
	}

	#[test]
	fn graph_collapse_keeps_true_edges_and_checkpoint_for_backward_navigation()
	{
		let commits = vec![
			c("m", &["a", "f"]),
			c("f", &["base"]),
			c("a", &["base"]),
			c("base", &[]),
		];
		let full = PreparedHistory::prepare(
			history(commits.clone()),
			0,
			&[],
			Vec::new(),
			None,
			None,
		)
		.unwrap();
		let collapsed = PreparedHistory::prepare(
			history(commits),
			0,
			&[],
			vec!["m".into(), "earlier-page-merge".into()],
			None,
			None,
		)
		.unwrap();
		assert_eq!(collapsed.hidden_commits, ["f"]);
		assert_eq!(collapsed.collapsed_merges, ["m", "earlier-page-merge"]);
		assert_eq!(collapsed.page_checkpoints, full.page_checkpoints);
		let merge = &collapsed.graph_layout.as_ref().unwrap().rows[0];
		let edge = merge
			.parent_edges
			.iter()
			.find(|edge| edge.parent_sha == "f")
			.unwrap();
		assert_eq!(
			edge.continuation,
			snip_core::graph::ContinuationKind::FilteredGap
		);
		assert!(edge.to_row.is_none());
	}

	fn walk_standard_history(
		mut load: impl FnMut(usize) -> browser::RepositoryHistory,
	) -> usize {
		let mut page =
			PreparedHistory::prepare(load(0), 0, &[], Vec::new(), None, None)
				.unwrap();
		let mut peak = page.retained_bytes();
		let mut total = page.commits.len();
		for index in 1..400 {
			assert!(page.history_has_more, "history stopped at page {index}");
			page = PreparedHistory::prepare(
				load(index),
				index,
				&page.page_checkpoints,
				page.collapsed_merges.clone(),
				None,
				None,
			)
			.unwrap();
			assert_eq!(
				page.graph_layout.as_ref().unwrap().rows[0].global_row,
				index * 50
			);
			total += page.commits.len();
			peak = peak.max(page.retained_bytes());
		}
		assert_eq!(total, 20_000);
		assert!(!page.history_has_more);
		assert!(peak <= MAX_RETAINED_GRAPH_BYTES);
		for index in [398, 200, 0] {
			page = PreparedHistory::prepare(
				load(index),
				index,
				&page.page_checkpoints,
				page.collapsed_merges.clone(),
				None,
				None,
			)
			.unwrap();
			assert_eq!(page.commit_page, index);
			assert_eq!(
				page.graph_layout.as_ref().unwrap().rows[0].global_row,
				index * 50
			);
		}
		peak
	}

	#[test]
	fn graph_standard_20k_history_remains_navigable_with_100_refs() {
		let commits: Vec<_> = (0..20_000)
			.rev()
			.map(|n| {
				let parents = match n {
					0 => vec![],
					1 | 2 => vec![0],
					3 => vec![2, 1],
					_ => vec![n - 1],
				};
				let mut commit = c(&format!("{n:040x}"), &[]);
				commit.parents = parents
					.into_iter()
					.map(|parent| format!("{parent:040x}"))
					.collect();
				commit
			})
			.collect();
		let peak = walk_standard_history(|page| browser::RepositoryHistory {
			root: String::new(),
			commits: commits[page * 50..(page + 1) * 50].to_vec(),
			refs: (0..100)
				.map(|n| browser::GitReference {
					name: format!("refs/heads/b{n}"),
					sha: format!("{:040x}", 19_999 - n),
				})
				.collect(),
			head: Some(format!("{:040x}", 19_999)),
			has_more: page < 399,
		});
		println!("20k/100-ref graph peak retained capacity: {peak} bytes");
	}

	#[test]
	#[ignore = "read-only standard workload proof; set SNIP_STANDARD_WORKLOAD and run explicitly"]
	fn graph_standard_fixture_20k_paging() {
		let fixture = std::env::var_os("SNIP_STANDARD_WORKLOAD").expect(
			"SNIP_STANDARD_WORKLOAD must point to the standard fixture",
		);
		let repo = std::path::PathBuf::from(fixture).join("repo-01-core");
		let git = Git::open(&repo).expect("standard fixture repo must exist");
		let peak = walk_standard_history(|page| {
			browser::history(&git, None, "", page * 50, 50).unwrap()
		});
		println!(
			"Standard Git fixture peak retained graph capacity: {peak} bytes"
		);
	}

	#[test]
	fn side_only_hides_just_the_merged_branch() {
		// m merges f2 (f2 -> f1 -> base) into main (a -> base).
		let commits = vec![
			c("m", &["a", "f2"]),
			c("f2", &["f1"]),
			c("a", &["base"]),
			c("f1", &["base"]),
			c("base", &[]),
		];
		let side = side_only(&commits, "m");
		assert_eq!(side, ["f1", "f2"].iter().map(|s| s.to_string()).collect());
		assert!(side_only(&commits, "a").is_empty(), "not a merge");
		assert!(side_only(&commits, "zz").is_empty(), "not loaded");
	}

	#[test]
	fn rev_tree_rows_are_bounded_and_mark_loading() {
		let mut t = RevTree::new("abc".into());
		assert!(t.rows()[0].marker.is_some(), "root not loaded yet");
		let entries = (0..MAX_REV_ROWS + 10)
			.map(|i| TreeEntry {
				path: format!("f{i}"),
				name: format!("f{i}"),
				kind: TreeKind::Blob,
			})
			.collect();
		t.dirs.insert(String::new(), (entries, false));
		let rows = t.rows();
		assert!(rows.len() <= MAX_REV_ROWS + 1);
		assert!(rows.last().unwrap().marker.is_some());
	}

	#[test]
	fn rev_tree_hard_cap_rejects_oversized_root_child_and_errors() {
		let mut tree = RevTree::new("abc".into());
		let huge = TreeEntry {
			path: "p".repeat(MAX_RETAINED_TREE_BYTES),
			name: "n".repeat(MAX_RETAINED_TREE_BYTES),
			kind: TreeKind::Blob,
		};
		tree.insert_dir("".into(), vec![huge.clone()], false);
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
		assert!(tree
			.dirs
			.values()
			.flat_map(|(entries, _)| entries)
			.all(|entry| entry.path.len() < MAX_RETAINED_TREE_BYTES));
		let child = "c".repeat(MAX_RETAINED_TREE_BYTES + 100);
		tree.insert_dir(child, vec![huge], false);
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
		assert!(tree
			.dirs
			.keys()
			.all(|key| key.len() <= MAX_RETAINED_TREE_BYTES));
		tree.note_error("err".into(), "e".repeat(MAX_RETAINED_TREE_BYTES * 4));
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
		assert!(tree
			.errors
			.values()
			.all(|err| err.len() <= MAX_STORED_ERROR_BYTES));
		for i in 0..80 {
			tree.insert_dir(
				format!("dir-{i}"),
				vec![TreeEntry {
					path: format!("file-{i}-{}", "x".repeat(4_000)),
					name: format!("file-{i}"),
					kind: TreeKind::Blob,
				}],
				false,
			);
			tree.expanded
				.insert(format!("exp-{i}-{}", "k".repeat(2_000)));
			tree.enforce_budget();
			assert!(
				tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES,
				"step {i} retained {} over {}",
				tree.retained_bytes(),
				MAX_RETAINED_TREE_BYTES
			);
		}
		tree.enforce_budget();
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
	}

	#[test]
	fn test_rev_tree_directory_budget_eviction() {
		let mut t = RevTree::new("abc".into());
		// Root directory
		t.insert_dir("".to_string(), vec![], false);
		// Add MAX_CACHED_DIRS + 10 directories
		for i in 0..(MAX_CACHED_DIRS + 10) {
			t.insert_dir(format!("dir_{i}"), vec![], false);
		}
		// Total directories must be bounded by MAX_CACHED_DIRS
		assert!(
			t.dirs.len() <= MAX_CACHED_DIRS,
			"dirs count {} must not exceed MAX_CACHED_DIRS {}",
			t.dirs.len(),
			MAX_CACHED_DIRS
		);
		assert!(
			t.dirs.contains_key(""),
			"small root stays while non-root listings fit"
		);
		assert!(
			!t.dirs.contains_key("dir_0"),
			"oldest non-root listing is evicted first"
		);
		assert!(
			t.dirs.contains_key(&format!("dir_{}", MAX_CACHED_DIRS + 9)),
			"newest listing stays"
		);
		assert!(t.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
	}

	#[test]
	fn test_rev_tree_ordinary_root_admission_and_truncation_retains_usable_root(
	) {
		for len in [10, 80] {
			for n in [1000, 2000] {
				let entries: Vec<TreeEntry> = (0..n)
					.map(|i| {
						let name = format!("{i:04}-{}", "x".repeat(len));
						TreeEntry {
							path: name.clone(),
							name,
							kind: TreeKind::Blob,
						}
					})
					.collect();
				let mut tree = RevTree::new("a".repeat(40));
				tree.insert_dir("".into(), entries, false);
				assert!(
					tree.dirs.contains_key(""),
					"root must be retained, not evicted (n={n}, len={len})"
				);
				assert!(
					tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES,
					"retained bytes {} exceeds budget {}",
					tree.retained_bytes(),
					MAX_RETAINED_TREE_BYTES
				);
				let rows = tree.rows();
				assert!(
					rows.len() > 1,
					"tree rows must be visible and usable, not loading forever"
				);
				assert!(
					!rows.iter().any(|r| r
						.marker
						.as_ref()
						.map(|m| m.key == "status_loading")
						.unwrap_or(false)),
					"root rows must not say loading forever"
				);
			}
		}
	}
}
