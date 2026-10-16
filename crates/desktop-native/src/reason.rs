//! The one place a [`Reason`] becomes a message.
//!
//! Every failure the app shows reaches the screen through [`reason_msg`]:
//! the mappers (`describe_msg`, `git_error_msg`, `destination_msg` ...) only
//! pick the [`Reason`] of their error. A reason is worded by a dictionary
//! key, so a result that outlives a switch of language is worded again in the
//! new one. What the sentence cannot word (a path, git's stderr, the OS's
//! text) trails it as data through [`Msg::detail`]; no error's `Display` is a
//! fallback. All matches are exhaustive: a new variant of any enum below does
//! not compile until it has a wording.

use snip_core::gitsrc::StartReason;
use snip_core::reason::{
	ErrorCode, Failure, PayloadErrorKind, Reason, StaleChange,
};

use crate::i18n::Msg;

/// The command that completes a shallow clone. Shown as data, not wording.
const UNSHALLOW_COMMAND: &str = "git fetch --unshallow";

/// The dictionary key of a worker refusal code. Total: a new [`ErrorCode`]
/// must be given a wording here.
pub fn refusal_key(code: ErrorCode) -> &'static str {
	match code {
		ErrorCode::Forbidden => "remote_refused_forbidden",
		ErrorCode::NotFound => "remote_not_found",
		ErrorCode::Unsupported => "remote_refused_unsupported",
		ErrorCode::BadRequest => "remote_refused_bad_request",
		ErrorCode::TooLarge => "remote_refused_too_large",
		ErrorCode::Io => "remote_refused_io",
		ErrorCode::VersionMismatch => "remote_refused_version",
		ErrorCode::NotARepository => "remote_refused_not_repo",
		ErrorCode::InvalidRevision => "remote_refused_bad_revision",
		ErrorCode::Timeout => "remote_refused_timeout",
		ErrorCode::Busy => "remote_refused_busy",
		ErrorCode::Cancelled => "remote_refused_cancelled",
		ErrorCode::OutsideShare => "remote_refused_outside_share",
		ErrorCode::Stale => "remote_refused_stale",
		ErrorCode::Collision => "remote_refused_collision",
	}
}

/// The dictionary key of a known start failure's wording.
pub fn start_reason_key(reason: StartReason) -> &'static str {
	match reason {
		StartReason::NoWorker => "remote_start_no_worker",
		StartReason::KeyLogin => "remote_start_key_login",
		StartReason::NoAnswer => "remote_start_no_answer",
		StartReason::Closed => "remote_start_closed",
	}
}

/// A known start failure as a message. The transport's stderr trails it
/// only where it is the useful part (ssh's own refusal): a missing or old
/// snip would drag in clap's usage text or ssh banners.
pub fn start_msg(reason: StartReason, detail: &str) -> Msg {
	let headline = Msg::new(start_reason_key(reason), []);
	match reason {
		StartReason::KeyLogin => Msg::detail(headline, detail),
		StartReason::NoWorker | StartReason::NoAnswer | StartReason::Closed => {
			headline
		}
	}
}

/// `rel` as the datum of a change, for the wordings that name one file.
fn changed(change: &StaleChange) -> &str {
	match change {
		StaleChange::Created { rel }
		| StaleChange::Deleted { rel }
		| StaleChange::Modified { rel } => rel,
		StaleChange::Repository | StaleChange::Preview => "",
	}
}

/// A destination that changed after its preview.
fn destination_changed(change: &StaleChange) -> Msg {
	match change {
		StaleChange::Created { rel } => {
			Msg::new("stale_created", [rel.clone()])
		}
		StaleChange::Deleted { rel } => {
			Msg::new("stale_deleted", [rel.clone()])
		}
		StaleChange::Modified { rel } => {
			Msg::new("stale_modified", [rel.clone()])
		}
		StaleChange::Repository => Msg::new("err_dest_repo_changed", []),
		StaleChange::Preview => Msg::new("err_preview_expired", []),
	}
}

