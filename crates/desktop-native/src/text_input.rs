//! Single-line text input with native IME support (marked text, candidate
//! window placement) through GPUI's `EntityInputHandler`.
//!
//! Adapted from GPUI 0.2.2 `examples/input.rs` (Apache-2.0). Boundaries are
//! per `char` instead of per grapheme to avoid a new dependency.

use std::ops::Range;

use gpui::{
	div, fill, point, prelude::*, px, relative, rgb, size, App, Bounds,
	ClipboardItem, Context, CursorStyle, Element, ElementId,
	ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
	Focusable, GlobalElementId, LayoutId, MouseButton, MouseDownEvent,
	MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
	SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
};

use crate::theme::*;

macro_rules! ime_trace {
	($($arg:tt)*) => {
		if std::env::var_os("SNIP_IME_TRACE").is_some() {
			eprintln!("[IME_TRACE] {}", format!($($arg)*));
		}
	};
}

gpui::actions!(
	text_input,
	[
		Backspace,
		Delete,
		Left,
		Right,
		SelectLeft,
		SelectRight,
		SelectAll,
		Home,
		End,
		Paste,
		Cut,
		Copy,
		Submit,
		SubmitPrev,
		Dismiss,
		MoveUp,
		MoveDown,
	]
);

/// Key bindings scoped to a focused input ("TextInput" key context), so they
/// never shadow the workbench shortcuts elsewhere.
pub fn bindings() -> Vec<gpui::KeyBinding> {
	use gpui::KeyBinding;
	let c = Some("TextInput");
	vec![
		KeyBinding::new("backspace", Backspace, c),
		KeyBinding::new("delete", Delete, c),
		KeyBinding::new("left", Left, c),
		KeyBinding::new("right", Right, c),
		KeyBinding::new("shift-left", SelectLeft, c),
		KeyBinding::new("shift-right", SelectRight, c),
		KeyBinding::new("ctrl-a", SelectAll, c),
		KeyBinding::new("cmd-a", SelectAll, c),
		KeyBinding::new("home", Home, c),
		KeyBinding::new("end", End, c),
		KeyBinding::new("ctrl-v", Paste, c),
		KeyBinding::new("cmd-v", Paste, c),
		KeyBinding::new("ctrl-c", Copy, c),
		KeyBinding::new("cmd-c", Copy, c),
		KeyBinding::new("ctrl-x", Cut, c),
		KeyBinding::new("cmd-x", Cut, c),
		KeyBinding::new("enter", Submit, c),
		KeyBinding::new("shift-enter", SubmitPrev, c),
		KeyBinding::new("escape", Dismiss, c),
		KeyBinding::new("up", MoveUp, c),
		KeyBinding::new("down", MoveDown, c),
	]
}

/// What the owner of an input reacts to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
	Changed,
	Submit,
	SubmitPrev,
	Dismiss,
	Up,
	Down,
}

pub struct TextInput {
	focus_handle: Option<FocusHandle>,
	content: SharedString,
	placeholder: SharedString,
	selected_range: Range<usize>,
	selection_reversed: bool,
	marked_range: Option<Range<usize>>,
	last_layout: Option<ShapedLine>,
	last_bounds: Option<Bounds<Pixels>>,
	is_selecting: bool,
	/// Last caret rectangle published to the platform IME, in logical pixels.
	last_ime_anchor: Option<[i32; 4]>,
	/// Extra publishes after the caret moves. XIM may not be connected on the
	/// first frame, and `invalidate_character_coordinates` runs on a later frame.
	ime_anchor_retries: u8,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
	pub fn new(
		placeholder: impl Into<SharedString>,
		tab_index: isize,
		cx: &mut Context<Self>,
	) -> Self {
		Self {
			focus_handle: Some(
				cx.focus_handle().tab_index(tab_index).tab_stop(true),
			),
			content: SharedString::default(),
			placeholder: placeholder.into(),
			selected_range: 0..0,
			selection_reversed: false,
			marked_range: None,
			last_layout: None,
			last_bounds: None,
			is_selecting: false,
			last_ime_anchor: None,
			ime_anchor_retries: 0,
		}
	}

	#[cfg(test)]
	pub fn new_for_test(initial_text: &str) -> Self {
		let bounded: String =
			initial_text.chars().take(Self::MAX_TOTAL_CHARS).collect();
		let len = bounded.len();
		Self {
			focus_handle: None,
			content: bounded.into(),
			placeholder: SharedString::default(),
			selected_range: len..len,
			selection_reversed: false,
			marked_range: None,
			last_layout: None,
			last_bounds: None,
			is_selecting: false,
			last_ime_anchor: None,
			ime_anchor_retries: 0,
		}
	}

