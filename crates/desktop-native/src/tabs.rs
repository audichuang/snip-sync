//! Workspace tabs: the one main window shows one workspace per tab.
//!
//! [`TabsRoot`] is the window's root view. It holds one
//! [`WorkbenchModel`] per [`WorkspaceTab`], draws the tab bar under the
//! native title bar and renders only the active model. A model knows it is a
//! tab through `ws_tab` and reports what only the root can do as a
//! [`TabEvent`]: open a workspace (in a new tab, or the tab that already has
//! it), drop itself after its close drained, and quit.
//!
//! Background tabs keep their state and are not refreshed when shown again.
//! A closing tab drains only its own owned jobs
//! ([`crate::lifecycle::GitLoad::own_tab`]); Quit drains every tab and still
//! waits for the process-wide Git counters.

use std::cell::Cell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use gpui::{
	div, prelude::*, px, rgb, AnyElement, App, Context, Entity, FocusHandle,
	MouseButton, ScrollHandle, SharedString, Subscription, Window,
};
use snip_remote::{RemoteHost, RemoteWorkspace};

use crate::i18n::{t, Locale};
use crate::icons::{icon_tinted, Icon};
use crate::theme::*;
use crate::ui::{clip_text, probe, probe_frame_end, tip, Probes};
use crate::{
	lifecycle, CloseWorkspace, NextWorkspaceTab, OpenWorkspace,
	PrevWorkspaceTab, Quit, WorkbenchModel, WorkspaceTab1, WorkspaceTab2,
	WorkspaceTab3, WorkspaceTab4, WorkspaceTab5, WorkspaceTab6, WorkspaceTab7,
	WorkspaceTab8, WorkspaceTab9,
};

/// A line of the root (the tab bar, quit, window size): never tagged, even
/// when a tab's event led to it.
macro_rules! root_log {
	($($arg:tt)*) => {
		with_log_tab(None, || app_log!($($arg)*))
	};
}

/// Height of the tab bar under the native title bar.
pub const TAB_BAR_H: f32 = 34.0;
const TAB_MAX_W: f32 = 220.0;

// ───────────────────────────── log tagging ─────────────────────────────

thread_local! {
	/// The tab whose code is running: set while a tab's owned future or
	/// spawned task is polled, and while the root calls into a tab.
	static LOG_TAB: Cell<Option<u32>> = const { Cell::new(None) };
	/// The tab the window shows.
	static ACTIVE_TAB: Cell<Option<u32>> = const { Cell::new(None) };
}

pub(crate) fn set_active_tab(id: Option<u32>) {
	ACTIVE_TAB.with(|a| a.set(id));
}

/// True unless `tab` is a workspace tab that is not the one shown.
pub(crate) fn is_shown(tab: Option<u32>) -> bool {
	tab.is_none() || ACTIVE_TAB.with(Cell::get) == tab
}

/// Runs `f` as code of `tab`.
pub(crate) fn with_log_tab<R>(tab: Option<u32>, f: impl FnOnce() -> R) -> R {
	let prev = LOG_TAB.with(|c| c.replace(tab));
	let out = f();
	LOG_TAB.with(|c| c.set(prev));
	out
}

/// A `[APP:…]` line printed by a background tab carries ` ws_tab=<id>`
/// inside its brackets; lines of the shown tab and of the root are left
/// byte for byte as a one-tab script expects them.
pub(crate) fn tag_line(line: String) -> String {
	let Some(tab) = LOG_TAB.with(Cell::get) else {
		return line;
	};
	if ACTIVE_TAB.with(Cell::get) == Some(tab) {
		return line;
	}
	tag_with(line, tab)
}

fn tag_with(line: String, tab: u32) -> String {
	if !line.starts_with("[APP:") || !line.ends_with(']') {
		return line;
	}
	let body = &line[..line.len() - 1];
	if body.contains(": ") {
		format!("{body} ws_tab={tab}]")
	} else {
		format!("{body}: ws_tab={tab}]")
	}
}

/// Polls `fut` as code of `tab`, so whatever it logs is tagged.
pub(crate) struct Tagged<F> {
	tab: Option<u32>,
	fut: Pin<Box<F>>,
}

pub(crate) fn tagged<F: Future>(tab: Option<u32>, fut: F) -> Tagged<F> {
	Tagged {
		tab,
		fut: Box::pin(fut),
	}
}

