//! Git log behaviour: paged graph, search, merge collapse, commit / range
//! selection, endpoint compare, HEAD, and a read-only tree of any commit.
//! Reads go through `snip-core` (`browser::history`, `gitsrc`, `graph`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crate::arm_cancel;

use crate::githost::GitHost;
use crate::graph_view;
use crate::i18n::Msg;
use crate::multi_log;
use crate::reader::{fnv1a, Preview, PreviewSource};
use crate::syntax::Language;
use crate::{e2e_on, WorkbenchModel, WorkbenchTab};
use gpui::{Context, ScrollStrategy};
use snip_core::browser::{self, BlobText, CommitSummary, TreeEntry, TreeKind};
use snip_core::gitsrc::GitSource;
use snip_core::gitview::{ChangedPathList, Read, ReadProfile, RepoView};
use snip_core::graph::{GraphCheckpoint, GraphLayout, MAX_SHA_LEN};
use snip_core::workspace::RepoIdentity;

/// Most commits of one multi-selection whose changed files are listed
/// (one listing each).
pub const MAX_SELECTION_READS: usize = 100;
/// Commits of an open multi-selection whose full details are read.
pub const MAX_SELECTION_DETAILS: usize = 20;
/// Files listed for one commit or compare; more are counted, not kept.
pub const MAX_COMMIT_FILES: usize = snip_core::gitview::MAX_COMMIT_FILES;
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
// copies are outside this application-data accounting. The ref selector
// borrows this model; its visible GPUI elements belong to the renderer scope.
const GRAPH_METADATA_RESERVE: usize = 4
	* (MAX_SHA_LEN + std::mem::size_of::<Option<String>>())
	+ 4 * crate::text_input::TextInput::MAX_TOTAL_CHARS
	+ std::mem::size_of::<crate::text_input::TextInput>()
	+ 2 * std::mem::size_of::<usize>() // Arc<str> reference counts
	+ 2 * MAX_STORED_ERROR_BYTES
	+ std::mem::size_of::<Option<String>>()
	+ std::mem::size_of::<Msg>()
	+ 2 * std::mem::size_of::<String>();

pub use browser::LogQuery;

/// Pages the log keeps at once. Scrolling past either end loads the next
/// page and evicts the farthest one; evicted pages are read back from their
/// graph checkpoints.
pub const MAX_WINDOW_PAGES: usize = 10;
/// Longest `user.email` kept to mark the user's own commits.
pub const MAX_USER_EMAIL: usize = snip_core::gitview::MAX_USER_EMAIL;

pub use snip_core::gitview::{clip_utf8, CommitDetails};

/// How a history read changes the loaded window of pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageLoad {
	/// A fresh window holding only this page.
	Replace(usize),
	/// The page after the window, evicting the first page when full.
	Next,
	/// The page before the window, evicting the last page when full.
	Prev,
}

/// The window after `load` brings in `page` (the fetched page's commits and
/// whether more follow it): `(commits, first, last, has_more)`. Every page but
/// the last is exactly `page_size` commits, so eviction is by whole pages.
#[allow(clippy::too_many_arguments)]
fn merge_window(
	window: &[CommitSummary],
	first: usize,
	last: usize,
	window_more: bool,
	load: PageLoad,
	page: Vec<CommitSummary>,
	page_more: bool,
	page_size: usize,
) -> (Vec<CommitSummary>, usize, usize, bool) {
	match load {
		PageLoad::Replace(p) => (page, p, p, page_more),
		PageLoad::Next => {
			let mut out = window.to_vec();
			out.extend(page);
			let mut first = first;
			let last = last + 1;
			if last - first + 1 > MAX_WINDOW_PAGES {
				out.drain(..page_size.min(out.len()));
				first += 1;
			}
			(out, first, last, page_more)
		}
		PageLoad::Prev => {
			let first = first.saturating_sub(1);
			let mut out = page;
			out.extend_from_slice(window);
			let (mut last, mut more) = (last, window_more);
			if last - first + 1 > MAX_WINDOW_PAGES {
				last -= 1;
				out.truncate(MAX_WINDOW_PAGES * page_size);
				more = true;
			}
			(out, first, last, more)
		}
	}
}

/// Per commit: reachable from `seeds` (HEAD, or the previous window's
/// marks). One pass, since topological order lists children first.
pub fn mark_on_head(
	commits: &[CommitSummary],
	seeds: impl IntoIterator<Item = String>,
) -> Vec<bool> {
	let mut reach: HashSet<String> = seeds.into_iter().collect();
	commits
		.iter()
		.map(|c| {
			let on = reach.contains(&c.sha);
			if on {
				reach.extend(c.parents.iter().cloned());
			}
			on
		})
		.collect()
}

/// Paths the Paths chip combines at most.
pub const MAX_LOG_PATHS: usize = 32;

/// A typed path as a repository-relative pathspec: trimmed, `/`-separated.
pub fn clean_log_path(path: &str) -> String {
	path.trim().replace('\\', "/").trim_matches('/').to_string()
}

/// The Date chip's custom range as `--since` / `--until` values covering
/// both whole days; `None` when a non-empty field is not `YYYY-MM-DD` or
/// the range ends before it starts.
pub fn date_range(
	since: &str,
	until: &str,
) -> Option<(Option<String>, Option<String>)> {
	let day = |s: &str| -> Option<Option<chrono::NaiveDate>> {
		if s.is_empty() {
			Some(None)
		} else {
			chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
				.ok()
				.map(Some)
		}
	};
	let (from, to) = (day(since)?, day(until)?);
	if let (Some(a), Some(b)) = (from, to) {
		if b < a {
			return None;
		}
	}
	Some((
		from.map(|d| format!("{d} 00:00:00")),
		to.map(|d| format!("{d} 23:59:59")),
	))
}