/// A failure's wording, headline and trailing datum.
fn headed(key: &'static str, datum: &str) -> Msg {
	Msg::detail(Msg::new(key, []), datum)
}

/// A payload fault in words: the kind picks the sentence, and the position
/// (where `serde_json` knew it) is filled into it. The library's own English
/// message is never data here.
fn invalid_payload(
	kind: PayloadErrorKind,
	line: Option<u64>,
	column: Option<u64>,
) -> Msg {
	match (kind, line, column) {
		(PayloadErrorKind::Syntax, Some(l), Some(c)) => Msg::new(
			"err_invalid_payload_syntax_at",
			[l.to_string(), c.to_string()],
		),
		(PayloadErrorKind::Data, Some(l), Some(c)) => Msg::new(
			"err_invalid_payload_data_at",
			[l.to_string(), c.to_string()],
		),
		(PayloadErrorKind::Eof, Some(l), Some(c)) => Msg::new(
			"err_invalid_payload_eof_at",
			[l.to_string(), c.to_string()],
		),
		(PayloadErrorKind::Syntax, ..) => {
			Msg::new("err_invalid_payload_syntax", [])
		}
		(PayloadErrorKind::Data, ..) => {
			Msg::new("err_invalid_payload_data", [])
		}
		(PayloadErrorKind::Eof, ..) => Msg::new("err_invalid_payload_eof", []),
		(PayloadErrorKind::Io, ..) => Msg::new("err_invalid_payload_io", []),
		(PayloadErrorKind::PreviewRow, ..) => {
			Msg::new("err_invalid_payload_preview_row", [])
		}
	}
}

// Every String field a Reason carries, and what the screen does with it. A
// field is either (i) DATA the user should see verbatim, trailing a worded
// headline (a path, a name, a sha, a rev, git's stderr, the OS's or the
// transport's own text, which develop showed too), or (ii) a library or
// product English diagnostic, which must be typed data and worded here.
// Classified (i): Start.detail, Disconnected.detail (transport text),
// WorkerTooOld.worker, GitFailed.stderr, NotARepository.path,
// InsideParentRepo.path, InvalidRevision.rev, Shallow.sha, Io.detail,
// NotFound/PermissionDenied/UnsafePath/NotRegularFile/OutsideRoot/Collision/
// StagedAndWorking/Unverifiable.path, AmbiguousRoots.basename/roots,
// PayloadTooLarge.path, Discontinuous.base/tip/at/first_parent,
// OutsideScope.path/scope, SourceChanged/DestinationChanged rel,
// SubmoduleUnreadable.detail, and Internal.detail (a command label or record
// name for a bug report under the localized "internal error" headline, or
// git's own output; never a sentence of ours).
// Classified (ii), now typed: InvalidPayload (`serde_json`'s message became
// kind + line + column). The zh leak test pins this list
// (`string_fields_are_classified`): a new String field fails it until it
// is classified here.

