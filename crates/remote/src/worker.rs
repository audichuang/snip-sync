//! The worker: the far end of a master's connection. A master starts it
//! over ssh (`snip serve --stdio`) and it answers that one master on stdin
//! and stdout until the stream ends. SSH has already authenticated the
//! user, so anything that user can read on this machine can be opened as a
//! workspace; requests about a workspace stay inside it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use snip_core::reason::{io_reason, Reason};
use snip_core::workspace::{DirectoryScan, ScanError, ScanStatus};

use crate::fault::Fault;
use crate::proto::{
	read_frame, write_frame, DirEntry, EntryKind, ErrorCode, RemoteWorkspace,
	Request, Response, Stat, MAX_DIR_ENTRIES, PROTOCOL_MAX, PROTOCOL_VERSION,
};
use crate::RemoteError;

/// First line `snip serve --stdio` prints, before any frame: a login
/// shell's banner ahead of it is skipped by the master.
pub const PREAMBLE: &str = "snip-serve-stdio/1";

/// A workspace this worker serves: a folder resolved by real path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoot {
	/// The real path as text: what requests name the workspace by.
	pub id: String,
	pub name: String,
	/// Real path; every request is resolved and contained against it.
	pub path: PathBuf,
}

impl SharedRoot {
	pub fn new(path: &Path) -> io::Result<Self> {
		let path = dunce::canonicalize(path)?;
		if !path.is_dir() {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				"a workspace must be a folder",
			));
		}
		let id = path.display().to_string();
		let name = path
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| id.clone());
		Ok(Self { id, name, path })
	}

	/// `spelled` as a master sends it: an absolute path, or `~` / `~/…`
	/// for this user's home.
	pub fn open(spelled: &str) -> io::Result<Self> {
		Self::new(&expand_home(spelled)?)
	}
}

/// This user's home folder.
pub fn home_dir() -> Option<PathBuf> {
	std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
		.filter(|h| !h.is_empty())
		.map(PathBuf::from)
}

fn expand_home(spelled: &str) -> io::Result<PathBuf> {
	let rest = if spelled == "~" {
		Some("")
	} else {
		spelled
			.strip_prefix("~/")
			.or_else(|| spelled.strip_prefix("~\\"))
	};
	let path = match rest {
		Some(rest) => home_dir()
			.ok_or_else(|| {
				io::Error::new(io::ErrorKind::NotFound, "no home folder")
			})?
			.join(rest),
		None => PathBuf::from(spelled),
	};
	if !path.is_absolute() {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"a workspace path must be absolute or start with ~",
		));
	}
	Ok(path)
}

#[derive(Debug, Clone, Default)]
pub struct WorkerOptions {
	/// Shown to masters.
	pub name: String,
	/// Highest protocol version this worker will negotiate.
	pub max_protocol: Option<u32>,
}

struct State {
	name: String,
	max_protocol: Option<u32>,
	git_requests: AtomicUsize,
	deadlines: Mutex<(Duration, Duration)>,
	/// A ListDir reply's entry cap; [`MAX_DIR_ENTRIES`] unless a test
	/// shrank it.
	dir_cap: AtomicUsize,
	jobs: crate::jobs::Jobs,
	repo_cache: Mutex<crate::gitserve::RepoCache>,
	prepared: crate::prepared::PreparedCache,
	stop: AtomicBool,
	connections: AtomicUsize,
	open_sequence: AtomicUsize,
	hello_hold: (Mutex<bool>, Condvar),
	active_hellos: AtomicUsize,
	max_hellos: AtomicUsize,
	/// Faults a test injected ([`crate::fault`]); only read while `armed`.
	faults: Mutex<crate::fault::FaultBook>,
	armed: AtomicBool,
}

/// One worker's state. `snip serve --stdio` serves a single connection;
/// tests serve several against one worker.
pub struct Worker {
	state: Arc<State>,
}

impl Worker {
	pub fn new(opts: WorkerOptions) -> Self {
		Self {
			state: Arc::new(State {
				name: opts.name,
				prepared: crate::prepared::PreparedCache::new(),
				max_protocol: opts.max_protocol,
				git_requests: AtomicUsize::new(0),
				deadlines: Mutex::new((
					crate::jobs::VIEW_DEADLINE,
					crate::jobs::SCAN_DEADLINE,
				)),
				dir_cap: AtomicUsize::new(MAX_DIR_ENTRIES),
				jobs: crate::jobs::Jobs::new(),
				repo_cache: Mutex::new(crate::gitserve::RepoCache::new()),
				stop: AtomicBool::new(false),
				connections: AtomicUsize::new(0),
				open_sequence: AtomicUsize::new(0),
				hello_hold: (Mutex::new(false), Condvar::new()),
				active_hellos: AtomicUsize::new(0),
				max_hellos: AtomicUsize::new(0),
				faults: Mutex::default(),
				armed: AtomicBool::new(false),
			}),
		}
	}

	/// Answers one master on `reader` / `writer` until the stream ends.
	pub fn serve(
		&self,
		reader: impl Read,
		writer: impl Write,
	) -> Result<(), RemoteError> {
		self.state.connections.fetch_add(1, Ordering::SeqCst);
		serve(reader, writer, &self.state)
	}

