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

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::HashMap;

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
