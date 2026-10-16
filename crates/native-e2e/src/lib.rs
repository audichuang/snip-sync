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

// The `[APP:<KIND>: <fixed fields> [ws_tab=<n> ]id=<id>]` lines carry an id that
// can hold any character a repository, branch or file name can: spaces, `=`,
// `:`, `]`, even a field's own name (`x=`, `by=`, `ws_tab=2`). So `id=` is the
// last field of its line and the id is everything after it up to the closing
// `]`; every other field comes before it and has no space in its value, so
// the first ` id=` is where the id starts. Nothing is read from the end of
// the line. `scripts/bench_native_memory.py` reads the same lines by the same
// rule (see `docs/real-ui-operator-protocol.md`, section 3).

/// The fixed fields of a `[APP:<kind>: …]` line and its id, which is the
/// last field.
fn id_line<'a>(line: &'a str, kind: &str) -> Option<(&'a str, &'a str)> {
	let marker = format!("[APP:{kind}: ");
	let rest = &line[line.find(&marker)? + marker.len()..];
	let rest = rest.trim_end().strip_suffix(']')?;
	match rest.strip_prefix("id=") {
		Some(id) => Some(("", id)),
		None => {
			let at = rest.find(" id=")?;
			Some((&rest[..at], &rest[at + 4..]))
		}
	}
}

/// The id of a `[APP:<kind>: … id=<id>]` line.
fn id_body<'a>(line: &'a str, kind: &str) -> Option<&'a str> {
	id_line(line, kind).map(|(_, id)| id)
}

/// The value of field `name` (`x`, `clipped`, …) among a line's fixed fields.
fn field<'a>(fields: &'a str, name: &str) -> Option<&'a str> {
	fields
		.split(' ')
		.find_map(|f| f.strip_prefix(name)?.strip_prefix('='))
}

/// Parses a `[APP:CTRL_BOUNDS: x=<n> y=<n> w=<n> h=<n> id=<id>]` line into
/// the control id and its `[x, y, w, h]`.
pub fn parse_bounds(line: &str) -> Option<(String, [i32; 4])> {
	let (fields, id) = id_line(line, "CTRL_BOUNDS")?;
	let mut v = [0; 4];
	for (slot, name) in v.iter_mut().zip(["x", "y", "w", "h"]) {
		*slot = field(fields, name)?.parse().ok()?;
	}
	Some((id.to_string(), v))
}

/// Parses a `[APP:CTRL_GONE: id=<id>]` line.
pub fn parse_gone(line: &str) -> Option<String> {
	Some(id_body(line, "CTRL_GONE")?.to_string())
}

/// Parses a duplicate probe line `[APP:CTRL_DUPLICATE: id=<id>]`.
pub fn parse_duplicate(line: &str) -> Option<String> {
	Some(id_body(line, "CTRL_DUPLICATE")?.to_string())
}

/// Parses a covered control audit line
/// `[APP:CTRL_COVERED: by=<encoded id> id=<id>]` into `(covered, by)`.
/// `by` has no space, `]`, `%` or line break in it (they are `%20`, `%5D`,
/// `%25`, `%0A`, `%0D`), so it ends at the first space and `id=` is last.
pub fn parse_covered(line: &str) -> Option<(String, String)> {
	let (fields, id) = id_line(line, "CTRL_COVERED")?;
	Some((id.to_string(), decode_by(field(fields, "by")?)))
}

/// `by` as the app wrote it, with its `%XX` escapes undone.
fn decode_by(encoded: &str) -> String {
	let mut out = String::with_capacity(encoded.len());
	let mut rest = encoded;
	while let Some(at) = rest.find('%') {
		out.push_str(&rest[..at]);
		let hex = rest
			.get(at + 1..at + 3)
			.and_then(|h| u8::from_str_radix(h, 16).ok());
		match hex {
			Some(b) if b.is_ascii() => {
				out.push(b as char);
				rest = &rest[at + 3..];
			}
			_ => {
				out.push('%');
				rest = &rest[at + 1..];
			}
		}
	}
	out.push_str(rest);
	out
}

