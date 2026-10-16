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
	let (model, cx) =
		open_sized(cx, ws.path().to_path_buf(), None, 1600., 900.);
	settle_until(cx, "the Log", |cx| {
		model.read_with(cx, |m, _| !m.commits.is_empty())
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