impl<F: Future> Future for Tagged<F> {
	type Output = F::Output;

	fn poll(
		mut self: Pin<&mut Self>,
		cx: &mut TaskContext<'_>,
	) -> Poll<Self::Output> {
		let tab = self.tab;
		with_log_tab(tab, || self.fut.as_mut().poll(cx))
	}
}

// ─────────────────────────── model ↔ root ───────────────────────────

/// What a tab asks of the root.
pub enum TabEvent {
	/// Open this workspace: switch to the tab that has it, or fill this
	/// tab when it is empty, or open a new tab.
	Open(OpenTarget),
	/// The close drained and released the workspace: drop the tab.
	Closed,
	/// Quit was pressed inside the tab.
	QuitRequested,
	/// This tab's part of a quit drained.
	QuitDrained,
	/// A drain gave up (timeout or a leaked Git slot); the tab stays.
	DrainFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
	/// A canonical local folder.
	Local(PathBuf),
	/// A worker folder, after the worker resolved its real path.
	Remote(Box<(RemoteHost, RemoteWorkspace)>),
}

/// Which workspace a tab shows: a local canonical path, or an ssh Host
/// alias with the worker's real path. Two aliases of one machine are two
/// workspaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsIdentity {
	Local(PathBuf),
	Remote { host: String, path: String },
}

impl OpenTarget {
	pub fn identity(&self) -> WsIdentity {
		match self {
			OpenTarget::Local(path) => WsIdentity::Local(path.clone()),
			OpenTarget::Remote(target) => WsIdentity::Remote {
				host: target.0.name.clone(),
				path: target.1.id.clone(),
			},
		}
	}

	pub fn into_intent(self) -> lifecycle::Intent {
		match self {
			OpenTarget::Local(path) => lifecycle::Intent::OpenWorkspace(path),
			OpenTarget::Remote(target) => {
				lifecycle::Intent::OpenRemoteWorkspace(target)
			}
		}
	}
}

impl WsIdentity {
	/// The workspace a drain is about to open.
	pub fn of_intent(intent: &lifecycle::Intent) -> Option<Self> {
		match intent {
			lifecycle::Intent::OpenWorkspace(path) => {
				Some(WsIdentity::Local(path.clone()))
			}
			lifecycle::Intent::OpenRemoteWorkspace(target) => {
				Some(WsIdentity::Remote {
					host: target.0.name.clone(),
					path: target.1.id.clone(),
				})
			}
			_ => None,
		}
	}
}

/// A remote tab's connection, as its label shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conn {
	Connecting,
	Connected,
	Failed,
}

/// What the tab bar shows for one tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabInfo {
	/// None: an empty tab.
	pub identity: Option<WsIdentity>,
	/// Full path for the hover text (`host:path` for a remote tab).
	pub full: String,
	pub conn: Option<Conn>,
	pub pasting: bool,
}

// ─────────────────────────────── labels ───────────────────────────────

fn components(identity: &WsIdentity) -> Vec<String> {
	let path = match identity {
		WsIdentity::Local(path) => path.as_path(),
		WsIdentity::Remote { path, .. } => Path::new(path.as_str()),
	};
	let mut parts: Vec<String> = path
		.components()
		.filter_map(|c| match c {
			std::path::Component::Normal(s) => {
				Some(s.to_string_lossy().into_owned())
			}
			_ => None,
		})
		.collect();
	if parts.is_empty() {
		parts.push(path.display().to_string());
	}
	parts
}

fn label_with(identity: &WsIdentity, depth: usize) -> String {
	let parts = components(identity);
	let from = parts.len().saturating_sub(depth.max(1));
	let name = parts[from..].join("/");
	match identity {
		WsIdentity::Local(_) => name,
		WsIdentity::Remote { host, .. } => format!("{host} ▸ {name}"),
	}
}

