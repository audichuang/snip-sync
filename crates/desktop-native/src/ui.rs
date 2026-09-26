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
	SharedString, Stateful, Window,
};
use snip_core::format::ChangeType;
use snip_core::transfer::SourceKind;
use snip_core::workspace::ScanStatus;

use crate::graph_view;
use crate::history::RevRow;
use crate::i18n::{t, tf};
use crate::icons::{icon, Icon};
use crate::paste::{PasteItem, PastePreviewPlan};
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

/// E2E only: last child of the root, so its prepaint runs after every probe
/// of the frame; announces controls that are no longer drawn.
fn probe_frame_end(probes: &Option<Probes>) -> Option<AnyElement> {
	let frame = probes.as_ref()?.0.clone();
	Some(
		canvas(
			move |_, _, _| {
				for id in frame.borrow_mut().end_frame() {
					app_log!("[APP:CTRL_GONE: id={}]", id);
				}
			},
			|_, _, _, _| {},
		)
		.absolute()
		.size_0()
		.into_any_element(),
	)
}

// ───────────────────────── small building blocks ─────────────────────────

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
			.bg(rgb(TOOLTIP_BG))
			.border_1()
			.border_color(rgb(DIVIDER))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(TEXT))
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
		(_, false) => TEXT_DISABLED,
		(Btn::Primary, true) => ACCENT_TEXT,
		_ => TEXT,
	};
	let (bg, border) = match kind {
		Btn::Primary if enabled => (Some(ACCENT), Some(ACCENT)),
		Btn::Ghost => (None, None),
		_ => (
			Some(BUTTON_BG),
			Some(if enabled { BUTTON_BORDER } else { DIVIDER }),
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
			d.hover(|s| s.bg(rgb(HOVER_BG)))
		})
		.focus(|s| s.border_color(rgb(FOCUS_RING)))
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
	// IntelliJ-style: small, thin border, accent fill with a painted check.
	div()
		.flex_shrink_0()
		.size(px(12.))
		.rounded(px(2.))
		.border_1()
		.flex()
		.items_center()
		.justify_center()
		.border_color(rgb(if checked { ACCENT } else { CHECK_BORDER }))
		.when(checked, |d| {
			d.bg(rgb(ACCENT)).child(icon(Icon::Check, 10., ACCENT_TEXT))
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
				.hover(|s| s.bg(rgb(HOVER_BG)))
		})
		.focus(|s| s.border_color(rgb(FOCUS_RING)))
		.child(icon(
			ic,
			14.,
			if enabled { TEXT_MUTED } else { TEXT_DISABLED },
		))
}

fn status_sep() -> Div {
	div().flex_shrink_0().w(px(1.)).h(px(12.)).bg(rgb(DIVIDER))
}

fn toolbar_divider() -> Div {
	div()
		.flex_shrink_0()
		.w(px(1.))
		.h(px(16.))
		.mx(px(4.))
		.bg(rgb(DIVIDER))
}

fn change_style(ct: Option<ChangeType>) -> (&'static str, u32) {
	match ct {
		Some(ChangeType::New) => ("A", GIT_ADDED),
		Some(ChangeType::Modified) => ("M", GIT_MODIFIED),
		Some(ChangeType::Deleted) => ("D", GIT_DELETED),
		Some(ChangeType::Moved) => ("R", GIT_MODIFIED),
		None => ("?", GIT_UNTRACKED),
	}
}

/// Operation label, color and reason key for a paste row.
fn paste_op(item: &PasteItem) -> (&'static str, u32, &'static str) {
	if !item.selected {
		("op_excluded", TEXT_MUTED, "reason_excluded")
	} else if item.is_delete {
		if item.dest_exists {
			("op_delete", ERROR, "reason_delete")
		} else {
			("op_skip", TEXT_MUTED, "reason_delete_missing")
		}
	} else if !item.dest_exists {
		("op_create", GIT_ADDED, "reason_create")
	} else if item.overwrite_allowed {
		("op_overwrite", WARNING, "reason_overwrite")
	} else {
		("op_skip", TEXT_MUTED, "reason_exists")
	}
}

fn short_date(iso: &str) -> String {
	iso.get(..16).unwrap_or(iso).replace('T', " ")
}