	pub fn text(&self) -> &str {
		&self.content
	}

	pub fn handle(&self) -> FocusHandle {
		self.focus_handle
			.clone()
			.expect("focus_handle must be initialized")
	}

	pub fn set_placeholder(&mut self, p: impl Into<SharedString>) {
		self.placeholder = p.into();
	}

	/// Drops the retained text without emitting [`InputEvent::Changed`].
	pub fn clear_retained(&mut self) {
		self.content = SharedString::default();
		self.selected_range = 0..0;
		self.selection_reversed = false;
		self.marked_range = None;
		self.last_layout = None;
		self.last_bounds = None;
		self.is_selecting = false;
	}

	pub const MAX_INPUT_CHARS: usize = 1024;
	pub const MAX_TOTAL_CHARS: usize = 4096;
	/// Frames to republish the caret after it moves. The XIM connection is
	/// created asynchronously, and GPUI applies the spot on a later frame.
	const IME_ANCHOR_REPUBLISH: u8 = 3;

	pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
		let bounded: String =
			text.chars().take(Self::MAX_TOTAL_CHARS).collect();
		let len = bounded.len();
		self.content = bounded.into();
		self.selected_range = len..len;
		self.marked_range = None;
		cx.emit(InputEvent::Changed);
		cx.notify();
	}

	fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
		if self.selected_range.is_empty() {
			self.move_to(self.previous_boundary(self.cursor_offset()), cx);
		} else {
			self.move_to(self.selected_range.start, cx)
		}
	}

	fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
		if self.selected_range.is_empty() {
			self.move_to(self.next_boundary(self.selected_range.end), cx);
		} else {
			self.move_to(self.selected_range.end, cx)
		}
	}

	fn select_left(
		&mut self,
		_: &SelectLeft,
		_: &mut Window,
		cx: &mut Context<Self>,
	) {
		self.select_to(self.previous_boundary(self.cursor_offset()), cx);
	}

	fn select_right(
		&mut self,
		_: &SelectRight,
		_: &mut Window,
		cx: &mut Context<Self>,
	) {
		self.select_to(self.next_boundary(self.cursor_offset()), cx);
	}

	fn select_all(
		&mut self,
		_: &SelectAll,
		_: &mut Window,
		cx: &mut Context<Self>,
	) {
		self.move_to(0, cx);
		self.select_to(self.content.len(), cx)
	}

	fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
		self.move_to(0, cx);
	}

	fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
		self.move_to(self.content.len(), cx);
	}

	fn backspace(
		&mut self,
		_: &Backspace,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if self.selected_range.is_empty() {
			self.select_to(self.previous_boundary(self.cursor_offset()), cx)
		}
		self.replace_text_in_range(None, "", window, cx)
	}

	fn delete(
		&mut self,
		_: &Delete,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if self.selected_range.is_empty() {
			self.select_to(self.next_boundary(self.cursor_offset()), cx)
		}
		self.replace_text_in_range(None, "", window, cx)
	}

	fn paste(
		&mut self,
		_: &Paste,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if let Some(text) =
			cx.read_from_clipboard().and_then(|item| item.text())
		{
			// Single line: newlines become spaces; streaming take without full allocation
			let text: String = text
				.chars()
				.take(Self::MAX_INPUT_CHARS)
				.map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
				.collect();
			self.replace_text_in_range(None, &text, window, cx);
		}
	}

	fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
		if !self.selected_range.is_empty() {
			cx.write_to_clipboard(ClipboardItem::new_string(
				self.content[self.selected_range.clone()].to_string(),
			));
		}
	}

	fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
		if !self.selected_range.is_empty() {
			cx.write_to_clipboard(ClipboardItem::new_string(
				self.content[self.selected_range.clone()].to_string(),
			));
			self.replace_text_in_range(None, "", window, cx)
		}
	}

	fn on_mouse_down(
		&mut self,
		event: &MouseDownEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if let Some(f) = &self.focus_handle {
			window.focus(f);
		}
		self.is_selecting = true;
		if event.modifiers.shift {
			self.select_to(self.index_for_mouse_position(event.position), cx);
		} else {
			self.move_to(self.index_for_mouse_position(event.position), cx)
		}
	}

	fn on_mouse_up(
		&mut self,
		_: &MouseUpEvent,
		_: &mut Window,
		_: &mut Context<Self>,
	) {
		self.is_selecting = false;
	}

	fn on_mouse_move(
		&mut self,
		event: &MouseMoveEvent,
		_: &mut Window,
		cx: &mut Context<Self>,
	) {
		if self.is_selecting {
			self.select_to(self.index_for_mouse_position(event.position), cx);
		}
	}

	fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
		self.selected_range = offset..offset;
		cx.notify()
	}

	fn cursor_offset(&self) -> usize {
		if self.selection_reversed {
			self.selected_range.start
		} else {
			self.selected_range.end
		}
	}

	fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
		if self.content.is_empty() {
			return 0;
		}
		let (Some(bounds), Some(line)) =
			(self.last_bounds.as_ref(), self.last_layout.as_ref())
		else {
			return 0;
		};
		if position.y < bounds.top() {
			return 0;
		}
		if position.y > bounds.bottom() {
			return self.content.len();
		}
		line.closest_index_for_x(position.x - bounds.left())
	}

	fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
		if self.selection_reversed {
			self.selected_range.start = offset
		} else {
			self.selected_range.end = offset
		};
		if self.selected_range.end < self.selected_range.start {
			self.selection_reversed = !self.selection_reversed;
			self.selected_range =
				self.selected_range.end..self.selected_range.start;
		}
		cx.notify()
	}

	fn offset_from_utf16(&self, offset: usize) -> usize {
		let mut utf8_offset = 0;
		let mut utf16_count = 0;
		for ch in self.content.chars() {
			if utf16_count >= offset {
				break;
			}
			utf16_count += ch.len_utf16();
			utf8_offset += ch.len_utf8();
		}
		utf8_offset
	}

	fn offset_to_utf16(&self, offset: usize) -> usize {
		let mut utf16_offset = 0;
		let mut utf8_count = 0;
		for ch in self.content.chars() {
			if utf8_count >= offset {
				break;
			}
			utf8_count += ch.len_utf8();
			utf16_offset += ch.len_utf16();
		}
		utf16_offset
	}

	fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
		self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
	}

	fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
		self.offset_from_utf16(range_utf16.start)
			..self.offset_from_utf16(range_utf16.end)
	}

	fn bound_admission(&self, range: &Range<usize>, new_text: &str) -> String {
		bound_text_admission(
			&self.content,
			range,
			new_text,
			Self::MAX_TOTAL_CHARS,
		)
	}

	fn previous_boundary(&self, offset: usize) -> usize {
		grapheme_previous_boundary(&self.content, offset)
	}

	fn next_boundary(&self, offset: usize) -> usize {
		grapheme_next_boundary(&self.content, offset)
	}

	fn emit(&mut self, ev: InputEvent, cx: &mut Context<Self>) {
		cx.emit(ev);
	}

	pub fn marked_range(&self) -> Option<Range<usize>> {
		self.marked_range.clone()
	}

	pub fn selected_range(&self) -> Range<usize> {
		self.selected_range.clone()
	}

	pub fn is_at_capacity(&self) -> bool {
		self.content.chars().count() >= Self::MAX_TOTAL_CHARS
	}

	pub fn char_count(&self) -> usize {
		self.content.chars().count()
	}

	pub fn handle_replace_text(
		&mut self,
		range_utf16: Option<Range<usize>>,
		new_text: &str,
	) {
		let range = range_utf16
			.as_ref()
			.map(|r| self.range_from_utf16(r))
			.or(self.marked_range.clone())
			.unwrap_or(self.selected_range.clone());
		let start = range.start.min(self.content.len());
		let end = range.end.min(self.content.len());
		let safe_range = start..end;
		let to_insert = self.bound_admission(&safe_range, new_text);
		let inserted_len = to_insert.len();
		self.content = (self.content[0..start].to_owned()
			+ &to_insert
			+ &self.content[end..])
			.into();
		self.selected_range = start + inserted_len..start + inserted_len;
		self.marked_range.take();
	}

	pub fn handle_replace_and_mark(
		&mut self,
		range_utf16: Option<Range<usize>>,
		new_text: &str,
		new_selected_range_utf16: Option<Range<usize>>,
	) {
		let range = range_utf16
			.as_ref()
			.map(|r| self.range_from_utf16(r))
			.or(self.marked_range.clone())
			.unwrap_or(self.selected_range.clone());
		let start = range.start.min(self.content.len());
		let end = range.end.min(self.content.len());
		let safe_range = start..end;
		let to_insert = self.bound_admission(&safe_range, new_text);
		let inserted_len = to_insert.len();
		self.content = (self.content[0..start].to_owned()
			+ &to_insert
			+ &self.content[end..])
			.into();
		self.marked_range = if to_insert.is_empty() {
			None
		} else {
			Some(start..start + inserted_len)
		};
		self.selected_range = new_selected_range_utf16
			.as_ref()
			.map(|r| utf16_range_in_str(&to_insert, r))
			.map(|r| r.start + start..r.end + start)
			.unwrap_or_else(|| start + inserted_len..start + inserted_len);
		ime_trace!(
			"mark bytes={} marked={:?} selected={:?}",
			to_insert.len(),
			self.marked_range,
			self.selected_range
		);
	}

	pub fn handle_unmark(&mut self) -> bool {
		self.marked_range.take().is_some()
	}

	pub fn compute_character_index_for_point(
		bounds: Option<Bounds<Pixels>>,
		layout_index_for_x: impl Fn(Pixels) -> Option<usize>,
		content: &str,
		point: Point<Pixels>,
	) -> Option<usize> {
		let line_point = bounds?.localize(&point)?;
		let utf8_index = layout_index_for_x(line_point.x)?;
		let clamped = utf8_index.min(content.len());
		Some(content[..clamped].chars().map(|c| c.len_utf16()).sum())
	}
}