	/// A connection to this worker on a thread of this process: write
	/// requests to the first end, read responses from the second. Dropping
	/// either end is a master hanging up. For tests.
	#[doc(hidden)]
	pub fn connect_in_process(
		self: &Arc<Self>,
	) -> io::Result<(io::PipeWriter, io::PipeReader)> {
		let (req_r, req_w) = io::pipe()?;
		let (res_r, res_w) = io::pipe()?;
		let worker = self.clone();
		std::thread::Builder::new()
			.name("snip-remote-inproc".into())
			.spawn(move || {
				let _ = worker.serve(req_r, res_w);
			})?;
		Ok((req_w, res_r))
	}

	/// Counts GitView/ScanRepos requests received, for tests and diagnostics.
	pub fn git_requests_seen(&self) -> usize {
		self.state.git_requests.load(Ordering::SeqCst)
	}

	#[doc(hidden)]
	pub fn set_prepared_cache_limits_for_tests(
		&self,
		bytes: usize,
		entries: usize,
		ttl: Duration,
	) {
		self.state
			.prepared
			.set_limits_for_tests(bytes, entries, ttl);
	}
	#[doc(hidden)]
	pub fn expire_prepared_for_tests(&self) {
		self.state.prepared.expire_for_tests();
	}
	#[doc(hidden)]
	pub fn prepared_cache_usage_for_tests(&self) -> (usize, usize) {
		self.state.prepared.usage_for_tests()
	}

	/// Arms a fault for the requests `rule` names ([`crate::fault`]). For
	/// tests: nothing else arms one.
	#[doc(hidden)]
	pub fn inject_fault_for_tests(&self, rule: crate::fault::FaultRule) {
		lock(&self.state.faults).add(rule);
		self.state.armed.store(true, Ordering::SeqCst);
	}

	/// Drops every injected fault and disarms the book. The scan failures of
	/// [`Self::set_scan_failures_for_tests`] are rules of the same book: they
	/// go too, and [`Self::scan_requests_for_tests`] stops counting until a
	/// fault is armed again. Call it before setting them, not after.
	#[doc(hidden)]
	pub fn clear_faults_for_tests(&self) {
		lock(&self.state.faults).clear();
		self.state.armed.store(false, Ordering::SeqCst);
	}

	/// Requests of `kind` the worker has seen while a fault was armed
	/// (counting starts at the first [`Self::inject_fault_for_tests`]). A
	/// hello is counted too.
	#[doc(hidden)]
	pub fn requests_seen_for_tests(
		&self,
		kind: crate::fault::RequestKind,
	) -> usize {
		lock(&self.state.faults).seen(kind)
	}

	#[doc(hidden)]
	pub fn set_deadlines_for_tests(&self, view: Duration, scan: Duration) {
		*lock(&self.state.deadlines) = (view, scan);
	}

	/// Inject only scan failures, after a normal hello/open/list: the first
	/// `eofs` scans of `workspace` are cut off (the stream closes), and every
	/// scan after them is refused with `refusal`, when there is one. A
	/// wrapper over [`crate::fault::FaultRule`]s; it starts the workspace's
	/// scan count and its open record over, and arms the book (so counting
	/// works) until [`Self::clear_faults_for_tests`] drops these rules too.
	#[doc(hidden)]
	pub fn set_scan_failures_for_tests(
		&self,
		workspace: &str,
		eofs: usize,
		refusal: Option<ErrorCode>,
	) {
		use crate::fault::{Fault, FaultRule, Match, RequestKind};
		let mut book = lock(&self.state.faults);
		book.remove_rules(RequestKind::ScanRepos, workspace);
		book.reset_seen_in(RequestKind::ScanRepos, workspace);
		book.watch(workspace);
		// `skip` stays 0 on both: a rule that still has turns decides, and
		// the later rule only sees the requests the earlier one let by.
		if eofs > 0 {
			book.add(FaultRule {
				on: Match::Kind(RequestKind::ScanRepos),
				workspace: Some(workspace.into()),
				skip: 0,
				times: eofs,
				fault: Fault::HangUp,
			});
		}
		if let Some(code) = refusal {
			book.add(FaultRule {
				on: Match::Kind(RequestKind::ScanRepos),
				workspace: Some(workspace.into()),
				skip: 0,
				times: usize::MAX,
				fault: Fault::Refuse(code),
			});
		}
		drop(book);
		self.state.armed.store(true, Ordering::SeqCst);
	}

	/// Scans of `workspace` the worker has seen since
	/// [`Self::set_scan_failures_for_tests`] last ran for it.
	#[doc(hidden)]
	pub fn scan_requests_for_tests(&self, workspace: &str) -> usize {
		lock(&self.state.faults)
			.seen_in(crate::fault::RequestKind::ScanRepos, workspace)
	}

	#[doc(hidden)]
	pub fn started_connections_for_tests(&self) -> usize {
		self.state.connections.load(Ordering::SeqCst)
	}

	/// First successful opens, only for explicitly fault-configured test
	/// workspaces. Storage stays bounded by those configured entries.
	#[doc(hidden)]
	pub fn opened_workspaces_for_tests(&self) -> Vec<String> {
		lock(&self.state.faults).opened()
	}

	/// Holds real hello responses for bounded handshake-admission tests.
	#[doc(hidden)]
	pub fn hold_hellos_for_tests(&self, hold: bool) {
		*lock(&self.state.hello_hold.0) = hold;
		self.state.hello_hold.1.notify_all();
	}

	#[doc(hidden)]
	pub fn active_hellos_for_tests(&self) -> usize {
		self.state.active_hellos.load(Ordering::SeqCst)
	}

