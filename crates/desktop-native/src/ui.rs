//! IntelliJ-style workbench layout: header (repo / ref selectors), tool
//! window rail, Project / Changes tool window, editor (reader, commit files,
//! or paste preview), bottom Git Log and status bar.
//!
//! State and behaviour live on `WorkbenchModel`; this module renders it and
//! wires real controls to those methods. Long lists use `uniform_list`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
	anchored, canvas, deferred, div, prelude::*, px, rgb, transparent_black,
	uniform_list, AnyElement, AnyView, App, Context, Div, ElementId,
	FontWeight, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
	ScrollWheelEvent, SharedString, Stateful, Window,
};
use snip_core::format::ChangeType;
use snip_core::transfer::SourceKind;
use snip_core::workspace::ScanStatus;

use crate::graph_view;
use crate::history::RevRow;
use crate::i18n::{t, tf, Locale};
use crate::icons::{file_icon, icon, icon_tinted, Icon};
use crate::paste::{split_dir, PasteItem, PasteNode, PastePreviewPlan};
use crate::reader::{DiffMode, PreviewSource};
use crate::selector::Pick;
use crate::theme::*;
use crate::tree::{command_for_row, FlattenedTreeRow, RowGesture};
use crate::{
	ApplyPaste, CancelPaste, CloseWorkspace, CopySelection, DeselectAllFiles,
	FindInFile, FindNext, FindPrev, FocusNext, FocusPrev, GotoLine,
	HistoryNextPage, HistoryPrevPage, LogDown, LogExtendDown, LogExtendUp,
	LogHead, LogOpen, LogSearchFocus, LogUp, NavDown, NavToggle, NavUp,
	OpenRefSelector, OpenRepoSelector, OpenWorkspace, PastePreview, Popover,
	Quit, ReaderClear, ReaderCopy, ReaderDown, ReaderPageDown, ReaderPageUp,
	ReaderSelectAll, ReaderUp, Refresh, RepoEntryKind, SelectAllFiles,
	SelectRepo1, SelectRepo2, ShowChanges, ShowProject, Splitter, ToggleLocale,
	ToggleLog, ToggleTab, TreeCollapse, TreeDown, TreeExpand, TreeOpen,
	TreeToggle, TreeUp, WorkbenchModel, WorkbenchTab,
};
use crate::{NextDiff, PrevDiff};

// ───────────────────────── E2E probes (opt-in) ─────────────────────────

/// Test-only delay before a confirmed paste write, so the E2E can prove the
/// confirmation controls are locked while applying. Ignored outside E2E mode.
pub fn e2e_apply_delay() -> Option<std::time::Duration> {
	if !crate::e2e_on() {
		return None;
	}
	std::env::var("SNIP_NATIVE_E2E_APPLY_DELAY_MS")
		.ok()?
		.parse()
		.ok()
		.map(std::time::Duration::from_millis)
}

/// Test-only hold after a preview read so close can race a finished result.
/// Ignored outside E2E mode. Not a product timer.
pub fn e2e_read_delay() -> Option<std::time::Duration> {
	if !crate::e2e_on() {
		return None;
	}
	std::env::var("SNIP_NATIVE_E2E_READ_DELAY_MS")
		.ok()?
		.parse()
		.ok()
		.map(std::time::Duration::from_millis)
}

/// Test-only hold for a project-tree directory read: the background worker
/// keeps its result while this file exists, so close and quit can be raced
/// against a read that has no Git child. Ignored outside E2E mode.
pub fn e2e_tree_hold() -> Option<std::path::PathBuf> {
	if !crate::e2e_on() {
		return None;
	}
	std::env::var_os("SNIP_NATIVE_E2E_TREE_HOLD_FILE")
		.filter(|v| !v.is_empty())
		.map(std::path::PathBuf::from)
}

/// E2E only: holds copy export between plan_export_with and revalidate_with.
pub fn e2e_export_hold() -> Option<std::path::PathBuf> {
	if !crate::e2e_on() {
		return None;
	}
	std::env::var_os("SNIP_NATIVE_E2E_EXPORT_HOLD_FILE")
		.filter(|v| !v.is_empty())
		.map(std::path::PathBuf::from)
}

/// Control bounds of the last two frames only, so the bookkeeping is
/// bounded by what is on screen, never by what was ever shown.
#[derive(Default)]
pub struct ProbeFrame {
	shown: HashMap<String, [i32; 4]>,
	seen: HashMap<String, [i32; 4]>,
}

impl ProbeFrame {
	/// Records a control drawn this frame; true when new or moved.
	pub fn report(&mut self, id: &str, v: [i32; 4]) -> bool {
		let changed = self.shown.get(id) != Some(&v);
		self.seen.insert(id.to_string(), v);
		changed
	}

	/// Closes the frame; returns controls drawn last frame but not this one.
	pub fn end_frame(&mut self) -> Vec<String> {
		let mut gone: Vec<String> = self
			.shown
			.keys()
			.filter(|k| !self.seen.contains_key(*k))
			.cloned()
			.collect();
		gone.sort();
		self.shown = std::mem::take(&mut self.seen);
		gone
	}

	#[cfg(test)]
	fn tracked(&self) -> usize {
		self.shown.len() + self.seen.len()
	}
}

#[derive(Clone)]
pub struct Probes(Rc<RefCell<ProbeFrame>>);

impl Probes {
	pub fn from_env() -> Option<Self> {
		crate::e2e_on().then(|| Self(Rc::default()))
	}
}

fn physical(b: gpui::Bounds<gpui::Pixels>, s: f32) -> [i32; 4] {
	[
		(f32::from(b.origin.x) * s).round() as i32,
		(f32::from(b.origin.y) * s).round() as i32,
		(f32::from(b.size.width) * s).round() as i32,
		(f32::from(b.size.height) * s).round() as i32,
	]
}

/// E2E only: reports the parent's real on-screen bounds (physical pixels)
/// when they change. The parent must be `.relative()`. Nothing otherwise.
pub fn probe(
	probes: &Option<Probes>,
	id: impl Into<String>,
) -> Option<AnyElement> {
	let frame = probes.as_ref()?.0.clone();
	let id = id.into();
	Some(
		canvas(
			move |b, window, _| {
				let v = physical(b, window.scale_factor());
				if frame.borrow_mut().report(&id, v) {
					app_log!(
						"[APP:CTRL_BOUNDS: id={} x={} y={} w={} h={}]",
						id,
						v[0],
						v[1],
						v[2],
						v[3]
					);
				}
			},
			|_, _, _, _| {},
		)
		.absolute()
		.top_0()
		.left_0()
		.size_full()
		.into_any_element(),
	)
}

/// E2E only: last child of the root, deferred above every popup and menu,
/// so its prepaint runs after every probe of the frame; announces controls
/// that are no longer drawn.
fn probe_frame_end(probes: &Option<Probes>) -> Option<AnyElement> {
	let frame = probes.as_ref()?.0.clone();
	Some(
		deferred(
			canvas(
				move |_, _, _| {
					for id in frame.borrow_mut().end_frame() {
						app_log!("[APP:CTRL_GONE: id={}]", id);
					}
				},
				|_, _, _, _| {},
			)
			.absolute()
			.size_0(),
		)
		.with_priority(usize::MAX)
		.into_any_element(),
	)
}

// ───────────────────────── small building blocks ─────────────────────────

/// Focus rings show only while the user navigates with the keyboard, like
/// IntelliJ; a mouse press hides them again.
static KEYBOARD_NAV: std::sync::atomic::AtomicBool =
	std::sync::atomic::AtomicBool::new(false);

pub(crate) fn keyboard_nav() -> bool {
	KEYBOARD_NAV.load(std::sync::atomic::Ordering::Relaxed)
}

/// Returns whether the mode changed (the caller re-renders).
pub(crate) fn set_keyboard_nav(on: bool) -> bool {
	KEYBOARD_NAV.swap(on, std::sync::atomic::Ordering::Relaxed) != on
}

/// Focus ring on a focusable control, drawn only in keyboard mode.
fn focus_ring<E: Styled + InteractiveElement>(el: E) -> E {
	if keyboard_nav() {
		el.focus(|s| s.border_color(rgb(pal().focus_ring)))
	} else {
		el
	}
}

struct Tip(SharedString);

impl Render for Tip {
	fn render(
		&mut self,
		_: &mut Window,
		_: &mut Context<Self>,
	) -> impl IntoElement {
		div()
			.px_2()
			.py_1()
			.max_w(px(420.))
			.rounded(px(4.))
			.font_family(UI_FONT)
			.bg(rgb(pal().tooltip_bg))
			.border_1()
			.border_color(rgb(pal().divider))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text))
			.child(self.0.clone())
	}
}

fn tip(
	text: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
	let text = text.into();
	move |_, cx| cx.new(|_| Tip(text.clone())).into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
	Primary,
	Default,
	Ghost,
}

/// A real button: keyboard-focusable (`tab`) with a visible focus ring;
/// Enter/Space on focus clicks it (GPUI keyboard click).
fn button(
	id: impl Into<ElementId>,
	label: impl Into<SharedString>,
	kind: Btn,
	enabled: bool,
	tab: isize,
) -> Stateful<Div> {
	let fg = match (kind, enabled) {
		(_, false) => pal().text_disabled,
		(Btn::Primary, true) => pal().accent_text,
		_ => pal().text,
	};
	let (bg, border) = match kind {
		Btn::Primary if enabled => (Some(pal().accent), Some(pal().accent)),
		Btn::Ghost => (None, None),
		_ => (
			Some(pal().button_bg),
			Some(if enabled {
				pal().button_border
			} else {
				pal().divider
			}),
		),
	};
	div()
		.id(id)
		.relative()
		.flex_shrink_0()
		.h(px(24.))
		.px(px(10.))
		.flex()
		.items_center()
		.gap(px(5.))
		.rounded(px(4.))
		.whitespace_nowrap()
		.text_size(px(UI_TEXT))
		.text_color(rgb(fg))
		.border_1()
		.map(|d| match border {
			Some(b) => d.border_color(rgb(b)),
			None => d.border_color(transparent_black()),
		})
		.when_some(bg, |d, b| d.bg(rgb(b)))
		.when(enabled, |d| d.cursor_pointer().tab_index(tab))
		.when(enabled && kind != Btn::Primary, |d| {
			d.hover(|s| s.bg(rgb(pal().hover_bg)))
		})
		.map(focus_ring)
		.child(label.into())
}

/// Flexible text slot that ends in "…" when it does not fit.
fn fill_text(text: impl Into<SharedString>) -> Div {
	div()
		.flex_1()
		.min_w_0()
		.overflow_hidden()
		.line_clamp(1)
		.text_ellipsis()
		.child(text.into())
}

fn clip_text(text: impl Into<SharedString>) -> Div {
	div()
		.min_w_0()
		.overflow_hidden()
		.line_clamp(1)
		.text_ellipsis()
		.child(text.into())
}

fn checkbox(checked: bool) -> Div {
	tri_checkbox(Some(checked))
}

/// IntelliJ checkbox with the "some children" state (`None`: a dash).
fn tri_checkbox(state: Option<bool>) -> Div {
	let on = state != Some(false);
	// IntelliJ-style: small, thin border, accent fill with a painted check.
	div()
		.flex_shrink_0()
		.size(px(12.))
		.rounded(px(2.))
		.border_1()
		.flex()
		.items_center()
		.justify_center()
		.border_color(rgb(if on { pal().accent } else { pal().check_border }))
		.when(state == Some(true), |d| {
			d.bg(rgb(pal().accent)).child(icon_tinted(
				Icon::Checked,
				10.,
				pal().accent_text,
			))
		})
		.when(state.is_none(), |d| {
			d.bg(rgb(pal().accent)).child(
				div()
					.w(px(6.))
					.h(px(2.))
					.rounded(px(1.))
					.bg(rgb(pal().accent_text)),
			)
		})
}

/// Compact square icon button for tool window / header toolbars. The label
/// goes into the tooltip; the probe id and click target stay on the element.
fn icon_button(
	id: impl Into<ElementId>,
	ic: Icon,
	tooltip: impl Into<SharedString>,
	enabled: bool,
	tab: isize,
) -> Stateful<Div> {
	div()
		.id(id)
		.relative()
		.flex_shrink_0()
		.size(px(22.))
		.flex()
		.items_center()
		.justify_center()
		.rounded(px(4.))
		.border_1()
		.border_color(transparent_black())
		.tooltip(tip(tooltip))
		.when(enabled, |d| {
			d.cursor_pointer()
				.tab_index(tab)
				.hover(|s| s.bg(rgb(pal().hover_bg)))
		})
		.map(focus_ring)
		.child(
			div()
				.flex()
				.when(!enabled, |d| d.opacity(0.4))
				.child(icon(ic, 14.)),
		)
}

fn toolbar_divider() -> Div {
	div()
		.flex_shrink_0()
		.w(px(1.))
		.h(px(16.))
		.mx(px(4.))
		.bg(rgb(pal().divider))
}

fn change_style(ct: Option<ChangeType>) -> (&'static str, u32) {
	match ct {
		Some(ChangeType::New) => ("A", pal().git_added),
		Some(ChangeType::Modified) => ("M", pal().git_modified),
		Some(ChangeType::Deleted) => ("D", pal().git_deleted),
		Some(ChangeType::Moved) => ("R", pal().git_modified),
		None => ("?", pal().git_untracked),
	}
}

/// Operation label, file-name colour and reason key for a paste row. The
/// colour follows IntelliJ's file status: created green, modified blue
/// (whether or not overwriting is allowed yet), deleted grey.
fn paste_op(item: &PasteItem) -> (&'static str, u32, &'static str) {
	if !item.selected {
		("op_excluded", pal().text_disabled, "reason_excluded")
	} else if item.is_delete {
		if item.dest_exists {
			("op_delete", pal().git_deleted, "reason_delete")
		} else {
			("op_skip", pal().text_disabled, "reason_delete_missing")
		}
	} else if !item.dest_exists {
		("op_create", pal().git_added, "reason_create")
	} else if item.overwrite_allowed {
		("op_overwrite", pal().git_modified, "reason_overwrite")
	} else {
		("op_skip", pal().git_modified, "reason_exists")
	}
}

fn short_date(iso: &str) -> String {
	iso.get(..16).unwrap_or(iso).replace('T', " ")
}

fn short(sha: &str) -> &str {
	sha.get(..7).unwrap_or(sha)
}

// ───────────────────────── log helpers (IJ-2a) ─────────────────────────

/// Dropdowns of the log: the filter chips and the More (⋮) button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogMenu {
	Branch,
	User,
	Date,
	Paths,
	More,
}

impl LogMenu {
	fn key(self) -> &'static str {
		match self {
			LogMenu::Branch => "branch",
			LogMenu::User => "user",
			LogMenu::Date => "date",
			LogMenu::Paths => "paths",
			LogMenu::More => "more",
		}
	}
}

/// What a log dropdown entry does when clicked.
type MenuAction =
	Box<dyn Fn(&mut WorkbenchModel, &mut Context<WorkbenchModel>)>;

/// Date chip presets: probe key, `git log --since` value, label key.
const DATE_PRESETS: [(&str, &str, &str); 4] = [
	("1d", "24 hours ago", "log_date_1d"),
	("7d", "7 days ago", "log_date_7d"),
	("30d", "30 days ago", "log_date_30d"),
	("1y", "1 year ago", "log_date_1y"),
];
/// Entries one filter dropdown lists; the branches pane search reaches more.
const MAX_LOG_MENU_ITEMS: usize = 200;
/// Rows per branches-pane group.
const MAX_BRANCH_ROWS: usize = 200;
const AUTHOR_W: f32 = 120.;
const DATE_W: f32 = 118.;

/// `refs/heads/main` → `main`, `refs/remotes/origin/x` → `origin/x`.
fn short_ref(name: &str) -> &str {
	["refs/heads/", "refs/remotes/", "refs/tags/"]
		.iter()
		.find_map(|p| name.strip_prefix(p))
		.unwrap_or(name)
}

/// Rough rendered width: CJK and other wide glyphs are a full em.
fn approx_text_w(s: &str, size: f32) -> f32 {
	s.chars()
		.map(|c| if c.is_ascii() { size * 0.55 } else { size })
		.sum()
}

/// [`log_date_in`] in the viewer's time zone, now.
fn log_date(iso: &str, loc: Locale) -> String {
	log_date_in(iso, loc, chrono::Utc::now(), &chrono::Local)
}

/// IntelliJ-style short date of an ISO-8601 commit date, shown in `tz`:
/// "Today 20:57", "Yesterday 20:57", else a locale short date.
fn log_date_in<Tz: chrono::TimeZone>(
	iso: &str,
	loc: Locale,
	now: chrono::DateTime<chrono::Utc>,
	tz: &Tz,
) -> String {
	use chrono::{Datelike, Timelike};
	let Ok(at) = chrono::DateTime::parse_from_rfc3339(iso) else {
		return short_date(iso);
	};
	let at = at.with_timezone(tz);
	let today = now.with_timezone(tz).date_naive();
	let time = format!("{:02}:{:02}", at.hour(), at.minute());
	let (y, mo, d) = (at.year(), at.month(), at.day());
	match (today - at.date_naive()).num_days() {
		0 => tf("log_today", loc, &[&time]),
		1 => tf("log_yesterday", loc, &[&time]),
		_ => match loc {
			Locale::ZhTw => format!("{y}/{mo}/{d} {time}"),
			Locale::En => format!("{mo}/{d}/{:02}, {time}", y % 100),
		},
	}
}

/// Square ghost icon button of the log toolbars (IntelliJ's 26px action
/// buttons); the label is its tooltip.
fn log_icon_button(
	id: &'static str,
	ic: Icon,
	tooltip: impl Into<SharedString>,
	enabled: bool,
	active: bool,
) -> Stateful<Div> {
	div()
		.id(id)
		.relative()
		.flex_shrink_0()
		.size(px(26.))
		.flex()
		.items_center()
		.justify_center()
		.rounded(px(4.))
		.border_1()
		.border_color(transparent_black())
		.tooltip(tip(tooltip))
		.when(active, |d| d.bg(rgb(pal().hover_bg)))
		.when(enabled, |d| {
			d.cursor_pointer().hover(|s| s.bg(rgb(pal().hover_bg)))
		})
		.focus(|s| s.border_color(rgb(pal().focus_ring)))
		.child(
			div()
				.flex()
				.when(!enabled, |d| d.opacity(0.4))
				.child(icon(ic, 16.)),
		)
}

/// The coloured label glyph of a ref.
fn label_icon(l: &graph_view::RefLabel) -> gpui::Svg {
	icon_tinted(if l.current { Icon::Head } else { Icon::Tag }, 14., l.color)
}

/// A row's ref labels, right-aligned at the end of the subject: label icon
/// plus name, at most two, the rest folded into `+N`. Also returns their
/// estimated width.
fn ref_label_elements(
	refs: &[snip_core::graph::RefInfo],
	current_branch: Option<&str>,
	row: &str,
	show_tips: bool,
) -> (Vec<AnyElement>, f32) {
	let (shown, hidden) = graph_view::visible_refs(refs, current_branch);
	let mut width = 0.;
	let mut out: Vec<AnyElement> = shown
		.iter()
		.map(|b| {
			let l = graph_view::ref_label(b, current_branch);
			width += 18. + approx_text_w(&l.text, SMALL_TEXT).min(140.) + 8.;
			let tooltip = std::iter::once(b.primary)
				.chain(b.merged.iter().copied())
				.map(|i| graph_view::format_ref_badge(i).0)
				.collect::<Vec<_>>()
				.join("\n");
			div()
				.id(SharedString::from(format!("ref-badge:{row}:{}", l.text)))
				.flex()
				.items_center()
				.gap(px(3.))
				.max_w(px(160.))
				.when(show_tips, |d| d.tooltip(tip(tooltip)))
				.child(label_icon(&l))
				.child(
					clip_text(l.text.clone())
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().log_ref_text)),
				)
				.into_any_element()
		})
		.collect();
	if hidden > 0 {
		width += 30.;
		let all = refs
			.iter()
			.map(|i| graph_view::format_ref_badge(i).0)
			.collect::<Vec<_>>()
			.join("\n");
		out.push(
			div()
				.id(SharedString::from(format!("ref-more:{row}")))
				.flex_shrink_0()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().log_ref_text))
				.when(show_tips, |d| d.tooltip(tip(all)))
				.child(format!("+{hidden}"))
				.into_any_element(),
		);
	}
	(out, width)
}

/// One row of the Project tool window.
enum ProjRow {
	Repo(usize),
	Work(FlattenedTreeRow),
	Rev(RevRow),
}

/// One row of the Git Changes tool window (section header or file change).
#[derive(Clone, Debug)]
pub enum ChangeItemRow {
	Header {
		label: &'static str,
		count: usize,
		group_id: &'static str,
	},
	File {
		file_idx: usize,
	},
}

impl WorkbenchModel {
	pub fn selected_count(&self) -> usize {
		match self.active_tab {
			WorkbenchTab::GitChanges => {
				self.files.iter().filter(|f| f.selected).count()
			}
			WorkbenchTab::FileExplorer => {
				let mut paths = Vec::new();
				if let Some(ref tree) = self.file_tree {
					tree.collect_selected_paths(&mut paths);
				}
				paths.len()
			}
		}
	}

