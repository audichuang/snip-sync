//! Paste on the worker: the master sends the payload and the user's
//! choices, and the worker plans and writes with the engine a local paste
//! runs ([`plan_import_with`], [`CommitReplayPreview`]) on its own disk.
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
use snip_core::restore::{RestorePlan, RestoreSelection};
use snip_core::transfer::{
	plan_import_with, CanonicalRootId, CommitReplayPreview,
	DestinationFreshnessSnapshot, TransferError, TransferImportPlan,
};

use crate::prepared::{CacheError, CachedImport, PreparedCache};
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

/// The paths a master sent back with a snapshot must be ones this paste
/// could have recorded: under `base`.
fn check_snapshot(
	base: &Path,
	freshness: &DestinationFreshnessSnapshot,
) -> Result<(), Refusal> {
	let inside = |p: &Path| p.is_absolute() && p.starts_with(base);
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
	let suggestion = crate::pastetarget::suggest(&primary, &paths, text);
	let (import, roots) = crate::pastetarget::import_mapping(
		primary,
		primary_id,
		suggestion.as_ref().filter(|_| mapping.adjust_paths),
		mapping.prefixes.iter().map(|(prefix, rel)| {
			if prefix.is_empty() || prefix.contains(['/', '\\']) {
				return Err(refused(ErrorCode::BadRequest, "invalid prefix"));
			}
			Ok::<_, Refusal>((prefix.clone(), destination(root, rel)?))
		}),
		|folder| {
			CanonicalRootId::new(folder).map_err(|e| Box::new(io_error(e)))
		},
	)?;
	let plan =
		plan_import_with(text, header, &roots, &import, &options(cancel))
			.map_err(|e| transfer_refusal(e, cancel))?;
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
	let run = || -> Result<Response, Refusal> {
		let Planned { plan, suggestion } =
			plan(root, dest, text, mapping, cancel)?;
		Ok(Response::ImportPlanned(ImportPlanned {
			digest: digest(plan.restore_plan())?,
			plan,
			suggestion,
		}))
	};
	run().unwrap_or_else(|reply| *reply)
}

pub(crate) fn prepare_import(
	root: &SharedRoot,
	dest: &str,
	text: String,
	mapping: PasteMapping,
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
		let Planned { plan, suggestion } =
			plan(root, dest, &text, &mapping, cancel)?;
		let metadata = crate::proto::ImportMetadata::of(
			&plan,
			&text,
			&mapping.header_format,
		)
		.map_err(|message| refused(ErrorCode::BadRequest, message))?;
		let digest = digest(plan.restore_plan())?;
		let destination = plan.roots().first().cloned().ok_or_else(|| {
			refused(ErrorCode::BadRequest, "no paste destination")
		})?;
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

pub(crate) fn apply_prepared(
	root: &SharedRoot,
	dest: &str,
	token: &str,
	selection: &RestoreSelection,
	expect: &ImportExpect,
	cache: &PreparedCache,
	cancel: &CancelToken,
) -> Response {
	let at = match destination(root, dest) {
		Ok(at) => at,
		Err(reply) => return *reply,
	};
	let lease = match cache.take(token, &root.path, &at, &expect.digest) {
		Ok(lease) => lease,
		Err(why) => return *refused(
			if why == CacheError::Scope { ErrorCode::Forbidden } else { ErrorCode::Stale },
			match why {
				CacheError::Missing => "prepared paste expired, was consumed, or belongs to another worker; preview again",
				CacheError::Scope => "prepared paste belongs to another workspace or destination; preview again",
				CacheError::Digest => "prepared paste does not match the preview; preview again",
			}),
	};
	let payload = lease.payload();
	// The raw input is retained, never an executable plan. Keep exactly the
	// legacy re-plan/digest/freshness/selection and locked-write checks.
	import_apply(
		root,
		dest,
		&payload.text,
		&payload.mapping,
		selection,
		expect,
		cancel,
	)
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
		check_snapshot(&root.path, &expect.freshness)?;
		expect
			.freshness
			.revalidate()
			.map_err(|e| transfer_refusal(e, cancel))?;
		let Planned { plan, .. } = plan(root, dest, text, mapping, cancel)?;
		if digest(plan.restore_plan())? != expect.digest
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
		let mut held_once = false;
		let result = plan
			.apply_observed(selection, &mut |_op, _outcome| {
				if !held_once {
					held_once = true;
					hold_after_first_write();
				}
			})
			.map_err(|e| transfer_refusal(e, cancel))?;
		Ok(Response::Imported(result))
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
	commits::parse_commit_payload(text)
		.map_err(|e| refused(ErrorCode::BadRequest, e.to_string()))
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
		Ok(Response::Replayed(result))
	};
	run().unwrap_or_else(|resp| *resp)
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
			matches!(import_apply(&root,"",text,&mapping,&RestoreSelection::default(),&full.expect(),&CancelToken::new()),Response::Imported(r) if r.created_count==1)
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
