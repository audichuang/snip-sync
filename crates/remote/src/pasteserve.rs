//! Paste on the worker: the master sends the payload and the user's
//! choices, and the worker plans and writes with the engine a local paste
//! runs ([`plan_import_parsed_with`], [`CommitReplayPreview`]) on its own disk.
//!
//! An Apply plans again and refuses as stale whatever changed
//! since the preview the master showed. Destinations stay inside the
//! workspace (the remote access scope); every other rule, the Git-directory
//! write guard included, is the core engine's and the same as locally.

use std::path::{Path, PathBuf};

use snip_core::commits;
use snip_core::gitrun::{CancelToken, GitPool, RunOptions};
use snip_core::gitsrc::Git;
use snip_core::gitview::ReadProfile;
use snip_core::reason::{Reason, StaleChange};
use snip_core::restore::{RestorePlan, RestoreSelection};
use snip_core::transfer::{
	plan_import_parsed_with, CanonicalRootId, CommitReplayPreview,
	DestinationError, DestinationFreshnessSnapshot, PlacedRoot, TransferError,
	TransferImportPlan,
};

use crate::codeless::Codeless;
use crate::prepared::{self, CacheError, CachedImport, PreparedCache};
use crate::proto::{
	valid_rel_path, ErrorCode, ImportExpect, ImportPlanned, PasteMapping,
	ReplayExpect, Response, CREATE_DIRS_VERSION,
};
use crate::worker::{io_error, SharedRoot};

type Refusal = Box<Response>;

fn refused(code: ErrorCode, message: impl Into<String>) -> Refusal {
	refused_with(code, message, None)
}

/// A refusal that also says why. `reason` must already have gone through
/// [`Reason::beside_code`]: it never carries a path or git's stderr.
fn refused_with(
	code: ErrorCode,
	message: impl Into<String>,
	reason: Option<Reason>,
) -> Refusal {
	Box::new(Response::Error {
		code,
		message: message.into(),
		patch: false,
		reason,
		suggestion: None,
	})
}

/// `refusal` with the restore-base suggestion a refused paste plan carries.
fn with_suggestion(
	mut refusal: Refusal,
	suggestion: Option<&snip_core::restore::RestoreBaseSuggestion>,
) -> Refusal {
	if let Response::Error {
		suggestion: slot, ..
	} = &mut *refusal
	{
		*slot = suggestion.cloned().map(Box::new);
	}
	refusal
}

/// Reads on the worker's own pool, cancelled with the job.
fn options(cancel: &CancelToken) -> RunOptions {
	let mut opts = ReadProfile::Interactive.options(Some(cancel.clone()));
	opts.pool = GitPool::Served;
	opts
}

/// A refused paste in the local paste's words. Stale and collision keep
/// their own codes so a master words them as `snip paste` and the app do.
fn transfer_refusal(err: TransferError, cancel: &CancelToken) -> Refusal {
	use snip_core::gitsrc::GitError;
	let code = match &err {
		TransferError::StaleDestination { .. } => ErrorCode::Stale,
		TransferError::TargetCollision { .. } => ErrorCode::Collision,
		// A master that sent no agreement for a folder it asked for.
		TransferError::NewDirsNotConfirmed { .. } => ErrorCode::BadRequest,
		TransferError::Commit(
			snip_core::commits::CommitError::OutsideScope { .. },
		) => ErrorCode::Forbidden,
		TransferError::Git(_) if cancel.is_cancelled() => ErrorCode::Cancelled,
		TransferError::Git(GitError::Cancelled { .. }) => ErrorCode::Cancelled,
		TransferError::Git(GitError::Timeout { .. }) => ErrorCode::Timeout,
		TransferError::Git(
			GitError::QueueFull { .. }
			| GitError::QueueTimeout { .. }
			| GitError::WorktreeBusy { .. },
		) => ErrorCode::Busy,
		TransferError::Git(GitError::NotARepository(_)) => {
			ErrorCode::NotARepository
		}
		_ => ErrorCode::Io,
	};
	// A cancelled job says so by its code; every other refusal sends what
	// the engine knows, scrubbed, so the master need not guess from text.
	let reason = if code == ErrorCode::Cancelled {
		None
	} else {
		err.reason().beside_code()
	};
	refused_with(code, err.paste_message(), reason)
}

fn stale(root: &Path, reason: &str) -> Refusal {
	transfer_refusal(
		TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: reason.into(),
			change: StaleChange::Preview,
		},
		&CancelToken::new(),
	)
}

/// The folder `rel` of the workspace as a paste destination: real, a
/// folder, inside the workspace. A Git directory is not refused here: the
/// engine lists every entry that would land in it as a skipped row, as a
/// local paste does.
fn destination(root: &SharedRoot, rel: &str) -> Result<PathBuf, Refusal> {
	if !valid_rel_path(rel, true) {
		return Err(refused(ErrorCode::BadRequest, "invalid destination"));
	}
	let path = if rel.is_empty() {
		root.path.clone()
	} else {
		snip_core::browser::inside(&root.path, rel)
			.map_err(|e| Box::new(io_error(e)))?
	};
	if !path.is_dir() {
		return Err(refused(
			ErrorCode::BadRequest,
			format!("'{}' is not a folder", path.display()),
		));
	}
	Ok(path)
}

