//! Every error the desktop words for the user must read as zh-TW in the
//! zh-TW UI (#222, #223). The test feeds every variant of the error types
//! the front end maps through the mapping functions, renders in zh-TW and
//! rejects a run of two ASCII words that is not data the test passed in.
//!
//! Each constructor below is an exhaustive `match`: a new variant does not
//! compile until it is listed here, and the count assertion then fails until
//! a sample of it exists. Nothing is allowed to leak: every message is
//! worded by [`reason_msg`], so a rendering with an English run, no Chinese
//! at all, or an empty slot fails the test (#225).

use super::*;
use crate::i18n::Locale;
use crate::invariants::{english_run, has_cjk};
use crate::reason::reason_msg;
use crate::PreviewErr;
use snip_core::commits::CommitError;
use snip_core::gitsrc::{Condition, GitError, StartReason};
use snip_core::reason::{Datum, Reason, StaleChange};
use snip_remote::{ConnectFailureKind, ErrorCode, RemoteError, WaitEnd};
use std::collections::BTreeSet;

/// Data the test passes in: it is user or worker text, shown as it is.
/// Only genuine data the UI is meant to echo (paths, names, numbers, a
/// message the paste itself worded) is sampled with it, because `zh()`
/// scrubs it: a mapper that echoes it instead of its own wording passes.
const D: &str = "datatoken";

/// Free text a mapper must REPLACE with its own wording (a worker's refusal
/// message, a start failure's message): multi-word English that `zh()` does
/// not scrub, so a mapper that echoes it is a leak.
const W: &str = "permission denied by worker";

/// The sample text of a refusal `code`'s message: the worker's English,
/// which no mapper may show.
fn refusal_text(_code: ErrorCode) -> &'static str {
	W
}

/// Literals a mapper may show as data (a command to run), scrubbed like `D`.
const ALLOWED_LITERALS: &[&str] = &["git fetch --unshallow"];

const ALL_CODES: [ErrorCode; 15] = [
	ErrorCode::Forbidden,
	ErrorCode::NotFound,
	ErrorCode::Unsupported,
	ErrorCode::BadRequest,
	ErrorCode::TooLarge,
	ErrorCode::Io,
	ErrorCode::VersionMismatch,
	ErrorCode::NotARepository,
	ErrorCode::InvalidRevision,
	ErrorCode::Timeout,
	ErrorCode::Busy,
	ErrorCode::Cancelled,
	ErrorCode::OutsideShare,
	ErrorCode::Stale,
	ErrorCode::Collision,
];
const ALL_REASONS: [StartReason; 6] = [
	StartReason::NoWorker,
	StartReason::KeyLogin,
	StartReason::NoAnswer,
	StartReason::Closed,
	StartReason::NoCommand,
	StartReason::CannotRun,
];
const ALL_CONDITIONS: [Condition; 5] = [
	Condition::GitmodulesNotFile,
	Condition::PathNotInSource,
	Condition::PathNotUtf8,
	Condition::CommitPathNotUtf8,
	Condition::GitPathOutput,
];

fn code_index(code: ErrorCode) -> usize {
	match code {
		ErrorCode::Forbidden => 0,
		ErrorCode::NotFound => 1,
		ErrorCode::Unsupported => 2,
		ErrorCode::BadRequest => 3,
		ErrorCode::TooLarge => 4,
		ErrorCode::Io => 5,
		ErrorCode::VersionMismatch => 6,
		ErrorCode::NotARepository => 7,
		ErrorCode::InvalidRevision => 8,
		ErrorCode::Timeout => 9,
		ErrorCode::Busy => 10,
		ErrorCode::Cancelled => 11,
		ErrorCode::OutsideShare => 12,
		ErrorCode::Stale => 13,
		ErrorCode::Collision => 14,
	}
}

fn reason_index(reason: StartReason) -> usize {
	match reason {
		StartReason::NoWorker => 0,
		StartReason::KeyLogin => 1,
		StartReason::NoAnswer => 2,
		StartReason::Closed => 3,
		StartReason::NoCommand => 4,
		StartReason::CannotRun => 5,
	}
}

/// The variant of a `RemoteError`: exhaustive on purpose.
fn remote_variant(e: &RemoteError) -> usize {
	match e {
		RemoteError::Io(_) => 0,
		RemoteError::Start { .. } => 1,
		RemoteError::Refused { .. } => 2,
		RemoteError::PatchTooLarge { .. } => 3,
		RemoteError::Protocol(_) => 4,
		RemoteError::Cancelled => 5,
		RemoteError::TimedOut => 6,
		RemoteError::Stalled { .. } => 7,
		RemoteError::WorkerTooOld { .. } => 8,
	}
}
const REMOTE_VARIANTS: usize = 9;

/// The variant of a `GitError`: exhaustive on purpose.
fn git_variant(e: &GitError) -> usize {
	match e {
		GitError::NotFound => 0,
		GitError::Failed { .. } => 1,
		GitError::Timeout { .. } => 2,
		GitError::Cancelled { .. } => 3,
		GitError::QueueTimeout { .. } => 4,
		GitError::QueueFull { .. } => 5,
		GitError::WorktreeBusy { .. } => 6,
		GitError::OutputLimit { .. } => 7,
		GitError::PatchLimit { .. } => 8,
		GitError::OutputHeldOpen { .. } => 9,
		GitError::NestedProcess { .. } => 10,
		GitError::Cleanup { .. } => 11,
		GitError::NotARepository(_) => 12,
		GitError::ToplevelAbove(_) => 13,
		GitError::InvalidRevision(_) => 14,
		GitError::Shallow(_) => 15,
		GitError::Malformed { .. } => 16,
		GitError::OutsideBoundary { .. } => 17,
		GitError::VerifyLimit { .. } => 18,
		GitError::Remote { .. } => 19,
		GitError::Io(_) => 20,
		GitError::Diagnostic(_) => 21,
		GitError::Condition(_) => 22,
		GitError::NotAFile { .. } => 23,
	}
}
const GIT_VARIANTS: usize = 24;

fn preview_variant(e: &PreviewErr) -> usize {
	match e {
		PreviewErr::TooLarge => 0,
		PreviewErr::PatchTooLarge => 1,
		PreviewErr::Failed(_) => 2,
	}
}
const PREVIEW_VARIANTS: usize = 3;

fn p() -> PathBuf {
	PathBuf::from(D)
}

