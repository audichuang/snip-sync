//! Wire format: build and parse clipboard payloads.
//!
//! Faithful port of ClipCodeVSCode `src/clipboardFormat.ts` (commit 0aa24c8).
//! Output must stay byte-identical with the TS and Kotlin implementations.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeType {
	New,
	Modified,
	Deleted,
	Moved,
}

impl ChangeType {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::New => "NEW",
			Self::Modified => "MODIFIED",
			Self::Deleted => "DELETED",
			Self::Moved => "MOVED",
		}
	}

	pub fn from_label(label: &str) -> Option<Self> {
		match label {
			"NEW" => Some(Self::New),
			"MODIFIED" => Some(Self::Modified),
			"DELETED" => Some(Self::Deleted),
			"MOVED" => Some(Self::Moved),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEntry {
	pub path: String,
	pub content: String,
	pub change_types: BTreeSet<ChangeType>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PayloadFile {
	pub path: String,
	pub content: Option<String>,
	pub change_type: Option<ChangeType>,
	pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildPayloadOptions {
	pub header_format: String,
	pub pre_text: String,
	pub post_text: String,
	pub add_extra_line_between_files: bool,
	pub files: Vec<PayloadFile>,
	/// Basename of the folder the paths are relative to. Empty = absent.
	pub source_root: Option<String>,
}

// ASCII whitespace only (never Unicode `\s`), and JS `.` which excludes the
// four JS line terminators. Must mirror the TS/Kotlin regexes exactly.
const WS: &str = r"[ \t\n\x0B\x0C\r]";
const DOT: &str = r"[^\n\r\x{2028}\x{2029}]";
const LABELS: &str = "NEW|MODIFIED|DELETED|MOVED";

static LABEL_PATTERN: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(&format!(r"\[({LABELS})\]")).unwrap());
static LEADING_LABEL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(&format!(r"^(?:\[(?:{LABELS})\]{WS}*)+")).unwrap()
});
static GENERIC_FILE_HEADER: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(&format!(
		r"^{WS}*(?:(//|#|/\*){WS}*)?[Ff][Ii][Ll][Ee]:{WS}*({DOT}+?){WS}*(?:\*/)?$"
	))
	.unwrap()
});

const ESCAPE_MARKER: &str = "//clipcode-esc: ";
const SOURCE_ROOT_MARKER: &str = "// clipcode-root: ";
const POST_TEXT_MARKER: &str = "// clipcode-end";
const PLACEHOLDER: &str = "$FILE_PATH";

fn is_ascii_ws(c: char) -> bool {
	matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// ASCII-only trim (includes `\x0B`); never `str::trim`.
pub fn ascii_trim(value: &str) -> &str {
	value.trim_matches(is_ascii_ws)
}

/// Read the source-root metadata from the first line, if present.
pub fn extract_source_root(clipboard_text: &str) -> Option<String> {
	let first_line = clipboard_text.split('\n').next().unwrap_or("");
	let value = ascii_trim(first_line.strip_prefix(SOURCE_ROOT_MARKER)?);
	(!value.is_empty()).then(|| value.to_string())
}

pub fn format_header(
	header_format: &str,
	clipboard_path: &str,
	change_type: Option<ChangeType>,
) -> String {
	let path_with_label = match change_type {
		Some(t) => format!("[{}] {clipboard_path}", t.as_str()),
		None => clipboard_path.to_string(),
	};
	// Literal replacement, never regex expansion.
	header_format.replace(PLACEHOLDER, &path_with_label)
}

pub fn build_payload(options: &BuildPayloadOptions) -> String {
	build_payload_internal(options, true)
}

pub fn build_git_payload(options: &BuildPayloadOptions) -> String {
	build_payload_internal(options, false)
}

pub(crate) struct PayloadLineWriter<'a, W: std::fmt::Write> {
	w: &'a mut W,
	pub has_written_line: bool,
}

