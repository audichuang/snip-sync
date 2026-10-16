//! The master: starts a worker on another machine over ssh
//! (`ssh <host> snip serve --stdio`) and calls it on that process's stdin
//! and stdout. Every call blocks; the desktop app makes them on its
//! background executor, as it does local reads.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read as IoRead, Write as IoWrite};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use snip_core::browser::{
	BlobText, CommitSummary, GitPreview, LogQuery, RefSnapshot, TreeEntry,
};
use snip_core::format::ChangeType;
use snip_core::gitrun::CancelToken;
use snip_core::gitsrc::{GitError, GitSource};
use snip_core::gitview::{
	ChangeList, ChangedPathList, CommitDetails, Read, ReadProfile, RepoView,
};
use snip_core::reason::{Datum, Reason, StaleChange};

use sha2::{Digest, Sha256};
use snip_core::commits::CommitCopyOutcome;
use snip_core::transfer::CopyOutcome;

use crate::proto::{
	read_frame, write_request_for, DirEntry, ErrorCode, ExportTarget, GitQuery,
	GitReply, ImportExpect, ImportPlanned, PasteMapping, RemoteWorkspace,
	ReplayExpect, RepoScan, Request, Response, Stat, GIT_CALL_LIMIT,
	JOINED_MAX, MAX_GIT_CALLS_IN_FLIGHT, PROTOCOL_MAX, PROTOCOL_VERSION,
};
use crate::worker::{Worker, PREAMBLE};
use crate::{ConnectFailureKind, RemoteError, StartReason, WaitEnd};

/// How long a new connection may take to reach the worker's hello: ssh
/// resolves, authenticates and starts a shell first.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
// The silence a call tolerates, between bytes, never a frame's whole
// transfer: a large reply may take longer than this to arrive whole on a
// slow link as long as it keeps moving. One rule, by where the call is
// (#191, [`silence_budget`]):
// - Before the first byte of its reply: this bound. A connection that went
//   dead while pooled (a sleep, a NAT timeout, no RST) fails fast.
// - Once the reply has started, anywhere (inside a frame, between the
//   frames of one reply, after a heartbeat): REPLY_IDLE_FACTOR times it.
//   From then on a pause says only that the link or the process froze: a
//   worker of protocol 2 and up sends a heartbeat every second while its
//   work runs, whatever the work is, so a live worker is never this quiet.
// The call's absolute deadline bounds both. A close or quit does not wait
// for either: it ends a call by its cancel token.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How much longer than the idle bound a reply that has started may pause.
const REPLY_IDLE_FACTOR: u32 = 6;

/// The silence a call tolerates: `idle` until the first byte of its reply,
/// [`REPLY_IDLE_FACTOR`] times that once the reply has started (#191).
fn silence_budget(idle: Duration, reply_started: bool) -> Duration {
	if reply_started {
		idle.saturating_mul(REPLY_IDLE_FACTOR)
	} else {
		idle
	}
}
/// How often a wait re-checks its deadline, its cancel token and the
/// stream's last-byte clock.
const WAIT_SLICE: Duration = Duration::from_millis(50);
/// Bytes a login shell may print before the preamble.
const MAX_BANNER: usize = 64 * 1024;
/// Bytes of the transport's stderr kept for an error message.
const MAX_STDERR: usize = 8 * 1024;
/// Idle connections kept per worker: each is an ssh process.
const POOL: usize = 2;
const PREPARED_IMPORT_LEASES: usize = 2;
/// In-flight starts, shared across Clients for the same host. Established
/// workers and idle connections do not hold a handshake slot.
const HANDSHAKES_PER_HOST: usize = 2;
/// Frames buffered between the worker's stdout and the caller. During a
/// call a full queue BLOCKS the pump on `send` — backpressure, so a reply
/// larger than the queue still reaches a slow consumer whole. While the
/// connection sits idle the queue plus one blocking `send` bound what a
/// speaking worker can make the master retain: [`FRAME_QUEUE`] × 8 MiB.
const FRAME_QUEUE: usize = 16;
pub const CALL_LIMIT_DEFAULT: Duration = Duration::from_secs(30);
/// A paste Apply: longer than the worker's own deadline for it, so the
/// master always hears how the write ended.
pub const APPLY_CALL_LIMIT: Duration =
	Duration::from_secs(crate::jobs::APPLY_DEADLINE.as_secs() + 60);

/// How a master reaches a worker.
#[derive(Clone)]
pub enum Transport {
	/// A command whose stdin and stdout speak the protocol, after
	/// [`PREAMBLE`]: `ssh … snip serve --stdio` ([`crate::ssh::command`]).
	Command(Vec<OsString>),
	/// A worker in this process, for tests.
	#[doc(hidden)]
	InProcess(Arc<Worker>),
}

/// A worker as the master knows it: what to show and how to start it.
#[derive(Clone)]
pub struct RemoteHost {
	/// The ssh host name, as the user picked it.
	pub name: String,
	pub transport: Transport,
}

impl std::fmt::Debug for RemoteHost {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("RemoteHost").field(&self.name).finish()
	}
}

/// The same host started the same way.
impl PartialEq for RemoteHost {
	fn eq(&self, other: &Self) -> bool {
		self.name == other.name
			&& match (&self.transport, &other.transport) {
				(Transport::Command(a), Transport::Command(b)) => a == b,
				(Transport::InProcess(a), Transport::InProcess(b)) => {
					Arc::ptr_eq(a, b)
				}
				_ => false,
			}
	}
}

impl Eq for RemoteHost {}

impl RemoteHost {
	/// A worker in this process, for tests.
	#[doc(hidden)]
	pub fn in_process(worker: Arc<Worker>) -> Self {
		Self {
			name: "test-worker".into(),
			transport: Transport::InProcess(worker),
		}
	}

	/// `host` reached through ssh, as [`crate::ssh::command`] spells it.
	pub fn ssh(host: &str) -> Self {
		Self {
			name: host.to_string(),
			transport: Transport::Command(crate::ssh::command(host)),
		}
	}
}

#[derive(PartialEq, Eq)]
enum TransportIdentity {
	Command(Vec<OsString>),
	InProcess(usize),
}

#[derive(PartialEq, Eq)]
struct HandshakeKey(String, TransportIdentity);

type HandshakeRegistry = Mutex<Vec<(HandshakeKey, Weak<HandshakeGate>)>>;

#[derive(Default)]
struct HandshakeGate {
	active: Mutex<usize>,
	waiting: AtomicUsize,
	changed: Condvar,
}

impl HandshakeGate {
	fn for_host(host: &RemoteHost) -> Arc<Self> {
		static GATES: OnceLock<HandshakeRegistry> = OnceLock::new();
		let key = HandshakeKey(
			host.name.clone(),
			match &host.transport {
				Transport::Command(args) => {
					TransportIdentity::Command(args.clone())
				}
				Transport::InProcess(worker) => {
					TransportIdentity::InProcess(Arc::as_ptr(worker) as usize)
				}
			},
		);
		let mut gates = GATES
			.get_or_init(Mutex::default)
			.lock()
			.unwrap_or_else(PoisonError::into_inner);
		gates.retain(|(_, gate)| gate.strong_count() > 0);
		if let Some(gate) = gates
			.iter()
			.find(|(stored, _)| *stored == key)
			.and_then(|(_, gate)| gate.upgrade())
		{
			return gate;
		}
		let gate = Arc::new(Self::default());
		gates.push((key, Arc::downgrade(&gate)));
		gate
	}

	fn acquire(
		self: &Arc<Self>,
		cancel: Option<&CancelToken>,
		deadline: Instant,
	) -> Result<HandshakeGuard, RemoteError> {
		let mut active =
			self.active.lock().unwrap_or_else(PoisonError::into_inner);
		loop {
			if cancel.is_some_and(|token| token.is_cancelled()) {
				return Err(RemoteError::Cancelled);
			}
			let remaining = deadline.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				// Nothing was sent: the wait was for a slot, not the worker.
				return Err(RemoteError::Start {
					kind: ConnectFailureKind::TimedOut,
					reason: None,
					detail: Datum::default(),
					message: "timed out waiting for a connection slot to that \
					          machine: other connections there are still \
					          starting; try again"
						.into(),
				});
			}
			if *active < HANDSHAKES_PER_HOST {
				*active += 1;
				return Ok(HandshakeGuard(self.clone()));
			}
			self.waiting.fetch_add(1, Ordering::SeqCst);
			active = self
				.changed
				.wait_timeout(active, remaining.min(WAIT_SLICE))
				.unwrap_or_else(PoisonError::into_inner)
				.0;
			self.waiting.fetch_sub(1, Ordering::SeqCst);
		}
	}
}

struct HandshakeGuard(Arc<HandshakeGate>);

impl Drop for HandshakeGuard {
	fn drop(&mut self) {
		let mut active =
			self.0.active.lock().unwrap_or_else(PoisonError::into_inner);
		*active -= 1;
		self.0.changed.notify_one();
	}
}

/// One frame in either direction, with a read deadline.
pub(crate) trait FrameIo {
	fn recv(
		&mut self,
		timeout: Duration,
	) -> Result<Option<Response>, RemoteError>;

	/// The next frame while bytes keep arriving: `idle` bounds the SILENCE
	/// before the call's reply starts, not the arrival of a whole frame — a
	/// large reply may take longer than that to cross a slow link. Once the
	/// reply has started, a pause anywhere in it may run to
	/// [`REPLY_IDLE_FACTOR`] times `idle` ([`silence_budget`]). The wait also
	/// ends at the absolute `deadline` or on `cancel`. The default has no
	/// byte clock and simply reads the next frame (tests feed memory
	/// streams).
	fn recv_idle(
		&mut self,
		idle: Duration,
		deadline: Instant,
		cancel: Option<&CancelToken>,
	) -> Result<Option<Response>, RemoteError> {
		let _ = (deadline, cancel);
		self.recv(idle)
	}

	/// The error for a call past its absolute deadline. A stream with a
	/// byte clock says how much arrived and for how long; the default has
	/// none.
	fn overdue(&self) -> RemoteError {
		RemoteError::TimedOut
	}
}

/// A plain stream has no deadline; tests feed one frames from memory.
impl<S: IoRead + IoWrite> FrameIo for S {
	fn recv(&mut self, _: Duration) -> Result<Option<Response>, RemoteError> {
		Ok(read_frame::<Response>(self)?)
	}
}

/// Milliseconds since this process started (offset by one hour so a clock
/// can be set into the past near process start without saturating at zero):
/// the base of the shared last-byte clock.
fn now_millis() -> u64 {
	static START: OnceLock<Instant> = OnceLock::new();
	const BASE_MILLIS: u64 = 60 * 60 * 1000;
	BASE_MILLIS + START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The stream's progress, shared with the read thread: when it last produced
/// a byte, and how many bytes it produced.
pub(crate) struct StreamClock {
	last_byte: AtomicU64,
	received: AtomicU64,
}

impl StreamClock {
	fn new() -> Arc<Self> {
		Arc::new(Self {
			last_byte: AtomicU64::new(now_millis()),
			received: AtomicU64::new(0),
		})
	}

	fn touch(&self, n: usize) {
		self.last_byte.store(now_millis(), Ordering::Relaxed);
		self.received.fetch_add(n as u64, Ordering::Relaxed);
	}

	fn restart(&self) {
		self.last_byte.store(now_millis(), Ordering::Relaxed);
	}

	fn silent_ms(&self) -> u64 {
		now_millis().saturating_sub(self.last_byte.load(Ordering::Relaxed))
	}

	fn received(&self) -> u64 {
		self.received.load(Ordering::Relaxed)
	}
}

/// Records when the stream last produced a byte, so a wait can tell a
/// frame that is still moving from a worker that went quiet.
struct Progress<R> {
	inner: R,
	clock: Arc<StreamClock>,
}

impl<R> Progress<R> {
	fn new(inner: R, clock: Arc<StreamClock>) -> Self {
		Self { inner, clock }
	}
}

impl<R: IoRead> IoRead for Progress<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		if n > 0 {
			self.clock.touch(n);
		}
		Ok(n)
	}
}

type Frames = Receiver<io::Result<Option<Response>>>;

/// One worker process (or in-process worker) and its hello.
pub struct Connection {
	writer: Box<dyn IoWrite + Send>,
	frames: Frames,
	child: Option<Child>,
	stderr: Arc<Mutex<Vec<u8>>>,
	worker_name: String,
	home: Option<String>,
	version: u32,
	/// A read timed out or failed: the stream may be mid-frame.
	broken: bool,
	pub(crate) frames_seen: usize,
	/// Frames queued and not yet consumed, shared with the read thread.
	/// Above zero while idle, the worker is speaking without a request.
	retained: Arc<AtomicUsize>,
	/// True while this connection's master is inside an exchange (request
	/// sent, its answer not fully consumed): frames may arrive, and the
	/// pump applies backpressure instead of hanging up. Shared with the
	/// read thread.
	in_flight: Arc<AtomicBool>,
	/// When the pump last pulled a byte off the stream, and how many bytes
	/// it pulled. Shared with the read thread; a wait ends when the stream
	/// goes silent past [`IO_TIMEOUT`].
	last_byte: Arc<StreamClock>,
	/// When the current call started, and the stream's byte count at that
	/// point: the call's reply is what arrives past it (#191).
	call_started: Instant,
	call_base: u64,
}

impl Connection {
	/// Starts the worker and says hello.
	pub fn open(
		transport: &Transport,
		my_name: &str,
	) -> Result<Self, RemoteError> {
		Self::open_with(transport, my_name, None)
	}

	/// [`Self::open`] that `cancel` ends: an ssh that is still connecting
	/// or waiting for the hello is killed (on drop) and the open fails
	/// with [`RemoteError::Cancelled`], instead of sitting out ssh's
	/// connect timeout and the hello's.
	pub fn open_with(
		transport: &Transport,
		my_name: &str,
		cancel: Option<&CancelToken>,
	) -> Result<Self, RemoteError> {
		Self::open_until(
			transport,
			my_name,
			cancel,
			Instant::now() + CONNECT_TIMEOUT,
		)
	}

	fn open_until(
		transport: &Transport,
		my_name: &str,
		cancel: Option<&CancelToken>,
		deadline: Instant,
	) -> Result<Self, RemoteError> {
		if cancel.is_some_and(|c| c.is_cancelled()) {
			return Err(RemoteError::Cancelled);
		}
		let (writer, frames, child, stderr, retained, in_flight, last_byte) =
			match transport {
				Transport::Command(argv) => spawn(argv)?,
				Transport::InProcess(worker) => {
					let (req_w, res_r) = worker.connect_in_process()?;
					let writer: Box<dyn IoWrite + Send> = Box::new(req_w);
					let (frames, retained, in_flight, last_byte) =
						read_frames(res_r);
					(
						writer,
						frames,
						None,
						Arc::default(),
						retained,
						in_flight,
						last_byte,
					)
				}
			};
		let mut conn = Self {
			writer,
			frames,
			child,
			stderr,
			worker_name: String::new(),
			home: None,
			version: 1,
			broken: false,
			frames_seen: 0,
			retained,
			in_flight,
			last_byte,
			call_started: Instant::now(),
			call_base: 0,
		};
		let hello = Request::Hello {
			version: PROTOCOL_VERSION,
			name: my_name.to_string(),
			max_version: Some(PROTOCOL_MAX),
		};
		// The hello exchange is a call: its reply may arrive before the
		// master waits for it, so the pump must see it as in flight.
		conn.in_flight.store(true, Ordering::SeqCst);
		let reply =
			conn.send_guarded(&hello, cancel, deadline).and_then(|()| {
				conn.recv_hello(
					deadline.saturating_duration_since(Instant::now()),
					cancel,
				)
			});
		conn.in_flight.store(false, Ordering::SeqCst);
		match reply {
			Ok(Some(Response::Hello {
				name,
				home,
				max_version,
				..
			})) => {
				conn.worker_name = name;
				conn.home = home;
				conn.version = max_version.unwrap_or(1).clamp(1, PROTOCOL_MAX);
				Ok(conn)
			}
			Ok(Some(Response::Error {
				code,
				message,
				patch,
				reason,
			})) => Err(refusal(code, message, patch, reason)),
			Ok(Some(_)) => Err(RemoteError::Protocol("expected hello".into())),
			Ok(None) | Err(RemoteError::Io(_)) | Err(RemoteError::TimedOut) => {
				let cause = match &reply {
					Err(RemoteError::TimedOut) => StartCause::NoAnswer,
					Err(RemoteError::Io(err))
						if err.kind() == io::ErrorKind::InvalidData =>
					{
						StartCause::BadData(err.to_string())
					}
					Err(RemoteError::Io(err)) => StartCause::Io(err.kind()),
					_ => StartCause::Ended,
				};
				Err(conn.start_error(cause, deadline))
			}
			Err(err) => Err(err),
		}
	}

	/// The hello reply within `timeout`, polling `cancel` meanwhile.
	fn recv_hello(
		&mut self,
		timeout: Duration,
		cancel: Option<&CancelToken>,
	) -> Result<Option<Response>, RemoteError> {
		let deadline = Instant::now() + timeout;
		loop {
			let now = Instant::now();
			if now >= deadline {
				return Err(RemoteError::TimedOut);
			}
			match self.frames.recv_timeout(WAIT_SLICE.min(deadline - now)) {
				Ok(frame) => {
					// The frame left the queue; the pump may read on.
					self.retained.fetch_sub(1, Ordering::Relaxed);
					return Ok(frame?);
				}
				Err(RecvTimeoutError::Disconnected) => return Ok(None),
				Err(RecvTimeoutError::Timeout) => {}
			}
			if cancel.is_some_and(|c| c.is_cancelled()) {
				return Err(RemoteError::Cancelled);
			}
		}
	}

	/// Why the worker never said hello, from the transport's exit status
	/// and stderr: ssh's own message, or that the far end has no snip.
	fn start_error(
		&mut self,
		cause: StartCause,
		handshake_deadline: Instant,
	) -> RemoteError {
		let status = self.child.as_mut().and_then(|c| {
			let deadline = (Instant::now() + Duration::from_secs(2))
				.min(handshake_deadline);
			loop {
				match c.try_wait() {
					Ok(Some(status)) => return Some(status),
					Ok(None) if Instant::now() < deadline => {
						std::thread::sleep(Duration::from_millis(20))
					}
					_ => return None,
				}
			}
		});
		// The stderr reader holds the other reference until the pipe closes:
		// wait for it, or a fast exit is classified before its message lands.
		let deadline =
			(Instant::now() + Duration::from_secs(2)).min(handshake_deadline);
		while status.is_some()
			&& Arc::strong_count(&self.stderr) > 1
			&& Instant::now() < deadline
		{
			std::thread::sleep(Duration::from_millis(10));
		}
		let stderr = String::from_utf8_lossy(
			&self.stderr.lock().unwrap_or_else(PoisonError::into_inner),
		)
		.trim()
		.to_string();
		let code = status.and_then(|s| s.code());
		RemoteError::Start {
			kind: start_failure_kind(&cause, code, &stderr),
			reason: start_reason(&cause, code, &stderr),
			detail: Datum::transport_stderr(stderr.clone()),
			message: start_message(cause, code, &stderr),
		}
	}

	pub fn worker_name(&self) -> &str {
		&self.worker_name
	}

	/// The worker's home folder, where browsing starts.
	pub fn home(&self) -> Option<&str> {
		self.home.as_deref()
	}

	pub fn version(&self) -> u32 {
		self.version
	}

	pub fn frames_seen(&self) -> usize {
		self.frames_seen
	}

	/// Frames the worker sent that no call has consumed: above zero while
	/// the connection sits idle, it spoke without a request.
	fn unsolicited_frames(&self) -> usize {
		self.retained.load(Ordering::Relaxed)
	}

	pub fn call(
		&mut self,
		request: &Request,
		cancel: Option<&CancelToken>,
		limit: Duration,
	) -> Result<Response, RemoteError> {
		// The whole call holds to one absolute deadline: the send included,
		// not only the wait for the answer.
		let deadline = Instant::now() + limit;
		let mut frames = 0;
		self.call_started = Instant::now();
		self.call_base = self.last_byte.received();
		self.in_flight.store(true, Ordering::SeqCst);
		let res = self.send_guarded(request, cancel, deadline).and_then(|()| {
			self.last_byte.restart();
			exchange(self, cancel, IO_TIMEOUT, deadline, &mut frames)
		});
		self.in_flight.store(false, Ordering::SeqCst);
		self.frames_seen = frames;
		// A refusal is an answer: the connection is still good.
		if res.as_ref().is_err_and(|e| e.code().is_none()) {
			self.broken = true;
		}
		res
	}

	/// Writes `request` under the call's absolute deadline. The write runs
	/// on its own thread: a `ChildStdin` whose worker stopped reading
	/// blocks without end, and only the transport's death unblocks it, so
	/// on the deadline (or a cancel) the process is killed and the call
	/// fails in time. The writer is abandoned with the thread when it
	/// cannot come back; the connection is broken either way.
	fn send_guarded(
		&mut self,
		request: &Request,
		cancel: Option<&CancelToken>,
		deadline: Instant,
	) -> Result<(), RemoteError> {
		let version = self.version;
		let mut writer =
			std::mem::replace(&mut self.writer, Box::new(DeadWriter));
		let request = request.clone();
		let (done_tx, done_rx) = mpsc::channel();
		let _ = std::thread::Builder::new()
			.name("snip-remote-write".into())
			.spawn(move || {
				let result = write_request_for(&mut writer, &request, version);
				let _ = done_tx.send((writer, result));
			});
		loop {
			match done_rx.recv_timeout(Duration::from_millis(20)) {
				Ok((writer, result)) => {
					self.writer = writer;
					return result.map_err(RemoteError::from);
				}
				Err(RecvTimeoutError::Disconnected) => {
					return Err(RemoteError::Protocol(
						"the send thread died".into(),
					));
				}
				Err(RecvTimeoutError::Timeout) => {}
			}
			if Instant::now() >= deadline {
				self.kill_transport();
				return Err(RemoteError::TimedOut);
			}
			if cancel.is_some_and(|c| c.is_cancelled()) {
				self.kill_transport();
				return Err(RemoteError::Cancelled);
			}
		}
	}

