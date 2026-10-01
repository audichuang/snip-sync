//! IntelliJ-style workbench layout: header (repo / ref selectors), tool
//! window rail, Project / Changes tool window, editor (reader, commit files,
//! or paste preview), bottom Git Log and status bar.
//!
//! State and behaviour live on `WorkbenchModel`; this module renders it and
//! wires real controls to those methods. Long lists use `uniform_list`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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
use crate::paste::{
	split_dir, PasteItem, PasteNode, PastePreviewPlan, RowAction, SkipCause,
};
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

mod changes;
mod editor;
mod header;
mod left;
mod log;
mod log_view;
mod paste;

use changes::*;
pub(crate) use changes::{
	change_rows, commit_file_rows, ChangeItemRow, ChangeLayout,
};
pub use log::LogMenu;
use log::*;

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
	/// Test only: probes on without the E2E environment.
	#[cfg(test)]
	pub fn for_test() -> Self {
		Self(Rc::default())
	}

	/// Test only: every control drawn in the last frames.
	#[cfg(test)]
	pub fn drawn(&self) -> Vec<String> {
		let f = self.0.borrow();
		let mut ids: Vec<String> =
			f.shown.keys().chain(f.seen.keys()).cloned().collect();
		ids.sort();
		ids.dedup();
		ids
	}

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

/// `Ctrl+Shift+X` as macOS prints it (`⇧⌘X`); unchanged elsewhere.
fn mac_keys(text: &str) -> String {
	if cfg!(target_os = "macos") {
		text.replace("Ctrl+Shift+", "⇧⌘")
	} else {
		text.to_string()
	}
}

/// A popup row styled like the context menu's items: no frame, hover fill.
fn menu_row(id: impl Into<ElementId>, tab: isize) -> Stateful<Div> {
	div()
		.id(id)
		.relative()
		.flex()
		.flex_row()
		.items_center()
		.gap(px(8.))
		.min_h(px(ROW_H + 4.))
		.px(px(8.))
		.rounded(px(4.))
		.border_1()
		.border_color(transparent_black())
		.cursor_pointer()
		.tab_index(tab)
		.hover(|s| s.bg(rgb(pal().selection_bg)))
		.map(focus_ring)
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
pub(crate) fn paste_op(item: &PasteItem) -> (&'static str, u32, &'static str) {
	paste_style(item.action())
}

pub(crate) fn paste_style(
	action: RowAction,
) -> (&'static str, u32, &'static str) {
	use RowAction::*;
	match action {
		Excluded { by_commit: true } => {
			("op_excluded", pal().text_disabled, "reason_commit_excluded")
		}
		Excluded { by_commit: false } => {
			("op_excluded", pal().text_disabled, "reason_excluded")
		}
		Skip(cause) => ("op_skip", pal().text_disabled, skip_reason_key(cause)),
		Delete => ("op_delete", pal().git_deleted, "reason_delete"),
		DeleteMissing => {
			("op_skip", pal().text_disabled, "reason_delete_missing")
		}
		Create => ("op_create", pal().git_added, "reason_create"),
		Overwrite => ("op_overwrite", pal().git_modified, "reason_overwrite"),
		OverwritePending => (
			"op_overwrite_pending",
			pal().git_modified,
			"reason_commit_overwrite_pending",
		),
		KeepExisting => ("op_skip", pal().git_modified, "reason_exists"),
		CommitRefused(conflict) => (
			"op_refused",
			pal().error,
			crate::paste::layout_conflict_key(conflict),
		),
	}
}

fn skip_reason_key(cause: SkipCause) -> &'static str {
	use SkipCause::*;
	match cause {
		Binary => "reason_nc_binary",
		NonUtf8 => "reason_nc_non_utf8",
		NonUtf8Path => "reason_nc_non_utf8_path",
		UnsupportedType => "reason_nc_unsupported",
		Unreadable => "reason_nc_unreadable",
		UnsafePath => "reason_skip_unsafe_path",
		NonUtf8Target => "reason_skip_non_utf8_target",
		Other => "reason_skip_generic",
	}
}

/// The three strings a commit header draws: subject (first message line,
/// or the placeholder), `name <email>` and the short date.
pub(crate) fn commit_header_labels(
	commit: &snip_core::commits::CommitPlan,
	loc: Locale,
) -> (String, String, String) {
	let subject = commit.message.lines().next().unwrap_or("").trim();
	let subject = if subject.is_empty() {
		t("commit_no_message", loc).to_string()
	} else {
		subject.to_string()
	};
	let subject = if commit.refused_by().is_some() {
		format!("{subject} ({})", t("commit_header_refused_suffix", loc))
	} else {
		subject
	};
	let author = format!("{} <{}>", commit.author_name, commit.author_email);
	(subject, author, short_date(&commit.author_date))
}

