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
pub(crate) mod copyserve;
pub(crate) mod gitserve;
pub(crate) mod jobs;
pub(crate) mod pasteserve;
pub mod pastetarget;
pub(crate) mod prepared;
pub mod proto;
pub mod ssh;
pub mod worker;

pub use client::{Client, Connection, RemoteHost, RemoteRepo, Transport};
pub use pastetarget::{
	FilePastePlan, PasteError, PasteRoute, PasteTarget, PreparedReplay,
	REPO_BUSY_TEXT,
};
pub use proto::{
	DirEntry, ErrorCode, ExportTarget, GitQuery, GitReply, ImportExpect,
	ImportPlanned, PasteMapping, RemoteWorkspace, ReplayExpect, RepoScan,
	Request, Response, ScannedRepo, PROTOCOL_MAX,
};
pub use snip_core::reason::{Reason, StaleChange};
pub use worker::{serve_stdio, SharedRoot, Worker, WorkerOptions};

/// Typed reason a transport ended before the worker's hello. Keep this
/// apart from its human-facing message so callers never parse SSH text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFailureKind {
	Io(io::ErrorKind),
	TransportClosed,
	TimedOut,
	Authentication,
	WorkerUnavailable,
	Protocol,
	Other,
}

pub use snip_core::gitsrc::StartReason;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
	#[error(transparent)]
	Io(#[from] io::Error),
	/// The worker did not start: ssh's message, or a missing snip.
	#[error("{0}")]
	Connect(String),
	#[error("{message}")]
	Start {
		kind: ConnectFailureKind,
		/// The known cause, when the start failure is one of them.
		reason: Option<StartReason>,
		/// The transport's own stderr, unchanged; empty when it said nothing.
		detail: String,
		message: String,
	},
	/// The worker answered with an error.
	#[error("{message}")]
	Refused {
		code: ErrorCode,
		message: String,
		/// Why, beyond the code; `None` from a worker that sends none (an
		/// old one) or whose code says it all. Boxed: `Reason` is large and
		/// this error travels in every `Result`.
		reason: Option<Box<Reason>>,
	},
	/// A `TooLarge` refusal whose file is fine: the diff built from it is
	/// over the limit (#214). The same wire code as [`Self::Refused`].
	#[error("{message}")]
	PatchTooLarge { message: String },
	#[error("protocol: {0}")]
	Protocol(String),
	#[error("cancelled")]
	Cancelled,
	#[error("the worker did not answer in time")]
	TimedOut,
	/// The stream went silent mid-call past the idle bound: no byte for `silent` s; `received` bytes of the answer arrived in `elapsed` s since the request.
	#[error("the worker did not answer in time: no data for {silent:.1} s; {received} bytes of its reply arrived in {elapsed:.1} s")]
	Stalled {
		silent: f64,
		received: u64,
		elapsed: f64,
	},
	#[error(
		"snip-sync on {worker} is too old for this (it speaks protocol {have}, this needs {need}); update it there"
	)]
	WorkerTooOld {
		worker: String,
		have: u32,
		need: u32,
	},
}

impl RemoteError {
	/// Why this failed, as data for a front end to word. The `Display` text
	/// stays the CLI's. An exhaustive match: a new variant must decide.
	pub fn reason(&self) -> Reason {
		match self {
			Self::Io(e) if e.kind() == io::ErrorKind::TimedOut => {
				Reason::TimedOut
			}
			Self::Io(e) => Reason::Disconnected {
				detail: e.to_string(),
			},
			Self::Connect(detail) => Reason::Start {
				start: None,
				detail: detail.clone(),
			},
			Self::Start { reason, detail, .. } => Reason::Start {
				start: *reason,
				detail: detail.clone(),
			},
			// A worker's own reason wins; without one the code words it.
			Self::Refused { code, reason, .. } => match reason {
				Some(r) => (**r).clone(),
				None => Reason::Refused { code: *code },
			},
			Self::PatchTooLarge { .. } => Reason::PatchTooLarge,
			Self::Protocol(_) => Reason::Protocol,
			Self::Cancelled => Reason::Cancelled,
			Self::TimedOut | Self::Stalled { .. } => Reason::TimedOut,
			Self::WorkerTooOld { worker, have, need } => Reason::WorkerTooOld {
				worker: worker.clone(),
				have: *have,
				need: *need,
			},
		}
	}

	/// Whether a caller may schedule another *read* attempt. This does
	/// not authorize retrying a write whose outcome may be unknown.
	pub fn is_transient_read_failure(&self) -> bool {
		match self {
			Self::Io(error) => transient_io(error.kind()),
			Self::Start {
				kind: ConnectFailureKind::Io(kind),
				..
			} => transient_io(*kind),
			Self::TimedOut
			| Self::Stalled { .. }
			| Self::Start {
				kind:
					ConnectFailureKind::TransportClosed
					| ConnectFailureKind::TimedOut,
				..
			} => true,
			Self::Refused {
				code: ErrorCode::Busy | ErrorCode::Timeout,
				..
			} => true,
			_ => false,
		}
	}

	/// Whether restoring a remote tab may try the open again. Wider than
	/// [`Self::is_transient_read_failure`], which the scan and paste paths
	/// keep: a start failure nobody classified (a network or a folder not
	/// up yet, ssh's own words) retries, and so does a folder that is not
	/// there yet. The known permanent ones stop: a refused permission, a
	/// worker that is missing, too old or speaks another protocol. Only
	/// a folder open reaches this, never a write: an outcome-unknown write
	/// is not retried through it.
	pub fn is_restore_retryable(&self) -> bool {
		match self {
			Self::Start { kind, .. } => !matches!(
				kind,
				ConnectFailureKind::Authentication
					| ConnectFailureKind::WorkerUnavailable
					| ConnectFailureKind::Protocol
			),
			Self::Refused {
				code: ErrorCode::NotFound | ErrorCode::Busy | ErrorCode::Timeout,
				..
			} => true,
			_ => self.is_transient_read_failure(),
		}
	}

	/// The request may have reached the worker and run there, but no answer
	/// came back: after a write, whether it happened is unknown. A refusal,
	/// a worker that never started, or one too old answered (or never got
	/// the request).
	pub fn outcome_unknown(&self) -> bool {
		matches!(
			self,
			Self::Io(_)
				| Self::TimedOut
				| Self::Stalled { .. }
				| Self::Protocol(_)
				| Self::Cancelled
		)
	}

	pub fn code(&self) -> Option<ErrorCode> {
		match self {
			Self::Refused { code, .. } => Some(*code),
			// Same wire refusal as `Refused { TooLarge }`, so everything
			// keyed on the code (a connection kept, a retry) agrees.
			Self::PatchTooLarge { .. } => Some(ErrorCode::TooLarge),
			_ => None,
		}
	}
}

fn transient_io(kind: io::ErrorKind) -> bool {
	matches!(
		kind,
		io::ErrorKind::UnexpectedEof
			| io::ErrorKind::ConnectionReset
			| io::ErrorKind::ConnectionAborted
			| io::ErrorKind::BrokenPipe
			| io::ErrorKind::TimedOut
			| io::ErrorKind::WouldBlock
			| io::ErrorKind::ConnectionRefused
			| io::ErrorKind::NotConnected
	)
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