/// Lazily listed tree of one commit (no checkout).
pub struct RevTree {
	pub sha: String,
	dirs: Vec<(String, (Vec<TreeEntry>, bool))>,
	/// Oldest directory key at the front. This is the eviction order.
	order: VecDeque<String>,
	expanded: Vec<String>,
	/// Oldest expanded key at the front. Not hash-map iteration order.
	expanded_order: VecDeque<String>,
	errors: Vec<(String, String)>,
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
		let expanded = vec![String::new()];
		Self {
			sha,
			dirs: Vec::new(),
			order: VecDeque::new(),
			expanded,
			expanded_order: VecDeque::new(),
			errors: Vec::new(),
			error_order: VecDeque::new(),
		}
	}

	pub fn retained_bytes(&self) -> usize {
		let mut total = std::mem::size_of::<Self>()
			.saturating_add(self.sha.capacity())
			.saturating_add(self.dirs.capacity().saturating_mul(
				std::mem::size_of::<(String, (Vec<TreeEntry>, bool))>(),
			))
			.saturating_add(
				self.errors
					.capacity()
					.saturating_mul(std::mem::size_of::<(String, String)>()),
			)
			.saturating_add(
				self.expanded
					.capacity()
					.saturating_mul(std::mem::size_of::<String>()),
			)
			.saturating_add(deque_slot_cost(&self.order))
			.saturating_add(deque_slot_cost(&self.expanded_order))
			.saturating_add(deque_slot_cost(&self.error_order));
		for (key, (entries, _)) in &self.dirs {
			total = total.saturating_add(key.capacity()).saturating_add(
				entries
					.capacity()
					.saturating_mul(std::mem::size_of::<TreeEntry>()),
			);
			for entry in entries {
				total = total
					.saturating_add(entry.path.capacity())
					.saturating_add(entry.name.capacity());
			}
		}
		for key in self
			.order
			.iter()
			.chain(&self.expanded_order)
			.chain(&self.error_order)
			.chain(&self.expanded)
		{
			total = total.saturating_add(key.capacity());
		}
		for (key, err) in &self.errors {
			total = total
				.saturating_add(key.capacity())
				.saturating_add(err.capacity());
		}
		total
	}

	fn dir(&self, path: &str) -> Option<&(Vec<TreeEntry>, bool)> {
		self.dirs
			.binary_search_by(|(key, _)| key.as_str().cmp(path))
			.ok()
			.map(|index| &self.dirs[index].1)
	}

	fn dir_mut(&mut self, path: &str) -> Option<&mut (Vec<TreeEntry>, bool)> {
		self.dirs
			.binary_search_by(|(key, _)| key.as_str().cmp(path))
			.ok()
			.map(|index| &mut self.dirs[index].1)
	}

	fn put_dir(&mut self, path: String, value: (Vec<TreeEntry>, bool)) {
		match self.dirs.binary_search_by(|(key, _)| key.cmp(&path)) {
			Ok(index) => self.dirs[index] = (path, value),
			Err(index) => self.dirs.insert(index, (path, value)),
		}
	}

	fn is_expanded(&self, path: &str) -> bool {
		self.expanded
			.binary_search_by(|key| key.as_str().cmp(path))
			.is_ok()
	}

	fn entry_count(&self) -> usize {
		self.dirs
			.iter()
			.map(|(_, (entries, _))| entries.len())
			.sum()
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
		self.dirs.retain(|(have, _)| have != &key);
		self.expanded.retain(|have| have != &key);
		self.expanded_order.retain(|item| item != &key);
		self.errors.retain(|(have, _)| have != &key);
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
		for (_, (entries, _)) in &mut self.dirs {
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
		let before = self.errors.len();
		self.errors.retain(|(have, _)| have != &key);
		self.errors.len() != before
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
			let dir_keys: Vec<String> =
				self.dirs.iter().map(|(key, _)| key.clone()).collect();
			let expanded_keys = self.expanded.to_vec();
			let error_keys: Vec<String> =
				self.errors.iter().map(|(key, _)| key.clone()).collect();
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
				self.expanded.retain(|have| have != &key);
				continue;
			}
			let oldest = self.order.front().cloned();
			if let Some(key) = oldest {
				if let Some((entries, truncated)) = self.dir_mut(&key) {
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
			if self.dir("").is_some() {
				if let Some((entries, _)) = self.dir("") {
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
		match self.errors.binary_search_by(|(key, _)| key.cmp(&dir)) {
			Ok(index) => self.errors[index] = (dir, err),
			Err(index) => self.errors.insert(index, (dir, err)),
		}
		self.enforce_budget();
	}

	/// Records an expanded directory. The key joins the back of the eviction
	/// order; collapsing or the byte cap drops the oldest keys first.
	pub fn note_expanded(&mut self, path: &str) {
		if let Err(index) =
			self.expanded.binary_search_by(|key| key.as_str().cmp(path))
		{
			self.expanded.insert(index, path.to_string());
		}
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
		self.dirs.retain(|(key, _)| !drop_dir(key));
		self.order.retain(|key| !drop_dir(key));
		self.errors.retain(|(key, _)| !drop_dir(key));
		self.error_order.retain(|key| !drop_dir(key));
		if path.is_empty() {
			self.expanded.retain(|have| !have.is_empty());
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
		self.dirs.retain(|(have, _)| have != &dir);
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
		self.put_dir(dir.clone(), (entries, truncated));
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
		if let Some(err) = self
			.errors
			.iter()
			.find_map(|(key, error)| (key == dir).then_some(error))
		{
			out.push(marker(depth, Msg::new("error_tree", [err.clone()])));
			return;
		}
		let Some((entries, truncated)) = self.dir(dir) else {
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
				e.kind == TreeKind::Tree && self.is_expanded(&e.path);
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

/// Commits one `git log` fetches; graph pages are sliced from this window,
/// so paging does not re-run the topological sort per page.
pub const HISTORY_WINDOW: usize = 500;

/// One graph walk: the refs and HEAD read when page 0 loaded (kept in the
/// model's `refs` / `head_sha`), what else that snapshot knew, and the
/// window of commits pages are sliced from. Later pages walk the snapshot's
/// tips, so refs moving meanwhile cannot duplicate or skip commits.
#[derive(Clone, Debug, Default)]
pub struct HistoryWalk {
	ref_filter: Option<String>,
	/// The ref filter's commit, resolved once for the whole walk.
	filter_tip: Option<String>,
	detached: bool,
	shallow: Vec<String>,
	start: usize,
	window: Vec<CommitSummary>,
	more: bool,
}

impl HistoryWalk {
	/// The page at `skip`, when the window holds all of it.
	fn page(
		&self,
		skip: usize,
		size: usize,
	) -> Option<(Vec<CommitSummary>, bool)> {
		let from = skip.checked_sub(self.start)?;
		let end = from.checked_add(size)?;
		let len = self.window.len();
		if end > len && self.more {
			return None;
		}
		let rows = self.window.get(from.min(len)..end.min(len))?.to_vec();
		Some((rows, end < len || self.more))
	}

	fn retained_bytes(&self) -> usize {
		let mut bytes = graph_view::vec_bytes(&self.window)
			.saturating_add(graph_view::vec_bytes(&self.shallow));
		for value in self
			.ref_filter
			.iter()
			.chain(self.filter_tip.iter())
			.chain(self.shallow.iter())
		{
			bytes = bytes.saturating_add(value.capacity());
		}
		for commit in &self.window {
			bytes = bytes.saturating_add(commit_bytes(commit));
		}
		bytes
	}
}

pub(crate) fn commit_bytes(commit: &CommitSummary) -> usize {
	let mut bytes = commit
		.sha
		.capacity()
		.saturating_add(commit.author_name.capacity())
		.saturating_add(commit.author_email.capacity())
		.saturating_add(commit.author_date.capacity())
		.saturating_add(commit.subject.capacity())
		.saturating_add(graph_view::vec_bytes(&commit.parents));
	for parent in &commit.parents {
		bytes = bytes.saturating_add(parent.capacity());
	}
	bytes
}

/// Reads one graph page. Page 0 (no `walk`) snapshots refs, HEAD and the
/// ref filter's commit; later pages reuse that snapshot, and need no Git at
/// all while the window already holds them.
#[allow(clippy::too_many_arguments)]
fn read_graph_page(
	host: &GitHost,
	repo_root: &std::path::Path,
	known: Option<&RepoIdentity>,
	walk: Option<(HistoryWalk, Vec<browser::GitReference>, Option<String>)>,
	ref_filter: Option<String>,
	skip: usize,
	size: usize,
	read: &Read,
) -> Result<(browser::RepositoryHistory, HistoryWalk), String> {
	let mut git: Option<Box<dyn RepoView>> = None;
	let open = || host.open(repo_root, known, read);
	let (mut walk, refs, head) = match walk {
		Some(reused) if reused.0.ref_filter == ref_filter => reused,
		_ => {
			let g = open()?;
			let snap = g.refs(read).map_err(|e| e.to_string())?;
			let filter_tip =
				match ref_filter.as_deref().filter(|r| !r.is_empty()) {
					Some(r) => Some(
						g.resolve_commit(r, read).map_err(|e| e.to_string())?,
					),
					None => None,
				};
			git = Some(g);
			let walk = HistoryWalk {
				ref_filter,
				filter_tip,
				detached: snap.detached,
				shallow: snap.shallow,
				more: true,
				..Default::default()
			};
			(walk, snap.refs, snap.head)
		}
	};
	if walk.page(skip, size).is_none() {
		let g = match git {
			Some(g) => g,
			None => open()?,
		};
		let tips = match &walk.filter_tip {
			Some(tip) => vec![tip.clone()],
			None => browser::RefSnapshot {
				refs: refs.clone(),
				head: head.clone(),
				..Default::default()
			}
			.tips(),
		};
		let mut start = skip / HISTORY_WINDOW * HISTORY_WINDOW;
		if skip.saturating_add(size) > start.saturating_add(HISTORY_WINDOW) {
			start = skip;
		}
		let (window, more) = g
			.log_from_tips(&tips, start, HISTORY_WINDOW.max(size), read)
			.map_err(|e| e.to_string())?;
		walk.start = start;
		walk.window = window;
		walk.more = more;
	}
	let (commits, has_more) = walk.page(skip, size).unwrap_or_default();
	Ok((
		browser::RepositoryHistory {
			root: String::new(),
			commits,
			refs,
			head,
			has_more,
		},
		walk,
	))
}

/// One path's preview. The change type comes from our own listing and a log
/// row carries the commit's full SHA and parents, so the preview neither
/// re-lists the source nor re-resolves the commit.
fn read_preview(
	g: &dyn RepoView,
	source: &GitSource,
	path: &str,
	change: Option<snip_core::format::ChangeType>,
	parents: Option<&[String]>,
	read: &Read,
) -> Result<browser::SourcePreview, String> {
	let preview = g
		.preview(source, path, change.map(|c| (c, parents)), read)
		.map_err(|e| e.to_string())?;
	Ok(browser::SourcePreview {
		content: preview.content,
		patch: preview.patch,
	})
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
	log_search: Option<LogQuery>,
	/// First and last page of the loaded window.
	first_page: usize,
	commit_page: usize,
	history_has_more: bool,
	/// Per commit: reachable from HEAD (IntelliJ tints those rows).
	on_head: Vec<bool>,
	/// The graph walk this page belongs to; `None` for search results.
	walk: Option<HistoryWalk>,
}

#[derive(Debug)]
enum GraphAdmissionError {
	Budget,
	Layout(String),
}

impl PreparedHistory {
	#[cfg(test)]
	fn prepare(
		history: browser::RepositoryHistory,
		page: usize,
		checkpoints: &[Option<GraphCheckpoint>],
		collapsed_merges: Vec<String>,
		active_ref_filter: Option<String>,
		log_search: Option<LogQuery>,
	) -> Result<Self, GraphAdmissionError> {
		Self::prepare_with(
			history,
			None,
			(page, page),
			checkpoints,
			collapsed_merges,
			active_ref_filter,
			log_search,
		)
	}

	fn prepare_with(
		history: browser::RepositoryHistory,
		walk: Option<HistoryWalk>,
		(first, page): (usize, usize),
		checkpoints: &[Option<GraphCheckpoint>],
		collapsed_merges: Vec<String>,
		active_ref_filter: Option<String>,
		log_search: Option<LogQuery>,
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
			first_page: first,
			commit_page: page,
			history_has_more: history.has_more,
			on_head: Vec::new(),
			walk,
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
		{
			// Every graph page leaves a checkpoint; none means the previous
			// page fell back to a plain list, so this one continues as one.
			let checkpoint = checkpoints.get(first).and_then(Option::as_ref);
			let (detached, shallow) = candidate
				.walk
				.as_ref()
				.map(|w| {
					(
						w.detached,
						w.shallow.iter().cloned().collect::<HashSet<String>>(),
					)
				})
				.unwrap_or_default();
			let layout_refs = graph_view::page_refs(
				&candidate.commits,
				&candidate.refs,
				candidate.head_sha.as_deref(),
				detached,
			);
			let layout = |commits: &[CommitSummary],
			              filtered: HashSet<String>| {
				graph_view::layout_page(
					commits,
					&layout_refs,
					candidate.head_sha.as_deref(),
					checkpoint,
					first > 0,
					filtered,
					shallow.clone(),
				)
				.map_err(GraphAdmissionError::Layout)
			};
			// Filtered results keep their graph, like IntelliJ: a parent the
			// filter left out ends in a dashed "skipped" stub. (Path filters
			// get rewritten parents from git, so their matches connect.) A
			// parent on a later page reconnects when that page joins the
			// window, since the whole window is laid out again.
			let skipped: HashSet<String> = if candidate.log_search.is_some() {
				let loaded: HashSet<&str> =
					candidate.commits.iter().map(|c| c.sha.as_str()).collect();
				candidate
					.commits
					.iter()
					.flat_map(|c| &c.parents)
					.filter(|p| !loaded.contains(p.as_str()))
					.cloned()
					.collect()
			} else {
				HashSet::new()
			};
			let full = layout(&candidate.commits, skipped.clone())?;
			if full.checkpoint.is_none()
				&& candidate.page_checkpoints.len() > page + 1
			{
				candidate.page_checkpoints[page + 1] = None;
			}
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
				layout(
					&shown,
					hidden.iter().chain(&skipped).cloned().collect(),
				)?
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
			.saturating_add(vec_bytes(&self.hidden_commits))
			.saturating_add(vec_bytes(&self.on_head));
		if let Some(q) = &self.log_search {
			bytes = bytes.saturating_add(vec_bytes(&q.paths));
		}
		for commit in &self.commits {
			bytes = bytes.saturating_add(commit_bytes(commit));
		}
		if let Some(walk) = &self.walk {
			bytes = bytes.saturating_add(walk.retained_bytes());
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
			.chain(self.log_search.iter().flat_map(|q| {
				[&q.text]
					.into_iter()
					.chain(&q.author)
					.chain(&q.since)
					.chain(&q.until)
					.chain(&q.paths)
			}))
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
		model.log_first_page = self.first_page;
		model.commit_page = self.commit_page;
		model.log_on_head = self.on_head;
		model.history_has_more = self.history_has_more;
		model.history_walk = self.walk;
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
		self.load_history_window(PageLoad::Replace(self.log_first_page), cx);
	}

	/// Reads one page and merges it into the loaded window (see [`PageLoad`]).
	fn load_history_window(&mut self, load: PageLoad, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		if let PageLoad::Replace(_) = load {
			if self.log_filter_pending() {
				// Reading every repo found so far would ignore the chip:
				// wait for discovery to reach the picked repositories.
				self.log_deferred = true;
				return;
			}
			self.log_deferred = false;
			let scope = self.log_scope();
			self.log_scope_key = scope.iter().map(|(r, _)| r.clone()).collect();
			self.log_feeds.clear();
			if scope.len() > 1 {
				self.start_merged_log(scope, cx);
				return;
			}
		} else if self.log_is_merged() {
			if load == PageLoad::Next {
				let target = self.commits.len() + self.history_page_size;
				self.merged_step(target, cx);
			}
			return;
		}
		let Some(repo_root) = self.log_root() else {
			return;
		};
		let page = match load {
			PageLoad::Replace(p) => p,
			PageLoad::Next => self.commit_page.saturating_add(1),
			PageLoad::Prev => match self.log_first_page.checked_sub(1) {
				Some(p) => p,
				None => return,
			},
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
		self.history_extending = !matches!(load, PageLoad::Replace(_));
		if !self.history_extending {
			self.history_error = None;
		}
		if matches!(load, PageLoad::Replace(0)) {
			self.history_loaded = false;
		}

		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();
		// A fresh page 0 takes a new snapshot; every other read continues
		// the snapshot's walk, so the window never mixes two ref states.
		let extending = !matches!(load, PageLoad::Replace(0));
		let walk = self
			.history_walk
			.clone()
			.filter(|_| extending && search.is_none())
			.map(|w| (w, self.refs.clone(), self.head_sha.clone()));
		let snapshot = (self.refs.clone(), self.head_sha.clone());
		let want_email = matches!(load, PageLoad::Replace(0));
		let repo_identity = self.identity_for(&repo_root);
		let host = self.git_host();

		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let res = bg
					.spawn(async move {
						let read = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel_bg),
						};
						let Some(query) = &search else {
							let email = want_email
								.then(|| {
									let g = host
										.open(
											&repo_root,
											repo_identity.as_ref(),
											&read,
										)
										.ok()?;
									g.user_email(&read)
								})
								.flatten();
							return read_graph_page(
								&host,
								&repo_root,
								repo_identity.as_ref(),
								walk,
								ref_filter,
								skip,
								page_size,
								&read,
							)
							.map(|(h, w)| (h, Some(w), email));
						};
						let g = host.open(
							&repo_root,
							repo_identity.as_ref(),
							&read,
						)?;
						// Refs only for labels: no topological walk for them.
						let (refs, head, email) = if extending {
							(snapshot.0, snapshot.1, None)
						} else {
							let snap =
								g.refs(&read).map_err(|e| e.to_string())?;
							(snap.refs, snap.head, g.user_email(&read))
						};
						let (commits, has_more) = g
							.history_query(
								ref_filter.as_deref(),
								query,
								skip,
								page_size,
								&read,
							)
							.map_err(|e| e.to_string())?;
						Ok((
							browser::RepositoryHistory {
								root: String::new(),
								commits,
								refs,
								head,
								has_more,
							},
							None,
							email,
						))
					})
					.await;

				let _ = this.update(&mut async_app, |model, cx| {
					if model.history_generation != task_generation {
						return;
					}
					model.history_extending = false;
					match res {
						Ok((hist, walk, email)) => {
							model.install_history(load, hist, walk, cx);
							if want_email {
								model.git_user_email = email;
							}
						}
						Err(e) => {
							model.history_autoload = false;
							model.report_graph_error(
								GraphAdmissionError::Layout(e),
							);
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// Merges a read page into the window, keeps the rows on screen where
	/// they were, and re-selects only after a fresh window.
	fn install_history(
		&mut self,
		load: PageLoad,
		hist: browser::RepositoryHistory,
		walk: Option<HistoryWalk>,
		cx: &mut Context<Self>,
	) {
		let (commits, first, last, has_more) = merge_window(
			&self.commits,
			self.log_first_page,
			self.commit_page,
			self.history_has_more,
			load,
			hist.commits,
			hist.has_more,
			self.history_page_size,
		);
		let seeds: Vec<String> = match load {
			PageLoad::Replace(_) => Vec::new(),
			_ => self
				.commits
				.iter()
				.zip(&self.log_on_head)
				.filter(|(_, on)| **on)
				.map(|(c, _)| c.sha.clone())
				.collect(),
		};
		// Scroll anchor: the top visible row and where it is drawn.
		let anchor = {
			let rows = self.display_commits();
			let offset = self.log_scroll.0.borrow().base_handle.offset();
			let top = (-f32::from(offset.y) / graph_view::ROW_HEIGHT).max(0.)
				as usize;
			rows.get(top).map(|c| (c.sha.clone(), top))
		};
		let merged = browser::RepositoryHistory {
			root: String::new(),
			commits,
			refs: hist.refs,
			head: hist.head,
			has_more,
		};
		let mut candidate = match PreparedHistory::prepare_with(
			merged,
			walk,
			(first, last),
			&self.page_checkpoints,
			self.collapsed_merges.clone(),
			self.active_ref_filter.clone(),
			self.log_search.clone(),
		) {
			Ok(candidate) => candidate,
			Err(error) => {
				self.history_autoload = false;
				self.report_graph_error(error);
				return;
			}
		};
		candidate.on_head = mark_on_head(
			&candidate.commits,
			candidate.head_sha.clone().into_iter().chain(seeds),
		);
		if let Err(error) = candidate.check_budget() {
			self.history_autoload = false;
			self.report_graph_error(error);
			return;
		}
		app_log!(
			"[APP:GRAPH_RETAINED: bytes={} limit={}]",
			candidate.retained_bytes(),
			MAX_RETAINED_GRAPH_BYTES
		);
		candidate.install(self);
		if matches!(load, PageLoad::Replace(_)) {
			self.history_loaded = true;
		}
		self.history_autoload = true;
		if let Some((sha, old)) =
			anchor.filter(|_| !matches!(load, PageLoad::Replace(_)))
		{
			match self.display_commits().iter().position(|c| c.sha == sha) {
				Some(new) => {
					let handle = self.log_scroll.0.borrow().base_handle.clone();
					let mut offset = handle.offset();
					offset.y -= gpui::px(
						(new as f32 - old as f32) * graph_view::ROW_HEIGHT,
					);
					handle.set_offset(offset);
					app_log!(
						"[APP:LOG_ANCHOR: row={} was={} offset={:.0}]",
						new,
						old,
						f32::from(offset.y)
					);
				}
				None => self.log_scroll.scroll_to_item(
					if load == PageLoad::Prev {
						0
					} else {
						self.display_commits().len().saturating_sub(1)
					},
					ScrollStrategy::Top,
				),
			}
		}
		let fallback =
			self.graph_layout.as_ref().is_some_and(|l| l.is_fallback);
		self.set_status(
			if fallback {
				app_log!("[APP:GRAPH_FALLBACK]");
				"status_graph_fallback"
			} else {
				"status_history_loaded"
			},
			[self.commits.len().to_string()],
		);
		app_log!("[APP:GRAPH_LOADED: commits={}]", self.commits.len());
		app_log!(
			"[APP:E2E_LOG: mode={} n={} first={} page={}]",
			if self.log_search.is_some() {
				"search"
			} else {
				"graph"
			},
			self.commits.len(),
			self.commits.first().map(|c| &c.sha[..7]).unwrap_or("-"),
			self.commit_page + 1
		);
		app_log!(
			"[APP:LOG_WINDOW: first_page={} last_page={} rows={}]",
			self.log_first_page + 1,
			self.commit_page + 1,
			self.commits.len()
		);
		if !matches!(load, PageLoad::Replace(_)) {
			return;
		}
		if let Some(anchor) = self.selected_commit.clone() {
			if self.reselect_after_load(&anchor, cx) {
				return;
			}
			if self.select_head_after_load {
				self.select_head_after_load = false;
				self.focus_head(cx);
			} else {
				self.selected_commit = None;
				self.log_selected.clear();
				self.commit_details = None;
			}
		} else if self.select_head_after_load {
			self.select_head_after_load = false;
			self.focus_head(cx);
		}
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
		match PreparedHistory::prepare_with(
			history,
			self.history_walk.clone(),
			(self.log_first_page, self.commit_page),
			&self.page_checkpoints,
			collapsed_merges,
			self.active_ref_filter.clone(),
			self.log_search.clone(),
		) {
			Ok(mut candidate) => {
				candidate.on_head = self.log_on_head.clone();
				candidate.install(self);
				app_log!("[APP:MERGE_COLLAPSE: sha={} collapsed={} hidden={} shown={}]",
                    &merge[..7.min(merge.len())], collapsed, side_only(&self.commits, &merge).len(), self.display_commits().len());
			}
			Err(error) => self.report_graph_error(error),
		}
		cx.notify();
	}

	/// Keymap alias of [`Self::history_next_page`] ("load more").
	#[allow(dead_code)] // bound by the chrome keymap at merge
	pub fn history_load_more(&mut self, cx: &mut Context<Self>) {
		self.history_next_page(cx);
	}

	/// Loads the page after the window (scrolling near the end does this).
	pub fn history_next_page(&mut self, cx: &mut Context<Self>) {
		if self.history_has_more && !self.history_extending {
			self.load_history_window(PageLoad::Next, cx);
		}
	}

	/// Loads the evicted page before the window (scrolling near the top).
	pub fn history_prev_page(&mut self, cx: &mut Context<Self>) {
		if self.log_first_page > 0 && !self.history_extending {
			self.load_history_window(PageLoad::Prev, cx);
		}
	}

	/// Called with the rows the log list is drawing: reads the neighbouring
	/// page once they come within a few rows of either end. After a failed
	/// read it waits for the next scroll gesture ([`Self::rearm_autoload`]).
	pub fn autoload_near(&mut self, rows: usize, cx: &mut Context<Self>) {
		const NEAR: usize = 5;
		if !self.history_autoload || self.history_extending || rows == 0 {
			return;
		}
		// From the scroll state, not the processor's range: the list also
		// renders row 0 alone to measure the row height.
		let (offset, height) = {
			let handle = &self.log_scroll.0.borrow().base_handle;
			(
				-f32::from(handle.offset().y),
				f32::from(handle.bounds().size.height),
			)
		};
		if height <= 0. {
			return;
		}
		let row = graph_view::ROW_HEIGHT;
		let start = (offset.max(0.) / row) as usize;
		let visible = start..start + (height / row).ceil() as usize;
		let load = if self.history_has_more && visible.end + NEAR >= rows {
			PageLoad::Next
		} else if self.log_first_page > 0 && visible.start <= NEAR {
			PageLoad::Prev
		} else {
			return;
		};
		// The list is being laid out: read on the next turn of the loop.
		self.history_extending = true;
		let this = cx.weak_entity();
		cx.defer(move |app| {
			let _ = this.update(app, |this, cx| {
				this.history_extending = false;
				this.load_history_window(load, cx);
			});
		});
	}

	/// A user scroll or key move re-enables loading after a failed read.
	pub fn rearm_autoload(&mut self) {
		self.history_autoload = true;
	}

	/// Changing query identity clears the old graph before the async read.
	/// Oversized metadata is refused without changing the previous query/page.
	fn reset_history_query(
		&mut self,
		reference: Option<String>,
		search: Option<LogQuery>,
	) -> bool {
		let empty = browser::RepositoryHistory {
			root: String::new(),
			commits: Vec::new(),
			refs: Vec::new(),
			head: None,
			has_more: false,
		};
		match PreparedHistory::prepare_with(
			empty,
			None,
			(0, 0),
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
		// The branch filter combines with the other filters, like IntelliJ.
		if !self.reset_history_query(ref_name, self.log_search.clone()) {
			cx.notify();
			return;
		}
		app_log!(
			"[APP:REF_FILTER: {}]",
			self.active_ref_filter.as_deref().unwrap_or("all")
		);
		self.load_history(cx);
	}

	/// The search field's text (message or hash) changed.
	pub fn start_log_search(&mut self, query: String, cx: &mut Context<Self>) {
		self.log_filter.text = query;
		self.apply_log_filter(cx);
	}

	/// Re-reads the log under `log_filter` (text, regex / case toggles,
	/// User, Date and Paths chips); an empty filter is the plain graph.
	pub fn apply_log_filter(&mut self, cx: &mut Context<Self>) {
		let query = self.log_filter.clone();
		let search = (!query.is_empty()).then_some(query);
		if !self.reset_history_query(self.active_ref_filter.clone(), search) {
			cx.notify();
			return;
		}
		app_log!(
			"[APP:LOG_SEARCH: active={} author={}]",
			self.log_search.is_some(),
			self.log_filter.author.is_some()
		);
		self.load_history(cx);
	}

	/// User chip: `None` shows every author.
	pub fn set_log_author(
		&mut self,
		author: Option<String>,
		cx: &mut Context<Self>,
	) {
		self.log_menu = None;
		self.log_filter.author = author
			.filter(|a| !a.is_empty())
			.map(|a| clip_utf8(a, MAX_USER_EMAIL));
		self.apply_log_filter(cx);
	}

	/// Date chip preset: a `git log --since` value, `None` for any date.
	pub fn set_log_since(
		&mut self,
		since: Option<&'static str>,
		cx: &mut Context<Self>,
	) {
		self.log_menu = None;
		self.log_date_error = false;
		self.log_filter.since = since.map(str::to_string);
		self.log_filter.until = None;
		self.apply_log_filter(cx);
	}

	/// Date chip custom range from its two fields (`YYYY-MM-DD`, either may
	/// be empty); both days are included. A malformed date is refused.
	pub fn apply_log_date_range(&mut self, cx: &mut Context<Self>) {
		let since = self.log_since_input.read(cx).text().trim().to_string();
		let until = self.log_until_input.read(cx).text().trim().to_string();
		match date_range(&since, &until) {
			Some((since, until)) => {
				self.log_menu = None;
				self.log_date_error = false;
				self.log_filter.since = since;
				self.log_filter.until = until;
				self.apply_log_filter(cx);
			}
			None => {
				self.log_date_error = true;
				app_log!("[APP:LOG_DATE_INVALID]");
				cx.notify();
			}
		}
	}

	/// Paths chip: adds a typed repository-relative path to the filter.
	pub fn add_log_path(&mut self, path: String, cx: &mut Context<Self>) {
		let path = clean_log_path(&path);
		if path.is_empty() || self.log_filter.paths.contains(&path) {
			return;
		}
		self.log_path_input.update(cx, |i, cx| i.set_text("", cx));
		self.toggle_log_path(path, cx);
	}

	/// Paths chip: checks or unchecks one path; the menu stays open.
	pub fn toggle_log_path(&mut self, path: String, cx: &mut Context<Self>) {
		let paths = &mut self.log_filter.paths;
		match paths.iter().position(|p| *p == path) {
			Some(i) => {
				paths.remove(i);
			}
			None if paths.len() < MAX_LOG_PATHS => paths.push(path),
			None => return,
		}
		app_log!("[APP:LOG_PATHS: n={}]", self.log_filter.paths.len());
		self.apply_log_filter(cx);
	}

	/// Paths picker: reads a folder (`""` is the root) the project tree has
	/// not read yet, through the project tree's own loader.
	pub fn load_picker_dir(&mut self, rel: &str, cx: &mut Context<Self>) {
		// The merged log names the (selected) repository first.
		let rel = if self.log_is_merged() {
			let Some(name) = self.repo().map(|r| r.name.clone()) else {
				return;
			};
			match rel.strip_prefix(name.as_str()) {
				Some("") => "",
				Some(rest) => match rest.strip_prefix('/') {
					Some(rest) => rest,
					None => return,
				},
				None => return,
			}
		} else {
			rel
		};
		let Some(tree) = self.file_tree.as_ref() else {
			return;
		};
		let mut node = tree;
		for part in rel.split('/').filter(|p| !p.is_empty()) {
			match node.children.iter().find(|c| c.name == part && c.is_dir) {
				Some(child) => node = child,
				None => return,
			}
		}
		if !node.is_loaded {
			self.dispatch_tree(
				Some(crate::tree::TreeCommand::Expand(
					crate::tree::NodeKey::from_utf8_rel(rel),
				)),
				cx,
			);
		}
	}

	/// Repo chip ✕ outside a repository scope: every path.
	pub fn clear_log_paths(&mut self, cx: &mut Context<Self>) {
		self.log_menu = None;
		self.log_filter.paths.clear();
		self.apply_log_filter(cx);
	}

	pub fn toggle_log_regex(&mut self, cx: &mut Context<Self>) {
		self.log_filter.regex = !self.log_filter.regex;
		app_log!("[APP:LOG_REGEX: {}]", self.log_filter.regex);
		self.reapply_text_toggle(cx);
	}

	pub fn toggle_log_match_case(&mut self, cx: &mut Context<Self>) {
		self.log_filter.match_case = !self.log_filter.match_case;
		app_log!("[APP:LOG_MATCH_CASE: {}]", self.log_filter.match_case);
		self.reapply_text_toggle(cx);
	}

	/// A text toggle only changes results while there is text.
	fn reapply_text_toggle(&mut self, cx: &mut Context<Self>) {
		if self.log_filter.text.trim().is_empty() {
			cx.notify();
		} else {
			self.apply_log_filter(cx);
		}
	}

	pub fn toggle_log_menu(
		&mut self,
		menu: crate::ui::LogMenu,
		cx: &mut Context<Self>,
	) {
		// The click that dismissed this menu (mouse down outside it) lands on
		// its own chip: that click closes, it does not reopen.
		// ponytail: time window, not hit testing; a >400ms press reopens.
		if let Some((m, at)) = self.log_menu_dismissed.take() {
			if m == menu && at.elapsed() < std::time::Duration::from_millis(400)
			{
				cx.notify();
				return;
			}
		}
		self.log_menu = (self.log_menu != Some(menu)).then_some(menu);
		if self.log_menu == Some(crate::ui::LogMenu::Repo) {
			self.pending_focus =
				Some(self.log_path_input.read(cx).handle().clone());
			self.load_picker_dir("", cx);
		}
		// IntelliJ's Branch popup opens with an empty field, sections shut.
		if self.log_menu == Some(crate::ui::LogMenu::Branch) {
			self.log_branch_menu_input
				.update(cx, |i, _| i.clear_retained());
			self.log_branch_menu_open.clear();
			self.pending_focus =
				Some(self.log_branch_menu_input.read(cx).handle().clone());
		}
		app_log!("[APP:LOG_MENU: {:?}]", self.log_menu);
		cx.notify();
	}

	/// Esc in a dropdown's field: the log keeps the keyboard, not the
	/// field that is no longer drawn.
	pub fn dismiss_log_menu(&mut self, cx: &mut Context<Self>) {
		self.close_log_menu(cx);
		self.pending_focus = Some(self.log_focus.clone());
	}

	pub fn close_log_menu(&mut self, cx: &mut Context<Self>) {
		if let Some(menu) = self.log_menu.take() {
			self.log_menu_dismissed = Some((menu, std::time::Instant::now()));
			cx.notify();
		}
	}

	/// Authors offered by the User chip: the loaded commits' authors, in
	/// order of appearance, bounded.
	pub fn log_authors(&self, max: usize) -> Vec<String> {
		let mut out: Vec<String> = Vec::new();
		for c in &self.commits {
			if out.len() >= max {
				break;
			}
			if !c.author_name.is_empty() && !out.contains(&c.author_name) {
				out.push(c.author_name.clone());
			}
		}
		out
	}

	/// Shows HEAD: back to the full graph on page 1, then selects HEAD.
	pub fn locate_head(&mut self, cx: &mut Context<Self>) {
		if self.log_is_merged() {
			self.locate_merged_head(cx);
			return;
		}
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
		self.log_filter = LogQuery::default();
		self.log_search_input.update(cx, |i, cx| i.set_text("", cx));
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

	/// Parents (plain SHAs) of a loaded commit, keyed by its row id.
	fn known_parents(&self, id: &str) -> Option<Vec<String>> {
		self.commits.iter().find(|c| c.sha == id).map(|c| {
			c.parents
				.iter()
				.map(|p| crate::multi_log::split_id(p).0.to_string())
				.collect()
		})
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
		self.log_selected.clear();
		self.reset_selection_details();
		self.compare = None;
		self.selected_file = None;
		self.selected_commit_file = None;
		self.commit_file_sel.clear();
		self.commit_files.clear();
		self.clear_preview();
		self.preview_loading = true;
		self.preview_error = None;
		let id = sha.to_string();
		let Some((root, sha)) = self.log_root_for(&id) else {
			return;
		};
		self.log_commit_root = Some(root.clone());
		app_log!(
			"[APP:LOG_SELECTION: n=1 repo={}]",
			self.log_repo_name(&root)
		);
		app_log!("[APP:COMMIT_SELECTED: {}]", &sha[..7.min(sha.len())]);
		let parents = self.known_parents(&id);
		self.load_commit_details(root.clone(), sha.clone(), id, cx);
		self.load_change_list(
			root,
			GitSource::Commit(sha.clone()),
			PreviewSource::CommitDiff { sha },
			parents,
			task_generation,
			cx,
		);
	}

	/// Full message, committer and containing branches for the details
	/// pane; its own generation, so opening a file does not drop it.
	fn load_commit_details(
		&mut self,
		root: std::path::PathBuf,
		sha: String,
		id: String,
		cx: &mut Context<Self>,
	) {
		self.details_generation = self.details_generation.wrapping_add(1);
		let generation = self.details_generation;
		if self.commit_details.as_ref().is_some_and(|d| d.sha != id) {
			self.commit_details = None;
		}
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.details_cancel);
		let identity = self.identity_for(&root);
		let host = self.git_host();
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
						let read = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel),
						};
						let g = host.open(&root, identity.as_ref(), &read)?;
						g.commit_details(&sha, &read).map_err(|e| e.to_string())
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.details_generation != generation {
						return;
					}
					// A failed read leaves the row's own fields on screen.
					// Details are keyed by the row id (merged log: with repo).
					model.commit_details = res.ok().map(|mut d| {
						app_log!(
							"[APP:COMMIT_DETAILS: {} branches={}]",
							&d.sha[..7.min(d.sha.len())],
							d.branches.len()
						);
						d.sha = id;
						d
					});
					cx.notify();
				});
			},
		);
	}

	/// A new selection starts with its commit list collapsed.
	fn reset_selection_details(&mut self) {
		self.log_selection_expanded = false;
		self.selection_details.clear();
		self.log_branches_all.clear();
	}

	/// Opens or closes the multi-selection's commit list; opening reads
	/// the first `MAX_SELECTION_DETAILS` commits' details in one job.
	pub fn toggle_selection_expanded(&mut self, cx: &mut Context<Self>) {
		self.log_selection_expanded = !self.log_selection_expanded;
		app_log!(
			"[APP:LOG_SELECTION_EXPANDED: {}]",
			self.log_selection_expanded
		);
		cx.notify();
		if !self.log_selection_expanded || !self.selection_details.is_empty() {
			return;
		}
		let reads: Vec<(PathBuf, Option<RepoIdentity>, String, String)> = self
			.log_selected
			.iter()
			.take(MAX_SELECTION_DETAILS)
			.filter_map(|id| {
				let (root, sha) = self.log_root_for(id)?;
				let identity = self.identity_for(&root);
				Some((root, identity, sha, id.clone()))
			})
			.collect();
		self.details_generation = self.details_generation.wrapping_add(1);
		let generation = self.details_generation;
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.details_cancel);
		let host = self.git_host();
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
						let read = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel),
						};
						// A failed read keeps that commit's row fields.
						reads
							.into_iter()
							.filter_map(|(root, identity, sha, id)| {
								let g = host
									.open(&root, identity.as_ref(), &read)
									.ok()?;
								let mut d =
									g.commit_details(&sha, &read).ok()?;
								d.sha = id;
								Some(d)
							})
							.collect::<Vec<_>>()
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.details_generation != generation {
						return;
					}
					app_log!("[APP:SELECTION_DETAILS: {}]", res.len());
					model.selection_details = res;
					cx.notify();
				});
			},
		);
	}

	/// Shift-selection: `selected_commit` stays the anchor.
	pub fn extend_range(&mut self, sha: &str, cx: &mut Context<Self>) {
		if sha.len() > MAX_SHA_LEN {
			self.report_graph_error(GraphAdmissionError::Budget);
			cx.notify();
			return;
		}
		let Some(anchor) = self.selected_commit.as_deref() else {
			self.select_commit(sha, cx);
			return;
		};
		if !same_repo(anchor, sha) {
			app_log!("[APP:RANGE_REFUSED: cross_repo]");
			self.set_status("status_log_cross_repo", []);
			cx.notify();
			return;
		}
		self.range_head = (self.selected_commit.as_deref() != Some(sha))
			.then(|| Box::<str>::from(sha).into_string());
		let (ids, chain_kind) = self.range_ids_with_kind();
		app_log!(
			"[APP:RANGE: commits={} chain={}]",
			ids.len().max(1),
			chain_kind.as_str()
		);
		if ids.len() > 1 {
			self.log_selected = ids;
			self.load_selection(cx);
		} else if let Some(anchor) = self.selected_commit.clone() {
			// Back to the anchor alone.
			if !self.log_selected.is_empty() {
				self.select_commit(&anchor, cx);
			}
		}
		cx.notify();
	}

	/// Cmd-click (macOS) / Ctrl-click: toggles one commit in or out of the
	/// selection, which may then have gaps. Another repository's commit is
	/// refused, like a shift range across repositories.
	pub fn toggle_commit(&mut self, sha: &str, cx: &mut Context<Self>) {
		if sha.len() > MAX_SHA_LEN {
			self.report_graph_error(GraphAdmissionError::Budget);
			cx.notify();
			return;
		}
		let Some(anchor) = self.selected_commit.clone() else {
			self.select_commit(sha, cx);
			return;
		};
		let current = if self.log_selected.is_empty() {
			vec![anchor.clone()]
		} else {
			self.log_selected.clone()
		};
		let toggled = {
			let rows = self.display_commits();
			let ids: Vec<&str> = rows.iter().map(|c| c.sha.as_str()).collect();
			toggle_selection(&ids, &current, sha)
		};
		let next = match toggled {
			Ok(next) => next,
			Err(_) => {
				app_log!("[APP:RANGE_REFUSED: cross_repo]");
				self.set_status("status_log_cross_repo", []);
				cx.notify();
				return;
			}
		};
		match next.len() {
			0 => {
				// The last selected commit toggled off: nothing selected.
				self.preview_generation += 1;
				self.details_generation =
					self.details_generation.wrapping_add(1);
				self.selected_commit = None;
				self.range_head = None;
				self.log_selected.clear();
				self.commit_details = None;
				self.reset_selection_details();
				self.commit_files.clear();
				self.selected_commit_file = None;
				self.commit_file_sel.clear();
				self.preview_loading = false;
				app_log!("[APP:LOG_SELECTION: n=0 repo=-]");
			}
			1 => self.select_commit(&next[0], cx),
			_ => {
				// The clicked commit leads when it joins; otherwise the
				// anchor stays unless it just left.
				let lead = if next.iter().any(|s| s == sha) {
					sha.to_string()
				} else if next.contains(&anchor) {
					anchor
				} else {
					next[0].clone()
				};
				self.selected_commit = Some(lead);
				self.range_head = None;
				self.log_selected = next;
				self.load_selection(cx);
			}
		}
		cx.notify();
	}

	/// After the log was read again: a multi-selection whose commits are
	/// all still loaded stays, else the anchor alone is selected again.
	/// False when the anchor is gone too.
	fn reselect_after_load(
		&mut self,
		anchor: &str,
		cx: &mut Context<Self>,
	) -> bool {
		let loaded = |id: &str| self.commits.iter().any(|c| c.sha == id);
		if self.log_selected.len() > 1
			&& self.log_selected.iter().all(|id| loaded(id))
		{
			self.load_selection(cx);
			true
		} else if loaded(anchor) {
			self.select_commit(anchor, cx);
			true
		} else {
			false
		}
	}

	/// The details of a multi-selection: the union of its commits' changed
	/// files (the newest change per path), read in one cancellable job of
	/// at most `MAX_SELECTION_READS` listings, then the first file's diff.
	pub fn load_selection(&mut self, cx: &mut Context<Self>) {
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.details_generation = self.details_generation.wrapping_add(1);
		self.commit_details = None;
		self.reset_selection_details();
		self.compare = None;
		self.selected_file = None;
		self.selected_commit_file = None;
		self.commit_file_sel.clear();
		self.commit_files.clear();
		self.commit_file_origin.clear();
		self.clear_preview();
		self.preview_loading = true;
		self.preview_error = None;
		let Some((root, _)) = self
			.log_selected
			.first()
			.and_then(|id| self.log_root_for(id))
		else {
			return;
		};
		self.log_commit_root = Some(root.clone());
		let n = self.log_selected.len();
		app_log!(
			"[APP:LOG_SELECTION: n={n} repo={}]",
			self.log_repo_name(&root)
		);
		if n > MAX_SELECTION_READS {
			self.set_status(
				"status_selection_truncated",
				[n.to_string(), MAX_SELECTION_READS.to_string()],
			);
		}
		let shas: Vec<String> = self
			.log_selected
			.iter()
			.take(MAX_SELECTION_READS)
			.map(|id| multi_log::split_id(id).0.to_string())
			.collect();
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.preview_cancel);
		let identity = self.identity_for(&root);
		let host = self.git_host();
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
						let listing = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel),
						};
						let g =
							host.open(&root, identity.as_ref(), &listing)?;
						// Lazy: each listing is folded in and dropped
						// before the next one is read.
						union_changed_files(shas.into_iter().map(|sha| {
							g.changed_paths(
								&GitSource::Commit(sha),
								MAX_COMMIT_FILES,
								&listing,
							)
							.map_err(|e| e.to_string())
						}))
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.preview_generation != task_generation {
						return;
					}
					match res {
						Ok((
							files,
							origin,
							total,
							gitlinks,
							total_is_lower_bound,
						)) => {
							app_log!("[APP:E2E_CHANGES: files={}]", total);
							if total > MAX_COMMIT_FILES {
								let key = if total_is_lower_bound {
									"status_commit_files_truncated_min"
								} else {
									"status_commit_files_truncated"
								};
								model.set_status(
									key,
									[
										total.to_string(),
										MAX_COMMIT_FILES.to_string(),
									],
								);
							}
							let first = files.first().map(|(p, _)| p.clone());
							model.commit_files = files;
							model.commit_file_origin = origin;
							model.commit_file_gitlinks = gitlinks;
							model.commit_files_truncated =
								total > MAX_COMMIT_FILES;
							match first {
								Some(path) => {
									model.select_commit_file(&path, cx)
								}
								None => model.show_preview_error(Msg::new(
									"status_no_changes",
									[],
								)),
							}
						}
						Err(e) => model
							.show_preview_error(Msg::new("error_history", [e])),
					}
					cx.notify();
				});
			},
		);
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

	/// Row ids the range selects and whether it forms a first-parent chain.
	/// A valid first-parent chain between the endpoints is selected in
	/// top-down display order; otherwise falls back to the visual range
	/// between them in the anchor's repository.
	pub fn range_ids_with_kind(&self) -> (Vec<String>, RangeChainKind) {
		let (Some(anchor), Some(head)) =
			(self.selected_commit.as_deref(), self.range_head.as_deref())
		else {
			return (Vec::new(), RangeChainKind::FirstParent);
		};
		let rows = self.display_commits();
		range_ids_with_kind(&rows, anchor, head)
	}

	/// Row ids the range selects: the first-parent chain between the endpoints
	/// when one exists, or all display rows between them belonging to the
	/// anchor's repository as a visual fallback.
	pub fn range_ids(&self) -> Vec<String> {
		self.range_ids_with_kind().0
	}

	/// The row is part of the log's selection.
	pub fn log_is_selected(&self, id: &str) -> bool {
		self.selected_commit.as_deref() == Some(id)
			|| self.log_selected.iter().any(|s| s == id)
	}

	/// Endpoint diff between the two ends of the range (older → newer).
	pub fn compare_range(&mut self, cx: &mut Context<Self>) {
		let Some((top, bottom)) = self.range_rows() else {
			return;
		};
		let rows = self.display_commits();
		let ends = self
			.log_root_for(&rows[top].sha)
			.zip(self.log_root_for(&rows[bottom].sha));
		drop(rows);
		let Some(((root, newer), (_, older))) = ends else {
			return;
		};
		let (newer, older) = (
			Box::<str>::from(newer).into_string(),
			Box::<str>::from(older).into_string(),
		);
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.compare = Some((older.clone(), newer.clone()));
		self.details_generation = self.details_generation.wrapping_add(1);
		self.commit_details = None;
		self.selected_commit_file = None;
		self.commit_file_sel.clear();
		self.commit_files.clear();
		self.clear_preview();
		self.preview_loading = true;
		self.preview_error = None;
		self.log_commit_root = Some(root.clone());
		app_log!("[APP:COMPARE: from={} to={}]", &older[..7], &newer[..7]);
		self.load_change_list(
			root,
			GitSource::Range(older.clone(), newer.clone()),
			PreviewSource::Compare {
				from: older,
				to: newer,
			},
			None,
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
		parents: Option<Vec<String>>,
		task_generation: u64,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.preview_cancel);
		let identity = self.identity_for(&root);
		let host = self.git_host();
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
						// The listing is metadata: a commit touching tens of
						// thousands of files lists under the interactive limit
						// and is cut to MAX_COMMIT_FILES afterwards.
						let listing_read = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel.clone()),
						};
						let preview_read = Read {
							profile: ReadProfile::PreviewStrict,
							cancel: Some(cancel),
						};
						// 有 identity 時不起 probe 程序；沒有則從列出的 root 開啟。
						let g =
							host.open(&root, identity.as_ref(), &listing_read)?;
						let list = g
							.changed_paths(
								&source,
								MAX_COMMIT_FILES,
								&listing_read,
							)
							.map_err(|e| e.to_string())?;
						let first = list.paths.first().map(|(p, change)| {
							(
								p.clone(),
								read_preview(
									g.as_ref(),
									&source,
									p,
									*change,
									parents.as_deref(),
									&preview_read,
								),
							)
						});
						Ok::<_, String>((
							list.paths,
							list.gitlinks,
							list.total,
							first,
						))
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.preview_generation != task_generation {
						return;
					}
					match res {
						Ok((mut files, gitlinks, total, first)) => {
							files.shrink_to_fit();
							app_log!("[APP:E2E_CHANGES: files={}]", total);
							if total > MAX_COMMIT_FILES {
								model.set_status(
									"status_commit_files_truncated",
									[
										total.to_string(),
										MAX_COMMIT_FILES.to_string(),
									],
								);
							}
							model.commit_files = files;
							model.commit_file_origin.clear();
							model.commit_file_gitlinks = gitlinks;
							model.commit_files_truncated =
								total > MAX_COMMIT_FILES;
							match first {
								Some((path, p)) => {
									model.selected_commit_file =
										Some(path.clone());
									model.commit_file_sel.clear();
									model.apply_source_preview(
										path,
										p.map(|p| (p, preview_source)),
									);
								}
								None => {
									model.show_preview_error(Msg::new(
										"status_no_changes",
										[],
									));
								}
							}
						}
						Err(e) => {
							model.show_preview_error(Msg::new(
								"error_history",
								[e],
							));
						}
					}
					cx.notify();
				});
			},
		);
	}

	/// Cmd/Ctrl-click in the changed files: adds or drops `path` from the
	/// selection; the open diff stays.
	pub fn toggle_commit_file(&mut self, path: &str, cx: &mut Context<Self>) {
		let sel = &mut self.commit_file_sel;
		if sel.is_empty() {
			sel.extend(self.selected_commit_file.clone());
		}
		match sel.iter().position(|p| p == path) {
			Some(i) => {
				sel.remove(i);
			}
			None => sel.push(path.to_string()),
		}
		cx.notify();
	}

	/// Shift-click in the changed files: selects the shown files from the
	/// open one to `path`.
	pub fn extend_commit_files(
		&mut self,
		shown: &[&str],
		path: &str,
		cx: &mut Context<Self>,
	) {
		let anchor = self.selected_commit_file.as_deref().unwrap_or(path);
		self.commit_file_sel = file_range(shown, anchor, path);
		cx.notify();
	}

	/// Opens one file of the selected commit or compare as a diff.
	pub fn select_commit_file(&mut self, path: &str, cx: &mut Context<Self>) {
		if !self.accepting_work() {
			return;
		}
		let (source, psource, root, parents) =
			match (&self.compare, &self.selected_commit) {
				(Some((a, b)), _) => (
					GitSource::Range(a.clone(), b.clone()),
					PreviewSource::Compare {
						from: a.clone(),
						to: b.clone(),
					},
					self.log_commit_root.clone(),
					None,
				),
				(None, _) if self.log_selected.len() > 1 => {
					match self.selection_file_source(path) {
						Some(s) => s,
						None => return,
					}
				}
				(None, Some(id)) => {
					let Some((root, sha)) = self.log_root_for(id) else {
						return;
					};
					(
						GitSource::Commit(sha.clone()),
						PreviewSource::CommitDiff { sha },
						Some(root),
						self.known_parents(id),
					)
				}
				_ => return,
			};
		let change = self
			.commit_files
			.iter()
			.find(|(p, _)| p == path)
			.and_then(|(_, c)| *c);
		let Some(root) = root else {
			return;
		};
		self.preview_generation += 1;
		let task_generation = self.preview_generation;
		self.selected_commit_file = Some(path.to_string());
		self.commit_file_sel.clear();
		self.preview_loading = true;
		let path = path.to_string();
		let for_bg = path.clone();
		let cancel = arm_cancel(&mut self.preview_cancel);
		let identity = self.identity_for(&root);
		let host = self.git_host();
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
						let read = Read {
							profile: ReadProfile::PreviewStrict,
							cancel: Some(cancel),
						};
						let g = host.open(&root, identity.as_ref(), &read)?;
						read_preview(
							g.as_ref(),
							&source,
							&for_bg,
							change,
							parents.as_deref(),
							&read,
						)
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

	/// A multi-selection's diff of `path`: the oldest selected commit's
	/// parent against the newest selected commit. A root commit has no
	/// parent to diff from, so then it is the file's diff in the newest
	/// selected commit that touched it.
	#[allow(clippy::type_complexity)]
	fn selection_file_source(
		&self,
		path: &str,
	) -> Option<(
		GitSource,
		PreviewSource,
		Option<std::path::PathBuf>,
		Option<Vec<String>>,
	)> {
		let newest = self.log_selected.first()?;
		let oldest = self.log_selected.last()?;
		let (root, to) = self.log_root_for(newest)?;
		if let Some(from) = self
			.known_parents(oldest)
			.and_then(|p| p.into_iter().next())
		{
			return Some((
				GitSource::Range(from.clone(), to.clone()),
				PreviewSource::Compare { from, to },
				Some(root),
				None,
			));
		}
		let idx = self.commit_files.iter().position(|(p, _)| p == path)?;
		let id = self
			.log_selected
			.get(*self.commit_file_origin.get(idx)? as usize)?;
		let (_, sha) = self.log_root_for(id)?;
		Some((
			GitSource::Commit(sha.clone()),
			PreviewSource::CommitDiff { sha },
			Some(root),
			self.known_parents(id),
		))
	}

	/// The repository and commit a changed-files row's file is read from:
	/// the commit that changed it last in a multi-selection, the newer end
	/// of a compare, else the selected commit.
	pub fn commit_file_rev(
		&self,
		path: &str,
	) -> Option<(std::path::PathBuf, String)> {
		if let Some((_, to)) = &self.compare {
			return Some((self.log_commit_root.clone()?, to.clone()));
		}
		if self.log_selected.len() > 1 {
			let idx = self.commit_files.iter().position(|(p, _)| p == path)?;
			let id = self
				.log_selected
				.get(*self.commit_file_origin.get(idx)? as usize)?;
			return self.log_root_for(id);
		}
		self.log_root_for(self.selected_commit.as_deref()?)
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
		let anchor = self.selected_commit.as_deref().filter(|_| extend);
		let next = match cur {
			Some(c) => step_row(&rows, c, delta, anchor),
			None => 0,
		};
		self.log_scroll.scroll_to_item(next, ScrollStrategy::Center);
		self.rearm_autoload();
		if extend && self.selected_commit.is_some() {
			self.extend_range(&rows[next], cx);
		} else {
			self.select_commit(&rows[next], cx);
		}
	}

	pub fn browse_commit_tree(&mut self, cx: &mut Context<Self>) {
		let Some((root, sha)) = self
			.selected_commit
			.as_deref()
			.and_then(|id| self.log_root_for(id))
		else {
			return;
		};
		// The commit tree lives in the Project tool window, which shows the
		// selected repository.
		if self.repo_root().as_ref() != Some(&root) {
			let name = self.log_repo_name(&root);
			app_log!("[APP:REV_TREE_REFUSED: other_repo]");
			self.set_status("status_log_tree_other_repo", [name]);
			cx.notify();
			return;
		}
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
		let host = self.git_host();
		let identity = self.identity_for(&root);

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
						let read = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel_bg),
						};
						let g = host.open(&root, identity.as_ref(), &read)?;
						g.commit_directory(
							&sha_bg,
							&dir_bg,
							MAX_REV_ROWS,
							&read,
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
			if tree.is_expanded(path) {
				tree.release_dir(path);
			} else {
				tree.note_expanded(path);
				app_log!("[APP:TREE_EXPANDED: {}]", path);
				if tree.dir(path).is_none() {
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
		let host = self.git_host();
		let identity = self.identity_for(&root);

		self.selected_file = None;
		self.selected_commit_file = Some(path.clone());
		self.commit_file_sel.clear();
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
						let read = Read {
							profile: ReadProfile::Preview,
							cancel: Some(cancel_bg),
						};
						let g = host.open(&root, identity.as_ref(), &read)?;
						g.commit_blob(
							&sha_bg,
							&path_bg,
							browser::MAX_BLOB_BYTES,
							&read,
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
						Ok(BlobText::Binary) => model.show_preview_error(
							Msg::new("error_binary", [path.clone()]),
						),
						Ok(BlobText::NotUtf8) => model.show_preview_error(
							Msg::new("error_not_utf8", [path.clone()]),
						),
						Ok(BlobText::TooLarge(n)) => {
							model.show_preview_error(Msg::new(
								"error_too_large",
								[path.clone(), n.to_string()],
							))
						}
						Err(e) => model.show_preview_error(Msg::new(
							"error_preview",
							[path.clone(), e],
						)),
					}
					cx.notify();
				});
			},
		);
	}
}

