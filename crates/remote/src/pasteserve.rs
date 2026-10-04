//! Paste on the worker: the master sends the payload and the user's
//! choices, and the worker plans and writes with the engine a local paste
//! runs ([`plan_import_with`], [`CommitReplayPreview`]) on its own disk.
//!
//! Stateless: an Apply plans again and refuses as stale whatever changed
//! since the preview the master showed. Nothing is ever written inside a
//! `.git` folder or a Git directory, and every destination stays inside
//! the workspace.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use snip_core::commits;
use snip_core::gitrun::{CancelToken, GitPool, RunOptions};
use snip_core::gitsrc::Git;
use snip_core::gitview::ReadProfile;
use snip_core::paths::has_git_segment;
use snip_core::restore::{
	suggest_restore_base, FsProbe, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	plan_import_with, CanonicalRootId, CommitReplayPreview,
	DestinationFreshnessSnapshot, ImportMapping, TransferError,
	TransferImportPlan,
};

use crate::proto::{
	valid_rel_path, ErrorCode, ImportExpect, ImportPlanned, PasteMapping,
	ReplayExpect, Response,
};
use crate::worker::{io_error, SharedRoot};

type Refusal = Box<Response>;

fn refused(code: ErrorCode, message: impl Into<String>) -> Refusal {
	Box::new(Response::Error {
		code,
		message: message.into(),
	})
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
	refused(code, err.paste_message())
}

fn stale(root: &Path, reason: &str) -> Refusal {
	transfer_refusal(
		TransferError::StaleDestination {
			root: root.to_path_buf(),
			reason: reason.into(),
		},
		&CancelToken::new(),
	)
}

/// A folder with the files of a Git directory (bare repository, separate
/// git dir, `.git` itself).
fn is_git_dir(dir: &Path) -> bool {
	dir.join("HEAD").is_file()
		&& dir.join("objects").is_dir()
		&& dir.join("refs").is_dir()
}

/// True when `path` (real, absolute) is or lies inside a `.git` folder or
/// a Git directory.
fn in_git_dir(path: &Path) -> bool {
	has_git_segment(&path.to_string_lossy()) || path.ancestors().any(is_git_dir)
}

/// Where `path` really lands, the deepest existing ancestor resolved: a
/// file that does not exist yet still lands under a real folder.
fn real_target(path: &Path) -> Option<PathBuf> {
	let mut rest = Vec::new();
	let mut cur = path;
	loop {
		if let Ok(real) = dunce::canonicalize(cur) {
			let mut out = real;
			for name in rest.iter().rev() {
				out.push(name);
			}
			return Some(out);
		}
		rest.push(cur.file_name()?.to_os_string());
		cur = cur.parent()?;
	}
}

/// The folder `rel` of the workspace as a paste destination: real, a
/// folder, inside the workspace and outside every Git directory.
fn destination(root: &SharedRoot, rel: &str) -> Result<PathBuf, Refusal> {
	if !valid_rel_path(rel, true) {
		return Err(refused(ErrorCode::BadRequest, "invalid destination"));
	}
	if has_git_segment(rel) {
		return Err(refused(
			ErrorCode::Forbidden,
			format!(
				"'{rel}' is inside a .git folder; paste never writes there"
			),
		));
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
	if in_git_dir(&path) {
		return Err(refused(
			ErrorCode::Forbidden,
			format!(
				"'{}' is inside a Git directory; paste never writes there",
				path.display()
			),
		));
	}
	Ok(path)
}

/// Refuses a plan that would write outside the workspace or into a Git
/// directory, however a symlink on the way resolves.
fn check_targets<'a>(
	root: &SharedRoot,
	targets: impl IntoIterator<Item = &'a Path>,
) -> Result<(), Refusal> {
	for target in targets {
		let real = real_target(target);
		let ok = real.as_deref().is_some_and(|real| {
			real.starts_with(&root.path) && !in_git_dir(real)
		});
		if !ok {
			return Err(refused(
				ErrorCode::Forbidden,
				format!(
					"'{}' is outside the workspace or inside a Git directory; \
					 paste never writes there",
					target.display()
				),
			));
		}
	}
	Ok(())
}