fn short(sha: &str) -> &str {
	sha.get(..7).unwrap_or(sha)
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

		let conflicted: Vec<usize> = self
			.files
			.iter()
			.enumerate()
			.filter(|(_, f)| f.is_conflict)
			.map(|(i, _)| i)
			.collect();
		if !conflicted.is_empty() {
			rows.push(ChangeItemRow::Header {
				label: "group_conflicted",
				count: conflicted.len(),
				group_id: "conflicted",
			});
			for idx in conflicted {
				rows.push(ChangeItemRow::File { file_idx: idx });
			}
		}

		let staged: Vec<usize> = self
			.files
			.iter()
			.enumerate()
			.filter(|(_, f)| !f.is_conflict && f.source == SourceKind::Staged)
			.map(|(i, _)| i)
			.collect();
		if !staged.is_empty() {
			rows.push(ChangeItemRow::Header {
				label: "group_staged",
				count: staged.len(),
				group_id: "staged",
			});
			for idx in staged {
				rows.push(ChangeItemRow::File { file_idx: idx });
			}
		}

		let unstaged: Vec<usize> = self
			.files
			.iter()
			.enumerate()
			.filter(|(_, f)| !f.is_conflict && f.source == SourceKind::Unstaged)
			.map(|(i, _)| i)
			.collect();
		if !unstaged.is_empty() {
			rows.push(ChangeItemRow::Header {
				label: "group_unstaged",
				count: unstaged.len(),
				group_id: "unstaged",
			});
			for idx in unstaged {
				rows.push(ChangeItemRow::File { file_idx: idx });
			}
		}

		let untracked: Vec<usize> = self
			.files
			.iter()
			.enumerate()
			.filter(|(_, f)| !f.is_conflict && f.source == SourceKind::Working)
			.map(|(i, _)| i)
			.collect();
		if !untracked.is_empty() {
			rows.push(ChangeItemRow::Header {
				label: "group_untracked",
				count: untracked.len(),
				group_id: "untracked",
			});
			for idx in untracked {
				rows.push(ChangeItemRow::File { file_idx: idx });
			}
		}

		rows
	}

	/// Keyboard: activate / expand / toggle the row under the tool cursor.
	fn tool_action(
		&mut self,
		action: &str,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if self.active_tab == WorkbenchTab::GitChanges {
			let rows = self.change_item_rows();
			let n = rows.len();
			if n == 0 {
				return;
			}
			match action {
				"up" | "down" => {
					let mut cur = self.selected_list_row.min(n - 1);
					if action == "down" {
						if let Some((i, _)) =
							rows.iter().enumerate().skip(cur + 1).find(
								|(_, r)| {
									matches!(r, ChangeItemRow::File { .. })
								},
							) {
							cur = i;
						}
					} else if let Some((i, _)) = rows[..cur]
						.iter()
						.enumerate()
						.rfind(|(_, r)| matches!(r, ChangeItemRow::File { .. }))
					{
						cur = i;
					}
					self.selected_list_row = cur;
					if let Some(ChangeItemRow::File { file_idx }) =
						rows.get(cur)
					{
						if let Some(f) = self.files.get(*file_idx) {
							let (p, s) = (f.path.clone(), f.source.clone());
							self.select_file_with_source(&p, s, cx);
						}
					}
				}
				"toggle" => {
					let cur = self.selected_list_row.min(n - 1);
					if let Some(ChangeItemRow::File { file_idx }) =
						rows.get(cur)
					{
						self.toggle_file(*file_idx, cx);
					}
				}
				"open" => {
					let cur = self.selected_list_row.min(n - 1);
					if let Some(ChangeItemRow::File { file_idx }) =
						rows.get(cur)
					{
						if let Some(f) = self.files.get(*file_idx) {
							let (p, s) = (f.path.clone(), f.source.clone());
							self.select_file_with_source(&p, s, cx);
						}
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
		match action {
			"up" => self.tree_cursor = self.tree_cursor.saturating_sub(1),
			"down" => self.tree_cursor = (self.tree_cursor + 1).min(last),
			_ => {
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
						if (dir
							&& ((expand && !r.expanded)
								|| (collapse && r.expanded)))
							|| action == "open"
						{
							self.rev_tree_click(&path, dir, cx);
						}
					}
					_ => {}
				}
			}
		}
		let _ = window;
		cx.notify();
	}

	fn splitter(&self, which: Splitter, cx: &mut Context<Self>) -> AnyElement {
		let id = match which {
			Splitter::Left => "splitter-left",
			Splitter::Bottom => "splitter-bottom",
		};
		let d = div()
			.id(id)
			.relative()
			.flex_shrink_0()
			.bg(rgb(BORDER))
			.hover(|s| s.bg(rgb(ACCENT)))
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
			.bg(rgb(PANEL_BG))
			.border_1()
			.border_color(rgb(BUTTON_BORDER))
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
					.hover(|s| s.bg(rgb(HOVER_BG)))
					.when(self.workspace_menu, |d| d.bg(rgb(HOVER_BG)))
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
			.bg(rgb(EDITOR_BG))
			.child(
				div()
					.max_w(px(420.))
					.px(px(16.))
					.text_color(rgb(TEXT_MUTED))
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
				.hover(|s| s.bg(rgb(HOVER_BG)))
				.when(open, |d| d.bg(rgb(HOVER_BG)))
				.focus(|s| s.border_color(rgb(FOCUS_RING)))
				.tooltip(tip(tooltip))
				.child(icon(ic, 14., TEXT_MUTED))
				.child(clip_text(label).font_weight(FontWeight::SEMIBOLD))
				.child(icon(Icon::ChevronDown, 10., TEXT_MUTED))
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
			.bg(rgb(HEADER_BG))
			.border_b_1()
			.border_color(rgb(BORDER))
			.child(
				div()
					.flex_shrink_0()
					.size(px(18.))
					.rounded(px(4.))
					.bg(rgb(ACCENT))
					.flex()
					.items_center()
					.justify_center()
					.text_size(px(10.))
					.font_weight(FontWeight::BOLD)
					.text_color(rgb(ACCENT_TEXT))
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
								Icon::Repo
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
					.gap(px(4.))
					.flex_shrink_0()
					.child(
						button(
							"btn-copy",
							format!("{} ({count})", t("btn_copy", loc)),
							Btn::Primary,
							copy_enabled,
							3,
						)
						.when(copy_enabled, |b| {
							b.on_click(cx.listener(|this, _, _, cx| {
								this.copy_selection_to_clipboard(cx)
							}))
						})
						.when_some(copy_reason, |b, r| b.tooltip(tip(r)))
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
					.child(
						icon_button(
							"btn-basket-clear",
							Icon::Clear,
							t("basket_clear", loc),
							self.basket_count() > 0,
							31,
						)
						.on_click(cx.listener(|this, _, _, cx| {
							this.clear_basket(cx);
						}))
						.children(probe(log, "btn-basket-clear")),
					)
					.child(
						button(
							"btn-paste",
							t("btn_paste", loc),
							Btn::Default,
							true,
							4,
						)
						.on_click(cx.listener(|this, _, window, cx| {
							this.trigger_paste_preview(window, cx)
						}))
						.children(probe(log, "btn-paste")),
					)
					.child(toolbar_divider())
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
					)
					.child(
						button(
							"btn-locale",
							t("btn_toggle_lang", loc),
							Btn::Ghost,
							true,
							6,
						)
						.px(px(6.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(TEXT_MUTED))
						.on_click(
							cx.listener(|this, _, _, cx| {
								this.toggle_locale(cx)
							}),
						)
						.children(probe(log, "btn-locale")),
					),
			)
			.into_any_element()
	}

	fn render_popover(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let q = self.selector_input.read(cx).text().to_string();
		let items = self.selector_items(&q);
		let title = match self.popover {
			Some(Popover::Repo) => t("selector_repo_title", loc),
			_ => t("selector_ref_title", loc),
		};
		let n = items.len();
		let list_h = (n.max(1) as f32 * 26.0).min(300.0);
		let cursor = self.popover_cursor;
		let panel = div()
			.id("selector-popover")
			.occlude()
			.w(px(340.))
			.flex()
			.flex_col()
			.bg(rgb(PANEL_BG))
			.border_1()
			.border_color(rgb(BUTTON_BORDER))
			.rounded(px(6.))
			.shadow_lg()
			.p(px(6.))
			.gap(px(6.))
			.on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_popover(cx)))
			.child(div().text_size(px(SMALL_TEXT)).text_color(rgb(TEXT_MUTED)).child(title))
			.child(
				div()
					.id("selector-input")
					.relative()
					.child(self.selector_input.clone())
					.children(probe(log, "selector-input")),
			)
			.child(
				div()
					.h(px(list_h))
					.when(n == 0, |d| {
						d.child(div().p(px(6.)).text_color(rgb(TEXT_MUTED)).child(t("selector_empty", loc)))
					})
					.when(n > 0, |d| {
						d.child(
							uniform_list(
								"selector-items",
								n,
								cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
									let q = this.selector_input.read(cx).text().to_string();
									let items = this.selector_items(&q);
									range
										.filter_map(|ix| items.get(ix).cloned().map(|it| (ix, it)))
										.map(|(ix, it)| {
											let pick = it.pick.clone();
											let active = match &it.pick {
												Pick::Repo(i) => this.selected_repo_idx == Some(*i),
												Pick::Ref(r) => r == &this.active_ref_filter,
											};
											div()
												.id(SharedString::from(it.id.clone()))
												.relative()
												.flex()
												.items_center()
												.gap(px(6.))
												.h(px(26.))
												.px(px(6.))
												.rounded(px(4.))
												.cursor_pointer()
												.when(ix == cursor, |d| d.bg(rgb(SELECTION_BG)))
												.when(ix != cursor, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
												.on_click(cx.listener(move |this, _, _, cx| this.choose(pick.clone(), cx)))
												.child(icon(
													if it.error {
														Icon::Warning
													} else {
														match it.pick {
															Pick::Repo(_) => Icon::Repo,
															Pick::Ref(_) => Icon::Branch,
														}
													},
													14.,
													if it.error { ERROR } else { TEXT_MUTED },
												))
												.child(fill_text(it.label.clone()).when(active, |d| d.font_weight(FontWeight::SEMIBOLD)))
												.when_some(it.group, |d, g| {
													d.child(div().flex_shrink_0().text_size(px(11.)).text_color(rgb(TEXT_DISABLED)).child(t(g, this.locale)))
												})
												.child(
													div()
														.flex_shrink_0()
														.text_size(px(11.))
														.text_color(rgb(if it.error { ERROR } else { TEXT_MUTED }))
														.child(it.detail.clone()),
												)
												.children(probe(&this.probes, it.id.clone()))
										})
										.collect::<Vec<_>>()
								}),
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
				.when(on, |d| {
					d.bg(rgb(RAIL_ACTIVE_BG)).child(
						// Accent marker on the rail edge for the open tool.
						div()
							.absolute()
							.left(px(-4.))
							.top(px(6.))
							.w(px(2.))
							.h(px(14.))
							.rounded(px(1.))
							.bg(rgb(ACCENT)),
					)
				})
				.when(!on, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
				.focus(|s| s.border_color(rgb(FOCUS_RING)))
				.tooltip(tip(label))
				.child(icon(ic, 16., if on { ACCENT_TEXT } else { TEXT_MUTED }))
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
			.bg(rgb(PANEL_BG))
			.border_r_1()
			.border_color(rgb(BORDER))
			.child(
				rail_button(
					"rail-project",
					Icon::Folder,
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
					Icon::Log,
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
					.when(self.is_adding_repo, |b| b.bg(rgb(RAIL_ACTIVE_BG)))
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
								Icon::More,
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
										label, count, group_id,
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
		.size_full();

		div()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.w(px(width))
			.h_full()
			.bg(rgb(PANEL_BG))
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
						.text_color(rgb(ERROR))
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
					.flex_1()
					.min_h_0()
					.when(n == 0, |d| {
						d.child(
							div().p(px(10.)).text_color(rgb(TEXT_MUTED)).child(
								if is_project {
									t("empty_project", loc)
								} else {
									t("clean_working_copy", loc)
								},
							),
						)
					})
					.child(list)
					.children(probe(&self.probes, "left-list")),
			)
			.into_any_element()
	}

	/// Bright while the left list has focus, grey otherwise, like IntelliJ.
	fn left_selection_bg(&self) -> u32 {
		if self.left_active {
			SELECTION_BG
		} else {
			SELECTION_INACTIVE_BG
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
							("+", s.changes.staged, GIT_ADDED),
							("~", s.changes.unstaged, GIT_MODIFIED),
							("?", s.changes.untracked, GIT_UNTRACKED),
							("!", s.changes.conflicted, GIT_CONFLICT),
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
									.text_color(rgb(TEXT_MUTED))
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
								.text_color(rgb(ERROR))
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
					.when(selected, |d| d.bg(rgb(self.left_selection_bg())))
					.when(!selected, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
					.when(cursor && self.left_active, |d| {
						d.border_1().border_color(rgb(FOCUS_RING))
					})
					.tooltip(tip(tooltip))
					.on_click(cx.listener(move |this, _, _, cx| {
						this.tree_cursor = ix;
						this.select_repo(idx, cx);
					}))
					.child(icon(
						if selected {
							Icon::ChevronDown
						} else {
							Icon::ChevronRight
						},
						10.,
						TEXT_MUTED,
					))
					.child(icon(
						if is_err { Icon::Warning } else { Icon::Repo },
						14.,
						if is_err { ERROR } else { FOLDER },
					))
					.child(
						clip_text(repo.name.clone())
							.font_weight(FontWeight::SEMIBOLD),
					)
					.when_some(kind_badge, |d, badge| {
						d.child(
							div()
								.flex_shrink_0()
								.px(px(4.))
								.rounded(px(3.))
								.border_1()
								.border_color(rgb(DIVIDER))
								.text_size(px(10.))
								.text_color(rgb(TEXT_MUTED))
								.child(badge),
						)
					})
					.child(
						fill_text(branch)
							.text_size(px(SMALL_TEXT))
							.text_color(rgb(TEXT_MUTED)),
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
				.text_color(rgb(if row.is_error { ERROR } else { TEXT_MUTED }))
				.when(is_actionable, |d| {
					d.cursor_pointer().hover(|s| s.bg(rgb(HOVER_BG)))
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
		let is_dir = row.is_dir;
		let selected_file = self.selected_file.as_deref() == Some(&rel);
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
				GIT_CONFLICT
			} else if f.source == SourceKind::Working {
				GIT_UNTRACKED
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
			.when(selected_file, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!selected_file, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
			.when(cursor && self.left_active, |d| {
				d.border_1().border_color(rgb(FOCUS_RING))
			})
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
					TEXT_MUTED,
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
							.text_color(rgb(TEXT_MUTED))
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
					Icon::File
				},
				14.,
				if is_dir { FOLDER } else { FILE },
			))
			.child(
				fill_text(row.name.clone())
					.when_some(name_color, |d, c| d.text_color(rgb(c))),
			)
			.when(row.is_nested_repo, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.rounded(px(3.))
						.border_1()
						.border_color(rgb(DIVIDER))
						.text_size(px(10.))
						.text_color(rgb(TEXT_MUTED))
						.child("repo"),
				)
			})
			.when(!is_valid_utf8, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(4.))
						.rounded(px(3.))
						.bg(rgb(HOVER_BG))
						.text_size(px(10.))
						.text_color(rgb(TEXT_MUTED))
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
				.text_color(rgb(TEXT_MUTED))
				.child(clip_text(m.render(self.locale)))
				.into_any_element();
		}
		let is_dir = row.kind == snip_core::browser::TreeKind::Tree;
		let submodule = row.kind == snip_core::browser::TreeKind::Submodule;
		let path = row.path.clone();
		let selected = self.selected_commit_file.as_deref() == Some(&path)
			&& self.rev_tree.is_some();
		let id = format!("rev-row:{path}");
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
			.when(selected, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!selected, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
			.when(cursor && self.left_active, |d| {
				d.border_1().border_color(rgb(FOCUS_RING))
			})
			.when(!submodule, |d| {
				d.on_click(cx.listener(move |this, _, _, cx| {
					this.tree_cursor = ix;
					this.rev_tree_click(&path, is_dir, cx);
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
					TEXT_MUTED,
				)
				.into_any_element()
			} else {
				div().flex_shrink_0().w(px(10.)).into_any_element()
			})
			.child(icon(
				if is_dir {
					if row.expanded {
						Icon::FolderOpen
					} else {
						Icon::Folder
					}
				} else if submodule {
					Icon::Repo
				} else {
					Icon::File
				},
				14.,
				if is_dir { FOLDER } else { FILE },
			))
			.child(fill_text(row.name.clone()))
			.when(submodule, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.text_size(px(10.))
						.text_color(rgb(TEXT_MUTED))
						.child(t("submodule", self.locale)),
				)
			})
			.children(probe(log, id))
			.into_any_element()
	}

	fn change_header_row(
		&self,
		label_key: &'static str,
		count: usize,
		group_id: &'static str,
	) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let text = format!("{} ({})", t(label_key, loc), count);
		let id = format!("change-header:{group_id}");
		div()
			.id(SharedString::from(id.clone()))
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.w_full()
			.h(px(ROW_H))
			.pl(px(10.))
			.pr(px(6.))
			.gap(px(6.))
			.text_size(px(11.))
			.font_weight(FontWeight::SEMIBOLD)
			.text_color(rgb(TEXT_MUTED))
			.child(text)
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
			GIT_CONFLICT
		} else if item.source == SourceKind::Working {
			GIT_UNTRACKED
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
		let selected = self.selected_file.as_deref() == Some(&path)
			&& self
				.selected_file_source
				.as_ref()
				.is_none_or(|s| s == &item.source);
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
		let chk_id = format!("change-chk:{path}");
		let src_chk_id = format!("change-chk:{source_str}:{path}");
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
			.pl(px(10.))
			.pr(px(6.))
			.gap(px(6.))
			.cursor_pointer()
			.when(selected, |d| d.bg(rgb(self.left_selection_bg())))
			.when(!selected, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
			.when(cursor && !selected && self.left_active, |d| {
				d.border_1().border_color(rgb(FOCUS_RING))
			})
			.tooltip(tip(format!("{path}  ({letter})")))
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
					.on_click(cx.listener(move |this, _, _, cx| {
						cx.stop_propagation();
						this.toggle_file(ix, cx);
					}))
					.child(checkbox(item.selected))
					.children(probe(log, chk_id))
					.children(probe(log, src_chk_id)),
			)
			.child(icon(
				if is_dir { Icon::Folder } else { Icon::File },
				14.,
				if is_dir { FOLDER } else { FILE },
			))
			.child(
				clip_text(name)
					.flex_shrink_0()
					.max_w(gpui::relative(0.7))
					.text_color(rgb(color))
					.when(deleted, |d| d.line_through()),
			)
			.child(
				fill_text(dir)
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(TEXT_MUTED)),
			)
			.children(probe(log, row_id))
			.children(probe(log, src_row_id))
			.into_any_element()
	}

	// ───────────────────────── editor ─────────────────────────

	/// Editor tab row. An empty `label` means nothing is open: the row stays
	/// (stable layout) but shows no tab, like IntelliJ's empty editor.
	fn tab_strip(&self, label: String, ic: Icon) -> Div {
		div()
			.flex()
			.flex_row()
			.flex_shrink_0()
			.h(px(32.))
			.bg(rgb(PANEL_BG))
			.border_b_1()
			.border_color(rgb(DIVIDER))
			.when(!label.is_empty(), |d| {
				d.child(
					div()
						.flex()
						.flex_row()
						.items_center()
						.gap(px(6.))
						.min_w_0()
						.max_w(px(360.))
						.px(px(12.))
						.pt(px(2.))
						.bg(rgb(EDITOR_BG))
						.border_b_2()
						.border_color(rgb(ACCENT))
						.text_size(px(UI_TEXT))
						.text_color(rgb(TEXT))
						.child(icon(ic, 14., FILE))
						.child(clip_text(label)),
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
									.text_color(rgb(TEXT_MUTED))
									.child(label.to_string()),
							)
							.child(
								div()
									.text_color(rgb(HINT_SHORTCUT))
									.child(keys),
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

	fn render_editor(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let (tab_label, crumbs, badge) = self.source_labels();
		let is_diff = self.preview.as_ref().is_some_and(|p| p.is_diff);
		let side = self.reader.diff_mode == DiffMode::SideBySide;
		let n_matches = self.reader.matches.len();
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
		let has_preview = self.preview.is_some();

		let toolbar = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(30.))
			.px(px(8.))
			.gap(px(6.))
			.border_b_1()
			.border_color(rgb(DIVIDER))
			.text_size(px(SMALL_TEXT))
			.child(icon(Icon::Search, 13., TEXT_MUTED))
			.child(
				div()
					.id("find-input")
					.relative()
					.w(px(150.))
					.min_w(px(80.))
					.child(self.find_input.clone())
					.children(probe(log, "find-input")),
			)
			.child(
				div()
					.id("find-count")
					.flex_shrink_0()
					.min_w(px(36.))
					.text_color(rgb(if n_matches == 0 {
						TEXT_DISABLED
					} else {
						TEXT_MUTED
					}))
					.child(find_label),
			)
			.child(
				button("btn-find-prev", "↑", Btn::Ghost, n_matches > 0, 32)
					.px(px(5.))
					.tooltip(tip(t("tip_find_prev", loc)))
					.on_click(
						cx.listener(|this, _, _, cx| this.find_step(false, cx)),
					)
					.children(probe(log, "btn-find-prev")),
			)
			.child(
				button("btn-find-next", "↓", Btn::Ghost, n_matches > 0, 33)
					.px(px(5.))
					.tooltip(tip(t("tip_find_next", loc)))
					.on_click(
						cx.listener(|this, _, _, cx| this.find_step(true, cx)),
					)
					.children(probe(log, "btn-find-next")),
			)
			.child(
				div()
					.id("goto-input")
					.relative()
					.w(px(64.))
					.flex_shrink_0()
					.child(self.goto_input.clone())
					.children(probe(log, "goto-input")),
			)
			.child(div().flex_1())
			.when(is_diff, |d| {
				d.child(
					button(
						"btn-diff-mode",
						if side {
							t("diff_inline", loc)
						} else {
							t("diff_side", loc)
						},
						Btn::Ghost,
						true,
						35,
					)
					.px(px(6.))
					.text_size(px(SMALL_TEXT))
					.child(icon(Icon::Diff, 12., TEXT_MUTED))
					.on_click(cx.listener(|this, _, _, cx| {
						this.reader.diff_mode = match this.reader.diff_mode {
							DiffMode::Inline => DiffMode::SideBySide,
							DiffMode::SideBySide => DiffMode::Inline,
						};
						this.reader.anchor = None;
						this.reader.head = None;
						app_log!(
							"[APP:DIFF_MODE: {:?}]",
							this.reader.diff_mode
						);
						cx.notify();
					}))
					.children(probe(log, "btn-diff-mode")),
				)
			})
			.when(
				self.selected_commit.is_some() && self.compare.is_none(),
				|d| {
					d.child(
						button(
							"btn-browse-tree",
							t("btn_browse_tree", loc),
							Btn::Ghost,
							true,
							36,
						)
						.px(px(6.))
						.text_size(px(SMALL_TEXT))
						.tooltip(tip(t("tip_browse_tree", loc)))
						.on_click(cx.listener(|this, _, _, cx| {
							this.browse_commit_tree(cx)
						}))
						.children(probe(log, "btn-browse-tree")),
					)
				},
			)
			.child(
				button(
					"btn-copy-view",
					t("btn_copy_view", loc),
					Btn::Ghost,
					has_preview,
					37,
				)
				.px(px(6.))
				.text_size(px(SMALL_TEXT))
				.tooltip(tip(t("tip_copy_view", loc)))
				.on_click(cx.listener(|this, _, _, cx| {
					this.copy_current_preview_content(cx)
				}))
				.children(probe(log, "btn-copy-view")),
			);

		let body: AnyElement = if self.preview_loading && self.preview.is_none()
		{
			div()
				.p(px(12.))
				.text_color(rgb(TEXT_MUTED))
				.child(t("status_loading", loc))
				.into_any_element()
		} else if let Some(err) = &self.preview_error {
			div()
				.id("editor-error")
				.relative()
				.flex()
				.gap(px(6.))
				.p(px(12.))
				.text_color(rgb(ERROR))
				.child(icon(Icon::Warning, 14., ERROR))
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
					this.reader.anchor = None;
					this.reader.head = None;
					cx.notify();
				}))
				.flex()
				.flex_col()
				.flex_1()
				.min_h_0()
				.when_some(
					self.preview.as_ref().and_then(|p| p.notice.clone()),
					|d, _| {
						d.child(
							div()
								.flex_shrink_0()
								.px(px(12.))
								.py(px(2.))
								.bg(rgb(PANEL_BG))
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(WARNING))
								.child(tf(
									"truncated_notice",
									loc,
									&[
										&crate::reader::MAX_PREVIEW_LINES,
										&(crate::reader::MAX_PREVIEW_BYTES
											/ 1024),
									],
								)),
						)
					},
				)
				.child(self.render_code_view(false, cx))
				.children(probe(log, "reader"))
				.into_any_element()
		} else {
			self.editor_empty_hints().into_any_element()
		};

		div()
			.flex()
			.flex_col()
			.flex_1()
			.min_w_0()
			.h_full()
			.bg(rgb(EDITOR_BG))
			.child(self.tab_strip(
				if self.preview.is_some()
					|| self.selected_commit.is_some()
					|| self.compare.is_some()
				{
					tab_label
				} else {
					String::new()
				},
				Icon::File,
			))
			.child(
				div()
					.flex()
					.flex_row()
					.items_center()
					.flex_shrink_0()
					.h(px(26.))
					.px(px(12.))
					.gap(px(8.))
					.border_b_1()
					.border_color(rgb(DIVIDER))
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
							.text_color(rgb(TEXT_MUTED))
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
								.bg(rgb(REF_BG))
								.text_color(rgb(TEXT))
								.overflow_hidden()
								.line_clamp(1)
								.text_ellipsis()
								.tooltip(tip(badge.clone()))
								.child(badge)
								.children(probe(log, "source-badge")),
						)
					}),
			)
			.when(
				self.selected_commit.is_some() || self.compare.is_some(),
				|d| d.child(self.render_commit_panel(cx)),
			)
			.child(toolbar)
			.child(body)
			.into_any_element()
	}

	/// Commit / compare header plus its changed files (virtualized).
	fn render_commit_panel(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let header: AnyElement = if let Some((from, to)) = &self.compare {
			div()
				.text_color(rgb(TEXT))
				.child(tf("compare_header", loc, &[&short(from), &short(to)]))
				.into_any_element()
		} else if let Some(c) = self
			.selected_commit
			.as_ref()
			.and_then(|s| self.commits.iter().find(|c| &c.sha == s))
		{
			let parents = if c.parents.is_empty() {
				t("root_commit", loc).to_string()
			} else {
				c.parents
					.iter()
					.map(|p| short(p).to_string())
					.collect::<Vec<_>>()
					.join(", ")
			};
			div()
				.flex()
				.flex_col()
				.child(
					div()
						.flex()
						.gap(px(8.))
						.child(
							div()
								.flex_shrink_0()
								.font_family(EDITOR_FONT)
								.text_color(rgb(TEXT_MUTED))
								.child(short(&c.sha).to_string()),
						)
						.child(
							fill_text(c.subject.clone())
								.font_weight(FontWeight::SEMIBOLD),
						),
				)
				.child(
					div()
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(TEXT_MUTED))
						.child(tf(
							"commit_meta",
							loc,
							&[
								&c.author_name,
								&short_date(&c.author_date),
								&parents,
							],
						)),
				)
				.into_any_element()
		} else {
			div().into_any_element()
		};
		let n = self.commit_files.len();
		div()
			.id("commit-panel")
			.relative()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.px(px(12.))
			.py(px(6.))
			.gap(px(4.))
			.bg(rgb(PANEL_BG))
			.border_b_1()
			.border_color(rgb(DIVIDER))
			.child(header)
			.child(
				div()
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(TEXT_MUTED))
					.child(tf("changed_files", loc, &[&n])),
			)
			.child(
				div().h(px((n.max(1) as f32 * 22.0).min(110.0))).child(
					uniform_list(
						"commit-files",
						n,
						cx.processor(
							|this, range: std::ops::Range<usize>, _, cx| {
								range
									.filter_map(|ix| {
										this.commit_files.get(ix).cloned()
									})
									.map(|(path, ct)| {
										let (letter, color) = change_style(ct);
										let sel = this
											.selected_commit_file
											.as_deref() == Some(&path)
											&& this.rev_tree.is_none();
										let id = format!("commit-file:{path}");
										let p2 = path.clone();
										div()
											.id(SharedString::from(id.clone()))
											.relative()
											.flex()
											.items_center()
											.gap(px(6.))
											.h(px(22.))
											.px(px(4.))
											.cursor_pointer()
											.when(sel, |d| {
												d.bg(rgb(SELECTION_BG))
											})
											.when(!sel, |d| {
												d.hover(|s| s.bg(rgb(HOVER_BG)))
											})
											.on_click(cx.listener(
												move |this, _, _, cx| {
													this.select_commit_file(
														&p2, cx,
													)
												},
											))
											.child(
												div()
													.w(px(10.))
													.flex_shrink_0()
													.text_color(rgb(color))
													.child(letter),
											)
											.child(icon(Icon::File, 13., color))
											.child(fill_text(path))
											.children(probe(&this.probes, id))
									})
									.collect::<Vec<_>>()
							},
						),
					)
					.size_full(),
				),
			)
			.children(probe(log, "commit-panel"))
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

		let action_bar = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(38.))
			.px(px(12.))
			.gap(px(8.))
			.bg(rgb(PANEL_BG))
			.border_b_1()
			.border_color(rgb(BORDER))
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
							.text_color(rgb(TEXT_MUTED))
							.child(format!("{}:", t("destination_label", loc))),
					)
					.child(fill_text(dest)),
			)
			.child(
				button(
					"btn-apply",
					if applying {
						t("applying", loc)
					} else {
						t("apply", loc)
					},
					Btn::Primary,
					!applying && mapping_ready,
					50,
				)
				.when(!mapping_ready, |b| {
					b.tooltip(tip(t("mapping_required", loc)))
				})
				.when(!applying && mapping_ready, |b| {
					b.on_click(cx.listener(|this, _, _, cx| {
						this.apply_paste_restore(cx)
					}))
				})
				.children(probe(log, "btn-apply")),
			)
			.child(
				// The write cannot be interrupted, so Cancel is locked until it
				// finishes rather than pretending to cancel it.
				button(
					"btn-cancel",
					t("cancel", loc),
					Btn::Default,
					!applying,
					51,
				)
				.when(!applying, |b| {
					b.on_click(cx.listener(|this, _, _, cx| {
						this.cancel_paste_preview(cx)
					}))
				})
				.when(applying, |b| {
					b.tooltip(tip(t("paste_busy_refused", loc)))
				})
				.children(probe(log, "btn-cancel")),
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
			.border_color(rgb(DIVIDER))
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(GIT_ADDED))
					.child(format!("{} {creates}", t("op_create", loc))),
			)
			.child(div().flex_shrink_0().text_color(rgb(WARNING)).child(
				format!("{} {overwrites}/{existing}", t("op_overwrite", loc)),
			))
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(ERROR))
					.child(format!("{} {deletes}", t("op_delete", loc))),
			)
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(TEXT_MUTED))
					.child(format!("{} {skips}", t("op_skip", loc))),
			)
			.child(
				fill_text(t("paste_keys", loc)).text_color(rgb(TEXT_DISABLED)),
			);

		let rows = plan.items.iter().enumerate().map(|(ix, item)| {
			let (op, op_color, _) = paste_op(item);
			let is_sel = ix == plan.selected_item_idx;
			let path = item.path.clone();
			let row_id = format!("paste-row:{path}");
			let inc_id = format!("paste-include:{path}");
			let ow_id = format!("paste-overwrite:{path}");
			let can_overwrite = item.dest_exists && !item.is_delete;
			let ow_on = item.overwrite_allowed;
			// IntelliJ-style: file name first, muted parent directory after it.
			let (parent, name) = match path.rsplit_once('/') {
				Some((p, n)) => (p.to_string(), n.to_string()),
				None => (String::new(), path.clone()),
			};
			div()
				.id(SharedString::from(row_id.clone()))
				.relative()
				.flex()
				.flex_row()
				.items_center()
				.flex_shrink_0()
				.h(px(26.))
				.pl(px(12.))
				.pr(px(8.))
				.gap(px(8.))
				.cursor_pointer()
				.when(is_sel, |d| d.bg(rgb(SELECTION_BG)))
				.when(!is_sel, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
				.tooltip(tip(format!("{path}\n→ {}", item.dest_path.display())))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.select_paste_item(ix, cx)
				}))
				.child(
					div()
						.id(SharedString::from(inc_id.clone()))
						.relative()
						.flex_shrink_0()
						.size(px(18.))
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
				.child(
					div()
						.flex_shrink_0()
						.w(px(60.))
						.text_size(px(SMALL_TEXT))
						.font_weight(FontWeight::SEMIBOLD)
						.text_color(rgb(op_color))
						.child(t(op, loc)),
				)
				.child(
					div()
						.flex()
						.flex_row()
						.items_center()
						.gap(px(8.))
						.flex_1()
						.min_w_0()
						.overflow_hidden()
						.child(
							clip_text(name)
								.flex_shrink_0()
								.max_w(gpui::relative(0.7))
								.when(!item.selected, |d| {
									d.text_color(rgb(TEXT_MUTED))
								}),
						)
						.child(
							fill_text(parent)
								.text_size(px(SMALL_TEXT))
								.text_color(rgb(TEXT_MUTED)),
						),
				)
				.child(
					clip_text(item.dest_root_name.clone())
						.flex_shrink_0()
						.max_w(px(110.))
						.text_size(px(10.))
						.px(px(5.))
						.rounded(px(3.))
						.bg(rgb(REF_BG))
						.text_color(rgb(TEXT_MUTED)),
				)
				.child(
					div()
						.flex()
						.justify_end()
						.flex_shrink_0()
						.min_w(px(44.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(TEXT_MUTED))
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
									WARNING
								} else {
									TEXT_MUTED
								}))
								.when(!applying, |d| {
									d.cursor_pointer()
										.hover(|s| s.bg(rgb(HOVER_BG)))
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
				.border_color(rgb(DIVIDER))
				.bg(rgb(PANEL_BG))
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
								d.border_t_1().border_color(rgb(DIVIDER))
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
										TEXT_MUTED
									} else {
										WARNING
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
			.bg(rgb(EDITOR_BG))
			.child(self.tab_strip(
				format!("{} ({})", t("paste_tab", loc), plan.items.len()),
				Icon::Changes,
			))
			.child(action_bar)
			.child(summary)
			.when(plan.whole_commit, |d| {
				d.child(
					div()
						.id("paste-commit-whole")
						.relative()
						.flex_shrink_0()
						.px(px(12.))
						.py(px(4.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(TEXT_MUTED))
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
						.bg(rgb(ERROR_BG))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(ERROR))
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
					.border_color(rgb(DIVIDER))
					.children(mappings)
					.children(rows),
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
						.bg(rgb(PANEL_BG))
						.text_size(px(SMALL_TEXT))
						.child(fill_text(detail_title))
						.child(
							div()
								.flex_shrink_0()
								.text_color(rgb(TEXT_MUTED))
								.child(reason),
						),
				)
			})
			.when(selected.is_some_and(|i| i.is_delete), |d| {
				d.child(
					div()
						.p(px(12.))
						.text_color(rgb(ERROR))
						.child(t("reason_delete", loc)),
				)
			})
			.when(selected.is_some_and(|i| !i.is_delete), |d| {
				d.child(self.render_code_view(true, cx))
			})
			.into_any_element()
	}

	// ───────────────────────── git log ─────────────────────────

	fn render_log(&self, height: f32, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale;
		let log = &self.probes;
		let repo_name = self.repo().map(|r| r.name.clone()).unwrap_or_default();
		let scope = match (&self.log_search, &self.active_ref_filter) {
			(Some(s), _) => tf(
				if s.author {
					"log_scope_author"
				} else {
					"log_scope_search"
				},
				loc,
				&[&s.query],
			),
			(None, Some(r)) => r.trim_start_matches("refs/heads/").to_string(),
			(None, None) => t("refs_all", loc).to_string(),
		};
		let range = self.range_rows();
		let header = div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(PANEL_HEADER_H + 2.0))
			.px(px(8.))
			.gap(px(6.))
			.border_b_1()
			.border_color(rgb(BORDER))
			.child(
				div()
					.flex_shrink_0()
					.font_weight(FontWeight::SEMIBOLD)
					.child("Git"),
			)
			.child(
				div()
					.flex_shrink_0()
					.h_full()
					.flex()
					.items_center()
					.border_b_2()
					.border_color(rgb(ACCENT))
					.child(t("log_tab", loc)),
			)
			.child(icon(Icon::Search, 13., TEXT_MUTED))
			.child(
				div()
					.id("log-search-input")
					.relative()
					.w(px(170.))
					.min_w(px(80.))
					.child(self.log_search_input.clone())
					.children(probe(log, "log-search-input")),
			)
			.child(
				button(
					"btn-search-author",
					t("search_author", loc),
					if self.search_by_author {
						Btn::Default
					} else {
						Btn::Ghost
					},
					true,
					41,
				)
				.px(px(6.))
				.h(px(20.))
				.text_size(px(SMALL_TEXT))
				.tooltip(tip(t("tip_search_author", loc)))
				.on_click(cx.listener(|this, _, _, cx| {
					this.search_by_author = !this.search_by_author;
					app_log!("[APP:SEARCH_AUTHOR: {}]", this.search_by_author);
					let q = this
						.log_search_input
						.read(cx)
						.text()
						.trim()
						.to_string();
					if !q.is_empty() {
						this.start_log_search(q, cx);
					}
					cx.notify();
				}))
				.children(probe(log, "btn-search-author")),
			)
			.child(
				fill_text(format!("{repo_name} · {scope}"))
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(TEXT_MUTED)),
			)
			.child(
				button(
					"btn-head",
					"HEAD",
					Btn::Ghost,
					self.head_sha.is_some(),
					42,
				)
				.px(px(6.))
				.h(px(20.))
				.text_size(px(SMALL_TEXT))
				.tooltip(tip(t("tip_head", loc)))
				.on_click(cx.listener(|this, _, _, cx| this.locate_head(cx)))
				.children(probe(log, "btn-head")),
			)
			.child(
				button(
					"btn-compare",
					t("btn_compare", loc),
					Btn::Ghost,
					range.is_some(),
					45,
				)
				.px(px(6.))
				.h(px(20.))
				.text_size(px(SMALL_TEXT))
				.tooltip(tip(match range {
					Some((a, b)) => tf("tip_compare", loc, &[&(b - a + 1)]),
					None => t("tip_compare_disabled", loc).to_string(),
				}))
				.when(range.is_some(), |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| this.compare_range(cx)),
					)
				})
				.children(probe(log, "btn-compare")),
			)
			.child(
				button(
					"btn-copy-commits",
					t("btn_copy_commits", loc),
					Btn::Ghost,
					self.selected_commit.is_some(),
					46,
				)
				.px(px(6.))
				.h(px(20.))
				.text_size(px(SMALL_TEXT))
				.tooltip(tip(t("tip_copy_commits", loc)))
				.when(self.selected_commit.is_some(), |b| {
					b.on_click(cx.listener(|this, _, _, cx| {
						this.copy_commits_to_clipboard(cx)
					}))
				})
				.children(probe(log, "btn-copy-commits")),
			)
			.child(
				button(
					"btn-prev-page",
					"‹",
					Btn::Ghost,
					self.commit_page > 0,
					43,
				)
				.px(px(6.))
				.h(px(20.))
				.tooltip(tip(t("page_prev", loc)))
				.when(self.commit_page > 0, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| {
							this.history_prev_page(cx)
						}),
					)
				})
				.children(probe(log, "btn-prev-page")),
			)
			.child(
				div()
					.id("page-indicator")
					.flex_shrink_0()
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(TEXT_MUTED))
					.child(format!("{}", self.commit_page + 1)),
			)
			.child(
				button(
					"btn-next-page",
					"›",
					Btn::Ghost,
					self.history_has_more,
					44,
				)
				.px(px(6.))
				.h(px(20.))
				.tooltip(tip(t("page_next", loc)))
				.when(self.history_has_more, |b| {
					b.on_click(
						cx.listener(|this, _, _, cx| {
							this.history_next_page(cx)
						}),
					)
				})
				.children(probe(log, "btn-next-page")),
			)
			.child(
				button("btn-log-hide", t("hide", loc), Btn::Ghost, true, 47)
					.px(px(6.))
					.h(px(20.))
					.text_size(px(SMALL_TEXT))
					.on_click(cx.listener(|this, _, _, cx| this.toggle_log(cx)))
					.children(probe(log, "btn-log-hide")),
			);

		// Refs sidebar grouped like IntelliJ's branches pane.
		let mut ref_rows: Vec<AnyElement> = Vec::new();
		let ref_entry = |id: String,
		                 label: String,
		                 glyph: Option<(Icon, u32)>,
		                 indent: f32,
		                 target: Option<String>,
		                 active: bool,
		                 cx: &mut Context<Self>| {
			div()
				.id(SharedString::from(id.clone()))
				.relative()
				.flex_shrink_0()
				.h(px(22.))
				.pl(px(indent))
				.pr(px(8.))
				.flex()
				.items_center()
				.gap(px(5.))
				.cursor_pointer()
				.when(active, |d| d.bg(rgb(SELECTION_BG)))
				.when(!active, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
				.tooltip(tip(label.clone()))
				.on_click(cx.listener(move |this, _, _, cx| {
					this.filter_by_ref(target.clone(), cx)
				}))
				.when_some(glyph, |d, (g, c)| d.child(icon(g, 12., c)))
				.child(clip_text(label).text_color(rgb(TEXT)))
				.children(probe(log, id))
				.into_any_element()
		};
		ref_rows.push(ref_entry(
			"ref-all".into(),
			t("refs_all", loc).to_string(),
			None,
			10.,
			None,
			self.active_ref_filter.is_none() && self.log_search.is_none(),
			cx,
		));
		if self.head_sha.is_some() {
			ref_rows.push(ref_entry(
				"ref:HEAD".into(),
				"HEAD".into(),
				Some((Icon::Commit, REF_HEAD)),
				10.,
				Some("HEAD".into()),
				self.active_ref_filter.as_deref() == Some("HEAD"),
				cx,
			));
		}
		for (key, prefix, color, glyph) in [
			("refs_local", "refs/heads/", REF_LOCAL, Icon::Branch),
			("refs_remote", "refs/remotes/", REF_REMOTE, Icon::Branch),
			("refs_tags", "refs/tags/", REF_TAG, Icon::Commit),
		] {
			let members: Vec<_> = self
				.refs
				.iter()
				.filter(|r| r.name.starts_with(prefix))
				.collect();
			if members.is_empty() {
				continue;
			}
			ref_rows.push(
				div()
					.flex_shrink_0()
					.h(px(22.))
					.mt(px(4.))
					.px(px(6.))
					.flex()
					.items_center()
					.gap(px(3.))
					.text_size(px(11.))
					.font_weight(FontWeight::SEMIBOLD)
					.text_color(rgb(TEXT_MUTED))
					.child(icon(Icon::ChevronDown, 10., TEXT_MUTED))
					.child(clip_text(format!(
						"{} ({})",
						t(key, loc),
						members.len()
					)))
					.into_any_element(),
			);
			// Bounded: very large ref sets are reached through the selector.
			for r in members.iter().take(200) {
				ref_rows.push(ref_entry(
					format!("ref:{}", r.name),
					r.name[prefix.len()..].to_string(),
					Some((glyph, color)),
					22.,
					Some(r.name.clone()),
					self.active_ref_filter.as_deref() == Some(r.name.as_str()),
					cx,
				));
			}
		}

		let searching = self.log_search.is_some();
		let gutter_w = if searching {
			8.0
		} else {
			self.graph_layout
				.as_ref()
				.map(graph_view::gutter_width)
				.unwrap_or(40.0)
		};
		let col_header = div()
			.relative()
			.w_full()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(22.))
			.pr(px(6.))
			.gap(px(6.))
			.border_b_1()
			.border_color(rgb(DIVIDER))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(TEXT_MUTED))
			.child(fill_text(t("col_message", loc)).pl(px(gutter_w)))
			.child(
				div()
					.flex_shrink_0()
					.w(px(100.))
					.child(t("col_author", loc)),
			)
			.child(div().flex_shrink_0().w(px(115.)).child(t("col_date", loc)))
			.child(div().flex_shrink_0().w(px(60.)).child(t("col_hash", loc)));

		let n = self.display_commits().len();
		let list = uniform_list(
			"log-rows",
			n,
			cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
				range
					.map(|ix| this.log_row(ix, gutter_w, cx))
					.collect::<Vec<_>>()
			}),
		)
		.track_scroll(self.log_scroll.clone())
		.size_full();

		div()
			.id("log-panel")
			.relative()
			.flex()
			.flex_col()
			.flex_shrink_0()
			.h(px(height))
			.bg(rgb(PANEL_BG))
			.child(header)
			.when_some(self.history_error.clone(), |d, err| {
				d.child(
					div()
						.id("log-error")
						.relative()
						.flex_shrink_0()
						.px(px(10.))
						.py(px(2.))
						.bg(rgb(ERROR_BG))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(ERROR))
						.child(tf("error_history", loc, &[&err]))
						.children(probe(log, "log-error")),
				)
			})
			.when(searching, |d| {
				d.child(
					div()
						.flex_shrink_0()
						.px(px(10.))
						.py(px(2.))
						.text_size(px(SMALL_TEXT))
						.text_color(rgb(WARNING))
						.child(tf("search_note", loc, &[&n])),
				)
			})
			.child(
				div()
					.flex()
					.flex_row()
					.flex_1()
					.min_h_0()
					.child(
						div()
							.id("refs-scroll")
							.flex()
							.flex_col()
							.flex_shrink_0()
							.w(px(180.))
							.h_full()
							.overflow_y_scroll()
							.border_r_1()
							.border_color(rgb(BORDER))
							.text_size(px(SMALL_TEXT))
							.py(px(2.))
							.children(ref_rows),
					)
					.child(
						div()
							.flex()
							.flex_col()
							.flex_1()
							.min_w_0()
							.bg(rgb(EDITOR_BG))
							.child(col_header)
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
									.flex_1()
									.min_h_0()
									.when(
										n == 0 && self.history_error.is_none(),
										|d| {
											d.child(
												div()
													.p(px(10.))
													.text_color(rgb(TEXT_MUTED))
													.child(t("empty_log", loc)),
											)
										},
									)
									.child(list)
									.children(probe(log, "log-list")),
							),
					),
			)
			.into_any_element()
	}

	fn log_row(
		&self,
		ix: usize,
		gutter_w: f32,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let rows = self.display_commits();
		let Some(c) = rows.get(ix).copied() else {
			return div().into_any_element();
		};
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
		let incoming = ix > 0
			&& self.graph_layout.as_ref().is_some_and(|l| {
				l.rows[..ix.min(l.rows.len())].iter().any(|prev| {
					prev.parent_edges.iter().any(|e| e.to_row == Some(ix))
				})
			});
		let is_merge = c.parents.len() > 1 && self.log_search.is_none();
		let collapsed = self.collapsed_merges.contains(&c.sha);
		let hidden_n = if collapsed {
			crate::history::side_only(&self.commits, &c.sha).len()
		} else {
			0
		};
		// Display only: at most two readable badges, the rest fold into `+N`
		// (full list in its tooltip). Ref data and filtering keep every ref.
		let current_branch = self
			.repo()
			.and_then(|r| r.summary.as_ref().ok())
			.and_then(|s| s.branch.clone());
		let badge = |id: String, color: gpui::Rgba, strong: bool| {
			let tint = |a: f32| gpui::Rgba { a, ..color };
			div()
				.id(SharedString::from(id))
				.min_w(px(24.))
				.max_w(px(120.))
				.h(px(16.))
				.px(px(5.))
				.flex()
				.items_center()
				.rounded(px(3.))
				.border_1()
				.border_color(tint(if strong { 0.7 } else { 0.35 }))
				.bg(tint(if strong { 0.25 } else { 0.12 }))
				.text_size(px(11.))
				.text_color(color)
				.when(strong, |d| d.font_weight(FontWeight::SEMIBOLD))
		};
		let labels: Vec<AnyElement> = graph_row
			.as_ref()
			.map(|r| {
				let (shown, hidden) = graph_view::visible_refs(
					&r.refs,
					current_branch.as_deref(),
				);
				let mut out: Vec<AnyElement> = shown
					.into_iter()
					.map(|b| {
						let info = b.primary;
						let (name, color) = graph_view::format_ref_badge(info);
						let strong = matches!(
							info.kind,
							snip_core::graph::RefKind::Head
						) || (matches!(
							info.kind,
							snip_core::graph::RefKind::Branch
						) && current_branch.as_deref()
							== Some(info.display_name.as_str()));
						let tooltip = std::iter::once(info)
							.chain(b.merged.iter().copied())
							.map(|i| graph_view::format_ref_badge(i).0)
							.collect::<Vec<_>>()
							.join("\n");
						badge(
							format!("ref-badge:{}:{}", short(&sha), name),
							color,
							strong,
						)
						.gap(px(3.))
						.tooltip(tip(tooltip))
						.child(clip_text(name))
						// Merged remote-tracking refs: small remote marker.
						.when(!b.merged.is_empty(), |d| {
							d.child(
								div()
									.flex_shrink_0()
									.text_size(px(10.))
									.text_color(rgb(crate::theme::REF_REMOTE))
									.child("⇅"),
							)
						})
						.into_any_element()
					})
					.collect();
				if hidden > 0 {
					let all = r
						.refs
						.iter()
						.map(|i| graph_view::format_ref_badge(i).0)
						.collect::<Vec<_>>()
						.join("\n");
					out.push(
						badge(
							format!("ref-more:{}", short(&sha)),
							rgb(TEXT_MUTED),
							false,
						)
						.flex_shrink_0()
						.min_w(px(0.))
						.tooltip(tip(all))
						.child(format!("+{hidden}"))
						.into_any_element(),
					);
				}
				out
			})
			.unwrap_or_default();
		let row_id = format!("commit-row:{}", short(&sha));
		let col_id = format!("collapse:{}", short(&sha));
		let merge_sha = sha.clone();
		div()
			.id(SharedString::from(row_id.clone()))
			.relative()
			.w_full()
			.flex()
			.flex_row()
			.items_center()
			.h(px(graph_view::ROW_HEIGHT))
			.pr(px(6.))
			.gap(px(6.))
			.cursor_pointer()
			.when(selected, |d| {
				d.bg(rgb(if self.log_active {
					SELECTION_BG
				} else {
					SELECTION_INACTIVE_BG
				}))
			})
			.when(in_range && !selected, |d| d.bg(rgb(RANGE_BG)))
			.when(!selected && !in_range, |d| d.hover(|s| s.bg(rgb(HOVER_BG))))
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
			.child(
				div()
					.flex_1()
					.min_w_0()
					.flex()
					.flex_row()
					.items_center()
					.gap(px(4.))
					.overflow_hidden()
					.child(
						div()
							.flex_shrink_0()
							.w(px(gutter_w))
							.h(px(graph_view::ROW_HEIGHT))
							.when_some(graph_row, |el, r| {
								el.child(
									canvas(
										|_, _, _| {},
										move |bounds, _, window, _| {
											graph_view::paint_row_graph(
												window, &r, incoming, bounds,
											);
										},
									)
									.size_full(),
								)
							}),
					)
					.when(is_merge, |d| {
						d.child(
							div()
								.id(SharedString::from(col_id.clone()))
								.relative()
								.flex_shrink_0()
								.flex()
								.items_center()
								.gap(px(2.))
								.px(px(2.))
								.rounded(px(3.))
								.hover(|s| s.bg(rgb(HOVER_BG)))
								.tooltip(tip(t(
									if collapsed {
										"tip_expand_merge"
									} else {
										"tip_collapse_merge"
									},
									self.locale,
								)))
								.on_click(cx.listener(move |this, _, _, cx| {
									cx.stop_propagation();
									this.toggle_collapse(merge_sha.clone(), cx);
								}))
								.child(icon(
									if collapsed {
										Icon::ChevronRight
									} else {
										Icon::ChevronDown
									},
									10.,
									TEXT_MUTED,
								))
								.when(collapsed, |d| {
									d.child(
										div()
											.text_size(px(11.))
											.text_color(rgb(WARNING))
											.child(tf(
												"collapsed_n",
												self.locale,
												&[&hidden_n],
											)),
									)
								})
								.children(probe(&self.probes, col_id.clone())),
						)
					})
					.child(
						div()
							.flex()
							.flex_row()
							.items_center()
							.gap(px(3.))
							.min_w_0()
							.max_w(gpui::relative(0.45))
							.overflow_hidden()
							.children(labels),
					)
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
							.tooltip(tip(c.subject.clone()))
							.child(c.subject.clone()),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("author:{}", short(&sha))))
					.flex_shrink_0()
					.text_size(px(SMALL_TEXT))
					.w(px(100.))
					.tooltip(tip(c.author_name.clone()))
					.child(
						clip_text(c.author_name.clone())
							.text_color(rgb(TEXT_MUTED)),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("date:{}", short(&sha))))
					.flex_shrink_0()
					.text_size(px(SMALL_TEXT))
					.w(px(115.))
					.tooltip(tip(short_date(&c.author_date)))
					.child(
						clip_text(short_date(&c.author_date))
							.text_color(rgb(TEXT_MUTED)),
					),
			)
			.child(
				div()
					.id(SharedString::from(format!("sha:{}", short(&sha))))
					.flex_shrink_0()
					.w(px(60.))
					.font_family(CODE_FONT)
					.text_size(px(SMALL_TEXT))
					.text_color(rgb(TEXT_MUTED))
					.tooltip(tip(c.sha.clone()))
					.child(short(&c.sha).to_string()),
			)
			.children(probe(&self.probes, row_id))
			.into_any_element()
	}

	fn render_status(&self) -> AnyElement {
		let loc = self.locale;
		let errors = self.repos.iter().filter(|r| r.summary.is_err()).count();
		let count_s = self.basket_count().to_string();
		let basket_n = count_s.clone();
		let basket_detail = self.basket_summary();
		let basket_label = if let Some(collision) = self.basket_collision_text()
		{
			tf("basket_collision", loc, &[&collision])
		} else if self.basket_count() == 0 {
			t("basket_empty", loc).to_string()
		} else {
			tf("basket_summary", loc, &[&basket_n, &basket_detail])
		};
		let repos_s = self.repos.len().to_string();
		let errors_s = errors.to_string();
		div()
			.id("status-bar")
			.relative()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(STATUS_H))
			.px(px(10.))
			.gap(px(8.))
			.bg(rgb(PANEL_BG))
			.border_t_1()
			.border_color(rgb(BORDER))
			.text_size(px(SMALL_TEXT))
			.text_color(rgb(TEXT_MUTED))
			.child(fill_text(self.status.render(loc)))
			.child(status_sep())
			.child(div().flex_shrink_0().child(tf(
				"status_copy_source",
				loc,
				&[&count_s],
			)))
			.child(status_sep())
			.child(
				div()
					.id("basket-summary")
					.relative()
					.flex_shrink_0()
					.max_w(px(360.))
					.min_w(px(80.))
					.overflow_hidden()
					.text_ellipsis()
					.tooltip(tip(basket_label.clone()))
					.child(basket_label)
					.children(probe(&self.probes, "basket-summary")),
			)
			.child(status_sep())
			.child(
				div()
					.flex_shrink_0()
					.text_color(rgb(if errors > 0 {
						ERROR
					} else {
						TEXT_MUTED
					}))
					.child(tf(
						"status_repo_count",
						loc,
						&[&repos_s, &errors_s],
					)),
			)
			.child(status_sep())
			.child({
				let jobs = self.lifecycle.live_jobs().to_string();
				div()
					.id("lifecycle-jobs")
					.relative()
					.flex_shrink_0()
					.child(tf("lifecycle_jobs", loc, &[&jobs]))
					.children(probe(&self.probes, "lifecycle-jobs"))
			})
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
		self.left_active = self.tree_focus.is_focused(window);
		self.log_active = self.log_focus.is_focused(window);
		self.reader_active = self.reader_focus.is_focused(window);
		let left_w = self.effective_left_w(vw);
		let bottom_h = self.effective_bottom_h(vh);

		let center = if !self.workspace_open && self.paste_preview.is_none() {
			self.render_workspace_closed()
		} else {
			match self.paste_preview {
				Some(ref plan) => self.render_paste(plan, cx),
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
			.on_action(cx.listener(|this, _: &PastePreview, window, cx| {
				this.trigger_paste_preview(window, cx)
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
				window.focus_next();
				app_log!("[APP:FOCUS: next]");
				cx.notify();
			}))
			.on_action(cx.listener(|_, _: &FocusPrev, window, cx| {
				window.focus_prev();
				app_log!("[APP:FOCUS: prev]");
				cx.notify();
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
				window.focus(&this.find_input.read(cx).handle());
				cx.notify();
			}))
			.on_action(cx.listener(|this, _: &GotoLine, window, cx| {
				window.focus(&this.goto_input.read(cx).handle());
				cx.notify();
			}))
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
			.bg(rgb(EDITOR_BG))
			.text_color(rgb(TEXT))
			.text_size(px(UI_TEXT))
			.child(self.render_header(cx))
			.child(
				div()
					.flex()
					.flex_row()
					.flex_1()
					.min_h_0()
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
			.child(self.render_status())
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
