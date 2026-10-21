//! The registry of the model's derived-state invariants (#230).
//!
//! Each member names a property that must hold between two views of the
//! same fact: the Log selection and the commit view, the repo set and every
//! consumer of it, a layer and its panel. A property that held once, in a
//! test written for one path, is not enough: any other path to the same
//! state can break it. So the properties live here, once, are evaluated by
//! the same `eval` everywhere, and are driven by the `fuzz_` lanes in
//! `src/tests/in_process/invariant_tests.rs` over random gestures.
//!
//! A failing seed replays: the same seed gives the same steps. CI runs only
//! the fixed seeds (gpui starts at 0 without `SEED`: local 0-3, paste 0-2,
//! remote 0-2), so it does not explore new ones. Exploring is a manual long
//! run: `SEED=<n> ITERATIONS=<k> cargo nextest run -p snip-desktop-native
//! fuzz_`.
//!
//! A member is
//!
//! - **A** (`When::Always`): true between any two steps of the event loop, or
//! - **Q** (`When::Quiescent`): true once nothing is loading.
//!
//! Each carries a status note in its doc:
//!
//! - **evidence**: it failed on the code before its fix; the commit and the
//!   test that shows it are named;
//! - **guard**: it may have held already; it names the issue it keeps fixed
//!   and a mutation that turns a test red.
//!
//! `Status::Pending(issue)` is a known violation that another issue owns:
//! `check` skips it, and a scripted test asserts it still fails, so fixing
//! the issue forces the member back to `Active`. No exemption depends on a
//! sequence of actions.
//!
//! The Log selection (`L`) is not evaluated while a file read holds the
//! shown selection aside (`held_selection`): the read took the selection off
//! the model on purpose and the preview still shows the old one. `P3` guards
//! that window and `P4` says it is closed once nothing is loading, so every
//! quiescent point checks `L` in full.

use std::path::{Path, PathBuf};

use gpui::App;

use crate::i18n::{Locale, Msg};
use crate::overlay::{OverlayInvariant, OverlayStack};
use crate::reader::PreviewSource;
use crate::WorkbenchModel;

/// One property of the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Invariant {
	// Log selection and the commit view.
	/// A range endpoint needs an anchor. Guard: #230 PR-0 (`extend_range`);
	/// clearing the selection without clearing `range_head` breaks it.
	L1,
	/// The commit view's open file is one of its files (or a file of the
	/// rev tree, which has its own list). Guard: #230 PR-0.
	L2,
	/// A multi-selected file or folder (`dir/`) is listed. Guard: #230 PR-0.
	L3,
	/// Per-file origins index the selected commits. Guard: #230 PR-0 (the
	/// reset drops every member together).
	L4,
	/// No files, no per-file state. Guard: #230 PR-0.
	L5,
	/// No selection, no commit view, no Log preview; a rev tree outlives the
	/// selection and keeps its own file selected and read as the Log's.
	/// Guard: #202 (the repo filter's hand-made reset left a preview
	/// behind); evidence for the rev tree premise: `fuzz_regression_a_rev_tree_file_is_not_a_commit_view_left_behind`.
	L6,
	/// A list that failed to reload keeps no selection it hid. Quiescent.
	/// Guard: #230 PR-0 (failed reload clears the selection).
	L7,
	/// The details belong to the selected commit. Guard: #230 PR-0.
	L8,
	// Preview and paste preview.
	/// A running preview read can be cancelled: its token is alive while the
	/// workspace accepts work. Guard: #230 PR-0 (a Log click during a drain).
	P1,
	/// A working-tree preview belongs to a repo the workspace has. Quiescent.
	/// Guard: #215, #227.
	P2,
	/// A selection held aside belongs to a read in flight. Guard: #230 PR-0.
	P3,
	/// Nothing is held once nothing is loading. Quiescent. Guard: #230 PR-0.
	P4,
	/// The paste cursor stands on a drawn row. Guard: #239.
	P5,
	/// No fold names a folder that is gone. Quiescent. Guard: #239.
	P6,
	/// The hint that offers to undo an adjustment is on screen whenever the
	/// adjustment is. Guard: #240.
	P7,
	/// The selected paste item exists. Guard: #239.
	P8,
	/// An open paste has a destination folder to show. Guard: #238.
	P9,
	/// The typed destination layer does not outlive its paste. Guard: #238,
	/// #226.
	P10,
	// Repo set and what reads it.
	/// The open repo is in the list and is the pinned one. Quiescent. Guard:
	/// #227.
	R1,
	/// The Changes nodes are the repos, in order, under their names.
	/// Quiescent. Guard: #227.
	R2,
	/// The Log scope is the one loaded. Quiescent. Guard: #227.
	R3,
	/// An idle paste preview has seen every repo. Quiescent. Guard: #215,
	/// #227; deleting `repos_changed`'s `refresh_paste_candidates` breaks it.
	R4,
	/// The roots the commit view and the open file name are the workspace's.
	/// Quiescent. Guard.
	R5,
	/// No Changes read is left running or waiting. Quiescent. Guard.
	R6,
	// Log scope as drawn.
	/// The branches pane marks the current branch of the repo the log is
	/// of. Guard: #173; reading the open repo's branch instead breaks it.
	B1,
	// Loading flags.
	/// Quiescent: no preview read.
	Q1,
	/// Quiescent: no history page read.
	Q2,
	/// Quiescent: no repo or tree load.
	Q3,
	/// Quiescent: no copy.
	Q4,
	/// Quiescent: no remote open.
	Q5,
	/// Quiescent: no paste preview.
	Q6,
	/// Quiescent: no unfinished job.
	Q7,
	// Remote menu.
	/// A listed host keeps its client for the next hop. Quiescent. Guard:
	/// #198.
	M2,
	/// The path field shows what was last written into it until the user
	/// types. Driver-only: it needs the driver's typing history. Guard: #190.
	M3,
	// Messages shown.
	/// Every message on screen reads as zh-TW wording once its data is taken
	/// out. Quiescent. Guard: #206, #213, #225, #233.
	D1,
	// Layers.
	/// Wraps `OverlayInvariant` O1 to O6. Guard: #203, #212, #226.
	O(OverlayInvariant),
	/// An open layer is drawn, a closed one is not. Driver-only. Guard: #226.
	O7,
	/// The keyboard is not on a closed layer. Driver-only. Guard: #226.
	O8,
}

