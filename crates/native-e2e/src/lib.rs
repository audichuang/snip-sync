//! Shared helpers for the real-app drivers in `tests/`.

use std::path::PathBuf;
use std::time::Duration;

/// The app under test: `SNIP_NATIVE_BIN`, set by `just native-smoke`,
/// `just native-lifecycle` and the acceptance shards. There is no fallback
/// build, so the executable a run reports is the one it drove.
pub fn native_bin() -> PathBuf {
	let bin = std::env::var_os("SNIP_NATIVE_BIN")
		.map(PathBuf::from)
		.expect(
		"SNIP_NATIVE_BIN must name the snip-desktop-native executable to drive",
	);
	assert!(bin.is_file(), "SNIP_NATIVE_BIN {bin:?} is not a file");
	bin
}

/// Every wait deadline goes through here. `SNIP_E2E_TIMEOUT_SCALE` (e.g. 2 in
/// CI) stretches them for a loaded machine: lavapipe renders on the CPU, so a
/// busy host makes a healthy app miss fixed deadlines.
pub fn scaled(d: Duration) -> Duration {
	static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
	d.mul_f64(*SCALE.get_or_init(|| {
		std::env::var("SNIP_E2E_TIMEOUT_SCALE")
			.ok()
			.and_then(|v| v.parse::<f64>().ok())
			.filter(|v| v.is_finite() && *v >= 1.0)
			.unwrap_or(1.0)
	}))
}

/// Wait for the clipboard to become readable and match the completed copy.
/// Callers that expect no change must first observe the completed refusal,
/// accept any readable text here, and compare it once outside this helper.
pub fn wait_for_clipboard<E: std::fmt::Debug>(
	mut read: impl FnMut() -> Result<String, E>,
	mut matches: impl FnMut(&str) -> bool,
	timeout: Duration,
) -> Result<String, String> {
	let deadline = std::time::Instant::now() + scaled(timeout);
	loop {
		let value = read();
		if let Ok(text) = &value {
			if matches(text) {
				return Ok(text.clone());
			}
		}
		let now = std::time::Instant::now();
		if now >= deadline {
			return Err(format!(
				"clipboard did not match before timeout; last read: {value:?}"
			));
		}
		std::thread::sleep(Duration::from_millis(40).min(deadline - now));
	}
}

/// Splits a paste row control id `paste-<kind>:<ix>:<path>` into its plan
/// index and path (the path may itself contain `:`).
pub fn parse_paste_id<'a>(id: &'a str, kind: &str) -> Option<(usize, &'a str)> {
	let rest = id
		.strip_prefix("paste-")?
		.strip_prefix(kind)?
		.strip_prefix(':')?;
	let (ix, path) = rest.split_once(':')?;
	Some((ix.parse().ok()?, path))
}

/// Every drawn `paste-<kind>` control for `path`, in plan order. One relative
/// path can appear several times in a plan (two roots, or two commits).
pub fn paste_ids<'a>(
	ids: impl IntoIterator<Item = &'a str>,
	kind: &str,
	path: &str,
) -> Vec<&'a str> {
	let mut found: Vec<(usize, &str)> = ids
		.into_iter()
		.filter_map(|id| {
			let (ix, p) = parse_paste_id(id, kind)?;
			(p == path).then_some((ix, id))
		})
		.collect();
	found.sort();
	found.into_iter().map(|(_, id)| id).collect()
}

/// Bounds of control `id`. A paste id written without the plan index
/// (`paste-overwrite:a.txt`) resolves to the first such row in plan order.
pub fn lookup_bounds(
	bounds: &std::collections::HashMap<String, [i32; 4]>,
	id: &str,
) -> Option<[i32; 4]> {
	if let Some(v) = bounds.get(id) {
		return Some(*v);
	}
	let rest = id.strip_prefix("paste-")?;
	let (kind, path) = rest.split_once(':')?;
	let first = paste_ids(bounds.keys().map(String::as_str), kind, path)
		.into_iter()
		.next()?;
	bounds.get(first).copied()
}