/// Tab labels: a local folder's name, `host ▸ name` for a remote one. Tabs
/// whose labels collide get parent folders until they differ (or the path
/// runs out). An empty tab is `new_tab`.
pub fn tab_labels(
	identities: &[Option<WsIdentity>],
	new_tab: &str,
) -> Vec<String> {
	let mut depth = vec![1usize; identities.len()];
	loop {
		let labels: Vec<Option<String>> = identities
			.iter()
			.zip(&depth)
			.map(|(id, d)| id.as_ref().map(|id| label_with(id, *d)))
			.collect();
		let mut grew = false;
		for i in 0..identities.len() {
			let (Some(label), Some(id)) = (&labels[i], &identities[i]) else {
				continue;
			};
			let clash = labels
				.iter()
				.enumerate()
				.any(|(j, other)| j != i && other.as_ref() == Some(label));
			if clash && depth[i] < components(id).len() {
				depth[i] += 1;
				grew = true;
			}
		}
		if !grew {
			return labels
				.into_iter()
				.map(|l| l.unwrap_or_else(|| new_tab.to_string()))
				.collect();
		}
	}
}

// ─────────────────────────────── the root ───────────────────────────────

pub struct WorkspaceTab {
	pub id: u32,
	pub model: Entity<WorkbenchModel>,
	_subs: [Subscription; 2],
}

pub struct TabsRoot {
	pub tabs: Vec<WorkspaceTab>,
	pub active: Option<usize>,
	next_id: u32,
	mode: String,
	pub focus_handle: FocusHandle,
	probes: Option<Probes>,
	/// Tabs whose part of a quit has not drained yet.
	quitting: Option<Vec<u32>>,
	/// Tabs whose part of the quit drained; reloaded if the quit stops.
	quit_drained: Vec<u32>,
	pub(crate) title: String,
	scroll: ScrollHandle,
	/// `cx.quit` was called (the test platform's quit does nothing).
	pub quit_sent: bool,
	/// The window's physical size last reported as `[APP:VIEWPORT]`.
	last_viewport: (i32, i32),
	/// `--restore-dir`: every tab's paste destination, as the one
	/// workbench kept it across a close and reopen.
	restore_dir: Option<PathBuf>,
}

/// What the first tab opens.
pub enum FirstTab {
	Local(PathBuf),
	/// Reconnected from an empty tab once the window is up.
	Remote(crate::remote::RecentRemote),
	Empty,
}

impl TabsRoot {
	pub fn new(
		first: FirstTab,
		restore_dir: Option<PathBuf>,
		mode: String,
		probes: Option<Probes>,
		window: &mut Window,
		cx: &mut Context<Self>,
	) -> Self {
		let mut root = Self {
			tabs: Vec::new(),
			active: None,
			next_id: 1,
			mode,
			focus_handle: cx.focus_handle(),
			probes,
			quitting: None,
			quit_drained: Vec::new(),
			title: String::new(),
			scroll: ScrollHandle::new(),
			quit_sent: false,
			last_viewport: (0, 0),
			restore_dir,
		};
		let workspace = match &first {
			FirstTab::Local(path) => {
				Some(dunce::canonicalize(path).unwrap_or(path.clone()))
			}
			_ => None,
		};
		let ix = root.add_tab(workspace, false, window, cx);
		root.activate(ix, window, cx);
		if let FirstTab::Remote(last) = first {
			let tab = &root.tabs[ix];
			let id = tab.id;
			with_log_tab(Some(id), || {
				tab.model.update(cx, |m, cx| m.reopen_last_remote(last, cx))
			});
		}
		root
	}

	fn add_tab(
		&mut self,
		workspace: Option<PathBuf>,
		menu_open: bool,
		window: &mut Window,
		cx: &mut Context<Self>,
	) -> usize {
		let id = self.next_id;
		self.next_id += 1;
		let mode = self.mode.clone();
		let probes = self.probes.clone();
		let restore_dir = self.restore_dir.clone();
		let model = with_log_tab(Some(id), || {
			cx.new(|cx| {
				let mut m = WorkbenchModel::new_tab(
					workspace,
					restore_dir,
					mode,
					id,
					probes,
					cx,
				);
				m.workspace_menu = menu_open;
				m
			})
		});
		let subs = [
			cx.subscribe_in(&model, window, Self::on_tab_event),
			cx.observe(&model, |_, _, cx| cx.notify()),
		];
		self.tabs.push(WorkspaceTab {
			id,
			model,
			_subs: subs,
		});
		if crate::e2e_on() {
			root_log!("[APP:WS_TAB_OPENED: id={id} count={}]", self.tabs.len());
		}
		self.tabs.len() - 1
	}