/// When a member must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum When {
	Always,
	Quiescent,
}

/// Whether `check` runs a member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
	Active,
	/// A known violation owned by this issue. A scripted test asserts it
	/// still fails.
	#[allow(dead_code)]
	// no member is pending today; the registry has the slot
	Pending(u32),
}

/// The model's fields are classified by the family that owns them
/// (`classify`); a family with members must have at least one.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Family {
	LogSelection,
	Preview,
	PastePanel,
	RepoSet,
	LogScope,
	Loading,
	RemoteMenu,
	Messages,
	Overlays,
	/// State the user sets directly (a filter, a selected file): nothing
	/// derives it from another field.
	Source,
	/// Widths, heights and fit results: layout is #228 and #229.
	Layout,
	/// The counters that drop a landing result (`generation.rs`).
	Generation,
	/// Cancel tokens: P1 and Q7 guard them indirectly.
	Cancel,
	/// Focus handles and pending focus requests.
	Focus,
	/// Text inputs.
	Input,
	/// Caches and last-frame cells.
	Cache,
	/// Seams only a test or the E2E driver uses.
	TestSeam,
	/// Configuration fixed when the model is built.
	Config,
}

/// The members, local ones first and the layers' behind them.
pub(crate) fn all() -> Vec<Invariant> {
	use Invariant::*;
	let mut all = vec![
		L1, L2, L3, L4, L5, L6, L7, L8, P1, P2, P3, P4, P5, P6, P7, P8, P9,
		P10, R1, R2, R3, R4, R5, R6, B1, Q1, Q2, Q3, Q4, Q5, Q6, Q7, M2, M3,
		D1, O7, O8,
	];
	all.extend(OverlayInvariant::ALL.iter().copied().map(O));
	all
}

impl Invariant {
	pub(crate) fn when(self) -> When {
		use Invariant::*;
		match self {
			L7 | P2 | P4 | P6 | R1 | R2 | R3 | R4 | R5 | R6 | Q1 | Q2 | Q3
			| Q4 | Q5 | Q6 | Q7 | M2 | D1 | O7 | O8 => When::Quiescent,
			// M3 holds at any time, but only the driver knows the history it
			// needs.
			L1 | L2 | L3 | L4 | L5 | L6 | L8 | P1 | P3 | P5 | P7 | P8 | P9
			| P10 | B1 | M3 => When::Always,
			O(o) if o.quiescent_only() => When::Quiescent,
			O(_) => When::Always,
		}
	}

