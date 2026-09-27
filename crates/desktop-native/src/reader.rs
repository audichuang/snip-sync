//! Read-only code reader: one retained preview (`Arc<str>` + line index),
//! virtualized rows, find, go-to-line, character selection with copy, and
//! inline / side-by-side diff. No per-line `String` is retained.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
	div, font, prelude::*, px, rgb, uniform_list, AnyElement, Bounds, Context,
	HighlightStyle, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent,
	MouseMoveEvent, Pixels, Point, ScrollStrategy, SharedString, StyledText,
	TextRun, UniformListScrollHandle, Window,
};

use snip_core::gitrun::{CancelToken, Overflow, RunOptions};
use snip_core::gitsrc::{Git, GitSource};

use crate::i18n::{t, tf};
use crate::syntax::{highlight_line, Language, SyntaxTheme};
use crate::theme::*;
use crate::WorkbenchModel;

/// Retained text cap per preview (core's preview limit).
pub const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
/// Line cap per preview.
pub const MAX_PREVIEW_LINES: usize = 50_000;
/// Find results kept (the count saturates visibly beyond this).
pub const MAX_MATCHES: usize = 5_000;
pub const LINE_H: f32 = 18.0;
/// Maximum bytes shaped and highlighted for a single line on screen to keep UI responsive.
pub const MAX_RENDER_LINE_BYTES: usize = 4096;

/// Where the text on screen comes from; drives the breadcrumb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewSource {
	/// A file read from the working tree (Project tool window).
	WorkingFile,
	/// Working-tree changes against HEAD (legacy `Working` compat source).
	WorkingChanges,
	/// Staged changes in the index against HEAD.
	StagedChanges,
	/// Unstaged changes in working tree against the index.
	UnstagedChanges,
	/// Diff of one commit against its first parent (or the empty tree).
	CommitDiff { sha: String },
	/// A file as stored in a commit, read without checkout.
	CommitFile { sha: String },
	/// Diff between two revisions, endpoints only.
	Compare { from: String, to: String },
	/// Body of an item in the paste preview (clipboard payload).
	PasteItem,
}

pub struct Preview {
	pub source: PreviewSource,
	pub path: Option<String>,
	pub text: Arc<str>,
	pub lines: Vec<Range<u32>>,
	pub is_diff: bool,
	pub lang: Language,
	pub notice: Option<String>,
	/// Index of the longest line, measured for horizontal scrolling.
	pub widest: usize,
	/// Diff layout, built once from `lines`.
	pub diff: Option<DiffRows>,
}

impl Preview {
	/// Retained reader data, including spare vector/string/path capacity and
	/// an Arc header/alignment allowance. Shared text is conservatively counted
	/// again when two retained previews refer to it. Renderer/font allocations
	/// and Reader's search/row-geometry metadata belong to other budget tiers.
	pub fn retained_bytes(&self) -> usize {
		let source_bytes = match &self.source {
			PreviewSource::CommitDiff { sha }
			| PreviewSource::CommitFile { sha } => sha.capacity(),
			PreviewSource::Compare { from, to } => {
				from.capacity().saturating_add(to.capacity())
			}
			_ => 0,
		};
		let mut bytes = std::mem::size_of::<Self>()
			.saturating_add(source_bytes)
			.saturating_add(self.path.as_ref().map_or(0, String::capacity))
			.saturating_add(self.text.len())
			.saturating_add(3 * std::mem::size_of::<usize>())
			.saturating_add(
				self.lines.capacity() * std::mem::size_of::<Range<u32>>(),
			)
			.saturating_add(self.notice.as_ref().map_or(0, String::capacity));
		if let Some(diff) = &self.diff {
			bytes = bytes
				.saturating_add(
					diff.inline.capacity() * std::mem::size_of::<InlineRow>(),
				)
				.saturating_add(
					diff.side.capacity() * std::mem::size_of::<SideRow>(),
				)
				.saturating_add(
					diff.shown.capacity() * std::mem::size_of::<usize>(),
				);
		}
		bytes
	}

	pub fn new(
		source: PreviewSource,
		path: Option<String>,
		mut text: String,
		is_diff: bool,
		lang: Language,
	) -> Self {
		let mut notice = None;
		if text.len() > MAX_PREVIEW_BYTES {
			let mut cut = MAX_PREVIEW_BYTES;
			while !text.is_char_boundary(cut) {
				cut -= 1;
			}
			text.truncate(cut);
			notice = Some("truncated".to_string());
		}
		let mut lines = Vec::new();
		let mut start = 0usize;
		for (i, b) in text.bytes().enumerate() {
			if b == b'\n' {
				lines.push(start as u32..strip_cr(&text, start, i) as u32);
				start = i + 1;
				if lines.len() >= MAX_PREVIEW_LINES {
					notice = Some("truncated".to_string());
					break;
				}
			}
		}
		if start < text.len() && lines.len() < MAX_PREVIEW_LINES {
			lines.push(start as u32..strip_cr(&text, start, text.len()) as u32);
		}
		let widest = lines
			.iter()
			.enumerate()
			.max_by_key(|(_, r)| r.end - r.start)
			.map(|(i, _)| i)
			.unwrap_or(0);
		let text: Arc<str> = text.into();
		let diff = is_diff.then(|| DiffRows::build(&text, &lines));
		// Measure the widest line actually drawn.
		let widest = diff
			.as_ref()
			.and_then(|d| {
				d.shown
					.iter()
					.copied()
					.max_by_key(|&i| lines[i].end - lines[i].start)
			})
			.unwrap_or(widest);
		Self {
			source,
			path,
			text,
			lines,
			is_diff,
			lang,
			notice,
			widest,
			diff,
		}
	}

	pub fn line(&self, ix: usize) -> &str {
		self.lines
			.get(ix)
			.map(|r| &self.text[r.start as usize..r.end as usize])
			.unwrap_or("")
	}

	/// Rows drawn in inline mode (hidden patch headers excluded).
	pub fn inline_rows(&self) -> usize {
		self.diff
			.as_ref()
			.map_or(self.lines.len(), |d| d.shown.len())
	}

	/// Preview line drawn at inline row `row`.
	pub fn inline_line(&self, row: usize) -> usize {
		self.diff
			.as_ref()
			.map_or(row, |d| d.shown.get(row).copied().unwrap_or(0))
	}

	/// Inline row showing `line`, or the next drawn row if it is hidden.
	pub fn inline_row_of(&self, line: usize) -> usize {
		match &self.diff {
			Some(d) => d
				.shown
				.partition_point(|&l| l < line)
				.min(d.shown.len().saturating_sub(1)),
			None => line,
		}
	}

	/// True when `line` is drawn (not a hidden patch header).
	pub fn is_shown(&self, line: usize) -> bool {
		self.diff
			.as_ref()
			.is_none_or(|d| d.shown.binary_search(&line).is_ok())
	}

	/// Bytes of `line` hidden before its text: the `+`/`-`/space marker of
	/// a diff body line. Columns stay raw byte offsets everywhere.
	pub fn text_start(&self, line: usize) -> usize {
		self.diff
			.as_ref()
			.and_then(|d| d.inline.get(line))
			.filter(|r| r.is_code())
			.map_or(0, |_| self.line(line).len().min(1))
	}

	/// True when `line` shows its own text (not hidden, not a fold row).
	pub fn is_text_row(&self, line: usize) -> bool {
		self.is_shown(line)
			&& self.diff.as_ref().is_none_or(|d| {
				d.inline.get(line).is_none_or(|r| r.kind != RowKind::Hunk)
			})
	}

	/// A patch with at least one hunk (not a binary or mode-only header).
	pub fn has_hunks(&self) -> bool {
		self.diff
			.as_ref()
			.is_some_and(|d| d.inline.iter().any(|r| r.kind == RowKind::Hunk))
	}

	/// Changed words of diff line `line` against its paired line, relative
	/// to the text after the marker.
	pub fn word_ranges(&self, line: usize) -> Vec<Range<usize>> {
		let Some(r) = self.diff.as_ref().and_then(|d| d.inline.get(line))
		else {
			return Vec::new();
		};
		let Some(other) = r.pair.map(|o| o as usize) else {
			return Vec::new();
		};
		let text = |l: usize| self.line(l).get(1..).unwrap_or("");
		let (a, b) = if r.kind == RowKind::Removed {
			(line, other)
		} else {
			(other, line)
		};
		let (old, new) = word_diff(text(a), text(b));
		if r.kind == RowKind::Removed {
			old
		} else {
			new
		}
	}

	/// Language used to colour code: a diff body is coloured as its file.
	pub fn code_lang(&self) -> Language {
		match (&self.path, self.is_diff) {
			(Some(path), true) => Language::from_path_or_ext(path, false),
			(None, true) => Language::Plain,
			_ => self.lang,
		}
	}

	/// FNV-1a over the retained text (E2E oracle only; never the text itself).
	pub fn fingerprint(&self) -> u64 {
		fnv1a(self.text.as_bytes())
	}
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
	let mut h: u64 = 0xcbf29ce484222325;
	for &b in bytes {
		h ^= b as u64;
		h = h.wrapping_mul(0x100000001b3);
	}
	h
}

fn strip_cr(text: &str, start: usize, end: usize) -> usize {
	if end > start && text.as_bytes()[end - 1] == b'\r' {
		end - 1
	} else {
		end
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
	Header,
	Hunk,
	Context,
	Added,
	Removed,
}

/// One inline diff row: the preview line plus old/new line numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineRow {
	pub kind: RowKind,
	pub old: Option<u32>,
	pub new: Option<u32>,
	/// Added/removed line of a block that both removes and adds (IntelliJ
	/// "modified"); a lone insertion or deletion keeps its own colour.
	pub modified: bool,
	/// Hunk rows: unchanged lines left out of the patch before this hunk,
	/// drawn as a collapsed "⋯ N unchanged lines" row instead of the `@@`.
	pub fold: u32,
	/// Modified-block lines: the line it is paired with on the other side,
	/// compared word by word.
	pub pair: Option<u32>,
}

impl InlineRow {
	fn new(kind: RowKind, old: Option<u32>, new: Option<u32>) -> Self {
		Self {
			kind,
			old,
			new,
			modified: false,
			fold: 0,
			pair: None,
		}
	}

	/// A body line of a hunk: its first byte is the `+`/`-`/space marker.
	pub fn is_code(&self) -> bool {
		matches!(
			self.kind,
			RowKind::Context | RowKind::Added | RowKind::Removed
		)
	}

	/// Background of a changed line, as IntelliJ colours its block.
	pub fn change(&self) -> Option<Change> {
		match (self.kind, self.modified) {
			(RowKind::Added | RowKind::Removed, true) => Some(Change::Modified),
			(RowKind::Added, false) => Some(Change::Inserted),
			(RowKind::Removed, false) => Some(Change::Deleted),
			_ => None,
		}
	}
}

/// A side-by-side row's place in its change block: the row's offset and
/// how many lines the block has on each side. Drives the tint and the
/// ribbon between the panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
	pub at: u32,
	pub left: u32,
	pub right: u32,
}

/// Which change a block (or a line of it) makes, as IntelliJ colours it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
	Inserted,
	Deleted,
	Modified,
}

impl Block {
	pub fn change(&self) -> Change {
		match (self.left, self.right) {
			(0, _) => Change::Inserted,
			(_, 0) => Change::Deleted,
			_ => Change::Modified,
		}
	}
}

impl Change {
	pub fn bg(self) -> u32 {
		match self {
			Change::Inserted => pal().diff_add_bg,
			Change::Deleted => pal().diff_del_bg,
			Change::Modified => pal().diff_mod_bg,
		}
	}
}

/// One side-by-side row: indices into preview lines for each side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SideRow {
	pub kind: RowKind,
	pub left: Option<(u32, usize)>,
	pub right: Option<(u32, usize)>,
	/// Fold (hunk) and patch-header rows span both sides with this line.
	pub full: Option<usize>,
	/// Set on rows of a change block.
	pub block: Option<Block>,
}

