//! The one place snip starts Git processes.
//!
//! Every Git process goes through `ManagedChild`: at most
//! [`MAX_CONCURRENT_GIT`] run at once, at most [`MAX_QUEUED_GIT`] callers
//! wait for a slot (each can time out or be cancelled), each run has a
//! deadline, stdout is capped, and on every exit path the whole process
//! tree is killed, the root reaped and the pipes closed before the budget
//! slot is released. If that cleanup cannot be confirmed the slot stays
//! taken ([`leaked_slots`]) and the call reports [`GitError::Cleanup`].
//!
//! Tree ownership:
//! - Unix: the child leads a new process group; the group is SIGKILLed.
//!   The root is never reaped before the group is killed: its exit is
//!   observed without reaping (`waitid(WNOWAIT)` on Linux/FreeBSD, kqueue
//!   `NOTE_EXIT` on macOS), so the root pid, and with it the group id,
//!   cannot be reused by an unrelated process while we may still signal it.
//!   Pipes are read non-blocking with `poll`, so a descendant that escaped
//!   the group (`setsid`) and keeps a pipe open cannot hang a read: the
//!   deadline still fires and [`GitError::OutputHeldOpen`] is returned.
//! - Windows: process-wrap creates a Job object, spawns the child
//!   suspended, assigns it to the job and only then resumes it, so every
//!   descendant is in the job before it can run; if job setup fails it
//!   terminates the suspended child. The job is terminated explicitly and
//!   the job handle, not a pid, is the tree's identity. Reader threads are
//!   joined only once they have finished (bounded wait). Its std `JobObject`
//!   is not kill-on-close, so a tree whose termination fails is reported
//!   (`GitError::Cleanup`) and its slot kept, never assumed dead.

use std::cell::Cell;
use std::io::{self, BufRead, Read};
use std::marker::PhantomData;
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::gitsrc::GitError;

/// Global budget: Git processes alive at once, across all repositories.
pub const MAX_CONCURRENT_GIT: usize = 2;
/// Callers allowed to wait for a slot; one more is refused at once
/// ([`GitError::QueueFull`]) instead of joining an unbounded queue.
pub const MAX_QUEUED_GIT: usize = 64;
const TICK: Duration = Duration::from_millis(20);
/// How long cleanup waits for a killed tree to die and its pipes to close.
const GRACE: Duration = Duration::from_secs(5);
const STDERR_CAP: usize = 64 * 1024;
const CHUNK: usize = 64 * 1024;

/// Cooperative cancellation shared between a caller and a running Git call.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn cancel(&self) {
		self.0.store(true, Ordering::SeqCst);
	}

	pub fn is_cancelled(&self) -> bool {
		self.0.load(Ordering::SeqCst)
	}
}

/// What happens when stdout exceeds [`RunOptions::max_stdout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
	/// Kill the process and fail with [`GitError::OutputLimit`].
	Error,
	/// Kill the process and return the first `max_stdout` bytes with
	/// [`RunOutput::truncated`] set. For previews only: the exit status of
	/// a truncated run is unknown.
	Truncate,
}

/// Limits of one Git call. `Default` keeps the legacy CLI/desktop paths
/// working on big repositories; new workflows should start from
/// [`RunOptions::interactive`] or [`RunOptions::preview`].
#[derive(Debug, Clone)]
pub struct RunOptions {
	/// Whole run, from spawn to the tree being reaped.
	pub timeout: Duration,
	/// Longest wait for a free slot in the global budget.
	pub queue_timeout: Duration,
	pub max_stdout: usize,
	pub overflow: Overflow,
	pub cancel: Option<CancelToken>,
}

impl Default for RunOptions {
	fn default() -> Self {
		Self {
			timeout: Duration::from_secs(300),
			queue_timeout: Duration::from_secs(120),
			max_stdout: 256 * 1024 * 1024,
			overflow: Overflow::Error,
			cancel: None,
		}
	}
}

impl RunOptions {
	/// Retained output a UI call may hold.
	pub const INTERACTIVE_MAX_STDOUT: usize = 64 * 1024 * 1024;
	/// Output a preview shows.
	pub const PREVIEW_MAX_STDOUT: usize = 1024 * 1024;

	/// A call a user is waiting on: 30 s, 10 s in the queue, 64 MiB of
	/// output at most (strict).
	pub fn interactive(cancel: Option<CancelToken>) -> Self {
		Self {
			timeout: Duration::from_secs(30),
			queue_timeout: Duration::from_secs(10),
			max_stdout: Self::INTERACTIVE_MAX_STDOUT,
			overflow: Overflow::Error,
			cancel,
		}
	}

	/// A preview: 15 s, 5 s in the queue, 1 MiB, explicitly truncated.
	pub fn preview(cancel: Option<CancelToken>) -> Self {
		Self {
			timeout: Duration::from_secs(15),
			queue_timeout: Duration::from_secs(5),
			max_stdout: Self::PREVIEW_MAX_STDOUT,
			overflow: Overflow::Truncate,
			cancel,
		}
	}

