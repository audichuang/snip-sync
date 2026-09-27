//! Read-only code reader: one retained preview (`Arc<str>` + line index),
//! virtualized rows, find, go-to-line, character selection with copy, and
//! inline / side-by-side diff. No per-line `String` is retained.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
	div, font, prelude::*, px, rgb, uniform_list, AnyElement, Bounds, Context,
	HighlightStyle, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent,
	MouseMoveEvent, Pixels, Point, ScrollStrategy, SharedString, StyledText,
	TextRun, UniformListScrollHandle, Window,
};

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
}

/// One side-by-side row: indices into preview lines for each side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SideRow {
	pub kind: RowKind,
	pub left: Option<(u32, usize)>,
	pub right: Option<(u32, usize)>,
	/// Header/hunk rows span both sides with this preview line.
	pub full: Option<usize>,
}

pub struct DiffRows {
	/// One entry per preview line (row kinds and numbers).
	pub inline: Vec<InlineRow>,
	pub side: Vec<SideRow>,
	/// Preview lines drawn in inline mode: patch headers (`diff --git`,
	/// `index`, `---`/`+++`, mode lines) are hidden once a hunk exists.
	pub shown: Vec<usize>,
	/// Largest old/new line number, sizes the gutter.
	pub max_num: u32,
}

fn parse_hunk(line: &str) -> Option<(u32, u32)> {
	// "@@ -a[,b] +c[,d] @@"
	let rest = line.strip_prefix("@@ -")?;
	let (old, rest) = rest.split_once(' ')?;
	let new = rest.strip_prefix('+')?.split(' ').next()?;
	let first = |s: &str| s.split(',').next()?.parse().ok();
	Some((first(old)?, first(new)?))
}

impl DiffRows {
	pub fn build(text: &str, lines: &[Range<u32>]) -> Self {
		let get =
			|i: usize| &text[lines[i].start as usize..lines[i].end as usize];
		let mut inline = Vec::with_capacity(lines.len());
		let mut side = Vec::new();
		let (mut old, mut new) = (0u32, 0u32);
		let mut in_hunk = false;
		let mut dels: Vec<(u32, usize)> = Vec::new();
		let mut adds: Vec<(u32, usize)> = Vec::new();
		let flush = |side: &mut Vec<SideRow>,
		             dels: &mut Vec<(u32, usize)>,
		             adds: &mut Vec<(u32, usize)>| {
			let n = dels.len().max(adds.len());
			for k in 0..n {
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
				});
			}
			dels.clear();
			adds.clear();
		};
		for i in 0..lines.len() {
			let l = get(i);
			if let Some((o, n)) =
				l.starts_with("@@").then(|| parse_hunk(l)).flatten()
			{
				flush(&mut side, &mut dels, &mut adds);
				old = o;
				new = n;
				in_hunk = true;
				inline.push(InlineRow {
					kind: RowKind::Hunk,
					old: None,
					new: None,
				});
				side.push(SideRow {
					kind: RowKind::Hunk,
					left: None,
					right: None,
					full: Some(i),
				});
				continue;
			}
			if l.starts_with("diff --git") {
				in_hunk = false;
			}
			if !in_hunk || l.starts_with('\\') {
				flush(&mut side, &mut dels, &mut adds);
				inline.push(InlineRow {
					kind: RowKind::Header,
					old: None,
					new: None,
				});
				side.push(SideRow {
					kind: RowKind::Header,
					left: None,
					right: None,
					full: Some(i),
				});
				continue;
			}
			match l.as_bytes().first() {
				Some(b'-') => {
					inline.push(InlineRow {
						kind: RowKind::Removed,
						old: Some(old),
						new: None,
					});
					dels.push((old, i));
					old += 1;
				}
				Some(b'+') => {
					inline.push(InlineRow {
						kind: RowKind::Added,
						old: None,
						new: Some(new),
					});
					adds.push((new, i));
					new += 1;
				}
				_ => {
					flush(&mut side, &mut dels, &mut adds);
					inline.push(InlineRow {
						kind: RowKind::Context,
						old: Some(old),
						new: Some(new),
					});
					side.push(SideRow {
						kind: RowKind::Context,
						left: Some((old, i)),
						right: Some((new, i)),
						full: None,
					});
					old += 1;
					new += 1;
				}
			}
		}
		flush(&mut side, &mut dels, &mut adds);
		// Hide the raw patch header, but never everything: a binary or
		// mode-only patch has no hunk and keeps its header visible.
		let has_hunk = inline.iter().any(|r| r.kind == RowKind::Hunk);
		let hidden = |i: usize| {
			has_hunk
				&& inline[i].kind == RowKind::Header
				&& !get(i).starts_with('\\')
		};
		let shown = (0..lines.len()).filter(|&i| !hidden(i)).collect();
		side.retain(|r| r.full.is_none_or(|i| !hidden(i)));
		let max_num = inline
			.iter()
			.flat_map(|r| [r.old, r.new])
			.flatten()
			.max()
			.unwrap_or(0);
		Self {
			inline,
			side,
			shown,
			max_num,
		}
	}
}

