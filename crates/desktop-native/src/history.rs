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
/// Commit message bytes kept for the details pane.
pub const MAX_DETAILS_MESSAGE: usize = 16 * 1024;
/// Branches listed as containing the selected commit; more are counted.
pub const MAX_CONTAINING_BRANCHES: usize = 20;
/// Longest `user.email` kept to mark the user's own commits.
const MAX_USER_EMAIL: usize = 256;

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

/// What the details pane shows beyond the log row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitDetails {
	pub sha: String,
	pub parents: Vec<String>,
	pub message: String,
	pub author: String,
	pub author_email: String,
	pub author_date: String,
	pub committer: String,
	pub committer_email: String,
	pub commit_date: String,
	pub branches: Vec<String>,
	/// More branches contain the commit than `branches` lists.
	pub branches_more: bool,
}

fn clip_utf8(mut s: String, max: usize) -> String {
	if s.len() > max {
		let mut end = max;
		while !s.is_char_boundary(end) {
			end -= 1;
		}
		s.truncate(end);
		s.push('…');
	}
	s.into_boxed_str().into_string()
}

fn read_commit_details(
	git: &Git,
	sha: &str,
	opts: &RunOptions,
) -> Result<CommitDetails, String> {
	let out = git
		.run_with(
			&[
				"show",
				"-s",
				"--no-show-signature",
				"--encoding=UTF-8",
				"--format=%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B",
				sha,
				"--",
			],
			opts,
		)
		.map_err(|e| e.to_string())?;
	let text = String::from_utf8_lossy(&out.stdout);
	let mut f = text.splitn(8, '\0');
	let parents: Vec<String> = f
		.next()
		.unwrap_or_default()
		.split_whitespace()
		.take(64)
		.map(str::to_string)
		.collect();
	let author = f.next().unwrap_or_default().to_string();
	let author_email = f.next().unwrap_or_default().to_string();
	let author_date = f.next().unwrap_or_default().to_string();
	let committer = f.next().unwrap_or_default().to_string();
	let committer_email = f.next().unwrap_or_default().to_string();
	let commit_date = f.next().unwrap_or_default().to_string();
	let message = f.next().unwrap_or_default().trim().to_string();
	// Best effort: a failure here only hides the branch list.
	let contains = git
		.run_with(
			&[
				"for-each-ref",
				&format!("--count={}", MAX_CONTAINING_BRANCHES + 1),
				"--contains",
				sha,
				"--format=%(refname:short)",
				"refs/heads",
				"refs/remotes",
			],
			opts,
		)
		.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
		.unwrap_or_default();
	let mut branches: Vec<String> = contains
		.lines()
		.filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
		.map(|l| clip_utf8(l.to_string(), 200))
		.collect();
	let branches_more = branches.len() > MAX_CONTAINING_BRANCHES;
	branches.truncate(MAX_CONTAINING_BRANCHES);
	Ok(CommitDetails {
		sha: sha.to_string(),
		parents,
		message: clip_utf8(message, MAX_DETAILS_MESSAGE),
		author: clip_utf8(author, 200),
		author_email: clip_utf8(author_email, 200),
		author_date: clip_utf8(author_date, 64),
		committer: clip_utf8(committer, 200),
		committer_email: clip_utf8(committer_email, 200),
		commit_date: clip_utf8(commit_date, 64),
		branches,
		branches_more,
	})
}

/// `user.email`, when set, to mark the user's own commits.
fn read_user_email(git: &Git, opts: &RunOptions) -> Option<String> {
	let out = git
		.run_with(&["config", "--get", "user.email"], opts)
		.ok()?;
	let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
	(!email.is_empty() && email.len() <= MAX_USER_EMAIL).then_some(email)
}

/// One row of the details pane's changed-files tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangedRow {
	/// A directory (full repository-relative path) and its file count.
	Dir {
		path: String,
		files: usize,
		expanded: bool,
	},
	/// Index into the commit's file list; `nested` under a directory row.
	File { idx: usize, nested: bool },
}

