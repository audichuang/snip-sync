//! The master: pairs with a worker once, then calls it over pinned TLS.
//! Every call blocks; the desktop app makes them on its background
//! executor, as it does local reads.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustls::{ClientConnection, StreamOwned};
use serde::{Deserialize, Serialize};

use crate::proto::{
	read_frame, write_frame, DirEntry, ErrorCode, RemoteWorkspace, Request,
	Response, Stat, PROTOCOL_VERSION,
};
use crate::tls::{client_config, pairing_proof, server_name, Fingerprint};
use crate::{to_hex, Identity, RemoteError};

/// The port a worker listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 47821;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle connections a client keeps for reuse.
const POOL: usize = 4;

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
			},
		)
		.map_err(tls_error)?;
		let reply = read_frame::<Response>(&mut tls).map_err(tls_error)?;
		let seen = seen.get().ok_or_else(|| {
			RemoteError::Protocol("the worker sent no certificate".into())
		})?;
		match reply {
			Some(Response::Hello { name, paired, .. }) => Ok(Self {
				tls,
				worker_name: name,
				paired,
				seen,
			}),
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

	pub fn call(&mut self, request: &Request) -> Result<Response, RemoteError> {
		write_frame(&mut self.tls, request).map_err(tls_error)?;
		match read_frame::<Response>(&mut self.tls).map_err(tls_error)? {
			Some(Response::Error { code, message }) => {
				Err(RemoteError::Refused { code, message })
			}
			Some(response) => Ok(response),
			None => Err(RemoteError::Io(io::Error::new(
				io::ErrorKind::UnexpectedEof,
				"the worker closed the connection",
			))),
		}
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
	match conn.call(&Request::Pair {
		name: my_name.to_string(),
		proof: to_hex(&proof),
	})? {
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
		let pooled = self
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.pop();
		let (mut conn, reused) = match pooled {
			Some(conn) => (conn, true),
			None => (self.connect()?, false),
		};
		let result = match conn.call(request) {
			// The worker closes idle connections; one fresh try.
			Err(RemoteError::Io(_)) if reused => {
				conn = self.connect()?;
				conn.call(request)
			}
			other => other,
		};
		if matches!(&result, Ok(_) | Err(RemoteError::Refused { .. })) {
			let mut idle =
				self.idle.lock().unwrap_or_else(PoisonError::into_inner);
			if idle.len() < POOL {
				idle.push(conn);
			}
		}
		result
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