fn plan_targets(plan: &RestorePlan) -> impl Iterator<Item = &Path> {
	let creates = plan.create_operations.iter().map(|o| &o.absolute_path);
	let deletes = plan.delete_operations.iter().map(|o| &o.absolute_path);
	creates.chain(deletes).map(PathBuf::as_path)
}

/// The paths a master sent back with a snapshot must be ones this
/// workspace could have recorded.
fn check_snapshot(
	root: &SharedRoot,
	freshness: &DestinationFreshnessSnapshot,
) -> Result<(), Refusal> {
	let inside = |p: &Path| p.is_absolute() && p.starts_with(&root.path);
	let ok = freshness.roots.keys().all(|r| inside(r.path()))
		&& freshness
			.target_files
			.iter()
			.all(|(p, t)| inside(p) && inside(t.root.path()));
	if ok {
		Ok(())
	} else {
		Err(refused(
			ErrorCode::BadRequest,
			"the preview names a path outside the workspace",
		))
	}
}

fn digest(plan: &RestorePlan) -> String {
	let json = serde_json::to_vec(plan).unwrap_or_default();
	Sha256::digest(&json)
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect()
}

struct Planned {
	plan: TransferImportPlan,
	suggestion: Option<snip_core::restore::RestoreBaseSuggestion>,
}

/// Plans exactly as `snip paste` and the app do, with the destination and
/// mapping resolved inside the workspace.
fn plan(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	mapping: &PasteMapping,
	cancel: &CancelToken,
) -> Result<Planned, Refusal> {
	let primary = destination(root, dest)?;
	let primary_id =
		CanonicalRootId::new(&primary).map_err(|e| Box::new(io_error(e)))?;
	let header = mapping.header_format.as_str();
	let entries = snip_core::format::parse_clipboard(text, header);
	if entries.is_empty() {
		return Err(refused(
			ErrorCode::BadRequest,
			"No Snipcode file headers found in clipboard.",
		));
	}
	let paths: Vec<String> = entries.into_iter().map(|e| e.path).collect();
	let suggestion = suggest_restore_base(
		&primary.to_string_lossy(),
		&paths,
		&FsProbe,
		snip_core::format::extract_source_root(text).as_deref(),
	);
	let mut import = match (&suggestion, mapping.adjust_paths) {
		(Some(s), true) => ImportMapping::from_restore_base(s, primary_id),
		_ => ImportMapping::with_primary(primary_id),
	};
	let mut roots = vec![primary];
	for (prefix, rel) in &mapping.prefixes {
		if prefix.is_empty() || prefix.contains(['/', '\\']) {
			return Err(refused(ErrorCode::BadRequest, "invalid prefix"));
		}
		let folder = destination(root, rel)?;
		let id =
			CanonicalRootId::new(&folder).map_err(|e| Box::new(io_error(e)))?;
		if !roots.contains(&folder) {
			roots.push(folder);
		}
		import.map_prefix(prefix.clone(), id);
	}
	let plan =
		plan_import_with(text, header, &roots, &import, &options(cancel))
			.map_err(|e| transfer_refusal(e, cancel))?;
	check_targets(root, plan_targets(plan.restore_plan()))?;
	Ok(Planned { plan, suggestion })
}

/// Answers [`crate::proto::Request::ImportPlan`].
pub(crate) fn import_plan(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	mapping: &PasteMapping,
	cancel: &CancelToken,
) -> Response {
	match plan(root, dest, text, mapping, cancel) {
		Ok(Planned { plan, suggestion }) => {
			Response::ImportPlanned(ImportPlanned {
				digest: digest(plan.restore_plan()),
				plan,
				suggestion,
			})
		}
		Err(resp) => *resp,
	}
}