/// The row `delta` away from `cur`, clamped to the log. With an anchor
/// (a Shift move), rows of other repositories are stepped over: the merged
/// log interleaves them and a range stays in the anchor's repository.
pub fn step_row(
	rows: &[String],
	cur: usize,
	delta: isize,
	anchor: Option<&str>,
) -> usize {
	let last = rows.len() as isize - 1;
	let mut at = cur as isize;
	loop {
		let next = (at + delta).clamp(0, last);
		if next == at {
			return cur.min(rows.len() - 1);
		}
		at = next;
		if anchor.is_none_or(|a| same_repo(a, &rows[at as usize])) {
			return at as usize;
		}
	}
}

/// Both ids name commits of one repository (plain SHAs always do).
pub fn same_repo(a: &str, b: &str) -> bool {
	multi_log::split_id(a).1 == multi_log::split_id(b).1
}

/// The selection mechanism used to resolve a commit range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeChainKind {
	/// Endpoints are connected by a contiguous first-parent chain.
	FirstParent,
	/// Endpoints do not form a first-parent chain; fallback to all visible
	/// rows between them belonging to the same repository.
	Visual,
}

impl RangeChainKind {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::FirstParent => "first_parent",
			Self::Visual => "visual",
		}
	}
}

/// Shift range: the first-parent chain between `anchor` and `head` in
/// top-down display order (`[upper, ..., lower]`). Returns `None` when the
/// endpoints do not form a chain, so the caller falls back to visual.
pub fn first_parent_range(
	rows: &[&CommitSummary],
	anchor: &str,
	head: &str,
) -> Option<Vec<String>> {
	if !same_repo(anchor, head) {
		return None;
	}
	let pos_anchor = rows.iter().position(|c| c.sha == anchor)?;
	let pos_head = rows.iter().position(|c| c.sha == head)?;

	let (upper_pos, lower_pos) = if pos_anchor <= pos_head {
		(pos_anchor, pos_head)
	} else {
		(pos_head, pos_anchor)
	};

	let lower_sha = &rows[lower_pos].sha;
	let by_sha: HashMap<&str, (usize, &CommitSummary)> = rows
		.iter()
		.enumerate()
		.map(|(idx, &c)| (c.sha.as_str(), (idx, c)))
		.collect();

	let mut chain = Vec::new();
	let mut curr_pos = upper_pos;
	let mut curr = rows[upper_pos];
	chain.push(curr.sha.clone());

	while curr.sha != *lower_sha {
		let parent_sha = curr.parents.first()?;
		let &(p_pos, next_commit) = by_sha.get(parent_sha.as_str())?;
		if p_pos <= curr_pos || p_pos > lower_pos {
			return None;
		}
		chain.push(next_commit.sha.clone());
		curr_pos = p_pos;
		curr = next_commit;
	}
	Some(chain)
}

