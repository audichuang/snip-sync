//! IntelliJ-style context menus and the tool-window shortcuts that are not
//! tied to one panel (hide tool window, back to editor, Log parent/child).
//!
//! A menu is a list of [`MenuEntry`] built from the row it was opened on.
//! Every entry runs an existing read-only feature or copies text; nothing
//! here writes to Git.

use gpui::{
	anchored, deferred, div, prelude::*, px, rgb, AnyElement, Context,
	FocusHandle, FontWeight, MouseButton, MouseMoveEvent, Pixels, Point,
	ScrollStrategy, SharedString, UniformListScrollHandle, Window,
};
use snip_core::transfer::SourceKind;

use crate::i18n::t;
use crate::theme::*;
use crate::tree::{command_for_row, FlattenedTreeRow, RowGesture, TreeCommand};
use crate::ui::probe;
use crate::WorkbenchModel;

/// Chrome state that belongs to no single panel.
pub struct Chrome {
	pub menu: Option<ContextMenu>,
	pub menu_focus: FocusHandle,
	/// Speed search text typed into the Project / Changes list.
	pub speed: String,
	/// Collapsed Changes groups (`change-header:<id>`).
	pub collapsed_groups: Vec<&'static str>,
	pub left_scroll: UniformListScrollHandle,
}

impl Chrome {
	pub fn new(cx: &mut Context<WorkbenchModel>) -> Self {
		Self {
			menu: None,
			menu_focus: cx.focus_handle(),
			speed: String::new(),
			collapsed_groups: Vec::new(),
			left_scroll: UniformListScrollHandle::new(),
		}
	}
}

/// Where focus returns when the menu closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuOrigin {
	Left,
	Log,
}

pub struct ContextMenu {
	pub origin: MenuOrigin,
	pub pos: Point<Pixels>,
	pub entries: Vec<MenuEntry>,
	/// Highlighted item (keyboard or hover).
	pub cursor: Option<usize>,
}

#[derive(Clone, Debug)]
pub enum MenuAct {
	Tree(TreeCommand),
	RevToggle { sha: String, path: String },
	ChangeToggle { idx: usize, path: String },
	ChangeOpen { idx: usize, path: String },
	GroupToggle(&'static str),
	CopyText(String),
	CommitDiff(String),
	CopyCommits(String),
	Select(String),
	BrowseTree(String),
}

#[derive(Clone, Debug)]
pub enum MenuEntry {
	Sep,
	Item {
		/// Probe id suffix: `menu-item:<id>`.
		id: &'static str,
		label: &'static str,
		shortcut: Option<String>,
		act: Option<MenuAct>,
	},
}

fn item(
	id: &'static str,
	label: &'static str,
	shortcut: Option<String>,
	act: Option<MenuAct>,
) -> MenuEntry {
	MenuEntry::Item {
		id,
		label,
		shortcut,
		act,
	}
}

/// Shortcut text as IntelliJ prints it on this platform.
fn secondary(key: &str) -> String {
	if cfg!(target_os = "macos") {
		format!("⌘{key}")
	} else {
		format!("Ctrl+{key}")
	}
}

fn copy_entries(abs: Option<String>, rel: &str) -> [MenuEntry; 2] {
	[
		item(
			"copy-path",
			"menu_copy_path",
			None,
			abs.map(MenuAct::CopyText),
		),
		item(
			"copy-relative-path",
			"menu_copy_relative_path",
			None,
			Some(MenuAct::CopyText(rel.to_string())),
		),
	]
}

fn basket_entry(selected: bool, act: Option<MenuAct>) -> MenuEntry {
	if selected {
		item(
			"remove-basket",
			"menu_remove_basket",
			Some("Space".into()),
			act,
		)
	} else {
		item("add-basket", "menu_add_basket", Some("Space".into()), act)
	}
}

impl WorkbenchModel {
	fn abs_path(&self, rel: &str) -> Option<String> {
		self.repo_root()
			.map(|r| r.join(rel.trim_end_matches('/')).display().to_string())
	}

	pub(crate) fn repo_row_menu(&self, idx: usize) -> Vec<MenuEntry> {
		let path = self.repos.get(idx).map(|r| r.root.display().to_string());
		vec![item(
			"copy-path",
			"menu_copy_path",
			None,
			path.map(MenuAct::CopyText),
		)]
	}