	/// The error for a call whose wait ended without an answer: how it
	/// ended, and how much of the reply arrived in how long. The one place
	/// that builds it, so no exit reports less than another (#191).
	fn unanswered(&self, ended: WaitEnd) -> RemoteError {
		RemoteError::Stalled {
			silent: self.last_byte.silent_ms() as f64 / 1000.0,
			received: self.last_byte.received().saturating_sub(self.call_base),
			elapsed: self.call_started.elapsed().as_secs_f64(),
			ended,
		}
	}

	/// Kills the transport process, closing the pipes a stuck write or
	/// read is parked on. Without a process (an in-process worker) there
	/// is nothing to kill; its stuck writer thread is simply abandoned.
	fn kill_transport(&mut self) {
		if let Some(child) = self.child.as_mut() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

/// Placeholder for a writer that is in flight, or was abandoned with the
/// thread that blocked writing it.
struct DeadWriter;

impl io::Write for DeadWriter {
	fn write(&mut self, _: &[u8]) -> io::Result<usize> {
		Err(io::Error::new(
			io::ErrorKind::BrokenPipe,
			"the transport's writer was abandoned",
		))
	}
	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl FrameIo for Connection {
	fn overdue(&self) -> RemoteError {
		self.unanswered(WaitEnd::Deadline)
	}

	fn recv(
		&mut self,
		timeout: Duration,
	) -> Result<Option<Response>, RemoteError> {
		match self.frames.recv_timeout(timeout) {
			Ok(frame) => {
				// The frame left the queue; the pump may read on.
				self.retained.fetch_sub(1, Ordering::Relaxed);
				Ok(frame?)
			}
			Err(RecvTimeoutError::Timeout) => Err(RemoteError::TimedOut),
			Err(RecvTimeoutError::Disconnected) => Ok(None),
		}
	}

	fn recv_idle(
		&mut self,
		idle: Duration,
		deadline: Instant,
		cancel: Option<&CancelToken>,
	) -> Result<Option<Response>, RemoteError> {
		loop {
			match self.frames.recv_timeout(WAIT_SLICE) {
				Ok(frame) => {
					// The frame left the queue; the pump may read on.
					self.retained.fetch_sub(1, Ordering::Relaxed);
					return Ok(frame?);
				}
				Err(RecvTimeoutError::Disconnected) => return Ok(None),
				Err(RecvTimeoutError::Timeout) => {}
			}
			if cancel.is_some_and(|c| c.is_cancelled()) {
				return Err(RemoteError::Cancelled);
			}
			if Instant::now() >= deadline {
				return Err(self.unanswered(WaitEnd::Deadline));
			}
			// No byte for the call's silence budget: the connection or the
			// process froze. Bytes still moving (a large frame on a slow
			// link, a heartbeat) keep the wait alive, and so does a reply
			// that has begun, for REPLY_IDLE_FACTOR times longer (#191).
			let started = self.last_byte.received() > self.call_base;
			if self.last_byte.silent_ms()
				>= silence_budget(idle, started).as_millis() as u64
			{
				return Err(self.unanswered(WaitEnd::Silence));
			}
		}
	}
}

impl Drop for Connection {
	fn drop(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

/// What the transport did while the master waited for the hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartCause {
	Io(io::ErrorKind),
	/// The hello never arrived and the transport still runs: it hung, or
	/// the far machine is too busy to answer.
	NoAnswer,
	/// The stream itself said why: a login shell's banner over the cap, a
	/// frame that breaks the protocol. The message is the stream's own.
	BadData(String),
	/// The transport ended before the hello: classify by exit status and
	/// stderr.
	Ended,
}

/// The known reason the worker did not start, when there is one.
///
/// A deliberate exception to "no meaning read back from an error's English
/// text" (#225): this and [`start_failure_kind`] classify a foreign tool's
/// stderr (ssh, the remote shell) at the place the error is produced, where
/// the exit status still exists. Every consumer receives the typed result.
pub(crate) fn start_reason(
	cause: &StartCause,
	code: Option<i32>,
	stderr: &str,
) -> Option<StartReason> {
	match cause {
		StartCause::NoAnswer if stderr.is_empty() => {
			Some(StartReason::NoAnswer)
		}
		StartCause::NoAnswer | StartCause::BadData(_) => None,
		StartCause::Ended | StartCause::Io(_) => {
			if code == Some(127) || stderr.contains("unrecognized subcommand") {
				Some(StartReason::NoWorker)
			} else if stderr.contains("Permission denied") {
				Some(StartReason::KeyLogin)
			} else if stderr.is_empty() {
				Some(StartReason::Closed)
			} else {
				None
			}
		}
	}
}

/// The user-facing reason a worker did not start.
pub(crate) fn start_message(
	cause: StartCause,
	code: Option<i32>,
	stderr: &str,
) -> String {
	match (start_reason(&cause, code, stderr), cause) {
		(Some(StartReason::NoAnswer), _) => {
			"the worker on that machine did not answer in time; check its \
			 load and connection, then try again"
				.into()
		}
		(Some(StartReason::NoWorker), _) => {
			"snip is not installed on that machine, or is too old for \
			 ssh connections; install snip-sync there"
				.into()
		}
		(Some(StartReason::KeyLogin), _) => format!(
			"ssh could not log in without a password; set up key login \
			 (ssh-copy-id) first: {stderr}"
		),
		(Some(StartReason::Closed), _) => {
			"the remote end closed the connection before snip started".into()
		}
		// Built by `spawn` with its own message, never classified here.
		(Some(StartReason::NoCommand | StartReason::CannotRun), _) => {
			stderr.to_string()
		}
		// The stream's own message: a banner, an out-of-step frame.
		(None, StartCause::BadData(message)) => message,
		// Whatever the transport said outranks the bare timeout.
		(None, _) => stderr.to_string(),
	}
}

/// Classify at the transport boundary, while exit status and the original
/// failure still exist; consumers receive the type and unchanged message.
fn start_failure_kind(
	cause: &StartCause,
	code: Option<i32>,
	stderr: &str,
) -> ConnectFailureKind {
	let lower = stderr.to_ascii_lowercase();
	if lower.contains("permission denied")
		|| lower.contains("host key verification failed")
		|| lower.contains("remote host identification has changed")
	{
		return ConnectFailureKind::Authentication;
	}
	if code == Some(127) || lower.contains("unrecognized subcommand") {
		return ConnectFailureKind::WorkerUnavailable;
	}
	match cause {
		StartCause::NoAnswer => ConnectFailureKind::TimedOut,
		StartCause::BadData(_) => ConnectFailureKind::Protocol,
		StartCause::Io(kind) if stderr.is_empty() => {
			ConnectFailureKind::Io(*kind)
		}
		StartCause::Ended | StartCause::Io(_)
			if stderr.is_empty()
				|| [
					"connection reset",
					"connection closed",
					"connection timed out",
					"operation timed out",
					"connection refused",
					"no route to host",
					"broken pipe",
					"connection aborted",
				]
				.iter()
				.any(|message| lower.contains(message)) =>
		{
			ConnectFailureKind::TransportClosed
		}
		StartCause::Ended | StartCause::Io(_) => ConnectFailureKind::Other,
	}
}

/// A started worker: where to write requests, the frames it answers, the
/// process to stop, what it said on stderr, its retained-frame counter,
/// the in-flight flag shared with the read thread, and the shared
/// last-byte clock.
type Started = (
	Box<dyn IoWrite + Send>,
	Frames,
	Option<Child>,
	Arc<Mutex<Vec<u8>>>,
	Arc<AtomicUsize>,
	Arc<AtomicBool>,
	Arc<StreamClock>,
);

fn spawn(argv: &[OsString]) -> Result<Started, RemoteError> {
	let (program, args) = argv.split_first().ok_or_else(|| {
		// The same text the CLI always printed, typed for a front end.
		RemoteError::Start {
			kind: ConnectFailureKind::WorkerUnavailable,
			reason: Some(StartReason::NoCommand),
			detail: Datum::default(),
			message: "no command to start the worker".into(),
		}
	})?;
	let mut child = Command::new(program)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|err| {
			let program = program.to_string_lossy();
			RemoteError::Start {
				kind: ConnectFailureKind::WorkerUnavailable,
				reason: Some(StartReason::CannotRun),
				// Data only: the program and what the OS said.
				detail: Datum::spawn_failure(&program, &err),
				message: format!("cannot run {program}: {err}"),
			}
		})?;
	let stdin: ChildStdin = child.stdin.take().expect("piped stdin");
	let stdout = child.stdout.take().expect("piped stdout");
	let stderr_pipe = child.stderr.take().expect("piped stderr");
	// Preallocated to its ceiling: the reader never writes past MAX_STDERR,
	// so the capacity (and any charge for it) never changes.
	let stderr = Arc::new(Mutex::new(Vec::with_capacity(MAX_STDERR)));
	let keep = stderr.clone();
	std::thread::Builder::new()
		.name("snip-remote-stderr".into())
		.spawn(move || {
			let mut pipe = stderr_pipe;
			let mut chunk = [0u8; 1024];
			while let Ok(n) = pipe.read(&mut chunk) {
				if n == 0 {
					break;
				}
				let mut buf =
					keep.lock().unwrap_or_else(PoisonError::into_inner);
				let room = MAX_STDERR.saturating_sub(buf.len());
				buf.extend_from_slice(&chunk[..n.min(room)]);
			}
		})?;
	let mut stdout = BufReader::new(stdout);
	let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
	let retained = Arc::new(AtomicUsize::new(0));
	let in_flight = Arc::new(AtomicBool::new(false));
	let last_byte = StreamClock::new();
	let pump_retained = Arc::clone(&retained);
	let pump_in_flight = Arc::clone(&in_flight);
	let pump_last_byte = Arc::clone(&last_byte);
	std::thread::Builder::new()
		.name("snip-remote-read".into())
		.spawn(move || {
			if let Err(err) = skip_banner(&mut stdout) {
				let _ = tx.send(Err(err));
				return;
			}
			pump_frames(
				Progress::new(stdout, pump_last_byte),
				tx,
				pump_retained,
				pump_in_flight,
			);
		})?;
	let writer: Box<dyn IoWrite + Send> = Box::new(stdin);
	Ok((
		writer,
		rx,
		Some(child),
		stderr,
		retained,
		in_flight,
		last_byte,
	))
}

/// Reads up to the [`PREAMBLE`] line, skipping what a login shell printed.
/// A clean end first is not an error: the caller reports the exit.
pub(crate) fn skip_banner(r: &mut impl BufRead) -> io::Result<()> {
	let mut seen = 0usize;
	let mut line = Vec::new();
	loop {
		line.clear();
		let n = r.take(MAX_BANNER as u64).read_until(b'\n', &mut line)?;
		if n == 0 {
			return Err(io::Error::new(
				io::ErrorKind::UnexpectedEof,
				"the worker ended before starting",
			));
		}
		if line.trim_ascii_end() == PREAMBLE.as_bytes() {
			return Ok(());
		}
		seen += n;
		if seen > MAX_BANNER {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"the remote shell printed too much before snip started",
			));
		}
	}
}

fn read_frames(
	r: impl IoRead + Send + 'static,
) -> (Frames, Arc<AtomicUsize>, Arc<AtomicBool>, Arc<StreamClock>) {
	let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
	let retained = Arc::new(AtomicUsize::new(0));
	let in_flight = Arc::new(AtomicBool::new(false));
	let last_byte = StreamClock::new();
	let pump_retained = Arc::clone(&retained);
	let pump_in_flight = Arc::clone(&in_flight);
	let pump_last_byte = Arc::clone(&last_byte);
	std::thread::spawn(move || {
		pump_frames(
			Progress::new(r, pump_last_byte),
			tx,
			pump_retained,
			pump_in_flight,
		)
	});
	(rx, retained, in_flight, last_byte)
}

/// Reads frames until the worker stops or the consumer does. A frame that
/// ARRIVES while an exchange is [`in_flight`] may be part of its answer:
/// the blocking `send` is the backpressure that carries a reply larger
/// than the queue to a slow consumer. A frame that arrives while no call
/// is running is out of step with the protocol — it is queued (accounted
/// in `retained`, so the pooled connection is never reused) and the pump
/// hangs up, which bounds what an unsolicited worker can make the master
/// retain. Dropping the receiver ends a pump parked in `send`.
fn pump_frames(
	mut r: impl IoRead,
	tx: SyncSender<io::Result<Option<Response>>>,
	retained: Arc<AtomicUsize>,
	in_flight: Arc<AtomicBool>,
) {
	loop {
		let frame = read_frame::<Response>(&mut r);
		let end = !matches!(frame, Ok(Some(_)));
		// Read BEFORE send: the consumer cannot finish the call this frame
		// belongs to before it is delivered, so a reply's last frame is
		// never mistaken for an unsolicited one.
		let unsolicited = !in_flight.load(Ordering::SeqCst);
		retained.fetch_add(1, Ordering::Relaxed);
		if tx.send(frame).is_err() {
			retained.fetch_sub(1, Ordering::Relaxed);
			return;
		}
		if end || unsolicited {
			return;
		}
	}
}

/// Waits for `request`'s answer, skipping heartbeats. Two bounds hold at
/// once: the stream must not go SILENT past `idle` (no byte at all — a
/// large reply crossing a slow link keeps moving and stays alive), and the
/// whole call holds to the absolute `deadline`, so a worker cannot stretch
/// one call by dripping frames. The request itself was sent by
/// [`Connection::send_guarded`] under the same deadline.
pub(crate) fn exchange(
	io: &mut impl FrameIo,
	cancel: Option<&CancelToken>,
	idle: Duration,
	deadline: Instant,
	frames: &mut usize,
) -> Result<Response, RemoteError> {
	*frames = 0;
	// A copy's text, or a large reply's JSON, arriving ahead of it.
	let mut text = String::new();
	let over_clipboard = |text: &str| {
		(text.len() > snip_core::transfer::CLIPBOARD_PAYLOAD_MAX).then(|| {
			RemoteError::Protocol(
				"the copied text is over the clipboard limit".into(),
			)
		})
	};
	loop {
		match io.recv_idle(idle, deadline, cancel)? {
			Some(response) => {
				*frames += 1;
				if Instant::now() >= deadline {
					return Err(io.overdue());
				}
				match response {
					Response::Chunk { data } => {
						if text.len() + data.len() > JOINED_MAX {
							return Err(RemoteError::Protocol(
								"the worker's reply is too large".into(),
							));
						}
						text.push_str(&data);
					}
					Response::Joined => {
						return match serde_json::from_str::<Response>(&text) {
							Ok(Response::Error {
								code,
								message,
								patch,
								reason,
							}) => Err(refusal(code, message, patch, reason)),
							Ok(
								Response::Chunk { .. }
								| Response::Joined
								| Response::Pending,
							)
							| Err(_) => Err(RemoteError::Protocol(
								"the worker's chunked reply is not valid"
									.into(),
							)),
							Ok(response) => Ok(response),
						};
					}
					Response::Copied(mut out) if !text.is_empty() => {
						if let Some(err) = over_clipboard(&text) {
							return Err(err);
						}
						out.payload = text;
						return Ok(Response::Copied(out));
					}
					Response::CommitsCopied(mut out) if !text.is_empty() => {
						if let Some(err) = over_clipboard(&text) {
							return Err(err);
						}
						out.text = text;
						return Ok(Response::CommitsCopied(out));
					}
					Response::Pending => {
						if cancel.is_some_and(|c| c.is_cancelled()) {
							return Err(RemoteError::Cancelled);
						}
					}
					Response::Error {
						code,
						message,
						patch,
						reason,
					} => {
						return Err(refusal(code, message, patch, reason));
					}
					response => return Ok(response),
				}
			}
			None => {
				return Err(RemoteError::Io(
					crate::iofault::IoFault::WorkerClosed.into(),
				));
			}
		}
	}
}

/// Pure resend condition for failed calls on reused connections.
pub(crate) fn should_resend(
	reused: bool,
	frames_seen: usize,
	err: &RemoteError,
) -> bool {
	if !reused || frames_seen > 0 {
		return false;
	}
	match err {
		RemoteError::Io(e) => !matches!(
			e.kind(),
			io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
		),
		_ => false,
	}
}

/// In-flight limiter for Git requests per Client.
#[derive(Debug)]
pub(crate) struct GitLimiter {
	in_flight: Mutex<usize>,
	cvar: Condvar,
}

impl GitLimiter {
	pub(crate) fn new() -> Self {
		Self {
			in_flight: Mutex::new(0),
			cvar: Condvar::new(),
		}
	}

	pub(crate) fn acquire(
		&self,
		cancel: Option<&CancelToken>,
		limit: Duration,
	) -> Result<GitLimiterGuard<'_>, RemoteError> {
		let start = Instant::now();
		let mut count = self
			.in_flight
			.lock()
			.unwrap_or_else(PoisonError::into_inner);
		loop {
			if cancel.is_some_and(|c| c.is_cancelled()) {
				return Err(RemoteError::Cancelled);
			}
			if start.elapsed() >= limit {
				return Err(RemoteError::TimedOut);
			}
			if *count < MAX_GIT_CALLS_IN_FLIGHT {
				*count += 1;
				return Ok(GitLimiterGuard { limiter: self });
			}
			let remaining = limit.saturating_sub(start.elapsed());
			let wait_time = remaining.min(Duration::from_millis(100));
			let (new_count, _) = self
				.cvar
				.wait_timeout(count, wait_time)
				.unwrap_or_else(PoisonError::into_inner);
			count = new_count;
		}
	}
}

#[derive(Debug)]
pub(crate) struct GitLimiterGuard<'a> {
	limiter: &'a GitLimiter,
}

impl<'a> Drop for GitLimiterGuard<'a> {
	fn drop(&mut self) {
		let mut count = self
			.limiter
			.in_flight
			.lock()
			.unwrap_or_else(PoisonError::into_inner);
		*count -= 1;
		self.limiter.cvar.notify_one();
	}
}

#[derive(Default)]
struct PreparedImports {
	active: AtomicUsize,
}

impl PreparedImports {
	fn reserve(self: &Arc<Self>) -> Option<PreparedImportPermit> {
		let cleanup = prepared_cleanup_sender()?;
		let mut active = self.active.load(Ordering::SeqCst);
		loop {
			if active >= PREPARED_IMPORT_LEASES {
				return None;
			}
			match self.active.compare_exchange_weak(
				active,
				active + 1,
				Ordering::SeqCst,
				Ordering::SeqCst,
			) {
				Ok(_) => {
					return Some(PreparedImportPermit {
						owner: self.clone(),
						cleanup,
					})
				}
				Err(now) => active = now,
			}
		}
	}
}

struct PreparedImportPermit {
	owner: Arc<PreparedImports>,
	cleanup: mpsc::Sender<PreparedCleanup>,
}

impl Drop for PreparedImportPermit {
	fn drop(&mut self) {
		self.owner.active.fetch_sub(1, Ordering::SeqCst);
	}
}

struct PreparedConnection {
	connection: Connection,
	token: String,
	_permit: PreparedImportPermit,
}

struct PreparedCleanup {
	connection: Connection,
	permit: PreparedImportPermit,
}

struct PendingPreparedChild {
	child: Child,
	_permit: PreparedImportPermit,
	started: Instant,
	warned: bool,
}

/// A dropped preview can originate on the UI thread. Reap its worker on
/// one cleanup thread, polling every child without a blocking wait. A
/// child that cannot yet be reaped continues to occupy its client's slot.
fn prepared_cleanup_sender() -> Option<mpsc::Sender<PreparedCleanup>> {
	static CLEANUP: OnceLock<Option<mpsc::Sender<PreparedCleanup>>> =
		OnceLock::new();
	CLEANUP.get_or_init(|| {
		let (sender, receiver) = mpsc::channel::<PreparedCleanup>();
		std::thread::Builder::new().name("snip-prepared-cleanup".into()).spawn(move || {
			let mut pending: Vec<PendingPreparedChild> = Vec::new();
			loop {
				match receiver.recv_timeout(Duration::from_millis(50)) {
					Ok(PreparedCleanup { mut connection, permit }) => {
						let child = connection.child.take();
						// Closing the pipes ends the worker's read. Remove the
						// child first so Connection::drop cannot call wait().
						drop(connection);
						if let Some(mut child) = child {
							let _ = child.kill();
							pending.push(PendingPreparedChild { child, _permit: permit, started: Instant::now(), warned: false });
						}
					}
					Err(RecvTimeoutError::Disconnected) if pending.is_empty() => break,
					Err(_) => {}
				}
				pending.retain_mut(|pending| {
					if matches!(pending.child.try_wait(), Ok(Some(_))) { return false; }
					if !pending.warned && pending.started.elapsed() >= Duration::from_secs(30) {
						eprintln!("[prepared paste] worker cleanup is still pending; its lease slot remains reserved");
						pending.warned = true;
					}
					true
				});
			}
		}).ok()?;
		Some(sender)
	}).clone()
}

