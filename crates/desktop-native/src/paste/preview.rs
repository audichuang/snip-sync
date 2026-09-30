//! Paste preview session.
//!
//! Owns the background job slot, 32 MiB shared budget admission and rollback,
//! the (selection, detail) pair, fold state, and auto-collapsed Git Log state
//! for the paste preview workflow (spec 3.2 / 4.3 / 5.1).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use snip_core::gitrun::CancelToken;

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
	Shown { remap: Option<(String, bool)> },
	Refused { err: Msg, closed: bool },
	WorkerLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav {
	Ignored,
	Moved(usize),
	Refused(Msg),
}

#[derive(Debug)]
pub enum Folded {
	NoPlan,
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
		}
	}

	pub fn plan(&self) -> Option<&PastePreviewPlan> {
		self.plan.as_ref()
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
		let built = outcome.result.and_then(|plan| {
			let detail = plan.detail_preview();
			pending.admit_ui(ordinary, Some(&plan), detail.as_ref())?;
			Ok((plan, detail))
		});
		match built {
			Ok((plan, detail)) => {
				self.plan = Some(plan);
				self.detail = detail;
				self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
				Landed::Shown { remap }
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

	pub fn remap(
		&mut self,
		prefix: String,
		destination: Option<PathBuf>,
		ordinary: Option<&Preview>,
	) -> Remapped {
		let keep = destination.is_none();
		self.invalidate_job();
		let pool = self.pending.clone();
		let mut pending = lock_pending(&pool);
		let Some(plan) = self.plan.as_mut() else {
			return Remapped::NoPlan;
		};
		plan.clear_file_plan();
		plan.error = None;
		self.detail = None;
		let chosen = match destination {
			Some(dest) => plan.choose_prefix_destination(&prefix, &dest),
			None => plan.choose_keep_relative(&prefix),
		};
		if let Err(err) = chosen {
			release(&mut pending, ordinary, Some(plan), None);
			return Remapped::Invalid(err);
		}
		if let Err(err) = pending.admit_ui(ordinary, Some(plan), None) {
			self.plan = None;
			release(&mut pending, ordinary, None, None);
			return Remapped::Dropped(err);
		}
		Remapped::Ready(PasteRequest::Remap { prefix, keep })
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
				self.detail = detail;
				self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
				Nav::Moved(idx)
			}
			Err(e) => Nav::Refused(e),
		}
	}

	pub fn step(&mut self, forward: bool, ordinary: Option<&Preview>) -> Nav {
		let Some(plan) = self.plan.as_ref() else {
			return Nav::Ignored;
		};
		let target = plan.step_target(forward);
		self.select(target, ordinary)
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

	/// Grows `ordinary` so the admitted state sits exactly at the 32 MiB cap.
	fn fill_budget(s: &PastePreview, ordinary: &mut Preview) {
		let used = {
			let st = lock_pending(&s.pending);
			st.total_with_ui(
				st.ui_bytes,
				st.apply_allowance,
				st.ui_raw.as_ref(),
			)
		};
		ordinary.source = PreviewSource::CommitFile {
			sha: String::with_capacity(MAX_RETAINED_PREVIEW_BYTES - used),
		};
		s.admit_ordinary(Some(ordinary)).unwrap();
	}

	fn git_init(dir: &Path) {
		let out = std::process::Command::new("git")
			.arg("init")
			.arg("-q")
			.arg(dir)
			.status()
			.expect("git init");
		assert!(out.success());
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
			)
		};
		o.source = PreviewSource::CommitFile {
			sha: String::with_capacity(
				MAX_RETAINED_PREVIEW_BYTES.saturating_sub(used + 1024),
			),
		};
		s.admit_ordinary(Some(&o)).unwrap();

		let dir = tempfile::tempdir().unwrap();
		let text = "// FILE: small.txt\ncontent\n";
		// Enqueue may succeed or fail depending on request bytes vs remaining 1024 bytes.
		match s.enqueue(PasteRequest::Clipboard {
			text: text.into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 1,
		}) {
			Ok(()) => {
				let job = s.start_job().unwrap();
				if let Some((work, cancel)) = job {
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
			}
			Err(err) => {
				assert_eq!(err.key, "preview_memory_limit");
				assert!(s.plan().is_none());
			}
		}
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
			Remapped::Ready(PasteRequest::Remap { keep: true, .. })
		));
	}

	#[test]
	fn begin_apply_refuses_until_the_plan_is_executable() {
		let mut s = PastePreview::new(None);
		assert_eq!(s.begin_apply().unwrap_err(), ApplyRefused::NoPlan);

		let dir = tempfile::tempdir().unwrap();
		s.enqueue(PasteRequest::Clipboard {
			text: "// FILE: a.txt\nbody\n".into(),
			dest: dir.path().into(),
			roots: Vec::new(),
			generation: 1,
		})
		.unwrap();
		assert_eq!(s.begin_apply().unwrap_err(), ApplyRefused::Loading);

		let ws = tempfile::tempdir().unwrap();
		fs::create_dir(ws.path().join("repo-a")).unwrap();
		fs::create_dir(ws.path().join("repo-b")).unwrap();
		let text = "// FILE: repo-a/x.txt\na\n// FILE: repo-b/y.txt\nb\n";
		let mut s_multi = landed(text, ws.path(), None);
		assert_eq!(
			s_multi.begin_apply().unwrap_err(),
			ApplyRefused::MappingRequired
		);

		let mut s_ok = landed("// FILE: a.txt\nbody\n", dir.path(), None);
		let worker = s_ok.begin_apply();
		assert!(worker.is_ok());
		assert_eq!(s_ok.begin_apply().unwrap_err(), ApplyRefused::Busy);
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
	}
}