/// A file paste's folder `rel` as a [`PlacedRoot`]: as [`destination`] does,
/// unless the master asked for folders that may not exist yet on a protocol
/// that has them. Then the folder may be missing, as long as its nearest
/// existing ancestor, symlinks resolved, is inside the workspace, and the
/// names below it are plain ones; nothing is made here.
fn placed_destination(
	root: &SharedRoot,
	rel: &str,
	allow_new: bool,
) -> Result<PlacedRoot, Refusal> {
	if !valid_rel_path(rel, true) {
		return Err(refused(ErrorCode::BadRequest, "invalid destination"));
	}
	if !allow_new {
		let path = destination(root, rel)?;
		return CanonicalRootId::new(&path)
			.map(PlacedRoot::from)
			.map_err(|e| Box::new(io_error(e)));
	}
	let path = if rel.is_empty() {
		root.path.clone()
	} else {
		root.path.join(rel)
	};
	PlacedRoot::resolve(Some(&root.path), &path).map_err(|e| match e {
		DestinationError::OutsideBound { .. } => {
			refused(ErrorCode::Forbidden, "Path leaves the workspace")
		}
		DestinationError::NotADirectory { .. }
		| DestinationError::InvalidName { .. } => refused_with(
			ErrorCode::BadRequest,
			e.to_string(),
			e.reason().beside_code(),
		),
		DestinationError::Io(e) => Box::new(io_error(e)),
	})
}

/// The paths a master sent back with a snapshot must be ones this paste
/// could have recorded: under `base`. The snapshot's `repos` keys are not
/// checked here: a repository's top can lie above the workspace, and what
/// the paste may lock is not known until it is planned again. `revalidate`
/// compares them with the set it derives itself, before it runs git in any.
fn check_snapshot(
	base: &Path,
	freshness: &DestinationFreshnessSnapshot,
) -> Result<(), Refusal> {
	let inside = |p: &Path| p.is_absolute() && p.starts_with(base);
	let ok = freshness.roots.keys().all(|r| inside(r.path()))
		&& freshness
			.target_files
			.iter()
			.all(|(p, t)| inside(p) && inside(t.root.path()))
		&& freshness.missing_dirs.iter().flatten().all(|p| inside(p))
		&& freshness.requested_dirs.iter().flatten().all(|p| inside(p))
		&& freshness
			.symlinked_dirs
			.iter()
			.flat_map(|m| m.keys())
			.all(|p| inside(p));
	if ok {
		Ok(())
	} else {
		Err(refused(
			ErrorCode::BadRequest,
			"the preview names a path this paste cannot have recorded",
		))
	}
}

fn digest(plan: &RestorePlan) -> Result<String, Refusal> {
	crate::proto::restore_plan_digest(plan)
		.map_err(|message| refused(ErrorCode::BadRequest, message))
}

struct Planned {
	plan: TransferImportPlan,
	/// The destination folder, which may not exist yet.
	primary: PlacedRoot,
	suggestion: Option<snip_core::restore::RestoreBaseSuggestion>,
	create_entries: Vec<usize>,
}

/// Plans exactly as `snip paste` and the app do, with the destination and
/// mapping resolved inside the workspace.
fn plan(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	mapping: &PasteMapping,
	negotiated: u32,
	cancel: &CancelToken,
) -> Result<Planned, Refusal> {
	if cancel.is_cancelled() {
		return Err(refused(ErrorCode::Cancelled, "cancelled"));
	}
	let allow_new = negotiated >= CREATE_DIRS_VERSION && mapping.allow_new_dirs;
	let primary = placed_destination(root, dest, allow_new)?;
	let header = mapping.header_format.as_str();
	let entries = snip_core::format::parse_clipboard(text, header);
	if cancel.is_cancelled() {
		return Err(refused(ErrorCode::Cancelled, "cancelled"));
	}
	if entries.is_empty() {
		return Err(refused(
			ErrorCode::BadRequest,
			"No Snipcode file headers found in clipboard.",
		));
	}
	let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
	let suggestion = crate::pastetarget::suggest(&primary.path(), &paths, text);
	// From here a refusal keeps the suggestion, as a local paste's note
	// does: it names this payload against this destination, whatever the
	// engine says to the plan after it.
	let keep = |refusal: Refusal| with_suggestion(refusal, suggestion.as_ref());
	let (import, roots) = crate::pastetarget::import_mapping(
		primary.clone(),
		None,
		suggestion.as_ref().filter(|_| mapping.adjust_paths),
		mapping.prefixes.iter().map(|(prefix, rel)| {
			if prefix.is_empty() || prefix.contains(['/', '\\']) {
				return Err(refused(ErrorCode::BadRequest, "invalid prefix"));
			}
			let placed = placed_destination(root, rel, allow_new)?;
			let spelled = placed.path();
			Ok::<_, Refusal>((prefix.clone(), placed, spelled))
		}),
	)
	.map_err(keep)?;
	let (plan, create_entries) =
		plan_import_parsed_with(entries, &roots, &import, &options(cancel))
			.map_err(|e| keep(transfer_refusal(e, cancel)))?;
	Ok(Planned {
		plan,
		primary,
		suggestion,
		create_entries,
	})
}

