//! File filtering (ignore rules, binary and size limits).
//!
//! Port of `filterMatcher.ts`.

use regex::Regex;

use crate::settings::{FilterAction, FilterRule, FilterType};

pub fn file_matches_filters(
	relative_path: &str,
	rules: &[FilterRule],
	use_include_filters: bool,
	use_exclude_filters: bool,
	absolute_path: Option<&str>,
) -> bool {
	let enabled = || rules.iter().filter(|r| r.enabled);
	if use_exclude_filters
		&& enabled()
			.filter(|r| r.action == FilterAction::Exclude)
			.any(|r| matches_rule(relative_path, r, absolute_path))
	{
		return false;
	}
	let mut includes = enabled()
		.filter(|r| r.action == FilterAction::Include)
		.peekable();
	if use_include_filters && includes.peek().is_some() {
		return includes.any(|r| matches_rule(relative_path, r, absolute_path));
	}
	true
}

pub fn matches_path(candidate_path: &str, rule_path: &str) -> bool {
	is_same_or_child(
		&normalize_path(candidate_path),
		&normalize_path(rule_path),
	)
}

/// A directory hit by an enabled PATH exclude rule can be pruned whole:
/// `matches_path` is same-or-child, so every file below it is excluded by the
/// same rule. PATTERN rules match file names and cannot prune directories.
pub fn directory_excluded(
	relative_path: &str,
	rules: &[FilterRule],
	absolute_path: Option<&str>,
) -> bool {
	rules.iter().any(|r| {
		r.enabled
			&& r.action == FilterAction::Exclude
			&& r.kind == FilterType::Path
			&& matches_rule(relative_path, r, absolute_path)
	})
}

pub fn overlaps_directory(directory_path: &str, rule_path: &str) -> bool {
	let directory = normalize_path(directory_path);
	let rule = normalize_path(rule_path);
	if directory.is_empty() {
		return true;
	}
	is_same_or_child(&directory, &rule) || is_same_or_child(&rule, &directory)
}

fn matches_rule(
	relative_path: &str,
	rule: &FilterRule,
	absolute_path: Option<&str>,
) -> bool {
	if rule.kind == FilterType::Pattern {
		return matches_pattern(&file_name(relative_path), &rule.value);
	}
	// TS tests `absolutePath &&`, so an empty string counts as absent.
	if let Some(abs) = absolute_path.filter(|p| !p.is_empty()) {
		if is_absolute_path(&rule.value) {
			return matches_path(abs, &rule.value);
		}
	}
	// A file outside every root is passed with its absolute path as
	// `relative_path`; it has no relative identity, so relative rules skip it
	// (IntelliJ's relativeFilterPath returns null for it).
	if is_absolute_path(relative_path) {
		return false;
	}
	matches_path(relative_path, &rule.value)
}

fn file_name(relative_path: &str) -> String {
	let normalized = normalize_path(relative_path);
	normalized
		.rsplit('/')
		.next()
		.unwrap_or_default()
		.to_string()
}

// JS `.` excludes these four line terminators; Rust's `.` only excludes `\n`.
const JS_DOT: &str = r"[^\n\r\u{2028}\u{2029}]";

fn matches_pattern(name: &str, pattern: &str) -> bool {
	let regex_pattern = if pattern.contains('*') || pattern.contains('?') {
		pattern
			.replace('.', r"\.")
			.replace('*', &format!("{JS_DOT}*"))
			.replace('?', JS_DOT)
	} else {
		// ponytail: raw patterns go to the Rust regex dialect as-is; JS-only
		// syntax (lookaround) falls back to substring match, and `.`/`\d`/`\w`
		// Unicode semantics differ slightly. A rewrite of JS regex syntax is
		// the upgrade path if a real rule ever trips on it.
		pattern.to_string()
	};
	// ponytail: compiled per call; cache compiled rules if large walks with
	// many PATTERN rules show up in profiles.
	// ponytail: `?` matches one code point here but one UTF-16 unit in JS, so
	// only names with astral characters (emoji) can differ.
	match Regex::new(&format!("^(?:{regex_pattern})$")) {
		Ok(re) => re.is_match(name),
		Err(_) => name.contains(pattern),
	}
}

fn is_same_or_child(candidate: &str, parent: &str) -> bool {
	if parent.is_empty() {
		return true;
	}
	if candidate.is_empty() {
		return false;
	}
	candidate == parent
		|| candidate
			.strip_prefix(parent)
			.is_some_and(|rest| rest.starts_with('/'))
}

/// JS `String.prototype.trim` set: Rust's White_Space minus U+0085, plus U+FEFF.
fn is_js_trim(c: char) -> bool {
	(c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}'
}

fn normalize_path(value: &str) -> String {
	let mut collapsed = String::with_capacity(value.len());
	for c in value.chars().map(|c| if c == '\\' { '/' } else { c }) {
		if !(c == '/' && collapsed.ends_with('/')) {
			collapsed.push(c);
		}
	}
	collapsed
		.trim_matches(is_js_trim)
		.trim_matches('/')
		.to_string()
}

