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
