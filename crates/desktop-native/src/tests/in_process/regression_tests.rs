//! Deterministic reproductions of what the `fuzz_` lanes of #230 found: each
//! is rebuilt with gestures and holds, never with a seed (a seed does not
//! survive a `rand` upgrade, and a remote worker's reply times are not in it).

use super::*;
use crate::invariants::{self, Invariant::*};
use crate::PreviewSource;
use gpui::TestAppContext;

/// D1: a message on screen reads as zh-TW wording. Pasting text that is no
/// payload showed "…有效的 snip-sync payload": two English words in a zh
/// message (found by `fuzz_local_workbench_keeps_derived_state`).
#[gpui::test]
fn fuzz_regression_a_pasted_non_payload_is_worded_in_zh(
	cx: &mut TestAppContext,
) {
	let ws = tempfile::tempdir().unwrap();
	repo(ws.path(), "alpha", &[]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle(cx);
	model.update(cx, |m, _| {
		m.test_clipboard = Some("just some text, no payload".into())
	});
	cx.simulate_keystrokes("ctrl-v");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert_eq!(m.status.key, "paste_err_not_payload", "{:?}", m.status);
		invariants::assert_holds(m, cx, &[D1]);
	});
}

/// The class behind it: no zh-TW entry of the dictionary carries a run of
/// two English words (a product term alone, like `Git` or `HEAD`, is fine).
/// Read from the source of the dictionary, so an entry no state reaches yet
/// is held too.
#[test]
fn fuzz_regression_no_zh_entry_carries_an_english_run() {
	let source = include_str!("../../i18n.rs");
	let zh = source
		.split("Locale::ZhTw => match key {")
		.nth(1)
		.and_then(|rest| rest.split("Locale::En => match key {").next())
		.expect("the zh-TW arm of `t`");
	let mut checked = 0;
	let mut leaks = Vec::new();
	for line in zh.lines() {
		let Some((key, text)) = line.trim().split_once("\" => \"") else {
			continue;
		};
		let key = key.trim_start_matches('"');
		let text = text.trim_end_matches(',').trim_end_matches('"');
		checked += 1;
		// A placeholder stands for data.
		let shape = text
			.replace("{0}", "\u{25c7}")
			.replace("{1}", "\u{25c7}")
			.replace("{2}", "\u{25c7}")
			.replace("{}", "\u{25c7}");
		if let Some(run) = invariants::english_run(&shape) {
			leaks.push(format!("{key}: {text} ({run})"));
		}
	}
	assert!(checked > 400, "the scan read only {checked} entries");
	assert!(
		leaks.is_empty(),
		"zh-TW entries with English wording:\n{}",
		leaks.join("\n")
	);
}

/// O7: an inline layer is drawn exactly when its pane is. The add-repo field
/// belongs to the Project pane, which the user can hide by pressing its rail
/// button again: the field is then open and not drawn, and that is right,
/// not a stray layer: the pane brings it back as it was. The check assumed
/// the pane is drawn whenever the Project tab is active (found by
/// `fuzz_local_workbench_keeps_derived_state`).
#[gpui::test]
fn fuzz_regression_a_hidden_project_pane_keeps_its_add_repo_field_open(
	cx: &mut TestAppContext,
) {
	let ws = tempfile::tempdir().unwrap();
	repo(ws.path(), "alpha", &[]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle(cx);
	cx.simulate_keystrokes("alt-1");
	settle(cx);
	click_probed(&model, cx, "btn-add-repo");
	let open = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| {
			m.overlays
				.is_open(crate::overlay::OverlayKind::AddRepoField)
		})
	};
	assert!(open(cx), "the field opens");
	overlays::assert_overlay_panels_match(cx, &model);
	click_probed(&model, cx, "rail-project");
	assert!(open(cx), "hiding the pane leaves the field open");
	assert!(
		cx.debug_bounds("input-add-repo").is_none(),
		"a hidden pane draws nothing"
	);
	model.read_with(cx, |m, _| assert!(!m.left_visible));
	overlays::assert_overlay_panels_match(cx, &model);
	// Shown again, the field is where it was left.
	click_probed(&model, cx, "rail-project");
	assert!(open(cx));
	assert!(cx.debug_bounds("input-add-repo").is_some());
	overlays::assert_overlay_panels_match(cx, &model);
}

/// A fixture whose first Log row is selected and shown.
fn commit_shown(
	cx: &mut TestAppContext,
) -> (
	tempfile::TempDir,
	Entity<WorkbenchModel>,
	&mut VisualTestContext,
) {
	commit_shown_with(cx, false)
}

/// `commit_shown`, and with `b_on_top` a newer commit that changes only
/// `b.txt`: the shown commit's change list then lacks `a.txt`, which its
/// tree still has.
fn commit_shown_with(
	cx: &mut TestAppContext,
	b_on_top: bool,
) -> (
	tempfile::TempDir,
	Entity<WorkbenchModel>,
	&mut VisualTestContext,
) {
	commit_shown_with_files(cx, b_on_top, &[])
}

