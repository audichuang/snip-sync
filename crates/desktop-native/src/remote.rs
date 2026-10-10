//! Remote workspaces (snip-remote) inside the workbench.
//!
//! The hosts are the ones `~/.ssh/config` names. Picking one starts
//! `snip serve --stdio` there over ssh; a folder of that machine then
//! opens in place of the local workspace. Its Project tree, previews and
//! Git views are read through that worker.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use snip_core::browser::SourcePreview;
use snip_core::workspace::{RepoIdentity, RepoKind, RepoSummary, ScanStatus};
use snip_remote::{Client, RemoteError, RemoteHost, RemoteWorkspace};

use gpui::Context;

use crate::i18n::Msg;
use crate::reader::PreviewSource;
use crate::tree::{
	listed_tree_result, FileTreeNode, ListedChild, TreeIo, TreeIoResult,
};
use crate::{arm_cancel, lifecycle, RepoEntry, WorkbenchModel};

pub use snip_remote::device_name;

/// A remote folder opened before, listed under its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentRemote {
	pub host: String,
	pub path: String,
}

const RECENT_FILE: &str = "remote-recent.json";
const MAX_RECENT: usize = 10;

/// Remote folders opened before; none without a config folder (a test, or
/// an e2e run without `SNIP_CONFIG_DIR`).
pub fn load_recent() -> Vec<RecentRemote> {
	crate::recent::config_dir()
		.map(|dir| snip_remote::load_json(&dir.join(RECENT_FILE)))
		.unwrap_or_default()
}

/// Moves `host`'s `path` to the front, keeping [`MAX_RECENT`].
pub(crate) fn remember_recent(
	list: &mut Vec<RecentRemote>,
	host: &str,
	path: &str,
) {
	list.retain(|r| !(r.host == host && r.path == path));
	list.insert(
		0,
		RecentRemote {
			host: host.to_string(),
			path: path.to_string(),
		},
	);
	list.truncate(MAX_RECENT);
	if let Some(dir) = crate::recent::config_dir() {
		let _ = snip_remote::save_json(&dir.join(RECENT_FILE), list);
	}
}

/// Names the remote folder that was the last workspace open; absent when
/// the last one was local.
const LAST_FILE: &str = "remote-last.json";

fn last_file() -> Option<PathBuf> {
	Some(crate::recent::config_dir()?.join(LAST_FILE))
}

/// The remote folder to reconnect to on launch: the last workspace open,
/// when it was remote. None without a config folder.
pub fn load_last() -> Option<RecentRemote> {
	last_file().and_then(|f| load_last_from(&f))
}

/// Records the workspace just opened: `Some` for a remote folder, `None`
/// for a local one. A failed write only loses the reconnect.
pub(crate) fn remember_last(last: Option<&RecentRemote>) {
	if let Some(f) = last_file() {
		save_last_to(&f, last);
	}
}

fn load_last_from(f: &Path) -> Option<RecentRemote> {
	snip_remote::load_json(f)
}

fn save_last_to(f: &Path, last: Option<&RecentRemote>) {
	match last {
		Some(last) => {
			let _ = snip_remote::save_json(f, last);
		}
		None => {
			let _ = std::fs::remove_file(f);
		}
	}
}

/// The hosts of `~/.ssh/config`, reached through ssh.
pub fn load_hosts() -> Vec<RemoteHost> {
	snip_remote::ssh::config_hosts()
		.iter()
		.map(|h| RemoteHost::ssh(h))
		.collect()
}

/// The remote workspace open in place of a local one.
#[derive(Clone)]
pub struct RemoteSession {
	pub client: Arc<Client>,
	pub workspace: RemoteWorkspace,
	/// Never a real path: the tree's root key while the session is open.
	pub root: PathBuf,
}

impl RemoteSession {
	pub fn new(host: RemoteHost, workspace: RemoteWorkspace) -> Self {
		use std::hash::{Hash, Hasher};
		let mut h = std::collections::hash_map::DefaultHasher::new();
		workspace.id.hash(&mut h);
		let root = PathBuf::from(format!(
			"snip-remote://{}/{:016x}",
			host.name,
			h.finish()
		));
		Self {
			client: Arc::new(Client::new(host, device_name())),
			workspace,
			root,
		}
	}

	pub fn host(&self) -> &str {
		&self.client.host().name
	}

	pub fn label(&self) -> String {
		format!("{} ▸ {}", self.host(), self.workspace.name)
	}