impl<'a, W: std::fmt::Write> PayloadLineWriter<'a, W> {
	pub fn new(w: &'a mut W) -> Self {
		Self {
			w,
			has_written_line: false,
		}
	}

	pub fn new_with_written(w: &'a mut W, has_written_line: bool) -> Self {
		Self {
			w,
			has_written_line,
		}
	}

	pub fn write_line(&mut self, line: &str) -> std::fmt::Result {
		if self.has_written_line {
			self.w.write_char('\n')?;
		}
		self.has_written_line = true;
		self.w.write_str(line)?;
		Ok(())
	}

	pub fn write_escaped(
		&mut self,
		text: &str,
		custom: Option<&HeaderPattern>,
	) -> std::fmt::Result {
		if self.has_written_line {
			self.w.write_char('\n')?;
		}
		self.has_written_line = true;
		write_escaped_content(self.w, text, custom)
	}
}

/// Streams `text` line-by-line directly to `w`, prepending `ESCAPE_MARKER` to header-shaped lines
/// without any heap allocation or String cloning.
pub(crate) fn write_escaped_content<W: std::fmt::Write>(
	w: &mut W,
	text: &str,
	custom: Option<&HeaderPattern>,
) -> std::fmt::Result {
	let mut lines = text.split('\n');
	if let Some(first) = lines.next() {
		if needs_escape(first, custom) {
			w.write_str(ESCAPE_MARKER)?;
		}
		w.write_str(first)?;
		for line in lines {
			w.write_char('\n')?;
			if needs_escape(line, custom) {
				w.write_str(ESCAPE_MARKER)?;
			}
			w.write_str(line)?;
		}
	}
	Ok(())
}

#[derive(Default)]
pub(crate) struct CountingWriter {
	pub count: usize,
}

impl std::fmt::Write for CountingWriter {
	fn write_str(&mut self, s: &str) -> std::fmt::Result {
		self.count += s.len();
		Ok(())
	}
}

#[derive(Debug, Clone)]
pub(crate) struct BorrowedPayloadOptions<'a, 'f> {
	pub header_format: &'a str,
	pub pre_text: &'a str,
	pub post_text: &'a str,
	pub add_extra_line_between_files: bool,
	pub source_root: Option<&'a str>,
	pub files: &'f [PayloadFile],
	pub include_empty_wrappers: bool,
}

pub(crate) fn write_payload_envelope<W: std::fmt::Write>(
	writer: &mut PayloadLineWriter<'_, W>,
	opts: &BorrowedPayloadOptions<'_, '_>,
	custom: Option<&HeaderPattern>,
) -> std::fmt::Result {
	if let Some(root) = opts.source_root.filter(|r| !r.is_empty()) {
		let meta = format!("{SOURCE_ROOT_MARKER}{root}");
		if find_header_path(&meta, custom).is_none() {
			writer.write_line(&meta)?;
		}
	}
	if opts.include_empty_wrappers || !opts.pre_text.is_empty() {
		writer.write_escaped(opts.pre_text, custom)?;
	}
	Ok(())
}

pub(crate) fn write_payload_file<W: std::fmt::Write>(
	writer: &mut PayloadLineWriter<'_, W>,
	file: &PayloadFile,
	header_format: &str,
	add_extra_line_between_files: bool,
	custom: Option<&HeaderPattern>,
) -> std::fmt::Result {
	writer.write_line(&format_header(
		header_format,
		&file.path,
		file.change_type,
	))?;
	match file.skipped_reason.as_deref() {
		Some(reason) if !reason.is_empty() => {
			let body = format!("// File skipped: {reason}");
			writer.write_escaped(&body, custom)?;
		}
		_ => {
			let body = file.content.as_deref().unwrap_or_default();
			writer.write_escaped(body, custom)?;
		}
	}
	if add_extra_line_between_files {
		writer.write_line("")?;
	}
	Ok(())
}

