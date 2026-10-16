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
use crate::paste::dest::{folder_rel, DestFolder};
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

	/// `path` (a tree path) as a destination folder of this workspace.
	pub(crate) fn dest_folder(&self, path: PathBuf) -> DestFolder {
		DestFolder::new(&self.workspace_root, path, self.remote_paste())
	}

	/// The tree path of the workspace folder `rel` ("" is the workspace).
	fn folder_path(&self, rel: &str) -> PathBuf {
		rel.split('/')
			.filter(|part| !part.is_empty())
			.fold(self.workspace_root.clone(), |path, part| path.join(part))
	}

	/// The folder the user named by `typed`, a path inside the workspace
	/// ("" or "." is the workspace itself), or why it cannot be one. Every
	/// gesture of the destination bar goes through here.
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
		typed: &str,
	) -> Result<DestFolder, Msg> {
		let typed = typed.trim();
		let text = if cfg!(windows) {
			typed.replace('\\', "/")
		} else {
			typed.to_string()
		};
		let absolute = Path::new(typed).is_absolute()
			|| text.starts_with('/')
			|| (cfg!(windows) && text.as_bytes().get(1) == Some(&b':'));
		if absolute {
			return Err(not_under_workspace(typed));
		}
		let rel = text.trim_end_matches('/');
		let rel = if rel == "." { "" } else { rel };
		if rel
			.split('/')
			.any(|part| !rel.is_empty() && matches!(part, "" | "." | ".."))
		{
			return Err(not_under_workspace(typed));
		}
		let path = self.folder_path(rel);
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
			rel: Some(rel.to_string()),
		})
	}

	/// The folder of workspace-relative `rel`, as a gesture of the bar names
	/// it; a refusal goes to the status line.
	pub(crate) fn paste_into_rel(
		&mut self,
		rel: &str,
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
		self.set_status_msg(err);
		cx.notify();
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
			self.paste.close_dest_edit();
			self.pending_focus = Some(self.paste_focus.clone());
			cx.notify();
			return;
		}
		self.user_action();
		let shown = self.shown_folder(&folder);
		self.paste.set_folder(folder.clone());
		self.paste.close_dest_edit();
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
		let rel = folder_rel(&self.workspace_root, &path, self.remote_paste());
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
		let rel = folder_rel(&self.workspace_root, &abs, false);
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
		cx.spawn(async move |this, cx| {
			crate::tabs::tagged(tab, async move {
				if let Ok(Ok(Some(paths))) = picked.await {
					if let Some(path) = paths.into_iter().next() {
						let _ = this.update(cx, |this, cx| {
							this.paste_dest_picked(path, cx)
						});
					}
				}
			})
			.await
		})
		.detach();
	}

	// The typed field. It is not an overlay: it is drawn in the panel's own
	// flow, has one switch (these three functions) and hands focus back to
	// the panel alone. Escape closes it first and the panel second.

	pub(crate) fn open_paste_dest_edit(&mut self, cx: &mut Context<Self>) {
		if self.paste.plan().is_none()
			|| self.paste.is_applying()
			|| !self.accepting_work()
		{
			return;
		}
		self.user_action();
		let rel = self
			.paste
			.folder()
			.and_then(|f| f.rel.clone())
			.unwrap_or_default();
		self.paste.open_dest_edit();
		self.paste_dest_input.update(cx, |input, cx| {
			input.set_text(&rel, cx);
			input.select_all_text(cx);
		});
		self.pending_focus = Some(self.paste_dest_input.read(cx).handle());
		app_log!("[APP:PASTE_DEST_EDIT: open]");
		cx.notify();
	}

	/// Enter in the field: the folder is used, or the field says why not.
	pub(crate) fn submit_paste_dest(&mut self, cx: &mut Context<Self>) {
		if self.paste.dest_edit().is_none() {
			return;
		}
		let typed = self.paste_dest_input.read(cx).text().to_string();
		match self.resolve_paste_folder(&typed) {
			Ok(folder) => self.retarget_paste(folder, "input", cx),
			Err(err) => {
				app_log!(
					"[APP:PASTE_DEST_REFUSED: via=input reason={}]",
					err.key
				);
				self.paste.set_dest_edit_error(Some(err));
				cx.notify();
			}
		}
	}

	/// Escape in the field: what was typed is dropped, the folder stays.
	pub(crate) fn dismiss_paste_dest(&mut self, cx: &mut Context<Self>) {
		if self.paste.dest_edit().is_none() {
			return;
		}
		self.paste.close_dest_edit();
		self.pending_focus = Some(self.paste_focus.clone());
		app_log!("[APP:PASTE_DEST_EDIT: closed]");
		cx.notify();
	}

	/// Typing makes the last refusal stale.
	pub(crate) fn paste_dest_changed(&mut self, cx: &mut Context<Self>) {
		if self.paste.dest_edit().is_some_and(|e| e.error.is_some()) {
			self.paste.set_dest_edit_error(None);
			cx.notify();
		}
	}
}
