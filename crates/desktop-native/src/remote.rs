//! Remote-node mode (snip-remote) inside the workbench.
//!
//! - Worker: a listener that outlives any window, started by `--worker`
//!   or from the workspace menu. It shares the `--share` folders and the
//!   workspace this app has open.
//! - Master: paired workers, and the remote workspace that replaces the
//!   local one while it is open. Its Project tree and file previews are
//!   read through the worker; everything else stays local and is refused
//!   while a remote workspace is open.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use snip_core::browser::SourcePreview;
use snip_core::transfer::SourceKind;
use snip_core::workspace::{RepoIdentity, RepoKind, RepoSummary, ScanStatus};
use snip_remote::{
	Client, Identity, PairedWorker, RemoteError, RemoteWorkspace, Worker,
	WorkerOptions, WorkerStore, TRUSTED_FILE, WORKERS_FILE,
};

use gpui::Context;

use crate::i18n::Msg;
use crate::reader::PreviewSource;
use crate::tree::{
	listed_tree_result, FileTreeNode, ListedChild, TreeIo, TreeIoResult,
};
use crate::{arm_cancel, lifecycle, RepoEntry, WorkbenchModel};

pub use snip_remote::DEFAULT_LISTEN;

/// Worker flags from the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerCli {
	pub enabled: bool,
	pub headless: bool,
	pub listen: Option<SocketAddr>,
	pub shares: Vec<PathBuf>,
}

pub use snip_remote::device_name;

/// The device identity, kept in the config folder; in memory only when
/// there is none (a test, or an e2e run without `SNIP_CONFIG_DIR`).
pub fn identity() -> Result<Arc<Identity>, String> {
	static ID: OnceLock<Result<Arc<Identity>, String>> = OnceLock::new();
	ID.get_or_init(|| {
		let id = match crate::recent::config_dir() {
			Some(dir) => Identity::load_or_create(&dir),
			None => Identity::generate(),
		};
		id.map(Arc::new).map_err(|e| e.to_string())
	})
	.clone()
}

fn config_file(name: &str) -> Option<PathBuf> {
	crate::recent::config_dir().map(|dir| dir.join(name))
}

fn worker_store() -> Option<WorkerStore> {
	config_file(WORKERS_FILE).map(WorkerStore::new)
}

// ───────────────────────── worker ─────────────────────────

struct WorkerHost {
	worker: Worker,
	shares: Vec<PathBuf>,
	open: Option<PathBuf>,
	code: Option<(String, Instant)>,
}

impl WorkerHost {
	fn apply_roots(&self) {
		let mut roots = self.shares.clone();
		roots.extend(self.open.clone());
		for (path, err) in self.worker.set_roots(&roots) {
			eprintln!(
				"snip-sync worker: not sharing {}: {err}",
				path.display()
			);
		}
	}
}

fn host() -> &'static Mutex<Option<WorkerHost>> {
	static HOST: OnceLock<Mutex<Option<WorkerHost>>> = OnceLock::new();
	HOST.get_or_init(|| Mutex::new(None))
}

fn with_host<T>(f: impl FnOnce(&mut Option<WorkerHost>) -> T) -> T {
	f(&mut host().lock().unwrap_or_else(PoisonError::into_inner))
}

/// What the workspace menu shows about this machine's worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStatus {
	pub addr: SocketAddr,
	pub fingerprint: String,
	pub shared: usize,
	pub masters: usize,
	/// The open pairing code and its seconds left.
	pub code: Option<(String, u64)>,
}

/// Starts the worker unless one runs. `open` is the workspace already open.
pub fn start_worker(
	listen: SocketAddr,
	shares: &[PathBuf],
	open: Option<&Path>,
) -> Result<SocketAddr, String> {
	with_host(|slot| {
		if let Some(host) = slot {
			return Ok(host.worker.local_addr());
		}
		let id = identity()?;
		let worker = Worker::start(
			listen,
			&id,
			WorkerOptions {
				name: device_name(),
				trust_file: config_file(TRUSTED_FILE),
				..Default::default()
			},
		)
		.map_err(|e| format!("{listen}: {e}"))?;
		let addr = worker.local_addr();
		let host = WorkerHost {
			worker,
			shares: shares.to_vec(),
			open: open.map(Path::to_path_buf),
			code: None,
		};
		host.apply_roots();
		*slot = Some(host);
		Ok(addr)
	})
}

pub fn stop_worker() {
	// Dropped outside the lock: stopping joins the accept thread.
	let host = with_host(Option::take);
	drop(host);
}

pub fn worker_running() -> bool {
	with_host(|slot| slot.is_some())
}

/// The workspace this app has open, shared while the worker runs.
pub fn worker_set_open(open: Option<&Path>) {
	with_host(|slot| {
		if let Some(host) = slot {
			host.open = open.map(Path::to_path_buf);
			host.apply_roots();
		}
	});
}

pub fn worker_open_pairing() -> Option<String> {
	with_host(|slot| {
		let host = slot.as_mut()?;
		let code = host.worker.open_pairing();
		host.code = Some((code.clone(), Instant::now()));
		Some(code)
	})
}

pub fn worker_status() -> Option<WorkerStatus> {
	with_host(|slot| {
		let host = slot.as_ref()?;
		let ttl = snip_remote::worker::PAIRING_TTL;
		let code = host.code.as_ref().and_then(|(code, at)| {
			let left = ttl.checked_sub(at.elapsed())?;
			host.worker
				.pairing_open()
				.then(|| (code.clone(), left.as_secs()))
		});
		Some(WorkerStatus {
			addr: host.worker.local_addr(),
			fingerprint: host.worker.fingerprint().short(),
			shared: host.worker.roots().len(),
			masters: host.worker.trusted().len(),
			code,
		})
	})
}

