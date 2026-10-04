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

/// A host's home folder and the folders in it, for the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostListing {
	pub home: String,
	pub folders: Vec<String>,
}

/// What master UI holds about remote work.
#[derive(Default)]
pub struct MasterState {
	pub hosts: Vec<RemoteHost>,
	pub recent: Vec<RecentRemote>,
	pub session: Option<RemoteSession>,
	pub busy: bool,
	pub message: Option<(bool, String)>,
	/// Host whose folders are listed, and the listing once it lands.
	pub browse: Option<(usize, Option<Result<HostListing, String>>)>,
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
		let Some(host) = self.remote.hosts.get(idx).cloned() else {
			return;
		};
		self.remote.browse = Some((idx, None));
		self.remote.message = None;
		self.pending_focus = Some(self.remote_path_input.read(cx).handle());
		cx.notify();
		let bg = cx.background_executor().clone();
		let name = host.name.clone();
		cx.spawn(async move |this, cx| {
			let result = bg
				.spawn(async move {
					let client = Client::new(host, device_name());
					let home = client.open_workspace("~")?;
					let (entries, _) = client.list_dir(&home.id, "")?;
					let folders = entries
						.into_iter()
						.filter(|e| {
							e.directory && e.utf8 && !e.name.starts_with('.')
						})
						.map(|e| e.name)
						.collect();
					Ok(HostListing {
						home: home.id,
						folders,
					})
				})
				.await
				.map_err(describe);
			let _ = this.update(cx, |this, cx| {
				// Another host is shown now.
				let current = this.remote.browse.as_ref().map(|(i, _)| *i);
				let same = current
					.and_then(|i| this.remote.hosts.get(i))
					.is_some_and(|h| h.name == name);
				if !same {
					return;
				}
				match &result {
					Ok(listing) => app_log!(
						"[APP:REMOTE_HOST_LISTED: host={name} folders={}]",
						listing.folders.len()
					),
					Err(err) => {
						app_log!("[APP:REMOTE_HOST_FAILED: host={name} {err}]")
					}
				}
				this.remote.browse = current.map(|i| (i, Some(result)));
				cx.notify();
			});
		})
		.detach();
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
		cx.notify();
		let bg = cx.background_executor().clone();
		cx.spawn(async move |this, cx| {
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
						this.remote_note(false, err, cx);
					}
				}
			});
		})
		.detach();
	}

	/// Opens the path typed into the menu, on the host being browsed.
	pub fn open_remote_typed(&mut self, cx: &mut Context<Self>) {
		let Some((idx, _)) = self.remote.browse else {
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
		match self.remote.hosts.iter().position(|h| h.name == recent.host) {
			Some(idx) => self.open_remote_path(idx, recent.path, cx),
			None => {
				let text = crate::i18n::tf(
					"remote_host_missing",
					self.locale,
					&[&recent.host],
				);
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