/// `commit_shown_with`, and `extra` files (name, bytes) committed with `a.txt`.
fn commit_shown_with_files<'a>(
	cx: &'a mut TestAppContext,
	b_on_top: bool,
	extra: &[(&str, &[u8])],
) -> (
	tempfile::TempDir,
	Entity<WorkbenchModel>,
	&'a mut VisualTestContext,
) {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	fs::write(alpha.join("a.txt"), "one").unwrap();
	for (name, bytes) in extra {
		fs::write(alpha.join(name), bytes).unwrap();
	}
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add a"]);
	if b_on_top {
		fs::write(alpha.join("b.txt"), "two").unwrap();
		git(&alpha, &["add", "."]);
		git(&alpha, &["commit", "-q", "-m", "add b"]);
	}
	repo(ws.path(), "beta", &[]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| !m.commits.is_empty() && m.repos.len() == 2)
	});
	// Probes on, then the first row the Log drew.
	click_probed(&model, cx, "btn-log-refresh");
	let row = model.read_with(cx, |m, _| {
		// The newest commit's row, when the fixture has a `b.txt` one.
		let wanted = m
			.commits
			.iter()
			.find(|c| b_on_top && c.subject == "add b")
			.map(|c| c.sha.get(..7).unwrap_or(&c.sha).to_string());
		m.probes
			.as_ref()
			.unwrap()
			.drawn()
			.into_iter()
			.find(|id| {
				id.starts_with("commit-row:")
					&& wanted.as_ref().is_none_or(|w| id.ends_with(w.as_str()))
			})
			.expect("the Log drew a row")
	});
	click_probed(&model, cx, &row);
	settle_until(cx, "the commit's diff", |cx| {
		model.read_with(cx, |m, _| {
			m.selected_commit.is_some()
				&& m.preview.is_some()
				&& !m.preview_loading
		})
	});
	model.read_with(cx, |m, _| assert!(m.preview_pending_from_log));
	(ws, model, cx)
}

/// L6: with no commit selected, nothing of the Log owns the preview. Closing
/// the editor tab drops the selection by hand (the way `apply_log_repo_filter`
/// did before #202) and kept the flag that says the Log's read is the latest
/// (found by `fuzz_local_workbench_keeps_derived_state`).
#[gpui::test]
fn fuzz_regression_closing_the_tab_leaves_the_log_no_preview_to_own(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	model.read_with(cx, |m, cx| {
		assert!(m.selected_commit.is_none());
		invariants::assert_holds(m, cx, &[L6]);
	});
}

/// The same class: releasing the workspace's state drops the selection too.
#[gpui::test]
fn fuzz_regression_releasing_the_workspace_leaves_the_log_no_preview_to_own(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	model.update_in(cx, |m, _, cx| m.release_workspace_state(cx));
	model.read_with(cx, |m, cx| {
		assert!(m.selected_commit.is_none());
		invariants::assert_holds(m, cx, &[L6]);
	});
}

/// L6, L8: closing the tab while the selected commit's details are still
/// being read must drop that read. `close_tab` cleared the selection by hand
/// without moving `details_generation`, so the details landed on a Log with
/// nothing selected (found by `fuzz_local_workbench_keeps_derived_state`:
/// select, next page, close the tab).
#[gpui::test]
fn fuzz_regression_a_details_read_in_flight_when_the_tab_closes_is_dropped(
	cx: &mut TestAppContext,
) {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	fs::write(alpha.join("a.txt"), "one").unwrap();
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add a"]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| !m.commits.is_empty())
	});
	// One turn of the event loop: the reads the click starts have not run
	// when the tab closes.
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[0].sha.clone();
		m.select_commit(&sha, cx);
		assert!(m.selected_commit.is_some());
		m.close_tab(0, cx);
	});
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.selected_commit.is_none());
		invariants::assert_holds(m, cx, &[L6, L8]);
	});
}

/// L6: switching the open repo drops the Log's selection and reads the repo
/// anew; the Log's claim on the latest preview read goes with it. The switch
/// cleared `selected_commit` and kept `preview_pending_from_log` (found by
/// `fuzz_local_workbench_keeps_derived_state`: Log row, then another repo).
#[gpui::test]
fn fuzz_regression_switching_the_repo_leaves_the_log_no_preview_to_own(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	model.read_with(cx, |m, _| assert!(m.selected_repo_idx.is_some()));
	// The other repo than the open one.
	let other = model.read_with(cx, |m, _| 1 - m.selected_repo_idx.unwrap());
	model.read_with(cx, |m, _| {
		assert!(m.selected_commit.is_some() && m.preview_pending_from_log)
	});
	model.update_in(cx, |m, _, cx| m.select_repo(other, cx));
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(
			m.selected_commit.is_none(),
			"the switch drops the selection"
		);
		invariants::assert_holds(m, cx, &[L6]);
	});
}

/// O7: a Log dropdown is part of the Log panel, so hiding the panel closes
/// it. Alt+9 (and the panel's hide button, Shift+Esc in the Log, and the
/// paste preview taking the Log's place) left the dropdown open over no panel
/// (found by `fuzz_local_workbench_keeps_derived_state`: open the Date
/// chip, then Alt+9).
#[gpui::test]
fn fuzz_regression_hiding_the_log_closes_its_dropdown(cx: &mut TestAppContext) {
	use crate::overlay::OverlayKind;
	use crate::ui::LogMenu;
	let (_ws, model, cx) = commit_shown(cx);
	let dropdown_open = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| {
			m.overlays.is_open(OverlayKind::LogMenu(LogMenu::Date))
		})
	};
	for hide in ["alt-9", "button", "shift-escape", "paste"] {
		click_probed(&model, cx, "log-filter-date");
		assert!(dropdown_open(cx), "{hide}: the dropdown opens");
		overlays::assert_overlay_panels_match(cx, &model);
		match hide {
			"alt-9" => cx.simulate_keystrokes("alt-9"),
			"button" => click_probed(&model, cx, "btn-log-hide"),
			"shift-escape" => {
				model.update_in(cx, |m, window, _| window.focus(&m.log_focus));
				cx.simulate_keystrokes("shift-escape")
			}
			_ => {
				model.update(cx, |m, _| {
					m.test_clipboard = Some("// FILE: x.txt\nx\n".into())
				});
				cx.simulate_keystrokes("ctrl-v");
			}
		}
		settle(cx);
		model.read_with(cx, |m, _| {
			assert!(!m.bottom_visible.get(), "{hide}: the Log is hidden")
		});
		assert!(!dropdown_open(cx), "{hide}: its dropdown went with it");
		overlays::assert_overlay_panels_match(cx, &model);
		// Back to a bare state for the next way to hide it.
		model.update_in(cx, |m, _, cx| m.cancel_paste_preview(cx));
		model.update_in(cx, |m, _, cx| {
			m.set_log_visible(true);
			cx.notify();
		});
		settle(cx);
	}
}