	/// Rail behaviour: clicking another tool window opens it, clicking the
	/// active one collapses the panel. Selections are never cleared here.
	pub fn activate_tool(&mut self, tab: WorkbenchTab, cx: &mut Context<Self>) {
		if self.active_tab == tab && self.left_visible {
			self.left_visible = false;
		} else {
			self.active_tab = tab;
			self.left_visible = true;
		}
		app_log!(
			"[APP:TAB_SWITCHED: {:?} visible={} selected={}]",
			self.active_tab,
			self.left_visible,
			self.selected_count()
		);
		cx.notify();
	}

	fn show_tool(
		&mut self,
		tab: WorkbenchTab,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		self.active_tab = tab;
		self.left_visible = true;
		window.focus(&self.tree_focus);
		app_log!(
			"[APP:TAB_SWITCHED: {:?} visible={} selected={}]",
			self.active_tab,
			self.left_visible,
			self.selected_count()
		);
		cx.notify();
	}

	fn toggle_log(&mut self, cx: &mut Context<Self>) {
		self.bottom_visible = !self.bottom_visible;
		// A manual toggle wins over the paste preview's auto-restore.
		self.log_before_paste = None;
		app_log!("[APP:LOG_PANEL: visible={}]", self.bottom_visible);
		cx.notify();
	}

	fn effective_left_w(&self, vw: f32) -> f32 {
		self.left_w.min((vw - RAIL_W) * 0.45)
	}

	fn effective_bottom_h(&self, vh: f32) -> f32 {
		self.bottom_h.min((vh - HEADER_H - STATUS_H) * 0.55)
	}

	fn drag_move(
		&mut self,
		ev: &MouseMoveEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let Some(which) = self.dragging else {
			return;
		};
		if ev.pressed_button != Some(MouseButton::Left) {
			self.end_drag(cx);
			return;
		}
		let vp = window.viewport_size();
		let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
		match which {
			Splitter::Left => {
				let want = f32::from(ev.position.x) - RAIL_W;
				self.left_w = want.min((vw - RAIL_W) * 0.45).max(LEFT_W_MIN);
			}
			Splitter::Bottom => {
				let want = vh - STATUS_H - f32::from(ev.position.y);
				self.bottom_h = want
					.min((vh - HEADER_H - STATUS_H) * 0.55)
					.max(BOTTOM_H_MIN);
			}
			Splitter::LogDetails => {
				let want = vw - SPLITTER - f32::from(ev.position.x);
				self.log_details_w = want.min(vw * 0.5).max(LOG_DETAILS_W_MIN);
			}
		}
		cx.notify();
	}

	fn end_drag(&mut self, cx: &mut Context<Self>) {
		if let Some(which) = self.dragging.take() {
			app_log!(
				"[APP:SPLIT_RESIZED: {:?} left_w={:.0} bottom_h={:.0}]",
				which,
				self.left_w,
				self.bottom_h
			);
			cx.notify();
		}
	}

	fn project_rows(&self) -> Vec<ProjRow> {
		if let Some(tree) = &self.rev_tree {
			return tree.rows().into_iter().map(ProjRow::Rev).collect();
		}
		let mut out = Vec::new();
		for idx in 0..self.repos.len() {
			out.push(ProjRow::Repo(idx));
			if self.selected_repo_idx == Some(idx) {
				if let Some(t) = &self.file_tree {
					out.extend(
						t.flatten_visible(t.visible_limit())
							.into_iter()
							.map(ProjRow::Work),
					);
				}
			}
		}
		out
	}

	fn change_item_rows(&self) -> Vec<ChangeItemRow> {
		let mut rows = Vec::new();
		for (group_id, label) in crate::menu::CHANGE_GROUPS {
			let members: Vec<usize> = self
				.files
				.iter()
				.enumerate()
				.filter(|(_, f)| crate::menu::change_group(f) == Some(group_id))
				.map(|(i, _)| i)
				.collect();
			if members.is_empty() {
				continue;
			}
			rows.push(ChangeItemRow::Header {
				label,
				count: members.len(),
				group_id,
			});
			if !self.chrome.collapsed_groups.contains(&group_id) {
				rows.extend(
					members
						.into_iter()
						.map(|file_idx| ChangeItemRow::File { file_idx }),
				);
			}
		}
		rows
	}

	/// Puts the keyboard row on the selected change (else the first change),
	/// never on a group header where Space/Enter do nothing.
	pub(crate) fn sync_list_row(&mut self) {
		let rows = self.change_item_rows();
		let file_row = |pred: &dyn Fn(&crate::FileChangeItem) -> bool| {
			rows.iter().position(|row| {
				matches!(row, ChangeItemRow::File { file_idx }
					if self.files.get(*file_idx).is_some_and(pred))
			})
		};
		let row = file_row(&|f| {
			self.selected_file.as_deref() == Some(f.path.as_str())
				&& self
					.selected_file_source
					.as_ref()
					.is_none_or(|s| s == &f.source)
		})
		.or_else(|| file_row(&|_| true))
		.unwrap_or(0);
		self.selected_list_row = row;
	}

	/// Moves the tool-window cursor by `delta` rows (arrows, PageUp/Down).
	/// In Changes a file row also opens that change, like before.
	fn tool_move(&mut self, delta: isize, cx: &mut Context<Self>) {
		let n = if self.active_tab == WorkbenchTab::GitChanges {
			self.change_item_rows().len()
		} else {
			self.project_rows().len()
		};
		if n == 0 {
			return;
		}
		let cur = if self.active_tab == WorkbenchTab::GitChanges {
			self.selected_list_row
		} else {
			self.tree_cursor
		}
		.min(n - 1);
		let next = (cur as isize + delta).clamp(0, n as isize - 1) as usize;
		self.set_tool_cursor(next, cx);
	}

	fn set_tool_cursor(&mut self, row: usize, cx: &mut Context<Self>) {
		self.chrome
			.left_scroll
			.scroll_to_item(row, gpui::ScrollStrategy::Top);
		if self.active_tab == WorkbenchTab::GitChanges {
			self.selected_list_row = row;
			if let Some(ChangeItemRow::File { file_idx }) =
				self.change_item_rows().get(row)
			{
				if let Some(f) = self.files.get(*file_idx) {
					let (p, s) = (f.path.clone(), f.source.clone());
					self.select_file_with_source(&p, s, cx);
				}
			}
		} else {
			self.tree_cursor = row;
		}
		cx.notify();
	}

	/// Rows one PageUp/PageDown moves in the left tool window.
	fn tool_page_rows(&self, window: &Window) -> isize {
		let h = f32::from(window.viewport_size().height);
		((h - HEADER_H - STATUS_H - PANEL_HEADER_H) / ROW_H) as isize - 2
	}

	/// Row labels the speed search matches against.
	fn tool_row_labels(&self) -> Vec<String> {
		if self.active_tab == WorkbenchTab::GitChanges {
			self.change_item_rows()
				.into_iter()
				.map(|r| match r {
					ChangeItemRow::Header { label, .. } => {
						t(label, self.locale).to_string()
					}
					ChangeItemRow::File { file_idx } => self
						.files
						.get(file_idx)
						.map(|f| {
							let p = f.path.trim_end_matches('/');
							p.rsplit('/').next().unwrap_or(p).to_string()
						})
						.unwrap_or_default(),
				})
				.collect()
		} else {
			self.project_rows()
				.into_iter()
				.map(|r| match r {
					ProjRow::Repo(i) => self.repos[i].name.clone(),
					ProjRow::Work(w) => w.name,
					ProjRow::Rev(r) => r.name,
				})
				.collect()
		}
	}

	/// Speed search: the next row (from the cursor, `step` 0 = the cursor
	/// itself) whose label contains the typed text, case-insensitively.
	fn speed_jump(&mut self, step: isize, cx: &mut Context<Self>) {
		let q = self.chrome.speed.to_lowercase();
		let labels = self.tool_row_labels();
		let n = labels.len();
		if n == 0 {
			return;
		}
		let cur = if self.active_tab == WorkbenchTab::GitChanges {
			self.selected_list_row
		} else {
			self.tree_cursor
		}
		.min(n - 1);
		let first = usize::from(step != 0);
		let hit = (first..n + first)
			.map(|k| {
				if step < 0 {
					(cur + n - k % n) % n
				} else {
					(cur + k) % n
				}
			})
			.find(|&i| labels[i].to_lowercase().contains(&q));
		app_log!(
			"[APP:SPEED_SEARCH: q={} row={}]",
			self.chrome.speed,
			hit.map(|r| r.to_string()).unwrap_or_else(|| "none".into())
		);
		match hit {
			Some(row) => self.set_tool_cursor(row, cx),
			None => cx.notify(),
		}
	}

	/// Typing in the Project / Changes list starts IntelliJ speed search.
	fn speed_key(&mut self, ev: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
		let k = &ev.keystroke;
		let m = &k.modifiers;
		if m.control || m.alt || m.platform || m.function {
			return;
		}
		if k.key == "backspace" {
			if self.chrome.speed.pop().is_none() {
				return;
			}
		} else {
			match k.key_char.as_deref() {
				Some(c)
					if !c.chars().any(char::is_control) && !c.is_empty() =>
				{
					self.chrome.speed.push_str(c)
				}
				_ => return,
			}
		}
		cx.stop_propagation();
		if self.chrome.speed.is_empty() {
			app_log!("[APP:SPEED_SEARCH: off]");
			cx.notify();
		} else {
			self.speed_jump(0, cx);
		}
	}

	/// Keyboard: activate / expand / toggle the row under the tool cursor.
	fn tool_action(
		&mut self,
		action: &str,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if matches!(action, "up" | "down") {
			let delta = if action == "up" { -1 } else { 1 };
			if self.chrome.speed.is_empty() {
				self.tool_move(delta, cx);
			} else {
				self.speed_jump(delta, cx);
			}
			return;
		}
		if self.active_tab == WorkbenchTab::GitChanges {
			let rows = self.change_item_rows();
			let Some(cur) = rows.len().checked_sub(1) else {
				return;
			};
			let cur = self.selected_list_row.min(cur);
			match (&rows[cur], action) {
				(ChangeItemRow::Header { group_id, .. }, _) => {
					let group = *group_id;
					let collapsed =
						self.chrome.collapsed_groups.contains(&group);
					match action {
						"toggle" => self.toggle_change_group(group, cx),
						"open" => self.toggle_group_collapsed(group, cx),
						"expand" if collapsed => {
							self.toggle_group_collapsed(group, cx)
						}
						"collapse" if !collapsed => {
							self.toggle_group_collapsed(group, cx)
						}
						_ => {}
					}
				}
				(ChangeItemRow::File { file_idx }, "toggle") => {
					self.toggle_file(*file_idx, cx);
				}
				(ChangeItemRow::File { .. }, "open") => {
					self.set_tool_cursor(cur, cx);
				}
				(ChangeItemRow::File { .. }, "collapse") => {
					// IntelliJ: Left on a leaf goes to its parent node.
					if let Some(h) = rows[..cur].iter().rposition(|r| {
						matches!(r, ChangeItemRow::Header { .. })
					}) {
						self.set_tool_cursor(h, cx);
					}
				}
				_ => {}
			}
			return;
		}
		let rows = self.project_rows();
		if rows.is_empty() {
			return;
		}
		let last = rows.len() - 1;
		self.tree_cursor = self.tree_cursor.min(last);
		let expand = action == "expand";
		let collapse = action == "collapse";
		match &rows[self.tree_cursor] {
			ProjRow::Repo(i) => {
				if action == "open" || expand {
					self.select_repo(*i, cx);
				}
			}
			ProjRow::Work(r) => {
				let gesture = match action {
					"toggle" => Some(RowGesture::Toggle),
					"expand" => Some(RowGesture::Expand),
					"collapse" => Some(RowGesture::Collapse),
					"open" => Some(RowGesture::Primary),
					_ => None,
				};
				if let Some(gesture) = gesture {
					let cmd = command_for_row(r, gesture);
					self.dispatch_tree(cmd, cx);
				}
			}
			ProjRow::Rev(r) if r.marker.is_none() => {
				let dir = r.kind == snip_core::browser::TreeKind::Tree;
				let path = r.path.clone();
				if action == "toggle" {
					if r.kind == snip_core::browser::TreeKind::Blob {
						if let Some(tree) = &self.rev_tree {
							let sha = tree.sha.clone();
							self.toggle_rev_file_selection(&sha, &path, cx);
						}
					}
				} else if (dir
					&& ((expand && !r.expanded) || (collapse && r.expanded)))
					|| action == "open"
				{
					self.rev_tree_click(&path, dir, cx);
				}
			}
			_ => {}
		}
		let _ = window;
		cx.notify();
	}

	/// Right-click on a left tool-window row: focus the list (the row is
	/// already the cursor) and open its menu.
	fn open_left_menu(
		&mut self,
		items: Vec<crate::menu::MenuEntry>,
		ev: &MouseDownEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		window.focus(&self.tree_focus);
		self.open_menu(
			crate::menu::MenuOrigin::Left,
			items,
			ev.position,
			window,
			cx,
		);
	}

	fn splitter(&self, which: Splitter, cx: &mut Context<Self>) -> AnyElement {
		let id = match which {
			Splitter::Left => "splitter-left",
			Splitter::Bottom => "splitter-bottom",
			Splitter::LogDetails => "splitter-log-details",
		};
		let d = div()
			.id(id)
			.relative()
			.flex_shrink_0()
			// Transparent: the splitter is the frame gap between islands.
			.hover(|s| s.bg(rgb(pal().divider)))
			.on_mouse_down(
				MouseButton::Left,
				cx.listener(move |this, _: &MouseDownEvent, _, cx| {
					this.dragging = Some(which);
					cx.stop_propagation();
					cx.notify();
				}),
			)
			.children(probe(&self.probes, id));
		match which {
			Splitter::Left => d.w(px(SPLITTER)).h_full().cursor_ew_resize(),
			Splitter::Bottom => d.h(px(SPLITTER)).w_full().cursor_ns_resize(),
			Splitter::LogDetails => d
				.w(px(SPLITTER))
				.h_full()
				.border_l_1()
				.border_color(rgb(pal().divider))
				.cursor_ew_resize(),
		}
		.into_any_element()
	}

	// ───────────────────────── header ─────────────────────────