	fn cancelled(&self) -> bool {
		self.cancel.as_ref().is_some_and(CancelToken::is_cancelled)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
	pub stdout: Vec<u8>,
	pub truncated: bool,
}

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

struct Budget {
	used: usize,
	waiting: usize,
	leaked: usize,
}

static BUDGET: Mutex<Budget> = Mutex::new(Budget {
	used: 0,
	waiting: 0,
	leaked: 0,
});
static FREED: Condvar = Condvar::new();

thread_local! {
	static HOLDING: Cell<bool> = const { Cell::new(false) };
}

fn budget() -> MutexGuard<'static, Budget> {
	BUDGET.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Slots taken, including leaked ones (diagnostics and tests).
pub fn in_flight() -> usize {
	budget().used
}

/// Callers waiting for a slot (diagnostics and tests).
pub fn queued() -> usize {
	budget().waiting
}

/// Slots that stay taken because a process tree could not be confirmed
/// dead. Each one permanently lowers the budget; nonzero means a bug or an
/// OS refusing to kill our own children.
pub fn leaked_slots() -> usize {
	budget().leaked
}

/// One budget slot. `!Send`: the per-thread nesting guard relies on the
/// permit being released on the thread that took it.
struct Permit(PhantomData<*const ()>);

impl Permit {
	fn acquire(opts: &RunOptions, args: &str) -> Result<Self, GitError> {
		// A thread that already holds a slot and waits for another can
		// deadlock the budget (two such threads hold both slots).
		if HOLDING.get() {
			return Err(GitError::NestedProcess { args: args.into() });
		}
		let mut b = budget();
		if b.used < MAX_CONCURRENT_GIT {
			b.used += 1;
			HOLDING.set(true);
			return Ok(Self(PhantomData));
		}
		if b.waiting >= MAX_QUEUED_GIT {
			return Err(GitError::QueueFull { args: args.into() });
		}
		b.waiting += 1;
		let deadline = Instant::now() + opts.queue_timeout;
		let result = loop {
			if opts.cancelled() {
				break Err(GitError::Cancelled { args: args.into() });
			}
			if b.used < MAX_CONCURRENT_GIT {
				b.used += 1;
				HOLDING.set(true);
				break Ok(Self(PhantomData));
			}
			let now = Instant::now();
			if now >= deadline {
				break Err(GitError::QueueTimeout { args: args.into() });
			}
			b = FREED
				.wait_timeout(b, TICK.min(deadline - now))
				.unwrap_or_else(PoisonError::into_inner)
				.0;
		};
		b.waiting -= 1;
		result
	}

	/// Keeps the slot taken forever: what it guarded may still be alive.
	fn leak(self) {
		budget().leaked += 1;
		HOLDING.set(false);
		std::mem::forget(self);
	}
}

impl Drop for Permit {
	fn drop(&mut self) {
		budget().used -= 1;
		HOLDING.set(false);
		FREED.notify_all();
	}
}

// ---------------------------------------------------------------------------
// Root exit without reaping (Unix)
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
mod exit_watch {
	use std::io;

	use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
	use nix::unistd::Pid;

	pub(super) struct ExitWatch {
		pid: Pid,
		exited: bool,
	}

	impl ExitWatch {
		pub(super) fn new(pid: u32) -> io::Result<Self> {
			let pid = i32::try_from(pid).map_err(io::Error::other)?;
			Ok(Self {
				pid: Pid::from_raw(pid),
				exited: false,
			})
		}

		/// `WNOWAIT`: the status stays queued and the pid stays ours.
		pub(super) fn exited(&mut self) -> io::Result<bool> {
			if !self.exited {
				let flags = WaitPidFlag::WEXITED
					| WaitPidFlag::WNOHANG
					| WaitPidFlag::WNOWAIT;
				self.exited = !matches!(
					waitid(Id::Pid(self.pid), flags)?,
					WaitStatus::StillAlive
				);
			}
			Ok(self.exited)
		}
	}
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod exit_watch {
	use std::io;

	use nix::errno::Errno;
	use nix::libc::timespec;
	use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

	/// kqueue `NOTE_EXIT` reports the exit and leaves the zombie unreaped.
	pub(super) struct ExitWatch {
		kq: Kqueue,
		exited: bool,
	}

	const NOW: timespec = timespec {
		tv_sec: 0,
		tv_nsec: 0,
	};

	fn event(pid: usize, flags: EventFlag, fflags: FilterFlag) -> KEvent {
		KEvent::new(pid, EventFilter::EVFILT_PROC, flags, fflags, 0, 0)
	}

	impl ExitWatch {
		pub(super) fn new(pid: u32) -> io::Result<Self> {
			let kq = Kqueue::new()?;
			let add = event(
				pid as usize,
				EventFlag::EV_ADD | EventFlag::EV_ONESHOT,
				FilterFlag::NOTE_EXIT,
			);
			let exited = match kq.kevent(&[add], &mut [], Some(NOW)) {
				Ok(_) => false,
				// Already a zombie: exited, still unreaped.
				Err(Errno::ESRCH) => true,
				Err(e) => return Err(e.into()),
			};
			Ok(Self { kq, exited })
		}

		pub(super) fn exited(&mut self) -> io::Result<bool> {
			if !self.exited {
				let mut out =
					[event(0, EventFlag::empty(), FilterFlag::empty())];
				self.exited = self.kq.kevent(&[], &mut out, Some(NOW))? > 0;
			}
			Ok(self.exited)
		}
	}
}

#[cfg(all(
	unix,
	not(any(
		target_os = "linux",
		target_os = "android",
		target_os = "freebsd",
		target_os = "macos",
		target_os = "ios"
	))
))]
compile_error!("gitrun: no way to observe a child's exit without reaping it");

// ---------------------------------------------------------------------------
// Pipes
// ---------------------------------------------------------------------------

enum Event {
	Data(usize, Vec<u8>),
	Eof(usize),
	Idle,
}

const STDOUT: usize = 0;
const STDERR: usize = 1;

#[cfg(unix)]
mod pipes {
	use std::io::{self, Read};
	use std::os::fd::{AsFd, AsRawFd};
	use std::process::{ChildStderr, ChildStdout};
	use std::time::Duration;

	use nix::errno::Errno;
	use nix::fcntl::{fcntl, FcntlArg, OFlag};
	use nix::poll::{poll, PollFd, PollFlags};