impl Drop for PreparedImportLease {
	fn drop(&mut self) {
		let state =
			self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
		if let PreparedImportState::Ready(ready) =
			std::mem::replace(state, PreparedImportState::Used)
		{
			let PreparedConnection {
				connection,
				token: _,
				_permit: permit,
			} = *ready;
			let sender = permit.cleanup.clone();
			if let Err(failed) =
				sender.send(PreparedCleanup { connection, permit })
			{
				// The reaper must never panic, but even a disconnected sender
				// must not route an unbounded child wait onto the UI thread.
				let PreparedCleanup {
					mut connection,
					permit,
				} = failed.0;
				let child = connection.child.take();
				drop(connection);
				if let Some(mut child) = child {
					let _ = child.kill();
					if !matches!(child.try_wait(), Ok(Some(_))) {
						// Cleanup is unconfirmed: do not grant another prepared
						// slot and conceal a potentially still-running worker.
						std::mem::forget(permit);
						eprintln!("[prepared paste] cleanup service unavailable; worker cleanup unconfirmed");
					}
				}
			}
		}
	}
}

enum PreparedImportState {
	Ready(Box<PreparedConnection>),
	Applying,
	Used,
	OutcomeUnknown,
}

/// What a prepared lease's token keeps on the worker: a file import's
/// preview or a commit replay's preview. A lease applies only its own kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreparedKind {
	Import,
	Replay,
}

/// The worker connection that owns a preview token. Clones of a file plan
/// share this one-use state; it never owns the Client itself.
pub(crate) struct PreparedImportLease {
	owner: Arc<PreparedImports>,
	workspace: String,
	dest: String,
	text_hash: [u8; 32],
	mapping: PasteMapping,
	digest: String,
	kind: PreparedKind,
	/// The connection's owned buffers, charged while the lease holds it
	/// (Ready and Applying). Fixed when the lease is made, so no transition
	/// between those states can raise an admitted charge.
	connection_bytes: usize,
	state: Mutex<PreparedImportState>,
}

/// Upper bound of what a lease owns through its connection: the boxed
/// `PreparedConnection`, its strings, and the stderr buffer at its
/// preallocated ceiling. Read once, when the lease is made.
fn connection_footprint(
	connection: &Connection,
	token_capacity: usize,
) -> usize {
	std::mem::size_of::<PreparedConnection>()
		.saturating_add(token_capacity)
		.saturating_add(connection.worker_name.capacity())
		.saturating_add(connection.home.as_ref().map_or(0, String::capacity))
		.saturating_add(MAX_STDERR)
}

impl std::fmt::Debug for PreparedImportLease {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PreparedImportLease")
			.field("workspace", &self.workspace)
			.field("dest", &self.dest)
			.finish_non_exhaustive()
	}
}

impl PreparedImportLease {
	/// Owned metadata and connection strings. No source body is retained
	/// here; transport frame queues remain separately bounded by FRAME_QUEUE.
	pub(crate) fn retained_heap_bytes(&self) -> usize {
		let mut bytes =
			std::mem::size_of::<Self>() + 2 * std::mem::size_of::<usize>();
		bytes = bytes
			.saturating_add(self.workspace.capacity())
			.saturating_add(self.dest.capacity())
			.saturating_add(self.digest.capacity())
			.saturating_add(self.mapping.header_format.capacity())
			.saturating_add(
				self.mapping.prefixes.capacity()
					* std::mem::size_of::<(String, String)>(),
			);
		for (prefix, folder) in &self.mapping.prefixes {
			bytes = bytes
				.saturating_add(prefix.capacity())
				.saturating_add(folder.capacity());
		}
		// Ready and Applying hold the same charge: an Apply that is refused
		// before any write restores Ready, and must not grow an admitted state.
		let holds_connection = matches!(
			&*self.state.lock().unwrap_or_else(PoisonError::into_inner),
			PreparedImportState::Ready(_) | PreparedImportState::Applying
		);
		if holds_connection {
			bytes = bytes.saturating_add(self.connection_bytes);
		}
		bytes
	}
}

impl PreparedImportLease {
	/// Ready → Applying. The one connection the lease reserved goes to this
	/// Apply; any other state refuses without touching the connection.
	fn begin_apply(&self) -> Result<Box<PreparedConnection>, RemoteError> {
		let mut state =
			self.state.lock().unwrap_or_else(PoisonError::into_inner);
		match &*state {
			PreparedImportState::Ready(_) => {}
			PreparedImportState::Applying => return Err(RemoteError::Refused { code: ErrorCode::Busy, message: "the prepared paste is already applying".into(), reason: None }),
			PreparedImportState::Used => return Err(prepared_stale("the prepared paste has already been used; preview again")),
			PreparedImportState::OutcomeUnknown => return Err(RemoteError::Protocol("the previous prepared paste has an unknown outcome; inspect the destination before pasting again".into())),
		}
		let PreparedImportState::Ready(ready) =
			std::mem::replace(&mut *state, PreparedImportState::Applying)
		else {
			unreachable!()
		};
		Ok(ready)
	}

	/// Applying → Ready after a refusal from before the worker's first write
	/// (Busy, Timeout) on a clean connection, so the same preview can apply
	/// again. Otherwise Used, or OutcomeUnknown when the outcome is unknown;
	/// a clean connection goes back through `keep`, never an unknown one.
	/// A check (`check_only`) never writes, so any clean answer keeps the
	/// token: the apply that follows can still use it.
	fn finish_apply<R>(
		&self,
		connection: Connection,
		token: String,
		permit: PreparedImportPermit,
		result: &Result<R, RemoteError>,
		check_only: bool,
		keep: impl FnOnce(Connection),
	) {
		let lost = result.as_ref().is_err_and(RemoteError::outcome_unknown);
		let clean = !lost && connection.unsolicited_frames() == 0;
		let retry = clean
			&& !connection.broken
			&& (check_only
				|| matches!(
					result,
					Err(RemoteError::Refused { code, .. })
						if crate::prepared::refused_before_write(*code)
				));
		let next = if retry {
			PreparedImportState::Ready(Box::new(PreparedConnection {
				connection,
				token,
				_permit: permit,
			}))
		} else {
			// An out-of-step or unknown outcome is never returned to the pool.
			if clean {
				keep(connection);
			}
			if lost && !check_only {
				PreparedImportState::OutcomeUnknown
			} else {
				PreparedImportState::Used
			}
		};
		*self.state.lock().unwrap_or_else(PoisonError::into_inner) = next;
	}

	/// Runs one Apply's state changes with `refusal` as the worker's answer.
	/// `during` runs while the lease is Applying, before the answer.
	#[doc(hidden)]
	pub fn refuse_apply_for_tests(
		&self,
		refusal: RemoteError,
		during: impl FnOnce(),
	) -> Result<(), RemoteError> {
		let ready = self.begin_apply()?;
		let PreparedConnection {
			connection,
			token,
			_permit,
		} = *ready;
		during();
		let result: Result<(), RemoteError> = Err(refusal);
		self.finish_apply(connection, token, _permit, &result, false, drop);
		result
	}
}

fn prepared_stale(message: &str) -> RemoteError {
	RemoteError::Refused {
		code: ErrorCode::Stale,
		message: message.into(),
		reason: Some(Box::new(Reason::DestinationChanged {
			change: StaleChange::Preview,
		})),
	}
}

/// Calls to one worker, reusing a few idle connections.
pub struct Client {
	host: RemoteHost,
	my_name: String,
	idle: Mutex<Vec<Connection>>,
	limiter: GitLimiter,
	handshakes: Arc<HandshakeGate>,
	prepared_imports: Arc<PreparedImports>,
}

impl Client {
	pub fn new(host: RemoteHost, my_name: String) -> Self {
		let handshakes = HandshakeGate::for_host(&host);
		Self {
			host,
			my_name,
			idle: Mutex::new(Vec::new()),
			limiter: GitLimiter::new(),
			handshakes,
			prepared_imports: Arc::default(),
		}
	}

	/// Pools a connection whose transport died while idle: the next request
	/// on it fails with a broken pipe before any reply and is resent on a
	/// fresh worker (#206), for tests of that path.
	#[doc(hidden)]
	pub fn pool_a_dead_connection_for_tests(&self) {
		struct Dead;
		impl io::Write for Dead {
			fn write(&mut self, _: &[u8]) -> io::Result<usize> {
				Err(io::Error::from(io::ErrorKind::BrokenPipe))
			}
			fn flush(&mut self) -> io::Result<()> {
				Ok(())
			}
		}
		self.keep(Connection {
			writer: Box::new(Dead),
			frames: mpsc::sync_channel(1).1,
			child: None,
			stderr: Arc::default(),
			worker_name: String::new(),
			home: None,
			version: u32::MAX,
			broken: false,
			frames_seen: 0,
			retained: Arc::default(),
			in_flight: Arc::default(),
			last_byte: StreamClock::new(),
			call_started: Instant::now(),
			call_base: 0,
		});
	}

	pub fn host(&self) -> &RemoteHost {
		&self.host
	}

	fn connect(&self) -> Result<Connection, RemoteError> {
		self.connect_with(None)
	}

	fn connect_with(
		&self,
		cancel: Option<&CancelToken>,
	) -> Result<Connection, RemoteError> {
		self.connect_until(cancel, Instant::now() + CONNECT_TIMEOUT)
	}

	fn connect_until(
		&self,
		cancel: Option<&CancelToken>,
		deadline: Instant,
	) -> Result<Connection, RemoteError> {
		self.connect_queued(cancel, deadline, CONNECT_TIMEOUT)
	}

	/// Waits for a handshake slot until `deadline` (the caller's limit), then
	/// gives the hello `hello` counted from the moment the slot is taken, and
	/// never past `deadline`. Time spent queued does not shorten the hello.
	fn connect_queued(
		&self,
		cancel: Option<&CancelToken>,
		deadline: Instant,
		hello: Duration,
	) -> Result<Connection, RemoteError> {
		let _guard = self.handshakes.acquire(cancel, deadline)?;
		Connection::open_until(
			&self.host.transport,
			&self.my_name,
			cancel,
			deadline.min(Instant::now() + hello),
		)
	}

	/// A fresh connection whose worker speaks at least `need`, or
	/// [`RemoteError::WorkerTooOld`]. A too-old worker is kept in the pool
	/// for the requests it can still serve.
	fn connect_at_least(
		&self,
		need: u32,
		cancel: Option<&CancelToken>,
		deadline: Instant,
	) -> Result<Connection, RemoteError> {
		let fresh = self.connect_until(cancel, deadline)?;
		if fresh.version() < need {
			let err = too_old_error(&fresh, need);
			self.keep(fresh);
			return Err(err);
		}
		Ok(fresh)
	}

	fn keep(&self, conn: Connection) {
		if conn.broken {
			return;
		}
		let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
		if idle.len() < POOL {
			idle.push(conn);
		}
	}

	fn take_connection(
		&self,
		need: u32,
		cancel: Option<&CancelToken>,
		deadline: Instant,
	) -> Result<(Connection, bool), RemoteError> {
		let pooled = self
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.pop();
		match pooled {
			Some(conn) if conn.unsolicited_frames() > 0 => {
				// Its stream is out of step. This is a new read/plan, never a
				// token Apply, so the old worker can be discarded here.
				drop(conn);
				Ok((self.connect_at_least(need, cancel, deadline)?, false))
			}
			Some(conn) if conn.version() >= need => Ok((conn, true)),
			Some(conn) => {
				let error = too_old_error(&conn, need);
				self.keep(conn);
				Err(error)
			}
			None => Ok((self.connect_at_least(need, cancel, deadline)?, false)),
		}
	}

	/// Starts the worker and returns its home folder.
	pub fn home(&self) -> Result<Option<String>, RemoteError> {
		let conn = self.connect()?;
		let home = conn.home().map(str::to_string);
		self.keep(conn);
		Ok(home)
	}

	pub fn call(&self, request: &Request) -> Result<Response, RemoteError> {
		self.call_with(request, None, GIT_CALL_LIMIT)
	}

	pub fn call_with(
		&self,
		request: &Request,
		cancel: Option<&CancelToken>,
		limit: Duration,
	) -> Result<Response, RemoteError> {
		let start = Instant::now();
		let is_git = runs_git(request);
		// A write is never sent twice, even when the first send seemed lost.
		let writes = matches!(
			request,
			Request::ImportApply { .. }
				| Request::ApplyPrepared { .. }
				| Request::ReplayApply {
					check_only: false,
					..
				} | Request::ApplyPreparedReplay {
				check_only: false,
				..
			}
		);
		let _guard = if is_git {
			Some(self.limiter.acquire(cancel, limit)?)
		} else {
			None
		};

		let need = request.needs_version();
		let (mut conn, reused) =
			self.take_connection(need, cancel, start + limit)?;

		let remaining = limit.saturating_sub(start.elapsed());
		if remaining.is_zero() {
			return Err(RemoteError::TimedOut);
		}
		let mut result = conn.call(request, cancel, remaining);

		if let Err(ref err) = result {
			if !writes && should_resend(reused, conn.frames_seen, err) {
				let remaining_retry = limit.saturating_sub(start.elapsed());
				if remaining_retry.is_zero() {
					return Err(RemoteError::TimedOut);
				}
				match self.connect_at_least(need, cancel, start + limit) {
					Ok(mut fresh) => {
						result = fresh.call(request, cancel, remaining_retry);
						conn = fresh;
					}
					// A fresh worker that cannot serve this request at all
					// outranks the first failure.
					Err(err @ RemoteError::WorkerTooOld { .. }) => {
						return Err(err)
					}
					// A start failure with a known cause is what the user
					// can act on; the dead pooled connection's I/O error is
					// only how it showed.
					Err(
						err @ RemoteError::Start {
							reason: Some(_), ..
						},
					) => return Err(err),
					// Without a replacement, the original failure stands.
					Err(_) => {}
				}
			}
		}
		self.keep(conn);
		result
	}

	/// Copies `items` of `workspace` as one snip-sync payload, run by the
	/// worker with the local copy engine.
	pub fn export_files(
		&self,
		workspace: &str,
		items: Vec<ExportTarget>,
		settings: &snip_core::settings::Settings,
		file_limit: usize,
		cancel: Option<&CancelToken>,
	) -> Result<CopyOutcome, RemoteError> {
		let req = Request::Export {
			workspace: workspace.into(),
			items,
			settings: settings.clone(),
			file_limit,
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Copied(out) => Ok(out),
			_ => Err(unexpected()),
		}
	}

	/// Copies every change of `source` in `repo` as one snip-sync payload,
	/// the selection resolved by the worker with the local copy engine's
	/// own `changed_items`.
	pub fn export_changes(
		&self,
		workspace: &str,
		repo: &str,
		source: &snip_core::gitsrc::GitSource,
		settings: &snip_core::settings::Settings,
		file_limit: usize,
		cancel: Option<&CancelToken>,
	) -> Result<CopyOutcome, RemoteError> {
		let req = Request::ExportChanges {
			workspace: workspace.into(),
			repo: repo.into(),
			source: source.clone(),
			settings: settings.clone(),
			file_limit,
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Copied(out) => Ok(out),
			_ => Err(unexpected()),
		}
	}

	/// Copies the commits `selected` (ending at `tip`) of `repo`.
	pub fn export_commits(
		&self,
		workspace: &str,
		repo: &str,
		tip: &str,
		selected: Vec<String>,
		cancel: Option<&CancelToken>,
	) -> Result<CommitCopyOutcome, RemoteError> {
		let req = Request::ExportCommits {
			workspace: workspace.into(),
			repo: repo.into(),
			tip: tip.into(),
			selected,
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::CommitsCopied(out) => Ok(out),
			_ => Err(unexpected()),
		}
	}

	/// Plans pasting the file payload `text` into the folder `dest`
	/// (relative to the workspace) on the worker. Nothing is written.
	pub fn import_plan(
		&self,
		workspace: &str,
		dest: &str,
		text: &str,
		mapping: &PasteMapping,
		cancel: Option<&CancelToken>,
	) -> Result<ImportPlanned, RemoteError> {
		let req = Request::ImportPlan {
			workspace: workspace.into(),
			dest: dest.into(),
			text: text.into(),
			mapping: mapping.clone(),
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::ImportPlanned(planned) => Ok(planned),
			_ => Err(unexpected()),
		}
	}

	/// File-paste facade: use metadata-only previews on protocol 7 and
	/// reserve the exact worker connection when it retains a payload token.
	/// A worker or client unable to retain a token chooses legacy Apply now,
	/// before any write has been sent.
	pub(crate) fn prepare_import(
		&self,
		workspace: &str,
		dest: &str,
		text: &str,
		mapping: &PasteMapping,
		cancel: Option<&CancelToken>,
	) -> Result<(ImportPlanned, Option<Arc<PreparedImportLease>>), RemoteError>
	{
		let start = Instant::now();
		let _guard = self.limiter.acquire(cancel, GIT_CALL_LIMIT)?;
		let permit = self.prepared_imports.reserve();
		let (mut connection, reused) = self.take_connection(
			crate::proto::PASTE_VERSION,
			cancel,
			start + GIT_CALL_LIMIT,
		)?;
		let request = |version| {
			if version >= crate::proto::PREPARED_IMPORT_VERSION {
				Request::PrepareImport {
					workspace: workspace.into(),
					dest: dest.into(),
					text: text.into(),
					mapping: mapping.clone(),
					retain: permit.is_some(),
				}
			} else {
				Request::ImportPlan {
					workspace: workspace.into(),
					dest: dest.into(),
					text: text.into(),
					mapping: mapping.clone(),
				}
			}
		};
		let mut reply = connection.call(
			&request(connection.version()),
			cancel,
			GIT_CALL_LIMIT.saturating_sub(start.elapsed()),
		);
		if let Err(error) = &reply {
			// Planning is read-only. Preserve the existing single retry on a
			// stale reused connection, selecting the new worker's protocol.
			if should_resend(reused, connection.frames_seen, error) {
				match self.connect_at_least(
					crate::proto::PASTE_VERSION,
					cancel,
					start + GIT_CALL_LIMIT,
				) {
					Ok(mut fresh) => {
						reply = fresh.call(
							&request(fresh.version()),
							cancel,
							GIT_CALL_LIMIT.saturating_sub(start.elapsed()),
						);
						connection = fresh;
					}
					Err(error @ RemoteError::WorkerTooOld { .. }) => {
						return Err(error)
					}
					Err(
						error @ RemoteError::Start {
							reason: Some(_), ..
						},
					) => return Err(error),
					Err(_) => {}
				}
			}
		}
		let (planned, token) = match reply {
			Ok(Response::ImportPrepared(prepared))
				if connection.version()
					>= crate::proto::PREPARED_IMPORT_VERSION =>
			{
				let planned = prepared
					.hydrate(text, &mapping.header_format)
					.map_err(RemoteError::Protocol)?;
				(planned, prepared.token)
			}
			Ok(Response::ImportPlanned(planned))
				if connection.version()
					< crate::proto::PREPARED_IMPORT_VERSION =>
			{
				(planned, None)
			}
			Ok(_) => return Err(unexpected()),
			Err(error) => {
				self.keep(connection);
				return Err(error);
			}
		};
		if cancel.is_some_and(|token| token.is_cancelled()) {
			return Err(RemoteError::Cancelled);
		}
		match (token, permit) {
			(Some(token), Some(permit)) => {
				let lease = PreparedImportLease {
					owner: self.prepared_imports.clone(),
					workspace: workspace.into(),
					dest: dest.into(),
					text_hash: Sha256::digest(text.as_bytes()).into(),
					mapping: mapping.clone(),
					digest: planned.digest.clone(),
					kind: PreparedKind::Import,
					connection_bytes: connection_footprint(
						&connection,
						token.capacity(),
					),
					state: Mutex::new(PreparedImportState::Ready(Box::new(
						PreparedConnection {
							connection,
							token,
							_permit: permit,
						},
					))),
				};
				Ok((planned, Some(Arc::new(lease))))
			}
			(None, _) => {
				self.keep(connection);
				Ok((planned, None))
			}
			(Some(_), None) => Err(RemoteError::Protocol(
				"worker retained a paste without a client lease".into(),
			)),
		}
	}

