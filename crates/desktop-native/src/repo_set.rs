//! The workspace's repositories, behind one gate (#227).
//!
//! Everything that is derived from the list of repositories used to be
//! brought up to date by whichever code path happened to touch the list:
//! the end of a scan did most of it, a hand-added repo did none of it, and
//! a path that did not know about a consumer left it stale until some
//! unrelated path recomputed it. Now the list is private to this module.
//! It changes through [`RepoSet::apply`], and a change that altered the
//! list must be handed to [`WorkbenchModel::repos_changed`], which brings
//! every consumer up to date. A caller cannot skip the hand-over:
//!
//! - the entries have no `DerefMut`, so a direct edit does not compile;
//! - [`RepoChange`] is `#[must_use]`, and CI builds with `-D warnings`;
//! - dropping a changed [`RepoChange`] unhanded panics in a debug build;
//! - a test counts the places that build a set.
//!
//! # What changes the list
//!
//! | Op | Caller |
//! |---|---|
//! | `Rescan` / `Merge` | `apply_scan_page`: one page of a local or remote scan |
//! | `Merge` | `install_added_repo`: a repo added by hand |
//! | `Remove` | `drop_vanished_repo`: a kept repo the scan did not find |
//! | `Clear` | a remote scan that failed after wiping the list |
//! | `Release` | `release_workspace_state`: the workspace closed |
//!
//! Two writes change an entry but not the list, and do not advance the
//! revision, or the Log would reload on every status read:
//! [`RepoSet::set_summary`] from `land_queued_changes` and
//! `update_open_summary`.
//!
//! # What is derived from the list
//!
//! Brought up to date at once, on every change, scan pages included:
//! the open repo's index, the repo selector's cursor and the candidate
//! destinations of an open paste preview. Brought up to date when no scan
//! is running: the Changes nodes and the Log's scope. They wait because the
//! first page of a Refresh shrinks the list to the open and hand-added
//! repos: settling Changes then would drop every other repo's rows to grow
//! them back as loading nodes, and settling the Log on each page would read
//! it again and again during one big scan.
//!
//! Derived from the list but not here, because they are read when drawn or
//! keyed by root: the Project rows, the header counts, the Log chip and the
//! selector rows, the Log's repo filter, the tree selections, the Changes
//! queue (its generation is its own revision). The workspace tree is built
//! by `ensure_ws_tree`, a policy of the end of a scan, and stays there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::Context;

use crate::{RepoEntry, WorkbenchModel};

/// A repo's identity as discovery keys it: toplevel and git dir.
type Key = (PathBuf, PathBuf);

/// The repos of the open workspace, in `(name, root)` order.
pub struct RepoSet {
	entries: Vec<RepoEntry>,
	/// Advances on every change of the list, never goes back.
	rev: u64,
	/// `(root, name)` of each entry, rebuilt when the list changes: what a
	/// consumer needs to map the list before a change onto the one after.
	index: Arc<[(PathBuf, String)]>,
	keys: Vec<Key>,
}

impl std::ops::Deref for RepoSet {
	type Target = [RepoEntry];

	fn deref(&self) -> &[RepoEntry] {
		&self.entries
	}
}

impl<'a> IntoIterator for &'a RepoSet {
	type Item = &'a RepoEntry;
	type IntoIter = std::slice::Iter<'a, RepoEntry>;

	fn into_iter(self) -> Self::IntoIter {
		self.entries.iter()
	}
}

/// How the list changes.
pub(crate) enum RepoOp {
	/// A newer read of a known repo replaces it, a new one is added.
	Merge(Vec<RepoEntry>),
	/// A Refresh's first page: the list shrinks to `kept`, then `page`
	/// merges in, in one change.
	Rescan {
		kept: Vec<RepoEntry>,
		page: Vec<RepoEntry>,
	},
	/// Removes a repo; the names left are not disambiguated again.
	Remove(Key),
	Clear,
	/// The workspace closed: the list and its capacity go.
	Release,
}

/// The outcome of [`RepoSet::apply`]: hand it to
/// [`WorkbenchModel::repos_changed`].
#[must_use = "a changed repo set must reach WorkbenchModel::repos_changed"]
pub(crate) struct RepoChange {
	changed: bool,
	before: Arc<[(PathBuf, String)]>,
	armed: bool,
}

impl RepoChange {
	/// `(root, name)` of the list as it was before the change.
	pub(crate) fn before(&self) -> &[(PathBuf, String)] {
		&self.before
	}
}

impl Drop for RepoChange {
	fn drop(&mut self) {
		if self.armed && !std::thread::panicking() {
			debug_assert!(
				false,
				"RepoChange dropped without repos_changed (#227)"
			);
		}
	}
}

impl RepoSet {
	/// Built once, by the workbench.
	pub(crate) fn new() -> Self {
		Self {
			entries: Vec::new(),
			rev: 0,
			index: Arc::from(Vec::new()),
			keys: Vec::new(),
		}
	}