	pub(crate) fn status(self) -> Status {
		use Invariant::*;
		match self {
			L1 | L2 | L3 | L4 | L5 | L6 | L7 | L8 | P1 | P2 | P3 | P4 | P5
			| P6 | P7 | P8 | P9 | P10 | R1 | R2 | R3 | R4 | R5 | R6 | B1
			| Q1 | Q2 | Q3 | Q4 | Q5 | Q6 | Q7 | M2 | M3 | D1 | O(_) | O7
			| O8 => Status::Active,
		}
	}

	/// Evaluated by the fuzz driver, not by `eval`: it needs what only the
	/// driver has (its typing history, the drawn frame).
	pub(crate) fn driver_only(self) -> bool {
		matches!(self, Invariant::M3 | Invariant::O7 | Invariant::O8)
	}

	#[cfg(test)]
	pub(crate) fn family(self) -> Family {
		use Invariant::*;
		match self {
			L1 | L2 | L3 | L4 | L5 | L6 | L7 | L8 => Family::LogSelection,
			P1 | P2 | P3 | P4 => Family::Preview,
			P5 | P6 | P7 | P8 | P9 | P10 | R4 => Family::PastePanel,
			R1 | R2 | R5 | R6 => Family::RepoSet,
			R3 | B1 => Family::LogScope,
			Q1 | Q2 | Q3 | Q4 | Q5 | Q6 | Q7 => Family::Loading,
			M2 | M3 => Family::RemoteMenu,
			D1 => Family::Messages,
			O(_) | O7 | O8 => Family::Overlays,
		}
	}

	/// The id as the E2E log and a failure message name it.
	pub(crate) fn label(self) -> String {
		match self {
			Invariant::O(o) => format!("O({o:?})"),
			other => format!("{other:?}"),
		}
	}
}

/// A member that does not hold, and what is wrong.
#[derive(Clone, Debug)]
pub(crate) struct Violation {
	pub id: Invariant,
	pub detail: String,
}

/// Every active, model-checkable member of `when` that does not hold; at
/// `Quiescent` the `Always` members are checked too.
pub(crate) fn check(
	m: &WorkbenchModel,
	cx: &App,
	when: When,
) -> Vec<Violation> {
	all()
		.into_iter()
		.filter(|id| {
			id.status() == Status::Active
				&& !id.driver_only()
				&& (id.when() == When::Always || when == When::Quiescent)
		})
		.filter_map(|id| eval(id, m, cx).map(|detail| Violation { id, detail }))
		.collect()
}

/// Panics, naming every member of `ids` that does not hold: a test after a
/// change it knows the model has to settle from.
#[cfg(test)]
pub(crate) fn assert_holds(m: &WorkbenchModel, cx: &App, ids: &[Invariant]) {
	let broken: Vec<String> = ids
		.iter()
		.filter_map(|&id| {
			eval(id, m, cx).map(|d| format!("{}: {d}", id.label()))
		})
		.collect();
	assert!(broken.is_empty(), "invariants broken: {broken:#?}");
}

impl WorkbenchModel {
	/// Prints one `[APP:INVARIANT_BROKEN]` line for each `Always` member that
	/// does not hold in the frame just drawn: once while it stays broken, and
	/// again when it breaks anew. Called only with probes on (an E2E run).
	pub(crate) fn log_broken_invariants(&self, cx: &App) {
		let broken = check(self, cx, When::Always);
		let mut logged = self.invariants_logged.borrow_mut();
		logged.retain(|id| broken.iter().any(|v| v.id == *id));
		for v in broken {
			if !logged.contains(&v.id) {
				logged.push(v.id);
				app_log!(
					"{}",
					crate::ui::probe_line::invariant_broken(
						&v.id.label(),
						&v.detail
					)
				);
			}
		}
	}
}

/// The roots a repo-keyed field may name: the workspace's repos, its folder
/// and that folder's canonical path.
fn known_root(m: &WorkbenchModel, root: &Path) -> bool {
	m.repos.iter().any(|r| r.root == root)
		|| m.workspace_root == root
		|| m.ws_home.as_deref() == Some(root)
}

fn roots_text(m: &WorkbenchModel) -> String {
	let roots: Vec<PathBuf> = m.repos.iter().map(|r| r.root.clone()).collect();
	format!("{roots:?} + {:?} + {:?}", m.workspace_root, m.ws_home)
}

/// The commit a rev-tree file belongs to: the open tree's, or, once "Back to
/// Working" left the tree, the one the file's tab remembers
/// (`CommitView::rev_file_sha`). The tab and the tree are independent, so the
/// file's ownership does not end with the tree.
fn rev_owner(m: &WorkbenchModel) -> Option<&str> {
	m.rev_tree
		.as_ref()
		.map(|tree| tree.sha.as_str())
		.or(m.commit_view.rev_file_sha.as_deref())
}