	use super::{Event, CHUNK};

	enum Stream {
		Out(ChildStdout),
		Err(ChildStderr),
	}

	/// Non-blocking pipe ends polled from the calling thread: no reader
	/// threads exist, so nothing can outlive the call.
	#[derive(Default)]
	pub(super) struct Pipes {
		streams: [Option<Stream>; 2],
	}

	fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
		let fd = fd.as_fd().as_raw_fd();
		let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
		fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
		Ok(())
	}

	impl Pipes {
		pub(super) fn attach_stdout(
			&mut self,
			s: ChildStdout,
		) -> io::Result<()> {
			nonblocking(&s)?;
			self.streams[super::STDOUT] = Some(Stream::Out(s));
			Ok(())
		}

		pub(super) fn attach_stderr(
			&mut self,
			s: ChildStderr,
		) -> io::Result<()> {
			nonblocking(&s)?;
			self.streams[super::STDERR] = Some(Stream::Err(s));
			Ok(())
		}

		pub(super) fn is_open(&self) -> bool {
			self.streams.iter().any(Option::is_some)
		}

		pub(super) fn next(&mut self, wait: Duration) -> io::Result<Event> {
			let ready: Vec<usize> = {
				let open: Vec<(usize, PollFd)> = self
					.streams
					.iter()
					.enumerate()
					.filter_map(|(i, s)| {
						let fd = match s.as_ref()? {
							Stream::Out(o) => PollFd::new(o, PollFlags::POLLIN),
							Stream::Err(e) => PollFd::new(e, PollFlags::POLLIN),
						};
						Some((i, fd))
					})
					.collect();
				if open.is_empty() {
					return Ok(Event::Idle);
				}
				let mut fds: Vec<PollFd> =
					open.iter().map(|(_, f)| *f).collect();
				let ms = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
				match poll(&mut fds, ms) {
					Ok(_) => {}
					Err(Errno::EINTR) => return Ok(Event::Idle),
					Err(e) => return Err(e.into()),
				}
				open.iter()
					.zip(&fds)
					// Unknown revents bits: try the read and let it decide.
					.filter(|(_, f)| f.any() != Some(false))
					.map(|((i, _), _)| *i)
					.collect()
			};
			for i in ready {
				let mut buf = vec![0; CHUNK];
				let read = match self.streams[i].as_mut() {
					Some(Stream::Out(o)) => o.read(&mut buf),
					Some(Stream::Err(e)) => e.read(&mut buf),
					None => continue,
				};
				match read {
					Ok(0) => {
						self.streams[i] = None;
						return Ok(Event::Eof(i));
					}
					Ok(n) => {
						buf.truncate(n);
						return Ok(Event::Data(i, buf));
					}
					Err(e)
						if matches!(
							e.kind(),
							io::ErrorKind::WouldBlock
								| io::ErrorKind::Interrupted
						) => {}
					Err(e) => return Err(e),
				}
			}
			Ok(Event::Idle)
		}

		/// Closes our ends; there is nothing to wait for.
		pub(super) fn close(&mut self, _grace: Duration) -> io::Result<()> {
			self.streams = [None, None];
			Ok(())
		}
	}
}

#[cfg(windows)]
mod pipes {
	use std::io::{self, Read};
	use std::process::{ChildStderr, ChildStdout};
	use std::sync::mpsc::{
		self, Receiver, RecvTimeoutError, Sender, SyncSender,
	};
	use std::thread::{self, JoinHandle};
	use std::time::{Duration, Instant};

	use super::{Event, CHUNK};

	type Msg = (usize, io::Result<Vec<u8>>);

	/// Windows anonymous pipes cannot be polled, so each gets a reader
	/// thread. A reader ends when every write handle in the tree is closed,
	/// which terminating the Job guarantees; `close` waits (bounded) for
	/// every reader to report its end before joining it.
	pub(super) struct Pipes {
		tx: Option<SyncSender<Msg>>,
		rx: Option<Receiver<Msg>>,
		done_tx: Option<Sender<()>>,
		done_rx: Receiver<()>,
		readers: Vec<JoinHandle<()>>,
		open: [bool; 2],
	}

	impl Default for Pipes {
		fn default() -> Self {
			let (tx, rx) = mpsc::sync_channel(4);
			let (done_tx, done_rx) = mpsc::channel();
			Self {
				tx: Some(tx),
				rx: Some(rx),
				done_tx: Some(done_tx),
				done_rx,
				readers: Vec::new(),
				open: [false; 2],
			}
		}
	}

	fn pump(
		i: usize,
		mut r: impl Read,
		tx: SyncSender<Msg>,
		_done: Sender<()>,
	) {
		// `_done` is dropped when this returns: that is the end signal.
		loop {
			let mut buf = vec![0; CHUNK];
			match r.read(&mut buf) {
				Ok(0) => {
					let _ = tx.send((i, Ok(Vec::new())));
					return;
				}
				Ok(n) => {
					buf.truncate(n);
					// A dropped receiver means the call is over.
					if tx.send((i, Ok(buf))).is_err() {
						return;
					}
				}
				Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
				Err(e) => {
					let _ = tx.send((i, Err(e)));
					return;
				}
			}
		}
	}

	impl Pipes {
		fn attach(
			&mut self,
			i: usize,
			r: impl Read + Send + 'static,
		) -> io::Result<()> {
			let (Some(tx), Some(done)) =
				(self.tx.clone(), self.done_tx.clone())
			else {
				return Err(io::Error::other("pipes already closed"));
			};
			let handle = thread::Builder::new()
				.name("snip-git-pipe".into())
				.spawn(move || pump(i, r, tx, done))?;
			self.readers.push(handle);
			self.open[i] = true;
			Ok(())
		}