/// Every `RemoteError` variant, `Refused` with every code, `Start` with
/// every reason and with none, `Io` as a timeout and as another kind.
fn remote_samples() -> Vec<(String, RemoteError)> {
	let mut v: Vec<(String, RemoteError)> = vec![
		(
			"Io(TimedOut)".into(),
			RemoteError::Io(std::io::Error::new(
				std::io::ErrorKind::TimedOut,
				D,
			)),
		),
		(
			"Io(Other)".into(),
			RemoteError::Io(std::io::Error::other(D)),
		),
		(
			"Start(None)".into(),
			RemoteError::Start {
				kind: ConnectFailureKind::Other,
				reason: None,
				detail: Datum::for_test(D),
				message: D.into(),
			},
		),
		(
			"PatchTooLarge".into(),
			RemoteError::PatchTooLarge { message: D.into() },
		),
		("Protocol".into(), RemoteError::Protocol(D.into())),
		("Cancelled".into(), RemoteError::Cancelled),
		("TimedOut".into(), RemoteError::TimedOut),
		(
			"Stalled".into(),
			RemoteError::Stalled {
				silent: 1.0,
				received: 2,
				elapsed: 3.0,
				ended: WaitEnd::Silence,
			},
		),
		(
			"Stalled(Deadline)".into(),
			RemoteError::Stalled {
				silent: 1.0,
				received: 2,
				elapsed: 3.0,
				ended: WaitEnd::Deadline,
			},
		),
		(
			"WorkerTooOld".into(),
			RemoteError::WorkerTooOld {
				worker: D.into(),
				have: 1,
				need: 2,
			},
		),
	];
	for reason in ALL_REASONS {
		v.push((
			format!("Start(Some#{})", reason_index(reason)),
			RemoteError::Start {
				kind: ConnectFailureKind::Other,
				reason: Some(reason),
				detail: Datum::for_test(D),
				message: W.into(),
			},
		));
	}
	for code in ALL_CODES {
		v.push((
			format!("Refused#{}", code_index(code)),
			RemoteError::Refused {
				code,
				message: refusal_text(code).into(),
				reason: None,
				suggestion: None,
			},
		));
	}
	// The English the remote layer raises itself (#233): none of it is the
	// OS's text, so none of it may reach a zh-TW screen.
	for (label, e) in english_io_errors() {
		v.push((format!("Io(english: {label})"), RemoteError::Io(e)));
	}
	v
}

/// `io::Error`s whose text is a sentence of ours, std's or a library's, as
/// the remote layer and the stream readers raise them.
fn english_io_errors() -> Vec<(&'static str, std::io::Error)> {
	use std::io::{Error, ErrorKind};
	let frame = |len: u32, body: &[u8]| {
		let mut bytes = len.to_be_bytes().to_vec();
		bytes.extend_from_slice(body);
		snip_remote::proto::read_frame::<snip_remote::Response>(&mut &bytes[..])
			.unwrap_err()
	};
	vec![
		(
			"worker closed",
			Error::new(
				ErrorKind::UnexpectedEof,
				"the worker closed the connection",
			),
		),
		(
			"writer abandoned",
			Error::new(
				ErrorKind::BrokenPipe,
				"the transport's writer was abandoned",
			),
		),
		("frame too large", frame(0x7fff_ffff, b"x")),
		("frame not json", frame(3, b"abc")),
		("frame cut short", frame(10, b"abc")),
		(
			"request too large",
			Error::new(ErrorKind::InvalidData, "request of 9 bytes exceeds 8"),
		),
	]
}

fn git_samples() -> Vec<(String, GitError)> {
	let a = || D.to_string();
	let mut v: Vec<(String, GitError)> = vec![
		("NotFound".into(), GitError::NotFound),
		(
			"Failed".into(),
			GitError::Failed {
				args: a(),
				stderr: a(),
				code: Some(1),
			},
		),
		("Timeout".into(), GitError::Timeout { args: a(), secs: 1 }),
		("Cancelled".into(), GitError::Cancelled { args: a() }),
		("QueueTimeout".into(), GitError::QueueTimeout { args: a() }),
		("QueueFull".into(), GitError::QueueFull { args: a() }),
		("WorktreeBusy".into(), GitError::WorktreeBusy { args: a() }),
		(
			"OutputLimit".into(),
			GitError::OutputLimit {
				args: a(),
				limit: 1,
			},
		),
		(
			"PatchLimit".into(),
			GitError::PatchLimit {
				path: a(),
				limit: 1,
			},
		),
		(
			"OutputHeldOpen".into(),
			GitError::OutputHeldOpen { args: a() },
		),
		(
			"NestedProcess".into(),
			GitError::NestedProcess { args: a() },
		),
		(
			"Cleanup".into(),
			GitError::Cleanup {
				args: a(),
				message: a(),
			},
		),
		("NotARepository".into(), GitError::NotARepository(p())),
		("ToplevelAbove".into(), GitError::ToplevelAbove(p())),
		("InvalidRevision".into(), GitError::InvalidRevision(a())),
		("Shallow".into(), GitError::Shallow(a())),
		(
			"Malformed".into(),
			GitError::Malformed {
				what: "label",
				output: a(),
			},
		),
		(
			"NotAFile".into(),
			GitError::NotAFile {
				spec: a(),
				kind: a(),
			},
		),
		// Its text is a technical label for a bug report (classified as
		// data next to `reason_msg`), shown verbatim like `Malformed`'s.
		("Diagnostic".into(), GitError::Diagnostic(D)),
		(
			"OutsideBoundary".into(),
			GitError::OutsideBoundary { what: "datatoken" },
		),
		(
			"VerifyLimit".into(),
			GitError::VerifyLimit { what: "datatoken" },
		),
		("Host".into(), GitError::remote(Reason::Protocol, a())),
		("Io".into(), GitError::Io(std::io::Error::other(D))),
		// English that is not data (#233): std's, a library's and ours.
		(
			"Io(english: pipe reader)".into(),
			GitError::Io(std::io::Error::other("Git pipe reader panicked")),
		),
		(
			"Io(english: read_exact)".into(),
			GitError::Io(
				std::io::Read::read_exact(&mut &b""[..], &mut [0u8; 1])
					.unwrap_err(),
			),
		),
		(
			"Io(english: not a regular file)".into(),
			GitError::Io(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"Not a regular file",
			)),
		),
		(
			"Cleanup(english)".into(),
			GitError::Cleanup {
				args: D.into(),
				message: "cannot signal process group after root process was \
				          reaped"
					.into(),
			},
		),
	];
	for condition in ALL_CONDITIONS {
		v.push((
			format!("Condition#{condition:?}"),
			GitError::Condition(condition),
		));
	}
	for reason in ALL_REASONS {
		v.push((
			format!("HostStart#{}", reason_index(reason)),
			GitError::remote(
				Reason::Start {
					start: Some(reason),
					detail: Datum::for_test(D),
				},
				W,
			),
		));
	}
	for code in ALL_CODES {
		v.push((
			format!("HostRefused#{}", code_index(code)),
			GitError::remote(Reason::Refused { code }, refusal_text(code)),
		));
	}
	v
}

/// Rendered in zh-TW. [`check`] cuts the data the test passed in out, so
/// the English words around a datum join into a run.
fn zh(msg: &Msg) -> String {
	msg.render(Locale::ZhTw)
}

/// `text` with the data the test passed in cut out.
fn scrub(text: &str) -> String {
	let mut out = text.replace(D, "");
	for lit in ALLOWED_LITERALS {
		out = out.replace(lit, "");
	}
	out
}

/// An empty slot in a rendering: an empty quote pair or bracket pair, a
/// trailing or doubled separator.
fn has_empty_slot(text: &str) -> bool {
	let t = text.trim_end();
	text.contains("「」")
		|| text.contains("（）")
		|| text.contains("()")
		|| text.contains("''")
		|| text.contains("\"\"")
		|| text.contains("：：")
		|| text.contains(": :")
		|| t.ends_with('：')
		|| t.ends_with(':')
		|| t.ends_with('、')
		|| t.ends_with(',')
}