pub struct DiffRows {
	/// One entry per preview line (row kinds and numbers).
	pub inline: Vec<InlineRow>,
	pub side: Vec<SideRow>,
	/// Preview lines drawn in unified mode, in order: hunk bodies plus one
	/// fold row per skipped unchanged region. Patch headers, `@@` lines and
	/// "\ No newline at end of file" are never drawn once a hunk exists.
	pub shown: Vec<usize>,
	/// Largest old/new line number, sizes the gutter.
	pub max_num: u32,
	/// The file may go on after the last hunk: its trailing context is full
	/// (git's 3 lines) and the patch does not end at "\ No newline".
	/// Drawn as one more fold row whose size is known only once expanded.
	pub trailing: bool,
}

/// "@@ -a[,b] +c[,d] @@" as the first old and new line the hunk covers
/// (an empty range names the line *before* it).
fn parse_hunk(line: &str) -> Option<(u32, u32)> {
	let rest = line.strip_prefix("@@ -")?;
	let (old, rest) = rest.split_once(' ')?;
	let new = rest.strip_prefix('+')?.split(' ').next()?;
	let first = |r: &str| -> Option<u32> {
		let (start, len) = match r.split_once(',') {
			Some((s, l)) => (s.parse::<u32>().ok()?, l.parse::<u32>().ok()?),
			None => (r.parse().ok()?, 1),
		};
		Some(if len == 0 { start + 1 } else { start })
	};
	Some((first(old)?, first(new)?))
}

/// The unchanged gap folded before hunk header `line`, as
/// (first old line, first new line, count).
pub fn fold_gap(p: &Preview, line: usize) -> Option<(u32, u32, u32)> {
	let r = p.diff.as_ref()?.inline.get(line)?;
	if r.kind != RowKind::Hunk || r.fold == 0 {
		return None;
	}
	let (old, new) = parse_hunk(p.line(line))?;
	Some((old.checked_sub(r.fold)?, new.checked_sub(r.fold)?, r.fold))
}

/// `expand_folds` target for the trailing fold after the last hunk.
pub const TRAILING_FOLD: usize = usize::MAX;

/// Why a fold could not be expanded (an i18n status key).
pub type ExpandError = &'static str;

/// The patch with the folds before the hunk headers `only` (every fold
/// when None) spliced back in as context, taken from `content`, the
/// new-side file. The result is still a patch: a synthetic `@@` header per
/// gap keeps the numbering, and `DiffRows` hides it (nothing left to fold).
pub fn expand_folds(
	p: &Preview,
	content: &str,
	only: Option<usize>,
) -> Result<String, ExpandError> {
	let d = p.diff.as_ref().ok_or("status_fold_failed")?;
	if p.notice.is_some() {
		return Err("status_fold_too_large");
	}
	let file: Vec<&str> = content.split_inclusive('\n').collect();
	fn bare(l: &str) -> &str {
		l.trim_end_matches('\n').trim_end_matches('\r')
	}
	// The file must still be the one the patch was made from.
	for (i, r) in d.inline.iter().enumerate() {
		if let (true, Some(n)) = (r.is_code(), r.new) {
			let body = p.line(i).get(1..).unwrap_or("");
			if file.get(n as usize - 1).map(|l| bare(l)) != Some(body) {
				return Err("status_fold_stale");
			}
		}
	}
	let mut out = String::with_capacity(p.text.len());
	let mut at = 0usize;
	for &line in &d.shown {
		if only.is_some_and(|o| o != line) {
			continue;
		}
		let Some((old, new, count)) = fold_gap(p, line) else {
			continue;
		};
		let start = p.lines[line].start as usize;
		out.push_str(&p.text[at..start]);
		out.push_str(&format!("@@ -{old},{count} +{new},{count} @@\n"));
		for n in new..new + count {
			let l = file.get(n as usize - 1).ok_or("status_fold_stale")?;
			out.push(' ');
			out.push_str(l);
			if !l.ends_with('\n') {
				out.push('\n');
			}
		}
		at = start;
		if out.len() > MAX_PREVIEW_BYTES {
			return Err("status_fold_too_large");
		}
	}
	out.push_str(&p.text[at..]);
	if d.trailing && only.is_none_or(|o| o == TRAILING_FOLD) {
		// Everything after the last line the patch shows, on both sides.
		let last = |f: fn(&InlineRow) -> Option<u32>| {
			d.inline.iter().filter_map(f).max().unwrap_or(0)
		};
		let (old, new) = (last(|r| r.old) + 1, last(|r| r.new) + 1);
		let count = (file.len() as u32 + 1).saturating_sub(new);
		if count > 0 {
			if !out.ends_with('\n') {
				out.push('\n');
			}
			out.push_str(&format!("@@ -{old},{count} +{new},{count} @@\n"));
			for l in &file[new as usize - 1..] {
				out.push(' ');
				out.push_str(l);
				if !l.ends_with('\n') {
					out.push_str("\n\\ No newline at end of file\n");
				}
			}
		}
	}
	if out.len() > MAX_PREVIEW_BYTES
		|| out.bytes().filter(|&b| b == b'\n').count() >= MAX_PREVIEW_LINES
	{
		return Err("status_fold_too_large");
	}
	Ok(out)
}

/// Longest line pair compared word by word; longer pairs get no inner
/// highlight (IntelliJ also gives up on huge lines).
const MAX_WORD_TOKENS: usize = 200;

/// Words (letters, digits, `_`), whitespace runs and single punctuation.
fn tokens(s: &str) -> Vec<Range<usize>> {
	let class = |c: char| {
		if c.is_alphanumeric() || c == '_' {
			0
		} else if c.is_whitespace() {
			1
		} else {
			2
		}
	};
	let mut out: Vec<Range<usize>> = Vec::new();
	let mut prev = None;
	for (i, c) in s.char_indices() {
		let k = class(c);
		match out.last_mut() {
			Some(r) if prev == Some(k) && k != 2 => r.end = i + c.len_utf8(),
			_ => out.push(i..i + c.len_utf8()),
		}
		prev = Some(k);
	}
	out
}

/// Changed byte ranges of `a` (old) and `b` (new): tokens outside their
/// longest common subsequence, adjacent ones merged.
pub fn word_diff(a: &str, b: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
	let (ta, tb) = (tokens(a), tokens(b));
	if ta.len() > MAX_WORD_TOKENS || tb.len() > MAX_WORD_TOKENS {
		return (Vec::new(), Vec::new());
	}
	let (n, m) = (ta.len(), tb.len());
	let w = m + 1;
	let mut lcs = vec![0u16; (n + 1) * w];
	for i in (0..n).rev() {
		for j in (0..m).rev() {
			lcs[i * w + j] = if a[ta[i].clone()] == b[tb[j].clone()] {
				lcs[(i + 1) * w + j + 1] + 1
			} else {
				lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
			};
		}
	}
	let (mut ca, mut cb) = (Vec::new(), Vec::new());
	let push = |v: &mut Vec<Range<usize>>, r: Range<usize>| match v.last_mut() {
		Some(last) if last.end == r.start => last.end = r.end,
		_ => v.push(r),
	};
	let (mut i, mut j) = (0, 0);
	while i < n && j < m {
		if a[ta[i].clone()] == b[tb[j].clone()] {
			i += 1;
			j += 1;
		} else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
			push(&mut ca, ta[i].clone());
			i += 1;
		} else {
			push(&mut cb, tb[j].clone());
			j += 1;
		}
	}
	for r in &ta[i..] {
		push(&mut ca, r.clone());
	}
	for r in &tb[j..] {
		push(&mut cb, r.clone());
	}
	(ca, cb)
}

/// Target of "go to line `n`" in a diff: the drawn row numbered `n` on the
/// old or new side, the fold hiding it, or else the next row after it.
pub fn diff_goto_row(p: &Preview, n: u32, old: bool) -> Option<usize> {
	let d = p.diff.as_ref()?;
	let mut after = None;
	for &line in &d.shown {
		let r = d.inline[line];
		let (start, count) = match fold_gap(p, line) {
			Some((o, nw, c)) => (if old { o } else { nw }, c),
			None => match if old { r.old } else { r.new } {
				Some(k) => (k, 1),
				None => continue,
			},
		};
		if (start..start + count).contains(&n) {
			return Some(line);
		}
		if start > n && after.is_none() {
			after = Some(line);
		}
	}
	after
}

/// The raw `a` and `c` of "@@ -a[,b] +c[,d] @@".
fn raw_starts(line: &str) -> Option<(u32, u32)> {
	let rest = line.strip_prefix("@@ -")?;
	let (old, rest) = rest.split_once(' ')?;
	let new = rest.strip_prefix('+')?.split(' ').next()?;
	let start = |r: &str| r.split(',').next()?.parse().ok();
	Some((start(old)?, start(new)?))
}

impl DiffRows {
	pub fn build(text: &str, lines: &[Range<u32>]) -> Self {
		let get =
			|i: usize| &text[lines[i].start as usize..lines[i].end as usize];
		let mut inline = Vec::with_capacity(lines.len());
		let mut side = Vec::new();
		let (mut old, mut new) = (0u32, 0u32);
		// First old line not yet accounted for; the gap up to the next hunk
		// is folded.
		let mut next_old = 1u32;
		let mut in_hunk = false;
		let mut dels: Vec<(u32, usize)> = Vec::new();
		let mut adds: Vec<(u32, usize)> = Vec::new();
		let flush = |inline: &mut Vec<InlineRow>,
		             side: &mut Vec<SideRow>,
		             dels: &mut Vec<(u32, usize)>,
		             adds: &mut Vec<(u32, usize)>| {
			let (nl, nr) = (dels.len(), adds.len());
			if nl > 0 && nr > 0 {
				for &(_, i) in dels.iter().chain(adds.iter()) {
					inline[i].modified = true;
				}
				for (&(_, l), &(_, r)) in dels.iter().zip(adds.iter()) {
					inline[l].pair = Some(r as u32);
					inline[r].pair = Some(l as u32);
				}
			}
			for k in 0..nl.max(nr) {
				let l = dels.get(k).copied();
				let r = adds.get(k).copied();
				side.push(SideRow {
					kind: match (l, r) {
						(Some(_), None) => RowKind::Removed,
						(None, Some(_)) => RowKind::Added,
						_ => RowKind::Context,
					},
					left: l,
					right: r,
					full: None,
					block: Some(Block {
						at: k as u32,
						left: nl as u32,
						right: nr as u32,
					}),
				});
			}
			dels.clear();
			adds.clear();
		};
		for i in 0..lines.len() {
			let l = get(i);
			if let Some((first, first_new)) =
				l.starts_with("@@").then(|| parse_hunk(l)).flatten()
			{
				flush(&mut inline, &mut side, &mut dels, &mut adds);
				let fold = first.saturating_sub(next_old);
				// Counters restart from the header's raw starts.
				let (o, n) = raw_starts(l).unwrap_or((first, first_new));
				old = o;
				new = n;
				in_hunk = true;
				inline.push(InlineRow {
					fold,
					..InlineRow::new(RowKind::Hunk, None, None)
				});
				if fold > 0 {
					side.push(SideRow {
						kind: RowKind::Hunk,
						left: None,
						right: None,
						full: Some(i),
						block: None,
					});
				}
				continue;
			}
			if l.starts_with("diff --git") {
				in_hunk = false;
				next_old = 1;
			}
			if !in_hunk || l.starts_with('\\') {
				flush(&mut inline, &mut side, &mut dels, &mut adds);
				inline.push(InlineRow::new(RowKind::Header, None, None));
				side.push(SideRow {
					kind: RowKind::Header,
					left: None,
					right: None,
					full: Some(i),
					block: None,
				});
				continue;
			}
			match l.as_bytes().first() {
				Some(b'-') => {
					inline.push(InlineRow::new(
						RowKind::Removed,
						Some(old),
						None,
					));
					dels.push((old, i));
					old += 1;
				}
				Some(b'+') => {
					inline.push(InlineRow::new(
						RowKind::Added,
						None,
						Some(new),
					));
					adds.push((new, i));
					new += 1;
				}
				_ => {
					flush(&mut inline, &mut side, &mut dels, &mut adds);
					inline.push(InlineRow::new(
						RowKind::Context,
						Some(old),
						Some(new),
					));
					side.push(SideRow {
						kind: RowKind::Context,
						left: Some((old, i)),
						right: Some((new, i)),
						full: None,
						block: None,
					});
					old += 1;
					new += 1;
				}
			}
			next_old = old;
		}
		flush(&mut inline, &mut side, &mut dels, &mut adds);
		// Patch chrome is hidden, but never everything: a binary or
		// mode-only patch has no hunk and keeps its header visible.
		let has_hunk = inline.iter().any(|r| r.kind == RowKind::Hunk);
		let drawn = |r: &InlineRow| {
			!has_hunk || r.is_code() || (r.kind == RowKind::Hunk && r.fold > 0)
		};
		let shown = (0..lines.len()).filter(|&i| drawn(&inline[i])).collect();
		side.retain(|r| r.full.is_none_or(|i| drawn(&inline[i])));
		let max_num = inline
			.iter()
			.flat_map(|r| [r.old, r.new])
			.flatten()
			.max()
			.unwrap_or(0);
		let tail_context = inline
			.iter()
			.rev()
			.take_while(|r| r.kind == RowKind::Context)
			.count();
		let trailing = has_hunk
			&& tail_context >= 3
			&& !lines.last().is_some_and(|r| {
				text[r.start as usize..r.end as usize].starts_with('\\')
			});
		Self {
			inline,
			side,
			shown,
			max_num,
			trailing,
		}
	}
}