	pub(crate) fn rev(&self) -> u64 {
		self.rev
	}

	pub(crate) fn roots(&self) -> Vec<PathBuf> {
		self.index.iter().map(|(root, _)| root.clone()).collect()
	}

	/// Where the repo of `key` is now.
	pub(crate) fn position_of(&self, key: &Key) -> Option<usize> {
		self.entries
			.iter()
			.position(|e| crate::repo_key_matches(e, key))
	}

	/// A repo's counts were read again. The list does not change.
	pub(crate) fn set_summary(
		&mut self,
		root: &Path,
		summary: crate::RepoSummary,
	) -> bool {
		match self.entries.iter_mut().find(|e| e.root == root) {
			Some(entry) => {
				entry.summary = Ok(summary);
				true
			}
			None => false,
		}
	}

	pub(crate) fn apply(
		&mut self,
		op: RepoOp,
		workspace_root: &Path,
	) -> RepoChange {
		let before = self.index.clone();
		let before_keys = std::mem::take(&mut self.keys);
		let release = matches!(op, RepoOp::Release);
		match op {
			RepoOp::Merge(page) => self.merge(page, workspace_root),
			RepoOp::Rescan { kept, page } => {
				self.entries.clear();
				self.merge(kept, workspace_root);
				self.merge(page, workspace_root);
			}
			RepoOp::Remove(key) => {
				if let Some(pos) = self.position_of(&key) {
					self.entries.remove(pos);
				}
			}
			RepoOp::Clear => self.entries.clear(),
			RepoOp::Release => crate::release_vec(&mut self.entries),
		}
		self.keys = self.entries.iter().map(crate::repo_key).collect();
		let index: Arc<[(PathBuf, String)]> = self
			.entries
			.iter()
			.map(|e| (e.root.clone(), e.name.clone()))
			.collect();
		let changed = release || self.keys != before_keys || *index != *before;
		if changed {
			self.index = index;
			self.rev += 1;
		}
		RepoChange {
			changed,
			before,
			armed: changed,
		}
	}

	fn merge(&mut self, page: Vec<RepoEntry>, workspace_root: &Path) {
		for entry in page {
			let key = crate::repo_key(&entry);
			match self.position_of(&key) {
				Some(pos) => self.entries[pos] = entry,
				None => self.entries.push(entry),
			}
		}
		WorkbenchModel::disambiguate_repo_names(
			&mut self.entries,
			workspace_root,
		);
		self.entries
			.sort_by(|a, b| a.name.cmp(&b.name).then(a.root.cmp(&b.root)));
	}

	/// A test's way to set the list up directly: the revision advances and
	/// nothing fans out.
	#[cfg(test)]
	pub(crate) fn replace_for_test(&mut self, entries: Vec<RepoEntry>) {
		self.entries = entries;
		self.keys = self.entries.iter().map(crate::repo_key).collect();
		self.index = self
			.entries
			.iter()
			.map(|e| (e.root.clone(), e.name.clone()))
			.collect();
		self.rev += 1;
	}
}

impl WorkbenchModel {
	/// The one place the consumers of the repo list are brought up to date.
	pub(crate) fn repos_changed(
		&mut self,
		mut change: RepoChange,
		cx: &mut Context<Self>,
	) {
		change.armed = false;
		if !change.changed {
			return;
		}
		app_log!(
			"[APP:REPOS_CHANGED: rev={} n={}]",
			self.repos.rev(),
			self.repos.len()
		);
		// An index kept across the change names the same repo or none.
		if let Some(idx) = self.selected_repo_idx {
			self.selected_repo_idx = match &self.pinned_repo {
				Some(key) => self.repos.position_of(key),
				None => Some(idx).filter(|idx| *idx < self.repos.len()),
			};
		}
		self.rebind_selector_cursor(change.before(), cx);
		self.refresh_paste_candidates(cx);
		if !self.is_loading {
			self.settle_repo_consumers(cx);
		}
		cx.notify();
	}