	/// Applies a token on its original connection. The lease returns to
	/// Ready after a refusal from before the worker's first write (Busy,
	/// Timeout), so the same preview can apply again. When the worker no
	/// longer holds the token and wrote nothing, the full text goes once on
	/// that same connection as `ImportApply`, which the worker re-plans and
	/// checks against the digest. An unknown outcome is never resent or
	/// retried; no path reconnects or obtains another worker.
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn apply_prepared_import(
		&self,
		lease: &PreparedImportLease,
		workspace: &str,
		dest: &str,
		text: &str,
		mapping: &PasteMapping,
		selection: &snip_core::restore::RestoreSelection,
		expect: &ImportExpect,
	) -> Result<snip_core::restore::RestoreExecutionResult, RemoteError> {
		let text_hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
		if !Arc::ptr_eq(&self.prepared_imports, &lease.owner)
			|| workspace != lease.workspace
			|| dest != lease.dest
			|| text_hash != lease.text_hash
			|| mapping != &lease.mapping
			|| expect.digest != lease.digest
			|| lease.kind != PreparedKind::Import
		{
			return Err(prepared_stale("the prepared paste belongs to another target or payload; preview again"));
		}
		let start = Instant::now();
		let _guard = self.limiter.acquire(None, APPLY_CALL_LIMIT)?;
		let ready = lease.begin_apply()?;
		let PreparedConnection {
			mut connection,
			token,
			_permit,
		} = *ready;
		let call =
			|connection: &mut Connection, request: &Request| match connection
				.call(
					request,
					None,
					APPLY_CALL_LIMIT.saturating_sub(start.elapsed()),
				) {
				Ok(Response::Imported(result)) => Ok(result),
				Ok(_) => Err(unexpected()),
				Err(error) => Err(error),
			};
		let result = if connection.broken || connection.unsolicited_frames() > 0
		{
			Err(prepared_stale(
				"the prepared paste connection changed; preview again",
			))
		} else {
			let applied = call(
				&mut connection,
				&Request::ApplyPrepared {
					workspace: workspace.into(),
					dest: dest.into(),
					token: token.clone(),
					selection: selection.clone(),
					expect: expect.clone(),
				},
			);
			match applied {
				// The worker dropped the payload before writing anything:
				// the full text, once, re-planned and digest-checked there.
				Err(RemoteError::Refused {
					code: ErrorCode::Stale,
					message,
					..
				}) if message == crate::prepared::TOKEN_GONE => call(
					&mut connection,
					&Request::ImportApply {
						workspace: workspace.into(),
						dest: dest.into(),
						text: text.into(),
						mapping: mapping.clone(),
						selection: selection.clone(),
						expect: expect.clone(),
					},
				),
				other => other,
			}
		};
		lease.finish_apply(
			connection,
			token,
			_permit,
			&result,
			false,
			|connection| self.keep(connection),
		);
		result
	}

	/// Commit-replay facade, as [`Self::prepare_import`]: on protocol 8 the
	/// worker keeps its own preview under a token, and the reply carries the
	/// plan for display. An older worker answers a plan only and no lease is
	/// made. A reused connection that went stale is resent once.
	pub(crate) fn prepare_replay(
		&self,
		workspace: &str,
		dest: &str,
		text: &str,
		payload: snip_core::commits::CommitsPayload,
		cancel: Option<&CancelToken>,
	) -> Result<
		(
			snip_core::transfer::CommitReplayPreview,
			Option<Arc<PreparedImportLease>>,
		),
		RemoteError,
	> {
		let start = Instant::now();
		let _guard = self.limiter.acquire(cancel, GIT_CALL_LIMIT)?;
		let permit = self.prepared_imports.reserve();
		let (mut connection, reused) = self.take_connection(
			crate::proto::PASTE_VERSION,
			cancel,
			start + GIT_CALL_LIMIT,
		)?;
		let request = |version| {
			if version >= crate::proto::PREPARED_REPLAY_VERSION {
				Request::PrepareReplay {
					workspace: workspace.into(),
					dest: dest.into(),
					text: text.into(),
					retain: permit.is_some(),
				}
			} else {
				Request::ReplayPlan {
					workspace: workspace.into(),
					dest: dest.into(),
					text: text.into(),
				}
			}
		};
		let mut reply = connection.call(
			&request(connection.version()),
			cancel,
			GIT_CALL_LIMIT.saturating_sub(start.elapsed()),
		);
		if let Err(error) = &reply {
			// Planning is read-only: the same single resend as a file plan.
			if should_resend(reused, connection.frames_seen, error) {
				match self.connect_at_least(
					crate::proto::PASTE_VERSION,
					cancel,
					start + GIT_CALL_LIMIT,
				) {
					Ok(mut fresh) => {
						reply = fresh.call(
							&request(fresh.version()),
							cancel,
							GIT_CALL_LIMIT.saturating_sub(start.elapsed()),
						);
						connection = fresh;
					}
					Err(error @ RemoteError::WorkerTooOld { .. }) => {
						return Err(error)
					}
					Err(
						error @ RemoteError::Start {
							reason: Some(_), ..
						},
					) => return Err(error),
					Err(_) => {}
				}
			}
		}
		let (preview, token, digest) = match reply {
			Ok(Response::ReplayPrepared(prepared))
				if connection.version()
					>= crate::proto::PREPARED_REPLAY_VERSION =>
			{
				let preview =
					snip_core::transfer::CommitReplayPreview::from_parts(
						prepared.expect.destination,
						payload,
						prepared.expect.plan,
						prepared.expect.freshness,
					);
				(preview, prepared.token, prepared.digest)
			}
			Ok(Response::ReplayPlanned(planned))
				if connection.version()
					< crate::proto::PREPARED_REPLAY_VERSION =>
			{
				let preview =
					snip_core::transfer::CommitReplayPreview::from_parts(
						planned.destination,
						payload,
						planned.plan,
						planned.freshness,
					);
				(preview, None, String::new())
			}
			Ok(_) => return Err(unexpected()),
			Err(error) => {
				self.keep(connection);
				return Err(error);
			}
		};
		if cancel.is_some_and(|token| token.is_cancelled()) {
			return Err(RemoteError::Cancelled);
		}
		match (token, permit) {
			(Some(token), Some(permit)) => {
				let lease = PreparedImportLease {
					owner: self.prepared_imports.clone(),
					workspace: workspace.into(),
					dest: dest.into(),
					text_hash: Sha256::digest(text.as_bytes()).into(),
					mapping: PasteMapping::default(),
					digest,
					kind: PreparedKind::Replay,
					connection_bytes: connection_footprint(
						&connection,
						token.capacity(),
					),
					state: Mutex::new(PreparedImportState::Ready(Box::new(
						PreparedConnection {
							connection,
							token,
							_permit: permit,
						},
					))),
				};
				Ok((preview, Some(Arc::new(lease))))
			}
			(None, _) => {
				self.keep(connection);
				Ok((preview, None))
			}
			(Some(_), None) => Err(RemoteError::Protocol(
				"worker retained a replay without a client lease".into(),
			)),
		}
	}

	/// Applies, or with `check_only` re-checks, the replay `lease` retained.
	/// Only the token and digest cross the wire. When the worker no longer
	/// holds the token and wrote nothing, the full text goes once on the same
	/// connection as `ReplayApply`, expected to `preview`. Lease rules as in
	/// [`Self::apply_prepared_import`].
	pub(crate) fn apply_prepared_replay(
		&self,
		lease: &PreparedImportLease,
		workspace: &str,
		dest: &str,
		text: &str,
		preview: &snip_core::transfer::CommitReplayPreview,
		check_only: bool,
	) -> Result<Option<snip_core::commits::ReplayResult>, RemoteError> {
		let text_hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
		if !Arc::ptr_eq(&self.prepared_imports, &lease.owner)
			|| lease.kind != PreparedKind::Replay
			|| workspace != lease.workspace
			|| dest != lease.dest
			|| text_hash != lease.text_hash
		{
			return Err(prepared_stale("the prepared paste belongs to another target or payload; preview again"));
		}
		let start = Instant::now();
		let limit = if check_only {
			GIT_CALL_LIMIT
		} else {
			APPLY_CALL_LIMIT
		};
		let _guard = self.limiter.acquire(None, limit)?;
		let ready = lease.begin_apply()?;
		let PreparedConnection {
			mut connection,
			token,
			_permit,
		} = *ready;
		let call =
			|connection: &mut Connection, request: &Request| match connection
				.call(request, None, limit.saturating_sub(start.elapsed()))
			{
				Ok(Response::Replayed(result)) if !check_only => {
					Ok(Some(result))
				}
				Ok(Response::Fresh) if check_only => Ok(None),
				Ok(_) => Err(unexpected()),
				Err(error) => Err(error),
			};
		let result = if connection.broken || connection.unsolicited_frames() > 0
		{
			Err(prepared_stale(
				"the prepared paste connection changed; preview again",
			))
		} else {
			let applied = call(
				&mut connection,
				&Request::ApplyPreparedReplay {
					workspace: workspace.into(),
					dest: dest.into(),
					token: token.clone(),
					digest: lease.digest.clone(),
					check_only,
				},
			);
			match applied {
				// The worker dropped the retained preview before writing:
				// the full text, once, planned and checked there.
				Err(RemoteError::Refused {
					code: ErrorCode::Stale,
					message,
					..
				}) if message == crate::prepared::TOKEN_GONE => call(
					&mut connection,
					&Request::ReplayApply {
						workspace: workspace.into(),
						dest: dest.into(),
						text: text.into(),
						expect: ReplayExpect::of(preview),
						check_only,
					},
				),
				other => other,
			}
		};
		lease.finish_apply(
			connection,
			token,
			_permit,
			&result,
			check_only,
			|connection| self.keep(connection),
		);
		result
	}

	#[doc(hidden)]
	pub fn prepared_imports_for_tests(&self) -> usize {
		self.prepared_imports.active.load(Ordering::SeqCst)
	}

	/// Writes what [`Self::import_plan`] previewed, as `selection` confirms,
	/// or refuses ([`ErrorCode::Stale`]) when the destination changed.
	#[allow(clippy::too_many_arguments)]
	pub fn import_apply(
		&self,
		workspace: &str,
		dest: &str,
		text: &str,
		mapping: &PasteMapping,
		selection: &snip_core::restore::RestoreSelection,
		expect: &ImportExpect,
		cancel: Option<&CancelToken>,
	) -> Result<snip_core::restore::RestoreExecutionResult, RemoteError> {
		let req = Request::ImportApply {
			workspace: workspace.into(),
			dest: dest.into(),
			text: text.into(),
			mapping: mapping.clone(),
			selection: selection.clone(),
			expect: expect.clone(),
		};
		match self.call_with(&req, cancel, APPLY_CALL_LIMIT)? {
			Response::Imported(result) => Ok(result),
			_ => Err(unexpected()),
		}
	}

	/// Plans replaying the commit payload `text` onto the repository at
	/// `dest`: the preview, put back together with the payload the caller
	/// already parsed.
	pub fn replay_plan(
		&self,
		workspace: &str,
		dest: &str,
		text: &str,
		payload: snip_core::commits::CommitsPayload,
		cancel: Option<&CancelToken>,
	) -> Result<snip_core::transfer::CommitReplayPreview, RemoteError> {
		let req = Request::ReplayPlan {
			workspace: workspace.into(),
			dest: dest.into(),
			text: text.into(),
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::ReplayPlanned(e) => {
				Ok(snip_core::transfer::CommitReplayPreview::from_parts(
					e.destination,
					payload,
					e.plan,
					e.freshness,
				))
			}
			_ => Err(unexpected()),
		}
	}

	/// Replays what [`Self::replay_plan`] previewed, or refuses
	/// ([`ErrorCode::Stale`]) when the repository changed. With
	/// `check_only`, only re-checks and answers `None`.
	pub fn replay_apply(
		&self,
		workspace: &str,
		dest: &str,
		text: &str,
		preview: &snip_core::transfer::CommitReplayPreview,
		check_only: bool,
		cancel: Option<&CancelToken>,
	) -> Result<Option<snip_core::commits::ReplayResult>, RemoteError> {
		let req = Request::ReplayApply {
			workspace: workspace.into(),
			dest: dest.into(),
			text: text.into(),
			expect: ReplayExpect::of(preview),
			check_only,
		};
		let limit = if check_only {
			GIT_CALL_LIMIT
		} else {
			APPLY_CALL_LIMIT
		};
		match self.call_with(&req, cancel, limit)? {
			Response::Replayed(result) if !check_only => Ok(Some(result)),
			Response::Fresh if check_only => Ok(None),
			_ => Err(unexpected()),
		}
	}

	/// Resolves `path` (absolute, or `~/…`) to a workspace on the worker.
	pub fn open_workspace(
		&self,
		path: &str,
	) -> Result<RemoteWorkspace, RemoteError> {
		self.open_workspace_with(path, None)
	}

	/// [`Self::open_workspace`] that `cancel` ends, the connect included.
	pub fn open_workspace_with(
		&self,
		path: &str,
		cancel: Option<&CancelToken>,
	) -> Result<RemoteWorkspace, RemoteError> {
		let request = Request::OpenWorkspace { path: path.into() };
		match self.call_with(&request, cancel, GIT_CALL_LIMIT)? {
			Response::Workspace(ws) => Ok(ws),
			_ => Err(unexpected()),
		}
	}

	pub fn scan_repos(
		&self,
		workspace: &str,
		under: Option<&str>,
		cancel: Option<&CancelToken>,
	) -> Result<RepoScan, RemoteError> {
		let req = Request::ScanRepos {
			workspace: workspace.to_string(),
			under: under.map(str::to_string),
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Repos(scan) => Ok(scan),
			_ => Err(unexpected()),
		}
	}

	pub fn git(
		&self,
		workspace: &str,
		repo: &str,
		profile: ReadProfile,
		query: GitQuery,
		cancel: Option<&CancelToken>,
	) -> Result<GitReply, RemoteError> {
		let req = Request::GitView {
			workspace: workspace.to_string(),
			repo: repo.to_string(),
			profile,
			query,
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Git(reply) => Ok(reply),
			_ => Err(unexpected()),
		}
	}

	/// The whole sorted listing of a remote folder, and whether the worker
	/// capped it ([`crate::proto::MAX_DIR_ENTRIES`]). One call: there is no
	/// continuation to ask for.
	pub fn list_dir(
		&self,
		workspace: &str,
		path: &str,
		cancel: Option<&CancelToken>,
	) -> Result<(Vec<DirEntry>, bool), RemoteError> {
		let req = Request::ListDir {
			workspace: workspace.into(),
			path: path.into(),
			folders_only: false,
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Dir { entries, truncated } => Ok((entries, truncated)),
			_ => Err(unexpected()),
		}
	}

	/// The folders `path` holds, as a folder picker lists them. Older workers
	/// send every entry; filtered here as well.
	pub fn list_folders(
		&self,
		workspace: &str,
		path: &str,
		cancel: Option<&CancelToken>,
	) -> Result<(Vec<DirEntry>, bool), RemoteError> {
		let req = Request::ListDir {
			workspace: workspace.into(),
			path: path.into(),
			folders_only: true,
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Dir {
				mut entries,
				truncated,
			} => {
				entries.retain(|e| e.directory && !e.name.starts_with('.'));
				Ok((entries, truncated))
			}
			_ => Err(unexpected()),
		}
	}

	pub fn stat(
		&self,
		workspace: &str,
		path: &str,
		cancel: Option<&CancelToken>,
	) -> Result<Stat, RemoteError> {
		let req = Request::Stat {
			workspace: workspace.into(),
			path: path.into(),
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Stat(stat) => Ok(stat),
			_ => Err(unexpected()),
		}
	}

	/// `None` for a binary or non-UTF-8 file.
	pub fn read(
		&self,
		workspace: &str,
		path: &str,
		cancel: Option<&CancelToken>,
	) -> Result<Option<String>, RemoteError> {
		let req = Request::Read {
			workspace: workspace.into(),
			path: path.into(),
		};
		match self.call_with(&req, cancel, GIT_CALL_LIMIT)? {
			Response::Text { content } => Ok(content),
			_ => Err(unexpected()),
		}
	}

	/// Reads a bounded, typed original destination for the shared commit
	/// preview engine. No source payload or replay plan crosses the wire.
	pub fn read_replay_preview(
		&self,
		workspace: &str,
		path: &std::path::Path,
		limit: usize,
		cancel: Option<&CancelToken>,
	) -> Result<snip_core::transfer::ReplayPreviewRead, RemoteError> {
		let request = Request::ReplayRead {
			workspace: workspace.into(),
			path: path.into(),
			limit,
		};
		match self.call_with(&request, cancel, GIT_CALL_LIMIT)? {
			Response::ReplayRead(read) => Ok(read),
			_ => Err(unexpected()),
		}
	}
}

fn unexpected() -> RemoteError {
	RemoteError::Protocol("unexpected reply".into())
}

/// The error for a worker too old for `need`.
fn too_old_error(conn: &Connection, need: u32) -> RemoteError {
	RemoteError::WorkerTooOld {
		worker: conn.worker_name().to_string(),
		have: conn.version(),
		need,
	}
}

/// Requests sharing worker job admission, including Git and paste previews:
/// bounded by
/// [`MAX_GIT_CALLS_IN_FLIGHT`] per client, so one master cannot stack more
/// git work on a worker than the worker can serve. That includes the
/// pastes into a repository: their freshness snapshot and re-check read
/// HEAD and the index with git (`capture_repo_freshness`). Listing,
/// ordinary reading and stating are plain fs work. Typed replay reads use
/// paste-job admission so their bounded reads and cancellation have the
/// same lifecycle as the other paste preview operations.
pub(crate) fn runs_git(request: &Request) -> bool {
	matches!(
		request,
		Request::ScanRepos { .. }
			| Request::ReplayRead { .. }
			| Request::GitView { .. }
			| Request::Export { .. }
			| Request::ExportCommits { .. }
			| Request::ExportChanges { .. }
			| Request::ImportPlan { .. }
			| Request::PrepareImport { .. }
			| Request::ImportApply { .. }
			| Request::ApplyPrepared { .. }
			| Request::ReplayPlan { .. }
			| Request::ReplayApply { .. }
			| Request::PrepareReplay { .. }
			| Request::ApplyPreparedReplay { .. }
	)
}

fn unexpected_reply() -> GitError {
	GitError::remote(Reason::Protocol, "unexpected reply from the worker")
}

/// Client-side implementation of `RepoView` over `Client::git`.
pub struct RemoteRepo {
	client: Arc<Client>,
	workspace: String,
	repo: String,
}

impl RemoteRepo {
	pub fn new(client: Arc<Client>, workspace: String, repo: String) -> Self {
		Self {
			client,
			workspace,
			repo,
		}
	}
}

impl RepoView for RemoteRepo {
	fn change_list(
		&self,
		max_rows: usize,
		read: &Read,
	) -> Result<ChangeList, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::ChangeList,
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::ChangeList(mut list) => {
				if list.rows.len() > max_rows {
					list.rows.truncate(max_rows);
				}
				Ok(list)
			}
			_ => Err(unexpected_reply()),
		}
	}

	fn refs(&self, read: &Read) -> Result<RefSnapshot, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::Refs,
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Refs(snap) => Ok(snap),
			_ => Err(unexpected_reply()),
		}
	}

	fn resolve_commit(
		&self,
		rev: &str,
		read: &Read,
	) -> Result<String, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::ResolveCommit {
				rev: rev.to_string(),
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Commit(sha) => Ok(sha),
			_ => Err(unexpected_reply()),
		}
	}

	fn log_from_tips(
		&self,
		tips: &[String],
		skip: usize,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<CommitSummary>, bool), GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::LogFromTips {
				tips: tips.to_vec(),
				skip,
				limit,
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Log { commits, more } => Ok((commits, more)),
			_ => Err(unexpected_reply()),
		}
	}

	fn history_query(
		&self,
		reference: Option<&str>,
		query: &LogQuery,
		skip: usize,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<CommitSummary>, bool), GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::HistoryQuery {
				reference: reference.map(str::to_string),
				query: query.clone(),
				skip,
				limit,
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Log { commits, more } => Ok((commits, more)),
			_ => Err(unexpected_reply()),
		}
	}

	fn commit_details(
		&self,
		sha: &str,
		read: &Read,
	) -> Result<CommitDetails, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::CommitDetails {
				sha: sha.to_string(),
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Details(details) => Ok(details),
			_ => Err(unexpected_reply()),
		}
	}

	fn user_email(&self, read: &Read) -> Option<String> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::UserEmail,
			read.cancel.as_ref(),
		);
		match reply {
			Ok(GitReply::UserEmail(email)) => email,
			_ => None,
		}
	}

	fn changed_paths(
		&self,
		source: &GitSource,
		max: usize,
		read: &Read,
	) -> Result<ChangedPathList, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::ChangedPaths {
				source: source.clone(),
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::ChangedPaths(mut list) => {
				if list.paths.len() > max {
					list.paths.truncate(max);
				}
				Ok(list)
			}
			_ => Err(unexpected_reply()),
		}
	}

	fn preview(
		&self,
		source: &GitSource,
		path: &str,
		listed: Option<(ChangeType, Option<&[String]>)>,
		read: &Read,
	) -> Result<GitPreview, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::Preview {
				source: source.clone(),
				path: path.to_string(),
				change: listed.map(|(c, _)| c),
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Preview(preview) => Ok(preview),
			_ => Err(unexpected_reply()),
		}
	}

	fn changed_file_text(
		&self,
		source: &GitSource,
		path: &str,
		max: u64,
		read: &Read,
	) -> Result<Option<String>, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::ChangedFileText {
				source: source.clone(),
				path: path.to_string(),
				max,
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::FileText(text) => Ok(text),
			_ => Err(unexpected_reply()),
		}
	}

	fn commit_directory(
		&self,
		rev: &str,
		dir: &str,
		limit: usize,
		read: &Read,
	) -> Result<(Vec<TreeEntry>, bool), GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::CommitDirectory {
				rev: rev.to_string(),
				dir: dir.to_string(),
				limit,
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Directory { entries, more } => Ok((entries, more)),
			_ => Err(unexpected_reply()),
		}
	}

	fn commit_blob(
		&self,
		rev: &str,
		path: &str,
		max: u64,
		read: &Read,
	) -> Result<BlobText, GitError> {
		let reply = self.client.git(
			&self.workspace,
			&self.repo,
			read.profile,
			GitQuery::CommitBlob {
				rev: rev.to_string(),
				path: path.to_string(),
				max,
			},
			read.cancel.as_ref(),
		)?;
		match reply {
			GitReply::Blob(blob) => Ok(blob),
			_ => Err(unexpected_reply()),
		}
	}
}