/// What the mappers produced, by label.
#[derive(Default)]
struct Found {
	/// English runs the data scrub does not cover.
	leaks: BTreeSet<String>,
	/// No text, or no Chinese character, once the data is scrubbed: a mapper
	/// that dropped its wording, or showed only data.
	no_wording: BTreeSet<String>,
	/// An empty quote pair, bracket pair or separator.
	empties: BTreeSet<String>,
}

fn check(label: String, text: &str, found: &mut Found) {
	let bare = scrub(text);
	let text = text.replace(D, "x");
	let text = text.as_str();
	if english_run(&bare).is_some() {
		found.leaks.insert(label.clone());
	}
	if !has_cjk(&bare) {
		found.no_wording.insert(label.clone());
	}
	if has_empty_slot(text) {
		found.empties.insert(label);
	}
}

fn preview_text(e: &PreviewErr) -> Option<String> {
	match e {
		PreviewErr::TooLarge | PreviewErr::PatchTooLarge => None,
		PreviewErr::Failed(m) => Some(zh(m)),
	}
}

/// Every dictionary key a message names, nested ones included.
fn keys_of(msg: &Msg, out: &mut Vec<String>) {
	out.push(msg.key.to_string());
	for &i in &msg.key_args {
		if let Some(arg) = msg.args.get(i) {
			out.push(arg.clone());
		}
	}
	if let Some(inner) = &msg.inner {
		keys_of(&inner.1, out);
	}
	for item in msg.list.iter().flat_map(|l| l.1.iter()) {
		keys_of(item, out);
	}
}

/// The variant of a `Reason`: exhaustive on purpose.
fn reason_variant(r: &Reason) -> usize {
	match r {
		Reason::Refused { .. } => 0,
		Reason::Start { .. } => 1,
		Reason::Disconnected { .. } => 2,
		Reason::Protocol => 3,
		Reason::WorkerTooOld { .. } => 4,
		Reason::TimedOut => 5,
		Reason::Cancelled => 6,
		Reason::GitMissing => 7,
		Reason::GitFailed { .. } => 8,
		Reason::GitBusy => 9,
		Reason::RepoBusy => 10,
		Reason::WorkerNeedsRestart => 11,
		Reason::TooLarge => 12,
		Reason::PatchTooLarge => 13,
		Reason::VerifyLimit => 14,
		Reason::NotARepository { .. } => 15,
		Reason::InsideParentRepo { .. } => 16,
		Reason::InvalidRevision { .. } => 17,
		Reason::Shallow { .. } => 18,
		Reason::OutsideShare => 19,
		Reason::Internal { .. } => 20,
		Reason::Io { .. } => 21,
		Reason::NotFound { .. } => 22,
		Reason::PermissionDenied { .. } => 23,
		Reason::EmptySelection => 24,
		Reason::UnsafePath { .. } => 25,
		Reason::NotRegularFile { .. } => 26,
		Reason::OutsideRoot { .. } => 27,
		Reason::Collision { .. } => 28,
		Reason::StagedAndWorking { .. } => 29,
		Reason::AmbiguousRoots { .. } => 30,
		Reason::PayloadTooLarge { .. } => 31,
		Reason::SourceChanged { .. } => 32,
		Reason::DestinationChanged { .. } => 33,
		Reason::CrossRepoCommits => 34,
		Reason::Discontinuous { .. } => 35,
		Reason::NotEnoughCommits { .. } => 36,
		Reason::NotCommitPayload => 37,
		Reason::InvalidPayload { .. } => 38,
		Reason::OutsideScope { .. } => 39,
		Reason::Unverifiable { .. } => 40,
		Reason::SubmoduleNotCheckedOut => 41,
		Reason::SubmoduleUnreadable { .. } => 42,
		Reason::ScanErrorsOmitted { .. } => 43,
		Reason::DepthLimitedOmitted { .. } => 44,
		Reason::WorkerClosed => 45,
	}
}
const REASON_VARIANTS: usize = 46;

fn stale_changes() -> Vec<StaleChange> {
	vec![
		StaleChange::Created { rel: D.into() },
		StaleChange::Deleted { rel: D.into() },
		StaleChange::Modified { rel: D.into() },
		StaleChange::Repository,
		StaleChange::Preview,
	]
}

/// Every `Reason` variant, with every `ErrorCode`, `StartReason` (and none)
/// and `StaleChange`. Data fields hold `D`, so a mapper that echoes anything
/// else is a leak.
pub(crate) fn reason_samples() -> Vec<(String, Reason)> {
	let d = || D.to_string();
	let mut v: Vec<(String, Reason)> = vec![
		(
			"Start(None)".into(),
			Reason::Start {
				start: None,
				detail: Datum::for_test(D),
			},
		),
		(
			"Disconnected".into(),
			Reason::Disconnected {
				detail: Datum::for_test(D),
			},
		),
		("WorkerClosed".into(), Reason::WorkerClosed),
		("Protocol".into(), Reason::Protocol),
		(
			"WorkerTooOld".into(),
			Reason::WorkerTooOld {
				worker: d(),
				have: 1,
				need: 2,
			},
		),
		("TimedOut".into(), Reason::TimedOut),
		("Cancelled".into(), Reason::Cancelled),
		("GitMissing".into(), Reason::GitMissing),
		(
			"GitFailed".into(),
			Reason::GitFailed {
				stderr: Datum::for_test(D),
			},
		),
		("GitBusy".into(), Reason::GitBusy),
		("RepoBusy".into(), Reason::RepoBusy),
		("WorkerNeedsRestart".into(), Reason::WorkerNeedsRestart),
		("TooLarge".into(), Reason::TooLarge),
		("PatchTooLarge".into(), Reason::PatchTooLarge),
		("VerifyLimit".into(), Reason::VerifyLimit),
		(
			"NotARepository".into(),
			Reason::NotARepository { path: d() },
		),
		(
			"InsideParentRepo".into(),
			Reason::InsideParentRepo { path: d() },
		),
		(
			"InvalidRevision".into(),
			Reason::InvalidRevision { rev: d() },
		),
		("Shallow".into(), Reason::Shallow { sha: d() }),
		("OutsideShare".into(), Reason::OutsideShare),
		(
			"Internal".into(),
			Reason::Internal {
				detail: Datum::for_test(D),
				condition: None,
			},
		),
		(
			"Io".into(),
			Reason::Io {
				detail: Datum::for_test(D),
			},
		),
		("NotFound".into(), Reason::NotFound { path: d() }),
		(
			"PermissionDenied".into(),
			Reason::PermissionDenied { path: d() },
		),
		("EmptySelection".into(), Reason::EmptySelection),
		("UnsafePath".into(), Reason::UnsafePath { path: d() }),
		(
			"NotRegularFile".into(),
			Reason::NotRegularFile { path: d() },
		),
		("OutsideRoot".into(), Reason::OutsideRoot { path: d() }),
		("Collision".into(), Reason::Collision { path: d() }),
		(
			"StagedAndWorking".into(),
			Reason::StagedAndWorking { path: d() },
		),
		(
			"AmbiguousRoots".into(),
			Reason::AmbiguousRoots {
				basename: d(),
				roots: vec![d(), d()],
			},
		),
		(
			"AmbiguousRoots(wire)".into(),
			Reason::AmbiguousRoots {
				basename: d(),
				roots: Vec::new(),
			},
		),
		(
			"PayloadTooLarge".into(),
			Reason::PayloadTooLarge {
				limit: 1,
				actual: 2,
				path: Some(d()),
			},
		),
		(
			"PayloadTooLarge(no path)".into(),
			Reason::PayloadTooLarge {
				limit: 1,
				actual: 2,
				path: None,
			},
		),
		("CrossRepoCommits".into(), Reason::CrossRepoCommits),
		(
			"Discontinuous".into(),
			Reason::Discontinuous {
				base: d(),
				tip: d(),
				at: d(),
				first_parent: Some(d()),
			},
		),
		(
			"NotEnoughCommits".into(),
			Reason::NotEnoughCommits {
				requested: 3,
				available: 2,
			},
		),
		("NotCommitPayload".into(), Reason::NotCommitPayload),
		(
			"InvalidPayload".into(),
			Reason::InvalidPayload {
				kind: snip_core::reason::PayloadErrorKind::Syntax,
				line: Some(1),
				column: Some(2),
			},
		),
		(
			"OutsideScope".into(),
			Reason::OutsideScope {
				path: d(),
				scope: d(),
			},
		),
		("Unverifiable".into(), Reason::Unverifiable { path: d() }),
		(
			"SubmoduleNotCheckedOut".into(),
			Reason::SubmoduleNotCheckedOut,
		),
		(
			"SubmoduleUnreadable".into(),
			Reason::SubmoduleUnreadable {
				detail: Datum::for_test(D),
			},
		),
		(
			"ScanErrorsOmitted".into(),
			Reason::ScanErrorsOmitted { count: 3 },
		),
		(
			"DepthLimitedOmitted".into(),
			Reason::DepthLimitedOmitted { count: 3 },
		),
	];
	for condition in ALL_CONDITIONS {
		v.push((
			format!("Internal({condition:?})"),
			Reason::Internal {
				detail: Datum::default(),
				condition: Some(condition),
			},
		));
	}
	for code in ALL_CODES {
		v.push((
			format!("Refused#{}", code_index(code)),
			Reason::Refused { code },
		));
	}
	for start in ALL_REASONS {
		v.push((
			format!("Start(Some#{})", reason_index(start)),
			Reason::Start {
				start: Some(start),
				detail: Datum::for_test(D),
			},
		));
	}
	for (i, change) in stale_changes().into_iter().enumerate() {
		v.push((
			format!("SourceChanged#{i}"),
			Reason::SourceChanged {
				change: change.clone(),
			},
		));
		v.push((
			format!("DestinationChanged#{i}"),
			Reason::DestinationChanged { change },
		));
	}
	v
}

