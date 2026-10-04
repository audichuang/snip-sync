//! snip-remote: remote workspaces. A master app (or the CLI) works on
//! folders of another machine through a worker it starts there over ssh.
//!
//! - Transport: `ssh <host> snip serve --stdio`, one JSON request per frame
//!   on the worker's stdin and stdout ([`proto`], [`ssh`]). SSH
//!   authenticates and encrypts; there is no pairing of its own.
//! - Hosts: the ones `~/.ssh/config` names ([`ssh::config_hosts`]).
//! - Scope: anything the ssh user can read can be opened as a workspace;
//!   each request stays inside its workspace ([`worker::SharedRoot`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{de::DeserializeOwned, Serialize};

pub mod client;
pub(crate) mod gitserve;
pub(crate) mod jobs;
pub mod proto;
pub mod ssh;
pub mod worker;

pub use client::{Client, Connection, RemoteHost, RemoteRepo, Transport};
pub use proto::{
	DirEntry, ErrorCode, GitQuery, GitReply, RemoteWorkspace, RepoScan,
	Request, Response, ScannedRepo, PROTOCOL_MAX,
};
pub use worker::{serve_stdio, SharedRoot, Worker, WorkerOptions};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
	#[error(transparent)]
	Io(#[from] io::Error),
	/// The worker did not start: ssh's message, or a missing snip.
	#[error("{0}")]
	Connect(String),
	/// The worker answered with an error.
	#[error("{message}")]
	Refused { code: ErrorCode, message: String },
	#[error("protocol: {0}")]
	Protocol(String),
	#[error("cancelled")]
	Cancelled,
	#[error("the worker did not answer in time")]
	TimedOut,
	#[error(
		"{worker} is too old for Git views (it speaks protocol {have}, this needs {need}); update it"
	)]
	WorkerTooOld {
		worker: String,
		have: u32,
		need: u32,
	},
}

impl RemoteError {
	pub fn code(&self) -> Option<ErrorCode> {
		match self {
			Self::Refused { code, .. } => Some(*code),
			_ => None,
		}
	}
}

/// The app's config folder, shared by the desktop app and the CLI.
/// `SNIP_CONFIG_DIR` overrides it.
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

/// A missing or unreadable file is an empty list.
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