	/// The "+" button and Cmd/Ctrl+Shift+O with no tab: an empty tab with
	/// the workspace menu open.
	pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
		let ix = self.add_tab(None, true, window, cx);
		self.activate(ix, window, cx);
	}

	pub fn activate(
		&mut self,
		ix: usize,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let Some(tab) = self.tabs.get(ix) else {
			return;
		};
		let (id, model) = (tab.id, tab.model.clone());
		if self.active == Some(ix) {
			// A click on the shown tab leaves the keyboard where it is.
			let fh = model.read(cx).focus_handle.clone();
			if !fh.contains_focused(window, cx) {
				window.focus(&fh);
			}
			return;
		}
		if let Some(old) = self.active.and_then(|i| self.tabs.get(i)) {
			// The tab gets back the pane it had the keyboard in; a field
			// or a menu it had open does not outlive the switch.
			let old_id = old.id;
			let keep = window.focused(cx).and_then(|f| {
				let m = old.model.read(cx);
				[&m.paste_focus, &m.reader_focus, &m.tree_focus, &m.log_focus]
					.into_iter()
					.find(|pane| pane.contains(&f, window))
					.cloned()
			});
			with_log_tab(Some(old_id), || {
				old.model.update(cx, |m, cx| m.leave_tab(keep, cx))
			});
		}
		self.active = Some(ix);
		set_active_tab(Some(id));
		self.scroll.scroll_to_item(ix);
		if crate::e2e_on() {
			root_log!("[APP:WS_TAB_ACTIVE: id={id} ix={ix}]");
		}
		// Focused now for the keys that follow at once, and again when
		// the tab first draws: the handle is not in this frame yet.
		let fh = model.update(cx, |m, _| {
			let fh = m.resume_focus.take().unwrap_or(m.focus_handle.clone());
			m.pending_focus = Some(fh.clone());
			fh
		});
		window.focus(&fh);
		cx.notify();
	}

	fn index_of(&self, model: &Entity<WorkbenchModel>) -> Option<usize> {
		self.tabs.iter().position(|t| &t.model == model)
	}

	fn find(&self, identity: &WsIdentity, cx: &App) -> Option<usize> {
		self.tabs.iter().position(|t| {
			t.model.read(cx).ws_identity().as_ref() == Some(identity)
		})
	}

	fn step(
		&mut self,
		delta: isize,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let n = self.tabs.len() as isize;
		if n == 0 {
			return;
		}
		let cur = self.active.unwrap_or(0) as isize;
		let ix = (cur + delta).rem_euclid(n) as usize;
		self.activate(ix, window, cx);
	}

	fn jump(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
		if ix < self.tabs.len() {
			self.activate(ix, window, cx);
		}
	}

	/// Cmd/Ctrl+W, Cmd/Ctrl+Shift+W, the tab's ×: the tab's own close
	/// drain, which a confirmed paste refuses.
	pub fn close_tab(
		&mut self,
		ix: usize,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let Some(tab) = self.tabs.get(ix) else {
			return;
		};
		if self.active != Some(ix) && tab.model.read(cx).blocks_close() {
			// The refusal is shown where the paste runs.
			self.activate(ix, window, cx);
		}
		let Some(tab) = self.tabs.get(ix) else {
			return;
		};
		let id = tab.id;
		with_log_tab(Some(id), || {
			tab.model.update(cx, |m, cx| {
				m.request_user_close(lifecycle::Intent::CloseWorkspace, cx)
			})
		});
	}

	fn remove_tab(
		&mut self,
		ix: usize,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let tab = self.tabs.remove(ix);
		if let Some(q) = self.quitting.as_mut() {
			q.retain(|id| *id != tab.id);
		}
		if crate::e2e_on() {
			root_log!(
				"[APP:WS_TAB_CLOSED: id={} count={}]",
				tab.id,
				self.tabs.len()
			);
		}
		drop(tab);
		match self.active {
			Some(a) if a == ix => {
				self.active = None;
				if self.tabs.is_empty() {
					set_active_tab(None);
					window.focus(&self.focus_handle);
					cx.notify();
				} else {
					self.activate(ix.min(self.tabs.len() - 1), window, cx);
				}
			}
			Some(a) if a > ix => {
				self.active = Some(a - 1);
				cx.notify();
			}
			_ => cx.notify(),
		}
		self.maybe_quit(cx);
	}

	fn open_target(
		&mut self,
		from: &Entity<WorkbenchModel>,
		target: OpenTarget,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let identity = target.identity();
		let from_ix = self.index_of(from);
		if let Some(ix) = self.find(&identity, cx) {
			self.activate(ix, window, cx);
			// An empty tab opened only to reach a workspace that is
			// already open goes away.
			if let Some(f) = from_ix.filter(|f| *f != ix) {
				if self.tabs[f].model.read(cx).is_empty_tab() {
					self.close_tab(f, window, cx);
				}
			}
			return;
		}
		let ix = match from_ix {
			Some(f) if self.tabs[f].model.read(cx).is_empty_tab() => f,
			_ => self.add_tab(None, false, window, cx),
		};
		self.activate(ix, window, cx);
		let tab = &self.tabs[ix];
		let id = tab.id;
		with_log_tab(Some(id), || {
			tab.model.update(cx, |m, cx| m.open_in_place(target, cx))
		});
	}

	fn on_tab_event(
		&mut self,
		model: &Entity<WorkbenchModel>,
		ev: &TabEvent,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let Some(ix) = self.index_of(model) else {
			return;
		};
		let id = self.tabs[ix].id;
		match ev {
			TabEvent::Open(target) => {
				self.open_target(model, target.clone(), window, cx)
			}
			TabEvent::Closed => self.remove_tab(ix, window, cx),
			TabEvent::QuitRequested => self.begin_quit(window, cx),
			TabEvent::QuitDrained => {
				let mut waiting = false;
				if let Some(q) = self.quitting.as_mut() {
					waiting = q.contains(&id);
					q.retain(|t| *t != id);
				}
				if waiting {
					self.quit_drained.push(id);
					self.maybe_quit(cx);
				} else {
					// The quit stopped meanwhile: reload what it cancelled.
					with_log_tab(Some(id), || {
						model.update(cx, |m, cx| m.resume_after_drain(cx))
					});
				}
			}
			TabEvent::DrainFailed => {
				if self.quitting.take().is_some() {
					for other in std::mem::take(&mut self.quit_drained) {
						if let Some(t) =
							self.tabs.iter().find(|t| t.id == other)
						{
							with_log_tab(Some(other), || {
								t.model.update(cx, |m, cx| {
									m.resume_after_drain(cx)
								})
							});
						}
					}
					self.activate(ix, window, cx);
				}
			}
		}
	}

	/// Cmd/Ctrl+Q and the window's close button. A tab with a confirmed
	/// paste refuses before any tab starts draining, and is shown.
	pub fn begin_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
		if crate::e2e_on() {
			root_log!("[APP:QUIT: deferred]");
		}
		if self.quitting.is_some() {
			return;
		}
		if let Some(ix) = self
			.tabs
			.iter()
			.position(|t| t.model.read(cx).blocks_close())
		{
			self.activate(ix, window, cx);
			let tab = &self.tabs[ix];
			let id = tab.id;
			with_log_tab(Some(id), || {
				tab.model.update(cx, |m, cx| {
					m.request_user_close(lifecycle::Intent::Quit, cx)
				})
			});
			return;
		}
		if self.tabs.is_empty() && crate::e2e_on() {
			// What a drained tab reports, for drivers that wait on it.
			let line = lifecycle::format_line(
				"drained",
				"quit",
				None,
				0,
				lifecycle::GitLoad::current(),
				0,
			);
			root_log!("[APP:LIFECYCLE: {line}]");
		}
		self.quitting = Some(self.tabs.iter().map(|t| t.id).collect());
		self.quit_drained.clear();
		for tab in &self.tabs {
			let id = tab.id;
			with_log_tab(Some(id), || {
				tab.model.update(cx, |m, cx| {
					m.request_user_close(lifecycle::Intent::Quit, cx)
				})
			});
		}
		self.maybe_quit(cx);
	}

	fn maybe_quit(&mut self, cx: &mut Context<Self>) {
		if self.quitting.as_ref().is_some_and(Vec::is_empty) {
			self.quit_sent = true;
			// Never inside the platform callback that asked (the window's
			// close button): the X11 client is still borrowed there, and a
			// deferred effect still runs inside it. A task runs on the next
			// turn of the event loop, as a drained tab's quit always did.
			cx.spawn(async move |_, cx| {
				let _ = cx.update(|cx| cx.quit());
			})
			.detach();
		}
	}

	fn locale(&self, cx: &App) -> Locale {
		self.active
			.and_then(|i| self.tabs.get(i))
			.map(|t| t.model.read(cx).locale)
			.unwrap_or_default()
	}

	/// Labels and info of every tab, in order.
	pub fn tab_views(&self, cx: &App) -> Vec<(String, TabInfo)> {
		let infos: Vec<TabInfo> = self
			.tabs
			.iter()
			.map(|t| t.model.read(cx).tab_info())
			.collect();
		let ids: Vec<Option<WsIdentity>> =
			infos.iter().map(|i| i.identity.clone()).collect();
		let labels = tab_labels(&ids, t("ws_tab_new", self.locale(cx)));
		labels.into_iter().zip(infos).collect()
	}

	fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
		let loc = self.locale(cx);
		let views = self.tab_views(cx);
		let mut strip = div()
			.id("ws-tabs")
			.flex()
			.flex_row()
			.items_center()
			.gap(px(2.))
			.min_w_0()
			.flex_shrink()
			.overflow_x_scroll()
			.track_scroll(&self.scroll);
		for (ix, (label, info)) in views.into_iter().enumerate() {
			strip = strip.child(self.render_tab(ix, label, info, loc, cx));
		}
		div()
			.flex()
			.flex_row()
			.items_center()
			.flex_shrink_0()
			.h(px(TAB_BAR_H))
			.px(px(6.))
			.gap(px(4.))
			.bg(rgb(pal().header_bg))
			.border_b_1()
			.border_color(rgb(pal().divider))
			.child(strip)
			.child(
				div()
					.id("ws-tab-new")
					.relative()
					.flex()
					.flex_shrink_0()
					.items_center()
					.justify_center()
					.size(px(24.))
					.rounded(px(4.))
					.cursor_pointer()
					.hover(|s| s.bg(rgb(pal().hover_bg)))
					.child(icon_tinted(Icon::Plus, 16., pal().text_muted))
					.tooltip(tip(t("tip_ws_tab_new", loc)))
					.debug_selector(|| "ws-tab-new".into())
					.on_click(cx.listener(|this, _, window, cx| {
						this.new_tab(window, cx)
					}))
					.children(probe(&self.probes, "ws-tab-new")),
			)
			.into_any_element()
	}

	fn render_tab(
		&self,
		ix: usize,
		label: String,
		info: TabInfo,
		loc: Locale,
		cx: &mut Context<Self>,
	) -> AnyElement {
		let active = self.active == Some(ix);
		let p = pal();
		let kind = match info.conn {
			Some(Conn::Failed) => (Icon::Warning, p.error),
			Some(Conn::Connecting) => (Icon::Refresh, p.text_muted),
			Some(Conn::Connected) => (Icon::RemoteBranch, p.accent),
			None => (Icon::Folder, p.text_muted),
		};
		let state = match info.conn {
			Some(Conn::Connecting) => "connecting",
			Some(Conn::Connected) => "connected",
			Some(Conn::Failed) => "failed",
			None => "local",
		};
		let hover = match info.conn {
			Some(Conn::Connecting) | Some(Conn::Failed) => {
				let what = if info.conn == Some(Conn::Failed) {
					t("ws_tab_failed", loc)
				} else {
					t("ws_tab_connecting", loc)
				};
				let base = if info.full.is_empty() {
					&label
				} else {
					&info.full
				};
				format!("{base} · {what}")
			}
			_ if info.full.is_empty() => label.clone(),
			_ => info.full.clone(),
		};
		let hover = if info.pasting {
			format!("{hover} · {}", t("ws_tab_pasting", loc))
		} else {
			hover
		};
		let tab_id = format!("ws-tab:{ix}");
		let close_id = format!("ws-tab-close:{ix}");
		let state_id = format!("ws-tab-state:{ix}:{state}");
		let paste_id = format!("ws-tab-pasting:{ix}");
		let selector = tab_id.clone();
		let close_selector = close_id.clone();
		div()
			.id(SharedString::from(tab_id.clone()))
			.relative()
			.flex()
			.flex_row()
			.flex_shrink_0()
			.items_center()
			.gap(px(4.))
			.h(px(26.))
			.max_w(px(TAB_MAX_W))
			.pl(px(8.))
			.pr(px(4.))
			.rounded(px(6.))
			.cursor_pointer()
			.when(active, |d| d.bg(rgb(p.range_bg)).text_color(rgb(p.text)))
			.when(!active, |d| {
				d.text_color(rgb(p.text_muted))
					.hover(|s| s.bg(rgb(p.hover_bg)))
			})
			.child(
				div()
					.relative()
					.flex_shrink_0()
					.child(icon_tinted(kind.0, 14., kind.1))
					.debug_selector({
						let id = state_id.clone();
						move || id
					})
					.children(probe(&self.probes, state_id)),
			)
			.child(clip_text(label))
			.when(info.pasting, |d| {
				d.child(
					div()
						.relative()
						.flex_shrink_0()
						.child(icon_tinted(Icon::Paste, 14., p.accent))
						.debug_selector({
							let id = paste_id.clone();
							move || id
						})
						.children(probe(&self.probes, paste_id.clone())),
				)
			})
			.child(
				div()
					.id(SharedString::from(close_id.clone()))
					.relative()
					.flex()
					.flex_shrink_0()
					.items_center()
					.justify_center()
					.size(px(16.))
					.rounded(px(3.))
					.hover(|s| s.bg(rgb(p.hover_bg)))
					.child(icon_tinted(Icon::Close, 12., p.text_muted))
					.tooltip(tip(crate::ui::mac_keys(t(
						"tip_ws_tab_close",
						loc,
					))))
					.debug_selector(move || close_selector)
					.on_mouse_down(MouseButton::Left, |_, _, cx| {
						cx.stop_propagation()
					})
					.on_click(cx.listener(move |this, _, window, cx| {
						cx.stop_propagation();
						this.close_tab(ix, window, cx);
					}))
					.children(probe(&self.probes, close_id)),
			)
			.tooltip(tip(hover))
			.debug_selector(move || selector)
			.on_mouse_down(
				MouseButton::Left,
				cx.listener(move |this, _, window, cx| {
					// No focusable ancestor may take the keyboard after us.
					window.prevent_default();
					this.activate(ix, window, cx)
				}),
			)
			.children(probe(&self.probes, tab_id))
			.into_any_element()
	}

	fn window_title(&self, cx: &App) -> String {
		let Some(ix) = self.active else {
			return "snip-sync".into();
		};
		let views = self.tab_views(cx);
		match (&views.get(ix), views.get(ix).map(|v| &v.1.identity)) {
			(Some((label, _)), Some(Some(_))) => format!("{label} — snip-sync"),
			_ => "snip-sync".into(),
		}
	}
}

