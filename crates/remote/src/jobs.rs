//! Git view and repo scan worker job machinery: admission, heartbeat,
//! deadline, cancellation.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use snip_core::gitrun::CancelToken;

use crate::proto::{chunks, write_frame, ErrorCode, Response, CHUNK_BYTES};

pub(crate) const MAX_GIT_JOBS: usize = 2; // running GitView/ScanRepos at once
pub(crate) const MAX_SCAN_JOBS: usize = 1; // of those, scans
pub(crate) const MAX_JOBS_WAITING: usize = 16; // more waiting -> Busy
pub(crate) const ADMIT_WAIT: Duration = Duration::from_secs(10);
pub(crate) const VIEW_DEADLINE: Duration = Duration::from_secs(60);
pub(crate) const SCAN_DEADLINE: Duration = Duration::from_secs(75);
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(1);
/// A paste Apply: writes are not cancelled midway, so this only stops a
/// worker that hangs. A master waits longer ([`crate::client`]).
pub(crate) const APPLY_DEADLINE: Duration = Duration::from_secs(30 * 60);

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobKind {
	View,
	Scan,
}

struct JobsInner {
	running: usize,
	scans: usize,
	waiting: usize,
	stopped: bool,
	next_id: u64,
	by_workspace: HashMap<String, Vec<(u64, CancelToken)>>,
}

struct JobsShared {
	inner: Mutex<JobsInner>,
	condvar: Condvar,
}

impl JobsShared {
	fn release(&self, workspace: &str, kind: JobKind, id: u64) {
		let mut inner =
			self.inner.lock().unwrap_or_else(PoisonError::into_inner);
		inner.running = inner.running.saturating_sub(1);
		if kind == JobKind::Scan {
			inner.scans = inner.scans.saturating_sub(1);
		}
		if let Some(list) = inner.by_workspace.get_mut(workspace) {
			list.retain(|(job_id, _)| *job_id != id);
			if list.is_empty() {
				inner.by_workspace.remove(workspace);
			}
		}
		self.condvar.notify_all();
	}
}

pub(crate) struct Jobs {
	shared: Arc<JobsShared>,
	max_git_jobs: usize,
	max_scan_jobs: usize,
	max_jobs_waiting: usize,
	admit_wait: Duration,
}

/// RAII: Drop releases the slot, removes the token, notifies waiters.
pub(crate) struct JobGuard {
	shared: Arc<JobsShared>,
	workspace: String,
	kind: JobKind,
	id: u64,
}

impl std::fmt::Debug for JobGuard {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("JobGuard")
			.field("workspace", &self.workspace)
			.field("kind", &self.kind)
			.field("id", &self.id)
			.finish()
	}
}

impl Drop for JobGuard {
	fn drop(&mut self) {
		self.shared.release(&self.workspace, self.kind, self.id);
	}
}

impl Jobs {
	pub fn new() -> Self {
		Self::with_limits(
			MAX_GIT_JOBS,
			MAX_SCAN_JOBS,
			MAX_JOBS_WAITING,
			ADMIT_WAIT,
		)
	}

	pub fn with_limits(
		max_git_jobs: usize,
		max_scan_jobs: usize,
		max_jobs_waiting: usize,
		admit_wait: Duration,
	) -> Self {
		Self {
			shared: Arc::new(JobsShared {
				inner: Mutex::new(JobsInner {
					running: 0,
					scans: 0,
					waiting: 0,
					stopped: false,
					next_id: 1,
					by_workspace: HashMap::new(),
				}),
				condvar: Condvar::new(),
			}),
			max_git_jobs,
			max_scan_jobs,
			max_jobs_waiting,
			admit_wait,
		}
	}