	fn render_workspace_chip(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let label = if self.workspace_open {
			self.workspace_root
				.file_name()
				.map(|n| n.to_string_lossy().to_string())
				.filter(|n| !n.is_empty())
				.unwrap_or_else(|| self.workspace_root.display().to_string())
		} else {
			t("workspace_none", loc).to_string()
		};
		let tip_text = if self.workspace_open {
			self.workspace_root.display().to_string()
		} else {
			t("workspace_none", loc).to_string()
		};
		let panel = div()
			.id("workspace-menu")
			.occlude()
			.w(px(300.))
			.flex()
			.flex_col()
			.gap(px(6.))
			.p(px(6.))
			.bg(rgb(pal().panel_bg))
			.border_1()
			.border_color(rgb(pal().button_border))
			.rounded(px(6.))
			.shadow_lg()
			.on_mouse_down_out(cx.listener(|this, _, _, cx| {
				this.workspace_menu = false;
				this.workspace_picker = false;
				cx.notify();
			}))
			.child(
				button(
					"btn-close-workspace",
					t("workspace_close", loc),
					Btn::Default,
					true,
					61,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.workspace_menu = false;
					this.request_user_close(
						crate::lifecycle::Intent::CloseWorkspace,
						cx,
					);
				}))
				.children(probe(log, "btn-close-workspace")),
			)
			.child(
				button(
					"btn-open-workspace",
					t("workspace_open", loc),
					Btn::Default,
					true,
					62,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.show_workspace_picker(cx);
				}))
				.children(probe(log, "btn-open-workspace")),
			)
			.when(self.workspace_picker, |d| {
				d.child(
					div()
						.id("workspace-path-input")
						.relative()
						.child(self.workspace_path_input.clone())
						.children(probe(log, "workspace-path-input")),
				)
				.child(
					button(
						"btn-workspace-open-confirm",
						t("workspace_open_confirm", loc),
						Btn::Primary,
						true,
						63,
					)
					.on_click(cx.listener(|this, _, _, cx| {
						let text = this
							.workspace_path_input
							.read(cx)
							.text()
							.trim()
							.to_string();
						this.confirm_open_workspace(&text, cx);
					}))
					.children(probe(log, "btn-workspace-open-confirm")),
				)
			});
		div()
			.relative()
			.flex_shrink_0()
			.max_w(px(180.))
			.child(
				div()
					.id("btn-workspace-menu")
					.relative()
					.flex()
					.items_center()
					.min_w_0()
					.max_w(px(160.))
					.h(px(26.))
					.px(px(8.))
					.rounded(px(4.))
					.cursor_pointer()
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.when(self.workspace_menu, |d| d.bg(rgb(pal().hover_bg)))
					.tooltip(tip(format!(
						"{tip_text} · {}",
						t("tip_workspace_menu", loc)
					)))
					.on_click(cx.listener(|this, _, _, cx| {
						this.toggle_workspace_menu(cx);
					}))
					.child(clip_text(label))
					.children(probe(log, "btn-workspace-menu")),
			)
			.when(self.workspace_menu, |d| {
				d.child(
					div().absolute().top(px(28.)).left_0().child(
						deferred(anchored().snap_to_window().child(panel))
							.with_priority(1),
					),
				)
			})
			.into_any_element()
	}

	fn render_workspace_closed(&self) -> AnyElement {
		let loc = self.locale;
		div()
			.id("workspace-closed")
			.relative()
			.flex_1()
			.min_w_0()
			.flex()
			.items_center()
			.justify_center()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(
				div()
					.max_w(px(420.))
					.px(px(16.))
					.text_color(rgb(pal().text_muted))
					.child(t("workspace_closed", loc)),
			)
			.children(probe(&self.probes, "workspace-closed"))
			.into_any_element()
	}

	fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;

		let repo = self.repo();
		let count = self.basket_count();
		let copy_reason = if self.is_copying {
			Some(t("btn_copying", loc))
		} else if count > 0 {
			None
		} else if self.selected_commit.is_some() && self.selected_file.is_none()
		{
			Some(t("btn_copy_commit_readonly", loc))
		} else {
			Some(t("btn_copy_empty", loc))
		};
		let copy_enabled = copy_reason.is_none();
		let branch = repo
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone())
			.unwrap_or_else(|| "-".into());
		let ref_label = match &self.active_ref_filter {
			Some(r) => r.trim_start_matches("refs/heads/").to_string(),
			None => branch.clone(),
		};
		let repo_error = repo.is_some_and(|r| r.summary.is_err());

		let selector = |id: &'static str,
		                ic: Icon,
		                label: String,
		                open: bool,
		                tab: isize,
		                tooltip: String| {
			div()
				.id(id)
				.relative()
				.flex()
				.items_center()
				.gap(px(5.))
				.min_w_0()
				.max_w(px(260.))
				.h(px(26.))
				.px(px(8.))
				.rounded(px(4.))
				.border_1()
				.border_color(transparent_black())
				.cursor_pointer()
				.tab_index(tab)
				.hover(|s| s.bg(rgb(pal().hover_bg)))
				.when(open, |d| d.bg(rgb(pal().hover_bg)))
				.map(focus_ring)
				// No tooltip over an open popup (IntelliJ hides it).
				.when(!open, |d| d.tooltip(tip(tooltip)))
				.child(icon(ic, 14.))
				.child(clip_text(label).font_weight(FontWeight::SEMIBOLD))
				.child(icon(Icon::ChevronDown, 10.))
				.children(probe(log, id))
		};

		div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(HEADER_H))
			.px(px(8.))
			.gap(px(4.))
			.bg(rgb(pal().header_bg))
			.child(
				div()
					.flex_shrink_0()
					.size(px(18.))
					.rounded(px(4.))
					.bg(rgb(pal().accent))
					.flex()
					.items_center()
					.justify_center()
					.text_size(px(10.))
					.font_weight(FontWeight::BOLD)
					.text_color(rgb(pal().accent_text))
					.child("S"),
			)
			.child(self.render_workspace_chip(cx))
			.child(toolbar_divider())
			.child(
				div()
					.relative()
					.flex()
					.min_w_0()
					.child(
						selector(
							"btn-repo-selector",
							if repo_error {
								Icon::Warning
							} else {
								Icon::Project
							},
							repo.map(|r| r.name.clone())
								.unwrap_or_else(|| t("no_repo", loc).into()),
							self.popover == Some(Popover::Repo),
							1,
							t("tip_repo_selector", loc).into(),
						)
						.on_click(cx.listener(|this, _, window, cx| {
							this.open_popover(Popover::Repo, window, cx)
						})),
					)
					.when(self.popover == Some(Popover::Repo), |d| {
						d.child(self.render_popover(cx))
					}),
			)
			.child(
				div()
					.relative()
					.flex()
					.min_w_0()
					.child(
						selector(
							"btn-ref-selector",
							Icon::Branch,
							ref_label,
							self.popover == Some(Popover::Ref),
							2,
							tf("tip_ref_selector", loc, &[&branch]),
						)
						.on_click(cx.listener(|this, _, window, cx| {
							this.open_popover(Popover::Ref, window, cx)
						})),
					)
					.when(self.popover == Some(Popover::Ref), |d| {
						d.child(self.render_popover(cx))
					}),
			)
			.child(div().flex_1())
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(2.))
					.flex_shrink_0()
					.child(
						// IntelliJ main toolbar widget: a transparent icon with a
						// small count badge instead of a filled button.
						icon_button(
							"btn-copy",
							Icon::Basket,
							copy_reason.map(str::to_string).unwrap_or_else(
								|| tf("tip_basket_copy", loc, &[count]),
							),
							copy_enabled,
							3,
						)
						.when(copy_enabled, |b| {
							b.on_click(cx.listener(|this, _, _, cx| {
								this.copy_selection_to_clipboard(cx)
							}))
						})
						.when(count > 0, |b| {
							b.child(
								div()
									.absolute()
									.top(px(-3.))
									.right(px(-4.))
									.min_w(px(13.))
									.h(px(13.))
									.px(px(3.))
									.rounded(px(7.))
									.flex()
									.items_center()
									.justify_center()
									.bg(rgb(pal().accent))
									.text_size(px(9.))
									.font_weight(FontWeight::SEMIBOLD)
									.text_color(rgb(pal().accent_text))
									.child(count.to_string()),
							)
						})
						.children(probe(log, "btn-copy")),
					)
					.when(self.is_copying, |row| {
						row.child(
							button(
								"btn-copy-cancel",
								t("btn_copy_cancel", loc),
								Btn::Default,
								true,
								32,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								this.cancel_copy(cx)
							}))
							.children(probe(log, "btn-copy-cancel")),
						)
					})
					.when(count > 0, |row| {
						row.child(
							icon_button(
								"btn-basket-clear",
								Icon::Close,
								t("basket_clear", loc),
								true,
								31,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								this.clear_basket(cx);
							}))
							.children(probe(log, "btn-basket-clear")),
						)
					})
					.child(
						icon_button(
							"btn-paste",
							Icon::Paste,
							t("btn_paste", loc),
							true,
							4,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.trigger_paste_preview(cx)
						}))
						.children(probe(log, "btn-paste")),
					)
					.child(
						icon_button(
							"btn-refresh",
							Icon::Refresh,
							t("btn_refresh", loc),
							true,
							5,
						)
						.on_click(
							cx.listener(|this, _, _, cx| this.reload_repos(cx)),
						)
						.children(probe(log, "btn-refresh")),
					),
			)
			.into_any_element()
	}

	/// IntelliJ branches / repositories popup: search field on top, then the
	/// list with Local / Remote / Tags section headers. Headers are display
	/// rows only; `popover_cursor` and the filter count stay over items.
	fn render_popover(&self, cx: &mut Context<Self>) -> AnyElement {
		const ITEM_H: f32 = 24.0;
		let log = &self.probes;
		let q = self.selector_input.read(cx).text().to_string();
		// (first item index, group) of each section, at most three.
		let mut sections: Vec<(usize, &'static str)> = Vec::new();
		let mut n = 0;
		let mut prev = None;
		for c in self.selector_candidates(&q) {
			let g = c.group();
			if g.is_some() && g != prev {
				sections.push((n, g.unwrap_or_default()));
			}
			prev = g;
			n += 1;
		}
		let rows = n + sections.len();
		let list_h = (rows.max(1) as f32 * ITEM_H).min(360.0);
		let cursor = self.popover_cursor;
		let panel = div()
			.id("selector-popover")
			.occlude()
			.w(px(360.))
			.flex()
			.flex_col()
			.bg(rgb(pal().popup_bg))
			.border_1()
			.border_color(rgb(pal().popup_border))
			.rounded(px(ISLAND_RADIUS))
			.shadow_lg()
			.p(px(4.))
			.gap(px(4.))
			.on_mouse_down_out(
				cx.listener(|this, _, _, cx| this.close_popover(cx)),
			)
			.child(
				div()
					.id("selector-input")
					.relative()
					.flex()
					.items_center()
					.gap(px(4.))
					.pl(px(6.))
					.child(icon(Icon::Search, 14.))
					.child(
						div()
							.flex_1()
							.min_w_0()
							.child(self.selector_input.clone()),
					)
					.children(probe(log, "selector-input")),
			)
			.child(div().h(px(1.)).mx(px(-4.)).bg(rgb(pal().popup_border)))
			.child(
				div()
					.h(px(list_h))
					.when(n == 0, |d| {
						d.child(
							div()
								.p(px(6.))
								.text_color(rgb(pal().text_muted))
								.child(t("selector_empty", self.locale)),
						)
					})
					.when(n > 0, |d| {
						d.child(
							uniform_list(
								"selector-items",
								rows,
								cx.processor(
									move |this,
									      range: std::ops::Range<usize>,
									      _,
									      cx| {
										this.popover_rows(
											&sections, range, cursor, cx,
										)
									},
								),
							)
							.size_full(),
						)
					}),
			);
		div()
			.absolute()
			.top(px(28.))
			.left_0()
			.child(
				deferred(anchored().snap_to_window().child(panel))
					.with_priority(1),
			)
			.into_any_element()
	}

	/// Display rows `range` of the popup: section headers and items.
	fn popover_rows(
		&self,
		sections: &[(usize, &'static str)],
		range: std::ops::Range<usize>,
		cursor: usize,
		cx: &mut Context<Self>,
	) -> Vec<Stateful<Div>> {
		// Display index -> Err(header group) or Ok(item index).
		let map = |d: usize| {
			let mut shift = 0;
			for (k, (start, g)) in sections.iter().enumerate() {
				if d == start + k {
					return Err(*g);
				}
				if d > start + k {
					shift = k + 1;
				}
			}
			Ok(d - shift)
		};
		let items: Vec<usize> =
			range.clone().filter_map(|d| map(d).ok()).collect();
		let q = self.selector_input.read(cx).text().to_string();
		let mut picked = match (items.first(), items.last()) {
			(Some(&a), Some(&b)) => self.selector_items(&q, a..b + 1).collect(),
			_ => Vec::new(),
		}
		.into_iter();
		let current_branch = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		range
			.map(|d| match map(d) {
				Err(g) => div()
					.id(SharedString::from(format!("selector-group:{g}")))
					.w_full()
					.flex()
					.items_center()
					.h(px(24.))
					.px(px(8.))
					.text_size(px(11.))
					.font_weight(FontWeight::SEMIBOLD)
					.text_color(rgb(pal().text_muted))
					.child(t(g, self.locale)),
				Ok(_) => {
					let Some((ix, it)) = picked.next() else {
						return div().id(SharedString::from(format!(
							"selector-gap:{d}"
						)));
					};
					let pick = it.pick.clone();
					let active = match &it.pick {
						Pick::Repo(i) => self.selected_repo_idx == Some(*i),
						Pick::Ref(r) => r == &self.active_ref_filter,
					};
					let ic = if it.error {
						Icon::Warning
					} else {
						match (&it.pick, it.group) {
							(Pick::Repo(_), _) => Icon::Project,
							(Pick::Ref(None), _) => Icon::GitLog,
							(Pick::Ref(Some(r)), _) if r == "HEAD" => {
								Icon::Head
							}
							(_, Some("refs_tags")) => Icon::Tag,
							(_, Some("refs_remote")) => Icon::RemoteBranch,
							_ if current_branch.as_deref()
								== Some(it.label.as_str()) =>
							{
								Icon::Head
							}
							_ => Icon::Branch,
						}
					};
					div()
						.id(SharedString::from(it.id.clone()))
						.relative()
						.w_full()
						.flex()
						.items_center()
						.gap(px(6.))
						.h(px(24.))
						.px(px(8.))
						.rounded(px(4.))
						.cursor_pointer()
						.when(ix == cursor, |d| d.bg(rgb(pal().selection_bg)))
						.when(ix != cursor, |d| {
							d.hover(|s| s.bg(rgb(pal().hover_bg)))
						})
						.on_click(cx.listener(move |this, _, _, cx| {
							this.choose(pick.clone(), cx)
						}))
						.child(icon(ic, 14.))
						.child(fill_text(it.label.clone()))
						.child(
							div()
								.flex_shrink_0()
								.text_size(px(11.))
								.text_color(rgb(if it.error {
									pal().error
								} else {
									pal().text_muted
								}))
								.child(it.detail.clone()),
						)
						.child(
							div()
								.flex_shrink_0()
								.w(px(14.))
								.when(active, |d| {
									d.child(icon(Icon::Checked, 14.))
								}),
						)
						.children(probe(&self.probes, it.id.clone()))
				}
			})
			.collect()
	}

	// ───────────────────────── rail ─────────────────────────

	fn render_rail(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let project_on =
			self.left_visible && self.active_tab == WorkbenchTab::FileExplorer;
		let changes_on =
			self.left_visible && self.active_tab == WorkbenchTab::GitChanges;
		let rail_button = |id: &'static str,
		                   ic: Icon,
		                   on: bool,
		                   label: &'static str,
		                   tab: isize| {
			div()
				.id(id)
				.relative()
				.size(px(28.))
				.rounded(px(5.))
				.flex()
				.items_center()
				.justify_center()
				.cursor_pointer()
				.border_1()
				.border_color(transparent_black())
				.tab_index(tab)
				// Islands: the open tool is a filled accent pill on the frame.
				.when(on, |d| d.bg(rgb(pal().rail_active_bg)))
				.when(!on, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.map(focus_ring)
				.tooltip(tip(label))
				.child(if on {
					icon_tinted(ic, 16., pal().accent_text).into_any_element()
				} else {
					icon(ic, 16.).into_any_element()
				})
				.children(probe(log, id))
		};
		div()
			.flex()
			.flex_col()
			.items_center()
			.flex_shrink_0()
			.w(px(RAIL_W))
			.h_full()
			.py(px(6.))
			.gap(px(4.))
			.child(
				rail_button(
					"rail-project",
					Icon::Project,
					project_on,
					t("tip_project", loc),
					10,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.activate_tool(WorkbenchTab::FileExplorer, cx)
				})),
			)
			.child(
				rail_button(
					"rail-changes",
					Icon::Changes,
					changes_on,
					t("tip_changes", loc),
					11,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.activate_tool(WorkbenchTab::GitChanges, cx)
				})),
			)
			.child(div().flex_1())
			.child(
				rail_button(
					"rail-log",
					Icon::GitLog,
					self.bottom_visible,
					t("tip_git_log", loc),
					12,
				)
				.on_click(cx.listener(|this, _, _, cx| this.toggle_log(cx))),
			)
			.into_any_element()
	}

	// ───────────────────────── left tool window ─────────────────────────

	fn render_left(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let is_project = self.active_tab == WorkbenchTab::FileExplorer;
		let title = if let (true, Some(tree)) = (is_project, &self.rev_tree) {
			tf("project_rev_title", loc, &[&short(&tree.sha)])
		} else if is_project {
			t("project", loc).to_string()
		} else {
			tf("changes_title", loc, &[&self.files.len()])
		};
		let header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(PANEL_HEADER_H))
			.pl(px(10.))
			.pr(px(4.))
			.gap(px(2.))
			.child(fill_text(title).font_weight(FontWeight::SEMIBOLD))
			.when(is_project && self.rev_tree.is_some(), |d| {
				d.child(
					icon_button(
						"btn-leave-tree",
						Icon::Back,
						t("btn_back_to_working", loc),
						true,
						13,
					)
					.on_click(
						cx.listener(|this, _, _, cx| this.leave_rev_tree(cx)),
					)
					.children(probe(&self.probes, "btn-leave-tree")),
				)
			})
			.when(is_project && self.rev_tree.is_none(), |d| {
				d.child(
					icon_button(
						"btn-add-repo",
						Icon::Plus,
						t("tip_add_repo", loc),
						true,
						12,
					)
					.when(self.is_adding_repo, |b| {
						b.bg(rgb(pal().rail_active_bg))
					})
					.on_click(cx.listener(|this, _, _, cx| {
						this.is_adding_repo = !this.is_adding_repo;
						cx.notify();
					}))
					.children(probe(&self.probes, "btn-add-repo")),
				)
				.when(
					matches!(
						self.discovery_status,
						Some(
							ScanStatus::LimitReached
								| ScanStatus::Incomplete | ScanStatus::More
								| ScanStatus::Cancelled | ScanStatus::TimedOut
						)
					),
					|d| {
						d.child(
							icon_button(
								"btn-discovery-continue",
								Icon::ArrowDown,
								t("btn_discovery_continue", loc),
								true,
								10,
							)
							.on_click(cx.listener(|this, _, _, cx| {
								this.continue_discovery(cx);
							}))
							.children(probe(
								&self.probes,
								"btn-discovery-continue",
							)),
						)
					},
				)
				.when(!self.discovery_errors.is_empty(), |d| {
					d.child(
						icon_button(
							"btn-discovery-retry",
							Icon::Refresh,
							t("btn_discovery_retry", loc),
							true,
							11,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.reload_repos(cx);
						}))
						.children(probe(&self.probes, "btn-discovery-retry")),
					)
				})
			})
			.when(!is_project, |d| {
				d.child(
					icon_button(
						"btn-select-all",
						Icon::SelectAll,
						t("btn_select_all", loc),
						true,
						13,
					)
					.on_click(
						cx.listener(|this, _, _, cx| this.select_all_files(cx)),
					),
				)
			})
			.when(self.rev_tree.is_none() || !is_project, |d| {
				d.child(
					icon_button(
						"btn-select-none",
						Icon::SelectNone,
						t("btn_deselect_all", loc),
						true,
						14,
					)
					.on_click(
						cx.listener(|this, _, _, cx| {
							this.deselect_all_files(cx)
						}),
					),
				)
			});
		let n = if is_project {
			self.project_rows().len()
		} else {
			self.change_item_rows().len()
		};
		let list = uniform_list(
			"left-rows",
			n,
			cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
				if this.active_tab == WorkbenchTab::FileExplorer {
					let rows = this.project_rows();
					let mut out = Vec::new();
					for (ix, row) in rows.into_iter().enumerate() {
						if range.contains(&ix) {
							out.push(this.project_row(ix, row, cx));
						}
					}
					out
				} else {
					let rows = this.change_item_rows();
					let mut out = Vec::new();
					for (row_idx, item) in rows.into_iter().enumerate() {
						if range.contains(&row_idx) {
							match item {
								ChangeItemRow::Header {
									label,
									count,
									group_id,
								} => {
									out.push(this.change_header_row(
										label, count, group_id, row_idx, cx,
									));
								}
								ChangeItemRow::File { file_idx } => {
									out.push(
										this.change_row(file_idx, row_idx, cx),
									);
								}
							}
						}
					}
					out
				}
			}),
		)
		// Islands: rows are inset and rounded inside the island.
		.px(px(4.))
		.track_scroll(self.chrome.left_scroll.clone())
		.size_full();

		div()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(width))
			.h_full()
			.bg(rgb(pal().panel_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(header)
			.when(is_project && self.is_adding_repo, |d| {
				d.child(
					div()
						.flex()
						.flex_row()
						.items_center()
						.px(px(6.))
						.py(px(4.))
						.gap(px(4.))
						.child(
							div()
								.flex_1()
								.id(SharedString::from("input-add-repo"))
								.child(self.add_repo_input.clone())
								.children(probe(
									&self.probes,
									"input-add-repo",
								)),
						),
				)
			})
			.when(is_project && !self.discovery_errors.is_empty(), |d| {
				let err_msg =
					format!("{} scan error(s)", self.discovery_errors.len());
				d.child(
					div()
						.id(SharedString::from("discovery-error"))
						.flex()
						.flex_row()
						.items_center()
						.px(px(10.))
						.py(px(2.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().error))
						.child(clip_text(err_msg))
						.children(probe(&self.probes, "discovery-error")),
				)
			})
			.child(
				div()
					.id("left-list")
					.relative()
					.key_context("ToolList")
					.track_focus(&self.tree_focus)
					.on_action(cx.listener(|this, _: &TreeUp, w, cx| {
						this.tool_action("up", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeDown, w, cx| {
						this.tool_action("down", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeExpand, w, cx| {
						this.tool_action("expand", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeCollapse, w, cx| {
						this.tool_action("collapse", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeOpen, w, cx| {
						this.tool_action("open", w, cx)
					}))
					.on_action(cx.listener(|this, _: &TreeToggle, w, cx| {
						this.tool_action("toggle", w, cx)
					}))
					.on_action(cx.listener(
						|this, _: &crate::ToolPageUp, w, cx| {
							this.tool_move(-this.tool_page_rows(w), cx)
						},
					))
					.on_action(cx.listener(
						|this, _: &crate::ToolPageDown, w, cx| {
							this.tool_move(this.tool_page_rows(w), cx)
						},
					))
					.on_key_down(cx.listener(
						|this, ev: &gpui::KeyDownEvent, _, cx| {
							this.speed_key(ev, cx)
						},
					))
					.flex_1()
					.min_h_0()
					.when(n == 0, |d| {
						d.child(
							div()
								.p(px(10.))
								.text_color(rgb(pal().text_muted))
								.child(if is_project {
									t("empty_project", loc)
								} else {
									t("clean_working_copy", loc)
								}),
						)
					})
					.child(list)
					.when(!self.chrome.speed.is_empty(), |d| {
						d.child(self.speed_search_popup())
					})
					.children(probe(&self.probes, "left-list")),
			)
			.into_any_element()
	}

	/// Row label with the speed-search match highlighted (IntelliJ paints
	/// the matched substring), plain text otherwise.
	fn speed_label(&self, text: String) -> Div {
		let q = self.chrome.speed.to_lowercase();
		let hit = (!q.is_empty() && self.left_active)
			.then(|| {
				// Lowercasing may change byte lengths; only highlight when
				// the match maps back onto the original text.
				let lower = text.to_lowercase();
				(lower.len() == text.len())
					.then(|| lower.find(&q).map(|i| i..i + q.len()))
					.flatten()
			})
			.flatten()
			.filter(|r| {
				text.is_char_boundary(r.start) && text.is_char_boundary(r.end)
			});
		let base = div()
			.min_w_0()
			.overflow_hidden()
			.line_clamp(1)
			.text_ellipsis();
		match hit {
			Some(range) => {
				base.child(gpui::StyledText::new(text).with_highlights([(
					range,
					gpui::HighlightStyle {
						// IntelliJ paints speed-search matches amber with
						// dark text in both themes.
						background_color: Some(rgb(LIGHT.find_bg).into()),
						color: Some(rgb(0x000000).into()),
						..Default::default()
					},
				)]))
			}
			None => base.child(text),
		}
	}

	/// IntelliJ speed search field, floating over the tool window header
	/// at the top-left of the list so no row is covered.
	fn speed_search_popup(&self) -> AnyElement {
		let none = self.tool_row_labels().iter().all(|l| {
			!l.to_lowercase().contains(&self.chrome.speed.to_lowercase())
		});
		div()
			.id("speed-search")
			.absolute()
			.top(px(3. - PANEL_HEADER_H))
			.left(px(6.))
			.flex()
			.items_center()
			.gap(px(4.))
			.h(px(22.))
			.px(px(6.))
			.max_w(px(240.))
			.rounded(px(4.))
			.bg(rgb(pal().popup_bg))
			.border_1()
			.border_color(rgb(if none {
				pal().error
			} else {
				pal().popup_border
			}))
			.shadow_md()
			.text_size(px(SMALL_TEXT))
			.child(icon(Icon::Search, 12.))
			.child(
				clip_text(self.chrome.speed.clone()).text_color(rgb(if none {
					pal().error
				} else {
					pal().text
				})),
			)
			.children(probe(&self.probes, "speed-search"))
			.into_any_element()
	}

	/// Bright while the left list has focus, grey otherwise, like IntelliJ.
	fn left_selection_bg(&self) -> u32 {
		if self.left_active {
			pal().selection_bg
		} else {
			pal().selection_inactive_bg
		}
	}

	fn project_row(
		&self,
		ix: usize,
		row: ProjRow,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let cursor = ix == self.tree_cursor;
		match row {
			ProjRow::Repo(idx) => {
				let repo = &self.repos[idx];
				let selected = self.selected_repo_idx == Some(idx);
				let id = format!("repo-row:{}", repo.name);
				let mut counts = div()
					.flex()
					.flex_row()
					.flex_shrink_0()
					.gap(px(5.))
					.text_size(px(SMALL_TEXT));
				let tooltip;
				match &repo.summary {
					Ok(s) => {
						let parts = [
							("+", s.changes.staged, pal().git_added),
							("~", s.changes.unstaged, pal().git_modified),
							("?", s.changes.untracked, pal().git_untracked),
							("!", s.changes.conflicted, pal().git_conflict),
						];
						let mut any = false;
						for (sym, n, color) in parts {
							if n > 0 {
								any = true;
								counts = counts.child(
									div()
										.text_color(rgb(color))
										.child(format!("{sym}{n}")),
								);
							}
						}
						if !any {
							counts = counts.child(
								div()
									.text_color(rgb(pal().text_muted))
									.child(t("clean", self.locale)),
							);
						}
						tooltip = format!(
							"{}\n{}",
							repo.root.display(),
							t("counts_tip", self.locale)
						);
					}
					Err(e) => {
						counts = counts.child(
							div()
								.text_color(rgb(pal().error))
								.child(t("repo_error_short", self.locale)),
						);
						tooltip = format!("{}\n{}", repo.root.display(), e);
					}
				}
				let branch = match &repo.summary {
					Ok(s) => {
						if let Some(ref b) = s.branch {
							b.clone()
						} else if let Some(ref h) = s.head {
							format!("({})", &h[..7.min(h.len())])
						} else {
							format!("({})", t("repo_unborn", self.locale))
						}
					}
					Err(_) => String::new(),
				};
				let kind_badge = match repo.kind {
					RepoEntryKind::LinkedWorktree => {
						Some(t("repo_kind_worktree", self.locale))
					}
					RepoEntryKind::Submodule => {
						Some(t("repo_kind_submodule", self.locale))
					}
					RepoEntryKind::UninitializedSubmodule => {
						Some(t("repo_kind_uninit_submodule", self.locale))
					}
					RepoEntryKind::Main => None,
				};
				let is_err = repo.summary.is_err()
					|| repo.kind == RepoEntryKind::UninitializedSubmodule;
				div()
					.id(SharedString::from(id.clone()))
					.relative()
					.flex()
					.flex_row()
					.items_center()
					.w_full()
					.h(px(ROW_H))
					.px(px(6.))
					.gap(px(5.))
					.cursor_pointer()
					.rounded(px(4.))
					.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
					.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
					.when(self.chrome.menu.is_none(), |d| {
						d.tooltip(tip(tooltip))
					})
					.on_click(cx.listener(move |this, _, _, cx| {
						this.tree_cursor = ix;
						this.select_repo(idx, cx);
					}))
					.on_mouse_down(
						MouseButton::Right,
						cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
							this.tree_cursor = ix;
							let items = this.repo_row_menu(idx);
							this.open_left_menu(items, ev, w, cx);
						}),
					)
					.child(icon(
						if selected {
							Icon::ChevronDown
						} else {
							Icon::ChevronRight
						},
						10.,
					))
					.child(icon(
						if is_err { Icon::Warning } else { Icon::Project },
						14.,
					))
					.child(
						self.speed_label(repo.name.clone())
							.font_weight(FontWeight::SEMIBOLD),
					)
					.when_some(kind_badge, |d, badge| {
						d.child(
							div()
								.flex_shrink_0()
								.px(px(4.))
								.rounded(px(3.))
								.border_1()
								.border_color(rgb(pal().divider))
								.text_size(px(10.))
								.text_color(rgb(pal().text_muted))
								.child(badge),
						)
					})
					.child(
						fill_text(branch)
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().text_muted)),
					)
					.child(counts)
					.children(probe(log, id))
					.into_any_element()
			}
			ProjRow::Work(row) => self.work_tree_row(ix, row, cursor, cx),
			ProjRow::Rev(row) => self.rev_tree_row(ix, row, cursor, cx),
		}
	}

	fn work_tree_row(
		&self,
		ix: usize,
		row: FlattenedTreeRow,
		cursor: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let indent = 8.0 + row.depth as f32 * 14.0;
		if row.is_error
			|| row.is_truncation_marker
			|| row.is_more_marker
			|| row.is_view_limit
			|| row.is_loading
		{
			let marker_id = if row.is_view_limit {
				format!("tree-view-more:{}", row.rel_path)
			} else if row.is_more_marker {
				format!("tree-continue:{}", row.rel_path)
			} else if row.is_error {
				format!("tree-retry:{}", row.rel_path)
			} else if row.is_loading {
				format!("tree-loading:{}", row.rel_path)
			} else {
				format!("tree-marker:{}", row.rel_path)
			};
			let action_row = row.clone();
			let is_actionable =
				command_for_row(&row, RowGesture::Primary).is_some();
			return div()
				.id(SharedString::from(marker_id.clone()))
				.relative()
				.w_full()
				.h(px(ROW_H))
				.pl(px(indent + 28.0))
				.flex()
				.items_center()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(if row.is_error {
					pal().error
				} else {
					pal().text_muted
				}))
				.when(is_actionable, |d| {
					d.cursor_pointer().hover(|s| s.bg(rgb(pal().hover_bg)))
				})
				.on_click(cx.listener(move |this, _, _, cx| {
					this.dispatch_tree(
						command_for_row(&action_row, RowGesture::Primary),
						cx,
					);
				}))
				.child(clip_text(row.name))
				.children(probe(log, marker_id))
				.into_any_element();
		}
		let rel = row.rel_path.clone();
		let click_row = row.clone();
		let check_row = row.clone();
		let menu_row = row.clone();
		let is_dir = row.is_dir;
		let row_id = if row.is_valid_utf8 {
			format!("tree-row:{rel}")
		} else {
			format!("tree-invalid:{}", row.id_suffix)
		};
		let chk_id = if row.is_valid_utf8 {
			format!("tree-chk:{rel}")
		} else {
			format!("tree-chk-invalid:{}", row.id_suffix)
		};
		let is_valid_utf8 = row.is_valid_utf8;

		// Git status as filename colour, like IntelliJ's Project view.
		let name_color = self.files.iter().find(|f| f.path == rel).map(|f| {
			if f.is_conflict {
				pal().git_conflict
			} else if f.source == SourceKind::Working {
				pal().git_untracked
			} else {
				change_style(f.change_type).1
			}
		});
		div()
			.id(SharedString::from(row_id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(indent))
			.pr(px(6.))
			.gap(px(5.))
			.when(is_valid_utf8, |d| d.cursor_pointer())
			.rounded(px(4.))
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.tree_cursor = ix;
					let items = this.work_row_menu(&menu_row);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.when(is_valid_utf8, |d| {
				d.on_click(cx.listener(move |this, _, _, cx| {
					this.tree_cursor = ix;
					this.dispatch_tree(
						command_for_row(&click_row, RowGesture::Primary),
						cx,
					);
				}))
			})
			.child(if is_dir {
				icon(
					if row.is_expanded {
						Icon::ChevronDown
					} else {
						Icon::ChevronRight
					},
					10.,
				)
				.into_any_element()
			} else {
				div().flex_shrink_0().w(px(10.)).into_any_element()
			})
			.child(
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.when(is_valid_utf8, |d| {
						d.cursor_pointer().on_click(cx.listener(
							move |this, _, _, cx| {
								cx.stop_propagation();
								this.tree_cursor = ix;
								this.dispatch_tree(
									command_for_row(
										&check_row,
										RowGesture::Toggle,
									),
									cx,
								);
							},
						))
					})
					.child(if is_valid_utf8 {
						checkbox(row.selected).into_any_element()
					} else {
						div()
							.text_size(px(9.))
							.text_color(rgb(pal().text_muted))
							.child("×")
							.into_any_element()
					})
					.children(probe(log, chk_id)),
			)
			.child(icon(
				if is_dir {
					if row.is_expanded {
						Icon::FolderOpen
					} else {
						Icon::Folder
					}
				} else {
					file_icon(&rel)
				},
				14.,
			))
			.child(
				self.speed_label(row.name.clone())
					.flex_1()
					.when_some(name_color, |d, c| d.text_color(rgb(c))),
			)
			.when(row.is_nested_repo, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.rounded(px(3.))
						.border_1()
						.border_color(rgb(pal().divider))
						.text_size(px(10.))
						.text_color(rgb(pal().text_muted))
						.child("repo"),
				)
			})
			.when(!is_valid_utf8, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.rounded(px(3.))
						.bg(rgb(pal().hover_bg))
						.text_size(px(10.))
						.text_color(rgb(pal().text_muted))
						.child("invalid UTF-8"),
				)
			})
			.children(probe(log, row_id))
			.into_any_element()
	}

	fn rev_tree_row(
		&self,
		ix: usize,
		row: RevRow,
		cursor: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let indent = 8.0 + row.depth as f32 * 14.0;
		if let Some(m) = &row.marker {
			return div()
				.w_full()
				.h(px(ROW_H))
				.pl(px(indent + 14.0))
				.flex()
				.items_center()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.child(clip_text(m.render(self.locale)))
				.into_any_element();
		}
		let is_dir = row.kind == snip_core::browser::TreeKind::Tree;
		let submodule = row.kind == snip_core::browser::TreeKind::Submodule;
		let is_file = row.kind == snip_core::browser::TreeKind::Blob;
		let path = row.path.clone();
		let tree_sha = self
			.rev_tree
			.as_ref()
			.map(|t| t.sha.clone())
			.unwrap_or_default();
		let is_basket_selected =
			is_file && self.is_rev_file_selected(&tree_sha, &path);
		let id = format!("rev-row:{path}");
		let chk_id = format!("rev-chk:{}:{}", tree_sha, path);
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(indent))
			.pr(px(6.))
			.gap(px(5.))
			.cursor_pointer()
			.rounded(px(4.))
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_mouse_down(MouseButton::Right, {
				let menu_path = path.clone();
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.tree_cursor = ix;
					let items = this.rev_row_menu(&menu_path, is_file);
					this.open_left_menu(items, ev, w, cx);
				})
			})
			.when(!submodule, |d| {
				let row_path = path.clone();
				d.on_click(cx.listener(move |this, _, _, cx| {
					this.tree_cursor = ix;
					this.rev_tree_click(&row_path, is_dir, cx);
				}))
			})
			.child(if is_dir {
				icon(
					if row.expanded {
						Icon::ChevronDown
					} else {
						Icon::ChevronRight
					},
					10.,
				)
				.into_any_element()
			} else {
				div().flex_shrink_0().w(px(10.)).into_any_element()
			})
			.child(if is_file {
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.cursor_pointer()
					.on_click(cx.listener({
						let chk_path = path.clone();
						let sha = tree_sha.clone();
						move |this, _, _, cx| {
							cx.stop_propagation();
							this.tree_cursor = ix;
							this.toggle_rev_file_selection(&sha, &chk_path, cx);
						}
					}))
					.child(checkbox(is_basket_selected))
					.children(probe(log, chk_id))
					.into_any_element()
			} else {
				div().flex_shrink_0().size(px(18.)).into_any_element()
			})
			.child(icon(
				if is_dir {
					if row.expanded {
						Icon::FolderOpen
					} else {
						Icon::Folder
					}
				} else if submodule {
					Icon::Project
				} else {
					file_icon(&path)
				},
				14.,
			))
			.child(self.speed_label(row.name.clone()).flex_1())
			.when(submodule, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.text_size(px(10.))
						.text_color(rgb(pal().text_muted))
						.child(t("submodule", self.locale)),
				)
			})
			.children(probe(log, id))
			.into_any_element()
	}

	/// Changes group node, like IntelliJ's commit tool window: chevron,
	/// tri-state group checkbox, name and count.
	fn change_header_row(
		&self,
		label_key: &'static str,
		count: usize,
		group_id: &'static str,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let collapsed = self.chrome.collapsed_groups.contains(&group_id);
		let cursor = row_idx == self.selected_list_row;
		let id = format!("change-header:{group_id}");
		let chk_id = format!("change-group-chk:{group_id}");
		let toggle_id = format!("change-group-toggle:{group_id}");
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(4.))
			.pr(px(6.))
			.gap(px(2.))
			.rounded(px(4.))
			.cursor_pointer()
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
				this.selected_list_row = row_idx;
				if ev.click_count() >= 2 {
					this.toggle_group_collapsed(group_id, cx);
				}
				cx.notify();
			}))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.selected_list_row = row_idx;
					let items = this.change_group_menu(group_id);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.child(
				div()
					.id(SharedString::from(toggle_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(16.))
					.flex()
					.items_center()
					.justify_center()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.selected_list_row = row_idx;
						this.toggle_group_collapsed(group_id, cx);
					}))
					.child(icon(
						if collapsed {
							Icon::ChevronRight
						} else {
							Icon::ChevronDown
						},
						10.,
					))
					.children(probe(log, toggle_id)),
			)
			.child(
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.toggle_change_group(group_id, cx);
					}))
					.child(tri_checkbox(self.group_state(group_id)))
					.children(probe(log, chk_id)),
			)
			.child(
				clip_text(t(label_key, loc))
					.ml(px(4.))
					.font_weight(FontWeight::SEMIBOLD),
			)
			.child(
				div()
					.flex_shrink_0()
					.ml(px(6.))
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(pal().text_muted))
					.child(tf("n_files", loc, &[count])),
			)
			.children(probe(log, id))
			.into_any_element()
	}

	fn change_row(
		&self,
		ix: usize,
		row_idx: usize,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let Some(item) = self.files.get(ix) else {
			return div().into_any_element();
		};
		let (letter, change_color) = change_style(item.change_type);
		// IntelliJ conveys status by filename colour; the group header above
		// already says staged / unstaged / untracked / conflicted.
		let color = if item.is_conflict {
			pal().git_conflict
		} else if item.source == SourceKind::Working {
			pal().git_untracked
		} else {
			change_color
		};
		let deleted = item.change_type == Some(ChangeType::Deleted);
		let path = item.path.clone();
		// Untracked directories come as `dir/`; label them by their own name.
		let is_dir = path.ends_with('/');
		let (dir, name) = match path.trim_end_matches('/').rsplit_once('/') {
			Some((d, n)) => (d.to_string(), n.to_string()),
			None => (String::new(), path.trim_end_matches('/').to_string()),
		};
		let cursor = row_idx == self.selected_list_row;
		let source_str = if item.is_conflict {
			"conflicted"
		} else {
			match item.source {
				SourceKind::Staged => "staged",
				SourceKind::Unstaged => "unstaged",
				SourceKind::Working => "untracked",
				_ => "working",
			}
		};
		let row_id = format!("change-row:{path}");
		let src_row_id = format!("change-row:{source_str}:{path}");
		// Like the project tree, a name that is not UTF-8 cannot be selected.
		let checkable = item.is_valid_utf8();
		let (chk_id, src_chk_id) = if checkable {
			(
				format!("change-chk:{path}"),
				format!("change-chk:{source_str}:{path}"),
			)
		} else {
			(
				format!("change-chk-invalid:{ix}"),
				format!("change-chk-invalid:{source_str}:{ix}"),
			)
		};
		let tooltip = if checkable {
			format!("{path}  ({letter})")
		} else {
			format!("{path}  ({letter})\n{}", t("change_not_utf8", self.locale))
		};
		let path_click = path.clone();
		let item_source = item.source.clone();

		div()
			.id(SharedString::from(format!("change-row-el:{ix}:{path}")))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(20.))
			.pr(px(6.))
			.gap(px(6.))
			.cursor_pointer()
			.rounded(px(4.))
			.when(cursor, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!cursor, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
			.on_mouse_down(
				MouseButton::Right,
				cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
					this.selected_list_row = row_idx;
					let items = this.change_row_menu(ix);
					this.open_left_menu(items, ev, w, cx);
				}),
			)
			.when(self.chrome.menu.is_none(), |d| d.tooltip(tip(tooltip)))
			.on_click(cx.listener(move |this, _, _, cx| {
				this.selected_list_row = row_idx;
				this.select_file_with_source(
					&path_click,
					item_source.clone(),
					cx,
				);
			}))
			.child(
				div()
					.id(SharedString::from(chk_id.clone()))
					.relative()
					.flex_shrink_0()
					.size(px(18.))
					.flex()
					.items_center()
					.justify_center()
					.when(checkable, |d| {
						d.on_click(cx.listener(move |this, _, _, cx| {
							cx.stop_propagation();
							this.selected_list_row = row_idx;
							this.toggle_file(ix, cx);
						}))
					})
					.child(if checkable {
						checkbox(item.selected).into_any_element()
					} else {
						div()
							.text_size(px(9.))
							.text_color(rgb(pal().text_muted))
							.child("×")
							.into_any_element()
					})
					.children(probe(log, chk_id))
					.children(probe(log, src_chk_id)),
			)
			.child(icon(
				if is_dir {
					Icon::Folder
				} else {
					file_icon(&path)
				},
				14.,
			))
			.child(
				self.speed_label(name)
					.flex_shrink_0()
					.max_w(gpui::relative(0.7))
					.text_color(rgb(color))
					.when(deleted, |d| d.line_through()),
			)
			.child(
				fill_text(dir)
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(pal().text_muted)),
			)
			.children(probe(log, row_id))
			.children(probe(log, src_row_id))
			.into_any_element()
	}

	// ───────────────────────── editor ─────────────────────────

	/// Editor tab row. An empty `label` means nothing is open: the row stays
	/// (stable layout) but shows no tab, like IntelliJ's empty editor.
	/// Editor tab bar with one Islands pill: file-type icon, the name (italic
	/// while it is a preview tab) and a ✕ when the tab can be closed.
	fn tab_strip(
		&self,
		label: String,
		ic: Icon,
		preview: bool,
		close: Option<fn(&mut Self, &mut Context<Self>)>,
		cx: &mut Context<Self>,
	) -> Div {
		let loc = self.locale;
		let log = &self.probes;
		div()
			.flex()
			.flex_row()
			.flex_shrink_0()
			.h(px(32.))
			.px(px(4.))
			.items_center()
			.border_b_1()
			.border_color(rgb(pal().divider))
			.on_mouse_down(MouseButton::Right, |_, w, cx| {
				w.dispatch_action(Box::new(crate::OpenTabMenu), cx)
			})
			.when(!label.is_empty(), |d| {
				d.child(
					div()
						.id("editor-tab")
						.relative()
						.flex()
						.flex_row()
						.items_center()
						.gap(px(6.))
						.min_w_0()
						.max_w(px(360.))
						.h(px(24.))
						.pl(px(8.))
						.pr(px(if close.is_some() { 4. } else { 10. }))
						// Islands selected tab: filled rounded pill.
						.rounded(px(6.))
						.bg(rgb(pal().range_bg))
						.text_size(px(UI_TEXT))
						.text_color(rgb(pal().text))
						.when(preview, |d| {
							d.tooltip(tip(t("tip_preview_tab", loc)))
								// Double-click keeps the tab, as in IntelliJ.
								.on_click(cx.listener(
									|this, ev: &gpui::ClickEvent, _, cx| {
										if ev.click_count() >= 2
											&& !this.reader.pinned
										{
											this.reader.pinned = true;
											app_log!("[APP:TAB_PINNED]");
											cx.notify();
										}
									},
								))
						})
						.child(icon(ic, 14.))
						.child(clip_text(label).when(preview, |d| d.italic()))
						.when_some(close, |d, close| {
							d.child(
								icon_button(
									"btn-tab-close",
									Icon::Close,
									t("tip_close_tab", loc),
									true,
									38,
								)
								.size(px(16.))
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									close(this, cx)
								}))
								.children(probe(log, "btn-tab-close")),
							)
						})
						.children(probe(log, "editor-tab")),
				)
			})
	}
	/// IntelliJ-style empty editor: the real shortcuts, centered and muted.
	fn editor_empty_hints(&self) -> Div {
		let loc = self.locale;
		let ctrl = if cfg!(target_os = "macos") {
			"⌘"
		} else {
			"Ctrl+"
		};
		let hints: [(&str, String); 7] = [
			(t("project", loc), "Alt+1".into()),
			(t("changes", loc), "Alt+0".into()),
			(t("git_log", loc), "Alt+9".into()),
			(t("hint_switch_repo", loc), "Alt+Shift+R".into()),
			(t("hint_switch_ref", loc), "Alt+Shift+B".into()),
			(t("paste_tab", loc), format!("{ctrl}V")),
			(t("btn_refresh", loc), format!("{ctrl}R")),
		];
		div()
			.flex()
			.flex_col()
			.flex_1()
			.min_h_0()
			.items_center()
			.justify_center()
			.overflow_hidden()
			.child(
				div()
					.flex()
					.flex_col()
					.gap(px(10.))
					.text_size(px(UI_TEXT))
					.children(hints.into_iter().map(|(label, keys)| {
						div()
							.flex()
							.flex_row()
							.gap(px(10.))
							.child(
								div()
									.w(px(140.))
									.flex()
									.justify_end()
									.text_color(rgb(pal().text_muted))
									.child(label.to_string()),
							)
							.child(
								div().text_color(rgb(pal().link)).child(keys),
							)
					})),
			)
	}

	/// Breadcrumb text and source badge: says exactly which version is shown.
	fn source_labels(&self) -> (String, String, String) {
		let loc = self.locale;
		let repo = self.repo().map(|r| r.name.clone()).unwrap_or_default();
		let Some(p) = &self.preview else {
			let tab = match (&self.selected_commit, &self.compare) {
				(_, Some((a, b))) => format!("{}..{}", short(a), short(b)),
				(Some(s), None) => format!("commit {}", short(s)),
				_ => t("no_file", loc).to_string(),
			};
			return (tab, repo, String::new());
		};
		let path = p.path.clone().unwrap_or_default();
		let name = path.rsplit('/').next().unwrap_or(&path).to_string();
		let segs = path.replace('/', " › ");
		match &p.source {
			PreviewSource::WorkingFile => (
				name,
				format!("{repo} › {segs}"),
				t("src_working_file", loc).into(),
			),
			PreviewSource::WorkingChanges => (
				name,
				format!("{repo} › {segs}"),
				t("src_working_diff", loc).into(),
			),
			PreviewSource::StagedChanges => (
				name,
				format!("{repo} › {segs}"),
				t("src_staged_diff", loc).into(),
			),
			PreviewSource::UnstagedChanges => (
				name,
				format!("{repo} › {segs}"),
				t("src_unstaged_diff", loc).into(),
			),
			PreviewSource::CommitDiff { sha } => {
				let parent_note = match self
					.commits
					.iter()
					.find(|c| &c.sha == sha)
					.map(|c| c.parents.len())
				{
					Some(0) => t("src_vs_empty_tree", loc).to_string(),
					Some(n) if n > 1 => {
						tf("src_vs_first_parent_merge", loc, &[&n])
					}
					_ => t("src_vs_first_parent", loc).to_string(),
				};
				(
					format!("{name} @{}", short(sha)),
					format!("{repo} › commit {} › {segs}", short(sha)),
					format!("commit {} · {parent_note}", short(sha)),
				)
			}
			PreviewSource::CommitFile { sha } => (
				format!("{name} @{}", short(sha)),
				format!("{repo} › @{} › {segs}", short(sha)),
				tf("src_commit_file", loc, &[&short(sha)]),
			),
			PreviewSource::Compare { from, to } => (
				format!("{name} {}..{}", short(from), short(to)),
				format!("{repo} › {}..{} › {segs}", short(from), short(to)),
				tf("src_compare", loc, &[&short(from), &short(to)]),
			),
			PreviewSource::PasteItem => (name, segs, String::new()),
		}
	}

	/// IntelliJ's search bar: an inline field with Match Case / Regex
	/// toggles, match count, previous / next, go-to-line and close. Hidden
	/// until Ctrl+F or Ctrl+G opens it; Esc closes it.
	fn find_bar(&self, cx: &mut Context<Self>) -> Stateful<Div> {
		let loc = self.locale;
		let log = &self.probes;
		let n_matches = self.reader.matches.len();
		let opts = self.reader.find_opts;
		let find_label = match self.reader.current {
			Some(c) if n_matches > 0 => {
				let more = if n_matches >= crate::reader::MAX_MATCHES {
					"+"
				} else {
					""
				};
				format!("{}/{}{}", c + 1, n_matches, more)
			}
			_ => "0/0".into(),
		};
		let toggle = |id: &'static str, ic: Icon, tip_key: &str, on: bool| {
			icon_button(id, ic, t(tip_key, loc), true, 34)
				.size(px(20.))
				.when(on, |d| {
					d.bg(rgb(pal().range_bg)).border_color(rgb(pal().accent))
				})
				.children(probe(log, id))
		};
		let field = div()
			.flex()
			.flex_row()
			.items_center()
			.gap(px(4.))
			.flex_1()
			.min_w(px(160.))
			.max_w(px(420.))
			.h(px(24.))
			.child(icon(Icon::Search, 14.))
			.child(
				div()
					.id("find-input")
					.relative()
					.flex_1()
					.min_w(px(60.))
					.child(self.find_input.clone())
					.children(probe(log, "find-input")),
			)
			.child(
				toggle(
					"btn-find-case",
					Icon::MatchCase,
					"tip_match_case",
					opts.match_case,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.toggle_find_option(false, cx)
				})),
			)
			.child(
				toggle("btn-find-regex", Icon::Regex, "tip_regex", opts.regex)
					.on_click(cx.listener(|this, _, _, cx| {
						this.toggle_find_option(true, cx)
					})),
			);
		div()
			.id("find-bar")
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(32.))
			.px(px(8.))
			.gap(px(6.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.bg(rgb(pal().panel_bg))
			.text_size(px(SMALL_TEXT))
			.child(field)
			.child(
				div()
					.id("find-count")
					.flex_shrink_0()
					.min_w(px(40.))
					.text_color(rgb(
						if self.reader.find_invalid
							|| (n_matches == 0
								&& !self.reader.find_query.is_empty())
						{
							pal().error
						} else {
							pal().text_muted
						},
					))
					.child(find_label),
			)
			.child(
				icon_button(
					"btn-find-prev",
					Icon::ArrowUp,
					t("tip_find_prev", loc),
					n_matches > 0,
					32,
				)
				.when(n_matches > 0, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| this.find_step(false, cx)),
					)
				})
				.children(probe(log, "btn-find-prev")),
			)
			.child(
				icon_button(
					"btn-find-next",
					Icon::ArrowDown,
					t("tip_find_next", loc),
					n_matches > 0,
					33,
				)
				.when(n_matches > 0, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| this.find_step(true, cx)),
					)
				})
				.children(probe(log, "btn-find-next")),
			)
			.child(toolbar_divider())
			.child(
				div()
					.id("goto-field")
					.flex()
					.flex_row()
					.items_center()
					.flex_shrink_0()
					.gap(px(4.))
					.h(px(24.))
					.px(px(6.))
					.rounded(px(4.))
					.border_1()
					.border_color(rgb(pal().button_border))
					.bg(rgb(pal().button_bg))
					.tooltip(tip(t("tip_goto", loc)))
					.child(icon(Icon::GoToLine, 14.))
					.child(
						div()
							.id("goto-input")
							.relative()
							.w(px(64.))
							.child(self.goto_input.clone())
							.children(probe(log, "goto-input")),
					),
			)
			.child(div().flex_1())
			.child(
				icon_button(
					"btn-find-close",
					Icon::Close,
					t("tip_find_close", loc),
					true,
					39,
				)
				.on_click(cx.listener(|this, _, _, cx| this.close_find(cx)))
				.children(probe(log, "btn-find-close")),
			)
			.children(probe(log, "find-bar"))
	}

	fn render_editor(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let (tab_label, crumbs, badge) = self.source_labels();
		let tab_icon = file_icon(&tab_label);
		let is_diff = self.preview.as_ref().is_some_and(|p| p.is_diff);
		let side = self.reader.diff_mode == DiffMode::SideBySide;
		let has_folds = self.preview.as_ref().is_some_and(|p| {
			p.diff.as_ref().is_some_and(|d| {
				d.trailing || d.shown.iter().any(|&l| d.inline[l].fold > 0)
			})
		});
		let has_preview = self.can_copy_preview();
		let preview_notice = self.preview.as_ref().and_then(|p| {
			if p.notice.is_some() {
				Some(tf(
					"truncated_notice",
					loc,
					&[
						&crate::reader::MAX_PREVIEW_LINES,
						&(crate::reader::MAX_PREVIEW_BYTES / 1024),
					],
				))
			} else if p.line(p.widest).len()
				> crate::reader::MAX_RENDER_LINE_BYTES
			{
				Some(t("line_truncated_notice", loc).to_string())
			} else {
				None
			}
		});

		// IntelliJ editor / diff toolbar: icon-only ghost buttons, the
		// label in the tooltip.
		let toolbar = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.gap(px(2.))
			.when(is_diff, |d| {
				d.child(
					icon_button(
						"btn-prev-diff",
						Icon::PrevDiff,
						t("tip_prev_diff", loc),
						true,
						35,
					)
					.on_click(cx.listener(|this, _, _, cx| this.prev_diff(cx)))
					.children(probe(log, "btn-prev-diff")),
				)
				.child(
					icon_button(
						"btn-next-diff",
						Icon::NextDiff,
						t("tip_next_diff", loc),
						true,
						35,
					)
					.on_click(cx.listener(|this, _, _, cx| this.next_diff(cx)))
					.children(probe(log, "btn-next-diff")),
				)
				.when(has_folds, |d| {
					d.child(
						icon_button(
							"btn-expand-folds",
							Icon::ExpandAll,
							t("tip_expand_folds", loc),
							!self.reader.expanding,
							35,
						)
						.when(!self.reader.expanding, |b| {
							b.on_click(cx.listener(|this, _, _, cx| {
								this.expand_folds(None, cx)
							}))
						})
						.children(probe(log, "btn-expand-folds")),
					)
				})
				.child(
					// Shows the current viewer; clicking switches to the other.
					icon_button(
						"btn-diff-mode",
						if side {
							Icon::SideBySide
						} else {
							Icon::Unified
						},
						t(
							if side {
								"tip_diff_unified"
							} else {
								"tip_diff_side"
							},
							loc,
						),
						true,
						35,
					)
					.on_click(
						cx.listener(|this, _, _, cx| this.toggle_diff_mode(cx)),
					)
					.children(probe(log, "btn-diff-mode")),
				)
				.child(toolbar_divider())
			})
			.when(
				self.selected_commit.is_some() && self.compare.is_none(),
				|d| {
					d.child(
						icon_button(
							"btn-browse-tree",
							Icon::Project,
							format!(
								"{} — {}",
								t("btn_browse_tree", loc),
								t("tip_browse_tree", loc)
							),
							true,
							36,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.browse_commit_tree(cx)
						}))
						// The loading commit header changes height when files arrive.
						// Only expose bounds for the completed revision's layout.
						.children(
							self.selected_commit
								.as_ref()
								.filter(|_| {
									log.is_some() && !self.preview_loading
								})
								.and_then(|sha| {
									probe(log, format!("btn-browse-tree:{sha}"))
								}),
						),
					)
				},
			)
			.child(
				icon_button(
					"btn-copy-view",
					Icon::Copy,
					format!(
						"{} — {}",
						t("btn_copy_view", loc),
						t("tip_copy_view", loc)
					),
					has_preview,
					37,
				)
				.when(has_preview, |b| {
					b.on_click(cx.listener(|this, _, _, cx| {
						this.copy_current_preview_content(cx)
					}))
				})
				.children(probe(log, "btn-copy-view")),
			);

		let body: AnyElement = if self.preview_loading && self.preview.is_none()
		{
			div()
				.p(px(12.))
				.text_color(rgb(pal().text_muted))
				.child(t("status_loading", loc))
				.into_any_element()
		} else if let Some(err) = &self.preview_error {
			div()
				.id("editor-error")
				.relative()
				.flex()
				.gap(px(6.))
				.p(px(12.))
				.text_color(rgb(pal().error))
				.child(icon(Icon::Warning, 14.))
				.child(div().flex_1().child(err.render(loc)))
				.children(probe(log, "editor-error"))
				.into_any_element()
		} else if self.preview.is_some() {
			div()
				.id("reader")
				.relative()
				.key_context("Reader")
				.track_focus(&self.reader_focus)
				.on_action(cx.listener(|this, _: &ReaderCopy, _, cx| {
					if !this.copy_reader_selection(cx) {
						this.copy_selection_to_clipboard(cx);
					}
				}))
				.on_action(cx.listener(|this, _: &ReaderSelectAll, _, cx| {
					this.select_all_text(cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderUp, _, cx| {
					this.move_cursor_line(-1, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderDown, _, cx| {
					this.move_cursor_line(1, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderPageUp, _, cx| {
					this.move_cursor_line(-30, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderPageDown, _, cx| {
					this.move_cursor_line(30, cx)
				}))
				.on_action(cx.listener(|this, _: &ReaderClear, _, cx| {
					// Esc clears the selection, then closes the find bar.
					if this.reader.selection().is_some() {
						this.reader.anchor = None;
						this.reader.head = None;
						cx.notify();
					} else {
						this.close_find(cx);
					}
				}))
				.flex()
				.flex_col()
				.flex_1()
				// Keep the notice and at least one code row visible at compact sizes.
				.min_h(px(48.))
				.when_some(preview_notice, |d, notice| {
					d.child(
						div()
							.id("reader-truncated-notice")
							.relative()
							.children(probe(log, "reader-truncated-notice"))
							.flex_shrink_0()
							.px(px(12.))
							.py(px(2.))
							.bg(rgb(pal().panel_bg))
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(pal().warning))
							.child(notice),
					)
				})
				.child(self.render_code_view(false, cx))
				.children(probe(log, "reader"))
				.into_any_element()
		} else {
			self.editor_empty_hints().into_any_element()
		};

		let showing = self.preview.is_some()
			|| self.selected_commit.is_some()
			|| self.compare.is_some();
		// Same model method as the tab menu's Close.
		let closable =
			self.open_tab_count() > 0 || self.preview_error.is_some();
		div()
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.h_full()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(self.tab_strip(
				if showing { tab_label } else { String::new() },
				tab_icon,
				self.preview.is_some() && !self.reader.pinned,
				closable.then_some(
					(|this, cx| this.close_tab(0, cx))
						as fn(&mut Self, &mut Context<Self>),
				),
				cx,
			))
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.flex_shrink_0()
					.h(px(28.))
					.pl(px(12.))
					.pr(px(6.))
					.gap(px(8.))
					.border_b_1()
					.border_color(rgb(pal().divider))
					.text_size(px(SMALL_TEXT))
					.child(
						div()
							.id("breadcrumb")
							.relative()
							.flex_1()
							.min_w_0()
							.overflow_hidden()
							.line_clamp(1)
							.text_ellipsis()
							.text_color(rgb(pal().text_muted))
							.tooltip(tip(crumbs.clone()))
							.child(crumbs)
							.children(probe(log, "breadcrumb")),
					)
					.when(!badge.is_empty(), |d| {
						d.child(
							div()
								.id("source-badge")
								.relative()
								.min_w_0()
								.max_w(px(300.))
								.px(px(6.))
								.rounded(px(3.))
								.bg(rgb(pal().ref_bg))
								.text_color(rgb(pal().text))
								.overflow_hidden()
								.line_clamp(1)
								.text_ellipsis()
								.tooltip(tip(badge.clone()))
								.child(badge)
								.children(probe(log, "source-badge")),
						)
					})
					.child(toolbar),
			)
			.when(self.reader.find_open, |d| d.child(self.find_bar(cx)))
			.child(body)
			.into_any_element()
	}

	// ───────────────────────── paste ─────────────────────────

	fn disambiguate_candidate_labels(candidates: &[PathBuf]) -> Vec<String> {
		let orig_basenames: Vec<String> = candidates
			.iter()
			.map(|p| {
				p.file_name()
					.map(|n| n.to_string_lossy().into_owned())
					.unwrap_or_else(|| p.display().to_string())
			})
			.collect();
		let mut labels = orig_basenames.clone();
		let mut has_dups = false;
		let mut dup_indices = Vec::new();
		for i in 0..candidates.len() {
			let is_dup = orig_basenames
				.iter()
				.enumerate()
				.any(|(j, l)| j != i && *l == orig_basenames[i]);
			if is_dup {
				has_dups = true;
				dup_indices.push(i);
			}
		}
		if has_dups {
			for &i in &dup_indices {
				let comps: Vec<_> = candidates[i]
					.components()
					.map(|c| c.as_os_str().to_string_lossy().into_owned())
					.collect();
				if comps.len() >= 2 {
					labels[i] = format!(
						"{}/{}",
						comps[comps.len() - 2],
						comps[comps.len() - 1]
					);
				} else {
					labels[i] = candidates[i].display().to_string();
				}
			}
			for i in 0..candidates.len() {
				let still_dup = labels
					.iter()
					.enumerate()
					.any(|(j, l)| j != i && *l == labels[i]);
				if still_dup {
					labels[i] = candidates[i].display().to_string();
				}
			}
		}
		labels
	}

	/// Destination, Apply and Cancel. Apply stays locked while a read-only
	/// preview or remap is still running, so an older plan cannot be written.
	fn paste_action_bar(
		&self,
		dest: String,
		applying: bool,
		mapping_ready: bool,
		executable: bool,
		cx: &mut Context<Self>,
	) -> Div {
		let loc = self.locale;
		let log = &self.probes;
		let loading = self.paste_loading;
		let can_apply = !applying && executable && !loading;
		div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(38.))
			.px(px(12.))
			.gap(px(8.))
			.bg(rgb(pal().panel_bg))
			.border_b_1()
			.border_color(rgb(pal().border))
			.child(
				div()
					.id("paste-dest")
					.flex()
					.flex_row()
					.items_center()
					.gap(px(6.))
					.flex_1()
					.min_w_0()
					.tooltip(tip(dest.clone()))
					.child(
						div()
							.flex_shrink_0()
							.text_color(rgb(pal().text_muted))
							.child(format!("{}:", t("destination_label", loc))),
					)
					.child(fill_text(dest)),
			)
			.child(
				button("btn-apply", "", Btn::Primary, can_apply, 50)
					.pl(px(6.))
					.child(icon_tinted(
						Icon::Apply,
						14.,
						if can_apply {
							pal().accent_text
						} else {
							pal().text_disabled
						},
					))
					.child(if applying {
						t("applying", loc)
					} else {
						t("apply", loc)
					})
					.when(loading, |b| {
						b.tooltip(tip(t("paste_loading_refused", loc)))
					})
					.when(!loading && !mapping_ready, |b| {
						b.tooltip(tip(t("mapping_required", loc)))
					})
					.when(can_apply, |b| {
						b.on_click(cx.listener(|this, _, _, cx| {
							this.apply_paste_restore(cx)
						}))
					})
					.children(probe(log, "btn-apply")),
			)
			.child(
				// The write cannot be interrupted, so Cancel is locked until it
				// finishes rather than pretending to cancel it.
				button("btn-cancel", "", Btn::Default, !applying, 51)
					.pl(px(6.))
					.child(icon(Icon::Cancel, 14.))
					.child(t("cancel", loc))
					.when(!applying, |b| {
						b.on_click(cx.listener(|this, _, _, cx| {
							this.cancel_paste_preview(cx)
						}))
					})
					.when(applying, |b| {
						b.tooltip(tip(t("paste_busy_refused", loc)))
					})
					.children(probe(log, "btn-cancel")),
			)
	}

	fn paste_loading_note(&self) -> Stateful<Div> {
		div()
			.id("paste-loading")
			.relative()
			.flex_shrink_0()
			.px(px(12.))
			.py(px(6.))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			.child(t("paste_loading", self.locale))
			.children(probe(&self.probes, "paste-loading"))
	}

	/// First preview still being read: no plan exists, so only Cancel works.
	fn render_paste_loading(&self, cx: &mut Context<Self>) -> AnyElement {
		let dest = self.current_restore_destination().display().to_string();
		div()
			.id("paste-panel")
			.key_context("PastePanel")
			.track_focus(&self.paste_focus)
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.min_h_0()
			.h_full()
			.overflow_hidden()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(self.tab_strip(
				t("paste_tab", self.locale).to_string(),
				Icon::Paste,
				false,
				Some(|this, cx| this.close_tab(0, cx)),
				cx,
			))
			.child(self.paste_action_bar(dest, false, false, false, cx))
			.child(self.paste_loading_note())
			.into_any_element()
	}

	fn render_paste(
		&self,
		plan: &PastePreviewPlan,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let mut skips = plan.plan.skipped_operations.len();
		let (mut creates, mut overwrites, mut existing, mut deletes) =
			(0, 0, 0, 0);
		for it in &plan.items {
			if it.dest_exists && !it.is_delete {
				existing += 1;
			}
			match paste_op(it).0 {
				"op_create" => creates += 1,
				"op_overwrite" => overwrites += 1,
				"op_delete" => deletes += 1,
				_ => skips += 1,
			}
		}
		let dest = plan.destination.display().to_string();
		let applying = plan.is_applying;
		let mapping_ready = plan.mapping_ready();
		let selected = plan.items.get(plan.selected_item_idx);

		let loading = self.paste_loading;
		let action_bar = self.paste_action_bar(
			dest,
			applying,
			mapping_ready,
			plan.executable(),
			cx,
		);
		let tab = self.tab_strip(
			format!("{} ({})", t("paste_tab", loc), plan.items.len()),
			Icon::Paste,
			false,
			(!applying).then_some(
				(|this, cx| this.close_tab(0, cx))
					as fn(&mut Self, &mut Context<Self>),
			),
			cx,
		);

		let summary = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(24.))
			.px(px(12.))
			.gap(px(12.))
			.text_size(px(SMALL_TEXT))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().git_added))
					.child(format!("{} {creates}", t("op_create", loc))),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().git_modified))
					.child(format!(
						"{} {overwrites}/{existing}",
						t("op_overwrite", loc)
					)),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().git_deleted))
					.child(format!("{} {deletes}", t("op_delete", loc))),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(pal().text_muted))
					.child(format!("{} {skips}", t("op_skip", loc))),
			)
			.child(
				fill_text(t("paste_keys", loc))
					.text_color(rgb(pal().text_disabled)),
			);

		// IntelliJ "Apply Patch": a change tree grouped by destination root
		// and directory; the file name's colour says what happens to it.
		let node_row = |depth: usize| {
			div()
				.flex()
				.flex_row()
				.items_center()
				.flex_shrink_0()
				.h(px(24.))
				.pl(px(8. + 18. * depth as f32))
				.pr(px(8.))
				.gap(px(6.))
		};
		let rows = plan.tree_rows().into_iter().map(|node| {
			let (ix, depth) = match node {
				PasteNode::Root(ref name, n) | PasteNode::Dir(ref name, n) => {
					let root = matches!(node, PasteNode::Root(..));
					let name = name.clone();
					return node_row(usize::from(!root))
						.child(icon(Icon::Folder, 14.))
						.child(clip_text(name).flex_shrink().when(root, |d| {
							d.font_weight(FontWeight::SEMIBOLD)
						}))
						.child(
							div()
								.flex_shrink_0()
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(n.to_string()),
						)
						.into_any_element();
				}
				PasteNode::File(ix, depth) => (ix, depth),
			};
			let item = &plan.items[ix];
			let (_, name_color, _) = paste_op(item);
			let is_sel = ix == plan.selected_item_idx;
			let path = item.path.clone();
			let row_id = format!("paste-row:{path}");
			let inc_id = format!("paste-include:{path}");
			let ow_id = format!("paste-overwrite:{path}");
			let can_overwrite = item.dest_exists && !item.is_delete;
			let ow_on = item.overwrite_allowed;
			let (_, name) = split_dir(&path);
			node_row(depth)
				.id(SharedString::from(row_id.clone()))
				.relative()
				.cursor_pointer()
				.when(is_sel, |d| d.bg(rgb(pal().selection_bg)))
				.when(!is_sel, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.tooltip(tip(format!("{path}\n→ {}", item.dest_path.display())))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.select_paste_item(ix, cx)
				}))
				.child(
					div()
						.id(SharedString::from(inc_id.clone()))
						.relative()
						.flex_shrink_0()
						.size(px(16.))
						.flex()
						.items_center()
						.justify_center()
						.when(applying, |d| {
							d.opacity(0.4)
								.tooltip(tip(t("paste_busy_refused", loc)))
						})
						// Still routed to the model while applying so the refusal
						// is explicit (it logs and explains).
						.on_click(cx.listener(move |this, _, _, cx| {
							cx.stop_propagation();
							this.toggle_paste_selected(ix, cx);
						}))
						.child(checkbox(item.selected))
						.children(probe(log, inc_id)),
				)
				.child(icon(file_icon(name), 14.))
				.child(
					clip_text(name.to_string())
						.flex_1()
						.text_color(rgb(name_color)),
				)
				.child(
					div()
						.flex_shrink_0()
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(if item.is_delete {
							String::new()
						} else {
							format!("{} B", item.bytes)
						}),
				)
				// Fixed column so the toggles line up whether or not a row
				// can be overwritten.
				.child(div().flex_shrink_0().w(px(88.)).when(
					can_overwrite,
					|d| {
						d.child(
							div()
								.id(SharedString::from(ow_id.clone()))
								.relative()
								.flex()
								.flex_row()
								.items_center()
								.gap(px(5.))
								.h(px(20.))
								.px(px(4.))
								.rounded(px(3.))
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(if ow_on {
									pal().text
								} else {
									pal().text_muted
								}))
								.when(!applying, |d| {
									d.cursor_pointer()
										.hover(|s| s.bg(rgb(pal().hover_bg)))
								})
								.when(applying, |d| {
									d.opacity(0.4).tooltip(tip(t(
										"paste_busy_refused",
										loc,
									)))
								})
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									this.toggle_paste_overwrite(ix, cx);
								}))
								.child(checkbox(ow_on))
								.child(clip_text(t("overwrite_toggle", loc)))
								.children(probe(log, ow_id)),
						)
					},
				))
				.children(probe(log, row_id))
				.into_any_element()
		});

		let (detail_title, reason) = match selected {
			Some(it) => (
				format!("{} → {}", it.path, it.dest_path.display()),
				t(paste_op(it).2, loc),
			),
			None => (String::new(), ""),
		};

		let mappings = (!plan.prefix_choices.is_empty()).then(|| {
			div()
				.id("paste-mappings")
				.relative()
				.flex()
				.flex_col()
				.flex_shrink_0()
				.mx(px(8.))
				.my(px(6.))
				.rounded(px(4.))
				.border_1()
				.border_color(rgb(pal().divider))
				.bg(rgb(pal().panel_bg))
				.children(plan.prefix_choices.iter().enumerate().map(
					|(n, choice)| {
						let prefix = choice.prefix.clone();
						let kept = choice.keep_relative;
						let target = if kept {
							format!(
								"{}/{}",
								plan.destination.display(),
								choice.prefix
							)
						} else {
							choice
								.destination
								.as_ref()
								.map(|p| p.display().to_string())
								.unwrap_or_else(|| {
									t("mapping_unresolved", loc).into()
								})
						};
						let resolved = kept || choice.destination.is_some();
						let target_id = format!("paste-map-target:{prefix}");
						let row_id = format!("paste-map:{prefix}");
						let labels = Self::disambiguate_candidate_labels(
							&choice.candidates,
						);
						let keep_id = format!("paste-map-keep:{prefix}");
						let prefix_keep = prefix.clone();
						let keep_btn = button(
							SharedString::from(keep_id.clone()),
							t("mapping_keep", loc),
							if kept { Btn::Primary } else { Btn::Default },
							!applying,
							69,
						)
						.tooltip(tip(format!(
							"{}/{}",
							plan.destination.display(),
							prefix
						)))
						.when(!applying, |b| {
							b.on_click(cx.listener(move |this, _, _, cx| {
								this.choose_paste_keep(&prefix_keep, cx);
							}))
						})
						.children(probe(log, keep_id));
						let picks = choice.candidates.iter().enumerate().map(
							|(idx, path)| {
								let picked =
									choice.destination.as_ref() == Some(path);
								let id =
									format!("paste-map-pick:{prefix}:{idx}");
								let full = path.display().to_string();
								let label = labels
									.get(idx)
									.cloned()
									.unwrap_or_else(|| full.clone());
								let prefix_click = prefix.clone();
								button(
									SharedString::from(id.clone()),
									label,
									if picked {
										Btn::Primary
									} else {
										Btn::Default
									},
									!applying,
									70 + idx as isize,
								)
								.tooltip(tip(full))
								.when(!applying, |b| {
									b.on_click(cx.listener(
										move |this, _, _, cx| {
											this.choose_paste_prefix(
												&prefix_click,
												idx,
												cx,
											);
										},
									))
								})
								.children(probe(log, id))
							},
						);
						// One wrapping line: source, choices, then the resolved
						// target filling what is left (wraps below when narrow).
						div()
							.id(SharedString::from(row_id.clone()))
							.relative()
							.flex()
							.flex_row()
							.flex_wrap()
							.items_center()
							.gap_x(px(6.))
							.gap_y(px(4.))
							.px(px(8.))
							.py(px(5.))
							.text_size(px(SMALL_TEXT))
							.when(n > 0, |d| {
								d.border_t_1().border_color(rgb(pal().divider))
							})
							.child(
								clip_text(tf(
									"mapping_prefix",
									loc,
									&[&prefix],
								))
								.flex_shrink_0()
								.max_w(px(240.))
								.font_weight(FontWeight::SEMIBOLD),
							)
							.child(keep_btn)
							.children(picks)
							.child(
								div()
									.id(SharedString::from(target_id.clone()))
									.relative()
									.flex_1()
									.min_w(px(180.))
									.overflow_hidden()
									.line_clamp(1)
									.text_ellipsis()
									.text_color(rgb(if resolved {
										pal().text_muted
									} else {
										pal().warning
									}))
									.tooltip(tip(target.clone()))
									.child(format!("→ {target}"))
									.children(probe(log, target_id)),
							)
							.children(probe(log, row_id))
					},
				))
				.children(probe(log, "paste-mappings"))
		});

		div()
			.id("paste-panel")
			.key_context("PastePanel")
			.track_focus(&self.paste_focus)
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.min_h_0()
			.h_full()
			// Never paint over the bottom tool window when space runs out.
			.overflow_hidden()
			.bg(rgb(pal().editor_bg))
			.rounded(px(ISLAND_RADIUS))
			.child(tab)
			.child(action_bar)
			.child(summary)
			.when(loading, |d| d.child(self.paste_loading_note()))
			.when(plan.whole_commit, |d| {
				d.child(
					div()
						.id("paste-commit-whole")
						.relative()
						.flex_shrink_0()
						.px(px(12.))
						.py(px(4.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(t("commit_whole_note", loc))
						.children(probe(log, "paste-commit-whole")),
				)
			})
			.when_some(plan.error.clone(), |d, err| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(12.))
						.py(px(4.))
						.bg(rgb(pal().error_bg))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().error))
						.child(err.render(loc)),
				)
			})
			// Mappings and rows share one scroll region: it takes its content
			// height up to 60% of the panel and shrinks first when the window
			// is small, so the action bar above stays pinned and every row is
			// reachable by scrolling.
			.child(
				div()
					.id("paste-items")
					.flex()
					.flex_col()
					.min_h(px(0.))
					.max_h(gpui::relative(0.6))
					.overflow_y_scroll()
					.border_b_1()
					.border_color(rgb(pal().divider))
					.children(mappings)
					.children(rows)
					.children(probe(log, "paste-items")),
			)
			.when(selected.is_some(), |d| {
				d.child(
					div()
						.flex()
						.flex_row()
						.items_center()
						.flex_shrink_0()
						.h(px(24.))
						.px(px(12.))
						.gap(px(8.))
						.bg(rgb(pal().panel_bg))
						.text_size(px(SMALL_TEXT))
						.child(fill_text(detail_title))
						.child(
							div()
								.flex_shrink_0()
								.text_color(rgb(pal().text_muted))
								.child(reason),
						),
				)
			})
			.when(selected.is_some_and(|i| i.is_delete), |d| {
				d.child(
					div()
						.p(px(12.))
						.text_color(rgb(pal().error))
						.child(t("reason_delete", loc)),
				)
			})
			.when(selected.is_some_and(|i| !i.is_delete), |d| {
				d.child(self.render_code_view(true, cx))
			})
			.into_any_element()
	}

	// ───────────────────────── git log ─────────────────────────

	// ───────────────────────── log (IJ-2a) ─────────────────────────

	/// Filter chip of the log's filter bar: `Name▾`, or `Name: value ✕`.
	fn log_chip(
		&self,
		menu: LogMenu,
		label: &str,
		value: Option<String>,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let log = &self.probes;
		let key = menu.key();
		let id = format!("log-filter-{key}");
		let clear_id = format!("log-filter-{key}-clear");
		let open = self.log_menu == Some(menu);
		let active = value.is_some();
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex_shrink_0()
			.h(px(24.))
			.px(px(6.))
			.flex()
			.items_center()
			.gap(px(3.))
			.rounded(px(4.))
			.cursor_pointer()
			.text_size(px(UI_TEXT))
			.when(open, |d| d.bg(rgb(pal().hover_bg)))
			.hover(|s| s.bg(rgb(pal().hover_bg)))
			.on_click(
				cx.listener(move |this, _, _, cx| {
					this.toggle_log_menu(menu, cx)
				}),
			)
			.child(
				div()
					.max_w(px(180.))
					.overflow_hidden()
					.line_clamp(1)
					.text_ellipsis()
					.text_color(rgb(pal().text))
					.child(match &value {
						Some(v) => format!("{label}: {v}"),
						None => label.to_string(),
					}),
			)
			.child(if active {
				div()
					.id(SharedString::from(clear_id.clone()))
					.relative()
					.flex()
					.rounded(px(3.))
					.hover(|s| s.bg(rgb(pal().divider)))
					.tooltip(tip(t("log_clear_filter", self.locale)))
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.clear_log_chip(menu, cx);
					}))
					.child(icon(Icon::Close, 12.))
					.children(probe(log, clear_id))
					.into_any_element()
			} else {
				icon(Icon::ChevronDown, 12.).into_any_element()
			})
			.when(open, |d| d.child(self.log_menu_panel(menu, cx)))
			.children(probe(log, id))
			.into_any_element()
	}

	fn clear_log_chip(&mut self, menu: LogMenu, cx: &mut Context<Self>) {
		self.log_menu = None;
		match menu {
			LogMenu::Branch => self.filter_by_ref(None, cx),
			LogMenu::User => self.set_log_author(None, cx),
			LogMenu::Date => self.set_log_since(None, cx),
			LogMenu::Paths => {
				self.log_path_input.update(cx, |i, cx| i.set_text("", cx));
				self.set_log_paths(String::new(), cx)
			}
			LogMenu::More => cx.notify(),
		}
	}

	/// The dropdown under a filter chip or the More button.
	fn log_menu_panel(
		&self,
		menu: LogMenu,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let item = |id: String,
		            label: String,
		            checked: bool,
		            on: MenuAction,
		            cx: &mut Context<Self>| {
			div()
				.id(SharedString::from(id.clone()))
				.relative()
				.h(px(24.))
				.px(px(8.))
				.flex()
				.items_center()
				.gap(px(6.))
				.rounded(px(4.))
				.cursor_pointer()
				.hover(|s| s.bg(rgb(pal().hover_bg)))
				.on_click(cx.listener(move |this, _, _, cx| {
					cx.stop_propagation();
					on(this, cx)
				}))
				.child(
					div()
						.flex_shrink_0()
						.w(px(14.))
						.when(checked, |d| d.child(icon(Icon::Checked, 14.))),
				)
				.child(fill_text(label))
				.children(probe(log, id))
				.into_any_element()
		};
		let mut items: Vec<AnyElement> = Vec::new();
		match menu {
			LogMenu::Branch => {
				let refs = std::iter::once("HEAD".to_string())
					.filter(|_| self.head_sha.is_some())
					.chain(self.refs.iter().map(|r| r.name.clone()))
					.take(MAX_LOG_MENU_ITEMS);
				for name in refs {
					let label = short_ref(&name).to_string();
					let checked =
						self.active_ref_filter.as_deref() == Some(&name);
					let target = name.clone();
					items.push(item(
						format!("log-branch:{name}"),
						label,
						checked,
						Box::new(move |this, cx| {
							this.log_menu = None;
							this.filter_by_ref(Some(target.clone()), cx)
						}),
						cx,
					));
				}
			}
			LogMenu::User => {
				if let Some(email) = self.git_user_email.clone() {
					let checked =
						self.log_filter.author.as_deref() == Some(&email);
					items.push(item(
						"log-user:me".into(),
						format!("{} ({email})", t("log_user_me", loc)),
						checked,
						Box::new(move |this, cx| {
							this.set_log_author(Some(email.clone()), cx)
						}),
						cx,
					));
				}
				for name in self.log_authors(MAX_LOG_MENU_ITEMS) {
					let checked =
						self.log_filter.author.as_deref() == Some(&name);
					let target = name.clone();
					items.push(item(
						format!("log-user:{name}"),
						name,
						checked,
						Box::new(move |this, cx| {
							this.set_log_author(Some(target.clone()), cx)
						}),
						cx,
					));
				}
			}
			LogMenu::Date => {
				for (key, since, label) in DATE_PRESETS {
					let checked =
						self.log_filter.since.as_deref() == Some(since);
					items.push(item(
						format!("log-date:{key}"),
						t(label, loc).to_string(),
						checked,
						Box::new(move |this, cx| {
							this.set_log_since(Some(since), cx)
						}),
						cx,
					));
				}
			}
			LogMenu::Paths => {
				items.push(
					div()
						.id("log-path-input")
						.relative()
						.w(px(240.))
						.child(self.log_path_input.clone())
						.children(probe(log, "log-path-input"))
						.into_any_element(),
				);
				items.push(
					div()
						.px(px(2.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.child(t("log_paths_hint", loc))
						.into_any_element(),
				);
			}
			LogMenu::More => {
				for (key, label, checked) in [
					("details", "log_more_details", self.log_details_visible),
					(
						"branches",
						"log_more_branches",
						self.log_branches_visible,
					),
					("hash", "log_more_hash", self.log_show_hash),
				] {
					items.push(item(
						format!("log-more:{key}"),
						t(label, loc).to_string(),
						checked,
						Box::new(move |this, cx| {
							this.log_menu = None;
							match key {
								"details" => {
									this.log_details_visible =
										!this.log_details_visible
								}
								"branches" => {
									this.log_branches_visible =
										!this.log_branches_visible
								}
								_ => this.log_show_hash = !this.log_show_hash,
							}
							app_log!("[APP:LOG_VIEW: {}]", key);
							cx.notify();
						}),
						cx,
					));
				}
			}
		}
		let panel = div()
			.id("log-menu")
			.occlude()
			.min_w(px(200.))
			.max_w(px(360.))
			.max_h(px(320.))
			.overflow_y_scroll()
			.flex()
			.flex_col()
			.p(px(4.))
			.bg(rgb(pal().panel_bg))
			.border_1()
			.border_color(rgb(pal().button_border))
			.rounded(px(6.))
			.shadow_lg()
			.text_size(px(UI_TEXT))
			.text_color(rgb(pal().text))
			.on_mouse_down_out(
				cx.listener(|this, _, _, cx| this.close_log_menu(cx)),
			)
			.children(items);
		div()
			.absolute()
			.top(px(26.))
			.left_0()
			.child(
				deferred(anchored().snap_to_window().child(panel))
					.with_priority(1),
			)
			.into_any_element()
	}

	/// IntelliJ's branches pane: search, HEAD, collapsible Local / Remote /
	/// Tags groups (no counts).
	fn render_branches(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let needle = self
			.branch_filter_input
			.read(cx)
			.text()
			.trim()
			.to_lowercase();
		let current = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		let mut rows: Vec<AnyElement> = Vec::new();
		let entry = |id: String,
		             label: String,
		             glyph: AnyElement,
		             indent: f32,
		             target: Option<String>,
		             active: bool,
		             cx: &mut Context<Self>| {
			div()
				.id(SharedString::from(id.clone()))
				.relative()
				.flex_shrink_0()
				.h(px(24.))
				.mx(px(4.))
				.pl(px(indent))
				.pr(px(6.))
				.flex()
				.items_center()
				.gap(px(6.))
				.rounded(px(4.))
				.cursor_pointer()
				.when(active, |d| d.bg(rgb(pal().selection_inactive_bg)))
				.when(!active, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.tooltip(tip(label.clone()))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.filter_by_ref(target.clone(), cx)
				}))
				.child(glyph)
				.child(clip_text(label).text_color(rgb(pal().text)))
				.children(probe(log, id))
				.into_any_element()
		};
		if self.head_sha.is_some() && needle.is_empty() {
			rows.push(entry(
				"ref:HEAD".into(),
				t("log_head_current", loc).to_string(),
				div().w(px(14.)).into_any_element(),
				10.,
				Some("HEAD".into()),
				self.active_ref_filter.as_deref() == Some("HEAD"),
				cx,
			));
		}
		for (key, prefix) in [
			("refs_local", "refs/heads/"),
			("refs_remote", "refs/remotes/"),
			("refs_tags", "refs/tags/"),
		] {
			let members: Vec<_> = self
				.refs
				.iter()
				.filter(|r| r.name.starts_with(prefix))
				.filter(|r| {
					needle.is_empty()
						|| r.name[prefix.len()..]
							.to_lowercase()
							.contains(&needle)
				})
				.collect();
			if members.is_empty() {
				continue;
			}
			let collapsed = self.branch_groups_collapsed.contains(&key);
			let gid = format!("branch-group:{key}");
			rows.push(
				div()
					.id(SharedString::from(gid.clone()))
					.relative()
					.flex_shrink_0()
					.h(px(24.))
					.mx(px(4.))
					.px(px(6.))
					.flex()
					.items_center()
					.gap(px(4.))
					.rounded(px(4.))
					.cursor_pointer()
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.on_click(cx.listener(move |this, _, _, cx| {
						match this
							.branch_groups_collapsed
							.iter()
							.position(|k| *k == key)
						{
							Some(i) => {
								this.branch_groups_collapsed.remove(i);
							}
							None => this.branch_groups_collapsed.push(key),
						}
						cx.notify();
					}))
					.child(icon(
						if collapsed {
							Icon::ChevronRight
						} else {
							Icon::ChevronDown
						},
						12.,
					))
					.child(clip_text(t(key, loc)).text_color(rgb(pal().text)))
					.children(probe(log, gid))
					.into_any_element(),
			);
			if collapsed {
				continue;
			}
			// Bounded: very large ref sets are reached through the search.
			for r in members.iter().take(MAX_BRANCH_ROWS) {
				let name = &r.name[prefix.len()..];
				let is_current =
					prefix == "refs/heads/" && current.as_deref() == Some(name);
				let glyph = match prefix {
					"refs/tags/" => icon_tinted(Icon::Tag, 14., pal().ref_tag)
						.into_any_element(),
					_ if is_current => {
						icon_tinted(Icon::Head, 14., pal().ref_head)
							.into_any_element()
					}
					_ => icon(Icon::Branch, 14.).into_any_element(),
				};
				rows.push(entry(
					format!("ref:{}", r.name),
					name.to_string(),
					glyph,
					24.,
					Some(r.name.clone()),
					self.active_ref_filter.as_deref() == Some(r.name.as_str()),
					cx,
				));
			}
		}
		div()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(self.log_branches_w()))
			.h_full()
			.border_r_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					.flex_shrink_0()
					.h(px(32.))
					.px(px(6.))
					.flex()
					.items_center()
					.gap(px(4.))
					.child(icon(Icon::Search, 14.))
					.child(
						div()
							.id("branch-filter-input")
							.relative()
							.flex_1()
							.min_w_0()
							.child(self.branch_filter_input.clone())
							.children(probe(log, "branch-filter-input")),
					),
			)
			.child(
				div()
					.id("refs-scroll")
					.flex()
					.flex_col()
					.flex_1()
					.min_h_0()
					.overflow_y_scroll()
					.text_size(px(UI_TEXT))
					.pb(px(4.))
					.children(rows),
			)
			.into_any_element()
	}

	/// The thin icon toolbar at the log's left edge (IntelliJ's branches
	/// toolbar).
	fn render_branch_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let shown = self.log_branches_visible;
		div()
			.flex()
			.flex_col()
			.items_center()
			.flex_shrink_0()
			.w(px(32.))
			.h_full()
			.py(px(4.))
			.gap(px(2.))
			.border_r_1()
			.border_color(rgb(pal().divider))
			.child(
				log_icon_button(
					"branches-toggle",
					if shown {
						Icon::Back
					} else {
						Icon::ChevronRight
					},
					t(
						if shown {
							"tip_branches_hide"
						} else {
							"tip_branches_show"
						},
						loc,
					),
					true,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.log_branches_visible = !this.log_branches_visible;
					app_log!("[APP:LOG_VIEW: branches]");
					cx.notify();
				}))
				.children(probe(log, "branches-toggle")),
			)
			.child(
				log_icon_button(
					"branches-expand-all",
					Icon::ExpandAll,
					t("tip_expand_all", loc),
					shown,
					false,
				)
				.when(shown, |d| {
					d.on_click(cx.listener(|this, _, _, cx| {
						this.branch_groups_collapsed.clear();
						cx.notify();
					}))
				})
				.children(probe(log, "branches-expand-all")),
			)
			.child(
				log_icon_button(
					"branches-collapse-all",
					Icon::CollapseAll,
					t("tip_collapse_all", loc),
					shown,
					false,
				)
				.when(shown, |d| {
					d.on_click(cx.listener(|this, _, _, cx| {
						this.branch_groups_collapsed =
							vec!["refs_local", "refs_remote", "refs_tags"];
						cx.notify();
					}))
				})
				.children(probe(log, "branches-collapse-all")),
			)
			.into_any_element()
	}

	fn render_log(&self, height: f32, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let range = self.range_rows();
		let header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(PANEL_HEADER_H + 2.0))
			.px(px(8.))
			.gap(px(8.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					.flex_shrink_0()
					.font_weight(FontWeight::SEMIBOLD)
					.child("Git"),
			)
			.child(
				div()
					.flex_shrink_0()
					.h(px(22.))
					.px(px(8.))
					.flex()
					.items_center()
					.rounded(px(4.))
					.bg(rgb(pal().selection_inactive_bg))
					.child(match &self.active_ref_filter {
						Some(r) => {
							format!("{}: {}", t("log_tab", loc), short_ref(r))
						}
						None => t("log_tab", loc).to_string(),
					}),
			)
			.child(div().flex_1())
			.child(
				log_icon_button(
					"btn-log-more",
					Icon::More,
					t("tip_log_more", loc),
					true,
					self.log_menu == Some(LogMenu::More),
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.toggle_log_menu(LogMenu::More, cx)
				}))
				.when(self.log_menu == Some(LogMenu::More), |d| {
					d.child(self.log_menu_panel(LogMenu::More, cx))
				})
				.children(probe(log, "btn-log-more")),
			)
			.child(
				log_icon_button(
					"btn-log-hide",
					Icon::Hide,
					t("hide", loc),
					true,
					false,
				)
				.tab_index(47)
				.on_click(cx.listener(|this, _, _, cx| this.toggle_log(cx)))
				.children(probe(log, "btn-log-hide")),
			);

		let text_toggle = |id: &'static str,
		                   ic: Icon,
		                   tooltip: &'static str,
		                   on: bool,
		                   cx: &mut Context<Self>| {
			div()
				.id(id)
				.relative()
				.flex_shrink_0()
				.size(px(20.))
				.flex()
				.items_center()
				.justify_center()
				.rounded(px(3.))
				.cursor_pointer()
				.tooltip(tip(t(tooltip, loc)))
				.when(on, |d| {
					d.bg(rgb(pal().selection_bg))
						.border_1()
						.border_color(rgb(pal().focus_ring))
				})
				.when(!on, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
				.on_click(cx.listener(move |this, _, _, cx| {
					if id == "btn-log-regex" {
						this.toggle_log_regex(cx)
					} else {
						this.toggle_log_match_case(cx)
					}
				}))
				.child(icon(ic, 14.))
				.children(probe(log, id))
		};
		let branch_value = self
			.active_ref_filter
			.as_deref()
			.map(|r| short_ref(r).to_string());
		let user_value = self.log_filter.author.clone().map(|a| {
			if self.git_user_email.as_deref() == Some(a.as_str()) {
				t("log_user_me", loc).to_string()
			} else {
				a
			}
		});
		let date_value = self.log_filter.since.as_deref().map(|s| {
			DATE_PRESETS
				.iter()
				.find(|(_, since, _)| *since == s)
				.map(|(_, _, label)| t(label, loc).to_string())
				.unwrap_or_else(|| s.to_string())
		});
		let paths_value = self.log_filter.paths.first().cloned();
		let filter_bar = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(34.))
			.px(px(6.))
			.gap(px(4.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				div()
					.w(px(240.))
					.min_w(px(96.))
					.h(px(26.))
					.px(px(6.))
					.flex()
					.items_center()
					.gap(px(4.))
					.rounded(px(4.))
					.border_1()
					.border_color(rgb(pal().button_border))
					.child(icon(Icon::Search, 14.))
					.child(
						div()
							.id("log-search-input")
							.relative()
							.flex_1()
							.min_w_0()
							.child(self.log_search_input.clone())
							.children(probe(log, "log-search-input")),
					)
					.child(text_toggle(
						"btn-log-regex",
						Icon::Regex,
						"tip_log_regex",
						self.log_filter.regex,
						cx,
					))
					.child(text_toggle(
						"btn-log-case",
						Icon::MatchCase,
						"tip_log_case",
						self.log_filter.match_case,
						cx,
					)),
			)
			// Chips give way first on a narrow window; the actions stay.
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(4.))
					.flex_1()
					.min_w_0()
					.overflow_hidden()
					.child(self.log_chip(
						LogMenu::Branch,
						t("log_chip_branch", loc),
						branch_value,
						cx,
					))
					.child(self.log_chip(
						LogMenu::User,
						t("log_chip_user", loc),
						user_value,
						cx,
					))
					.child(self.log_chip(
						LogMenu::Date,
						t("log_chip_date", loc),
						date_value,
						cx,
					))
					.child(self.log_chip(
						LogMenu::Paths,
						t("log_chip_paths", loc),
						paths_value,
						cx,
					)),
			)
			.child(
				log_icon_button(
					"btn-log-refresh",
					Icon::Refresh,
					t("tip_log_refresh", loc),
					true,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					app_log!("[APP:LOG_REFRESH]");
					this.apply_log_filter(cx)
				}))
				.children(probe(log, "btn-log-refresh")),
			)
			.child(
				log_icon_button(
					"btn-head",
					Icon::Locate,
					t("tip_head", loc),
					self.head_sha.is_some(),
					false,
				)
				.tab_index(42)
				.on_click(cx.listener(|this, _, _, cx| this.locate_head(cx)))
				.children(probe(log, "btn-head")),
			)
			.child(
				log_icon_button(
					"btn-compare",
					Icon::Diff,
					match range {
						Some((a, b)) => tf("tip_compare", loc, &[&(b - a + 1)]),
						None => t("tip_compare_disabled", loc).to_string(),
					},
					range.is_some(),
					false,
				)
				.when(range.is_some(), |b| {
					b.tab_index(45).on_click(
						cx.listener(|this, _, _, cx| this.compare_range(cx)),
					)
				})
				.children(probe(log, "btn-compare")),
			)
			.child(
				log_icon_button(
					"btn-copy-commits",
					Icon::Copy,
					t("tip_copy_commits", loc),
					self.selected_commit.is_some(),
					false,
				)
				.when(self.selected_commit.is_some(), |b| {
					b.tab_index(46).on_click(cx.listener(|this, _, _, cx| {
						this.copy_commits_to_clipboard(cx)
					}))
				})
				.children(probe(log, "btn-copy-commits")),
			);

		let gutter_w = self
			.graph_layout
			.as_ref()
			.map(graph_view::gutter_width)
			.unwrap_or(40.0);
		let rows = self.display_commits();
		let n = rows.len();
		// Tint rows on the current branch only when others are shown too.
		let on_head: Rc<Vec<bool>> = {
			let marks: HashMap<&str, bool> = self
				.commits
				.iter()
				.zip(&self.log_on_head)
				.map(|(c, on)| (c.sha.as_str(), *on))
				.collect();
			let v: Vec<bool> = rows
				.iter()
				.map(|c| marks.get(c.sha.as_str()).copied().unwrap_or(false))
				.collect();
			Rc::new(if v.iter().all(|on| *on) {
				Vec::new()
			} else {
				v
			})
		};
		drop(rows);
		let list_w = f32::from(
			self.log_scroll.0.borrow().base_handle.bounds().size.width,
		);
		let loading_row = self.history_extending && self.history_has_more;
		let list = uniform_list(
			"log-rows",
			n + usize::from(loading_row),
			cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
				this.autoload_near(n, cx);
				range
					.map(|ix| {
						if ix >= n {
							return this.log_loading_row();
						}
						let tint = on_head.get(ix).copied().unwrap_or(false);
						this.log_row(ix, gutter_w, list_w, tint, cx)
					})
					.collect::<Vec<_>>()
			}),
		)
		.track_scroll(self.log_scroll.clone())
		.size_full();

		div()
			.id("log-panel")
			.relative()
			.child({
				// Remembers the panel's width so the side panes can give way.
				let width = self.log_width.clone();
				canvas(
					move |b, _, _| width.set(f32::from(b.size.width)),
					|_, _, _, _| {},
				)
				.absolute()
				.size_full()
			})
			.flex()
			.flex_col()
			.flex_shrink_0()
			.h(px(height))
			.bg(rgb(pal().panel_bg))
			.rounded(px(ISLAND_RADIUS))
			.overflow_hidden()
			.child(header)
			.when_some(self.history_error.clone(), |d, err| {
				d.child(
					div()
						.id("log-error")
						.relative()
						.flex_shrink_0()
						.px(px(10.))
						.py(px(2.))
						.bg(rgb(pal().error_bg))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().error))
						.child(tf("error_history", loc, &[&err]))
						.children(probe(log, "log-error")),
				)
			})
			.child(
				div()
					.flex()
					.flex_row()
					.flex_1()
					.min_h_0()
					.child(self.render_branch_toolbar(cx))
					.when(self.log_branches_visible, |d| {
						d.child(self.render_branches(cx))
					})
					.child(
						div()
							.flex()
							.flex_col()
							.flex_1()
							.min_w_0()
							.child(filter_bar)
							.child(
								div()
									.id("log-list")
									.relative()
									.key_context("GitLog")
									.track_focus(&self.log_focus)
									.on_action(cx.listener(
										|this, _: &LogUp, _, cx| {
											this.log_move(-1, false, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogDown, _, cx| {
											this.log_move(1, false, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogExtendUp, _, cx| {
											this.log_move(-1, true, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogExtendDown, _, cx| {
											this.log_move(1, true, cx)
										},
									))
									.on_action(cx.listener(
										|this, _: &LogOpen, window, cx| {
											window.focus(&this.reader_focus);
											cx.notify();
										},
									))
									.on_action(cx.listener(
										|this,
										 _: &LogSearchFocus,
										 window,
										 cx| {
											window.focus(
												&this
													.log_search_input
													.read(cx)
													.handle(),
											);
										},
									))
									.on_action(cx.listener(
										|this, _: &LogHead, _, cx| {
											this.locate_head(cx)
										},
									))
									.on_scroll_wheel(cx.listener(
										|this, _: &ScrollWheelEvent, _, cx| {
											if !this.history_autoload {
												this.rearm_autoload();
												cx.notify();
											}
										},
									))
									.flex_1()
									.min_h_0()
									.when(
										n == 0
											&& self.history_error.is_none()
											&& !self.history_extending,
										|d| {
											d.child(
												div()
													.p(px(10.))
													.text_color(rgb(
														pal().text_muted
													))
													.child(t("empty_log", loc)),
											)
										},
									)
									.child(list)
									.children(probe(log, "log-list")),
							),
					)
					.when(self.log_details_visible, |d| {
						d.child(self.splitter(Splitter::LogDetails, cx))
							.child(self.render_commit_panel(cx))
					}),
			)
			.into_any_element()
	}

	/// Branches pane width: narrower when the log itself is narrow.
	fn log_branches_w(&self) -> f32 {
		let w = self.log_width.get();
		if w > 0. && w < 1200. {
			LOG_BRANCHES_W * 0.75
		} else {
			LOG_BRANCHES_W
		}
	}

	/// Details pane width, at most 28% of the log so the list keeps room.
	fn log_details_width(&self) -> f32 {
		let w = self.log_width.get();
		if w > 0. {
			self.log_details_w.min((w * 0.28).max(LOG_DETAILS_W_MIN))
		} else {
			self.log_details_w
		}
	}

	fn log_loading_row(&self) -> AnyElement {
		div()
			.id("log-loading")
			.relative()
			.w_full()
			.h(px(graph_view::ROW_HEIGHT))
			.px(px(10.))
			.flex()
			.items_center()
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			.child(t("log_loading_more", self.locale))
			.children(probe(&self.probes, "log-loading"))
			.into_any_element()
	}

	fn log_row(
		&self,
		ix: usize,
		gutter_w: f32,
		list_w: f32,
		on_head: bool,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let rows = self.display_commits();
		let Some(c) = rows.get(ix).copied() else {
			return div().into_any_element();
		};
		let loc = self.locale;
		// Every tooltip of the row is registered only when this is true (a
		// context menu over the log will turn it off).
		let show_tips = true;
		let selected = self.selected_commit.as_deref() == Some(&c.sha);
		let in_range =
			self.range_rows().is_some_and(|(a, b)| ix >= a && ix <= b);
		let sha = c.sha.clone();
		let sha_click = sha.clone();
		let graph_row = self
			.graph_layout
			.as_ref()
			.and_then(|l| l.rows.get(ix))
			.cloned();
		let strokes = self
			.graph_layout
			.as_ref()
			.zip(graph_row.as_ref())
			.map(|(l, r)| graph_view::row_strokes(l, r))
			.unwrap_or_default();
		let is_merge = c.parents.len() > 1 && self.log_search.is_none();
		let collapsed = self.collapsed_merges.contains(&c.sha);
		let hidden_n = if collapsed {
			crate::history::side_only(&self.commits, &c.sha).len()
		} else {
			0
		};
		let current_branch = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		let (labels, labels_w) = graph_row
			.as_ref()
			.map(|r| {
				ref_label_elements(
					&r.refs,
					current_branch.as_deref(),
					short(&sha),
					show_tips,
				)
			})
			.unwrap_or_default();
		let mine = self
			.git_user_email
			.as_deref()
			.is_some_and(|e| e.eq_ignore_ascii_case(&c.author_email));
		let hash_w = if self.log_show_hash { 64. } else { 0. };
		// ponytail: estimated text width (no shaping per row); a subject
		// near the edge may miss or gain its tooltip.
		let subject_room =
			list_w - gutter_w - AUTHOR_W - DATE_W - hash_w - labels_w - 40.;
		let truncated = approx_text_w(&c.subject, UI_TEXT) > subject_room;
		let row_id = format!("commit-row:{}", short(&sha));
		let col_id = format!("collapse:{}", short(&sha));
		let merge_sha = sha.clone();
		let node_x = graph_row
			.as_ref()
			.map(|r| graph_view::lane_x(r.node.lane))
			.unwrap_or(0.);
		let date = log_date(&c.author_date, loc);
		div()
			.id(SharedString::from(row_id.clone()))
			.relative()
			.w_full()
			.flex()
			.flex_row()
			.items_center()
			.h(px(graph_view::ROW_HEIGHT))
			.pr(px(8.))
			.gap(px(8.))
			.cursor_pointer()
			.when(on_head && !selected && !in_range, |d| {
				d.bg(rgb(pal().log_current_branch_bg))
			})
			.when(selected, |d| {
				d.bg(rgb(if self.log_active {
					pal().selection_bg
				} else {
					pal().selection_inactive_bg
				}))
			})
			.when(in_range && !selected, |d| d.bg(rgb(pal().range_bg)))
			.when(!selected && !in_range, |d| {
				d.hover(|s| s.bg(rgb(pal().hover_bg)))
			})
			.on_click(cx.listener(
				move |this, ev: &gpui::ClickEvent, window, cx| {
					window.focus(&this.log_focus);
					if ev.modifiers().shift {
						this.extend_range(&sha_click, cx);
					} else {
						this.select_commit(&sha_click, cx);
					}
				},
			))
			.on_mouse_down(MouseButton::Right, {
				let sha = sha.clone();
				cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
					window.focus(&this.log_focus);
					this.open_log_menu(&sha, ev.position, window, cx);
				})
			})
			.child(
				div()
					.relative()
					.flex_shrink_0()
					.w(px(gutter_w))
					.h(px(graph_view::ROW_HEIGHT))
					.when_some(graph_row, |el, r| {
						el.child(
							canvas(
								|_, _, _| {},
								move |bounds, _, window, _| {
									graph_view::paint_row_graph(
										window, &r, &strokes, bounds,
									);
								},
							)
							.size_full(),
						)
					})
					// Merge rows collapse from their graph node.
					.when(is_merge, |d| {
						d.child(
							div()
								.id(SharedString::from(col_id.clone()))
								.absolute()
								.left(px(node_x - 7.))
								.top(px(graph_view::ROW_HEIGHT / 2. - 7.))
								.size(px(14.))
								.rounded(px(7.))
								.hover(|s| s.bg(rgb(pal().hover_bg)))
								.when(show_tips, |d| {
									d.tooltip(tip(t(
										if collapsed {
											"tip_expand_merge"
										} else {
											"tip_collapse_merge"
										},
										loc,
									)))
								})
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									this.toggle_collapse(merge_sha.clone(), cx);
								}))
								.children(probe(&self.probes, col_id.clone())),
						)
					}),
			)
			.child(
				div()
					.flex_1()
					.min_w_0()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(6.))
					.overflow_hidden()
					.child(
						div()
							.id(SharedString::from(format!(
								"subject:{}",
								short(&sha)
							)))
							.flex_1()
							.min_w_0()
							.overflow_hidden()
							.line_clamp(1)
							.text_ellipsis()
							.when(show_tips && truncated, |d| {
								d.tooltip(tip(c.subject.clone()))
							})
							.child(c.subject.clone()),
					)
					.when(collapsed, |d| {
						d.child(
							div()
								.flex_shrink_0()
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(pal().text_muted))
								.child(tf("collapsed_n", loc, &[&hidden_n])),
						)
					})
					.child(
						div()
							.flex_shrink_0()
							.ml_auto()
							.flex()
							.flex_row()
							.items_center()
							.gap(px(8.))
							.children(labels),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("author:{}", short(&sha))))
					.flex_shrink_0()
					.w(px(AUTHOR_W))
					.when(show_tips, |d| {
						d.tooltip(tip(format!(
							"{} <{}>",
							c.author_name, c.author_email
						)))
					})
					.child(
						clip_text(c.author_name.clone()).when(mine, |d| {
							d.font_weight(FontWeight::SEMIBOLD)
						}),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("date:{}", short(&sha))))
					.flex_shrink_0()
					.w(px(DATE_W))
					.when(show_tips, |d| {
						d.tooltip(tip(short_date(&c.author_date)))
					})
					.child(clip_text(date)),
			)
			.when(self.log_show_hash, |d| {
				d.child(
					div()
						.id(SharedString::from(format!("sha:{}", short(&sha))))
						.flex_shrink_0()
						.w(px(hash_w))
						.font_family(CODE_FONT)
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(pal().text_muted))
						.when(show_tips, |d| d.tooltip(tip(c.sha.clone())))
						.child(short(&c.sha).to_string()),
				)
			})
			.children(probe(&self.probes, row_id))
			.into_any_element()
	}

	/// The log's right pane: the selected commit's changed files grouped by
	/// directory, then its details (or the compare's range).
	fn render_commit_panel(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let rows = Rc::new(crate::history::changed_file_rows(
			&self.commit_files,
			&self.changed_dirs_collapsed,
		));
		let n = self.commit_files.len();
		let files_header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(30.))
			.px(px(8.))
			.gap(px(4.))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(
				fill_text(if n > 0 {
					tf("changed_files", loc, &[&n])
				} else {
					String::new()
				})
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted)),
			)
			.child(
				log_icon_button(
					"details-expand-all",
					Icon::ExpandAll,
					t("tip_expand_all", loc),
					n > 0,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.changed_dirs_collapsed.clear();
					cx.notify();
				}))
				.children(probe(log, "details-expand-all")),
			)
			.child(
				log_icon_button(
					"details-collapse-all",
					Icon::CollapseAll,
					t("tip_collapse_all", loc),
					n > 0,
					false,
				)
				.on_click(cx.listener(|this, _, _, cx| {
					this.changed_dirs_collapsed =
						crate::history::changed_file_rows(
							&this.commit_files,
							&[],
						)
						.into_iter()
						.filter_map(|r| match r {
							crate::history::ChangedRow::Dir {
								path, ..
							} => Some(path),
							_ => None,
						})
						.collect();
					cx.notify();
				}))
				.children(probe(log, "details-collapse-all")),
			);
		let files = uniform_list(
			"commit-files",
			rows.len(),
			cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
				range
					.filter_map(|ix| rows.get(ix).cloned())
					.map(|row| this.changed_file_row(row, cx))
					.collect::<Vec<_>>()
			}),
		)
		.size_full();

		let details: AnyElement = if let Some((from, to)) = &self.compare {
			div()
				.text_color(rgb(pal().text))
				.font_weight(FontWeight::SEMIBOLD)
				.child(tf("compare_header", loc, &[&short(from), &short(to)]))
				.into_any_element()
		} else if let Some(sha) = self.selected_commit.as_deref() {
			self.commit_details_view(sha).into_any_element()
		} else {
			div()
				.text_color(rgb(pal().text_muted))
				.child(t("log_details_empty", loc))
				.into_any_element()
		};
		div()
			.id("commit-panel")
			.relative()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(self.log_details_width()))
			.h_full()
			.child(
				div()
					.flex()
					.flex_col()
					.flex_1()
					.min_h_0()
					.child(files_header)
					.child(div().flex_1().min_h_0().py(px(2.)).child(files)),
			)
			.child(
				div()
					.id("commit-details")
					.relative()
					.flex_1()
					.min_h_0()
					.overflow_y_scroll()
					.border_t_1()
					.border_color(rgb(pal().divider))
					.p(px(12.))
					.child(details)
					.children(probe(log, "commit-details")),
			)
			.children(probe(log, "commit-panel"))
			.into_any_element()
	}

	fn changed_file_row(
		&self,
		row: crate::history::ChangedRow,
		cx: &mut Context<Self>,
	) -> AnyElement {
		use crate::history::ChangedRow;
		let loc = self.locale;
		match row {
			ChangedRow::Dir {
				path,
				files,
				expanded,
			} => {
				let id = format!("commit-dir:{path}");
				let p2 = path.clone();
				div()
					.id(SharedString::from(id.clone()))
					.relative()
					.flex()
					.items_center()
					.gap(px(5.))
					.h(px(ROW_H))
					.w_full()
					.px(px(8.))
					.cursor_pointer()
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.on_click(cx.listener(move |this, _, _, cx| {
						let dirs = &mut this.changed_dirs_collapsed;
						match dirs.iter().position(|d| d == &p2) {
							Some(i) => {
								dirs.remove(i);
							}
							None => dirs.push(p2.clone()),
						}
						cx.notify();
					}))
					.child(icon(
						if expanded {
							Icon::ChevronDown
						} else {
							Icon::ChevronRight
						},
						12.,
					))
					.child(icon(Icon::Folder, 14.))
					.child(clip_text(path).text_color(rgb(pal().text)))
					.child(
						div()
							.flex_shrink_0()
							.text_color(rgb(pal().text_muted))
							.child(tf("log_dir_files", loc, &[&files])),
					)
					.children(probe(&self.probes, id))
					.into_any_element()
			}
			ChangedRow::File { idx, nested } => {
				let Some((path, ct)) = self.commit_files.get(idx).cloned()
				else {
					return div().into_any_element();
				};
				let (_, color) = change_style(ct);
				let sel = self.selected_commit_file.as_deref() == Some(&path)
					&& self.rev_tree.is_none();
				let id = format!("commit-file:{path}");
				let name = if nested {
					path.rsplit('/').next().unwrap_or(&path).to_string()
				} else {
					path.clone()
				};
				let p2 = path.clone();
				div()
					.id(SharedString::from(id.clone()))
					.relative()
					.flex()
					.items_center()
					.gap(px(5.))
					.h(px(ROW_H))
					.w_full()
					.pl(px(if nested { 42. } else { 8. }))
					.pr(px(8.))
					.cursor_pointer()
					.when(sel, |d| d.bg(rgb(pal().selection_bg)))
					.when(!sel, |d| d.hover(|s| s.bg(rgb(pal().hover_bg))))
					.tooltip(tip(path.clone()))
					.on_click(cx.listener(move |this, _, _, cx| {
						this.select_commit_file(&p2, cx)
					}))
					.child(icon(file_icon(&path), 14.))
					.child(fill_text(name).text_color(rgb(color)))
					.children(probe(&self.probes, id))
					.into_any_element()
			}
		}
	}

	/// Message, hash, author and date, the commit's refs and the branches
	/// that contain it, like IntelliJ's commit details.
	fn commit_details_view(&self, sha: &str) -> Div {
		let loc = self.locale;
		let row = self.commits.iter().find(|c| c.sha == sha);
		let details = self.commit_details.as_ref().filter(|d| d.sha == sha);
		let message = details
			.map(|d| d.message.clone())
			.or_else(|| row.map(|c| c.subject.clone()))
			.unwrap_or_default();
		let (subject, body) = match message.split_once('\n') {
			Some((s, b)) => (s.to_string(), b.trim().to_string()),
			None => (message, String::new()),
		};
		let (author, email, date) = match (details, row) {
			(Some(d), _) => (
				d.author.clone(),
				d.author_email.clone(),
				d.author_date.clone(),
			),
			(None, Some(c)) => (
				c.author_name.clone(),
				c.author_email.clone(),
				c.author_date.clone(),
			),
			_ => Default::default(),
		};
		let current_branch = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		let refs = self
			.display_commits()
			.iter()
			.position(|c| c.sha == sha)
			.and_then(|ix| {
				self.graph_layout.as_ref().and_then(|l| l.rows.get(ix))
			})
			.map(|r| r.refs.clone())
			.unwrap_or_default();
		let muted = |s: String| {
			div()
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.child(s)
		};
		div()
			.flex()
			.flex_col()
			.gap(px(6.))
			.text_color(rgb(pal().text))
			.child(div().font_weight(FontWeight::SEMIBOLD).child(subject))
			.when(!body.is_empty(), |d| {
				d.child(div().whitespace_normal().child(body))
			})
			.child(
				div()
					.flex()
					.flex_wrap()
					.gap(px(4.))
					.child(
						div()
							.font_family(CODE_FONT)
							.text_size(px(SMALL_TEXT))
							.child(short(sha).to_string()),
					)
					.child(div().child(author))
					.when(!email.is_empty(), |d| {
						d.child(
							div()
								.text_color(rgb(pal().link))
								.child(format!("<{email}>")),
						)
					}),
			)
			.child(muted(tf("log_details_on", loc, &[&log_date(&date, loc)])))
			.when_some(
				details.filter(|d| {
					d.commit_date != d.author_date
						|| d.committer_email != d.author_email
				}),
				|el, d| {
					el.child(muted(tf(
						"log_details_committed",
						loc,
						&[&d.committer, &log_date(&d.commit_date, loc)],
					)))
				},
			)
			.when(!refs.is_empty(), |d| {
				let badges = graph_view::merge_tracking_refs(&refs);
				d.child(div().flex().flex_col().gap(px(2.)).children(
					badges.iter().map(|b| {
						let l =
							graph_view::ref_label(b, current_branch.as_deref());
						div()
							.flex()
							.items_center()
							.gap(px(4.))
							.child(label_icon(&l))
							.child(l.text)
					}),
				))
			})
			.when_some(details.filter(|d| !d.branches.is_empty()), |el, d| {
				let mut list = d.branches.join(", ");
				if d.branches_more {
					list.push_str(", …");
				}
				el.child(muted(tf(
					"log_details_in_branches",
					loc,
					&[
						&format!(
							"{}{}",
							d.branches.len(),
							if d.branches_more { "+" } else { "" }
						),
						&list,
					],
				)))
			})
	}

	/// IntelliJ status bar: message on the left, borderless widgets on the
	/// right with no separators; zero counters are not shown.
	fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let errors = self.repos.iter().filter(|r| r.summary.is_err()).count();
		let basket_n = self.basket_count().to_string();
		let (basket_detail, collision) = &self.basket_view;
		let basket_label = if let Some(collision) = collision {
			tf("basket_collision", loc, &[collision])
		} else if self.basket_count() == 0 {
			t("basket_empty", loc).to_string()
		} else {
			tf("basket_summary", loc, &[&basket_n, basket_detail])
		};
		let repos_s = self.repos.len().to_string();
		let repo_label = if errors > 0 {
			tf("status_repo_count", loc, &[&repos_s, &errors.to_string()])
		} else {
			tf("status_repo_count_ok", loc, &[&repos_s])
		};
		let jobs = self.lifecycle.live_jobs();
		let branch =
			self.repo().and_then(|r| r.summary.as_ref().ok()).map(|s| {
				match (&s.branch, &s.head) {
					(Some(b), _) => b.clone(),
					(None, Some(h)) => short(h).to_string(),
					(None, None) => t("repo_unborn", loc).to_string(),
				}
			});
		let widget = || {
			div()
				.flex_shrink_0()
				.flex()
				.items_center()
				.gap(px(4.))
				.h(px(STATUS_H - 4.))
				.px(px(6.))
				.rounded(px(4.))
		};
		div()
			.id("status-bar")
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(STATUS_H))
			.pl(px(10.))
			.pr(px(6.))
			.gap(px(2.))
			.bg(rgb(pal().frame_bg))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(pal().text_muted))
			.child(fill_text(self.status.render(loc)).mr(px(8.)))
			.child(
				widget()
					.id("basket-summary")
					.relative()
					.max_w(px(360.))
					.min_w(px(40.))
					.overflow_hidden()
					.tooltip(tip(basket_label.clone()))
					.child(clip_text(basket_label))
					.children(probe(&self.probes, "basket-summary")),
			)
			.child(
				widget()
					.text_color(rgb(if errors > 0 {
						pal().error
					} else {
						pal().text_muted
					}))
					.child(repo_label),
			)
			.when(jobs > 0, |d| {
				d.child(
					widget()
						.id("lifecycle-jobs")
						.relative()
						.child(tf("lifecycle_jobs", loc, &[&jobs.to_string()]))
						.children(probe(&self.probes, "lifecycle-jobs")),
				)
			})
			.when_some(branch, |d, b| {
				// VCS widget: branch icon and name, like IntelliJ's Git widget.
				d.child(
					widget()
						.id("status-vcs")
						.relative()
						.max_w(px(200.))
						.cursor_pointer()
						.hover(|s| s.bg(rgb(pal().hover_bg)))
						.tooltip(tip(t("tip_vcs_branch", loc)))
						.on_click(cx.listener(|this, _, window, cx| {
							this.open_popover(Popover::Ref, window, cx)
						}))
						.child(icon(Icon::Branch, 12.))
						.child(clip_text(b).text_color(rgb(pal().text)))
						.children(probe(&self.probes, "status-vcs")),
				)
			})
			.child(
				button(
					"btn-locale",
					t("btn_toggle_lang", loc),
					Btn::Ghost,
					true,
					6,
				)
				.h(px(STATUS_H - 4.))
				.px(px(6.))
				.text_size(px(SMALL_TEXT))
				.text_color(rgb(pal().text_muted))
				.tooltip(tip(t("tip_language", loc)))
				.on_click(cx.listener(|this, _, _, cx| this.toggle_locale(cx)))
				.children(probe(&self.probes, "btn-locale")),
			)
			.children(probe(&self.probes, "status-bar"))
			.into_any_element()
	}
}