	/// What waits for the end of a scan: the Changes nodes and the Log's
	/// scope.
	pub(crate) fn settle_repo_consumers(&mut self, cx: &mut Context<Self>) {
		self.sync_change_slots();
		self.sync_list_row();
		self.sync_log_scope(cx);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::RepoEntryKind;

	fn entry(root: &str, name: &str) -> RepoEntry {
		RepoEntry {
			root: PathBuf::from(root),
			name: name.into(),
			kind: RepoEntryKind::Main,
			identity: None,
			summary: Err(crate::test_failure("offline")),
		}
	}

	fn summary() -> crate::RepoSummary {
		crate::RepoSummary {
			identity: snip_core::workspace::RepoIdentity {
				toplevel: PathBuf::from("/ws/a"),
				git_dir: PathBuf::from("/ws/a/.git"),
				common_dir: PathBuf::from("/ws/a/.git"),
				kind: snip_core::workspace::RepoKind::Main,
			},
			head: None,
			branch: None,
			changes: Default::default(),
		}
	}

	fn key(root: &str) -> Key {
		(PathBuf::from(root), PathBuf::new())
	}

	fn seen(change: RepoChange, set: &RepoSet) -> (bool, u64) {
		let mut change = change;
		change.armed = false;
		(change.changed, set.rev())
	}

	/// Every op moves the revision when the list changes and only then.
	#[test]
	fn repo_set_ops_bump_rev_only_on_change() {
		let ws = Path::new("/ws");
		let mut set = RepoSet::new();
		let rev = set.rev();

		let ch = set.apply(RepoOp::Merge(vec![entry("/ws/a", "a")]), ws);
		assert_eq!(seen(ch, &set), (true, rev + 1), "a new repo");
		let ch = set.apply(RepoOp::Merge(vec![entry("/ws/a", "a")]), ws);
		assert_eq!(seen(ch, &set), (false, rev + 1), "the same repo again");

		let mut again = entry("/ws/a", "a");
		again.summary = Ok(summary());
		let ch = set.apply(RepoOp::Merge(vec![again]), ws);
		assert_eq!(seen(ch, &set), (false, rev + 1), "only the summary");
		assert!(set[0].summary.is_ok(), "the newer read replaced it");

		let ch = set.apply(
			RepoOp::Merge(vec![entry("/ws/b", "b"), entry("/ws/c", "c")]),
			ws,
		);
		assert_eq!(seen(ch, &set), (true, rev + 2));

		let ch = set.apply(
			RepoOp::Rescan {
				kept: vec![entry("/ws/a", "a")],
				page: vec![entry("/ws/b", "b")],
			},
			ws,
		);
		assert_eq!(seen(ch, &set), (true, rev + 3), "the list shrank");
		let ch = set.apply(
			RepoOp::Rescan {
				kept: vec![entry("/ws/a", "a")],
				page: vec![entry("/ws/b", "b")],
			},
			ws,
		);
		assert_eq!(seen(ch, &set), (false, rev + 3), "rescanned to the same");

		let ch = set.apply(RepoOp::Remove(key("/ws/zzz")), ws);
		assert_eq!(seen(ch, &set), (false, rev + 3), "not there");
		let ch = set.apply(RepoOp::Remove(key("/ws/b")), ws);
		assert_eq!(seen(ch, &set), (true, rev + 4), "removed");

		let ch = set.apply(RepoOp::Clear, ws);
		assert_eq!(seen(ch, &set), (true, rev + 5), "cleared");
		let ch = set.apply(RepoOp::Clear, ws);
		assert_eq!(seen(ch, &set), (false, rev + 5), "already empty");

		set.apply(RepoOp::Merge(vec![entry("/ws/a", "a")]), ws)
			.armed_off();
		let before = set.rev();
		let ch = set.apply(RepoOp::Release, ws);
		assert_eq!(seen(ch, &set), (true, before + 1), "release always");
		assert_eq!(set.entries.capacity(), 0, "the capacity is released");
		let ch = set.apply(RepoOp::Release, ws);
		assert_eq!(seen(ch, &set), (true, before + 2));

		// Writing a summary is not a change of the list.
		set.apply(RepoOp::Merge(vec![entry("/ws/a", "a")]), ws)
			.armed_off();
		let before = set.rev();
		assert!(set.set_summary(Path::new("/ws/a"), summary()));
		assert_eq!(set.rev(), before);
	}

	impl RepoChange {
		fn armed_off(mut self) {
			self.armed = false;
		}
	}

	/// A change nobody handed to `repos_changed` is caught at its drop.
	#[test]
	#[should_panic(expected = "#227")]
	fn an_unfanned_repo_change_panics() {
		let mut set = RepoSet::new();
		let _change = set
			.apply(RepoOp::Merge(vec![entry("/ws/a", "a")]), Path::new("/ws"));
	}

	/// The set is built once, by the workbench: a second one made
	/// elsewhere would be a list with no gate behind it.
	#[test]
	fn repo_set_is_constructed_only_by_the_workbench() {
		fn visit(dir: &Path, found: &mut usize) {
			for entry in std::fs::read_dir(dir).unwrap() {
				let path = entry.unwrap().path();
				if path.is_dir() {
					visit(&path, found);
				} else if path.extension().is_some_and(|e| e == "rs") {
					let text = std::fs::read_to_string(&path).unwrap();
					let product =
						text.split("mod tests {").next().unwrap_or_default();
					*found +=
						product.matches(concat!("RepoSet", "::new(")).count();
				}
			}
		}
		let mut found = 0;
		visit(
			&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
			&mut found,
		);
		assert_eq!(found, 1, "only WorkbenchModel::new builds the set");
	}
}