	#[doc(hidden)]
	pub fn max_hellos_for_tests(&self) -> usize {
		self.state.max_hellos.load(Ordering::SeqCst)
	}

	/// Shrinks a ListDir reply's entry cap, so the truncation path is
	/// testable without a 20,001-entry folder on a slow machine.
	#[doc(hidden)]
	pub fn set_dir_cap_for_tests(&self, cap: usize) {
		self.state.dir_cap.store(cap.max(1), Ordering::SeqCst);
	}

	#[doc(hidden)]
	pub fn running_jobs(&self) -> usize {
		self.state.jobs.running()
	}

	#[doc(hidden)]
	pub fn jobs_waiting(&self) -> usize {
		self.state.jobs.waiting()
	}

	/// Cancels every job; each connection ends at its next request.
	pub fn stop(&self) {
		self.state.stop.store(true, Ordering::SeqCst);
		self.hold_hellos_for_tests(false);
		self.state.jobs.cancel_all();
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		self.stop();
	}
}

/// `snip serve --stdio`: prints [`PREAMBLE`], then serves the master on
/// stdin / stdout until it hangs up.
pub fn serve_stdio(opts: WorkerOptions) -> Result<(), RemoteError> {
	let stdin = io::stdin();
	let stdout = io::stdout();
	let mut out = stdout.lock();
	writeln!(out, "{PREAMBLE}")?;
	out.flush()?;
	Worker::new(opts).serve(stdin.lock(), out)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn serve(
	mut reader: impl Read,
	mut writer: impl Write,
	state: &State,
) -> Result<(), RemoteError> {
	// A stopped worker hangs up on every master, as a dead process would.
	if state.stop.load(Ordering::SeqCst) {
		return Ok(());
	}
	let Some(hello) = read_frame::<Request>(&mut reader)? else {
		return Ok(());
	};
	// A hello meets the faults named for it: how a start fails.
	let hello_fault = if state.armed.load(Ordering::SeqCst) {
		lock(&state.faults).take(&hello)
	} else {
		None
	};
	match hello_fault {
		Some(Fault::HangUp) => return Ok(()),
		Some(Fault::Garbage) => {
			writer.write_all(&8u32.to_be_bytes())?;
			writer.write_all(b"not json")?;
			writer.flush()?;
			return Ok(());
		}
		Some(Fault::Refuse(code)) => {
			write_frame(&mut writer, &error(code, "injected fault".into()))?;
			return Ok(());
		}
		_ => {}
	}
	let negotiated = match hello {
		Request::Hello {
			version,
			max_version,
			..
		} if version == PROTOCOL_VERSION => {
			let worker_max = state.max_protocol.unwrap_or(PROTOCOL_MAX).max(1);
			let master_max = max_version.unwrap_or(1);
			master_max.min(worker_max)
		}
		Request::Hello { version, .. } => {
			write_frame(
				&mut writer,
				&error(
					ErrorCode::VersionMismatch,
					format!(
						"worker speaks protocol {PROTOCOL_VERSION}, master {version}"
					),
				),
			)?;
			return Ok(());
		}
		_ => {
			write_frame(
				&mut writer,
				&error(ErrorCode::BadRequest, "expected hello".into()),
			)?;
			return Ok(());
		}
	};
	let active = state.active_hellos.fetch_add(1, Ordering::SeqCst) + 1;
	state.max_hellos.fetch_max(active, Ordering::SeqCst);
	let held = || -> Result<(), RemoteError> {
		let end = Instant::now() + Duration::from_secs(15);
		let mut hold = lock(&state.hello_hold.0);
		while *hold {
			let remaining = end.saturating_duration_since(Instant::now());
			if remaining.is_zero() {
				return Err(RemoteError::TimedOut);
			}
			hold = state
				.hello_hold
				.1
				.wait_timeout(hold, remaining)
				.unwrap_or_else(PoisonError::into_inner)
				.0;
		}
		Ok(())
	};
	let held = held();
	state.active_hellos.fetch_sub(1, Ordering::SeqCst);
	held?;
	write_frame(
		&mut writer,
		&Response::Hello {
			version: PROTOCOL_VERSION,
			name: state.name.clone(),
			home: home_dir().map(|h| h.display().to_string()),
			max_version: Some(negotiated),
		},
	)?;
	// A paste's text arriving ahead of its request.
	let mut pending = String::new();
	let mut overflow = false;
	// A request too large for one frame, arriving as JSON pieces.
	let mut frame_json = String::new();
	let mut frame_overflow = false;
	while !state.stop.load(Ordering::SeqCst) {
		let Some(mut request) = read_frame::<Request>(&mut reader)? else {
			return Ok(());
		};
		if state.stop.load(Ordering::SeqCst) {
			return Ok(());
		}
		if let Request::Chunk { data } = request {
			if overflow
				|| pending.len() + data.len()
					> snip_core::transfer::CLIPBOARD_PAYLOAD_MAX
			{
				overflow = true;
				pending = String::new();
			} else {
				pending.push_str(&data);
			}
			continue;
		}
		if let Request::FrameChunk { data } = request {
			if frame_overflow
				|| frame_json.len() + data.len() > crate::proto::JOINED_MAX
			{
				frame_overflow = true;
				frame_json = String::new();
			} else {
				frame_json.push_str(&data);
			}
			continue;
		}
		if let Request::FrameJoin = request {
			if frame_overflow {
				crate::jobs::write_response(
					&mut writer,
					&error(
						ErrorCode::TooLarge,
						"the request is over the join limit".into(),
					),
				)?;
				continue;
			}
			let joined = std::mem::take(&mut frame_json);
			match serde_json::from_str::<Request>(&joined) {
				Ok(parsed) => request = parsed,
				Err(_) => {
					crate::jobs::write_response(
						&mut writer,
						&error(
							ErrorCode::BadRequest,
							"the joined request is not valid".into(),
						),
					)?;
					continue;
				}
			}
		} else if !frame_json.is_empty() {
			crate::jobs::write_response(
				&mut writer,
				&error(
					ErrorCode::BadRequest,
					"frame chunks must end with a join".into(),
				),
			)?;
			continue;
		}
		let joined = std::mem::take(&mut pending);
		if std::mem::replace(&mut overflow, false) {
			crate::jobs::write_response(
				&mut writer,
				&error(
					ErrorCode::TooLarge,
					"the pasted text is over the clipboard limit".into(),
				),
			)?;
			continue;
		}
		if !joined.is_empty() {
			match request.text_mut() {
				Some(text) if text.is_empty() => *text = joined,
				_ => {
					crate::jobs::write_response(
						&mut writer,
						&error(
							ErrorCode::BadRequest,
							"text chunks must precede a paste request".into(),
						),
					)?;
					continue;
				}
			}
		}
		let fault = if state.armed.load(Ordering::SeqCst) {
			lock(&state.faults).take(&request)
		} else {
			None
		};
		// Counted before a fault can end the request: a scan that was cut
		// off still reached the worker.
		if matches!(
			request,
			Request::ScanRepos { .. } | Request::GitView { .. }
		) {
			state.git_requests.fetch_add(1, Ordering::SeqCst);
		}
		// Dispatch-layer faults act before any work; the reply-layer ones
		// ride on the `Answer`.
		match &fault {
			Some(Fault::Refuse(code)) => {
				crate::jobs::Answer::new(&mut writer, None)
					.reply(&error(*code, "injected fault".into()))?;
				continue;
			}
			Some(Fault::HangUp) => return Ok(()),
			Some(Fault::Garbage) => {
				writer.write_all(&8u32.to_be_bytes())?;
				writer.write_all(b"not json")?;
				writer.flush()?;
				return Ok(());
			}
			Some(Fault::HeartbeatThenHangUp) => {
				crate::jobs::Answer::new(&mut writer, None).heartbeat()?;
				return Ok(());
			}
			Some(Fault::Stall(d)) => std::thread::sleep(*d),
			Some(
				Fault::Delay(_)
				| Fault::PageDelay(_)
				| Fault::RunThenHangUp
				| Fault::TruncatedReply
				| Fault::Throttle { .. },
			)
			| None => {}
		}
		let mut answer = crate::jobs::Answer::new(&mut writer, fault);
		match request {
			Request::ScanRepos { workspace, under } => {
				if negotiated < crate::proto::GIT_VIEWS_VERSION {
					answer.reply(&error(
						ErrorCode::Unsupported,
						"Git views are not available on this worker yet".into(),
					))?;
				} else {
					let (_, scan_deadline) = *lock(&state.deadlines);
					let cancel = snip_core::gitrun::CancelToken::new();
					crate::jobs::run_job(
						&mut answer,
						scan_deadline,
						cancel,
						|job_cancel, job_deadline| match state.jobs.admit(
							&workspace,
							crate::jobs::JobKind::Scan,
							job_cancel,
						) {
							Ok(_guard) => {
								let initial_root =
									match state.get_shared_root(&workspace) {
										Ok(r) => r,
										Err(resp) => return resp,
									};
								let reply = crate::gitserve::scan(
									&initial_root,
									under.as_deref(),
									job_cancel,
									job_deadline,
									scan_deadline,
								);
								verify_root_unchanged(
									state,
									&workspace,
									&initial_root,
									reply,
								)
							}
							Err(code) => map_admit_error(code),
						},
					)?;
				}
			}
			Request::GitView {
				workspace,
				repo,
				profile,
				query,
			} => {
				if negotiated < crate::proto::GIT_VIEWS_VERSION {
					answer.reply(&error(
						ErrorCode::Unsupported,
						"Git views are not available on this worker yet".into(),
					))?;
				} else if let Err(msg) =
					crate::gitserve::validate(&repo, &query)
				{
					answer.reply(&error(ErrorCode::BadRequest, msg))?;
				} else {
					let (view_deadline, _) = *lock(&state.deadlines);
					let cancel = snip_core::gitrun::CancelToken::new();
					crate::jobs::run_job(
						&mut answer,
						view_deadline,
						cancel,
						|job_cancel, _job_deadline| match state.jobs.admit(
							&workspace,
							crate::jobs::JobKind::View,
							job_cancel,
						) {
							Ok(_guard) => {
								let initial_root =
									match state.get_shared_root(&workspace) {
										Ok(r) => r,
										Err(resp) => return resp,
									};
								let read = snip_core::gitview::Read {
									profile,
									cancel: Some(job_cancel.clone()),
								};
								let reply = crate::gitserve::handle_git_view(
									&initial_root,
									&repo,
									query,
									&read,
									&state.repo_cache,
								);
								verify_root_unchanged(
									state,
									&workspace,
									&initial_root,
									reply,
								)
							}
							Err(code) => map_admit_error(code),
						},
					)?;
				}
			}
			Request::Export {
				workspace,
				items,
				settings,
				file_limit,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::Copy,
					crate::proto::TRANSFER_VERSION,
					|root, cancel| {
						crate::copyserve::export(
							root, items, &settings, file_limit, cancel,
						)
					},
				)?;
			}
			Request::ExportCommits {
				workspace,
				repo,
				tip,
				selected,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::Copy,
					crate::proto::TRANSFER_VERSION,
					|root, cancel| {
						crate::copyserve::export_commits(
							root, &repo, &tip, &selected, cancel,
						)
					},
				)?;
			}
			Request::ExportChanges {
				workspace,
				repo,
				source,
				settings,
				file_limit,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::Copy,
					crate::proto::EXPORT_CHANGES_VERSION,
					|root, cancel| {
						crate::copyserve::export_changes(
							root, &repo, &source, &settings, file_limit, cancel,
						)
					},
				)?;
			}
			Request::PrepareImport {
				workspace,
				dest,
				text,
				mapping,
				retain,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PastePlan,
					crate::proto::PREPARED_IMPORT_VERSION,
					|root, cancel| {
						crate::pasteserve::prepare_import(
							root,
							&dest,
							text,
							mapping,
							retain,
							negotiated,
							&state.prepared,
							cancel,
						)
					},
				)?;
			}
			Request::ApplyPrepared {
				workspace,
				dest,
				token,
				selection,
				expect,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PasteApply,
					crate::proto::PREPARED_IMPORT_VERSION,
					|root, cancel| {
						crate::pasteserve::apply_prepared(
							root,
							&dest,
							&token,
							&selection,
							&expect,
							negotiated,
							&state.prepared,
							cancel,
						)
					},
				)?;
			}
			Request::ImportPlan {
				workspace,
				dest,
				text,
				mapping,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PastePlan,
					crate::proto::PASTE_VERSION,
					|root, cancel| {
						crate::pasteserve::import_plan(
							root, &dest, &text, &mapping, negotiated, cancel,
						)
					},
				)?;
			}
			Request::ImportApply {
				workspace,
				dest,
				text,
				mapping,
				selection,
				expect,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PasteApply,
					crate::proto::PASTE_VERSION,
					|root, cancel| {
						crate::pasteserve::import_apply(
							root, &dest, &text, &mapping, &selection, &expect,
							negotiated, cancel,
						)
					},
				)?;
			}
			Request::ReplayRead {
				workspace,
				path,
				limit,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PastePlan,
					crate::proto::REPLAY_PREVIEW_VERSION,
					|root, cancel| {
						crate::pasteserve::replay_read(
							root, &path, limit, cancel,
						)
					},
				)?;
			}
			Request::ReplayPlan {
				workspace,
				dest,
				text,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PastePlan,
					crate::proto::PASTE_VERSION,
					|root, cancel| {
						crate::pasteserve::replay_plan(
							root, &dest, &text, cancel,
						)
					},
				)?;
			}
			Request::PrepareReplay {
				workspace,
				dest,
				text,
				retain,
			} => {
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					Job::PastePlan,
					crate::proto::PREPARED_REPLAY_VERSION,
					|root, cancel| {
						crate::pasteserve::prepare_replay(
							root,
							&dest,
							text,
							retain,
							&state.prepared,
							cancel,
						)
					},
				)?;
			}
			Request::ApplyPreparedReplay {
				workspace,
				dest,
				token,
				digest,
				check_only,
			} => {
				let job = if check_only {
					Job::PastePlan
				} else {
					Job::PasteApply
				};
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					job,
					crate::proto::PREPARED_REPLAY_VERSION,
					|root, cancel| {
						crate::pasteserve::apply_prepared_replay(
							root,
							&dest,
							&token,
							&digest,
							check_only,
							&state.prepared,
							cancel,
						)
					},
				)?;
			}
			Request::ReplayApply {
				workspace,
				dest,
				text,
				expect,
				check_only,
			} => {
				let job = if check_only {
					Job::PastePlan
				} else {
					Job::PasteApply
				};
				serve_copy(
					&mut answer,
					state,
					negotiated,
					&workspace,
					job,
					crate::proto::PASTE_VERSION,
					|root, cancel| {
						crate::pasteserve::replay_apply(
							root, &dest, &text, expect, check_only, cancel,
						)
					},
				)?;
			}
			other => {
				let (view_deadline, _) = *lock(&state.deadlines);
				crate::jobs::run_inline(
					&mut answer,
					negotiated,
					view_deadline,
					|ctx| state.handle(other, ctx),
				)?;
			}
		}
		if answer.hang_up() {
			return Ok(());
		}
	}
	Ok(())
}

