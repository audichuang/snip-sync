//! Owned work for one native workbench process.
//!
//! Tracks background jobs until their futures finish, cancels read-only Git
//! work that already carries a [`CancelToken`], and refuses to call the
//! session finished while a job, a Git child, or a leaked budget slot remains.
//! Dropping a GPUI [`Task`] does not interrupt a blocking poll, so tasks stay
//! here until the finish flag is set by the future itself.
//!
//! Quit is deferred. [`crate::WorkbenchModel::begin_quit`] never calls
//! `cx.quit`. That call exists only after a drain reports no owned jobs and
//! no Git children.
//!
//! Export preparation receives the copy job's token:
//! `transfer::plan_export_with`, `ExportPlan::revalidate_with`, and
//! `plan_commit_export_exact_with`. `Git::open_with` is used for every open
//! that sits in front of a reader which already takes that token.
//!
//! Native change listing and preview pass the job's options through nested
//! metadata, revision and cat-file reads (`list_changed_paths_with` and
//! `git_preview_with`). Remaining blocking filesystem reads are drained:
//! - `browser::file_preview` from `read_preview` in `main.rs`, for a project
//!   tree file and for `SourceKind::File`, reads with bounded `std::fs` I/O.
//! - Working-file content inside `read_changed_file_with` checks cancellation
//!   around its bounded filesystem read, but cannot interrupt the OS read.
//!
//! Paste-preview planning is read-only and receives the paste job's token:
//! `PastePreviewPlan::build_from_clipboard_text_with` and
//! `rebuild_file_plan_with` reach `transfer::plan_import_with` and
//! `CommitReplayPreview::capture_with`. A new paste, a mapping change,
//! Cancel, Escape and a workspace close all cancel the job in flight.
//!
//! Writes are the opposite:
//! - Confirmed paste/replay (`PastePreviewPlan::execute`, including
//!   `Git::open` in `paste.rs` `execute_commit`) must not be cancelled
//!   mid-write. Close, open, and quit are refused until that job finishes.
//!
//! `DirectoryScan` / `ScanBudget::cancel` already accepts a token; discovery
//! passes it. Project-tree reads have no Git child, so the Git counters never
//! see them: `submit_tree_io` in `main.rs` runs its worker through
//! [`crate::WorkbenchModel::spawn_owned`], and a result that arrives after a
//! close or reopen is dropped by the lifecycle generation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::Task;
use snip_core::gitrun::CancelToken;

/// Longer than `gitrun`'s 5s cleanup grace, so a cancelled child can be
/// reaped before the session gives up. This is not a test delay.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
	/// Read whose [`snip_core::gitrun::RunOptions`] receives this job's token.
	CancellableRead,
	/// Read or export preparation whose core API has no token. Drain it.
	UncancellableRead,
	/// Confirmed paste or replay. Never cancelled.
	Mutating,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Intent {
	Quit,
	CloseWorkspace,
	OpenWorkspace(std::path::PathBuf),
}

