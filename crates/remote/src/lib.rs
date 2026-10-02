//! snip-remote: remote-node mode. A master app operates the shared
//! workspaces of a worker app (the same desktop build) over a private
//! network such as Tailscale.
//!
//! - Transport: TLS 1.3 over TCP, one JSON request per frame ([`proto`]).
//! - Identity: one self-signed certificate per install, pinned by its
//!   SHA-256 fingerprint ([`tls`]). Both sides present one.
//! - Pairing: the worker opens a one-time code; the master proves it knows
//!   the code, bound to both certificates of the connection
//!   ([`tls::pairing_proof`]). After that the master's certificate is
//!   trusted and the worker's is pinned.
//! - Scope: a worker answers only inside the folders it shares, resolved
//!   by real path ([`worker::SharedRoot`]).
//!
//! This is a control channel for files on another machine. It is not a
//! clipboard transport: copy and paste stay as `docs/spec.md` describes.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};

pub mod client;
pub mod proto;
pub mod store;
pub mod tls;
pub mod worker;

pub use client::{pair, Client, Connection, PairedWorker, DEFAULT_PORT};
pub use proto::{DirEntry, ErrorCode, RemoteWorkspace, Request, Response};
pub use store::{TrustedMasterStore, WorkerStore};
pub use tls::{Fingerprint, Identity};
pub use worker::{SharedRoot, TrustedMaster, Worker, WorkerOptions};

/// Master's list of paired workers, in the app's config folder.
pub const WORKERS_FILE: &str = "remote-workers.json";
/// Worker's list of trusted masters, in the app's config folder.
pub const TRUSTED_FILE: &str = "remote-trusted-masters.json";

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
	#[error(transparent)]
	Io(#[from] io::Error),
	#[error("TLS: {0}")]
	Tls(#[from] rustls::Error),
	#[error("certificate: {0}")]
	Cert(String),
	/// The worker answered with an error.
	#[error("{message}")]
	Refused { code: ErrorCode, message: String },
	#[error("protocol: {0}")]
	Protocol(String),
}

impl RemoteError {
	pub fn code(&self) -> Option<ErrorCode> {
		match self {
			Self::Refused { code, .. } => Some(*code),
			_ => None,
		}
	}
}

/// Where a worker listens unless told otherwise: every interface, so the
/// Tailscale address answers. `--listen <tailscale-ip>:47821` narrows it.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:47821";

/// The app's config folder, shared by the desktop app and the CLI so both
/// use one device identity and one list of pairings. `SNIP_CONFIG_DIR`
/// overrides it.
pub fn default_config_dir() -> Option<PathBuf> {
	if let Some(dir) = std::env::var_os("SNIP_CONFIG_DIR") {
		return Some(PathBuf::from(dir));
	}
	let home = || {
		std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
			.map(PathBuf::from)
	};
	if cfg!(target_os = "macos") {
		// The Homebrew cask's `zap` removes this folder.
		Some(
			home()?
				.join("Library/Application Support/com.audichuang.snip-sync"),
		)
	} else if cfg!(windows) {
		Some(PathBuf::from(std::env::var_os("APPDATA")?).join("snip-sync"))
	} else {
		Some(
			std::env::var_os("XDG_CONFIG_HOME")
				.map(PathBuf::from)
				.or_else(|| Some(home()?.join(".config")))?
				.join("snip-sync"),
		)
	}
}

/// This install's name as peers see it.
pub fn device_name() -> String {
	let from_env = std::env::var("SNIP_DEVICE_NAME")
		.or_else(|_| std::env::var("COMPUTERNAME"))
		.or_else(|_| std::env::var("HOSTNAME"))
		.ok()
		.filter(|n| !n.trim().is_empty());
	from_env
		.or_else(|| {
			let out = std::process::Command::new("hostname").output().ok()?;
			let name = String::from_utf8(out.stdout).ok()?;
			let name = name.trim().to_string();
			(!name.is_empty()).then_some(name)
		})
		.unwrap_or_else(|| "snip-sync".to_string())
}

/// A worker with no window (`snip worker`, or the desktop app's
/// `--worker --headless`): serves `shares` until the process is stopped,
/// with a pairing code open from the start. Returns only on a start error.
pub fn run_headless_worker(
	listen: SocketAddr,
	shares: &[PathBuf],
	config_dir: Option<&Path>,
) -> Result<std::convert::Infallible, RemoteError> {
	if shares.is_empty() {
		return Err(RemoteError::Protocol(
			"a headless worker needs at least one shared folder".into(),
		));
	}
	let identity = match config_dir {
		Some(dir) => Identity::load_or_create(dir)?,
		None => Identity::generate()?,
	};
	let worker = Worker::start(
		listen,
		&identity,
		WorkerOptions {
			name: device_name(),
			trust_file: config_dir.map(|d| d.join(TRUSTED_FILE)),
		},
	)?;
	for (path, err) in worker.set_roots(shares) {
		eprintln!("not sharing {}: {err}", path.display());
	}
	if worker.roots().is_empty() {
		return Err(RemoteError::Protocol("no folder could be shared".into()));
	}
	let code = worker.open_pairing();
	println!("snip-sync worker listening on {}", worker.local_addr());
	println!("fingerprint {}", worker.fingerprint().short());
	for root in worker.roots() {
		println!("sharing {}", root.path.display());
	}
	println!(
		"pairing code {code} (valid {} minutes; restart for a new one)",
		worker::PAIRING_TTL.as_secs() / 60
	);
	loop {
		std::thread::sleep(Duration::from_secs(3600));
	}
}

/// A missing or unreadable file is an empty list: it only loses pairings.
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
	fs::read(path)
		.ok()
		.and_then(|bytes| serde_json::from_slice(&bytes).ok())
		.unwrap_or_default()
}

pub fn save_json<T: Serialize + ?Sized>(
	path: &Path,
	value: &T,
) -> io::Result<()> {
	if let Some(dir) = path.parent() {
		if !dir.as_os_str().is_empty() {
			fs::create_dir_all(dir)?;
		}
	}
	let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
	// Written aside and renamed, so a crash never leaves half a list.
	static COUNTER: AtomicU64 = AtomicU64::new(0);
	let pid = std::process::id();
	let count = COUNTER.fetch_add(1, Ordering::Relaxed);
	let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
	let tmp = path.with_file_name(format!("{file_name}.{pid}.{count}.tmp"));
	let res = (|| {
		fs::write(&tmp, bytes)?;
		fs::rename(&tmp, path)
	})();
	if res.is_err() {
		let _ = fs::remove_file(&tmp);
	}
	res
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn from_hex(text: &str) -> Option<Vec<u8>> {
	if !text.len().is_multiple_of(2) {
		return None;
	}
	(0..text.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
		.collect()
}