/// `--worker --headless`: the CLI's `snip worker`, from the desktop build.
pub fn run_headless(cli: &WorkerCli) -> ! {
	let listen = cli
		.listen
		.unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("a socket address"));
	let Err(err) = snip_remote::run_headless_worker(
		listen,
		&cli.shares,
		crate::recent::config_dir().as_deref(),
		None,
	);
	eprintln!("Error: cannot start the worker: {err}");
	std::process::exit(1);
}

// ───────────────────────── master ─────────────────────────

pub fn load_workers() -> Vec<PairedWorker> {
	worker_store().map(|s| s.load()).unwrap_or_default()
}

/// Two-line tooltip for a paired worker row: name on first line, address and full grouped fingerprint on second.
pub fn remote_worker_tip(worker: &PairedWorker) -> String {
	let fp = snip_remote::Fingerprint::from_hex(&worker.fingerprint)
		.map(|f| f.short())
		.unwrap_or_else(|| worker.fingerprint.clone());
	format!("{}\n{} · {}", worker.name, worker.addr, fp)
}

/// The remote workspace open in place of a local one.
pub struct RemoteSession {
	pub client: Arc<Client>,
	pub workspace: RemoteWorkspace,
	/// Never a real path: the tree's root key while the session is open.
	pub root: PathBuf,
}

impl RemoteSession {
	pub fn new(
		worker: PairedWorker,
		workspace: RemoteWorkspace,
	) -> Result<Self, String> {
		let root = PathBuf::from(format!(
			"snip-remote://{}/{}",
			worker.fingerprint, workspace.id
		));
		let client = Client::new(worker, identity()?, device_name())
			.map_err(|e| e.to_string())?;
		Ok(Self {
			client: Arc::new(client),
			workspace,
			root,
		})
	}

	pub fn label(&self) -> String {
		format!("{} ▸ {}", self.client.worker().name, self.workspace.name)
	}

	pub fn tip(&self) -> String {
		format!("{}:{}", self.client.worker().name, self.workspace.path)
	}
}

/// Formats a repo path for display: in a remote session, maps paths under session root to worker:workspace_path[/rel].
pub fn display_path(session: Option<&RemoteSession>, root: &Path) -> String {
	if let Some(session) = session {
		if let Some(rel) = remote_rel(&session.root, root) {
			let base = session.workspace.path.trim_end_matches('/');
			let worker = &session.client.worker().name;
			if rel.is_empty() {
				return format!("{worker}:{base}");
			} else {
				return format!("{worker}:{base}/{rel}");
			}
		}
		return session.tip();
	}
	root.display().to_string()
}

/// A worker's shared workspaces, or why they could not be listed.
pub type WorkspaceListing = Result<Vec<RemoteWorkspace>, String>;

/// What master UI holds about remote work.
#[derive(Default)]
pub struct MasterState {
	pub workers: Vec<PairedWorker>,
	pub session: Option<RemoteSession>,
	/// The pairing form is shown.
	pub pairing: bool,
	pub busy: bool,
	pub message: Option<(bool, String)>,
	/// Worker whose workspaces are listed, and the listing once it lands.
	pub browse: Option<(usize, Option<WorkspaceListing>)>,
	pub scan_error: Option<Msg>,
}