/// All matches of `query` (ASCII case-insensitive when the query is ASCII),
/// as (line, byte start, byte end), capped at `MAX_MATCHES`.
pub fn find_matches(p: &Preview, query: &str) -> Vec<(usize, usize, usize)> {
	let mut out = Vec::new();
	if query.is_empty() {
		return out;
	}
	let q = query.as_bytes();
	let fold = query.is_ascii();
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
					return out;
				}
				i += q.len();
			} else {
				i += 1;
			}
		}
	}
	out
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
pub fn selected_text(p: &Preview, (a, b): (Pos, Pos)) -> String {
	let (a, b) = if a <= b { (a, b) } else { (b, a) };
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

/// Splits `line` into disjoint styled runs: syntax color, find match and
/// selection backgrounds (selection wins).
fn line_highlights(
	line: &str,
	lang: Language,
	theme: &SyntaxTheme,
	finds: &[(usize, usize, bool)],
	sel: Option<Range<usize>>,
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
		self.reader.matches = find_matches(p, &self.reader.find_query);
		// Matches inside hidden patch headers cannot be shown.
		self.reader.matches.retain(|m| p.is_shown(m.0));
		self.reader.current = (!self.reader.matches.is_empty()).then_some(0);
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
		let total = if let Some(p) = &self.preview {
			p.lines.len()
		} else {
			return;
		};
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
		let (line, b) = geom
			.iter()
			.find(|(_, b)| pos.y >= b.top() && pos.y < b.bottom())
			.copied()
			.or_else(|| {
				// Dragging past the visible rows clamps to the first/last.
				let first = geom.iter().min_by_key(|(l, _)| *l).copied()?;
				let last = geom.iter().max_by_key(|(l, _)| *l).copied()?;
				Some(if pos.y < first.1.top() { first } else { last })
			})?;
		// Pointer coordinates belong to the clipped text rendered in this row.
		let (text, _) = clip_line(p.line(line));
		if text.is_empty() {
			return Some((line, 0));
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
		Some((line, shaped.closest_index_for_x(pos.x - b.left())))
	}

	fn reader_mouse_down(
		&mut self,
		ev: &MouseDownEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		window.focus(&self.reader_focus);
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
			let chars = selected_text(p, sel).chars().count();
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
		let text = selected_text(p, sel);
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
		let rows = if side {
			p.diff.as_ref().map(|d| d.side.len()).unwrap_or(0)
		} else {
			p.inline_rows()
		};
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
			cx.processor(move |this, range: Range<usize>, _window, _cx| {
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
				if side {
					range.map(|ix| this.side_row(p, ix)).collect::<Vec<_>>()
				} else {
					range
						.map(|row| {
							this.inline_row(p, p.inline_line(row), !paste)
						})
						.collect::<Vec<_>>()
				}
			}),
		)
		.track_scroll(scroll)
		.with_horizontal_sizing_behavior(if side {
			ListHorizontalSizingBehavior::FitList
		} else {
			ListHorizontalSizingBehavior::Unconstrained
		})
		.with_width_from_item(Some(widest))
		.size_full();
		let view = div()
			.id(if paste {
				"paste-code-view"
			} else {
				"code-view"
			})
			.flex_1()
			.min_h_0()
			.font_family(EDITOR_FONT)
			.text_size(px(CODE_TEXT));
		if paste {
			return view.child(list).into_any_element();
		}
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
		.into_any_element()
	}

	fn inline_row(
		&self,
		p: &Preview,
		ix: usize,
		interactive: bool,
	) -> AnyElement {
		let theme = SyntaxTheme::default();
		let (render_text, _) = clip_line(p.line(ix));
		let (finds, sel) = if interactive {
			let current = self.reader.current;
			let finds: Vec<(usize, usize, bool)> = self
				.reader
				.matches
				.iter()
				.enumerate()
				.filter(|(_, m)| m.0 == ix)
				.map(|(i, m)| (m.1, m.2, Some(i) == current))
				.collect();
			(finds, self.reader.selected_in_line(ix, render_text.len()))
		} else {
			(Vec::new(), None)
		};
		let diff_row = p.diff.as_ref().and_then(|d| d.inline.get(ix)).copied();
		let kind = diff_row.map(|r| r.kind);
		// Hunk and header rows keep diff colouring; code is coloured as its
		// file with the +/- marker tinted.
		let lang = match kind {
			Some(RowKind::Hunk | RowKind::Header) => Language::Diff,
			_ => p.code_lang(),
		};
		let mut hl = line_highlights(render_text, lang, &theme, &finds, sel);
		match kind {
			Some(RowKind::Added) => tint_marker(&mut hl, theme.diff_add),
			Some(RowKind::Removed) => tint_marker(&mut hl, theme.diff_remove),
			_ => {}
		}
		let row_bg = match kind {
			Some(RowKind::Added) => Some(pal().diff_add_bg),
			Some(RowKind::Removed) => Some(pal().diff_del_bg),
			Some(RowKind::Hunk) => Some(pal().diff_hunk_bg),
			_ if interactive && ix == self.reader.cursor_line => {
				Some(pal().current_line_bg)
			}
			_ => None,
		};
		let num_w = gutter_num_w(p);
		let num = |n: Option<u32>| gutter_num(num_w, n);
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
		let geom = interactive.then(|| self.reader.row_geom.clone());
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
				div()
					.relative()
					.flex_shrink_0()
					.pr(px(16.))
					.child(
						StyledText::new(SharedString::from(
							render_text.to_string(),
						))
						.with_highlights(hl),
					)
					.when_some(geom, |d, geom| {
						d.child(
							gpui::canvas(
								move |b, _, _| geom.borrow_mut().push((ix, b)),
								|_, _, _, _| {},
							)
							.absolute()
							.top_0()
							.left_0()
							.size_full(),
						)
						.children(crate::ui::probe(&self.probes, probe_id))
					}),
			)
			.into_any_element()
	}

	fn side_row(&self, p: &Preview, ix: usize) -> AnyElement {
		let theme = SyntaxTheme::default();
		let Some(r) = p.diff.as_ref().and_then(|d| d.side.get(ix)).copied()
		else {
			return div().into_any_element();
		};
		if let Some(line) = r.full {
			let (text, _) = clip_line(p.line(line));
			return div()
				.id(("side-line", ix))
				.flex()
				.w_full()
				.h(px(LINE_H))
				.pl(px(8.))
				.whitespace_nowrap()
				.overflow_hidden()
				.when(r.kind == RowKind::Hunk, |d| {
					d.bg(rgb(pal().diff_hunk_bg))
				})
				.child(
					StyledText::new(SharedString::from(text.to_string()))
						.with_highlights(line_highlights(
							text,
							Language::Diff,
							&theme,
							&[],
							None,
						)),
				)
				.into_any_element();
		}
		let num_w = gutter_num_w(p);
		let lang = p.code_lang();
		let half = |cell: Option<(u32, usize)>, add: bool, side_id: String| {
			let (bg, body) = match cell {
				Some((n, line)) => {
					let raw = p.line(line);
					// Drop the +/-/space marker; the side already says it.
					let (text, _) = clip_line(raw.get(1..).unwrap_or(""));
					let changed = raw.starts_with(if add { '+' } else { '-' });
					(
						changed.then_some(if add {
							pal().diff_add_bg
						} else {
							pal().diff_del_bg
						}),
						div()
							.flex()
							.child(
								gutter_num(num_w, Some(n)).mr(px(GUTTER_GAP)),
							)
							.child(
								StyledText::new(SharedString::from(
									text.to_string(),
								))
								.with_highlights(line_highlights(
									text,
									lang,
									&theme,
									&[],
									None,
								)),
							),
					)
				}
				None => (Some(pal().diff_empty_bg), div()),
			};
			div()
				.relative()
				.flex_1()
				.min_w_0()
				.overflow_hidden()
				.whitespace_nowrap()
				.when_some(bg, |d, c| d.bg(rgb(c)))
				.child(body)
				.children(crate::ui::probe(&self.probes, side_id))
		};
		div()
			.id(("side-line", ix))
			.flex()
			.flex_row()
			.w_full()
			.h(px(LINE_H))
			.child(half(r.left, false, format!("side-left:{ix}")))
			.child(div().w(px(1.)).h_full().bg(rgb(pal().divider)))
			.child(half(r.right, true, format!("side-right:{ix}")))
			.into_any_element()
	}
}