/// Every `TransferError` variant: exhaustive on purpose.
fn transfer_variant(e: &TransferError) -> usize {
	match e {
		TransferError::EmptySelection => 0,
		TransferError::UnsafePath(_) => 1,
		TransferError::SpecialFile(_) => 2,
		TransferError::DestinationNotRegular(_) => 3,
		TransferError::PathNotFound(_) => 4,
		TransferError::PathOutsideRoot(_) => 5,
		TransferError::UnknownRoot(_) => 6,
		TransferError::StagedWorkingConflict { .. } => 7,
		TransferError::MixedSourceCollision { .. } => 8,
		TransferError::AmbiguousRootBasenames { .. } => 9,
		TransferError::WireHeaderCollision { .. } => 10,
		TransferError::TargetCollision { .. } => 11,
		TransferError::PayloadLimitExceeded { .. } => 12,
		TransferError::StaleSource { .. } => 13,
		TransferError::StaleDestination { .. } => 14,
		TransferError::UnmappedEntry { .. } => 15,
		TransferError::CrossRepoCommitsNotSupported => 16,
		TransferError::DiscontinuousCommits { .. } => 17,
		TransferError::Git(_) => 18,
		TransferError::Commit(_) => 19,
		TransferError::Io(_) => 20,
	}
}
const TRANSFER_VARIANTS: usize = 21;

fn transfer_samples() -> Vec<(String, TransferError)> {
	let d = || D.to_string();
	let mut v: Vec<(String, TransferError)> = vec![
		("EmptySelection".into(), TransferError::EmptySelection),
		("UnsafePath".into(), TransferError::UnsafePath(d())),
		("SpecialFile".into(), TransferError::SpecialFile(d())),
		(
			"DestinationNotRegular".into(),
			TransferError::DestinationNotRegular(d()),
		),
		("PathNotFound".into(), TransferError::PathNotFound(p())),
		(
			"PathOutsideRoot".into(),
			TransferError::PathOutsideRoot(p()),
		),
		("UnknownRoot".into(), TransferError::UnknownRoot(p())),
		(
			"StagedWorkingConflict".into(),
			TransferError::StagedWorkingConflict {
				root: p(),
				path: d(),
			},
		),
		(
			"MixedSourceCollision".into(),
			TransferError::MixedSourceCollision {
				root: p(),
				path: d(),
				msg: W.into(),
			},
		),
		(
			"AmbiguousRootBasenames".into(),
			TransferError::AmbiguousRootBasenames {
				basename: d(),
				roots: vec![p(), p()],
			},
		),
		(
			"WireHeaderCollision".into(),
			TransferError::WireHeaderCollision { wire_path: d() },
		),
		(
			"TargetCollision".into(),
			TransferError::TargetCollision {
				path: p(),
				msg: W.into(),
			},
		),
		(
			"PayloadLimitExceeded".into(),
			TransferError::PayloadLimitExceeded {
				limit: 1,
				actual: 2,
				reason: W.into(),
				path: Some(d()),
			},
		),
		(
			"UnmappedEntry".into(),
			TransferError::UnmappedEntry { raw_path: d() },
		),
		(
			"CrossRepoCommitsNotSupported".into(),
			TransferError::CrossRepoCommitsNotSupported,
		),
		(
			"DiscontinuousCommits".into(),
			TransferError::DiscontinuousCommits {
				base: d(),
				tip: d(),
				at: d(),
				first_parent: Some(d()),
			},
		),
		(
			"Git".into(),
			TransferError::Git(GitError::Failed {
				args: d(),
				stderr: d(),
				code: Some(1),
			}),
		),
		("Io".into(), TransferError::Io(std::io::Error::other(D))),
	];
	for (i, change) in stale_changes().into_iter().enumerate() {
		v.push((
			format!("StaleSource#{i}"),
			TransferError::StaleSource {
				root: p(),
				reason: W.into(),
				change: change.clone(),
			},
		));
		v.push((
			format!("StaleDestination#{i}"),
			TransferError::StaleDestination {
				root: p(),
				reason: W.into(),
				change,
			},
		));
	}
	for (label, e) in commit_samples() {
		v.push((format!("Commit({label})"), TransferError::Commit(e)));
	}
	v
}

/// Every `CommitError` variant: exhaustive on purpose.
fn commit_variant(e: &CommitError) -> usize {
	match e {
		CommitError::Git(_) => 0,
		CommitError::Empty => 1,
		CommitError::NotEnoughCommits { .. } => 2,
		CommitError::Discontinuous { .. } => 3,
		CommitError::NotCommitPayload => 4,
		CommitError::InvalidPayload(_) => 5,
		CommitError::OutsideScope { .. } => 6,
		CommitError::PayloadLimit { .. } => 7,
	}
}
const COMMIT_VARIANTS: usize = 8;