fn short_date(iso: &str) -> String {
	iso.get(..16).unwrap_or(iso).replace('T', " ")
}

fn short(sha: &str) -> &str {
	sha.get(..7).unwrap_or(sha)
}

/// One row of the Project tool window.
enum ProjRow {
	/// Repo index and its indent depth in the workspace tree.
	Repo(usize, usize),
	Work(FlattenedTreeRow),
	/// A workspace-tree row outside every repo.
	Ws(FlattenedTreeRow),
	Rev(RevRow),
}

/// Repos shown where their folder sits in the workspace tree, keyed by that
/// folder's path relative to `ws_root`. A repo inside another repo below
/// `ws_root` (or outside the workspace) is not reachable there and stays a
/// top-level row; a repo at `ws_root` itself is the tree's root row.
fn placed_repos(
	repos: &[crate::RepoEntry],
	ws_root: &std::path::Path,
) -> HashMap<String, usize> {
	let mut out = HashMap::new();
	for (idx, repo) in repos.iter().enumerate() {
		let Some(rel) = repo
			.root
			.strip_prefix(ws_root)
			.ok()
			.and_then(|rel| rel.to_str())
			.filter(|rel| !rel.is_empty())
		else {
			continue;
		};
		let nested = repos.iter().any(|other| {
			other.root != repo.root
				&& repo.root.starts_with(&other.root)
				&& other.root != ws_root
				&& other.root.starts_with(ws_root)
		});
		if !nested {
			out.insert(rel.replace('\\', "/"), idx);
		}
	}
	out
}

