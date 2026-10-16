//! One owner for every layer drawn over the workbench (#226).
//!
//! A *floating layer* (workspace menu, Log dropdown, repo / ref selector,
//! context menu) is an anchored panel over the panes; at most one is open.
//! An *inline layer* (the reader's find bar, the Project pane's add-repo
//! field) is part of its pane and stays open while a floating one opens.
//!
//! The rules every layer shares live here and nowhere else: which layers
//! may be open together, what Escape does, where the keyboard goes when a
//! layer closes, what a press on the opening button does, what a tab
//! switch drops. A layer's state is private to [`OverlayStack`]; the only
//! ways in are `open_overlay`, `toggle_overlay`, `close_overlay` and
//! `drop_overlays`, so a path that "forgot" one of the rules cannot be
//! written.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
	canvas, div, prelude::*, App, Bounds, Context, Div, Entity, FocusHandle,
	Global, MouseDownEvent, Pixels, Stateful, Window, WindowId,
};

use crate::menu::{ContextMenu, MenuOrigin};
use crate::text_input::TextInput;
use crate::ui::LogMenu;
use crate::{Popover, WorkbenchModel};

/// Every layer there is. The windows of a layer's kind are drawn by the
/// code that owns the panel, but their *state* is only here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OverlayKind {
	// Floating.
	WorkspaceMenu,
	LogMenu(LogMenu),
	Selector(Popover),
	ContextMenu,
	// Inline.
	FindBar,
	AddRepoField,
}

impl OverlayKind {
	/// Every value, for the tests that must not miss a kind.
	#[cfg(test)]
	pub(crate) const ALL: [OverlayKind; 12] = [
		OverlayKind::WorkspaceMenu,
		OverlayKind::LogMenu(LogMenu::Repo),
		OverlayKind::LogMenu(LogMenu::Branch),
		OverlayKind::LogMenu(LogMenu::User),
		OverlayKind::LogMenu(LogMenu::Date),
		OverlayKind::LogMenu(LogMenu::More),
		OverlayKind::LogMenu(LogMenu::FilterMore),
		OverlayKind::Selector(Popover::Repo),
		OverlayKind::Selector(Popover::Ref),
		OverlayKind::ContextMenu,
		OverlayKind::FindBar,
		OverlayKind::AddRepoField,
	];

	/// Anchored over the panes, one at a time.
	pub(crate) const fn is_floating(self) -> bool {
		match self {
			OverlayKind::WorkspaceMenu
			| OverlayKind::LogMenu(_)
			| OverlayKind::Selector(_)
			| OverlayKind::ContextMenu => true,
			OverlayKind::FindBar | OverlayKind::AddRepoField => false,
		}
	}

	/// A tooltip would land on this layer's panel or on the controls under
	/// it, so none shows while it is open. An inline layer is part of its
	/// pane and takes none of that away.
	pub(crate) const fn suppresses_tooltips(self) -> bool {
		match self {
			OverlayKind::WorkspaceMenu
			| OverlayKind::LogMenu(_)
			| OverlayKind::Selector(_)
			| OverlayKind::ContextMenu => true,
			OverlayKind::FindBar | OverlayKind::AddRepoField => false,
		}
	}

	/// The debug selector the layer's panel is drawn with: a driver, or a
	/// test, finds out whether the panel is on screen from it.
	pub(crate) const fn panel_selector(self) -> &'static str {
		match self {
			OverlayKind::WorkspaceMenu => "workspace-menu",
			OverlayKind::LogMenu(_) => "log-menu",
			OverlayKind::Selector(_) => "selector-popover",
			OverlayKind::ContextMenu => "context-menu",
			OverlayKind::FindBar => "find-bar",
			OverlayKind::AddRepoField => "input-add-repo",
		}
	}
}

/// The control that opens a layer: a second press on it closes the layer
/// again, and the layer's own outside-press close leaves it alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TriggerId {
	Workspace,
	RepoSelector,
	RefSelector,
	LogMoreButton,
	LogChip(LogMenu),
	LogFilterMore,
	/// The branch widget of the status bar, which opens the ref selector.
	StatusBranch,
}

/// Where the keyboard goes when a layer closes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ReturnFocus {
	/// The handle that held it when the layer opened (or its pane).
	To(FocusHandle),
	/// The workbench root.
	Root,
}

/// Why a layer closes. Only the log lines tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseWhy {
	Escape,
	OutsidePress,
	Trigger,
	Picked,
	Replaced,
	Teardown,
}

/// What `drop_overlays` drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropScope {
	Floating,
	All,
	Kind(OverlayKind),
	/// Whichever Log dropdown is open.
	AnyLogMenu,
}

/// What a layer keeps besides being open.
pub(crate) enum LayerState {
	None,
	WorkspaceMenu { picker: bool },
	Selector { cursor: usize },
	ContextMenu(ContextMenu),
}

impl LayerState {
	/// A new layer's state, for the kinds that need no seed.
	fn initial(kind: OverlayKind) -> Self {
		match kind {
			OverlayKind::WorkspaceMenu => {
				LayerState::WorkspaceMenu { picker: false }
			}
			OverlayKind::Selector(_) => LayerState::Selector { cursor: 0 },
			OverlayKind::ContextMenu => {
				unreachable!("a context menu opens with its entries")
			}
			OverlayKind::LogMenu(_)
			| OverlayKind::FindBar
			| OverlayKind::AddRepoField => LayerState::None,
		}
	}