/// What a copy or paste job is, for its protocol and deadline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
	Copy,
	/// Reads only: a preview, or a re-check.
	PastePlan,
	/// Writes. Its deadline is long: a write cannot be cancelled midway, and
	/// a master must hear how it ended rather than a time-out.
	PasteApply,
}

/// Runs a copy or paste as a job of `workspace`, as Git views run. `need`
/// is the protocol the request itself requires: a request a worker this
/// old cannot even parse never reaches dispatch (the master gates on its
/// `needs_version`), so this is the semantic gate beside it.
fn serve_copy(
	answer: &mut crate::jobs::Answer<'_, impl Write>,
	state: &State,
	negotiated: u32,
	workspace: &str,
	job: Job,
	need: u32,
	op: impl FnOnce(&SharedRoot, &snip_core::gitrun::CancelToken) -> Response + Send,
) -> io::Result<()> {
	let what = match job {
		Job::Copy => "copy",
		Job::PastePlan | Job::PasteApply => "paste",
	};
	if negotiated < need {
		return answer.reply(&error(
			ErrorCode::Unsupported,
			format!("{what} is not available on this worker yet"),
		));
	}
	let (view_deadline, _) = *lock(&state.deadlines);
	let deadline = if job == Job::PasteApply {
		crate::jobs::APPLY_DEADLINE.max(view_deadline)
	} else {
		view_deadline
	};
	let cancel = snip_core::gitrun::CancelToken::new();
	// A write that runs past its deadline still answers with its own
	// response: a Timeout here would report a landed write as a failure.
	let reply = if job == Job::PasteApply {
		crate::jobs::DeadlineReply::WriteOutcome
	} else {
		crate::jobs::DeadlineReply::Fail
	};
	crate::jobs::run_job_with(
		answer,
		crate::jobs::HEARTBEAT,
		deadline,
		cancel,
		reply,
		|job_cancel, _| {
			match state.jobs.admit(
				workspace,
				crate::jobs::JobKind::View,
				job_cancel,
			) {
				Ok(_guard) => {
					let root = match state.get_shared_root(workspace) {
						Ok(r) => r,
						Err(resp) => return resp,
					};
					let reply = op(&root, job_cancel);
					if job == Job::PasteApply {
						// Whatever was written is reported as it is.
						reply
					} else {
						verify_root_unchanged(state, workspace, &root, reply)
					}
				}
				Err(code) => map_admit_error(code),
			}
		},
	)
}