impl EntityInputHandler for TextInput {
	fn text_for_range(
		&mut self,
		range_utf16: Range<usize>,
		actual_range: &mut Option<Range<usize>>,
		_: &mut Window,
		_: &mut Context<Self>,
	) -> Option<String> {
		let range = self.range_from_utf16(&range_utf16);
		actual_range.replace(self.range_to_utf16(&range));
		Some(self.content[range].to_string())
	}

	fn selected_text_range(
		&mut self,
		_: bool,
		_: &mut Window,
		_: &mut Context<Self>,
	) -> Option<UTF16Selection> {
		Some(UTF16Selection {
			range: self.range_to_utf16(&self.selected_range),
			reversed: self.selection_reversed,
		})
	}

	fn marked_text_range(
		&self,
		_: &mut Window,
		_: &mut Context<Self>,
	) -> Option<Range<usize>> {
		self.marked_range
			.as_ref()
			.map(|range| self.range_to_utf16(range))
	}

	fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
		if self.handle_unmark() {
			cx.emit(InputEvent::Changed);
			cx.notify();
		}
	}

	fn replace_text_in_range(
		&mut self,
		range_utf16: Option<Range<usize>>,
		new_text: &str,
		_: &mut Window,
		cx: &mut Context<Self>,
	) {
		self.handle_replace_text(range_utf16, new_text);
		cx.emit(InputEvent::Changed);
		cx.notify();
	}

	fn replace_and_mark_text_in_range(
		&mut self,
		range_utf16: Option<Range<usize>>,
		new_text: &str,
		new_selected_range_utf16: Option<Range<usize>>,
		_: &mut Window,
		cx: &mut Context<Self>,
	) {
		self.handle_replace_and_mark(
			range_utf16,
			new_text,
			new_selected_range_utf16,
		);
		cx.notify();
	}

	fn bounds_for_range(
		&mut self,
		range_utf16: Range<usize>,
		bounds: Bounds<Pixels>,
		_: &mut Window,
		_: &mut Context<Self>,
	) -> Option<Bounds<Pixels>> {
		let last_layout = self.last_layout.as_ref()?;
		let range = self.range_from_utf16(&range_utf16);
		Some(range_bounds_in_element(
			bounds,
			last_layout.x_for_index(range.start),
			last_layout.x_for_index(range.end),
		))
	}

	fn character_index_for_point(
		&mut self,
		point: Point<Pixels>,
		_: &mut Window,
		_: &mut Context<Self>,
	) -> Option<usize> {
		let last_layout = self.last_layout.as_ref()?;
		Self::compute_character_index_for_point(
			self.last_bounds,
			|x| last_layout.index_for_x(x),
			&self.content,
			point,
		)
	}
}