/// Row ids the range selects and whether it forms a first-parent chain.
/// A valid first-parent chain between the endpoints is selected in
/// top-down display order; otherwise falls back to the visual range
/// between them in the anchor's repository.
pub fn range_ids_with_kind(
	rows: &[&CommitSummary],
	anchor: &str,
	head: &str,
) -> (Vec<String>, RangeChainKind) {
	if let Some(chain) = first_parent_range(rows, anchor, head) {
		return (chain, RangeChainKind::FirstParent);
	}
	let ids: Vec<&str> = rows.iter().map(|c| c.sha.as_str()).collect();
	(range_between(&ids, anchor, head), RangeChainKind::Visual)
}

/// Shift range: the rows from `anchor` to `head` (display order, both
/// included) that belong to the anchor's repository.
pub fn range_between(rows: &[&str], anchor: &str, head: &str) -> Vec<String> {
	let pos = |id: &str| rows.iter().position(|r| *r == id);
	let (Some(a), Some(b)) = (pos(anchor), pos(head)) else {
		return Vec::new();
	};
	rows[a.min(b)..=a.max(b)]
		.iter()
		.filter(|r| same_repo(r, anchor))
		.map(|r| r.to_string())
		.collect()
}

/// Shift range over the shown changed files, both ends included; just
/// `head` when the anchor is not shown (e.g. in a collapsed folder).
pub fn file_range(shown: &[&str], anchor: &str, head: &str) -> Vec<String> {
	let pos = |p: &str| shown.iter().position(|s| *s == p);
	match (pos(anchor), pos(head)) {
		(Some(a), Some(b)) => shown[a.min(b)..=a.max(b)]
			.iter()
			.map(|s| s.to_string())
			.collect(),
		_ => vec![head.to_string()],
	}
}

