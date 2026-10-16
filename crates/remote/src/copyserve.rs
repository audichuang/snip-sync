//! Copy on the worker: the same engine a local copy runs
//! ([`snip_core::transfer::copy_selection`], `plan_commit_export_exact_with`),
//! on paths of the workspace a master names.

use std::path::PathBuf;

use snip_core::gitrun::{CancelToken, GitPool};
use snip_core::gitsrc::{Git, GitSource};
use snip_core::gitview::ReadProfile;
use snip_core::reason::Reason;
use snip_core::settings::Settings;
use snip_core::transfer::{
	changed_items, copy_selection, plan_commit_export_exact_with,
	selection_from_paths, CanonicalRootId, ExportItem, ExportSelection,
	SourceKind, CLIPBOARD_PAYLOAD_MAX,
};

use crate::proto::{
	valid_rel_path, valid_rev, ErrorCode, ExportTarget, Response,
};
use crate::worker::{io_error, SharedRoot};

fn refused(code: ErrorCode, message: impl Into<String>) -> Response {
	Response::Error {
		code,
		message: message.into(),
		patch: false,
		reason: None,
	}
}

/// A refusal that carries an engine error's reason, scrubbed by
/// [`Reason::beside_code`] so it names no path of this machine. The code and
/// the message stay what they were (the CLI's bytes); a cancelled job says
/// so by its code alone.
fn refused_err(
	code: ErrorCode,
	message: impl Into<String>,
	reason: Reason,
) -> Response {
	Response::Error {
		code,
		message: message.into(),
		patch: false,
		reason: if code == ErrorCode::Cancelled {
			None
		} else {
			reason.beside_code()
		},
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

/// Answers [`crate::proto::Request::Export`]. A target with an empty
/// `path` is the whole target folder: its children, gathered by the same
/// [`selection_from_paths`] a local `snip copy .` runs, so the worker
/// expands it exactly as the local CLI does — the master lists nothing.
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
	let mut selection_skipped = 0usize;
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
		if t.path.is_empty() {
			if t.source != SourceKind::File || t.change_type.is_some() {
				return refused(ErrorCode::BadRequest, "invalid copy item");
			}
			let whole = match selection_from_paths(
				&dir,
				&dir,
				std::slice::from_ref(&dir),
			) {
				Ok(whole) => whole,
				Err(err) => {
					return refused_err(
						ErrorCode::Io,
						err.to_string(),
						err.reason(),
					)
				}
			};
			selection_skipped += whole.skipped;
			export.extend(whole.sel.items);
			continue;
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
		Err(snip_core::transfer::TransferError::EmptySelection) => {
			// An empty folder selected as a whole, with nothing under it.
			return refused(ErrorCode::BadRequest, "nothing to copy");
		}
		Err(err) => {
			return refused_err(
				ErrorCode::BadRequest,
				err.to_string(),
				err.reason(),
			)
		}
	};
	match copy_selection(sel, settings, file_limit, &options(cancel), |_| {}) {
		Ok(mut out) => {
			out.skipped += selection_skipped;
			Response::Copied(out)
		}
		Err(err) if cancel.is_cancelled() => {
			refused_err(ErrorCode::Cancelled, err.to_string(), err.reason())
		}
		Err(err) => refused_err(ErrorCode::Io, err.to_string(), err.reason()),
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
		Err(err) => {
			return refused_err(
				ErrorCode::NotARepository,
				err.to_string(),
				err.reason(),
			)
		}
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
			return refused_err(code, err.to_string(), err.reason());
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
		Err(err) => {
			return refused_err(
				ErrorCode::BadRequest,
				err.to_string(),
				err.reason(),
			)
		}
	};
	let sel = if graph {
		sel.with_filter_root(Some(dir))
	} else {
		sel
	};
	match copy_selection(sel, settings, file_limit, &opts, |_| {}) {
		Ok(out) => Response::Copied(out),
		Err(err) if cancel.is_cancelled() => {
			refused_err(ErrorCode::Cancelled, err.to_string(), err.reason())
		}
		Err(err) => refused_err(ErrorCode::Io, err.to_string(), err.reason()),
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
		Err(err) => {
			return refused_err(
				ErrorCode::NotARepository,
				err.to_string(),
				err.reason(),
			)
		}
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
			refused_err(ErrorCode::Cancelled, err.to_string(), err.reason())
		}
		Err(err) => refused_err(ErrorCode::Io, err.to_string(), err.reason()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::Command;

	fn git(dir: &std::path::Path, args: &[&str]) -> String {
		let out = Command::new("git")
			.arg("-C")
			.arg(dir)
			.args(["-c", "user.name=t", "-c", "user.email=t@t"])
			.args(["-c", "commit.gpgsign=false"])
			.args(args)
			.output()
			.unwrap();
		assert!(out.status.success(), "git {args:?}");
		String::from_utf8(out.stdout).unwrap().trim().to_string()
	}

	/// A remote ExportCommits refusal carries the engine's Reason, as a
	/// local copy does, and the serialized frame names no path of the
	/// worker's machine.
	#[test]
	fn export_commits_refusal_carries_scrubbed_reason() {
		let tmp = tempfile::tempdir().unwrap();
		let dir = tmp.path();
		git(dir, &["init", "-q"]);
		let mut shas = Vec::new();
		for n in 0..3 {
			std::fs::write(dir.join("f.txt"), format!("{n}")).unwrap();
			git(dir, &["add", "."]);
			git(dir, &["commit", "-q", "-m", &format!("c{n}")]);
			shas.push(git(dir, &["rev-parse", "HEAD"]));
		}
		let root = SharedRoot::new(dir).unwrap();
		let cancel = CancelToken::new();

		// A selection that skips the middle commit is not a chain.
		let resp = export_commits(
			&root,
			"",
			&shas[2],
			&[shas[0].clone(), shas[2].clone()],
			&cancel,
		);
		let Response::Error {
			code,
			reason: Some(reason),
			..
		} = &resp
		else {
			panic!("expected a typed refusal, got {resp:?}");
		};
		assert_eq!(*code, ErrorCode::Io);
		assert!(matches!(reason, Reason::Discontinuous { .. }), "{reason:?}");
		let wire = serde_json::to_string(&resp).unwrap();
		let shown = root.path.display().to_string();
		assert!(!wire.contains(&shown), "{wire}");

		// A payload over the limit keeps its own reason too.
		let err = snip_core::commits::CommitError::PayloadLimit {
			limit: 1,
			actual: 2,
		};
		let resp = refused_err(ErrorCode::Io, err.to_string(), err.reason());
		assert!(matches!(
			resp,
			Response::Error {
				reason: Some(Reason::PayloadTooLarge { .. }),
				..
			}
		));
		// A cancelled job says so by its code alone.
		let resp =
			refused_err(ErrorCode::Cancelled, "cancelled", Reason::Cancelled);
		assert!(matches!(resp, Response::Error { reason: None, .. }));
	}
}
