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
use snip_remote::{
	Client, Identity, PairedWorker, RemoteError, RemoteWorkspace, Worker,
	WorkerOptions, WorkerStore, TRUSTED_FILE, WORKERS_FILE,
};

use gpui::Context;

use crate::i18n::Msg;
use crate::reader::PreviewSource;
use crate::tree::{
	listed_tree_result, FileTreeNode, ListedChild, NodeKey, TreeCommand,
	TreeEffect, TreeIo, TreeIoResult,
};
use crate::{lifecycle, WorkbenchModel};

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
	);
	eprintln!("Error: cannot start the worker: {err}");
	std::process::exit(1);
}

// ───────────────────────── master ─────────────────────────

pub fn load_workers() -> Vec<PairedWorker> {
	worker_store().map(|s| s.load()).unwrap_or_default()
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
}

/// Blocking: the remote counterpart of [`crate::tree::execute_tree_io`].
pub fn tree_io(client: &Client, workspace: &str, io: TreeIo) -> TreeIoResult {
	let listed = match io.key.utf8_rel() {
		Some(rel) => client
			.list_dir(workspace, &rel)
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
			.map_err(describe),
		None => Err("non-UTF-8 folder name".to_string()),
	};
	listed_tree_result(io, listed)
}

/// Blocking: a file preview read on the worker. Only working-tree files
/// are served in this slice.
pub fn read_preview(
	client: &Client,
	workspace: &str,
	path: &str,
) -> Result<(SourcePreview, PreviewSource), String> {
	client
		.read(workspace, path)
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

// ───────────────────────── model ─────────────────────────

impl WorkbenchModel {
	/// The worker client and workspace id while a remote workspace is open.
	pub(crate) fn remote_target(&self) -> Option<(Arc<Client>, String)> {
		self.remote
			.session
			.as_ref()
			.map(|s| (s.client.clone(), s.workspace.id.clone()))
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
		if let Some(store) = worker_store() {
			if let Err(err) = store.forget(&fp) {
				self.remote_note(
					false,
					format!("cannot save pairings: {err}"),
					cx,
				);
			}
			self.remote.workers = load_workers();
		} else {
			self.remote.workers.remove(idx);
		}
		self.remote.browse = None;
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
		self.resume_remote_tree(cx);
	}

	/// Refresh: the remote tree is read again from its root.
	pub(crate) fn reload_remote_tree(&mut self, cx: &mut Context<Self>) {
		let Some(root) = self.remote.session.as_ref().map(|s| s.root.clone())
		else {
			return;
		};
		self.ws_tree = Some(FileTreeNode::unloaded_root(&root));
		self.resume_remote_tree(cx);
	}

	fn resume_remote_tree(&mut self, cx: &mut Context<Self>) {
		let Some(tree) = self.ws_tree.as_mut() else {
			return;
		};
		if let TreeEffect::Io(io) =
			tree.start(TreeCommand::Expand(NodeKey::root()))
		{
			self.submit_tree_io(io, cx);
		}
		cx.notify();
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
}