pub(crate) fn write_payload_footer<W: std::fmt::Write>(
	writer: &mut PayloadLineWriter<'_, W>,
	opts: &BorrowedPayloadOptions<'_, '_>,
	custom: Option<&HeaderPattern>,
) -> std::fmt::Result {
	if opts.include_empty_wrappers || !opts.post_text.is_empty() {
		if !opts.post_text.is_empty()
			&& find_header_path(POST_TEXT_MARKER, custom).is_none()
		{
			writer.write_line(POST_TEXT_MARKER)?;
		}
		writer.write_escaped(opts.post_text, custom)?;
	}
	Ok(())
}

pub(crate) fn write_payload_borrowed<W: std::fmt::Write>(
	w: &mut W,
	opts: &BorrowedPayloadOptions<'_, '_>,
) -> std::fmt::Result {
	let mut writer = PayloadLineWriter::new(w);
	let custom = HeaderPattern::new(opts.header_format);
	let custom = custom.as_ref();

	write_payload_envelope(&mut writer, opts, custom)?;

	for file in opts.files {
		write_payload_file(
			&mut writer,
			file,
			opts.header_format,
			opts.add_extra_line_between_files,
			custom,
		)?;
	}

	write_payload_footer(&mut writer, opts, custom)?;

	Ok(())
}

fn build_payload_internal(
	options: &BuildPayloadOptions,
	include_empty_wrappers: bool,
) -> String {
	let mut out = String::new();
	let opts = BorrowedPayloadOptions {
		header_format: &options.header_format,
		pre_text: &options.pre_text,
		post_text: &options.post_text,
		add_extra_line_between_files: options.add_extra_line_between_files,
		source_root: options.source_root.as_deref(),
		files: &options.files,
		include_empty_wrappers,
	};
	write_payload_borrowed(&mut out, &opts).unwrap();
	out
}

fn needs_escape(line: &str, custom: Option<&HeaderPattern>) -> bool {
	if line.starts_with(ESCAPE_MARKER) {
		return true;
	}
	if line == POST_TEXT_MARKER
		|| line.strip_suffix('\r') == Some(POST_TEXT_MARKER)
	{
		return true;
	}
	// Test what the parser will see: it drops one trailing `\r`.
	let as_parsed = line.strip_suffix('\r').unwrap_or(line);
	if find_header_path(as_parsed, custom).is_none() {
		return false;
	}
	// Degenerate formats match the escaped line too; escaping is pointless.
	find_header_path(&format!("{ESCAPE_MARKER}{as_parsed}"), custom).is_none()
}

// Inverse of escape_content: strip exactly one leading marker per line.
fn unescape_content(text: &str) -> String {
	text.split('\n')
		.map(|line| line.strip_prefix(ESCAPE_MARKER).unwrap_or(line))
		.collect::<Vec<_>>()
		.join("\n")
}

pub fn parse_clipboard(content: &str, header_format: &str) -> Vec<ParsedEntry> {
	let custom = HeaderPattern::new(header_format);
	let custom = custom.as_ref();

	// Mirror JS `split(/\r?\n/)`: a `\r` is dropped only before a `\n`, so the
	// final segment keeps any trailing `\r`.
	let mut lines: Vec<&str> = content.split('\n').collect();
	let last = lines.len() - 1;
	for line in &mut lines[..last] {
		*line = line.strip_suffix('\r').unwrap_or(line);
	}
	let mut lines = lines.as_slice();
	if lines[0].starts_with(SOURCE_ROOT_MARKER) {
		lines = &lines[1..];
	}

	let mut entries = Vec::new();
	let mut current: Option<(String, BTreeSet<ChangeType>)> = None;
	let mut body: Vec<&str> = Vec::new();
	let mut flush = |current: &mut Option<(String, BTreeSet<ChangeType>)>,
	                 body: &mut Vec<&str>| {
		if let Some((path, change_types)) = current.take() {
			entries.push(ParsedEntry {
				path,
				content: unescape_content(&join_content(body)),
				change_types,
			});
		}
		body.clear();
	};

	for &line in lines {
		let raw_path = find_header_path(line, custom);
		// The header wins over the end marker.
		if raw_path.is_none() && line == POST_TEXT_MARKER {
			flush(&mut current, &mut body);
			continue;
		}
		if let Some(raw_path) = raw_path {
			flush(&mut current, &mut body);
			current = Some((
				strip_leading_labels(raw_path),
				extract_leading_labels(raw_path),
			));
		} else if current.is_some() {
			body.push(line);
		}
	}
	flush(&mut current, &mut body);
	entries
}