/// Space between the line-number gutter and the code.
const GUTTER_GAP: f32 = 8.0;

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

/// Colours the leading +/- marker of a diff line on its own.
fn tint_marker(hl: &mut Vec<(Range<usize>, HighlightStyle)>, c: gpui::Rgba) {
	let Some(first) = hl.first_mut().filter(|(r, _)| r.start == 0) else {
		return;
	};
	if first.0.end > 1 {
		let mut rest = first.clone();
		rest.0.start = 1;
		first.0.end = 1;
		first.1.color = Some(c.into());
		hl.insert(1, rest);
	} else {
		first.1.color = Some(c.into());
	}
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
		assert_eq!(find_matches(&p, "HELLO"), vec![(0, 0, 5), (0, 6, 11)]);
		let m = find_matches(&p, "中文");
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
		assert_eq!(
			d.inline[4],
			InlineRow {
				kind: RowKind::Context,
				old: Some(3),
				new: Some(3)
			}
		);
		assert_eq!(
			d.inline[5],
			InlineRow {
				kind: RowKind::Removed,
				old: Some(4),
				new: None
			}
		);
		assert_eq!(
			d.inline[6],
			InlineRow {
				kind: RowKind::Added,
				old: None,
				new: Some(4)
			}
		);
		assert_eq!(
			d.inline[8],
			InlineRow {
				kind: RowKind::Context,
				old: Some(5),
				new: Some(6)
			}
		);
		// Side by side: old/new paired on one row, extra addition alone.
		let changed: Vec<_> =
			d.side.iter().filter(|r| r.full.is_none()).collect();
		assert_eq!(changed[1].left, Some((4, 5)));
		assert_eq!(changed[1].right, Some((4, 6)));
		assert_eq!(changed[2].left, None);
		assert_eq!(changed[2].right, Some((5, 7)));
	}

	#[test]
	fn patch_header_is_hidden_but_text_is_kept() {
		let diff = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n\\ No newline at end of file\n";
		let p = preview(diff, true);
		let d = p.diff.as_ref().unwrap();
		// Hunk, body and the "\ No newline" marker are drawn; headers not.
		assert_eq!(d.shown, vec![4, 5, 6, 7, 8]);
		assert_eq!(p.inline_rows(), 5);
		assert_eq!(p.inline_line(0), 4);
		assert_eq!(p.inline_row_of(0), 0, "hidden line maps to next row");
		assert_eq!(p.inline_row_of(6), 2);
		assert!(!p.is_shown(3) && p.is_shown(4));
		assert!(d.side.iter().all(|r| r.full.is_none_or(|i| i >= 4)));
		assert_eq!(&*p.text, diff, "retained text (copy source) unchanged");
		assert_eq!(p.widest, 8);
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
		let hl =
			line_highlights(render_text, Language::Rust, &theme, &finds, sel);

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