fn error(code: ErrorCode, message: String) -> Response {
	error_with(code, message, None)
}

/// An error reply that also says why. `reason` must already have gone
/// through [`Reason::beside_code`]: it never carries a path or git's stderr.
pub(crate) fn error_with(
	code: ErrorCode,
	message: String,
	reason: Option<Reason>,
) -> Response {
	Response::Error {
		code,
		message,
		patch: false,
		reason,
		suggestion: None,
	}
}

fn map_admit_error(code: ErrorCode) -> Response {
	match code {
		ErrorCode::Busy => {
			if snip_core::gitrun::served_leaked() > 0 {
				if !crate::gitserve::SERVED_LEAK_WARNED
					.swap(true, std::sync::atomic::Ordering::Relaxed)
				{
					eprintln!(
						"[worker] Git permit leak detected; restart required"
					);
				}
				error_with(
					ErrorCode::Busy,
					"a Git process on the worker could not be cleaned up and the worker needs a restart".into(),
					Some(Reason::WorkerNeedsRestart),
				)
			} else {
				error(
					ErrorCode::Busy,
					"the worker is busy with other Git requests".into(),
				)
			}
		}
		ErrorCode::Cancelled => error(ErrorCode::Cancelled, "cancelled".into()),
		other => error(other, "the worker cannot admit this job".into()),
	}
}

