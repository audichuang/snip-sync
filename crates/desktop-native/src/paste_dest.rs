//! Choosing the folder a paste lands in (#238, spec 5.1).
//!
//! The default is the one the paste always had (`--restore-dir`, else the
//! pinned repo, else the workspace folder), worked out when the preview
//! opens. From then on the user owns it: the breadcrumb, the chips, the
//! typed field, the folder dialog and the Project menu all end in
//! [`WorkbenchModel::retarget_paste`], which plans the previewed payload
//! again, never re-reading the clipboard.

use std::path::{Path, PathBuf};

use gpui::{Context, PathPromptOptions};

use crate::i18n::{self, Msg};
use crate::overlay::{CloseWhy, DropScope, OverlayKind, ReturnFocus};
use crate::paste::dest::{DestEdit, DestFolder, WsRel};
use crate::WorkbenchModel;

/// A refusal in the one wording the engine and the worker use for a path
/// outside the workspace.
fn not_under_workspace(shown: &str) -> Msg {
	Msg::with_key_arg(
		"paste_err_destination",
		[shown.to_string(), "err_not_under_workspace".to_string()],
		1,
	)
}

impl WorkbenchModel {
	fn remote_paste(&self) -> bool {
		self.remote.session.is_some()
	}

	/// The typed destination field, while it is open: an inline layer of
	/// the overlay stack (#226).
	pub(crate) fn paste_dest_edit(&self) -> Option<&DestEdit> {
		self.overlays.paste_dest_edit()
	}

	/// Closes the typed field without a return of the keyboard: the
	/// preview it belongs to is closing or starting over.
	pub(crate) fn drop_paste_dest_edit(&mut self) {
		self.drop_overlays(DropScope::Kind(OverlayKind::PasteDestField));
	}

	/// Closes the typed field once its folder is used; the keyboard goes
	/// back to the panel.
	fn close_paste_dest_edit(&mut self, cx: &mut Context<Self>) {
		self.close_overlay(OverlayKind::PasteDestField, CloseWhy::Picked, cx);
	}

	/// `path` (a tree path) as a destination folder of this workspace.
	pub(crate) fn dest_folder(&self, path: PathBuf) -> DestFolder {
		DestFolder::new(&self.workspace_root, path, self.remote_paste())
	}

	/// The folder a person typed: the text is read as a path inside the
	/// workspace ([`WsRel::parse_typed`] trims it), then judged like any
	/// other. Only the typed field goes through here.
	pub(crate) fn resolve_typed_folder(
		&self,
		typed: &str,
	) -> Result<DestFolder, Msg> {
		match WsRel::parse_typed(typed) {
			Ok(rel) => self.resolve_paste_folder(&rel),
			Err(shown) => Err(not_under_workspace(&shown)),
		}
	}

	/// The folder `rel` names, or why it cannot be one. Every gesture of the
	/// destination bar goes through here, with a place it read off the file
	/// system: a name is data and is never trimmed or rewritten.
	///
	/// A local folder is checked now: it must exist, be a folder, and stay
	/// inside the workspace with its links followed, so a refused name
	/// leaves the open plan as it was. A remote folder can only be judged
	/// by its worker: it is planned there, and its refusal lands with the
	/// plan.
	// #241 (PR-4) replaces this with `PlacedRoot::resolve(bound)`; until
	// then keep the rules equal to `pasteserve::destination`.
	pub(crate) fn resolve_paste_folder(
		&self,
		rel: &WsRel,
	) -> Result<DestFolder, Msg> {
		let typed = rel.as_str();
		let path = rel.to_path(&self.workspace_root);
		// The engine cannot write into it (see `WsRel::ends_with_space`):
		// said now, not as a failed Apply after a plan that looked ready.
		if rel.ends_with_space() {
			return Err(Msg::with_key_arg(
				"paste_err_destination",
				[
					typed.to_string(),
					"err_dest_name_ends_with_space".to_string(),
				],
				1,
			));
		}
		if !self.remote_paste() {
			let root = &self.workspace_root;
			let inside = |p: &Path| {
				dunce::canonicalize(p)
					.map_err(|e| crate::paste::destination_error(p, &e))
			};
			let real_root = inside(root)?;
			let real = inside(&path)?;
			if !real.starts_with(&real_root) {
				return Err(not_under_workspace(typed));
			}
			if !real.is_dir() {
				return Err(Msg::new(
					"paste_err_destination_not_dir",
					[path.display().to_string()],
				));
			}
		}
		Ok(DestFolder {
			path,
			rel: Some(rel.clone()),
		})
	}

