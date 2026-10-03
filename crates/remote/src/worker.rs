//! The worker: a listener that serves the shared workspaces of this machine
//! to paired masters. It runs on its own threads, independent of any window,
//! so the same code serves a GUI session and a headless one.

use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use snip_core::workspace::{DirectoryScan, ScanBudget, ScanError, ScanStatus};

use crate::proto::{
	read_frame, write_frame, DirEntry, EntryKind, ErrorCode, RemoteWorkspace,
	Request, Response, Stat, MAX_DIR_ENTRIES, PROTOCOL_MAX, PROTOCOL_VERSION,
};
use crate::tls::{normalize_code, pairing_proof, server_config, Fingerprint};
use crate::{Identity, RemoteError};

/// How long a pairing code stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
/// Wrong proofs before an open code is withdrawn.
pub const PAIRING_ATTEMPTS: u32 = 5;
/// Connections served at once, idle pooled ones included (each is one
/// blocked thread). More wait for a slot.
pub const MAX_CONNECTIONS: usize = 64;
/// Connections waiting for a slot; beyond this they are closed on accept.
pub const MAX_WAITING: usize = 64;
/// How long a connection waits for a slot: under the master's 5 s read
/// timeout, so a master gives up only after the worker did.
pub const SLOT_WAIT: Duration = Duration::from_secs(4);
/// A master's idle pooled connection is closed after this; the master
/// reconnects on its next call.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// A folder this worker lets paired masters read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedRoot {
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
				"a shared workspace must be a folder",
			));
		}
		let id = Fingerprint::of(path.as_os_str().as_encoded_bytes()).to_hex()
			[..16]
			.to_string();
		let name = path
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| path.display().to_string());
		Ok(Self { id, name, path })
	}
}

/// A master this worker serves, known by its certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedMaster {
	pub name: String,
	pub fingerprint: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorkerOptions {
	/// Shown to masters.
	pub name: String,
	/// Where trusted masters are kept; `None` keeps them in memory only.
	pub trust_file: Option<PathBuf>,
	/// Highest protocol version this worker will negotiate.
	pub max_protocol: Option<u32>,
}

struct PairingWindow {
	code: String,
	expires: Instant,
	failures: u32,
}

struct State {
	name: String,
	fingerprint: Fingerprint,
	tls: Arc<ServerConfig>,
	trust_file: Option<PathBuf>,
	max_protocol: Option<u32>,
	git_requests: AtomicUsize,
	deadlines: Mutex<(Duration, Duration)>,
	jobs: crate::jobs::Jobs,
	repo_cache: Mutex<crate::gitserve::RepoCache>,
	trusted: Mutex<Vec<TrustedMaster>>,
	pairing: Mutex<Option<PairingWindow>>,
	roots: RwLock<Vec<SharedRoot>>,
	connections: Arc<AtomicUsize>,
	waiting: AtomicUsize,
	stop: AtomicBool,
}

pub(crate) struct ConnSlot(Arc<AtomicUsize>);

impl Drop for ConnSlot {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::SeqCst);
	}
}

pub struct Worker {
	state: Arc<State>,
	addr: SocketAddr,
	accept: Option<JoinHandle<()>>,
}

impl Worker {
	pub fn start(
		bind: SocketAddr,
		identity: &Identity,
		opts: WorkerOptions,
	) -> Result<Self, RemoteError> {
		let listener = TcpListener::bind(bind)?;
		// Polled, so a stop needs no wake-up connection.
		listener.set_nonblocking(true)?;
		let addr = listener.local_addr()?;
		let trusted = opts
			.trust_file
			.as_deref()
			.map(|f| {
				crate::store::TrustedMasterStore::new(f.to_path_buf()).load()
			})
			.unwrap_or_default();
		let state = Arc::new(State {
			name: opts.name,
			fingerprint: identity.fingerprint(),
			tls: server_config(identity)?,
			trust_file: opts.trust_file,
			max_protocol: opts.max_protocol,
			git_requests: AtomicUsize::new(0),
			deadlines: Mutex::new((
				crate::jobs::VIEW_DEADLINE,
				crate::jobs::SCAN_DEADLINE,
			)),
			jobs: crate::jobs::Jobs::new(),
			repo_cache: Mutex::new(crate::gitserve::RepoCache::new()),
			trusted: Mutex::new(trusted),
			pairing: Mutex::new(None),
			roots: RwLock::new(Vec::new()),
			connections: Arc::new(AtomicUsize::new(0)),
			waiting: AtomicUsize::new(0),
			stop: AtomicBool::new(false),
		});
		let accept_state = state.clone();
		let accept = std::thread::Builder::new()
			.name("snip-remote-accept".into())
			.spawn(move || accept_loop(listener, accept_state))?;
		Ok(Self {
			state,
			addr,
			accept: Some(accept),
		})
	}

