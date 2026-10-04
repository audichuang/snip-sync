//! The master: starts a worker on another machine over ssh
//! (`ssh <host> snip serve --stdio`) and calls it on that process's stdin
//! and stdout. Every call blocks; the desktop app makes them on its
//! background executor, as it does local reads.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read as IoRead, Write as IoWrite};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
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

use snip_core::commits::CommitCopyOutcome;
use snip_core::transfer::CopyOutcome;

use crate::proto::{
	read_frame, write_request, DirEntry, ErrorCode, ExportTarget, GitQuery,
	GitReply, ImportExpect, ImportPlanned, PasteMapping, RemoteWorkspace,
	ReplayExpect, RepoScan, Request, Response, Stat, GIT_CALL_LIMIT,
	JOINED_MAX, MAX_GIT_CALLS_IN_FLIGHT, PROTOCOL_MAX, PROTOCOL_VERSION,
};
use crate::worker::{Worker, PREAMBLE};
use crate::RemoteError;

/// How long a new connection may take to reach the worker's hello: ssh
/// resolves, authenticates and starts a shell first.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
// Kept under the desktop app's 8 s drain deadline (lifecycle.rs): a stalled
// worker must fail a call in time. It is idle time per frame; a long job
// sends a heartbeat every second.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes a login shell may print before the preamble.
const MAX_BANNER: usize = 64 * 1024;
/// Bytes of the transport's stderr kept for an error message.
const MAX_STDERR: usize = 8 * 1024;
/// Idle connections kept per worker: each is an ssh process.
const POOL: usize = 2;
/// Frames buffered between the worker's stdout and the caller. Bounded so
/// a worker cannot make the master allocate without end while its
/// connection sits idle in the pool; at [`MAX_FRAME`] a piece, retained
/// bytes are bounded by [`FRAME_QUEUE`] × 8 MiB.
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

/// One frame in either direction, with a read deadline.
pub(crate) trait FrameIo {
	fn recv(
		&mut self,
		timeout: Duration,
	) -> Result<Option<Response>, RemoteError>;
}

/// A plain stream has no deadline; tests feed one frames from memory.
impl<S: IoRead + IoWrite> FrameIo for S {
	fn recv(&mut self, _: Duration) -> Result<Option<Response>, RemoteError> {
		Ok(read_frame::<Response>(self)?)
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
}

impl Connection {
	/// Starts the worker and says hello.
	pub fn open(
		transport: &Transport,
		my_name: &str,
	) -> Result<Self, RemoteError> {
		let (writer, frames, child, stderr, retained) = match transport {
			Transport::Command(argv) => spawn(argv)?,
			Transport::InProcess(worker) => {
				let (req_w, res_r) = worker.connect_in_process()?;
				let writer: Box<dyn IoWrite + Send> = Box::new(req_w);
				let (frames, retained) = read_frames(res_r);
				(writer, frames, None, Arc::default(), retained)
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
		};
		let hello = Request::Hello {
			version: PROTOCOL_VERSION,
			name: my_name.to_string(),
			max_version: Some(PROTOCOL_MAX),
		};
		let reply = conn
			.send_guarded(&hello, None, Instant::now() + CONNECT_TIMEOUT)
			.and_then(|()| conn.recv(CONNECT_TIMEOUT));
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
			Ok(Some(Response::Error { code, message })) => {
				Err(RemoteError::Refused { code, message })
			}
			Ok(Some(_)) => Err(RemoteError::Protocol("expected hello".into())),
			Ok(None) | Err(RemoteError::Io(_)) | Err(RemoteError::TimedOut) => {
				Err(conn.start_error())
			}
			Err(err) => Err(err),
		}
	}