	/// The folder at `rel`, as a gesture of the bar names it; a refusal goes
	/// to the status line.
	pub(crate) fn paste_into_rel(
		&mut self,
		rel: &WsRel,
		via: &'static str,
		cx: &mut Context<Self>,
	) {
		match self.resolve_paste_folder(rel) {
			Ok(folder) => self.retarget_paste(folder, via, cx),
			Err(err) => self.refuse_paste_folder(err, via, cx),
		}
	}

	fn refuse_paste_folder(
		&mut self,
		err: Msg,
		via: &'static str,
		cx: &mut Context<Self>,
	) {
		app_log!("[APP:PASTE_DEST_REFUSED: via={via} reason={}]", err.key);
		self.set_status_msg(err.clone());
		if self.paste.plan().is_some() {
			// The bar says it too, beside the plan that stays as it was.
			self.paste.set_dest_refusal(Some(err));
			self.focus_paste_panel(cx);
		}
		cx.notify();
	}

	/// Hands the focus back to the panel, or to the typed field when it is
	/// open: Enter must not reach the panel while a path is being typed.
	fn focus_paste_panel(&mut self, cx: &mut Context<Self>) {
		self.pending_focus = Some(if self.paste_dest_edit().is_some() {
			self.paste_dest_input.read(cx).handle()
		} else {
			self.paste_focus.clone()
		});
	}

	/// The destination as the log and the bar name it.
	fn shown_folder(&self, folder: &DestFolder) -> String {
		match self.paste.plan() {
			Some(plan) => plan.shown(&folder.path),
			None => crate::remote::display_path(
				self.remote.session.as_ref(),
				&folder.path,
			),
		}
	}

	/// Plans the open preview's payload again into `folder`. The previewed
	/// text is what is pasted: nothing is read from the clipboard.
	pub(crate) fn retarget_paste(
		&mut self,
		folder: DestFolder,
		via: &'static str,
		cx: &mut Context<Self>,
	) {
		// No plan, no panel: a result that arrives after the preview closed
		// (the folder dialog) changes nothing.
		if self.paste.plan().is_none() {
			return;
		}
		if self.refuse_while_applying("destination", cx) {
			return;
		}
		if !self.accepting_work() {
			return;
		}
		// The folder already shown, and a plan that stands as it is built:
		// nothing to plan. A plan the engine refused is tried again.
		let settled = self.paste.plan().is_some_and(|plan| {
			!self.paste.is_loading()
				&& (plan.executable() || !plan.mapping_ready())
		});
		if settled && self.paste.folder().is_some_and(|f| f.path == folder.path)
		{
			app_log!("[APP:PASTE_DEST_SAME: via={via}]");
			self.close_paste_dest_edit(cx);
			self.pending_focus = Some(self.paste_focus.clone());
			cx.notify();
			return;
		}
		self.user_action();
		let shown = self.shown_folder(&folder);
		self.paste.set_folder(folder.clone());
		self.close_paste_dest_edit(cx);
		let roots = self.repos.roots();
		match self.paste.retarget(
			folder.path,
			roots,
			self.generation,
			self.preview.as_ref(),
		) {
			crate::paste::preview::Remapped::NoPlan => {}
			crate::paste::preview::Remapped::Invalid(err) => {
				self.set_status_msg(err);
			}
			crate::paste::preview::Remapped::Dropped(err) => {
				self.set_status_msg(err);
				self.restore_log_after_paste();
			}
			crate::paste::preview::Remapped::Ready(request) => {
				self.queue_paste(request, cx)
			}
		}
		app_log!("[APP:PASTE_DEST: via={via} dest={shown}]");
		self.pending_focus = Some(self.paste_focus.clone());
		cx.notify();
	}

	/// "Paste into this folder" of the Project tree: moves an open preview,
	/// or opens one from the clipboard there.
	pub(crate) fn paste_into_folder(
		&mut self,
		path: PathBuf,
		cx: &mut Context<Self>,
	) {
		let rel =
			WsRel::of_folder(&self.workspace_root, &path, self.remote_paste());
		let Some(rel) = rel else {
			let shown = path.display().to_string();
			self.refuse_paste_folder(not_under_workspace(&shown), "menu", cx);
			return;
		};
		let folder = match self.resolve_paste_folder(&rel) {
			Ok(folder) => folder,
			Err(err) => {
				self.refuse_paste_folder(err, "menu", cx);
				return;
			}
		};
		if self.paste.plan().is_some() && !self.paste.is_applying() {
			self.retarget_paste(folder, "menu", cx);
		} else {
			self.trigger_paste_preview_into(Some(folder), cx);
		}
	}