	/// The state a kind's layer must have (invariant O1).
	#[allow(dead_code)] // the checker of #230 reaches it through `eval`
	fn fits(&self, kind: OverlayKind) -> bool {
		matches!(
			(kind, self),
			(OverlayKind::WorkspaceMenu, LayerState::WorkspaceMenu { .. })
				| (OverlayKind::Selector(_), LayerState::Selector { .. })
				| (OverlayKind::ContextMenu, LayerState::ContextMenu(_))
				| (
					OverlayKind::LogMenu(_)
						| OverlayKind::FindBar
						| OverlayKind::AddRepoField,
					LayerState::None
				)
		)
	}
}

pub(crate) struct Layer {
	kind: OverlayKind,
	return_to: ReturnFocus,
	trigger: Option<TriggerId>,
	/// Every focus handle that belongs to the layer: the keyboard is the
	/// layer's to give back while it is on one of them.
	owned: Vec<FocusHandle>,
	state: LayerState,
}

/// Whether a closed layer's pending return of the keyboard still applies,
/// from where the keyboard is at the next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spot {
	/// Nothing holds it (the handle's element left the frame).
	None,
	/// On a handle the closed layer owned.
	Owned,
	/// On the workbench root.
	Root,
	/// Somewhere a press or a key put it since: not the layer's to take.
	Elsewhere,
}

impl Spot {
	pub(crate) const fn returns(self) -> bool {
		match self {
			Spot::None | Spot::Owned | Spot::Root => true,
			Spot::Elsewhere => false,
		}
	}
}

/// What a layer opened while another owned the keyboard returns to: the
/// older layer's own target, so that opening two layers in a row and
/// closing them lands where the first one was opened from. The flag says
/// a return queued for the older layer is dead.
pub(crate) fn inherited_return(
	older: &ReturnFocus,
	keyboard_was_in_older: bool,
	requested: ReturnFocus,
) -> (ReturnFocus, bool) {
	if keyboard_was_in_older {
		(older.clone(), true)
	} else {
		(requested, false)
	}
}

/// Whether pressing a trigger closes the open layer or opens a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Toggle {
	Close(OverlayKind),
	Open,
}

/// Where a trigger was last drawn.
type TriggerSlot = Rc<Cell<Option<Bounds<Pixels>>>>;

pub(crate) struct OverlayStack {
	floating: Option<Layer>,
	find_bar: Option<Layer>,
	add_repo: Option<Layer>,
	triggers: RefCell<HashMap<TriggerId, TriggerSlot>>,
	/// A layer closed and its keyboard target is waiting for the next
	/// frame: (target, the closed layer's handles).
	pending_return: Option<(ReturnFocus, Vec<FocusHandle>)>,
	/// What the last frame told the tooltip gate (its window and whether
	/// tooltips were off): invariant O6 compares it with the layers.
	published_gate: Cell<Option<(WindowId, bool)>>,
}

impl OverlayStack {
	pub(crate) fn new() -> Self {
		Self {
			floating: None,
			find_bar: None,
			add_repo: None,
			triggers: RefCell::new(HashMap::new()),
			pending_return: None,
			published_gate: Cell::new(None),
		}
	}

	fn slot(&self, kind: OverlayKind) -> &Option<Layer> {
		match kind {
			OverlayKind::FindBar => &self.find_bar,
			OverlayKind::AddRepoField => &self.add_repo,
			_ => &self.floating,
		}
	}

	fn slot_mut(&mut self, kind: OverlayKind) -> &mut Option<Layer> {
		match kind {
			OverlayKind::FindBar => &mut self.find_bar,
			OverlayKind::AddRepoField => &mut self.add_repo,
			_ => &mut self.floating,
		}
	}

	pub(crate) fn is_open(&self, kind: OverlayKind) -> bool {
		self.slot(kind).as_ref().is_some_and(|l| l.kind == kind)
	}

	/// The open floating layer, if any.
	pub(crate) fn floating(&self) -> Option<OverlayKind> {
		self.floating.as_ref().map(|l| l.kind)
	}

	/// The open Log dropdown, if one is.
	pub(crate) fn log_menu(&self) -> Option<LogMenu> {
		match self.floating.as_ref()?.kind {
			OverlayKind::LogMenu(m) => Some(m),
			_ => None,
		}
	}

	/// The open repo / ref selector, if one is.
	pub(crate) fn selector(&self) -> Option<Popover> {
		match self.floating.as_ref()?.kind {
			OverlayKind::Selector(p) => Some(p),
			_ => None,
		}
	}

	pub(crate) fn context_menu(&self) -> Option<&ContextMenu> {
		match &self.floating.as_ref()?.state {
			LayerState::ContextMenu(m) => Some(m),
			_ => None,
		}
	}

	/// The workspace menu is showing its typed-path field.
	pub(crate) fn workspace_picker(&self) -> bool {
		matches!(
			self.floating.as_ref().map(|l| &l.state),
			Some(LayerState::WorkspaceMenu { picker: true })
		)
	}

	/// The selector's highlighted row (0 while none is open).
	pub(crate) fn selector_cursor(&self) -> usize {
		match self.floating.as_ref().map(|l| &l.state) {
			Some(LayerState::Selector { cursor }) => *cursor,
			_ => 0,
		}
	}

	/// The selector's highlighted row, to move it. A layer's inner state is
	/// all this reaches: it cannot open or close anything.
	pub(crate) fn selector_cursor_mut(&mut self) -> Option<&mut usize> {
		match &mut self.floating.as_mut()?.state {
			LayerState::Selector { cursor } => Some(cursor),
			_ => None,
		}
	}

	/// The context menu's items and cursor, to move the cursor.
	pub(crate) fn context_menu_mut(&mut self) -> Option<&mut ContextMenu> {
		match &mut self.floating.as_mut()?.state {
			LayerState::ContextMenu(m) => Some(m),
			_ => None,
		}
	}