/// Answers [`crate::proto::Request::ImportApply`]: the preview's snapshot
/// is checked first, as a local Apply does, then the plan made again must
/// be the one previewed.
pub(crate) fn import_apply(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	mapping: &PasteMapping,
	selection: &RestoreSelection,
	expect: &ImportExpect,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		check_snapshot(root, &expect.freshness)?;
		expect
			.freshness
			.revalidate()
			.map_err(|e| transfer_refusal(e, cancel))?;
		let Planned { plan, .. } = plan(root, dest, text, mapping, cancel)?;
		if digest(plan.restore_plan()) != expect.digest
			|| plan.destination_freshness() != &expect.freshness
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
		let result = plan
			.apply(selection)
			.map_err(|e| transfer_refusal(e, cancel))?;
		Ok(Response::Imported(result))
	};
	run().unwrap_or_else(|resp| *resp)
}

/// The repository a commit payload replays onto: the top folder of the
/// repository at `dest`, which must lie inside the workspace too.
fn replay_root(
	root: &SharedRoot,
	dest: &str,
	cancel: &CancelToken,
) -> Result<PathBuf, Refusal> {
	let dir = destination(root, dest)?;
	let git = Git::open_with(&dir, &options(cancel))
		.map_err(|e| transfer_refusal(TransferError::Git(e), cancel))?;
	let top =
		dunce::canonicalize(git.root()).map_err(|e| Box::new(io_error(e)))?;
	if !top.starts_with(&root.path) || in_git_dir(&top) {
		return Err(refused(
			ErrorCode::Forbidden,
			format!(
				"the repository '{}' is not inside the workspace",
				top.display()
			),
		));
	}
	Ok(top)
}

fn payload(text: &str) -> Result<commits::CommitsPayload, Refusal> {
	commits::parse_commit_payload(text)
		.map_err(|e| refused(ErrorCode::BadRequest, e.to_string()))
}

fn replay_targets(plan: &commits::CommitReplayPlan) -> Vec<&Path> {
	plan.commits
		.iter()
		.flat_map(|c| &c.files)
		.flat_map(|f| [&f.absolute_path, &f.old_absolute_path])
		.flatten()
		.map(PathBuf::as_path)
		.collect()
}

/// Answers [`crate::proto::Request::ReplayPlan`].
pub(crate) fn replay_plan(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let top = replay_root(root, dest, cancel)?;
		let payload = payload(text)?;
		let preview =
			CommitReplayPreview::capture_with(&top, &payload, &options(cancel))
				.map_err(|e| transfer_refusal(e, cancel))?;
		check_targets(root, replay_targets(preview.plan()))?;
		Ok(Response::ReplayPlanned(ReplayExpect::of(&preview)))
	};
	run().unwrap_or_else(|resp| *resp)
}

/// Answers [`crate::proto::Request::ReplayApply`]: the preview put back
/// together re-plans the payload and re-reads every recorded path under the
/// replay lock, exactly as a local Apply.
pub(crate) fn replay_apply(
	root: &SharedRoot,
	dest: &str,
	text: &str,
	expect: ReplayExpect,
	check_only: bool,
	cancel: &CancelToken,
) -> Response {
	let run = || -> Result<Response, Refusal> {
		let top = replay_root(root, dest, cancel)?;
		if expect.destination != top || expect.plan.root != top {
			return Err(stale(
				&top,
				"replay eligibility changed after preview",
			));
		}
		check_snapshot(root, &expect.freshness)?;
		check_targets(root, replay_targets(&expect.plan))?;
		let payload = payload(text)?;
		let preview = CommitReplayPreview::from_parts(
			expect.destination,
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
		Ok(Response::Replayed(result))
	};
	run().unwrap_or_else(|resp| *resp)
}