/// Whether `path` is a file a rev tree owns: the open tree lists it, or the
/// preview shows it as a file of the owning commit, or the tab kept it after
/// the tree was left. Only then may a commit view name it with no row of the
/// Log's list behind it.
fn tree_owns_file(m: &WorkbenchModel, path: &str) -> bool {
	let Some(owner) = rev_owner(m) else {
		return false;
	};
	m.rev_tree
		.as_ref()
		.is_some_and(|tree| tree.lists_file(path))
		|| m.preview.as_ref().is_some_and(|p| {
			p.path.as_deref() == Some(path)
				&& matches!(
					&p.source,
					PreviewSource::CommitFile { sha } if sha == owner
				)
		}) || (m.rev_tree.is_none()
		&& m.commit_view.selected_commit_file.as_deref() == Some(path))
}

/// One member; `None` means it holds. A member the driver evaluates
/// (`driver_only`) is `None` here.
pub(crate) fn eval(
	id: Invariant,
	m: &WorkbenchModel,
	cx: &App,
) -> Option<String> {
	use Invariant::*;
	// The selection a read holds aside is not on the model: the L members
	// say nothing then (see the module doc).
	if matches!(id, L1 | L2 | L3 | L4 | L5 | L6 | L7 | L8)
		&& m.held_selection.is_some()
	{
		return None;
	}
	let view = &m.commit_view;
	match id {
		L1 => (m.range_head.is_some() && m.selected_commit.is_none())
			.then(|| "a range endpoint without a selected commit".to_string()),
		L2 => {
			let open = view.selected_commit_file.as_ref()?;
			let listed = view.commit_files.iter().any(|(p, _)| p == open)
				|| tree_owns_file(m, open);
			(!listed).then(|| {
				format!("the open commit file {open:?} is not in the list")
			})
		}
		L3 => view.commit_file_sel.iter().find_map(|sel| {
			let listed = view.commit_files.iter().any(|(p, _)| {
				p == sel
					|| sel
						.strip_suffix('/')
						.is_some_and(|dir| p.starts_with(&format!("{dir}/")))
			}) || tree_owns_file(m, sel);
			(!listed).then(|| {
				format!("{sel:?} is selected and not in the file list")
			})
		}),
		L4 => {
			let origin = &view.commit_file_origin;
			if origin.is_empty() {
				return None;
			}
			if origin.len() != view.commit_files.len() {
				return Some(format!(
					"{} origins for {} files",
					origin.len(),
					view.commit_files.len()
				));
			}
			let n = m.log_selected.len();
			origin
				.iter()
				.find(|&&o| o as usize >= n)
				.map(|o| format!("origin {o} points past {n} selected commits"))
		}
		L5 => {
			if !view.commit_files.is_empty() {
				return None;
			}
			if !view.commit_file_origin.is_empty() {
				return Some("origins without files".into());
			}
			if !view.commit_file_gitlinks.is_empty() {
				return Some("gitlinks without files".into());
			}
			if !view.commit_file_sel.is_empty() {
				return Some("a file selection without files".into());
			}
			if view.commit_files_truncated {
				return Some("a truncated flag without files".into());
			}
			view.commit_files_first_parent_same
				.is_some()
				.then(|| "first-parent data without files".to_string())
		}
		L6 => {
			let selected = m.selected_commit.is_some()
				|| !m.log_selected.is_empty()
				|| m.compare.is_some();
			if selected {
				return None;
			}
			// A rev tree outlives the selection and keeps one of its own
			// files selected here (`open_rev_file`); only then is the rest
			// allowed: no commit file selection of the Log, and the Log's
			// preview flag is the tree file's.
			let tree_file = view
				.selected_commit_file
				.as_deref()
				.is_some_and(|path| tree_owns_file(m, path));
			let reset = if tree_file {
				view.is_reset_but_for_tree_file()
			} else {
				view.is_reset()
			};
			if !reset {
				return Some("no selection, but a commit view is left".into());
			}
			if m.preview_pending_from_log && !tree_file {
				return Some(
					"no selection, but the preview is marked from the Log"
						.into(),
				);
			}
			// The one commit preview left legal is the open tree's own file:
			// a CommitFile of the tree's commit that the tree lists. A
			// CommitDiff or Compare, or another commit's file, is a
			// leftover even while a tree is open.
			let stale = m.preview.as_ref().is_some_and(|p| {
				let from_log = matches!(
					p.source,
					PreviewSource::CommitDiff { .. }
						| PreviewSource::CommitFile { .. }
						| PreviewSource::Compare { .. }
				);
				let tree_own = rev_owner(m).is_some_and(|owner| {
					matches!(
						&p.source,
						PreviewSource::CommitFile { sha } if sha == owner
					) && p.path.as_deref().is_some_and(|path| {
						// Listed by the tree, or the file the tree opened
						// (a listing the budget evicted does not answer), or
						// the one its tab kept after the tree was left.
						m.rev_tree
							.as_ref()
							.is_some_and(|tree| tree.lists_file(path))
							|| view.selected_commit_file.as_deref()
								== Some(path)
					})
				});
				from_log && !tree_own
			});
			stale.then(|| {
				"no selection, but the preview is a commit's".to_string()
			})
		}
		L7 => {
			if !m.commits.is_empty() || m.log_deferred {
				return None;
			}
			(m.selected_commit.is_some()
				|| !m.log_selected.is_empty()
				|| m.compare.is_some())
			.then(|| "an empty log keeps a selection".to_string())
		}
		L8 => {
			let details = view.commit_details.as_ref()?;
			(Some(&details.sha) != m.selected_commit.as_ref()).then(|| {
				format!(
					"details of {} for the selection {:?}",
					details.sha, m.selected_commit
				)
			})
		}
		P1 => {
			if !(m.preview_loading && m.accepting_work()) {
				return None;
			}
			match &m.preview_cancel {
				None => Some("a preview is loading with no token".into()),
				Some(t) if t.is_cancelled() => {
					Some("a preview is loading with a cancelled token".into())
				}
				Some(_) => None,
			}
		}
		P2 => {
			let p = m.preview.as_ref()?;
			let working = matches!(
				p.source,
				PreviewSource::WorkingFile
					| PreviewSource::WorkingChanges
					| PreviewSource::StagedChanges
					| PreviewSource::UnstagedChanges
			);
			let root = p.root.as_ref().filter(|_| working)?;
			(!known_root(m, root)).then(|| {
				format!("the preview is of {root:?}, not of {}", roots_text(m))
			})
		}
		P3 => m.held_selection.as_ref().and_then(|_| {
			(!(m.preview.is_some()
				&& m.preview_error.is_none()
				&& m.preview_loading))
				.then(|| {
					"a selection is held, with no read in flight over a \
					 shown preview"
						.to_string()
				})
		}),
		P4 => m
			.held_selection
			.is_some()
			.then(|| "a selection is still held aside".to_string()),
		P5 => {
			let plan = m.paste.plan()?;
			(!plan.whole_commit
				&& !plan.items.is_empty()
				&& !plan.cursor_visible())
			.then(|| {
				format!(
					"the paste cursor {:?} is on no drawn row",
					plan.cursor()
				)
			})
		}
		P6 => {
			let n = m.paste.plan()?.stale_folds();
			(n > 0).then(|| format!("{n} fold(s) name a folder that is gone"))
		}
		P7 => {
			let plan = m.paste.plan()?;
			(plan.restore_hint().is_none() && plan.adjust_paths).then(|| {
				"paths are adjusted and the hint that undoes it is hidden"
					.to_string()
			})
		}
		P8 => {
			let plan = m.paste.plan()?;
			(!plan.items.is_empty()
				&& plan.selected_item_idx >= plan.items.len())
			.then(|| {
				format!(
					"item {} of {}",
					plan.selected_item_idx,
					plan.items.len()
				)
			})
		}
		P9 => (m.paste.is_open() && m.paste.folder().is_none())
			.then(|| "a paste is open with no destination folder".to_string()),
		P10 => (m
			.overlays
			.is_open(crate::overlay::OverlayKind::PasteDestField)
			&& !m.paste.is_open())
		.then(|| "the destination field is open over no paste".to_string()),
		R1 => {
			let i = m.selected_repo_idx?;
			let Some(entry) = m.repos.get(i) else {
				return Some(format!(
					"the open repo {i} is not among {} repos",
					m.repos.len()
				));
			};
			let key = m.pinned_repo.as_ref()?;
			(!crate::repo_key_matches(entry, key)).then(|| {
				format!("the index {i} does not name the pinned repo {key:?}")
			})
		}
		R2 => {
			let mut have: Vec<&Path> =
				m.change_repos.iter().map(|s| s.root.as_path()).collect();
			let mut want: Vec<&Path> =
				m.repos.iter().map(|r| r.root.as_path()).collect();
			have.sort();
			want.sort();
			if have != want {
				return Some(format!(
					"the Changes nodes {have:?} are not the repos {want:?}"
				));
			}
			for slot in &m.change_repos {
				let repo = m.repos.iter().find(|r| r.root == slot.root);
				if Some(slot.name.as_str()) != repo.map(|r| r.name.as_str()) {
					return Some(format!(
						"the Changes node {:?} is not named as its repo",
						slot.root
					));
				}
			}
			let order: Vec<_> = m
				.change_repos
				.iter()
				.map(|s| (s.name.as_str(), s.root.as_path()))
				.collect();
			(!order.is_sorted()).then(|| {
				format!("the Changes nodes are out of order: {order:?}")
			})
		}
		R3 => {
			if !(m.history_loaded
				&& !m.log_deferred
				&& !m.log_scope_key.is_empty()
				&& m.log_reload_for.is_none())
			{
				return None;
			}
			let scope: Vec<PathBuf> =
				m.log_scope().into_iter().map(|(r, _)| r).collect();
			(scope != m.log_scope_key).then(|| {
				format!(
					"the loaded Log is of {:?}, the scope is {scope:?}",
					m.log_scope_key
				)
			})
		}
		R4 => {
			let plan = m.paste.plan()?;
			if m.paste.is_loading()
				|| m.paste.is_applying()
				|| plan.prefix_choices.is_empty()
			{
				return None;
			}
			let want = crate::paste::roots_digest(&m.repos.roots());
			(plan.seen_roots() != want).then(|| {
				"an idle paste preview has not seen the current repos"
					.to_string()
			})
		}
		R5 => {
			for (what, root) in [
				("the commit's repo", view.log_commit_root.as_ref()),
				("the open file's repo", m.selected_file_root.as_ref()),
			] {
				if let Some(root) = root.filter(|r| !known_root(m, r)) {
					return Some(format!(
						"{what} {root:?} is not among {}",
						roots_text(m)
					));
				}
			}
			None
		}
		R6 => {
			if let Some(slot) = m
				.change_repos
				.iter()
				.find(|s| matches!(s.state, crate::ChangeRepoState::Loading))
			{
				return Some(format!("{:?} is still loading", slot.root));
			}
			(!m.changes_queue.is_idle())
				.then(|| "a Changes read is running or waiting".to_string())
		}
		B1 => {
			let drawn = m.last_branch_current.take();
			m.last_branch_current.set(drawn.clone());
			let drawn = drawn?;
			let want = match m.log_scope_key.as_slice() {
				[one] => m
					.repos
					.iter()
					.find(|r| &r.root == one)
					.and_then(|r| r.summary.as_ref().ok())
					.and_then(|s| s.branch.clone()),
				_ => None,
			};
			(drawn != want).then(|| {
				format!("the branches pane marked {drawn:?}, the repo is on {want:?}")
			})
		}
		Q1 => m.preview_loading.then(|| "a preview is loading".into()),
		Q2 => m.history_extending.then(|| "a log page is loading".into()),
		Q3 => m.is_loading.then(|| "the workspace is loading".into()),
		Q4 => m.is_copying.then(|| "a copy is running".into()),
		Q5 => m.remote.busy.then(|| "a remote open is running".into()),
		Q6 => m.paste.is_loading().then(|| "a paste is loading".into()),
		Q7 => {
			let n = m.lifecycle.live_jobs();
			(n > 0).then(|| format!("{n} job(s) have not finished"))
		}
		M2 => {
			let browse = m.remote.browse.as_ref()?;
			(matches!(browse.listing, Some(Ok(_)))
				&& !m.remote.busy
				&& !browse.has_client())
			.then(|| "a listed host browse lost its client".to_string())
		}
		M3 | O7 | O8 => None,
		D1 => shown_messages(m).into_iter().find_map(|(source, msg)| {
			let shape = msg.render_shape(Locale::ZhTw);
			if let Some(run) = english_run(&shape) {
				return Some(format!(
					"{source}: {} reads {shape:?} (English run {run:?})",
					msg.key
				));
			}
			(!has_cjk(&shape)).then(|| {
				format!(
					"{source}: {} reads {shape:?}, with no Chinese",
					msg.key
				)
			})
		}),
		O(o) => {
			OverlayStack::eval(o, m, cx).map(|d| format!("{}: {d}", o.what()))
		}
	}
}

