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
use crate::reason::reason_msg;
use crate::repo_set::RepoOp;
use crate::tree::{
	listed_tree_result, FileTreeNode, ListedChild, TreeIo, TreeIoResult,
};
use crate::{arm_cancel, RepoEntry, WorkbenchModel};

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
	// Other workspace tabs saved since this copy was read.
	if crate::recent::config_dir().is_some() {
		*list = load_recent();
	}
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
	#[cfg(test)]
	if let Some(hosts) = TEST_HOSTS.with(|hosts| hosts.borrow().clone()) {
		return hosts;
	}
	snip_remote::ssh::config_hosts()
		.iter()
		.map(|h| RemoteHost::ssh(h))
		.collect()
}

/// The `REMOTE_HOST_FAILED` e2e line. Drivers read it as developers did:
/// the English wording, never a dictionary key a rename could change.
fn host_failed_line(name: &str, err: &Msg) -> String {
	format!(
		"[APP:REMOTE_HOST_FAILED: host={name} {}]",
		err.render(crate::i18n::Locale::En)
	)
}

#[cfg(test)]
thread_local! {
	static TEST_HOSTS: std::cell::RefCell<Option<Vec<RemoteHost>>> = const { std::cell::RefCell::new(None) };
	static TEST_RESTORE_DELAY: std::cell::RefCell<Option<(String, std::time::Duration)>> = const { std::cell::RefCell::new(None) };
}

/// Test-scoped host discovery without changing the user's SSH config.
#[cfg(test)]
pub(crate) fn with_test_hosts<R>(
	hosts: Vec<RemoteHost>,
	f: impl FnOnce() -> R,
) -> R {
	struct Reset(Option<Vec<RemoteHost>>);
	impl Drop for Reset {
		fn drop(&mut self) {
			TEST_HOSTS.with(|hosts| *hosts.borrow_mut() = self.0.take());
		}
	}
	let _reset = Reset(TEST_HOSTS.with(|slot| slot.replace(Some(hosts))));
	f()
}

/// Holds only a selected saved tab at the existing async pre-probe seam.
#[cfg(test)]
pub(crate) fn with_test_restore_delay<R>(
	path: &str,
	delay: std::time::Duration,
	f: impl FnOnce() -> R,
) -> R {
	struct Reset(Option<(String, std::time::Duration)>);
	impl Drop for Reset {
		fn drop(&mut self) {
			TEST_RESTORE_DELAY.with(|slot| *slot.borrow_mut() = self.0.take());
		}
	}
	let _reset = Reset(
		TEST_RESTORE_DELAY
			.with(|slot| slot.replace(Some((path.into(), delay)))),
	);
	f()
}

/// A validated remote folder and its live client awaiting workspace handoff.
#[derive(Clone)]
pub struct OpenedRemote {
	pub client: Arc<Client>,
	pub workspace: RemoteWorkspace,
}

impl std::fmt::Debug for OpenedRemote {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("OpenedRemote")
			.field("host", self.client.host())
			.field("workspace", &self.workspace)
			.finish()
	}
}

impl PartialEq for OpenedRemote {
	fn eq(&self, other: &Self) -> bool {
		self.client.host() == other.client.host()
			&& self.workspace == other.workspace
	}
}
impl Eq for OpenedRemote {}

/// The successfully probed client is carried into the workspace session.
#[derive(Clone)]
pub struct RemoteSession {
	pub client: Arc<Client>,
	pub workspace: RemoteWorkspace,
	/// Never a real path: the tree's root key while the session is open.
	pub root: PathBuf,
}

impl RemoteSession {
	#[cfg(test)]
	pub fn new(host: RemoteHost, workspace: RemoteWorkspace) -> Self {
		Self::from_opened(OpenedRemote {
			client: Arc::new(Client::new(host, device_name())),
			workspace,
		})
	}

