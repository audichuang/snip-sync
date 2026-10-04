//! The worker: the far end of a master's connection. A master starts it
//! over ssh (`snip serve --stdio`) and it answers that one master on stdin
//! and stdout until the stream ends. SSH has already authenticated the
//! user, so anything that user can read on this machine can be opened as a
//! workspace; requests about a workspace stay inside it.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use snip_core::workspace::{DirectoryScan, ScanBudget, ScanError, ScanStatus};

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
	jobs: crate::jobs::Jobs,
	repo_cache: Mutex<crate::gitserve::RepoCache>,
	stop: AtomicBool,
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
				max_protocol: opts.max_protocol,
				git_requests: AtomicUsize::new(0),
				deadlines: Mutex::new((
					crate::jobs::VIEW_DEADLINE,
					crate::jobs::SCAN_DEADLINE,
				)),
				jobs: crate::jobs::Jobs::new(),
				repo_cache: Mutex::new(crate::gitserve::RepoCache::new()),
				stop: AtomicBool::new(false),
			}),
		}
	}

	/// Answers one master on `reader` / `writer` until the stream ends.
	pub fn serve(
		&self,
		reader: impl Read,
		writer: impl Write,
	) -> Result<(), RemoteError> {
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
	pub fn set_deadlines_for_tests(&self, view: Duration, scan: Duration) {
		*lock(&self.state.deadlines) = (view, scan);
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
		match request {
			Request::ScanRepos { workspace, under } => {
				state.git_requests.fetch_add(1, Ordering::SeqCst);
				if negotiated < crate::proto::GIT_VIEWS_VERSION {
					crate::jobs::write_response(
						&mut writer,
						&error(
							ErrorCode::Unsupported,
							"Git views are not available on this worker yet"
								.into(),
						),
					)?;
				} else {
					let (_, scan_deadline) = *lock(&state.deadlines);
					let cancel = snip_core::gitrun::CancelToken::new();
					crate::jobs::run_job(
						&mut writer,
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
				state.git_requests.fetch_add(1, Ordering::SeqCst);
				if negotiated < crate::proto::GIT_VIEWS_VERSION {
					crate::jobs::write_response(
						&mut writer,
						&error(
							ErrorCode::Unsupported,
							"Git views are not available on this worker yet"
								.into(),
						),
					)?;
				} else if let Err(msg) =
					crate::gitserve::validate(&repo, &query)
				{
					crate::jobs::write_response(
						&mut writer,
						&error(ErrorCode::BadRequest, msg),
					)?;
				} else {
					let (view_deadline, _) = *lock(&state.deadlines);
					let cancel = snip_core::gitrun::CancelToken::new();
					crate::jobs::run_job(
						&mut writer,
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
					&mut writer,
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
					&mut writer,
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
					&mut writer,
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
			Request::ImportPlan {
				workspace,
				dest,
				text,
				mapping,
			} => {
				serve_copy(
					&mut writer,
					state,
					negotiated,
					&workspace,
					Job::PastePlan,
					crate::proto::PASTE_VERSION,
					|root, cancel| {
						crate::pasteserve::import_plan(
							root, &dest, &text, &mapping, cancel,
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
					&mut writer,
					state,
					negotiated,
					&workspace,
					Job::PasteApply,
					crate::proto::PASTE_VERSION,
					|root, cancel| {
						crate::pasteserve::import_apply(
							root, &dest, &text, &mapping, &selection, &expect,
							cancel,
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
					&mut writer,
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
					&mut writer,
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
				let response = state.handle(other);
				crate::jobs::write_response(&mut writer, &response)?;
			}
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
	writer: &mut impl Write,
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
		return crate::jobs::write_response(
			writer,
			&error(
				ErrorCode::Unsupported,
				format!("{what} is not available on this worker yet"),
			),
		);
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
		writer,
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
	Response::Error { code, message }
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
				error(
					ErrorCode::Busy,
					"a Git process on the worker could not be cleaned up and the worker needs a restart".into(),
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
		_ if err.to_string().contains("exceeds 1 MiB") => ErrorCode::TooLarge,
		_ => ErrorCode::Io,
	};
	error(code, err.to_string())
}

impl State {
	#[allow(clippy::result_large_err)]
	fn handle(&self, request: Request) -> Response {
		match request {
			Request::Hello { .. } => {
				error(ErrorCode::BadRequest, "hello was already sent".into())
			}
			Request::OpenWorkspace { path } => match SharedRoot::open(&path) {
				Ok(root) => Response::Workspace(RemoteWorkspace {
					id: root.id,
					name: root.name,
					path: root.path.display().to_string(),
				}),
				Err(err) => io_error(err),
			},
			Request::ListDir {
				workspace,
				path,
				offset,
			} => self
				.resolve(&workspace, &path)
				.and_then(|(root, dir)| list_dir(&root, &dir, offset))
				.unwrap_or_else(|e| e),
			Request::Stat { workspace, path } => self
				.resolve(&workspace, &path)
				.and_then(|(_, p)| stat(&p))
				.unwrap_or_else(|e| e),
			Request::Read { workspace, path } => match self.root(&workspace) {
				Ok(root) => {
					match snip_core::browser::file_preview(&root, &path) {
						Ok(p) => Response::Text { content: p.content },
						Err(err) => io_error(err),
					}
				}
				Err(e) => e,
			},
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
			| Request::ImportApply { .. }
			| Request::ReplayPlan { .. }
			| Request::ReplayApply { .. }
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

/// The scan stops here even if the directory holds more: the sorted
/// continuation's memory bound on the worker. Far past any real folder; a
/// listing that hits it reports `truncated` with no next page.
const MAX_DIR_SCAN_ENTRIES: usize = 50_000;

/// Lists `dir` inside the shared `root`, sorted (folders first), served as
/// pages of [`MAX_DIR_ENTRIES`] starting at `offset`. A symlink to a
/// folder inside the share lists as a folder, as the copy engine treats
/// it; one that leads out of the share (or into `.git`) stays a plain
/// entry the master cannot open. `next` names the following page, and is
/// absent when the listing ended here.
#[allow(clippy::result_large_err)]
fn list_dir(
	root: &Path,
	dir: &Path,
	offset: usize,
) -> Result<Response, Response> {
	let mut scan = DirectoryScan::open(dir).map_err(io_error)?;
	let mut entries = Vec::new();
	let mut truncated = false;
	loop {
		let page = match scan.next_page(&ScanBudget::visits(256)) {
			Ok(page) => page,
			Err(ScanError::Changed) => {
				return Err(error(
					ErrorCode::Io,
					"the folder changed while it was read; try again".into(),
				))
			}
			Err(ScanError::Io(err)) => return Err(io_error(err)),
		};
		for entry in page.entries {
			if entries.len() >= MAX_DIR_SCAN_ENTRIES {
				truncated = true;
				break;
			}
			let path = dir.join(&entry.name);
			let directory = entry.directory
				|| (entry.symlink
					&& snip_core::transfer::is_safe_dir_symlink(root, &path));
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
		if truncated {
			break;
		}
		match page.status {
			ScanStatus::Complete => break,
			ScanStatus::Incomplete
			| ScanStatus::LimitReached
			| ScanStatus::Cancelled
			| ScanStatus::TimedOut => {
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
	// A scan that hit its own cap cannot be continued (the rest was never
	// sorted in): the listing ends here, truncated. Otherwise a page short
	// of the sorted listing's end has a next page.
	let scan_truncated = truncated;
	let total = entries.len();
	let page_end = (offset + MAX_DIR_ENTRIES).min(total);
	let page: Vec<DirEntry> = if offset < total {
		entries[offset..page_end].to_vec()
	} else {
		Vec::new()
	};
	let has_next = !scan_truncated && page_end < total;
	Ok(Response::Dir {
		entries: page,
		truncated: scan_truncated || page_end < total,
		next: has_next.then_some(page_end),
	})
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
