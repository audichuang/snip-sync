//! Unified diff computation for file comparisons.

use similar::TextDiff;

/// Computes a unified Git-style diff of `old` against `new` for the given file `path`.
///
/// Returns `(patch_text, truncated)`.
/// If `old` and `new` are identical, returns an empty string and `false`.
/// Respects `max_bytes`, truncating at the last newline within the limit
/// (or at a char boundary if no newline is found).
pub fn unified(
	path: &str,
	old: &str,
	new: &str,
	max_bytes: usize,
) -> (String, bool) {
	if old == new {
		return (String::new(), false);
	}
	let text_diff = TextDiff::from_lines(old, new);
	let mut patch = text_diff
		.unified_diff()
		.context_radius(usize::MAX / 4)
		.header(&format!("a/{path}"), &format!("b/{path}"))
		.to_string();
	if patch.is_empty() {
		return (String::new(), false);
	}
	if patch.len() > max_bytes {
		// The cap can fall inside a multi-byte character: back off first.
		let mut limit = max_bytes;
		while !patch.is_char_boundary(limit) {
			limit -= 1;
		}
		let cut = patch[..limit].rfind('\n').map_or(limit, |idx| idx + 1);
		patch.truncate(cut);
		(patch, true)
	} else {
		(patch, false)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn diff_changed_lines() {
		let old = "line 1\nold 2\nline 3\n";
		let new = "line 1\nnew 2\nline 3\n";
		let (patch, truncated) = unified("sample.txt", old, new, 1024 * 1024);
		assert!(!truncated);
		assert!(patch.contains("--- a/sample.txt\n"));
		assert!(patch.contains("+++ b/sample.txt\n"));
		assert!(patch.contains(" line 1\n"));
		assert!(patch.contains("-old 2\n"));
		assert!(patch.contains("+new 2\n"));
		assert!(patch.contains(" line 3\n"));
	}

	#[test]
	fn diff_identical_input() {
		let old = "hello\nworld\n";
		let new = "hello\nworld\n";
		let (patch, truncated) = unified("same.txt", old, new, 1024 * 1024);
		assert_eq!(patch, "");
		assert!(!truncated);
	}

	#[test]
	fn diff_no_trailing_newline() {
		let old = "start\nmiddle";
		let new = "start\nmiddle modified";
		let (patch, truncated) = unified("eof.txt", old, new, 1024 * 1024);
		assert!(!truncated);
		assert!(patch.contains(r"\ No newline at end of file"));
		assert!(patch.contains("-middle\n"));
		assert!(patch.contains("+middle modified\n"));
	}

	#[test]
	fn diff_crlf() {
		let old = "header\r\nold content\r\nfooter\r\n";
		let new = "header\r\nnew content\r\nfooter\r\n";
		let (patch, truncated) = unified("dos.txt", old, new, 1024 * 1024);
		assert!(!truncated);
		assert!(patch.contains("--- a/dos.txt\n"));
		assert!(patch.contains("+++ b/dos.txt\n"));
		assert!(patch.contains("-old content\r\n"));
		assert!(patch.contains("+new content\r\n"));
		assert!(patch.contains(" header\r\n"));
		assert!(patch.contains(" footer\r\n"));
	}

	#[test]
	fn diff_truncation_cap() {
		let old = "a\n".repeat(50);
		let new = "b\n".repeat(50);
		let (patch, truncated) = unified("big.txt", &old, &new, 60);
		assert!(truncated);
		assert!(patch.len() <= 60);
		assert!(patch.ends_with('\n'));
		assert!(patch.contains("--- a/big.txt\n"));
	}

	#[test]
	fn diff_truncation_inside_a_multibyte_char() {
		let old = "中文\n".repeat(40);
		let new = "新的內容\n".repeat(40);
		let (full, _) = unified("zh.txt", &old, &new, usize::MAX);
		for max in 1..full.len() {
			let (patch, truncated) = unified("zh.txt", &old, &new, max);
			assert!(truncated);
			assert!(patch.len() <= max);
			assert!(full.starts_with(&patch));
		}
	}
}