	pub fn local_addr(&self) -> SocketAddr {
		self.addr
	}

	pub fn fingerprint(&self) -> Fingerprint {
		self.state.fingerprint
	}

	/// Counts GitView/ScanRepos requests received, for tests and diagnostics.
	pub fn git_requests_seen(&self) -> usize {
		self.state.git_requests.load(Ordering::SeqCst)
	}

	/// Opens a one-time pairing code, replacing any open one.
	pub fn open_pairing(&self) -> String {
		let code = new_code();
		*lock(&self.state.pairing) = Some(PairingWindow {
			code: normalize_code(&code),
			expires: Instant::now() + PAIRING_TTL,
			failures: 0,
		});
		code
	}

	pub fn close_pairing(&self) {
		*lock(&self.state.pairing) = None;
	}

	/// Whether a code is open and unexpired.
	pub fn pairing_open(&self) -> bool {
		lock(&self.state.pairing)
			.as_ref()
			.is_some_and(|w| w.expires > Instant::now())
	}

	/// Replaces the shared folders. A folder that cannot be resolved is left
	/// out and returned with its error.
	pub fn set_roots(&self, paths: &[PathBuf]) -> Vec<(PathBuf, io::Error)> {
		let mut roots: Vec<SharedRoot> = Vec::new();
		let mut errors = Vec::new();
		for path in paths {
			match SharedRoot::new(path) {
				Ok(root) if roots.iter().any(|r| r.id == root.id) => {}
				Ok(root) => roots.push(root),
				Err(err) => errors.push((path.clone(), err)),
			}
		}
		let old_ids: Vec<String> = self
			.state
			.roots
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
			.map(|r| r.id.clone())
			.collect();
		let new_ids: std::collections::HashSet<_> =
			roots.iter().map(|r| &r.id).collect();
		let removed: Vec<String> = old_ids
			.into_iter()
			.filter(|id| !new_ids.contains(id))
			.collect();
		if !removed.is_empty() {
			self.state.jobs.cancel_workspaces(&removed);
		}
		lock(&self.state.repo_cache).clear();
		*self
			.state
			.roots
			.write()
			.unwrap_or_else(PoisonError::into_inner) = roots;
		errors
	}

	pub fn roots(&self) -> Vec<SharedRoot> {
		self.state
			.roots
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}

	pub fn trusted(&self) -> Vec<TrustedMaster> {
		lock(&self.state.trusted).clone()
	}

