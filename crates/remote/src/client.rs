//! The master: pairs with a worker once, then calls it over pinned TLS.
//! Every call blocks; the desktop app makes them on its background
//! executor, as it does local reads.

use std::io::{self, Read as IoRead, Write as IoWrite};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustls::{ClientConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use snip_core::browser::{
	BlobText, CommitSummary, GitPreview, LogQuery, RefSnapshot, TreeEntry,
};
use snip_core::format::ChangeType;
use snip_core::gitrun::CancelToken;
use snip_core::gitsrc::{GitError, GitSource};
use snip_core::gitview::{
	ChangeList, ChangedPathList, CommitDetails, Read, ReadProfile, RepoView,
};

use crate::proto::{
	read_frame, write_frame, DirEntry, ErrorCode, GitQuery, GitReply,
	RemoteWorkspace, RepoScan, Request, Response, Stat, GIT_CALL_LIMIT,
	MAX_GIT_CALLS_IN_FLIGHT, PROTOCOL_MAX, PROTOCOL_VERSION,
};
use crate::tls::{client_config, pairing_proof, server_name, Fingerprint};
use crate::{to_hex, Identity, RemoteError};

/// The port a worker listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 47821;
// Kept under the desktop app's 8 s drain deadline (lifecycle.rs): a call
// cannot be cancelled mid-read, so a stalled worker must fail it in time.
// The read timeout is idle time per read, so a slow but moving transfer
// still completes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle connections a client keeps for reuse.
const POOL: usize = 4;
/// Default limit for non-git calls.
pub const CALL_LIMIT_DEFAULT: Duration = Duration::from_secs(30);

/// A worker this master has paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedWorker {
	pub name: String,
	/// As the user typed it: a Tailscale IP or MagicDNS name, maybe a port.
	pub addr: String,
	/// Hex SHA-256 of the worker's certificate.
	pub fingerprint: String,
}

impl PairedWorker {
	pub fn pin(&self) -> Result<Fingerprint, RemoteError> {
		Fingerprint::from_hex(&self.fingerprint).ok_or_else(|| {
			RemoteError::Protocol("stored worker fingerprint is damaged".into())
		})
	}
}

/// `host`, `host:port`, `ip`, `[v6]:port`; a missing port is
/// [`DEFAULT_PORT`].
pub fn resolve_addr(addr: &str) -> io::Result<Vec<SocketAddr>> {
	let addr = addr.trim();
	if let Ok(ip) = addr.parse::<IpAddr>() {
		return Ok(vec![SocketAddr::new(ip, DEFAULT_PORT)]);
	}
	let with_port = match addr.rsplit_once(':') {
		Some((_, port)) if port.parse::<u16>().is_ok() => addr.to_string(),
		_ => format!("{addr}:{DEFAULT_PORT}"),
	};
	let found: Vec<SocketAddr> = with_port.to_socket_addrs()?.collect();
	if found.is_empty() {
		return Err(io::Error::new(
			io::ErrorKind::NotFound,
			format!("{addr} does not resolve"),
		));
	}
	Ok(found)
}

pub struct Connection {
	tls: StreamOwned<ClientConnection, TcpStream>,
	worker_name: String,
	paired: bool,
	seen: Fingerprint,
	version: u32,
	pub(crate) frames_seen: usize,
}

impl Connection {
	/// Connects and says hello. With `pin`, a worker presenting another
	/// certificate fails here.
	pub fn open(
		addr: &str,
		identity: &Identity,
		pin: Option<Fingerprint>,
		my_name: &str,
	) -> Result<Self, RemoteError> {
		let mut last = None;
		let mut tcp = None;
		for sock in resolve_addr(addr)? {
			match TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT) {
				Ok(stream) => {
					tcp = Some(stream);
					break;
				}
				Err(err) => last = Some(err),
			}
		}
		let tcp = match tcp {
			Some(tcp) => tcp,
			None => {
				return Err(last
					.unwrap_or_else(|| io::Error::other("no address"))
					.into())
			}
		};
		tcp.set_nodelay(true)?;
		tcp.set_read_timeout(Some(IO_TIMEOUT))?;
		tcp.set_write_timeout(Some(IO_TIMEOUT))?;
		let (config, seen) = client_config(identity, pin)?;
		let conn = ClientConnection::new(config, server_name())?;
		let mut tls = StreamOwned::new(conn, tcp);
		write_frame(
			&mut tls,
			&Request::Hello {
				version: PROTOCOL_VERSION,
				name: my_name.to_string(),
				max_version: Some(PROTOCOL_MAX),
			},
		)
		.map_err(tls_error)?;
		let reply = read_frame::<Response>(&mut tls).map_err(tls_error)?;
		let seen = seen.get().ok_or_else(|| {
			RemoteError::Protocol("the worker sent no certificate".into())
		})?;
		match reply {
			Some(Response::Hello {
				name,
				paired,
				max_version,
				..
			}) => {
				let version = max_version.unwrap_or(1).clamp(1, PROTOCOL_MAX);
				Ok(Self {
					tls,
					worker_name: name,
					paired,
					seen,
					version,
					frames_seen: 0,
				})
			}
			Some(Response::Error { code, message }) => {
				Err(RemoteError::Refused { code, message })
			}
			_ => Err(RemoteError::Protocol("expected hello".into())),
		}
	}

	pub fn worker_name(&self) -> &str {
		&self.worker_name
	}

	pub fn paired(&self) -> bool {
		self.paired
	}

	pub fn version(&self) -> u32 {
		self.version
	}

	pub fn seen_fingerprint(&self) -> Fingerprint {
		self.seen
	}

	pub fn frames_seen(&self) -> usize {
		self.frames_seen
	}

	pub fn call(
		&mut self,
		request: &Request,
		cancel: Option<&CancelToken>,
		limit: Duration,
	) -> Result<Response, RemoteError> {
		exchange(&mut self.tls, request, cancel, limit, &mut self.frames_seen)
	}
}