impl Render for TabsRoot {
	fn render(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) -> impl IntoElement {
		let title = self.window_title(cx);
		if title != self.title {
			window.set_window_title(&title);
			self.title = title;
		}
		let vp = window.viewport_size();
		let sf = window.scale_factor();
		let phys = (
			(f32::from(vp.width) * sf).round() as i32,
			(f32::from(vp.height) * sf).round() as i32,
		);
		if self.probes.is_some() && phys != self.last_viewport {
			self.last_viewport = phys;
			root_log!("[APP:VIEWPORT: {}x{}]", phys.0, phys.1);
		}
		if self.active.is_none() && window.focused(cx).is_none() {
			window.focus(&self.focus_handle);
		}
		let body: AnyElement = match self.active.and_then(|i| self.tabs.get(i))
		{
			Some(tab) => div()
				.flex()
				.flex_col()
				.flex_1()
				.min_h_0()
				.child(tab.model.clone())
				.into_any_element(),
			None => div()
				.id("ws-tabs-empty")
				.flex_1()
				.min_h_0()
				.bg(rgb(pal().frame_bg))
				.into_any_element(),
		};
		let no_tab = self.active.is_none();
		div()
			// Only the empty window holds the keyboard itself: a focusable
			// root would take it from a tab that is not drawn yet.
			.when(no_tab, |d| d.track_focus(&self.focus_handle))
			.key_context("WorkspaceTabs")
			.on_action(cx.listener(|this, _: &Quit, window, cx| {
				this.begin_quit(window, cx)
			}))
			.on_action(cx.listener(|this, _: &CloseWorkspace, window, cx| {
				if let Some(ix) = this.active {
					this.close_tab(ix, window, cx)
				}
			}))
			.on_action(cx.listener(|this, _: &OpenWorkspace, window, cx| {
				// Only reached with no tab: a tab opens its own dialog.
				this.new_tab(window, cx);
				if let Some(tab) = this.active.and_then(|i| this.tabs.get(i)) {
					tab.model.update(cx, |m, cx| m.open_folder_dialog(cx));
				}
			}))
			.on_action(cx.listener(|this, _: &PrevWorkspaceTab, window, cx| {
				this.step(-1, window, cx)
			}))
			.on_action(cx.listener(|this, _: &NextWorkspaceTab, window, cx| {
				this.step(1, window, cx)
			}))
			.on_action(
				cx.listener(|this, _: &WorkspaceTab1, w, cx| {
					this.jump(0, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab2, w, cx| {
					this.jump(1, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab3, w, cx| {
					this.jump(2, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab4, w, cx| {
					this.jump(3, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab5, w, cx| {
					this.jump(4, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab6, w, cx| {
					this.jump(5, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab7, w, cx| {
					this.jump(6, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab8, w, cx| {
					this.jump(7, w, cx)
				}),
			)
			.on_action(
				cx.listener(|this, _: &WorkspaceTab9, w, cx| {
					this.jump(8, w, cx)
				}),
			)
			.flex()
			.flex_col()
			.size_full()
			.bg(rgb(pal().frame_bg))
			.font_family(UI_FONT)
			.text_color(rgb(pal().text))
			.text_size(px(UI_TEXT))
			.child(self.render_tab_bar(cx))
			.child(body)
			// With a tab shown, its own frame end closes the frame.
			.when(no_tab, |d| d.children(probe_frame_end(&self.probes)))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn local(p: &str) -> Option<WsIdentity> {
		Some(WsIdentity::Local(PathBuf::from(p)))
	}

	fn remote(host: &str, p: &str) -> Option<WsIdentity> {
		Some(WsIdentity::Remote {
			host: host.into(),
			path: p.into(),
		})
	}

	#[test]
	fn labels_are_the_folder_name_and_host_name_for_remote() {
		let got = tab_labels(
			&[local("/w/alpha"), remote("box", "/srv/beta"), None],
			"New tab",
		);
		assert_eq!(got, ["alpha", "box ▸ beta", "New tab"]);
	}

	#[test]
	fn colliding_labels_get_parent_folders_until_they_differ() {
		let got = tab_labels(
			&[
				local("/a/x/app"),
				local("/b/x/app"),
				local("/c/web"),
				remote("box", "/srv/app"),
			],
			"",
		);
		assert_eq!(got, ["a/x/app", "b/x/app", "web", "box ▸ app"]);
	}

	#[test]
	fn the_same_remote_name_on_two_hosts_needs_no_parent() {
		let got = tab_labels(
			&[remote("one", "/srv/app"), remote("two", "/srv/app")],
			"",
		);
		assert_eq!(got, ["one ▸ app", "two ▸ app"]);
	}

	#[test]
	fn nested_folders_keep_their_own_names() {
		let got = tab_labels(&[local("/w/repo"), local("/w/repo/sub")], "");
		assert_eq!(got, ["repo", "sub"]);
	}

	#[test]
	fn background_lines_are_tagged_inside_the_brackets() {
		assert_eq!(
			tag_with("[APP:WORKSPACE: state=open generation=2]".into(), 3),
			"[APP:WORKSPACE: state=open generation=2 ws_tab=3]"
		);
		assert_eq!(
			tag_with("[APP:PASTE_APPLYING]".into(), 3),
			"[APP:PASTE_APPLYING: ws_tab=3]"
		);
		assert_eq!(tag_with("[READY:IDLE]".into(), 3), "[READY:IDLE]");
	}

	#[test]
	fn only_a_background_tab_tags_its_lines() {
		set_active_tab(Some(1));
		let shown = with_log_tab(Some(1), || tag_line("[APP:X: a=1]".into()));
		let hidden = with_log_tab(Some(2), || tag_line("[APP:X: a=1]".into()));
		let root = tag_line("[APP:X: a=1]".into());
		set_active_tab(None);
		assert_eq!(shown, "[APP:X: a=1]");
		assert_eq!(hidden, "[APP:X: a=1 ws_tab=2]");
		assert_eq!(root, "[APP:X: a=1]");
	}
}