fn verify_root_unchanged(
	state: &State,
	workspace: &str,
	initial_root: &SharedRoot,
	reply: Response,
) -> Response {
	match state.get_shared_root(workspace) {
		Ok(cur)
			if cur.id == initial_root.id && cur.path == initial_root.path =>
		{
			reply
		}
		_ => error(
			ErrorCode::Forbidden,
			"the workspace folder changed while it was read".into(),
		),
	}
}

pub(crate) fn io_error(err: io::Error) -> Response {
	let code = match err.kind() {
		io::ErrorKind::NotFound => ErrorCode::NotFound,
		io::ErrorKind::PermissionDenied => ErrorCode::Forbidden,
		io::ErrorKind::InvalidInput => ErrorCode::Forbidden,
		io::ErrorKind::FileTooLarge => ErrorCode::TooLarge,
		_ => ErrorCode::Io,
	};
	// The code says it all for most kinds, so the reason is usually `None`;
	// going through `beside_code` keeps a new kind from skipping the scrub.
	error_with(code, err.to_string(), io_reason(&err, "").beside_code())
}

impl State {
	#[allow(clippy::result_large_err)]
	fn handle(&self, request: Request, ctx: &crate::jobs::JobCtx) -> Response {
		match request {
			Request::Hello { .. } => {
				error(ErrorCode::BadRequest, "hello was already sent".into())
			}
			Request::OpenWorkspace { path } => {
				if let Err(stop) = ctx.unit() {
					return stop;
				}
				match SharedRoot::open(&path) {
					Ok(root) => {
						lock(&self.faults).note_open(&root.id, || {
							self.open_sequence.fetch_add(1, Ordering::SeqCst)
						});
						Response::Workspace(RemoteWorkspace {
							id: root.id,
							name: root.name,
							path: root.path.display().to_string(),
						})
					}
					Err(err) => io_error(err),
				}
			}
			Request::ListDir {
				workspace,
				path,
				folders_only,
			} => self
				.resolve(&workspace, &path)
				.and_then(|(root, dir)| {
					let cap = self.dir_cap.load(Ordering::SeqCst);
					list_dir(&root, &dir, cap, folders_only, ctx)
				})
				.unwrap_or_else(|e| e),
			Request::Stat { workspace, path } => {
				if let Err(stop) = ctx.unit() {
					return stop;
				}
				self.resolve(&workspace, &path)
					.and_then(|(_, p)| stat(&p))
					.unwrap_or_else(|e| e)
			}
			Request::Read { workspace, path } => {
				if let Err(stop) = ctx.unit() {
					return stop;
				}
				match self.root(&workspace) {
					Ok(root) => {
						match snip_core::browser::file_preview(&root, &path) {
							Ok(p) => Response::Text { content: p.content },
							Err(err) => io_error(err),
						}
					}
					Err(e) => e,
				}
			}
			Request::ScanRepos { .. } | Request::GitView { .. } => {
				self.git_requests.fetch_add(1, Ordering::SeqCst);
				error(
					ErrorCode::Unsupported,
					"Git views are not available on this worker yet".into(),
				)
			}
			Request::Export { .. }
			| Request::ExportCommits { .. }
			| Request::ExportChanges { .. }
			| Request::ImportPlan { .. }
			| Request::PrepareImport { .. }
			| Request::ApplyPrepared { .. }
			| Request::ImportApply { .. }
			| Request::ReplayRead { .. }
			| Request::ReplayPlan { .. }
			| Request::ReplayApply { .. }
			| Request::PrepareReplay { .. }
			| Request::ApplyPreparedReplay { .. }
			| Request::Chunk { .. }
			| Request::FrameChunk { .. }
			| Request::FrameJoin => error(
				ErrorCode::BadRequest,
				"copy and paste requests are served as jobs".into(),
			),
			Request::Write { .. } | Request::Rename { .. } => error(
				ErrorCode::Unsupported,
				"not available on this worker yet".into(),
			),
		}
	}