// Drop only structural blank lines at both ends; keep the file's own
// whitespace otherwise.
fn join_content(lines: &[&str]) -> String {
	let blank = |l: &&str| ascii_trim(l).is_empty();
	let start = lines.iter().position(|l| !blank(l)).unwrap_or(lines.len());
	let end = lines
		.iter()
		.rposition(|l| !blank(l))
		.map_or(start, |i| i + 1);
	lines[start..end].join("\n")
}

pub fn extract_leading_labels(path: &str) -> BTreeSet<ChangeType> {
	let Some(prefix) = LEADING_LABEL_PATTERN.find(path) else {
		return BTreeSet::new();
	};
	LABEL_PATTERN
		.captures_iter(prefix.as_str())
		.filter_map(|c| ChangeType::from_label(&c[1]))
		.collect()
}

pub fn strip_leading_labels(path: &str) -> String {
	ascii_trim(&LEADING_LABEL_PATTERN.replace(path, "")).to_string()
}

fn find_header_path<'a>(
	line: &'a str,
	custom: Option<&HeaderPattern>,
) -> Option<&'a str> {
	if let Some(path) = custom.and_then(|c| c.find(line)) {
		return Some(path);
	}
	let caps = GENERIC_FILE_HEADER.captures(line)?;
	let raw_path = caps.get(2)?.as_str();
	if caps.get(1).is_none() && !is_likely_bare_file_header_path(raw_path) {
		return None;
	}
	Some(raw_path)
}

fn is_likely_bare_file_header_path(raw_path: &str) -> bool {
	let stripped = strip_leading_labels(raw_path);
	let path = ascii_trim(&stripped);
	if path.is_empty()
		|| path.starts_with(['"', '\''])
		|| path.ends_with([',', ';'])
	{
		return false;
	}
	path.contains(['/', '\\', '.'])
}

/// Custom header matcher equivalent to the TS regex
/// `^seg0(.+?)seg1(?:\1)...segN$`. Hand-rolled because the `regex` crate has
/// no backreferences: every repeat must equal the first capture, so the
/// capture length is fixed by the line length and the match is unique.
pub(crate) struct HeaderPattern {
	segments: Vec<String>,
}

impl HeaderPattern {
	pub(crate) fn new(header_format: &str) -> Option<Self> {
		let segments: Vec<String> = header_format
			.split(PLACEHOLDER)
			.map(str::to_string)
			.collect();
		(segments.len() >= 2).then_some(Self { segments })
	}