impl Render for WorkbenchModel {
	fn render(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) -> impl IntoElement {
		if let Some(h) = self.pending_focus.take() {
			window.focus(&h);
		}
		let vp = window.viewport_size();
		let (vw, vh) = (f32::from(vp.width), f32::from(vp.height));
		let s = window.scale_factor();
		let phys = ((vw * s).round() as i32, (vh * s).round() as i32);
		if self.probes.is_some() && phys != self.last_viewport {
			self.last_viewport = phys;
			app_log!("[APP:VIEWPORT: {}x{}]", phys.0, phys.1);
		}
		// A context menu keeps its list looking focused while it is open.
		let menu_origin = self.chrome.menu.as_ref().map(|m| m.origin);
		self.left_active = self.tree_focus.is_focused(window)
			|| menu_origin == Some(crate::menu::MenuOrigin::Left);
		self.log_active = self.log_focus.is_focused(window)
			|| menu_origin == Some(crate::menu::MenuOrigin::Log);
		if !self.left_active && !self.chrome.speed.is_empty() {
			// IntelliJ ends speed search when the list loses focus.
			self.chrome.speed.clear();
		}
		self.reader_active = self.reader_focus.is_focused(window);
		let left_w = self.effective_left_w(vw);
		let bottom_h = self.effective_bottom_h(vh);

		let center = if !self.workspace_open && self.paste_preview.is_none() {
			self.render_workspace_closed()
		} else {
			match self.paste_preview {
				Some(ref plan) => self.render_paste(plan, cx),
				None if self.paste_loading => self.render_paste_loading(cx),
				None => self.render_editor(cx),
			}
		};

		div()
			.track_focus(&self.focus_handle)
			.key_context("Workbench")
			.on_action(cx.listener(|this, _: &Quit, _, cx| {
				this.begin_quit(cx);
			}))
			.on_action(cx.listener(|this, _: &CloseWorkspace, _, cx| {
				this.request_user_close(
					crate::lifecycle::Intent::CloseWorkspace,
					cx,
				);
			}))
			.on_action(cx.listener(|this, _: &OpenWorkspace, _, cx| {
				this.show_workspace_picker(cx);
			}))
			.on_action(cx.listener(|this, _: &CopySelection, _, cx| {
				this.copy_selection_to_clipboard(cx)
			}))
			.on_action(cx.listener(|this, _: &PastePreview, _, cx| {
				this.trigger_paste_preview(cx)
			}))
			.on_action(cx.listener(|this, _: &ApplyPaste, _, cx| {
				this.apply_paste_restore(cx)
			}))
			.on_action(cx.listener(|this, _: &CancelPaste, _, cx| {
				this.cancel_paste_preview(cx)
			}))
			.on_action(
				cx.listener(|this, _: &Refresh, _, cx| this.reload_repos(cx)),
			)
			.on_action(cx.listener(|this, _: &DeselectAllFiles, _, cx| {
				this.deselect_all_files(cx)
			}))
			.on_action(cx.listener(|this, _: &SelectAllFiles, _, cx| {
				this.select_all_files(cx)
			}))
			.on_action(cx.listener(|this, _: &HistoryNextPage, _, cx| {
				this.history_next_page(cx)
			}))
			.on_action(cx.listener(|this, _: &HistoryPrevPage, _, cx| {
				this.history_prev_page(cx)
			}))
			.on_action(cx.listener(|this, _: &SelectRepo1, _, cx| {
				if !this.repos.is_empty() {
					this.select_repo(0, cx);
				}
			}))
			.on_action(cx.listener(|this, _: &SelectRepo2, _, cx| {
				if this.repos.len() > 1 {
					this.select_repo(1, cx);
				}
			}))
			.on_action(cx.listener(|this, _: &ToggleTab, window, cx| {
				let next = match this.active_tab {
					WorkbenchTab::GitChanges => WorkbenchTab::FileExplorer,
					WorkbenchTab::FileExplorer => WorkbenchTab::GitChanges,
				};
				this.show_tool(next, window, cx);
			}))
			.on_action(cx.listener(|_, _: &FocusNext, window, cx| {
				set_keyboard_nav(true);
				window.focus_next();
				app_log!("[APP:FOCUS: next]");
				cx.notify();
			}))
			.on_action(cx.listener(|_, _: &FocusPrev, window, cx| {
				set_keyboard_nav(true);
				window.focus_prev();
				app_log!("[APP:FOCUS: prev]");
				cx.notify();
			}))
			// Any mouse press leaves keyboard mode (focus rings off).
			.capture_any_mouse_down(cx.listener(|_, _, _, cx| {
				if set_keyboard_nav(false) {
					cx.notify();
				}
			}))
			.on_action(cx.listener(|this, _: &crate::OpenTabMenu, w, cx| {
				let (items, pos) = (this.tab_menu(), w.mouse_position());
				this.open_menu(
					crate::menu::MenuOrigin::Editor,
					items,
					pos,
					w,
					cx,
				)
			}))
			// Arrow keys count as keyboard navigation, like Tab.
			.capture_action(|_: &TreeUp, _, _| {
				set_keyboard_nav(true);
			})
			.capture_action(|_: &TreeDown, _, _| {
				set_keyboard_nav(true);
			})
			.capture_action(|_: &LogUp, _, _| {
				set_keyboard_nav(true);
			})
			.capture_action(|_: &LogDown, _, _| {
				set_keyboard_nav(true);
			})
			.on_action(cx.listener(|this, _: &crate::HideToolWindow, w, cx| {
				this.hide_active_tool(w, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::FocusEditor, w, cx| {
				this.escape_to_editor(w, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::LogParent, _, cx| {
				this.log_go(true, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::LogChild, _, cx| {
				this.log_go(false, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::LogPageUp, _, cx| {
				this.log_move(-this.log_page_rows(), false, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::LogPageDown, _, cx| {
				this.log_move(this.log_page_rows(), false, cx)
			}))
			.on_action(cx.listener(|this, _: &ShowProject, window, cx| {
				this.show_tool(WorkbenchTab::FileExplorer, window, cx)
			}))
			.on_action(cx.listener(|this, _: &ShowChanges, window, cx| {
				this.show_tool(WorkbenchTab::GitChanges, window, cx)
			}))
			.on_action(cx.listener(|this, _: &ToggleLog, window, cx| {
				this.toggle_log(cx);
				if this.bottom_visible {
					window.focus(&this.log_focus);
				}
			}))
			.on_action(cx.listener(|this, _: &OpenRepoSelector, window, cx| {
				this.open_popover(Popover::Repo, window, cx)
			}))
			.on_action(cx.listener(|this, _: &OpenRefSelector, window, cx| {
				this.open_popover(Popover::Ref, window, cx)
			}))
			.on_action(cx.listener(|this, _: &ToggleLocale, _, cx| {
				this.toggle_locale(cx)
			}))
			.on_action(cx.listener(|this, _: &FindInFile, window, cx| {
				this.open_find(false, window, cx)
			}))
			.on_action(cx.listener(|this, _: &GotoLine, window, cx| {
				this.open_find(true, window, cx)
			}))
			.on_action(
				cx.listener(|this, _: &NextDiff, _, cx| this.next_diff(cx)),
			)
			.on_action(
				cx.listener(|this, _: &PrevDiff, _, cx| this.prev_diff(cx)),
			)
			.on_action(
				cx.listener(|this, _: &FindNext, _, cx| {
					this.find_step(true, cx)
				}),
			)
			.on_action(cx.listener(|this, _: &FindPrev, _, cx| {
				this.find_step(false, cx)
			}))
			.on_action(cx.listener(|this, _: &NavUp, _, cx| {
				if let Some(ref mut p) = this.paste_preview {
					p.select_prev();
					let ix = p.selected_item_idx;
					this.select_paste_item(ix, cx);
				}
			}))
			.on_action(cx.listener(|this, _: &NavDown, _, cx| {
				if let Some(ref mut p) = this.paste_preview {
					p.select_next();
					let ix = p.selected_item_idx;
					this.select_paste_item(ix, cx);
				}
			}))
			.on_action(cx.listener(|this, _: &NavToggle, _, cx| {
				if let Some(ref p) = this.paste_preview {
					let idx = p.selected_item_idx;
					let overwrite = p
						.items
						.get(idx)
						.is_some_and(|i| i.dest_exists && !i.is_delete);
					if overwrite {
						this.toggle_paste_overwrite(idx, cx);
					} else {
						this.toggle_paste_selected(idx, cx);
					}
				}
			}))
			.on_mouse_move(cx.listener(
				|this, ev: &MouseMoveEvent, window, cx| {
					this.drag_move(ev, window, cx)
				},
			))
			.on_mouse_up(
				MouseButton::Left,
				cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_drag(cx)),
			)
			.flex()
			.flex_col()
			.size_full()
			// Islands: the frame shows through the gaps between panels.
			.bg(rgb(pal().frame_bg))
			.font_family(UI_FONT)
			.text_color(rgb(pal().text))
			.text_size(px(UI_TEXT))
			.child(self.render_header(cx))
			.child(
				div()
					.flex()
					.flex_row()
					.flex_1()
					.min_h_0()
					.pr(px(SPLITTER))
					.pb(px(SPLITTER))
					.child(self.render_rail(cx))
					.child(
						div()
							.flex()
							.flex_col()
							.flex_1()
							.min_w_0()
							.child(
								div()
									.flex()
									.flex_row()
									.flex_1()
									.min_h_0()
									.when(self.left_visible, |d| {
										d.child(self.render_left(left_w, cx))
											.child(
												self.splitter(
													Splitter::Left,
													cx,
												),
											)
									})
									.child(center),
							)
							.when(self.bottom_visible, |d| {
								d.child(self.splitter(Splitter::Bottom, cx))
									.child(self.render_log(bottom_h, cx))
							}),
					),
			)
			.child(self.render_status(cx))
			.children(self.render_context_menu(cx))
			.children(probe_frame_end(&self.probes))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn probe_bookkeeping_is_bounded_by_one_frame() {
		let mut f = ProbeFrame::default();
		for frame in 0..1000 {
			for row in 0..3 {
				f.report(&format!("tree-row:{frame}/{row}"), [0, row, 10, 10]);
			}
			f.end_frame();
			assert!(f.tracked() <= 3, "bookkeeping grew to {}", f.tracked());
		}
	}

	#[test]
	fn probe_reports_changes_and_disappearance() {
		let mut f = ProbeFrame::default();
		assert!(f.report("btn-apply", [1, 2, 3, 4]));
		assert!(f.end_frame().is_empty());
		assert!(!f.report("btn-apply", [1, 2, 3, 4]), "unchanged is quiet");
		f.end_frame();
		assert!(f.report("btn-apply", [5, 2, 3, 4]), "moved is reported");
		f.end_frame();
		assert_eq!(f.end_frame(), vec!["btn-apply".to_string()]);
		assert!(f.report("btn-apply", [5, 2, 3, 4]), "reappearing is new");
	}

	#[test]
	fn log_dates_are_relative_in_the_viewers_zone() {
		use chrono::{FixedOffset, TimeZone, Utc};
		// Viewer at UTC+8; now is 2026-09-28 10:00 UTC = 18:00 local.
		let tz = FixedOffset::east_opt(8 * 3600).unwrap();
		let now = Utc.with_ymd_and_hms(2026, 9, 28, 10, 0, 0).unwrap();
		let d = |iso: &str, loc| log_date_in(iso, loc, now, &tz);
		// Shown in the viewer's zone, whatever zone the commit was made in.
		assert_eq!(d("2026-09-28T00:05:00+00:00", Locale::En), "Today 08:05");
		assert_eq!(d("2026-09-27T15:59:00Z", Locale::ZhTw), "昨天 23:59");
		// 16:30 UTC on the 27th is already the 28th at +08:00.
		assert_eq!(d("2026-09-27T16:30:00Z", Locale::En), "Today 00:30");
		// A commit at 02:00 on the 29th in +14:00 is the 28th at 20:00 here.
		assert_eq!(d("2026-09-29T02:00:00+14:00", Locale::En), "Today 20:00");
		assert_eq!(
			d("2026-08-19T20:57:00+08:00", Locale::En),
			"8/19/26, 20:57"
		);
		assert_eq!(
			d("2025-08-19T12:57:00+00:00", Locale::ZhTw),
			"2025/8/19 20:57"
		);
		assert_eq!(d("garbage", Locale::En), "garbage");
		// The real local zone formats without panicking.
		assert!(!log_date("2026-09-28T00:05:00+00:00", Locale::En).is_empty());
	}

	#[test]
	fn test_disambiguate_candidate_labels() {
		let unique =
			vec![PathBuf::from("/a/repo-one"), PathBuf::from("/b/repo-two")];
		assert_eq!(
			WorkbenchModel::disambiguate_candidate_labels(&unique),
			vec!["repo-one", "repo-two"]
		);

		let dups = vec![
			PathBuf::from("/tmp/projA/lib"),
			PathBuf::from("/tmp/projB/lib"),
			PathBuf::from("/other/standalone"),
		];
		assert_eq!(
			WorkbenchModel::disambiguate_candidate_labels(&dups),
			vec!["projA/lib", "projB/lib", "standalone"]
		);
	}
}