/// Returns true when `s` is empty or a relative path consisting only of normal UTF-8 components.
pub(crate) fn valid_rel(s: &str) -> bool {
	if s.is_empty() {
		return true;
	}
	if s.contains('\0') || s.contains('\\') {
		return false;
	}
	for part in s.split('/') {
		if part.is_empty() || part == "." || part == ".." {
			return false;
		}
	}
	let p = Path::new(s);
	p.components()
		.all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Strips `session_root` from `path` and formats normal components with "/" for worker communication.
pub(crate) fn remote_rel(session_root: &Path, path: &Path) -> Option<String> {
	let rel = path.strip_prefix(session_root).ok()?;
	let mut parts = Vec::new();
	for comp in rel.components() {
		match comp {
			std::path::Component::Normal(c) => {
				let s = c.to_str()?;
				parts.push(s);
			}
			_ => return None,
		}
	}
	Some(parts.join("/"))
}

/// Joins two relative path fragments with a single "/" separator, trimming existing slashes.
pub(crate) fn join_rel(prefix: &str, rel: &str) -> String {
	let p = prefix.trim_matches('/');
	let r = rel.trim_matches('/');
	if p.is_empty() && r.is_empty() {
		String::new()
	} else if p.is_empty() {
		r.to_string()
	} else if r.is_empty() {
		p.to_string()
	} else {
		format!("{p}/{r}")
	}
}

/// Only a key for `RepoSummary.identity`, must not be given to local git.
pub(crate) fn key_identity(
	root: &Path,
	kind: snip_core::gitview::FoundKind,
) -> RepoIdentity {
	let repo_kind = match kind {
		snip_core::gitview::FoundKind::Main => RepoKind::Main,
		snip_core::gitview::FoundKind::LinkedWorktree => {
			RepoKind::LinkedWorktree
		}
		snip_core::gitview::FoundKind::Submodule
		| snip_core::gitview::FoundKind::UninitializedSubmodule => {
			RepoKind::Submodule
		}
	};
	let git_dir = root.join(".git");
	RepoIdentity {
		toplevel: root.to_path_buf(),
		git_dir: git_dir.clone(),
		common_dir: git_dir,
		kind: repo_kind,
	}
}

/// Status message when a remote scan ends with a non-Complete status.
pub(crate) fn scan_incomplete_msg(
	status: ScanStatus,
	repos_len: usize,
) -> Option<Msg> {
	if status != ScanStatus::Complete {
		Some(Msg::new("remote_scan_incomplete", [repos_len.to_string()]))
	} else {
		None
	}
}

/// Repositories, notes, and pagination status produced by translating a worker RepoScan.
pub(crate) struct RemoteScanEntries {
	pub repos: Vec<RepoEntry>,
	pub errors: Vec<(PathBuf, String)>,
	pub depth_limited: Vec<PathBuf>,
	pub error_overflow: usize,
	pub depth_overflow: usize,
	pub status: ScanStatus,
}

/// Translates and bounds a worker RepoScan onto the fake session root, dropping invalid paths.
pub(crate) fn remote_scan_entries(
	session_root: &Path,
	scan: snip_remote::RepoScan,
) -> RemoteScanEntries {
	let mut repos = Vec::with_capacity(scan.repos.len());
	let mut errors = Vec::with_capacity(scan.errors.len());
	let mut depth_limited = Vec::with_capacity(scan.depth_limited.len());

	for scanned in scan.repos {
		if !scanned.utf8 || !valid_rel(&scanned.rel) {
			errors.push((
				session_root.to_path_buf(),
				"worker returned an invalid path".to_string(),
			));
			continue;
		}
		let mut root = session_root.to_path_buf();
		if !scanned.rel.is_empty() {
			for part in scanned.rel.split('/') {
				root.push(part);
			}
		}
		let kind = scanned.kind;
		let summary = scanned.summary.map(|s| RepoSummary {
			identity: key_identity(&root, kind),
			head: s.head,
			branch: s.branch,
			changes: s.changes,
		});
		repos.push(RepoEntry {
			root,
			name: scanned.name,
			kind: kind.into(),
			identity: None,
			summary,
		});
	}

	for (raw_rel, msg) in scan.errors {
		let rel = if raw_rel == "." { "" } else { raw_rel.as_str() };
		if !valid_rel(rel) {
			errors.push((
				session_root.to_path_buf(),
				"worker returned an invalid path".to_string(),
			));
			continue;
		}
		let mut path = session_root.to_path_buf();
		if !rel.is_empty() {
			for part in rel.split('/') {
				path.push(part);
			}
		}
		errors.push((path, msg));
	}

	for raw_rel in scan.depth_limited {
		let rel = if raw_rel == "." { "" } else { raw_rel.as_str() };
		if !valid_rel(rel) {
			errors.push((
				session_root.to_path_buf(),
				"worker returned an invalid path".to_string(),
			));
			continue;
		}
		let mut path = session_root.to_path_buf();
		if !rel.is_empty() {
			for part in rel.split('/') {
				path.push(part);
			}
		}
		depth_limited.push(path);
	}

	let status = match scan.status {
		ScanStatus::More => ScanStatus::Incomplete,
		other => other,
	};

	RemoteScanEntries {
		repos,
		errors,
		depth_limited,
		error_overflow: scan.error_overflow,
		depth_overflow: scan.depth_overflow,
		status,
	}
}

/// Blocking: the remote counterpart of [`crate::tree::execute_tree_io`].
pub fn tree_io(
	client: &Client,
	workspace: &str,
	session_root: &Path,
	io: TreeIo,
) -> TreeIoResult {
	let prefix = match remote_rel(session_root, &io.base) {
		Some(p) => p,
		None => {
			return listed_tree_result(
				io,
				Err("not under the workspace".to_string()),
			)
		}
	};
	let listed = match io.key.utf8_rel() {
		Some(key_rel) => {
			let req_path = join_rel(&prefix, &key_rel);
			client
				.list_dir(workspace, &req_path)
				.map(|(entries, truncated)| {
					let children = entries
						.into_iter()
						.map(|e| ListedChild {
							name: e.name,
							utf8: e.utf8,
							directory: e.directory,
							nested_repo: e.nested_repo,
						})
						.collect();
					(children, truncated)
				})
				.map_err(describe)
		}
		None => Err("non-UTF-8 folder name".to_string()),
	};
	listed_tree_result(io, listed)
}

/// Blocking: a file preview read on the worker. Only working-tree files
/// are served in this slice.
pub fn read_preview(
	client: &Client,
	workspace: &str,
	prefix: &str,
	path: &str,
) -> Result<(SourcePreview, PreviewSource), String> {
	let full_path = join_rel(prefix, path);
	client
		.read(workspace, &full_path)
		.map(|content| {
			(
				SourcePreview {
					content,
					patch: String::new(),
				},
				PreviewSource::WorkingFile,
			)
		})
		.map_err(describe)
}

pub fn describe(err: RemoteError) -> String {
	match err {
		RemoteError::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => {
			"the worker did not answer in time".into()
		}
		other => other.to_string(),
	}
}

fn apply_pairing(
	store: Option<&WorkerStore>,
	workers: &mut Vec<PairedWorker>,
	worker: PairedWorker,
) -> Result<usize, String> {
	match store {
		Some(store) => {
			if let Err(e) = store.add(worker.clone()) {
				*workers = store.load();
				return Err(format!("cannot save pairings: {e}"));
			}
			*workers = store.load();
			let idx = workers
				.iter()
				.position(|w| w.fingerprint == worker.fingerprint)
				.unwrap_or(0);
			Ok(idx)
		}
		None => {
			workers.retain(|w| {
				w.fingerprint != worker.fingerprint && w.addr != worker.addr
			});
			workers.insert(0, worker);
			Ok(0)
		}
	}
}

fn apply_forget(
	store: Option<&WorkerStore>,
	workers: &mut Vec<PairedWorker>,
	fingerprint: &str,
) -> Result<(), String> {
	match store {
		Some(store) => {
			let res = store.forget(fingerprint);
			*workers = store.load();
			res.map(|_| ())
				.map_err(|e| format!("cannot save pairings: {e}"))
		}
		None => {
			workers.retain(|w| w.fingerprint != fingerprint);
			Ok(())
		}
	}
}

// ───────────────────────── model ─────────────────────────

impl WorkbenchModel {
	/// The worker client, workspace id and session root while a remote workspace is open.
	pub(crate) fn remote_target(
		&self,
	) -> Option<(Arc<Client>, String, PathBuf)> {
		self.remote
			.session
			.as_ref()
			.map(|s| (s.client.clone(), s.workspace.id.clone(), s.root.clone()))
	}

	/// Worker workspace path for a path under session.root.
	pub(crate) fn remote_worker_path(
		&self,
		root_or_path: &Path,
	) -> Option<String> {
		let session = self.remote.session.as_ref()?;
		let rel = remote_rel(&session.root, root_or_path)?;
		let base = session.workspace.path.trim_end_matches('/');
		if rel.is_empty() {
			Some(base.to_string())
		} else {
			Some(format!("{base}/{rel}"))
		}
	}

	/// Refuses local operations when a remote session is active, setting the unsupported status.
	pub(crate) fn remote_blocks(&mut self) -> bool {
		if self.remote.session.is_some() {
			self.set_status("remote_unsupported", []);
			true
		} else {
			false
		}
	}

	fn remote_note(&mut self, ok: bool, text: String, cx: &mut Context<Self>) {
		self.remote.message = Some((ok, text));
		cx.notify();
	}

	/// Starts or stops this machine's worker (role switch).
	pub fn toggle_worker_mode(&mut self, cx: &mut Context<Self>) {
		if worker_running() {
			stop_worker();
			app_log!("[APP:REMOTE_WORKER: state=stopped]");
			self.remote_note(
				true,
				crate::i18n::t("remote_worker_stopped", self.locale).into(),
				cx,
			);
			return;
		}
		let open = (self.workspace_open && self.remote.session.is_none())
			.then(|| self.workspace_root.clone());
		let listen = DEFAULT_LISTEN.parse().expect("a socket address");
		match start_worker(listen, &[], open.as_deref()) {
			Ok(addr) => {
				app_log!("[APP:REMOTE_WORKER: state=listening addr={addr}]");
				self.remote.message = None;
			}
			Err(err) => self.remote.message = Some((false, err)),
		}
		cx.notify();
	}

	pub fn open_worker_pairing(&mut self, cx: &mut Context<Self>) {
		if worker_open_pairing().is_some() {
			app_log!("[APP:REMOTE_PAIRING: state=open]");
		}
		cx.notify();
	}

	pub fn show_remote_pairing(&mut self, cx: &mut Context<Self>) {
		self.remote.pairing = !self.remote.pairing;
		if self.remote.pairing {
			self.pending_focus = Some(self.remote_addr_input.read(cx).handle());
		}
		cx.notify();
	}

	/// Pairs with the worker typed into the form, then lists its workspaces.
	pub fn pair_remote_worker(&mut self, cx: &mut Context<Self>) {
		if self.remote.busy {
			return;
		}
		let addr = self.remote_addr_input.read(cx).text().trim().to_string();
		let code = self.remote_code_input.read(cx).text().trim().to_string();
		if addr.is_empty() || code.is_empty() {
			let text = crate::i18n::t("remote_pair_missing", self.locale);
			self.remote_note(false, text.into(), cx);
			return;
		}
		let id = match identity() {
			Ok(id) => id,
			Err(err) => return self.remote_note(false, err, cx),
		};
		self.remote.busy = true;
		self.remote.message = None;
		cx.notify();
		let bg = cx.background_executor().clone();
		cx.spawn(async move |this, cx| {
			let result = bg
				.spawn(async move {
					snip_remote::pair(&addr, &code, &id, &device_name())
				})
				.await;
			let _ = this.update(cx, |this, cx| {
				this.remote.busy = false;
				match result {
					Ok(worker) => {
						app_log!(
							"[APP:REMOTE_PAIRED: name={} fp={}]",
							worker.name,
							&worker.fingerprint[..16]
						);
						let store = worker_store();
						let idx = match apply_pairing(
							store.as_ref(),
							&mut this.remote.workers,
							worker,
						) {
							Ok(idx) => idx,
							Err(msg) => {
								this.remote_note(false, msg, cx);
								return;
							}
						};
						this.remote.pairing = false;
						for input in [
							this.remote_addr_input.clone(),
							this.remote_code_input.clone(),
						] {
							input.update(cx, |i, _| i.clear_retained());
						}
						this.browse_remote_worker(idx, cx);
					}
					Err(err) => {
						let text = describe(err);
						app_log!("[APP:REMOTE_PAIR_FAILED: {text}]");
						this.remote_note(false, text, cx);
					}
				}
			});
		})
		.detach();
	}

	/// Lists worker `idx`'s shared workspaces in the menu.
	pub fn browse_remote_worker(&mut self, idx: usize, cx: &mut Context<Self>) {
		let Some(worker) = self.remote.workers.get(idx).cloned() else {
			return;
		};
		let id = match identity() {
			Ok(id) => id,
			Err(err) => return self.remote_note(false, err, cx),
		};
		self.remote.browse = Some((idx, None));
		self.remote.message = None;
		cx.notify();
		let bg = cx.background_executor().clone();
		let fingerprint = worker.fingerprint.clone();
		cx.spawn(async move |this, cx| {
			let result = bg
				.spawn(async move {
					Client::new(worker, id, device_name())
						.and_then(|c| c.list_workspaces())
						.map_err(describe)
				})
				.await;
			let _ = this.update(cx, |this, cx| {
				// The list moved or another worker is shown now.
				let current = this.remote.browse.as_ref().map(|(i, _)| *i);
				let same = current
					.and_then(|i| this.remote.workers.get(i))
					.is_some_and(|w| w.fingerprint == fingerprint);
				if !same {
					return;
				}
				if let Ok(items) = &result {
					app_log!("[APP:REMOTE_WORKSPACES: count={}]", items.len());
				}
				this.remote.browse = current.map(|i| (i, Some(result)));
				cx.notify();
			});
		})
		.detach();
	}

	pub fn forget_remote_worker(&mut self, idx: usize, cx: &mut Context<Self>) {
		let Some(worker) = self.remote.workers.get(idx) else {
			return;
		};
		let fp = worker.fingerprint.clone();
		let store = worker_store();
		if let Err(err) =
			apply_forget(store.as_ref(), &mut self.remote.workers, &fp)
		{
			self.remote_note(false, err, cx);
		}
		self.remote.browse = None;
		// Its open workspace goes too: the session would keep reading
		// with a trust the user just withdrew.
		let open_here = self
			.remote
			.session
			.as_ref()
			.is_some_and(|s| s.client.worker().fingerprint == fp);
		if open_here && !self.remote.workers.iter().any(|w| w.fingerprint == fp)
		{
			self.request_user_close(lifecycle::Intent::CloseWorkspace, cx);
		}
		cx.notify();
	}

	/// Opens a listed remote workspace after the usual close checks.
	pub fn open_remote_workspace(
		&mut self,
		worker: usize,
		workspace: usize,
		cx: &mut Context<Self>,
	) {
		let Some(paired) = self.remote.workers.get(worker).cloned() else {
			return;
		};
		let Some(Some(Ok(items))) = self
			.remote
			.browse
			.as_ref()
			.filter(|(i, _)| *i == worker)
			.map(|(_, items)| items)
		else {
			return;
		};
		let Some(ws) = items.get(workspace).cloned() else {
			return;
		};
		self.workspace_menu = false;
		self.request_user_close(
			lifecycle::Intent::OpenRemoteWorkspace(Box::new((paired, ws))),
			cx,
		);
	}

	pub(crate) fn finish_open_remote(
		&mut self,
		worker: PairedWorker,
		workspace: RemoteWorkspace,
		cx: &mut Context<Self>,
	) {
		self.release_workspace_state(cx);
		remote_drop_local_share();
		let session = match RemoteSession::new(worker, workspace) {
			Ok(session) => session,
			Err(err) => {
				self.workspace_open = false;
				self.status = Msg::new("remote_open_failed", [err]);
				cx.notify();
				return;
			}
		};
		let root = session.root.clone();
		let label = session.label();
		self.workspace_root = root.clone();
		self.remote.session = Some(session);
		self.workspace_open = true;
		self.workspace_menu = false;
		self.workspace_picker = false;
		self.ws_home = Some(root.clone());
		self.ws_tree = Some(FileTreeNode::unloaded_root(&root));
		app_log!(
			"[APP:REMOTE_OPENED: {label} generation={}]",
			self.lifecycle.generation()
		);
		self.status = Msg::new("remote_opened", [label]);
		self.launch_remote_scan(None, true, cx);
		self.resume_ws_tree(cx);
	}

	/// Scans the worker workspace for Git repositories in the background, updating discovery state.
	pub(crate) fn launch_remote_scan(
		&mut self,
		under: Option<PathBuf>,
		wipe: bool,
		cx: &mut Context<Self>,
	) {
		if !self.accepting_work() {
			return;
		}
		let (client, ws, session_root) = match &self.remote.session {
			Some(s) => {
				(s.client.clone(), s.workspace.id.clone(), s.root.clone())
			}
			None => return,
		};
		let under_rel = match under {
			Some(p) => match remote_rel(&session_root, &p) {
				Some(rel) => Some(rel),
				None => return,
			},
			None => None,
		};
		if wipe {
			self.remote.scan_error = None;
		}
		self.discovery_generation = self.discovery_generation.wrapping_add(1);
		let generation = self.discovery_generation;
		let cancel = arm_cancel(&mut self.scan_cancel);
		self.is_loading = true;
		self.set_status("status_scanning", []);
		let lifecycle_generation = self.lifecycle.generation();
		let cancel_job = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();

		self.spawn_owned(
			cx,
			lifecycle::JobKind::CancellableRead,
			Some(cancel_job),
			async move {
				let bg = async_app.background_executor().clone();
				let cancel_bg = cancel.clone();
				let client_bg = client.clone();
				let ws_bg = ws.clone();
				let under_bg = under_rel.clone();
				let result = bg
					.spawn(async move {
						client_bg.scan_repos(
							&ws_bg,
							under_bg.as_deref(),
							Some(&cancel_bg),
						)
					})
					.await;
				let _ = this.update(&mut async_app, |model, cx| {
					if model.discovery_generation != generation
						|| model.lifecycle.generation() != lifecycle_generation
					{
						return;
					}
					match result {
						Ok(scan) => {
							let e = remote_scan_entries(&session_root, scan);
							if wipe {
								model.begin_rescan();
							}
							model.merge_repo_entries(e.repos);
							for err in e.errors {
								model.push_discovery_error(err);
							}
							for depth in e.depth_limited {
								model.push_depth_limit(depth);
							}
							model.discovery_error_overflow = model
								.discovery_error_overflow
								.saturating_add(e.error_overflow);
							model.discovery_depth_overflow = model
								.discovery_depth_overflow
								.saturating_add(e.depth_overflow);
							model.discovery_status = Some(e.status);
							app_log!(
								"[APP:DISCOVERY_PROGRESS: repos={} status={:?} visited=0]",
								model.repos.len(),
								e.status
							);
							model.place_selection(cx);
							model.finish_discovery(cx);
							if let Some(msg) =
								scan_incomplete_msg(e.status, model.repos.len())
							{
								model.set_status(msg.key, msg.args);
							}
						}
						Err(RemoteError::Cancelled) => {}
						Err(err) => {
							let (msg, text) = match err {
								RemoteError::WorkerTooOld {
									worker,
									have,
									need,
								} => {
									let text = (RemoteError::WorkerTooOld {
										worker: worker.clone(),
										have,
										need,
									})
									.to_string();
									(
										Msg::new(
											"remote_worker_too_old",
											[worker, have.to_string()],
										),
										text,
									)
								}
								other => {
									let text = describe(other);
									(
										Msg::new(
											"remote_scan_failed",
											[text.clone()],
										),
										text,
									)
								}
							};
							app_log!("[APP:REMOTE_SCAN_FAILED: {text}]");
							model.remote.scan_error = Some(msg);
							model.discovery_status =
								Some(ScanStatus::Incomplete);
							model.is_loading = false;
							model.refresh_reload = false;
							if wipe {
								let file_sel = (model
									.selected_commit
									.is_none() && model
									.selected_file_source
									== Some(SourceKind::File))
								.then(|| {
									let path = model.selected_file.clone()?;
									let root = model
										.selected_file_root
										.clone()
										.or_else(|| {
											model
												.remote
												.session
												.as_ref()
												.map(|s| s.root.clone())
										})?;
									Some((root, path))
								})
								.flatten();
								let ws_root =
									model.ws_home.clone().or_else(|| {
										model
											.remote
											.session
											.as_ref()
											.map(|s| s.root.clone())
									});
								let is_ws_file_tree =
									model.file_tree.as_ref().is_some_and(|t| {
										Some(&t.full_path) == ws_root.as_ref()
									});
								let mut saved_ws_expanded = std::mem::take(
									&mut model.restore_ws_expanded,
								);
								let saved_expanded = if is_ws_file_tree {
									if let Some(tree) = &model.file_tree {
										tree.collect_expanded_paths(
											&mut saved_ws_expanded,
										);
									}
									std::mem::take(&mut model.restore_expanded)
								} else {
									Vec::new()
								};
								model.repos.clear();
								model.pinned_repo = None;
								model.selected_repo_idx = None;
								model.release_repo_state();
								model.restore_ws_expanded = saved_ws_expanded;
								if !saved_expanded.is_empty() {
									model
										.restore_ws_expanded
										.extend(saved_expanded.clone());
									model.restore_expanded = saved_expanded;
								}
								model.sync_change_slots();
								if let Some((root, path)) = file_sel {
									model.select_file_in(
										Some(root),
										&path,
										SourceKind::File,
										cx,
									);
								}
							}
							model.ensure_ws_tree(cx);
							if !model.tree_worker_alive {
								model.resume_ws_tree(cx);
							}
							cx.notify();
						}
					}
				});
			},
		);
	}
}

/// A master browsing another machine shares nothing of its own.
fn remote_drop_local_share() {
	worker_set_open(None);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample_worker(name: &str, addr: &str, fp: &str) -> PairedWorker {
		PairedWorker {
			name: name.into(),
			addr: addr.into(),
			fingerprint: fp.into(),
		}
	}

	#[test]
	fn apply_pairing_fails_on_unwritable_store() {
		let tmp = tempfile::tempdir().unwrap();
		let regular_file = tmp.path().join("a_file");
		std::fs::write(&regular_file, "blocking").unwrap();
		let bad_path = regular_file.join(WORKERS_FILE);
		let store = WorkerStore::new(bad_path);

		let existing = sample_worker("w1", "1.1.1.1:1", "fp1");
		let mut workers = vec![existing.clone()];
		let new_worker = sample_worker("w2", "2.2.2.2:2", "fp2");

		let res = apply_pairing(Some(&store), &mut workers, new_worker.clone());
		let err = res.expect_err("expected error for unwritable path");
		assert!(
			err.contains("cannot save pairings"),
			"expected error containing 'cannot save pairings', got: {err}"
		);
		assert!(
			!workers
				.iter()
				.any(|w| w.fingerprint == new_worker.fingerprint),
			"new worker should not be in workers on save failure"
		);
	}

	#[test]
	fn apply_pairing_succeeds_and_locates_worker_with_concurrent_addition() {
		let tmp = tempfile::tempdir().unwrap();
		let store = WorkerStore::in_config_dir(tmp.path());
		let w1 = sample_worker("w1", "1.1.1.1:1", "fp1");
		store.add(w1.clone()).unwrap();

		let mut workers = store.load();
		let w_concurrent =
			sample_worker("w_concurrent", "2.2.2.2:2", "fp_concurrent");
		store.add(w_concurrent.clone()).unwrap();

		let new_worker = sample_worker("w_new", "3.3.3.3:3", "fp_new");
		let idx = apply_pairing(Some(&store), &mut workers, new_worker.clone())
			.expect("apply_pairing should succeed");

		assert_eq!(workers[idx].fingerprint, new_worker.fingerprint);
		assert_eq!(workers.len(), 3);
	}

	#[test]
	fn apply_pairing_none_store_in_memory_and_dedupes() {
		let mut workers = vec![
			sample_worker("w1", "1.1.1.1:1", "fp1"),
			sample_worker("w2", "2.2.2.2:2", "fp2"),
		];

		let w3 = sample_worker("w3", "3.3.3.3:3", "fp3");
		let idx = apply_pairing(None, &mut workers, w3.clone()).unwrap();
		assert_eq!(idx, 0);
		assert_eq!(workers[0], w3);
		assert_eq!(workers.len(), 3);

		let w1_updated = sample_worker("w1_renamed", "1.1.1.1:99", "fp1");
		let idx =
			apply_pairing(None, &mut workers, w1_updated.clone()).unwrap();
		assert_eq!(idx, 0);
		assert_eq!(workers[0], w1_updated);
		assert_eq!(workers.len(), 3);
		assert_eq!(
			workers.iter().filter(|w| w.fingerprint == "fp1").count(),
			1
		);

		let w_same_addr = sample_worker("w_new_addr", "2.2.2.2:2", "fp_diff");
		let idx =
			apply_pairing(None, &mut workers, w_same_addr.clone()).unwrap();
		assert_eq!(idx, 0);
		assert_eq!(workers[0], w_same_addr);
		assert_eq!(workers.len(), 3);
		assert_eq!(workers.iter().filter(|w| w.addr == "2.2.2.2:2").count(), 1);
		assert!(!workers.iter().any(|w| w.fingerprint == "fp2"));
	}

	#[test]
	fn apply_forget_store_removes_by_fingerprint_and_persists() {
		let tmp = tempfile::tempdir().unwrap();
		let store = WorkerStore::in_config_dir(tmp.path());
		let w1 = sample_worker("w1", "1.1.1.1:1", "fp1");
		let w2 = sample_worker("w2", "2.2.2.2:2", "fp2");
		store.add(w1.clone()).unwrap();
		store.add(w2.clone()).unwrap();

		let mut workers = store.load();
		apply_forget(Some(&store), &mut workers, "fp1")
			.expect("apply_forget should succeed");

		assert_eq!(workers.len(), 1);
		assert_eq!(workers[0].fingerprint, "fp2");

		let persisted = store.load();
		assert_eq!(persisted.len(), 1);
		assert_eq!(persisted[0].fingerprint, "fp2");
	}

	#[test]
	fn apply_forget_stale_in_memory_list_preserves_concurrent_addition() {
		let tmp = tempfile::tempdir().unwrap();
		let store = WorkerStore::in_config_dir(tmp.path());
		let w2 = sample_worker("w2", "2.2.2.2:2", "fp2");
		let w1 = sample_worker("w1", "1.1.1.1:1", "fp1");
		store.add(w2.clone()).unwrap();
		store.add(w1.clone()).unwrap();

		// GUI loads [w1, w2]
		let mut workers = store.load();
		assert_eq!(workers.len(), 2);
		assert_eq!(workers[0].fingerprint, "fp1");
		assert_eq!(workers[1].fingerprint, "fp2");

		// Another process adds w3 at the front
		let w3 = sample_worker("w3", "3.3.3.3:3", "fp3");
		store.add(w3.clone()).unwrap();

		// Forget w1 by fingerprint
		apply_forget(Some(&store), &mut workers, "fp1")
			.expect("apply_forget should succeed");

		// Must leave exactly w2 and w3 both in memory and on disk
		assert_eq!(workers.len(), 2);
		assert!(workers.iter().any(|w| w.fingerprint == "fp2"));
		assert!(workers.iter().any(|w| w.fingerprint == "fp3"));
		assert!(!workers.iter().any(|w| w.fingerprint == "fp1"));

		let on_disk = store.load();
		assert_eq!(on_disk.len(), 2);
		assert!(on_disk.iter().any(|w| w.fingerprint == "fp2"));
		assert!(on_disk.iter().any(|w| w.fingerprint == "fp3"));
		assert!(!on_disk.iter().any(|w| w.fingerprint == "fp1"));
	}

	#[test]
	fn apply_forget_none_store_removes_by_fingerprint() {
		let mut workers = vec![
			sample_worker("w1", "1.1.1.1:1", "fp1"),
			sample_worker("w2", "2.2.2.2:2", "fp2"),
			sample_worker("w3", "3.3.3.3:3", "fp3"),
		];

		apply_forget(None, &mut workers, "fp2")
			.expect("apply_forget should succeed");
		assert_eq!(workers.len(), 2);
		assert_eq!(workers[0].fingerprint, "fp1");
		assert_eq!(workers[1].fingerprint, "fp3");

		// Forgetting a non-existent fingerprint is a no-op
		apply_forget(None, &mut workers, "fp_unknown")
			.expect("apply_forget should succeed");
		assert_eq!(workers.len(), 2);
	}

	#[test]
	fn apply_forget_unwritable_store_returns_error() {
		let tmp = tempfile::tempdir().unwrap();
		let regular_file = tmp.path().join("a_file");
		std::fs::write(&regular_file, "blocking").unwrap();
		let bad_path = regular_file.join(WORKERS_FILE);
		let store = WorkerStore::new(bad_path);

		let mut workers = vec![sample_worker("w1", "1.1.1.1:1", "fp1")];

		let err = apply_forget(Some(&store), &mut workers, "fp1")
			.expect_err("expected error for unwritable path");
		assert!(
			err.contains("cannot save pairings"),
			"expected error containing 'cannot save pairings', got: {err}"
		);
	}

	#[test]
	fn worker_supplied_absolute_or_dotdot_rel_is_dropped() {
		// Table test for valid_rel
		let valid_cases = [
			("", true),
			("a", true),
			("a/b", true),
			("a/b/c", true),
			(".", false),
			("..", false),
			("/etc", false),
			("../x", false),
			("a/../../b", false),
			("a//b", false),
			("a/b/", false),
			("a\\b", false),
			("a\0b", false),
			("a/./b", false),
		];
		#[cfg(windows)]
		assert!(!valid_rel("C:/x"), "valid_rel(\"C:/x\")");
		for (input, expected) in valid_cases {
			assert_eq!(valid_rel(input), expected, "valid_rel({input:?})");
		}

		// Table test for remote_rel
		let session_root = Path::new("snip-remote://fp123/ws456");
		assert_eq!(remote_rel(session_root, session_root), Some(String::new()));
		assert_eq!(
			remote_rel(session_root, &session_root.join("alpha")),
			Some("alpha".to_string())
		);
		assert_eq!(
			remote_rel(session_root, &session_root.join("alpha").join("beta")),
			Some("alpha/beta".to_string())
		);
		assert_eq!(remote_rel(session_root, Path::new("/etc")), None);
		assert_eq!(
			remote_rel(session_root, &session_root.join("..").join("outside")),
			None
		);

		// Table test for join_rel
		assert_eq!(join_rel("", ""), "");
		assert_eq!(join_rel("a", ""), "a");
		assert_eq!(join_rel("", "b"), "b");
		assert_eq!(join_rel("a", "b"), "a/b");
		assert_eq!(join_rel("/a/", "/b/"), "a/b");
		assert_eq!(join_rel("a/b", "c/d"), "a/b/c/d");

		// Test remote_scan_entries
		let invalid_rels = vec![
			"/etc".to_string(),
			"../x".to_string(),
			"a/../../b".to_string(),
			"a//b".to_string(),
			"nul\0byte".to_string(),
		];
		let mut repos = Vec::new();
		let mut errors = Vec::new();
		let mut depth_limited = Vec::new();

		for inv in &invalid_rels {
			repos.push(snip_remote::ScannedRepo {
				rel: inv.clone(),
				utf8: true,
				name: "inv".into(),
				kind: snip_core::gitview::FoundKind::Main,
				summary: Ok(snip_core::gitview::StatusSummary {
					head: Some("main".into()),
					branch: Some("main".into()),
					changes: snip_core::workspace::ChangeCounts::default(),
				}),
			});
			errors.push((inv.clone(), "some error".into()));
			depth_limited.push(inv.clone());
		}

		// Non-UTF8 repo
		repos.push(snip_remote::ScannedRepo {
			rel: "valid_name".into(),
			utf8: false,
			name: "lossy".into(),
			kind: snip_core::gitview::FoundKind::Main,
			summary: Ok(snip_core::gitview::StatusSummary {
				head: None,
				branch: None,
				changes: snip_core::workspace::ChangeCounts::default(),
			}),
		});

		// Valid items: "" in repo, "." in error, "." in depth_limited, "a/b" in repo
		repos.push(snip_remote::ScannedRepo {
			rel: "".into(),
			utf8: true,
			name: "root_repo".into(),
			kind: snip_core::gitview::FoundKind::Main,
			summary: Ok(snip_core::gitview::StatusSummary {
				head: Some("main".into()),
				branch: Some("main".into()),
				changes: snip_core::workspace::ChangeCounts::default(),
			}),
		});
		repos.push(snip_remote::ScannedRepo {
			rel: "a/b".into(),
			utf8: true,
			name: "nested_repo".into(),
			kind: snip_core::gitview::FoundKind::Main,
			summary: Ok(snip_core::gitview::StatusSummary {
				head: Some("main".into()),
				branch: Some("main".into()),
				changes: snip_core::workspace::ChangeCounts::default(),
			}),
		});
		errors.push((".".into(), "error at root".into()));
		depth_limited.push(".".into());

		let scan = snip_remote::RepoScan {
			repos,
			errors,
			error_overflow: 7,
			depth_limited,
			depth_overflow: 3,
			status: ScanStatus::More,
		};

		let res = remote_scan_entries(session_root, scan);
		assert_eq!(res.status, ScanStatus::Incomplete);
		assert_eq!(res.error_overflow, 7);
		assert_eq!(res.depth_overflow, 3);

		// 2 valid repos
		assert_eq!(res.repos.len(), 2);
		assert_eq!(res.repos[0].root, session_root);
		assert_eq!(
			res.repos[0].summary.as_ref().unwrap().identity,
			key_identity(session_root, snip_core::gitview::FoundKind::Main)
		);
		assert_eq!(res.repos[1].root, session_root.join("a").join("b"));
		assert_eq!(
			res.repos[1].summary.as_ref().unwrap().identity,
			key_identity(
				&session_root.join("a").join("b"),
				snip_core::gitview::FoundKind::Main
			)
		);

		// 1 valid depth limit (from ".")
		assert_eq!(res.depth_limited.len(), 1);
		assert_eq!(res.depth_limited[0], session_root);

		// Errors should contain: 1 valid error (from ".") + 5 invalid repo errors + 1 non-utf8 repo error + 5 invalid error rels + 5 invalid depth rels = 17 errors
		let invalid_msg = "worker returned an invalid path";
		let invalid_count = res
			.errors
			.iter()
			.filter(|(p, msg)| p == session_root && msg == invalid_msg)
			.count();
		assert_eq!(
			invalid_count,
			invalid_rels.len() + 1 + invalid_rels.len() + invalid_rels.len()
		);

		// Ensure no entry's path escapes session_root
		for r in &res.repos {
			assert!(r.root.starts_with(session_root));
		}
		for (p, _) in &res.errors {
			assert!(p.starts_with(session_root));
		}
		for p in &res.depth_limited {
			assert!(p.starts_with(session_root));
		}
	}

	#[test]
	fn scan_incomplete_msg_for_various_statuses() {
		// Complete -> None
		assert_eq!(scan_incomplete_msg(ScanStatus::Complete, 3), None);

		// TimedOut -> Some(remote_scan_incomplete)
		let msg = scan_incomplete_msg(ScanStatus::TimedOut, 3).unwrap();
		assert_eq!(msg.key, "remote_scan_incomplete");
		assert_eq!(msg.args, vec!["3".to_string()]);

		// LimitReached -> Some(remote_scan_incomplete)
		let msg = scan_incomplete_msg(ScanStatus::LimitReached, 0).unwrap();
		assert_eq!(msg.key, "remote_scan_incomplete");
		assert_eq!(msg.args, vec!["0".to_string()]);

		// Incomplete -> Some(remote_scan_incomplete)
		let msg = scan_incomplete_msg(ScanStatus::Incomplete, 12).unwrap();
		assert_eq!(msg.key, "remote_scan_incomplete");
		assert_eq!(msg.args, vec!["12".to_string()]);

		// More -> Some(remote_scan_incomplete)
		let msg = scan_incomplete_msg(ScanStatus::More, 5).unwrap();
		assert_eq!(msg.key, "remote_scan_incomplete");
		assert_eq!(msg.args, vec!["5".to_string()]);
	}

	#[test]
	fn test_remote_worker_tip() {
		let worker = snip_remote::PairedWorker {
			name: "ubuntu-ui".into(),
			addr: "100.95.28.19:47899".into(),
			fingerprint:
				"5134d3b34076abcd1234567890abcdef5134d3b34076abcd1234567890abcdef"
					.into(),
		};
		let tip = remote_worker_tip(&worker);
		assert_eq!(tip, "ubuntu-ui\n100.95.28.19:47899 · 5134-D3B3-4076-ABCD");

		let bad_worker = snip_remote::PairedWorker {
			name: "test-node".into(),
			addr: "127.0.0.1:12345".into(),
			fingerprint: "invalid-hex-fingerprint".into(),
		};
		let bad_tip = remote_worker_tip(&bad_worker);
		assert_eq!(
			bad_tip,
			"test-node\n127.0.0.1:12345 · invalid-hex-fingerprint"
		);
	}
}