	#[allow(clippy::result_large_err)]
	pub(crate) fn get_shared_root(
		&self,
		workspace: &str,
	) -> Result<SharedRoot, Response> {
		SharedRoot::open(workspace).map_err(io_error)
	}

	#[allow(clippy::result_large_err)]
	fn root(&self, workspace: &str) -> Result<PathBuf, Response> {
		self.get_shared_root(workspace).map(|r| r.path)
	}

	#[allow(clippy::result_large_err)]
	fn resolve(
		&self,
		workspace: &str,
		path: &str,
	) -> Result<(PathBuf, PathBuf), Response> {
		let root = self.root(workspace)?;
		let resolved =
			snip_core::browser::inside(&root, path).map_err(io_error)?;
		Ok((root, resolved))
	}
}

/// Lists `dir` inside the shared `root` as ONE sorted reply (folders
/// first), at most `cap` entries; more than that is reported as
/// `truncated`, with no continuation to ask for — one reply is the whole
/// listing, so two replies can never miss or double a name between pages.
/// A symlink to a folder inside the share lists as a folder, as the copy
/// engine treats it; one that leads out of the share (or into `.git`)
/// stays a plain entry the master cannot open.
#[allow(clippy::result_large_err)]
fn list_dir(
	root: &Path,
	dir: &Path,
	cap: usize,
	folders_only: bool,
	ctx: &crate::jobs::JobCtx,
) -> Result<Response, Response> {
	let mut scan = DirectoryScan::open(dir).map_err(io_error)?;
	let mut entries = Vec::new();
	let mut truncated = false;
	'scan: loop {
		ctx.unit()?;
		let page = match scan.next_page(&ctx.budget(256)) {
			Ok(page) => page,
			Err(ScanError::Changed) => {
				return Err(error(
					ErrorCode::Io,
					"the folder changed while it was read; try again".into(),
				))
			}
			Err(ScanError::Io(err)) => return Err(io_error(err)),
		};
		// A page cut short by the job's end is no result: the listing must
		// not pass for the whole folder, and nothing is read after it.
		if let Some(stop) = crate::jobs::JobCtx::refusal_for(page.status) {
			return Err(stop);
		}
		for entry in page.entries {
			// A page holds up to 256 entries, each one a stat or two on a
			// slow disk: the job's end is looked at for every one.
			if let Some(stop) = ctx.stop() {
				return Err(stop);
			}
			if entries.len() >= cap {
				truncated = true;
				break 'scan;
			}
			// The filter runs before the push, so the cap above counts only
			// the entries a folder picker is shown. A hidden name is out
			// whatever it is, so it is not looked into.
			if folders_only && entry.name.to_string_lossy().starts_with('.') {
				continue;
			}
			let path = dir.join(&entry.name);
			let directory = entry.directory
				|| (entry.symlink
					&& snip_core::transfer::is_safe_dir_symlink(root, &path));
			if folders_only && !directory {
				continue;
			}
			let nested_repo =
				directory && fs::symlink_metadata(path.join(".git")).is_ok();
			entries.push(DirEntry {
				utf8: entry.utf8_name().is_some(),
				name: entry.name.to_string_lossy().into_owned(),
				directory,
				symlink: entry.symlink,
				nested_repo,
			});
		}
		match page.status {
			ScanStatus::Complete => break,
			ScanStatus::Incomplete | ScanStatus::LimitReached => {
				truncated = true;
				break;
			}
			// Ended the job: returned above. Should that ever change, a
			// half-read folder still says so rather than pass as whole.
			ScanStatus::Cancelled | ScanStatus::TimedOut => {
				truncated = true;
				break;
			}
			ScanStatus::More => {}
		}
	}
	entries.sort_by(|a, b| {
		b.directory
			.cmp(&a.directory)
			.then_with(|| a.name.cmp(&b.name))
	});
	Ok(Response::Dir { entries, truncated })
}