/// Cmd/Ctrl-click: `selection` with `id` toggled in or out, in display
/// order. A selection stays in one repository, so an id of another one
/// is refused.
pub fn toggle_selection(
	rows: &[&str],
	selection: &[String],
	id: &str,
) -> Result<Vec<String>, &'static str> {
	if selection.iter().any(|s| !same_repo(s, id)) {
		return Err("cross_repo");
	}
	let mut out: Vec<String> =
		selection.iter().filter(|s| *s != id).cloned().collect();
	if out.len() == selection.len() {
		out.push(id.to_string());
	}
	let order: HashMap<&str, usize> =
		rows.iter().enumerate().map(|(i, r)| (*r, i)).collect();
	out.sort_by_key(|s| order.get(s.as_str()).copied().unwrap_or(usize::MAX));
	Ok(out)
}

/// A commit's changed paths and their change types.
pub type ChangedFiles = Vec<(String, Option<snip_core::format::ChangeType>)>;

/// Changed files of several commits (newest first) as one list, like
/// IntelliJ's multi-commit selection: each path once, with the change of
/// the newest commit touching it and that commit's index. At most
/// `MAX_COMMIT_FILES` are kept; the count of distinct paths comes next,
/// then the kept paths that are a submodule commit in that newest commit.
/// Each list comes with its gitlinks.
#[allow(clippy::type_complexity)]
pub fn union_changed_files<E>(
	lists: impl IntoIterator<Item = Result<ChangedPathList, E>>,
) -> Result<(ChangedFiles, Vec<u32>, usize, Vec<String>, bool), E> {
	let mut seen = HashSet::new();
	let mut files = Vec::new();
	let mut origin = Vec::new();
	let mut gitlinks = Vec::new();
	let mut total_is_lower_bound = false;
	let mut max_list_total: usize = 0;
	for (i, list) in lists.into_iter().enumerate() {
		let list = list?;
		if list.total > list.paths.len() {
			total_is_lower_bound = true;
		}
		max_list_total = max_list_total.max(list.total);
		for (path, change) in list.paths {
			if !seen.insert(path.clone()) {
				continue;
			}
			if files.len() < MAX_COMMIT_FILES {
				if list.gitlinks.contains(&path) {
					gitlinks.push(path.clone());
				}
				files.push((path, change));
				origin.push(i as u32);
			}
		}
	}
	let distinct_paths_seen = seen.len();
	let total = distinct_paths_seen.max(max_list_total);
	Ok((files, origin, total, gitlinks, total_is_lower_bound))
}