impl Intent {
	pub fn name(&self) -> &'static str {
		match self {
			Self::Quit => "quit",
			Self::CloseWorkspace => "close-workspace",
			Self::OpenWorkspace(_) => "open-workspace",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitLoad {
	pub in_flight: usize,
	pub queued: usize,
	pub leaked: usize,
}

impl GitLoad {
	pub fn current() -> Self {
		Self {
			in_flight: snip_core::gitrun::in_flight(),
			queued: snip_core::gitrun::queued(),
			leaked: snip_core::gitrun::leaked_slots(),
		}
	}

	pub fn idle() -> Self {
		Self {
			in_flight: 0,
			queued: 0,
			leaked: 0,
		}
	}

	pub fn is_clear(self) -> bool {
		self.in_flight == 0 && self.queued == 0 && self.leaked == 0
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
	/// A confirmed write is in flight. Do not cancel it and do not quit.
	RefusedApplying,
	/// A drain is already running. Quit upgrades the intent in place.
	Busy,
	/// Drain started. The handler must not quit on this return.
	Accepted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
	Idle,
	Draining,
	Ready(Intent),
	Failed {
		intent: Intent,
		reason: &'static str,
	},
}

/// Set when the owned future finishes or is dropped after that poll returns.
pub struct FinishFlag(Arc<AtomicBool>);

impl Drop for FinishFlag {
	fn drop(&mut self) {
		self.0.store(true, Ordering::SeqCst);
	}
}

struct Job {
	id: u64,
	kind: JobKind,
	cancel: Option<CancelToken>,
	finished: Arc<AtomicBool>,
	task: Option<Task<()>>,
}

enum Phase {
	Idle,
	Draining { intent: Intent, started: Instant },
}

pub struct Lifecycle {
	generation: u64,
	next_id: u64,
	jobs: Vec<Job>,
	phase: Phase,
}

impl Lifecycle {
	pub fn new(generation: u64) -> Self {
		Self {
			generation,
			next_id: 1,
			jobs: Vec::new(),
			phase: Phase::Idle,
		}
	}

	pub fn generation(&self) -> u64 {
		self.generation
	}

	pub fn is_draining(&self) -> bool {
		matches!(self.phase, Phase::Draining { .. })
	}

	pub fn intent_name(&self) -> &'static str {
		match &self.phase {
			Phase::Draining { intent, .. } => intent.name(),
			Phase::Idle => "none",
		}
	}

	pub fn register(
		&mut self,
		kind: JobKind,
		cancel: Option<CancelToken>,
	) -> (u64, FinishFlag) {
		let cancel = match kind {
			JobKind::CancellableRead => cancel,
			JobKind::UncancellableRead | JobKind::Mutating => None,
		};
		let finished = Arc::new(AtomicBool::new(false));
		let id = self.next_id;
		self.next_id = self.next_id.wrapping_add(1);
		self.jobs.push(Job {
			id,
			kind,
			cancel,
			finished: Arc::clone(&finished),
			task: None,
		});
		(id, FinishFlag(finished))
	}

	pub fn attach(&mut self, id: u64, task: Task<()>) {
		if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
			job.task = Some(task);
		}
	}

	pub fn has_mutating(&self) -> bool {
		self.jobs.iter().any(|job| {
			job.kind == JobKind::Mutating
				&& !job.finished.load(Ordering::SeqCst)
		})
	}

	pub fn unfinished(&mut self) -> usize {
		self.reap();
		self.jobs.len()
	}

	/// Jobs whose futures have not finished. Does not drop completed tasks.
	pub fn live_jobs(&self) -> usize {
		self.jobs
			.iter()
			.filter(|job| !job.finished.load(Ordering::SeqCst))
			.count()
	}

	pub fn request(&mut self, intent: Intent, now: Instant) -> Request {
		if self.has_mutating() {
			return Request::RefusedApplying;
		}
		if let Phase::Draining {
			intent: current, ..
		} = &mut self.phase
		{
			if matches!(intent, Intent::Quit) {
				*current = Intent::Quit;
			}
			return Request::Busy;
		}
		self.cancel_cancellable();
		self.generation = self.generation.wrapping_add(1);
		self.phase = Phase::Draining {
			intent,
			started: now,
		};
		Request::Accepted
	}

	/// Puts a drain back after a ready check raced with a new Git child.
	pub fn resume(&mut self, intent: Intent, now: Instant) {
		if !self.is_draining() {
			self.phase = Phase::Draining {
				intent,
				started: now,
			};
		}
	}

	pub fn cancel_cancellable(&mut self) {
		for job in &self.jobs {
			if job.kind == JobKind::CancellableRead {
				if let Some(token) = &job.cancel {
					token.cancel();
				}
			}
		}
	}

	pub fn poll_at(&mut self, now: Instant, git: GitLoad) -> Step {
		self.reap();
		let Phase::Draining { intent, started } = &self.phase else {
			return Step::Idle;
		};
		let intent = intent.clone();
		let started = *started;
		let pending = !self.jobs.is_empty();
		if !pending && git.is_clear() {
			self.phase = Phase::Idle;
			return Step::Ready(intent);
		}
		if !pending && git.in_flight == 0 && git.queued == 0 && git.leaked > 0 {
			self.phase = Phase::Idle;
			return Step::Failed {
				intent,
				reason: "leaked",
			};
		}
		if now.saturating_duration_since(started) >= DRAIN_DEADLINE {
			self.phase = Phase::Idle;
			return Step::Failed {
				intent,
				reason: "timeout",
			};
		}
		Step::Draining
	}

	fn reap(&mut self) {
		self.jobs.retain(|job| !job.finished.load(Ordering::SeqCst));
	}
}

impl Drop for Lifecycle {
	fn drop(&mut self) {
		self.cancel_cancellable();
	}
}

pub fn format_line(
	phase: &str,
	intent: &str,
	reason: Option<&str>,
	jobs: usize,
	git: GitLoad,
	generation: u64,
) -> String {
	match reason {
		Some(reason) => format!(
			"phase={phase} intent={intent} reason={reason} jobs={jobs} inflight={} queued={} leaked={} generation={generation}",
			git.in_flight, git.queued, git.leaked
		),
		None => format!(
			"phase={phase} intent={intent} jobs={jobs} inflight={} queued={} leaked={} generation={generation}",
			git.in_flight, git.queued, git.leaked
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn fresh() -> Lifecycle {
		Lifecycle::new(1)
	}

	#[test]
	fn quit_is_not_ready_until_owned_work_and_git_children_are_gone() {
		let mut life = fresh();
		let now = Instant::now();
		let token = CancelToken::new();
		let flag = life
			.register(JobKind::CancellableRead, Some(token.clone()))
			.1;
		assert_eq!(life.request(Intent::Quit, now), Request::Accepted);
		assert!(token.is_cancelled());
		assert_eq!(life.generation(), 2);
		let busy = GitLoad {
			in_flight: 1,
			queued: 0,
			leaked: 0,
		};
		assert_eq!(life.poll_at(now, busy), Step::Draining);
		drop(flag);
		assert_eq!(
			life.poll_at(now, busy),
			Step::Draining,
			"a finished future with a live Git child is not drained"
		);
		assert_eq!(
			life.poll_at(now, GitLoad::idle()),
			Step::Ready(Intent::Quit)
		);
		assert_eq!(life.poll_at(now, GitLoad::idle()), Step::Idle);
	}

	#[test]
	fn mutating_work_refuses_close_without_cancelling_reads() {
		let mut life = fresh();
		let now = Instant::now();
		let read = CancelToken::new();
		let _read = life
			.register(JobKind::CancellableRead, Some(read.clone()))
			.1;
		let _write = life.register(JobKind::Mutating, None).1;
		assert_eq!(life.request(Intent::Quit, now), Request::RefusedApplying);
		assert!(!read.is_cancelled());
		assert_eq!(life.generation(), 1);
		assert!(!life.is_draining());
	}

	#[test]
	fn uncancellable_read_is_drained_and_not_reported_cancelled() {
		let mut life = fresh();
		let now = Instant::now();
		let token = CancelToken::new();
		let flag = life
			.register(JobKind::UncancellableRead, Some(token.clone()))
			.1;
		assert_eq!(
			life.request(Intent::CloseWorkspace, now),
			Request::Accepted
		);
		assert!(
			!token.is_cancelled(),
			"a legacy API token must not be described as cancelled"
		);
		assert_eq!(life.poll_at(now, GitLoad::idle()), Step::Draining);
		drop(flag);
		assert_eq!(
			life.poll_at(now, GitLoad::idle()),
			Step::Ready(Intent::CloseWorkspace)
		);
	}

	#[test]
	fn leaked_slot_and_timeout_are_not_drained() {
		let mut life = fresh();
		let now = Instant::now();
		assert_eq!(life.request(Intent::Quit, now), Request::Accepted);
		let leaked = GitLoad {
			in_flight: 0,
			queued: 0,
			leaked: 1,
		};
		assert_eq!(
			life.poll_at(now, leaked),
			Step::Failed {
				intent: Intent::Quit,
				reason: "leaked",
			}
		);

		let mut life = fresh();
		let _flag = life.register(JobKind::CancellableRead, None).1;
		assert_eq!(life.request(Intent::Quit, now), Request::Accepted);
		let later = now + DRAIN_DEADLINE;
		assert_eq!(
			life.poll_at(
				later,
				GitLoad {
					in_flight: 1,
					..GitLoad::idle()
				}
			),
			Step::Failed {
				intent: Intent::Quit,
				reason: "timeout",
			}
		);
	}

	#[test]
	fn quit_during_close_upgrades_the_intent_without_a_second_generation() {
		let mut life = fresh();
		let now = Instant::now();
		let _flag = life.register(JobKind::CancellableRead, None).1;
		assert_eq!(
			life.request(Intent::CloseWorkspace, now),
			Request::Accepted
		);
		assert_eq!(life.generation(), 2);
		assert_eq!(life.request(Intent::Quit, now), Request::Busy);
		assert_eq!(life.generation(), 2);
		assert_eq!(life.intent_name(), "quit");
	}

	#[test]
	fn drained_line_requires_zero_jobs_and_children() {
		let line = format_line("drained", "quit", None, 0, GitLoad::idle(), 3);
		assert_eq!(
			line,
			"phase=drained intent=quit jobs=0 inflight=0 queued=0 leaked=0 generation=3"
		);
		let blocked = format_line(
			"failed",
			"quit",
			Some("timeout"),
			1,
			GitLoad {
				in_flight: 1,
				queued: 0,
				leaked: 0,
			},
			3,
		);
		assert!(!blocked.contains("phase=drained"));
		assert!(blocked.contains("reason=timeout"));
	}
}