/// L6, L8: switching the open repo drops the Log's selection, and so must
/// drop the details read of the commit that was selected: it landed on a Log
/// with nothing selected (found by `fuzz_local_workbench_keeps_derived_state`:
/// right-click a row, then a repo chevron).
#[gpui::test]
fn fuzz_regression_switching_the_repo_drops_the_details_read_in_flight(
	cx: &mut TestAppContext,
) {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	fs::write(alpha.join("a.txt"), "one").unwrap();
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add a"]);
	repo(ws.path(), "beta", &[]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log and both repos", |cx| {
		model.read_with(cx, |m, _| {
			!m.commits.is_empty() && m.repos.len() == 2 && !m.is_loading
		})
	});
	// One turn of the event loop: the details read has not run when the
	// repo changes.
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[0].sha.clone();
		m.select_commit(&sha, cx);
		assert!(m.selected_commit.is_some());
		let other = 1 - m.selected_repo_idx.expect("a repo is open");
		m.select_repo(other, cx);
		assert!(
			m.selected_commit.is_none(),
			"the switch drops the selection"
		);
	});
	settle(cx);
	model.read_with(cx, |m, cx| {
		invariants::assert_holds(m, cx, &[L6, L8]);
	});
}

/// L6, L8: opening a file takes the Log's selection off the model (its
/// preview is replaced), and the details read of the commit that was selected
/// must not land on the Log it left: it did, on a view with no selection
/// (found by `fuzz_local_workbench_keeps_derived_state`: click a Log row, then
/// a Changes row before the details arrive).
#[gpui::test]
fn fuzz_regression_a_details_read_does_not_land_on_a_selection_a_file_took(
	cx: &mut TestAppContext,
) {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	fs::write(alpha.join("a.txt"), "one").unwrap();
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add a"]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| !m.commits.is_empty() && !m.is_loading)
	});
	// One turn of the event loop: the details read has not run when the file
	// is opened.
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[0].sha.clone();
		m.select_commit(&sha, cx);
		assert!(m.selected_commit.is_some());
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m.selected_commit.is_none(), "the file takes the selection");
	});
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.selected_commit.is_none());
		invariants::assert_holds(m, cx, &[L6, L8]);
	});
}

/// R6: a repo whose own load was in flight when another repo was opened is
/// read by the Changes queue instead, so its node never stays Loading. The
/// "previous repo" it is found by was read after `select_repo` had pinned the
/// new one, and was always `None` (found by
/// `fuzz_local_workbench_keeps_derived_state`: open beta, open gamma at once).
#[gpui::test]
fn fuzz_regression_the_repo_left_while_loading_is_read_by_the_changes_queue(
	cx: &mut TestAppContext,
) {
	let ws = tempfile::tempdir().unwrap();
	for name in ["alpha", "beta", "gamma"] {
		repo(ws.path(), name, &[]);
	}
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "every repo's Changes", |cx| {
		model.read_with(cx, |m, _| {
			m.repos.len() == 3
				&& m.change_repos.len() == 3
				&& m.change_repos
					.iter()
					.all(|r| r.state == crate::ChangeRepoState::Loaded)
		})
	});
	// One turn: beta's own read has not run when gamma is opened.
	model.update_in(cx, |m, _, cx| {
		m.select_repo(1, cx);
		m.select_repo(2, cx);
	});
	// Bounded on the queue going idle, not on the node leaving Loading: a
	// repo nothing reads stays Loading for good, and R6 must say so rather
	// than the wait timing out.
	settle_until(cx, "the Changes queue to drain", |cx| {
		model.read_with(cx, |m, _| m.changes_queue.is_idle())
	});
	model.read_with(cx, |m, cx| {
		invariants::assert_holds(m, cx, &[R6]);
	});
}

/// O7: the reader's find bar is drawn exactly when the reader is. The paste
/// preview takes the reader's place, so the find bar is open and not drawn
/// while a preview is open, and comes back as it was when the preview closes:
/// not a stray layer. The check assumed the reader is always there (found by
/// `fuzz_paste_preview_follows_the_repo_set`: Ctrl+F, then a paste).
#[gpui::test]
fn fuzz_regression_a_paste_preview_hides_the_reader_and_keeps_its_find_bar_open(
	cx: &mut TestAppContext,
) {
	use crate::overlay::OverlayKind;
	let ws = tempfile::tempdir().unwrap();
	repo(ws.path(), "alpha", &[]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle(cx);
	model.update_in(cx, |m, _, cx| {
		m.select_file_with_source("base.txt", crate::SourceKind::File, cx)
	});
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	cx.simulate_keystrokes("ctrl-f");
	settle(cx);
	let open = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| m.overlays.is_open(OverlayKind::FindBar))
	};
	assert!(open(cx), "the find bar opens");
	assert!(cx.debug_bounds("find-bar").is_some());
	overlays::assert_overlay_panels_match(cx, &model);

	model.update(cx, |m, _| {
		m.test_clipboard = Some("// FILE: x.txt\nx\n".into())
	});
	// The find field has the keyboard, and Ctrl+V there pastes into it.
	click_probed(&model, cx, "btn-paste");
	settle_until(cx, "the preview", |cx| {
		model.read_with(cx, |m, _| m.paste.plan().is_some())
	});
	assert!(open(cx), "the preview leaves the find bar open");
	assert!(cx.debug_bounds("find-bar").is_none(), "the reader is gone");
	overlays::assert_overlay_panels_match(cx, &model);

	model.update_in(cx, |m, _, cx| m.cancel_paste_preview(cx));
	settle(cx);
	assert!(open(cx));
	assert!(cx.debug_bounds("find-bar").is_some(), "the reader is back");
	overlays::assert_overlay_panels_match(cx, &model);
}