	fn from_opened(opened: OpenedRemote) -> Self {
		let OpenedRemote { client, workspace } = opened;
		use std::hash::{Hash, Hasher};
		let mut h = std::collections::hash_map::DefaultHasher::new();
		workspace.id.hash(&mut h);
		let root = PathBuf::from(format!(
			"snip-remote://{}/{:016x}",
			client.host().name,
			h.finish()
		));
		Self {
			client,
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
	pub listing: Option<Result<FolderListing, Msg>>,
	/// A listing that lands with another number is for a folder no longer
	/// shown.
	seq: u64,
	/// Kept across hops on one host, so a click does not start ssh again.
	client: Option<Arc<Client>>,
	/// Ends this listing's connect and calls once another listing, a
	/// close or a quit replaces it.
	cancel: snip_core::gitrun::CancelToken,
	/// What this code last wrote into the path field; a listing that lands
	/// overwrites the field only while it still holds this (the user did not
	/// type meanwhile).
	prefill: String,
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
	/// The Client this browse keeps, for tests.
	#[cfg(test)]
	pub(crate) fn client_for_tests(&self) -> Option<Arc<Client>> {
		self.client.clone()
	}

	pub fn seq(&self) -> u64 {
		self.seq
	}

	/// Whether this browse keeps its Client across hops (M2 of
	/// `invariants`): the field is private, and the test accessor above is
	/// not compiled into the product.
	pub(crate) fn has_client(&self) -> bool {
		self.client.is_some()
	}

	/// A browse of host `host` that listed nothing yet, for the breakers of
	/// `invariants`.
	#[cfg(test)]
	pub(crate) fn for_tests(host: usize) -> Self {
		Self {
			host,
			path: String::new(),
			listing: None,
			seq: 0,
			client: None,
			cancel: snip_core::gitrun::CancelToken::new(),
			prefill: String::new(),
		}
	}

	/// What this code last wrote into the path field (M3 of `invariants`).
	#[cfg(test)]
	pub(crate) fn prefill(&self) -> &str {
		&self.prefill
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
	pub message: Option<(bool, Msg)>,
	/// The host whose folders are listed, and the folder shown.
	pub browse: Option<Browse>,
	pub(crate) browse_seq: u64,
	pub scan_error: Option<Msg>,
	/// A scan still in flight or waiting to retry. Kept across a cancelled
	/// drain so an initial scan with no selected repository can resume.
	pub(crate) scan_recovery: Option<ScanRecovery>,
	pub(crate) restore_priority: Option<RestorePriority>,
	/// The remote folder this tab was reopened for at launch, until it
	/// opens: the tab shows and saves it meanwhile, and after a failure.
	pub restoring: Option<Restoring>,
}

impl MasterState {
	/// Ends the menu's host browse: its listing is cancelled and its Client
	/// (and that Client's ssh connections) dropped (#187).
	pub(crate) fn end_browse(&mut self) {
		if let Some(b) = self.browse.take() {
			b.cancel.cancel();
		}
	}
}

/// Only the first probe of saved tabs on the active tab's host is ordered.
/// Other hosts, later retries and established-session reads are independent.
pub(crate) enum RestorePriority {
	First(Arc<std::sync::atomic::AtomicBool>),
	After(Arc<std::sync::atomic::AtomicBool>),
}

pub(crate) const RESTORE_PRIORITY_LIMIT: std::time::Duration =
	std::time::Duration::from_secs(30);

struct ReleaseRestorePriority(Arc<std::sync::atomic::AtomicBool>);

impl Drop for ReleaseRestorePriority {
	fn drop(&mut self) {
		self.0.store(true, std::sync::atomic::Ordering::SeqCst);
	}
}

#[derive(Clone, Debug)]
pub(crate) struct ScanRecovery {
	pub under: Option<PathBuf>,
	pub wipe: bool,
	pub retries: u8,
}

/// Three additional attempts, then leave the visible error for Refresh.
pub(crate) fn scan_retry_delay(retries: u8) -> Option<std::time::Duration> {
	match retries {
		0..=2 => Some(std::time::Duration::from_secs(1 << retries)),
		_ => None,
	}
}

/// A remote tab of the last session, being reconnected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restoring {
	pub folder: RecentRemote,
	/// Why the last attempt failed; the tab shows it until one succeeds.
	pub failed: Option<Msg>,
	/// The `open_seq` of the attempt in flight.
	pub(crate) open_seq: Option<u64>,
	/// Attempts made; the next one waits [`retry_delay`] of it.
	pub(crate) attempts: u32,
	/// Tells a retry timer of an earlier restore of this tab from this one.
	pub(crate) id: u64,
}

/// How long a failed reconnect waits before the next try: 5 s, doubled
/// each time, at most a minute.
pub fn retry_delay(attempts: u32) -> std::time::Duration {
	let doublings = attempts.saturating_sub(1).min(4);
	std::time::Duration::from_secs((5u64 << doublings).min(60))
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
	pub errors: Vec<(PathBuf, snip_core::reason::Failure)>,
	pub depth_limited: Vec<PathBuf>,
	pub error_overflow: usize,
	pub depth_overflow: usize,
	pub status: ScanStatus,
}

/// A worker's scan row. A worker that sent no reason (an old one) is worded
/// by its code alone: the row's text is the worker's English and kept only
/// for the log.
fn scan_failure(
	text: String,
	reason: Option<snip_core::reason::Reason>,
) -> snip_core::reason::Failure {
	snip_core::reason::Failure::new(
		text,
		reason.unwrap_or(snip_core::reason::Reason::Refused {
			code: snip_remote::ErrorCode::Io,
		}),
	)
}

/// A path the worker sent that cannot be a path under the share.
fn invalid_path() -> snip_core::reason::Failure {
	snip_core::reason::Failure::new(
		"worker returned an invalid path",
		snip_core::reason::Reason::Protocol,
	)
}

/// Translates and bounds a worker RepoScan onto the fake session root, dropping invalid paths.
pub(crate) fn remote_scan_entries(
	session_root: &Path,
	scan: snip_remote::RepoScan,
) -> RemoteScanEntries {
	let mut repos = Vec::with_capacity(scan.repos.len());
	let mut errors = Vec::with_capacity(scan.errors.len());
	let mut reasons = scan.error_reasons.into_iter();
	let mut depth_limited = Vec::with_capacity(scan.depth_limited.len());

	for scanned in scan.repos {
		if !scanned.utf8 || !valid_rel(&scanned.rel) {
			errors.push((session_root.to_path_buf(), invalid_path()));
			continue;
		}
		let mut root = session_root.to_path_buf();
		if !scanned.rel.is_empty() {
			for part in scanned.rel.split('/') {
				root.push(part);
			}
		}
		let kind = scanned.kind;
		let reason = scanned.reason;
		let summary = scanned
			.summary
			.map(|s| RepoSummary {
				identity: key_identity(&root, kind),
				head: s.head,
				branch: s.branch,
				changes: s.changes,
			})
			.map_err(|text| scan_failure(text, reason));
		repos.push(RepoEntry {
			root,
			name: scanned.name,
			kind: kind.into(),
			identity: None,
			summary,
		});
	}

	for (raw_rel, msg) in scan.errors {
		// Index-aligned with the rows; an old worker sends none.
		let reason = reasons.next().flatten();
		let rel = if raw_rel == "." { "" } else { raw_rel.as_str() };
		if !valid_rel(rel) {
			errors.push((session_root.to_path_buf(), invalid_path()));
			continue;
		}
		let mut path = session_root.to_path_buf();
		if !rel.is_empty() {
			for part in rel.split('/') {
				path.push(part);
			}
		}
		errors.push((path, scan_failure(msg, reason)));
	}

	for raw_rel in scan.depth_limited {
		let rel = if raw_rel == "." { "" } else { raw_rel.as_str() };
		if !valid_rel(rel) {
			errors.push((session_root.to_path_buf(), invalid_path()));
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
				Err(Msg::new("err_not_under_workspace", [])),
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
				.map_err(describe_msg)
		}
		None => Err(Msg::new("err_folder_not_utf8", [])),
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
) -> Result<(SourcePreview, PreviewSource), crate::PreviewErr> {
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
		.map_err(|e| match e {
			RemoteError::Refused {
				code: snip_remote::ErrorCode::TooLarge,
				..
			} => crate::PreviewErr::TooLarge,
			other => crate::PreviewErr::Failed(describe_msg(other)),
		})
}

/// A remote failure as a message: worded when it is drawn, in the language
/// then in force, so a result that outlives a switch of language never keeps
/// the old one. The one translation point is [`reason_msg`]; the worker's own
/// message for a refusal is OS or protocol English and is never shown.
pub fn describe_msg(err: RemoteError) -> Msg {
	reason_msg(&err.reason())
}

pub use crate::reason::describe_msg_at;

/// A Git view's error as a message.
pub fn git_error_msg(err: snip_core::gitsrc::GitError) -> Msg {
	reason_msg(&err.reason())
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

	fn remote_note(&mut self, ok: bool, text: Msg, cx: &mut Context<Self>) {
		self.remote.message = Some((ok, text));
		cx.notify();
	}

	/// A failed open's Client goes back to the menu's browse of the same host
	/// when that browse holds none, so the next click in the menu does not
	/// start ssh again (#198). A refused open leaves the pooled connection
	/// warm; a broken one left the pool empty and the next call reconnects.
	fn return_browse_client(&mut self, idx: usize, client: Arc<Client>) {
		let same_host = self
			.remote
			.hosts
			.get(idx)
			.is_some_and(|h| h.name == client.host().name);
		if !same_host {
			return;
		}
		if let Some(b) = self
			.remote
			.browse
			.as_mut()
			.filter(|b| b.host == idx && b.client.is_none())
		{
			b.client = Some(client);
		}
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
		if self.lifecycle.is_draining() {
			return;
		}
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
		let cancel = snip_core::gitrun::CancelToken::new();
		let previous = self.remote.browse.replace(Browse {
			host: idx,
			path: path.clone(),
			listing: None,
			seq,
			client: Some(client.clone()),
			cancel: cancel.clone(),
			prefill: path.clone(),
		});
		if let Some(previous) = previous {
			// Its listing would be dropped anyway (another seq).
			previous.cancel.cancel();
		}
		// The field shows the requested folder at once, never the previous
		// host's path (#190).
		self.remote_path_input
			.update(cx, |f, cx| f.set_text(&path, cx));
		self.remote.message = None;
		self.pending_focus = Some(self.remote_path_input.read(cx).handle());
		cx.notify();
		let bg = cx.background_executor().clone();
		let this = cx.weak_entity();
		let mut async_app = cx.to_async();
		let token = cancel.clone();
		#[cfg(test)]
		let open_delay = self.e2e_remote_open_delay;
		// Like an open, the listing's ssh belongs to this tab: a close or
		// quit cancels it and waits for the connection to go.
		self.spawn_owned(cx, Some(cancel), async move {
			#[cfg(test)]
			if let Some(delay) = open_delay {
				hold_open(&mut async_app, delay, &token).await;
			}
			let result = bg
				.spawn(async move {
					let cancel = Some(&token);
					let ws = client.open_workspace_with(&path, cancel)?;
					let (entries, _) =
						client.list_folders(&ws.id, "", cancel)?;
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
				.map_err(describe_msg);
			let _ = this.update(&mut async_app, |this, cx| {
				this.remote_listing_landed(seq, result, cx);
			});
		});
	}

	/// A folder listing lands; dropped when another folder or host is
	/// shown now.
	pub(crate) fn remote_listing_landed(
		&mut self,
		seq: u64,
		result: Result<FolderListing, Msg>,
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
				// Overwrite the field only while it still holds what this code
				// wrote; a user who typed meanwhile keeps the text (#190).
				let untouched =
					self.remote_path_input.read(cx).text() == browse.prefill;
				if untouched {
					browse.prefill = path.clone();
					self.remote_path_input
						.update(cx, |field, cx| field.set_text(&path, cx));
				}
			}
			Err(err) => {
				app_log!("{}", host_failed_line(&name, err));
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
		if self.remote.busy || self.lifecycle.is_draining() {
			return;
		}
		if path.trim().is_empty() {
			return self.remote_note(
				false,
				Msg::new("remote_path_missing", []),
				cx,
			);
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
		let this = cx.weak_entity();
		let mut async_app = cx.to_async();
		// The tab owns the open until its connection is dropped (SSH kill +
		// wait). A close or quit cancels it, which kills an ssh that is
		// still connecting, so the drain does not wait out ssh's connect
		// timeout and the hello's.
		let cancel = snip_core::gitrun::CancelToken::new();
		let token = cancel.clone();
		// Construct the release guard before spawning: dropping an unpolled
		// active open must also release its background restores.
		let (priority_wait, priority_release) =
			match self.remote.restore_priority.take() {
				Some(RestorePriority::First(gate)) => {
					(None, Some(ReleaseRestorePriority(gate)))
				}
				Some(RestorePriority::After(gate)) => (Some(gate), None),
				None => (None, None),
			};
		// The workspace opened from the menu keeps the browse's warm
		// connection instead of starting another ssh.
		let reuse = self
			.remote
			.browse
			.as_mut()
			.filter(|b| b.host == idx)
			.and_then(|b| b.client.take())
			.filter(|c| c.host().name == host.name);
		self.spawn_owned(cx, Some(cancel), async move {
			let _priority_release = priority_release;
			if let Some(gate) = priority_wait {
				let executor = async_app.background_executor().clone();
				let deadline = executor.now() + RESTORE_PRIORITY_LIMIT;
				while !gate.load(std::sync::atomic::Ordering::SeqCst)
					&& !token.is_cancelled()
				{
					let remaining =
						deadline.saturating_duration_since(executor.now());
					if remaining.is_zero() {
						break;
					}
					executor
						.timer(
							remaining.min(std::time::Duration::from_millis(50)),
						)
						.await;
				}
			}
			// Tests hold the open at the test clock so a user action can
			// land while it is in flight; a cancel ends the hold as it
			// ends a real connect.
			#[cfg(test)]
			if let Some(delay) = open_delay {
				hold_open(&mut async_app, delay, &token).await;
			}
			let shown_path = path.trim().to_string();
			let result = bg
				.spawn(async move {
					let client = reuse.unwrap_or_else(|| {
						Arc::new(Client::new(host, device_name()))
					});
					match client.open_workspace_with(path.trim(), Some(&token))
					{
						Ok(workspace) => Ok(OpenedRemote { client, workspace }),
						Err(err) => Err((client, err)),
					}
				})
				.await;
			let _ = this.update(&mut async_app, |this, cx| {
				this.remote.busy = false;
				if this.remote.open_seq != open_seq {
					// The user opened a local workspace, closed the
					// workspace or started another remote open meanwhile:
					// neither the workspace nor the status bar moves.
					app_log!(
						"[APP:REMOTE_OPEN_STALE: seq={open_seq} now={}]",
						this.remote.open_seq
					);
					this.resume_remote_restore(cx);
					return;
				}
				let restore = this
					.remote
					.restoring
					.as_ref()
					.is_some_and(|r| r.open_seq == Some(open_seq));
				match result {
					Ok(opened) => {
						this.drop_overlays(crate::overlay::DropScope::Kind(
							crate::overlay::OverlayKind::WorkspaceMenu,
						));
						let target =
							crate::tabs::OpenTarget::Remote(Box::new(opened));
						if restore {
							// The tab already stands for this folder.
							this.open_in_place(target, cx);
						} else {
							this.route_open(target, cx);
						}
					}
					Err((client, err)) => {
						this.return_browse_client(idx, client);
						let retryable = err.is_restore_retryable();
						// The reason stays a message: worded when drawn, in
						// the language then in force (#206 #208).
						// The log line is the engine's text, not the UI's.
						app_log!("[APP:REMOTE_OPEN_FAILED: {err}]");
						let err = describe_msg_at(err, &shown_path);
						// Also on the status bar: a reconnect on launch runs
						// with the menu closed.
						this.set_status_msg(Msg::with_msg_arg(
							"remote_open_failed",
							[String::new()],
							0,
							err.clone(),
						));
						this.remote_note(false, err.clone(), cx);
						if restore {
							this.restore_failed(err, retryable, cx);
						}
					}
				}
			});
		});
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

	/// A remote tab of the last session: shown and saved as that folder
	/// at once, connected in the background. A failure keeps the tab, says
	/// why and tries again later.
	#[cfg(test)]
	pub fn restore_remote(
		&mut self,
		folder: RecentRemote,
		cx: &mut Context<Self>,
	) {
		self.prepare_remote_restore(folder, cx);
		self.start_remote_restore(None, cx);
	}

	/// Establish identity before starting any saved tab's background open.
	pub(crate) fn prepare_remote_restore(
		&mut self,
		folder: RecentRemote,
		cx: &mut Context<Self>,
	) {
		#[cfg(test)]
		if let Some(delay) = TEST_RESTORE_DELAY.with(|slot| {
			slot.borrow()
				.as_ref()
				.filter(|(path, _)| *path == folder.path)
				.map(|(_, delay)| *delay)
		}) {
			self.e2e_remote_open_delay = Some(delay);
		}
		app_log!(
			"[APP:REMOTE_REOPEN: host={} path={}]",
			folder.host,
			folder.path
		);
		self.set_status(
			"remote_reconnecting",
			[format!("{} ▸ {}", folder.host, folder.path)],
		);
		static IDS: std::sync::atomic::AtomicU64 =
			std::sync::atomic::AtomicU64::new(0);
		self.remote.restoring = Some(Restoring {
			folder,
			failed: None,
			open_seq: None,
			attempts: 0,
			id: IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
		});
		self.remote.restore_priority = None;
		cx.notify();
	}

	pub(crate) fn start_remote_restore(
		&mut self,
		priority: Option<RestorePriority>,
		cx: &mut Context<Self>,
	) {
		self.remote.restore_priority = priority;
		self.attempt_restore(cx);
	}

	/// Refresh on a restored tab whose reconnect failed: tries it now instead
	/// of when its backoff timer fires. Returns false when there is no failed
	/// restore to retry (none, still in flight, or a drain is under way).
	pub(crate) fn refresh_failed_restore(
		&mut self,
		cx: &mut Context<Self>,
	) -> bool {
		let Some(r) = self.remote.restoring.as_ref() else {
			return false;
		};
		if r.failed.is_none()
			|| r.open_seq.is_some()
			|| self.remote.busy
			|| self.lifecycle.is_draining()
		{
			return false;
		}
		// The attempt makes the armed timer stale: it checks `attempts`.
		app_log!("[APP:REMOTE_RETRY: attempt={}]", r.attempts + 1);
		self.attempt_restore(cx);
		true
	}

	/// A stopped drain invalidated an in-flight restore. Its old transport
	/// must finish before retrying; either the result or drain may land first.
	pub(crate) fn resume_remote_restore(&mut self, cx: &mut Context<Self>) {
		if self.remote.busy || self.lifecycle.is_draining() {
			return;
		}
		let interrupted = self.remote.restoring.as_ref().is_some_and(|r| {
			r.open_seq.is_some_and(|seq| seq != self.remote.open_seq)
		});
		if interrupted {
			let r = self.remote.restoring.as_mut().unwrap();
			r.open_seq = None;
			app_log!("[APP:REMOTE_RETRY: attempt={}]", r.attempts + 1);
			self.attempt_restore(cx);
		}
	}

	fn attempt_restore(&mut self, cx: &mut Context<Self>) {
		let Some(folder) =
			self.remote.restoring.as_ref().map(|r| r.folder.clone())
		else {
			return;
		};
		if !self.remote.hosts.iter().any(|h| h.name == folder.host) {
			// A host added to ~/.ssh/config since the last try.
			if let Some(h) =
				load_hosts().into_iter().find(|h| h.name == folder.host)
			{
				self.remote.hosts.push(h);
			}
		}
		if let Some(r) = self.remote.restoring.as_mut() {
			r.attempts += 1;
		}
		let host = folder.host.clone();
		self.open_remote_folder(folder, cx);
		if !self.remote.hosts.iter().any(|h| h.name == host) {
			let why = Msg::new("remote_host_missing", [host]);
			return self.restore_failed(why, true, cx);
		}
		if self.remote.busy {
			let seq = self.remote.open_seq;
			if let Some(r) = self.remote.restoring.as_mut() {
				r.open_seq = Some(seq);
			}
		}
	}

	/// A failure keeps the tab and its reason. A failure that may clear
	/// (see `RemoteError::is_restore_retryable`) retries after
	/// [`retry_delay`]; a try during a drain waits until later.
	fn restore_failed(
		&mut self,
		why: Msg,
		retryable: bool,
		cx: &mut Context<Self>,
	) {
		let Some(r) = self.remote.restoring.as_mut() else {
			return;
		};
		r.failed = Some(why);
		r.open_seq = None;
		if !retryable {
			return;
		}
		let (attempts, id) = (r.attempts, r.id);
		let delay = retry_delay(attempts);
		let tab = self.ws_tab;
		cx.spawn(async move |this, cx| {
			crate::tabs::tagged(tab, async move {
				cx.background_executor().timer(delay).await;
				let _ = this.update(cx, |this, cx| {
					let due = this.remote.restoring.as_ref().is_some_and(|r| {
						r.id == id
							&& r.attempts == attempts
							&& r.open_seq.is_none()
					});
					if !due {
						return;
					}
					if this.lifecycle.is_draining() || this.remote.busy {
						// A drain (a quit that may still be stopped) or
						// another open of this tab is under way: try after
						// it. A tab that closes drops this timer with it.
						return this.restore_failed(
							this.remote
								.restoring
								.as_ref()
								.and_then(|r| r.failed.clone())
								.unwrap_or_else(|| {
									Msg::new("remote_restore_waiting", [])
								}),
							true,
							cx,
						);
					}
					app_log!("[APP:REMOTE_RETRY: attempt={}]", attempts + 1);
					this.attempt_restore(cx);
				});
			})
			.await
		})
		.detach();
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
				let text = Msg::new("remote_host_missing", [folder.host]);
				self.set_status_msg(Msg::with_msg_arg(
					"remote_open_failed",
					[String::new()],
					0,
					text.clone(),
				));
				self.remote_note(false, text, cx);
			}
		}
	}

	pub(crate) fn finish_open_remote(
		&mut self,
		opened: OpenedRemote,
		cx: &mut Context<Self>,
	) {
		self.release_workspace_state(cx);
		remember_recent(
			&mut self.remote.recent,
			&opened.client.host().name,
			&opened.workspace.id,
		);
		self.remote.restoring = None;
		let session = RemoteSession::from_opened(opened);
		let root = session.root.clone();
		let label = session.label();
		self.workspace_root = root.clone();
		self.remote.session = Some(session);
		self.workspace_open = true;
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
		self.launch_remote_scan_attempt(under, wipe, 0, cx);
	}

	pub(crate) fn resume_remote_scan(&mut self, cx: &mut Context<Self>) {
		if !self.accepting_work()
			|| !self.scan_cancel.as_ref().is_some_and(|c| c.is_cancelled())
		{
			return;
		}
		if let Some(recovery) = self.remote.scan_recovery.clone() {
			self.launch_remote_scan_attempt(
				recovery.under,
				recovery.wipe,
				recovery.retries,
				cx,
			);
		}
	}

	fn launch_remote_scan_attempt(
		&mut self,
		under: Option<PathBuf>,
		wipe: bool,
		retries: u8,
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
		let under_rel = match under.as_ref() {
			Some(p) => match remote_rel(&session_root, p) {
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
		self.remote.scan_recovery = Some(ScanRecovery {
			under: under.clone(),
			wipe,
			retries,
		});
		self.is_loading = true;
		self.set_status("status_scanning", []);
		let lifecycle_generation = self.lifecycle.generation();
		let cancel_job = cancel.clone();
		let mut async_app = cx.to_async();
		let this = cx.weak_entity();

		self.spawn_owned(
			cx,
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
				let retry_delay = result.as_ref().err()
					.filter(|err| err.is_transient_read_failure())
					.and_then(|_| scan_retry_delay(retries));
				let landed = this.update(&mut async_app, |model, cx| {
					if model.discovery_generation != generation
						|| model.lifecycle.generation() != lifecycle_generation
					{
						return false;
					}
					match result {
						Ok(scan) => {
							model.remote.scan_recovery = None;
							model.remote.scan_error = None;
							let e = remote_scan_entries(&session_root, scan);
							if retries > 0 {
								app_log!("[APP:REMOTE_SCAN_RECOVERED: retries={retries} repos={}]", e.repos.len());
							}
							model.apply_scan_page(wipe, e.repos, cx);
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
							if retry_delay.is_none() {
								model.remote.scan_recovery = None;
							}
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
									// The reason stays a message: worded when
									// drawn, in the language then in force.
									let text = other.to_string();
									let why = describe_msg(other);
									(
										Msg::with_msg_arg(
											"remote_scan_failed",
											[String::new()],
											0,
											why,
										),
										text,
									)
								}
							};
							app_log!("[APP:REMOTE_SCAN_FAILED: {text}]");
							if let Some(delay) = retry_delay {
								app_log!("[APP:REMOTE_SCAN_RETRY: attempt={} delay_ms={}]", retries + 1, delay.as_millis());
							}
							model.status = msg.clone();
							model.remote.scan_error = Some(msg);
							model.discovery_status =
								Some(ScanStatus::Incomplete);
							model.is_loading = false;
							model.refresh_reload = false;
							if wipe {
								model.pinned_repo = None;
								model.selected_repo_idx = None;
								model.release_repo_state();
								// The scan is over (`is_loading` is off), so
								// the Changes nodes go with the list.
								let change = model.repos.apply(
									RepoOp::Clear,
									&model.workspace_root,
								);
								model.repos_changed(change, cx);
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
					true
				}).unwrap_or(false);
				if !landed { return; }
				let Some(delay) = retry_delay else { return; };
				// The scan owns this cancellable wait. Close/Quit do not wait
				// for a full backoff, and Refresh cancels the old timer too.
				wait_cancelled(&mut async_app, delay, &cancel).await;
				if cancel.is_cancelled() { return; }
				let _ = this.update(&mut async_app, |model, cx| {
					if model.discovery_generation == generation
						&& model.lifecycle.generation() == lifecycle_generation
						&& model.accepting_work()
					{
						model.launch_remote_scan_attempt(under, wipe, retries + 1, cx);
					}
				});
			},
		);
	}
}

async fn wait_cancelled(
	cx: &mut gpui::AsyncApp,
	delay: std::time::Duration,
	cancel: &snip_core::gitrun::CancelToken,
) {
	let slice = std::time::Duration::from_millis(50);
	let executor = cx.background_executor().clone();
	let deadline = executor.now() + delay;
	while !cancel.is_cancelled() {
		let remaining = deadline.saturating_duration_since(executor.now());
		if remaining.is_zero() {
			break;
		}
		executor.timer(slice.min(remaining)).await;
	}
}

/// Tests hold a connect at the test clock so a user action can land while
/// it is in flight; a cancel ends the hold as it ends a real connect.
#[cfg(test)]
async fn hold_open(
	cx: &mut gpui::AsyncApp,
	delay: std::time::Duration,
	cancel: &snip_core::gitrun::CancelToken,
) {
	wait_cancelled(cx, delay, cancel).await;
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::i18n::Locale;

	#[test]
	fn host_failed_line_is_english_wording_not_a_key() {
		let err =
			crate::reason::reason_msg(&snip_core::reason::Reason::Cancelled);
		let line = host_failed_line("box", &err);
		assert_eq!(
			line,
			format!(
				"[APP:REMOTE_HOST_FAILED: host=box {}]",
				err.render(Locale::En)
			)
		);
		assert!(!line.contains(err.key), "{line}");
	}

	use crate::reason::refusal_key;
	use snip_core::gitsrc::StartReason;
	use snip_remote::ConnectFailureKind;

	fn start(reason: Option<StartReason>, detail: &str) -> RemoteError {
		RemoteError::Start {
			kind: ConnectFailureKind::Other,
			reason,
			detail: snip_core::reason::Datum::for_test(detail),
			message: "english text the CLI prints".into(),
		}
	}

	/// A Git view's lost worker is worded in the interface language; any
	/// other git error is worded by its reason, never by its text (#206).
	#[test]
	fn git_errors_word_a_lost_worker_in_the_interface_language() {
		use snip_core::gitsrc::GitError;
		let lost = || {
			GitError::remote(
				snip_core::reason::Reason::Start {
					start: Some(StartReason::NoWorker),
					detail: snip_core::reason::Datum::default(),
				},
				"snip is not installed",
			)
		};
		// One stored message, worded in whichever language draws it.
		let msg = git_error_msg(lost());
		assert_eq!(
			msg.render(Locale::ZhTw),
			crate::i18n::t("remote_start_no_worker", Locale::ZhTw)
		);
		assert_eq!(
			msg.render(Locale::En),
			crate::i18n::t("remote_start_no_worker", Locale::En)
		);
		let boom = git_error_msg(GitError::remote(
			snip_core::reason::Reason::Protocol,
			"boom",
		));
		assert_eq!(
			boom.render(Locale::ZhTw),
			crate::i18n::t("err_protocol", Locale::ZhTw)
		);
		assert_eq!(
			boom.render(Locale::En),
			crate::i18n::t("err_protocol", Locale::En)
		);
	}

	/// Every worker refusal that reaches a Git view keeps its code: the ones
	/// is worded in zh-TW, never by the worker's text (#213).
	#[test]
	fn git_view_refusals_are_worded_for_every_error_code() {
		use snip_remote::ErrorCode as C;
		let all = [
			C::Forbidden,
			C::NotFound,
			C::Unsupported,
			C::BadRequest,
			C::TooLarge,
			C::Io,
			C::VersionMismatch,
			C::NotARepository,
			C::InvalidRevision,
			C::Timeout,
			C::Busy,
			C::Cancelled,
			C::OutsideShare,
			C::Stale,
			C::Collision,
		];
		// Exhaustive: a new code fails to compile here until it is listed.
		for code in all {
			match code {
				C::Forbidden
				| C::NotFound
				| C::Unsupported
				| C::BadRequest
				| C::TooLarge
				| C::Io
				| C::VersionMismatch
				| C::NotARepository
				| C::InvalidRevision
				| C::Timeout
				| C::Busy
				| C::Cancelled
				| C::OutsideShare
				| C::Stale
				| C::Collision => {}
			}
		}
		// Codes that GitError gives a variant of their own are not this
		// path; every other code must carry through.
		let own_variant =
			[C::TooLarge, C::InvalidRevision, C::Timeout, C::Cancelled];
		let mut checked = 0;
		for code in all {
			if own_variant.contains(&code) {
				continue;
			}
			let err = snip_core::gitsrc::GitError::from(RemoteError::Refused {
				code,
				message: "worker English".into(),
				reason: None,
				suggestion: None,
			});
			assert!(
				matches!(
					&err,
					snip_core::gitsrc::GitError::Remote { reason, .. }
						if matches!(
							**reason,
							snip_core::reason::Reason::Refused { .. }
						)
				),
				"{code:?}: {err:?}"
			);
			let msg = git_error_msg(err);
			let key = refusal_key(code);
			assert_eq!(msg.key, key, "{code:?}");
			assert_eq!(
				msg.render(Locale::ZhTw),
				crate::i18n::t(key, Locale::ZhTw)
			);
			assert_ne!(
				msg.render(Locale::ZhTw),
				msg.render(Locale::En),
				"{code:?} has a zh-TW wording"
			);
			checked += 1;
		}
		assert_eq!(checked, all.len() - own_variant.len());
	}

	/// A known failure (start reason or refusal) is worded in the interface
	/// language with the transport's stderr as detail; an unknown one stays
	/// ssh's own words (#204, #213).
	#[test]
	fn known_failures_are_worded_in_the_interface_language() {
		let zh = |e| describe_msg(e).render(Locale::ZhTw);
		let en = |e| describe_msg(e).render(Locale::En);
		assert_eq!(
			zh(start(Some(StartReason::NoWorker), "")),
			"那台機器沒有安裝 snip，或版本太舊而不支援 ssh 連線；請在那台機器安裝或更新 snip-sync"
		);
		assert_eq!(
			zh(start(Some(StartReason::KeyLogin), "u@h: Permission denied")),
			"ssh 無法免密碼登入；請先設定金鑰登入（ssh-copy-id）：u@h: Permission denied"
		);
		assert_eq!(
			zh(start(Some(StartReason::NoAnswer), "")),
			"那台機器上的 worker 沒有及時回應；請檢查它的負載與連線後再試一次"
		);
		assert_eq!(
			zh(start(Some(StartReason::Closed), "")),
			"遠端在 snip 啟動前就關閉了連線"
		);
		assert_eq!(
			en(start(Some(StartReason::NoWorker), "usage: snip\n  --help")),
			"snip is not installed on that machine, or is too old for ssh \
			 connections; install or update snip-sync there"
		);
		assert_eq!(zh(start(None, "")), "無法在那台機器啟動 snip");
		assert_eq!(
			zh(start(None, "ssh: connect timed out")),
			"無法在那台機器啟動 snip：ssh: connect timed out"
		);
		assert_eq!(
			zh(RemoteError::Protocol("x".into())),
			"無法解讀遠端的回應，兩台機器的 snip-sync 版本可能不一致"
		);
		// A path that does not exist is worded, not the OS text (#213).
		let gone = || RemoteError::Refused {
			code: snip_remote::ErrorCode::NotFound,
			message: "No such file or directory (os error 2)".into(),
			reason: None,
			suggestion: None,
		};
		assert_eq!(zh(gone()), "找不到遠端的資料夾或檔案，請確認路徑是否存在");
		assert_eq!(
			en(gone()),
			"The remote folder or file was not found; check that the path exists"
		);
	}

	/// Every refusal code a user can hit is worded in both languages; the
	/// worker's English OS text never reaches the screen, and the user's
	/// path stays as the detail (#213). The exhaustive match makes a new
	/// `ErrorCode` a compile error here until it is decided.
	#[test]
	fn every_refusal_code_is_worded_without_raw_os_text() {
		use snip_remote::ErrorCode as C;
		fn decide(code: C) -> usize {
			match code {
				C::Forbidden => 0,
				C::NotFound => 1,
				C::Unsupported => 2,
				C::BadRequest => 3,
				C::TooLarge => 4,
				C::Io => 5,
				C::VersionMismatch => 6,
				C::NotARepository => 7,
				C::InvalidRevision => 8,
				C::Timeout => 9,
				C::Busy => 10,
				C::Cancelled => 11,
				C::OutsideShare => 12,
				C::Stale => 13,
				C::Collision => 14,
			}
		}
		let all = [
			C::Forbidden,
			C::NotFound,
			C::Unsupported,
			C::BadRequest,
			C::TooLarge,
			C::Io,
			C::VersionMismatch,
			C::NotARepository,
			C::InvalidRevision,
			C::Timeout,
			C::Busy,
			C::Cancelled,
			C::OutsideShare,
			C::Stale,
			C::Collision,
		];
		for (i, code) in all.into_iter().enumerate() {
			assert_eq!(decide(code), i, "{code:?} is listed out of order");
		}
		let os = "No such file or directory (os error 2)";
		let refused = |code| RemoteError::Refused {
			code,
			message: os.into(),
			reason: None,
			suggestion: None,
		};
		for code in all {
			for locale in [Locale::ZhTw, Locale::En] {
				let plain = describe_msg(refused(code)).render(locale);
				let at =
					describe_msg_at(refused(code), "/srv/gone").render(locale);
				assert!(!plain.contains("os error"), "{code:?} {plain}");
				assert!(!at.contains("os error"), "{code:?} {at}");
				assert!(at.ends_with("/srv/gone"), "{code:?} {at}");
				assert_ne!(plain, os);
			}
			// No English left in the zh-TW text.
			let zh = describe_msg(refused(code)).render(Locale::ZhTw);
			assert!(
				!zh.replace("Git", "")
					.chars()
					.any(|c| c.is_ascii_alphabetic()),
				"{code:?} zh-TW text is not translated: {zh}"
			);
		}
		assert_eq!(
			describe_msg_at(refused(C::NotFound), "/srv/gone")
				.render(Locale::ZhTw),
			"找不到遠端的資料夾或檔案，請確認路徑是否存在：/srv/gone"
		);
	}

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
	fn a_failed_reconnect_waits_longer_each_time_up_to_a_minute() {
		let secs: Vec<u64> =
			(1..=7).map(|n| retry_delay(n).as_secs()).collect();
		assert_eq!(secs, [5, 10, 20, 40, 60, 60, 60]);
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
				reason: None,
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
			reason: None,
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
			reason: None,
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
			reason: None,
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
			error_reasons: Vec::new(),
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
			.filter(|(p, f)| {
				p == session_root
					&& f.message == invalid_msg
					&& f.reason == snip_core::reason::Reason::Protocol
			})
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

	/// A scan row keeps the worker's reason; an old worker's, which sends
	/// none, falls back to its code, and an invalid path in between does
	/// not shift the reasons of the rows after it.
	#[test]
	fn remote_scan_rows_carry_the_workers_reason() {
		use snip_core::reason::Reason;
		let session_root = Path::new("/session");
		let scanned =
			|rel: &str, reason: Option<Reason>| snip_remote::ScannedRepo {
				rel: rel.into(),
				utf8: true,
				name: rel.into(),
				kind: snip_core::gitview::FoundKind::UninitializedSubmodule,
				summary: Err("worker english".into()),
				reason,
			};
		let scan = snip_remote::RepoScan {
			repos: vec![
				scanned("a", Some(Reason::SubmoduleNotCheckedOut)),
				scanned("b", None),
			],
			errors: vec![
				("x".into(), "one".into()),
				("../escape".into(), "two".into()),
				("y".into(), "three".into()),
				("z".into(), "four".into()),
			],
			error_reasons: vec![
				Some(Reason::TimedOut),
				Some(Reason::GitBusy),
				Some(Reason::Cancelled),
			],
			error_overflow: 0,
			depth_limited: Vec::new(),
			depth_overflow: 0,
			status: ScanStatus::Complete,
		};
		let res = remote_scan_entries(session_root, scan);
		let io = Reason::Refused {
			code: snip_remote::ErrorCode::Io,
		};
		let reasons: Vec<_> = res
			.repos
			.iter()
			.map(|r| r.summary.as_ref().unwrap_err().reason.clone())
			.collect();
		assert_eq!(reasons, vec![Reason::SubmoduleNotCheckedOut, io.clone()]);
		let rows: Vec<_> = res
			.errors
			.iter()
			.map(|(p, f)| (p.clone(), f.reason.clone()))
			.collect();
		assert_eq!(
			rows,
			vec![
				(session_root.join("x"), Reason::TimedOut),
				(session_root.to_path_buf(), Reason::Protocol),
				(session_root.join("y"), Reason::Cancelled),
				(session_root.join("z"), io),
			]
		);
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