/// The message for a reason. Exhaustive: no `_` arm.
pub fn reason_msg(r: &Reason) -> Msg {
	match r {
		Reason::Refused { code } => Msg::new(refusal_key(*code), []),
		Reason::Start {
			start: Some(start),
			detail,
		} => start_msg(*start, detail),
		Reason::Start {
			start: None,
			detail,
		} => headed("err_start_failed", detail),
		Reason::Disconnected { detail } => headed("err_disconnected", detail),
		Reason::Protocol => Msg::new("err_protocol", []),
		Reason::WorkerTooOld { worker, have, need } => Msg::new(
			"err_worker_outdated",
			[worker.clone(), have.to_string(), need.to_string()],
		),
		Reason::TimedOut => Msg::new("err_timed_out", []),
		Reason::Cancelled => Msg::new("err_cancelled", []),
		Reason::GitMissing => Msg::new("err_git_missing", []),
		Reason::GitFailed { stderr } => headed("err_git_failed", stderr),
		Reason::GitBusy => Msg::new("err_git_busy", []),
		Reason::RepoBusy => Msg::new("paste_repo_busy", []),
		Reason::WorkerNeedsRestart => Msg::new("err_worker_restart", []),
		Reason::TooLarge => Msg::new("err_too_large", []),
		Reason::PatchTooLarge => Msg::new("err_patch_too_large", []),
		Reason::VerifyLimit => Msg::new("err_verify_limit", []),
		Reason::NotARepository { path } => headed("err_not_repo", path),
		Reason::InsideParentRepo { path } => {
			headed("err_inside_parent_repo", path)
		}
		Reason::InvalidRevision { rev } => headed("err_invalid_revision", rev),
		Reason::Shallow { sha } => Msg::detail(
			Msg::new("err_shallow", [UNSHALLOW_COMMAND.to_string()]),
			sha,
		),
		Reason::OutsideShare => Msg::new("remote_refused_outside_share", []),
		Reason::Internal { detail } => headed("err_internal", detail),
		Reason::Io { detail } => headed("err_io", detail),
		Reason::NotFound { path } => headed("err_not_found", path),
		Reason::PermissionDenied { path } => {
			headed("err_permission_denied", path)
		}
		Reason::EmptySelection => Msg::new("err_empty_selection", []),
		Reason::UnsafePath { path } => headed("err_unsafe_path", path),
		Reason::NotRegularFile { path } => headed("err_not_regular", path),
		Reason::OutsideRoot { path } => headed("err_outside_root", path),
		Reason::Collision { path } => headed("err_collision", path),
		Reason::StagedAndWorking { path } => {
			headed("err_staged_and_working", path)
		}
		Reason::AmbiguousRoots { basename, roots } => Msg::detail(
			Msg::new("err_ambiguous_roots", []),
			if roots.is_empty() {
				basename.clone()
			} else {
				roots.join(", ")
			},
		),
		Reason::PayloadTooLarge {
			limit,
			actual,
			path,
		} => Msg::detail(
			Msg::new(
				"err_payload_too_large",
				[limit.to_string(), actual.to_string()],
			),
			path.as_deref().unwrap_or(""),
		),
		Reason::SourceChanged { change } => {
			headed("err_source_changed", changed(change))
		}
		Reason::DestinationChanged { change } => destination_changed(change),
		Reason::CrossRepoCommits => Msg::new("err_cross_repo_commits", []),
		Reason::Discontinuous {
			base,
			tip,
			at,
			first_parent: _,
		} => Msg::detail(
			Msg::new("err_discontinuous", [base.clone(), tip.clone()]),
			at,
		),
		Reason::NotEnoughCommits {
			requested,
			available,
		} => Msg::new(
			"err_not_enough_commits",
			[requested.to_string(), available.to_string()],
		),
		Reason::NotCommitPayload => Msg::new("err_not_commit_payload", []),
		Reason::InvalidPayload { kind, line, column } => {
			invalid_payload(*kind, *line, *column)
		}
		Reason::OutsideScope { path, scope } => {
			Msg::detail(Msg::new("err_outside_scope", [path.clone()]), scope)
		}
		Reason::Unverifiable { path } => headed("err_unverifiable", path),
		Reason::SubmoduleNotCheckedOut => {
			Msg::new("err_submodule_not_checked_out", [])
		}
		Reason::SubmoduleUnreadable { detail } => {
			headed("err_submodule_unreadable", detail)
		}
		Reason::ScanErrorsOmitted { count } => {
			Msg::new("err_scan_errors_omitted", [count.to_string()])
		}
		Reason::DepthLimitedOmitted { count } => {
			Msg::new("err_depth_limited_omitted", [count.to_string()])
		}
	}
}

/// A scan row's wording. The row's English `message` is for the CLI and the
/// log, never for the screen.
pub fn failure_msg(f: &Failure) -> Msg {
	reason_msg(&f.reason)
}