	/// The folder the OS dialog returned. Outside the workspace it is
	/// refused: the destination bar only names folders inside it.
	pub fn paste_dest_picked(&mut self, abs: PathBuf, cx: &mut Context<Self>) {
		let rel = WsRel::of_folder(&self.workspace_root, &abs, false);
		match rel {
			Some(rel) => self.paste_into_rel(&rel, "browse", cx),
			None => {
				let shown = abs.display().to_string();
				self.refuse_paste_folder(
					not_under_workspace(&shown),
					"browse",
					cx,
				);
			}
		}
	}

	/// The OS folder dialog, local workspaces only.
	pub fn browse_paste_dest(&mut self, cx: &mut Context<Self>) {
		if self.remote_paste() || self.paste.plan().is_none() {
			return;
		}
		let picked = cx.prompt_for_paths(PathPromptOptions {
			files: false,
			directories: true,
			multiple: false,
			prompt: Some(
				i18n::t("paste_dest_browse_confirm", self.locale).into(),
			),
		});
		let tab = self.ws_tab;
		// The dialog is asynchronous: the preview it was opened over may
		// have been cancelled, or replaced by a new paste, by the time a
		// folder comes back.
		let session = self.paste.payload_token();
		cx.spawn(async move |this, cx| {
			crate::tabs::tagged(tab, async move {
				if let Ok(Ok(Some(paths))) = picked.await {
					if let Some(path) = paths.into_iter().next() {
						let _ = this.update(cx, |this, cx| {
							let same =
								match (&session, this.paste.payload_token()) {
									(Some(was), Some(now)) => was.ptr_eq(&now),
									_ => false,
								};
							if same {
								this.paste_dest_picked(path, cx)
							} else {
								app_log!("[APP:PASTE_DEST_IGNORED: via=browse stale]");
							}
						});
					}
				}
			})
			.await
		})
		.detach();
	}

	// The typed field. It is drawn in the panel's own flow, but it is an
	// inline layer of the overlay stack (#226), like the add-repo field:
	// the stack's one Escape rule closes it (and the panel at the next
	// Escape), and its close gives the keyboard back to the panel.

	pub(crate) fn open_paste_dest_edit(&mut self, cx: &mut Context<Self>) {
		if self.paste.plan().is_none()
			|| self.paste.is_applying()
			|| !self.accepting_work()
		{
			return;
		}
		self.user_action();
		self.paste.set_dest_refusal(None);
		let rel = self
			.paste
			.folder()
			.and_then(|f| f.rel.clone())
			.unwrap_or_else(WsRel::root);
		self.paste_dest_input.update(cx, |input, cx| {
			input.set_text(rel.as_str(), cx);
			input.select_all_text(cx);
		});
		if self.overlays.is_open(OverlayKind::PasteDestField) {
			// Opened again: the folder's name again, the old refusal gone.
			if let Some(edit) = self.overlays.paste_dest_edit_mut() {
				edit.error = None;
			}
			self.pending_focus = Some(self.paste_dest_input.read(cx).handle());
			app_log!("[APP:PASTE_DEST_EDIT: open]");
		} else {
			self.open_overlay(
				OverlayKind::PasteDestField,
				ReturnFocus::To(self.paste_focus.clone()),
				None,
				None,
				cx,
			);
		}
		cx.notify();
	}

	/// Enter in the field: the folder is used, or the field says why not.
	pub(crate) fn submit_paste_dest(&mut self, cx: &mut Context<Self>) {
		if self.paste_dest_edit().is_none() {
			return;
		}
		let typed = self.paste_dest_input.read(cx).text().to_string();
		// The field opened on the folder's own name, and left as it is, is
		// still that folder: a name with a space at its end is no typing.
		let untouched = self
			.paste
			.folder()
			.and_then(|f| f.rel.as_ref())
			.filter(|rel| rel.as_str() == typed)
			.cloned();
		let resolved = match untouched {
			Some(rel) => self.resolve_paste_folder(&rel),
			None => self.resolve_typed_folder(&typed),
		};
		match resolved {
			Ok(folder) => self.retarget_paste(folder, "input", cx),
			Err(err) => {
				app_log!(
					"[APP:PASTE_DEST_REFUSED: via=input reason={}]",
					err.key
				);
				if let Some(edit) = self.overlays.paste_dest_edit_mut() {
					edit.error = Some(err);
				}
				cx.notify();
			}
		}
	}

	/// Typing makes the last refusal stale.
	pub(crate) fn paste_dest_changed(&mut self, cx: &mut Context<Self>) {
		let stale = self.paste_dest_edit().is_some_and(|e| e.error.is_some())
			|| self.paste.dest_refusal().is_some();
		if stale {
			if let Some(edit) = self.overlays.paste_dest_edit_mut() {
				edit.error = None;
			}
			self.paste.set_dest_refusal(None);
			cx.notify();
		}
	}
}