	/// Called INSIDE run_job's op, on the job thread, so the heartbeat covers the wait.
	/// Waits up to ADMIT_WAIT; returns early when `cancel` fires;
	/// Err(Busy) when the queue is full, the wait ran out, or the worker stopped.
	pub fn admit(
		&self,
		workspace: &str,
		kind: JobKind,
		cancel: &CancelToken,
	) -> Result<JobGuard, ErrorCode> {
		let mut inner = self
			.shared
			.inner
			.lock()
			.unwrap_or_else(PoisonError::into_inner);

		if inner.stopped {
			return Err(ErrorCode::Busy);
		}
		if cancel.is_cancelled() {
			return Err(ErrorCode::Cancelled);
		}
		if inner.waiting >= self.max_jobs_waiting {
			return Err(ErrorCode::Busy);
		}

		inner.waiting += 1;
		let deadline = Instant::now() + self.admit_wait;

		loop {
			let can_admit = inner.running < self.max_git_jobs
				&& match kind {
					JobKind::View => true,
					JobKind::Scan => inner.scans < self.max_scan_jobs,
				};

			if can_admit {
				inner.waiting = inner.waiting.saturating_sub(1);
				inner.running += 1;
				if kind == JobKind::Scan {
					inner.scans += 1;
				}
				let id = inner.next_id;
				inner.next_id += 1;
				inner
					.by_workspace
					.entry(workspace.to_string())
					.or_default()
					.push((id, cancel.clone()));

				return Ok(JobGuard {
					shared: self.shared.clone(),
					workspace: workspace.to_string(),
					kind,
					id,
				});
			}

			if inner.stopped {
				inner.waiting = inner.waiting.saturating_sub(1);
				return Err(ErrorCode::Busy);
			}
			if cancel.is_cancelled() {
				inner.waiting = inner.waiting.saturating_sub(1);
				return Err(ErrorCode::Cancelled);
			}

			let now = Instant::now();
			if now >= deadline {
				inner.waiting = inner.waiting.saturating_sub(1);
				return Err(ErrorCode::Busy);
			}

			let wait = (deadline - now).min(Duration::from_millis(50));
			let (new_inner, _) = self
				.shared
				.condvar
				.wait_timeout(inner, wait)
				.unwrap_or_else(PoisonError::into_inner);
			inner = new_inner;
		}
	}

	pub fn cancel_all(&self) {
		let mut inner = self
			.shared
			.inner
			.lock()
			.unwrap_or_else(PoisonError::into_inner);
		inner.stopped = true;
		for list in inner.by_workspace.values() {
			for (_, token) in list {
				token.cancel();
			}
		}
		self.shared.condvar.notify_all();
	}

	pub fn running(&self) -> usize {
		self.shared
			.inner
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.running
	}

	pub fn waiting(&self) -> usize {
		self.shared
			.inner
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.waiting
	}
}

/// Where one request's answer goes. A heartbeat and the answer are separate
/// calls: work runs under heartbeats, and a test fault ([`crate::fault`])
/// acts on the answer alone, never cutting into a heartbeat (#191, #230).
pub(crate) struct Answer<'w, W: Write> {
	out: &'w mut W,
	fault: Option<crate::fault::Fault>,
}

impl<'w, W: Write> Answer<'w, W> {
	pub(crate) fn new(
		out: &'w mut W,
		fault: Option<crate::fault::Fault>,
	) -> Self {
		Self { out, fault }
	}

	/// One `Pending` frame, flushed.
	pub(crate) fn heartbeat(&mut self) -> io::Result<()> {
		write_response(self.out, &Response::Pending)
	}

	/// The request's whole answer (its chunks and `Joined` included),
	/// flushed. Once per request.
	pub(crate) fn reply(&mut self, response: &Response) -> io::Result<()> {
		write_response(self.out, response)
	}

	/// Extra time the work behind this request takes: a `Fault::Delay`.
	pub(crate) fn work_delay(&self) -> Option<Duration> {
		// An exhaustive match: a new `Fault` stops this compiling until it
		// decides what it means here.
		match self.fault.as_ref()? {
			crate::fault::Fault::Delay(d) => Some(*d),
			crate::fault::Fault::PageDelay(_) => None,
		}
	}

	/// Extra time each unit of the work takes: a `Fault::PageDelay`.
	pub(crate) fn page_delay(&self) -> Option<Duration> {
		match self.fault.as_ref()? {
			crate::fault::Fault::PageDelay(d) => Some(*d),
			crate::fault::Fault::Delay(_) => None,
		}
	}
}