		pub(super) fn attach_stdout(
			&mut self,
			s: ChildStdout,
		) -> io::Result<()> {
			self.attach(super::STDOUT, s)
		}

		pub(super) fn attach_stderr(
			&mut self,
			s: ChildStderr,
		) -> io::Result<()> {
			self.attach(super::STDERR, s)
		}

		pub(super) fn is_open(&self) -> bool {
			self.open.iter().any(|o| *o)
		}

		pub(super) fn next(&mut self, wait: Duration) -> io::Result<Event> {
			let Some(rx) = self.rx.as_ref().filter(|_| self.is_open()) else {
				return Ok(Event::Idle);
			};
			match rx.recv_timeout(wait) {
				Ok((i, Ok(data))) if data.is_empty() => {
					self.open[i] = false;
					Ok(Event::Eof(i))
				}
				Ok((i, Ok(data))) => Ok(Event::Data(i, data)),
				Ok((i, Err(e))) if e.kind() == io::ErrorKind::BrokenPipe => {
					self.open[i] = false;
					Ok(Event::Eof(i))
				}
				Ok((i, Err(e))) => {
					self.open[i] = false;
					Err(e)
				}
				Err(RecvTimeoutError::Timeout) => Ok(Event::Idle),
				Err(RecvTimeoutError::Disconnected) => {
					self.open = [false; 2];
					Ok(Event::Idle)
				}
			}
		}

		/// Waits up to `grace` for every reader to end, then joins them.
		/// Called only after the Job was terminated or closed. A reader
		/// still blocked after `grace` is an error and stays unjoined.
		pub(super) fn close(&mut self, grace: Duration) -> io::Result<()> {
			// Unblocks a reader waiting to hand over a chunk.
			self.rx = None;
			self.tx = None;
			self.done_tx = None;
			self.open = [false; 2];
			let deadline = Instant::now() + grace;
			loop {
				let left = deadline.saturating_duration_since(Instant::now());
				match self.done_rx.recv_timeout(left) {
					Err(RecvTimeoutError::Disconnected) => break,
					Err(RecvTimeoutError::Timeout) => {
						return Err(io::Error::new(
							io::ErrorKind::TimedOut,
							format!(
								"{} Git pipe reader(s) still blocked",
								self.readers.len()
							),
						));
					}
					Ok(()) => {}
				}
			}
			let mut panicked = false;
			for h in self.readers.drain(..) {
				panicked |= h.join().is_err();
			}
			if panicked {
				return Err(io::Error::other("Git pipe reader panicked"));
			}
			Ok(())
		}
	}

	impl Drop for Pipes {
		fn drop(&mut self) {
			// Never blocks past the grace period. A reader that still has
			// not ended is left running: its pipe is held by a process
			// outside our Job, which `ManagedChild` has reported.
			let _ = self.close(super::GRACE);
		}
	}
}

use pipes::Pipes;

// ---------------------------------------------------------------------------
// Process tree
// ---------------------------------------------------------------------------

/// Unix: the root leads its own process group (`process_group(0)`).
#[cfg(unix)]
mod tree {
	use std::io;
	use std::os::unix::process::CommandExt;
	use std::process::{
		Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus,
	};

	use nix::errno::Errno;
	use nix::sys::signal::{killpg, Signal};
	use nix::unistd::Pid;

	pub(super) struct Tree {
		child: Child,
	}

	impl Tree {
		pub(super) fn spawn(cmd: &mut Command) -> io::Result<Self> {
			cmd.process_group(0);
			Ok(Self {
				child: cmd.spawn()?,
			})
		}

		pub(super) fn id(&self) -> u32 {
			self.child.id()
		}

		pub(super) fn pipes(
			&mut self,
		) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
			let c = &mut self.child;
			(c.stdin.take(), c.stdout.take(), c.stderr.take())
		}

		/// SIGKILL to the group. The caller guarantees the root is
		/// unreaped, so the group id is still ours.
		pub(super) fn kill_group(&mut self) -> io::Result<()> {
			let pgid =
				i32::try_from(self.child.id()).map_err(io::Error::other)?;
			match killpg(Pid::from_raw(pgid), Signal::SIGKILL) {
				// ESRCH: the group is already empty.
				Ok(()) | Err(Errno::ESRCH) => Ok(()),
				Err(e) => Err(e.into()),
			}
		}

		pub(super) fn kill_root(&mut self) -> io::Result<()> {
			self.child.kill()
		}

		pub(super) fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
			self.child.try_wait()
		}
	}
}

/// Windows: process-wrap's Job object. Job setup happens with the child
/// suspended; if it fails, process-wrap terminates that child before
/// returning the error, so a failed spawn leaves nothing running.
#[cfg(windows)]
mod tree {
	use std::io;
	use std::process::{
		ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus,
	};

	use process_wrap::std::{
		ChildWrapper, CommandWrap, CreationFlags, JobObject,
	};
	use windows::Win32::System::Threading::CREATE_NO_WINDOW;

	pub(super) struct Tree {
		child: Box<dyn ChildWrapper>,
	}

	impl Tree {
		pub(super) fn spawn(cmd: &mut Command) -> io::Result<Self> {
			let mut wrap =
				CommandWrap::from(std::mem::replace(cmd, Command::new("")));
			// A GUI process must not flash a console. Set through the
			// wrapper: JobObject would otherwise replace the flags.
			wrap.wrap(CreationFlags(CREATE_NO_WINDOW)).wrap(JobObject);
			let child = wrap.spawn();
			*cmd = wrap.into_command();
			Ok(Self { child: child? })
		}

		pub(super) fn pipes(
			&mut self,
		) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
			(
				self.child.stdin().take(),
				self.child.stdout().take(),
				self.child.stderr().take(),
			)
		}