	/// Shows or hides the workspace menu's typed-path field, while it is
	/// open.
	pub(crate) fn set_workspace_picker(&mut self, on: bool) {
		if let Some(Layer {
			state: LayerState::WorkspaceMenu { picker },
			..
		}) = self.floating.as_mut()
		{
			*picker = on;
		}
	}

	/// Where a trigger was last drawn.
	pub(crate) fn trigger_slot(&self, id: TriggerId) -> TriggerSlot {
		self.triggers.borrow_mut().entry(id).or_default().clone()
	}

	/// True when a press at `at` is on the control that opened the open
	/// layer `kind`: that press is the trigger's to handle.
	fn on_trigger(&self, kind: OverlayKind, at: gpui::Point<Pixels>) -> bool {
		let Some(trigger) = self
			.slot(kind)
			.as_ref()
			.filter(|l| l.kind == kind)
			.and_then(|l| l.trigger)
		else {
			return false;
		};
		self.trigger_slot(trigger)
			.get()
			.is_some_and(|b| b.contains(&at))
	}

	/// A pending return of the keyboard exists (it is applied at the next
	/// frame).
	#[allow(dead_code)] // the checker of #230 reaches it through `eval`
	pub(crate) fn has_pending_return(&self) -> bool {
		self.pending_return.is_some()
	}

	/// Drops a pending return: the tab is going away from the window.
	pub(crate) fn clear_pending_return(&mut self) {
		self.pending_return = None;
	}

	/// Where the keyboard resumes when the tab it is in comes back, when
	/// it is on a floating layer: the pane that layer would give it to.
	pub(crate) fn return_target_for(
		&self,
		focused: &FocusHandle,
		window: &Window,
	) -> Option<FocusHandle> {
		let layer = self.floating.as_ref()?;
		if !layer.owned.iter().any(|o| o.contains(focused, window)) {
			return None;
		}
		match &layer.return_to {
			ReturnFocus::To(h) => Some(h.clone()),
			ReturnFocus::Root => None,
		}
	}

	/// Puts `layer` in its slot. Returns the layers it displaces: a
	/// floating layer displaces the open floating one; an inline layer
	/// displaces it too (it opens over nothing floating), and an inline
	/// layer that is open keeps its place.
	fn install(&mut self, layer: Layer) -> Vec<Layer> {
		// An inline layer is already open only when a caller asked twice;
		// the floating slot is the only one that displaces.
		let displaced: Vec<Layer> = self.floating.take().into_iter().collect();
		let kind = layer.kind;
		*self.slot_mut(kind) = Some(layer);
		displaced
	}

	fn take(&mut self, kind: OverlayKind) -> Option<Layer> {
		let slot = self.slot_mut(kind);
		if slot.as_ref().is_some_and(|l| l.kind == kind) {
			slot.take()
		} else {
			None
		}
	}

	/// What a press on `trigger` does now.
	pub(crate) fn toggle_for(&self, trigger: TriggerId) -> Toggle {
		match self.floating.as_ref() {
			Some(l) if l.trigger == Some(trigger) => Toggle::Close(l.kind),
			_ => Toggle::Open,
		}
	}

	/// Whether every open layer's state belongs to its kind (O1).
	#[allow(dead_code)] // the checker of #230 reaches it through `eval`
	fn states_fit(&self) -> Option<OverlayKind> {
		[&self.floating, &self.find_bar, &self.add_repo]
			.into_iter()
			.flatten()
			.find(|l| !l.state.fits(l.kind))
			.map(|l| l.kind)
	}
}

// ───────────────────────── invariants ─────────────────────────

/// The properties of the layers that hold at any time they are asked
/// (a test after `settle`, or the fuzz harness of #230).
// #230's invariant catalog calls these from production code at the end of a
// frame; until it lands only the tests do.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OverlayInvariant {
	/// Every layer's state belongs to its kind.
	O1,
	/// A host browse exists only under the open workspace menu.
	O2,
	/// A workspace tab that is not shown has no floating layer.
	O3,
	/// An open selector's cursor is on a candidate.
	O4,
	/// No return of the keyboard is left waiting after a frame.
	O5,
	/// The window's tooltip gate is closed exactly while a floating layer
	/// is open.
	O6,
}

#[allow(dead_code)]
impl OverlayInvariant {
	pub(crate) const ALL: &'static [OverlayInvariant] = &[
		OverlayInvariant::O1,
		OverlayInvariant::O2,
		OverlayInvariant::O3,
		OverlayInvariant::O4,
		OverlayInvariant::O5,
		OverlayInvariant::O6,
	];

	/// Holds only once nothing is loading: a ref selector's candidates are
	/// read asynchronously, so its cursor can lead them for a moment.
	pub(crate) const fn quiescent_only(self) -> bool {
		match self {
			OverlayInvariant::O4 => true,
			OverlayInvariant::O1
			| OverlayInvariant::O2
			| OverlayInvariant::O3
			| OverlayInvariant::O5
			| OverlayInvariant::O6 => false,
		}
	}

	/// One line for a failure message.
	pub(crate) const fn what(self) -> &'static str {
		match self {
			OverlayInvariant::O1 => "a layer's state belongs to its kind",
			OverlayInvariant::O2 => {
				"a host browse exists only under the open workspace menu"
			}
			OverlayInvariant::O3 => {
				"a workspace tab that is not shown has no floating layer"
			}
			OverlayInvariant::O4 => "the selector cursor is on a candidate",
			OverlayInvariant::O5 => {
				"no return of the keyboard is waiting after a frame"
			}
			OverlayInvariant::O6 => {
				"the tooltip gate is closed exactly while a floating layer is open"
			}
		}
	}
}