/// Answers [`crate::proto::Request::ImportPlan`].
pub(crate) fn import_plan(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	mapping: &PasteMapping,
	negotiated: u32,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let Planned {
			plan, suggestion, ..
		} = plan(root, dest, text, mapping, negotiated, cancel)?;
		Ok(Response::ImportPlanned(ImportPlanned {
			digest: digest(plan.restore_plan())?,
			plan,
			suggestion,
		}))
	};
	run().unwrap_or_else(|reply| *reply)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_import(
	root: &SharedRoot,
	dest: &str,
	text: String,
	mapping: PasteMapping,
	retain: bool,
	negotiated: u32,
	cache: &PreparedCache,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		if text.len() > snip_core::transfer::CLIPBOARD_PAYLOAD_MAX {
			return Err(refused(
				ErrorCode::TooLarge,
				"the pasted text is over the clipboard limit",
			));
		}
		let Planned {
			plan,
			primary,
			suggestion,
			create_entries,
		} = plan(root, dest, &text, &mapping, negotiated, cancel)?;
		let metadata = crate::proto::ImportMetadata::of_entry_indices(
			&plan,
			&create_entries,
		)
		.map_err(|message| refused(ErrorCode::BadRequest, message))?;
		let digest = digest(plan.restore_plan())?;
		// The folder pasted into, which may not exist yet: the roots'
		// first is only its anchor.
		let destination = primary.path();
		drop(plan);
		if cancel.is_cancelled() {
			return Err(refused(ErrorCode::Cancelled, "cancelled"));
		}
		let token = retain
			.then(|| {
				cache.insert(CachedImport {
					text,
					mapping,
					workspace: root.path.clone(),
					destination,
					digest: digest.clone(),
					replay: None,
				})
			})
			.flatten();
		Ok(Response::ImportPrepared(crate::proto::ImportPrepared {
			metadata,
			digest,
			suggestion,
			token,
		}))
	};
	run().unwrap_or_else(|reply| *reply)
}

/// The refusal for a token the cache would not hand out.
fn cache_refusal(why: CacheError) -> Response {
	let stale = Some(Reason::DestinationChanged {
		change: StaleChange::Preview,
	});
	let (code, message, reason) = match why {
		CacheError::Missing => (ErrorCode::Stale, prepared::TOKEN_GONE, stale),
		// Forbidden words it.
		CacheError::Scope => (
			ErrorCode::Forbidden,
			"prepared paste belongs to another workspace or destination; preview again",
			None,
		),
		CacheError::Digest => (
			ErrorCode::Stale,
			"prepared paste does not match the preview; preview again",
			stale,
		),
	};
	*refused_with(code, message, reason)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_prepared(
	root: &SharedRoot,
	dest: &str,
	token: &str,
	selection: &RestoreSelection,
	expect: &ImportExpect,
	negotiated: u32,
	cache: &PreparedCache,
	cancel: &CancelToken,
) -> Response {
	// Only the key of the cached preview: `allow_new` is as loose as the
	// protocol allows. What the Apply may make is decided by the cached
	// mapping, in `import_apply`.
	let at =
		match placed_destination(root, dest, negotiated >= CREATE_DIRS_VERSION)
		{
			Ok(at) => at.path(),
			Err(reply) => return *reply,
		};
	let lease = match cache.take(token, &root.path, &at, &expect.digest) {
		Ok(lease) => lease,
		Err(why) => return cache_refusal(why),
	};
	if lease.payload().replay.is_some() {
		return *refused(
			ErrorCode::Forbidden,
			"prepared paste is not a file paste; preview again",
		);
	}
	let payload = lease.payload();
	// The raw input is retained, never an executable plan. Keep exactly the
	// legacy re-plan/digest/freshness/selection and locked-write checks.
	let response = import_apply(
		root,
		dest,
		&payload.text,
		&payload.mapping,
		selection,
		expect,
		negotiated,
		cancel,
	);
	// A refusal from before the first write keeps the payload for a retry.
	if matches!(
		&response,
		Response::Error { code, .. } if prepared::refused_before_write(*code)
	) {
		lease.restore();
	}
	response
}

/// `SNIP_E2E_PASTE_HOLD` names an absolute path: when set and the
/// file exists, an Apply that just committed its first write waits until
/// the file disappears (at most 60 s, polled every 50 ms), so a protocol
/// run can cut the connection mid-write deterministically. Unset, or the
/// path relative or file absent: no pause.
fn hold_after_first_write() {
	let Some(path) = std::env::var_os("SNIP_E2E_PASTE_HOLD") else {
		return;
	};
	let path = PathBuf::from(path);
	if !path.is_absolute() || !path.is_file() {
		return;
	}
	let deadline =
		std::time::Instant::now() + std::time::Duration::from_secs(60);
	while path.is_file() && std::time::Instant::now() < deadline {
		std::thread::sleep(std::time::Duration::from_millis(50));
	}
}

/// Answers [`crate::proto::Request::ImportApply`]: the preview's snapshot
/// is checked first, as a local Apply does, then the plan made again must
/// be the one previewed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn import_apply(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	mapping: &PasteMapping,
	selection: &RestoreSelection,
	expect: &ImportExpect,
	negotiated: u32,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		check_snapshot(&root.path, &expect.freshness)?;
		expect
			.freshness
			.revalidate()
			.map_err(|e| transfer_refusal(e, cancel))?;
		let Planned { plan, .. } =
			plan(root, dest, text, mapping, negotiated, cancel)?;
		if digest(plan.restore_plan())? != expect.digest
			|| !plan
				.destination_freshness()
				.matches_preview(&expect.freshness)
		{
			let at = plan.roots().first().cloned().unwrap_or_default();
			return Err(stale(&at, "destination changed after preview"));
		}
		let creates = plan.create_operations().len();
		let deletes = plan.delete_operations().len();
		if selection.unchecked_creates.iter().any(|&i| i >= creates)
			|| selection.unchecked_deletes.iter().any(|&i| i >= deletes)
		{
			return Err(refused(ErrorCode::BadRequest, "invalid selection"));
		}
		if cancel.is_cancelled() {
			return Err(refused(ErrorCode::Cancelled, "cancelled"));
		}
		let mut held_once = false;
		let result = plan
			.apply_observed(selection, &mut |_op, _outcome| {
				if !held_once {
					held_once = true;
					hold_after_first_write();
				}
			})
			.map_err(|e| transfer_refusal(e, cancel))?;
		Ok(Response::Imported(Codeless::new(result)))
	};
	run().unwrap_or_else(|resp| *resp)
}