/// L6: with no commit selected the commit view is empty, except for the
/// file a rev tree has open: "Browse tree at this commit" outlives the
/// editor tab (its own button leaves it), and a file of it is selected in the
/// commit view with no commit selected. The check read that as a view left
/// behind (found by `fuzz_local_workbench_keeps_derived_state`: browse a
/// commit's tree, close the tab, open a file of the tree).
#[gpui::test]
fn fuzz_regression_a_rev_tree_file_is_not_a_commit_view_left_behind(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let rev_row = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with("rev-row:"))
		})
	};
	let browse = model.read_with(cx, |m, _| {
		m.probes
			.as_ref()
			.unwrap()
			.drawn()
			.into_iter()
			.find(|id| id.starts_with("btn-browse-tree:"))
			.expect("the commit offers its tree")
	});
	click_probed(&model, cx, &browse);
	settle_until(cx, "the tree", |cx| rev_row(cx).is_some());
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	model.read_with(cx, |m, _| {
		assert!(m.rev_tree.is_some(), "closing the tab leaves the tree");
		assert!(m.selected_commit.is_none());
	});
	let row = rev_row(cx).expect("a file of the tree");
	click_probed(&model, cx, &row);
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	model.read_with(cx, |m, cx| {
		assert!(m.selected_commit.is_none());
		assert!(m.commit_view.selection().is_some());
		invariants::assert_holds(m, cx, &[L2, L6]);
	});
	// "Back to Working" leaves the tree, not the editor tab: the file stays
	// open (the tab and the tree are independent, as closing the tab above
	// leaves the tree), and the view still accounts for it.
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview.is_some(), "the file's tab stays open");
		assert!(m.selected_commit.is_none());
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
	});
	// The tab is what owns the file now: closing it takes the selection.
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.preview.is_none());
		assert!(m.commit_view.selection().is_none());
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
	});
}

/// L2/L6: a working-file read that is still in flight holds the commit view
/// (with the tree's open file) aside. "Back to Working" then leaves the tree
/// while the live view is empty, and the ownership of the file has to be
/// recorded on the held view, or a refusal of the read (the retained budget)
/// restores a view nothing owns.
#[gpui::test]
fn fuzz_regression_leaving_a_rev_tree_while_its_file_is_held_keeps_the_owner(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let drawn_with = |cx: &mut VisualTestContext, prefix: &str| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with(prefix))
		})
	};
	let browse = drawn_with(cx, "btn-browse-tree:").expect("the tree");
	click_probed(&model, cx, &browse);
	settle_until(cx, "the tree", |cx| drawn_with(cx, "rev-row:").is_some());
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	let row = drawn_with(cx, "rev-row:").expect("a file of the tree");
	click_probed(&model, cx, &row);
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	// A working-file read starts and is held: it sets the selection aside.
	model.update(cx, |m, _| {
		m.e2e_read_delay = Some(std::time::Duration::from_secs(60))
	});
	model.update_in(cx, |m, _, cx| {
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m.held_selection.is_some(), "the selection is held");
		assert!(m.commit_view.selection().is_none());
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	// The read is refused by the retained budget and gives the selection back.
	model.update(cx, |m, _| m.restore_shown_selection());
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview.is_some(), "the file's tab stays open");
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
	});
}

/// L2/L6: the file a left rev tree kept is the tab's, and the tab alone
/// excuses it: another commit takes it with the rest of the view, and a view
/// that lost its owner but kept the preview is still caught.
#[gpui::test]
fn fuzz_regression_a_left_rev_tree_file_ends_with_the_next_selection(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let drawn_with = |cx: &mut VisualTestContext, prefix: &str| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with(prefix))
		})
	};
	// The commit's tree, a file of it open in the tab, then "Back to Working".
	let tree_file_then_leave = |cx: &mut VisualTestContext| {
		let browse = drawn_with(cx, "btn-browse-tree:").expect("the tree");
		click_probed(&model, cx, &browse);
		settle_until(cx, "the tree", |cx| drawn_with(cx, "rev-row:").is_some());
		model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
		settle(cx);
		let row = drawn_with(cx, "rev-row:").expect("a file of the tree");
		click_probed(&model, cx, &row);
		settle_until(cx, "the file", |cx| {
			model
				.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
		});
		click_probed(&model, cx, "btn-leave-tree");
		settle(cx);
	};
	tree_file_then_leave(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none() && m.selected_commit.is_none());
		assert!(m.commit_view.rev_owner().is_some(), "the tab is the owner");
		invariants::assert_holds(m, cx, &[L1, L2, L3, L4, L5, L6]);
	});
	// A view that lost its owner but kept a commit's preview is a leftover.
	model.update(cx, |m, _| {
		let mut sel = m.commit_view.selection().cloned().unwrap();
		sel.rev_owner = None;
		m.set_commit_file_selection(Some(sel));
	});
	model.read_with(cx, |m, cx| {
		let broken = invariants::check(m, cx, crate::invariants::When::Always);
		assert!(
			broken.iter().any(|v| v.id == L6),
			"a kept preview with no owner is caught"
		);
	});
	// Another commit takes the file's selection with the rest of the view.
	model.update(cx, |m, _| {
		let sha = m.commits[0].sha.clone();
		m.set_commit_file_selection(Some(
			crate::history::CommitFileSel::from_tree("a.txt", &sha),
		));
	});
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[0].sha.clone();
		m.select_commit(&sha, cx)
	});
	settle_until(cx, "the commit", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	model.read_with(cx, |m, cx| {
		assert!(m.commit_view.rev_owner().is_none(), "the owner went");
		invariants::assert_holds(m, cx, &[L1, L2, L3, L4, L5, L6]);
	});
}