/// Converts a UTF-16 code-unit range within `s` to a clamped UTF-8 byte range within `s`.
fn utf16_range_in_str(s: &str, r: &Range<usize>) -> Range<usize> {
	let mut utf8_start = 0;
	let mut utf8_end = 0;
	let mut utf16_count = 0;
	for ch in s.chars() {
		if utf16_count < r.start {
			utf8_start += ch.len_utf8();
		}
		if utf16_count < r.end {
			utf8_end += ch.len_utf8();
		}
		utf16_count += ch.len_utf16();
	}
	let start = utf8_start.min(s.len());
	let end = utf8_end.max(start).min(s.len());
	start..end
}

pub fn grapheme_previous_boundary(s: &str, offset: usize) -> usize {
	use unicode_segmentation::UnicodeSegmentation;
	let clamped = offset.min(s.len());
	s[..clamped]
		.grapheme_indices(true)
		.next_back()
		.map(|(i, _)| i)
		.unwrap_or(0)
}

pub fn grapheme_next_boundary(s: &str, offset: usize) -> usize {
	use unicode_segmentation::UnicodeSegmentation;
	let clamped = offset.min(s.len());
	s[clamped..]
		.grapheme_indices(true)
		.next()
		.map(|(i, g)| clamped + i + g.len())
		.unwrap_or(s.len())
}