/// The master's side of a worker refusal: a diff over its limit keeps its
/// own variant, so it is not read as the file being too large.
pub(crate) fn refusal(
	code: ErrorCode,
	message: String,
	patch: bool,
	reason: Option<Reason>,
) -> RemoteError {
	if patch && code == ErrorCode::TooLarge {
		RemoteError::PatchTooLarge { message }
	} else {
		RemoteError::Refused {
			code,
			message,
			reason: reason.map(Box::new),
		}
	}
}

// The code-keyed arms below decide from the code alone, which is sound only
// while the worker sends no reason for those codes (`Reason::beside_code`
// answers `None` for InvalidRevision, Cancelled and Timeout). TooLarge is
// the one code a reason refines (VerifyLimit), so its arm checks the reason.
impl From<RemoteError> for GitError {
	fn from(err: RemoteError) -> Self {
		match err {
			RemoteError::Refused {
				code: ErrorCode::InvalidRevision,
				message,
				..
			} => GitError::InvalidRevision(message),
			// The worker names neither path nor limit; its limit is the
			// preview limit, and the caller knows the path.
			RemoteError::PatchTooLarge { .. } => GitError::PatchLimit {
				path: String::new(),
				limit: snip_core::browser::PREVIEW_LIMIT,
			},
			RemoteError::Refused {
				code: ErrorCode::TooLarge,
				ref reason,
				..
			} if reason
				.as_deref()
				.is_none_or(|r| matches!(r, Reason::TooLarge)) =>
			{
				GitError::OutputLimit {
					args: "remote view".into(),
					limit: 0,
				}
			}
			RemoteError::Refused {
				code: ErrorCode::Cancelled,
				..
			}
			| RemoteError::Cancelled => GitError::Cancelled {
				args: "remote view".into(),
			},
			RemoteError::Refused {
				code: ErrorCode::Timeout,
				..
			} => GitError::Timeout {
				args: "remote view".into(),
				secs: GIT_CALL_LIMIT.as_secs(),
			},
			// The rest has no typed variant of its own: the CLI prints the
			// error's text, a front end words the reason. TimedOut and
			// Stalled say how long and how much arrived: their own text, not
			// a 90 s limit they may never have reached (#191).
			other => GitError::remote(other.reason(), other.to_string()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::proto::write_frame;

	/// A worker that cannot be started for a reason of ours is typed, with
	/// the CLI's text unchanged and the OS's words as data only.
	#[test]
	fn a_command_that_cannot_start_is_typed() {
		let Err(error) = spawn(&[]) else {
			panic!("an empty command started")
		};
		assert_eq!(error.to_string(), "no command to start the worker");
		assert_eq!(
			error.reason(),
			Reason::Start {
				start: Some(StartReason::NoCommand),
				detail: Datum::default(),
			}
		);
		let missing = std::env::temp_dir().join("snip-no-such-program-225");
		let Err(error) = spawn(&[missing.clone().into_os_string()]) else {
			panic!("a missing program started")
		};
		let program = missing.to_string_lossy();
		assert!(
			error
				.to_string()
				.starts_with(&format!("cannot run {program}: ")),
			"{error}"
		);
		let Reason::Start { start, detail } = error.reason() else {
			panic!("{error:?}")
		};
		assert_eq!(start, Some(StartReason::CannotRun));
		assert!(
			detail.as_str().starts_with(&format!("{program}: ")),
			"{detail:?}"
		);
		assert!(!error.is_restore_retryable());
	}

	struct PreparedWrites(Arc<Mutex<Vec<u8>>>);
	impl IoWrite for PreparedWrites {
		fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
			self.0.lock().unwrap().extend_from_slice(bytes);
			Ok(bytes.len())
		}
		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	struct PreparedFixture {
		client: Arc<Client>,
		lease: Arc<PreparedImportLease>,
		expect: ImportExpect,
		writes: Arc<Mutex<Vec<u8>>>,
		reply: SyncSender<io::Result<Option<Response>>>,
		retained: Arc<AtomicUsize>,
	}

	impl PreparedFixture {
		fn new() -> Self {
			let client = Arc::new(Client::new(
				RemoteHost::in_process(Arc::new(Worker::new(
					Default::default(),
				))),
				"test".into(),
			));
			let permit = client.prepared_imports.reserve().unwrap();
			let (reply, frames) = mpsc::sync_channel(1);
			let writes = Arc::new(Mutex::new(Vec::new()));
			let retained = Arc::new(AtomicUsize::new(0));
			let mut connection = test_conn(
				Box::new(PreparedWrites(writes.clone())),
				frames,
				retained.clone(),
				Arc::new(AtomicBool::new(false)),
			);
			connection.version = crate::proto::PREPARED_IMPORT_VERSION;
			let expect = ImportExpect {
				digest: "expected".into(),
				freshness: snip_core::transfer::DestinationFreshnessSnapshot {
					roots: Default::default(),
					target_files: Default::default(),
				},
			};
			let token: String = "opaque-token".into();
			let connection_bytes =
				connection_footprint(&connection, token.capacity());
			let lease = Arc::new(PreparedImportLease {
				owner: client.prepared_imports.clone(),
				workspace: "/workspace".into(),
				dest: String::new(),
				text_hash: Sha256::digest(b"source").into(),
				mapping: PasteMapping::default(),
				digest: expect.digest.clone(),
				kind: PreparedKind::Import,
				connection_bytes,
				state: Mutex::new(PreparedImportState::Ready(Box::new(
					PreparedConnection {
						connection,
						token,
						_permit: permit,
					},
				))),
			});
			Self {
				client,
				lease,
				expect,
				writes,
				reply,
				retained,
			}
		}

		fn apply(
			&self,
		) -> Result<snip_core::restore::RestoreExecutionResult, RemoteError> {
			self.client.apply_prepared_import(
				&self.lease,
				"/workspace",
				"",
				"source",
				&PasteMapping::default(),
				&Default::default(),
				&self.expect,
			)
		}

		fn start(
			&self,
		) -> std::thread::JoinHandle<
			Result<snip_core::restore::RestoreExecutionResult, RemoteError>,
		> {
			let client = self.client.clone();
			let lease = self.lease.clone();
			let expect = self.expect.clone();
			std::thread::spawn(move || {
				client.apply_prepared_import(
					&lease,
					"/workspace",
					"",
					"source",
					&PasteMapping::default(),
					&Default::default(),
					&expect,
				)
			})
		}

		fn answer(&self, response: Option<Response>) {
			let end = Instant::now() + scaled(Duration::from_secs(5));
			loop {
				let bytes = self.writes.lock().unwrap();
				let complete = bytes.len() >= 4
					&& bytes.len()
						>= 4 + u32::from_be_bytes(
							bytes[..4].try_into().unwrap(),
						) as usize;
				drop(bytes);
				if complete {
					break;
				}
				assert!(Instant::now() < end, "prepared request never sent");
				std::thread::sleep(Duration::from_millis(5));
			}
			self.retained.fetch_add(1, Ordering::SeqCst);
			self.reply.send(Ok(response)).unwrap();
		}
	}

	fn join_prepared(
		thread: std::thread::JoinHandle<
			Result<snip_core::restore::RestoreExecutionResult, RemoteError>,
		>,
	) -> Result<snip_core::restore::RestoreExecutionResult, RemoteError> {
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while !thread.is_finished() {
			assert!(Instant::now() < end, "prepared apply did not finish");
			std::thread::sleep(Duration::from_millis(5));
		}
		thread.join().unwrap()
	}

	#[test]
	fn prepared_lease_checks_binding_and_clones_share_one_use() {
		let fixture = PreparedFixture::new();
		let other = Client::new(fixture.client.host.clone(), "other".into());
		for (client, workspace, dest, text, mapping) in [
			(&other, "/workspace", "", "source", PasteMapping::default()),
			(
				fixture.client.as_ref(),
				"/other",
				"",
				"source",
				PasteMapping::default(),
			),
			(
				fixture.client.as_ref(),
				"/workspace",
				"other",
				"source",
				PasteMapping::default(),
			),
			(
				fixture.client.as_ref(),
				"/workspace",
				"",
				"changed",
				PasteMapping::default(),
			),
			(
				fixture.client.as_ref(),
				"/workspace",
				"",
				"source",
				PasteMapping {
					adjust_paths: true,
					..Default::default()
				},
			),
		] {
			assert!(matches!(
				client.apply_prepared_import(
					&fixture.lease,
					workspace,
					dest,
					text,
					&mapping,
					&Default::default(),
					&fixture.expect
				),
				Err(RemoteError::Refused {
					code: ErrorCode::Stale,
					..
				})
			));
		}
		assert!(fixture.writes.lock().unwrap().is_empty());
		assert_eq!(fixture.client.prepared_imports_for_tests(), 1);
		let first = fixture.start();
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while fixture.writes.lock().unwrap().is_empty() {
			assert!(Instant::now() < end, "first clone did not start");
			std::thread::sleep(Duration::from_millis(5));
		}
		assert!(matches!(
			fixture.apply(),
			Err(RemoteError::Refused {
				code: ErrorCode::Busy,
				..
			})
		));
		fixture.answer(Some(Response::Imported(Default::default())));
		join_prepared(first).unwrap();
		let before = fixture.writes.lock().unwrap().len();
		assert!(matches!(
			fixture.apply(),
			Err(RemoteError::Refused {
				code: ErrorCode::Stale,
				..
			})
		));
		assert_eq!(fixture.writes.lock().unwrap().len(), before);
		let bytes = fixture.writes.lock().unwrap();
		let mut reader = bytes.as_slice();
		assert!(matches!(
			read_frame::<Request>(&mut reader).unwrap(),
			Some(Request::ApplyPrepared { .. })
		));
		assert!(reader.is_empty(), "exactly one write request");
		assert_eq!(fixture.client.prepared_imports_for_tests(), 0);
	}

	#[test]
	fn prepared_unknown_and_known_refusal_are_one_use_without_resend() {
		for response in [
			None,
			Some(Response::Error {
				code: ErrorCode::Stale,
				message: "expired; preview again".into(),
				patch: false,
				reason: None,
			}),
		] {
			let unknown = response.is_none();
			let fixture = PreparedFixture::new();
			let first = fixture.start();
			fixture.answer(response);
			let error = join_prepared(first).unwrap_err();
			assert_eq!(error.outcome_unknown(), unknown, "{error:?}");
			let before = fixture.writes.lock().unwrap().len();
			let error = fixture.apply().unwrap_err();
			assert_eq!(error.outcome_unknown(), unknown, "{error:?}");
			assert_eq!(
				fixture.writes.lock().unwrap().len(),
				before,
				"no token resend or legacy fallback"
			);
			assert_eq!(fixture.client.prepared_imports_for_tests(), 0);
		}
	}

	#[test]
	fn restoring_ready_after_a_refused_apply_never_grows_the_charge() {
		let fixture = PreparedFixture::new();
		let ready = fixture.lease.retained_heap_bytes();
		let wait_for_request = |before: usize| {
			let end = Instant::now() + scaled(Duration::from_secs(5));
			while fixture.writes.lock().unwrap().len() == before {
				assert!(Instant::now() < end, "apply request was never sent");
				std::thread::sleep(Duration::from_millis(5));
			}
		};
		let first = fixture.start();
		wait_for_request(0);
		let applying = fixture.lease.retained_heap_bytes();
		fixture.answer(Some(Response::Error {
			code: ErrorCode::Busy,
			message: "busy".into(),
			patch: false,
			reason: None,
		}));
		assert!(matches!(
			join_prepared(first),
			Err(RemoteError::Refused {
				code: ErrorCode::Busy,
				..
			})
		));
		let restored = fixture.lease.retained_heap_bytes();
		assert_eq!(applying, ready, "Applying keeps the connection charged");
		assert!(
			restored <= ready,
			"Ready never costs more than before: {restored} > {ready}"
		);

		// The Busy refusal left the lease Ready: the same preview applies again.
		let before = fixture.writes.lock().unwrap().len();
		let second = fixture.start();
		wait_for_request(before);
		fixture.answer(Some(Response::Imported(Default::default())));
		join_prepared(second).unwrap();
		assert!(fixture.lease.retained_heap_bytes() <= ready);
	}

	#[test]
	fn a_check_only_refusal_keeps_the_lease_ready() {
		let fixture = PreparedFixture::new();
		let ready = fixture.lease.begin_apply().unwrap();
		let PreparedConnection {
			connection,
			token,
			_permit,
		} = *ready;
		// A check writes nothing, so a refused check leaves the token usable
		// for the apply that follows, even though Stale is not retryable there.
		let refusal: Result<(), RemoteError> = Err(RemoteError::Refused {
			code: ErrorCode::Stale,
			message: "expired; preview again".into(),
			reason: None,
		});
		fixture
			.lease
			.finish_apply(connection, token, _permit, &refusal, true, drop);
		assert!(
			fixture.lease.begin_apply().is_ok(),
			"a refused check must keep the lease ready for an apply"
		);
	}

	#[test]
	fn prepared_cap_and_last_clone_drop_release_slots() {
		let fixture = PreparedFixture::new();
		let extra = fixture.client.prepared_imports.reserve().unwrap();
		assert!(fixture.client.prepared_imports.reserve().is_none());
		drop(extra);
		assert_eq!(fixture.client.prepared_imports_for_tests(), 1);
		let client = fixture.client.clone();
		let last = fixture.lease.clone();
		drop(fixture);
		assert_eq!(
			client.prepared_imports_for_tests(),
			1,
			"another plan clone keeps its lease"
		);
		drop(last);
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while client.prepared_imports_for_tests() != 0 {
			assert!(
				Instant::now() < end,
				"last clone did not release its lease"
			);
			std::thread::sleep(Duration::from_millis(5));
		}
		let canceled = CancelToken::new();
		canceled.cancel();
		assert!(matches!(
			client.prepare_import(
				"/workspace",
				"",
				"source",
				&PasteMapping::default(),
				Some(&canceled)
			),
			Err(RemoteError::Cancelled)
		));
		assert_eq!(client.prepared_imports_for_tests(), 0);
	}

	#[test]
	fn same_host_clients_bound_real_handshakes() {
		let worker = Arc::new(Worker::new(Default::default()));
		worker.hold_hellos_for_tests(true);
		let mut threads = Vec::new();
		for _ in 0..6 {
			let worker = worker.clone();
			threads.push(std::thread::spawn(move || {
				let client =
					Client::new(RemoteHost::in_process(worker), "test".into());
				client.home()
			}));
		}
		// Observe actual worker hellos while they are deliberately held. The
		// bound also lets a correct two-at-a-time implementation finish.
		let end = Instant::now() + scaled(Duration::from_millis(500));
		while worker.active_hellos_for_tests() < 6 && Instant::now() < end {
			std::thread::sleep(Duration::from_millis(5));
		}
		worker.hold_hellos_for_tests(false);
		for thread in threads {
			let end = Instant::now() + scaled(Duration::from_secs(5));
			while !thread.is_finished() {
				assert!(
					Instant::now() < end,
					"handshake thread did not finish"
				);
				std::thread::sleep(Duration::from_millis(5));
			}
			thread.join().unwrap().unwrap();
		}
		assert_eq!(worker.started_connections_for_tests(), 6);
		assert!(
			worker.max_hellos_for_tests() <= 2,
			"{} simultaneous hellos",
			worker.max_hellos_for_tests()
		);
	}

	#[test]
	fn handshake_queue_cancels_times_out_and_does_not_block_another_host() {
		let worker = Arc::new(Worker::new(Default::default()));
		worker.hold_hellos_for_tests(true);
		struct Release(Arc<Worker>);
		impl Drop for Release {
			fn drop(&mut self) {
				self.0.hold_hellos_for_tests(false);
			}
		}
		let release = Release(worker.clone());
		let host = RemoteHost::in_process(worker.clone());
		let first = Arc::new(Client::new(host.clone(), "first".into()));
		let second = Arc::new(Client::new(host.clone(), "second".into()));
		let mut holders = Vec::new();
		for client in [first.clone(), second.clone()] {
			holders.push(std::thread::spawn(move || client.home()));
		}
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while worker.active_hellos_for_tests() < 2 {
			assert!(Instant::now() < end, "two hellos never started");
			std::thread::sleep(Duration::from_millis(5));
		}
		let queued = Arc::new(Client::new(host.clone(), "queued".into()));
		let token = CancelToken::new();
		let (started, entered) = mpsc::channel();
		let waiter = {
			let queued = queued.clone();
			let token = token.clone();
			std::thread::spawn(move || {
				started.send(()).unwrap();
				queued.connect_with(Some(&token)).err().unwrap()
			})
		};
		entered
			.recv_timeout(scaled(Duration::from_secs(5)))
			.unwrap();
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while queued.handshakes.waiting.load(Ordering::SeqCst) == 0 {
			assert!(
				Instant::now() < end,
				"third client never entered the handshake queue"
			);
			std::thread::sleep(Duration::from_millis(5));
		}
		token.cancel();
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while !waiter.is_finished() {
			assert!(Instant::now() < end, "queued cancel did not finish");
			std::thread::sleep(Duration::from_millis(5));
		}
		assert!(matches!(waiter.join().unwrap(), RemoteError::Cancelled));
		let started = Instant::now();
		let error = queued
			.connect_until(None, started + scaled(Duration::from_millis(100)))
			.err()
			.unwrap();
		assert!(error.is_transient_read_failure());
		assert!(!error.outcome_unknown(), "queued connect sent no write");
		assert!(started.elapsed() < scaled(Duration::from_secs(2)));
		assert_eq!(
			worker.started_connections_for_tests(),
			2,
			"queued calls must not spawn transports"
		);
		let other = Arc::new(Worker::new(Default::default()));
		let other_client = Client::new(
			RemoteHost {
				name: "another-host".into(),
				transport: Transport::InProcess(other.clone()),
			},
			"other".into(),
		);
		other_client
			.connect_until(
				None,
				Instant::now() + scaled(Duration::from_secs(2)),
			)
			.unwrap();
		assert_eq!(other.started_connections_for_tests(), 1);
		drop(release);
		for holder in holders {
			let end = Instant::now() + scaled(Duration::from_secs(5));
			while !holder.is_finished() {
				assert!(Instant::now() < end, "held connection did not finish");
				std::thread::sleep(Duration::from_millis(5));
			}
			holder.join().unwrap().unwrap();
		}
		assert_eq!(
			*first.handshakes.active.lock().unwrap(),
			0,
			"established idle connections must not hold handshake permits"
		);
		queued.home().unwrap();
		assert_eq!(worker.started_connections_for_tests(), 3);
	}

	/// Holds every hello of `worker` until dropped, so two started
	/// connections keep both handshake slots busy.
	struct HoldHellos(Arc<Worker>);

	impl HoldHellos {
		fn new(worker: &Arc<Worker>) -> Self {
			worker.hold_hellos_for_tests(true);
			Self(worker.clone())
		}
	}

	impl Drop for HoldHellos {
		fn drop(&mut self) {
			self.0.hold_hellos_for_tests(false);
		}
	}

	#[test]
	fn a_connect_queued_past_the_hello_budget_still_connects() {
		let worker = Arc::new(Worker::new(Default::default()));
		let hold = HoldHellos::new(&worker);
		let host = RemoteHost::in_process(worker.clone());
		let mut holders = Vec::new();
		for name in ["first", "second"] {
			let client = Client::new(host.clone(), name.into());
			holders.push(std::thread::spawn(move || client.home()));
		}
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while worker.active_hellos_for_tests() < 2 {
			assert!(Instant::now() < end, "two hellos never started");
			std::thread::sleep(Duration::from_millis(5));
		}
		// A hello budget far shorter than the queue wait below: were the
		// budget counted from before the queue, the connect would time out.
		let hello = scaled(Duration::from_millis(200));
		let queued = Arc::new(Client::new(host.clone(), "queued".into()));
		let waiter = {
			let queued = queued.clone();
			std::thread::spawn(move || {
				queued.connect_queued(
					None,
					Instant::now() + scaled(Duration::from_secs(10)),
					hello,
				)
			})
		};
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while queued.handshakes.waiting.load(Ordering::SeqCst) == 0 {
			assert!(Instant::now() < end, "queued connect never waited");
			std::thread::sleep(Duration::from_millis(5));
		}
		let queued_at = Instant::now();
		let queued_for = scaled(Duration::from_millis(600));
		while queued_at.elapsed() < queued_for {
			assert!(!waiter.is_finished(), "queued connect ended in the queue");
			std::thread::sleep(Duration::from_millis(5));
		}
		drop(hold);
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while !waiter.is_finished() {
			assert!(Instant::now() < end, "queued connect did not finish");
			std::thread::sleep(Duration::from_millis(5));
		}
		waiter
			.join()
			.unwrap()
			.expect("a queue wait past the hello budget must still connect");
		for holder in holders {
			let end = Instant::now() + scaled(Duration::from_secs(5));
			while !holder.is_finished() {
				assert!(Instant::now() < end, "held connection did not finish");
				std::thread::sleep(Duration::from_millis(5));
			}
			holder.join().unwrap().unwrap();
		}
		assert_eq!(worker.started_connections_for_tests(), 3);
	}

	#[test]
	fn a_handshake_slot_timeout_does_not_blame_the_worker() {
		let worker = Arc::new(Worker::new(Default::default()));
		let hold = HoldHellos::new(&worker);
		let host = RemoteHost::in_process(worker.clone());
		let mut holders = Vec::new();
		for name in ["first", "second"] {
			let client = Client::new(host.clone(), name.into());
			holders.push(std::thread::spawn(move || client.home()));
		}
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while worker.active_hellos_for_tests() < 2 {
			assert!(Instant::now() < end, "two hellos never started");
			std::thread::sleep(Duration::from_millis(5));
		}
		let queued = Client::new(host.clone(), "queued".into());
		let error = queued
			.connect_until(
				None,
				Instant::now() + scaled(Duration::from_millis(100)),
			)
			.err()
			.unwrap();
		let message = error.to_string();
		assert!(
			matches!(
				error,
				RemoteError::Start {
					kind: ConnectFailureKind::TimedOut,
					..
				}
			),
			"{message}"
		);
		assert!(error.is_transient_read_failure());
		assert!(!error.outcome_unknown(), "queued connect sent no write");
		assert!(message.contains("connection slot"), "{message}");
		assert!(!message.contains("did not answer"), "{message}");
		drop(hold);
		for holder in holders {
			let end = Instant::now() + scaled(Duration::from_secs(5));
			while !holder.is_finished() {
				assert!(Instant::now() < end, "held connection did not finish");
				std::thread::sleep(Duration::from_millis(5));
			}
			holder.join().unwrap().unwrap();
		}
		assert_eq!(worker.started_connections_for_tests(), 2);
	}

	#[test]
	fn a_worker_closed_before_hello_keeps_a_typed_transport_failure() {
		let worker = Arc::new(Worker::new(Default::default()));
		worker.stop();
		let error = Connection::open(&Transport::InProcess(worker), "test")
			.err()
			.unwrap();
		assert!(matches!(error, RemoteError::Start { .. }), "{error:?}");
		assert!(error.is_transient_read_failure(), "{error:?}");
		assert!(!error.outcome_unknown());
		assert_eq!(
			error.to_string(),
			"the remote end closed the connection before snip started"
		);
	}

	#[test]
	fn startup_errors_preserve_typed_transience_and_existing_messages() {
		for (cause, code, stderr, transient) in [
			(StartCause::Ended, None, "", true),
			(
				StartCause::Io(io::ErrorKind::ConnectionReset),
				None,
				"",
				true,
			),
			(StartCause::NoAnswer, None, "", true),
			(
				StartCause::Ended,
				Some(255),
				"kex_exchange_identification: read: Connection reset by peer",
				true,
			),
			(
				StartCause::Ended,
				Some(255),
				"Permission denied (publickey)",
				false,
			),
			(
				StartCause::Ended,
				Some(255),
				"Host key verification failed",
				false,
			),
			(StartCause::Ended, Some(127), "snip: not found", false),
			(StartCause::BadData("invalid frame".into()), None, "", false),
			(
				StartCause::Io(io::ErrorKind::PermissionDenied),
				None,
				"",
				false,
			),
		] {
			let error = RemoteError::Start {
				kind: start_failure_kind(&cause, code, stderr),
				reason: start_reason(&cause, code, stderr),
				detail: Datum::for_test(stderr),
				message: start_message(cause.clone(), code, stderr),
			};
			assert_eq!(
				error.is_transient_read_failure(),
				transient,
				"{error:?}"
			);
			assert_eq!(error.to_string(), start_message(cause, code, stderr));
			assert!(!error.outcome_unknown(), "no request ran before hello");
		}
		for kind in [
			io::ErrorKind::UnexpectedEof,
			io::ErrorKind::ConnectionReset,
			io::ErrorKind::BrokenPipe,
			io::ErrorKind::TimedOut,
		] {
			assert!(RemoteError::Io(io::Error::from(kind))
				.is_transient_read_failure());
		}
		for code in [ErrorCode::Busy, ErrorCode::Timeout] {
			assert!(RemoteError::Refused {
				code,
				message: "irrelevant".into(),
				reason: None,
			}
			.is_transient_read_failure());
		}
		for error in [
			RemoteError::Cancelled,
			RemoteError::Start {
				kind: ConnectFailureKind::Other,
				reason: None,
				detail: Datum::for_test("Connection reset by peer"),
				message: "Connection reset by peer".into(),
			},
			RemoteError::Protocol("bad frame".into()),
			RemoteError::WorkerTooOld {
				worker: "old".into(),
				have: 1,
				need: 2,
			},
			RemoteError::Refused {
				code: ErrorCode::Forbidden,
				message: "Connection reset".into(),
				reason: None,
			},
			RemoteError::Refused {
				code: ErrorCode::BadRequest,
				message: "bad".into(),
				reason: None,
			},
		] {
			assert!(!error.is_transient_read_failure(), "{error:?}");
		}
	}

	/// A restore retries a start that ssh failed for a reason nobody
	/// classified, while a permission refusal and the other known permanent
	/// starts stay permanent.
	#[test]
	fn restore_retries_unclassified_starts_and_not_known_permanent_ones() {
		for stderr in [
			"ssh: Could not resolve hostname box: nodename nor servname provided, or not known",
			"ssh: Could not resolve hostname box: Name or service not known",
			"ssh: connect to host box port 22: Network is unreachable",
		] {
			let kind = start_failure_kind(&StartCause::Ended, Some(255), stderr);
			assert_eq!(kind, ConnectFailureKind::Other, "{stderr}");
			let error = RemoteError::Start {
				kind,
				reason: None,
				detail: Datum::for_test(stderr),
				message: start_message(StartCause::Ended, Some(255), stderr),
			};
			assert!(error.is_restore_retryable(), "{stderr}");
			assert!(!error.outcome_unknown(), "no request ran before hello");
		}
		let denied = RemoteError::Start {
			kind: start_failure_kind(
				&StartCause::Ended,
				Some(255),
				"user@box: Permission denied (publickey).",
			),
			reason: None,
			detail: Datum::default(),
			message: String::new(),
		};
		assert!(!denied.is_restore_retryable(), "{denied:?}");
		assert!(!denied.is_transient_read_failure(), "{denied:?}");
		for kind in [
			ConnectFailureKind::Authentication,
			ConnectFailureKind::WorkerUnavailable,
			ConnectFailureKind::Protocol,
		] {
			let error = RemoteError::Start {
				kind,
				reason: None,
				detail: Datum::default(),
				message: String::new(),
			};
			assert!(!error.is_restore_retryable(), "{error:?}");
		}
		for code in [ErrorCode::Busy, ErrorCode::Timeout, ErrorCode::NotFound] {
			let error = RemoteError::Refused {
				code,
				message: String::new(),
				reason: None,
			};
			assert!(error.is_restore_retryable(), "{error:?}");
		}
		for code in [ErrorCode::Forbidden, ErrorCode::Unsupported] {
			let error = RemoteError::Refused {
				code,
				message: String::new(),
				reason: None,
			};
			assert!(!error.is_restore_retryable(), "{error:?}");
		}
		for error in [
			RemoteError::WorkerTooOld {
				worker: "old".into(),
				have: 1,
				need: 2,
			},
			RemoteError::Protocol("bad frame".into()),
			RemoteError::Cancelled,
		] {
			assert!(!error.is_restore_retryable(), "{error:?}");
		}
	}

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

	fn fresh_clock() -> Arc<StreamClock> {
		StreamClock::new()
	}

	/// A connection over ready-made parts, as the real one is assembled in
	/// [`Connection::open`]: no child, no stderr, hello already done.
	fn test_conn(
		writer: Box<dyn IoWrite + Send>,
		frames: Frames,
		retained: Arc<AtomicUsize>,
		in_flight: Arc<AtomicBool>,
	) -> Connection {
		Connection {
			writer,
			frames,
			child: None,
			stderr: Arc::default(),
			worker_name: String::new(),
			home: None,
			version: 1,
			broken: false,
			frames_seen: 0,
			retained,
			in_flight,
			last_byte: fresh_clock(),
			call_started: Instant::now(),
			call_base: 0,
		}
	}

	struct FakeDuplex {
		incoming: io::Cursor<Vec<u8>>,
		outgoing: Vec<u8>,
	}

	impl FakeDuplex {
		fn new(responses: &[Response]) -> Self {
			let mut buf = Vec::new();
			for res in responses {
				write_frame(&mut buf, res).unwrap();
			}
			Self {
				incoming: io::Cursor::new(buf),
				outgoing: Vec::new(),
			}
		}
	}

	impl io::Read for FakeDuplex {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			self.incoming.read(buf)
		}
	}

	impl io::Write for FakeDuplex {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.outgoing.write(buf)
		}
		fn flush(&mut self) -> io::Result<()> {
			self.outgoing.flush()
		}
	}

	/// Feeds its frames with a pause before each read, so a deadline can
	/// pass between them.
	struct SlowDuplex {
		incoming: io::Cursor<Vec<u8>>,
		pause: Duration,
	}

	impl io::Read for SlowDuplex {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			std::thread::sleep(self.pause);
			self.incoming.read(buf)
		}
	}

