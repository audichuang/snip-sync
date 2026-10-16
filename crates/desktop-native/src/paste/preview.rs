//! Paste preview session.
//!
//! Owns the background job slot, 32 MiB shared budget admission and rollback,
//! the (selection, detail) pair, fold state, and auto-collapsed Git Log state
//! for the paste preview workflow (spec 3.2 / 4.3 / 5.1).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use snip_core::gitrun::CancelToken;

use super::dest::DestFolder;
use super::*;
use crate::i18n::Msg;
use crate::reader::Preview;

pub struct PastePreview {
	plan: Option<PastePreviewPlan>,
	detail: Option<Preview>,
	/// A read-only preview or mapping rebuild is running; nothing can be applied until it lands.
	loading: bool,
	/// Bumped for every paste request, remap and cancel; an older result is dropped.
	generation: u64,
	pending: Arc<Mutex<PastePending>>,
	worker: Option<(u64, CancelToken)>,
	cancel: Option<CancelToken>,
	scroll: gpui::UniformListScrollHandle,
	/// Log visibility saved when the preview auto-collapsed it.
	log_before: Option<bool>,
	/// Test-only delay before a confirmed write (E2E only).
	apply_delay: Option<std::time::Duration>,
	/// The destination folder as the user picked it. Survives a loading,
	/// refused or retargeting plan; reset by `clear`.
	folder: Option<DestFolder>,
	/// Why the last destination gesture of the bar (breadcrumb, chip, menu,
	/// dialog) named a folder the workspace cannot take. The open plan was
	/// left as it was; the bar says why until the destination moves or the
	/// user types.
	dest_refusal: Option<Msg>,
}

#[derive(Debug)]
pub enum Polled {
	Running,
	Settled {
		discarded: bool,
		landed: Option<Landed>,
	},
}