	pub fn tip(&self) -> String {
		format!("{}:{}", self.host(), self.workspace.path)
	}
}

/// Formats a repo path for display: in a remote session, maps paths under session root to host:workspace_path[/rel].
pub fn display_path(session: Option<&RemoteSession>, root: &Path) -> String {
	if let Some(session) = session {
		if let Some(rel) = remote_rel(&session.root, root) {
			let base = session.workspace.path.trim_end_matches('/');
			let host = session.host();
			if rel.is_empty() {
				return format!("{host}:{base}");
			} else {
				return format!("{host}:{base}/{rel}");
			}
		}
		return session.tip();
	}
	root.display().to_string()
}

/// A folder of a host and the folders in it, for the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderListing {
	/// The worker's real path of the folder.
	pub path: String,
	pub folders: Vec<String>,
}

/// The folder a host's block in the menu shows. Clicking a subfolder
/// enters it; only "open this folder" opens it as the workspace.
pub struct Browse {
	pub host: usize,
	/// The worker's real path once listed, else as asked for (`~`, `…/..`).
	pub path: String,
	pub listing: Option<Result<FolderListing, String>>,
	/// A listing that lands with another number is for a folder no longer
	/// shown.
	seq: u64,
	/// Kept across hops on one host, so a click does not start ssh again.
	client: Option<Arc<Client>>,
}

impl std::fmt::Debug for Browse {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Browse")
			.field("host", &self.host)
			.field("path", &self.path)
			.field("listing", &self.listing)
			.field("seq", &self.seq)
			.finish()
	}
}

impl Browse {
	pub fn seq(&self) -> u64 {
		self.seq
	}
}

/// `name` inside `dir`, as the worker resolves it (`..` included).
pub(crate) fn child_path(dir: &str, name: &str) -> String {
	format!("{}/{name}", dir.trim_end_matches(['/', '\\']))
}