/// Parses a duplicate probe line `[APP:CTRL_DUPLICATE: id=...]`.
pub fn parse_duplicate(line: &str) -> Option<String> {
	let rest = line.split("[APP:CTRL_DUPLICATE: id=").nth(1)?;
	Some(rest.trim_end().strip_suffix(']')?.to_string())
}

/// Parses a covered control audit line `[APP:CTRL_COVERED: id=... by=...]`.
pub fn parse_covered(line: &str) -> Option<(String, String)> {
	let rest = line.split("[APP:CTRL_COVERED: id=").nth(1)?;
	let (id_a, rest_b) = rest.split_once(" by=")?;
	Some((
		id_a.to_string(),
		rest_b.trim_end().strip_suffix(']')?.to_string(),
	))
}

/// Parses a `TEXT_FIT` line `[APP:TEXT_FIT: id=<id> ... clipped=<0|1> ...]`:
/// the id of the text and whether an ancestor clips it.
pub fn parse_text_fit(line: &str) -> Option<(String, bool)> {
	let rest = line.split("[APP:TEXT_FIT: id=").nth(1)?;
	let (id, rest) = rest.split_once(' ')?;
	let clipped = rest.split(" clipped=").nth(1)?.starts_with('1');
	Some((id.to_string(), clipped))
}

/// The id of a `TEXT_FIT` line that has `clipped=1`: an ancestor cuts the
/// text's box.
pub fn parse_text_fit_clipped(line: &str) -> Option<String> {
	let (id, clipped) = parse_text_fit(line)?;
	clipped.then_some(id)
}

/// Parses a `[APP:TEXT_FIT_DRIFT: id=<id> fit=<n> drawn=<n>]` line: the box
/// drawn is not the width the solver gave.
pub fn parse_text_fit_drift(line: &str) -> Option<String> {
	let rest = line.split("[APP:TEXT_FIT_DRIFT: id=").nth(1)?;
	let (id, _) = rest.split_once(' ')?;
	Some(id.to_string())
}