#[allow(clippy::result_large_err)]
fn stat(path: &Path) -> Result<Response, Response> {
	let meta = fs::metadata(path).map_err(io_error)?;
	let kind = if meta.is_dir() {
		EntryKind::Directory
	} else if meta.is_file() {
		EntryKind::File
	} else {
		EntryKind::Other
	};
	let modified = meta
		.modified()
		.ok()
		.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
		.map(|d| d.as_secs());
	Ok(Response::Stat(Stat {
		kind,
		size: meta.len(),
		modified,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The code comes from the error's kind, never from its message: a file
	/// over the preview limit is `TooLarge` whatever the text says.
	#[test]
	fn io_error_codes_come_from_the_kind_not_the_text() {
		let code = |kind, text: &str| {
			let Response::Error { code, reason, .. } =
				io_error(io::Error::new(kind, text))
			else {
				panic!("expected Error");
			};
			// Never the message: a reason holds no OS text or path.
			let wire = serde_json::to_string(&reason).unwrap();
			assert!(!wire.contains(text), "{kind:?}: {wire}");
			code
		};
		assert_eq!(
			code(io::ErrorKind::FileTooLarge, "unrelated words"),
			ErrorCode::TooLarge
		);
		// The old text guess is gone: these words on another kind are Io.
		assert_eq!(
			code(io::ErrorKind::Other, "Preview exceeds 1 MiB"),
			ErrorCode::Io
		);
		assert_eq!(
			code(io::ErrorKind::PermissionDenied, "Path leaves the workspace"),
			ErrorCode::Forbidden
		);
		assert_eq!(code(io::ErrorKind::NotFound, "x"), ErrorCode::NotFound);
	}

	#[test]
	fn shared_root_id_is_stable_through_a_symlinked_spelling() {
		let dir = tempfile::tempdir().unwrap();
		let real = dir.path().join("real");
		fs::create_dir(&real).unwrap();
		let a = SharedRoot::new(&real).unwrap();
		#[cfg(unix)]
		{
			let link = dir.path().join("link");
			std::os::unix::fs::symlink(&real, &link).unwrap();
			assert_eq!(SharedRoot::new(&link).unwrap(), a);
		}
		assert_eq!(a.name, "real");
		assert!(SharedRoot::new(&dir.path().join("missing")).is_err());
	}

	/// An op that is handed a cancel and a deadline must use them: a job
	/// that must stop gets its refusal back, and the work is not started.
	/// The folder has a file, so a listing that ignored the job would be
	/// seen answering with it.
	#[test]
	fn every_inline_op_refuses_a_stopped_job() {
		use crate::fault::RequestKind as K;
		use snip_core::gitrun::CancelToken;
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("a.txt"), "hello").unwrap();
		let root = dir.path().display().to_string();
		let worker = Worker::new(WorkerOptions::default());
		let state = &worker.state;
		let request = |kind: K| match kind {
			K::OpenWorkspace => Request::OpenWorkspace { path: root.clone() },
			K::ListDir => Request::ListDir {
				workspace: root.clone(),
				path: String::new(),
				folders_only: false,
			},
			K::Stat => Request::Stat {
				workspace: root.clone(),
				path: "a.txt".into(),
			},
			K::Read => Request::Read {
				workspace: root.clone(),
				path: "a.txt".into(),
			},
			other => panic!("{other:?} is not an inline op"),
		};
		let live = || crate::jobs::JobCtx::for_tests(CancelToken::new(), None);
		let cancelled = || {
			let cancel = CancelToken::new();
			cancel.cancel();
			crate::jobs::JobCtx::for_tests(cancel, None)
		};
		let expired = || {
			crate::jobs::JobCtx::for_tests(
				CancelToken::new(),
				Some(Instant::now()),
			)
		};
		let kinds: Vec<K> = crate::fault::ALL_REQUEST_KINDS
			.into_iter()
			.filter(|k| k.class() == crate::fault::Class::Inline)
			.collect();
		assert_eq!(kinds.len(), 4, "{kinds:?}");
		for kind in kinds {
			let answer = state.handle(request(kind), &live());
			assert!(
				!matches!(answer, Response::Error { .. }),
				"{kind:?} fails with a live job: {answer:?}"
			);
			for (ctx, want) in [
				(cancelled(), ErrorCode::Cancelled),
				(expired(), ErrorCode::Timeout),
			] {
				match state.handle(request(kind), &ctx) {
					Response::Error { code, .. } => {
						assert_eq!(code, want, "{kind:?}")
					}
					other => {
						panic!("{kind:?} worked for a stopped job: {other:?}")
					}
				}
			}
		}
	}

	#[test]
	fn a_workspace_is_absolute_or_under_home() {
		assert!(expand_home("relative/dir").is_err());
		if let Some(home) = home_dir() {
			assert_eq!(expand_home("~").unwrap(), home);
			assert_eq!(expand_home("~/a/b").unwrap(), home.join("a/b"));
		}
		let dir = tempfile::tempdir().unwrap();
		let spelled = dir.path().display().to_string();
		assert_eq!(
			SharedRoot::open(&spelled).unwrap().path,
			dunce::canonicalize(dir.path()).unwrap()
		);
	}
}