/// One read of a merged-log feed: its page (plain SHAs) and, on the
/// feed's first read, its refs and the tips later pages walk.
struct FeedRead {
	commits: Vec<CommitSummary>,
	more: bool,
	snapshot: Option<browser::RefSnapshot>,
	tips: Vec<String>,
	email: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn read_feed_page(
	host: &GitHost,
	root: &std::path::Path,
	known: Option<&RepoIdentity>,
	first: bool,
	tips: Vec<String>,
	ref_filter: Option<&str>,
	search: Option<&LogQuery>,
	skip: usize,
	want_email: bool,
	read: &Read,
) -> Result<FeedRead, String> {
	let g = host.open(root, known, read)?;
	let mut out = FeedRead {
		commits: Vec::new(),
		more: false,
		snapshot: None,
		tips,
		email: want_email.then(|| g.user_email(read)).flatten(),
	};
	if first {
		let snap = g.refs(read).map_err(|e| e.to_string())?;
		let tips = match ref_filter.filter(|r| !r.is_empty()) {
			// A branch filter picks that branch in every repository that
			// has it; the others show nothing.
			Some(r) => g.resolve_commit(r, read).ok().map(|t| vec![t]),
			None => Some(snap.tips()),
		};
		out.snapshot = Some(snap);
		match tips {
			Some(tips) => out.tips = tips,
			None => return Ok(out),
		}
	}
	let (commits, more) = match search {
		Some(q) => {
			g.history_query(ref_filter, q, skip, multi_log::FEED_PAGE, read)
		}
		None => g.log_from_tips(&out.tips, skip, multi_log::FEED_PAGE, read),
	}
	.map_err(|e| e.to_string())?;
	out.commits = commits;
	out.more = more;
	Ok(out)
}

/// The log over the workspace's repositories (IntelliJ's multi-root log).
impl WorkbenchModel {
	/// The repositories the log shows: the Repository chip's picks, else
	/// every repository of the workspace.
	pub fn log_scope(&self) -> Vec<(PathBuf, String)> {
		let all = self.repos.iter().map(|r| (r.root.clone(), r.name.clone()));
		let picked: Vec<_> = all
			.clone()
			.filter(|(root, _)| self.log_repo_filter.contains(root))
			.collect();
		if picked.is_empty() {
			all.collect()
		} else {
			picked
		}
	}

	/// The Repository chip picked repositories that discovery has not
	/// reached yet (a reopened workspace is still being scanned).
	fn log_filter_pending(&self) -> bool {
		self.is_loading
			&& !self.log_repo_filter.is_empty()
			&& !self
				.repos
				.iter()
				.any(|r| self.log_repo_filter.contains(&r.root))
	}

	/// The loaded log merges several repositories.
	pub fn log_is_merged(&self) -> bool {
		self.log_scope_key.len() > 1
	}

	/// The single-repository log's root.
	fn log_root(&self) -> Option<PathBuf> {
		match self.log_scope_key.as_slice() {
			[one] => Some(one.clone()),
			_ => None,
		}
	}

	/// The repository and plain SHA of a log row id.
	pub fn log_root_for(&self, id: &str) -> Option<(PathBuf, String)> {
		let (sha, feed) = multi_log::split_id(id);
		let root = match feed {
			Some(i) => self.log_feeds.get(i)?.root.clone(),
			None => self.log_root()?,
		};
		Some((root, sha.to_string()))
	}

	/// The identity discovery resolved for a listed root.
	fn identity_for(&self, root: &std::path::Path) -> Option<RepoIdentity> {
		self.repos
			.iter()
			.find(|r| r.root.as_path() == root)
			.and_then(|r| r.identity.clone())
	}

	pub fn log_repo_name(&self, root: &std::path::Path) -> String {
		// A remote root is an internal key, not a path to show.
		if let Some(session) =
			self.remote.session.as_ref().filter(|s| s.root == root)
		{
			return session.label();
		}
		self.repos
			.iter()
			.find(|r| r.root == root)
			.map(|r| r.name.clone())
			.unwrap_or_else(|| root.display().to_string())
	}

	/// A repository's root-stripe color: its place in the workspace, so
	/// it stays the same whatever the Repository chip shows.
	pub fn log_repo_color(&self, root: &std::path::Path) -> usize {
		self.repos.iter().position(|r| r.root == root).unwrap_or(0)
	}

	/// The merged log's repository of a row and its stripe color.
	pub fn log_row_repo(&self, id: &str) -> Option<(&multi_log::Feed, usize)> {
		let feed = self.log_feeds.get(multi_log::split_id(id).1?)?;
		Some((feed, self.log_repo_color(&feed.root)))
	}

	/// The checked-out branch of the repository a log row belongs to.
	pub fn log_current_branch(&self, id: &str) -> Option<String> {
		let (root, _) = self.log_root_for(id)?;
		self.repos
			.iter()
			.find(|r| r.root == root)?
			.summary
			.as_ref()
			.ok()?
			.branch
			.clone()
	}

	/// Whether selecting a repository resets and reloads the log. The log
	/// shows the workspace, not the selected repository: only a refresh or
	/// a changed set of repositories reloads it.
	pub fn log_reloads_on_repo_switch(&self, preserve_anchors: bool) -> bool {
		preserve_anchors
			|| self.log_scope_key.is_empty()
			|| !self
				.log_scope()
				.iter()
				.map(|(r, _)| r)
				.eq(&self.log_scope_key)
			|| (self.commits.is_empty()
				&& self.history_error.is_none()
				&& !self.history_extending)
	}

	/// A discovery that changed the workspace's repositories reloads a
	/// loaded log.
	pub fn sync_log_scope(&mut self, cx: &mut Context<Self>) {
		if self.log_deferred {
			if self.reset_history_query(
				self.active_ref_filter.clone(),
				self.log_search.clone(),
			) {
				self.load_history(cx);
			}
			return;
		}
		if self.log_scope_key.is_empty()
			|| self
				.log_scope()
				.iter()
				.map(|(r, _)| r)
				.eq(&self.log_scope_key)
		{
			return;
		}
		app_log!("[APP:LOG_SCOPE_CHANGED]");
		if self.reset_history_query(
			self.active_ref_filter.clone(),
			self.log_search.clone(),
		) {
			self.load_history(cx);
		}
	}

	/// After a repository switch that kept the log: it is loaded as it was.
	pub fn log_kept_on_repo_switch(&self) {
		if !self.history_extending {
			app_log!("[APP:GRAPH_LOADED: commits={}]", self.commits.len());
		}
	}

	/// Repository chip: checks or unchecks one repository (the last one
	/// stays). All checked is the whole workspace.
	pub fn toggle_log_repo(&mut self, root: PathBuf, cx: &mut Context<Self>) {
		let mut picked: Vec<PathBuf> =
			self.log_scope().into_iter().map(|(r, _)| r).collect();
		match picked.iter().position(|r| *r == root) {
			Some(_) if picked.len() == 1 => return,
			Some(i) => {
				picked.remove(i);
			}
			None => picked.push(root),
		}
		if picked.len() == self.repos.len() {
			picked.clear();
		}
		self.log_repo_filter = picked;
		self.apply_log_repo_filter(cx);
	}

	/// Repository chip: exactly these repositories (none: all of them).
	pub fn set_log_repos(
		&mut self,
		roots: Vec<PathBuf>,
		cx: &mut Context<Self>,
	) {
		self.log_menu = None;
		self.log_repo_filter = roots;
		self.apply_log_repo_filter(cx);
	}

	/// Paths are repository-relative in one repository and prefixed with
	/// the repository in the merged log, so a new scope starts without.
	fn apply_log_repo_filter(&mut self, cx: &mut Context<Self>) {
		app_log!("[APP:LOG_REPOS: n={}]", self.log_scope().len());
		self.log_filter.paths.clear();
		self.log_paths_expanded.clear();
		self.selected_commit = None;
		self.range_head = None;
		self.log_selected.clear();
		self.commit_details = None;
		self.apply_log_filter(cx);
	}

	/// What Copy Commits exports: the repository, its name, the tip and
	/// the selected commits as plain SHAs. A first-parent chain is one
	/// repository's, so a selection spanning two is refused.
	pub fn commit_copy_target(
		&self,
	) -> Result<(PathBuf, String, String, Vec<String>), &'static str> {
		let ids = match (self.range_rows(), &self.selected_commit) {
			// A Cmd/Ctrl-click selection may have gaps: core refuses it
			// as not contiguous.
			_ if self.log_selected.len() > 1 => self.log_selected.clone(),
			(Some(_), _) => self.range_ids(),
			(None, Some(sel))
				if self.display_commits().iter().any(|c| &c.sha == sel) =>
			{
				vec![sel.clone()]
			}
			_ => Vec::new(),
		};
		let tip = ids.first().ok_or("no_selection")?;
		if ids.iter().any(|id| !same_repo(id, tip)) {
			return Err("cross_repo");
		}
		let (root, tip_sha) = self.log_root_for(tip).ok_or("no_selection")?;
		let selected = ids
			.iter()
			.map(|id| multi_log::split_id(id).0.to_string())
			.collect();
		let name = self.log_repo_name(&root);
		Ok((root, name, tip_sha, selected))
	}

	fn start_merged_log(
		&mut self,
		scope: Vec<(PathBuf, String)>,
		cx: &mut Context<Self>,
	) {
		if !self.reset_history_query(
			self.active_ref_filter.clone(),
			self.log_search.clone(),
		) {
			cx.notify();
			return;
		}
		// Paths name their repository first; a repository with none of the
		// picked paths is left out, one picked whole keeps all its history.
		let paths = self
			.log_search
			.as_ref()
			.map(|q| q.paths.clone())
			.unwrap_or_default();
		self.log_feeds = scope
			.into_iter()
			.filter_map(|(root, name)| {
				let mut mine = Vec::new();
				let mut whole = paths.is_empty();
				for p in &paths {
					if *p == name {
						whole = true;
					} else if let Some(rel) = p
						.strip_prefix(name.as_str())
						.and_then(|r| r.strip_prefix('/'))
					{
						mine.push(rel.to_string());
					}
				}
				if whole {
					mine.clear();
				} else if mine.is_empty() {
					return None;
				}
				Some(multi_log::Feed::new(root, name, mine))
			})
			.collect();
		self.history_generation += 1;
		let _ = arm_cancel(&mut self.history_cancel);
		self.history_error = None;
		self.history_loaded = false;
		app_log!("[APP:MULTI_LOG: repos={}]", self.log_feeds.len());
		self.merged_step(self.history_page_size, cx);
	}

	/// Merges read commits up to `target` rows, reading the feed that gates
	/// the merge first (one read at a time).
	fn merged_step(&mut self, target: usize, cx: &mut Context<Self>) {
		let want = target
			.min(graph_view::MAX_LAYOUT_ROWS)
			.saturating_sub(self.commits.len());
		let order = multi_log::merge_order(&self.log_feeds, want);
		if order.len() < want {
			if let Some(i) = multi_log::next_read(&self.log_feeds) {
				self.read_feed(i, target, cx);
				return;
			}
		}
		self.install_merged(&order, cx);
		cx.notify();
	}