	pub(crate) fn work_row_menu(
		&self,
		row: &FlattenedTreeRow,
	) -> Vec<MenuEntry> {
		let toggle = row
			.is_valid_utf8
			.then(|| command_for_row(row, RowGesture::Toggle))
			.flatten()
			.map(MenuAct::Tree);
		let mut v = vec![basket_entry(row.selected, toggle), MenuEntry::Sep];
		v.extend(copy_entries(self.abs_path(&row.rel_path), &row.rel_path));
		v
	}

	pub(crate) fn rev_row_menu(
		&self,
		path: &str,
		is_file: bool,
	) -> Vec<MenuEntry> {
		let sha = self.rev_tree.as_ref().map(|t| t.sha.clone());
		let mut v = Vec::new();
		if let (true, Some(sha)) = (is_file, sha) {
			let selected = self.is_rev_file_selected(&sha, path);
			v.push(basket_entry(
				selected,
				Some(MenuAct::RevToggle {
					sha,
					path: path.to_string(),
				}),
			));
			v.push(MenuEntry::Sep);
		}
		v.push(item(
			"copy-relative-path",
			"menu_copy_relative_path",
			None,
			Some(MenuAct::CopyText(path.to_string())),
		));
		v
	}

	pub(crate) fn change_row_menu(&self, idx: usize) -> Vec<MenuEntry> {
		let Some(f) = self.files.get(idx) else {
			return Vec::new();
		};
		let path = f.path.clone();
		let toggle = f.is_valid_utf8().then(|| MenuAct::ChangeToggle {
			idx,
			path: path.clone(),
		});
		let mut v = vec![
			basket_entry(f.selected, toggle),
			item(
				"show-diff",
				"menu_show_diff",
				Some(secondary("D")),
				Some(MenuAct::ChangeOpen {
					idx,
					path: path.clone(),
				}),
			),
			MenuEntry::Sep,
		];
		v.extend(copy_entries(self.abs_path(&path), &path));
		v
	}

	pub(crate) fn change_group_menu(
		&self,
		group: &'static str,
	) -> Vec<MenuEntry> {
		let all = self.group_state(group) == Some(true);
		vec![basket_entry(all, Some(MenuAct::GroupToggle(group)))]
	}

	fn log_neighbours(&self, sha: &str) -> (Option<String>, Option<String>) {
		let rows = self.display_commits();
		let parent = rows
			.iter()
			.find(|c| c.sha == sha)
			.and_then(|c| c.parents.first())
			.filter(|p| rows.iter().any(|c| &c.sha == *p))
			.cloned();
		let child = rows
			.iter()
			.rev()
			.find(|c| c.parents.iter().any(|p| p == sha))
			.map(|c| c.sha.clone());
		(parent, child)
	}

	pub(crate) fn log_row_menu(&self, sha: &str) -> Vec<MenuEntry> {
		let (parent, child) = self.log_neighbours(sha);
		vec![
			item(
				"show-diff",
				"menu_show_diff",
				Some(secondary("D")),
				Some(MenuAct::CommitDiff(sha.to_string())),
			),
			item(
				"copy-revision",
				"menu_copy_revision",
				None,
				Some(MenuAct::CopyText(sha.to_string())),
			),
			item(
				"copy-commits",
				"btn_copy_commits",
				None,
				Some(MenuAct::CopyCommits(sha.to_string())),
			),
			MenuEntry::Sep,
			item(
				"go-parent",
				"menu_go_parent",
				Some("←".into()),
				parent.map(MenuAct::Select),
			),
			item(
				"go-child",
				"menu_go_child",
				Some("→".into()),
				child.map(MenuAct::Select),
			),
			MenuEntry::Sep,
			item(
				"browse-tree",
				"menu_browse_tree",
				None,
				Some(MenuAct::BrowseTree(sha.to_string())),
			),
		]
	}

	/// Right-click on a Log row: IntelliJ keeps a selection that already
	/// contains the row, otherwise selects it first.
	pub(crate) fn open_log_menu(
		&mut self,
		sha: &str,
		pos: Point<Pixels>,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let in_selection = self.selected_commit.as_deref() == Some(sha)
			|| self.range_rows().is_some_and(|(a, b)| {
				self.display_commits()
					.get(a..=b)
					.is_some_and(|s| s.iter().any(|c| c.sha == sha))
			});
		if !in_selection {
			self.select_commit(sha, cx);
		}
		let entries = self.log_row_menu(sha);
		self.open_menu(MenuOrigin::Log, entries, pos, window, cx);
	}