		/// TerminateJobObject: every process in the job, whether or not the
		/// root has exited. The job handle is the identity; no pid is used.
		pub(super) fn kill_group(&mut self) -> io::Result<()> {
			self.child.start_kill()
		}

		pub(super) fn kill_root(&mut self) -> io::Result<()> {
			self.child.inner_mut().start_kill()
		}

		/// Never blocks: std's try_wait plus a zero-timeout job poll.
		pub(super) fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
			self.child.try_wait()
		}
	}
}

// ---------------------------------------------------------------------------
// Managed child
// ---------------------------------------------------------------------------

fn spawn_error(e: io::Error) -> GitError {
	if e.kind() == io::ErrorKind::NotFound {
		GitError::NotFound
	} else {
		GitError::Io(e)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeCleanup {
	Unconfirmed,
	Confirmed,
	Failed,
}

#[cfg(test)]
thread_local! {
	static INJECT_KILL_TREE_FAILURE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn test_inject_kill_tree_failure(fail: bool) {
	INJECT_KILL_TREE_FAILURE.set(fail);
}

/// A spawned Git process tree that holds a budget slot until the tree is
/// dead, the root reaped and the pipes closed.
pub(crate) struct ManagedChild {
	args: String,
	tree: tree::Tree,
	#[cfg(unix)]
	watch: exit_watch::ExitWatch,
	stdin: Option<ChildStdin>,
	pipes: Pipes,
	/// Set once the root is reaped (Unix) or observed exited (Windows).
	status: Option<ExitStatus>,
	/// Whether tree cleanup has been confirmed, failed, or not yet confirmed.
	tree_cleanup: TreeCleanup,
	/// Cleanup confirmed; the permit is gone.
	clean: bool,
	permit: Option<Permit>,
}

impl ManagedChild {
	pub(crate) fn spawn(
		mut cmd: Command,
		args: &str,
		input: bool,
		opts: &RunOptions,
	) -> Result<Self, GitError> {
		let permit = Permit::acquire(opts, args)?;
		cmd.stdin(if input { Stdio::piped() } else { Stdio::null() })
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		let spawned = tree::Tree::spawn(&mut cmd);
		// A missing working directory also spawns as NotFound; that is not
		// a missing git.
		let dir_missing = cmd.get_current_dir().is_some_and(|d| !d.is_dir());
		let mut tree = spawned.map_err(|e| {
			if dir_missing {
				GitError::Io(e)
			} else {
				spawn_error(e)
			}
		})?;
		#[cfg(unix)]
		let watch = match exit_watch::ExitWatch::new(tree.id()) {
			Ok(w) => w,
			Err(e) => {
				// No way to observe the root without reaping it: kill the
				// group now, while the pid is certainly ours.
				let killed = tree.kill_group();
				let reaped = tree.kill_root().and_then(|()| {
					let deadline = Instant::now() + GRACE;
					while tree.try_reap()?.is_none() {
						if Instant::now() >= deadline {
							return Err(io::ErrorKind::TimedOut.into());
						}
						std::thread::sleep(TICK);
					}
					Ok(())
				});
				if killed.is_err() || reaped.is_err() {
					permit.leak();
				}
				return Err(GitError::Io(e));
			}
		};
		let (stdin, stdout, stderr) = tree.pipes();
		let mut proc = Self {
			args: args.into(),
			tree,
			#[cfg(unix)]
			watch,
			stdin,
			pipes: Pipes::default(),
			status: None,
			tree_cleanup: TreeCleanup::Unconfirmed,
			clean: false,
			permit: Some(permit),
		};
		let attached = match (stdout, stderr) {
			(Some(out), Some(err)) => proc
				.pipes
				.attach_stdout(out)
				.and_then(|()| proc.pipes.attach_stderr(err)),
			_ => Err(io::Error::other("Git pipes unavailable")),
		};
		if let Err(e) = attached {
			proc.finish()?;
			return Err(GitError::Io(e));
		}
		Ok(proc)
	}

	pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
		self.stdin.take()
	}

	#[cfg(all(test, unix))]
	fn id(&self) -> u32 {
		self.tree.id()
	}

	fn cleanup_error(&self, message: String) -> GitError {
		GitError::Cleanup {
			args: self.args.clone(),
			message,
		}
	}

	/// Kills every process of the tree. Unix: only while the root is
	/// unreaped, so the group id cannot belong to anyone else.
	fn kill_tree(&mut self) -> io::Result<()> {
		#[cfg(test)]
		if INJECT_KILL_TREE_FAILURE.get() {
			self.tree_cleanup = TreeCleanup::Failed;
			return Err(io::Error::other("injected process tree kill failure"));
		}

		match self.tree_cleanup {
			TreeCleanup::Confirmed => Ok(()),
			TreeCleanup::Failed => Err(io::Error::other(
				"process tree cleanup previously failed and cannot be confirmed",
			)),
			TreeCleanup::Unconfirmed => {
				#[cfg(unix)]
				if self.status.is_some() {
					self.tree_cleanup = TreeCleanup::Failed;
					return Err(io::Error::other(
						"cannot signal process group after root process was reaped; tree cleanup unconfirmed",
					));
				}
				match self.tree.kill_group() {
					Ok(()) => {
						self.tree_cleanup = TreeCleanup::Confirmed;
						Ok(())
					}
					Err(e) => {
						self.tree_cleanup = TreeCleanup::Failed;
						Err(e)
					}
				}
			}
		}
	}

	/// Whether the root has exited, without reaping it on Unix.
	fn poll_exit(&mut self) -> Result<bool, GitError> {
		if self.status.is_some() {
			return Ok(true);
		}
		#[cfg(unix)]
		let exited = self.watch.exited();
		// Windows: the open process handle keeps the pid from being reused
		// and the job is the tree's identity, so observing is reaping.
		#[cfg(windows)]
		let exited = self.tree.try_reap().map(|s| {
			self.status = s;
			s.is_some()
		});
		exited
			.map_err(|e| self.cleanup_error(format!("watching the root: {e}")))
	}

	/// Reaps the root within `GRACE`.
	fn reap(&mut self) -> io::Result<Option<ExitStatus>> {
		let deadline = Instant::now() + GRACE;
		loop {
			if let Some(s) = self.tree.try_reap()? {
				return Ok(Some(s));
			}
			if Instant::now() >= deadline {
				return Ok(None);
			}
			std::thread::sleep(TICK);
		}
	}

	/// Kills what is left of the tree, reaps the root and closes the pipes;
	/// only then is the budget slot released. Every failure is reported,
	/// and a failed cleanup keeps the slot: [`Drop`] retries once and leaks
	/// the slot if the tree still cannot be confirmed dead.
	fn finish(&mut self) -> Result<Option<ExitStatus>, GitError> {
		if self.clean {
			return Ok(self.status);
		}
		self.stdin = None;
		let mut problems = Vec::new();
		if let Err(e) = self.kill_tree() {
			problems.push(format!("killing the process tree: {e}"));
			// Fall back to the root alone (Unix: unreaped, so still ours).
			// If already reaped, do not resignal!
			if self.status.is_none() {
				if let Err(e) = self.tree.kill_root() {
					problems.push(format!("killing the root: {e}"));
				}
			}
		}
		if self.status.is_none() {
			match self.reap() {
				Ok(Some(s)) => self.status = Some(s),
				Ok(None) => problems.push(format!(
					"the root process did not exit within {}s",
					GRACE.as_secs()
				)),
				Err(e) => problems.push(format!("reaping the root: {e}")),
			}
		}
		if let Err(e) = self.pipes.close(GRACE) {
			problems.push(format!("closing pipes: {e}"));
		}
		if !problems.is_empty() {
			return Err(self.cleanup_error(problems.join("; ")));
		}
		if self.tree_cleanup != TreeCleanup::Confirmed {
			return Err(
				self.cleanup_error("process tree cleanup unconfirmed".into())
			);
		}
		self.clean = true;
		drop(self.permit.take());
		Ok(self.status)
	}

	/// Waits for the next pipe event while enforcing deadline and cancel.
	fn next_event(
		&mut self,
		deadline: Instant,
		opts: &RunOptions,
	) -> Result<Event, GitError> {
		if opts.cancelled() {
			self.finish()?;
			return Err(GitError::Cancelled {
				args: self.args.clone(),
			});
		}
		let now = Instant::now();
		if now >= deadline {
			// Root gone but a pipe still open: something outside our
			// reach holds it.
			let held = self.poll_exit()?;
			self.finish()?;
			return Err(if held {
				GitError::OutputHeldOpen {
					args: self.args.clone(),
				}
			} else {
				GitError::Timeout {
					args: self.args.clone(),
					secs: opts.timeout.as_secs(),
				}
			});
		}
		let wait = TICK.min(deadline - now);
		if !self.pipes.is_open() {
			std::thread::sleep(wait);
			return Ok(Event::Idle);
		}
		self.pipes.next(wait).map_err(GitError::Io)
	}
}

impl Drop for ManagedChild {
	fn drop(&mut self) {
		// Explicit paths call `finish` and report its errors; this is the
		// fallback for early returns, panics and a failed first cleanup.
		if !self.clean && self.finish().is_err() {
			if let Some(permit) = self.permit.take() {
				permit.leak();
			}
		}
	}
}

/// Output of a finished run: the status is `None` when the run was cut
/// short by [`Overflow::Truncate`].
pub(crate) struct Finished {
	pub status: Option<ExitStatus>,
	pub stdout: Vec<u8>,
	pub stderr: Vec<u8>,
	pub truncated: bool,
}

/// Runs `cmd` to completion under the budget, deadline and output cap.
pub(crate) fn run(
	cmd: Command,
	args: &str,
	input: Option<&[u8]>,
	opts: &RunOptions,
) -> Result<Finished, GitError> {
	let mut proc = ManagedChild::spawn(cmd, args, input.is_some(), opts)?;
	std::thread::scope(|scope| {
		// A scoped writer: joined before returning, and unblocked by the
		// tree being killed if the child never reads.
		let writer = proc.take_stdin().map(|mut w| {
			let data = input.unwrap_or_default();
			scope.spawn(move || io::Write::write_all(&mut w, data))
		});
		let result = pump(&mut proc, opts);
		let finished = proc.finish();
		let written = writer.map(|h| h.join());
		let out = result?;
		finished?;
		match written {
			Some(Err(_)) => {
				return Err(proc.cleanup_error("stdin writer panicked".into()))
			}
			// The child may exit without reading everything; its status says
			// whether that was a failure.
			Some(Ok(Err(e))) if e.kind() != io::ErrorKind::BrokenPipe => {
				return Err(GitError::Io(e))
			}
			_ => {}
		}
		Ok(out)
	})
}

fn pump(
	proc: &mut ManagedChild,
	opts: &RunOptions,
) -> Result<Finished, GitError> {
	let deadline = Instant::now() + opts.timeout;
	let mut stdout = Vec::new();
	let mut stderr = Vec::new();
	let mut leftovers_killed = false;
	loop {
		let exited = proc.poll_exit()?;
		if exited && !proc.pipes.is_open() {
			break;
		}
		if exited && !leftovers_killed {
			// The root is gone but its pipes are not: descendants still
			// hold them. They are part of this call's tree, and on Unix the
			// unreaped root still owns the group id.
			leftovers_killed = true;
			proc.kill_tree().map_err(|e| {
				proc.cleanup_error(format!("killing leftovers: {e}"))
			})?;
		}
		match proc.next_event(deadline, opts)? {
			Event::Data(STDOUT, chunk) => {
				if stdout.len() + chunk.len() > opts.max_stdout {
					proc.finish()?;
					if opts.overflow == Overflow::Error {
						return Err(GitError::OutputLimit {
							args: proc.args.clone(),
							limit: opts.max_stdout,
						});
					}
					let room = opts.max_stdout - stdout.len();
					stdout.extend_from_slice(&chunk[..room]);
					return Ok(Finished {
						status: None,
						stdout,
						stderr,
						truncated: true,
					});
				}
				stdout.extend_from_slice(&chunk);
			}
			Event::Data(_, chunk) => {
				let room = STDERR_CAP.saturating_sub(stderr.len());
				stderr.extend_from_slice(&chunk[..room.min(chunk.len())]);
			}
			Event::Eof(_) | Event::Idle => {}
		}
	}
	let status = proc.finish()?;
	Ok(Finished {
		status,
		stdout,
		stderr,
		truncated: false,
	})
}

// ---------------------------------------------------------------------------
// Long-lived reader (cat-file --batch)
// ---------------------------------------------------------------------------

/// A [`ManagedChild`] whose stdout is read as a `BufRead` with a per-request
/// deadline. Timeouts surface as `TimedOut`, cancellation as `Interrupted`.
pub(crate) struct Session {
	proc: ManagedChild,
	opts: RunOptions,
	buf: Vec<u8>,
	pos: usize,
	deadline: Instant,
	stdin: Option<ChildStdin>,
	stdout_eof: bool,
}

impl Session {
	pub(crate) fn spawn(
		cmd: Command,
		args: &str,
		opts: RunOptions,
	) -> Result<Self, GitError> {
		let mut proc = ManagedChild::spawn(cmd, args, true, &opts)?;
		let stdin = proc.take_stdin();
		Ok(Self {
			proc,
			deadline: Instant::now() + opts.timeout,
			opts,
			buf: Vec::new(),
			pos: 0,
			stdin,
			stdout_eof: false,
		})
	}

	/// Starts a new request: resets the deadline.
	pub(crate) fn begin(&mut self) -> Option<&mut ChildStdin> {
		self.deadline = Instant::now() + self.opts.timeout;
		self.stdin.as_mut()
	}

	/// Maps an io error raised by this reader back to the Git error.
	pub(crate) fn error(&self, e: io::Error) -> GitError {
		match e.kind() {
			io::ErrorKind::TimedOut => GitError::Timeout {
				args: self.proc.args.clone(),
				secs: self.opts.timeout.as_secs(),
			},
			io::ErrorKind::Interrupted => GitError::Cancelled {
				args: self.proc.args.clone(),
			},
			_ => GitError::Io(e),
		}
	}

	/// Ends the session: closes stdin so the process exits, then cleans up
	/// the tree. Reports cleanup failures; dropping instead kills silently.
	pub(crate) fn close(mut self) -> Result<(), GitError> {
		self.stdin = None;
		let deadline = Instant::now() + self.opts.timeout;
		// Past the deadline `next_event` kills the tree and reports it.
		while !self.proc.poll_exit()? {
			self.proc.next_event(deadline, &self.opts)?;
		}
		self.proc.finish()?;
		Ok(())
	}
}

impl Read for Session {
	fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
		let n = {
			let buf = self.fill_buf()?;
			let n = buf.len().min(out.len());
			out[..n].copy_from_slice(&buf[..n]);
			n
		};
		self.consume(n);
		Ok(n)
	}
}

impl BufRead for Session {
	fn fill_buf(&mut self) -> io::Result<&[u8]> {
		while self.pos >= self.buf.len() {
			if self.stdout_eof {
				return Ok(&[]);
			}
			if self.opts.cancelled() {
				return Err(io::ErrorKind::Interrupted.into());
			}
			let now = Instant::now();
			if now >= self.deadline {
				return Err(io::ErrorKind::TimedOut.into());
			}
			match self.proc.pipes.next(TICK.min(self.deadline - now))? {
				Event::Data(STDOUT, chunk) => {
					self.buf = chunk;
					self.pos = 0;
				}
				Event::Eof(STDOUT) => self.stdout_eof = true,
				// stderr is not part of the protocol; its pipe just has to
				// keep draining so the process never blocks on it.
				Event::Data(..) | Event::Eof(_) | Event::Idle => {}
			}
		}
		Ok(&self.buf[self.pos..])
	}

	fn consume(&mut self, n: usize) {
		self.pos = (self.pos + n).min(self.buf.len());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	static SERIAL: Mutex<()> = Mutex::new(());

	fn serial() -> MutexGuard<'static, ()> {
		SERIAL.lock().unwrap_or_else(|e| e.into_inner())
	}

	#[cfg(unix)]
	fn ps_stat(pid: u32) -> String {
		let out = Command::new("ps")
			.args(["-o", "stat=", "-p", &pid.to_string()])
			.output()
			.unwrap();
		String::from_utf8_lossy(&out.stdout).trim().to_string()
	}

	/// The root's exit is seen while it is still an unreaped zombie, so
	/// its pid (the group id we signal) cannot have been reused; it is
	/// reaped only by `finish`, after the group kill.
	#[cfg(unix)]
	#[test]
	fn root_is_observed_exited_but_stays_unreaped_until_the_group_kill() {
		let _s = serial();
		let mut cmd = Command::new("sh");
		cmd.args(["-c", "exit 3"]);
		let mut proc =
			ManagedChild::spawn(cmd, "sh", false, &RunOptions::default())
				.unwrap();
		let pid = proc.id();
		let deadline = Instant::now() + Duration::from_secs(10);
		while !proc.poll_exit().unwrap() {
			assert!(Instant::now() < deadline, "exit never observed");
			std::thread::sleep(TICK);
		}
		// Observed twice: WNOWAIT / NOTE_EXIT did not consume anything.
		assert!(proc.poll_exit().unwrap());
		assert!(ps_stat(pid).starts_with('Z'), "{}", ps_stat(pid));
		assert_eq!(proc.status, None);
		let status = proc.finish().unwrap().unwrap();
		assert_eq!(status.code(), Some(3));
		assert!(proc.clean && proc.permit.is_none());
		assert!(!ps_stat(pid).starts_with('Z'));
		// Idempotent, and never signals the (now free) pid again.
		assert_eq!(proc.finish().unwrap(), Some(status));
	}

	#[test]
	fn a_leaked_permit_keeps_its_slot_but_frees_the_thread() {
		let _s = serial();
		let opts = RunOptions::default();
		let before = leaked_slots();
		let permit = Permit::acquire(&opts, "t").unwrap();
		permit.leak();
		assert_eq!(leaked_slots(), before + 1);
		// The thread may start Git again; the slot itself stays counted.
		let again = Permit::acquire(&opts, "t").unwrap();
		drop(again);
		// Give the leaked slot back so other tests keep the full budget.
		let mut b = budget();
		b.used -= 1;
		b.leaked -= 1;
		drop(b);
		FREED.notify_all();
	}
	struct InjectionGuard;
	impl Drop for InjectionGuard {
		fn drop(&mut self) {
			test_inject_kill_tree_failure(false);
		}
	}

	#[test]
	fn test_helper_sleep() {
		if std::env::var_os("SNIP_TEST_SLEEP_HELPER").is_some() {
			std::thread::sleep(Duration::from_secs(30));
		}
	}

	fn test_long_running_command() -> Command {
		let exe = std::env::current_exe().expect("current test exe");
		let mut cmd = Command::new(exe);
		cmd.env("SNIP_TEST_SLEEP_HELPER", "1").args([
			"--exact",
			"gitrun::tests::test_helper_sleep",
			"--nocapture",
		]);
		cmd
	}

	fn test_exit_command() -> Command {
		let exe = std::env::current_exe().expect("current test exe");
		let mut cmd = Command::new(exe);
		cmd.args([
			"--exact",
			"gitrun::tests::test_helper_sleep",
			"--nocapture",
		]);
		cmd
	}

	#[test]
	fn sticky_tree_kill_failure_retains_leaked_slot_without_resignal() {
		let _s = serial();
		if std::env::var_os("SNIP_TEST_ISOLATED").is_none() {
			let exe = std::env::current_exe().expect("current test exe");
			let status = Command::new(exe)
				.env("SNIP_TEST_ISOLATED", "1")
				.args([
					"--exact",
					"gitrun::tests::sticky_tree_kill_failure_retains_leaked_slot_without_resignal",
					"--nocapture",
				])
				.status()
				.expect("spawn isolated test subprocess");
			assert!(status.success(), "isolated subprocess test failed");
			return;
		}

		// Isolated process guarantees a fresh budget without concurrent thread races.
		assert_eq!(leaked_slots(), 0);
		assert_eq!(in_flight(), 0);

		// Case 1: First finish fails with injected kill_tree failure.
		// Injection is reset to false BEFORE drop. Drop's finish() must still fail
		// due to sticky TreeCleanup::Failed state, leaking the slot without resignaling.
		{
			let cmd = test_long_running_command();
			let mut proc = ManagedChild::spawn(
				cmd,
				"test-proc-1",
				false,
				&RunOptions::default(),
			)
			.unwrap();

			let _inj = InjectionGuard;
			test_inject_kill_tree_failure(true);

			let err = proc.finish().unwrap_err();
			assert!(matches!(err, GitError::Cleanup { .. }), "{err:?}");
			assert!(!proc.clean, "must not be clean after failed finish");
			assert!(proc.permit.is_some(), "permit must not be released yet");
			assert!(
				proc.status.is_some(),
				"root must have been reaped by fallback"
			);

			// Crucial: reset injection to FALSE before Drop.
			test_inject_kill_tree_failure(false);

			drop(proc);

			assert_eq!(
				leaked_slots(),
				1,
				"sticky cleanup failure must leak the slot via Drop"
			);
			assert_eq!(in_flight(), 1);
		}

		// Case 2: Direct drop without prior finish() invocation leaks slot when kill_tree fails.
		{
			let cmd = test_long_running_command();
			let proc = ManagedChild::spawn(
				cmd,
				"test-proc-2",
				false,
				&RunOptions::default(),
			)
			.unwrap();

			let _inj = InjectionGuard;
			test_inject_kill_tree_failure(true);

			drop(proc);

			assert_eq!(
				leaked_slots(),
				2,
				"direct drop failure must also leak slot"
			);
			assert_eq!(in_flight(), 2);
		}
	}

	#[test]
	fn normal_drop_without_prior_finish_cleans_up_and_releases_permit() {
		let _s = serial();
		let opts = RunOptions::default();
		let before_leaked = leaked_slots();

		let cmd = test_exit_command();
		let proc = ManagedChild::spawn(cmd, "exit", false, &opts).unwrap();
		assert!(!proc.clean);
		assert!(proc.permit.is_some());

		drop(proc);

		assert_eq!(leaked_slots(), before_leaked);
	}
}