#[allow(dead_code)]
impl OverlayStack {
	/// One invariant; `None` means it holds. O5 is only meaningful after
	/// the tab has rendered.
	pub(crate) fn eval(
		id: OverlayInvariant,
		m: &WorkbenchModel,
		cx: &App,
	) -> Option<String> {
		let o = &m.overlays;
		match id {
			OverlayInvariant::O1 => o
				.states_fit()
				.map(|k| format!("{k:?} holds a state of another kind")),
			OverlayInvariant::O2 => (m.remote.browse.is_some()
				&& !o.is_open(OverlayKind::WorkspaceMenu))
			.then(|| "a host browse without the workspace menu".to_string()),
			OverlayInvariant::O3 => (!crate::tabs::is_shown(m.ws_tab)
				&& o.floating().is_some())
			.then(|| format!("{:?} is open in a background tab", o.floating())),
			OverlayInvariant::O4 => {
				o.selector()?;
				let q = m.selector_input.read(cx).text().to_string();
				let n = m.selector_candidates(&q).count();
				let cursor = o.selector_cursor();
				let ok = if n == 0 { cursor == 0 } else { cursor < n };
				(!ok).then(|| format!("cursor {cursor} over {n} candidates"))
			}
			OverlayInvariant::O5 => o.has_pending_return().then(|| {
				"a return of the keyboard is still waiting".to_string()
			}),
			OverlayInvariant::O6 => {
				// Nothing published yet: the tab has not drawn a frame.
				let (window, on) = o.published_gate.get()?;
				let want =
					o.floating().is_some_and(OverlayKind::suppresses_tooltips);
				let gate = cx
					.try_global::<TooltipGate>()
					.is_some_and(|g| g.0.contains(&window));
				(on != want || gate != on).then(|| {
					format!(
						"published {on}, gate {gate}, layers say {want} ({:?})",
						o.floating()
					)
				})
			}
		}
	}

	/// Every invariant that does not hold, with what is wrong.
	pub(crate) fn check_invariants(
		m: &WorkbenchModel,
		cx: &App,
	) -> Vec<(OverlayInvariant, String)> {
		OverlayInvariant::ALL
			.iter()
			.filter_map(|&id| Self::eval(id, m, cx).map(|d| (id, d)))
			.collect()
	}
}

// ───────────────────────── tooltip gate ─────────────────────────

/// The windows where no tooltip shows: a floating layer is open there. Read
/// by every tooltip as it is drawn, so a control cannot forget to ask.
#[derive(Default)]
pub(crate) struct TooltipGate(HashSet<WindowId>);

impl Global for TooltipGate {}

/// Tells the gate whether `window` has a floating layer open. Called by the
/// shown tab on every frame: a tooltip already on screen reads it again and
/// goes the frame after a layer opens.
pub(crate) fn publish_tooltip_gate(window: &Window, cx: &mut App, on: bool) {
	let id = window.window_handle().window_id();
	let gate = cx.default_global::<TooltipGate>();
	if on {
		gate.0.insert(id);
	} else {
		gate.0.remove(&id);
	}
}

/// No tooltip shows in `window` now.
pub(crate) fn tooltips_suppressed(window: &Window, cx: &App) -> bool {
	let id = window.window_handle().window_id();
	cx.try_global::<TooltipGate>()
		.is_some_and(|gate| gate.0.contains(&id))
}

impl WorkbenchModel {
	/// Publishes this frame's tooltip rule for the window.
	pub(crate) fn publish_gate(&self, window: &Window, cx: &mut App) {
		let on = self
			.overlays
			.floating()
			.is_some_and(OverlayKind::suppresses_tooltips);
		publish_tooltip_gate(window, cx, on);
		self.overlays
			.published_gate
			.set(Some((window.window_handle().window_id(), on)));
	}
}

// ───────────────────────── focus targets ─────────────────────────

/// The text fields of the workbench: the only list of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FieldId {
	Find,
	Goto,
	LogSearch,
	Selector,
	AddRepo,
	WorkspacePath,
	RemotePath,
	LogPath,
	LogBranchMenu,
	LogSince,
	LogUntil,
	BranchFilter,
}

/// What holds the keyboard, for the one Escape rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusTarget {
	Root,
	ToolList,
	GitLog,
	Reader,
	PastePanel,
	Menu,
	Field(FieldId),
	Other,
}

impl FocusTarget {
	/// One of each, for the keymap test.
	#[cfg(test)]
	pub(crate) const KINDS: [FocusTarget; 8] = [
		FocusTarget::Root,
		FocusTarget::ToolList,
		FocusTarget::GitLog,
		FocusTarget::Reader,
		FocusTarget::PastePanel,
		FocusTarget::Menu,
		FocusTarget::Field(FieldId::Find),
		FocusTarget::Other,
	];

	/// The key context its element is drawn with.
	#[cfg(test)]
	pub(crate) const fn context(self) -> Option<&'static str> {
		match self {
			FocusTarget::Root | FocusTarget::Other => None,
			FocusTarget::ToolList => Some("ToolList"),
			FocusTarget::GitLog => Some("GitLog"),
			FocusTarget::Reader => Some("Reader"),
			FocusTarget::PastePanel => Some("PastePanel"),
			FocusTarget::Menu => Some("ContextMenu"),
			FocusTarget::Field(_) => Some("TextInput"),
		}
	}
}

// ───────────────────────── the model's side ─────────────────────────