/// Parses a `[APP:TEXT_FIT_GONE: id=<id>]` line: the text is not drawn any
/// more.
pub fn parse_text_fit_gone(line: &str) -> Option<String> {
	let rest = line.split("[APP:TEXT_FIT_GONE: id=").nth(1)?;
	Some(rest.trim_end().strip_suffix(']')?.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

	#[test]
	fn text_fit_lines_are_parsed() {
		let line = "[APP:TEXT_FIT: id=text-fit:row:ab12:subject rule=readable6 natural=501 floor=91 fit=156 drawn=156 visible=140 clipped=1 cut=1 shown=23/79]";
		assert_eq!(
			parse_text_fit(line),
			Some(("text-fit:row:ab12:subject".to_string(), true))
		);
		assert_eq!(
			parse_text_fit_clipped(line).as_deref(),
			Some("text-fit:row:ab12:subject")
		);
		let whole = line.replace("clipped=1", "clipped=0");
		assert_eq!(parse_text_fit_clipped(&whole), None);
		assert_eq!(
			parse_text_fit(&whole),
			Some(("text-fit:row:ab12:subject".to_string(), false))
		);
		// A branch name is part of an id, with its slashes.
		let pane = "[APP:TEXT_FIT: id=text-fit:ref:refs/heads/feature/x:pane-name rule=readable6 natural=9 floor=9 fit=9 drawn=9 visible=9 clipped=0 cut=0 shown=1/1]";
		assert_eq!(
			parse_text_fit(pane).unwrap().0,
			"text-fit:ref:refs/heads/feature/x:pane-name"
		);
		let drift =
			"[APP:TEXT_FIT_DRIFT: id=text-fit:row:ab12:date fit=126 drawn=120]";
		assert_eq!(
			parse_text_fit_drift(drift).as_deref(),
			Some("text-fit:row:ab12:date")
		);
		assert_eq!(parse_text_fit_drift(line), None);
		assert_eq!(parse_text_fit(drift), None);
		assert_eq!(
			parse_text_fit_gone(
				"[APP:TEXT_FIT_GONE: id=text-fit:row:ab12:date]"
			)
			.as_deref(),
			Some("text-fit:row:ab12:date")
		);
		assert_eq!(parse_text_fit_gone(line), None);
	}

	#[test]
	fn clipboard_wait_retries_unavailable_and_stale_values() {
		let mut values = [
			Err("ContentNotAvailable"),
			Ok("old".into()),
			Ok("expected".into()),
		]
		.into_iter();
		assert_eq!(
			wait_for_clipboard(
				|| values.next().expect("too many reads"),
				|s| s == "expected",
				Duration::from_secs(1)
			)
			.unwrap(),
			"expected"
		);
	}

	#[test]
	fn clipboard_timeout_reports_last_value_or_error() {
		for value in [Ok("wrong".into()), Err("unavailable")] {
			let err =
				wait_for_clipboard(|| value.clone(), |_| false, Duration::ZERO)
					.unwrap_err();
			assert!(
				err.contains(if value.is_ok() {
					"wrong"
				} else {
					"unavailable"
				}),
				"{err}"
			);
		}
	}

	#[test]
	fn unchanged_clipboard_reads_do_not_hide_a_wrong_value() {
		let mut values = [
			Err("unavailable"),
			Ok("unexpected change".into()),
			Ok("old".into()),
		]
		.into_iter();
		let actual = wait_for_clipboard(
			|| values.next().unwrap(),
			|_| true,
			Duration::from_secs(1),
		)
		.unwrap();
		assert_eq!(actual, "unexpected change");
		assert_eq!(values.next(), Some(Ok("old".into())));
	}

	#[test]
	fn paste_ids_resolve_by_path_and_keep_plan_order() {
		let ids = [
			"paste-overwrite:10:a:b.txt",
			"paste-overwrite:2:a:b.txt",
			"paste-overwrite:3:other.txt",
			"paste-include:1:a:b.txt",
			"btn-apply",
		];
		assert_eq!(
			paste_ids(ids, "overwrite", "a:b.txt"),
			["paste-overwrite:2:a:b.txt", "paste-overwrite:10:a:b.txt"]
		);
		assert_eq!(
			parse_paste_id("paste-row:7:長路徑/a b.txt", "row"),
			Some((7, "長路徑/a b.txt"))
		);
		assert_eq!(parse_paste_id("paste-row:x:a.txt", "row"), None);
		let bounds: HashMap<String, [i32; 4]> = [
			("paste-overwrite:5:same.txt".to_string(), [5, 0, 1, 1]),
			("paste-overwrite:1:same.txt".to_string(), [1, 0, 1, 1]),
		]
		.into();
		assert_eq!(
			lookup_bounds(&bounds, "paste-overwrite:same.txt"),
			Some([1, 0, 1, 1])
		);
		assert_eq!(
			lookup_bounds(&bounds, "paste-overwrite:5:same.txt"),
			Some([5, 0, 1, 1])
		);
		assert_eq!(lookup_bounds(&bounds, "paste-overwrite:none.txt"), None);
		assert_eq!(
			parse_duplicate("[APP:CTRL_DUPLICATE: id=btn-refresh]").as_deref(),
			Some("btn-refresh")
		);
		assert_eq!(parse_duplicate("[APP:LOG_CHIPS: ..."), None);
		assert_eq!(
			parse_covered(
				"[APP:CTRL_COVERED: id=log-filter-user by=btn-log-refresh]"
			),
			Some((
				"log-filter-user".to_string(),
				"btn-log-refresh".to_string()
			))
		);
		assert_eq!(parse_covered("[APP:CTRL_DUPLICATE: id=foo]"), None);
	}
}