	fn find<'a>(&self, line: &'a str) -> Option<&'a str> {
		let (first, rest) = self.segments.split_first()?;
		let (last, inner) = rest.split_last()?;
		let middle = line.strip_prefix(first.as_str())?;
		let middle = middle.strip_suffix(last.as_str())?;
		let repeats = inner.len() + 1;
		let inner_len: usize = inner.iter().map(String::len).sum();
		let path_len = middle.len().checked_sub(inner_len)?;
		if path_len == 0 || path_len % repeats != 0 {
			return None;
		}
		let path = middle.get(..path_len / repeats)?;
		// JS `.` does not match line terminators.
		if path.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
			return None;
		}
		let mut tail = &middle[path.len()..];
		for seg in inner {
			tail = tail.strip_prefix(seg.as_str())?.strip_prefix(path)?;
		}
		tail.is_empty().then_some(path)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const DEFAULT: &str = "// file: $FILE_PATH";
	const DELETED_FILE_MARKER: &str =
		"// This file has been deleted in this change";

	fn file(path: &str, content: &str) -> PayloadFile {
		PayloadFile {
			path: path.into(),
			content: Some(content.into()),
			..Default::default()
		}
	}

	fn opts(
		header: &str,
		pre: &str,
		post: &str,
		extra: bool,
		files: Vec<PayloadFile>,
	) -> BuildPayloadOptions {
		BuildPayloadOptions {
			header_format: header.into(),
			pre_text: pre.into(),
			post_text: post.into(),
			add_extra_line_between_files: extra,
			files,
			source_root: None,
		}
	}

	fn labels(items: &[ChangeType]) -> BTreeSet<ChangeType> {
		items.iter().copied().collect()
	}

	#[test]
	fn formats_default_and_labeled_headers() {
		assert_eq!(
			format_header(DEFAULT, "src/main.ts", None),
			"// file: src/main.ts"
		);
		assert_eq!(
			format_header(DEFAULT, "src/main.ts", Some(ChangeType::Modified)),
			"// file: [MODIFIED] src/main.ts"
		);
		assert_eq!(
			format_header("$FILE_PATH -> $FILE_PATH", "src/main.ts", None),
			"src/main.ts -> src/main.ts"
		);
	}

	#[test]
	fn repeated_placeholders_round_trip() {
		let format = "$FILE_PATH -> $FILE_PATH";
		let path = "src/$&$$.ts";
		let payload = build_payload(&opts(
			format,
			"",
			"",
			false,
			vec![file(path, "content")],
		));
		let parsed: Vec<_> = parse_clipboard(&payload, format)
			.into_iter()
			.map(|e| (e.path, e.content))
			.collect();
		assert_eq!(parsed, vec![(path.to_string(), "content".to_string())]);
		assert!(parse_clipboard("one -> two\ncontent", format).is_empty());
	}

	#[test]
	fn escapes_crlf_line_that_parses_as_custom_header() {
		let format = "### $FILE_PATH";
		let payload = build_payload(&opts(
			format,
			"",
			"",
			false,
			vec![file("src/main.kt", "### src/a.kt\r\nreal body line")],
		));
		assert!(
			payload.contains("//clipcode-esc: ### src/a.kt"),
			"{payload}"
		);
		let entries = parse_clipboard(&payload, format);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].path, "src/main.kt");
		assert!(entries[0].content.contains("### src/a.kt"));
		assert!(entries[0].content.contains("real body line"));
	}

	#[test]
	fn parses_custom_and_generic_headers() {
		assert_eq!(
			parse_clipboard("### src/a.ts\none", "### $FILE_PATH"),
			vec![ParsedEntry {
				path: "src/a.ts".into(),
				content: "one".into(),
				change_types: BTreeSet::new(),
			}]
		);
		assert_eq!(
			parse_clipboard("# file: src/b.ts\ntwo", "missing placeholder"),
			vec![ParsedEntry {
				path: "src/b.ts".into(),
				content: "two".into(),
				change_types: BTreeSet::new(),
			}]
		);
	}

	#[test]
	fn does_not_split_inline_source_text() {
		let parsed = parse_clipboard(
			"// file: src/app.ts\nconst config = {\n  file: undefined,\n  note: \"do not split // file: src/nope.ts here\"\n};\n// file: src/next.ts\nnext();",
			DEFAULT,
		);
		assert_eq!(parsed.len(), 2);
		assert_eq!(parsed[0].path, "src/app.ts");
		assert!(parsed[0].content.contains("file: undefined,"));
		assert!(parsed[0].content.contains("do not split"));
		assert_eq!(parsed[1].path, "src/next.ts");
	}

	#[test]
	fn extracts_and_strips_leading_labels_only() {
		assert_eq!(
			extract_leading_labels("[DELETED] [MOVED] src/a.ts"),
			labels(&[ChangeType::Deleted, ChangeType::Moved])
		);
		assert_eq!(strip_leading_labels("[DELETED] src/a.ts"), "src/a.ts");
		assert!(extract_leading_labels("src/[DELETED]/a.ts").is_empty());
	}

	#[test]
	fn builds_with_wrappers_extra_lines_and_skipped_markers() {
		let payload = build_payload(&opts(
			DEFAULT,
			"<files>",
			"</files>",
			true,
			vec![
				file("src/a.ts", "one"),
				PayloadFile {
					path: "src/b.ts".into(),
					skipped_reason: Some(
						"size exceeds limit (999 bytes)".into(),
					),
					..Default::default()
				},
			],
		));
		assert_eq!(
			payload,
			"<files>\n// file: src/a.ts\none\n\n// file: src/b.ts\n// File skipped: size exceeds limit (999 bytes)\n\n// clipcode-end\n</files>"
		);
		let entries = parse_clipboard(&payload, DEFAULT);
		let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
		assert_eq!(paths, ["src/a.ts", "src/b.ts"]);
		assert_eq!(
			entries[1].content,
			"// File skipped: size exceeds limit (999 bytes)"
		);
	}

	#[test]
	fn literal_end_marker_line_round_trips() {
		let content = "before\n// clipcode-end\nafter";
		let payload = build_payload(&opts(
			DEFAULT,
			"",
			"FOOTER",
			false,
			vec![file("doc.md", content)],
		));
		assert!(payload.contains("//clipcode-esc: // clipcode-end"));
		let entries = parse_clipboard(&payload, DEFAULT);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].content, content);
	}

	#[test]
	fn regular_payload_keeps_empty_wrapper_slots() {
		let payload = build_payload(&opts(
			DEFAULT,
			"",
			"",
			true,
			vec![file("src/a.ts", "one")],
		));
		assert_eq!(payload, "\n// file: src/a.ts\none\n\n");
	}

	#[test]
	fn preserves_content_whitespace() {
		let c = |s| parse_clipboard(s, DEFAULT).remove(0).content;
		assert_eq!(c("// file: src/a.ts\n  indented"), "  indented");
		assert_eq!(c("// file: src/a.ts\na\n   \nb"), "a\n   \nb");
		assert_eq!(c("// file: src/a.ts\na  \n  b"), "a  \n  b");
		assert_eq!(c("// file: src/a.ts\nplain\ncontent"), "plain\ncontent");
	}

	#[test]
	fn removes_separator_blank_and_wrappers() {
		let payload = build_payload(&opts(
			DEFAULT,
			"",
			"",
			true,
			vec![file("src/a.ts", "one"), file("src/b.ts", "two")],
		));
		let parsed = parse_clipboard(&payload, DEFAULT);
		assert_eq!(parsed.len(), 2);
		assert_eq!(parsed[0].content, "one");
		assert_eq!(parsed[1].content, "two");
	}

	#[test]
	fn round_trips_header_shaped_and_marker_content() {
		for content in [
			"before\n// file: src/evil.ts\nafter",
			"//clipcode-esc: // file: src/x.ts",
		] {
			let payload = build_payload(&opts(
				DEFAULT,
				"",
				"",
				true,
				vec![file("src/a.ts", content)],
			));
			assert!(payload.contains("//clipcode-esc: "));
			let parsed = parse_clipboard(&payload, DEFAULT);
			assert_eq!(parsed.len(), 1);
			assert_eq!(parsed[0].path, "src/a.ts");
			assert_eq!(parsed[0].content, content);
		}
	}

	#[test]
	fn header_shaped_post_text_is_not_a_file() {
		let payload = build_payload(&opts(
			DEFAULT,
			"",
			"// file: src/footer.ts",
			true,
			vec![file("src/a.ts", "one")],
		));
		let parsed = parse_clipboard(&payload, DEFAULT);
		assert_eq!(parsed.len(), 1);
		assert_eq!(parsed[0].path, "src/a.ts");
	}

	#[test]
	fn source_root_round_trips_and_is_ignored_by_parser() {
		let mut o = opts(DEFAULT, "", "", true, vec![file("src/a.ts", "x")]);
		o.source_root = Some("inv-svc-console".into());
		let payload = build_payload(&o);
		assert!(payload.starts_with("// clipcode-root: inv-svc-console\n"));
		assert_eq!(
			extract_source_root(&payload).as_deref(),
			Some("inv-svc-console")
		);
		let parsed = parse_clipboard(&payload, DEFAULT);
		assert_eq!(parsed.len(), 1);
		assert_eq!(parsed[0].path, "src/a.ts");
		assert_eq!(parsed[0].content, "x");
	}

	#[test]
	fn permissive_header_suppresses_metadata_line() {
		let mut o =
			opts("// $FILE_PATH", "", "", true, vec![file("src/a.ts", "x")]);
		o.source_root = Some("repo".into());
		let payload = build_payload(&o);
		assert!(!payload.contains("clipcode-root"));
		let parsed = parse_clipboard(&payload, "// $FILE_PATH");
		assert!(parsed.iter().all(|e| !e.path.contains("clipcode-root")));
	}

	#[test]
	fn parser_drops_leading_metadata_line() {
		let text = "// clipcode-root: repo\n// $FILE_PATH-shaped? no\n// file: src/a.ts\nx";
		let parsed = parse_clipboard(text, DEFAULT);
		assert_eq!(parsed.len(), 1);
		assert_eq!(parsed[0].path, "src/a.ts");
	}

	#[test]
	fn extract_source_root_absent() {
		let payload = build_payload(&opts(
			DEFAULT,
			"",
			"",
			true,
			vec![file("src/a.ts", "x")],
		));
		assert_eq!(extract_source_root(&payload), None);
		assert_eq!(extract_source_root("// file: src/a.ts\nx"), None);
		assert_eq!(extract_source_root("// clipcode-root:  \t\nx"), None);
		assert_eq!(
			extract_source_root("// clipcode-root: repo\r\nx").as_deref(),
			Some("repo")
		);
	}

	#[test]
	fn degenerate_header_does_not_mark_every_line() {
		let payload = build_payload(&opts(
			"$FILE_PATH",
			"",
			"",
			true,
			vec![file("src/a.ts", "line one\nline two")],
		));
		assert!(!payload.contains("clipcode-esc"));
	}

	#[test]
	fn git_payload_skips_empty_wrappers() {
		let payload = build_git_payload(&opts(
			DEFAULT,
			"",
			"",
			true,
			vec![PayloadFile {
				path: "src/old.ts".into(),
				content: Some(DELETED_FILE_MARKER.into()),
				change_type: Some(ChangeType::Deleted),
				skipped_reason: None,
			}],
		));
		assert_eq!(
			payload,
			format!("// file: [DELETED] src/old.ts\n{DELETED_FILE_MARKER}\n")
		);
	}

	#[test]
	fn turkish_dotless_i_is_not_a_header() {
		assert!(
			parse_clipboard("// f\u{131}le: phantom.ts", DEFAULT).is_empty()
		);
		assert_eq!(
			parse_clipboard("// FILE: a.ts\nbody", DEFAULT)[0].path,
			"a.ts"
		);
		assert_eq!(
			parse_clipboard("// File: b.ts\nbody", DEFAULT)[0].path,
			"b.ts"
		);
	}

	#[test]
	fn ascii_trim_includes_vertical_tab_only_ascii() {
		assert_eq!(ascii_trim("\x0B\t a \x0C\r\n"), "a");
		assert_eq!(ascii_trim("\u{1C}a\u{FEFF}"), "\u{1C}a\u{FEFF}");
	}

	#[test]
	fn final_segment_keeps_trailing_cr() {
		// JS split(/\r?\n/) leaves a trailing `\r` on the last segment.
		let parsed = parse_clipboard("// file: a.ts\nx\r\ny\r", DEFAULT);
		assert_eq!(parsed[0].content, "x\ny\r");
	}
}