/// The repository a commit payload replays onto: the top folder of the
/// repository at `dest`, as a local replay finds it (it may lie above the
/// workspace, as local Git views follow it), plus the write scope: the
/// opened folder the master named. Replay targets outside the scope are
/// refused by the shared engine, so replaying the whole repository means
/// opening the repository root.
fn replay_root(
	root: &SharedRoot,
	dest: &str,
	cancel: &CancelToken,
) -> Result<(PathBuf, PathBuf), Refusal> {
	let scope = destination(root, dest)?;
	let git = Git::open_with(&scope, &options(cancel))
		.map_err(|e| transfer_refusal(TransferError::Git(e), cancel))?;
	let top =
		dunce::canonicalize(git.root()).map_err(|e| Box::new(io_error(e)))?;
	Ok((top, scope))
}

fn payload(text: &str) -> Result<commits::CommitsPayload, Refusal> {
	commits::parse_commit_payload(text).map_err(|e| {
		refused_with(
			ErrorCode::BadRequest,
			e.to_string(),
			e.reason().beside_code(),
		)
	})
}

/// A selected preview's original destination. The shared reader preserves
/// file kind (especially symlinks) instead of presenting their targets as
/// the regular file that replay would replace.
pub(crate) fn replay_read(
	root: &SharedRoot,
	path: &Path,
	limit: usize,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let rel = path
			.strip_prefix(&root.path)
			.ok()
			.and_then(|p| p.to_str())
			.map(|p| {
				if cfg!(windows) {
					p.replace('\\', "/")
				} else {
					p.to_string()
				}
			});
		let Some(rel) = rel.filter(|p| valid_rel_path(p, false)) else {
			return Err(refused(ErrorCode::BadRequest, "invalid preview path"));
		};
		// Validate the resolved boundary, but retain the lexical path for
		// the reader's symlink_metadata check.
		snip_core::browser::inside(&root.path, &rel)
			.map_err(|e| Box::new(io_error(e)))?;
		let read = snip_core::transfer::read_replay_preview_with(
			path,
			limit.min(snip_core::browser::PREVIEW_LIMIT),
			&options(cancel),
		)
		.map_err(|e| transfer_refusal(e, cancel))?;
		snip_core::browser::inside(&root.path, &rel)
			.map_err(|e| Box::new(io_error(e)))?;
		Ok(Response::ReplayRead(read))
	};
	run().unwrap_or_else(|resp| *resp)
}

/// Answers [`crate::proto::Request::ReplayPlan`].
pub(crate) fn replay_plan(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let (top, scope) = replay_root(root, dest, cancel)?;
		let payload = payload(text)?;
		let preview = CommitReplayPreview::capture_in(
			&top,
			&scope,
			&payload,
			&options(cancel),
		)
		.map_err(|e| transfer_refusal(e, cancel))?;
		Ok(Response::ReplayPlanned(ReplayExpect::of(&preview)))
	};
	run().unwrap_or_else(|resp| *resp)
}