#[derive(Debug)]
pub enum Landed {
	/// A candidate plan was admitted into the session and installed as `self.plan`.
	///
	/// Invariant: whenever `Landed::Shown` is produced, `self.plan` is guaranteed to be `Some`.
	Shown {
		remap: Option<Remap>,
		/// The plan answers a destination change, not a first preview or a
		/// mapping choice.
		retargeted: bool,
	},
	Refused {
		err: Msg,
		closed: bool,
	},
	WorkerLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav {
	Ignored,
	Moved(usize),
	/// The cursor is on a folder row now (it has no file detail).
	MovedDir,
	Refused(Msg),
}

#[derive(Debug)]
pub enum Folded {
	NoPlan,
	/// A fold of a folder that is not shown, which does nothing.
	Ignored,
	Kept,
	Reselected(Nav),
}

pub enum Remapped {
	NoPlan,
	Invalid(Msg),
	Dropped(Msg),
	Ready(PasteRequest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyRefused {
	Loading,
	NoPlan,
	Busy,
	MappingRequired,
	NotExecutable,
}

/// Re-admits a state that only shrank since its last admission. Dropping or
/// shrinking retained UI data never needs more budget than was admitted.
/// The argument-free budget error adds no retained heap, so replacing any
/// admitted error with it cannot grow the state.
fn release(
	pending: &mut PastePending,
	ordinary: Option<&Preview>,
	plan: Option<&PastePreviewPlan>,
	detail: Option<&Preview>,
) {
	pending
		.admit_ui(ordinary, plan, detail)
		.expect("a smaller retained paste state fits its previous admission");
}

impl PastePreview {
	pub(crate) fn commit_diff_request(
		&self,
		idx: usize,
	) -> Result<Option<CommitDiffRequest>, Msg> {
		match self.plan.as_ref() {
			Some(plan) => plan.commit_diff_request(idx, &self.pending),
			None => Ok(None),
		}
	}

	pub fn new(apply_delay: Option<std::time::Duration>) -> Self {
		Self {
			plan: None,
			detail: None,
			loading: false,
			generation: 0,
			pending: Arc::new(Mutex::new(PastePending::default())),
			worker: None,
			cancel: None,
			scroll: gpui::UniformListScrollHandle::new(),
			log_before: None,
			apply_delay,
			folder: None,
			dest_refusal: None,
		}
	}

	/// The destination folder the user picked for this preview.
	pub fn folder(&self) -> Option<&DestFolder> {
		self.folder.as_ref()
	}

	/// Records the destination folder. Set after `clear`, which resets it.
	pub fn set_folder(&mut self, folder: DestFolder) {
		self.folder = Some(folder);
		self.dest_refusal = None;
	}

	/// The refusal the destination bar still shows.
	pub fn dest_refusal(&self) -> Option<&Msg> {
		self.dest_refusal.as_ref()
	}

	pub fn set_dest_refusal(&mut self, refusal: Option<Msg>) {
		self.dest_refusal = refusal;
	}

	/// Names the open plan's payload, so an answer that arrives later (the
	/// OS folder dialog) can tell whether it is still the same paste.
	pub fn payload_token(&self) -> Option<std::sync::Weak<str>> {
		self.plan.as_ref().map(|p| p.payload_token())
	}

	/// The session holds no plan any more (dropped by the budget).
	fn forget_destination(&mut self) {
		self.folder = None;
		self.dest_refusal = None;
	}

	pub fn plan(&self) -> Option<&PastePreviewPlan> {
		self.plan.as_ref()
	}

	/// A test swaps the client the plan's remote holds.
	#[cfg(test)]
	pub fn plan_mut(&mut self) -> Option<&mut PastePreviewPlan> {
		self.plan.as_mut()
	}

	/// A test holds retained bytes the way a running apply does, so a
	/// refusal needs neither a worker nor a clock to race.
	#[cfg(test)]
	pub fn hold_retained_for_test(&self, bytes: usize) {
		lock_pending(&self.pending).apply_active = bytes;
	}

	pub fn detail(&self) -> Option<&Preview> {
		self.detail.as_ref()
	}

	pub fn is_loading(&self) -> bool {
		self.loading
	}

	pub fn is_open(&self) -> bool {
		self.plan.is_some() || self.loading
	}

	pub fn is_applying(&self) -> bool {
		self.plan.as_ref().is_some_and(|p| p.is_applying)
	}

	pub fn scroll(&self) -> &gpui::UniformListScrollHandle {
		&self.scroll
	}

	pub fn apply_delay(&self) -> Option<std::time::Duration> {
		self.apply_delay
	}

	pub fn worker_id(&self) -> Option<u64> {
		self.worker.as_ref().map(|(id, _)| *id)
	}

	pub fn has_background_work(&self) -> bool {
		lock_pending(&self.pending).has_pending() || self.worker.is_some()
	}

	pub fn admit_ordinary(
		&self,
		ordinary: Option<&Preview>,
	) -> Result<(), Msg> {
		lock_pending(&self.pending).admit_ui(
			ordinary,
			self.plan.as_ref(),
			self.detail.as_ref(),
		)
	}

	pub fn release_ordinary(&self) {
		release(
			&mut lock_pending(&self.pending),
			None,
			self.plan.as_ref(),
			self.detail.as_ref(),
		);
	}

	pub fn invalidate_job(&mut self) {
		if let Some(token) = self.cancel.take() {
			token.cancel();
		}
		self.generation = self.generation.wrapping_add(1);
		lock_pending(&self.pending).invalidate(self.generation);
		self.loading = false;
	}

	pub fn clear(&mut self, ordinary: Option<&Preview>) {
		self.plan = None;
		self.detail = None;
		self.forget_destination();
		release(&mut lock_pending(&self.pending), ordinary, None, None);
	}

	pub fn enqueue(&mut self, request: PasteRequest) -> Result<(), Msg> {
		let result = lock_pending(&self.pending).enqueue(request);
		if let Err(err) = result {
			self.loading = false;
			Err(err)
		} else {
			self.loading = true;
			Ok(())
		}
	}

	pub fn poll(
		&mut self,
		worker_live: bool,
		ordinary: Option<&Preview>,
	) -> Polled {
		if self.worker.is_some() && worker_live {
			return Polled::Running;
		}
		let finished = self.worker.take();
		let discarded = finished
			.as_ref()
			.is_some_and(|(_, token)| token.is_cancelled());
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		let landed = if let Some(outcome) = pending.ready.take() {
			self.loading = false;
			Some(self.land(outcome, &mut pending, ordinary))
		} else if finished
			.as_ref()
			.is_some_and(|(_, token)| !token.is_cancelled())
			&& !pending.has_pending()
			&& self.loading
		{
			self.loading = false;
			Some(Landed::WorkerLost)
		} else {
			None
		};
		Polled::Settled { discarded, landed }
	}

	fn land(
		&mut self,
		outcome: PasteOutcome,
		pending: &mut PastePending,
		ordinary: Option<&Preview>,
	) -> Landed {
		let remap = outcome.remap;
		let retargeted = outcome.retargeted;
		let built = outcome.result.and_then(|mut plan| {
			// Folds and the folder cursor survive a remap only where the
			// folder is still there.
			plan.normalize_cursor();
			let detail = plan.detail_preview();
			pending.admit_ui(ordinary, Some(&plan), detail.as_ref())?;
			Ok((plan, detail))
		});
		match built {
			Ok((plan, detail)) => {
				self.plan = Some(plan);
				self.detail = detail;
				self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
				Landed::Shown { remap, retargeted }
			}
			Err(err) => {
				if pending
					.admit_ui(
						ordinary,
						self.plan.as_ref(),
						self.detail.as_ref(),
					)
					.is_err()
				{
					self.plan = None;
					self.detail = None;
					self.forget_destination();
					release(pending, ordinary, None, None);
				}
				Landed::Refused {
					err,
					closed: self.plan.is_none(),
				}
			}
		}
	}

	pub fn start_job(
		&mut self,
	) -> Result<Option<(PasteWorker, CancelToken)>, Msg> {
		let cancel = CancelToken::new();
		match PastePending::start(
			&self.pending,
			self.plan.as_ref(),
			cancel.clone(),
		) {
			Ok(Some(w)) => Ok(Some((w, cancel))),
			Ok(None) => Ok(None),
			Err(e) => {
				self.loading = false;
				Err(e)
			}
		}
	}

	pub fn bind_job(&mut self, id: u64, cancel: CancelToken) {
		self.cancel = Some(cancel.clone());
		self.worker = Some((id, cancel));
	}

	pub fn record_error(&mut self, err: Msg, ordinary: Option<&Preview>) {
		let mut pending = lock_pending(&self.pending);
		if let Some(plan) = &mut self.plan {
			plan.error = Some(err);
		}
		if pending
			.admit_ui(ordinary, self.plan.as_ref(), self.detail.as_ref())
			.is_err()
		{
			if let Some(plan) = &mut self.plan {
				plan.error = Some(preview_budget_error());
			}
			release(
				&mut pending,
				ordinary,
				self.plan.as_ref(),
				self.detail.as_ref(),
			);
		}
	}

	/// Brings an open, idle preview's mapping candidates up to date with the
	/// repos found since it opened. A busy, loading or closed preview is left.
	pub fn refresh_candidates(
		&mut self,
		known_roots: &[PathBuf],
		ordinary: Option<&Preview>,
	) -> Result<bool, Msg> {
		if self.loading {
			return Ok(false);
		}
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		let Some(plan) = self.plan.as_mut() else {
			return Ok(false);
		};
		if plan.is_applying {
			return Ok(false);
		}
		// A refused refresh must cost nothing: find what is news and project
		// its size first, and allocate only once the budget admits it.
		let seen = plan.seen_roots;
		let Some(found) = plan.new_roots(known_roots) else {
			return Ok(false);
		};
		let headroom =
			pending.ui_headroom(ordinary, plan, self.detail.as_ref());
		if plan.candidate_growth_bytes(&found, headroom) > headroom {
			// The set was not admitted, so it is still news next time.
			plan.seen_roots = seen;
			return Err(preview_budget_error());
		}
		// Build the additions on copies and keep the admitted allocations
		// aside, so a refusal puts back exactly what was admitted (a truncate
		// keeps the grown capacity, which the budget counts).
		let Some(admitted) = plan.extend_candidates(&found) else {
			// Nothing to add: the digest still moves on, as it always did.
			return Ok(false);
		};
		if let Err(err) =
			pending.admit_ui(ordinary, Some(&*plan), self.detail.as_ref())
		{
			for (choice, kept) in plan.prefix_choices.iter_mut().zip(admitted) {
				if let Some(kept) = kept {
					choice.candidates = kept;
				}
			}
			plan.seen_roots = seen;
			return Err(err);
		}
		Ok(true)
	}

	pub fn remap(
		&mut self,
		prefix: String,
		destination: Option<PathBuf>,
		ordinary: Option<&Preview>,
	) -> Remapped {
		let keep = destination.is_none();
		self.replan(ordinary, |plan| {
			match destination {
				Some(dest) => plan.choose_prefix_destination(&prefix, &dest),
				None => plan.choose_keep_relative(&prefix),
			}?;
			Ok(Remap::Prefix { prefix, keep })
		})
	}

	/// Re-plans the shell with a changed choice. The one path every choice
	/// takes, so an older plan can never be applied while the new one is
	/// still being built: the file plan is cleared and the UI disarmed first.
	fn replan(
		&mut self,
		ordinary: Option<&Preview>,
		choose: impl FnOnce(&mut PastePreviewPlan) -> Result<Remap, Msg>,
	) -> Remapped {
		self.invalidate_job();
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		let Some(plan) = self.plan.as_mut() else {
			return Remapped::NoPlan;
		};
		plan.clear_file_plan();
		plan.error = None;
		self.detail = None;
		let remap = match choose(plan) {
			Ok(remap) => remap,
			Err(err) => {
				release(&mut pending, ordinary, Some(plan), None);
				return Remapped::Invalid(err);
			}
		};
		if let Err(err) = pending.admit_ui(ordinary, Some(plan), None) {
			self.plan = None;
			self.forget_destination();
			release(&mut pending, ordinary, None, None);
			return Remapped::Dropped(err);
		}
		Remapped::Ready(PasteRequest::Remap(remap))
	}

	/// Re-plans with the restore-base hint's choice. Goes through
	/// [`Self::replan`] like a prefix choice, so the plan planned without it
	/// cannot be applied while the new one is built.
	pub fn adjust_paths(
		&mut self,
		on: bool,
		ordinary: Option<&Preview>,
	) -> Remapped {
		self.replan(ordinary, |plan| {
			plan.choose_adjust_paths(on)?;
			Ok(Remap::AdjustPaths(on))
		})
	}

	/// Moves the open preview to `dest`: the same payload planned again
	/// there. The shell is disarmed first, so neither Apply nor a second
	/// gesture can act on what was planned for the old folder.
	pub fn retarget(
		&mut self,
		dest: PathBuf,
		roots: Vec<PathBuf>,
		generation: u64,
		ordinary: Option<&Preview>,
	) -> Remapped {
		self.invalidate_job();
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		let Some(plan) = self.plan.as_mut() else {
			return Remapped::NoPlan;
		};
		plan.disarm_for(dest.clone());
		self.detail = None;
		if let Err(err) = pending.admit_ui(ordinary, Some(plan), None) {
			self.plan = None;
			self.forget_destination();
			release(&mut pending, ordinary, None, None);
			return Remapped::Dropped(err);
		}
		Remapped::Ready(PasteRequest::Retarget {
			dest,
			roots,
			generation,
		})
	}

	pub fn toggle_overwrite(&mut self, idx: usize) -> Option<bool> {
		let p = self.plan.as_mut()?;
		p.toggle_overwrite(idx);
		Some(p.items.get(idx).is_some_and(|i| i.overwrite_allowed))
	}

	pub fn toggle_selected(&mut self, idx: usize) -> Option<bool> {
		let p = self.plan.as_mut()?;
		p.toggle_selected(idx);
		Some(p.items.get(idx).is_some_and(|i| i.selected))
	}

	pub fn select(&mut self, idx: usize, ordinary: Option<&Preview>) -> Nav {
		let Some(plan) = self.plan.as_mut() else {
			return Nav::Ignored;
		};
		if idx >= plan.items.len() {
			return Nav::Ignored;
		}
		let detail = plan.detail_at(idx);
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		match pending.admit_ui(ordinary, Some(plan), detail.as_ref()) {
			Ok(()) => {
				plan.selected_item_idx = idx;
				plan.cursor_dir = None;
				self.detail = detail;
				self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
				Nav::Moved(idx)
			}
			Err(e) => Nav::Refused(e),
		}
	}

	/// Puts the cursor on a folder row. A folder has no file detail.
	pub fn select_dir(
		&mut self,
		key: DirKey,
		ordinary: Option<&Preview>,
	) -> Nav {
		let Some(plan) = self.plan.as_mut() else {
			return Nav::Ignored;
		};
		let on = PasteCursor::Dir(key.clone());
		if !plan.cursor_rows().iter().any(|(c, ..)| *c == on) {
			return Nav::Ignored;
		}
		let before = plan.cursor_dir.replace(key);
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		match pending.admit_ui(ordinary, Some(plan), None) {
			Ok(()) => {
				self.detail = None;
				Nav::MovedDir
			}
			Err(e) => {
				plan.cursor_dir = before;
				Nav::Refused(e)
			}
		}
	}

	/// Moves the cursor to a row `step_target` or an arrow key chose.
	pub fn move_to(
		&mut self,
		cursor: PasteCursor,
		ordinary: Option<&Preview>,
	) -> Nav {
		match cursor {
			PasteCursor::Item(idx) => self.select(idx, ordinary),
			PasteCursor::Dir(key) => self.select_dir(key, ordinary),
		}
	}

	pub fn replace_detail(
		&mut self,
		idx: usize,
		detail: Preview,
		ordinary: Option<&Preview>,
	) -> Result<(), Msg> {
		let Some(plan) = self.plan.as_ref() else {
			return Err(Msg::new("paste_err_nothing", []));
		};
		// A read that finishes after the cursor moved on, to another file
		// or to a folder, is not shown.
		if plan.selected_item_idx != idx || plan.cursor_dir.is_some() {
			return Ok(());
		}
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		pending.admit_ui(ordinary, Some(plan), Some(&detail))?;
		self.detail = Some(detail);
		self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
		Ok(())
	}

	pub fn step(&mut self, forward: bool, ordinary: Option<&Preview>) -> Nav {
		let Some(plan) = self.plan.as_ref() else {
			return Nav::Ignored;
		};
		let target = plan.step_target(forward);
		self.move_to(target, ordinary)
	}

	/// Folds or unfolds a folder. The fold grows the retained state (its
	/// key), so unlike a commit fold it is admitted first and a refusal
	/// undoes all of it, never leaving a cursor on a row nobody can see.
	/// A key that names no row shown now (a click from a frame the plan has
	/// left behind) does nothing.
	pub fn toggle_dir_fold(
		&mut self,
		key: DirKey,
		ordinary: Option<&Preview>,
	) -> Folded {
		let Some(plan) = self.plan.as_mut() else {
			return Folded::NoPlan;
		};
		if !plan.has_dir_row(&key) {
			return Folded::Ignored;
		}
		let was_folded = plan.collapsed_dirs.contains(&key);
		let (item_before, dir_before) =
			(plan.selected_item_idx, plan.cursor_dir.clone());
		let moved = plan.toggle_dir_collapsed(&key);
		let (nav, moved_detail) = match moved {
			Some(PasteCursor::Dir(on)) => {
				plan.cursor_dir = Some(on);
				(Some(Nav::MovedDir), None)
			}
			Some(PasteCursor::Item(ix)) => {
				plan.selected_item_idx = ix;
				plan.cursor_dir = None;
				(Some(Nav::Moved(ix)), plan.detail_at(ix))
			}
			None => (None, None),
		};
		let detail = if nav.is_some() {
			moved_detail.as_ref()
		} else {
			self.detail.as_ref()
		};
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		match pending.admit_ui(ordinary, Some(plan), detail) {
			Ok(()) => match nav {
				Some(nav) => {
					self.detail = moved_detail;
					self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
					Folded::Reselected(nav)
				}
				None => Folded::Kept,
			},
			Err(e) => {
				if was_folded {
					plan.collapsed_dirs.insert(key);
				} else {
					plan.collapsed_dirs.remove(&key);
				}
				plan.selected_item_idx = item_before;
				plan.cursor_dir = dir_before;
				Folded::Reselected(Nav::Refused(e))
			}
		}
	}

	/// The include box of a folder.
	pub fn toggle_dir_selected(&mut self, key: &DirKey) -> Option<bool> {
		self.plan.as_mut()?.set_dir_selected(key)
	}

	pub fn toggle_fold(
		&mut self,
		c: usize,
		ordinary: Option<&Preview>,
	) -> Folded {
		let Some(plan) = self.plan.as_mut() else {
			return Folded::NoPlan;
		};
		match plan.toggle_commit_collapsed(c) {
			None => Folded::Kept,
			Some(ix) => Folded::Reselected(self.select(ix, ordinary)),
		}
	}

	pub fn begin_apply(&mut self) -> Result<PasteApplyWorker, ApplyRefused> {
		if self.loading {
			return Err(ApplyRefused::Loading);
		}
		let Some(plan) = self.plan.as_mut() else {
			return Err(ApplyRefused::NoPlan);
		};
		if plan.is_applying {
			return Err(ApplyRefused::Busy);
		}
		if !plan.mapping_ready() {
			return Err(ApplyRefused::MappingRequired);
		}
		if !plan.executable() {
			return Err(ApplyRefused::NotExecutable);
		}
		plan.is_applying = true;
		Ok(PasteApplyWorker::new(plan, &self.pending))
	}

	pub fn apply_failed(&mut self, err: Msg, ordinary: Option<&Preview>) {
		if let Some(plan) = &mut self.plan {
			plan.is_applying = false;
		}
		self.record_error(err, ordinary);
	}

	/// Hides the Git Log for the paste preview, remembering the prior state once
	/// (a re-paste over an open preview keeps the first saved value). Returns
	/// true when the panel was actually hidden by this call.
	pub fn collapse_log(&mut self, visible: &mut bool) -> bool {
		self.log_before.get_or_insert(*visible);
		std::mem::replace(visible, false)
	}

	/// Restores Git Log visibility once the paste preview closes, unless the
	/// user toggled it themselves while the preview was open.
	pub fn restore_log(&mut self, visible: &mut bool) -> bool {
		match self.log_before.take() {
			Some(true) if !*visible => {
				*visible = true;
				true
			}
			_ => false,
		}
	}

	pub fn forget_log_restore(&mut self) {
		self.log_before = None;
	}
}

#[cfg(test)]
mod tests {
	use super::super::tests::git_init;
	use super::*;
	use crate::reader::PreviewSource;
	use crate::syntax::Language;
	use snip_core::gitrun::RunOptions;
	use std::fs;
	use std::path::Path;

	fn landed(
		text: &str,
		dest: &Path,
		ordinary: Option<&Preview>,
	) -> PastePreview {
		let mut s = PastePreview::new(None);
		s.enqueue(PasteRequest::Clipboard {
			text: text.into(),
			dest: dest.into(),
			roots: Vec::new(),
			generation: 1,
			remote: None,
		})
		.unwrap();
		let (work, cancel) = s.start_job().unwrap().expect("job");
		work.run(&RunOptions::default());
		s.bind_job(1, cancel);
		assert!(matches!(
			s.poll(false, ordinary),
			Polled::Settled {
				landed: Some(Landed::Shown { .. }),
				..
			}
		));
		s
	}

	fn small_ordinary() -> Preview {
		Preview::new(
			PreviewSource::WorkingFile,
			None,
			"old".into(),
			false,
			Language::Plain,
		)
	}

	/// One commit adding `a.txt`, for the replay tests of a retarget.
	fn commit_text() -> String {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		snip_core::commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "add a".into(),
				author_name: "Ann".into(),
				author_email: "ann@example.invalid".into(),
				author_date: "2026-10-13T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "a.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("A\n".into()),
					not_copied: None,
				}],
			}],
		})
	}

	/// Runs the retarget job `request` asks for, as the model does.
	fn run_retarget(session: &mut PastePreview, request: PasteRequest) {
		session.enqueue(request).unwrap();
		let (work, cancel) = session.start_job().unwrap().expect("job");
		work.run(&RunOptions::default());
		session.bind_job(1, cancel);
	}

	/// Between the gesture and the new plan nothing planned for the old
	/// folder can be applied, a replayed commit included (#238).
	#[test]
	fn retarget_disarms_the_shell_before_the_job_runs() {
		let dir = tempfile::tempdir().unwrap();
		let other = tempfile::tempdir().unwrap();
		git_init(dir.path());
		for text in ["// FILE: a.txt\nA\n".to_string(), commit_text()] {
			let mut session = landed(&text, dir.path(), None);
			assert!(session.plan().unwrap().executable());
			let Remapped::Ready(request) =
				session.retarget(other.path().into(), Vec::new(), 2, None)
			else {
				panic!("a plan is open");
			};
			let plan = session.plan().unwrap();
			assert_eq!(plan.destination, other.path());
			assert!(!plan.executable());
			assert!(plan.items.is_empty());
			assert!(plan.commit_preview.is_none());
			assert_eq!(
				session.begin_apply().err(),
				Some(ApplyRefused::NotExecutable)
			);
			session.enqueue(request).unwrap();
			assert_eq!(
				session.begin_apply().err(),
				Some(ApplyRefused::Loading)
			);
		}
	}

	/// A folder the engine cannot use lands as a shell with the engine's own
	/// words, not as a refused job; only the memory budget refuses (#238).
	#[test]
	fn retarget_to_a_missing_folder_lands_a_shell_with_the_engine_error() {
		let dir = tempfile::tempdir().unwrap();
		let plain = tempfile::tempdir().unwrap();
		git_init(dir.path());
		let missing = dir.path().join("missing");
		for (text, to) in [
			("// FILE: a.txt\nA\n".to_string(), missing.clone()),
			// A replay has no repository in a plain folder.
			(commit_text(), plain.path().to_path_buf()),
		] {
			let mut session = landed(&text, dir.path(), None);
			let Remapped::Ready(request) =
				session.retarget(to.clone(), Vec::new(), 2, None)
			else {
				panic!("a plan is open");
			};
			run_retarget(&mut session, request);
			let Polled::Settled {
				landed: Some(Landed::Shown { retargeted, .. }),
				..
			} = session.poll(false, None)
			else {
				panic!("the shell lands");
			};
			assert!(retargeted);
			let plan = session.plan().unwrap();
			assert_eq!(plan.destination, to);
			assert!(!plan.executable());
			assert!(plan.error.is_some(), "the refusal is the plan's error");
		}
		let missing_key = {
			let mut session = landed("// FILE: a.txt\nA\n", dir.path(), None);
			let Remapped::Ready(request) =
				session.retarget(missing.clone(), Vec::new(), 2, None)
			else {
				panic!("a plan is open");
			};
			run_retarget(&mut session, request);
			session.poll(false, None);
			session.plan().unwrap().error.as_ref().unwrap().key
		};
		assert_eq!(missing_key, "paste_err_destination");

		// The budget still refuses.
		let mut shell = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: a.txt\nA\n",
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		shell.disarm_for(dir.path().into());
		let err = shell
			.retarget_with(dir.path(), &[], 2, &RunOptions::default(), 5)
			.expect_err("over the budget");
		assert_eq!(err.key, "preview_memory_limit");
	}

	/// The engine suggests dropping `sub/` for a folder named `sub`, then
	/// refuses the plan as it stands (a regular file `sub` is where the
	/// directory `sub/` would go). The refusal lands with the suggestion:
	/// [Adjust paths] is what recovers, and it plans (#240).
	#[test]
	fn a_retarget_the_engine_refuses_keeps_the_restore_base_hint() {
		let ws = tempfile::tempdir().unwrap();
		let first = ws.path().join("first");
		let sub = ws.path().join("sub");
		for dir in [&first, &sub] {
			std::fs::create_dir(dir).unwrap();
			git_init(dir);
		}
		std::fs::write(sub.join("sub"), "a file named like the folder")
			.unwrap();
		let payload = "// clipcode-root: mono\n// FILE: sub/a.ts\nx\n";
		let mut session = landed(payload, &first, None);
		assert!(session.plan().unwrap().executable());
		let Remapped::Ready(request) =
			session.retarget(sub.clone(), Vec::new(), 2, None)
		else {
			panic!("a plan is open");
		};
		run_retarget(&mut session, request);
		let Polled::Settled {
			landed: Some(Landed::Shown { retargeted, .. }),
			..
		} = session.poll(false, None)
		else {
			panic!("the refusal lands as a shell");
		};
		assert!(retargeted);
		let plan = session.plan().unwrap();
		assert_eq!(plan.destination, sub);
		assert!(!plan.executable());
		assert!(plan.error.is_some(), "the engine's refusal");
		assert!(
			plan.restore_hint().is_some(),
			"the suggestion that recovers was thrown away with the plan"
		);
		let Remapped::Ready(request) = session.adjust_paths(true, None) else {
			panic!("the hint is there to be taken");
		};
		run_retarget(&mut session, request);
		assert!(matches!(
			session.poll(false, None),
			Polled::Settled {
				landed: Some(Landed::Shown { .. }),
				..
			}
		));
		let plan = session.plan().unwrap();
		assert!(plan.executable(), "{:?}", plan.error);
		assert_eq!(
			plan.items
				.iter()
				.map(|i| i.path.as_str())
				.collect::<Vec<_>>(),
			["a.ts"]
		);
	}

	/// The other way round: the adjusted plan is the one the engine refuses
	/// (a directory `a.ts` stands where the file goes). The refusal keeps the
	/// suggestion, so [Use as-is] still undoes the adjustment (#240).
	#[test]
	fn an_adjustment_the_engine_refuses_keeps_the_restore_base_hint() {
		let ws = tempfile::tempdir().unwrap();
		let sub = ws.path().join("sub");
		std::fs::create_dir(&sub).unwrap();
		git_init(&sub);
		std::fs::create_dir(sub.join("a.ts")).unwrap();
		std::fs::write(sub.join("a.ts").join("kept"), "x").unwrap();
		let payload = "// clipcode-root: mono\n// FILE: sub/a.ts\nx\n";
		let mut session = landed(payload, &sub, None);
		assert!(session.plan().unwrap().restore_hint().is_some());
		let Remapped::Ready(request) = session.adjust_paths(true, None) else {
			panic!("the hint is there to be taken");
		};
		run_retarget(&mut session, request);
		session.poll(false, None);
		let plan = session.plan().unwrap();
		assert!(plan.adjust_paths);
		assert!(!plan.executable(), "the adjusted plan is refused");
		assert!(plan.error.is_some());
		assert!(
			plan.restore_hint().is_some(),
			"the refusal threw the hint away: no [Use as-is]"
		);
		let Remapped::Ready(request) = session.adjust_paths(false, None) else {
			panic!("[Use as-is] is there to be taken");
		};
		run_retarget(&mut session, request);
		session.poll(false, None);
		let plan = session.plan().unwrap();
		assert!(plan.executable(), "{:?}", plan.error);
	}

	#[test]
	fn cancelling_a_preview_keeps_a_held_replay_read_in_repaste_admission() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let dir = tempfile::tempdir().unwrap();
		crate::paste::tests::git_init(dir.path());
		let payload = snip_core::commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "large captured commit".into(),
				author_name: "Ann".into(),
				author_email: "ann@example.invalid".into(),
				author_date: "2026-10-13T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "large.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("x".repeat(2 * 1024 * 1024)),
					not_copied: None,
				}],
			}],
		});
		let mut session = landed(&payload, dir.path(), None);
		drop(payload);
		let read = session.commit_diff_request(0).unwrap().unwrap();
		let held = Arc::downgrade(&read.preview);
		// Cancel removes the UI, while the queued reader still owns the old
		// captured plan. A rapid paste must account for that retained payload.
		session.invalidate_job();
		session.clear(None);
		assert!(held.upgrade().is_some());
		let next = || PasteRequest::Clipboard {
			text: "n".repeat(MAX_RETAINED_PREVIEW_BYTES - 1024 * 1024),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 2,
			remote: None,
		};
		assert_eq!(
			session.enqueue(next()).unwrap_err().key,
			"preview_memory_limit"
		);
		// Once an unpolled/cancelled read is dropped, the same paste fits.
		drop(read);
		assert!(held.upgrade().is_none());
		assert!(session.enqueue(next()).is_ok());
	}

	#[test]
	fn a_detail_read_of_the_ui_preview_is_charged_only_once() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let dir = tempfile::tempdir().unwrap();
		crate::paste::tests::git_init(dir.path());
		let payload = snip_core::commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "shared commit".into(),
				author_name: "Ann".into(),
				author_email: "ann@example.invalid".into(),
				author_date: "2026-10-13T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "shared.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("x".repeat(2 * 1024 * 1024)),
					not_copied: None,
				}],
			}],
		});
		let mut session = landed(&payload, dir.path(), None);
		let used = |s: &PastePreview| {
			let st = lock_pending(&s.pending);
			st.total_with_ui(
				st.ui_bytes,
				st.apply_allowance,
				st.ui_raw.as_ref(),
				st.ui_commit.as_ref(),
			)
		};
		let ui_only = used(&session);
		// The read shares the UI's commit preview, which the UI already counts.
		let read = session.commit_diff_request(0).unwrap().unwrap();
		assert_eq!(used(&session), ui_only);
		// Once the UI lets the preview go, the held read is the only owner
		// and must be charged in full.
		session.clear(None);
		assert!(Arc::downgrade(&read.preview).upgrade().is_some());
		assert!(used(&session) >= 2 * 1024 * 1024);
		drop(read);
		assert!(used(&session) < 2 * 1024 * 1024);
	}

	#[test]
	fn repeated_detail_reads_of_a_dropped_ui_preview_cannot_grow_release() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let dir = tempfile::tempdir().unwrap();
		crate::paste::tests::git_init(dir.path());
		let payload = snip_core::commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "repeatedly read commit".into(),
				author_name: "Ann".into(),
				author_email: "ann@example.invalid".into(),
				author_date: "2026-10-13T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "repeated.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("x".repeat(2 * 1024 * 1024)),
					not_copied: None,
				}],
			}],
		});
		let mut session = landed(&payload, dir.path(), None);
		drop(payload);
		let used = |s: &PastePreview| {
			let st = lock_pending(&s.pending);
			st.total_with_ui(
				st.ui_bytes,
				st.apply_allowance,
				st.ui_raw.as_ref(),
				st.ui_commit.as_ref(),
			)
		};
		let ui_only = used(&session);
		// Twenty reads of one 2 MiB preview would be 40 MiB if each were
		// charged in full after the UI lets go. Each read is free while the
		// UI holds the Arc, so all of them are admitted.
		let reads: Vec<_> = (0..20)
			.map(|_| session.commit_diff_request(0).unwrap().unwrap())
			.collect();
		assert_eq!(used(&session), ui_only);
		// Dropping the UI's preview must not panic in `release`, and the
		// shared preview is charged once, not once per read.
		session.clear(None);
		let after = used(&session);
		assert!(after <= ui_only);
		assert!(after >= 2 * 1024 * 1024);
		assert!(after < 4 * 1024 * 1024);
		drop(reads);
		assert!(used(&session) < 2 * 1024 * 1024);
	}

	/// Grows `ordinary` so the admitted state sits exactly at the 32 MiB cap.
	fn fill_budget(s: &PastePreview, ordinary: &mut Preview) {
		let used = {
			let st = lock_pending(&s.pending);
			st.total_with_ui(
				st.ui_bytes,
				st.apply_allowance,
				st.ui_raw.as_ref(),
				st.ui_commit.as_ref(),
			)
		};
		ordinary.source = PreviewSource::CommitFile {
			sha: String::with_capacity(MAX_RETAINED_PREVIEW_BYTES - used),
		};
		s.admit_ordinary(Some(ordinary)).unwrap();
	}

	#[test]
	fn refused_step_leaves_selection_and_detail_unchanged() {
		let dir = tempfile::tempdir().unwrap();
		let payload = format!(
			"// FILE: small.txt\nx\n// FILE: large.txt\n{}\n",
			"z\n".repeat(2000)
		);
		let mut o = small_ordinary();
		let mut s = landed(&payload, dir.path(), Some(&o));
		assert_eq!(s.plan().unwrap().selected_item_idx, 0);
		fill_budget(&s, &mut o);
		let res = s.step(false, Some(&o));
		assert!(
			matches!(res, Nav::Refused(ref e) if e.key == "preview_memory_limit")
		);
		assert_eq!(s.plan().unwrap().selected_item_idx, 0);
		assert_eq!(
			s.detail().and_then(|d| d.path.as_deref()),
			Some("small.txt")
		);

		let mut s2 = landed(&payload, dir.path(), None);
		assert_eq!(s2.step(false, None), Nav::Moved(1));
		assert_eq!(
			s2.detail().and_then(|d| d.path.as_deref()),
			Some("large.txt")
		);
	}

	/// Plans the writes again for `prefix` (kept under the destination) and
	/// lands the result, as a click on its "keep" button does.
	fn land_remap(s: &mut PastePreview, prefix: &str) {
		let Remapped::Ready(request) = s.remap(prefix.into(), None, None)
		else {
			panic!("{prefix}: no remap to run");
		};
		s.enqueue(request).unwrap();
		let (work, cancel) = s.start_job().unwrap().expect("job");
		work.run(&RunOptions::default());
		s.bind_job(1, cancel);
		assert!(matches!(
			s.poll(false, None),
			Polled::Settled {
				landed: Some(Landed::Shown { .. }),
				..
			}
		));
	}

	/// A remap resets the cursor to the first row. When a fold that survived
	/// the remap hides that row, the cursor goes to the folder instead of
	/// staying on a row nobody can see (and Space, the detail pane and a
	/// diff read would act on it).
	#[test]
	fn a_remap_whose_first_row_is_folded_away_lands_on_its_folder() {
		let dir = tempfile::tempdir().unwrap();
		let mut s = landed(
			"// FILE: a/x/1.txt\n1\n// FILE: b/y/2.txt\n2\n",
			dir.path(),
			None,
		);
		land_remap(&mut s, "a");
		land_remap(&mut s, "b");
		let (first, other) =
			(index_of(&s, "a/x/1.txt"), index_of(&s, "b/y/2.txt"));
		assert_eq!(first, 0, "the remap's reset cursor is on the first row");
		let ax = dir_key(&s, "a/x");
		assert_eq!(s.select(other, None), Nav::Moved(other));
		assert!(matches!(s.toggle_dir_fold(ax.clone(), None), Folded::Kept));

		land_remap(&mut s, "a");
		let plan = s.plan().unwrap();
		assert!(plan.collapsed_dirs.contains(&ax), "the fold survived");
		assert_eq!(plan.cursor(), PasteCursor::Dir(ax));
		assert!(plan.detail_preview().is_none());
		assert!(s.detail().is_none());
	}

	const SRC_TREE: &str = "// clipcode-root: src\n// FILE: src/app/a.ts\nA\n// FILE: src/app/b.ts\nB\n// FILE: src/lib/c.ts\nC\n";

	fn dir_key(s: &PastePreview, path: &str) -> DirKey {
		let plan = s.plan().unwrap();
		DirKey {
			commit: None,
			root: plan.items[0].dest_root.clone(),
			path: path.into(),
		}
	}

	fn index_of(s: &PastePreview, path: &str) -> usize {
		s.plan()
			.unwrap()
			.items
			.iter()
			.position(|i| i.path == path)
			.unwrap()
	}

	/// A click carries the key its frame drew. When the plan has moved on, a
	/// key that names no row now must not leave a fold (or a cursor) behind.
	#[test]
	fn folding_a_folder_that_is_not_shown_does_nothing() {
		let dir = tempfile::tempdir().unwrap();
		let mut s = landed(SRC_TREE, dir.path(), None);
		let before = s.plan().unwrap().cursor();
		let ghost = dir_key(&s, "src/gone");
		assert!(matches!(s.toggle_dir_fold(ghost, None), Folded::Ignored));
		assert!(s.plan().unwrap().collapsed_dirs.is_empty());
		assert_eq!(s.plan().unwrap().cursor(), before);

		// `src/app` is no row while `src` is folded.
		let (src, app) = (dir_key(&s, "src"), dir_key(&s, "src/app"));
		assert!(matches!(
			s.toggle_dir_fold(src.clone(), None),
			Folded::Reselected(Nav::MovedDir)
		));
		assert!(matches!(s.toggle_dir_fold(app, None), Folded::Ignored));
		assert_eq!(s.plan().unwrap().collapsed_dirs.len(), 1);
		assert!(s.plan().unwrap().collapsed_dirs.contains(&src));
	}

	/// A folder fold grows the retained set (its key), so unlike a commit
	/// fold (#refused_fold_reselect_keeps_the_hidden_row_with_its_detail)
	/// a refusal undoes the whole fold, not only the move.
	#[test]
	fn refused_dir_fold_leaves_tree_and_cursor_unchanged() {
		let dir = tempfile::tempdir().unwrap();
		let mut o = small_ordinary();
		let mut s = landed(SRC_TREE, dir.path(), Some(&o));
		let c = index_of(&s, "src/lib/c.ts");
		assert_eq!(s.select(c, Some(&o)), Nav::Moved(c));
		fill_budget(&s, &mut o);
		let app = dir_key(&s, "src/app");
		let res = s.toggle_dir_fold(app, Some(&o));
		assert!(
			matches!(res, Folded::Reselected(Nav::Refused(ref e)) if e.key == "preview_memory_limit"),
			"{res:?}"
		);
		let plan = s.plan().unwrap();
		assert!(plan.collapsed_dirs.is_empty());
		assert_eq!(plan.selected_item_idx, c);
		assert!(plan.cursor_dir.is_none());
		assert_eq!(
			s.detail().and_then(|d| d.path.as_deref()),
			Some("src/lib/c.ts")
		);
		// With room, the same fold goes through.
		let mut s = landed(SRC_TREE, dir.path(), None);
		let c = index_of(&s, "src/lib/c.ts");
		assert_eq!(s.select(c, None), Nav::Moved(c));
		let app = dir_key(&s, "src/app");
		assert!(matches!(s.toggle_dir_fold(app.clone(), None), Folded::Kept));
		assert!(s.plan().unwrap().collapsed_dirs.contains(&app));
	}

	#[test]
	fn selecting_a_folder_drops_the_detail_and_a_file_brings_it_back() {
		let dir = tempfile::tempdir().unwrap();
		let mut s = landed(SRC_TREE, dir.path(), None);
		let b = index_of(&s, "src/app/b.ts");
		assert_eq!(s.select(b, None), Nav::Moved(b));
		assert!(s.detail().is_some());
		let app = dir_key(&s, "src/app");
		assert_eq!(s.select_dir(app.clone(), None), Nav::MovedDir);
		assert!(s.detail().is_none());
		assert_eq!(s.plan().unwrap().cursor_dir, Some(app));
		// A late diff read for the file the cursor left is not shown.
		let stale = s.plan().unwrap().detail_at(b).unwrap();
		assert!(s.replace_detail(b, stale, None).is_ok());
		assert!(s.detail().is_none());
		assert_eq!(s.select(b, None), Nav::Moved(b));
		assert!(s.plan().unwrap().cursor_dir.is_none());
		assert_eq!(
			s.detail().and_then(|d| d.path.as_deref()),
			Some("src/app/b.ts")
		);
	}

	#[test]
	fn refused_navigation_keeps_plan_selection_detail_and_busy_state() {
		let dir = tempfile::tempdir().unwrap();
		let payload = format!(
			"// FILE: small.txt\nx\n// FILE: large.txt\n{}\n",
			"z\n".repeat(2000)
		);
		let mut o = small_ordinary();
		let mut s = landed(&payload, dir.path(), Some(&o));
		fill_budget(&s, &mut o);
		let prior_fp = s.detail().unwrap().fingerprint();
		let selections: Vec<_> = s
			.plan()
			.unwrap()
			.items
			.iter()
			.map(|i| (i.selected, i.overwrite_allowed))
			.collect();
		let worker = s.begin_apply().expect("begin_apply succeeds");
		let res = s.select(1, Some(&o));
		assert!(
			matches!(res, Nav::Refused(ref e) if e.key == "preview_memory_limit")
		);
		assert!(s.is_applying());
		assert!(s.plan().unwrap().import_plan.is_some());
		assert_eq!(
			s.plan()
				.unwrap()
				.items
				.iter()
				.map(|i| (i.selected, i.overwrite_allowed))
				.collect::<Vec<_>>(),
			selections
		);
		assert_eq!(s.detail().unwrap().fingerprint(), prior_fp);

		drop(worker);
		let res = s.plan().unwrap().execute().unwrap().files;
		assert_eq!(res.created_count, 2);
		assert_eq!(
			fs::read_to_string(dir.path().join("large.txt")).unwrap(),
			"z\n".repeat(2000).trim_end_matches('\n')
		);
	}

	/// A remote paste whose in-process worker kept its payload, so the
	/// preview holds a live prepared lease.
	fn landed_remote(
		text: &str,
		ws: &Path,
		ordinary: Option<&Preview>,
	) -> PastePreview {
		let worker = Arc::new(snip_remote::Worker::new(Default::default()));
		let remote = RemotePaste {
			session: crate::remote::RemoteSession::new(
				snip_remote::RemoteHost::in_process(worker),
				snip_remote::RemoteWorkspace {
					id: ws.display().to_string(),
					name: "w".into(),
					path: ws.display().to_string(),
				},
			),
		};
		let dest = remote.session.root.clone();
		let mut s = PastePreview::new(None);
		s.enqueue(PasteRequest::Clipboard {
			text: text.into(),
			dest,
			roots: Vec::new(),
			generation: 1,
			remote: Some(remote),
		})
		.unwrap();
		let (work, cancel) = s.start_job().unwrap().expect("job");
		work.run(&RunOptions::default());
		s.bind_job(1, cancel);
		assert!(matches!(
			s.poll(false, ordinary),
			Polled::Settled {
				landed: Some(Landed::Shown { .. }),
				..
			}
		));
		s
	}

	/// Row navigation while a remote Apply is pending, then a Busy refusal
	/// that restores the lease to Ready, must not panic: the Applying state
	/// may not free charge the restored connection takes back.
	#[test]
	fn navigation_during_a_refused_remote_apply_is_admitted_once() {
		use snip_remote::{ErrorCode, RemoteError};
		let ws = tempfile::tempdir().unwrap();
		// Row 1 is a few hundred bytes larger than row 0: far below the
		// connection's charge, far above the zero slack left at the limit.
		let payload = format!(
			"// FILE: small.txt\nx\n// FILE: large.txt\n{}\n",
			"z\n".repeat(20)
		);
		let mut o = small_ordinary();
		let mut s = landed_remote(&payload, ws.path(), Some(&o));
		let file = s
			.plan()
			.unwrap()
			.import_plan
			.clone()
			.expect("a remote file plan");
		assert!(file.uses_prepared_import(), "the worker kept the payload");
		// Near the limit with the lease Ready: no slack beyond this state.
		fill_budget(&s, &mut o);
		let worker = s.begin_apply().expect("begin_apply succeeds");
		let mut nav = None;
		let refusal = file.refuse_prepared_apply_for_tests(
			RemoteError::Refused {
				code: ErrorCode::Busy,
				message: "the worker is busy".into(),
				reason: None,
				suggestion: None,
			},
			|| nav = Some(s.select(1, Some(&o))),
		);
		assert!(matches!(
			refusal,
			Err(RemoteError::Refused {
				code: ErrorCode::Busy,
				..
			})
		));
		drop(worker);
		s.apply_failed(Msg::new("paste_err_destination", []), Some(&o));
		assert!(!s.is_applying());
		assert!(
			matches!(nav, Some(Nav::Refused(ref e)) if e.key == "preview_memory_limit"),
			"navigation during Apply spent charge the lease holds"
		);
		assert_eq!(
			s.plan().unwrap().error.as_ref().map(|e| e.key),
			Some("paste_err_destination")
		);
	}

	#[test]
	fn refused_fold_reselect_keeps_the_hidden_row_with_its_detail() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let dest = tempfile::tempdir().unwrap();
		git_init(dest.path());
		let payload = snip_core::commits::to_clipboard_text(&CommitsPayload {
			commits: vec![
				CommitRecord {
					message: "c0\n".into(),
					author_name: "A".into(),
					author_email: "a@test.invalid".into(),
					author_date: "2026-09-25T12:00:00+00:00".into(),
					files: vec![CommitFile {
						path: "a.txt".into(),
						old_path: None,
						change: FileChange::Added,
						content: Some("x".into()),
						not_copied: None,
					}],
				},
				CommitRecord {
					message: "c1\n".into(),
					author_name: "A".into(),
					author_email: "a@test.invalid".into(),
					author_date: "2026-09-25T12:00:00+00:00".into(),
					files: vec![CommitFile {
						path: "big.txt".into(),
						old_path: None,
						change: FileChange::Added,
						content: Some("y".repeat(10000)),
						not_copied: None,
					}],
				},
			],
		});
		let mut o = small_ordinary();
		let mut s = landed(&payload, dest.path(), Some(&o));
		fill_budget(&s, &mut o);
		let res = s.toggle_fold(0, Some(&o));
		assert!(
			matches!(res, Folded::Reselected(Nav::Refused(ref e)) if e.key == "preview_memory_limit")
		);
		assert!(s.plan().unwrap().collapsed_commits.contains(&0));
		assert_eq!(s.plan().unwrap().selected_item_idx, 0);
		assert_eq!(s.detail().and_then(|d| d.path.as_deref()), Some("a.txt"));

		let res2 = s.toggle_fold(0, Some(&o));
		assert!(matches!(res2, Folded::Kept));
	}

	/// A refused landing of new repos rolls the candidates back, so the same
	/// repo set must be tried again once there is room (#215).
	#[test]
	fn refused_candidates_are_retried_with_the_same_repo_set() {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		let repo = ws.join("repo01");
		std::fs::create_dir_all(&repo).unwrap();
		let mut o = small_ordinary();
		let mut s = landed("// FILE: repo01/a.txt\nbody\n", &ws, Some(&o));
		assert!(!s.plan().unwrap().prefix_choices.is_empty());
		let count = |s: &PastePreview| {
			s.plan().unwrap().prefix_choices[0].candidates.len()
		};
		let before = count(&s);
		fill_budget(&s, &mut o);
		let retained = s.plan().unwrap().retained_bytes();
		let roots = [ws.clone(), repo.clone()];
		let err = s.refresh_candidates(&roots, Some(&o)).unwrap_err();
		assert_eq!(err.key, "preview_memory_limit");
		assert_eq!(count(&s), before, "the refused candidates are gone");
		assert_eq!(
			s.plan().unwrap().retained_bytes(),
			retained,
			"a refusal restores the admitted plan, capacity included"
		);
		// Enter with unresolved mappings records an error against the same
		// full budget; it must not trip the release assertion.
		s.record_error(
			Msg {
				key: "paste_mapping_unresolved",
				args: vec!["x".repeat(40)],
				key_args: Vec::new(),
				inner: None,
				list: None,
			},
			Some(&o),
		);
		// Room again: the same set is still news and is admitted.
		let small = small_ordinary();
		s.admit_ordinary(Some(&small)).unwrap();
		assert!(s.refresh_candidates(&roots, Some(&small)).unwrap());
		assert!(count(&s) > before);
	}

	/// Thousands of prefixes x hundreds of discovered repos would need
	/// gigabytes of candidates. The refresh projects the size first and
	/// refuses without allocating (#215).
	#[test]
	fn refresh_with_many_prefixes_refuses_before_allocating() {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		let long = "r".repeat(80);
		let roots: Vec<PathBuf> = (0..256)
			.map(|i| {
				let d = ws.join(format!("{long}{i:03}"));
				std::fs::create_dir_all(&d).unwrap();
				d
			})
			.collect();
		let mut payload = String::new();
		for i in 0..5000 {
			payload.push_str(&format!("// FILE: p{i}/a.txt\nx\n"));
		}
		let o = small_ordinary();
		let mut s = landed(&payload, &ws, Some(&o));
		assert_eq!(s.plan().unwrap().prefix_choices.len(), 5000);
		let retained = s.plan().unwrap().retained_bytes();
		let ptrs: Vec<*const PathBuf> = s
			.plan()
			.unwrap()
			.prefix_choices
			.iter()
			.map(|c| c.candidates.as_ptr())
			.collect();
		crate::paste::GROWTH_CHOICES_VISITED.with(|c| c.set(0));
		let started = std::time::Instant::now();
		let err = s.refresh_candidates(&roots, Some(&o)).unwrap_err();
		let took = started.elapsed();
		assert_eq!(err.key, "preview_memory_limit");
		assert_eq!(s.plan().unwrap().retained_bytes(), retained);
		// Nothing was rebuilt: every choice still owns its allocation.
		let after: Vec<*const PathBuf> = s
			.plan()
			.unwrap()
			.prefix_choices
			.iter()
			.map(|c| c.candidates.as_ptr())
			.collect();
		assert_eq!(ptrs, after, "a candidate list was replaced");
		// The projection stops at the budget: it looks at a few choices,
		// not all 5000 (about 700 when written).
		let visited = crate::paste::GROWTH_CHOICES_VISITED.with(|c| c.get());
		assert!(
			visited < 2500,
			"the projection visited {visited} of 5000 choices"
		);
		// Backstop only; the old code needed about 23 s.
		let limit = std::time::Duration::from_secs(
			10 * std::env::var("SNIP_E2E_TIMEOUT_SCALE")
				.ok()
				.and_then(|v| v.parse::<u64>().ok())
				.unwrap_or(1),
		);
		assert!(took < limit, "the refusal took {took:?}");
		// Still news: a smaller set is admitted afterwards.
		assert!(s.refresh_candidates(&roots[..1], Some(&o)).unwrap());
	}

	#[test]
	fn refresh_adds_new_repos_when_they_fit() {
		let tmp = tempfile::tempdir().unwrap();
		let ws = tmp.path().join("ws");
		let repos: Vec<PathBuf> = (0..3)
			.map(|i| {
				let d = ws.join(format!("repo{i}"));
				std::fs::create_dir_all(&d).unwrap();
				d
			})
			.collect();
		let o = small_ordinary();
		let mut s = landed("// FILE: repo0/a.txt\nbody\n", &ws, Some(&o));
		let before = s.plan().unwrap().prefix_choices[0].candidates.len();
		let retained = s.plan().unwrap().retained_bytes();
		assert!(s.refresh_candidates(&repos, Some(&o)).unwrap());
		let after = s.plan().unwrap().prefix_choices[0].candidates.len();
		assert!(after > before);
		assert!(s.plan().unwrap().retained_bytes() > retained);
		// Same set again: nothing to add.
		assert!(!s.refresh_candidates(&repos, Some(&o)).unwrap());
	}

	#[test]
	fn landing_over_budget_drops_the_candidate_and_reports_closed() {
		let mut s = PastePreview::new(None);
		let mut o = small_ordinary();
		s.admit_ordinary(Some(&o)).unwrap();
		let used = {
			let st = lock_pending(&s.pending);
			st.total_with_ui(
				st.ui_bytes,
				st.apply_allowance,
				st.ui_raw.as_ref(),
				st.ui_commit.as_ref(),
			)
		};

		let dir = tempfile::tempdir().unwrap();
		let text = "// FILE: small.txt\ncontent\n";
		let req = PasteRequest::Clipboard {
			text: text.into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 1,
			remote: None,
		};
		let req_bytes = req.bytes();
		let headroom =
			req_bytes + std::mem::size_of::<crate::paste::PasteInput>() + 64;
		o.source = PreviewSource::CommitFile {
			sha: String::with_capacity(
				MAX_RETAINED_PREVIEW_BYTES.saturating_sub(used + headroom),
			),
		};
		s.admit_ordinary(Some(&o)).unwrap();

		s.enqueue(req).unwrap();
		let (work, cancel) = s.start_job().unwrap().expect("job");
		work.run(&RunOptions::default());
		s.bind_job(1, cancel);
		let polled = s.poll(false, Some(&o));
		assert!(matches!(
			polled,
			Polled::Settled {
				landed: Some(Landed::Refused {
					ref err,
					closed: true,
				}),
				..
			} if err.key == "preview_memory_limit"
		));
		assert!(s.plan().is_none());
		assert!(s.detail().is_none());
	}

	#[test]
	fn refused_landing_with_existing_plan_rolls_back_and_closes_when_readmission_fails(
	) {
		let dir = tempfile::tempdir().unwrap();
		let mut s = PastePreview::new(None);
		let o = small_ordinary();
		s.admit_ordinary(Some(&o)).unwrap();

		let text1 = "// FILE: initial.txt\nhello world\n";
		s.enqueue(PasteRequest::Clipboard {
			text: text1.into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 1,
			remote: None,
		})
		.unwrap();
		let (work1, cancel1) = s.start_job().unwrap().expect("job 1");
		work1.run(&RunOptions::default());
		s.bind_job(1, cancel1);
		let polled1 = s.poll(false, Some(&o));
		assert!(matches!(
			polled1,
			Polled::Settled {
				landed: Some(Landed::Shown { .. }),
				..
			}
		));
		assert!(s.plan().is_some());
		assert!(s.detail().is_some());

		let text2 = "// FILE: next.txt\nmore content\n";
		let req2 = PasteRequest::Clipboard {
			text: text2.into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 2,
			remote: None,
		};
		s.enqueue(req2).unwrap();
		let (work2, cancel2) = s.start_job().unwrap().expect("job 2");
		work2.run(&RunOptions::default());
		s.bind_job(2, cancel2);

		let empty_s = PastePreview::new(None);
		let base_used = {
			let st = lock_pending(&empty_s.pending);
			st.total_with_ui(
				st.ui_bytes,
				st.apply_allowance,
				st.ui_raw.as_ref(),
				st.ui_commit.as_ref(),
			)
		};
		// 以捏造（從未 admit）的狀態觸發防禦性 rollback 分支；
		// `base_used + 500` 的餘裕確保 release() 維持在其不變量之內。
		let o_bloated = Preview::new(
			PreviewSource::CommitFile {
				sha: String::with_capacity(
					MAX_RETAINED_PREVIEW_BYTES.saturating_sub(base_used + 500),
				),
			},
			Some("bloated.txt".into()),
			String::new(),
			false,
			Language::Plain,
		);

		let polled2 = s.poll(false, Some(&o_bloated));
		assert!(matches!(
			polled2,
			Polled::Settled {
				landed: Some(Landed::Refused {
					ref err,
					closed: true,
				}),
				..
			} if err.key == "preview_memory_limit"
		));
		assert!(s.plan().is_none());
		assert!(s.detail().is_none());
		assert!(s.admit_ordinary(Some(&o_bloated)).is_ok());
	}

	#[test]
	fn stale_result_after_invalidate_never_lands() {
		let dir = tempfile::tempdir().unwrap();
		let mut s = PastePreview::new(None);
		s.enqueue(PasteRequest::Clipboard {
			text: "// FILE: a.txt\nbody\n".into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 1,
			remote: None,
		})
		.unwrap();
		let (work, cancel) = s.start_job().unwrap().expect("job");
		s.bind_job(1, cancel);
		s.invalidate_job();
		work.run(&RunOptions::default());
		let polled = s.poll(false, None);
		assert!(matches!(
			polled,
			Polled::Settled {
				discarded: true,
				landed: None,
			}
		));
		assert!(s.plan().is_none());
		assert!(!s.is_loading());
		assert!(!s.has_background_work());
	}

	#[test]
	fn record_error_over_budget_keeps_only_the_budget_key() {
		let dir = tempfile::tempdir().unwrap();
		let mut o = small_ordinary();
		let mut s = landed("// FILE: a.txt\nbody\n", dir.path(), Some(&o));
		fill_budget(&s, &mut o);
		s.record_error(
			Msg::new("paste_err_plan", ["e".repeat(1000)]),
			Some(&o),
		);
		assert_eq!(
			s.plan().unwrap().error.as_ref().map(|e| e.key),
			Some("preview_memory_limit")
		);

		let mut s2 = landed("// FILE: a.txt\nbody\n", dir.path(), None);
		s2.record_error(Msg::new("paste_err_plan", ["e".into()]), None);
		assert_eq!(
			s2.plan().unwrap().error.as_ref().map(|e| e.key),
			Some("paste_err_plan")
		);
	}

	#[test]
	fn remap_invalid_prefix_keeps_a_disarmed_shell() {
		let ws = tempfile::tempdir().unwrap();
		fs::create_dir(ws.path().join("repo-a")).unwrap();
		fs::create_dir(ws.path().join("repo-b")).unwrap();
		let text = "// FILE: repo-a/x.txt\na\n// FILE: repo-b/y.txt\nb\n";
		let mut s = landed(text, ws.path(), None);
		let res = s.remap("nope".into(), None, None);
		assert!(
			matches!(res, Remapped::Invalid(ref err) if err.key == "mapping_unknown_prefix")
		);
		assert!(s.plan().is_some());
		assert!(s.plan().unwrap().items.is_empty());
		assert!(!s.plan().unwrap().executable());
		assert!(s.detail().is_none());

		let res2 = s.remap("repo-a".into(), None, None);
		assert!(matches!(
			res2,
			Remapped::Ready(PasteRequest::Remap(Remap::Prefix {
				keep: true,
				..
			}))
		));
	}

	#[test]
	fn adjusting_paths_disarms_the_old_plan_until_the_replan_lands() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("sub");
		fs::create_dir(&dest).unwrap();
		let mut s = landed(
			"// clipcode-root: mono\n// FILE: sub/a.ts\nx\n",
			&dest,
			None,
		);
		let paths = |s: &PastePreview| -> Vec<String> {
			s.plan()
				.unwrap()
				.items
				.iter()
				.map(|i| i.path.clone())
				.collect()
		};
		assert_eq!(paths(&s), ["sub/a.ts"]);

		let Remapped::Ready(req) = s.adjust_paths(true, None) else {
			panic!("the hint is there, so the choice is accepted");
		};
		assert!(matches!(req, PasteRequest::Remap(Remap::AdjustPaths(true))));
		// The plan made without the adjustment cannot be applied meanwhile.
		assert!(s.plan().unwrap().items.is_empty());
		assert!(!s.plan().unwrap().executable());
		assert_eq!(s.begin_apply().err(), Some(ApplyRefused::NotExecutable));

		s.enqueue(req).unwrap();
		let (work, cancel) = s.start_job().unwrap().expect("job");
		work.run(&RunOptions::default());
		s.bind_job(2, cancel);
		assert!(matches!(
			s.poll(false, None),
			Polled::Settled {
				landed: Some(Landed::Shown {
					remap: Some(Remap::AdjustPaths(true)),
					retargeted: false,
				}),
				..
			}
		));
		assert_eq!(paths(&s), ["a.ts"]);
		assert!(s.plan().unwrap().executable());
	}

	#[test]
	fn begin_apply_refuses_until_the_plan_is_executable() {
		let mut s = PastePreview::new(None);
		assert_eq!(s.begin_apply().err(), Some(ApplyRefused::NoPlan));

		let dir = tempfile::tempdir().unwrap();
		s.enqueue(PasteRequest::Clipboard {
			text: "// FILE: a.txt\nbody\n".into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 1,
			remote: None,
		})
		.unwrap();
		assert_eq!(s.begin_apply().err(), Some(ApplyRefused::Loading));

		let ws = tempfile::tempdir().unwrap();
		fs::create_dir(ws.path().join("repo-a")).unwrap();
		fs::create_dir(ws.path().join("repo-b")).unwrap();
		let text = "// FILE: repo-a/x.txt\na\n// FILE: repo-b/y.txt\nb\n";
		let mut s_multi = landed(text, ws.path(), None);
		assert_eq!(
			s_multi.begin_apply().err(),
			Some(ApplyRefused::MappingRequired)
		);

		let mut s_ok = landed("// FILE: a.txt\nbody\n", dir.path(), None);
		let worker = s_ok.begin_apply();
		assert!(worker.is_ok());
		assert_eq!(s_ok.begin_apply().err(), Some(ApplyRefused::Busy));
		drop(worker);

		s_ok.apply_failed(Msg::new("stale_created", ["x".into()]), None);
		assert!(!s_ok.is_applying());
		assert_eq!(
			s_ok.plan().unwrap().error.as_ref().map(|e| e.key),
			Some("stale_created")
		);
	}

	#[test]
	fn ordinary_admission_counts_the_open_paste_state() {
		let dir = tempfile::tempdir().unwrap();
		let s = landed("// FILE: a.txt\nbody\n", dir.path(), None);
		let mut o = small_ordinary();
		o.source = PreviewSource::CommitFile {
			sha: String::with_capacity(MAX_RETAINED_PREVIEW_BYTES),
		};
		assert!(s.admit_ordinary(Some(&o)).is_err());
		assert!(s.admit_ordinary(None).is_ok());
	}

	#[test]
	fn paste_log_collapse_and_restore() {
		// Open then close restores the visible log, exactly once.
		let mut s = PastePreview::new(None);
		let mut vis = true;
		assert!(s.collapse_log(&mut vis));
		assert!(!vis);
		// A re-paste over the open preview keeps the first saved state.
		assert!(!s.collapse_log(&mut vis));
		assert!(s.restore_log(&mut vis));
		assert!(vis);
		vis = false;
		assert!(!s.restore_log(&mut vis));
		assert!(!vis, "must not restore twice");
		assert!(s.log_before.is_none());

		// A log that was already hidden stays hidden.
		let mut s = PastePreview::new(None);
		let mut vis = false;
		assert!(!s.collapse_log(&mut vis));
		assert!(!s.restore_log(&mut vis));
		assert!(!vis);

		// A manual toggle (toggle_log clears the saved state) wins.
		let mut s = PastePreview::new(None);
		let mut vis = true;
		s.collapse_log(&mut vis);
		s.forget_log_restore();
		assert!(!s.restore_log(&mut vis));
		assert!(!vis);

		// Reopened by a path that does not clear the saved state: no flip.
		let mut s = PastePreview::new(None);
		let mut vis = true;
		s.collapse_log(&mut vis);
		vis = true;
		assert!(!s.restore_log(&mut vis));
		assert!(vis);
		assert!(s.log_before.is_none());
	}
}