/// L6: an open rev tree excuses only its own file. The selection a manual
/// reset leaves behind (a Log commit file, a file selection, the Log's
/// preview flag with no tree file selected) is still a violation while the
/// tree is open.
#[gpui::test]
fn fuzz_regression_an_open_rev_tree_excuses_only_its_own_file(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let rev_row = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with("rev-row:"))
		})
	};
	let browse = model.read_with(cx, |m, _| {
		m.probes
			.as_ref()
			.unwrap()
			.drawn()
			.into_iter()
			.find(|id| id.starts_with("btn-browse-tree:"))
			.expect("the commit offers its tree")
	});
	click_probed(&model, cx, &browse);
	settle_until(cx, "the tree", |cx| rev_row(cx).is_some());
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	let row = rev_row(cx).expect("a file of the tree");
	click_probed(&model, cx, &row);
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	let l6 = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, cx| {
			m.rev_tree.is_some().then(|| invariants::eval(L6, m, cx))
		})
	};
	assert_eq!(l6(cx), Some(None), "the tree's own file is legal");
	let eval = |cx: &mut VisualTestContext, id| {
		model.read_with(cx, |m, cx| invariants::eval(id, m, cx))
	};
	assert!(eval(cx, L2).is_none(), "L2 allows the tree's own file");

	// The flag with no tree file selected.
	let file = model.update(cx, |m, _| {
		let file = m.commit_view.selection().cloned().expect("a file");
		m.set_commit_file_selection(None);
		file
	});
	assert!(l6(cx).unwrap().is_some(), "a leftover flag is caught");
	model.update(cx, |m, _| {
		m.preview_pending_from_log = false;
	});
	assert_eq!(l6(cx), Some(None), "no selection, no flag");

	// A selected file the tree does not list and the preview is not.
	model.update(cx, |m, _| {
		m.set_commit_file_selection(Some("no/such/file".into()));
	});
	assert!(l6(cx).unwrap().is_some(), "a stranger's file is caught");
	assert!(eval(cx, L2).is_some(), "L2 catches a stranger's file too");
	model.update(cx, |m, _| {
		m.set_commit_file_selection(Some(file));
		m.commit_view.commit_file_sel = vec!["other".into()];
	});
	assert!(
		l6(cx).unwrap().is_some(),
		"a leftover file selection is caught"
	);
	assert!(
		eval(cx, L3).is_some(),
		"L3 catches a selection the tree does not list"
	);

	// A commit preview that is not the tree's own file, with a tree open.
	model.update(cx, |m, _| {
		m.commit_view.commit_file_sel.clear();
		m.set_commit_file_selection(None);
	});
	assert_eq!(l6(cx), Some(None), "the clean state holds");
	let own =
		model.update(cx, |m, _| m.preview.as_ref().unwrap().source.clone());
	for source in [
		PreviewSource::CommitDiff {
			sha: "deadbeef".into(),
		},
		PreviewSource::CommitFile {
			sha: "deadbeef".into(),
		},
	] {
		model.update(cx, |m, _| m.preview.as_mut().unwrap().source = source);
		assert!(
			l6(cx).unwrap().is_some(),
			"a stranger's commit preview is caught with a tree open"
		);
	}
	model.update(cx, |m, _| m.preview.as_mut().unwrap().source = own);
	assert_eq!(l6(cx), Some(None), "the tree's own file is legal again");

	// The tree's commit, but a file the tree does not list: a leftover.
	let own_path = model.update(cx, |m, _| {
		m.preview
			.as_mut()
			.unwrap()
			.path
			.replace("no/such/file".into())
	});
	assert!(
		l6(cx).unwrap().is_some(),
		"a preview of a file the tree does not list is caught"
	);
	model.update(cx, |m, _| m.preview.as_mut().unwrap().path = own_path);
	assert_eq!(l6(cx), Some(None), "the listed file is legal again");
}

/// A fixture with two commits in the open repo, the Log read.
fn two_commits(
	cx: &mut TestAppContext,
) -> (
	tempfile::TempDir,
	Entity<WorkbenchModel>,
	&mut VisualTestContext,
) {
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	fs::write(alpha.join("a.txt"), "one").unwrap();
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add a"]);
	fs::write(alpha.join("b.txt"), "two").unwrap();
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add b"]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| m.commits.len() >= 2 && !m.is_loading)
	});
	(ws, model, cx)
}

/// L6, L8: the details of a multi-selection must not land on the view when
/// a file took the selection while the read was in flight (the class of
/// `fuzz_regression_a_details_read_does_not_land_on_a_selection_a_file_took`,
/// for the multi-selection's list). Without the guard in
/// `view_of_multi_selection` the list lands on the live view, which then
/// outlives the selection.
#[gpui::test]
fn fuzz_regression_a_selection_list_read_does_not_land_after_a_file_took_the_selection(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = two_commits(cx);
	let shown = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	};
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[0].sha.clone();
		m.select_commit(&sha, cx);
	});
	settle_until(cx, "the first commit's preview", |cx| shown(cx));
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[1].sha.clone();
		m.extend_range(&sha, cx);
		assert_eq!(m.log_selected.len(), 2, "two commits are selected");
	});
	settle_until(cx, "the selection's preview", |cx| shown(cx));
	// One turn of the event loop: the list read has not run when the file
	// takes the selection.
	model.update_in(cx, |m, _, cx| {
		m.toggle_selection_expanded(cx);
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m.log_selected.is_empty(), "the file takes the selection");
	});
	settle_until(cx, "the file", |cx| shown(cx));
	model.read_with(cx, |m, cx| {
		assert!(m.held_selection.is_none(), "the file's read landed");
		assert!(
			m.commit_view.selection_details.is_empty(),
			"a list landed with nothing selected"
		);
		invariants::assert_holds(m, cx, &[L6, L8]);
	});
}