/// Parses a `TEXT_FIT` line
/// `[APP:TEXT_FIT: rule=… clipped=<0|1> … id=<id>]`: the id of the text and
/// whether an ancestor clips it.
pub fn parse_text_fit(line: &str) -> Option<(String, bool)> {
	let (fields, id) = id_line(line, "TEXT_FIT")?;
	field(fields, "rule")?;
	Some((id.to_string(), field(fields, "clipped")? == "1"))
}

/// The id of a `TEXT_FIT` line that has `clipped=1`: an ancestor cuts the
/// text's box.
pub fn parse_text_fit_clipped(line: &str) -> Option<String> {
	let (id, clipped) = parse_text_fit(line)?;
	clipped.then_some(id)
}

/// Parses a `[APP:TEXT_FIT_DRIFT: fit=<n> drawn=<n> id=<id>]` line: the box
/// drawn is not the width the solver gave. A
/// `[APP:TEXT_FIT_STYLE_DRIFT: measured=<style> drawn=<style> id=<id>]` line
/// counts as well: the box is drawn in another family, size or weight than
/// the one its floor and counts were measured in (a family may have spaces).
pub fn parse_text_fit_drift(line: &str) -> Option<String> {
	id_body(line, "TEXT_FIT_DRIFT")
		.or_else(|| id_body(line, "TEXT_FIT_STYLE_DRIFT"))
		.map(str::to_string)
}