/// Rectangle of a text range inside a single-line element. A caret passes
/// the same x twice; GPUI sends that rectangle's bottom-right corner as
/// the XIM spot.
pub fn range_bounds_in_element(
	element: Bounds<Pixels>,
	x_start: Pixels,
	x_end: Pixels,
) -> Bounds<Pixels> {
	Bounds::from_corners(
		point(element.left() + x_start, element.top()),
		point(element.left() + x_end, element.bottom()),
	)
}

/// Stable logical-pixel key for the caret rectangle. Used only to avoid
/// republishing an unchanged spot every frame.
pub fn ime_anchor_key(bounds: Bounds<Pixels>) -> [i32; 4] {
	[
		f32::from(bounds.origin.x).round() as i32,
		f32::from(bounds.origin.y).round() as i32,
		f32::from(bounds.size.width).round() as i32,
		f32::from(bounds.size.height).round() as i32,
	]
}

pub fn bound_text_admission(
	current_content: &str,
	range: &Range<usize>,
	new_text: &str,
	max_total: usize,
) -> String {
	let current_len = current_content.chars().count();
	let start = range.start.min(current_content.len());
	let end = range.end.min(current_content.len());
	let removed_len = current_content[start..end].chars().count();
	let available =
		max_total.saturating_sub(current_len.saturating_sub(removed_len));
	new_text.chars().take(available).collect()
}

struct TextElement {
	input: Entity<TextInput>,
}

struct PrepaintState {
	line: Option<ShapedLine>,
	cursor: Option<PaintQuad>,
	selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
	type Element = Self;
	fn into_element(self) -> Self::Element {
		self
	}
}

impl Element for TextElement {
	type RequestLayoutState = ();
	type PrepaintState = PrepaintState;

	fn id(&self) -> Option<ElementId> {
		None
	}

	fn source_location(
		&self,
	) -> Option<&'static core::panic::Location<'static>> {
		None
	}

	fn request_layout(
		&mut self,
		_: Option<&GlobalElementId>,
		_: Option<&gpui::InspectorElementId>,
		window: &mut Window,
		cx: &mut App,
	) -> (LayoutId, Self::RequestLayoutState) {
		let mut style = Style::default();
		style.size.width = relative(1.).into();
		style.size.height = window.line_height().into();
		(window.request_layout(style, [], cx), ())
	}

	fn prepaint(
		&mut self,
		_: Option<&GlobalElementId>,
		_: Option<&gpui::InspectorElementId>,
		bounds: Bounds<Pixels>,
		_: &mut Self::RequestLayoutState,
		window: &mut Window,
		cx: &mut App,
	) -> Self::PrepaintState {
		let input = self.input.read(cx);
		let content = input.content.clone();
		let selected_range = input.selected_range.clone();
		let cursor = input.cursor_offset();
		let style = window.text_style();
		let (display_text, text_color) = if content.is_empty() {
			(input.placeholder.clone(), rgb(TEXT_DISABLED).into())
		} else {
			(content, style.color)
		};
		let run = TextRun {
			len: display_text.len(),
			font: style.font(),
			color: text_color,
			background_color: None,
			underline: None,
			strikethrough: None,
		};
		let runs = if let Some(marked) = input.marked_range.as_ref() {
			// IME pre-edit text is underlined like the platform does.
			vec![
				TextRun {
					len: marked.start,
					..run.clone()
				},
				TextRun {
					len: marked.end - marked.start,
					underline: Some(UnderlineStyle {
						color: Some(run.color),
						thickness: px(1.0),
						wavy: false,
					}),
					..run.clone()
				},
				TextRun {
					len: display_text.len() - marked.end,
					..run
				},
			]
			.into_iter()
			.filter(|r| r.len > 0)
			.collect()
		} else {
			vec![run]
		};
		let font_size = style.font_size.to_pixels(window.rem_size());
		let line = window.text_system().shape_line(
			display_text,
			font_size,
			&runs,
			None,
		);
		let cursor_pos = line.x_for_index(cursor);
		let (selection, cursor) = if selected_range.is_empty() {
			(
				None,
				Some(fill(
					Bounds::new(
						point(bounds.left() + cursor_pos, bounds.top()),
						size(px(1.5), bounds.bottom() - bounds.top()),
					),
					rgb(TEXT),
				)),
			)
		} else {
			(
				Some(fill(
					Bounds::from_corners(
						point(
							bounds.left()
								+ line.x_for_index(selected_range.start),
							bounds.top(),
						),
						point(
							bounds.left()
								+ line.x_for_index(selected_range.end),
							bounds.bottom(),
						),
					),
					rgb(SELECTION_BG),
				)),
				None,
			)
		};
		PrepaintState {
			line: Some(line),
			cursor,
			selection,
		}
	}