/// Every message the window can show, by where it is shown. Each source is
/// the one the screen draws from; a new one is added here with its widget.
pub(crate) fn shown_messages(m: &WorkbenchModel) -> Vec<(&'static str, Msg)> {
	let mut out: Vec<(&'static str, Msg)> = vec![("status", m.status.clone())];
	let mut push = |source: &'static str, msg: Option<&Msg>| {
		if let Some(msg) = msg {
			out.push((source, msg.clone()));
		}
	};
	push("preview_error", m.preview_error.as_ref());
	push("history_error", m.history_error.as_ref());
	push("toast", m.toast.as_ref().map(|(_, _, msg)| msg));
	push(
		"remote.message",
		m.remote.message.as_ref().map(|(_, msg)| msg),
	);
	push("remote.scan_error", m.remote.scan_error.as_ref());
	push(
		"remote.restoring",
		m.remote.restoring.as_ref().and_then(|r| r.failed.as_ref()),
	);
	push(
		"remote.browse",
		m.remote
			.browse
			.as_ref()
			.and_then(|b| b.listing.as_ref())
			.and_then(|l| l.as_ref().err()),
	);
	push("paste.dest_refusal", m.paste.dest_refusal());
	push(
		"paste.dest_edit",
		m.overlays.paste_dest_edit().and_then(|e| e.error.as_ref()),
	);
	push("paste.error", m.paste.plan().and_then(|p| p.error.as_ref()));
	push(
		"paste.notice",
		m.paste.detail().and_then(|p| p.notice.as_ref()),
	);
	push(
		"preview.notice",
		m.preview.as_ref().and_then(|p| p.notice.as_ref()),
	);
	for repo in &m.change_repos {
		if let crate::ChangeRepoState::Failed(err) = &repo.state {
			out.push((
				"changes",
				Msg::with_msg_arg(
					"changes_repo_error",
					[String::new()],
					0,
					err.clone(),
				),
			));
		}
	}
	if !m.discovery_errors.is_empty() {
		out.push((
			"discovery_errors",
			Msg::new(
				"scan_errors_count",
				[m.discovery_errors.len().to_string()],
			),
		));
	}
	if let Some(msg) = m.failed_feeds_msg() {
		out.push(("failed_feeds", msg));
	}
	out
}