fn commit_samples() -> Vec<(String, CommitError)> {
	let d = || D.to_string();
	vec![
		(
			"Git".into(),
			CommitError::Git(GitError::Failed {
				args: d(),
				stderr: d(),
				code: Some(1),
			}),
		),
		("Empty".into(), CommitError::Empty),
		(
			"NotEnoughCommits".into(),
			CommitError::NotEnoughCommits {
				requested: 3,
				available: 2,
			},
		),
		(
			"Discontinuous".into(),
			CommitError::Discontinuous {
				base: d(),
				tip: d(),
				at: d(),
				first_parent: None,
			},
		),
		("NotCommitPayload".into(), CommitError::NotCommitPayload),
		(
			"InvalidPayload".into(),
			CommitError::InvalidPayload(
				snip_core::commits::PayloadFault::preview_row(),
			),
		),
		(
			"OutsideScope".into(),
			CommitError::OutsideScope {
				path: d(),
				scope: p(),
			},
		),
		(
			"PayloadLimit".into(),
			CommitError::PayloadLimit {
				limit: 1,
				actual: 2,
			},
		),
	]
}

#[test]
fn every_error_the_desktop_words_reads_as_zh_tw() {
	let mut found = Found::default();

	let remotes: Vec<_> = remote_samples().into_iter().collect();
	let seen: BTreeSet<_> =
		remotes.iter().map(|(_, e)| remote_variant(e)).collect();
	assert_eq!(
		seen.len(),
		REMOTE_VARIANTS,
		"a RemoteError sample is missing"
	);
	let seen: BTreeSet<_> = remotes
		.iter()
		.filter_map(|(_, e)| match e {
			RemoteError::Refused { code, .. } => Some(code_index(*code)),
			_ => None,
		})
		.collect();
	assert_eq!(seen.len(), ALL_CODES.len());

	let gits: Vec<_> = git_samples();
	let seen: BTreeSet<_> = gits.iter().map(|(_, e)| git_variant(e)).collect();
	assert_eq!(seen.len(), GIT_VARIANTS, "a GitError sample is missing");

	let seen: BTreeSet<_> = transfer_samples()
		.iter()
		.map(|(_, e)| transfer_variant(e))
		.collect();
	assert_eq!(
		seen.len(),
		TRANSFER_VARIANTS,
		"a TransferError sample is missing"
	);
	let seen: BTreeSet<_> = commit_samples()
		.iter()
		.map(|(_, e)| commit_variant(e))
		.collect();
	assert_eq!(
		seen.len(),
		COMMIT_VARIANTS,
		"a CommitError sample is missing"
	);

	let remote_rebuild = |label: &str| {
		remote_samples()
			.into_iter()
			.find(|(l, _)| l == label)
			.unwrap()
			.1
	};
	let git_rebuild = |label: &str| {
		git_samples()
			.into_iter()
			.find(|(l, _)| l == label)
			.unwrap()
			.1
	};
	let transfer_rebuild = |label: &str| {
		transfer_samples()
			.into_iter()
			.find(|(l, _)| l == label)
			.unwrap()
			.1
	};

	for (label, _) in &remotes {
		let l = |mapper: &str| format!("{mapper}/{label}");
		check(
			l("describe_msg"),
			&zh(&crate::remote::describe_msg(remote_rebuild(label))),
			&mut found,
		);
		check(
			l("describe_msg_at"),
			&zh(&crate::remote::describe_msg_at(remote_rebuild(label), D)),
			&mut found,
		);
		for (name, wrap) in [
			(
				"remote",
				PasteError::Remote as fn(RemoteError) -> PasteError,
			),
			("unknown", PasteError::OutcomeUnknown),
		] {
			let path = Path::new(D);
			check(
				l(&format!("destination_msg[{name}]")),
				&zh(&destination_msg(None, path, wrap(remote_rebuild(label)))),
				&mut found,
			);
			check(
				l(&format!("file_apply_msg[{name}]")),
				&zh(&file_apply_msg(None, path, wrap(remote_rebuild(label)))),
				&mut found,
			);
			for applying in [false, true] {
				check(
					l(&format!("commit_msg[{name},{applying}]")),
					&zh(&commit_msg(
						None,
						path,
						wrap(remote_rebuild(label)),
						applying,
					)),
					&mut found,
				);
			}
		}
	}

	let mut previews = Vec::new();
	for (label, _) in &gits {
		check(
			format!("git_error_msg/{label}"),
			&zh(&crate::remote::git_error_msg(git_rebuild(label))),
			&mut found,
		);
		let preview = PreviewErr::git(git_rebuild(label));
		// The two sizes are worded by the preview pane, not by a message.
		if let Some(text) = preview_text(&preview) {
			check(format!("PreviewErr::git/{label}"), &text, &mut found);
		}
		previews.push(preview);
		for applying in [false, true] {
			check(
				format!("commit_msg[local git,{applying}]/{label}"),
				&zh(&commit_msg(
					None,
					Path::new(D),
					PasteError::Local(TransferError::Git(git_rebuild(label))),
					applying,
				)),
				&mut found,
			);
		}
		check(
			format!("file_apply_msg[local git]/{label}"),
			&zh(&file_apply_msg(
				None,
				Path::new(D),
				PasteError::Local(TransferError::Git(git_rebuild(label))),
			)),
			&mut found,
		);
	}
	previews.push(PreviewErr::from(std::io::Error::other(D)));
	previews.push(PreviewErr::from(std::io::Error::from(
		std::io::ErrorKind::FileTooLarge,
	)));
	let seen: BTreeSet<_> = previews.iter().map(preview_variant).collect();
	assert_eq!(
		seen.len(),
		PREVIEW_VARIANTS,
		"a PreviewErr sample is missing"
	);
	// `Failed` from an I/O error that is not the size limit.
	let io_preview = &previews[previews.len() - 2];
	check(
		"PreviewErr::from(io)/Other".into(),
		&preview_text(io_preview).unwrap(),
		&mut found,
	);

	for (label, _) in transfer_samples() {
		for (mapper, text) in [
			(
				"destination_msg[local]",
				zh(&destination_msg(
					None,
					Path::new(D),
					PasteError::Local(transfer_rebuild(&label)),
				)),
			),
			(
				"file_apply_msg[local]",
				zh(&file_apply_msg(
					None,
					Path::new(D),
					PasteError::Local(transfer_rebuild(&label)),
				)),
			),
			(
				"commit_msg[local,false]",
				zh(&commit_msg(
					None,
					Path::new(D),
					PasteError::Local(transfer_rebuild(&label)),
					false,
				)),
			),
			(
				"commit_msg[local,true]",
				zh(&commit_msg(
					None,
					Path::new(D),
					PasteError::Local(transfer_rebuild(&label)),
					true,
				)),
			),
		] {
			check(format!("{mapper}/{label}"), &text, &mut found);
		}
	}

	for (name, err) in [
		(
			"Destination",
			PasteError::Destination {
				path: p(),
				source: std::io::Error::other(D),
			},
		),
		("NoReplayResult", PasteError::NoReplayResult),
	] {
		check(
			format!("destination_msg/{name}"),
			&zh(&destination_msg(None, Path::new(D), err)),
			&mut found,
		);
	}

	assert!(
		found.leaks.is_empty(),
		"English leaks into zh-TW: {:#?}",
		found.leaks
	);
	assert!(
		found.no_wording.is_empty(),
		"samples with no Chinese wording in zh-TW (give them wording): {:#?}",
		found.no_wording
	);
	assert!(
		found.empties.is_empty(),
		"samples with an empty slot: {:#?}",
		found.empties
	);
}

