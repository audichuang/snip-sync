//! Deterministic reproductions of what the `fuzz_` lanes of #230 found: each
//! is rebuilt with gestures and holds, never with a seed (a seed does not
//! survive a `rand` upgrade, and a remote worker's reply times are not in it).

use super::*;
use crate::invariants::{self, Invariant::*};
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
	let ws = tempfile::tempdir().unwrap();
	let alpha = repo(ws.path(), "alpha", &[]);
	fs::write(alpha.join("a.txt"), "one").unwrap();
	git(&alpha, &["add", "."]);
	git(&alpha, &["commit", "-q", "-m", "add a"]);
	repo(ws.path(), "beta", &[]);
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| !m.commits.is_empty() && m.repos.len() == 2)
	});
	// Probes on, then the first row the Log drew.
	click_probed(&model, cx, "btn-log-refresh");
	let row = model.read_with(cx, |m, _| {
		m.probes
			.as_ref()
			.unwrap()
			.drawn()
			.into_iter()
			.find(|id| id.starts_with("commit-row:"))
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
	for hide in ["alt-9", "button", "paste"] {
		click_probed(&model, cx, "log-filter-date");
		assert!(dropdown_open(cx), "{hide}: the dropdown opens");
		overlays::assert_overlay_panels_match(cx, &model);
		match hide {
			"alt-9" => cx.simulate_keystrokes("alt-9"),
			"button" => click_probed(&model, cx, "btn-log-hide"),
			_ => {
				model.update(cx, |m, _| {
					m.test_clipboard = Some("// FILE: x.txt\nx\n".into())
				});
				cx.simulate_keystrokes("ctrl-v");
			}
		}
		settle(cx);
		model.read_with(cx, |m, _| {
			assert!(!m.bottom_visible, "{hide}: the Log is hidden")
		});
		assert!(!dropdown_open(cx), "{hide}: its dropdown went with it");
		overlays::assert_overlay_panels_match(cx, &model);
		// Back to a bare state for the next way to hide it.
		model.update_in(cx, |m, _, cx| m.cancel_paste_preview(cx));
		model.update_in(cx, |m, _, cx| {
			m.bottom_visible = true;
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
	settle_until(cx, "the Changes of the repo that was left", |cx| {
		model.read_with(cx, |m, _| {
			m.change_repos
				.iter()
				.all(|r| r.state != crate::ChangeRepoState::Loading)
		})
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
		assert!(m.commit_view.selected_commit_file.is_some());
		invariants::assert_holds(m, cx, &[L2, L6]);
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

	// The flag with no tree file selected.
	let file = model.update(cx, |m, _| {
		m.commit_view.selected_commit_file.take().expect("a file")
	});
	assert!(l6(cx).unwrap().is_some(), "a leftover flag is caught");
	model.update(cx, |m, _| {
		m.preview_pending_from_log = false;
	});
	assert_eq!(l6(cx), Some(None), "no selection, no flag");

	// A selected file the tree does not list and the preview is not.
	model.update(cx, |m, _| {
		m.commit_view.selected_commit_file = Some("no/such/file".into());
	});
	assert!(l6(cx).unwrap().is_some(), "a stranger's file is caught");
	model.update(cx, |m, _| {
		m.commit_view.selected_commit_file = Some(file);
		m.commit_view.commit_file_sel = vec!["other".into()];
	});
	assert!(
		l6(cx).unwrap().is_some(),
		"a leftover file selection is caught"
	);
}