/// Answers [`crate::proto::Request::PrepareReplay`]: the preview is the
/// worker's own, and with `retain` it is kept under a token for one later
/// [`apply_prepared_replay`].
pub(crate) fn prepare_replay(
	root: &SharedRoot,
	dest: &str,
	text: String,
	retain: bool,
	cache: &PreparedCache,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		if text.len() > snip_core::transfer::CLIPBOARD_PAYLOAD_MAX {
			return Err(refused(
				ErrorCode::TooLarge,
				"the pasted text is over the clipboard limit",
			));
		}
		let (top, scope) = replay_root(root, dest, cancel)?;
		let payload = payload(&text)?;
		let preview = CommitReplayPreview::capture_in(
			&top,
			&scope,
			&payload,
			&options(cancel),
		)
		.map_err(|e| transfer_refusal(e, cancel))?;
		drop(payload);
		let expect = ReplayExpect::of(&preview);
		let digest = crate::proto::json_digest(&expect)
			.map_err(|m| refused(ErrorCode::BadRequest, m))?;
		if cancel.is_cancelled() {
			return Err(refused(ErrorCode::Cancelled, "cancelled"));
		}
		let token = retain
			.then(|| {
				cache.insert(CachedImport {
					text: String::new(),
					mapping: PasteMapping::default(),
					workspace: root.path.clone(),
					destination: scope.clone(),
					digest: digest.clone(),
					replay: Some(Box::new(preview)),
				})
			})
			.flatten();
		Ok(Response::ReplayPrepared(crate::proto::ReplayPrepared {
			expect,
			digest,
			token,
		}))
	};
	run().unwrap_or_else(|r| *r)
}

/// Answers [`crate::proto::Request::ApplyPreparedReplay`] from the preview
/// the token holds. A check, and a refusal from before the first write,
/// keep the token.
pub(crate) fn apply_prepared_replay(
	root: &SharedRoot,
	dest: &str,
	token: &str,
	digest: &str,
	check_only: bool,
	cache: &PreparedCache,
	cancel: &CancelToken,
) -> Response {
	let scope = match destination(root, dest) {
		Ok(s) => s,
		Err(r) => return *r,
	};
	let lease = match cache.take(token, &root.path, &scope, digest) {
		Ok(l) => l,
		Err(why) => return cache_refusal(why),
	};
	let Some(preview) = lease.payload().replay.as_deref() else {
		return *refused(
			ErrorCode::Forbidden,
			"prepared paste is not a commit replay; preview again",
		);
	};
	let response = run_prepared_replay(root, dest, preview, check_only, cancel);
	// A check never writes; a refusal from before the first write keeps the token too.
	if check_only
		|| matches!(&response, Response::Error { code, .. } if prepared::refused_before_write(*code))
	{
		lease.restore();
	}
	response
}

fn run_prepared_replay(
	root: &SharedRoot,
	dest: &str,
	preview: &CommitReplayPreview,
	check_only: bool,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let (top, _) = replay_root(root, dest, cancel)?;
		if preview.destination() != top {
			return Err(stale(
				&top,
				"replay eligibility changed after preview",
			));
		}
		if check_only {
			preview
				.revalidate_with(&options(cancel))
				.map_err(|e| transfer_refusal(e, cancel))?;
			return Ok(Response::Fresh);
		}
		let opts = RunOptions {
			cancel: Some(cancel.clone()),
			pool: GitPool::Served,
			..RunOptions::default()
		};
		let result = preview
			.apply_with(&opts)
			.map_err(|e| transfer_refusal(e, cancel))?;
		Ok(Response::Replayed(Codeless::new(result)))
	};
	run().unwrap_or_else(|resp| *resp)
}

/// Answers [`crate::proto::Request::ReplayApply`]: the preview put back
/// together re-plans the payload and re-reads every recorded path under the
/// replay lock, exactly as a local Apply. The scope is derived from `dest`
/// again, so a master cannot widen it by sending a preview of its own.
pub(crate) fn replay_apply(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	expect: ReplayExpect,
	check_only: bool,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let (top, scope) = replay_root(root, dest, cancel)?;
		if expect.destination != top || expect.plan.root != top {
			return Err(stale(
				&top,
				"replay eligibility changed after preview",
			));
		}
		check_snapshot(&top, &expect.freshness)?;
		let payload = payload(text)?;
		let preview = CommitReplayPreview::from_parts_scoped(
			expect.destination,
			scope,
			payload,
			expect.plan,
			expect.freshness,
		);
		if check_only {
			preview
				.revalidate_with(&options(cancel))
				.map_err(|e| transfer_refusal(e, cancel))?;
			return Ok(Response::Fresh);
		}
		// A local Apply's options: no read deadline once it is confirmed.
		let opts = RunOptions {
			cancel: Some(cancel.clone()),
			pool: GitPool::Served,
			..RunOptions::default()
		};
		let result = preview
			.apply_with(&opts)
			.map_err(|e| transfer_refusal(e, cancel))?;
		Ok(Response::Replayed(Codeless::new(result)))
	};
	run().unwrap_or_else(|resp| *resp)
}