/// `reason_msg` is the single translation point: every `Reason`, as the
/// producer builds it and as a worker sends it (`beside_code`), words in both
/// languages with every key in both dictionaries, no English run in zh-TW,
/// Chinese in every zh-TW rendering, no CJK in English, and no empty slot.
#[test]
fn every_reason_is_worded_in_both_languages() {
	let samples = reason_samples();
	let seen: BTreeSet<_> =
		samples.iter().map(|(_, r)| reason_variant(r)).collect();
	assert_eq!(seen.len(), REASON_VARIANTS, "a Reason sample is missing");
	assert_eq!(seen.iter().max(), Some(&(REASON_VARIANTS - 1)));

	let mut found = Found::default();
	for (label, reason) in &samples {
		let wire = reason.clone().beside_code();
		for (label, reason) in std::iter::once((label.clone(), reason.clone()))
			.chain(wire.map(|w| (format!("{label}/beside_code"), w)))
		{
			let msg = reason_msg(&reason);
			check(label.clone(), &zh(&msg), &mut found);
			let en = msg.render(Locale::En).replace(D, "x");
			assert!(!has_cjk(&en), "{label}: CJK in English: {en}");
			assert!(!has_empty_slot(&en), "{label}: empty slot in {en}");
			assert!(!en.trim().is_empty(), "{label}: blank English");
			let mut keys = Vec::new();
			keys_of(&msg, &mut keys);
			for key in keys {
				for loc in [Locale::ZhTw, Locale::En] {
					assert!(
						!crate::i18n::t(&key, loc).is_empty(),
						"{label}: key {key} missing in {loc:?}"
					);
				}
			}
		}
	}
	// A failed file of a restore is worded through the same reasons, alone
	// and in the result card.
	for (label, reason) in &samples {
		let failure = |path: &str| snip_core::restore::RestoreFailure {
			path: path.into(),
			reason: Some(reason.clone()),
		};
		let files = |n: usize| snip_core::restore::RestoreExecutionResult {
			errors: vec!["english".into(); n],
			failures: (0..n).map(|i| failure(&format!("f{i}.txt"))).collect(),
			..Default::default()
		};
		for n in [1, 2] {
			let status = PasteApplyResult {
				files: files(n),
				created_commits: Vec::new(),
			}
			.status();
			check(format!("restore/{label}/{n}"), &zh(&status), &mut found);
			let en = status.render(Locale::En).replace(D, "x");
			assert!(!has_empty_slot(&en), "{label}: empty slot in {en}");
			assert!(!en.contains("english"), "{label}: {en}");
		}
	}

	assert!(found.leaks.is_empty(), "English leaks: {:#?}", found.leaks);
	assert!(
		found.no_wording.is_empty(),
		"no Chinese wording: {:#?}",
		found.no_wording
	);
	assert!(
		found.empties.is_empty(),
		"empty slots: {:#?}",
		found.empties
	);
}

/// A shallow clone's recovery command stays in the message as data.
#[test]
fn a_shallow_clone_keeps_its_recovery_command() {
	let msg = reason_msg(&Reason::Shallow { sha: D.into() });
	for loc in [Locale::ZhTw, Locale::En] {
		let text = msg.render(loc);
		assert!(text.contains("git fetch --unshallow"), "{text}");
	}
}

#[test]
fn the_leak_detector_sees_a_run_of_english_words() {
	assert!(english_run("遠端 the worker did not answer").is_some());
	assert!(english_run("無法讀取 snip-sync 的 worker 輸出").is_none());
	assert!(english_run(" ssh 無法登入").is_none());
	assert!(english_run("git  failed: ").is_some());
}

#[test]
fn the_empty_slot_detector_sees_a_missing_datum() {
	assert!(has_empty_slot("「」在開啟的資料夾之外"));
	assert!(has_empty_slot("上限（）"));
	assert!(has_empty_slot("操作逾時："));
	assert!(!has_empty_slot("操作逾時：/x"));
}

/// Every String a [`Reason`] carries, as `variant.field` (nested tags
/// skipped), from the JSON of all samples.
fn string_fields() -> BTreeSet<String> {
	fn walk(prefix: &str, v: &serde_json::Value, out: &mut BTreeSet<String>) {
		match v {
			serde_json::Value::String(_) => {
				out.insert(prefix.to_string());
			}
			serde_json::Value::Array(items) => {
				for i in items {
					walk(prefix, i, out);
				}
			}
			serde_json::Value::Object(map) => {
				for (k, v) in map {
					// The serde tag names a variant, and `kind`, `code`,
					// `start` and `condition` are closed enums: neither is free text.
					if !matches!(
						k.as_str(),
						"r" | "kind" | "code" | "start" | "condition"
					) {
						walk(&format!("{prefix}.{k}"), v, out);
					}
				}
			}
			_ => {}
		}
	}
	let mut out = BTreeSet::new();
	for (_, r) in reason_samples() {
		let json = serde_json::to_value(&r).unwrap();
		let tag = json["r"].as_str().unwrap().to_string();
		walk(&tag, &json, &mut out);
	}
	out
}

/// Every String field of every `Reason` is either data the screen shows
/// verbatim or not there: the classification lives next to `reason_msg`
/// (`crate::reason`). A new String field fails here until it is classified
/// as data (listed below) or made typed. A library's English message is
/// never data: `InvalidPayload` once held `serde_json`'s text and a zh-TW
/// screen showed it.
#[test]
fn string_fields_are_classified() {
	const DATA: &[&str] = &[
		"ambiguous_roots.basename",
		"ambiguous_roots.roots",
		"collision.path",
		"destination_changed.change.rel",
		"discontinuous.at",
		"discontinuous.base",
		"discontinuous.first_parent",
		"discontinuous.tip",
		"disconnected.detail",
		"git_failed.stderr",
		"inside_parent_repo.path",
		"internal.detail",
		"invalid_revision.rev",
		"io.detail",
		"not_a_repository.path",
		"not_found.path",
		"not_regular_file.path",
		"outside_root.path",
		"outside_scope.path",
		"outside_scope.scope",
		"payload_too_large.path",
		"permission_denied.path",
		"shallow.sha",
		"source_changed.change.rel",
		"staged_and_working.path",
		"start.detail",
		"submodule_unreadable.detail",
		"unsafe_path.path",
		"unverifiable.path",
		"worker_too_old.worker",
	];
	let actual = string_fields();
	let expected: BTreeSet<String> =
		DATA.iter().map(|s| s.to_string()).collect();
	assert_eq!(
		actual, expected,
		"a Reason String field is unclassified: data (list it) or typed"
	);
}

