//! Every error the desktop words for the user must read as zh-TW in the
//! zh-TW UI (#222, #223). The test feeds every variant of the error types
//! the front end maps through the mapping functions, renders in zh-TW and
//! rejects a run of two ASCII words that is not data the test passed in.
//!
//! Each constructor below is an exhaustive `match`: a new variant does not
//! compile until it is listed here, and the count assertion then fails until
//! a sample of it exists. A variant that still leaks goes into `KNOWN_GAPS`
//! (#225 replaces the raw fallbacks with a reason type); the list is asserted
//! exactly, so it can only shrink.

use super::*;
use crate::i18n::Locale;
use crate::PreviewErr;
use snip_core::gitsrc::{GitError, StartReason};
use snip_core::reason::Reason;
use snip_remote::{ConnectFailureKind, ErrorCode, RemoteError};
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

/// The sample text of a refusal `code`'s message: replaced by wording when
/// the code has a dictionary key, echoed (the paste's own text) when not.
fn refusal_text(code: ErrorCode) -> &'static str {
	if crate::remote::refusal_key(code).is_some() {
		W
	} else {
		D
	}
}

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
const ALL_REASONS: [StartReason; 4] = [
	StartReason::NoWorker,
	StartReason::KeyLogin,
	StartReason::NoAnswer,
	StartReason::Closed,
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
	}
}