impl WorkbenchModel {
	/// Every text field, by id.
	pub(crate) fn text_inputs(&self) -> [(FieldId, &Entity<TextInput>); 12] {
		[
			(FieldId::Find, &self.find_input),
			(FieldId::Goto, &self.goto_input),
			(FieldId::LogSearch, &self.log_search_input),
			(FieldId::Selector, &self.selector_input),
			(FieldId::AddRepo, &self.add_repo_input),
			(FieldId::WorkspacePath, &self.workspace_path_input),
			(FieldId::RemotePath, &self.remote_path_input),
			(FieldId::LogPath, &self.log_path_input),
			(FieldId::LogBranchMenu, &self.log_branch_menu_input),
			(FieldId::LogSince, &self.log_since_input),
			(FieldId::LogUntil, &self.log_until_input),
			(FieldId::BranchFilter, &self.branch_filter_input),
		]
	}

	/// What holds the keyboard now.
	pub(crate) fn focus_target(
		&self,
		window: &Window,
		cx: &App,
	) -> FocusTarget {
		let Some(f) = window.focused(cx) else {
			return FocusTarget::Root;
		};
		for (id, input) in self.text_inputs() {
			if input.read(cx).handle() == f {
				return FocusTarget::Field(id);
			}
		}
		let panes = [
			(&self.chrome.menu_focus, FocusTarget::Menu),
			(&self.paste_focus, FocusTarget::PastePanel),
			(&self.reader_focus, FocusTarget::Reader),
			(&self.tree_focus, FocusTarget::ToolList),
			(&self.log_focus, FocusTarget::GitLog),
		];
		for (pane, target) in panes {
			if pane.contains(&f, window) {
				return target;
			}
		}
		if f == self.focus_handle {
			FocusTarget::Root
		} else {
			FocusTarget::Other
		}
	}

	/// Where the keyboard is now, as a layer would give it back.
	pub(crate) fn return_here(&self, window: &Window, cx: &App) -> ReturnFocus {
		match window.focused(cx) {
			Some(f) if f != self.focus_handle => ReturnFocus::To(f),
			_ => ReturnFocus::Root,
		}
	}

	/// Every handle that belongs to a layer.
	pub(crate) fn owned_handles(
		&self,
		kind: OverlayKind,
		cx: &App,
	) -> Vec<FocusHandle> {
		let h = |i: &Entity<TextInput>| i.read(cx).handle();
		match kind {
			OverlayKind::WorkspaceMenu => {
				vec![h(&self.workspace_path_input), h(&self.remote_path_input)]
			}
			OverlayKind::LogMenu(LogMenu::Repo) => {
				vec![h(&self.log_path_input)]
			}
			OverlayKind::LogMenu(LogMenu::Branch) => {
				vec![h(&self.log_branch_menu_input)]
			}
			OverlayKind::LogMenu(LogMenu::Date) => {
				vec![h(&self.log_since_input), h(&self.log_until_input)]
			}
			OverlayKind::LogMenu(
				LogMenu::User | LogMenu::More | LogMenu::FilterMore,
			) => Vec::new(),
			OverlayKind::Selector(_) => vec![h(&self.selector_input)],
			OverlayKind::ContextMenu => vec![self.chrome.menu_focus.clone()],
			OverlayKind::FindBar => {
				vec![h(&self.find_input), h(&self.goto_input)]
			}
			OverlayKind::AddRepoField => vec![h(&self.add_repo_input)],
		}
	}

	/// The keyboard to `h`: now when there is a window, else at the next
	/// frame.
	pub(crate) fn focus_into(
		&mut self,
		h: FocusHandle,
		window: Option<&mut Window>,
	) {
		match window {
			Some(w) => w.focus(&h),
			None => self.pending_focus = Some(h),
		}
	}

	/// Opens a layer that needs no seed state (not a context menu).
	pub(crate) fn open_overlay(
		&mut self,
		kind: OverlayKind,
		ret: ReturnFocus,
		trigger: Option<TriggerId>,
		window: Option<&mut Window>,
		cx: &mut Context<Self>,
	) {
		self.open_layer(
			kind,
			ret,
			trigger,
			LayerState::initial(kind),
			window,
			cx,
		)
	}

	/// Opens a layer; the one door in. A floating layer replaces the open
	/// one, and an inline layer closes it: the keyboard is a floating
	/// layer's to give back, so the newer layer inherits where the older
	/// one would have sent it.
	pub(crate) fn open_layer(
		&mut self,
		kind: OverlayKind,
		ret: ReturnFocus,
		trigger: Option<TriggerId>,
		state: LayerState,
		window: Option<&mut Window>,
		cx: &mut Context<Self>,
	) {
		if !kind.is_floating() && self.overlays.is_open(kind) {
			// Already open: whoever asked takes the keyboard into it.
			return;
		}
		if kind.is_floating() && !crate::tabs::is_shown(self.ws_tab) {
			// A floating layer needs the pointer and the frame: a callback
			// that lands in a background tab opens nothing.
			return;
		}
		let mut ret = ret;
		if let Some(w) = window.as_deref() {
			if let Some(f) = w.focused(cx) {
				let in_layer = |owned: &[FocusHandle]| {
					owned.iter().any(|o| o.contains(&f, w))
				};
				let older = self
					.overlays
					.floating
					.as_ref()
					.filter(|l| in_layer(&l.owned))
					.map(|l| l.return_to.clone())
					.or_else(|| {
						self.overlays
							.pending_return
							.as_ref()
							.filter(|(_, owned)| in_layer(owned))
							.map(|(r, _)| r.clone())
					});
				if let Some(older) = older {
					ret = inherited_return(&older, true, ret).0;
				}
			}
		}
		// The keyboard is the new layer's from here: a return queued for a
		// layer closed in the same press must not take it back.
		self.overlays.pending_return = None;
		let owned = self.owned_handles(kind, cx);
		let displaced = self.overlays.install(Layer {
			kind,
			return_to: ret,
			trigger,
			owned,
			state,
		});
		for layer in displaced {
			self.teardown(layer, CloseWhy::Replaced);
		}
		self.on_open(kind, window, cx);
		cx.notify();
	}