#[cfg(test)]
mod reason_tests {
	use super::*;
	use crate::codeless::Codeless;
	use snip_core::commits::{CommitError, ReplayFailure, ReplayResult};
	use snip_core::reason::Datum;

	fn reason_of(refusal: Refusal) -> (ErrorCode, Option<Reason>, String) {
		let Response::Error {
			code,
			reason,
			message,
			..
		} = *refusal
		else {
			panic!("expected Error");
		};
		(code, reason, message)
	}

	/// A refusal names what the engine knows, scrubbed, so the master never
	/// reads English back; the whole reply names no path of this machine.
	#[test]
	fn a_refusal_sends_a_scrubbed_reason() {
		let secret = "/opt/secret/repo";
		let cancel = CancelToken::new();
		let stale = TransferError::StaleDestination {
			root: secret.into(),
			reason: "target file 'a' was created externally after preview"
				.into(),
			change: StaleChange::Created { rel: "a".into() },
		};
		let (code, reason, _) = reason_of(transfer_refusal(stale, &cancel));
		assert_eq!(code, ErrorCode::Stale);
		assert_eq!(
			reason,
			Some(Reason::DestinationChanged {
				change: StaleChange::Created { rel: "a".into() }
			})
		);
		let busy =
			TransferError::Git(snip_core::gitsrc::GitError::WorktreeBusy {
				args: "paste".into(),
			});
		let (code, reason, _) = reason_of(transfer_refusal(busy, &cancel));
		assert_eq!((code, reason), (ErrorCode::Busy, Some(Reason::RepoBusy)));
		let git = TransferError::Git(snip_core::gitsrc::GitError::Failed {
			args: secret.into(),
			stderr: format!("fatal: {secret}"),
			code: Some(1),
		});
		// The message is the local paste's words; the reason is the part
		// that must hold no path or stderr.
		let (_, reason, _) = reason_of(transfer_refusal(git, &cancel));
		let wire = serde_json::to_string(&reason).unwrap();
		assert!(!wire.contains("secret"), "{wire}");
		assert!(wire.contains("git_failed"), "{wire}");
		let scope = TransferError::Commit(CommitError::OutsideScope {
			path: "a/b".into(),
			scope: secret.into(),
		});
		let (_, reason, _) = reason_of(transfer_refusal(scope, &cancel));
		let wire = serde_json::to_string(&reason).unwrap();
		assert!(!wire.contains("secret"), "{wire}");
		assert!(wire.contains("\"a/b\""), "{wire}");
		// A cancelled job says so by its code alone.
		cancel.cancel();
		let git = TransferError::Git(snip_core::gitsrc::GitError::Failed {
			args: "x".into(),
			stderr: "killed".into(),
			code: None,
		});
		assert_eq!(
			reason_of(transfer_refusal(git, &cancel)).0,
			ErrorCode::Cancelled
		);
		assert_eq!(
			reason_of(transfer_refusal(
				TransferError::Git(snip_core::gitsrc::GitError::Cancelled {
					args: "x".into()
				}),
				&cancel
			))
			.1,
			None
		);
	}

	/// A token the cache would not hand out is a stale preview; the wrong
	/// workspace is a plain Forbidden.
	#[test]
	fn cache_refusals_say_the_preview_is_stale() {
		let reason = |why| {
			let Response::Error { code, reason, .. } = cache_refusal(why)
			else {
				panic!("expected Error");
			};
			(code, reason)
		};
		let preview = Some(Reason::DestinationChanged {
			change: StaleChange::Preview,
		});
		assert_eq!(
			reason(CacheError::Missing),
			(ErrorCode::Stale, preview.clone())
		);
		assert_eq!(reason(CacheError::Digest), (ErrorCode::Stale, preview));
		assert_eq!(reason(CacheError::Scope), (ErrorCode::Forbidden, None));
	}

	/// A replay result leaves the worker with its failure's reason scrubbed.
	#[test]
	fn a_replay_failure_reason_is_scrubbed_before_it_is_sent() {
		let failed = |reason| ReplayResult {
			created: vec![],
			failure: Some(ReplayFailure {
				index: 0,
				message: "m".into(),
				error: "e".into(),
				layout_conflict: None,
				conflict_path: None,
				reason: Some(reason),
			}),
		};
		let sent = |reason| {
			Codeless::new(failed(reason))
				.into_inner()
				.failure
				.unwrap()
				.reason
		};
		assert_eq!(
			sent(Reason::UnsafePath {
				path: "/etc/x".into()
			}),
			Some(Reason::UnsafePath {
				path: String::new()
			})
		);
		// No code travels with a replay failure, so the reasons a code
		// would say stay, scrubbed (the master cannot tell a refused
		// directory from a generic I/O error otherwise).
		assert_eq!(
			sent(Reason::Io {
				detail: Datum::for_test("/opt/secret")
			}),
			Some(Reason::Io {
				detail: Datum::default()
			})
		);
		for r in [
			Reason::PermissionDenied { path: "/p".into() },
			Reason::NotFound { path: "/p".into() },
			Reason::TimedOut,
			Reason::Cancelled,
			Reason::OutsideShare,
		] {
			let want = r.clone().scrubbed();
			assert_eq!(sent(r.clone()), Some(want), "{r:?}");
		}
		let wire = serde_json::to_string(&Response::Replayed(Codeless::new(
			failed(Reason::OutsideScope {
				path: "a".into(),
				scope: "/opt/secret".into(),
			}),
		)))
		.unwrap();
		assert!(!wire.contains("secret"), "{wire}");
	}
}