	pub(crate) fn open_menu(
		&mut self,
		origin: MenuOrigin,
		entries: Vec<MenuEntry>,
		pos: Point<Pixels>,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if entries.is_empty() {
			return;
		}
		self.close_popover(cx);
		self.workspace_menu = false;
		let ids: Vec<&str> = entries
			.iter()
			.filter_map(|e| match e {
				MenuEntry::Item { id, .. } => Some(*id),
				MenuEntry::Sep => None,
			})
			.collect();
		app_log!("[APP:MENU_OPEN: {:?} items={}]", origin, ids.join(","));
		self.chrome.menu = Some(ContextMenu {
			origin,
			pos,
			entries,
			cursor: None,
		});
		window.focus(&self.chrome.menu_focus);
		// Opened from a mouse press: keep the list below from taking focus.
		window.prevent_default();
		cx.notify();
	}

	pub(crate) fn close_menu(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let Some(m) = self.chrome.menu.take() else {
			return;
		};
		if self.chrome.menu_focus.is_focused(window) {
			window.focus(match m.origin {
				MenuOrigin::Left => &self.tree_focus,
				MenuOrigin::Log => &self.log_focus,
			});
		}
		app_log!("[APP:MENU_CLOSED]");
		cx.notify();
	}

	/// Enabled items, in order, for keyboard movement.
	fn menu_enabled(&self) -> Vec<usize> {
		self.chrome
			.menu
			.as_ref()
			.map(|m| {
				m.entries
					.iter()
					.enumerate()
					.filter(|(_, e)| {
						matches!(e, MenuEntry::Item { act: Some(_), .. })
					})
					.map(|(i, _)| i)
					.collect()
			})
			.unwrap_or_default()
	}

	pub(crate) fn menu_step(&mut self, down: bool, cx: &mut Context<Self>) {
		let enabled = self.menu_enabled();
		let Some(m) = self.chrome.menu.as_mut() else {
			return;
		};
		if enabled.is_empty() {
			return;
		}
		let pos = m.cursor.and_then(|c| enabled.iter().position(|&i| i == c));
		let next = match (pos, down) {
			(None, true) => 0,
			(None, false) => enabled.len() - 1,
			(Some(p), true) => (p + 1) % enabled.len(),
			(Some(p), false) => (p + enabled.len() - 1) % enabled.len(),
		};
		m.cursor = Some(enabled[next]);
		cx.notify();
	}

	pub(crate) fn menu_confirm(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let cursor = self.chrome.menu.as_ref().and_then(|m| m.cursor);
		if let Some(ix) = cursor {
			self.menu_pick(ix, window, cx);
		}
	}

	fn menu_pick(
		&mut self,
		ix: usize,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let picked =
			self.chrome
				.menu
				.as_ref()
				.and_then(|m| match m.entries.get(ix) {
					Some(MenuEntry::Item {
						id, act: Some(act), ..
					}) => Some((*id, act.clone())),
					_ => None,
				});
		let Some((id, act)) = picked else {
			return;
		};
		self.close_menu(window, cx);
		app_log!("[APP:MENU_ACTION: {id}]");
		self.run_menu_act(act, window, cx);
	}

	fn copy_text(&mut self, text: &str) {
		match snip_core::clip::write_text(text) {
			Ok(()) => {
				self.set_status("status_text_copied", [text.to_string()]);
				app_log!("[APP:TEXT_COPIED: bytes={}]", text.len());
			}
			Err(e) => {
				self.set_status("status_clipboard_failed", [e.to_string()])
			}
		}
	}

	fn run_menu_act(
		&mut self,
		act: MenuAct,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let file_at = |this: &Self, idx: usize, path: &str| {
			this.files
				.get(idx)
				.filter(|f| f.path == path)
				.map(|f| f.source.clone())
		};
		match act {
			MenuAct::Tree(cmd) => self.dispatch_tree(Some(cmd), cx),
			MenuAct::RevToggle { sha, path } => {
				self.toggle_rev_file_selection(&sha, &path, cx)
			}
			MenuAct::ChangeToggle { idx, path } => {
				if file_at(self, idx, &path).is_some() {
					self.toggle_file(idx, cx);
				}
			}
			MenuAct::ChangeOpen { idx, path } => {
				if let Some(source) = file_at(self, idx, &path) {
					self.select_file_with_source(&path, source, cx);
					window.focus(&self.reader_focus);
				}
			}
			MenuAct::GroupToggle(group) => self.toggle_change_group(group, cx),
			MenuAct::CopyText(text) => self.copy_text(&text),
			MenuAct::CommitDiff(sha) => {
				if self.selected_commit.as_deref() != Some(sha.as_str()) {
					self.select_commit(&sha, cx);
				}
				window.focus(&self.reader_focus);
			}
			MenuAct::CopyCommits(_) => self.copy_commits_to_clipboard(cx),
			MenuAct::Select(sha) => self.log_select_row(&sha, cx),
			MenuAct::BrowseTree(sha) => {
				if self.selected_commit.as_deref() != Some(sha.as_str()) {
					self.select_commit(&sha, cx);
				}
				self.browse_commit_tree(cx);
			}
		}
		cx.notify();
	}