/// A payload that does not parse is worded in the interface language
/// whatever `serde_json` said: no English from the library reaches zh-TW.
#[test]
fn an_invalid_payload_is_worded_without_the_librarys_text() {
	use snip_core::commits::{parse_commit_payload, CommitError};
	let parse_error = |body: &str| match parse_commit_payload(&format!(
		"// snip-sync commits v1\n{body}"
	)) {
		Err(CommitError::InvalidPayload(f)) => f,
		other => panic!("expected InvalidPayload, got {other:?}"),
	};
	let mut kinds = BTreeSet::new();
	for body in ["{bad", "{\"a\":", "[]", "{\"commits\":7}", ""] {
		let fault = parse_error(body);
		kinds.insert(format!("{:?}", fault.kind));
		let reason = CommitError::InvalidPayload(fault.clone()).reason();
		let msg = reason_msg(&reason);
		let mut found = Found::default();
		check(format!("payload {body:?}"), &zh(&msg), &mut found);
		assert!(found.leaks.is_empty(), "{:#?}", found.leaks);
		let en = msg.render(Locale::En);
		assert!(!en.contains(&fault.text), "{en}");
		assert!(!zh(&msg).contains(&fault.text), "{}", zh(&msg));
		// The CLI keeps the library's text.
		assert_eq!(
			CommitError::InvalidPayload(fault.clone()).to_string(),
			format!("invalid commits payload: {}", fault.text)
		);
	}
	assert!(
		kinds.len() >= 2,
		"the probes cover several kinds: {kinds:?}"
	);
}

/// An English sentence of ours, as a producer once put it into a data field.
const PRODUCT_SENTENCE: &str = "the folder is not a regular file";

/// The String fields classified as data whose producer is, or once was,
/// able to carry a sentence of our own (not a path, a name or git's or the
/// OS's words): the six [`snip_core::reason::Datum`] fields. Each one must
/// have its sentences typed at the producer.
pub(crate) const PRODUCT_TEXT_FIELDS: &[&str] = &[
	"disconnected.detail",
	"git_failed.stderr",
	"internal.detail",
	"io.detail",
	"start.detail",
	"submodule_unreadable.detail",
];

/// `sample` with the String at `field` (`variant.field`) replaced.
fn with_field(sample: &Reason, field: &str, text: &str) -> Option<Reason> {
	let mut json = serde_json::to_value(sample).ok()?;
	let (variant, key) = field.split_once('.')?;
	if json["r"].as_str()? != variant {
		return None;
	}
	json[key] = serde_json::Value::String(text.into());
	serde_json::from_value(json).ok()
}

/// A data field echoes what it is given, so the only guard is at the
/// producer: a sentence of ours must be typed, never stored as data. This
/// pins both halves. (1) The leak detector does see such a sentence in each
/// field that could carry one, so a producer that regresses to a string
/// fails the zh-TW sweep. (2) Every producer known to have said such a
/// sentence now words it from a type: nothing of its English text reaches
/// zh-TW or the data slot.
#[test]
fn product_text_never_reaches_a_data_field() {
	let samples = reason_samples();
	for field in PRODUCT_TEXT_FIELDS {
		let mut tried = 0;
		for (label, sample) in &samples {
			let Some(reason) = with_field(sample, field, PRODUCT_SENTENCE)
			else {
				continue;
			};
			// A sample whose wording does not show the field is no test.
			let other = with_field(sample, field, "x").unwrap();
			if zh(&reason_msg(&other)) == zh(&reason_msg(&reason)) {
				continue;
			}
			tried += 1;
			let mut found = Found::default();
			check(
				format!("{field} <- {label}"),
				&zh(&reason_msg(&reason)),
				&mut found,
			);
			assert!(
				!found.leaks.is_empty(),
				"{field} <- {label}: an English sentence in the field went \
				 unnoticed"
			);
		}
		assert!(tried > 0, "{field}: no sample to test");
	}

	// Every classified product-text field is in the data list: the list
	// above cannot name a field `string_fields_are_classified` does not know.
	let known = string_fields();
	for field in PRODUCT_TEXT_FIELDS {
		assert!(known.contains(*field), "{field} is not a Reason field");
	}

	// (2) The producers, one by one.
	for condition in ALL_CONDITIONS {
		let error = GitError::Condition(condition);
		// The CLI's bytes are unchanged.
		assert_eq!(
			error.to_string(),
			format!("unexpected git output: {}", condition.text())
		);
		let reason = error.reason();
		let Reason::Internal {
			detail,
			condition: Some(typed),
		} = &reason
		else {
			panic!("{condition:?}: not typed: {reason:?}");
		};
		assert!(detail.is_empty() && *typed == condition, "{reason:?}");
		for r in [Some(reason.clone()), reason.clone().beside_code()]
			.into_iter()
			.flatten()
		{
			let msg = reason_msg(&r);
			let mut found = Found::default();
			check(format!("{condition:?}"), &zh(&msg), &mut found);
			assert!(found.leaks.is_empty(), "{:#?}", found.leaks);
			assert!(found.no_wording.is_empty(), "no Chinese: {condition:?}");
			for loc in [Locale::ZhTw, Locale::En] {
				assert!(
					!msg.render(loc).contains(condition.text()),
					"{condition:?} shows the CLI's English in {loc:?}"
				);
			}
		}
	}
	// Our own sentences inside a git error: the producers that once wrote
	// them into `Malformed` hold only git's bytes as data now. Each keeps its
	// CLI Display, and none of the label or sentence reaches a reason.
	let sentences = [
		"rev-parse returned invalid commit OID",
		"unknown object format",
		"is a tree, not a file",
	];
	let producers = [
		(
			snip_core::gitsrc::full_oid(D).unwrap_err(),
			format!(
				"unexpected git output: rev-parse returned invalid commit \
				 OID: {D}"
			),
		),
		(
			snip_core::gitsrc::empty_tree_of(D).unwrap_err(),
			format!("unexpected git output: unknown object format: {D}"),
		),
		(
			snip_core::blob::not_a_file(D, "tree"),
			format!("unexpected git output: '{D}' is a tree, not a file"),
		),
	];
	for (error, display) in producers {
		assert_eq!(error.to_string(), display);
		let reason = error.reason();
		let json = serde_json::to_string(&reason).unwrap();
		for r in [Some(reason.clone()), reason.clone().beside_code()]
			.into_iter()
			.flatten()
		{
			let msg = reason_msg(&r);
			let mut found = Found::default();
			check(format!("{error}"), &zh(&msg), &mut found);
			assert!(found.leaks.is_empty(), "{:#?}", found.leaks);
			for sentence in sentences {
				assert!(!json.contains(sentence), "{sentence} in {json}");
				for loc in [Locale::ZhTw, Locale::En] {
					assert!(
						!msg.render(loc).contains(sentence),
						"{sentence} shown in {loc:?}"
					);
				}
			}
		}
	}
	for start in [StartReason::NoCommand, StartReason::CannotRun] {
		let error = RemoteError::Start {
			kind: ConnectFailureKind::WorkerUnavailable,
			reason: Some(start),
			detail: Datum::for_test(D),
			message: "cannot run ssh: the sentence of the CLI".into(),
		};
		let msg = crate::remote::describe_msg(error);
		let mut found = Found::default();
		check(format!("{start:?}"), &zh(&msg), &mut found);
		assert!(found.leaks.is_empty(), "{:#?}", found.leaks);
		assert!(found.no_wording.is_empty(), "no Chinese: {start:?}");
		assert!(!zh(&msg).contains("the sentence of the CLI"));
	}
}