pub(crate) fn has_cjk(text: &str) -> bool {
	text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// The first run of two ASCII words in `text`, if any. A word is letters,
/// digits and hyphens that starts with a letter, so `snip-sync` and `ssh`
/// are words, and a lone product term between Chinese is not a run.
pub(crate) fn english_run(text: &str) -> Option<String> {
	use std::sync::LazyLock;
	static RUN: LazyLock<regex::Regex> = LazyLock::new(|| {
		regex::Regex::new(
			r"[A-Za-z][A-Za-z0-9-]*(?:[ \t]+[A-Za-z][A-Za-z0-9-]*)+",
		)
		.expect("a fixed pattern")
	});
	RUN.find(text).map(|m| m.as_str().to_string())
}

/// One list of `field => Family` builds both the exhaustive destructure and
/// the table, so a field cannot be in one and missing from the other.
#[cfg(test)]
macro_rules! classify_fields {
	($m:ident; $($field:ident => $family:ident),+ $(,)?) => {{
		let WorkbenchModel { $($field: _),+ } = $m;
		vec![$((stringify!($field), Family::$family)),+]
	}};
}

/// Every field of the model, by the family that owns it. The destructure
/// has no `..`: a field added to the model does not build until it is named
/// here, so a new piece of state cannot skip the question of what invariant
/// ties it to the rest.
#[cfg(test)]
pub(crate) fn classify(m: &WorkbenchModel) -> Vec<(&'static str, Family)> {
	classify_fields! {
		m;
		workspace_root => Config,
		restore_dir => Config,
		repos => RepoSet,
		selected_repo_idx => RepoSet,
		commits => Source,
		refs => Source,
		head_sha => Source,
		graph_layout => Source,
		active_ref_filter => Source,
		commit_page => Source,
		history_has_more => Source,
		history_page_size => Config,
		history_error => Messages,
		page_checkpoints => Source,
		log_search => Source,
		collapsed_merges => Source,
		hidden_commits => Source,
		history_walk => Source,
		selected_commit => LogSelection,
		range_head => LogSelection,
		log_selected => LogSelection,
		log_scroll => Source,
		select_head_after_load => Source,
		log_first_page => Source,
		history_extending => Loading,
		history_loaded => Source,
		history_autoload => Source,
		log_on_head => Source,
		log_filter => Source,
		log_path_input => Input,
		log_branch_menu_input => Input,
		log_branch_menu_open => Source,
		log_since_input => Input,
		log_until_input => Input,
		log_date_error => Source,
		log_paths_expanded => Source,
		branch_filter_input => Input,
		branch_groups_collapsed => Source,
		log_branches_visible => Layout,
		log_details_visible => Layout,
		log_show_hash => Layout,
		log_details_w => Layout,
		log_width => Layout,
		log_filter_layout => Layout,
		log_fit => Layout,
		changed_dirs_collapsed => Source,
		log_details_by_dir => Layout,
		commit_rows_cache => Cache,
		log_details_h => Layout,
		log_selection_expanded => Source,
		log_branches_all => Source,
		details_generation => Generation,
		details_cancel => Cancel,
		git_user_email => Source,
		commit_view => LogSelection,
		compare => LogSelection,
		files => Source,
		changes_loaded => Source,
		file_tree => Source,
		ws_tree => Source,
		ws_home => Source,
		rev_tree => Source,
		active_tab => Source,
		selected_file => Source,
		selected_file_source => Source,
		selected_file_root => Source,
		tree_cursor => Source,
		selected_list_row => Source,
		preview => Preview,
		preview_loading => Preview,
		held_selection => Preview,
		preview_pending_from_log => Preview,
		preview_error => Preview,
		reader => Cache,
		find_input => Input,
		goto_input => Input,
		log_search_input => Input,
		selector_input => Input,
		overlays => Overlays,
		tree_selections => Source,
		history_cancel => Cancel,
		preview_cancel => Preview,
		tree_cancel => Cancel,
		rev_tree_cancel => Cancel,
		repo_cancel => Cancel,
		scan_cancel => Cancel,
		copy_cancel => Cancel,
		paste_diff_cancel => Cancel,
		paste_diff_generation => Generation,
		discovery => Source,
		discovery_status => Source,
		discovery_errors => Source,
		discovery_depth_limited => Source,
		discovery_error_overflow => Source,
		discovery_depth_overflow => Source,
		discovery_generation => Generation,
		pinned_repo => RepoSet,
		carried_repo => Source,
		refresh_reload => Source,
		manual_repos => Source,
		tree_queue => Source,
		tree_worker => Generation,
		tree_worker_alive => Source,
		restore_expanded => Source,
		restore_ws_expanded => Source,
		add_cancel => Cancel,
		add_repo_input => Input,
		paste => PastePanel,
		status => Messages,
		user_action_seq => Generation,
		status_error_action => Generation,
		toast => Messages,
		toast_seq => Generation,
		is_loading => Loading,
		is_copying => Loading,
		locale => Config,
		generation => Generation,
		preview_generation => Generation,
		history_generation => Generation,
		history_replace_loads => TestSeam,
		tree_generation => Generation,
		mode => Config,
		focus_handle => Focus,
		reader_focus => Focus,
		tree_focus => Focus,
		left_active => Focus,
		log_active => Focus,
		reader_active => Focus,
		log_focus => Focus,
		paste_focus => Focus,
		panel_focus => Focus,
		find_bar_focus => Focus,
		left_w => Layout,
		bottom_h => Layout,
		left_visible => Layout,
		bottom_visible => Layout,
		dragging => Layout,
		last_viewport => Layout,
		viewport_h => Layout,
		probes => TestSeam,
		pending_focus => Focus,
		focus_lost_guard => Focus,
		escape_guard => Focus,
		e2e_read_delay => TestSeam,
		e2e_log_read_delay => TestSeam,
		e2e_tree_hold => TestSeam,
		e2e_export_hold => TestSeam,
		e2e_remote_open_delay => TestSeam,
		e2e_copy_hold => TestSeam,
		e2e_paste_diff_hold => TestSeam,
		test_clipboard => TestSeam,
		workspace_open => Source,
		workspace_path_input => Input,
		paste_dest_input => Input,
		recent_workspaces => Config,
		lifecycle => Loading,
		watch_running => Source,
		last_life_log => Cache,
		repo_collapsed => Layout,
		ws_collapsed => Layout,
		chrome => Source,
		change_repos => RepoSet,
		changes_queue => RepoSet,
		changes_cancel => Cancel,
		changes_generation => Generation,
		last_changes_empty => Cache,
		last_log_empty => Cache,
		last_branch_current => Cache,
		invariants_logged => Cache,
		preview_error_root => Preview,
		log_repo_filter => Source,
		log_scope_key => LogScope,
		log_feeds => LogScope,
		log_deferred => LogScope,
		log_reload_for => LogScope,
		remote => RemoteMenu,
		remote_path_input => Input,
		ws_tab => Config,
		top_inset => Layout,
		resume_focus => Focus,
	}
}