fn is_absolute_path(value: &str) -> bool {
	let b = value.as_bytes();
	let slash = |c: u8| c == b'/' || c == b'\\';
	b.first().copied().is_some_and(slash)
		|| (b.len() >= 3
			&& b[0].is_ascii_alphabetic()
			&& b[1] == b':'
			&& slash(b[2]))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rule(kind: FilterType, action: FilterAction, value: &str) -> FilterRule {
		FilterRule {
			kind,
			action,
			value: value.to_string(),
			enabled: true,
		}
	}
	use FilterAction::{Exclude, Include};
	use FilterType::{Path, Pattern};

	#[test]
	fn path_matching_is_segment_aware() {
		assert!(matches_path("module-a/src/file.ts", "module-a"));
		assert!(!matches_path("module-alpha/src/file.ts", "module-a"));
	}

	#[test]
	fn directory_overlap_allows_traversal_toward_included_children() {
		assert!(overlaps_directory("src", "src/features"));
		assert!(overlaps_directory("src/features", "src"));
		assert!(!overlaps_directory("scripts", "src/features"));
	}

	#[test]
	fn filters_use_explicit_type_and_patterns_match_filename_only() {
		let rules = [
			rule(Path, Include, "src"),
			rule(Pattern, Exclude, "*.test.ts"),
		];
		assert!(file_matches_filters(
			"src/main.ts",
			&rules,
			true,
			true,
			None
		));
		assert!(!file_matches_filters(
			"src/main.test.ts",
			&rules,
			true,
			true,
			None
		));
		assert!(!file_matches_filters(
			"docs/main.ts",
			&rules,
			true,
			true,
			None
		));
	}

	#[test]
	fn absolute_path_rules_match_against_the_absolute_path_when_provided() {
		let absolute = "/tmp/project/src/secret.ts";
		let rules = [rule(Path, Exclude, absolute)];
		assert!(!file_matches_filters(
			"src/secret.ts",
			&rules,
			true,
			true,
			Some(absolute)
		));
	}

	#[test]
	fn pattern_alternation_is_anchored_as_a_whole() {
		let rules = [rule(Pattern, Exclude, "foo|bar")];
		assert!(!file_matches_filters("foo", &rules, false, true, None));
		assert!(!file_matches_filters("bar", &rules, false, true, None));
		assert!(file_matches_filters("foobaz", &rules, false, true, None));
		assert!(file_matches_filters("bazbar", &rules, false, true, None));
	}

	#[test]
	fn a_file_outside_every_root_skips_relative_path_rules() {
		let outside = "/repo/secret.txt";
		let include = [rule(Path, Include, "repo/secret.txt")];
		assert!(!file_matches_filters(
			outside,
			&include,
			true,
			false,
			Some(outside)
		));

		let exclude = [rule(Path, Exclude, "repo/secret.txt")];
		assert!(file_matches_filters(
			outside,
			&exclude,
			false,
			true,
			Some(outside)
		));

		let absolute = [rule(Path, Exclude, outside)];
		assert!(!file_matches_filters(
			outside,
			&absolute,
			false,
			true,
			Some(outside)
		));

		assert!(file_matches_filters(
			"repo/secret.txt",
			&include,
			true,
			false,
			Some("/ws/repo/secret.txt")
		));
	}

	#[test]
	fn glob_and_regex_details_follow_js() {
		// `?` is one char, `.` is literal in globs, and neither crosses `\r`.
		assert!(matches_pattern("a.ts", "?.ts"));
		assert!(!matches_pattern("ab.ts", "?.ts"));
		assert!(!matches_pattern("axts", "*.ts"));
		assert!(!matches_pattern("a\rb.ts", "*.ts"));
		// Invalid regex falls back to substring match.
		assert!(matches_pattern("x(y", "("));
		// Disabled rules and PATTERN rules never prune directories.
		let mut off = rule(Path, Exclude, "node_modules");
		off.enabled = false;
		assert!(!directory_excluded("node_modules", &[off], None));
		assert!(!directory_excluded(
			"dist",
			&[rule(Pattern, Exclude, "dist")],
			None
		));
		assert!(directory_excluded(
			"a/node_modules",
			&[rule(Path, Exclude, "a\\node_modules/")],
			None
		));
	}

	#[test]
	fn normalize_path_matches_js() {
		assert_eq!(normalize_path("\\\\a\\\\b//c/ "), "a/b/c");
		assert_eq!(normalize_path("\u{FEFF} a/b\u{2028}"), "a/b");
		assert_eq!(normalize_path("a\u{85}"), "a\u{85}");
		assert!(is_absolute_path("C:\\x"));
		assert!(is_absolute_path("\\\\server\\share"));
		assert!(!is_absolute_path("C:x"));
	}
}