/// Sleeps `d` in slices, or until `cancel` fires.
fn pause(d: Duration, cancel: &CancelToken) {
	let end = Instant::now() + d;
	loop {
		let left = end.saturating_duration_since(Instant::now());
		if left.is_zero() || cancel.is_cancelled() {
			return;
		}
		std::thread::sleep(left.min(Duration::from_millis(20)));
	}
}

pub(crate) fn write_response<W: Write>(
	w: &mut W,
	response: &Response,
) -> io::Result<()> {
	// A paste plan or many-file metadata may exceed a frame; join its JSON.
	// So may a reply that lists what happened to every file: an apply that
	// wrote files must reach the master as a result, never as `TooLarge`.
	// Every master that can send a paste request joins chunked replies, so a
	// master that predates the typed failures reassembles these too. `Repos`
	// stays one frame: a scan needs only git views v2, masters before
	// protocol 4 do not know `Joined`, and the scan is bounded anyway.
	if matches!(
		response,
		Response::Imported(_)
			| Response::Replayed(_)
			| Response::ImportPlanned(_)
			| Response::ImportPrepared(_)
			| Response::ReplayPlanned(_)
			| Response::ReplayPrepared(_)
	) {
		let json = serde_json::to_string(response).map_err(io::Error::other)?;
		if json.len() > CHUNK_BYTES {
			for data in chunks(&json) {
				write_frame(
					w,
					&Response::Chunk {
						data: data.to_string(),
					},
				)?;
			}
			return write_frame(w, &Response::Joined);
		}
	}
	// A copy's text goes ahead in chunks; the reply itself carries none.
	let text = match response {
		Response::Copied(out) => Some(out.payload.as_str()),
		Response::CommitsCopied(out) => Some(out.text.as_str()),
		_ => None,
	};
	if let Some(text) = text.filter(|t| t.len() > CHUNK_BYTES) {
		for data in chunks(text) {
			write_frame(
				w,
				&Response::Chunk {
					data: data.to_string(),
				},
			)?;
		}
		let mut reply = response.clone();
		match &mut reply {
			Response::Copied(out) => out.payload.clear(),
			Response::CommitsCopied(out) => out.text.clear(),
			_ => {}
		}
		return write_response(w, &reply);
	}
	match write_frame(w, response) {
		Err(err) if err.kind() == io::ErrorKind::InvalidData => write_frame(
			w,
			&Response::Error {
				code: ErrorCode::TooLarge,
				message: err.to_string(),
				patch: false,
				reason: None,
			},
		),
		other => other,
	}
}

/// Proof that a request is answered under [`run_inline`]: only this module
/// makes one.
pub(crate) struct Covered {
	page_delay: Option<Duration>,
}

impl Covered {
	/// Starts one unit of work: a test's `Fault::PageDelay` passes here.
	pub(crate) fn unit(&self) {
		if let Some(d) = self.page_delay {
			std::thread::sleep(d);
		}
	}
}

/// Answers a request `State::handle` serves. From protocol 2 on the work runs
/// under heartbeats like any job, so a master hears from a live worker within
/// a second; a protocol 1 master does not know `Pending` and gets the answer
/// alone. Like a view, it has `deadline` and answers `Timeout` past it.
pub(crate) fn run_inline<W: Write>(
	answer: &mut Answer<'_, W>,
	negotiated: u32,
	deadline: Duration,
	op: impl FnOnce(Covered) -> Response + Send,
) -> io::Result<()> {
	let covered = Covered {
		page_delay: answer.page_delay(),
	};
	if negotiated < crate::proto::GIT_VIEWS_VERSION {
		if let Some(d) = answer.work_delay() {
			std::thread::sleep(d);
		}
		return answer.reply(&op(covered));
	}
	run_job_with(
		answer,
		HEARTBEAT,
		deadline,
		CancelToken::new(),
		DeadlineReply::Fail,
		|_, _| op(covered),
	)
}

