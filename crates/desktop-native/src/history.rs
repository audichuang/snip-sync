//! Git log behaviour: paged graph, search, merge collapse, commit / range
//! selection, endpoint compare, HEAD, and a read-only tree of any commit.
//! Reads go through `snip-core` (`browser::history`, `gitsrc`, `graph`);
//! the few missing reads use `core_shim` until core exposes them.

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

impl WorkbenchModel {
	/// Commits visible in the log, in display order.
	pub fn display_commits(&self) -> Vec<&CommitSummary> {
		self.commits
			.iter()
			.filter(|c| !self.hidden_commits.contains(&c.sha))
			.collect()
	}

	pub fn load_history(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		let Some(repo_root) = self.repo_root() else {
			return;
		};
		let ref_filter = self.active_ref_filter.clone();
		let search = self.log_search.clone();
		let page = self.commit_page;
		let page_size = self.history_page_size;
		let skip = page * page_size;
		let checkpoint = self.page_checkpoints.get(page).cloned().flatten();

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
								model.commits = hist.commits;
								model.refs = hist.refs;
								model.head_sha = hist.head;
								model.history_has_more = hist.has_more;
								model.hidden_commits.clear();
								if model.log_search.is_none() {
									// Checkpoint for the next page comes from the
									// full (uncollapsed) layout of this page.
									if let Ok(full) =
										graph_view::layout_commits_paged(
											&model.commits,
											&model.refs,
											model.head_sha.as_deref(),
											checkpoint.as_ref(),
										) {
										if let Some(next_cp) =
											full.checkpoint.clone()
										{
											if model.page_checkpoints.len()
												<= page + 1
											{
												model
													.page_checkpoints
													.resize(page + 2, None);
											}
											model.page_checkpoints[page + 1] =
												Some(next_cp);
										}
										model.graph_layout = Some(full);
									}
									model.apply_collapse(checkpoint.as_ref());
								} else {
									// Search results are not a history: no rails, so
									// no parent relation is implied between them.
									model.graph_layout = None;
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
								app_log!("[APP:HISTORY_ERROR]");
								model.commits.clear();
								model.graph_layout = None;
								model.history_error = Some(e.clone());
								model.set_status("error_history", [e]);
							}
						}
						cx.notify();
					});
			},
		);
	}

	/// Recomputes the displayed layout for the collapsed merges on this page.
	fn apply_collapse(
		&mut self,
		checkpoint: Option<&snip_core::graph::GraphCheckpoint>,
	) {
		let mut hidden = HashSet::new();
		for m in &self.collapsed_merges {
			hidden.extend(side_only(&self.commits, m));
		}
		self.hidden_commits = hidden;
		if self.hidden_commits.is_empty() {
			return;
		}
		let shown: Vec<CommitSummary> = self
			.commits
			.iter()
			.filter(|c| !self.hidden_commits.contains(&c.sha))
			.cloned()
			.collect();
		match graph_view::layout_commits_filtered(
			&shown,
			&self.refs,
			self.head_sha.as_deref(),
			checkpoint,
			self.hidden_commits.clone(),
		) {
			Ok(l) => self.graph_layout = Some(l),
			Err(e) => self.history_error = Some(e),
		}
	}

	pub fn toggle_collapse(&mut self, merge: String, cx: &mut Context<Self>) {
		let collapsed = if self.collapsed_merges.remove(&merge) {
			false
		} else {
			self.collapsed_merges.insert(merge.clone());
			true
		};
		// Re-layout from scratch for this page (checkpoint stays per page).
		let checkpoint = self
			.page_checkpoints
			.get(self.commit_page)
			.cloned()
			.flatten();
		if let Ok(full) = graph_view::layout_commits_paged(
			&self.commits,
			&self.refs,
			self.head_sha.as_deref(),
			checkpoint.as_ref(),
		) {
			self.graph_layout = Some(full);
		}
		self.apply_collapse(checkpoint.as_ref());
		let hidden = side_only(&self.commits, &merge).len();
		app_log!(
			"[APP:MERGE_COLLAPSE: sha={} collapsed={} hidden={} shown={}]",
			&merge[..7.min(merge.len())],
			collapsed,
			hidden,
			self.display_commits().len()
		);
		cx.notify();
	}

	pub fn history_next_page(&mut self, cx: &mut Context<Self>) {
		if self.history_has_more {
			self.commit_page += 1;
			self.load_history(cx);
		}
	}

	pub fn history_prev_page(&mut self, cx: &mut Context<Self>) {
		if self.commit_page > 0 {
			self.commit_page -= 1;
			self.load_history(cx);
		}
	}

	pub fn filter_by_ref(
		&mut self,
		ref_name: Option<String>,
		cx: &mut Context<Self>,
	) {
		app_log!("[APP:REF_FILTER: {}]", ref_name.as_deref().unwrap_or("all"));
		self.active_ref_filter = ref_name;
		self.log_search = None;
		self.commit_page = 0;
		self.page_checkpoints = vec![None];
		self.collapsed_merges.clear();
		self.load_history(cx);
	}

	pub fn start_log_search(&mut self, query: String, cx: &mut Context<Self>) {
		self.log_search = (!query.is_empty()).then_some(LogSearch {
			query,
			author: self.search_by_author,
		});
		app_log!(
			"[APP:LOG_SEARCH: active={} author={}]",
			self.log_search.is_some(),
			self.search_by_author
		);
		self.commit_page = 0;
		self.page_checkpoints = vec![None];
		self.collapsed_merges.clear();
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
		self.log_search = None;
		self.active_ref_filter = None;
		self.commit_page = 0;
		self.page_checkpoints = vec![None];
		self.collapsed_merges.clear();
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
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.selected_commit = Some(sha.to_string());
		self.range_head = None;
		self.compare = None;
		self.selected_file = None;
		self.selected_commit_file = None;
		self.commit_files.clear();
		self.preview = None;
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
		if self.selected_commit.is_none() {
			self.select_commit(sha, cx);
			return;
		}
		self.range_head = (self.selected_commit.as_deref() != Some(sha))
			.then(|| sha.to_string());
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
		let (newer, older) = (rows[top].sha.clone(), rows[bottom].sha.clone());
		drop(rows);
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.compare = Some((older.clone(), newer.clone()));
		self.selected_commit_file = None;
		self.commit_files.clear();
		self.preview = None;
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
						// `list_changed_paths` still uses default Git options.
						let files = gitsrc::list_changed_paths(&git, &source)
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
					model.apply_source_preview(
						path.clone(),
						res.map(|p| (p, psource)),
					);
					app_log!("[APP:PREVIEW_LOADED: {}]", path);
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
							model.set_preview(Preview::new(
								PreviewSource::CommitFile { sha: sha.clone() },
								Some(path.clone()),
								s,
								false,
								lang,
							));
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
					app_log!("[APP:PREVIEW_LOADED: {}]", path);
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