	impl io::Write for SlowDuplex {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			Ok(buf.len())
		}
		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	#[test]
	fn exchange_skips_pending_frames_and_counts_frames() {
		let mut stream = FakeDuplex::new(&[
			Response::Pending,
			Response::Pending,
			Response::Text { content: None },
		]);
		let mut frames = 0;
		let res = exchange(
			&mut stream,
			None,
			IO_TIMEOUT,
			Instant::now() + Duration::from_secs(10),
			&mut frames,
		)
		.unwrap();

		assert_eq!(res, Response::Text { content: None });
		assert_eq!(frames, 3);
	}

	#[test]
	fn exchange_pending_followed_by_cancelled_token_returns_cancelled() {
		let mut stream =
			FakeDuplex::new(&[Response::Pending, Response::Pending]);
		let cancel = CancelToken::new();
		cancel.cancel();
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			Some(&cancel),
			IO_TIMEOUT,
			Instant::now() + Duration::from_secs(10),
			&mut frames,
		)
		.unwrap_err();

		assert!(matches!(err, RemoteError::Cancelled));
		assert_eq!(frames, 1);
	}

	#[test]
	fn exchange_zero_limit_with_pending_returns_timed_out() {
		let mut stream = FakeDuplex::new(&[Response::Pending]);
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			None,
			IO_TIMEOUT,
			Instant::now(),
			&mut frames,
		)
		.unwrap_err();

		assert!(matches!(err, RemoteError::TimedOut));
		assert_eq!(frames, 1);
	}

	#[test]
	fn exchange_error_frame_returns_refused() {
		let mut stream = FakeDuplex::new(&[Response::Error {
			code: ErrorCode::NotFound,
			message: "missing".into(),
			patch: false,
			reason: None,
		}]);
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			None,
			IO_TIMEOUT,
			Instant::now() + Duration::from_secs(10),
			&mut frames,
		)
		.unwrap_err();

		match err {
			RemoteError::Refused { code, message, .. } => {
				assert_eq!(code, ErrorCode::NotFound);
				assert_eq!(message, "missing");
			}
			other => panic!("expected Refused, got {other:?}"),
		}
		assert_eq!(frames, 1);
	}

	fn too_large(patch: bool) -> Response {
		Response::Error {
			code: ErrorCode::TooLarge,
			message: "big".into(),
			patch,
			reason: None,
		}
	}

	/// Runs one `call` on a connection whose worker answers with `frames`.
	fn call_with_frames(
		frames: Vec<Response>,
	) -> (Result<Response, RemoteError>, bool) {
		let (tx, rx) = mpsc::sync_channel(frames.len() + 1);
		for f in frames {
			tx.send(Ok(Some(f))).unwrap();
		}
		let mut conn = test_conn(
			Box::new(std::io::sink()),
			rx,
			Arc::default(),
			Arc::default(),
		);
		let req = Request::Stat {
			workspace: "w".into(),
			path: "a".into(),
		};
		let res = conn.call(&req, None, scaled(Duration::from_secs(10)));
		(res, conn.broken)
	}

	/// A refusal is an answer on every reply path (direct frame and the
	/// chunked/joined one): the connection stays poolable, and only the
	/// worker's `patch` flag turns TooLarge into the diff-limit error (#214).
	#[test]
	fn too_large_refusals_keep_the_connection_and_keep_patch_apart() {
		let joined = |patch: bool| {
			let json = serde_json::to_string(&too_large(patch)).unwrap();
			vec![Response::Chunk { data: json }, Response::Joined]
		};
		for (path, frames, patch) in [
			("direct", vec![too_large(true)], true),
			("direct", vec![too_large(false)], false),
			("joined", joined(true), true),
			("joined", joined(false), false),
		] {
			let (res, broken) = call_with_frames(frames);
			let err = res.unwrap_err();
			assert_eq!(err.code(), Some(ErrorCode::TooLarge), "{path} {err:?}");
			assert!(!broken, "{path} patch={patch}: connection marked broken");
			match (GitError::from(err), patch) {
				(GitError::PatchLimit { .. }, true) => {}
				(GitError::OutputLimit { .. }, false) => {}
				(other, _) => panic!("{path} patch={patch}: {other:?}"),
			}
		}
	}

	#[test]
	fn exchange_eof_returns_unexpected_eof() {
		let mut stream = FakeDuplex::new(&[]);
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			None,
			IO_TIMEOUT,
			Instant::now() + Duration::from_secs(10),
			&mut frames,
		)
		.unwrap_err();

		match err {
			RemoteError::Io(e) => {
				assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
			}
			other => panic!("expected Io(UnexpectedEof), got {other:?}"),
		}
		assert_eq!(frames, 0);
	}

	/// A stream that ends between frames is the worker having closed: the
	/// reason says so in its own words, while the CLI keeps its sentence and
	/// a reused connection is still tried again.
	#[test]
	fn a_clean_close_mid_call_is_worker_closed() {
		let mut stream = FakeDuplex::new(&[]);
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			None,
			IO_TIMEOUT,
			Instant::now() + Duration::from_secs(10),
			&mut frames,
		)
		.unwrap_err();
		assert_eq!(err.to_string(), "the worker closed the connection");
		assert_eq!(
			serde_json::to_value(err.reason()).unwrap(),
			serde_json::json!({"r": "worker_closed"})
		);
		assert!(should_resend(true, 0, &err));
	}

	#[test]
	fn git_error_conversion_table() {
		let err1 = RemoteError::Refused {
			code: ErrorCode::InvalidRevision,
			message: "bad rev".into(),
			reason: None,
		};
		assert!(matches!(
			GitError::from(err1),
			GitError::InvalidRevision(ref msg) if msg == "bad rev"
		));

		let err2 = RemoteError::Refused {
			code: ErrorCode::Cancelled,
			message: "stop".into(),
			reason: None,
		};
		assert!(matches!(
			GitError::from(err2),
			GitError::Cancelled { ref args } if args == "remote view"
		));

		let err3 = RemoteError::Cancelled;
		assert!(matches!(
			GitError::from(err3),
			GitError::Cancelled { ref args } if args == "remote view"
		));

		let err4 = RemoteError::Refused {
			code: ErrorCode::Timeout,
			message: "slow".into(),
			reason: None,
		};
		assert!(matches!(
			GitError::from(err4),
			GitError::Timeout { ref args, secs }
				if args == "remote view" && secs == GIT_CALL_LIMIT.as_secs()
		));

		let err5 = RemoteError::TimedOut;
		let g5 = GitError::from(err5);
		assert!(
			matches!(
				&g5,
				GitError::Remote { reason, .. }
					if matches!(**reason, Reason::TimedOut)
			),
			"{g5:?}"
		);

		let err6 = RemoteError::WorkerTooOld {
			worker: "w".into(),
			have: 1,
			need: 2,
		};
		let g6 = GitError::from(err6);
		assert!(
			matches!(
				&g6,
				GitError::Remote { reason, .. }
					if matches!(**reason, Reason::WorkerTooOld {
						have: 1,
						need: 2,
						..
					})
			),
			"{g6:?}"
		);

		let err7 = RemoteError::Io(io::Error::other("disk error"));
		let g7 = GitError::from(err7);
		assert_eq!(g7.to_string(), "disk error");
		assert!(matches!(
			g7,
			GitError::Remote { reason, .. }
				if matches!(*reason, Reason::Disconnected { .. })
		));

		let err8 = RemoteError::Refused {
			code: ErrorCode::NotFound,
			message: "missing".into(),
			reason: None,
		};
		assert!(matches!(
			GitError::from(err8),
			GitError::Remote {
				ref reason,
				ref message,
			} if matches!(
				**reason,
				Reason::Refused { code: ErrorCode::NotFound }
			) && message == "missing"
		));

		// A worker that did not start keeps its known cause for the GUI, and
		// the CLI's English text unchanged (#206).
		let started = RemoteError::Start {
			kind: ConnectFailureKind::WorkerUnavailable,
			reason: Some(StartReason::NoWorker),
			detail: Datum::for_test("sh: snip: not found"),
			message: "snip is not installed on that machine".into(),
		};
		let git = GitError::from(started);
		assert!(
			matches!(
				git,
				GitError::Remote { ref reason, .. }
					if matches!(
						**reason,
						Reason::Start {
							start: Some(StartReason::NoWorker),
							..
						}
					)
			),
			"{git:?}"
		);
		assert_eq!(git.to_string(), "snip is not installed on that machine");
		let unknown = RemoteError::Start {
			kind: ConnectFailureKind::Other,
			reason: None,
			detail: Datum::for_test("boom"),
			message: "boom".into(),
		};
		assert!(matches!(
			GitError::from(unknown),
			GitError::Remote { reason, .. }
				if matches!(*reason, Reason::Start { start: None, .. })
		));

		// A worker's own reason rides through to the Git view's error, and
		// without one the code words it.
		let typed = refusal(
			ErrorCode::Busy,
			"busy".into(),
			false,
			Some(Reason::GitBusy),
		);
		assert_eq!(typed.reason(), Reason::GitBusy);
		assert!(matches!(
			GitError::from(typed),
			GitError::Remote { reason, message }
				if *reason == Reason::GitBusy && message == "busy"
		));
		let bare = refusal(ErrorCode::Busy, "busy".into(), false, None);
		assert_eq!(
			bare.reason(),
			Reason::Refused {
				code: ErrorCode::Busy
			}
		);
		// A diff over its limit keeps its own variant whatever the reason.
		assert!(matches!(
			refusal(
				ErrorCode::TooLarge,
				"m".into(),
				true,
				Some(Reason::PatchTooLarge)
			),
			RemoteError::PatchTooLarge { .. }
		));

		let err_too_large = RemoteError::Refused {
			code: ErrorCode::TooLarge,
			message: "refs payload exceeded frame limit".into(),
			reason: None,
		};
		assert!(matches!(
			GitError::from(err_too_large),
			GitError::OutputLimit { ref args, limit }
				if args == "remote view" && limit == 0
		));
		// TooLarge with a reason that refines it is not the output limit:
		// the worker's "repository too large to verify" keeps its sentence.
		let verify = refusal(
			ErrorCode::TooLarge,
			"repository too large to verify".into(),
			false,
			Some(Reason::VerifyLimit),
		);
		assert!(matches!(
			GitError::from(verify),
			GitError::Remote { reason, .. }
				if *reason == Reason::VerifyLimit
		));
		let plain = refusal(
			ErrorCode::TooLarge,
			"m".into(),
			false,
			Some(Reason::TooLarge),
		);
		assert!(matches!(
			GitError::from(plain),
			GitError::OutputLimit { .. }
		));
	}

	#[test]
	fn should_resend_predicate_table() {
		let reset =
			RemoteError::Io(io::Error::from(io::ErrorKind::ConnectionReset));
		let timed_out =
			RemoteError::Io(io::Error::from(io::ErrorKind::TimedOut));
		let would_block =
			RemoteError::Io(io::Error::from(io::ErrorKind::WouldBlock));
		let refused = RemoteError::Refused {
			code: ErrorCode::Unsupported,
			message: "no".into(),
			reason: None,
		};

		// reused + 0 frames + ConnectionReset -> true
		assert!(should_resend(true, 0, &reset));
		// not reused -> false
		assert!(!should_resend(false, 0, &reset));
		// reused + 1 frame -> false
		assert!(!should_resend(true, 1, &reset));
		// TimedOut / WouldBlock -> false
		assert!(!should_resend(true, 0, &timed_out));
		assert!(!should_resend(true, 0, &would_block));
		// Cancelled / TimedOut / Refused -> false
		assert!(!should_resend(true, 0, &RemoteError::Cancelled));
		assert!(!should_resend(true, 0, &RemoteError::TimedOut));
		assert!(!should_resend(true, 0, &refused));
	}

	#[test]
	fn in_flight_limiter_bounds_and_cancels() {
		let limiter = GitLimiter::new();
		let g1 = limiter
			.acquire(None, scaled(Duration::from_secs(1)))
			.unwrap();
		let g2 = limiter
			.acquire(None, scaled(Duration::from_secs(1)))
			.unwrap();
		let g3 = limiter
			.acquire(None, scaled(Duration::from_secs(1)))
			.unwrap();
		let g4 = limiter
			.acquire(None, scaled(Duration::from_secs(1)))
			.unwrap();

		// 5th with short limit returns TimedOut
		let timed_out = limiter
			.acquire(None, scaled(Duration::from_millis(20)))
			.unwrap_err();
		assert!(matches!(timed_out, RemoteError::TimedOut));

		// 5th with cancelled token returns Cancelled
		let cancel = CancelToken::new();
		cancel.cancel();
		let cancelled = limiter
			.acquire(Some(&cancel), scaled(Duration::from_secs(1)))
			.unwrap_err();
		assert!(matches!(cancelled, RemoteError::Cancelled));

		// Release one guard lets the 5th through
		drop(g1);
		let g5 = limiter
			.acquire(None, scaled(Duration::from_millis(100)))
			.expect("releasing one guard should allow acquisition");
		drop((g2, g3, g4, g5));
	}

	/// A worker that keeps speaking while nobody listens cannot make the
	/// master buffer without end: the FIRST unsolicited frame ends the pump
	/// (the connection is discarded at its next use), and the ssh writer is
	/// left with a closed pipe.
	#[test]
	fn an_idle_flood_of_frames_is_bounded_and_ends_the_read() {
		let (r, mut w) = io::pipe().unwrap();
		let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
		let retained = Arc::new(AtomicUsize::new(0));
		let in_flight = Arc::new(AtomicBool::new(false));
		let pump_retained = Arc::clone(&retained);
		let pump_in_flight = Arc::clone(&in_flight);
		let pump = std::thread::spawn(move || {
			pump_frames(r, tx, pump_retained, pump_in_flight);
		});
		// ~64 KiB frames: the OS pipe cannot buffer many, so the producer
		// stalls as soon as the pump does.
		let body = "x".repeat(64 * 1024);
		let producer = std::thread::spawn(move || {
			let mut written = 0usize;
			for _ in 0..10_000 {
				let frame = Response::Text {
					content: Some(body.clone()),
				};
				if write_frame(&mut w, &frame).is_err() {
					break;
				}
				written += 1;
			}
			written
		});

		// The pump hangs up on the first unsolicited frame; the producer
		// then hits a closed pipe well before 10,000 frames.
		let producer = producer.join().unwrap();
		pump.join().unwrap();
		assert_eq!(
			retained.load(std::sync::atomic::Ordering::Relaxed),
			1,
			"one unsolicited frame is queued as the marker, no more"
		);
		assert!(
			producer < 200,
			"an unconsumed pump must stop early, wrote {producer}"
		);
		drop(rx);
	}

	/// A pump parked in `send` (queue full, consumer gone) ends when the
	/// connection drops the receiver — bounded, never stuck for good.
	#[test]
	fn the_pump_unblocks_when_the_connection_drops() {
		let (r, mut w) = io::pipe().unwrap();
		let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
		let retained = Arc::new(AtomicUsize::new(0));
		let in_flight = Arc::new(AtomicBool::new(true));
		let pump_retained = Arc::clone(&retained);
		let pump_in_flight = Arc::clone(&in_flight);
		let pump = std::thread::spawn(move || {
			pump_frames(r, tx, pump_retained, pump_in_flight);
		});
		let body = "x".repeat(64 * 1024);
		let producer = std::thread::spawn(move || {
			for _ in 0..40 {
				let frame = Response::Text {
					content: Some(body.clone()),
				};
				if write_frame(&mut w, &frame).is_err() {
					break;
				}
			}
		});
		// The queue (plus the pipe) fills; the pump parks in `send`.
		std::thread::sleep(scaled(Duration::from_millis(200)));
		drop(rx);
		assert!(
			join_bounded(pump, scaled(Duration::from_secs(5))),
			"dropping the receiver must unblock the parked pump"
		);
		producer.join().unwrap();
	}

	/// A segmented reply far larger than the frame queue, with the consumer
	/// paused while it streams in: the pump must apply backpressure (block)
	/// rather than hang up, and every piece still arrives once the consumer
	/// resumes.
	#[test]
	fn a_paused_consumer_still_receives_a_20mib_segmented_reply() {
		let (r, w) = io::pipe().unwrap();
		let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
		let retained = Arc::new(AtomicUsize::new(0));
		let in_flight = Arc::new(AtomicBool::new(false));
		let pump_retained = Arc::clone(&retained);
		let pump_in_flight = Arc::clone(&in_flight);
		let pump = std::thread::spawn(move || {
			pump_frames(r, tx, pump_retained, pump_in_flight);
		});
		let piece = "x".repeat(1024 * 1024);
		let producer = std::thread::spawn(move || {
			let mut w = w;
			let mut wrote = 0usize;
			for _ in 0..20 {
				if write_frame(
					&mut w,
					&Response::Chunk {
						data: piece.clone(),
					},
				)
				.is_err()
				{
					break;
				}
				wrote += 1;
			}
			let _ = write_frame(
				&mut w,
				&Response::Copied(CopyOutcome {
					payload: String::new(),
					copied: 1,
					chars: 0,
					lines: 0,
					words: 0,
					tokens: 0,
					skipped: 0,
					truncated: false,
				}),
			);
			wrote
		});
		// The exchange is running; its consumer is paused while the reply
		// streams in.
		in_flight.store(true, Ordering::SeqCst);
		std::thread::sleep(scaled(Duration::from_millis(300)));
		let mut conn = test_conn(
			Box::new(std::io::sink()),
			rx,
			Arc::clone(&retained),
			Arc::clone(&in_flight),
		);
		let deadline = Instant::now() + scaled(Duration::from_secs(30));
		let mut text = String::new();
		let reply = loop {
			assert!(
				Instant::now() < deadline,
				"the reply never finished arriving"
			);
			match conn.recv(scaled(Duration::from_secs(5))).unwrap() {
				Some(Response::Chunk { data }) => text.push_str(&data),
				Some(other) => break other,
				None => panic!("the pump hung up while a reply was streaming"),
			}
		};
		in_flight.store(false, Ordering::SeqCst);
		match reply {
			Response::Copied(out) => assert_eq!(out.copied, 1),
			other => panic!("expected Copied, got {other:?}"),
		}
		assert_eq!(text.len(), 20 * 1024 * 1024);
		assert_eq!(
			producer.join().unwrap(),
			20,
			"the producer wrote every piece"
		);
		drop(conn);
		assert!(
			join_bounded(pump, scaled(Duration::from_secs(5))),
			"dropping the receiver must end the pump"
		);
	}

	/// Joins `handle` within `limit`, so a hung thread fails the test
	/// instead of hanging it.
	fn join_bounded(
		handle: std::thread::JoinHandle<()>,
		limit: Duration,
	) -> bool {
		let (tx, rx) = mpsc::channel();
		std::thread::spawn(move || {
			let _ = handle.join();
			let _ = tx.send(());
		});
		rx.recv_timeout(limit).is_ok()
	}

	/// Every consumed frame releases its retained slot, so a call that
	/// drains its answers leaves the connection reusable.
	#[test]
	fn consuming_frames_releases_the_retained_cap() {
		let (r, w) = io::pipe().unwrap();
		let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
		let retained = Arc::new(AtomicUsize::new(0));
		for i in 0..3 {
			retained.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
			tx.send(Ok(Some(Response::Text {
				content: Some(i.to_string()),
			})))
			.unwrap();
		}
		let mut conn = test_conn(
			Box::new(w),
			rx,
			Arc::clone(&retained),
			Arc::new(AtomicBool::new(false)),
		);
		drop(r);
		for _ in 0..3 {
			conn.recv(Duration::from_secs(1)).unwrap().unwrap();
		}
		assert_eq!(conn.unsolicited_frames(), 0);
		assert_eq!(
			retained.load(std::sync::atomic::Ordering::Relaxed),
			0,
			"the queue is drained: a pooled connection is clean"
		);
	}

	/// A pooled connection that dies mid-request, and a restart that fails
	/// for a known reason: the caller gets the reason (it words it in its
	/// own language), not the dead connection's English I/O text.
	#[cfg(unix)]
	#[test]
	fn a_restart_that_fails_with_a_known_reason_outranks_the_dead_connection() {
		struct DeadPipe;
		impl io::Write for DeadPipe {
			fn write(&mut self, _: &[u8]) -> io::Result<usize> {
				Err(io::Error::from(io::ErrorKind::BrokenPipe))
			}
			fn flush(&mut self) -> io::Result<()> {
				Ok(())
			}
		}
		let host = RemoteHost {
			name: "gone".into(),
			// The command exits 127 before any hello: snip is not there.
			transport: Transport::Command(vec![
				"/bin/sh".into(),
				"-c".into(),
				"exit 127".into(),
			]),
		};
		let client = Client::new(host, "test".into());
		let dead = test_conn(
			Box::new(DeadPipe),
			mpsc::sync_channel(1).1,
			Arc::default(),
			Arc::default(),
		);
		client.keep(dead);
		let err = client
			.call(&Request::Stat {
				workspace: "w".into(),
				path: "a".into(),
			})
			.unwrap_err();
		assert!(
			matches!(
				err,
				RemoteError::Start {
					reason: Some(StartReason::NoWorker),
					..
				}
			),
			"{err:?}"
		);
	}

	/// The same precedence in the paste plans: a dead pooled connection,
	/// then a restart that exits 127. Planning is read-only and resent once;
	/// the known start reason outranks the dead connection's I/O error.
	#[cfg(unix)]
	#[test]
	fn a_known_start_reason_outranks_the_dead_connection_in_paste_plans() {
		let gone = || {
			let client = Client::new(
				RemoteHost {
					name: "gone".into(),
					transport: Transport::Command(vec![
						"/bin/sh".into(),
						"-c".into(),
						"exit 127".into(),
					]),
				},
				"test".into(),
			);
			client.pool_a_dead_connection_for_tests();
			client
		};
		let no_worker = |err: &RemoteError| {
			matches!(
				err,
				RemoteError::Start {
					reason: Some(StartReason::NoWorker),
					..
				}
			)
		};
		let err = gone()
			.prepare_import("/w", "", "source", &PasteMapping::default(), None)
			.unwrap_err();
		assert!(no_worker(&err), "prepare_import: {err:?}");
		let err = gone()
			.prepare_replay(
				"/w",
				"",
				"source",
				snip_core::commits::CommitsPayload { commits: vec![] },
				None,
			)
			.unwrap_err();
		assert!(no_worker(&err), "prepare_replay: {err:?}");
	}

	/// A writer the write side can be parked inside, like `ChildStdin` on a
	/// worker that stopped reading.
	struct ParkedWriter(std::sync::Mutex<Option<mpsc::Receiver<()>>>);

	impl io::Write for ParkedWriter {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			let receiver =
				self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
			match receiver {
				Some(receiver) => {
					// Park forever, as a write to a full pipe does, until
					// the gate is dropped.
					let _ = receiver.recv();
					Err(io::Error::from(io::ErrorKind::BrokenPipe))
				}
				None => Ok(buf.len()),
			}
		}
		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	fn parked_connection(gate: mpsc::Receiver<()>) -> Connection {
		test_conn(
			Box::new(ParkedWriter(std::sync::Mutex::new(Some(gate)))),
			mpsc::sync_channel(1).1,
			Arc::default(),
			Arc::default(),
		)
	}

	/// A request the far end never reads fails by its deadline instead of
	/// blocking forever, and the call returns in time.
	#[test]
	fn a_request_nobody_reads_fails_by_its_deadline() {
		let (gate_tx, gate_rx) = mpsc::channel();
		let mut conn = parked_connection(gate_rx);
		// A body big enough that no pipe buffer swallows it whole.
		let request = Request::ImportPlan {
			workspace: "w".into(),
			dest: String::new(),
			text: "x".repeat(1024 * 1024),
			mapping: Default::default(),
		};
		let started = Instant::now();
		let deadline = started + scaled(Duration::from_millis(300));
		let err = conn.send_guarded(&request, None, deadline).unwrap_err();
		assert!(matches!(err, RemoteError::TimedOut), "{err:?}");
		assert!(
			started.elapsed() < scaled(Duration::from_secs(5)),
			"the send must fail by its deadline, took {:?}",
			started.elapsed()
		);
		drop(gate_tx);
	}

	/// A cancelled send fails at once instead of blocking forever.
	#[test]
	fn a_cancelled_send_fails_at_once() {
		let (gate_tx, gate_rx) = mpsc::channel();
		let mut conn = parked_connection(gate_rx);
		let cancel = CancelToken::new();
		cancel.cancel();
		let request = Request::OpenWorkspace { path: "~".into() };
		let started = Instant::now();
		let deadline = started + scaled(Duration::from_secs(30));
		let err = conn
			.send_guarded(&request, Some(&cancel), deadline)
			.unwrap_err();
		assert!(matches!(err, RemoteError::Cancelled), "{err:?}");
		assert!(started.elapsed() < scaled(Duration::from_secs(2)));
		drop(gate_tx);
	}

	/// A send that goes out in time hands the writer back: the connection
	/// carries on reading its answers.
	#[test]
	fn a_send_that_goes_out_keeps_the_connection_whole() {
		let (r, w) = io::pipe().unwrap();
		let mut conn = test_conn(
			Box::new(w),
			mpsc::sync_channel(1).1,
			Arc::default(),
			Arc::default(),
		);
		let request = Request::OpenWorkspace { path: "~".into() };
		let deadline = Instant::now() + scaled(Duration::from_secs(5));
		conn.send_guarded(&request, None, deadline).unwrap();
		// The pipe holds one small frame; the read side sees a clean end
		// once it is dropped.
		drop(r);
		assert_eq!(conn.recv(scaled(Duration::from_secs(1))).unwrap(), None);
	}

	/// Every received frame checks the call's absolute deadline, chunk
	/// frames included: a worker may not stretch one call by dripping
	/// chunks.
	#[test]
	fn exchange_holds_chunk_frames_to_the_absolute_deadline() {
		let mut buf = Vec::new();
		for i in 0..3 {
			write_frame(
				&mut buf,
				&Response::Chunk {
					data: format!("piece {i}"),
				},
			)
			.unwrap();
		}
		let mut stream = SlowDuplex {
			incoming: io::Cursor::new(buf),
			pause: scaled(Duration::from_millis(200)),
		};
		let mut frames = 0;
		let deadline = Instant::now() + scaled(Duration::from_millis(300));
		let err =
			exchange(&mut stream, None, IO_TIMEOUT, deadline, &mut frames)
				.unwrap_err();
		assert!(matches!(err, RemoteError::TimedOut), "{err:?}");
	}

	/// A reply that takes longer than the idle timeout to arrive whole, but
	/// never goes silent for that long, must succeed: the idle timeout
	/// bounds the stream's silence, not a frame's total transfer. On the
	/// old whole-frame reading this call always timed out.
	#[test]
	fn a_slow_but_steady_reply_outlasts_the_idle_timeout() {
		let (r, mut w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		let response = Response::Text {
			content: Some("x".repeat(1024 * 1024)),
		};
		let mut bytes = Vec::new();
		write_frame(&mut bytes, &response).unwrap();
		let producer = std::thread::spawn(move || {
			// 16 KiB every 50 ms: about three seconds of streaming, three
			// times the idle bound below, while every silence stays far
			// under it. The bound leaves room for decoding the frame after
			// its last byte, which is silence too.
			for piece in bytes.chunks(16 * 1024) {
				if w.write_all(piece).is_err() {
					return;
				}
				std::thread::sleep(scaled(Duration::from_millis(50)));
			}
		});
		let mut conn =
			test_conn(Box::new(std::io::sink()), frames, retained, in_flight);
		conn.last_byte = last_byte;
		let mut frames = 0;
		let reply = exchange(
			&mut conn,
			None,
			scaled(Duration::from_secs(1)),
			Instant::now() + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.expect("a steadily arriving reply must not time out");
		match reply {
			Response::Text { content } => {
				assert_eq!(content.map(|c| c.len()), Some(1024 * 1024));
			}
			other => panic!("expected Text, got {other:?}"),
		}
		assert_eq!(frames, 1);
		producer.join().unwrap();
	}

	/// A stream that goes silent — bytes stopped, no end of stream — ends
	/// the call by the idle timeout, well before the call's own deadline.
	#[test]
	fn a_worker_gone_silent_ends_the_call_by_the_idle_timeout() {
		let (r, w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		let mut conn =
			test_conn(Box::new(std::io::sink()), frames, retained, in_flight);
		conn.last_byte = last_byte;
		let mut frames = 0;
		let started = Instant::now();
		let err = exchange(
			&mut conn,
			None,
			scaled(Duration::from_millis(300)),
			started + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.unwrap_err();
		let text = err.to_string();
		assert!(
			matches!(
				err,
				RemoteError::Stalled {
					received: 0,
					ended: WaitEnd::Silence,
					..
				}
			),
			"{err:?}"
		);
		assert!(text.contains("did not answer in time"), "{text}");
		assert!(text.contains("0 bytes"), "{text}");
		assert!(
			started.elapsed() < scaled(Duration::from_secs(5)),
			"the silence must end the call promptly, took {:?}",
			started.elapsed()
		);
		// The pipe stays open until here: no end of stream, only silence.
		drop(w);
	}

	/// A reply that has started arriving may pause longer than the idle
	/// bound mid-frame: the worker answered and the link is slow. The pause
	/// here is under REPLY_IDLE_FACTOR times the bound, so the call
	/// succeeds (#191).
	#[test]
	fn a_reply_paused_mid_frame_outlasts_the_idle_bound() {
		let (r, mut w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		let response = Response::Text {
			content: Some("x".repeat(64 * 1024)),
		};
		let mut bytes = Vec::new();
		write_frame(&mut bytes, &response).unwrap();
		let rest = bytes.split_off(bytes.len() / 2);
		let producer = std::thread::spawn(move || {
			if w.write_all(&bytes).is_err() {
				return;
			}
			std::thread::sleep(scaled(Duration::from_millis(900)));
			let _ = w.write_all(&rest);
		});
		let mut conn =
			test_conn(Box::new(std::io::sink()), frames, retained, in_flight);
		conn.last_byte = last_byte;
		let mut frames = 0;
		let reply = exchange(
			&mut conn,
			None,
			scaled(Duration::from_millis(300)),
			Instant::now() + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.expect("a reply paused mid-frame within the bound must succeed");
		match reply {
			Response::Text { content } => {
				assert_eq!(content.map(|c| c.len()), Some(64 * 1024));
			}
			other => panic!("expected Text, got {other:?}"),
		}
		assert_eq!(frames, 1);
		producer.join().unwrap();
	}

	/// A reply that stops partway through a frame, with the pipe still open,
	/// ends the call as a stall that says how much arrived and how long the
	/// call ran (#191).
	#[test]
	fn a_reply_stalled_mid_frame_reports_its_bytes_and_time() {
		let (r, mut w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		let mut bytes = Vec::new();
		write_frame(
			&mut bytes,
			&Response::Text {
				content: Some("x".repeat(64 * 1024)),
			},
		)
		.unwrap();
		assert!(bytes.len() > 10_000, "the frame must outlast the cut");
		w.write_all(&bytes[..10_000]).unwrap();
		let mut conn =
			test_conn(Box::new(std::io::sink()), frames, retained, in_flight);
		conn.last_byte = last_byte;
		let mut frames = 0;
		let started = Instant::now();
		let err = exchange(
			&mut conn,
			None,
			scaled(Duration::from_millis(200)),
			started + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.unwrap_err();
		assert!(
			started.elapsed() < scaled(Duration::from_secs(10)),
			"the stall must end the call promptly, took {:?}",
			started.elapsed()
		);
		match err {
			RemoteError::Stalled {
				received, elapsed, ..
			} => {
				assert_eq!(received, 10_000);
				assert!(elapsed > 0.0, "elapsed {elapsed}");
			}
			other => panic!("expected Stalled, got {other:?}"),
		}
		// The pipe stays open until the assertions have run.
		drop(w);
	}

	/// A pipe-fed connection whose exchange is in flight, as the pump needs
	/// for a reply of several frames: with no call in flight it ends after
	/// the first whole frame (it reads that as unsolicited). Returns the
	/// pipe's write end and the queue count of frames the pump has retained.
	fn pipe_conn() -> (Connection, io::PipeWriter, Arc<AtomicUsize>) {
		let (r, w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		in_flight.store(true, Ordering::SeqCst);
		let mut conn = test_conn(
			Box::new(std::io::sink()),
			frames,
			Arc::clone(&retained),
			in_flight,
		);
		conn.last_byte = last_byte;
		(conn, w, retained)
	}

	/// Waits, bounded, until the pump has queued `n` frames.
	fn wait_for_retained(retained: &AtomicUsize, n: usize) {
		let end = Instant::now() + scaled(Duration::from_secs(5));
		while retained.load(Ordering::Relaxed) < n {
			assert!(Instant::now() < end, "the pump never queued {n} frames");
			std::thread::sleep(Duration::from_millis(5));
		}
	}

	fn frame_bytes(response: &Response) -> Vec<u8> {
		let mut bytes = Vec::new();
		write_frame(&mut bytes, response).unwrap();
		bytes
	}

	/// The frames of a 64 KiB `Text` reply sent as two chunks and a join.
	fn chunked_text_frames() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
		let json = serde_json::to_string(&Response::Text {
			content: Some("x".repeat(64 * 1024)),
		})
		.unwrap();
		let (first, second) = json.split_at(json.len() / 2);
		(
			frame_bytes(&Response::Chunk { data: first.into() }),
			frame_bytes(&Response::Chunk {
				data: second.into(),
			}),
			frame_bytes(&Response::Joined),
		)
	}

	/// A pause between the frames of one reply is silence the reply has
	/// already begun: it gets the long bound, as a pause inside a frame does
	/// (#191). The first chunk was consumed when the pause began, so a bound
	/// that reset at every consumed frame treated the pause as a new wait.
	#[test]
	fn a_reply_paused_between_chunk_frames_outlasts_the_idle_bound() {
		let (mut conn, mut w, _retained) = pipe_conn();
		let (first, second, joined) = chunked_text_frames();
		let producer = std::thread::spawn(move || {
			if w.write_all(&first).is_err() {
				return;
			}
			std::thread::sleep(scaled(Duration::from_millis(900)));
			let _ = w.write_all(&second);
			let _ = w.write_all(&joined);
		});
		let mut frames = 0;
		let reply = exchange(
			&mut conn,
			None,
			scaled(Duration::from_millis(300)),
			Instant::now() + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.expect(
			"a pause between a reply's frames within the bound must succeed",
		);
		match reply {
			Response::Text { content } => {
				assert_eq!(content.map(|c| c.len()), Some(64 * 1024));
			}
			other => panic!("expected Text, got {other:?}"),
		}
		assert_eq!(frames, 3);
		producer.join().unwrap();
	}

	/// After a heartbeat the worker is alive and working: silence from there
	/// ends only at the long bound, not at the short one that guards a
	/// connection that never answered (#191).
	#[test]
	fn a_reply_silent_after_a_heartbeat_ends_only_at_the_long_bound() {
		let (mut conn, mut w, retained) = pipe_conn();
		let pending = frame_bytes(&Response::Pending);
		w.write_all(&pending).unwrap();
		wait_for_retained(&retained, 1);
		let idle = scaled(Duration::from_millis(100));
		let mut frames = 0;
		let started = Instant::now();
		let err = exchange(
			&mut conn,
			None,
			idle,
			started + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.unwrap_err();
		match err {
			RemoteError::Stalled {
				received,
				ended: WaitEnd::Silence,
				..
			} => assert_eq!(received, pending.len() as u64),
			other => panic!("expected a Silence stall, got {other:?}"),
		}
		assert!(
			started.elapsed() >= idle * 5,
			"the wait after a heartbeat must outlast the idle bound, ended after {:?}",
			started.elapsed()
		);
		assert!(
			started.elapsed() < scaled(Duration::from_secs(5)),
			"the wait must still end, took {:?}",
			started.elapsed()
		);
		assert_eq!(frames, 1);
		// The pipe stays open until the assertions have run.
		drop(w);
	}

	/// The same pause after a heartbeat that ends in the answer: a worker
	/// that heartbeats and then answers late is not stalled.
	#[test]
	fn a_reply_paused_after_a_heartbeat_outlasts_the_idle_bound() {
		let (mut conn, mut w, _retained) = pipe_conn();
		let pending = frame_bytes(&Response::Pending);
		let text = frame_bytes(&Response::Text {
			content: Some("done".into()),
		});
		let producer = std::thread::spawn(move || {
			if w.write_all(&pending).is_err() {
				return;
			}
			std::thread::sleep(scaled(Duration::from_millis(900)));
			let _ = w.write_all(&text);
		});
		let mut frames = 0;
		let reply = exchange(
			&mut conn,
			None,
			scaled(Duration::from_millis(300)),
			Instant::now() + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.expect("a pause after a heartbeat within the bound must succeed");
		assert!(
			matches!(reply, Response::Text { content: Some(ref c) } if c == "done"),
			"{reply:?}"
		);
		assert_eq!(frames, 2);
		producer.join().unwrap();
	}

	/// The call's own time limit ends a reply that keeps flowing, and says
	/// how much arrived and for how long, like a silence does (#191).
	#[test]
	fn the_call_deadline_with_bytes_flowing_reports_bytes_and_time() {
		let (mut conn, mut w, _retained) = pipe_conn();
		let bytes = frame_bytes(&Response::Text {
			content: Some("x".repeat(1024 * 1024)),
		});
		// Detached: the frame takes longer than the call, and the producer
		// ends when the pipe's reader is gone.
		std::thread::spawn(move || {
			for piece in bytes.chunks(16 * 1024) {
				if w.write_all(piece).is_err() {
					return;
				}
				std::thread::sleep(scaled(Duration::from_millis(50)));
			}
		});
		let mut frames = 0;
		let err = exchange(
			&mut conn,
			None,
			scaled(Duration::from_secs(1)),
			Instant::now() + scaled(Duration::from_millis(600)),
			&mut frames,
		)
		.unwrap_err();
		let text = err.to_string();
		match err {
			RemoteError::Stalled {
				received,
				elapsed,
				ended: WaitEnd::Deadline,
				..
			} => {
				assert!(received > 0, "received {received}");
				assert!(
					elapsed >= scaled(Duration::from_millis(500)).as_secs_f64(),
					"elapsed {elapsed}"
				);
			}
			other => panic!("expected a Deadline stall, got {other:?}"),
		}
		assert!(text.contains("bytes"), "{text}");
		assert!(text.contains("time limit"), "{text}");
	}

	/// The deadline check after a frame arrives reports progress too: here a
	/// heartbeat is already queued when the call's time is up.
	#[test]
	fn a_frame_past_the_deadline_reports_bytes_and_time() {
		let (mut conn, mut w, retained) = pipe_conn();
		let pending = frame_bytes(&Response::Pending);
		w.write_all(&pending).unwrap();
		wait_for_retained(&retained, 1);
		let mut frames = 0;
		let err = exchange(
			&mut conn,
			None,
			scaled(Duration::from_secs(1)),
			Instant::now(),
			&mut frames,
		)
		.unwrap_err();
		match err {
			RemoteError::Stalled {
				received,
				ended: WaitEnd::Deadline,
				..
			} => assert_eq!(received, pending.len() as u64),
			other => panic!("expected a Deadline stall, got {other:?}"),
		}
		assert_eq!(frames, 1);
		drop(w);
	}

	/// A reply not yet begun gets the idle bound, one that has begun
	/// REPLY_IDLE_FACTOR times it (#191).
	#[test]
	fn silence_budget_by_reply_state() {
		let idle = Duration::from_millis(300);
		for (started, expected) in
			[(false, idle), (true, idle * REPLY_IDLE_FACTOR)]
		{
			assert_eq!(silence_budget(idle, started), expected);
		}
	}

	#[test]
	fn stalled_text_names_how_the_wait_ended() {
		let stall = |ended| RemoteError::Stalled {
			silent: 5.0,
			received: 1234,
			elapsed: 6.0,
			ended,
		};
		let silence = stall(WaitEnd::Silence).to_string();
		let deadline = stall(WaitEnd::Deadline).to_string();
		for text in [&silence, &deadline] {
			assert!(
				text.starts_with("the worker did not answer in time"),
				"{text}"
			);
			assert!(text.contains("1234 bytes"), "{text}");
		}
		assert!(silence.contains("no data for"), "{silence}");
		assert!(deadline.contains("time limit passed"), "{deadline}");
	}

	/// A timeout keeps its progress when a Git caller reads it: the CLI
	/// prints this text, and 90 s was never what elapsed (#191).
	#[test]
	fn remote_timeouts_keep_their_progress_as_git_errors() {
		let stalled = GitError::from(RemoteError::Stalled {
			silent: 5.0,
			received: 1234,
			elapsed: 6.0,
			ended: WaitEnd::Silence,
		});
		assert!(stalled.to_string().contains("1234 bytes"), "{stalled}");
		assert!(matches!(stalled.reason(), Reason::TimedOut), "{stalled:?}");
		let bare = GitError::from(RemoteError::TimedOut);
		assert!(!bare.to_string().contains("90s"), "{bare}");
		assert!(matches!(bare.reason(), Reason::TimedOut), "{bare:?}");
	}

	#[test]
	fn stalled_is_a_transient_read_failure_with_unknown_outcome() {
		let error = RemoteError::Stalled {
			silent: 5.0,
			received: 1,
			elapsed: 6.0,
			ended: WaitEnd::Silence,
		};
		assert!(error.is_transient_read_failure());
		assert!(error.outcome_unknown());
	}

	/// A cancel ends a call whose worker never answers: the wait polls the
	/// token instead of sitting out the idle timeout or the call's limit.
	#[test]
	fn a_cancel_ends_a_call_with_no_answer() {
		let (r, w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		let mut conn =
			test_conn(Box::new(std::io::sink()), frames, retained, in_flight);
		conn.last_byte = last_byte;
		let token = CancelToken::new();
		let canceller = {
			let token = token.clone();
			std::thread::spawn(move || {
				std::thread::sleep(scaled(Duration::from_millis(100)));
				token.cancel();
			})
		};
		let mut frames = 0;
		let started = Instant::now();
		let err = exchange(
			&mut conn,
			Some(&token),
			scaled(Duration::from_secs(30)),
			started + scaled(Duration::from_secs(30)),
			&mut frames,
		)
		.unwrap_err();
		assert!(matches!(err, RemoteError::Cancelled), "{err:?}");
		assert!(
			started.elapsed() < scaled(Duration::from_secs(5)),
			"the cancel must end the call promptly, took {:?}",
			started.elapsed()
		);
		drop(w);
		canceller.join().unwrap();
	}

	/// A cancel ends an open whose transport never says hello (ssh still
	/// connecting to a host that does not answer): the open returns at
	/// once and the transport process is gone, instead of holding a tab's
	/// close past its drain deadline for ssh's and the hello's timeouts.
	#[cfg(unix)]
	#[test]
	fn a_cancel_ends_an_open_that_never_says_hello() {
		let tmp = tempfile::tempdir().unwrap();
		let pid_file = tmp.path().join("pid");
		let script =
			format!("echo $$ > '{}'; exec sleep 60", pid_file.display());
		let transport = Transport::Command(vec![
			"/bin/sh".into(),
			"-c".into(),
			script.into(),
		]);
		let token = CancelToken::new();
		let canceller = {
			let (token, pid_file) = (token.clone(), pid_file.clone());
			std::thread::spawn(move || {
				let limit = Instant::now() + scaled(Duration::from_secs(5));
				while !pid_file.exists() {
					assert!(
						Instant::now() < limit,
						"the transport never started"
					);
					std::thread::sleep(Duration::from_millis(10));
				}
				token.cancel();
			})
		};
		let started = Instant::now();
		let err = Connection::open_with(&transport, "test", Some(&token))
			.err()
			.expect("a cancelled open must fail");
		assert!(matches!(err, RemoteError::Cancelled), "{err:?}");
		assert!(
			started.elapsed() < scaled(Duration::from_secs(5)),
			"the cancel must end the open promptly, took {:?}",
			started.elapsed()
		);
		canceller.join().unwrap();
		let pid = std::fs::read_to_string(&pid_file).unwrap();
		let alive = std::process::Command::new("kill")
			.args(["-0", pid.trim()])
			.status()
			.unwrap()
			.success();
		assert!(
			!alive,
			"the transport {} must be killed and reaped",
			pid.trim()
		);

		// Already cancelled: nothing is started.
		let err = Connection::open_with(&transport, "test", Some(&token))
			.err()
			.expect("a cancelled open must fail");
		assert!(matches!(err, RemoteError::Cancelled), "{err:?}");
	}

	/// A connection whose idle timer has expired while sitting idle must
	/// not fail a new call immediately: the idle timer must be reset to
	/// the send time, so a worker answering within the idle timeout succeeds.
	#[test]
	fn a_call_resets_the_idle_timer_on_send() {
		let (r, mut w) = io::pipe().unwrap();
		let (frames, retained, in_flight, last_byte) = read_frames(r);
		let mut conn =
			test_conn(Box::new(std::io::sink()), frames, retained, in_flight);
		conn.last_byte = last_byte;
		// The connection sat idle well past IO_TIMEOUT.
		conn.last_byte
			.last_byte
			.store(now_millis().saturating_sub(60_000), Ordering::Relaxed);
		let worker = std::thread::spawn(move || {
			// The worker takes about half of idle to compute its answer:
			// longer than the 50 ms WAIT_SLICE, but well under IO_TIMEOUT.
			std::thread::sleep(Duration::from_millis(2500));
			let reply = Response::Workspace(RemoteWorkspace {
				id: "ws-1".into(),
				name: "test".into(),
				path: "/test/workspace".into(),
			});
			let mut buf = Vec::new();
			write_frame(&mut buf, &reply).unwrap();
			let _ = w.write_all(&buf);
		});
		let reply = conn
			.call(
				&Request::OpenWorkspace { path: "~".into() },
				None,
				scaled(Duration::from_secs(30)),
			)
			.expect("a call on an idle connection must reset the idle timer and succeed");
		match reply {
			Response::Workspace(ws) => assert_eq!(ws.path, "/test/workspace"),
			other => panic!("expected Workspace, got {other:?}"),
		}
		assert!(
			join_bounded(worker, scaled(Duration::from_secs(5))),
			"the worker thread must finish promptly"
		);
	}

	/// Replacing an out-of-step connection must refuse a worker too old
	/// for the request, as a fresh connect does — not the worker's generic
	/// `Unsupported` refusal.
	#[test]
	fn a_reconnect_after_unsolicited_frames_reports_a_worker_too_old() {
		use crate::worker::WorkerOptions;

		let worker = Arc::new(Worker::new(WorkerOptions {
			name: "old".into(),
			max_protocol: Some(1),
		}));
		let client = Client::new(RemoteHost::in_process(worker), "test".into());

		// A connection the worker spoke on with nobody asking: its reply
		// was pumped while no call was in flight, so the master discards
		// it at the next use.
		let mut stale = client.connect().unwrap();
		let deadline = Instant::now() + scaled(Duration::from_secs(5));
		stale
			.send_guarded(
				&Request::OpenWorkspace { path: "~".into() },
				None,
				deadline,
			)
			.unwrap();
		let limit = Instant::now() + scaled(Duration::from_secs(5));
		while stale.unsolicited_frames() == 0 {
			assert!(
				Instant::now() < limit,
				"the worker's unsolicited reply never arrived"
			);
			std::thread::sleep(Duration::from_millis(10));
		}
		client.idle.lock().unwrap().push(stale);

		let err = client
			.call_with(
				&Request::ScanRepos {
					workspace: "~".into(),
					under: None,
				},
				None,
				scaled(GIT_CALL_LIMIT),
			)
			.unwrap_err();
		match err {
			RemoteError::WorkerTooOld { have, need, .. } => {
				assert_eq!((have, need), (1, 2));
			}
			other => panic!("expected WorkerTooOld, got {other:?}"),
		}
	}

	/// The requests whose worker side runs git — the in-flight limiter's
	/// set. Every request type is listed, so a new one must be placed
	/// deliberately.
	#[test]
	fn requests_that_run_git_on_the_worker_are_limited() {
		use snip_core::gitsrc::GitSource;
		use snip_core::gitview::ReadProfile;

		let settings = snip_core::settings::Settings::default();
		let expect = ImportExpect {
			digest: String::new(),
			freshness: snip_core::transfer::DestinationFreshnessSnapshot {
				roots: Default::default(),
				target_files: Default::default(),
			},
		};
		let requests = [
			(
				Request::Hello {
					version: 1,
					name: "m".into(),
					max_version: None,
				},
				false,
			),
			(Request::OpenWorkspace { path: "~".into() }, false),
			(
				Request::ListDir {
					workspace: "w".into(),
					path: "p".into(),
					folders_only: false,
				},
				false,
			),
			(
				Request::Stat {
					workspace: "w".into(),
					path: "p".into(),
				},
				false,
			),
			(
				Request::Read {
					workspace: "w".into(),
					path: "p".into(),
				},
				false,
			),
			(
				Request::ReplayRead {
					workspace: "w".into(),
					path: "p".into(),
					limit: 1024,
				},
				true, // bounded by the same worker job admission as paste plans
			),
			(
				Request::Write {
					workspace: "w".into(),
					path: "p".into(),
					content: "c".into(),
				},
				false,
			),
			(
				Request::Rename {
					workspace: "w".into(),
					from: "a".into(),
					to: "b".into(),
				},
				false,
			),
			(
				Request::ScanRepos {
					workspace: "w".into(),
					under: None,
				},
				true,
			),
			(
				Request::GitView {
					workspace: "w".into(),
					repo: "r".into(),
					profile: ReadProfile::Interactive,
					query: GitQuery::ChangeList,
				},
				true,
			),
			(
				Request::Export {
					workspace: "w".into(),
					items: vec![],
					settings: settings.clone(),
					file_limit: 1,
				},
				true,
			),
			(
				Request::ExportCommits {
					workspace: "w".into(),
					repo: "r".into(),
					tip: "HEAD".into(),
					selected: vec!["a".into()],
				},
				true,
			),
			(
				Request::ExportChanges {
					workspace: "w".into(),
					repo: "r".into(),
					source: GitSource::Working,
					settings: settings.clone(),
					file_limit: 1,
				},
				true,
			),
			(
				Request::ImportPlan {
					workspace: "w".into(),
					dest: String::new(),
					text: String::new(),
					mapping: PasteMapping::default(),
				},
				true, // its freshness snapshot reads HEAD and the index
			),
			(
				Request::PrepareImport {
					workspace: "w".into(),
					dest: String::new(),
					text: String::new(),
					mapping: PasteMapping::default(),
					retain: true,
				},
				true,
			),
			(
				Request::ApplyPrepared {
					workspace: "w".into(),
					dest: String::new(),
					token: "token".into(),
					selection: Default::default(),
					expect: expect.clone(),
				},
				true,
			),
			(
				Request::ImportApply {
					workspace: "w".into(),
					dest: String::new(),
					text: String::new(),
					mapping: PasteMapping::default(),
					selection: Default::default(),
					expect: expect.clone(),
				},
				true, // its re-check reads HEAD and the index
			),
			(
				Request::ReplayPlan {
					workspace: "w".into(),
					dest: "d".into(),
					text: String::new(),
				},
				true,
			),
			(
				Request::ReplayApply {
					workspace: "w".into(),
					dest: "d".into(),
					text: String::new(),
					expect: ReplayExpect {
						destination: Default::default(),
						plan: snip_core::commits::CommitReplayPlan {
							root: Default::default(),
							commits: vec![],
						},
						freshness:
							snip_core::transfer::DestinationFreshnessSnapshot {
								roots: Default::default(),
								target_files: Default::default(),
							},
					},
					check_only: false,
				},
				true,
			),
			(Request::Chunk { data: "d".into() }, false),
			(Request::FrameChunk { data: "d".into() }, false),
			(Request::FrameJoin, false),
		];
		for (request, git) in requests {
			assert_eq!(runs_git(&request), git, "{request:?}");
		}
	}

	/// The known start failures carry a reason for callers that word them
	/// in their own language, and keep the transport's stderr untouched.
	#[test]
	fn start_reason_names_the_known_causes_and_keeps_stderr() {
		let reason = |cause, code, stderr| start_reason(&cause, code, stderr);
		assert_eq!(
			reason(StartCause::Ended, Some(127), ""),
			Some(StartReason::NoWorker)
		);
		assert_eq!(
			reason(
				StartCause::Ended,
				Some(2),
				"error: unrecognized subcommand 'serve'"
			),
			Some(StartReason::NoWorker)
		);
		assert_eq!(
			reason(
				StartCause::Ended,
				Some(255),
				"u@h: Permission denied (publickey)."
			),
			Some(StartReason::KeyLogin)
		);
		assert_eq!(
			reason(StartCause::NoAnswer, None, ""),
			Some(StartReason::NoAnswer)
		);
		assert_eq!(
			reason(StartCause::Ended, Some(255), ""),
			Some(StartReason::Closed)
		);
		// Unknown stderr and a stream's own message stay unclassified.
		assert_eq!(reason(StartCause::Ended, Some(1), "boom"), None);
		assert_eq!(reason(StartCause::NoAnswer, None, "ssh: x"), None);
		assert_eq!(reason(StartCause::BadData("bad".into()), None, ""), None);
	}

	/// A transport that is alive but silent through the whole hello wait
	/// says so, instead of the old closed-connection message.
	#[test]
	fn a_silent_alive_transport_says_it_did_not_answer() {
		let msg = start_message(StartCause::NoAnswer, None, "");
		assert!(msg.contains("did not answer in time"), "{msg}");
		assert!(!msg.contains("closed the connection"), "{msg}");
	}

	/// A timeout still defers to what the transport itself said.
	#[test]
	fn a_timed_out_hello_keeps_stderr_that_says_why() {
		let msg = start_message(StartCause::NoAnswer, None, "ssh: swapped out");
		assert_eq!(msg, "ssh: swapped out");
	}

	/// A stream that broke the protocol keeps its own message: a login
	/// shell printing too much is named as such.
	#[test]
	fn a_banner_over_the_cap_keeps_its_own_message() {
		let msg = start_message(
			StartCause::BadData(
				"the remote shell printed too much before snip started".into(),
			),
			Some(0),
			"",
		);
		assert!(msg.contains("printed too much"), "{msg}");
	}

	/// The ended-transport classification is what it was.
	#[test]
	fn an_ended_transport_keeps_the_old_classification() {
		assert_eq!(
			start_message(StartCause::Ended, None, ""),
			"the remote end closed the connection before snip started"
		);
		assert!(start_message(StartCause::Ended, Some(127), "")
			.contains("not installed"));
		assert!(start_message(
			StartCause::Ended,
			Some(1),
			"unrecognized subcommand"
		)
		.contains("not installed"));
		assert!(start_message(
			StartCause::Ended,
			Some(255),
			"Permission denied"
		)
		.contains("ssh-copy-id"));
		assert_eq!(start_message(StartCause::Ended, Some(1), "boom"), "boom");
	}
}