/// Runs `op` on a scoped thread; writes Pending every `HEARTBEAT` until it
/// returns, then the response. A failed write cancels `op`; `deadline`
/// cancels it too and answers Timeout. Returns only after `op` returned
/// (std::thread::scope), so the connection slot is never freed while the
/// job still runs. Converts write_frame InvalidData into TooLarge like serve().
pub(crate) fn run_job<W: Write>(
	w: &mut Answer<'_, W>,
	deadline: Duration,
	cancel: CancelToken,
	op: impl FnOnce(&CancelToken, Instant) -> Response + Send,
) -> io::Result<()> {
	run_job_with(w, HEARTBEAT, deadline, cancel, DeadlineReply::Fail, op)
}

/// What the caller answers once a job runs past its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadlineReply {
	/// The op is cancelled, its response discarded, Timeout answered: right
	/// for reads, where nothing was written.
	Fail,
	/// A write's own response is never discarded: the op is cancelled (a
	/// confirmed write ignores that), Pending keeps the master informed
	/// while the write finishes, and the real response goes out whatever
	/// it says. A master waits longer than this job's deadline
	/// ([`crate::client::APPLY_CALL_LIMIT`]), so a definite Timeout here
	/// would be the only frame that turns a landed write into a reported
	/// failure.
	WriteOutcome,
}

pub(crate) fn run_job_with<W: Write>(
	w: &mut Answer<'_, W>,
	heartbeat: Duration,
	deadline: Duration,
	cancel: CancelToken,
	reply: DeadlineReply,
	op: impl FnOnce(&CancelToken, Instant) -> Response + Send,
) -> io::Result<()> {
	let (tx, rx) = std::sync::mpsc::sync_channel::<Response>(1);
	let start = Instant::now();
	let deadline_instant = start + deadline;
	let write = reply == DeadlineReply::WriteOutcome;
	let delay = w.work_delay();

	std::thread::scope(|s| {
		let op_cancel = cancel.clone();
		s.spawn(move || {
			// A test's delay runs where the work does, under the heartbeats
			// and inside the job's deadline.
			if let Some(d) = delay {
				pause(d, &op_cancel);
			}
			let res = op(&op_cancel, deadline_instant);
			let _ = tx.send(res);
		});

		loop {
			let now = Instant::now();
			if now >= deadline_instant {
				cancel.cancel();
				return past_deadline_reply(w, &rx, heartbeat, write);
			}

			let remaining = deadline_instant - now;
			let wait_time = heartbeat.min(remaining);

			match rx.recv_timeout(wait_time) {
				Ok(response) => {
					return w.reply(&response);
				}
				Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
					if Instant::now() >= deadline_instant {
						cancel.cancel();
						return past_deadline_reply(w, &rx, heartbeat, write);
					}
					if let Err(err) = w.heartbeat() {
						cancel.cancel();
						let _ = rx.recv();
						return Err(err);
					}
				}
				Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
					// op panicked; thread::scope will join and rethrow the panic
					break;
				}
			}
		}
		Ok(())
	})
}