/// L8: the details of a commit that land while a file read holds its
/// selection aside belong to the held view, so a read that gives the
/// selection back finds them. Without the held branch of `view_of_selected_commit` they
/// were dropped and the details pane kept the row's own fields.
#[gpui::test]
fn fuzz_regression_details_landing_on_a_held_selection_survive_its_restore(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = two_commits(cx);
	let shown = |cx: &mut VisualTestContext| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	};
	model.update_in(cx, |m, _, cx| {
		let sha = m.commits[0].sha.clone();
		m.select_commit(&sha, cx);
	});
	settle_until(cx, "the commit's preview and details", |cx| {
		shown(cx)
			&& model
				.read_with(cx, |m, _| m.commit_view.commit_details.is_some())
	});
	// The file's read is held, so the selection stays aside while the
	// details land.
	model.update(cx, |m, _| {
		m.e2e_read_delay = Some(std::time::Duration::from_secs(60))
	});
	let id = model.update_in(cx, |m, _, cx| {
		let id = m.selected_commit.clone().expect("a selection");
		m.commit_view.commit_details = None;
		m.reload_selected_commit_details_for_tests(cx);
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m.held_selection.is_some(), "the selection is held");
		id
	});
	settle(cx);
	// The file's read is refused (the retained budget) and gives the
	// selection back.
	model.update(cx, |m, _| {
		let held = m.held_selection.as_ref().expect("still held");
		assert_eq!(
			held.view.commit_details.as_ref().map(|d| d.sha.as_str()),
			Some(id.as_str()),
			"the details landed on the held view"
		);
		m.restore_shown_selection();
		assert_eq!(
			m.commit_view
				.commit_details
				.as_ref()
				.map(|d| d.sha.as_str()),
			Some(id.as_str()),
			"the restored view has its details"
		);
	});
}

#[gpui::test]
fn fuzz_regression_a_live_tree_file_selection_does_not_hide_the_held_owner(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let drawn_with = |cx: &mut VisualTestContext, prefix: &str| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with(prefix))
		})
	};
	let browse = drawn_with(cx, "btn-browse-tree:").expect("the tree");
	click_probed(&model, cx, &browse);
	settle_until(cx, "the tree", |cx| drawn_with(cx, "rev-row:").is_some());
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	let row = drawn_with(cx, "rev-row:").expect("a file of the tree");
	click_probed(&model, cx, &row);
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	// A working-file read starts and is held: it sets the selection aside.
	model.update(cx, |m, _| {
		m.e2e_read_delay = Some(std::time::Duration::from_secs(60))
	});
	model.update_in(cx, |m, _, cx| {
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m.held_selection.is_some(), "the selection is held");
		assert!(m.commit_view.selection().is_none());
		// The selection phase of another rev-file read. No I/O is run so
		// leaving the tree deterministically happens before it lands.
		let sha = m.rev_tree.as_ref().unwrap().sha.clone();
		m.hold_shown_commit_file("a.txt", true, Some(&sha));
		m.preview_pending_from_log = true;
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	// The read is refused by the retained budget and gives the selection back.
	model.update(cx, |m, _| m.restore_shown_selection());
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview.is_some(), "the file's tab stays open");
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
	});
}

#[gpui::test]
fn fuzz_regression_a_partial_hold_promoted_to_full_drops_the_tree_file_owner(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let drawn_with = |cx: &mut VisualTestContext, prefix: &str| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with(prefix))
		})
	};
	let browse = drawn_with(cx, "btn-browse-tree:").expect("the tree");
	click_probed(&model, cx, &browse);
	settle_until(cx, "the tree", |cx| drawn_with(cx, "rev-row:").is_some());
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	let row = drawn_with(cx, "rev-row:").expect("a file of the tree");
	click_probed(&model, cx, &row);
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	// Land a working preview while the tree stays open, with no commit.
	model.update_in(cx, |m, _, cx| {
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
	});
	settle_until(cx, "working file", |cx| {
		model.read_with(cx, |m, _| m.preview.is_some() && !m.preview_loading)
	});
	model.update(cx, |m, _| {
		assert!(m.selected_commit.is_none());
		assert!(m.commit_view.selection().is_none());
		// Selection phase of a pending rev-file read, before its I/O lands.
		let sha = m.rev_tree.as_ref().unwrap().sha.clone();
		m.hold_shown_commit_file("a.txt", true, Some(&sha));
		m.preview_pending_from_log = true;
		m.preview_loading = true;
		assert!(m
			.held_selection
			.as_ref()
			.unwrap()
			.commit_file_only
			.is_some());
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.update(cx, |m, _| {
		m.e2e_read_delay = Some(std::time::Duration::from_secs(60));
	});
	model.update_in(cx, |m, _, cx| {
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m
			.held_selection
			.as_ref()
			.unwrap()
			.commit_file_only
			.is_none());
	});
	// The full restore must not retain the pending rev file's ownership.
	model.update(cx, |m, _| m.restore_shown_selection());
	model.read_with(cx, |m, cx| {
		assert!(m.commit_view.selection().is_none());
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
	});
}

