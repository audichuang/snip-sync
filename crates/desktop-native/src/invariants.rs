//! The registry of the model's derived-state invariants (#230).
//!
//! Each member names a property that must hold between two views of the
//! same fact: the Log selection and the commit view, the repo set and every
//! consumer of it, a layer and its panel. A property that held once, in a
//! test written for one path, is not enough: any other path to the same
//! state can break it. So the properties live here, once, are evaluated by
//! the same `eval` everywhere, and are driven by the `fuzz_` lanes in
//! `main.rs` `tests::in_process::fuzz` over random gestures.
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
	/// No selection, no commit view, no Log preview. Guard: #202 (the repo
	/// filter's hand-made reset left a preview behind).
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
	/// The counters that drop a landing result (`gen.rs`).
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
			let listed = view.commit_files.iter().any(|(p, _)| p == open);
			(!listed && m.rev_tree.is_none()).then(|| {
				format!("the open commit file {open:?} is not in the list")
			})
		}
		L3 => {
			if m.rev_tree.is_some() {
				return None;
			}
			view.commit_file_sel.iter().find_map(|sel| {
				let listed = view.commit_files.iter().any(|(p, _)| {
					p == sel
						|| sel.strip_suffix('/').is_some_and(|dir| {
							p.starts_with(&format!("{dir}/"))
						})
				});
				(!listed).then(|| {
					format!("{sel:?} is selected and not in the file list")
				})
			})
		}
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
			if !view.is_reset() {
				return Some("no selection, but a commit view is left".into());
			}
			if m.preview_pending_from_log {
				return Some(
					"no selection, but the preview is marked from the Log"
						.into(),
				);
			}
			let from_log = m.preview.as_ref().is_some_and(|p| {
				matches!(
					p.source,
					PreviewSource::CommitDiff { .. }
						| PreviewSource::CommitFile { .. }
						| PreviewSource::Compare { .. }
				)
			});
			// A file of the rev tree is shown from the Log without a selected
			// row: its tree is the selection.
			(from_log && m.rev_tree.is_none()).then(|| {
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

/// Every field of the model, by the family that owns it. The destructure
/// has no `..`: a field added to the model does not build until it is named
/// here, so a new piece of state cannot skip the question of what invariant
/// ties it to the rest.
#[cfg(test)]
pub(crate) fn classify(m: &WorkbenchModel) -> Vec<(&'static str, Family)> {
	let WorkbenchModel {
		workspace_root: _,
		restore_dir: _,
		repos: _,
		selected_repo_idx: _,
		commits: _,
		refs: _,
		head_sha: _,
		graph_layout: _,
		active_ref_filter: _,
		commit_page: _,
		history_has_more: _,
		history_page_size: _,
		history_error: _,
		page_checkpoints: _,
		log_search: _,
		collapsed_merges: _,
		hidden_commits: _,
		history_walk: _,
		selected_commit: _,
		range_head: _,
		log_selected: _,
		log_scroll: _,
		select_head_after_load: _,
		log_first_page: _,
		history_extending: _,
		history_loaded: _,
		history_autoload: _,
		log_on_head: _,
		log_filter: _,
		log_path_input: _,
		log_branch_menu_input: _,
		log_branch_menu_open: _,
		log_since_input: _,
		log_until_input: _,
		log_date_error: _,
		log_paths_expanded: _,
		branch_filter_input: _,
		branch_groups_collapsed: _,
		log_branches_visible: _,
		log_details_visible: _,
		log_show_hash: _,
		log_details_w: _,
		log_width: _,
		log_filter_layout: _,
		log_fit: _,
		changed_dirs_collapsed: _,
		log_details_by_dir: _,
		commit_rows_cache: _,
		log_details_h: _,
		log_selection_expanded: _,
		log_branches_all: _,
		details_generation: _,
		details_cancel: _,
		git_user_email: _,
		commit_view: _,
		compare: _,
		files: _,
		changes_loaded: _,
		file_tree: _,
		ws_tree: _,
		ws_home: _,
		rev_tree: _,
		active_tab: _,
		selected_file: _,
		selected_file_source: _,
		selected_file_root: _,
		tree_cursor: _,
		selected_list_row: _,
		preview: _,
		preview_loading: _,
		held_selection: _,
		preview_pending_from_log: _,
		preview_error: _,
		reader: _,
		find_input: _,
		goto_input: _,
		log_search_input: _,
		selector_input: _,
		overlays: _,
		tree_selections: _,
		history_cancel: _,
		preview_cancel: _,
		tree_cancel: _,
		rev_tree_cancel: _,
		repo_cancel: _,
		scan_cancel: _,
		copy_cancel: _,
		paste_diff_cancel: _,
		paste_diff_generation: _,
		discovery: _,
		discovery_status: _,
		discovery_errors: _,
		discovery_depth_limited: _,
		discovery_error_overflow: _,
		discovery_depth_overflow: _,
		discovery_generation: _,
		pinned_repo: _,
		carried_repo: _,
		refresh_reload: _,
		manual_repos: _,
		tree_queue: _,
		tree_worker: _,
		tree_worker_alive: _,
		restore_expanded: _,
		restore_ws_expanded: _,
		add_cancel: _,
		add_repo_input: _,
		paste: _,
		status: _,
		user_action_seq: _,
		status_error_action: _,
		toast: _,
		toast_seq: _,
		is_loading: _,
		is_copying: _,
		locale: _,
		generation: _,
		preview_generation: _,
		history_generation: _,
		history_replace_loads: _,
		tree_generation: _,
		mode: _,
		focus_handle: _,
		reader_focus: _,
		tree_focus: _,
		left_active: _,
		log_active: _,
		reader_active: _,
		log_focus: _,
		paste_focus: _,
		panel_focus: _,
		find_bar_focus: _,
		left_w: _,
		bottom_h: _,
		left_visible: _,
		bottom_visible: _,
		dragging: _,
		last_viewport: _,
		viewport_h: _,
		probes: _,
		pending_focus: _,
		focus_lost_guard: _,
		escape_guard: _,
		e2e_read_delay: _,
		e2e_log_read_delay: _,
		e2e_tree_hold: _,
		e2e_export_hold: _,
		e2e_remote_open_delay: _,
		e2e_copy_hold: _,
		e2e_paste_diff_hold: _,
		test_clipboard: _,
		workspace_open: _,
		workspace_path_input: _,
		paste_dest_input: _,
		recent_workspaces: _,
		lifecycle: _,
		watch_running: _,
		last_life_log: _,
		repo_collapsed: _,
		ws_collapsed: _,
		chrome: _,
		change_repos: _,
		changes_queue: _,
		changes_cancel: _,
		changes_generation: _,
		last_changes_empty: _,
		last_log_empty: _,
		last_branch_current: _,
		invariants_logged: _,
		preview_error_root: _,
		log_repo_filter: _,
		log_scope_key: _,
		log_feeds: _,
		log_deferred: _,
		log_reload_for: _,
		remote: _,
		remote_path_input: _,
		ws_tab: _,
		top_inset: _,
		resume_focus: _,
	} = m;
	vec![
		("workspace_root", Family::Config),
		("restore_dir", Family::Config),
		("repos", Family::RepoSet),
		("selected_repo_idx", Family::RepoSet),
		("commits", Family::Source),
		("refs", Family::Source),
		("head_sha", Family::Source),
		("graph_layout", Family::Source),
		("active_ref_filter", Family::Source),
		("commit_page", Family::Source),
		("history_has_more", Family::Source),
		("history_page_size", Family::Config),
		("history_error", Family::Messages),
		("page_checkpoints", Family::Source),
		("log_search", Family::Source),
		("collapsed_merges", Family::Source),
		("hidden_commits", Family::Source),
		("history_walk", Family::Source),
		("selected_commit", Family::LogSelection),
		("range_head", Family::LogSelection),
		("log_selected", Family::LogSelection),
		("log_scroll", Family::Source),
		("select_head_after_load", Family::Source),
		("log_first_page", Family::Source),
		("history_extending", Family::Loading),
		("history_loaded", Family::Source),
		("history_autoload", Family::Source),
		("log_on_head", Family::Source),
		("log_filter", Family::Source),
		("log_path_input", Family::Input),
		("log_branch_menu_input", Family::Input),
		("log_branch_menu_open", Family::Source),
		("log_since_input", Family::Input),
		("log_until_input", Family::Input),
		("log_date_error", Family::Source),
		("log_paths_expanded", Family::Source),
		("branch_filter_input", Family::Input),
		("branch_groups_collapsed", Family::Source),
		("log_branches_visible", Family::Layout),
		("log_details_visible", Family::Layout),
		("log_show_hash", Family::Layout),
		("log_details_w", Family::Layout),
		("log_width", Family::Layout),
		("log_filter_layout", Family::Layout),
		("log_fit", Family::Layout),
		("changed_dirs_collapsed", Family::Source),
		("log_details_by_dir", Family::Layout),
		("commit_rows_cache", Family::Cache),
		("log_details_h", Family::Layout),
		("log_selection_expanded", Family::Source),
		("log_branches_all", Family::Source),
		("details_generation", Family::Generation),
		("details_cancel", Family::Cancel),
		("git_user_email", Family::Source),
		("commit_view", Family::LogSelection),
		("compare", Family::LogSelection),
		("files", Family::Source),
		("changes_loaded", Family::Source),
		("file_tree", Family::Source),
		("ws_tree", Family::Source),
		("ws_home", Family::Source),
		("rev_tree", Family::Source),
		("active_tab", Family::Source),
		("selected_file", Family::Source),
		("selected_file_source", Family::Source),
		("selected_file_root", Family::Source),
		("tree_cursor", Family::Source),
		("selected_list_row", Family::Source),
		("preview", Family::Preview),
		("preview_loading", Family::Preview),
		("held_selection", Family::Preview),
		("preview_pending_from_log", Family::Preview),
		("preview_error", Family::Preview),
		("reader", Family::Cache),
		("find_input", Family::Input),
		("goto_input", Family::Input),
		("log_search_input", Family::Input),
		("selector_input", Family::Input),
		("overlays", Family::Overlays),
		("tree_selections", Family::Source),
		("history_cancel", Family::Cancel),
		("preview_cancel", Family::Preview),
		("tree_cancel", Family::Cancel),
		("rev_tree_cancel", Family::Cancel),
		("repo_cancel", Family::Cancel),
		("scan_cancel", Family::Cancel),
		("copy_cancel", Family::Cancel),
		("paste_diff_cancel", Family::Cancel),
		("paste_diff_generation", Family::Generation),
		("discovery", Family::Source),
		("discovery_status", Family::Source),
		("discovery_errors", Family::Source),
		("discovery_depth_limited", Family::Source),
		("discovery_error_overflow", Family::Source),
		("discovery_depth_overflow", Family::Source),
		("discovery_generation", Family::Generation),
		("pinned_repo", Family::RepoSet),
		("carried_repo", Family::Source),
		("refresh_reload", Family::Source),
		("manual_repos", Family::Source),
		("tree_queue", Family::Source),
		("tree_worker", Family::Generation),
		("tree_worker_alive", Family::Source),
		("restore_expanded", Family::Source),
		("restore_ws_expanded", Family::Source),
		("add_cancel", Family::Cancel),
		("add_repo_input", Family::Input),
		("paste", Family::PastePanel),
		("status", Family::Messages),
		("user_action_seq", Family::Generation),
		("status_error_action", Family::Generation),
		("toast", Family::Messages),
		("toast_seq", Family::Generation),
		("is_loading", Family::Loading),
		("is_copying", Family::Loading),
		("locale", Family::Config),
		("generation", Family::Generation),
		("preview_generation", Family::Generation),
		("history_generation", Family::Generation),
		("history_replace_loads", Family::TestSeam),
		("tree_generation", Family::Generation),
		("mode", Family::Config),
		("focus_handle", Family::Focus),
		("reader_focus", Family::Focus),
		("tree_focus", Family::Focus),
		("left_active", Family::Focus),
		("log_active", Family::Focus),
		("reader_active", Family::Focus),
		("log_focus", Family::Focus),
		("paste_focus", Family::Focus),
		("panel_focus", Family::Focus),
		("find_bar_focus", Family::Focus),
		("left_w", Family::Layout),
		("bottom_h", Family::Layout),
		("left_visible", Family::Layout),
		("bottom_visible", Family::Layout),
		("dragging", Family::Layout),
		("last_viewport", Family::Layout),
		("viewport_h", Family::Layout),
		("probes", Family::TestSeam),
		("pending_focus", Family::Focus),
		("focus_lost_guard", Family::Focus),
		("escape_guard", Family::Focus),
		("e2e_read_delay", Family::TestSeam),
		("e2e_log_read_delay", Family::TestSeam),
		("e2e_tree_hold", Family::TestSeam),
		("e2e_export_hold", Family::TestSeam),
		("e2e_remote_open_delay", Family::TestSeam),
		("e2e_copy_hold", Family::TestSeam),
		("e2e_paste_diff_hold", Family::TestSeam),
		("test_clipboard", Family::TestSeam),
		("workspace_open", Family::Source),
		("workspace_path_input", Family::Input),
		("paste_dest_input", Family::Input),
		("recent_workspaces", Family::Config),
		("lifecycle", Family::Loading),
		("watch_running", Family::Source),
		("last_life_log", Family::Cache),
		("repo_collapsed", Family::Layout),
		("ws_collapsed", Family::Layout),
		("chrome", Family::Source),
		("change_repos", Family::RepoSet),
		("changes_queue", Family::RepoSet),
		("changes_cancel", Family::Cancel),
		("changes_generation", Family::Generation),
		("last_changes_empty", Family::Cache),
		("last_log_empty", Family::Cache),
		("last_branch_current", Family::Cache),
		("invariants_logged", Family::Cache),
		("preview_error_root", Family::Preview),
		("log_repo_filter", Family::Source),
		("log_scope_key", Family::LogScope),
		("log_feeds", Family::LogScope),
		("log_deferred", Family::LogScope),
		("log_reload_for", Family::LogScope),
		("remote", Family::RemoteMenu),
		("remote_path_input", Family::Input),
		("ws_tab", Family::Config),
		("top_inset", Family::Layout),
		("resume_focus", Family::Focus),
	]
}