/// The answer once a job is past its deadline. A read's response is
/// discarded and Timeout answered; a write's own response follows, with
/// Pending covering the silence while it finishes.
fn past_deadline_reply<W: Write>(
	w: &mut Answer<'_, W>,
	rx: &std::sync::mpsc::Receiver<Response>,
	heartbeat: Duration,
	write: bool,
) -> io::Result<()> {
	if !write {
		let _ = rx.recv();
		return w.reply(&Response::Error {
			code: ErrorCode::Timeout,
			message: "the job timed out".into(),
			patch: false,
			reason: None,
		});
	}
	loop {
		match rx.recv_timeout(heartbeat) {
			Ok(response) => return w.reply(&response),
			Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
				// The master waits longer than the job's deadline; these
				// heartbeats keep its per-frame idle timeout from firing
				// while the write finishes.
				if let Err(err) = w.heartbeat() {
					let _ = rx.recv();
					return Err(err);
				}
			}
			Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
				// op panicked; thread::scope joins and rethrows.
				return Ok(());
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::proto::{read_frame, Stat};

	fn timeout_scale() -> u32 {
		std::env::var("SNIP_E2E_TIMEOUT_SCALE")
			.ok()
			.and_then(|v| v.parse::<u32>().ok())
			.unwrap_or(1)
			.max(1)
	}

	fn scaled(d: Duration) -> Duration {
		d.saturating_mul(timeout_scale())
	}

	#[test]
	fn admit_respects_the_running_cap_and_releases_on_drop() {
		let jobs =
			Jobs::with_limits(2, 1, 16, scaled(Duration::from_millis(50)));
		let cancel = CancelToken::new();

		let g1 = jobs.admit("ws", JobKind::View, &cancel).unwrap();
		assert_eq!(jobs.running(), 1);

		let g2 = jobs.admit("ws", JobKind::View, &cancel).unwrap();
		assert_eq!(jobs.running(), 2);

		// Cap reached: next admit fails with Busy after admit_wait
		let err = jobs.admit("ws", JobKind::View, &cancel).unwrap_err();
		assert_eq!(err, ErrorCode::Busy);
		assert_eq!(jobs.running(), 2);

		// Drop one guard
		drop(g1);
		assert_eq!(jobs.running(), 1);

		// Now another admit succeeds
		let g3 = jobs.admit("ws", JobKind::View, &cancel).unwrap();
		assert_eq!(jobs.running(), 2);
		drop(g2);
		drop(g3);
		assert_eq!(jobs.running(), 0);
	}

	#[test]
	fn a_scan_waits_for_the_single_scan_slot_while_a_view_runs() {
		let jobs = Arc::new(Jobs::with_limits(
			2,
			1,
			16,
			scaled(Duration::from_millis(300)),
		));
		let cancel = CancelToken::new();

		// Run 1 view and 1 scan (uses both 2 git slots and the 1 scan slot)
		let _v = jobs.admit("ws", JobKind::View, &cancel).unwrap();
		let s = jobs.admit("ws", JobKind::Scan, &cancel).unwrap();

		let jobs_clone = jobs.clone();
		let cancel_clone = cancel.clone();
		let (started_tx, started_rx) = std::sync::mpsc::channel();

		let t = std::thread::spawn(move || {
			started_tx.send(()).unwrap();
			jobs_clone.admit("ws", JobKind::Scan, &cancel_clone)
		});

		started_rx
			.recv_timeout(scaled(Duration::from_secs(1)))
			.expect("thread started");

		// Wait until the thread enters waiting
		let deadline = Instant::now() + scaled(Duration::from_secs(1));
		while jobs.waiting() < 1 && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(5));
		}
		assert_eq!(jobs.running(), 2);
		assert_eq!(jobs.waiting(), 1);

		// Release the scan slot while the view still runs
		drop(s);

		let result = t.join().expect("thread finished cleanly");
		let s2 = result.expect("second scan should be admitted");
		assert_eq!(jobs.running(), 2);
		assert_eq!(jobs.waiting(), 0);
		drop(s2);
	}

	#[test]
	fn admit_answers_busy_after_the_wait_runs_out() {
		let wait_limit = scaled(Duration::from_millis(40));
		let jobs = Jobs::with_limits(1, 1, 16, wait_limit);
		let cancel = CancelToken::new();

		let _g = jobs.admit("ws", JobKind::View, &cancel).unwrap();
		let start = Instant::now();
		let err = jobs.admit("ws", JobKind::View, &cancel).unwrap_err();
		assert_eq!(err, ErrorCode::Busy);
		assert!(start.elapsed() >= scaled(Duration::from_millis(35)));
	}

	#[test]
	fn admit_answers_busy_when_the_queue_is_full() {
		let jobs = Arc::new(Jobs::with_limits(
			1,
			1,
			2,
			scaled(Duration::from_millis(300)),
		));
		let cancel = CancelToken::new();

		let _g = jobs.admit("ws", JobKind::View, &cancel).unwrap();

		// Put 2 waiters into the queue
		let mut threads = Vec::new();
		for _ in 0..2 {
			let j = jobs.clone();
			let c = cancel.clone();
			threads.push(std::thread::spawn(move || {
				let _ = j.admit("ws", JobKind::View, &c);
			}));
		}

		// Wait until waiting == 2
		let deadline = Instant::now() + scaled(Duration::from_secs(1));
		while jobs.waiting() < 2 && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(10));
		}
		assert_eq!(jobs.waiting(), 2);

		// 3rd waiter exceeds MAX_JOBS_WAITING (2) -> immediate Busy
		let err = jobs.admit("ws", JobKind::View, &cancel).unwrap_err();
		assert_eq!(err, ErrorCode::Busy);
		assert_eq!(jobs.waiting(), 2);

		for t in threads {
			let _ = t.join();
		}
	}

	#[test]
	fn admit_returns_early_when_cancelled() {
		let jobs = Arc::new(Jobs::with_limits(
			1,
			1,
			16,
			scaled(Duration::from_secs(5)),
		));
		let cancel_running = CancelToken::new();
		let _g = jobs.admit("ws", JobKind::View, &cancel_running).unwrap();

		let cancel_waiting = CancelToken::new();
		let jobs_clone = jobs.clone();
		let cancel_waiting_clone = cancel_waiting.clone();

		let t = std::thread::spawn(move || {
			jobs_clone.admit("ws", JobKind::View, &cancel_waiting_clone)
		});

		let deadline = Instant::now() + scaled(Duration::from_secs(1));
		while jobs.waiting() < 1 && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(5));
		}
		assert_eq!(jobs.waiting(), 1);
		cancel_waiting.cancel();

		let start = Instant::now();
		let res = t.join().expect("join");
		assert!(start.elapsed() < scaled(Duration::from_secs(2)));
		assert_eq!(res.err(), Some(ErrorCode::Cancelled));
	}

	#[test]
	fn cancel_all_cancels_every_token_and_refuses_new_jobs() {
		let jobs = Jobs::with_limits(4, 2, 16, scaled(Duration::from_secs(5)));
		let c1 = CancelToken::new();
		let c2 = CancelToken::new();

		let _g1 = jobs.admit("ws1", JobKind::View, &c1).unwrap();
		let _g2 = jobs.admit("ws2", JobKind::View, &c2).unwrap();

		jobs.cancel_all();

		assert!(c1.is_cancelled());
		assert!(c2.is_cancelled());

		let c3 = CancelToken::new();
		let err = jobs.admit("ws3", JobKind::View, &c3).unwrap_err();
		assert_eq!(err, ErrorCode::Busy);
	}

	/// A writer the job's op can read back while `run_job_with` writes.
	#[derive(Clone, Default)]
	struct SharedBuf(Arc<Mutex<Vec<u8>>>);

	impl Write for SharedBuf {
		fn write(&mut self, data: &[u8]) -> io::Result<usize> {
			self.0.lock().unwrap().extend_from_slice(data);
			Ok(data.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	fn pending_frames(bytes: &[u8]) -> usize {
		let mut cursor = std::io::Cursor::new(bytes);
		let mut n = 0;
		while let Ok(Some(Response::Pending)) =
			read_frame::<Response>(&mut cursor)
		{
			n += 1;
		}
		n
	}

	#[test]
	fn run_job_sends_at_least_two_pending_frames_before_the_result() {
		let buf = SharedBuf::default();
		let cancel = CancelToken::new();
		let heartbeat = Duration::from_millis(20);
		let deadline = scaled(Duration::from_secs(10));

		// The op returns once two heartbeats were written, not after a
		// fixed sleep a slow machine can outlast with one heartbeat.
		let seen = buf.clone();
		run_job_with(
			&mut Answer::new(&mut buf.clone(), None),
			heartbeat,
			deadline,
			cancel,
			DeadlineReply::Fail,
			|_, end| {
				while pending_frames(&seen.0.lock().unwrap()) < 2
					&& Instant::now() < end
				{
					std::thread::sleep(Duration::from_millis(5));
				}
				Response::Stat(Stat {
					kind: crate::proto::EntryKind::File,
					size: 42,
					modified: None,
				})
			},
		)
		.unwrap();

		let bytes = buf.0.lock().unwrap().clone();
		let mut cursor = std::io::Cursor::new(bytes);
		let mut pending_count = 0;
		let got_stat;

		loop {
			match read_frame::<Response>(&mut cursor).unwrap() {
				Some(Response::Pending) => pending_count += 1,
				Some(Response::Stat(s)) => {
					assert_eq!(s.size, 42);
					got_stat = true;
					break;
				}
				other => panic!("unexpected frame: {other:?}"),
			}
		}

		assert!(
			pending_count >= 2,
			"expected at least 2 pending frames, got {pending_count}"
		);
		assert!(got_stat);
	}

	struct FailingWriter;
	impl Write for FailingWriter {
		fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
			Err(io::Error::other("simulated write error"))
		}
		fn flush(&mut self) -> io::Result<()> {
			Err(io::Error::other("simulated write error"))
		}
	}

	#[test]
	fn a_failing_writer_cancels_the_op() {
		let mut writer = FailingWriter;
		let cancel = CancelToken::new();
		let heartbeat = scaled(Duration::from_millis(10));
		let deadline = scaled(Duration::from_secs(2));

		let op_cancelled = Arc::new(Mutex::new(false));
		let op_cancelled_clone = op_cancelled.clone();

		let res = run_job_with(
			&mut Answer::new(&mut writer, None),
			heartbeat,
			deadline,
			cancel,
			DeadlineReply::Fail,
			|token, _| {
				let start = Instant::now();
				let limit = scaled(Duration::from_secs(2));
				while !token.is_cancelled() {
					if start.elapsed() > limit {
						panic!("op was not cancelled within deadline");
					}
					std::thread::sleep(Duration::from_millis(5));
				}
				*op_cancelled_clone.lock().unwrap() = true;
				Response::Pending
			},
		);

		assert!(res.is_err());
		assert!(*op_cancelled.lock().unwrap());
	}

	#[test]
	fn a_job_past_its_deadline_is_cancelled() {
		let mut buf = Vec::new();
		let cancel = CancelToken::new();
		let heartbeat = scaled(Duration::from_millis(10));
		let deadline = scaled(Duration::from_millis(30));

		let op_cancelled = Arc::new(Mutex::new(false));
		let op_cancelled_clone = op_cancelled.clone();

		run_job_with(
			&mut Answer::new(&mut buf, None),
			heartbeat,
			deadline,
			cancel,
			DeadlineReply::Fail,
			|token, _| {
				let start = Instant::now();
				let limit = scaled(Duration::from_secs(2));
				while !token.is_cancelled() {
					if start.elapsed() > limit {
						panic!("op was not cancelled within deadline");
					}
					std::thread::sleep(Duration::from_millis(5));
				}
				*op_cancelled_clone.lock().unwrap() = true;
				Response::Text {
					content: Some("finished late".into()),
				}
			},
		)
		.unwrap();

		assert!(*op_cancelled.lock().unwrap());

		let mut cursor = std::io::Cursor::new(buf);
		let mut last_error = None;
		while let Some(frame) = read_frame::<Response>(&mut cursor).unwrap() {
			match frame {
				Response::Pending => {}
				Response::Error { code, message, .. } => {
					last_error = Some((code, message));
				}
				other => panic!("unexpected frame: {other:?}"),
			}
		}

		let (code, _) = last_error.expect("expected error frame");
		assert_eq!(code, ErrorCode::Timeout);
	}

	/// A write that finishes after its deadline keeps its own response:
	/// Pending heartbeats cover the silence, then the real response goes
	/// out. A Timeout refusal here would report a failure for a write that
	/// landed, and a master cannot tell that apart from the truth.
	#[test]
	fn a_write_job_answers_its_real_response_past_the_deadline() {
		let buf = SharedBuf::default();
		let cancel = CancelToken::new();
		let heartbeat = scaled(Duration::from_millis(10));
		let deadline = scaled(Duration::from_millis(30));

		// A confirmed write ignores cancellation and finishes late: past
		// the deadline, and only once a heartbeat has gone out (waited for,
		// not counted during a fixed sleep).
		let written = buf.0.clone();
		run_job_with(
			&mut Answer::new(&mut buf.clone(), None),
			heartbeat,
			deadline,
			cancel,
			DeadlineReply::WriteOutcome,
			move |_token, _| {
				let start = Instant::now();
				let bound = scaled(Duration::from_secs(10));
				let heartbeat_seen = || {
					let bytes = written.lock().unwrap().clone();
					let mut cursor = std::io::Cursor::new(bytes);
					matches!(
						read_frame::<Response>(&mut cursor),
						Ok(Some(Response::Pending))
					)
				};
				while (start.elapsed() < deadline * 2 || !heartbeat_seen())
					&& start.elapsed() < bound
				{
					std::thread::sleep(Duration::from_millis(5));
				}
				Response::Text {
					content: Some("the write landed".into()),
				}
			},
		)
		.unwrap();

		let mut cursor = std::io::Cursor::new(buf.0.lock().unwrap().clone());
		let mut pending_after = 0usize;
		let final_response = loop {
			match read_frame::<Response>(&mut cursor).unwrap() {
				Some(Response::Pending) => pending_after += 1,
				Some(response) => break response,
				None => panic!("the response was discarded"),
			}
		};
		assert_eq!(
			final_response,
			Response::Text {
				content: Some("the write landed".into())
			}
		);
		assert!(
			pending_after >= 1,
			"heartbeats cover the silence while the write finishes"
		);
	}

	/// A read past its deadline is cancelled, but a call that cannot be
	/// interrupted (a slow filesystem) keeps the worker busy: the master
	/// hears Pending while it ends, then the Timeout, which stays the last
	/// frame. Silence there reads as a dead worker (#191).
	#[test]
	fn a_read_job_past_its_deadline_keeps_heartbeating_until_it_ends() {
		let buf = SharedBuf::default();
		let heartbeat = scaled(Duration::from_millis(100));
		let deadline = scaled(Duration::from_millis(30));

		run_job_with(
			&mut Answer::new(&mut buf.clone(), None),
			heartbeat,
			deadline,
			CancelToken::new(),
			DeadlineReply::Fail,
			|_, _| {
				// Ignores its cancel, as a blocked call does.
				std::thread::sleep(scaled(Duration::from_millis(450)));
				Response::Text {
					content: Some("finished late".into()),
				}
			},
		)
		.unwrap();

		let mut cursor = std::io::Cursor::new(buf.0.lock().unwrap().clone());
		let mut pending = 0usize;
		let last = loop {
			match read_frame::<Response>(&mut cursor).unwrap() {
				Some(Response::Pending) => pending += 1,
				Some(frame) => break frame,
				None => panic!("no reply"),
			}
		};
		assert!(
			matches!(
				last,
				Response::Error {
					code: ErrorCode::Timeout,
					..
				}
			),
			"{last:?}"
		);
		assert!(
			read_frame::<Response>(&mut cursor).unwrap().is_none(),
			"the Timeout is the last frame"
		);
		assert!(
			pending >= 1,
			"no heartbeat while the job ran past its deadline"
		);
	}

	#[test]
	fn run_job_turns_an_oversized_reply_into_too_large() {
		let mut buf = Vec::new();
		let cancel = CancelToken::new();
		let heartbeat = scaled(Duration::from_secs(1));
		let deadline = scaled(Duration::from_secs(2));

		// MAX_FRAME is 8 MiB in proto.rs
		let huge_string = "x".repeat(crate::proto::MAX_FRAME + 1024);

		run_job_with(
			&mut Answer::new(&mut buf, None),
			heartbeat,
			deadline,
			cancel,
			DeadlineReply::Fail,
			move |_, _| Response::Text {
				content: Some(huge_string),
			},
		)
		.unwrap();

		let mut cursor = std::io::Cursor::new(buf);
		let frame = read_frame::<Response>(&mut cursor)
			.unwrap()
			.expect("expected frame");
		match frame {
			Response::Error { code, .. } => {
				assert_eq!(code, ErrorCode::TooLarge);
			}
			other => panic!("expected TooLarge error, got {other:?}"),
		}
	}
}