	/// What a layer does as it opens.
	fn on_open(
		&mut self,
		kind: OverlayKind,
		window: Option<&mut Window>,
		cx: &mut Context<Self>,
	) {
		match kind {
			OverlayKind::Selector(popover) => {
				self.selector_input.update(cx, |i, cx| i.set_text("", cx));
				let h = self.selector_input.read(cx).handle();
				self.focus_into(h, window);
				let n = self.selector_candidates("").count();
				app_log!("[APP:SELECTOR_OPEN: {:?} items={}]", popover, n);
			}
			OverlayKind::LogMenu(menu) => {
				match menu {
					LogMenu::Repo => {
						let h = self.log_path_input.read(cx).handle();
						self.focus_into(h, window);
						self.load_picker_dir("", cx);
					}
					// IntelliJ's Branch popup opens with an empty field,
					// sections shut.
					LogMenu::Branch => {
						self.log_branch_menu_input
							.update(cx, |i, _| i.clear_retained());
						self.log_branch_menu_open.clear();
						let h = self.log_branch_menu_input.read(cx).handle();
						self.focus_into(h, window);
					}
					LogMenu::User
					| LogMenu::Date
					| LogMenu::More
					| LogMenu::FilterMore => {}
				}
				app_log!("[APP:LOG_MENU: Some({:?})]", menu);
			}
			OverlayKind::WorkspaceMenu => {
				if crate::recent::config_dir().is_some() {
					// Another workspace tab may have opened something since.
					self.recent_workspaces = crate::recent::load();
					self.remote.recent = crate::remote::load_recent();
				}
			}
			OverlayKind::ContextMenu => {
				if let Some(w) = window {
					w.focus(&self.chrome.menu_focus);
					// Opened from a mouse press: keep the list below from
					// taking focus.
					w.prevent_default();
				}
			}
			OverlayKind::FindBar => {
				let q = self.find_input.read(cx).text().to_string();
				self.run_find(&q, cx);
				app_log!("[APP:FIND_BAR: open]");
			}
			OverlayKind::AddRepoField => {}
		}
	}

	/// A press on a layer's trigger: closes what that trigger opened, else
	/// opens. An inline layer toggles by itself.
	pub(crate) fn toggle_overlay(
		&mut self,
		kind: OverlayKind,
		trigger: Option<TriggerId>,
		ret: ReturnFocus,
		window: Option<&mut Window>,
		cx: &mut Context<Self>,
	) {
		let close = match trigger {
			Some(t) if kind.is_floating() => {
				match self.overlays.toggle_for(t) {
					Toggle::Close(k) => Some(k),
					Toggle::Open => None,
				}
			}
			_ => self.overlays.is_open(kind).then_some(kind),
		};
		match close {
			Some(k) => self.close_overlay(k, CloseWhy::Trigger, cx),
			None => self.open_overlay(kind, ret, trigger, window, cx),
		}
	}

	/// Closes the open Log dropdown, if one is.
	pub(crate) fn close_log_menus(
		&mut self,
		why: CloseWhy,
		cx: &mut Context<Self>,
	) {
		if let Some(menu) = self.overlays.log_menu() {
			self.close_overlay(OverlayKind::LogMenu(menu), why, cx);
		}
	}

	/// Closes a layer and queues the keyboard's way back.
	pub(crate) fn close_overlay(
		&mut self,
		kind: OverlayKind,
		why: CloseWhy,
		cx: &mut Context<Self>,
	) {
		let Some(layer) = self.overlays.take(kind) else {
			return;
		};
		let queued = (layer.return_to.clone(), layer.owned.clone());
		self.teardown(layer, why);
		self.overlays.pending_return = Some(queued);
		cx.notify();
	}

	/// Drops layers without a return of the keyboard: a release, a tab
	/// switch or a callback that changed what the layer was about.
	pub(crate) fn drop_overlays(&mut self, scope: DropScope) {
		let kinds: Vec<OverlayKind> = [
			self.overlays.floating.as_ref(),
			self.overlays.find_bar.as_ref(),
			self.overlays.add_repo.as_ref(),
		]
		.into_iter()
		.flatten()
		.map(|l| l.kind)
		.filter(|k| match scope {
			DropScope::Floating => k.is_floating(),
			DropScope::All => true,
			DropScope::Kind(want) => *k == want,
			DropScope::AnyLogMenu => matches!(k, OverlayKind::LogMenu(_)),
		})
		.collect();
		for kind in kinds {
			if let Some(layer) = self.overlays.take(kind) {
				self.teardown(layer, CloseWhy::Teardown);
			}
		}
	}

	/// What a layer leaves behind as it goes: the one place a layer's
	/// sub-state is released.
	fn teardown(&mut self, layer: Layer, why: CloseWhy) {
		match layer.kind {
			// The menu's host browse, and the ssh it holds, end with it
			// (#187).
			OverlayKind::WorkspaceMenu => self.remote.end_browse(),
			OverlayKind::Selector(_) => app_log!("[APP:SELECTOR_CLOSED]"),
			OverlayKind::ContextMenu => app_log!("[APP:MENU_CLOSED]"),
			// A press on the chip prints the close; any other way out does
			// not (docs/real-ui-operator-protocol.md).
			OverlayKind::LogMenu(_) => {
				if why == CloseWhy::Trigger {
					app_log!("[APP:LOG_MENU: None]");
				}
			}
			OverlayKind::FindBar => {
				self.reader.find_query.clear();
				self.reader.matches.clear();
				self.reader.current = None;
				self.reader.find_invalid = false;
				app_log!("[APP:FIND_BAR: closed]");
			}
			OverlayKind::AddRepoField => {}
		}
	}

