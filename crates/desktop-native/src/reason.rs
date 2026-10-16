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

use snip_core::clip::ClipFault;
use snip_core::gitsrc::{Condition, StartReason};
use snip_core::graph::{
	ContinuityFault, GraphError, InputFault, MAX_REF_NAME_LEN,
};
use snip_core::reason::{
	ErrorCode, Failure, PayloadErrorKind, Reason, StaleChange,
};

use snip_remote::RemoteError;

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
		StartReason::NoCommand => "remote_start_no_command",
		StartReason::CannotRun => "remote_start_cannot_run",
	}
}

/// The dictionary key of a user-caused condition's wording.
pub fn condition_key(condition: Condition) -> &'static str {
	match condition {
		Condition::GitmodulesNotFile => "cond_gitmodules_not_file",
		Condition::PathNotInSource => "cond_path_not_in_source",
		Condition::PathNotUtf8 => "cond_path_not_utf8",
		Condition::CommitPathNotUtf8 => "cond_commit_path_not_utf8",
		Condition::GitPathOutput => "cond_git_path_output",
	}
}

/// A known start failure as a message. The transport's stderr trails it
/// only where it is the useful part (ssh's own refusal): a missing or old
/// snip would drag in clap's usage text or ssh banners.
pub fn start_msg(reason: StartReason, detail: &str) -> Msg {
	let headline = Msg::new(start_reason_key(reason), []);
	match reason {
		// The OS's own words (the program and why it failed) trail it.
		StartReason::KeyLogin | StartReason::CannotRun => {
			Msg::detail(headline, detail)
		}
		StartReason::NoWorker
		| StartReason::NoAnswer
		| StartReason::Closed
		| StartReason::NoCommand => headline,
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
// kind + line + column); the product sentences of Internal ("`.gitmodules`
// is not a file", "path is not in this Git source") became `Condition`, and
// those of Start ("no command to start the worker", "cannot run ...")
// became `StartReason::NoCommand` / `CannotRun` (the OS's words stay data).
// A producer that can say a sentence of ours to a user types it; only a
// bug-report label may stay in `Internal.detail` (`GitError::Diagnostic`).
// The zh leak test pins this list (`string_fields_are_classified`) and
// feeds each producer of product text (`product_text_never_reaches_a_data_field`):
// a new String field fails it until it is classified here.

/// The message for a reason. Exhaustive: no `_` arm.
pub fn reason_msg(r: &Reason) -> Msg {
	match r {
		Reason::Refused { code } => Msg::new(refusal_key(*code), []),
		Reason::Start {
			start: Some(start),
			detail,
		} => start_msg(*start, detail.as_str()),
		Reason::Start {
			start: None,
			detail,
		} => headed("err_start_failed", detail.as_str()),
		Reason::Disconnected { detail } => {
			headed("err_disconnected", detail.as_str())
		}
		Reason::WorkerClosed => Msg::new("err_worker_closed", []),
		Reason::Protocol => Msg::new("err_protocol", []),
		Reason::WorkerTooOld { worker, have, need } => Msg::new(
			"err_worker_outdated",
			[worker.clone(), have.to_string(), need.to_string()],
		),
		Reason::TimedOut => Msg::new("err_timed_out", []),
		Reason::Cancelled => Msg::new("err_cancelled", []),
		Reason::GitMissing => Msg::new("err_git_missing", []),
		Reason::GitFailed { stderr } => {
			headed("err_git_failed", stderr.as_str())
		}
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
		Reason::Internal {
			condition: Some(condition),
			..
		} => Msg::new(condition_key(*condition), []),
		Reason::Internal {
			detail,
			condition: None,
		} => headed("err_internal", detail.as_str()),
		Reason::Io { detail } => headed("err_io", detail.as_str()),
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
			headed("err_submodule_unreadable", detail.as_str())
		}
		Reason::ScanErrorsOmitted { count } => {
			Msg::new("err_scan_errors_omitted", [count.to_string()])
		}
		Reason::DepthLimitedOmitted { count } => {
			Msg::new("err_depth_limited_omitted", [count.to_string()])
		}
	}
}

/// A remote failure as a message for a failure of the user's `path`: a worded
/// refusal keeps the path as the detail, so the status line says which path
/// failed. Any other failure is worded by its reason alone.
pub fn describe_msg_at(err: RemoteError, path: &str) -> Msg {
	let msg = reason_msg(&err.reason());
	match err {
		RemoteError::Refused { .. } => Msg::detail(msg, path),
		_ => msg,
	}
}

/// A scan row's wording. The row's English `message` is for the CLI and the
/// log, never for the screen.
pub fn failure_msg(f: &Failure) -> Msg {
	reason_msg(&f.reason)
}

/// A commit graph the layout refused, in words. The layout's own `Display`
/// is an English diagnostic for the CLI and the log: it is never data here.
/// A fault only a bug can cause (nothing a repository can make git hand us)
/// trails the headline as a fixed label for a bug report. Exhaustive: a new
/// error or fault does not compile until it has a wording.
pub fn graph_error_msg(e: &GraphError) -> Msg {
	let bug = |label: &'static str| {
		Msg::detail(Msg::new("err_graph_layout", []), label)
	};
	match e {
		GraphError::RowLimitExceeded { requested, limit } => Msg::new(
			"graph_err_rows",
			[requested.to_string(), limit.to_string()],
		),
		GraphError::RefLimitExceeded { requested, limit } => Msg::new(
			"graph_err_refs",
			[requested.to_string(), limit.to_string()],
		),
		GraphError::ByteLimitExceeded { total, limit } => {
			Msg::new("graph_err_bytes", [total.to_string(), limit.to_string()])
		}
		// The history view steps down to a plain list before it gets here,
		// but a message is owed all the same.
		GraphError::FrontierLimitExceeded { current, limit } => Msg::new(
			"graph_err_frontier",
			[current.to_string(), limit.to_string()],
		),
		GraphError::InvalidInput { fault } => match fault {
			InputFault::CommitSha { .. }
			| InputFault::ParentSha { .. }
			| InputFault::RefSha { .. } => Msg::new("graph_commit_id_too_long", []),
			InputFault::RefNameTooLong { .. } => Msg::new(
				"graph_err_ref_name_too_long",
				[MAX_REF_NAME_LEN.to_string()],
			),
			InputFault::RefNameEmpty { .. } => bug("empty_ref_name"),
			InputFault::FrontierSha => bug("frontier_sha"),
			InputFault::CheckpointSha => bug("checkpoint_sha"),
		},
		GraphError::InvalidConfig { .. } => bug("invalid_config"),
		GraphError::ContinuityError { fault } => match fault {
			ContinuityFault::RepeatsLastCommit { .. } => bug("page_continuity"),
		},
		GraphError::DuplicateCommit { .. } => bug("duplicate_commit"),
	}
}

/// What a clipboard library error means, in words.
pub fn clip_fault_msg(f: ClipFault) -> Msg {
	Msg::new(
		match f {
			ClipFault::Empty => "clip_err_empty",
			ClipFault::Unsupported => "clip_err_unsupported",
			ClipFault::Occupied => "clip_err_occupied",
			ClipFault::Conversion => "clip_err_conversion",
			ClipFault::Other => "clip_err_other",
		},
		[],
	)
}

/// A failed clipboard write, with the cause in words.
pub fn clipboard_failed(e: &arboard::Error) -> Msg {
	Msg::with_msg_arg(
		"status_clipboard_failed",
		[String::new()],
		0,
		clip_fault_msg(snip_core::clip::fault(e)),
	)
}

/// A failed clipboard read, with the cause in words.
pub fn clipboard_read_failed(e: &arboard::Error) -> Msg {
	Msg::with_msg_arg(
		"status_clipboard_read_failed",
		[String::new()],
		0,
		clip_fault_msg(snip_core::clip::fault(e)),
	)
}