/// IntelliJ's "group by directory": one row per directory holding changed
/// files, its files under it; files at the root come last.
pub fn changed_file_rows(
	files: &[(String, Option<snip_core::format::ChangeType>)],
	collapsed: &[String],
) -> Vec<ChangedRow> {
	let mut dirs: std::collections::BTreeMap<&str, Vec<usize>> =
		Default::default();
	let mut root = Vec::new();
	for (idx, (path, _)) in files.iter().enumerate() {
		match path.rsplit_once('/') {
			Some((dir, _)) => dirs.entry(dir).or_default().push(idx),
			None => root.push(idx),
		}
	}
	let mut out = Vec::with_capacity(files.len() + dirs.len());
	for (dir, idxs) in dirs {
		let expanded = !collapsed.iter().any(|c| c == dir);
		out.push(ChangedRow::Dir {
			path: dir.to_string(),
			files: idxs.len(),
			expanded,
		});
		if expanded {
			out.extend(
				idxs.into_iter()
					.map(|idx| ChangedRow::File { idx, nested: true }),
			);
		}
	}
	out.extend(
		root.into_iter()
			.map(|idx| ChangedRow::File { idx, nested: false }),
	);
	out
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

fn commit_bytes(commit: &CommitSummary) -> usize {
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
fn read_graph_page(
	repo_root: &std::path::Path,
	walk: Option<(HistoryWalk, Vec<browser::GitReference>, Option<String>)>,
	ref_filter: Option<String>,
	skip: usize,
	size: usize,
	opts: &RunOptions,
) -> Result<(browser::RepositoryHistory, HistoryWalk), String> {
	let mut git = None;
	let open = || -> Result<Git, String> {
		Git::open_with(repo_root, opts).map_err(|e| e.to_string())
	};
	let (mut walk, refs, head) = match walk {
		Some(reused) if reused.0.ref_filter == ref_filter => reused,
		_ => {
			let g = open()?;
			let snap =
				browser::refs_with(&g, opts).map_err(|e| e.to_string())?;
			let filter_tip =
				match ref_filter.as_deref().filter(|r| !r.is_empty()) {
					Some(r) => Some(
						g.resolve_commit_with(r, opts)
							.map_err(|e| e.to_string())?,
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
		let git = match git {
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
		let (window, more) = browser::log_from_tips_with(
			&git,
			&tips,
			start,
			HISTORY_WINDOW.max(size),
			opts,
		)
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
	git: &Git,
	source: &GitSource,
	path: &str,
	change: Option<snip_core::format::ChangeType>,
	parents: Option<&[String]>,
	opts: &RunOptions,
) -> Result<browser::SourcePreview, String> {
	let preview = match change {
		Some(change) => {
			browser::git_preview_for(git, source, path, change, parents, opts)
		}
		None => browser::git_preview_with(git, source, path, opts),
	};
	preview
		.map(|p| browser::SourcePreview {
			content: p.content,
			patch: p.patch,
		})
		.map_err(|e| e.to_string())
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
		if candidate.log_search.is_none() {
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
			let full = layout(&candidate.commits, HashSet::new())?;
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
				layout(&shown, hidden.clone())?
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
		let Some(repo_root) = self.repo_root() else {
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

		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(cancel),
			async move {
				let res = bg
					.spawn(async move {
						let opts = crate::interactive_read_opts(cancel_bg);
						let Some(query) = &search else {
							let email = want_email
								.then(|| {
									let git =
										Git::at_known_root(repo_root.clone());
									read_user_email(&git, &opts)
								})
								.flatten();
							return read_graph_page(
								&repo_root, walk, ref_filter, skip, page_size,
								&opts,
							)
							.map(|(h, w)| (h, Some(w), email));
						};
						let git = Git::open_with(&repo_root, &opts)
							.map_err(|e| e.to_string())?;
						// Refs only for labels: no topological walk for them.
						let (refs, head, email) = if extending {
							(snapshot.0, snapshot.1, None)
						} else {
							let snap = browser::refs_with(&git, &opts)
								.map_err(|e| e.to_string())?;
							(snap.refs, snap.head, read_user_email(&git, &opts))
						};
						let (commits, has_more) = browser::history_query_with(
							&git,
							ref_filter.as_deref(),
							query,
							skip,
							page_size,
							&opts,
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
			if self.commits.iter().any(|c| c.sha == anchor) {
				self.select_commit(&anchor, cx);
			} else if self.select_head_after_load {
				self.select_head_after_load = false;
				self.focus_head(cx);
			} else {
				self.selected_commit = None;
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

	/// Date chip: a `git log --since` value, `None` for any date.
	pub fn set_log_since(
		&mut self,
		since: Option<&'static str>,
		cx: &mut Context<Self>,
	) {
		self.log_menu = None;
		self.log_filter.since = since.map(str::to_string);
		self.apply_log_filter(cx);
	}

	/// Paths chip: one repository-relative path; empty clears it.
	pub fn set_log_paths(&mut self, path: String, cx: &mut Context<Self>) {
		self.log_menu = None;
		let path = path.trim().trim_matches('/').replace('\\', "/");
		self.log_filter.paths = if path.is_empty() {
			Vec::new()
		} else {
			vec![path]
		};
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
		self.log_menu = (self.log_menu != Some(menu)).then_some(menu);
		app_log!("[APP:LOG_MENU: {:?}]", self.log_menu);
		cx.notify();
	}

	pub fn close_log_menu(&mut self, cx: &mut Context<Self>) {
		if self.log_menu.take().is_some() {
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

	/// Parents of a loaded commit, keyed by its full SHA.
	fn known_parents(&self, sha: &str) -> Option<Vec<String>> {
		self.commits
			.iter()
			.find(|c| c.sha == sha)
			.map(|c| c.parents.clone())
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
		let parents = self.known_parents(&sha);
		self.load_commit_details(root.clone(), sha.clone(), cx);
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
		cx: &mut Context<Self>,
	) {
		self.details_generation = self.details_generation.wrapping_add(1);
		let generation = self.details_generation;
		if self.commit_details.as_ref().is_some_and(|d| d.sha != sha) {
			self.commit_details = None;
		}
		if !self.accepting_work() {
			return;
		}
		let cancel = arm_cancel(&mut self.details_cancel);
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
						let opts = crate::interactive_read_opts(cancel);
						read_commit_details(
							&Git::at_known_root(root),
							&sha,
							&opts,
						)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.details_generation != generation {
						return;
					}
					// A failed read leaves the row's own fields on screen.
					model.commit_details = res.ok();
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
		self.details_generation = self.details_generation.wrapping_add(1);
		self.commit_details = None;
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
						let listing =
							crate::interactive_read_opts(cancel.clone());
						let opts = RunOptions {
							cancel: Some(cancel),
							max_stdout: browser::PREVIEW_LIMIT,
							overflow: snip_core::gitrun::Overflow::Error,
							..RunOptions::preview(None)
						};
						// The repository's root is known: no probe processes.
						let git = Git::at_known_root(root);
						let files = gitsrc::list_changed_paths_with(
							&git, &source, &listing,
						)
						.map_err(|e| e.to_string())?;
						let first = files.first().map(|(p, change)| {
							(
								p.clone(),
								read_preview(
									&git,
									&source,
									p,
									*change,
									parents.as_deref(),
									&opts,
								),
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
		let change = self
			.commit_files
			.iter()
			.find(|(p, _)| p == path)
			.and_then(|(_, c)| *c);
		let parents = match &source {
			GitSource::Commit(sha) => self.known_parents(sha),
			_ => None,
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
						let git = Git::at_known_root(root);
						read_preview(
							&git,
							&source,
							&for_bg,
							change,
							parents.as_deref(),
							&opts,
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
		self.rearm_autoload();
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
		let opts = RunOptions::default();
		let (first, walk) =
			read_graph_page(root, None, None, 0, 2, &opts).unwrap();
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
			read_graph_page(root, reuse, None, 2, 2, &opts).unwrap();
		let reuse = Some((walk, second.refs.clone(), second.head.clone()));
		let (third, _) =
			read_graph_page(root, reuse, None, 4, 2, &opts).unwrap();
		assert!(!third.has_more);
		let seen: Vec<String> = [first, second, third]
			.into_iter()
			.flat_map(|h| h.commits.into_iter().map(|c| c.subject))
			.collect();
		assert_eq!(seen, ["c5", "c4", "c3", "c2", "c1", "c0"]);
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
	fn changed_files_group_by_directory() {
		use snip_core::format::ChangeType;
		let files: Vec<(String, Option<ChangeType>)> =
			["README.md", "src/a.rs", "docs/x.md", "src/b.rs"]
				.iter()
				.map(|p| (p.to_string(), Some(ChangeType::Modified)))
				.collect();
		let rows = changed_file_rows(&files, &["src".to_string()]);
		assert_eq!(
			rows,
			[
				ChangedRow::Dir {
					path: "docs".into(),
					files: 1,
					expanded: true
				},
				ChangedRow::File {
					idx: 2,
					nested: true
				},
				ChangedRow::Dir {
					path: "src".into(),
					files: 2,
					expanded: false
				},
				ChangedRow::File {
					idx: 0,
					nested: false
				},
			]
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