/// Generic frame exchange loop over any `Read + Write` stream.
pub(crate) fn exchange<S: IoRead + IoWrite>(
	stream: &mut S,
	request: &Request,
	cancel: Option<&CancelToken>,
	limit: Duration,
	frames: &mut usize,
) -> Result<Response, RemoteError> {
	*frames = 0;
	write_frame(stream, request).map_err(tls_error)?;
	let started = Instant::now();
	loop {
		let reply = read_frame::<Response>(stream).map_err(tls_error)?;
		match reply {
			Some(Response::Pending) => {
				*frames += 1;
				if cancel.is_some_and(|c| c.is_cancelled()) {
					return Err(RemoteError::Cancelled);
				}
				if started.elapsed() >= limit {
					return Err(RemoteError::TimedOut);
				}
			}
			Some(Response::Error { code, message }) => {
				*frames += 1;
				return Err(RemoteError::Refused { code, message });
			}
			Some(response) => {
				*frames += 1;
				return Ok(response);
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

/// A pin mismatch surfaces from rustls inside an `io::Error`; keep its text.
fn tls_error(err: io::Error) -> RemoteError {
	RemoteError::Io(err)
}

/// Pairs with the worker at `addr` using the code it shows. The worker's
/// certificate is trusted from here on; compare
/// [`Fingerprint::short`] on both screens to rule out a relay.
pub fn pair(
	addr: &str,
	code: &str,
	identity: &Identity,
	my_name: &str,
) -> Result<PairedWorker, RemoteError> {
	let mut conn = Connection::open(addr, identity, None, my_name)?;
	let worker_fp = conn.seen;
	let proof = pairing_proof(code, &worker_fp, &identity.fingerprint());
	match conn.call(
		&Request::Pair {
			name: my_name.to_string(),
			proof: to_hex(&proof),
		},
		None,
		CALL_LIMIT_DEFAULT,
	)? {
		Response::Paired { name } => Ok(PairedWorker {
			name,
			addr: addr.trim().to_string(),
			fingerprint: worker_fp.to_hex(),
		}),
		_ => Err(RemoteError::Protocol("expected paired".into())),
	}
}

/// Calls to one paired worker, reusing a few idle connections.
pub struct Client {
	worker: PairedWorker,
	pin: Fingerprint,
	identity: Arc<Identity>,
	my_name: String,
	idle: Mutex<Vec<Connection>>,
	limiter: GitLimiter,
}

impl Client {
	pub fn new(
		worker: PairedWorker,
		identity: Arc<Identity>,
		my_name: String,
	) -> Result<Self, RemoteError> {
		Ok(Self {
			pin: worker.pin()?,
			worker,
			identity,
			my_name,
			idle: Mutex::new(Vec::new()),
			limiter: GitLimiter::new(),
		})
	}

	pub fn worker(&self) -> &PairedWorker {
		&self.worker
	}

	fn connect(&self) -> Result<Connection, RemoteError> {
		let conn = Connection::open(
			&self.worker.addr,
			&self.identity,
			Some(self.pin),
			&self.my_name,
		)?;
		if !conn.paired {
			return Err(RemoteError::Refused {
				code: ErrorCode::NotPaired,
				message: "the worker no longer trusts this master; pair again"
					.into(),
			});
		}
		Ok(conn)
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
			Request::ScanRepos { .. } | Request::GitView { .. }
		);
		let _guard = if is_git {
			Some(self.limiter.acquire(cancel, limit)?)
		} else {
			None
		};

		let need = request.needs_version();
		let pooled = self
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.pop();

		let (mut conn, reused) = match pooled {
			Some(conn) if need > 1 && conn.version() < need => {
				drop(conn);
				self.idle
					.lock()
					.unwrap_or_else(PoisonError::into_inner)
					.retain(|c| c.version() >= need);
				let fresh = self.connect()?;
				if fresh.version() < need {
					let worker = fresh.worker_name().to_string();
					let have = fresh.version();
					let mut idle = self
						.idle
						.lock()
						.unwrap_or_else(PoisonError::into_inner);
					if idle.len() < POOL {
						idle.push(fresh);
					}
					return Err(RemoteError::WorkerTooOld {
						worker,
						have,
						need,
					});
				}
				(fresh, false)
			}
			Some(conn) => (conn, true),
			None => {
				let fresh = self.connect()?;
				if need > 1 && fresh.version() < need {
					let worker = fresh.worker_name().to_string();
					let have = fresh.version();
					let mut idle = self
						.idle
						.lock()
						.unwrap_or_else(PoisonError::into_inner);
					if idle.len() < POOL {
						idle.push(fresh);
					}
					return Err(RemoteError::WorkerTooOld {
						worker,
						have,
						need,
					});
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
			if should_resend(reused, conn.frames_seen, err) {
				let remaining_retry = limit.saturating_sub(start.elapsed());
				if remaining_retry.is_zero() {
					return Err(RemoteError::TimedOut);
				}
				if let Ok(mut fresh) = self.connect() {
					if need <= 1 || fresh.version() >= need {
						result = fresh.call(request, cancel, remaining_retry);
						conn = fresh;
					}
				}
			}
		}

		if matches!(&result, Ok(_) | Err(RemoteError::Refused { .. })) {
			let mut idle =
				self.idle.lock().unwrap_or_else(PoisonError::into_inner);
			if idle.len() < POOL {
				idle.push(conn);
			}
		}

		result
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

	pub fn list_workspaces(&self) -> Result<Vec<RemoteWorkspace>, RemoteError> {
		match self.call(&Request::ListWorkspaces)? {
			Response::Workspaces { items } => Ok(items),
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

	#[test]
	fn exchange_skips_pending_frames_and_counts_frames() {
		let mut stream = FakeDuplex::new(&[
			Response::Pending,
			Response::Pending,
			Response::Workspaces { items: vec![] },
		]);
		let req = Request::ListWorkspaces;
		let mut frames = 0;
		let res = exchange(
			&mut stream,
			&req,
			None,
			Duration::from_secs(10),
			&mut frames,
		)
		.unwrap();

		assert_eq!(res, Response::Workspaces { items: vec![] });
		assert_eq!(frames, 3);
	}

	#[test]
	fn exchange_pending_followed_by_cancelled_token_returns_cancelled() {
		let mut stream =
			FakeDuplex::new(&[Response::Pending, Response::Pending]);
		let cancel = CancelToken::new();
		cancel.cancel();
		let req = Request::ListWorkspaces;
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			&req,
			Some(&cancel),
			Duration::from_secs(10),
			&mut frames,
		)
		.unwrap_err();

		assert!(matches!(err, RemoteError::Cancelled));
		assert_eq!(frames, 1);
	}

	#[test]
	fn exchange_zero_limit_with_pending_returns_timed_out() {
		let mut stream = FakeDuplex::new(&[Response::Pending]);
		let req = Request::ListWorkspaces;
		let mut frames = 0;
		let err =
			exchange(&mut stream, &req, None, Duration::ZERO, &mut frames)
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
		let req = Request::ListWorkspaces;
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			&req,
			None,
			Duration::from_secs(10),
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
		let req = Request::ListWorkspaces;
		let mut frames = 0;
		let err = exchange(
			&mut stream,
			&req,
			None,
			Duration::from_secs(10),
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
		let g1 = limiter.acquire(None, Duration::from_secs(1)).unwrap();
		let g2 = limiter.acquire(None, Duration::from_secs(1)).unwrap();
		let g3 = limiter.acquire(None, Duration::from_secs(1)).unwrap();
		let g4 = limiter.acquire(None, Duration::from_secs(1)).unwrap();

		// 5th with short limit returns TimedOut
		let timed_out = limiter
			.acquire(None, Duration::from_millis(20))
			.unwrap_err();
		assert!(matches!(timed_out, RemoteError::TimedOut));

		// 5th with cancelled token returns Cancelled
		let cancel = CancelToken::new();
		cancel.cancel();
		let cancelled = limiter
			.acquire(Some(&cancel), Duration::from_secs(1))
			.unwrap_err();
		assert!(matches!(cancelled, RemoteError::Cancelled));

		// Release one guard lets the 5th through
		drop(g1);
		let g5 = limiter
			.acquire(None, Duration::from_millis(100))
			.expect("releasing one guard should allow acquisition");
		drop((g2, g3, g4, g5));
	}
}