/// All matches of `query` (ASCII case-insensitive when the query is ASCII),
/// as (line, byte start, byte end), capped at `MAX_MATCHES`.
/// With `match_case` the search is exact; with `regex` the query is a
/// regular expression (None when it does not compile).
pub fn find_matches(
	p: &Preview,
	query: &str,
	opts: FindOptions,
) -> Option<Vec<(usize, usize, usize)>> {
	let mut out = Vec::new();
	if query.is_empty() {
		return Some(out);
	}
	if opts.regex {
		let re = regex::RegexBuilder::new(query)
			.case_insensitive(!opts.match_case)
			.size_limit(1 << 20)
			.build()
			.ok()?;
		for ix in 0..p.lines.len() {
			for m in re.find_iter(p.line(ix)) {
				if m.start() < m.end() {
					out.push((ix, m.start(), m.end()));
					if out.len() >= MAX_MATCHES {
						return Some(out);
					}
				}
			}
		}
		return Some(out);
	}
	let q = query.as_bytes();
	let fold = !opts.match_case && query.is_ascii();
	for ix in 0..p.lines.len() {
		let line = p.line(ix).as_bytes();
		let mut i = 0;
		while i + q.len() <= line.len() {
			let hay = &line[i..i + q.len()];
			let hit = if fold {
				hay.eq_ignore_ascii_case(q)
			} else {
				hay == q
			};
			if hit {
				out.push((ix, i, i + q.len()));
				if out.len() >= MAX_MATCHES {
					return Some(out);
				}
				i += q.len();
			} else {
				i += 1;
			}
		}
	}
	Some(out)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindOptions {
	pub match_case: bool,
	pub regex: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffMode {
	Inline,
	SideBySide,
}

/// A text position: (line index, byte column). The line just beyond the
/// index, with column zero, denotes the end of all retained preview text.
pub type Pos = (usize, usize);

/// Text bounds of the rows drawn in the last frame (bounded by viewport).
pub type RowGeometry = Rc<RefCell<Vec<(usize, Bounds<Pixels>)>>>;

pub struct Reader {
	pub matches: Vec<(usize, usize, usize)>,
	pub current: Option<usize>,
	pub anchor: Option<Pos>,
	pub head: Option<Pos>,
	pub selecting: bool,
	pub cursor_line: usize,
	pub diff_mode: DiffMode,
	pub scroll: UniformListScrollHandle,
	pub row_geom: RowGeometry,
	/// Last find query, re-applied when a new preview replaces the text.
	pub find_query: String,
	/// The find / go-to row is shown (Ctrl+F / Ctrl+G open it, Esc closes).
	pub find_open: bool,
	pub find_opts: FindOptions,
	/// The regex query does not compile.
	pub find_invalid: bool,
	/// The tab was pinned (double-clicked); otherwise it is IntelliJ's
	/// italic preview tab that the next opened file replaces.
	pub pinned: bool,
	/// A fold expansion is reading the new-side file.
	pub expanding: bool,
	pub fold_cancel: Option<CancelToken>,
	/// Side by side: the old (left) pane was clicked last, so go-to-line
	/// counts old line numbers.
	pub left_pane: bool,
	/// Side by side: the pane a selection belongs to (true = left).
	pub sel_side: Option<bool>,
	/// Code view bounds of the last frame (pane hit-testing).
	pub view_bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl Default for Reader {
	fn default() -> Self {
		Self {
			matches: Vec::new(),
			current: None,
			anchor: None,
			head: None,
			selecting: false,
			cursor_line: 0,
			diff_mode: DiffMode::Inline,
			scroll: UniformListScrollHandle::new(),
			row_geom: Rc::default(),
			find_query: String::new(),
			find_open: false,
			find_opts: FindOptions::default(),
			find_invalid: false,
			pinned: false,
			expanding: false,
			fold_cancel: None,
			left_pane: false,
			sel_side: None,
			view_bounds: Rc::default(),
		}
	}
}

impl Reader {
	pub fn reset_for_new_preview(&mut self) {
		self.row_geom.borrow_mut().clear();
		self.matches.clear();
		self.current = None;
		self.anchor = None;
		self.head = None;
		self.selecting = false;
		self.cursor_line = 0;
		self.pinned = false;
		self.left_pane = false;
		self.expanding = false;
		if let Some(c) = self.fold_cancel.take() {
			c.cancel();
		}
		self.scroll.scroll_to_item(0, ScrollStrategy::Top);
	}

	pub fn release_retained(&mut self) {
		*self.row_geom.borrow_mut() = Vec::new();
		self.matches = Vec::new();
		self.anchor = None;
		self.head = None;
		self.current = None;
		self.selecting = false;
	}

	pub fn selection(&self) -> Option<(Pos, Pos)> {
		let (a, h) = (self.anchor?, self.head?);
		(a != h).then(|| if a <= h { (a, h) } else { (h, a) })
	}

	/// Byte range of the selection within `line` (whose length is `len`).
	pub fn selected_in_line(
		&self,
		line: usize,
		len: usize,
	) -> Option<Range<usize>> {
		let ((l0, c0), (l1, c1)) = self.selection()?;
		if line < l0 || line > l1 {
			return None;
		}
		let s = if line == l0 { c0.min(len) } else { 0 };
		let e = if line == l1 { c1.min(len) } else { len };
		(s < e || (line < l1 && s == e)).then_some(s..e)
	}
}

/// Copy the original byte span, preserving line endings and unindexed text.
/// A diff copies what is on screen instead: body lines without their
/// markers, patch chrome and fold rows left out.
#[cfg(test)]
pub fn selected_text(p: &Preview, sel: (Pos, Pos)) -> String {
	selected_text_on(p, sel, None)
}

/// [`selected_text`] for one pane of the side-by-side viewer: `Some(true)`
/// copies what the left (old) pane shows, `Some(false)` the right (new).
pub fn selected_text_on(
	p: &Preview,
	(a, b): (Pos, Pos),
	side: Option<bool>,
) -> String {
	let (a, b) = if a <= b { (a, b) } else { (b, a) };
	if let Some(d) = p.diff.as_ref().filter(|_| p.has_hunks()) {
		let raw = |line: usize| {
			let r = &p.lines[line];
			(r.start as usize, r.end as usize)
		};
		let mut out = String::new();
		let last = b.0.min(p.lines.len().saturating_sub(1));
		let mut first = true;
		for line in a.0..=last {
			if line >= p.lines.len()
				|| !d.inline[line].is_code()
				|| !on_side(d.inline[line].kind, side)
			{
				continue;
			}
			let (start, end) = raw(line);
			let mut s = start + if line == a.0 { a.1.max(1) } else { 1 };
			let mut e = if line == b.0 { start + b.1 } else { end };
			s = s.min(end);
			e = e.clamp(s, end);
			while !p.text.is_char_boundary(s) {
				s -= 1;
			}
			while !p.text.is_char_boundary(e) {
				e += 1;
			}
			if !first {
				out.push('\n');
			}
			first = false;
			out.push_str(&p.text[s..e]);
		}
		return out;
	}
	let offset = |(line, column): Pos, round_up: bool| {
		let Some(range) = p.lines.get(line) else {
			return p.text.len();
		};
		let mut byte = range.start as usize
			+ column.min((range.end - range.start) as usize);
		while !p.text.is_char_boundary(byte) {
			if round_up {
				byte += 1;
			} else {
				byte -= 1;
			}
		}
		byte
	};
	p.text[offset(a, false)..offset(b, true)].to_string()
}

/// Whether a diff line of `kind` is drawn in pane `side` (None: unified).
pub fn on_side(kind: RowKind, side: Option<bool>) -> bool {
	match side {
		Some(true) => kind != RowKind::Added,
		Some(false) => kind != RowKind::Removed,
		None => true,
	}
}

/// Splits `line` into disjoint styled runs: syntax color, find match and
/// selection backgrounds (selection wins).
fn line_highlights(
	line: &str,
	lang: Language,
	theme: &SyntaxTheme,
	finds: &[(usize, usize, bool)],
	sel: Option<Range<usize>>,
	words: &[Range<usize>],
) -> Vec<(Range<usize>, HighlightStyle)> {
	let mut cuts = vec![0, line.len()];
	let tokens = highlight_line(line, lang, theme);
	let mut colors: Vec<(Range<usize>, gpui::Rgba)> = Vec::new();
	if tokens.iter().map(|(t, _)| t.len()).sum::<usize>() == line.len() {
		let mut at = 0;
		for (t, c) in tokens {
			colors.push((at..at + t.len(), c));
			at += t.len();
			cuts.push(at);
		}
	}
	for &(s, e, _) in finds {
		cuts.extend([s, e]);
	}
	if let Some(r) = &sel {
		cuts.extend([r.start, r.end]);
	}
	for r in words {
		cuts.extend([r.start, r.end]);
	}
	cuts.retain(|&c| c <= line.len() && line.is_char_boundary(c));
	cuts.sort_unstable();
	cuts.dedup();
	let mut out = Vec::new();
	for w in cuts.windows(2) {
		let (s, e) = (w[0], w[1]);
		if s == e {
			continue;
		}
		let color = colors
			.iter()
			.find(|(r, _)| r.start <= s && e <= r.end)
			.map(|(_, c)| (*c).into());
		let find = finds.iter().find(|&&(fs, fe, _)| fs <= s && e <= fe);
		let bg = if sel.as_ref().is_some_and(|r| r.start <= s && e <= r.end) {
			Some(rgb(pal().selection_bg).into())
		} else {
			find.map(|&(_, _, cur)| {
				rgb(if cur {
					pal().find_current_bg
				} else {
					pal().find_bg
				})
				.into()
			})
			.or_else(|| {
				words
					.iter()
					.any(|r| r.start <= s && e <= r.end)
					.then(|| rgb(pal().diff_word_bg).into())
			})
		};
		out.push((
			s..e,
			HighlightStyle {
				color,
				background_color: bg,
				..Default::default()
			},
		));
	}
	out
}

impl WorkbenchModel {
	pub fn reader_scroll_to(&mut self, line: usize) {
		let row = self.display_row_for_line(line);
		self.reader.cursor_line = line;
		self.reader
			.scroll
			.scroll_to_item(row, ScrollStrategy::Center);
	}

	/// Display row showing preview line `line` in the current diff mode;
	/// a hidden header line maps to the next drawn row.
	fn display_row_for_line(&self, line: usize) -> usize {
		match (&self.preview, self.reader.diff_mode) {
			(Some(p), DiffMode::SideBySide) => p
				.diff
				.as_ref()
				.and_then(|d| {
					let has = |r: &SideRow, f: &dyn Fn(usize) -> bool| {
						r.full.is_some_and(f)
							|| r.left.is_some_and(|(_, i)| f(i))
							|| r.right.is_some_and(|(_, i)| f(i))
					};
					d.side.iter().position(|r| has(r, &|i| i == line)).or_else(
						|| d.side.iter().position(|r| has(r, &|i| i >= line)),
					)
				})
				.unwrap_or(0),
			(Some(p), DiffMode::Inline) => p.inline_row_of(line),
			(None, _) => line,
		}
	}

	/// Preview line at display row `row` (inverse of the above).
	fn line_for_display_row(&self, row: usize) -> usize {
		match (&self.preview, self.reader.diff_mode) {
			(Some(p), DiffMode::SideBySide) if p.diff.is_some() => p
				.diff
				.as_ref()
				.and_then(|d| d.side.get(row))
				.and_then(|r| r.full.or(r.right.or(r.left).map(|(_, i)| i)))
				.unwrap_or(0),
			(Some(p), _) => p.inline_line(row),
			(None, _) => row,
		}
	}

	pub fn run_find(&mut self, query: &str, cx: &mut Context<Self>) {
		query.clone_into(&mut self.reader.find_query);
		if self.preview.is_none() {
			return;
		}
		self.refind();
		if let Some(&(line, _, _)) = self.reader.matches.first() {
			self.reader_scroll_to(line);
		}
		app_log!(
			"[APP:FIND: matches={} first_line={}]",
			self.reader.matches.len(),
			self.reader.matches.first().map(|m| m.0 + 1).unwrap_or(0)
		);
		cx.notify();
	}

	/// Recomputes the matches of the last query against the current preview.
	pub(crate) fn refind(&mut self) {
		let Some(p) = &self.preview else {
			return;
		};
		let found =
			find_matches(p, &self.reader.find_query, self.reader.find_opts);
		self.reader.find_invalid = found.is_none();
		self.reader.matches = found.unwrap_or_default();
		// Matches in hidden patch chrome or on a diff marker cannot be shown.
		self.reader
			.matches
			.retain(|m| p.is_text_row(m.0) && m.1 >= p.text_start(m.0));
		self.reader.current = (!self.reader.matches.is_empty()).then_some(0);
	}

	/// Ctrl+F / Ctrl+G: reveal the find row and focus its find or go-to
	/// field; the query typed before is searched again.
	pub fn open_find(
		&mut self,
		goto: bool,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if !self.reader.find_open {
			self.reader.find_open = true;
			let q = self.find_input.read(cx).text().to_string();
			self.run_find(&q, cx);
			app_log!("[APP:FIND_BAR: open]");
		}
		let input = if goto {
			&self.goto_input
		} else {
			&self.find_input
		};
		window.focus(&input.read(cx).handle());
		cx.notify();
	}

	/// Esc: hide the find row and its highlights, back to the text.
	pub fn close_find(&mut self, cx: &mut Context<Self>) {
		self.pending_focus = Some(self.reader_focus.clone());
		if self.reader.find_open {
			self.reader.find_open = false;
			self.reader.find_query.clear();
			self.reader.matches.clear();
			self.reader.current = None;
			self.reader.find_invalid = false;
			app_log!("[APP:FIND_BAR: closed]");
		}
		cx.notify();
	}

	pub fn toggle_find_option(&mut self, regex: bool, cx: &mut Context<Self>) {
		let o = &mut self.reader.find_opts;
		if regex {
			o.regex = !o.regex;
		} else {
			o.match_case = !o.match_case;
		}
		app_log!(
			"[APP:FIND_OPTS: match_case={} regex={}]",
			o.match_case,
			o.regex
		);
		let q = self.reader.find_query.clone();
		self.run_find(&q, cx);
	}

	/// F7: the next change block (unified or side-by-side), wrapping around.
	pub fn next_diff(&mut self, cx: &mut Context<Self>) {
		self.step_change(true, cx)
	}

	/// Shift+F7: the previous change block, wrapping around.
	pub fn prev_diff(&mut self, cx: &mut Context<Self>) {
		self.step_change(false, cx)
	}

	fn step_change(&mut self, forward: bool, cx: &mut Context<Self>) {
		let Some(d) = self.preview.as_ref().and_then(|p| p.diff.as_ref())
		else {
			return;
		};
		let changed = |k: usize| d.inline[d.shown[k]].change().is_some();
		let starts: Vec<usize> = (0..d.shown.len())
			.filter(|&k| changed(k) && (k == 0 || !changed(k - 1)))
			.map(|k| d.shown[k])
			.collect();
		let cur = self.reader.cursor_line;
		let target = if forward {
			starts.iter().find(|&&l| l > cur).or(starts.first())
		} else {
			starts.iter().rev().find(|&&l| l < cur).or(starts.last())
		};
		if let Some(&line) = target {
			self.reader_scroll_to(line);
			app_log!("[APP:DIFF_STEP: line={}]", line + 1);
			cx.notify();
		}
	}

	/// Click on a "⋯ N unchanged lines" fold (`only`) or Expand All (None):
	/// reads the new-side file in the background and splices the missing
	/// lines in. Only the expanded lines stay retained, under the same
	/// budget as any preview; on failure the fold stays and the status says
	/// why.
	pub fn expand_folds(
		&mut self,
		only: Option<usize>,
		cx: &mut Context<Self>,
	) {
		if self.reader.expanding || !self.accepting_work() {
			return;
		}
		let Some(p) = &self.preview else {
			return;
		};
		let source = match &p.source {
			PreviewSource::WorkingChanges | PreviewSource::UnstagedChanges => {
				GitSource::Working
			}
			PreviewSource::StagedChanges => GitSource::Staged,
			PreviewSource::CommitDiff { sha } => GitSource::Commit(sha.clone()),
			PreviewSource::Compare { from, to } => {
				GitSource::Range(from.clone(), to.clone())
			}
			_ => return,
		};
		let (Some(path), Some(root)) = (p.path.clone(), self.repo_root())
		else {
			return;
		};
		let shown = Arc::as_ptr(&p.text) as *const u8 as usize;
		self.reader.expanding = true;
		let cancel = crate::arm_cancel(&mut self.reader.fold_cancel);
		app_log!("[APP:FOLD_EXPANDING: all={}]", only.is_none());
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();
		let bg = cx.background_executor().clone();
		let job_cancel = cancel.clone();
		self.spawn_owned(
			cx,
			crate::lifecycle::JobKind::CancellableRead,
			Some(job_cancel),
			async move {
				let result = bg
					.spawn(async move {
						read_new_side(&root, &source, &path, cancel)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					model.finish_expand(shown, only, result, cx)
				});
			},
		);
		cx.notify();
	}

	fn finish_expand(
		&mut self,
		shown: usize,
		only: Option<usize>,
		result: Result<String, String>,
		cx: &mut Context<Self>,
	) {
		self.reader.expanding = false;
		self.reader.fold_cancel = None;
		let Some(p) = self
			.preview
			.as_ref()
			.filter(|p| Arc::as_ptr(&p.text) as *const u8 as usize == shown)
		else {
			// Another file (or version) replaced the preview meanwhile.
			return;
		};
		let expanded = match result {
			Ok(content) => expand_folds(p, &content, only)
				.map_err(|key| crate::i18n::Msg::new(key, [])),
			Err(e) => Err(crate::i18n::Msg::new("status_fold_failed", [e])),
		};
		let tail_done = only.is_none_or(|o| o == TRAILING_FOLD)
			|| p.diff.as_ref().is_some_and(|d| !d.trailing);
		let next = expanded.map(|text| {
			let mut n = Preview::new(
				p.source.clone(),
				p.path.clone(),
				text,
				true,
				p.lang,
			);
			if let Some(d) = n.diff.as_mut().filter(|_| tail_done) {
				d.trailing = false;
			}
			n
		});
		let next = next.and_then(|n| {
			crate::paste::lock_pending(&self.paste_pending.clone())
				.admit_ui(
					Some(&n),
					self.paste_preview.as_ref(),
					self.paste_detail.as_ref(),
				)
				.map(|()| n)
		});
		match next {
			Ok(next) => {
				// Rows above each spliced gap keep their index; the cursor
				// moves down by what was inserted above it.
				let before = p.lines.len();
				let cursor = self.reader.cursor_line;
				let d = p.diff.as_ref();
				let shift: usize = d.map_or(0, |d| {
					d.shown
						.iter()
						.filter(|&&l| {
							l <= cursor && only.is_none_or(|o| o == l)
						})
						.filter_map(|&l| fold_gap(p, l))
						.map(|(_, _, c)| c as usize + 1)
						.sum()
				});
				let added = next.lines.len() - before;
				self.preview = Some(next);
				self.reader.cursor_line = cursor + shift;
				self.reader.anchor = None;
				self.reader.head = None;
				self.refind();
				app_log!("[APP:FOLD_EXPANDED: lines={added}]");
			}
			Err(msg) => {
				app_log!("[APP:FOLD_REFUSED: {}]", msg.key);
				self.status = msg;
			}
		}
		cx.notify();
	}

	pub fn toggle_diff_mode(&mut self, cx: &mut Context<Self>) {
		self.reader.diff_mode = match self.reader.diff_mode {
			DiffMode::Inline => DiffMode::SideBySide,
			DiffMode::SideBySide => DiffMode::Inline,
		};
		self.reader.anchor = None;
		self.reader.head = None;
		self.reader.sel_side = None;
		app_log!("[APP:DIFF_MODE: {:?}]", self.reader.diff_mode);
		cx.notify();
	}

	pub fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
		let n = self.reader.matches.len();
		if n == 0 {
			return;
		}
		let cur = self.reader.current.unwrap_or(0);
		let next = if forward {
			(cur + 1) % n
		} else {
			(cur + n - 1) % n
		};
		self.reader.current = Some(next);
		let line = self.reader.matches[next].0;
		self.reader_scroll_to(line);
		app_log!("[APP:FIND_AT: {}/{} line={}]", next + 1, n, line + 1);
		cx.notify();
	}

	pub fn goto_line(&mut self, text: &str, cx: &mut Context<Self>) {
		let Some(p) = &self.preview else {
			return;
		};
		// A diff counts file lines: new side, or old side when the left
		// pane of the side-by-side viewer was clicked last.
		if let Some(d) = p.diff.as_ref().filter(|_| p.has_hunks()) {
			let old = self.reader.diff_mode == DiffMode::SideBySide
				&& self.reader.left_pane;
			let side = |r: &InlineRow| if old { r.old } else { r.new };
			let max = d.inline.iter().filter_map(side).max().unwrap_or(0);
			let target = text
				.trim()
				.parse::<u32>()
				.ok()
				.filter(|n| (1..=max).contains(n))
				.and_then(|n| Some((n, diff_goto_row(p, n, old)?)));
			match target {
				Some((n, line)) => {
					let text_row = p.is_text_row(line);
					let (start, len) = (p.text_start(line), p.line(line).len());
					self.reader_scroll_to(line);
					self.reader.sel_side = (self.reader.diff_mode
						== DiffMode::SideBySide)
						.then_some(old);
					self.reader.anchor = text_row.then_some((line, start));
					self.reader.head = text_row.then_some((line, len));
					app_log!(
						"[APP:GOTO: line={} side={}]",
						n,
						if old { "old" } else { "new" }
					);
					self.set_status("status_goto", [n.to_string()]);
				}
				None => {
					self.set_status("status_goto_invalid", [max.to_string()]);
					app_log!("[APP:GOTO_INVALID]");
				}
			}
			cx.notify();
			return;
		}
		let total = p.lines.len();
		match text.trim().parse::<usize>() {
			Ok(n) if n >= 1 && n <= total => {
				// A hidden patch header line resolves to the next drawn line.
				let (line, line_len) = self
					.preview
					.as_ref()
					.map(|p| {
						let l = p.inline_line(p.inline_row_of(n - 1));
						(l, p.line(l).len())
					})
					.unwrap_or((n - 1, 0));
				self.reader_scroll_to(line);
				self.reader.anchor = Some((line, 0));
				self.reader.head = Some((line, line_len));
				app_log!("[APP:GOTO: line={}]", n);
				self.set_status("status_goto", [n.to_string()]);
			}
			_ => {
				self.set_status("status_goto_invalid", [total.to_string()]);
				app_log!("[APP:GOTO_INVALID]");
			}
		}
		cx.notify();
	}

	fn hit_test(&self, pos: Point<Pixels>, window: &mut Window) -> Option<Pos> {
		let p = self.preview.as_ref()?;
		let geom = self.reader.row_geom.borrow();
		// Side by side, only the selection's pane counts.
		let center = {
			let v = self.reader.view_bounds.get();
			v.left() + v.size.width / 2.
		};
		let rows: Vec<(usize, Bounds<Pixels>)> = geom
			.iter()
			.copied()
			.filter(|(_, b)| match self.reader.sel_side {
				Some(left) => (b.left() < center) == left,
				None => true,
			})
			.collect();
		let (line, b) = rows
			.iter()
			.find(|(_, b)| pos.y >= b.top() && pos.y < b.bottom())
			.copied()
			.or_else(|| {
				// Past the rows (or on a blank filler row): the nearest row
				// above, or the first one when above them all.
				rows.iter()
					.filter(|(_, b)| b.top() <= pos.y)
					.max_by(|x, y| {
						x.1.top()
							.partial_cmp(&y.1.top())
							.unwrap_or(std::cmp::Ordering::Equal)
					})
					.or_else(|| rows.iter().min_by_key(|(l, _)| *l))
					.copied()
			})?;
		// Pointer coordinates belong to the clipped text rendered in this row
		// (after a hidden diff marker).
		let off = p.text_start(line);
		let (text, _) = clip_line(&p.line(line)[off..]);
		if text.is_empty() || !p.is_text_row(line) {
			return Some((line, off));
		}
		let run = TextRun {
			len: text.len(),
			font: font(EDITOR_FONT),
			color: rgb(pal().text).into(),
			background_color: None,
			underline: None,
			strikethrough: None,
		};
		let shaped = window.text_system().shape_line(
			SharedString::from(text.to_string()),
			px(CODE_TEXT),
			&[run],
			None,
		);
		Some((line, off + shaped.closest_index_for_x(pos.x - b.left())))
	}

	fn reader_mouse_down(
		&mut self,
		ev: &MouseDownEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		window.focus(&self.reader_focus);
		let side = self.reader.diff_mode == DiffMode::SideBySide;
		if side {
			let b = self.reader.view_bounds.get();
			self.reader.left_pane =
				ev.position.x < b.left() + b.size.width / 2.;
		}
		let pane = side.then_some(self.reader.left_pane);
		if !(ev.modifiers.shift && self.reader.sel_side == pane) {
			self.reader.sel_side = pane;
		}
		let Some(pos) = self.hit_test(ev.position, window) else {
			return;
		};
		if ev.modifiers.shift && self.reader.anchor.is_some() {
			self.reader.head = Some(pos);
		} else if ev.click_count >= 2 {
			// Double click selects the whole line.
			let len = self
				.preview
				.as_ref()
				.map(|p| p.line(pos.0).len())
				.unwrap_or(0);
			self.reader.anchor = Some((pos.0, 0));
			self.reader.head = Some((pos.0, len));
		} else {
			self.reader.anchor = Some(pos);
			self.reader.head = Some(pos);
		}
		self.reader.cursor_line = pos.0;
		self.reader.selecting = true;
		cx.notify();
	}

	fn reader_mouse_move(
		&mut self,
		ev: &MouseMoveEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if !self.reader.selecting {
			return;
		}
		if ev.pressed_button != Some(MouseButton::Left) {
			self.reader_mouse_up(cx);
			return;
		}
		if let Some(pos) = self.hit_test(ev.position, window) {
			if self.reader.head != Some(pos) {
				self.reader.head = Some(pos);
				cx.notify();
			}
		}
	}

	pub fn reader_mouse_up(&mut self, cx: &mut Context<Self>) {
		if !self.reader.selecting {
			return;
		}
		self.reader.selecting = false;
		if let (Some(p), Some(sel)) = (&self.preview, self.reader.selection()) {
			let chars = selected_text_on(p, sel, self.reader.sel_side)
				.chars()
				.count();
			app_log!(
				"[APP:SELECTION: from={}:{} to={}:{} chars={}]",
				sel.0 .0 + 1,
				sel.0 .1,
				sel.1 .0 + 1,
				sel.1 .1,
				chars
			);
		}
		cx.notify();
	}

	/// Copies the reader selection. Returns false when nothing is selected.
	pub fn copy_reader_selection(&mut self, cx: &mut Context<Self>) -> bool {
		if !self.can_copy_preview() {
			return false;
		}
		let (Some(p), Some(sel)) = (&self.preview, self.reader.selection())
		else {
			return false;
		};
		let text = selected_text_on(p, sel, self.reader.sel_side);
		let chars = text.chars().count();
		match snip_core::clip::write_text(&text) {
			Ok(()) => {
				self.set_status("status_copied_selection", [chars.to_string()]);
				app_log!(
					"[APP:SELECTION_COPIED: chars={} fnv={:x}]",
					chars,
					fnv1a(text.as_bytes())
				);
			}
			Err(e) => {
				self.set_status("status_clipboard_failed", [e.to_string()]);
			}
		}
		cx.notify();
		true
	}

	pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
		if let Some(p) = &self.preview {
			self.reader.sel_side = (self.reader.diff_mode
				== DiffMode::SideBySide)
				.then_some(self.reader.left_pane);
			self.reader.anchor = Some((0, 0));
			self.reader.head = Some((p.lines.len(), 0));
			cx.notify();
		}
	}

	pub fn move_cursor_line(&mut self, delta: isize, cx: &mut Context<Self>) {
		if let Some(p) = &self.preview {
			// Step over drawn rows so hidden patch headers are skipped.
			let rows = match (&p.diff, self.reader.diff_mode) {
				(Some(d), DiffMode::SideBySide) => d.side.len(),
				_ => p.inline_rows(),
			};
			let max = rows.saturating_sub(1) as isize;
			let row = self.display_row_for_line(self.reader.cursor_line);
			let next = (row as isize + delta).clamp(0, max) as usize;
			let line = self.line_for_display_row(next);
			self.reader_scroll_to(line);
			cx.notify();
		}
	}

	/// Virtualized code rows. `paste` shows the selected paste item (read
	/// only, own scroll); otherwise the reader preview with find/selection.
	pub fn render_code_view(
		&self,
		paste: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let p = if paste {
			self.paste_detail.as_ref()
		} else {
			self.preview.as_ref()
		};
		let Some(p) = p else {
			return div().flex_1().into_any_element();
		};
		let side = !paste
			&& p.is_diff
			&& self.reader.diff_mode == DiffMode::SideBySide;
		let body_rows = if side {
			p.diff.as_ref().map(|d| d.side.len()).unwrap_or(0)
		} else {
			p.inline_rows()
		};
		let tail = !paste && p.diff.as_ref().is_some_and(|d| d.trailing);
		let rows = body_rows + usize::from(tail);
		// Row (not line) index of the widest drawn line.
		let widest = if side { 0 } else { p.inline_row_of(p.widest) };
		let scroll = if paste {
			self.paste_scroll.clone()
		} else {
			self.reader.scroll.clone()
		};
		let list = uniform_list(
			if paste { "paste-rows" } else { "code-rows" },
			rows,
			cx.processor(move |this, range: Range<usize>, _window, cx| {
				if !paste {
					this.reader.row_geom.borrow_mut().clear();
				}
				let p = if paste {
					this.paste_detail.as_ref()
				} else {
					this.preview.as_ref()
				};
				let Some(p) = p else {
					return Vec::new();
				};
				let num_w = gutter_num_w(p);
				range
					.map(|row| {
						if row >= body_rows {
							let indent = if side { 1. } else { 2. } * num_w;
							this.fold_row(
								("code-line-end", row),
								TRAILING_FOLD,
								t("diff_fold_end", this.locale).to_string(),
								indent + GUTTER_GAP,
								cx,
							)
							.w_full()
							.into_any_element()
						} else if side {
							this.side_row(p, row, cx)
						} else {
							this.inline_row(p, p.inline_line(row), !paste, cx)
						}
					})
					.collect::<Vec<_>>()
			}),
		)
		.track_scroll(scroll)
		.with_horizontal_sizing_behavior(if side {
			ListHorizontalSizingBehavior::FitList
		} else {
			ListHorizontalSizingBehavior::Unconstrained
		})
		.with_width_from_item(Some(widest))
		.when(side, |l| {
			l.with_decoration(Ribbons {
				model: cx.weak_entity(),
			})
		})
		.size_full();
		let view = div()
			.id(if paste {
				"paste-code-view"
			} else {
				"code-view"
			})
			.relative()
			.flex_1()
			.min_h_0()
			.font_family(EDITOR_FONT)
			.text_size(px(CODE_TEXT));
		if paste {
			return view.child(list).into_any_element();
		}
		let bounds = self.reader.view_bounds.clone();
		view.on_mouse_down(
			MouseButton::Left,
			cx.listener(Self::reader_mouse_down),
		)
		.on_mouse_move(cx.listener(Self::reader_mouse_move))
		.on_mouse_up(
			MouseButton::Left,
			cx.listener(|this, _, _, cx| this.reader_mouse_up(cx)),
		)
		.child(list)
		.child(
			gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
				.absolute()
				.top_0()
				.left_0()
				.size_full(),
		)
		.into_any_element()
	}

	fn inline_row(
		&self,
		p: &Preview,
		ix: usize,
		interactive: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let theme = SyntaxTheme::default();
		let diff_row = p.diff.as_ref().and_then(|d| d.inline.get(ix)).copied();
		let num_w = gutter_num_w(p);
		let geom = interactive.then(|| self.reader.row_geom.clone());
		let geom_probe = move |d: gpui::Div| match geom {
			Some(geom) => d.child(
				gpui::canvas(
					move |b, _, _| geom.borrow_mut().push((ix, b)),
					|_, _, _, _| {},
				)
				.absolute()
				.top_0()
				.left_0()
				.size_full(),
			),
			None => d,
		};
		if let Some(r) = diff_row.filter(|r| r.kind == RowKind::Hunk) {
			return self
				.fold_row(
					("code-line", ix),
					ix,
					tf("diff_fold", self.locale, &[r.fold]),
					num_w * 2.0 + GUTTER_GAP,
					cx,
				)
				.min_w_full()
				.child(geom_probe(div().absolute().size_full()))
				.into_any_element();
		}
		let off = p.text_start(ix);
		let (render_text, _) = clip_line(&p.line(ix)[off..]);
		let (finds, sel) = if interactive {
			let current = self.reader.current;
			let finds: Vec<(usize, usize, bool)> = self
				.reader
				.matches
				.iter()
				.enumerate()
				.filter(|(_, m)| m.0 == ix)
				.map(|(i, m)| (m.1 - off, m.2 - off, Some(i) == current))
				.collect();
			let sel = self
				.reader
				.selected_in_line(ix, off + render_text.len())
				.map(|r| {
					r.start.saturating_sub(off)..r.end.saturating_sub(off)
				});
			(finds, sel)
		} else {
			(Vec::new(), None)
		};
		// Visible patch headers (a patch without hunks) keep diff colouring;
		// hunk bodies are coloured as their file.
		let lang = match diff_row.map(|r| r.kind) {
			Some(RowKind::Header) => Language::Diff,
			_ => p.code_lang(),
		};
		let words = p.word_ranges(ix);
		let hl =
			line_highlights(render_text, lang, &theme, &finds, sel, &words);
		let row_bg = match diff_row.and_then(|r| r.change()) {
			Some(c) => Some(c.bg()),
			None if interactive && ix == self.reader.cursor_line => {
				Some(pal().current_line_bg)
			}
			None => None,
		};
		// Line numbers stay legible on a tinted row.
		let num = |n: Option<u32>| {
			gutter_num(num_w, n)
				.when(row_bg.is_some() && diff_row.is_some(), |d| {
					d.text_color(rgb(pal().text_muted))
				})
		};
		let gutter = match diff_row {
			Some(r) => div()
				.flex()
				.flex_shrink_0()
				.child(num(r.old))
				.child(num(r.new)),
			None => {
				div().flex().flex_shrink_0().child(num(Some(ix as u32 + 1)))
			}
		};
		let probe_id = format!("code-text:{}", ix + 1);
		div()
			.id(("code-line", ix))
			.flex()
			.flex_row()
			// Fill the viewport so row backgrounds are not ragged.
			.min_w_full()
			.h(px(LINE_H))
			.whitespace_nowrap()
			.when_some(row_bg, |d, c| d.bg(rgb(c)))
			// Keyboard focus cue now that the editor has no focus frame; every
			// line carries the 2px edge so the cursor line does not shift.
			.border_l_2()
			.border_color(
				if interactive
					&& self.reader_active
					&& ix == self.reader.cursor_line
				{
					rgb(pal().focus_ring).into()
				} else {
					gpui::transparent_black()
				},
			)
			.child(gutter.mr(px(GUTTER_GAP)))
			.child(
				geom_probe(
					div().relative().flex_shrink_0().pr(px(16.)).child(
						StyledText::new(SharedString::from(
							render_text.to_string(),
						))
						.with_highlights(hl),
					),
				)
				.when(interactive, |d| {
					d.children(crate::ui::probe(&self.probes, probe_id))
				}),
			)
			.into_any_element()
	}

	/// IntelliJ's collapsed unchanged fragment: a muted band between dashed
	/// separators in place of the `@@` header, saying how many lines the
	/// patch left out. Clicking it reads them in.
	fn fold_row(
		&self,
		id: (&'static str, usize),
		line: usize,
		label: String,
		indent: f32,
		cx: &mut Context<Self>,
	) -> gpui::Stateful<gpui::Div> {
		let probe_id = if line == TRAILING_FOLD {
			"diff-fold:end".to_string()
		} else {
			format!("diff-fold:{}", line + 1)
		};
		div()
			.id(id)
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.gap(px(4.))
			.h(px(LINE_H))
			.pl(px(indent))
			.whitespace_nowrap()
			.bg(rgb(pal().diff_hunk_bg))
			.font_family(UI_FONT)
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			.cursor_pointer()
			.hover(|s| s.text_color(rgb(pal().link)))
			.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
			.on_click(cx.listener(move |this, _, _, cx| {
				this.expand_folds(Some(line), cx)
			}))
			.child(
				gpui::canvas(
					|_, _, _| {},
					|b, _, window, _| {
						for y in [b.top() + px(0.5), b.bottom() - px(0.5)] {
							let mut path = gpui::PathBuilder::stroke(px(1.))
								.dash_array(&[px(3.), px(3.)]);
							path.move_to(gpui::point(b.left(), y));
							path.line_to(gpui::point(b.right(), y));
							if let Ok(path) = path.build() {
								window.paint_path(path, rgb(pal().divider));
							}
						}
					},
				)
				.absolute()
				.top_0()
				.left_0()
				.size_full(),
			)
			.child(crate::icons::icon(crate::icons::Icon::ExpandAll, 12.))
			.child(label)
			.children(crate::ui::probe(&self.probes, probe_id))
	}

	fn side_row(
		&self,
		p: &Preview,
		ix: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let theme = SyntaxTheme::default();
		let Some(r) = p.diff.as_ref().and_then(|d| d.side.get(ix)).copied()
		else {
			return div().into_any_element();
		};
		let num_w = gutter_num_w(p);
		if let Some(line) = r.full {
			if r.kind == RowKind::Hunk {
				let fold = p
					.diff
					.as_ref()
					.and_then(|d| d.inline.get(line))
					.map_or(0, |r| r.fold);
				return self
					.fold_row(
						("side-line", ix),
						line,
						tf("diff_fold", self.locale, &[fold]),
						num_w + GUTTER_GAP,
						cx,
					)
					.w_full()
					.into_any_element();
			}
			let (text, _) = clip_line(p.line(line));
			return div()
				.id(("side-line", ix))
				.flex()
				.w_full()
				.h(px(LINE_H))
				.pl(px(8.))
				.whitespace_nowrap()
				.overflow_hidden()
				.child(
					StyledText::new(SharedString::from(text.to_string()))
						.with_highlights(line_highlights(
							text,
							Language::Diff,
							&theme,
							&[],
							None,
							&[],
						)),
				)
				.into_any_element();
		}
		let lang = p.code_lang();
		let tint = r.block.map(|b| b.change().bg());
		// IntelliJ mirrors the left gutter: both line-number columns sit
		// against the divider, with the ribbons between them.
		let half = |cell: Option<(u32, usize)>, left: bool, side_id: String| {
			let body = cell.map(|(n, line)| {
				let off = p.text_start(line);
				let (text, _) = clip_line(&p.line(line)[off..]);
				let finds: Vec<(usize, usize, bool)> = self
					.reader
					.matches
					.iter()
					.enumerate()
					.filter(|(_, m)| m.0 == line)
					.map(|(i, m)| {
						(m.1 - off, m.2 - off, Some(i) == self.reader.current)
					})
					.collect();
				let words = p.word_ranges(line);
				let sel = (self.reader.sel_side == Some(left))
					.then(|| {
						self.reader.selected_in_line(line, off + text.len())
					})
					.flatten()
					.map(|r| {
						r.start.saturating_sub(off)..r.end.saturating_sub(off)
					});
				let geom = self.reader.row_geom.clone();
				let code = div()
					.flex_1()
					.min_w_0()
					.overflow_hidden()
					.pl(px(if left { 8. } else { GUTTER_GAP }))
					.child(
						div()
							.relative()
							.child(
								StyledText::new(SharedString::from(
									text.to_string(),
								))
								.with_highlights(line_highlights(
									text, lang, &theme, &finds, sel, &words,
								)),
							)
							.child(
								gpui::canvas(
									move |b, _, _| {
										geom.borrow_mut().push((line, b))
									},
									|_, _, _, _| {},
								)
								.absolute()
								.top_0()
								.left_0()
								.size_full(),
							),
					);
				let num = gutter_num(num_w, Some(n))
					.when(tint.is_some(), |d| {
						d.text_color(rgb(pal().text_muted))
					});
				if left {
					div().flex().size_full().child(code).child(num)
				} else {
					div().flex().size_full().child(num).child(code)
				}
			});
			div()
				.relative()
				.flex_1()
				.min_w_0()
				.overflow_hidden()
				.whitespace_nowrap()
				.when_some(tint.filter(|_| cell.is_some()), |d, c| d.bg(rgb(c)))
				.children(body)
				.children(crate::ui::probe(&self.probes, side_id))
		};
		div()
			.id(("side-line", ix))
			.flex()
			.flex_row()
			.w_full()
			.h(px(LINE_H))
			.child(half(r.left, true, format!("side-left:{ix}")))
			.child(
				div()
					.flex_shrink_0()
					.w(px(RIBBON_W))
					.h_full()
					.border_l_1()
					.border_r_1()
					.border_color(rgb(pal().divider)),
			)
			.child(half(r.right, false, format!("side-right:{ix}")))
			.into_any_element()
	}
}

/// The new-side file a diff was made from (bounded by the preview cap).
fn read_new_side(
	root: &std::path::Path,
	source: &GitSource,
	path: &str,
	cancel: CancelToken,
) -> Result<String, String> {
	let opts = RunOptions {
		cancel: Some(cancel),
		max_stdout: MAX_PREVIEW_BYTES,
		overflow: Overflow::Error,
		..RunOptions::interactive(None)
	};
	let git = Git::open_with(root, &opts).map_err(|e| e.to_string())?;
	snip_core::gitsrc::read_changed_file_with(
		&git,
		source,
		path,
		MAX_PREVIEW_BYTES as u64,
		&opts,
	)
	.map_err(|e| e.to_string())?
	.and_then(|f| f.content)
	.ok_or_else(|| "no new-side text".to_string())
}

/// Space between the line-number gutter and the code.
const GUTTER_GAP: f32 = 8.0;
/// Width of the divider between side-by-side panes that holds the ribbons.
const RIBBON_W: f32 = 28.0;

/// The side-by-side viewer's connectors: for every change block on screen,
/// a band from the left block to the right one whose bottom edge is a
/// cubic Bézier, like IntelliJ's diff divider.
struct Ribbons {
	model: gpui::WeakEntity<WorkbenchModel>,
}

impl gpui::UniformListDecoration for Ribbons {
	fn compute(
		&self,
		visible: Range<usize>,
		bounds: Bounds<Pixels>,
		scroll: Point<Pixels>,
		row_h: Pixels,
		_count: usize,
		_window: &mut Window,
		cx: &mut gpui::App,
	) -> AnyElement {
		// (first row, left rows, right rows, colour) of blocks on screen.
		let mut blocks: Vec<(usize, u32, u32, u32)> = Vec::new();
		if let Some(d) = self.model.upgrade().and_then(|m| {
			m.read(cx).preview.as_ref().and_then(|p| {
				p.diff.as_ref().map(|d| {
					visible
						.clone()
						.filter_map(|ix| Some((ix, d.side.get(ix)?.block?)))
						.map(|(ix, b)| {
							(
								ix - b.at as usize,
								b.left,
								b.right,
								b.change().bg(),
							)
						})
						.collect::<Vec<_>>()
				})
			})
		}) {
			blocks = d;
			blocks.dedup_by_key(|b| b.0);
		}
		gpui::canvas(
			|_, _, _| {},
			move |b, _, window, _| {
				let x0 = b.left() + (b.size.width - px(RIBBON_W)) / 2. + px(1.);
				let x1 = x0 + px(RIBBON_W - 2.);
				let xm = (x0 + x1) / 2.;
				window.with_content_mask(
					Some(gpui::ContentMask { bounds: b }),
					|window| {
						for &(row, left, right, color) in &blocks {
							let top = b.top() + scroll.y + row_h * row as f32;
							let lb = top + row_h * left as f32;
							let rb = top + row_h * right as f32;
							let mut path = gpui::PathBuilder::fill();
							path.move_to(gpui::point(x0, top));
							path.line_to(gpui::point(x1, top));
							path.line_to(gpui::point(x1, rb));
							path.cubic_bezier_to(
								gpui::point(x0, lb),
								gpui::point(xm, rb),
								gpui::point(xm, lb),
							);
							path.close();
							if let Ok(path) = path.build() {
								window.paint_path(path, rgb(color));
							}
						}
					},
				);
			},
		)
		.w(bounds.size.width)
		.h(bounds.size.height)
		.into_any_element()
	}
}

/// Width of one line-number column, sized for the largest number shown.
fn gutter_num_w(p: &Preview) -> f32 {
	let max = p.diff.as_ref().map_or(p.lines.len() as u32, |d| d.max_num);
	let digits = max.max(1).ilog10() + 1;
	digits.max(3) as f32 * 7.5 + 12.0
}

fn gutter_num(w: f32, n: Option<u32>) -> gpui::Div {
	div()
		.flex_shrink_0()
		.w(px(w))
		.pr(px(4.))
		.flex()
		.justify_end()
		.text_color(rgb(pal().line_number))
		.child(n.map(|n| n.to_string()).unwrap_or_default())
}

/// A line cut to what is shaped on screen, and whether it was cut.
fn clip_line(text: &str) -> (&str, bool) {
	if text.len() <= MAX_RENDER_LINE_BYTES {
		return (text, false);
	}
	let mut cut = MAX_RENDER_LINE_BYTES;
	while cut > 0 && !text.is_char_boundary(cut) {
		cut -= 1;
	}
	(&text[..cut], true)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn retained_bytes_counts_spare_diff_and_source_capacity() {
		let mut p = Preview::new(
			PreviewSource::CommitDiff {
				sha: String::with_capacity(8192),
			},
			Some("a.txt".into()),
			"@@ -1 +1 @@\n-a\n+b\n".into(),
			true,
			Language::Diff,
		);
		assert!(p.retained_bytes() > 8192);
		let before = p.retained_bytes();
		let rows = &mut p.diff.as_mut().unwrap().side;
		let old = rows.capacity();
		rows.reserve(4096);
		let growth = (rows.capacity() - old) * std::mem::size_of::<SideRow>();
		assert_eq!(p.retained_bytes(), before + growth);
		let before = p.retained_bytes();
		let old = p.lines.capacity();
		p.lines.reserve(4096);
		assert_eq!(
			p.retained_bytes(),
			before
				+ (p.lines.capacity() - old)
					* std::mem::size_of::<Range<u32>>()
		);
		assert_eq!(p.text.as_ref(), "@@ -1 +1 @@\n-a\n+b\n");
	}

	fn preview(text: &str, diff: bool) -> Preview {
		Preview::new(
			PreviewSource::WorkingFile,
			None,
			text.to_string(),
			diff,
			Language::Plain,
		)
	}

	#[test]
	fn lines_index_without_copies_and_crlf() {
		let p = preview("a\r\nbb\n\nlast", false);
		assert_eq!(p.lines.len(), 4);
		assert_eq!(p.line(0), "a");
		assert_eq!(p.line(1), "bb");
		assert_eq!(p.line(2), "");
		assert_eq!(p.line(3), "last");
		assert_eq!(p.widest, 3);
	}

	#[test]
	fn preview_is_bounded() {
		let big = "x\n".repeat(MAX_PREVIEW_LINES + 10);
		let p = preview(&big, false);
		assert_eq!(p.lines.len(), MAX_PREVIEW_LINES);
		assert!(p.notice.is_some());
		let huge = "y".repeat(MAX_PREVIEW_BYTES + 5);
		let p = preview(&huge, false);
		assert!(p.text.len() <= MAX_PREVIEW_BYTES);
	}

	#[test]
	fn find_is_case_insensitive_for_ascii_and_exact_for_cjk() {
		let p = preview("Hello hello\n中文測試 中文\n", false);
		let find = |q: &str, match_case: bool, regex: bool| {
			find_matches(&p, q, FindOptions { match_case, regex })
		};
		assert_eq!(
			find("HELLO", false, false),
			Some(vec![(0, 0, 5), (0, 6, 11)])
		);
		assert_eq!(find("HELLO", true, false), Some(vec![]));
		assert_eq!(
			find("h.llo", false, true),
			Some(vec![(0, 0, 5), (0, 6, 11)])
		);
		assert_eq!(find("h.llo", true, true), Some(vec![(0, 6, 11)]));
		assert_eq!(find("(", false, true), None, "invalid regex");
		let m = find("中文", false, false).unwrap();
		assert_eq!(m.len(), 2);
		assert_eq!(&p.line(1)[m[1].1..m[1].2], "中文");
	}

	#[test]
	fn selection_text_spans_lines() {
		let p = preview("abc\ndef\nghi", false);
		assert_eq!(selected_text(&p, ((0, 1), (2, 2))), "bc\ndef\ngh");
		let r = Reader {
			anchor: Some((2, 2)),
			head: Some((0, 1)),
			..Default::default()
		};
		assert_eq!(r.selection(), Some(((0, 1), (2, 2))));
		assert_eq!(r.selected_in_line(1, 3), Some(0..3));
		assert_eq!(r.selected_in_line(0, 3), Some(1..3));
	}

	#[test]
	fn diff_rows_number_and_pair_lines() {
		let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -3,3 +3,3 @@\n ctx\n-old\n+new\n+extra\n tail\n";
		let p = preview(diff, true);
		let d = p.diff.as_ref().unwrap();
		assert_eq!(d.inline[3].kind, RowKind::Hunk);
		assert_eq!(d.inline[3].fold, 2, "old lines 1-2 are folded");
		assert_eq!(d.shown[0], 3, "the fold row replaces the @@ header");
		assert_eq!(
			d.inline[4],
			InlineRow {
				kind: RowKind::Context,
				old: Some(3),
				new: Some(3),
				modified: false,
				fold: 0,
				pair: None
			}
		);
		assert_eq!(
			d.inline[5],
			InlineRow {
				kind: RowKind::Removed,
				old: Some(4),
				new: None,
				modified: true,
				fold: 0,
				pair: Some(6)
			}
		);
		assert_eq!(
			d.inline[6],
			InlineRow {
				kind: RowKind::Added,
				old: None,
				new: Some(4),
				modified: true,
				fold: 0,
				pair: Some(5)
			}
		);
		assert_eq!(
			d.inline[8],
			InlineRow {
				kind: RowKind::Context,
				old: Some(5),
				new: Some(6),
				modified: false,
				fold: 0,
				pair: None
			}
		);
		// Side by side: old/new paired on one row, extra addition alone.
		let changed: Vec<_> =
			d.side.iter().filter(|r| r.full.is_none()).collect();
		assert_eq!(changed[1].left, Some((4, 5)));
		assert_eq!(changed[1].right, Some((4, 6)));
		assert_eq!(changed[2].left, None);
		assert_eq!(changed[2].right, Some((5, 7)));
		// One modified block (1 old line, 2 new) drives tint and ribbon.
		assert_eq!(changed[0].block, None);
		assert_eq!(
			changed[2].block,
			Some(Block {
				at: 1,
				left: 1,
				right: 2
			})
		);
		assert_eq!(changed[1].block.unwrap().change(), Change::Modified);
		assert_eq!(d.inline[5].change(), Some(Change::Modified));
		assert_eq!(d.inline[4].change(), None);
	}

	#[test]
	fn folds_count_the_lines_between_hunks() {
		// New file (-0,0), deletion (+0,0) and a pure insertion after line 5.
		for (patch, folds) in [
			("@@ -0,0 +1,2 @@\n+a\n+b\n", vec![]),
			("@@ -1,2 +0,0 @@\n-a\n-b\n", vec![]),
			("@@ -5,0 +6,1 @@\n+x\n", vec![5]),
			(
				"@@ -2,2 +2,2 @@\n-a\n+b\n c\n@@ -10,1 +10,1 @@\n-d\n+e\n",
				vec![1, 6],
			),
		] {
			let p = preview(patch, true);
			let d = p.diff.as_ref().unwrap();
			let got: Vec<u32> = d
				.shown
				.iter()
				.map(|&l| d.inline[l])
				.filter(|r| r.kind == RowKind::Hunk)
				.map(|r| r.fold)
				.collect();
			assert_eq!(got, folds, "{patch}");
			let side_folds =
				d.side.iter().filter(|r| r.kind == RowKind::Hunk).count();
			assert_eq!(side_folds, folds.len());
		}
	}

	#[test]
	fn word_diff_marks_changed_tokens_only() {
		let (old, new) =
			word_diff("let x = foo(a, b);", "let y = foo(a, c, b);");
		let pick = |s: &'static str, r: &[Range<usize>]| -> Vec<&'static str> {
			r.iter().map(|r| &s[r.clone()]).collect()
		};
		assert_eq!(pick("let x = foo(a, b);", &old), ["x"]);
		assert_eq!(pick("let y = foo(a, c, b);", &new), ["y", "c, "]);
		// Multibyte words stay whole and on char boundaries.
		let (o, n) = word_diff("名稱 = 舊值", "名稱 = 新值");
		assert_eq!(&"名稱 = 舊值"[o[0].clone()], "舊值");
		assert_eq!(&"名稱 = 新值"[n[0].clone()], "新值");
		// Identical lines: nothing; overlong lines: nothing (work is capped).
		assert_eq!(word_diff("same", "same"), (vec![], vec![]));
		let long = "a ".repeat(MAX_WORD_TOKENS);
		assert_eq!(word_diff(&long, "b"), (vec![], vec![]));
		// A modified block pairs its lines; words are relative to the text
		// after the marker.
		let p = preview("@@ -1,1 +1,1 @@\n-let x = 1;\n+let y = 1;\n", true);
		assert_eq!(p.word_ranges(1), vec![4..5]);
		assert_eq!(p.word_ranges(2), vec![4..5]);
		assert!(p.word_ranges(0).is_empty());
	}

	#[test]
	fn fold_expansion_splices_the_new_side_and_is_charged() {
		let file: String = (1..=12).map(|i| format!("line {i}\n")).collect();
		let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -4,2 +4,2 @@\n line 4\n-old 5\n+line 5\n@@ -10,1 +10,1 @@\n-old 10\n+line 10\n";
		let p = preview(diff, true);
		let d = p.diff.as_ref().unwrap();
		let folds: Vec<usize> = d
			.shown
			.iter()
			.copied()
			.filter(|&l| d.inline[l].fold > 0)
			.collect();
		assert_eq!(folds, vec![3, 7]);
		assert_eq!(fold_gap(&p, 3), Some((1, 1, 3)));
		assert_eq!(fold_gap(&p, 7), Some((6, 6, 4)));
		// One fold: the gap is spliced in as context, still a valid patch.
		let one = expand_folds(&p, &file, Some(7)).unwrap();
		let q = preview(&one, true);
		let qd = q.diff.as_ref().unwrap();
		let folded: Vec<u32> = qd
			.shown
			.iter()
			.map(|&l| qd.inline[l].fold)
			.filter(|&f| f > 0)
			.collect();
		assert_eq!(folded, vec![3], "only the first fold is left");
		let news: Vec<u32> =
			qd.shown.iter().filter_map(|&l| qd.inline[l].new).collect();
		assert_eq!(news, (4..=10).collect::<Vec<u32>>());
		assert_eq!(q.line(qd.shown[4]), " line 6");
		// Every fold, and the budget sees every spliced byte.
		let all = expand_folds(&p, &file, None).unwrap();
		let r = preview(&all, true);
		let rd = r.diff.as_ref().unwrap();
		assert!(rd.shown.iter().all(|&l| rd.inline[l].kind != RowKind::Hunk));
		assert_eq!(
			rd.shown.iter().filter(|&&l| rd.inline[l].is_code()).count(),
			12
		);
		assert!(
			r.retained_bytes() >= p.retained_bytes() + (all.len() - diff.len())
		);
		assert!(crate::paste::admit_preview_state(Some(&r), None, None).is_ok());
		// A file that changed since the patch is refused, the fold stays.
		let edited = file.replace("line 4", "edited");
		assert_eq!(expand_folds(&p, &edited, None), Err("status_fold_stale"));
		// So is a spliced result beyond the preview line cap.
		let n = MAX_PREVIEW_LINES + 1;
		let far = preview(&format!("@@ -{n},1 +{n},1 @@\n-old\n+new\n"), true);
		let big = format!("{}new\n", "x\n".repeat(MAX_PREVIEW_LINES));
		assert_eq!(
			expand_folds(&far, &big, None),
			Err("status_fold_too_large")
		);
	}

	#[test]
	fn trailing_fold_follows_full_context_and_expands_to_eof() {
		// Full trailing context: the file may go on.
		let diff = "@@ -2,4 +2,4 @@\n-a\n+A\n c\n d\n e\n";
		let p = preview(diff, true);
		assert!(p.diff.as_ref().unwrap().trailing);
		// Short context, a "\\ No newline" end, a new file: it ends there.
		for done in [
			"@@ -2,2 +2,2 @@\n-a\n+A\n c\n",
			"@@ -2,4 +2,4 @@\n-a\n+A\n c\n d\n e\n\\ No newline at end of file\n",
			"@@ -0,0 +1,3 @@\n+a\n+b\n+c\n",
		] {
			assert!(!preview(done, true).diff.as_ref().unwrap().trailing, "{done}");
		}
		let file = "x\nA\nc\nd\ne\nf\ng";
		// Expanding the tail only appends the rest, keeping the missing
		// final newline as git would.
		let out = expand_folds(&p, file, Some(TRAILING_FOLD)).unwrap();
		assert_eq!(
			out,
			format!(
				"{diff}@@ -6,2 +6,2 @@\n f\n g\n\\ No newline at end of file\n"
			)
		);
		let q = preview(&out, true);
		let qd = q.diff.as_ref().unwrap();
		assert!(!qd.trailing);
		let news: Vec<u32> =
			qd.shown.iter().filter_map(|&l| qd.inline[l].new).collect();
		assert_eq!(news, (2..=7).collect::<Vec<u32>>());
		// Expanding one middle fold leaves the tail alone.
		assert_eq!(
			expand_folds(&p, file, Some(0))
				.unwrap()
				.matches("@@")
				.count(),
			4
		);
	}

	#[test]
	fn side_by_side_selection_copies_one_pane() {
		let diff = "@@ -1,3 +1,3 @@\n ctx\n-old\n+new\n tail\n";
		let p = preview(diff, true);
		let all = ((0, 0), (p.lines.len(), 0));
		assert_eq!(selected_text_on(&p, all, Some(true)), "ctx\nold\ntail");
		assert_eq!(selected_text_on(&p, all, Some(false)), "ctx\nnew\ntail");
		assert_eq!(selected_text_on(&p, all, None), "ctx\nold\nnew\ntail");
		// A drag inside the right pane from "new" col 1 to "tail" col 3.
		assert_eq!(
			selected_text_on(&p, ((3, 2), (4, 3)), Some(false)),
			"ew\nta"
		);
		assert!(on_side(RowKind::Context, Some(true)));
		assert!(!on_side(RowKind::Added, Some(true)));
		assert!(!on_side(RowKind::Removed, Some(false)));
	}

	#[test]
	fn goto_line_in_a_diff_counts_file_lines() {
		let diff =
			"@@ -4,3 +4,4 @@\n a\n-b\n+B\n+B2\n c\n@@ -20,1 +21,1 @@\n-x\n+y\n";
		let p = preview(diff, true);
		// New side: line 5 is "B" (preview line 3), 6 is "B2".
		assert_eq!(diff_goto_row(&p, 5, false), Some(3));
		assert_eq!(diff_goto_row(&p, 6, false), Some(4));
		// Old side: line 5 is "b" (preview line 2).
		assert_eq!(diff_goto_row(&p, 5, true), Some(2));
		// Hidden by a fold: the fold row; before any: the first fold.
		assert_eq!(diff_goto_row(&p, 10, false), Some(6));
		assert_eq!(diff_goto_row(&p, 2, false), Some(0));
		assert_eq!(diff_goto_row(&p, 21, false), Some(8));
		assert_eq!(diff_goto_row(&p, 20, true), Some(7));
		// Past the last hunk: nothing.
		assert_eq!(diff_goto_row(&p, 30, false), None);
	}

	#[test]
	fn diff_copy_and_find_skip_markers_and_chrome() {
		let diff =
			"@@ -3,2 +3,2 @@\n ctx\n-old\n+new\n\\ No newline at end of file\n";
		let p = preview(diff, true);
		// Select all copies the lines on screen without +/-/space markers.
		assert_eq!(
			selected_text(&p, ((0, 0), (p.lines.len(), 0))),
			"ctx\nold\nnew"
		);
		// A partial selection keeps raw columns (marker at column 0).
		assert_eq!(selected_text(&p, ((1, 2), (3, 2))), "tx\nold\nn");
		assert_eq!(p.text_start(1), 1);
		assert_eq!(p.text_start(0), 0);
		assert!(!p.is_text_row(0), "fold row");
		assert!(!p.is_text_row(4), "no-newline marker is hidden");
		let hits = find_matches(&p, "+new", FindOptions::default()).unwrap();
		assert_eq!(hits, vec![(3, 0, 4)]);
		assert!(hits.iter().all(|m| m.1 < p.text_start(m.0)));
	}

	#[test]
	fn patch_header_is_hidden_but_text_is_kept() {
		let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n\\ No newline at end of file\n";
		let p = preview(diff, true);
		let d = p.diff.as_ref().unwrap();
		// Only the body is drawn: headers, the `@@` line (nothing folded
		// before line 1) and "\ No newline at end of file" are not.
		assert_eq!(d.shown, vec![5, 6, 7]);
		assert_eq!(p.inline_rows(), 3);
		assert_eq!(p.inline_line(0), 5);
		assert_eq!(p.inline_row_of(0), 0, "hidden line maps to next row");
		assert_eq!(p.inline_row_of(6), 1);
		assert!(!p.is_shown(4) && p.is_shown(5) && !p.is_shown(8));
		assert!(d.side.iter().all(|r| r.full.is_none()));
		assert_eq!(&*p.text, diff, "retained text (copy source) unchanged");
		assert_eq!(p.widest, 7);
		// No hunk (binary patch): nothing is hidden.
		let bin = "diff --git a/x b/x\nBinary files a/x and b/x differ\n";
		let p = preview(bin, true);
		assert_eq!(p.inline_rows(), 2);
	}

	#[test]
	fn selection_preserves_raw_line_endings_and_utf8_endpoints() {
		let p = preview("ab繁體\r\n第二𝄞行\r\nlast\n", false);
		// Deliberately split UTF-8 characters at both endpoints; selection
		// includes their whole bytes and preserves the intervening CRLF.
		let reader = Reader {
			anchor: Some((1, 8)),
			head: Some((0, 3)),
			..Default::default()
		};
		assert_eq!(
			selected_text(&p, reader.selection().unwrap()),
			"繁體\r\n第二𝄞"
		);
		assert_eq!(
			selected_text(&p, ((0, 0), (0, p.line(0).len()))),
			"ab繁體",
			"a manual whole-line selection still excludes its terminator"
		);
		assert_eq!(selected_text(&p, ((1, 0), (1, 6))), "第二");
	}

	#[test]
	fn selection_end_of_text_preserves_bom_and_terminal_newlines() {
		for raw in [
			"",
			"\u{feff}  exact source\t \r\n第二行\r\n\r\n",
			"one\n",
			"one\r\n",
			"\r\n\r\n",
			"tail",
		] {
			let p = preview(raw, false);
			assert_eq!(selected_text(&p, ((0, 0), (p.lines.len(), 0))), raw);
		}
	}

	#[test]
	fn selection_end_of_text_reaches_beyond_the_line_index_cap() {
		let raw = "row\r\n".repeat(MAX_PREVIEW_LINES + 1);
		let p = preview(&raw, false);
		assert_eq!(p.lines.len(), MAX_PREVIEW_LINES);
		assert_eq!(&*p.text, raw);
		let reader = Reader {
			anchor: Some((0, 0)),
			head: Some((p.lines.len(), 0)),
			..Default::default()
		};
		assert_eq!(selected_text(&p, reader.selection().unwrap()), raw);
		assert_eq!(
			reader.selected_in_line(MAX_PREVIEW_LINES - 1, 3),
			Some(0..3),
			"the last indexed row remains fully highlighted"
		);
	}

	#[test]
	fn highlights_are_disjoint_and_cover_line() {
		let theme = SyntaxTheme::default();
		let h = line_highlights(
			"let x = 1;",
			Language::Rust,
			&theme,
			&[(4, 5, true)],
			Some(2..6),
			&[0..3, 8..10],
		);
		let mut at = 0;
		for (r, _) in &h {
			assert_eq!(r.start, at);
			at = r.end;
		}
		assert_eq!(at, 10);
	}

	#[test]
	fn test_long_line_1mib_responsiveness_and_integrity() {
		use std::time::Instant;

		// Construct a 1MiB line with multibyte CJK and surrogate characters near 4096 cut boundary
		let mut line_content = String::with_capacity(1024 * 1024);
		line_content.push_str(&"a".repeat(4091));
		line_content.push_str("繁體中文𝄞符號"); // cross 4096 boundary
		let remaining = (1024usize * 1024).saturating_sub(line_content.len());
		line_content.push_str(&"b".repeat(remaining));

		let p = Preview::new(
			PreviewSource::WorkingFile,
			Some("huge_line.rs".into()),
			line_content.clone(),
			false,
			Language::Rust,
		);

		// Assert retained full 1MiB content
		assert_eq!(p.line(0).len(), 1024 * 1024);

		let theme = SyntaxTheme::default();
		let start = Instant::now();

		// Exercise the same clipping used by rendering and pointer hit-testing.
		let raw = p.line(0);
		let (render_text, is_truncated) = clip_line(raw);

		assert!(is_truncated);
		assert_eq!(render_text.len(), 4094);
		assert!(render_text.ends_with('繁'));
		assert!(raw.is_char_boundary(render_text.len()));

		// Shape & highlight the clipped render text
		let finds = vec![(10, 20, false), (4080, 4095, true)];
		let sel = Some(4000..4092);
		let hl = line_highlights(
			render_text,
			Language::Rust,
			&theme,
			&finds,
			sel,
			&[],
		);

		let elapsed = start.elapsed();
		assert!(
			elapsed.as_millis() < 50,
			"1MiB line render clipping took too long: {:?}",
			elapsed
		);

		// Highlights cover render_text completely and disjointly
		let mut at = 0;
		for (r, _) in &hl {
			assert_eq!(r.start, at);
			at = r.end;
		}
		assert_eq!(at, render_text.len());

		// Original full source selection & copy integrity:
		// Full line selection extracts all 1MiB bytes without corruption
		let full_sel_text = selected_text(&p, ((0, 0), (0, 1024 * 1024)));
		assert_eq!(full_sel_text.len(), 1024 * 1024);
		assert_eq!(full_sel_text, line_content);

		// Subslice spanning the 4096 boundary preserves multibyte characters exactly
		let cross_sel_text = selected_text(&p, ((0, 4091), (0, 4110)));
		assert!(cross_sel_text.starts_with("繁體"));
	}

	#[test]
	fn release_retained_drops_match_capacity() {
		let mut reader = Reader::default();
		let geometry = reader.row_geom.clone();
		geometry.borrow_mut().reserve(32);
		geometry.borrow_mut().push((0, Bounds::default()));
		reader.reset_for_new_preview();
		assert!(geometry.borrow().is_empty());
		assert!(geometry.borrow().capacity() >= 32);
		geometry.borrow_mut().push((1, Bounds::default()));
		reader.matches.reserve(32);
		reader.matches.push((0, 1, 2));
		reader.anchor = Some((3, 4));
		reader.head = Some((5, 6));
		reader.current = Some(0);
		reader.selecting = true;
		reader.release_retained();
		assert!(reader.matches.is_empty());
		assert_eq!(reader.matches.capacity(), 0);
		assert!(geometry.borrow().is_empty());
		assert_eq!(geometry.borrow().capacity(), 0);
		assert!(reader.anchor.is_none());
		assert!(reader.head.is_none());
		assert!(reader.current.is_none());
		assert!(!reader.selecting);
	}
}