	// ───────────────────────── Changes groups ─────────────────────────

	/// Tri-state of a Changes group: all / none selected, `None` if mixed.
	/// Unselectable (non-UTF-8) names do not count.
	pub(crate) fn group_state(&self, group: &str) -> Option<bool> {
		let mut sel = self
			.files
			.iter()
			.filter(|f| change_group(f) == Some(group) && f.is_valid_utf8())
			.map(|f| f.selected);
		let first = sel.next().unwrap_or(false);
		sel.all(|s| s == first).then_some(first)
	}

	/// IntelliJ group checkbox: selects the whole group unless it already is
	/// fully selected, in which case it clears it.
	pub(crate) fn toggle_change_group(
		&mut self,
		group: &str,
		cx: &mut Context<Self>,
	) {
		let select = self.group_state(group) != Some(true);
		let mut candidate = self.selection_candidate();
		for f in &mut candidate.files {
			if change_group(f) == Some(group) && f.is_valid_utf8() {
				f.selected = select;
			}
		}
		candidate.replace_git_group = true;
		candidate.remove_only = !select;
		candidate.status = Some(crate::i18n::Msg::new(
			if select {
				"status_selected_all"
			} else {
				"status_selection_removed"
			},
			[],
		));
		if self.install_selection_candidate(candidate) {
			self.log_basket();
			app_log!("[APP:GROUP_TOGGLED: {group} selected={select}]");
		}
		cx.notify();
	}

	pub(crate) fn toggle_group_collapsed(
		&mut self,
		group: &'static str,
		cx: &mut Context<Self>,
	) {
		let c = &mut self.chrome.collapsed_groups;
		let collapsed = if let Some(i) = c.iter().position(|g| *g == group) {
			c.remove(i);
			false
		} else {
			c.push(group);
			true
		};
		app_log!("[APP:GROUP_COLLAPSED: {group} collapsed={collapsed}]");
		cx.notify();
	}

	// ───────────────────────── shortcuts ─────────────────────────

	/// Shift+Esc: hides the focused tool window (else the open left one)
	/// and returns focus to the editor.
	pub(crate) fn hide_active_tool(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if self.log_focus.is_focused(window) && self.bottom_visible {
			self.bottom_visible = false;
			self.log_before_paste = None;
			app_log!("[APP:LOG_PANEL: visible=false]");
		} else if self.left_visible {
			self.activate_tool(self.active_tab, cx);
		}
		window.focus(&self.reader_focus);
		cx.notify();
	}

	/// Esc in a tool window: first ends speed search, then goes to the editor.
	pub(crate) fn escape_to_editor(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		if !self.chrome.speed.is_empty() {
			self.chrome.speed.clear();
			app_log!("[APP:SPEED_SEARCH: off]");
		} else {
			window.focus(&self.reader_focus);
			app_log!("[APP:FOCUS: editor]");
		}
		cx.notify();
	}

	fn log_select_row(&mut self, sha: &str, cx: &mut Context<Self>) {
		if let Some(ix) =
			self.display_commits().iter().position(|c| c.sha == sha)
		{
			self.log_scroll.scroll_to_item(ix, ScrollStrategy::Center);
			self.select_commit(sha, cx);
		}
	}

	/// Log ←/→: first parent / first child on the loaded page.
	pub(crate) fn log_go(&mut self, parent: bool, cx: &mut Context<Self>) {
		let Some(sel) = self.selected_commit.clone() else {
			return;
		};
		let (p, c) = self.log_neighbours(&sel);
		if let Some(sha) = if parent { p } else { c } {
			self.log_select_row(&sha, cx);
		}
	}

	/// Rows one PageUp/PageDown moves in the Log.
	pub(crate) fn log_page_rows(&self) -> isize {
		((self.bottom_h / crate::graph_view::ROW_HEIGHT) as isize - 3).max(1)
	}