/// Opens the tree of the shown commit (the one that changed only `b.txt`) and
/// the file `a.txt` in it, which that commit did not change.
fn open_unchanged_tree_file(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) {
	let drawn_with = |cx: &mut VisualTestContext, id: &str| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|d| d == id)
		})
	};
	let browse = model
		.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with("btn-browse-tree:"))
		})
		.expect("the tree");
	click_probed(model, cx, &browse);
	settle_until(cx, "a.txt in the tree", |cx| {
		drawn_with(cx, "rev-row:a.txt").is_some()
	});
	model.read_with(cx, |m, _| {
		assert!(
			!m.commit_view.commit_files.iter().any(|(p, _)| p == "a.txt"),
			"the commit did not change a.txt"
		);
	});
	click_probed(model, cx, "rev-row:a.txt");
	settle_until(cx, "the file", |cx| {
		model.read_with(cx, |m, _| {
			m.preview.is_some()
				&& !m.preview_loading
				&& m.commit_view.selected_path() == Some("a.txt")
		})
	});
}

/// L2/L6: a file the commit did not change, opened from its tree, belongs to
/// the tree's commit from the moment it is opened: the commit stays selected,
/// so nothing "adds" the owner when "Back to Working" removes the tree, and
/// the tab's preview is still not a leftover.
#[gpui::test]
fn fuzz_regression_an_unchanged_tree_file_is_owned_from_the_open(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown_with(cx, true);
	open_unchanged_tree_file(&model, cx);
	let tree_sha = model.read_with(cx, |m, _| {
		m.rev_tree.as_ref().expect("the tree").sha.clone()
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.selected_commit.is_some(), "the commit stays selected");
		assert!(m.preview.is_some(), "the file's tab stays open");
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
		assert_eq!(m.commit_view.rev_owner(), Some(tree_sha.as_str()));
	});
}

/// The same file when a working-file read holds the selection aside in part
/// (the tree file's selection only, the commit stays live) and the tree is
/// left before the read is refused: the restored selection is still owned.
#[gpui::test]
fn fuzz_regression_a_restored_unchanged_tree_file_keeps_its_owner(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown_with(cx, true);
	open_unchanged_tree_file(&model, cx);
	model.update(cx, |m, _| {
		m.e2e_read_delay = Some(std::time::Duration::from_secs(60))
	});
	model.update_in(cx, |m, _, cx| {
		m.select_file_with_source("a.txt", crate::SourceKind::File, cx);
		assert!(m.held_selection.is_some(), "the selection is held");
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.update(cx, |m, _| m.restore_shown_selection());
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview.is_some(), "the file's tab stays open");
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
		assert!(m.commit_view.rev_owner().is_some(), "the owner came back");
	});
}

/// Presses Refresh and waits until the reload has landed.
fn refresh_and_settle(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
) {
	click_probed(model, cx, "btn-refresh");
	settle_until(cx, "the refresh to finish", |cx| {
		model.read_with(cx, |m, _| {
			!m.is_loading
				&& !m.refresh_reload
				&& m.changes_loaded
				&& !m.preview_loading
		})
	});
}

/// L2/L6: Refresh keeps the shown tree file's tab and its tree, so it keeps
/// the file's selection with its owner too. It dropped the selection whole
/// (`drop_commit_selection`) and left the preview, and "Back to Working"
/// then found a commit file nobody owned. The preview and the selection of
/// a commit file live and die together.
#[gpui::test]
fn fuzz_regression_refresh_keeps_the_owner_of_a_tree_file_no_commit_selects(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown(cx);
	let browse = model
		.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with("btn-browse-tree:"))
		})
		.expect("the tree");
	click_probed(&model, cx, &browse);
	settle_until(cx, "a tree row", |cx| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.iter()
				.any(|id| id.starts_with("rev-row:"))
		})
	});
	// The tab closes the selection; the tree stays.
	model.update_in(cx, |m, _, cx| m.close_tab(0, cx));
	settle(cx);
	let row = model
		.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|id| id.starts_with("rev-row:"))
		})
		.expect("a tree row");
	click_probed(&model, cx, &row);
	settle_until(cx, "the tree file", |cx| {
		model.read_with(cx, |m, _| {
			m.preview.is_some()
				&& !m.preview_loading
				&& m.commit_view.rev_owner().is_some()
		})
	});
	assert!(model.read_with(cx, |m, _| m.selected_commit.is_none()));
	refresh_and_settle(&model, cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_some(), "Refresh kept the tree");
		assert!(m.preview.is_some(), "Refresh kept the file's tab");
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview.is_some(), "the file's tab stays open");
		invariants::assert_holds(m, cx, &[L2, L3, L6]);
		assert!(m.commit_view.rev_owner().is_some(), "the owner stayed");
	});
}