	/// The keyboard goes back to a closed layer's target when it is still
	/// where the layer left it. Before the frame's own `pending_focus`.
	pub(crate) fn apply_pending_return(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) {
		let Some((ret, owned)) = self.overlays.pending_return.take() else {
			return;
		};
		let spot = match window.focused(cx) {
			None => Spot::None,
			Some(f) if owned.iter().any(|o| o.contains(&f, window)) => {
				Spot::Owned
			}
			Some(f) if f == self.focus_handle => Spot::Root,
			Some(_) => Spot::Elsewhere,
		};
		if !spot.returns() {
			return;
		}
		match ret {
			ReturnFocus::To(h) => window.focus(&h),
			ReturnFocus::Root => window.focus(&self.focus_handle),
		}
	}

	/// The one Escape rule. True when the key was taken.
	///
	/// 1. An IME composition owns it.
	/// 2. An open floating layer closes.
	/// 3. Otherwise what holds the keyboard decides.
	pub(crate) fn on_escape(
		&mut self,
		window: &mut Window,
		cx: &mut Context<Self>,
	) -> bool {
		let composing = self.text_inputs().into_iter().any(|(_, i)| {
			let i = i.read(cx);
			i.marked_range().is_some() && i.handle().is_focused(window)
		});
		if composing {
			return false;
		}
		if let Some(kind) = self.overlays.floating() {
			self.close_overlay(kind, CloseWhy::Escape, cx);
			return true;
		}
		match self.focus_target(window, cx) {
			FocusTarget::Field(FieldId::Find | FieldId::Goto) => {
				self.close_overlay(OverlayKind::FindBar, CloseWhy::Escape, cx);
				true
			}
			FocusTarget::Field(FieldId::AddRepo) => {
				self.close_overlay(
					OverlayKind::AddRepoField,
					CloseWhy::Escape,
					cx,
				);
				true
			}
			FocusTarget::Field(FieldId::LogSearch) => {
				self.log_search_input.update(cx, |i, cx| i.set_text("", cx));
				self.start_log_search(String::new(), cx);
				true
			}
			FocusTarget::Field(_) => false,
			// Escape clears the selection, then closes the find bar.
			FocusTarget::Reader => {
				if self.reader.selection().is_some() {
					self.reader.anchor = None;
					self.reader.head = None;
					cx.notify();
					true
				} else if self.overlays.is_open(OverlayKind::FindBar) {
					self.close_overlay(
						OverlayKind::FindBar,
						CloseWhy::Escape,
						cx,
					);
					true
				} else {
					false
				}
			}
			// First ends speed search, then goes to the editor.
			FocusTarget::ToolList | FocusTarget::GitLog => {
				if !self.chrome.speed.is_empty() {
					self.chrome.speed.clear();
					app_log!("[APP:SPEED_SEARCH: off]");
				} else {
					window.focus(&self.reader_focus);
					app_log!("[APP:FOCUS: editor]");
				}
				cx.notify();
				true
			}
			FocusTarget::PastePanel => {
				self.cancel_paste_preview(cx);
				true
			}
			// An open menu is a floating layer: step 2 took it.
			FocusTarget::Menu | FocusTarget::Root | FocusTarget::Other => false,
		}
	}

	/// A canvas that records where a trigger is drawn: a press there is the
	/// trigger's, not an outside press of the layer it opened.
	pub(crate) fn overlay_trigger_slot(&self, id: TriggerId) -> Div {
		let slot = self.overlays.trigger_slot(id);
		div().absolute().top_0().left_0().size_full().child(
			canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {})
				.absolute()
				.top_0()
				.left_0()
				.size_full(),
		)
	}

	/// The head of every floating layer's panel: its id and debug selector,
	/// that it blocks the panes under it, and the outside-press close that
	/// leaves the layer's own trigger alone.
	pub(crate) fn overlay_panel(
		&self,
		kind: OverlayKind,
		cx: &mut Context<Self>,
	) -> Stateful<Div> {
		let selector = kind.panel_selector();
		#[allow(clippy::disallowed_methods)] // the one panel builder
		let panel = div()
			.id(selector)
			.debug_selector(move || selector.into())
			.occlude()
			.on_mouse_down_out(cx.listener(
				move |this, ev: &MouseDownEvent, _, cx| {
					if !this.overlays.on_trigger(kind, ev.position) {
						this.close_overlay(kind, CloseWhy::OutsidePress, cx);
					}
				},
			));
		panel
	}

	/// The open repo selector's cursor follows its repo when the list
	/// changes under it: it is an index into the filtered list, so a repo
	/// found ahead of it would move it onto another one, or is clamped when
	/// that repo is gone. `before` is the list as the cursor last saw it.
	pub(crate) fn rebind_selector_cursor(
		&mut self,
		before: &[(std::path::PathBuf, String)],
		cx: &mut Context<Self>,
	) {
		if self.overlays.floating()
			!= Some(OverlayKind::Selector(Popover::Repo))
		{
			return;
		}
		let q = self.selector_input.read(cx).text().to_string();
		let query = q.to_lowercase();
		let under_cursor = before
			.iter()
			.filter(|(_, name)| {
				query.is_empty() || name.to_lowercase().contains(&query)
			})
			.nth(self.overlays.selector_cursor())
			.map(|(root, _)| root);
		let moved = under_cursor.and_then(|root| {
			self.selector_candidates(&q)
				.position(|row| row.repo_root() == Some(root.as_path()))
		});
		let last = self.selector_candidates(&q).count().saturating_sub(1);
		let now = self.overlays.selector_cursor();
		if let Some(cursor) = self.overlays.selector_cursor_mut() {
			*cursor = moved.unwrap_or(now.min(last));
		}
	}
}