/// True for `/`, and for a drive root (`C:\`, `C:`) of a Windows worker:
/// no "up one level" there.
pub(crate) fn is_root(path: &str) -> bool {
	let p = path.trim_end_matches(['/', '\\']);
	let b = p.as_bytes();
	p.is_empty() || (b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

/// What master UI holds about remote work.
#[derive(Default)]
pub struct MasterState {
	pub hosts: Vec<RemoteHost>,
	pub recent: Vec<RecentRemote>,
	pub session: Option<RemoteSession>,
	pub busy: bool,
	/// Bumped when the workspace the user is looking at changes under an
	/// in-flight open: a local open, a close or a newer remote open. A
	/// result landing with an older seq is dropped, workspace and status
	/// bar untouched.
	pub open_seq: u64,
	pub message: Option<(bool, String)>,
	/// The host whose folders are listed, and the folder shown.
	pub browse: Option<Browse>,
	pub(crate) browse_seq: u64,
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

/// A worker's absolute `path` relative to its `root`, both spelled by the
/// worker. Compared as text so a Windows worker's `C:\…` paths resolve on
/// a Unix master too; the result uses "/". Only a Windows root (`C:\…`,
/// `\\server\…`) splits on `\`: on Unix it is a legal file name character.
pub(crate) fn worker_rel(root: &str, path: &Path) -> Option<String> {
	let seps: &[char] = if root.contains('\\') {
		&['/', '\\']
	} else {
		&['/']
	};
	let rest = path.to_str()?.strip_prefix(root.trim_end_matches(seps))?;
	if !rest.is_empty() && !rest.starts_with(seps) {
		return None;
	}
	let parts: Vec<&str> = rest.split(seps).filter(|p| !p.is_empty()).collect();
	if parts.iter().any(|p| *p == "." || *p == "..") {
		return None;
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
/// A fresh Expand/Retry fetches the worker's WHOLE listing once; a
/// continuation (LoadMore, entries still in `held`) is the ordinary local
/// admission path — nothing to fetch, no remote branch. The job's cancel
/// token ends a fetch against a host that stopped answering.
pub fn tree_io(
	client: &Client,
	workspace: &str,
	session_root: &Path,
	io: TreeIo,
	cancel: Option<&snip_core::gitrun::CancelToken>,
) -> TreeIoResult {
	if !io.held.is_empty() {
		return crate::tree::execute_tree_io(
			io,
			&snip_core::gitrun::CancelToken::new(),
		);
	}
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
				.list_dir(workspace, &req_path, cancel)
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
/// are served in this slice. The job's cancel token ends a read against a
/// host that stopped answering.
pub fn read_preview(
	client: &Client,
	workspace: &str,
	prefix: &str,
	path: &str,
	cancel: Option<&snip_core::gitrun::CancelToken>,
) -> Result<(SourcePreview, PreviewSource), String> {
	let full_path = join_rel(prefix, path);
	client
		.read(workspace, &full_path, cancel)
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

	/// Lists host `idx`'s home folder in the menu.
	pub fn browse_remote_host(&mut self, idx: usize, cx: &mut Context<Self>) {
		self.browse_remote_folder(idx, "~".to_string(), cx);
	}

	/// Lists `path` (absolute, or `~/…`) of host `idx` in the menu.
	pub fn browse_remote_folder(
		&mut self,
		idx: usize,
		path: String,
		cx: &mut Context<Self>,
	) {
		let Some(host) = self.remote.hosts.get(idx).cloned() else {
			return;
		};
		let client = self
			.remote
			.browse
			.as_ref()
			.filter(|b| b.host == idx)
			.and_then(|b| b.client.clone())
			.filter(|c| c.host().name == host.name)
			.unwrap_or_else(|| Arc::new(Client::new(host, device_name())));
		self.remote.browse_seq = self.remote.browse_seq.wrapping_add(1);
		let seq = self.remote.browse_seq;
		self.remote.browse = Some(Browse {
			host: idx,
			path: path.clone(),
			listing: None,
			seq,
			client: Some(client.clone()),
		});
		self.remote.message = None;
		self.pending_focus = Some(self.remote_path_input.read(cx).handle());
		cx.notify();
		let bg = cx.background_executor().clone();
		cx.spawn(async move |this, cx| {
			let result = bg
				.spawn(async move {
					let ws = client.open_workspace(&path)?;
					let (entries, _) = client.list_dir(&ws.id, "", None)?;
					let folders = entries
						.into_iter()
						.filter(|e| {
							e.directory && e.utf8 && !e.name.starts_with('.')
						})
						.map(|e| e.name)
						.collect();
					Ok(FolderListing {
						path: ws.id,
						folders,
					})
				})
				.await
				.map_err(describe);
			let _ = this.update(cx, |this, cx| {
				this.remote_listing_landed(seq, result, cx);
			});
		})
		.detach();
	}

	/// A folder listing lands; dropped when another folder or host is
	/// shown now.
	pub(crate) fn remote_listing_landed(
		&mut self,
		seq: u64,
		result: Result<FolderListing, String>,
		cx: &mut Context<Self>,
	) {
		let Some(browse) = self.remote.browse.as_mut().filter(|b| b.seq == seq)
		else {
			return;
		};
		let name = self
			.remote
			.hosts
			.get(browse.host)
			.map(|h| h.name.clone())
			.unwrap_or_default();
		match &result {
			Ok(listing) => {
				app_log!(
					"[APP:REMOTE_HOST_LISTED: host={name} folders={} path={}]",
					listing.folders.len(),
					listing.path
				);
				browse.path = listing.path.clone();
				let path = listing.path.clone();
				self.remote_path_input
					.update(cx, |field, cx| field.set_text(&path, cx));
			}
			Err(err) => {
				app_log!("[APP:REMOTE_HOST_FAILED: host={name} {err}]");
				browse.client = None;
			}
		}
		if let Some(browse) = self.remote.browse.as_mut() {
			browse.listing = Some(result);
		}
		cx.notify();
	}

	/// Enters subfolder `name` of the folder shown.
	pub fn enter_remote_folder(&mut self, name: &str, cx: &mut Context<Self>) {
		let Some(b) = &self.remote.browse else {
			return;
		};
		let (idx, path) = (b.host, child_path(&b.path, name));
		self.browse_remote_folder(idx, path, cx);
	}

	/// Lists the parent of the folder shown.
	pub fn remote_up(&mut self, cx: &mut Context<Self>) {
		let Some(b) = &self.remote.browse else {
			return;
		};
		if is_root(&b.path) {
			return;
		}
		let (idx, path) = (b.host, child_path(&b.path, ".."));
		self.browse_remote_folder(idx, path, cx);
	}

	/// Opens the folder shown as the workspace.
	pub fn open_remote_here(&mut self, cx: &mut Context<Self>) {
		let Some(b) = &self.remote.browse else {
			return;
		};
		let Some(Ok(listing)) = &b.listing else {
			return;
		};
		let (idx, path) = (b.host, listing.path.clone());
		self.open_remote_path(idx, path, cx);
	}

	/// Opens `path` (absolute, or `~/…`) of host `idx` as the workspace,
	/// after the usual close checks.
	pub fn open_remote_path(
		&mut self,
		idx: usize,
		path: String,
		cx: &mut Context<Self>,
	) {
		let Some(host) = self.remote.hosts.get(idx).cloned() else {
			return;
		};
		if self.remote.busy {
			return;
		}
		if path.trim().is_empty() {
			let text = crate::i18n::t("remote_path_missing", self.locale);
			return self.remote_note(false, text.into(), cx);
		}
		self.remote.busy = true;
		self.remote.message = None;
		// Any open that follows (local or remote) invalidates this one.
		self.remote.open_seq = self.remote.open_seq.wrapping_add(1);
		let open_seq = self.remote.open_seq;
		#[cfg(test)]
		let open_delay = self.e2e_remote_open_delay;
		cx.notify();
		let bg = cx.background_executor().clone();
		cx.spawn(async move |this, cx| {
			// Tests hold the open at the test clock so a user action can
			// land while it is in flight.
			#[cfg(test)]
			if let Some(delay) = open_delay {
				cx.background_executor().timer(delay).await;
			}
			let probe = host.clone();
			let result = bg
				.spawn(async move {
					Client::new(probe, device_name())
						.open_workspace(path.trim())
						.map_err(describe)
				})
				.await;
			let _ = this.update(cx, |this, cx| {
				this.remote.busy = false;
				if this.remote.open_seq != open_seq {
					// The user opened a local workspace, closed the
					// workspace or started another remote open meanwhile:
					// neither the workspace nor the status bar moves.
					app_log!(
						"[APP:REMOTE_OPEN_STALE: seq={open_seq} now={}]",
						this.remote.open_seq
					);
					return;
				}
				match result {
					Ok(ws) => {
						this.workspace_menu = false;
						this.request_user_close(
							lifecycle::Intent::OpenRemoteWorkspace(Box::new((
								host, ws,
							))),
							cx,
						);
					}
					Err(err) => {
						app_log!("[APP:REMOTE_OPEN_FAILED: {err}]");
						// Also on the status bar: a reconnect on launch runs
						// with the menu closed.
						this.set_status("remote_open_failed", [err.clone()]);
						this.remote_note(false, err, cx);
					}
				}
			});
		})
		.detach();
	}

	/// Opens the path typed into the menu, on the host being browsed.
	pub fn open_remote_typed(&mut self, cx: &mut Context<Self>) {
		let Some(idx) = self.remote.browse.as_ref().map(|b| b.host) else {
			return;
		};
		let path = self.remote_path_input.read(cx).text().to_string();
		self.open_remote_path(idx, path, cx);
	}

	/// Opens a recent remote folder: its host from `~/.ssh/config`.
	pub fn open_remote_recent(&mut self, n: usize, cx: &mut Context<Self>) {
		let Some(recent) = self.remote.recent.get(n).cloned() else {
			return;
		};
		self.open_remote_folder(recent, cx);
	}

	/// Launched without `--workspace` after a remote workspace was the last
	/// one open: reconnects to it in the background. A failure leaves the
	/// app with no workspace and says why.
	pub fn reopen_last_remote(
		&mut self,
		last: RecentRemote,
		cx: &mut Context<Self>,
	) {
		app_log!("[APP:REMOTE_REOPEN: host={} path={}]", last.host, last.path);
		self.set_status(
			"remote_reconnecting",
			[format!("{} ▸ {}", last.host, last.path)],
		);
		self.open_remote_folder(last, cx);
	}

	/// Opens `folder` on its host from `~/.ssh/config`.
	fn open_remote_folder(
		&mut self,
		folder: RecentRemote,
		cx: &mut Context<Self>,
	) {
		match self.remote.hosts.iter().position(|h| h.name == folder.host) {
			Some(idx) => self.open_remote_path(idx, folder.path, cx),
			None => {
				let text = crate::i18n::tf(
					"remote_host_missing",
					self.locale,
					&[&folder.host],
				);
				self.set_status_msg(Msg::new(
					"remote_open_failed",
					[text.clone()],
				));
				self.remote_note(false, text, cx);
			}
		}
	}

	pub(crate) fn finish_open_remote(
		&mut self,
		host: RemoteHost,
		workspace: RemoteWorkspace,
		cx: &mut Context<Self>,
	) {
		self.release_workspace_state(cx);
		remember_recent(&mut self.remote.recent, &host.name, &workspace.id);
		remember_last(self.remote.recent.first());
		let session = RemoteSession::new(host, workspace);
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
		self.set_status_msg(Msg::new("remote_opened", [label]));
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
							model.status = msg.clone();
							model.remote.scan_error = Some(msg);
							model.discovery_status =
								Some(ScanStatus::Incomplete);
							model.is_loading = false;
							model.refresh_reload = false;
							if wipe {
								model.repos.clear();
								model.pinned_repo = None;
								model.selected_repo_idx = None;
								model.release_repo_state();
								model.sync_change_slots();
							}
							model.ensure_ws_tree(cx);
							if model
								.ws_tree
								.as_ref()
								.is_some_and(|t| !t.is_loaded)
							{
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

#[cfg(test)]
mod tests {
	use super::*;

	/// Worker paths resolve by the worker's spelling, on any master OS.
	#[test]
	fn worker_rel_reads_unix_and_windows_worker_paths() {
		let rel = |root: &str, path: &str| worker_rel(root, Path::new(path));
		assert_eq!(
			rel("/srv/ws", "/srv/ws/a/b.txt").as_deref(),
			Some("a/b.txt")
		);
		assert_eq!(rel("/srv/ws/", "/srv/ws/a.txt").as_deref(), Some("a.txt"));
		assert_eq!(
			rel(r"C:\Users\me\ws", r"C:\Users\me\ws\src\x.rs").as_deref(),
			Some("src/x.rs")
		);
		// On a Unix worker `\` is part of the name, not a folder.
		assert_eq!(
			rel("/srv/ws", r"/srv/ws/a\b.txt").as_deref(),
			Some(r"a\b.txt")
		);
		assert_eq!(rel("/srv/ws", "/srv/ws2/a.txt"), None);
		assert_eq!(rel("/srv/ws", "/srv/ws/../etc/passwd"), None);
		assert_eq!(rel("/srv/ws", "/other/a.txt"), None);
	}

	#[test]
	fn recent_folders_move_to_the_front_and_stay_bounded() {
		let mut list = Vec::new();
		for n in 0..12 {
			remember_recent(&mut list, "h", &format!("/p{n}"));
		}
		assert_eq!(list.len(), MAX_RECENT);
		assert_eq!(list[0].path, "/p11");
		remember_recent(&mut list, "h", "/p5");
		assert_eq!(list[0].path, "/p5");
		assert_eq!(list.iter().filter(|r| r.path == "/p5").count(), 1);
		remember_recent(&mut list, "other", "/p5");
		assert_eq!(list[0].host, "other");
		assert_eq!(list[1].host, "h");
	}

	#[test]
	fn last_remote_workspace_round_trips_and_a_local_one_clears_it() {
		assert_eq!(load_last(), None, "no config folder under cfg(test)");
		let tmp = tempfile::tempdir().unwrap();
		let f = tmp.path().join("cfg").join(LAST_FILE);
		assert_eq!(load_last_from(&f), None);
		let last = RecentRemote {
			host: "macmini".into(),
			path: "/Users/x/ck/cat".into(),
		};
		save_last_to(&f, Some(&last));
		assert_eq!(load_last_from(&f), Some(last));
		save_last_to(&f, None);
		assert!(!f.exists());
		assert_eq!(load_last_from(&f), None);
		save_last_to(&f, None);
	}

	#[test]
	fn folder_browser_paths_and_roots() {
		assert_eq!(child_path("/home/u", "ck"), "/home/u/ck");
		assert_eq!(child_path("/home/u/", ".."), "/home/u/..");
		assert_eq!(child_path("/", "etc"), "/etc");
		assert_eq!(child_path(r"C:\Users\u", "ck"), r"C:\Users\u/ck");
		assert_eq!(child_path(r"C:\", "Users"), "C:/Users");
		for root in ["/", "", r"C:\", "C:", "c:/"] {
			assert!(is_root(root), "{root:?}");
		}
		for not in ["/home", "~", r"C:\Users", "CC:", "1:"] {
			assert!(!is_root(not), "{not:?}");
		}
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
}
