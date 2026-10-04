//! Copy on the worker: the same engine a local copy runs
//! ([`snip_core::transfer::copy_selection`], `plan_commit_export_exact_with`),
//! on paths of the workspace a master names.

use std::path::PathBuf;

use snip_core::gitrun::{CancelToken, GitPool};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::gitview::ReadProfile;
use snip_core::settings::Settings;
use snip_core::transfer::{
	changed_items, copy_selection, plan_commit_export_exact_with,
	CanonicalRootId, ExportItem, ExportSelection, SourceKind,
	CLIPBOARD_PAYLOAD_MAX,
};

use crate::proto::{
	valid_rel_path, valid_rev, ErrorCode, ExportTarget, Response,
};
use crate::worker::{io_error, SharedRoot};

fn refused(code: ErrorCode, message: impl Into<String>) -> Response {
	Response::Error {
		code,
		message: message.into(),
	}
}

fn valid_source(source: &SourceKind) -> bool {
	match source {
		SourceKind::Commit { rev } => valid_rev(rev),
		SourceKind::Range { base, tip } => valid_rev(base) && valid_rev(tip),
		_ => true,
	}
}

/// Interactive reads on the worker's own pool, with no output cap of
/// their own: the copy's file size limit bounds them.
fn options(cancel: &CancelToken) -> snip_core::gitrun::RunOptions {
	let mut opts = ReadProfile::Interactive.options(Some(cancel.clone()));
	opts.pool = GitPool::Served;
	opts
}

/// A folder of the workspace, by its relative path ("" is the workspace).
#[allow(clippy::result_large_err)]
fn folder(root: &SharedRoot, rel: &str) -> Result<PathBuf, Response> {
	if rel.is_empty() {
		return Ok(root.path.clone());
	}
	snip_core::browser::inside(&root.path, rel).map_err(io_error)
}

/// Answers [`crate::proto::Request::Export`].
pub(crate) fn export(
	root: &SharedRoot,
	items: Vec<ExportTarget>,
	settings: &Settings,
	file_limit: usize,
	cancel: &CancelToken,
) -> Response {
	if items.is_empty() {
		return refused(ErrorCode::BadRequest, "nothing to copy");
	}
	let mut roots: Vec<PathBuf> = Vec::new();
	let mut export = Vec::with_capacity(items.len());
	for t in items {
		if !valid_rel_path(&t.root, true)
			|| !valid_rel_path(&t.path, true)
			|| !valid_source(&t.source)
		{
			return refused(ErrorCode::BadRequest, "invalid copy item");
		}
		let dir = match folder(root, &t.root) {
			Ok(dir) => dir,
			Err(resp) => return resp,
		};
		let id = match CanonicalRootId::new(&dir) {
			Ok(id) => id,
			Err(err) => return io_error(err),
		};
		if !roots.iter().any(|r| r == id.path()) {
			roots.push(id.path().to_path_buf());
		}
		export.push(ExportItem {
			root: id,
			relative_path: t.path,
			source: t.source,
			change_type: t.change_type,
			gitlink: false,
		});
	}
	let primary = roots.first().cloned();
	let sel = match ExportSelection::new(roots, primary, export) {
		Ok(sel) => sel,
		Err(err) => return refused(ErrorCode::BadRequest, err.to_string()),
	};
	match copy_selection(sel, settings, file_limit, &options(cancel), |_| {}) {
		Ok(out) => Response::Copied(out),
		Err(err) if cancel.is_cancelled() => {
			refused(ErrorCode::Cancelled, err.to_string())
		}
		Err(err) => refused(ErrorCode::Io, err.to_string()),
	}
}

/// Answers [`crate::proto::Request::ExportChanges`]: the change selection
/// for `source` is resolved here with the shared [`changed_items`], so the
/// copy sees exactly what a local `snip copy --working/--staged/--commit`
/// sees — same order, dedup, change types, and staged-only entries.
pub(crate) fn export_changes(
	root: &SharedRoot,
	repo: &str,
	source: &GitSource,
	settings: &Settings,
	file_limit: usize,
	cancel: &CancelToken,
) -> Response {
	if !valid_rel_path(repo, true) || !valid_change_source(source) {
		return refused(ErrorCode::BadRequest, "invalid change copy");
	}
	let dir = match folder(root, repo) {
		Ok(dir) => dir,
		Err(resp) => return resp,
	};
	let opts = options(cancel);
	let git = match Git::open_with(&dir, &opts) {
		Ok(git) => git,
		Err(err) => return refused(ErrorCode::NotARepository, err.to_string()),
	};
	// Local copy_git: commit/range sources resolve against the repository
	// root (their paths are repo-relative); working/staged keep the opened
	// folder and skip changes outside it.
	let graph = matches!(source, GitSource::Commit(_) | GitSource::Range(..));
	let effective_root = if graph {
		git.root().to_path_buf()
	} else {
		dir.clone()
	};
	let root_id = match CanonicalRootId::new(&effective_root) {
		Ok(id) => id,
		Err(err) => return io_error(err),
	};
	let changed = match changed_items(&root_id, &git, source, &opts) {
		Ok(changed) => changed,
		Err(err) => {
			let code = if cancel.is_cancelled() {
				ErrorCode::Cancelled
			} else {
				ErrorCode::Io
			};
			return refused(code, err.to_string());
		}
	};
	if changed.items.is_empty() {
		return refused(ErrorCode::BadRequest, "No Git changes found to copy.");
	}
	let sel = match ExportSelection::new(
		vec![effective_root.clone()],
		Some(effective_root),
		changed.items,
	) {
		Ok(sel) => sel,
		Err(err) => return refused(ErrorCode::BadRequest, err.to_string()),
	};
	let sel = if graph {
		sel.with_filter_root(Some(dir))
	} else {
		sel
	};
	match copy_selection(sel, settings, file_limit, &opts, |_| {}) {
		Ok(out) => Response::Copied(out),
		Err(err) if cancel.is_cancelled() => {
			refused(ErrorCode::Cancelled, err.to_string())
		}
		Err(err) => refused(ErrorCode::Io, err.to_string()),
	}
}

fn valid_change_source(source: &GitSource) -> bool {
	match source {
		GitSource::Commit(rev) => valid_rev(rev),
		GitSource::Range(base, tip) => valid_rev(base) && valid_rev(tip),
		GitSource::Working | GitSource::Staged => true,
	}
}

/// Answers [`crate::proto::Request::ExportCommits`].
pub(crate) fn export_commits(
	root: &SharedRoot,
	repo: &str,
	tip: &str,
	selected: &[String],
	cancel: &CancelToken,
) -> Response {
	if !valid_rel_path(repo, true)
		|| !valid_rev(tip)
		|| selected.is_empty()
		|| !selected.iter().all(|s| valid_rev(s))
	{
		return refused(ErrorCode::BadRequest, "invalid commit copy");
	}
	let dir = match folder(root, repo) {
		Ok(dir) => dir,
		Err(resp) => return resp,
	};
	let opts = options(cancel);
	let git = match Git::open_with(&dir, &opts) {
		Ok(git) => git,
		Err(err) => return refused(ErrorCode::NotARepository, err.to_string()),
	};
	match plan_commit_export_exact_with(
		&git,
		tip,
		selected,
		&opts,
		CLIPBOARD_PAYLOAD_MAX,
	) {
		Ok(export) => Response::CommitsCopied(export.outcome()),
		Err(err) if cancel.is_cancelled() => {
			refused(ErrorCode::Cancelled, err.to_string())
		}
		Err(err) => refused(ErrorCode::Io, err.to_string()),
	}
}