#[cfg(test)]
mod prepared_import_tests {
	use super::*;
	use std::fs;
	use std::time::Duration;

	fn preview(
		root: &SharedRoot,
		text: &str,
		mapping: &PasteMapping,
		cache: &PreparedCache,
	) -> crate::proto::ImportPrepared {
		match prepare_import(
			root,
			"",
			text.into(),
			mapping.clone(),
			true,
			crate::proto::PROTOCOL_MAX,
			cache,
			&CancelToken::new(),
		) {
			Response::ImportPrepared(reply) => reply,
			other => panic!("failed to prepare: {other:?}"),
		}
	}
	fn apply(
		root: &SharedRoot,
		dest: &str,
		text: &str,
		mapping: &PasteMapping,
		reply: &crate::proto::ImportPrepared,
		cache: &PreparedCache,
		selection: RestoreSelection,
	) -> Response {
		apply_prepared(
			root,
			dest,
			reply.token.as_ref().unwrap(),
			&selection,
			&reply
				.hydrate(text, &mapping.header_format)
				.unwrap()
				.expect(),
			crate::proto::PROTOCOL_MAX,
			cache,
			&CancelToken::new(),
		)
	}
	fn fixture() -> (tempfile::TempDir, SharedRoot, PreparedCache) {
		let dir = tempfile::tempdir().unwrap();
		let root = SharedRoot::new(dir.path()).unwrap();
		(dir, root, PreparedCache::new())
	}

	#[test]
	fn prepared_apply_replans_and_consumes_the_token_once() {
		let (_dir, root, cache) = fixture();
		let text = "// FILE: new.txt\ncreated\n";
		let mapping = PasteMapping::default();
		let reply = preview(&root, text, &mapping, &cache);
		assert!(
			matches!(apply(&root,"",text,&mapping,&reply,&cache,RestoreSelection::default()),Response::Imported(r) if r.created_count==1)
		);
		let bytes = fs::read(root.path.join("new.txt")).unwrap();
		assert!(matches!(
			apply(
				&root,
				"",
				text,
				&mapping,
				&reply,
				&cache,
				RestoreSelection::default()
			),
			Response::Error {
				code: ErrorCode::Stale,
				..
			}
		));
		assert_eq!(fs::read(root.path.join("new.txt")).unwrap(), bytes);
		assert_eq!(cache.usage_for_tests(), (0, 0));
	}

	#[test]
	fn prepared_wrong_scope_expiry_selection_and_staleness_never_write() {
		let (_dir, root, cache) = fixture();
		let text = "// FILE: file.txt\nreplacement\n";
		let mapping = PasteMapping::default();
		fs::create_dir(root.path.join("other")).unwrap();
		let reply = preview(&root, text, &mapping, &cache);
		assert!(matches!(
			apply(
				&root,
				"other",
				text,
				&mapping,
				&reply,
				&cache,
				RestoreSelection::default()
			),
			Response::Error {
				code: ErrorCode::Forbidden,
				..
			}
		));
		assert!(!root.path.join("other/file.txt").exists());
		assert!(!root.path.join("file.txt").exists());
		let reply = preview(&root, text, &mapping, &cache);
		cache.expire_for_tests();
		assert!(matches!(
			apply(
				&root,
				"",
				text,
				&mapping,
				&reply,
				&cache,
				RestoreSelection::default()
			),
			Response::Error {
				code: ErrorCode::Stale,
				..
			}
		));
		let reply = preview(&root, text, &mapping, &cache);
		assert!(matches!(
			apply(
				&root,
				"",
				text,
				&mapping,
				&reply,
				&cache,
				RestoreSelection {
					unchecked_creates: [99].into_iter().collect(),
					..Default::default()
				}
			),
			Response::Error {
				code: ErrorCode::BadRequest,
				..
			}
		));
		assert!(!root.path.join("file.txt").exists());
		fs::write(root.path.join("file.txt"), "before").unwrap();
		let reply = preview(&root, text, &mapping, &cache);
		fs::write(root.path.join("file.txt"), "external").unwrap();
		assert!(matches!(
			apply(
				&root,
				"",
				text,
				&mapping,
				&reply,
				&cache,
				RestoreSelection {
					overwrite_existing: true,
					..Default::default()
				}
			),
			Response::Error {
				code: ErrorCode::Stale,
				..
			}
		));
		assert_eq!(fs::read(root.path.join("file.txt")).unwrap(), b"external");
		assert_eq!(cache.usage_for_tests(), (0, 0));
	}