/// Every commit graph error, with every fault inside it, as the history view
/// words it. The matches are exhaustive: a new error or fault does not
/// compile until it is listed here.
fn graph_errors() -> Vec<(String, snip_core::graph::GraphError)> {
	use snip_core::graph::{
		ConfigFault, ContinuityFault, GraphError, InputFault,
	};
	fn error_index(e: &GraphError) -> usize {
		match e {
			GraphError::InvalidConfig { .. } => 0,
			GraphError::FrontierLimitExceeded { .. } => 1,
			GraphError::RowLimitExceeded { .. } => 2,
			GraphError::RefLimitExceeded { .. } => 3,
			GraphError::ByteLimitExceeded { .. } => 4,
			GraphError::InvalidInput { .. } => 5,
			GraphError::DuplicateCommit { .. } => 6,
			GraphError::ContinuityError { .. } => 7,
		}
	}
	fn config_index(f: ConfigFault) -> usize {
		match f {
			ConfigFault::MaxFrontierSize => 0,
			ConfigFault::MaxRows => 1,
			ConfigFault::MaxRefs => 2,
			ConfigFault::MaxTotalBytes => 3,
			ConfigFault::UnitX => 4,
			ConfigFault::UnitY => 5,
			ConfigFault::OffsetX => 6,
		}
	}
	fn input_index(f: &InputFault) -> usize {
		match f {
			InputFault::CommitSha { .. } => 0,
			InputFault::ParentSha { .. } => 1,
			InputFault::RefSha { .. } => 2,
			InputFault::RefNameEmpty { .. } => 3,
			InputFault::RefNameTooLong { .. } => 4,
			InputFault::FrontierSha => 5,
			InputFault::CheckpointSha => 6,
		}
	}
	fn continuity_index(f: &ContinuityFault) -> usize {
		match f {
			ContinuityFault::RepeatsLastCommit { .. } => 0,
		}
	}
	let mut v: Vec<(String, GraphError)> = vec![
		(
			"FrontierLimit".into(),
			GraphError::FrontierLimitExceeded {
				current: 5,
				limit: 4,
			},
		),
		(
			"RowLimit".into(),
			GraphError::RowLimitExceeded {
				requested: 5,
				limit: 4,
			},
		),
		(
			"RefLimit".into(),
			GraphError::RefLimitExceeded {
				requested: 5,
				limit: 4,
			},
		),
		(
			"ByteLimit".into(),
			GraphError::ByteLimitExceeded { total: 5, limit: 4 },
		),
		(
			"Duplicate".into(),
			GraphError::DuplicateCommit {
				sha: "abc".into(),
				row: 1,
			},
		),
	];
	for field in [
		ConfigFault::MaxFrontierSize,
		ConfigFault::MaxRows,
		ConfigFault::MaxRefs,
		ConfigFault::MaxTotalBytes,
		ConfigFault::UnitX,
		ConfigFault::UnitY,
		ConfigFault::OffsetX,
	] {
		v.push((
			format!("Config#{}", config_index(field)),
			GraphError::InvalidConfig { field },
		));
	}
	for fault in [
		InputFault::CommitSha { row: 1 },
		InputFault::ParentSha {
			index: 1,
			commit: "abc".into(),
		},
		InputFault::RefSha { index: 1 },
		InputFault::RefNameEmpty { index: 1 },
		InputFault::RefNameTooLong { index: 1 },
		InputFault::FrontierSha,
		InputFault::CheckpointSha,
	] {
		v.push((
			format!("Input#{}", input_index(&fault)),
			GraphError::InvalidInput { fault },
		));
	}
	let fault = ContinuityFault::RepeatsLastCommit { sha: "abc".into() };
	v.push((
		format!("Continuity#{}", continuity_index(&fault)),
		GraphError::ContinuityError { fault },
	));
	let seen: BTreeSet<_> = v.iter().map(|(_, e)| error_index(e)).collect();
	assert_eq!(seen.len(), 8, "a GraphError has no sample");
	v
}

/// No English of the layout's reaches the zh-TW screen: its `Display` is a
/// diagnostic for the CLI and the log (#233).
#[test]
fn every_graph_error_reads_as_zh_tw() {
	let mut found = Found::default();
	for (label, error) in graph_errors() {
		let msg = crate::reason::graph_error_msg(&error);
		check(label.clone(), &zh(&msg), &mut found);
		assert!(
			!msg.render(Locale::En).contains(&error.to_string()),
			"{label}: the layout's own text reached the screen"
		);
	}
	assert!(found.leaks.is_empty(), "{:#?}", found.leaks);
	assert!(found.no_wording.is_empty(), "{:#?}", found.no_wording);
	assert!(found.empties.is_empty(), "{:#?}", found.empties);
}

/// The clipboard library's English never reaches the screen: each fault is
/// worded, and the old way of showing its text is seen by the detector.
#[test]
fn every_clipboard_fault_reads_as_zh_tw() {
	use snip_core::clip::ClipFault;
	fn fault_index(f: ClipFault) -> usize {
		match f {
			ClipFault::Empty => 0,
			ClipFault::Unsupported => 1,
			ClipFault::Occupied => 2,
			ClipFault::Conversion => 3,
			ClipFault::Other => 4,
		}
	}
	let mut found = Found::default();
	let faults = [
		ClipFault::Empty,
		ClipFault::Unsupported,
		ClipFault::Occupied,
		ClipFault::Conversion,
		ClipFault::Other,
	];
	let seen: BTreeSet<_> = faults.iter().map(|f| fault_index(*f)).collect();
	assert_eq!(seen.len(), 5);
	for fault in faults {
		let label = format!("{fault:?}");
		check(
			format!("clip_fault_msg/{label}"),
			&zh(&crate::reason::clip_fault_msg(fault)),
			&mut found,
		);
	}
	for (label, e) in [
		("empty", arboard::Error::ContentNotAvailable),
		("unsupported", arboard::Error::ClipboardNotSupported),
		("occupied", arboard::Error::ClipboardOccupied),
		("conversion", arboard::Error::ConversionFailure),
		(
			"unknown",
			arboard::Error::Unknown {
				description: "developer words".into(),
			},
		),
	] {
		check(
			format!("clipboard_failed/{label}"),
			&zh(&crate::reason::clipboard_failed(&e)),
			&mut found,
		);
		check(
			format!("clipboard_read_failed/{label}"),
			&zh(&crate::reason::clipboard_read_failed(&e)),
			&mut found,
		);
	}
	assert!(found.leaks.is_empty(), "{:#?}", found.leaks);
	assert!(found.no_wording.is_empty(), "{:#?}", found.no_wording);
	assert!(found.empties.is_empty(), "{:#?}", found.empties);

	// The detector sees the old wording: the library's text as the argument.
	let mut old = Found::default();
	let legacy = Msg::new(
		"status_clipboard_failed",
		[arboard::Error::ContentNotAvailable.to_string()],
	);
	check("legacy".into(), &zh(&legacy), &mut old);
	assert!(!old.leaks.is_empty(), "{}", zh(&legacy));
}