/// Where a context menu of `origin` gives the keyboard back.
pub(crate) fn origin_pane(
	m: &WorkbenchModel,
	origin: MenuOrigin,
) -> FocusHandle {
	match origin {
		MenuOrigin::Left => m.tree_focus.clone(),
		MenuOrigin::Log => m.log_focus.clone(),
		MenuOrigin::Editor => m.reader_focus.clone(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn layer(kind: OverlayKind, trigger: Option<TriggerId>) -> Layer {
		Layer {
			kind,
			return_to: ReturnFocus::Root,
			trigger,
			owned: Vec::new(),
			state: if kind == OverlayKind::ContextMenu {
				LayerState::ContextMenu(ContextMenu {
					origin: MenuOrigin::Log,
					pos: gpui::Point::default(),
					entries: Vec::new(),
					cursor: None,
				})
			} else {
				LayerState::initial(kind)
			},
		}
	}

	/// At most one floating layer: a second one displaces the first.
	#[test]
	fn at_most_one_floating_layer() {
		let mut s = OverlayStack::new();
		let sel = OverlayKind::Selector(Popover::Repo);
		let log = OverlayKind::LogMenu(LogMenu::User);
		assert!(s.install(layer(sel, None)).is_empty());
		let displaced = s.install(layer(log, None));
		assert_eq!(s.floating(), Some(log));
		assert_eq!(displaced.iter().map(|l| l.kind).collect::<Vec<_>>(), [sel]);
		assert!(!s.is_open(sel));
	}

	/// An inline layer closes the floating one; a floating one leaves the
	/// inline layers alone.
	#[test]
	fn inline_opens_close_floating_and_survive_floating_opens() {
		let mut s = OverlayStack::new();
		let menu = OverlayKind::WorkspaceMenu;
		s.install(layer(menu, None));
		let displaced = s.install(layer(OverlayKind::FindBar, None));
		assert_eq!(
			displaced.iter().map(|l| l.kind).collect::<Vec<_>>(),
			[menu]
		);
		assert!(s.floating().is_none() && s.is_open(OverlayKind::FindBar));
		s.install(layer(OverlayKind::ContextMenu, None));
		assert!(s.is_open(OverlayKind::FindBar), "the find bar stays");
		assert_eq!(s.floating(), Some(OverlayKind::ContextMenu));
		s.install(layer(OverlayKind::AddRepoField, None));
		assert!(s.is_open(OverlayKind::FindBar));
		assert!(s.is_open(OverlayKind::AddRepoField));
		assert!(s.floating().is_none(), "an inline open drops the menu");
	}

	/// The control that opened a layer closes what it opened last, even
	/// when the layer was swapped for another under the same trigger.
	#[test]
	fn trigger_toggle_closes_what_it_opened() {
		let mut s = OverlayStack::new();
		let t = TriggerId::LogFilterMore;
		assert_eq!(s.toggle_for(t), Toggle::Open);
		s.install(layer(OverlayKind::LogMenu(LogMenu::FilterMore), Some(t)));
		assert_eq!(
			s.toggle_for(t),
			Toggle::Close(OverlayKind::LogMenu(LogMenu::FilterMore))
		);
		// The overflow menu hands over to a sub-menu under the same trigger.
		s.install(layer(OverlayKind::LogMenu(LogMenu::Branch), Some(t)));
		assert_eq!(
			s.toggle_for(t),
			Toggle::Close(OverlayKind::LogMenu(LogMenu::Branch))
		);
		assert_eq!(
			s.toggle_for(TriggerId::LogChip(LogMenu::Branch)),
			Toggle::Open,
			"another trigger opens its own layer"
		);
	}

	/// The keyboard is given back only from inside the layer, the root or
	/// nowhere.
	#[test]
	fn focus_returns_only_from_inside() {
		assert!(Spot::None.returns());
		assert!(Spot::Owned.returns());
		assert!(Spot::Root.returns());
		assert!(!Spot::Elsewhere.returns());
	}

	/// Every kind has a place: a floating layer's tooltip rule and panel
	/// selector are decided for each, and the selectors tell the panels
	/// apart.
	#[test]
	fn every_kind_has_policy() {
		assert_eq!(OverlayKind::ALL.len(), 12);
		let mut seen = std::collections::HashSet::new();
		for k in OverlayKind::ALL {
			assert!(seen.insert(k), "{k:?} listed once");
			assert!(!k.panel_selector().is_empty());
		}
		let selectors: std::collections::HashSet<_> = OverlayKind::ALL
			.iter()
			.map(|k| k.panel_selector())
			.collect();
		assert_eq!(selectors.len(), 6, "one selector per kind of panel");
		assert_eq!(
			OverlayKind::ALL.iter().filter(|k| k.is_floating()).count(),
			10
		);
	}

	/// A layer opened while the keyboard is in an older one gives it back
	/// where the older one would have.
	#[test]
	fn replacing_a_layer_inherits_its_return() {
		let older = ReturnFocus::Root;
		let (got, clear) = inherited_return(&older, true, ReturnFocus::Root);
		assert_eq!(got, older);
		assert!(clear, "the return queued for the older layer is dead");
		let (got, clear) = inherited_return(&older, false, ReturnFocus::Root);
		assert_eq!(got, ReturnFocus::Root);
		assert!(!clear);
	}

	/// Every kind's state belongs to it (O1's predicate).
	#[test]
	fn a_state_fits_only_its_own_kind() {
		let mut s = OverlayStack::new();
		s.install(layer(OverlayKind::WorkspaceMenu, None));
		assert_eq!(s.states_fit(), None);
		s.floating.as_mut().unwrap().state = LayerState::None;
		assert_eq!(s.states_fit(), Some(OverlayKind::WorkspaceMenu));
	}
}