impl WorkbenchModel {
	pub fn selected_count(&self) -> usize {
		match self.active_tab {
			WorkbenchTab::GitChanges => {
				self.files.iter().filter(|f| f.selected).count()
			}
			WorkbenchTab::FileExplorer => {
				let mut paths = Vec::new();
				for tree in
					[&self.file_tree, &self.ws_tree].into_iter().flatten()
				{
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
		self.paste.forget_log_restore();
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
			Splitter::LogFiles => {
				// The log island ends at the status bar.
				let want = vh - STATUS_H - f32::from(ev.position.y);
				self.log_details_h = Some(
					want.min(self.effective_bottom_h(vh) * 0.8)
						.max(LOG_DETAILS_H_MIN),
				);
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
		// The workspace folder's tree: its own, or the open workspace
		// repo's. Until it is read every repo is a top-level row.
		let home = self.ws_repo_idx();
		let (tree, ws) = match (&self.ws_tree, home) {
			(Some(t), _) => (Some(t), true),
			(None, Some(idx)) if self.selected_repo_idx == Some(idx) => {
				(self.file_tree.as_ref(), false)
			}
			_ => (None, true),
		};
		let mut body = Vec::new();
		// Repos given a row inside the tree; the rest are top-level rows.
		let mut shown = Vec::new();
		if let Some(t) = tree.filter(|t| t.is_loaded) {
			let placed = placed_repos(&self.repos, &t.full_path);
			let open = match home {
				Some(idx) => {
					shown.push(idx);
					body.push(ProjRow::Repo(idx, 0));
					self.repo_row_open(idx)
				}
				None => true,
			};
			// A folded root hides the repos inside it too.
			if !open {
				shown.extend(placed.values().copied());
			}
			for mut row in t
				.flatten_visible(t.visible_limit())
				.into_iter()
				.filter(|_| open)
			{
				// A plain workspace's children are top-level rows.
				if home.is_none() {
					row.depth = row.depth.saturating_sub(1);
				}
				match placed.get(&row.rel_path).filter(|_| row.is_dir) {
					Some(&idx) => {
						shown.push(idx);
						self.push_repo_rows(idx, row.depth, &mut body);
					}
					None if ws => body.push(ProjRow::Ws(row)),
					None => body.push(ProjRow::Work(row)),
				}
			}
		}
		let mut out = Vec::new();
		for idx in 0..self.repos.len() {
			if !shown.contains(&idx) {
				self.push_repo_rows(idx, 0, &mut out);
			}
		}
		out.append(&mut body);
		out
	}

	/// Shift-click: the rows from the cursor to `ix` that belong to the
	/// same tree become the Project selection. The cursor stays the anchor.
	pub(crate) fn select_tree_range(
		&mut self,
		ix: usize,
		ws: bool,
		cx: &mut Context<Self>,
	) {
		let rows = self.project_rows();
		let (from, to) = (self.tree_cursor.min(ix), self.tree_cursor.max(ix));
		let rels: Vec<String> = rows
			.into_iter()
			.skip(from)
			.take(to.saturating_sub(from) + 1)
			.filter_map(|row| match row {
				ProjRow::Ws(r) if ws => Some(r),
				ProjRow::Work(r) if !ws => Some(r),
				_ => None,
			})
			.filter(|r| {
				r.is_valid_utf8
					&& !(r.is_error
						|| r.is_loading || r.is_more_marker
						|| r.is_truncation_marker
						|| r.is_view_limit)
			})
			.map(|r| r.rel_path)
			.collect();
		self.select_tree_rows_alone(ws, &rels, cx);
	}

	/// Whether a repo row shows its files. The workspace repo's row stays
	/// open while another repo is open: the repos inside sit in its tree.
	pub(crate) fn repo_row_open(&self, idx: usize) -> bool {
		if self.selected_repo_idx == Some(idx) {
			return !self.repo_collapsed;
		}
		self.ws_tree.is_some()
			&& self.ws_repo_idx() == Some(idx)
			&& !self.ws_collapsed
	}

	/// A repo row and, when it is the open expanded repo, its tree.
	fn push_repo_rows(&self, idx: usize, depth: usize, out: &mut Vec<ProjRow>) {
		out.push(ProjRow::Repo(idx, depth));
		if self.selected_repo_idx == Some(idx) && !self.repo_collapsed {
			if let Some(t) = &self.file_tree {
				out.extend(
					t.flatten_visible(t.visible_limit()).into_iter().map(
						|mut row| {
							row.depth += depth;
							ProjRow::Work(row)
						},
					),
				);
			}
		}
	}

	fn change_item_rows(&self) -> Vec<ChangeItemRow> {
		change_rows(
			&self.change_repos,
			&self.files,
			|group| self.group_collapsed(group),
			|slot, group| self.repo_changes_collapsed(slot, group),
			&self.chrome.speed,
			&ChangeLayout {
				by_dir: self.chrome.changes_by_dir,
				expanded: |slot, group, dir| {
					self.change_dir_expanded(slot, group, dir)
				},
			},
		)
	}

	/// Puts the keyboard row on the selected change (else the first change),
	/// never on a group header where Space/Enter do nothing.
	pub(crate) fn sync_list_row(&mut self) {
		let rows = self.change_item_rows();
		let file_row = |pred: &dyn Fn(&crate::FileChangeItem) -> bool| {
			rows.iter().position(|row| {
				matches!(row, ChangeItemRow::File { file_idx, .. }
					if self.files.get(*file_idx).is_some_and(pred))
			})
		};
		let shown_slot = self.preview_root().and_then(|root| {
			self.change_repos.iter().position(|s| s.root == root)
		});
		let row = file_row(&|f| {
			Some(f.repo as usize) == shown_slot
				&& self.selected_file.as_deref() == Some(f.path.as_str())
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
			if let Some(ChangeItemRow::File { file_idx, .. }) =
				self.change_item_rows().get(row)
			{
				self.select_change(*file_idx, cx);
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
					ChangeItemRow::Repo { slot, .. } => self
						.change_repos
						.get(slot)
						.map(|s| s.name.clone())
						.unwrap_or_default(),
					ChangeItemRow::Note { .. } => String::new(),
					ChangeItemRow::Header { label, .. } => {
						t(label, self.locale).to_string()
					}
					ChangeItemRow::Dir { name, .. } => name,
					ChangeItemRow::File { file_idx, .. } => self
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
					ProjRow::Repo(i, _) => self.repos[i].name.clone(),
					ProjRow::Work(w) | ProjRow::Ws(w) => w.name,
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
			// IntelliJ: Left on a leaf or a collapsed node goes to its parent.
			let parent = rows[cur].depth().and_then(|d| {
				rows[..cur]
					.iter()
					.rposition(|r| r.depth().is_some_and(|p| p < d))
			});
			match (&rows[cur], action) {
				(ChangeItemRow::Repo { slot, group_id, .. }, _) => {
					let (slot, group) = (*slot, *group_id);
					let collapsed = self.repo_changes_collapsed(slot, group);
					match action {
						"toggle" => self.toggle_change_repo(slot, group, cx),
						"open" => self.toggle_repo_collapsed(slot, group, cx),
						"expand" if collapsed => {
							self.toggle_repo_collapsed(slot, group, cx)
						}
						"collapse" if !collapsed => {
							self.toggle_repo_collapsed(slot, group, cx)
						}
						"collapse" => {
							if let Some(p) = parent {
								self.set_tool_cursor(p, cx);
							}
						}
						_ => {}
					}
				}
				(
					ChangeItemRow::Dir {
						slot,
						group_id,
						path,
						..
					},
					_,
				) => {
					let (slot, group) = (*slot, *group_id);
					let collapsed =
						!self.change_dir_expanded(slot, group, path);
					match action {
						"toggle" => {
							self.toggle_change_dir(slot, group, path, cx)
						}
						"open" => {
							self.toggle_dir_collapsed(slot, group, path, cx)
						}
						"expand" if collapsed => {
							self.toggle_dir_collapsed(slot, group, path, cx)
						}
						"collapse" if !collapsed => {
							self.toggle_dir_collapsed(slot, group, path, cx)
						}
						"collapse" => {
							if let Some(p) = parent {
								self.set_tool_cursor(p, cx);
							}
						}
						_ => {}
					}
				}
				(ChangeItemRow::Note { .. }, _) => {}
				(ChangeItemRow::Header { group_id, .. }, _) => {
					let group = *group_id;
					let collapsed = self.group_collapsed(group);
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
				(ChangeItemRow::File { file_idx, .. }, "toggle") => {
					self.toggle_file(*file_idx, cx);
				}
				(ChangeItemRow::File { .. }, "open") => {
					self.set_tool_cursor(cur, cx);
				}
				(ChangeItemRow::File { .. }, "collapse") => {
					if let Some(p) = parent {
						self.set_tool_cursor(p, cx);
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
			ProjRow::Repo(i, _) => {
				let i = *i;
				if action == "open" || action == "toggle" {
					self.toggle_repo_row(i, cx);
				} else if expand || collapse {
					self.set_repo_row_expanded(i, expand, cx);
				}
			}
			ProjRow::Work(r) | ProjRow::Ws(r) => {
				let ws = matches!(&rows[self.tree_cursor], ProjRow::Ws(_));
				let gesture = match action {
					"toggle" => Some(RowGesture::Toggle),
					"expand" => Some(RowGesture::Expand),
					"collapse" => Some(RowGesture::Collapse),
					"open" => Some(RowGesture::Primary),
					_ => None,
				};
				if let Some(gesture) = gesture {
					let cmd = command_for_row(r, gesture);
					if ws {
						self.dispatch_ws_tree(cmd, cx);
					} else {
						self.dispatch_tree(cmd, cx);
					}
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
			Splitter::LogFiles => "splitter-log-files",
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
			Splitter::LogFiles => d
				.h(px(SPLITTER))
				.w_full()
				.border_t_1()
				.border_color(rgb(pal().divider))
				.cursor_ns_resize(),
		}
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

		let center = if !self.workspace_open && self.paste.plan().is_none() {
			self.render_workspace_closed(cx)
		} else {
			match self.paste.plan() {
				Some(plan) => self.render_paste(plan, cx),
				None if self.paste.is_loading() => {
					self.render_paste_loading(cx)
				}
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
				this.open_folder_dialog(cx);
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
				this.step_paste_selection(false, cx);
			}))
			.on_action(cx.listener(|this, _: &NavDown, _, cx| {
				this.step_paste_selection(true, cx);
			}))
			.on_action(cx.listener(|this, _: &NavToggle, _, cx| {
				if let Some(p) = this.paste.plan() {
					let idx = p.selected_item_idx;
					if !p.display_order().contains(&idx) {
						// Folded away: the row is not on screen.
						return;
					}
					let overwrite =
						p.items.get(idx).is_some_and(PasteItem::overwritable);
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
			.children(self.render_toast())
			.children(self.render_context_menu(cx))
			.children(probe_frame_end(&self.probes))
	}
}

impl WorkbenchModel {
	/// The copy result card: bottom center, above the status bar.
	fn render_toast(&self) -> Option<AnyElement> {
		let (_, ok, msg) = self.toast.as_ref()?;
		let (glyph, accent) = if *ok {
			(Icon::Apply, pal().diff_add_bg)
		} else {
			(Icon::Error, pal().diff_removed_bg)
		};
		Some(
			div()
				.absolute()
				.bottom(px(STATUS_H + 16.))
				.left_0()
				.right_0()
				.flex()
				.justify_center()
				.child(
					// No occlude: the card only informs, clicks reach the
					// rows under it.
					div()
						.id("copy-toast")
						.flex()
						.flex_row()
						.items_center()
						.gap(px(10.))
						.max_w(px(640.))
						.px(px(16.))
						.py(px(10.))
						.bg(rgb(pal().popup_bg))
						.border_1()
						.border_color(rgb(accent))
						.border_l_4()
						.rounded(px(8.))
						.shadow_lg()
						.text_size(px(UI_TEXT))
						.text_color(rgb(pal().text))
						.child(icon(glyph, 18.))
						.child(msg.render(self.locale))
						.children(probe(&self.probes, "copy-toast")),
				)
				.into_any_element(),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The workspace tree lists plain folders and files; repo folders are
	/// placed as repo rows, a repo inside a repo stays top-level, and a
	/// workspace spelled through a symlink still matches Git's toplevels.
	#[cfg(unix)]
	#[test]
	fn workspace_tree_mixes_plain_entries_and_placed_repos() {
		use crate::tree::{FileTreeNode, NodeKey};
		use std::fs;
		let dir = tempfile::tempdir().unwrap();
		let real = dunce::canonicalize(dir.path()).unwrap().join("ws");
		for d in ["app/.git", "app/sub/.git", "group/lib/.git", "notes"] {
			fs::create_dir_all(real.join(d)).unwrap();
		}
		fs::write(real.join("notes/readme.txt"), "n\n").unwrap();
		fs::write(real.join("group/plain.txt"), "p\n").unwrap();
		fs::write(real.join("group/lib/x.rs"), "x\n").unwrap();
		fs::write(real.join("top.txt"), "t\n").unwrap();
		let link = dir.path().join("ws-link");
		std::os::unix::fs::symlink(&real, &link).unwrap();

		let repo = |rel: &str| crate::RepoEntry {
			root: real.join(rel),
			name: rel.rsplit('/').next().unwrap().to_string(),
			kind: crate::RepoEntryKind::Main,
			identity: None,
			summary: Err("mock".into()),
		};
		let repos = vec![repo("app"), repo("app/sub"), repo("group/lib")];
		// The user-given spelling does not match Git's resolved roots.
		assert!(placed_repos(&repos, &link).is_empty());
		let root = snip_core::transfer::CanonicalRootId::new(&link).unwrap();
		let placed = placed_repos(&repos, root.path());
		assert_eq!(placed.get("app"), Some(&0));
		assert_eq!(placed.get("group/lib"), Some(&2));
		assert!(!placed.values().any(|&i| i == 1), "nested repo stays top");
		// The workspace folder itself a repo (INVI_SRC): it is the root row,
		// and the repos inside it still sit at their folders.
		let mut in_home = vec![repo("")];
		in_home.extend(repos.iter().map(|r| crate::RepoEntry {
			root: r.root.clone(),
			name: r.name.clone(),
			kind: crate::RepoEntryKind::Main,
			identity: None,
			summary: Err("mock".into()),
		}));
		in_home[0].root = root.path().to_path_buf();
		let placed = placed_repos(&in_home, root.path());
		assert_eq!(placed.get("app"), Some(&1));
		assert_eq!(placed.get("group/lib"), Some(&3));
		assert_eq!(placed.len(), 2, "{placed:?}");

		let mut tree = FileTreeNode::new_root(root.path());
		tree.toggle_expand("group", root.path());
		let rows = tree.flatten_visible(tree.visible_limit());
		let names: Vec<_> = rows.iter().map(|r| r.rel_path.as_str()).collect();
		for want in ["app", "group", "group/lib", "group/plain.txt", "notes"] {
			assert!(names.contains(&want), "{want} missing: {names:?}");
		}
		assert!(names.contains(&"top.txt"), "{names:?}");
		// Selecting a plain folder selects it alone (Copy walks it later,
		// never into the repo inside); the repo folder is not selectable.
		let picked = tree
			.selection_for_toggle(&NodeKey::from_utf8_rel("group"))
			.unwrap();
		assert_eq!(picked, ["group"]);
		assert!(tree
			.selection_for_toggle(&NodeKey::from_utf8_rel("group/lib"))
			.is_none());
	}

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
	fn path_picker_lists_loaded_folders_and_opens_expanded_ones() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		std::fs::create_dir_all(root.join("src/app")).unwrap();
		std::fs::write(root.join("src/lib.rs"), "").unwrap();
		std::fs::write(root.join("README.md"), "").unwrap();
		let mut tree = crate::tree::FileTreeNode::new_root(root);
		let names = |rows: Vec<PathPick>| -> Vec<String> {
			rows.into_iter()
				.map(|p| format!("{}{}", " ".repeat(p.depth), p.rel))
				.collect()
		};
		// `src` is not read yet: it can be opened, but shows nothing yet.
		let rows = path_picker_rows(&tree, &["src".into()], 10);
		assert!(rows.iter().any(|p| p.rel == "src" && p.expandable));
		assert_eq!(rows.len(), 2);
		tree.toggle_expand("src", root);
		let closed = path_picker_rows(&tree, &[], 10);
		assert!(closed.iter().any(|p| p.rel == "src" && p.expandable));
		assert_eq!(closed.len(), 2);
		let open = names(path_picker_rows(&tree, &["src".into()], 10));
		assert!(open.contains(&" src/app".to_string()), "{open:?}");
		assert!(open.contains(&" src/lib.rs".to_string()), "{open:?}");
		assert_eq!(path_picker_rows(&tree, &["src".into()], 3).len(), 3);
	}

	#[test]
	fn path_picker_filters_the_loaded_tree_by_typed_text() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		std::fs::create_dir_all(root.join("src/Query")).unwrap();
		std::fs::create_dir_all(root.join("docs")).unwrap();
		std::fs::write(root.join("src/Query/run.rs"), "").unwrap();
		std::fs::write(root.join("src/lib.rs"), "").unwrap();
		std::fs::write(root.join("docs/query.md"), "").unwrap();
		std::fs::write(root.join("README.md"), "").unwrap();
		let mut tree = crate::tree::FileTreeNode::new_root(root);
		for d in ["src", "src/Query", "docs"] {
			tree.toggle_expand(d, root);
		}
		let shape = |rows: Vec<PathPick>| -> Vec<String> {
			rows.into_iter()
				.map(|p| {
					format!(
						"{}{}{}",
						" ".repeat(p.depth),
						p.rel,
						if p.expanded { "/" } else { "" }
					)
				})
				.collect()
		};
		// Case-insensitive, on the path; ancestors kept and shown open,
		// the rest of the tree left out.
		let hits = shape(path_picker_matches(&tree, "query", "", 50));
		assert_eq!(
			hits,
			[
				"docs/",
				" docs/query.md",
				"src/",
				" src/Query/",
				"  src/Query/run.rs"
			],
			"{hits:?}"
		);
		assert!(path_picker_matches(&tree, "nothing", "", 50).is_empty());
		// Bounded by rows; the merged log names the repository first.
		assert_eq!(path_picker_matches(&tree, "query", "", 2).len(), 2);
		let merged = path_picker_matches(&tree, "readme", "repo/", 50);
		assert_eq!(shape(merged), ["repo/README.md"]);
		// The merged log's needle can name the repository, as its picks do.
		let merged = path_picker_matches(&tree, "repo/src/q", "repo/", 50);
		assert_eq!(
			shape(merged),
			["repo/src/", " repo/src/Query/", "  repo/src/Query/run.rs"]
		);
		// The filter decides what shows: its folders cannot be collapsed.
		assert!(path_picker_matches(&tree, "query", "", 50)
			.iter()
			.all(|p| !p.expandable));
	}

	#[test]
	fn details_list_a_few_branches_until_show_all() {
		use super::log_view::branches_list;
		let names: Vec<String> = (1..=7).map(|i| format!("b{i}")).collect();
		assert_eq!(
			branches_list(&names[..2], false, false),
			("b1, b2".to_string(), false)
		);
		assert_eq!(
			branches_list(&names, false, false),
			("b1, b2, b3, b4, b5, …".to_string(), true)
		);
		assert_eq!(
			branches_list(&names, false, true),
			("b1, b2, b3, b4, b5, b6, b7".to_string(), false)
		);
		// The read itself was cut: more exist than Show all can list.
		assert_eq!(
			branches_list(&names[..2], true, true),
			("b1, b2, …".to_string(), false)
		);
	}

	#[test]
	fn branch_popup_splits_local_and_remotes_and_flattens_a_filter() {
		let refs: Vec<_> = [
			"refs/heads/main",
			"refs/heads/feat/Login",
			"refs/remotes/origin/HEAD",
			"refs/remotes/origin/main",
			"refs/remotes/origin/feat/login",
			"refs/remotes/upstream/main",
			// A second repository of the merged log has `main` too.
			"refs/heads/main",
		]
		.iter()
		.map(|n| snip_core::browser::GitReference {
			name: n.to_string(),
			sha: "a".into(),
		})
		.collect();
		let shape = |rows: Vec<BranchRow>| -> Vec<String> {
			rows.into_iter()
				.map(|r| match r {
					BranchRow::Group {
						key,
						depth,
						collapsed,
						..
					} => format!(
						"{depth}G {key}{}",
						if collapsed { " ›" } else { "" }
					),
					BranchRow::Ref { label, depth, .. } => {
						format!("{depth} {label}")
					}
				})
				.collect()
		};
		// Closed sections: Local and one per remote, no Remote parent.
		assert_eq!(
			shape(branch_popup_rows(&refs, "", &[], Locale::En)),
			[
				"0G refs_local ›",
				"0G remote:origin ›",
				"0G remote:upstream ›"
			]
		);
		let open = ["refs_local".to_string(), "remote:origin".to_string()];
		assert_eq!(
			shape(branch_popup_rows(&refs, "", &open, Locale::En)),
			[
				"0G refs_local",
				"1 main",
				"1 feat/Login",
				"0G remote:origin",
				"1 main",
				"1 feat/login",
				"0G remote:upstream ›",
			]
		);
		// Typing: every match, local and remote, flat and case-insensitive.
		assert_eq!(
			shape(branch_popup_rows(&refs, "login", &[], Locale::En)),
			["0 feat/Login", "0 origin/feat/login"]
		);
	}

	#[test]
	fn remote_branches_group_by_remote_name() {
		let refs: Vec<_> = [
			"refs/heads/main",
			"refs/remotes/upstream/main",
			"refs/remotes/origin/HEAD",
			"refs/remotes/origin/main",
			"refs/remotes/origin/feature/x",
			"refs/tags/v1",
		]
		.iter()
		.map(|n| snip_core::browser::GitReference {
			name: n.to_string(),
			sha: "a".into(),
		})
		.collect();
		let shape = |rows: Vec<BranchRow>| -> Vec<String> {
			rows.into_iter()
				.map(|r| match r {
					BranchRow::Group {
						key,
						depth,
						collapsed,
						..
					} => format!(
						"{depth}G {key}{}",
						if collapsed { " +" } else { "" }
					),
					BranchRow::Ref { label, depth, .. } => {
						format!("{depth} {label}")
					}
				})
				.collect()
		};
		assert_eq!(
			shape(branch_rows(&refs, "", &[], Locale::En)),
			[
				"0G refs_local",
				"1 main",
				"0G refs_remote",
				"1G remote:origin",
				"2 main",
				"2 feature/x",
				"1G remote:upstream",
				"2 main",
				"0G refs_tags",
				"1 v1",
			]
		);
		let collapsed = ["remote:origin".to_string(), "refs_tags".to_string()];
		assert_eq!(
			shape(branch_rows(&refs, "main", &collapsed, Locale::En)),
			[
				"0G refs_local",
				"1 main",
				"0G refs_remote",
				"1G remote:origin +",
				"1G remote:upstream",
				"2 main",
			]
		);
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

	#[test]
	fn paste_style_maps_each_action_to_its_key_and_colour() {
		use super::paste_style;
		use crate::paste::{RowAction, SkipCause};
		let pal = crate::theme::pal();
		assert_eq!(
			paste_style(RowAction::Excluded { by_commit: true }),
			("op_excluded", pal.text_disabled, "reason_commit_excluded")
		);
		assert_eq!(
			paste_style(RowAction::Excluded { by_commit: false }),
			("op_excluded", pal.text_disabled, "reason_excluded")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::Binary)),
			("op_skip", pal.text_disabled, "reason_nc_binary")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::NonUtf8)),
			("op_skip", pal.text_disabled, "reason_nc_non_utf8")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::NonUtf8Path)),
			("op_skip", pal.text_disabled, "reason_nc_non_utf8_path")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::UnsupportedType)),
			("op_skip", pal.text_disabled, "reason_nc_unsupported")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::Unreadable)),
			("op_skip", pal.text_disabled, "reason_nc_unreadable")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::UnsafePath)),
			("op_skip", pal.text_disabled, "reason_skip_unsafe_path")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::NonUtf8Target)),
			("op_skip", pal.text_disabled, "reason_skip_non_utf8_target")
		);
		assert_eq!(
			paste_style(RowAction::Skip(SkipCause::Other)),
			("op_skip", pal.text_disabled, "reason_skip_generic")
		);
		assert_eq!(
			paste_style(RowAction::Delete),
			("op_delete", pal.git_deleted, "reason_delete")
		);
		assert_eq!(
			paste_style(RowAction::DeleteMissing),
			("op_skip", pal.text_disabled, "reason_delete_missing")
		);
		assert_eq!(
			paste_style(RowAction::Create),
			("op_create", pal.git_added, "reason_create")
		);
		assert_eq!(
			paste_style(RowAction::Overwrite),
			("op_overwrite", pal.git_modified, "reason_overwrite")
		);
		assert_eq!(
			paste_style(RowAction::OverwritePending),
			(
				"op_overwrite_pending",
				pal.git_modified,
				"reason_commit_overwrite_pending"
			)
		);
		assert_eq!(
			paste_style(RowAction::KeepExisting),
			("op_skip", pal.git_modified, "reason_exists")
		);
		use snip_core::commits::LayoutConflict;
		assert_eq!(
			paste_style(RowAction::CommitRefused(
				LayoutConflict::RenamedFromIsDirectory
			)),
			("op_refused", pal.error, "reason_refused_renamed_from_dir")
		);
		assert_eq!(
			paste_style(RowAction::CommitRefused(
				LayoutConflict::DeleteTargetIsDirectory
			)),
			("op_refused", pal.error, "reason_refused_delete_dir")
		);
		assert_eq!(
			paste_style(RowAction::CommitRefused(
				LayoutConflict::DirectoryInTheWay
			)),
			("op_refused", pal.error, "reason_refused_dir_in_way")
		);
		assert_eq!(
			paste_style(RowAction::CommitRefused(
				LayoutConflict::FileInTheWayOfParent
			)),
			("op_refused", pal.error, "reason_refused_file_in_way")
		);

		fn all_skip_causes() -> Vec<SkipCause> {
			// 每個變體各放一個代表值；新增變體時一併加入 seed，否則下方分支不會執行
			let seed = [
				SkipCause::Binary,
				SkipCause::NonUtf8,
				SkipCause::NonUtf8Path,
				SkipCause::UnsupportedType,
				SkipCause::Unreadable,
				SkipCause::UnsafePath,
				SkipCause::NonUtf8Target,
				SkipCause::Other,
			];
			let mut out = Vec::new();
			for cause in seed {
				match cause {
					SkipCause::Binary => out.push(SkipCause::Binary),
					SkipCause::NonUtf8 => out.push(SkipCause::NonUtf8),
					SkipCause::NonUtf8Path => out.push(SkipCause::NonUtf8Path),
					SkipCause::UnsupportedType => {
						out.push(SkipCause::UnsupportedType)
					}
					SkipCause::Unreadable => out.push(SkipCause::Unreadable),
					SkipCause::UnsafePath => out.push(SkipCause::UnsafePath),
					SkipCause::NonUtf8Target => {
						out.push(SkipCause::NonUtf8Target)
					}
					SkipCause::Other => out.push(SkipCause::Other),
				}
			}
			out
		}

		fn all_actions() -> Vec<RowAction> {
			// 每個變體各放一個代表值；新增變體時一併加入 seed，否則下方分支不會執行
			let seed = [
				RowAction::Excluded { by_commit: false },
				RowAction::Skip(SkipCause::Other),
				RowAction::Delete,
				RowAction::DeleteMissing,
				RowAction::Create,
				RowAction::Overwrite,
				RowAction::OverwritePending,
				RowAction::KeepExisting,
				RowAction::CommitRefused(
					LayoutConflict::RenamedFromIsDirectory,
				),
			];
			let mut out = Vec::new();
			for action in seed {
				match action {
					RowAction::Excluded { .. } => {
						out.push(RowAction::Excluded { by_commit: true });
						out.push(RowAction::Excluded { by_commit: false });
					}
					RowAction::Skip(_) => {
						for cause in all_skip_causes() {
							out.push(RowAction::Skip(cause));
						}
					}
					RowAction::Delete => out.push(RowAction::Delete),
					RowAction::DeleteMissing => {
						out.push(RowAction::DeleteMissing)
					}
					RowAction::Create => out.push(RowAction::Create),
					RowAction::Overwrite => out.push(RowAction::Overwrite),
					RowAction::OverwritePending => {
						out.push(RowAction::OverwritePending)
					}
					RowAction::KeepExisting => {
						out.push(RowAction::KeepExisting)
					}
					RowAction::CommitRefused(_) => {
						out.push(RowAction::CommitRefused(
							LayoutConflict::RenamedFromIsDirectory,
						));
						out.push(RowAction::CommitRefused(
							LayoutConflict::DeleteTargetIsDirectory,
						));
						out.push(RowAction::CommitRefused(
							LayoutConflict::DirectoryInTheWay,
						));
						out.push(RowAction::CommitRefused(
							LayoutConflict::FileInTheWayOfParent,
						));
					}
				}
			}
			out
		}

		for action in all_actions() {
			let (op_key, _, reason_key) = paste_style(action);
			assert!(!crate::i18n::t(op_key, Locale::ZhTw).is_empty());
			assert!(!crate::i18n::t(op_key, Locale::En).is_empty());
			assert!(!crate::i18n::t(reason_key, Locale::ZhTw).is_empty());
			assert!(!crate::i18n::t(reason_key, Locale::En).is_empty());
		}
	}
}