	// ───────────────────────── rendering ─────────────────────────

	pub(crate) fn render_context_menu(
		&self,
		cx: &mut Context<Self>,
	) -> Option<AnyElement> {
		let m = self.chrome.menu.as_ref()?;
		let loc = self.locale;
		let log = &self.probes;
		let rows: Vec<AnyElement> = m
			.entries
			.iter()
			.enumerate()
			.map(|(ix, e)| match e {
				MenuEntry::Sep => div()
					.h(px(1.))
					.mx(px(8.))
					.my(px(4.))
					.bg(rgb(pal().popup_border))
					.into_any_element(),
				MenuEntry::Item {
					id,
					label,
					shortcut,
					act,
				} => {
					let enabled = act.is_some();
					let hot = m.cursor == Some(ix) && enabled;
					let pid = format!("menu-item:{id}");
					div()
						.id(SharedString::from(pid.clone()))
						.relative()
						.flex()
						.flex_row()
						.items_center()
						.gap(px(16.))
						.h(px(ROW_H))
						.px(px(8.))
						.rounded(px(4.))
						.text_color(rgb(if enabled {
							pal().text
						} else {
							pal().text_disabled
						}))
						.when(hot, |d| d.bg(rgb(pal().selection_bg)))
						.when(enabled, |d| {
							d.cursor_pointer()
								.on_mouse_move(cx.listener(
									move |this, _: &MouseMoveEvent, _, cx| {
										if let Some(m) =
											this.chrome.menu.as_mut()
										{
											if m.cursor != Some(ix) {
												m.cursor = Some(ix);
												cx.notify();
											}
										}
									},
								))
								.on_click(cx.listener(
									move |this, _, window, cx| {
										this.menu_pick(ix, window, cx)
									},
								))
						})
						.child(
							div()
								.flex_1()
								.min_w_0()
								.whitespace_nowrap()
								.child(t(label, loc)),
						)
						.when_some(shortcut.clone(), |d, s| {
							d.child(
								div()
									.flex_shrink_0()
									.text_color(rgb(if enabled {
										pal().text_muted
									} else {
										pal().text_disabled
									}))
									.child(s),
							)
						})
						.children(probe(log, pid))
						.into_any_element()
				}
			})
			.collect();
		let panel = div()
			.id("context-menu")
			.relative()
			.occlude()
			.track_focus(&self.chrome.menu_focus)
			.key_context("ContextMenu")
			.on_action(cx.listener(|this, _: &crate::MenuUp, _, cx| {
				this.menu_step(false, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::MenuDown, _, cx| {
				this.menu_step(true, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::MenuConfirm, w, cx| {
				this.menu_confirm(w, cx)
			}))
			.on_action(cx.listener(|this, _: &crate::MenuCancel, w, cx| {
				this.close_menu(w, cx)
			}))
			.on_mouse_down_out(
				cx.listener(|this, _, w, cx| this.close_menu(w, cx)),
			)
			// A right click inside the menu must not reach the row below.
			.on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
			.min_w(px(200.))
			.max_w(px(360.))
			.flex()
			.flex_col()
			.p(px(4.))
			.bg(rgb(pal().popup_bg))
			.border_1()
			.border_color(rgb(pal().popup_border))
			.rounded(px(ISLAND_RADIUS))
			.shadow_lg()
			.font_family(UI_FONT)
			.text_size(px(UI_TEXT))
			.font_weight(FontWeight::NORMAL)
			.children(rows)
			.children(probe(log, "context-menu"));
		Some(
			div()
				.absolute()
				.top_0()
				.left_0()
				.child(
					deferred(
						anchored()
							.position(m.pos)
							.snap_to_window_with_margin(px(4.))
							.child(panel),
					)
					.with_priority(2),
				)
				.into_any_element(),
		)
	}
}

/// Changes group of a file, as `change_item_rows` shows it.
pub(crate) fn change_group(f: &crate::FileChangeItem) -> Option<&'static str> {
	if f.is_conflict {
		return Some("conflicted");
	}
	match f.source {
		SourceKind::Staged => Some("staged"),
		SourceKind::Unstaged => Some("unstaged"),
		SourceKind::Working => Some("untracked"),
		_ => None,
	}
}

/// Changes groups in display order: (id, label key).
pub(crate) const CHANGE_GROUPS: [(&str, &str); 4] = [
	("conflicted", "group_conflicted"),
	("staged", "group_staged"),
	("unstaged", "group_unstaged"),
	("untracked", "group_untracked"),
];