/// The variant of a `RemoteError`: exhaustive on purpose.
fn remote_variant(e: &RemoteError) -> usize {
	match e {
		RemoteError::Io(_) => 0,
		RemoteError::Connect(_) => 1,
		RemoteError::Start { .. } => 2,
		RemoteError::Refused { .. } => 3,
		RemoteError::PatchTooLarge { .. } => 4,
		RemoteError::Protocol(_) => 5,
		RemoteError::Cancelled => 6,
		RemoteError::TimedOut => 7,
		RemoteError::Stalled { .. } => 8,
		RemoteError::WorkerTooOld { .. } => 9,
	}
}
const REMOTE_VARIANTS: usize = 10;

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
		GitError::Malformed(_) => 16,
		GitError::OutsideBoundary { .. } => 17,
		GitError::VerifyLimit { .. } => 18,
		GitError::Remote { .. } => 19,
		GitError::Io(_) => 20,
	}
}
const GIT_VARIANTS: usize = 21;

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
		("Connect".into(), RemoteError::Connect(D.into())),
		(
			"Start(None)".into(),
			RemoteError::Start {
				kind: ConnectFailureKind::Other,
				reason: None,
				detail: D.into(),
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
				detail: D.into(),
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
			},
		));
	}
	v
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
		("Malformed".into(), GitError::Malformed(a())),
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
	];
	for reason in ALL_REASONS {
		v.push((
			format!("HostStart#{}", reason_index(reason)),
			GitError::remote(
				Reason::Start {
					start: Some(reason),
					detail: a(),
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

/// Rendered in zh-TW with the data the test passed in cut out, so the
/// English words around a datum join into a run.
fn zh(msg: &Msg) -> String {
	msg.render(Locale::ZhTw).replace(D, "")
}

/// The first run of two ASCII words in `text`, if any. A word is letters,
/// digits and hyphens that starts with a letter, so `snip-sync` and `ssh`
/// are words, and a lone product term between Chinese is not a run.
fn english_run(text: &str) -> Option<String> {
	let re = regex::Regex::new(
		r"[A-Za-z][A-Za-z0-9-]*(?:[ \t]+[A-Za-z][A-Za-z0-9-]*)+",
	)
	.unwrap();
	re.find(text).map(|m| m.as_str().to_string())
}

/// What the mappers produced: labels that leak English, and labels whose
/// text is empty once the data is scrubbed (a mapper that dropped its wording).
#[derive(Default)]
struct Found {
	leaks: BTreeSet<String>,
	empty: BTreeSet<String>,
}

fn check(label: String, text: &str, found: &mut Found) {
	if english_run(text).is_some() {
		found.leaks.insert(label);
	} else if text.trim().is_empty() {
		found.empty.insert(label);
	}
}

/// Mappings that still leak English into the zh-TW UI, `mapper/variant`.
/// #225 (a reason type for the desktop) removes these; fix one and delete
/// its line, or the test fails. A new leak is not allowed in.
const KNOWN_GAPS: &[&str] = &[
	"PreviewErr::git/Cancelled",
	"PreviewErr::git/Cleanup",
	"PreviewErr::git/Failed",
	"PreviewErr::git/InvalidRevision",
	"PreviewErr::git/Malformed",
	"PreviewErr::git/NestedProcess",
	"PreviewErr::git/NotARepository",
	"PreviewErr::git/NotFound",
	"PreviewErr::git/OutputHeldOpen",
	"PreviewErr::git/OutsideBoundary",
	"PreviewErr::git/QueueFull",
	"PreviewErr::git/QueueTimeout",
	"PreviewErr::git/Shallow",
	"PreviewErr::git/Timeout",
	"PreviewErr::git/ToplevelAbove",
	"PreviewErr::git/VerifyLimit",
	"PreviewErr::git/WorktreeBusy",
	"commit_msg[local git,false]/Cancelled",
	"commit_msg[local git,false]/Cleanup",
	"commit_msg[local git,false]/Failed",
	"commit_msg[local git,false]/HostRefused#0",
	"commit_msg[local git,false]/HostRefused#1",
	"commit_msg[local git,false]/HostRefused#10",
	"commit_msg[local git,false]/HostRefused#11",
	"commit_msg[local git,false]/HostRefused#12",
	"commit_msg[local git,false]/HostRefused#2",
	"commit_msg[local git,false]/HostRefused#3",
	"commit_msg[local git,false]/HostRefused#4",
	"commit_msg[local git,false]/HostRefused#5",
	"commit_msg[local git,false]/HostRefused#6",
	"commit_msg[local git,false]/HostRefused#7",
	"commit_msg[local git,false]/HostRefused#8",
	"commit_msg[local git,false]/HostRefused#9",
	"commit_msg[local git,false]/HostStart#0",
	"commit_msg[local git,false]/HostStart#1",
	"commit_msg[local git,false]/HostStart#2",
	"commit_msg[local git,false]/HostStart#3",
	"commit_msg[local git,false]/InvalidRevision",
	"commit_msg[local git,false]/Malformed",
	"commit_msg[local git,false]/NestedProcess",
	"commit_msg[local git,false]/NotARepository",
	"commit_msg[local git,false]/NotFound",
	"commit_msg[local git,false]/OutputHeldOpen",
	"commit_msg[local git,false]/OutputLimit",
	"commit_msg[local git,false]/OutsideBoundary",
	"commit_msg[local git,false]/PatchLimit",
	"commit_msg[local git,false]/QueueFull",
	"commit_msg[local git,false]/QueueTimeout",
	"commit_msg[local git,false]/Shallow",
	"commit_msg[local git,false]/Timeout",
	"commit_msg[local git,false]/ToplevelAbove",
	"commit_msg[local git,false]/VerifyLimit",
	"commit_msg[local git,true]/Cancelled",
	"commit_msg[local git,true]/Cleanup",
	"commit_msg[local git,true]/Failed",
	"commit_msg[local git,true]/HostRefused#0",
	"commit_msg[local git,true]/HostRefused#1",
	"commit_msg[local git,true]/HostRefused#10",
	"commit_msg[local git,true]/HostRefused#11",
	"commit_msg[local git,true]/HostRefused#12",
	"commit_msg[local git,true]/HostRefused#2",
	"commit_msg[local git,true]/HostRefused#3",
	"commit_msg[local git,true]/HostRefused#4",
	"commit_msg[local git,true]/HostRefused#5",
	"commit_msg[local git,true]/HostRefused#6",
	"commit_msg[local git,true]/HostRefused#7",
	"commit_msg[local git,true]/HostRefused#8",
	"commit_msg[local git,true]/HostRefused#9",
	"commit_msg[local git,true]/HostStart#0",
	"commit_msg[local git,true]/HostStart#1",
	"commit_msg[local git,true]/HostStart#2",
	"commit_msg[local git,true]/HostStart#3",
	"commit_msg[local git,true]/InvalidRevision",
	"commit_msg[local git,true]/Malformed",
	"commit_msg[local git,true]/NestedProcess",
	"commit_msg[local git,true]/NotARepository",
	"commit_msg[local git,true]/NotFound",
	"commit_msg[local git,true]/OutputHeldOpen",
	"commit_msg[local git,true]/OutputLimit",
	"commit_msg[local git,true]/OutsideBoundary",
	"commit_msg[local git,true]/PatchLimit",
	"commit_msg[local git,true]/QueueFull",
	"commit_msg[local git,true]/QueueTimeout",
	"commit_msg[local git,true]/Shallow",
	"commit_msg[local git,true]/Timeout",
	"commit_msg[local git,true]/ToplevelAbove",
	"commit_msg[local git,true]/VerifyLimit",
	"commit_msg[remote,false]/Io(TimedOut)",
	"commit_msg[remote,false]/Stalled",
	"commit_msg[remote,false]/TimedOut",
	"commit_msg[remote,false]/WorkerTooOld",
	"commit_msg[remote,true]/Io(TimedOut)",
	"commit_msg[remote,true]/Stalled",
	"commit_msg[remote,true]/TimedOut",
	"commit_msg[remote,true]/WorkerTooOld",
	"describe_msg/Io(TimedOut)",
	"describe_msg/Stalled",
	"describe_msg/TimedOut",
	"describe_msg/WorkerTooOld",
	"describe_msg_at/Io(TimedOut)",
	"describe_msg_at/Stalled",
	"describe_msg_at/TimedOut",
	"describe_msg_at/WorkerTooOld",
	"destination_msg/NoReplayResult",
	"destination_msg[remote]/Io(TimedOut)",
	"destination_msg[remote]/Stalled",
	"destination_msg[remote]/TimedOut",
	"destination_msg[remote]/WorkerTooOld",
	"destination_msg[unknown]/Io(TimedOut)",
	"destination_msg[unknown]/Stalled",
	"destination_msg[unknown]/TimedOut",
	"destination_msg[unknown]/WorkerTooOld",
	"file_apply_msg[local git]/Cancelled",
	"file_apply_msg[local git]/Cleanup",
	"file_apply_msg[local git]/Failed",
	"file_apply_msg[local git]/HostRefused#0",
	"file_apply_msg[local git]/HostRefused#1",
	"file_apply_msg[local git]/HostRefused#10",
	"file_apply_msg[local git]/HostRefused#11",
	"file_apply_msg[local git]/HostRefused#12",
	"file_apply_msg[local git]/HostRefused#2",
	"file_apply_msg[local git]/HostRefused#3",
	"file_apply_msg[local git]/HostRefused#4",
	"file_apply_msg[local git]/HostRefused#5",
	"file_apply_msg[local git]/HostRefused#6",
	"file_apply_msg[local git]/HostRefused#7",
	"file_apply_msg[local git]/HostRefused#8",
	"file_apply_msg[local git]/HostRefused#9",
	"file_apply_msg[local git]/HostStart#0",
	"file_apply_msg[local git]/HostStart#1",
	"file_apply_msg[local git]/HostStart#2",
	"file_apply_msg[local git]/HostStart#3",
	"file_apply_msg[local git]/InvalidRevision",
	"file_apply_msg[local git]/Malformed",
	"file_apply_msg[local git]/NestedProcess",
	"file_apply_msg[local git]/NotARepository",
	"file_apply_msg[local git]/NotFound",
	"file_apply_msg[local git]/OutputHeldOpen",
	"file_apply_msg[local git]/OutputLimit",
	"file_apply_msg[local git]/OutsideBoundary",
	"file_apply_msg[local git]/PatchLimit",
	"file_apply_msg[local git]/QueueFull",
	"file_apply_msg[local git]/QueueTimeout",
	"file_apply_msg[local git]/Shallow",
	"file_apply_msg[local git]/Timeout",
	"file_apply_msg[local git]/ToplevelAbove",
	"file_apply_msg[local git]/VerifyLimit",
	"file_apply_msg[remote]/Io(TimedOut)",
	"file_apply_msg[remote]/Stalled",
	"file_apply_msg[remote]/TimedOut",
	"file_apply_msg[remote]/WorkerTooOld",
	"git_error_msg/Cancelled",
	"git_error_msg/Cleanup",
	"git_error_msg/Failed",
	"git_error_msg/InvalidRevision",
	"git_error_msg/Malformed",
	"git_error_msg/NestedProcess",
	"git_error_msg/NotARepository",
	"git_error_msg/NotFound",
	"git_error_msg/OutputHeldOpen",
	"git_error_msg/OutputLimit",
	"git_error_msg/OutsideBoundary",
	"git_error_msg/PatchLimit",
	"git_error_msg/QueueFull",
	"git_error_msg/QueueTimeout",
	"git_error_msg/Shallow",
	"git_error_msg/Timeout",
	"git_error_msg/ToplevelAbove",
	"git_error_msg/VerifyLimit",
	"git_error_msg/WorktreeBusy",
];

/// Mapped samples whose whole text is data the UI echoes (the sample is `D`,
/// scrubbed to nothing). Every other mapped sample must render wording; the
/// list is asserted exactly.
const ECHO_ONLY: &[&str] = &[
	"PreviewErr::from(io)/Other",
	"PreviewErr::git/Host",
	"PreviewErr::git/HostRefused#13",
	"PreviewErr::git/HostRefused#14",
	"PreviewErr::git/Io",
	"PreviewErr::git/OutputLimit",
	"PreviewErr::git/PatchLimit",
	"describe_msg/Connect",
	"describe_msg/Io(Other)",
	"describe_msg/PatchTooLarge",
	"describe_msg/Refused#13",
	"describe_msg/Refused#14",
	"describe_msg/Start(None)",
	"describe_msg_at/Connect",
	"describe_msg_at/Io(Other)",
	"describe_msg_at/PatchTooLarge",
	"describe_msg_at/Refused#13",
	"describe_msg_at/Refused#14",
	"describe_msg_at/Start(None)",
	"git_error_msg/Host",
	"git_error_msg/HostRefused#13",
	"git_error_msg/HostRefused#14",
	"git_error_msg/Io",
];

fn preview_text(e: &PreviewErr) -> String {
	match e {
		PreviewErr::TooLarge | PreviewErr::PatchTooLarge => String::new(),
		PreviewErr::Failed(m) => zh(m),
	}
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
		check(
			format!("PreviewErr::git/{label}"),
			&preview_text(&preview),
			&mut found,
		);
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
	check(
		"PreviewErr::from(io)/Other".into(),
		&preview_text(&previews[previews.len() - 2]),
		&mut found,
	);

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

	let leaks = found.leaks;
	let known: BTreeSet<String> =
		KNOWN_GAPS.iter().map(|s| s.to_string()).collect();
	// A mapped sample renders wording; only a sample that is data alone, or
	// no text by design, may scrub to nothing.
	let echo: BTreeSet<String> =
		ECHO_ONLY.iter().map(|s| s.to_string()).collect();
	let blank: Vec<_> = found.empty.difference(&echo).collect();
	let wordy: Vec<_> = echo.difference(&found.empty).collect();
	assert!(
		blank.is_empty() && wordy.is_empty(),
		"samples that render nothing in zh-TW (give them wording): {blank:#?}\n\
		 echo-only samples that now render wording (delete from ECHO_ONLY): {wordy:#?}"
	);
	let new: Vec<_> = leaks.difference(&known).collect();
	let fixed: Vec<_> = known.difference(&leaks).collect();
	assert!(
		new.is_empty() && fixed.is_empty(),
		"English leaks into zh-TW.\nnew leaks (fix them, or #225): {new:#?}\n\
		 fixed (delete from KNOWN_GAPS): {fixed:#?}"
	);
}

#[test]
fn the_leak_detector_sees_a_run_of_english_words() {
	assert!(english_run("遠端 the worker did not answer").is_some());
	assert!(english_run("無法讀取 snip-sync 的 worker 輸出").is_none());
	assert!(english_run(" ssh 無法登入").is_none());
	assert!(english_run("git  failed: ").is_some());
}