	fn read_feed(&mut self, i: usize, target: usize, cx: &mut Context<Self>) {
		let Some(feed) = self.log_feeds.get(i) else {
			return;
		};
		if !self.accepting_work() {
			self.history_extending = false;
			return;
		}
		let root = feed.root.clone();
		let first = !feed.loaded;
		let tips = feed.tips.clone();
		let skip = feed.skip;
		let search = self
			.log_search
			.clone()
			.map(|mut q| {
				q.paths = feed.paths.clone();
				q
			})
			.filter(|q| !q.is_empty());
		let ref_filter = self.active_ref_filter.clone();
		let want_email = first && i == 0;
		self.history_generation += 1;
		let generation = self.history_generation;
		let cancel = arm_cancel(&mut self.history_cancel);
		self.history_extending = true;
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let cancel_bg = cancel.clone();
		let host = self.git_host();
		let identity = self.identity_for(&root);
		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let res = bg
					.spawn(async move {
						let read = Read {
							profile: ReadProfile::Interactive,
							cancel: Some(cancel_bg),
						};
						read_feed_page(
							&host,
							&root,
							identity.as_ref(),
							first,
							tips,
							ref_filter.as_deref(),
							search.as_ref(),
							skip,
							want_email,
							&read,
						)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.history_generation != generation {
						return;
					}
					model.history_extending = false;
					let error = model.apply_feed_read(i, res);
					match error {
						Some(e) if model.log_feeds.iter().all(|f| f.failed) => {
							model.history_autoload = false;
							model.report_graph_error(
								GraphAdmissionError::Layout(e),
							);
							cx.notify();
						}
						_ => model.merged_step(target, cx),
					}
				});
			},
		);
	}

	/// Takes a feed read in, namespacing its ids; a failed read ends that
	/// feed only. Returns the error.
	fn apply_feed_read(
		&mut self,
		i: usize,
		res: Result<FeedRead, String>,
	) -> Option<String> {
		let feed = self.log_feeds.get_mut(i)?;
		let read = match res {
			Ok(read) => read,
			Err(e) => {
				app_log!("[APP:MULTI_LOG_FEED_ERROR: {}]", feed.name);
				feed.failed = true;
				return Some(e);
			}
		};
		let ns = |sha: &str| multi_log::ns_id(sha, i);
		if let Some(snap) = read.snapshot {
			feed.head = snap.head.as_deref().map(ns);
			if let Some(head) = feed.head.clone().filter(|_| snap.detached) {
				self.refs.push(browser::GitReference {
					name: "HEAD".into(),
					sha: head,
				});
			}
			self.refs.extend(snap.refs.into_iter().map(|r| {
				browser::GitReference {
					sha: ns(&r.sha),
					name: r.name,
				}
			}));
		}
		feed.tips = read.tips;
		let commits = read
			.commits
			.into_iter()
			.map(|mut c| {
				c.sha = ns(&c.sha);
				for p in &mut c.parents {
					*p = ns(p);
				}
				c
			})
			.collect();
		feed.push_page(commits, read.more);
		if read.email.is_some() {
			self.git_user_email = read.email;
		}
		None
	}

	/// Appends the merged rows `order` names and lays the whole list out
	/// again, under the same budget as a single repository's window.
	fn install_merged(&mut self, order: &[usize], cx: &mut Context<Self>) {
		let first = self.commits.is_empty();
		let mut taken = vec![0usize; self.log_feeds.len()];
		let mut commits = self.commits.clone();
		for &i in order {
			commits.push(self.log_feeds[i].pending[taken[i]].clone());
			taken[i] += 1;
		}
		let more = self
			.log_feeds
			.iter()
			.zip(&taken)
			.any(|(f, n)| f.pending.len() > *n || f.can_read());
		// ponytail: the merged log stops at one layout's rows (no page
		// eviction across repositories); the Repository chip pages deeper.
		let capped = more && commits.len() >= graph_view::MAX_LAYOUT_ROWS;
		let heads: Vec<String> = self
			.log_feeds
			.iter()
			.filter_map(|f| f.head.clone())
			.collect();
		let feeds_bytes: usize = self
			.log_feeds
			.iter()
			.map(multi_log::Feed::retained_bytes)
			.sum();
		let history = browser::RepositoryHistory {
			root: String::new(),
			commits,
			refs: self.refs.clone(),
			head: None,
			has_more: more && !capped,
		};
		let prepared = PreparedHistory::prepare_with(
			history,
			None,
			(0, 0),
			&[],
			self.collapsed_merges.clone(),
			self.active_ref_filter.clone(),
			self.log_search.clone(),
		)
		.and_then(|mut c| {
			c.on_head = mark_on_head(&c.commits, heads);
			if c.retained_bytes().saturating_add(feeds_bytes)
				> MAX_RETAINED_GRAPH_BYTES
			{
				Err(GraphAdmissionError::Budget)
			} else {
				Ok(c)
			}
		});
		let candidate = match prepared {
			Ok(candidate) => candidate,
			Err(error) => {
				self.history_autoload = false;
				self.report_graph_error(error);
				return;
			}
		};
		app_log!(
			"[APP:GRAPH_RETAINED: bytes={} limit={}]",
			candidate.retained_bytes().saturating_add(feeds_bytes),
			MAX_RETAINED_GRAPH_BYTES
		);
		candidate.install(self);
		self.history_loaded = true;
		for (feed, n) in self.log_feeds.iter_mut().zip(taken) {
			feed.pending.drain(..n);
		}
		self.history_autoload = true;
		let n = self.commits.len();
		if capped {
			app_log!("[APP:MULTI_LOG_CAPPED: rows={n}]");
			self.set_status("status_log_merged_cap", [n.to_string()]);
		} else {
			self.set_status("status_history_loaded", [n.to_string()]);
		}
		app_log!(
			"[APP:MULTI_LOG_LOADED: repos={} rows={n}]",
			self.log_feeds.len()
		);
		app_log!("[APP:GRAPH_LOADED: commits={n}]");
		app_log!(
			"[APP:E2E_LOG: mode={} n={n} first={} page=1]",
			if self.log_search.is_some() {
				"search"
			} else {
				"graph"
			},
			self.commits.first().map(|c| &c.sha[..7]).unwrap_or("-"),
		);
		if first {
			self.select_head_after_load = false;
			if let Some(anchor) = self.selected_commit.clone() {
				if !self.reselect_after_load(&anchor, cx) {
					self.selected_commit = None;
					self.log_selected.clear();
					self.commit_details = None;
				}
			}
		}
	}

	/// Go to HEAD in the merged log: the selected repository's, else the
	/// first one's.
	fn locate_merged_head(&mut self, cx: &mut Context<Self>) {
		let selected = self.repo_root();
		let head = self
			.log_feeds
			.iter()
			.find(|f| Some(&f.root) == selected.as_ref())
			.or(self.log_feeds.first())
			.and_then(|f| f.head.clone());
		let Some(head) = head else {
			return;
		};
		if let Some(ix) =
			self.display_commits().iter().position(|c| c.sha == head)
		{
			self.log_scroll.scroll_to_item(ix, ScrollStrategy::Center);
			app_log!("[APP:HEAD_LOCATED: row={}]", ix);
			self.select_commit(&head, cx);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use snip_core::gitsrc::Git;
	use std::process::Command;

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
	fn rev_tree_vec_tables_count_capacity_after_churn() {
		let mut tree = RevTree::new("tip".into());
		let before = tree.retained_bytes();
		let (dirs, expanded, errors) = (
			tree.dirs.capacity(),
			tree.expanded.capacity(),
			tree.errors.capacity(),
		);
		tree.dirs.reserve_exact(13);
		tree.expanded.reserve_exact(17);
		tree.errors.reserve_exact(19);
		assert_eq!(
			tree.retained_bytes() - before,
			(tree.dirs.capacity() - dirs)
				* std::mem::size_of::<(String, (Vec<TreeEntry>, bool))>()
				+ (tree.expanded.capacity() - expanded)
					* std::mem::size_of::<String>()
				+ (tree.errors.capacity() - errors)
					* std::mem::size_of::<(String, String)>()
		);
		for i in 0..100 {
			tree.insert_dir(format!("dir-{i}"), Vec::new(), false);
		}
		let retained: Vec<_> =
			tree.dirs.iter().map(|(key, _)| key.clone()).collect();
		for path in retained {
			tree.release_dir(&path);
		}
		assert!(tree.dirs.is_empty() && tree.errors.is_empty());
		assert_eq!(tree.dirs.capacity(), 0);
		assert_eq!(tree.errors.capacity(), 0);
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
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
			Some(LogQuery {
				text: oversized_short_string(),
				..Default::default()
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
	fn thousands_of_refs_keep_the_page_and_the_selector_complete() {
		let previous = first_page();
		let mut many = history(vec![c("c1", &["c0"])]);
		// 5000 refs elsewhere in the repository plus 1001 on this page.
		many.refs = (0..6001)
			.map(|n| browser::GitReference {
				name: format!("refs/heads/b{n}"),
				sha: if n < 1001 {
					"c1".into()
				} else {
					format!("{n:040x}")
				},
			})
			.collect();
		let page = PreparedHistory::prepare(
			many,
			1,
			&previous.page_checkpoints,
			Vec::new(),
			None,
			None,
		)
		.unwrap();
		assert_eq!(page.refs.len(), 6001, "the ref selector keeps every ref");
		let row = &page.graph_layout.as_ref().unwrap().rows[0];
		assert_eq!(row.refs.len(), graph_view::MAX_LAYOUT_REFS);
	}

	#[test]
	fn too_many_rails_fall_back_to_a_list_and_later_pages_still_load() {
		// 300 branch tips whose parents are all on later pages.
		let tips: Vec<CommitSummary> = (0..300)
			.map(|n| c(&format!("t{n}"), &[&format!("p{n}")]))
			.collect();
		let page0 = PreparedHistory::prepare(
			history(tips),
			0,
			&[],
			Vec::new(),
			None,
			None,
		)
		.unwrap();
		assert!(page0.graph_layout.as_ref().unwrap().is_fallback);
		assert_eq!(page0.page_checkpoints.get(1).cloned().flatten(), None);
		let page1 = PreparedHistory::prepare(
			history(vec![c("p0", &[])]),
			1,
			&page0.page_checkpoints,
			Vec::new(),
			None,
			None,
		)
		.unwrap();
		assert!(page1.graph_layout.as_ref().unwrap().is_fallback);
		assert_eq!(page1.commits.len(), 1);
	}

	#[test]
	fn detached_head_gets_a_badge_and_shallow_cut_is_not_a_root() {
		let mut page = history(vec![c("h", &["b"]), c("b", &[])]);
		page.head = Some("h".into());
		let walk = HistoryWalk {
			detached: true,
			shallow: vec!["b".into()],
			..Default::default()
		};
		let prepared = PreparedHistory::prepare_with(
			page,
			Some(walk),
			(0, 0),
			&[],
			Vec::new(),
			None,
			None,
		)
		.unwrap();
		let layout = prepared.graph_layout.as_ref().unwrap();
		assert!(layout.rows[0]
			.refs
			.iter()
			.any(|r| r.kind == snip_core::graph::RefKind::Head));
		assert_eq!(
			layout.rows[1].node.node_type,
			snip_core::graph::NodeType::ShallowRoot
		);
		assert!(prepared.refs.is_empty(), "HEAD is a badge, not a ref");
	}

	#[test]
	fn history_walk_slices_pages_and_knows_when_to_fetch() {
		let walk = HistoryWalk {
			start: 100,
			window: (0..120).map(|n| c(&n.to_string(), &[])).collect(),
			more: true,
			..Default::default()
		};
		let (rows, more) = walk.page(150, 50).unwrap();
		assert_eq!((rows[0].sha.as_str(), rows.len(), more), ("50", 50, true));
		assert!(walk.page(200, 50).is_none(), "runs past the window");
		assert!(walk.page(50, 50).is_none(), "before the window");
		let last = HistoryWalk {
			more: false,
			..walk
		};
		let (rows, more) = last.page(200, 50).unwrap();
		assert_eq!((rows.len(), more), (20, false));
		assert_eq!(last.page(250, 50).unwrap().0.len(), 0);
	}

	#[test]
	fn later_graph_pages_come_from_the_page_zero_window() {
		use std::process::Command;
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let git = |args: &[&str]| {
			let out = Command::new("git")
				.current_dir(root)
				.args(args)
				.output()
				.unwrap();
			assert!(out.status.success(), "{args:?}");
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};
		git(&["init", "-q", "-b", "main"]);
		for n in 0..6 {
			git(&[
				"-c",
				"user.name=A",
				"-c",
				"user.email=a@x",
				"commit",
				"-q",
				"--allow-empty",
				"-m",
				&format!("c{n}"),
			]);
		}
		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let host = crate::githost::GitHost::Local;
		let (first, walk) =
			read_graph_page(&host, root, None, None, None, 0, 2, &read)
				.unwrap();
		assert!(first.has_more);
		// The repository moves on; later pages are sliced from the window
		// page 0 fetched (frozen tips on refetch are covered in browser.rs).
		git(&[
			"-c",
			"user.name=A",
			"-c",
			"user.email=a@x",
			"commit",
			"-q",
			"--allow-empty",
			"-m",
			"new",
		]);
		let reuse = Some((walk, first.refs.clone(), first.head.clone()));
		let (second, walk) =
			read_graph_page(&host, root, None, reuse, None, 2, 2, &read)
				.unwrap();
		let reuse = Some((walk, second.refs.clone(), second.head.clone()));
		let (third, _) =
			read_graph_page(&host, root, None, reuse, None, 4, 2, &read)
				.unwrap();
		assert!(!third.has_more);
		let seen: Vec<String> = [first, second, third]
			.into_iter()
			.flat_map(|h| h.commits.into_iter().map(|c| c.subject))
			.collect();
		assert_eq!(seen, ["c5", "c4", "c3", "c2", "c1", "c0"]);
	}

	#[test]
	fn a_later_graph_page_in_the_window_runs_no_git() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		let git = |args: &[&str]| {
			let out = Command::new("git")
				.current_dir(root)
				.args(args)
				.output()
				.unwrap();
			assert!(out.status.success(), "{args:?}");
			String::from_utf8(out.stdout).unwrap().trim().to_string()
		};
		git(&["init", "-q", "-b", "main"]);
		for n in 0..6 {
			git(&[
				"-c",
				"user.name=A",
				"-c",
				"user.email=a@x",
				"commit",
				"-q",
				"--allow-empty",
				"-m",
				&format!("c{n}"),
			]);
		}
		let read = Read {
			profile: ReadProfile::Interactive,
			cancel: None,
		};
		let host = crate::githost::GitHost::Local;
		let (first, walk) =
			read_graph_page(&host, root, None, None, None, 0, 2, &read)
				.unwrap();
		assert!(first.has_more);
		let reuse = Some((walk, first.refs.clone(), first.head.clone()));
		let nonexistent = std::path::Path::new("/nonexistent/graph/page/test");
		let before_flight = snip_core::gitrun::in_flight();
		let before_queued = snip_core::gitrun::queued();
		let (second, _) =
			read_graph_page(&host, nonexistent, None, reuse, None, 2, 2, &read)
				.unwrap();
		assert_eq!(second.commits.len(), 2);
		assert_eq!(snip_core::gitrun::in_flight(), before_flight);
		assert_eq!(snip_core::gitrun::queued(), before_queued);
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

	/// 130 commits, a merge every 10 whose side parent is 5 back.
	fn branchy(len: usize) -> Vec<CommitSummary> {
		(0..len)
			.rev()
			.map(|n| {
				let parents: Vec<String> = match n {
					0 => vec![],
					n if n % 10 == 0 && n >= 5 => vec![n - 1, n - 5],
					n => vec![n - 1],
				}
				.into_iter()
				.map(|p| format!("{p:040x}"))
				.collect();
				let mut commit = c(&format!("{n:040x}"), &[]);
				commit.parents = parents;
				commit
			})
			.collect()
	}

	/// (sha, lane, colour) of every row of a page's layout.
	fn nodes(page: &PreparedHistory) -> Vec<(String, usize, usize)> {
		page.graph_layout
			.as_ref()
			.unwrap()
			.rows
			.iter()
			.map(|r| (r.sha.clone(), r.node.lane, r.node.color_index))
			.collect()
	}

	#[test]
	fn a_window_of_pages_lays_out_like_the_pages_one_by_one() {
		let all = branchy(130);
		let size = 50;
		let page = |p: usize| {
			let end = ((p + 1) * size).min(all.len());
			browser::RepositoryHistory {
				root: String::new(),
				commits: all[p * size..end].to_vec(),
				refs: Vec::new(),
				head: None,
				has_more: end < all.len(),
			}
		};
		// Page by page, as the old pager did.
		let mut single = Vec::new();
		let mut checkpoints: Vec<Option<GraphCheckpoint>> = Vec::new();
		for p in 0..3 {
			let prepared = PreparedHistory::prepare(
				page(p),
				p,
				&checkpoints,
				Vec::new(),
				None,
				None,
			)
			.unwrap();
			single.extend(nodes(&prepared));
			checkpoints = prepared.page_checkpoints;
		}
		// Growing window, then evicting the first page and reading it back.
		let mut window =
			PreparedHistory::prepare(page(0), 0, &[], Vec::new(), None, None)
				.unwrap();
		for load in [PageLoad::Next, PageLoad::Next] {
			let next = page(window.commit_page + 1);
			let (commits, first, last, more) = merge_window(
				&window.commits,
				window.first_page,
				window.commit_page,
				window.history_has_more,
				load,
				next.commits,
				next.has_more,
				size,
			);
			window = PreparedHistory::prepare_with(
				browser::RepositoryHistory {
					commits,
					has_more: more,
					..page(0)
				},
				None,
				(first, last),
				&window.page_checkpoints,
				Vec::new(),
				None,
				None,
			)
			.unwrap();
		}
		assert_eq!((window.first_page, window.commit_page), (0, 2));
		assert_eq!(nodes(&window), single);
		assert_eq!(window.page_checkpoints, checkpoints);
		let tail = PreparedHistory::prepare_with(
			browser::RepositoryHistory {
				commits: all[size..].to_vec(),
				has_more: false,
				..page(0)
			},
			None,
			(1, 2),
			&window.page_checkpoints,
			Vec::new(),
			None,
			None,
		)
		.unwrap();
		assert_eq!(nodes(&tail), single[size..].to_vec());
	}

	#[test]
	fn filtered_results_keep_a_graph_with_skipped_stubs() {
		// Matches: m3 (parent m2, a match) and m2 (parent x, filtered out).
		let page = PreparedHistory::prepare(
			history(vec![c("m3", &["m2"]), c("m2", &["x"]), c("m1", &[])]),
			0,
			&[],
			Vec::new(),
			None,
			Some(LogQuery {
				author: Some("me".into()),
				..Default::default()
			}),
		)
		.unwrap();
		let rows = &page.graph_layout.as_ref().expect("a graph").rows;
		assert_eq!(rows.len(), 3);
		let edge = |row: usize| rows[row].parent_edges[0].clone();
		assert_eq!(edge(0).to_row, Some(1), "adjacent matches connect");
		assert_eq!(
			edge(1).continuation,
			snip_core::graph::ContinuationKind::FilteredGap
		);
		assert!(edge(1).to_row.is_none());
	}

	#[test]
	fn the_window_evicts_the_far_end_when_full() {
		let size = 2;
		let rows = |from: usize, n: usize| -> Vec<CommitSummary> {
			(from..from + n).map(|i| c(&i.to_string(), &[])).collect()
		};
		let last = MAX_WINDOW_PAGES - 1;
		let full = rows(0, MAX_WINDOW_PAGES * size);
		let (out, first, l, more) = merge_window(
			&full,
			0,
			last,
			true,
			PageLoad::Next,
			rows(full.len(), 1),
			false,
			size,
		);
		assert_eq!((first, l, more), (1, last + 1, false));
		assert_eq!(out.len(), full.len() - size + 1);
		assert_eq!(out[0].sha, "2");
		let (out, first, l, more) = merge_window(
			&out,
			1,
			last + 1,
			false,
			PageLoad::Prev,
			rows(0, size),
			true,
			size,
		);
		assert_eq!((first, l, more), (0, last, true));
		assert_eq!(out.len(), MAX_WINDOW_PAGES * size);
		assert_eq!(out[0].sha, "0");
		let (out, first, l, _) = merge_window(
			&out,
			0,
			last,
			true,
			PageLoad::Replace(4),
			rows(8, size),
			true,
			size,
		);
		assert_eq!((out.len(), first, l), (size, 4, 4));
	}

	#[test]
	fn head_marks_follow_parents_across_the_window() {
		let commits = vec![
			c("side", &["b"]),
			c("head", &["b"]),
			c("b", &["a"]),
			c("a", &[]),
		];
		assert_eq!(
			mark_on_head(&commits, ["head".to_string()]),
			[false, true, true, true]
		);
		assert_eq!(
			mark_on_head(&commits[2..], ["b".to_string()]),
			[true, true]
		);
		assert!(mark_on_head(&commits, []).iter().all(|on| !on));
	}

	#[test]
	fn date_range_covers_whole_days_and_refuses_bad_input() {
		assert_eq!(
			date_range("2026-01-02", "2026-01-31"),
			Some((
				Some("2026-01-02 00:00:00".into()),
				Some("2026-01-31 23:59:59".into())
			))
		);
		assert_eq!(
			date_range("", "2026-01-31"),
			Some((None, Some("2026-01-31 23:59:59".into())))
		);
		assert_eq!(date_range("", ""), Some((None, None)));
		assert_eq!(date_range("2026-02-30", ""), None);
		assert_eq!(date_range("yesterday", ""), None);
		assert_eq!(date_range("2026-02-01", "2026-01-01"), None);
		assert_eq!(clean_log_path(" /src\\app/ "), "src/app");
	}

	#[test]
	fn log_selection_toggle_range_and_repos() {
		// The merged log: repo 0 and repo 1 interleaved.
		let rows = ["a3@0", "b2@1", "a2@0", "b1@1", "a1@0"];
		let ids =
			|v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
		// Shift range keeps the anchor's repository only, either direction.
		assert_eq!(
			range_between(&rows, "a3@0", "a1@0"),
			ids(&["a3@0", "a2@0", "a1@0"])
		);
		assert_eq!(
			range_between(&rows, "a1@0", "a3@0"),
			ids(&["a3@0", "a2@0", "a1@0"])
		);
		assert!(range_between(&rows, "a1@0", "gone@0").is_empty());
		// Shift+Down/Up step over the other repository's rows.
		let owned = ids(&rows);
		assert_eq!(step_row(&owned, 0, 1, Some("a3@0")), 2);
		assert_eq!(step_row(&owned, 2, 1, Some("a3@0")), 4);
		assert_eq!(step_row(&owned, 4, 1, Some("a3@0")), 4);
		assert_eq!(step_row(&owned, 4, -1, Some("a3@0")), 2);
		assert_eq!(step_row(&owned, 1, 1, Some("b2@1")), 3);
		assert_eq!(step_row(&owned, 3, 1, Some("b2@1")), 3);
		// A plain move walks every row.
		assert_eq!(step_row(&owned, 0, 1, None), 1);
		assert_eq!(step_row(&owned, 0, -1, None), 0);
		// Toggle in (display order, with a gap), toggle out.
		let sel = toggle_selection(&rows, &ids(&["a1@0"]), "a3@0").unwrap();
		assert_eq!(sel, ids(&["a3@0", "a1@0"]));
		let sel = toggle_selection(&rows, &sel, "a2@0").unwrap();
		assert_eq!(sel, ids(&["a3@0", "a2@0", "a1@0"]));
		let sel = toggle_selection(&rows, &sel, "a3@0").unwrap();
		assert_eq!(sel, ids(&["a2@0", "a1@0"]));
		assert!(toggle_selection(&rows, &ids(&["a1@0"]), "a1@0")
			.unwrap()
			.is_empty());
		// Another repository's commit is refused, the selection unchanged.
		assert_eq!(toggle_selection(&rows, &sel, "b2@1"), Err("cross_repo"));
		// A single repository's plain SHAs are one repository.
		assert!(toggle_selection(&["x", "y"], &ids(&["x"]), "y").is_ok());
	}

	#[test]
	fn first_parent_range_and_fallback() {
		// Display order (newest first): C4, C3, SIDE, C2, C1, base.
		// C3 is a merge commit: first parent C2, second parent SIDE.
		let commits = [
			c("C4", &["C3"]),
			c("C3", &["C2", "SIDE"]),
			c("SIDE", &["C2"]),
			c("C2", &["C1"]),
			c("C1", &["base"]),
			c("base", &[]),
		];
		let rows: Vec<&CommitSummary> = commits.iter().collect();

		// C1 -> C3 and C3 -> C1 both give [C3, C2, C1] in top-down display order.
		assert_eq!(
			first_parent_range(&rows, "C1", "C3"),
			Some(vec!["C3".into(), "C2".into(), "C1".into()])
		);
		assert_eq!(
			first_parent_range(&rows, "C3", "C1"),
			Some(vec!["C3".into(), "C2".into(), "C1".into()])
		);

		// Sibling pair: SIDE and C3.
		// C3's first parent is C2, so C3 -> SIDE is not a first-parent chain.
		assert_eq!(first_parent_range(&rows, "C3", "SIDE"), None);
		assert_eq!(first_parent_range(&rows, "SIDE", "C3"), None);
		assert_eq!(
			range_ids_with_kind(&rows, "C3", "SIDE").1,
			RangeChainKind::Visual
		);
		assert_eq!(
			range_ids_with_kind(&rows, "SIDE", "C3").1,
			RangeChainKind::Visual
		);

		// Visual fallback between C3 and SIDE contains the rows between them.
		let ids: Vec<&str> = rows.iter().map(|c| c.sha.as_str()).collect();
		assert_eq!(range_between(&ids, "C3", "SIDE"), vec!["C3", "SIDE"]);

		// Mirror real fixture where `side` branches from `base` (docs/real-ui-operator-fixture.sh ~137):
		// SIDE as upper endpoint with its first parent (base) below the lower endpoint -> visual.
		let real_fixture_commits = [
			c("C4", &["C3"]),
			c("C3", &["C2", "SIDE"]),
			c("SIDE", &["base"]),
			c("C2", &["C1"]),
			c("C1", &["base"]),
			c("base", &[]),
		];
		let real_rows: Vec<&CommitSummary> =
			real_fixture_commits.iter().collect();
		assert_eq!(first_parent_range(&real_rows, "SIDE", "C2"), None);
		assert_eq!(
			range_ids_with_kind(&real_rows, "SIDE", "C2").1,
			RangeChainKind::Visual
		);
		assert_eq!(
			range_ids_with_kind(&real_rows, "C2", "SIDE").1,
			RangeChainKind::Visual
		);
		assert_eq!(first_parent_range(&real_rows, "SIDE", "C1"), None);
		assert_eq!(
			range_ids_with_kind(&real_rows, "SIDE", "C1").1,
			RangeChainKind::Visual
		);
		assert_eq!(
			range_ids_with_kind(&real_rows, "C1", "SIDE").1,
			RangeChainKind::Visual
		);

		// Multi-repo namespaced ids ("sha@0" style) interleaved with repo 1 rows
		// work and stay in the anchor repo.
		let multi_commits = vec![
			c("C4@0", &["C3@0"]),
			c("r1@1", &[]),
			c("C3@0", &["C2@0", "SIDE@0"]),
			c("r2@1", &["r1@1"]),
			c("SIDE@0", &["C2@0"]),
			c("C2@0", &["C1@0"]),
			c("r3@1", &["r2@1"]),
			c("C1@0", &["base@0"]),
			c("base@0", &[]),
		];
		let multi_rows: Vec<&CommitSummary> = multi_commits.iter().collect();

		assert_eq!(
			first_parent_range(&multi_rows, "C1@0", "C3@0"),
			Some(vec!["C3@0".into(), "C2@0".into(), "C1@0".into()])
		);
		assert_eq!(first_parent_range(&multi_rows, "C3@0", "SIDE@0"), None);

		// Unloaded parent: parent id not in rows (e.g. C2 is missing).
		let missing_parent_commits = [
			c("C4", &["C3"]),
			c("C3", &["C2", "SIDE"]),
			c("SIDE", &["C2"]),
			c("C1", &["base"]),
			c("base", &[]),
		];
		let missing_rows: Vec<&CommitSummary> =
			missing_parent_commits.iter().collect();
		assert_eq!(first_parent_range(&missing_rows, "C1", "C3"), None);
	}

	#[test]
	fn file_range_spans_shown_files_either_way() {
		let shown = ["a@1/x", "b", "c", "d"];
		assert_eq!(file_range(&shown, "c", "a@1/x"), ["a@1/x", "b", "c"]);
		assert_eq!(file_range(&shown, "b", "d"), ["b", "c", "d"]);
		assert_eq!(file_range(&shown, "hidden", "b"), ["b"]);
	}

	#[test]
	fn union_of_selected_commits_keeps_the_newest_change() {
		use snip_core::format::ChangeType::{Deleted, Modified, New};
		let list = |v: &[(&str, snip_core::format::ChangeType)]| {
			v.iter()
				.map(|(p, c)| (p.to_string(), Some(*c)))
				.collect::<Vec<_>>()
		};
		// Newest first: a.txt deleted in the newest, added in the oldest.
		// A submodule path is a gitlink as its newest commit lists it.
		let links = |v: &[&str]| v.iter().map(|p| p.to_string()).collect();
		let (files, origin, total, gitlinks, total_is_lower_bound) =
			union_changed_files(
				[
					ChangedPathList {
						paths: list(&[("a.txt", Deleted), ("b.txt", Modified)]),
						gitlinks: links(&["b.txt"]),
						total: 2,
					},
					ChangedPathList {
						paths: list(&[("c.txt", New)]),
						gitlinks: links(&[]),
						total: 1,
					},
					ChangedPathList {
						paths: list(&[("a.txt", New), ("c.txt", Modified)]),
						gitlinks: links(&["a.txt", "c.txt"]),
						total: 2,
					},
				]
				.map(Ok::<_, ()>),
			)
			.unwrap();
		assert_eq!(
			files,
			list(&[("a.txt", Deleted), ("b.txt", Modified), ("c.txt", New)])
		);
		assert_eq!(origin, [0, 0, 1]);
		assert_eq!(total, 3);
		assert_eq!(gitlinks, ["b.txt"]);
		assert!(!total_is_lower_bound);
		// Distinct paths past the cap are counted, not kept.
		let many: Vec<_> = (0..MAX_COMMIT_FILES + 2)
			.map(|i| (format!("f{i}"), Some(Modified)))
			.collect();
		let (files, origin, total, _, bound) =
			union_changed_files([many.clone(), many].map(|l| {
				Ok::<_, ()>(ChangedPathList {
					total: l.len(),
					paths: l,
					gitlinks: Vec::new(),
				})
			}))
			.unwrap();
		assert_eq!(files.len(), MAX_COMMIT_FILES);
		assert_eq!(origin.len(), MAX_COMMIT_FILES);
		assert_eq!(total, MAX_COMMIT_FILES + 2);
		assert!(!bound);

		// Truncated list (total > paths.len()) sets the lower-bound flag and total >= that list's total
		let (files, _, total, _, bound) = union_changed_files(
			[
				ChangedPathList {
					paths: list(&[("x.txt", New)]),
					gitlinks: links(&[]),
					total: 100,
				},
				ChangedPathList {
					paths: list(&[("y.txt", New)]),
					gitlinks: links(&[]),
					total: 1,
				},
			]
			.map(Ok::<_, ()>),
		)
		.unwrap();
		assert_eq!(files.len(), 2);
		assert!(bound);
		assert!(total >= 100);
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
		t.put_dir(String::new(), (entries, false));
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
			.iter()
			.flat_map(|(_, (entries, _))| entries)
			.all(|entry| entry.path.len() < MAX_RETAINED_TREE_BYTES));
		let child = "c".repeat(MAX_RETAINED_TREE_BYTES + 100);
		tree.insert_dir(child, vec![huge], false);
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
		assert!(tree
			.dirs
			.iter()
			.map(|(key, _)| key)
			.all(|key| key.len() <= MAX_RETAINED_TREE_BYTES));
		tree.note_error("err".into(), "e".repeat(MAX_RETAINED_TREE_BYTES * 4));
		assert!(tree.retained_bytes() <= MAX_RETAINED_TREE_BYTES);
		assert!(tree
			.errors
			.iter()
			.map(|(_, err)| err)
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
			tree.note_expanded(&format!("exp-{i}-{}", "k".repeat(2_000)));
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
			t.dir("").is_some(),
			"small root stays while non-root listings fit"
		);
		assert!(
			t.dir("dir_0").is_none(),
			"oldest non-root listing is evicted first"
		);
		assert!(
			t.dir(&format!("dir_{}", MAX_CACHED_DIRS + 9)).is_some(),
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
					tree.dir("").is_some(),
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