	#[test]
	fn prepared_capacity_refusal_keeps_metadata_and_legacy_apply_available() {
		let (_dir, root, cache) = fixture();
		let text = "// FILE: file.txt\nbody\n";
		let mapping = PasteMapping::default();
		cache.set_limits_for_tests(0, 8, Duration::from_secs(300));
		let reply = preview(&root, text, &mapping, &cache);
		assert!(reply.token.is_none());
		let full = reply.hydrate(text, "").unwrap();
		assert!(
			matches!(import_apply(&root,"",text,&mapping,&RestoreSelection::default(),&full.expect(),crate::proto::PROTOCOL_MAX,&CancelToken::new()),Response::Imported(r) if r.created_count==1)
		);
		assert_eq!(cache.usage_for_tests(), (0, 0));
	}

	#[test]
	fn prepared_payload_keeps_prefix_mapping_and_adjusted_restore_base() {
		let (_dir, root, cache) = fixture();
		fs::create_dir(root.path.join("mapped")).unwrap();
		let mapping = PasteMapping {
			prefixes: vec![("incoming".into(), "mapped".into())],
			..Default::default()
		};
		let text = "// FILE: incoming/a.txt\nprefix body\n";
		let reply = preview(&root, text, &mapping, &cache);
		assert!(
			matches!(apply(&root,"",text,&mapping,&reply,&cache,RestoreSelection::default()),Response::Imported(r) if r.created_count==1)
		);
		assert!(root.path.join("mapped/a.txt").is_file());
		assert!(!root.path.join("incoming/a.txt").exists());
		let (_dir, root, cache) = fixture();
		fs::create_dir_all(root.path.join("project/src")).unwrap();
		fs::create_dir_all(root.path.join("project/lib")).unwrap();
		let text = "// FILE: src/a.txt\nA\n// FILE: lib/b.txt\nB\n";
		let mapping = PasteMapping {
			adjust_paths: true,
			..Default::default()
		};
		let reply = preview(&root, text, &mapping, &cache);
		assert!(reply.suggestion.is_some());
		assert!(
			matches!(apply(&root,"",text,&mapping,&reply,&cache,RestoreSelection::default()),Response::Imported(r) if r.created_count==2)
		);
		assert!(root.path.join("project/src/a.txt").is_file());
		assert!(root.path.join("project/lib/b.txt").is_file());
		assert!(!root.path.join("src/a.txt").exists());
	}

	#[test]
	fn a_refused_plan_still_carries_the_restore_base_suggestion() {
		let (_dir, root, cache) = fixture();
		fs::create_dir(root.path.join("proj")).unwrap();
		// Adjusted, `proj/a.txt` and `a.txt` are one file: the engine
		// refuses the plan, but what it suggested for it is still true.
		let text =
			"// clipcode-root: work\n// FILE: proj/a.txt\nA\n// FILE: a.txt\nB\n";
		let run = |adjust_paths| {
			prepare_import(
				&root,
				"proj",
				text.into(),
				PasteMapping {
					adjust_paths,
					..Default::default()
				},
				false,
				crate::proto::PROTOCOL_MAX,
				&cache,
				&CancelToken::new(),
			)
		};
		let strip = snip_core::restore::RestoreBase::Strip {
			segment: "proj".into(),
		};
		match run(true) {
			Response::Error {
				code: ErrorCode::Collision,
				suggestion: Some(s),
				..
			} => assert_eq!(s.base, strip),
			other => {
				panic!("expected a collision with a suggestion: {other:?}")
			}
		}
		// As-is the plan stands, and says the same thing beside it.
		match run(false) {
			Response::ImportPrepared(p) => {
				assert_eq!(p.suggestion.map(|s| s.base), Some(strip));
			}
			other => panic!("expected a plan: {other:?}"),
		}
		// A refusal before the destination is known has nothing to suggest.
		match prepare_import(
			&root,
			"missing",
			text.into(),
			PasteMapping::default(),
			false,
			crate::proto::PROTOCOL_MAX,
			&cache,
			&CancelToken::new(),
		) {
			Response::Error {
				suggestion: None, ..
			} => {}
			other => panic!("expected a refusal without one: {other:?}"),
		}
	}

	#[cfg(unix)]
	#[test]
	fn prepared_scope_uses_the_canonical_workspace_not_its_symlink_spelling() {
		let dir = tempfile::tempdir().unwrap();
		let actual = dir.path().join("actual");
		fs::create_dir(&actual).unwrap();
		let alias = dir.path().join("alias");
		std::os::unix::fs::symlink(&actual, &alias).unwrap();
		let root = SharedRoot::new(&alias).unwrap();
		let cache = PreparedCache::new();
		let text = "// FILE: a.txt\nA\n";
		let mapping = PasteMapping::default();
		let reply = preview(&root, text, &mapping, &cache);
		assert!(
			matches!(apply(&SharedRoot::new(&actual).unwrap(),"",text,&mapping,&reply,&cache,RestoreSelection::default()),Response::Imported(r) if r.created_count==1)
		);
		assert!(actual.join("a.txt").is_file());
	}
}