	fn paint(
		&mut self,
		_: Option<&GlobalElementId>,
		_: Option<&gpui::InspectorElementId>,
		bounds: Bounds<Pixels>,
		_: &mut Self::RequestLayoutState,
		prepaint: &mut Self::PrepaintState,
		window: &mut Window,
		cx: &mut App,
	) {
		let focus_handle = self.input.read(cx).focus_handle.clone();
		if let Some(ref fh) = focus_handle {
			window.handle_input(
				fh,
				ElementInputHandler::new(bounds, self.input.clone()),
				cx,
			);
		}
		if let Some(selection) = prepaint.selection.take() {
			window.paint_quad(selection)
		}
		let Some(line) = prepaint.line.take() else {
			return;
		};
		let _ = line.paint(bounds.origin, window.line_height(), window, cx);
		let focused = focus_handle
			.as_ref()
			.is_some_and(|fh| fh.is_focused(window));
		if focused {
			if let Some(cursor) = prepaint.cursor.take() {
				window.paint_quad(cursor);
			}
		}
		// GPUI forwards XNSpotLocation only after invalidate_character_coordinates.
		// Fcitx places the candidate on the client-window bottom when that spot
		// was never set. Republish for a few frames so a late XIM connection
		// still receives the caret.
		let anchor = {
			let input = self.input.read(cx);
			let x = line.x_for_index(input.cursor_offset());
			ime_anchor_key(range_bounds_in_element(bounds, x, x))
		};
		let mut publish_ime = false;
		self.input.update(cx, |input, _| {
			input.last_layout = Some(line);
			input.last_bounds = Some(bounds);
			if focused {
				if input.last_ime_anchor != Some(anchor) {
					input.last_ime_anchor = Some(anchor);
					input.ime_anchor_retries = TextInput::IME_ANCHOR_REPUBLISH;
				}
				if input.ime_anchor_retries > 0 {
					input.ime_anchor_retries -= 1;
					publish_ime = true;
				}
			} else {
				input.last_ime_anchor = None;
				input.ime_anchor_retries = 0;
			}
		});
		if publish_ime {
			ime_trace!(
				"anchor x={} y={} w={} h={}",
				anchor[0],
				anchor[1],
				anchor[2],
				anchor[3]
			);
			window.invalidate_character_coordinates();
			window.request_animation_frame();
		}
	}
}

impl Render for TextInput {
	fn render(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) -> impl IntoElement {
		let focused = self
			.focus_handle
			.as_ref()
			.is_some_and(|f| f.is_focused(window));
		let at_capacity = self.is_at_capacity();
		let border_color = if at_capacity {
			rgb(WARNING)
		} else if focused {
			rgb(ACCENT)
		} else {
			rgb(BUTTON_BORDER)
		};
		div()
			.id("text-input-field")
			.flex()
			.items_center()
			.key_context("TextInput")
			.when_some(self.focus_handle.clone(), |d, f| d.track_focus(&f))
			.cursor(CursorStyle::IBeam)
			.on_action(cx.listener(Self::backspace))
			.on_action(cx.listener(Self::delete))
			.on_action(cx.listener(Self::left))
			.on_action(cx.listener(Self::right))
			.on_action(cx.listener(Self::select_left))
			.on_action(cx.listener(Self::select_right))
			.on_action(cx.listener(Self::select_all))
			.on_action(cx.listener(Self::home))
			.on_action(cx.listener(Self::end))
			.on_action(cx.listener(Self::paste))
			.on_action(cx.listener(Self::cut))
			.on_action(cx.listener(Self::copy))
			.on_action(cx.listener(|this, _: &Submit, _, cx| {
				this.emit(InputEvent::Submit, cx)
			}))
			.on_action(cx.listener(|this, _: &SubmitPrev, _, cx| {
				this.emit(InputEvent::SubmitPrev, cx)
			}))
			.on_action(cx.listener(|this, _: &Dismiss, _, cx| {
				this.emit(InputEvent::Dismiss, cx)
			}))
			.on_action(cx.listener(|this, _: &MoveUp, _, cx| {
				this.emit(InputEvent::Up, cx)
			}))
			.on_action(cx.listener(|this, _: &MoveDown, _, cx| {
				this.emit(InputEvent::Down, cx)
			}))
			.on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
			.on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
			.on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
			.on_mouse_move(cx.listener(Self::on_mouse_move))
			.h(px(22.))
			.w_full()
			.px(px(6.))
			.rounded(px(3.))
			.border_1()
			.border_color(border_color)
			.bg(rgb(EDITOR_BG))
			.text_size(px(SMALL_TEXT))
			.line_height(px(16.))
			.overflow_hidden()
			.child(
				div()
					.flex_1()
					.min_w_0()
					.child(TextElement { input: cx.entity() }),
			)
			.when(at_capacity, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.text_size(px(10.))
						.text_color(rgb(WARNING))
						.child(format!(
							"{}/{}",
							self.char_count(),
							Self::MAX_TOTAL_CHARS
						)),
				)
			})
	}
}