/// Parses a `[APP:TEXT_FIT_GONE: id=<id>]` line: the text is not drawn any
/// more.
pub fn parse_text_fit_gone(line: &str) -> Option<String> {
	Some(id_body(line, "TEXT_FIT_GONE")?.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

	/// The tail a background tab's line gets: the tag is a field before
	/// `id=`, which is the last field of the line.
	const TAGS: [&str; 2] = ["", "ws_tab=2 "];

	/// Ids a repository, a branch or a file can give, whole: spaces, `=`, `:`,
	/// a `]`, the line's own field names, and the background tab's tag. Every
	/// id runs from `id=` to the line's closing `]`, so none of them can
	/// move where it ends.
	const ODD_IDS: [&str; 12] = [
		"commit-row:my repo:ab12cd3",
		"commit-row:a=b:c d:ab12cd3",
		"commit-row:odd ] name:ab12cd3",
		"commit-row:x x=1 y=2 w=3 h=4",
		"change-row:report ws_tab=2",
		"change-row:a rule=whole by=b fit=1 drawn=2",
		"commit-row: lead and trail ",
		"row:ends in by=",
		"row:ends in x=",
		"row:ends in id=",
		"row:has id=inside it",
		"row:ws_tab=2 id=x",
	];

	/// What the app writes for a `CTRL_COVERED`'s second id: no space, `]`,
	/// `%` or line break, so the first ` id=` of the line is the real one.
	fn encode_by(by: &str) -> String {
		let mut out = String::new();
		for c in by.chars() {
			match c {
				'%' => out.push_str("%25"),
				' ' => out.push_str("%20"),
				']' => out.push_str("%5D"),
				'\n' => out.push_str("%0A"),
				'\r' => out.push_str("%0D"),
				c => out.push(c),
			}
		}
		out
	}

	#[test]
	fn text_fit_lines_are_parsed() {
		let line = "[APP:TEXT_FIT: rule=readable6 natural=501 floor=91 fit=156 drawn=156 visible=140 clipped=1 cut=1 shown=23/79 id=text-fit:row:ab12:subject]";
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
		let pane = "[APP:TEXT_FIT: rule=readable6 natural=9 floor=9 fit=9 drawn=9 visible=9 clipped=0 cut=0 shown=1/1 id=text-fit:ref:refs/heads/feature/x:pane-name]";
		assert_eq!(
			parse_text_fit(pane).unwrap().0,
			"text-fit:ref:refs/heads/feature/x:pane-name"
		);
		let drift =
			"[APP:TEXT_FIT_DRIFT: fit=126 drawn=120 id=text-fit:row:ab12:date]";
		assert_eq!(
			parse_text_fit_drift(drift).as_deref(),
			Some("text-fit:row:ab12:date")
		);
		assert_eq!(parse_text_fit_drift(line), None);
		assert_eq!(parse_text_fit(drift), None);
		let style = "[APP:TEXT_FIT_STYLE_DRIFT: measured=Inter/13/400 drawn=Inter/13/600 id=text-fit:row:ab12:author]";
		assert_eq!(
			parse_text_fit_drift(style).as_deref(),
			Some("text-fit:row:ab12:author")
		);
		assert_eq!(parse_text_fit(style), None);
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
	fn any_character_in_an_id_is_read_back() {
		for id in ODD_IDS {
			for tag in TAGS {
				let fit = format!("[APP:TEXT_FIT: rule=readable6 natural=501 floor=91 fit=156 drawn=156 visible=140 clipped=1 cut=1 shown=23/79 {tag}id={id}]");
				assert_eq!(
					parse_text_fit(&fit),
					Some((id.to_string(), true)),
					"{fit}"
				);
				assert_eq!(
					parse_text_fit_clipped(&fit).as_deref(),
					Some(id),
					"{fit}"
				);
				let drift = format!(
					"[APP:TEXT_FIT_DRIFT: fit=126 drawn=120 {tag}id={id}]"
				);
				assert_eq!(
					parse_text_fit_drift(&drift).as_deref(),
					Some(id),
					"{drift}"
				);
				let style = format!("[APP:TEXT_FIT_STYLE_DRIFT: measured=JetBrains Mono/13/400 drawn=Inter/13/600 {tag}id={id}]");
				assert_eq!(
					parse_text_fit_drift(&style).as_deref(),
					Some(id),
					"{style}"
				);
				let gone = format!("[APP:TEXT_FIT_GONE: {tag}id={id}]");
				assert_eq!(
					parse_text_fit_gone(&gone).as_deref(),
					Some(id),
					"{gone}"
				);
				let bounds = format!(
					"[APP:CTRL_BOUNDS: x=12 y=-3 w=100 h=20 {tag}id={id}]"
				);
				assert_eq!(
					parse_bounds(&bounds),
					Some((id.to_string(), [12, -3, 100, 20])),
					"{bounds}"
				);
				let ctrl_gone = format!("[APP:CTRL_GONE: {tag}id={id}]");
				assert_eq!(
					parse_gone(&ctrl_gone).as_deref(),
					Some(id),
					"{ctrl_gone}"
				);
				let dup = format!("[APP:CTRL_DUPLICATE: {tag}id={id}]");
				assert_eq!(parse_duplicate(&dup).as_deref(), Some(id), "{dup}");
			}
		}
	}

	/// A line that is not the kind asked for is not read as it: the same id
	/// under another kind's name.
	#[test]
	fn a_line_of_another_kind_is_not_read() {
		assert_eq!(parse_gone("[APP:CTRL_DUPLICATE: id=a]"), None);
		assert_eq!(parse_duplicate("[APP:CTRL_GONE: id=a]"), None);
		assert_eq!(parse_bounds("[APP:CTRL_GONE: id=a]"), None);
		assert_eq!(parse_covered("[APP:CTRL_GONE: id=a]"), None);
		assert_eq!(parse_text_fit_gone("[APP:CTRL_GONE: id=a]"), None);
	}

	/// The covered control is the line's `id=`; the one that covers it is
	/// a field before it, with no space in it, so a name that holds
	/// ` id=`, ` ws_tab=2` or ` by=` cannot move either id.
	#[test]
	fn a_covered_pair_reads_both_ids_whole() {
		let bys = [
			"btn-log-refresh",
			"pick-repo:my repo",
			"row: id=x ws_tab=2",
			"row ends in by=",
			"row 100% ] done",
		];
		for tag in TAGS {
			for id in ODD_IDS {
				for by in bys {
					let line = format!(
						"[APP:CTRL_COVERED: by={} {tag}id={id}]",
						encode_by(by)
					);
					assert_eq!(
						parse_covered(&line),
						Some((id.to_string(), by.to_string())),
						"{line}"
					);
				}
			}
		}
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
				"[APP:CTRL_COVERED: by=btn-log-refresh id=log-filter-user]"
			),
			Some((
				"log-filter-user".to_string(),
				"btn-log-refresh".to_string()
			))
		);
		assert_eq!(parse_covered("[APP:CTRL_DUPLICATE: id=foo]"), None);
	}
}