/// A plain repo switch ends the tree file's preview and tree, so it ends the
/// file's selection and its owner too: a selection of a file no preview
/// shows is what the other direction of L9 (L10) catches. The helper that
/// keeps a tree file's selection was also used by a plain switch, which
/// cleared the preview and the tree after it (#230).
#[gpui::test]
fn fuzz_regression_a_repo_switch_drops_the_tree_file_selection_with_its_preview(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) = commit_shown_with(cx, true);
	open_unchanged_tree_file(&model, cx);
	model.read_with(cx, |m, cx| {
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
	click_probed(&model, cx, "btn-repo-selector");
	click_probed(&model, cx, "pick-repo:beta");
	settle(cx);
	model.read_with(cx, |m, cx| {
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
		assert!(m.rev_tree.is_none(), "the switch closed the tree");
		assert!(m.preview.is_none(), "the switch cleared the preview");
		assert_eq!(
			m.commit_view.selected_path(),
			None,
			"no file is selected of a preview that is gone"
		);
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
}

/// The same switch to a repo that cannot be read: the error page is beta's
/// and names no file of alpha's tree (it was labelled "beta > a.txt").
#[gpui::test]
fn fuzz_regression_a_repo_switch_to_an_unreadable_repo_keeps_no_tree_file(
	cx: &mut TestAppContext,
) {
	let (ws, model, cx) = commit_shown_with(cx, true);
	open_unchanged_tree_file(&model, cx);
	// beta stops being a repository after the scan listed it.
	fs::remove_dir_all(ws.path().join("beta").join(".git")).unwrap();
	click_probed(&model, cx, "btn-repo-selector");
	click_probed(&model, cx, "pick-repo:beta");
	settle(cx);
	model.read_with(cx, |m, cx| {
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
		assert!(m.preview_error.is_some(), "beta's error page is shown");
		assert_eq!(m.commit_view.selected_path(), None);
		let (_, crumbs, _) = m.source_labels();
		assert!(
			!crumbs.contains("a.txt"),
			"the error page is labelled {crumbs:?}"
		);
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
}

/// L10 compares the repo a selection was made in with the repo of the error
/// page, each on its own. Alpha's tree-file selection left over beside beta's
/// error page is a selection of a file no preview shows, and the current repo
/// being beta must not make it look like beta's (#230).
#[gpui::test]
fn l10_rejects_a_selection_of_another_repo_beside_an_error_page(
	cx: &mut TestAppContext,
) {
	let (ws, model, cx) = commit_shown_with(cx, true);
	open_unchanged_tree_file(&model, cx);
	let (sha, alpha_root) = model.read_with(cx, |m, _| {
		(
			m.commit_view
				.rev_owner()
				.expect("an owned file")
				.to_string(),
			m.repo_root().expect("alpha"),
		)
	});
	fs::remove_dir_all(ws.path().join("beta").join(".git")).unwrap();
	click_probed(&model, cx, "btn-repo-selector");
	click_probed(&model, cx, "pick-repo:beta");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.preview_error.is_some(), "beta's error page is shown");
		assert_ne!(m.repo_root(), Some(alpha_root.clone()), "beta is current");
		invariants::assert_holds(m, cx, &[L10]);
	});
	model.update(cx, |m, cx| {
		m.set_commit_file_selection(Some(
			crate::history::CommitFileSel::from_tree("a.txt", &sha)
				.with_root(Some(alpha_root)),
		));
		assert!(
			invariants::eval(L10, m, cx).is_some(),
			"alpha's selection beside beta's error page must be caught"
		);
	});
}

/// Opens `name` from the tree the commit's Browse Tree button opens and waits
/// until its read has ended, whatever it ended in.
fn open_tree_file_any_result(
	model: &Entity<WorkbenchModel>,
	cx: &mut VisualTestContext,
	name: &str,
) {
	let drawn = |cx: &mut VisualTestContext, wanted: &dyn Fn(&str) -> bool| {
		model.read_with(cx, |m, _| {
			m.probes
				.as_ref()
				.unwrap()
				.drawn()
				.into_iter()
				.find(|d| wanted(d))
		})
	};
	let browse =
		drawn(cx, &|id| id.starts_with("btn-browse-tree:")).expect("the tree");
	click_probed(model, cx, &browse);
	let row = format!("rev-row:{name}");
	settle_until(cx, "the file in the tree", |cx| {
		drawn(cx, &|id| id == row).is_some()
	});
	click_probed(model, cx, &row);
	settle_until(cx, "the file's read to end", |cx| {
		model.read_with(cx, |m, _| {
			!m.preview_loading && m.commit_view.selected_path() == Some(name)
		})
	});
}

/// L10: a tree file whose read ends in an error page (a binary file here)
/// keeps its selection, owner included, when "Back to Working" closes the
/// tree. The error page belongs to the repo the file was opened in, which the
/// closed tree no longer says (#230).
#[gpui::test]
fn fuzz_regression_a_tree_file_error_page_survives_back_to_working(
	cx: &mut TestAppContext,
) {
	let (_ws, model, cx) =
		commit_shown_with_files(cx, true, &[("bin.dat", b"\0\x01\x02\0")]);
	open_tree_file_any_result(&model, cx, "bin.dat");
	model.read_with(cx, |m, cx| {
		assert!(m.preview_error.is_some(), "a binary file is an error page");
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview_error.is_some(), "the error page stays");
		assert!(m.commit_view.rev_owner().is_some(), "the owner stays");
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
}

/// The same when the file cannot be read at all (its object is gone).
#[gpui::test]
fn fuzz_regression_an_unreadable_tree_file_survives_back_to_working(
	cx: &mut TestAppContext,
) {
	let (ws, model, cx) =
		commit_shown_with_files(cx, true, &[("c.txt", b"gone")]);
	let alpha = ws.path().join("alpha");
	let out = std::process::Command::new("git")
		.current_dir(&alpha)
		.args(["rev-parse", "HEAD~1:c.txt"])
		.output()
		.expect("git must run");
	let blob = String::from_utf8(out.stdout).unwrap();
	let blob = blob.trim();
	let object = alpha
		.join(".git")
		.join("objects")
		.join(&blob[..2])
		.join(&blob[2..]);
	let mut perms = fs::metadata(&object).unwrap().permissions();
	#[allow(clippy::permissions_set_readonly_false)]
	perms.set_readonly(false);
	fs::set_permissions(&object, perms).unwrap();
	fs::remove_file(&object).unwrap();
	open_tree_file_any_result(&model, cx, "c.txt");
	model.read_with(cx, |m, cx| {
		assert!(m.preview_error.is_some(), "the read failed");
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
	click_probed(&model, cx, "btn-leave-tree");
	settle(cx);
	model.read_with(cx, |m, cx| {
		assert!(m.rev_tree.is_none(), "the tree is left");
		assert!(m.preview_error.is_some(), "the error page stays");
		invariants::assert_holds(m, cx, &[L2, L3, L6, L9, L10]);
	});
}