	pub fn active_connections(&self) -> usize {
		self.state.connections.load(Ordering::SeqCst)
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

	/// Stops accepting; open connections end at their next request.
	pub fn stop(&mut self) {
		self.state.stop.store(true, Ordering::SeqCst);
		self.state.jobs.cancel_all();
		if let Some(accept) = self.accept.take() {
			let _ = accept.join();
		}
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		self.stop();
	}
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn new_code() -> String {
	use ring::rand::SecureRandom;
	let mut bytes = [0u8; 8];
	ring::rand::SystemRandom::new()
		.fill(&mut bytes)
		.expect("the OS random source");
	let chars: String = bytes
		.iter()
		.map(|b| CODE_ALPHABET[(*b & 31) as usize] as char)
		.collect();
	format!("{}-{}", &chars[..4], &chars[4..])
}

fn accept_loop(listener: TcpListener, state: Arc<State>) {
	while !state.stop.load(Ordering::SeqCst) {
		match listener.accept() {
			Ok((tcp, _)) => {
				if state.waiting.fetch_add(1, Ordering::SeqCst) >= MAX_WAITING {
					state.waiting.fetch_sub(1, Ordering::SeqCst);
					continue;
				}
				let conn_state = state.clone();
				let spawned = std::thread::Builder::new()
					.name("snip-remote-conn".into())
					.spawn(move || {
						let slot = conn_state.take_slot();
						conn_state.waiting.fetch_sub(1, Ordering::SeqCst);
						if let Some(_slot) = slot {
							let _ = serve(tcp, &conn_state);
						}
					});
				if spawned.is_err() {
					state.waiting.fetch_sub(1, Ordering::SeqCst);
				}
			}
			Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
				std::thread::sleep(ACCEPT_POLL);
			}
			Err(_) => std::thread::sleep(ACCEPT_POLL),
		}
	}
}

fn serve(tcp: TcpStream, state: &State) -> Result<(), RemoteError> {
	// TCP keepalive: best effort attempt to detect vanished masters.
	let sock = socket2::SockRef::from(&tcp);
	let mut keepalive =
		socket2::TcpKeepalive::new().with_time(Duration::from_secs(15));
	keepalive = keepalive.with_interval(Duration::from_secs(5));
	// Ignore errors if the platform or socket doesn't support keepalive parameters.
	let _ = sock.set_tcp_keepalive(&keepalive);

	tcp.set_nonblocking(false)?;
	tcp.set_nodelay(true)?;
	tcp.set_read_timeout(Some(IO_TIMEOUT))?;
	tcp.set_write_timeout(Some(IO_TIMEOUT))?;
	let conn = ServerConnection::new(state.tls.clone())?;
	let mut tls = StreamOwned::new(conn, tcp);
	// The handshake completes inside the first read.
	let Some(hello) = read_frame::<Request>(&mut tls)? else {
		return Ok(());
	};
	let peer = tls
		.conn
		.peer_certificates()
		.and_then(|certs| certs.first())
		.map(|cert| Fingerprint::of(cert))
		.ok_or_else(|| RemoteError::Protocol("no client certificate".into()))?;
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
				&mut tls,
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
				&mut tls,
				&error(ErrorCode::BadRequest, "expected hello".into()),
			)?;
			return Ok(());
		}
	};
	write_frame(
		&mut tls,
		&Response::Hello {
			version: PROTOCOL_VERSION,
			name: state.name.clone(),
			paired: state.is_trusted(&peer),
			max_version: Some(negotiated),
		},
	)?;
	tls.sock.set_read_timeout(Some(IDLE_TIMEOUT))?;
	while !state.stop.load(Ordering::SeqCst) {
		let Some(request) = read_frame::<Request>(&mut tls)? else {
			return Ok(());
		};
		match request {
			Request::ScanRepos { workspace, under } => {
				state.git_requests.fetch_add(1, Ordering::SeqCst);
				if !state.is_trusted(&peer) {
					crate::jobs::write_response(
						&mut tls,
						&error(
							ErrorCode::NotPaired,
							"this master is not paired with the worker".into(),
						),
					)?;
				} else if negotiated < crate::proto::GIT_VIEWS_VERSION {
					crate::jobs::write_response(
						&mut tls,
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
						&mut tls,
						scan_deadline,
						cancel,
						|job_cancel| match state.jobs.admit(
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
				if !state.is_trusted(&peer) {
					crate::jobs::write_response(
						&mut tls,
						&error(
							ErrorCode::NotPaired,
							"this master is not paired with the worker".into(),
						),
					)?;
				} else if negotiated < crate::proto::GIT_VIEWS_VERSION {
					crate::jobs::write_response(
						&mut tls,
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
						&mut tls,
						&error(ErrorCode::BadRequest, msg),
					)?;
				} else {
					let (view_deadline, _) = *lock(&state.deadlines);
					let cancel = snip_core::gitrun::CancelToken::new();
					crate::jobs::run_job(
						&mut tls,
						view_deadline,
						cancel,
						|job_cancel| match state.jobs.admit(
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
			other => {
				let response = state.handle(&peer, other, negotiated);
				crate::jobs::write_response(&mut tls, &response)?;
			}
		}
	}
	Ok(())
}

fn error(code: ErrorCode, message: String) -> Response {
	Response::Error { code, message }
}

fn map_admit_error(code: ErrorCode) -> Response {
	match code {
		ErrorCode::Busy => error(
			ErrorCode::Busy,
			"the worker is busy with other Git requests".into(),
		),
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
			"that workspace is not shared by this worker".into(),
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
	/// Waits up to [`SLOT_WAIT`] for one of [`MAX_CONNECTIONS`] slots.
	fn take_slot(&self) -> Option<ConnSlot> {
		let deadline = Instant::now() + SLOT_WAIT;
		loop {
			// compare_exchange rather than fetch_update: newer toolchains
			// deprecate that name, and CI denies warnings.
			let n = self.connections.load(Ordering::SeqCst);
			if n < MAX_CONNECTIONS
				&& self
					.connections
					.compare_exchange(
						n,
						n + 1,
						Ordering::SeqCst,
						Ordering::SeqCst,
					)
					.is_ok()
			{
				return Some(ConnSlot(self.connections.clone()));
			}
			if n < MAX_CONNECTIONS {
				// Lost a race for the slot: look again at once.
				continue;
			}
			if Instant::now() >= deadline || self.stop.load(Ordering::SeqCst) {
				return None;
			}
			std::thread::sleep(Duration::from_millis(10));
		}
	}

	fn is_trusted(&self, peer: &Fingerprint) -> bool {
		let hex = peer.to_hex();
		lock(&self.trusted).iter().any(|m| m.fingerprint == hex)
	}

	#[allow(clippy::result_large_err)]
	fn handle(
		&self,
		peer: &Fingerprint,
		request: Request,
		_negotiated: u32,
	) -> Response {
		match request {
			Request::Hello { .. } => {
				error(ErrorCode::BadRequest, "hello was already sent".into())
			}
			Request::Pair { name, proof } => self.pair(peer, name, &proof),
			_ if !self.is_trusted(peer) => error(
				ErrorCode::NotPaired,
				"this master is not paired with the worker".into(),
			),
			Request::ListWorkspaces => Response::Workspaces {
				items: self
					.roots
					.read()
					.unwrap_or_else(PoisonError::into_inner)
					.iter()
					.map(|r| RemoteWorkspace {
						id: r.id.clone(),
						name: r.name.clone(),
						path: r.path.display().to_string(),
					})
					.collect(),
			},
			Request::ListDir { workspace, path } => self
				.resolve(&workspace, &path)
				.and_then(|(root, dir)| list_dir(&root, &dir))
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
			Request::Write { .. } | Request::Rename { .. } => error(
				ErrorCode::Unsupported,
				"not available on this worker yet".into(),
			),
		}
	}

	fn pair(&self, peer: &Fingerprint, name: String, proof: &str) -> Response {
		if self.is_trusted(peer) {
			return Response::Paired {
				name: self.name.clone(),
			};
		}
		let mut window = lock(&self.pairing);
		let Some(open) = window.as_mut().filter(|w| w.expires > Instant::now())
		else {
			*window = None;
			return error(
				ErrorCode::PairingRefused,
				"no pairing code is open on the worker".into(),
			);
		};
		let expected = pairing_proof(&open.code, &self.fingerprint, peer);
		let given = crate::from_hex(proof).unwrap_or_default();
		if !constant_time_eq(&expected, &given) {
			open.failures += 1;
			if open.failures >= PAIRING_ATTEMPTS {
				*window = None;
			}
			return error(
				ErrorCode::PairingRefused,
				"the pairing code does not match".into(),
			);
		}
		let master = TrustedMaster {
			name,
			fingerprint: peer.to_hex(),
		};
		if let Some(file) = &self.trust_file {
			let store = crate::store::TrustedMasterStore::new(file.clone());
			if let Err(err) = store.add(master.clone()) {
				return error(
					ErrorCode::Io,
					format!("cannot save the trusted master: {err}"),
				);
			}
		}
		// One code pairs one master.
		*window = None;
		drop(window);
		let mut trusted = lock(&self.trusted);
		trusted.push(master);
		Response::Paired {
			name: self.name.clone(),
		}
	}

	#[allow(clippy::result_large_err)]
	pub(crate) fn get_shared_root(
		&self,
		workspace: &str,
	) -> Result<SharedRoot, Response> {
		self.roots
			.read()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
			.find(|r| r.id == workspace)
			.cloned()
			.ok_or_else(|| {
				error(
					ErrorCode::Forbidden,
					"that workspace is not shared by this worker".into(),
				)
			})
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len()
		&& a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Lists `dir` inside the shared `root`. A symlink to a folder inside the
/// share lists as a folder, as the copy engine treats it; one that leads
/// out of the share (or into `.git`) stays a plain entry the master cannot
/// open.
#[allow(clippy::result_large_err)]
fn list_dir(root: &Path, dir: &Path) -> Result<Response, Response> {
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
			if entries.len() >= MAX_DIR_ENTRIES {
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

	#[test]
	fn codes_use_the_unambiguous_alphabet() {
		let code = new_code();
		assert_eq!(code.len(), 9);
		assert!(normalize_code(&code)
			.bytes()
			.all(|b| CODE_ALPHABET.contains(&b)));
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

	#[test]
	fn a_panicking_job_frees_its_connection_slot() {
		let counter = Arc::new(AtomicUsize::new(1));
		let slot = ConnSlot(counter.clone());
		let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			let _held_slot = slot;
			let mut buf = Vec::new();
			let _ = crate::jobs::run_job(
				&mut buf,
				Duration::from_secs(1),
				snip_core::gitrun::CancelToken::new(),
				|_| panic!("simulated job panic"),
			);
		}));
		assert_eq!(counter.load(Ordering::SeqCst), 0);
	}
}