impl Focusable for TextInput {
	fn focus_handle(&self, _: &App) -> FocusHandle {
		self.handle()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clear_retained_drops_text_without_touching_placeholder() {
		let mut input = TextInput::new_for_test("workspace/repo");
		input.placeholder = "路徑".into();
		input.marked_range = Some(0..4);
		input.clear_retained();
		assert_eq!(input.text(), "");
		assert_eq!(input.selected_range, 0..0);
		assert!(input.marked_range.is_none());
		assert!(input.last_layout.is_none());
		assert_eq!(input.placeholder.as_ref(), "路徑");
	}

	#[test]
	fn test_utf16_range_in_str_ascii_and_cjk_and_surrogate() {
		assert_eq!(utf16_range_in_str("Hello", &(1..4)), 1..4);

		// "測試" is 2 CJK characters (6 UTF-8 bytes, 2 UTF-16 code units)
		assert_eq!(utf16_range_in_str("測試", &(0..1)), 0..3);
		assert_eq!(utf16_range_in_str("測試", &(1..2)), 3..6);

		// '𝄞' (musical G-clef, U+1D11E) is 4 UTF-8 bytes and 2 UTF-16 code units (surrogate pair)
		let s = "測試𝄞結束";
		// '測' = 0..3 (utf16: 0..1)
		// '試' = 3..6 (utf16: 1..2)
		// '𝄞'  = 6..10 (utf16: 2..4)
		// '結' = 10..13 (utf16: 4..5)
		// '束' = 13..16 (utf16: 5..6)
		assert_eq!(utf16_range_in_str(s, &(2..4)), 6..10);
		// Clamped when exceeding length
		assert_eq!(utf16_range_in_str(s, &(5..10)), 13..16);
	}

	#[test]
	fn test_grapheme_boundaries() {
		// Combining accent: 'e' + '\u{0301}' (é represented as two code points)
		let text = "cafe\u{0301} test";
		// 'c'=0..1, 'a'=1..2, 'f'=2..3. "e\u{0301}" starts at 3 and ends at 6 (3 bytes).
		assert_eq!(grapheme_next_boundary(text, 3), 6);
		assert_eq!(grapheme_previous_boundary(text, 6), 3);

		// Emoji with multiple code points / zero-width joiner: "👨‍👩‍👧"
		let family = "hi 👨‍👩‍👧 bye";
		let emoji_start = 3;
		let emoji_end = grapheme_next_boundary(family, emoji_start);
		assert!(emoji_end > emoji_start);
		assert_eq!(grapheme_previous_boundary(family, emoji_end), emoji_start);
	}

	#[test]
	fn test_surrogate_pair_composition_selection_and_replacement() {
		let prefix = "繁體中文前綴";
		let new_marked = "輸入𝄞符號";
		// '𝄞' is at utf16 code units 2..4 within new_marked
		let rel_sel_utf16 = 2..4;
		let rel_sel_utf8 = utf16_range_in_str(new_marked, &rel_sel_utf16);
		assert_eq!(&new_marked[rel_sel_utf8.clone()], "𝄞");

		let composed = format!("{prefix}{new_marked}");
		let abs_start = prefix.len() + rel_sel_utf8.start;
		let abs_end = prefix.len() + rel_sel_utf8.end;
		assert_eq!(&composed[abs_start..abs_end], "𝄞");
	}

	#[test]
	fn test_bound_admission() {
		let current = "abcdef";
		let admitted = bound_text_admission(current, &(2..4), "xyz", 10);
		assert_eq!(admitted, "xyz");

		// Total length cap: current len is 6, removing 2 leaves 4. max_total is 6, so only 2 chars admitted.
		let restricted = bound_text_admission(current, &(2..4), "hello", 6);
		assert_eq!(restricted, "he");
	}

	#[test]
	fn test_ime_composition_handler_regression_with_surrogates_and_unmark() {
		// 1. Initial state with non-ASCII prefix
		let mut input = TextInput::new_for_test("搜尋前綴:");
		assert_eq!(input.text(), "搜尋前綴:");
		assert_eq!(input.marked_range(), None);

		// 2. IME begins composition with surrogate pair: "測試𝄞符號"
		// '𝄞' (musical G-clef) is utf-16 code units 2..4 within "測試𝄞符號"
		input.handle_replace_and_mark(None, "測試𝄞符號", Some(2..4));
		assert_eq!(input.text(), "搜尋前綴:測試𝄞符號");
		assert_eq!(input.marked_range(), Some(13..29)); // 13 is byte len of "搜尋前綴:"
		let sel = input.selected_range();
		assert_eq!(&input.text()[sel], "𝄞");

		// 3. IME updates candidate text: replaces the marked range with "候選字𠮷"
		input.handle_replace_and_mark(None, "候選字𠮷", Some(0..3));
		assert_eq!(input.text(), "搜尋前綴:候選字𠮷");
		assert_eq!(input.marked_range(), Some(13..26)); // "候選字𠮷" is 13 bytes
		let sel2 = input.selected_range();
		assert_eq!(&input.text()[sel2], "候選字");

		// 4. IME completes composition -> unmark_text called
		let had_mark = input.handle_unmark();
		assert!(had_mark, "unmark must return true when mark was present");
		assert_eq!(input.marked_range(), None);
		assert_eq!(input.text(), "搜尋前綴:候選字𠮷");

		// 5. Test bounded truncation with surrogate pairs near capacity
		// Fill near capacity: MAX_TOTAL_CHARS - 2 characters
		let fill_chars = "國".repeat(TextInput::MAX_TOTAL_CHARS - 2);
		let mut bounded_input = TextInput::new_for_test(&fill_chars);
		assert_eq!(bounded_input.char_count(), TextInput::MAX_TOTAL_CHARS - 2);
		assert!(!bounded_input.is_at_capacity());

		// Insert 4 characters including a surrogate pair '𝄞' -> only 2 characters admitted
		bounded_input.handle_replace_and_mark(None, "文𝄞字", None);
		assert!(bounded_input.is_at_capacity());
		assert_eq!(bounded_input.char_count(), TextInput::MAX_TOTAL_CHARS);
		// Verified user-visible capacity state
		assert!(bounded_input.text().ends_with("文𝄞"));
	}

	#[test]
	fn test_character_point_origin_and_caret_translation() {
		// Bounds positioned at left=100.0, top=50.0, width=200.0, height=30.0
		let bounds = Some(Bounds::new(
			point(px(100.0), px(50.0)),
			size(px(200.0), px(30.0)),
		));
		let content = "Hello 繁體 𝄞 World";

		// Click inside bounds at absolute point (140.0, 60.0).
		// Relative line_point.x must be 140.0 - 100.0 = 40.0.
		// If origin offset is ignored, x would be 140.0 (pointing to wrong char).
		let click_pt = point(px(140.0), px(60.0));
		let index = TextInput::compute_character_index_for_point(
			bounds,
			|x| {
				assert_eq!(
					x,
					px(40.0),
					"x must be translated relative to bounds origin"
				);
				Some(6) // UTF-8 byte offset for "繁"
			},
			content,
			click_pt,
		);
		// Char 6 in "Hello 繁體 𝄞 World" has 6 UTF-16 code units (ASCII "Hello ")
		assert_eq!(index, Some(6));

		// Click outside bounds: returns None
		let outside_pt = point(px(50.0), px(10.0));
		let outside_index = TextInput::compute_character_index_for_point(
			bounds,
			|_| Some(0),
			content,
			outside_pt,
		);
		assert_eq!(outside_index, None);
	}

	#[test]
	fn test_caret_anchor_is_the_field_baseline() {
		// Field sits inside the window. The spot GPUI sends is this
		// rectangle's bottom-right corner, which is the caret baseline.
		let field =
			Bounds::new(point(px(120.0), px(470.0)), size(px(180.0), px(16.0)));
		let caret = range_bounds_in_element(field, px(0.0), px(0.0));
		assert_eq!(f32::from(caret.origin.x), 120.0);
		assert_eq!(f32::from(caret.origin.y), 470.0);
		assert_eq!(f32::from(caret.bottom()), 486.0);
		let spot_y = f32::from(caret.origin.y + caret.size.height);
		assert_eq!(spot_y, 486.0);

		let moved = range_bounds_in_element(
			Bounds::new(point(px(120.0), px(350.0)), size(px(180.0), px(16.0))),
			px(12.0),
			px(12.0),
		);
		assert_ne!(ime_anchor_key(caret), ime_anchor_key(moved));
		assert_eq!(ime_anchor_key(moved)[0], 132);
		assert_eq!(ime_anchor_key(moved)[1], 350);
	}
}