	/// Why the worker never said hello, from the transport's exit status
	/// and stderr: ssh's own message, or that the far end has no snip.
	fn start_error(&mut self) -> RemoteError {
		let status = self.child.as_mut().and_then(|c| {
			let deadline = Instant::now() + Duration::from_secs(2);
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
		let deadline = Instant::now() + Duration::from_secs(2);
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
		RemoteError::Connect(start_message(
			status.and_then(|s| s.code()),
			&stderr,
		))
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
		let res = self
			.send_guarded(request, cancel, deadline)
			.and_then(|()| exchange(self, cancel, deadline, &mut frames));
		self.frames_seen = frames;
		if res.is_err() && !matches!(res, Err(RemoteError::Refused { .. })) {
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
		let mut writer =
			std::mem::replace(&mut self.writer, Box::new(DeadWriter));
		let request = request.clone();
		let (done_tx, done_rx) = mpsc::channel();
		let _ = std::thread::Builder::new()
			.name("snip-remote-write".into())
			.spawn(move || {
				let result = write_request(&mut writer, &request);
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
}

impl Drop for Connection {
	fn drop(&mut self) {
		if let Some(mut child) = self.child.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

/// The user-facing reason a worker did not start.
pub(crate) fn start_message(code: Option<i32>, stderr: &str) -> String {
	if code == Some(127) || stderr.contains("unrecognized subcommand") {
		return "snip is not installed on that machine, or is too old for \
		        ssh connections; install snip-sync there"
			.into();
	}
	if stderr.contains("Permission denied") {
		return format!(
			"ssh could not log in without a password; set up key login \
			 (ssh-copy-id) first: {stderr}"
		);
	}
	if stderr.is_empty() {
		return "the remote end closed the connection before snip started"
			.into();
	}
	stderr.to_string()
}

/// A started worker: where to write requests, the frames it answers, the
/// process to stop, what it said on stderr, and its retained-frame counter.
type Started = (
	Box<dyn IoWrite + Send>,
	Frames,
	Option<Child>,
	Arc<Mutex<Vec<u8>>>,
	Arc<AtomicUsize>,
);

fn spawn(argv: &[OsString]) -> Result<Started, RemoteError> {
	let (program, args) = argv.split_first().ok_or_else(|| {
		RemoteError::Connect("no command to start the worker".into())
	})?;
	let mut child = Command::new(program)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|err| {
			RemoteError::Connect(format!(
				"cannot run {}: {err}",
				program.to_string_lossy()
			))
		})?;
	let stdin: ChildStdin = child.stdin.take().expect("piped stdin");
	let stdout = child.stdout.take().expect("piped stdout");
	let stderr_pipe = child.stderr.take().expect("piped stderr");
	let stderr = Arc::new(Mutex::new(Vec::new()));
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
	let pump_retained = Arc::clone(&retained);
	std::thread::Builder::new()
		.name("snip-remote-read".into())
		.spawn(move || {
			if let Err(err) = skip_banner(&mut stdout) {
				let _ = tx.send(Err(err));
				return;
			}
			pump_frames(stdout, tx, pump_retained);
		})?;
	let writer: Box<dyn IoWrite + Send> = Box::new(stdin);
	Ok((writer, rx, Some(child), stderr, retained))
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

fn read_frames(r: impl IoRead + Send + 'static) -> (Frames, Arc<AtomicUsize>) {
	let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
	let retained = Arc::new(AtomicUsize::new(0));
	let pump_retained = Arc::clone(&retained);
	std::thread::spawn(move || pump_frames(r, tx, pump_retained));
	(rx, retained)
}

/// Reads frames until the worker stops or the consumer does. The queue is
/// bounded and every queued frame is accounted in `retained`, so a worker
/// that keeps speaking while nobody listens cannot grow the master's
/// memory: at [`FRAME_QUEUE`] retained frames the pump hangs up, which the
/// caller sees as a worker that closed the connection.
fn pump_frames(
	mut r: impl IoRead,
	tx: SyncSender<io::Result<Option<Response>>>,
	retained: Arc<AtomicUsize>,
) {
	loop {
		if retained.load(Ordering::Relaxed) >= FRAME_QUEUE {
			return;
		}
		let frame = read_frame::<Response>(&mut r);
		let end = !matches!(frame, Ok(Some(_)));
		retained.fetch_add(1, Ordering::Relaxed);
		if tx.send(frame).is_err() {
			retained.fetch_sub(1, Ordering::Relaxed);
			return;
		}
		if end {
			return;
		}
	}
}

/// Waits for `request`'s answer, skipping heartbeats. Every frame — chunk
/// frames included — holds to the absolute `deadline`, so a worker cannot
/// stretch one call by dripping frames. The request itself was sent by
/// [`Connection::send_guarded`] under the same deadline.
pub(crate) fn exchange(
	io: &mut impl FrameIo,
	cancel: Option<&CancelToken>,
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
		match io.recv(IO_TIMEOUT)? {
			Some(response) => {
				*frames += 1;
				if Instant::now() >= deadline {
					return Err(RemoteError::TimedOut);
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
							Ok(Response::Error { code, message }) => {
								Err(RemoteError::Refused { code, message })
							}
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
					Response::Error { code, message } => {
						return Err(RemoteError::Refused { code, message });
					}
					response => return Ok(response),
				}
			}
			None => {
				return Err(RemoteError::Io(io::Error::new(
					io::ErrorKind::UnexpectedEof,
					"the worker closed the connection",
				)));
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

/// Calls to one worker, reusing a few idle connections.
pub struct Client {
	host: RemoteHost,
	my_name: String,
	idle: Mutex<Vec<Connection>>,
	limiter: GitLimiter,
}

impl Client {
	pub fn new(host: RemoteHost, my_name: String) -> Self {
		Self {
			host,
			my_name,
			idle: Mutex::new(Vec::new()),
			limiter: GitLimiter::new(),
		}
	}

	pub fn host(&self) -> &RemoteHost {
		&self.host
	}

	fn connect(&self) -> Result<Connection, RemoteError> {
		Connection::open(&self.host.transport, &self.my_name)
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
		let is_git = matches!(
			request,
			Request::ScanRepos { .. }
				| Request::GitView { .. }
				| Request::Export { .. }
				| Request::ExportCommits { .. }
				| Request::ImportPlan { .. }
				| Request::ImportApply { .. }
				| Request::ReplayPlan { .. }
				| Request::ReplayApply { .. }
		);
		// A write is never sent twice, even when the first send seemed lost.
		let writes = matches!(
			request,
			Request::ImportApply { .. }
				| Request::ReplayApply {
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
		let too_old = |conn: Connection| {
			let err = RemoteError::WorkerTooOld {
				worker: conn.worker_name().to_string(),
				have: conn.version(),
				need,
			};
			self.keep(conn);
			err
		};
		let pooled = self
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.pop();
		let (mut conn, reused) = match pooled {
			Some(conn) if conn.unsolicited_frames() > 0 => {
				// The worker spoke while nobody was asking. Whatever the
				// reason, its stream is out of step with the protocol:
				// drop the connection (killing its process) and start a
				// clean one.
				(self.connect()?, false)
			}
			Some(conn) if conn.version() >= need => (conn, true),
			Some(conn) => return Err(too_old(conn)),
			None => {
				let fresh = self.connect()?;
				if fresh.version() < need {
					return Err(too_old(fresh));
				}
				(fresh, false)
			}
		};

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
				if let Ok(mut fresh) = self.connect() {
					if fresh.version() < need {
						return Err(too_old(fresh));
					}
					result = fresh.call(request, cancel, remaining_retry);
					conn = fresh;
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
		match self.call(&Request::OpenWorkspace { path: path.into() })? {
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

	pub fn list_dir(
		&self,
		workspace: &str,
		path: &str,
	) -> Result<(Vec<DirEntry>, bool), RemoteError> {
		match self.call(&Request::ListDir {
			workspace: workspace.into(),
			path: path.into(),
		})? {
			Response::Dir { entries, truncated } => Ok((entries, truncated)),
			_ => Err(unexpected()),
		}
	}

	pub fn stat(
		&self,
		workspace: &str,
		path: &str,
	) -> Result<Stat, RemoteError> {
		match self.call(&Request::Stat {
			workspace: workspace.into(),
			path: path.into(),
		})? {
			Response::Stat(stat) => Ok(stat),
			_ => Err(unexpected()),
		}
	}

	/// `None` for a binary or non-UTF-8 file.
	pub fn read(
		&self,
		workspace: &str,
		path: &str,
	) -> Result<Option<String>, RemoteError> {
		match self.call(&Request::Read {
			workspace: workspace.into(),
			path: path.into(),
		})? {
			Response::Text { content } => Ok(content),
			_ => Err(unexpected()),
		}
	}
}

fn unexpected() -> RemoteError {
	RemoteError::Protocol("unexpected reply".into())
}

fn unexpected_reply() -> GitError {
	GitError::Host("unexpected reply from the worker".into())
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

impl From<RemoteError> for GitError {
	fn from(err: RemoteError) -> Self {
		match err {
			RemoteError::Refused {
				code: ErrorCode::InvalidRevision,
				message,
			} => GitError::InvalidRevision(message),
			RemoteError::Refused {
				code: ErrorCode::TooLarge,
				..
			} => GitError::OutputLimit {
				args: "remote view".into(),
				limit: 0,
			},
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
			}
			| RemoteError::TimedOut => GitError::Timeout {
				args: "remote view".into(),
				secs: GIT_CALL_LIMIT.as_secs(),
			},
			other => GitError::Host(other.to_string()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::proto::write_frame;

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
		let err = exchange(&mut stream, None, Instant::now(), &mut frames)
			.unwrap_err();

		assert!(matches!(err, RemoteError::TimedOut));
		assert_eq!(frames, 1);
	}

	#[test]
	fn exchange_error_frame_returns_refused() {
		let mut stream = FakeDuplex::new(&[Response::Error {
			code: ErrorCode::NotFound,
			message: "missing".into(),
		}]);
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			None,
			Instant::now() + Duration::from_secs(10),
			&mut frames,
		)
		.unwrap_err();

		match err {
			RemoteError::Refused { code, message } => {
				assert_eq!(code, ErrorCode::NotFound);
				assert_eq!(message, "missing");
			}
			other => panic!("expected Refused, got {other:?}"),
		}
		assert_eq!(frames, 1);
	}

	#[test]
	fn exchange_eof_returns_unexpected_eof() {
		let mut stream = FakeDuplex::new(&[]);
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			None,
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

	#[test]
	fn git_error_conversion_table() {
		let err1 = RemoteError::Refused {
			code: ErrorCode::InvalidRevision,
			message: "bad rev".into(),
		};
		assert!(matches!(
			GitError::from(err1),
			GitError::InvalidRevision(ref msg) if msg == "bad rev"
		));

		let err2 = RemoteError::Refused {
			code: ErrorCode::Cancelled,
			message: "stop".into(),
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
		};
		assert!(matches!(
			GitError::from(err4),
			GitError::Timeout { ref args, secs }
				if args == "remote view" && secs == GIT_CALL_LIMIT.as_secs()
		));

		let err5 = RemoteError::TimedOut;
		assert!(matches!(
			GitError::from(err5),
			GitError::Timeout { ref args, secs }
				if args == "remote view" && secs == GIT_CALL_LIMIT.as_secs()
		));

		let err6 = RemoteError::WorkerTooOld {
			worker: "w".into(),
			have: 1,
			need: 2,
		};
		assert!(matches!(GitError::from(err6), GitError::Host(_)));

		let err7 = RemoteError::Io(io::Error::other("disk error"));
		assert!(matches!(GitError::from(err7), GitError::Host(_)));

		let err8 = RemoteError::Refused {
			code: ErrorCode::NotFound,
			message: "missing".into(),
		};
		assert!(matches!(GitError::from(err8), GitError::Host(_)));

		let err_too_large = RemoteError::Refused {
			code: ErrorCode::TooLarge,
			message: "refs payload exceeded frame limit".into(),
		};
		assert!(matches!(
			GitError::from(err_too_large),
			GitError::OutputLimit { ref args, limit }
				if args == "remote view" && limit == 0
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
	/// master buffer without end: the pump hangs up once the retained
	/// frames reach the cap, and the ssh writer is left with a closed pipe.
	#[test]
	fn an_idle_flood_of_frames_is_bounded_and_ends_the_read() {
		let (r, mut w) = io::pipe().unwrap();
		let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE);
		let retained = Arc::new(AtomicUsize::new(0));
		let pump_retained = Arc::clone(&retained);
		let pump = std::thread::spawn(move || {
			pump_frames(r, tx, pump_retained);
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

		// The pump hangs up at the cap; the producer then hits a closed
		// pipe well before 10,000 frames.
		let producer = producer.join().unwrap();
		pump.join().unwrap();
		assert_eq!(
			retained.load(std::sync::atomic::Ordering::Relaxed),
			FRAME_QUEUE
		);
		assert!(
			producer < 200,
			"an unconsumed pump must stop at the cap, wrote {producer}"
		);
		drop(rx);
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
		let mut conn = Connection {
			writer: Box::new(w),
			frames: rx,
			child: None,
			stderr: Arc::default(),
			worker_name: String::new(),
			home: None,
			version: 1,
			broken: false,
			frames_seen: 0,
			retained: Arc::clone(&retained),
		};
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
		Connection {
			writer: Box::new(ParkedWriter(std::sync::Mutex::new(Some(gate)))),
			frames: mpsc::sync_channel(1).1,
			child: None,
			stderr: Arc::default(),
			worker_name: String::new(),
			home: None,
			version: 1,
			broken: false,
			frames_seen: 0,
			retained: Arc::default(),
		}
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
		let mut conn = Connection {
			writer: Box::new(w),
			frames: mpsc::sync_channel(1).1,
			child: None,
			stderr: Arc::default(),
			worker_name: String::new(),
			home: None,
			version: 1,
			broken: false,
			frames_seen: 0,
			retained: Arc::default(),
		};
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
			exchange(&mut stream, None, deadline, &mut frames).unwrap_err();
		assert!(matches!(err, RemoteError::TimedOut), "{err:?}");
	}
}
