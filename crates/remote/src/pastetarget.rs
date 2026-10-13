//! Where a paste plans and writes. Both front ends plan, check and write
//! through `PasteTarget`: a local target runs the core paste engine on
//! this machine, a remote one asks the workspace's worker, which runs the
//! same engine on its own disk (`crate::pasteserve`).
//!
//! Each keeps its own freshness check. A local Apply writes the plan it
//! kept, re-checked under the repository write lock. A worker keeps no
//! state: its Apply plans again and refuses as stale whatever differs from
//! the preview.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use snip_core::commits::{CommitsPayload, ReplayResult};
use snip_core::gitrun::RunOptions;
use snip_core::gitsrc::GitError;
use snip_core::restore::{
	suggest_restore_base, FsProbe, RestoreBaseSuggestion,
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	plan_import_with, CanonicalRootId, CommitReplayPreview,
	DestinationFreshnessSnapshot, ImportMapping, ReplayFilePreview,
	ReplayFileSide, TransferError, TransferImportPlan,
};

use crate::proto::{ErrorCode, ImportExpect, PasteMapping};
use crate::{Client, RemoteError};

/// The restore base `--adjust-paths` would apply, probed under `primary`.
pub(crate) fn suggest(
	primary: &Path,
	paths: &[String],
	text: &str,
) -> Option<RestoreBaseSuggestion> {
	suggest_restore_base(
		&primary.to_string_lossy(),
		paths,
		&FsProbe,
		snip_core::format::extract_source_root(text).as_deref(),
	)
}

/// The routing every file paste plans with: `snip paste`, the app and a
/// worker. Entries land in `primary`, or under it adjusted by
/// `restore_base`. Entries under `prefix/` land in that prefix's folder.
/// Returns the mapping and the destination roots `plan_import_with` needs:
/// `primary` first, then each folder once. The prefixes are read in order,
/// so a caller that validates and resolves each one meets its errors in
/// order.
pub(crate) fn import_mapping<E>(
	primary: PathBuf,
	primary_id: CanonicalRootId,
	restore_base: Option<&RestoreBaseSuggestion>,
	prefixes: impl IntoIterator<Item = Result<(String, PathBuf), E>>,
	root_id: impl Fn(&Path) -> Result<CanonicalRootId, E>,
) -> Result<(ImportMapping, Vec<PathBuf>), E> {
	let mut mapping = match restore_base {
		Some(s) => ImportMapping::from_restore_base(s, primary_id),
		None => ImportMapping::with_primary(primary_id),
	};
	let mut roots = vec![primary];
	for prefix in prefixes {
		let (prefix, folder) = prefix?;
		let id = root_id(&folder)?;
		if !roots.contains(&folder) {
			roots.push(folder);
		}
		mapping.map_prefix(prefix, id);
	}
	Ok((mapping, roots))
}

/// How `GitError::WorktreeBusy` prints, so a worker's refusal is known.
/// A unit test renders the real `GitError` and checks it still contains
/// this, so the copy cannot drift from core's `#[error]` unnoticed.
pub const REPO_BUSY_TEXT: &str = "another paste is writing to this repository";

/// Where a paste plans and writes.
#[derive(Clone)]
pub enum PasteTarget {
	/// This machine. `dest` is the folder pasted into, as the caller spells it.
	Local { dest: PathBuf },
	/// A folder of a worker's workspace.
	Remote {
		client: Arc<Client>,
		workspace: String,
		/// Relative to the workspace; "" is the workspace itself.
		dest: String,
	},
}

/// Where the entries of a file payload land besides the destination, and
/// how the payload is read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PasteRoute {
	/// Entries under `prefix/` land in the folder: an absolute folder for a
	/// local target, a workspace-relative one for a remote target. Build a
	/// route for the target it goes to: a worker refuses an absolute folder
	/// (`valid_rel_path`), but a local target would resolve a relative one
	/// against the working directory.
	pub prefixes: Vec<(String, PathBuf)>,
	/// The CLI's `--adjust-paths`: plan with the restore-base suggestion.
	pub adjust_paths: bool,
	/// The settings' header format ("" is the default).
	pub header_format: String,
}

impl PasteRoute {
	/// The route as the worker receives it. A remote folder came from a
	/// workspace-relative `String`, so the conversion loses nothing.
	pub fn mapping(&self) -> PasteMapping {
		PasteMapping {
			prefixes: self
				.prefixes
				.iter()
				.map(|(prefix, folder)| {
					(prefix.clone(), folder.to_string_lossy().into_owned())
				})
				.collect(),
			adjust_paths: self.adjust_paths,
			header_format: self.header_format.clone(),
		}
	}
}

/// A planned file paste: the engine's plan, with the target's paths, and
/// from a worker its digest of that plan. It is applied by the target that
/// made it.
#[derive(Debug, Clone)]
pub struct FilePastePlan {
	plan: TransferImportPlan,
	/// Remote only: the worker's digest of `plan`, sent back with the Apply.
	digest: Option<String>,
}

impl FilePastePlan {
	pub fn import_plan(&self) -> &TransferImportPlan {
		&self.plan
	}
	pub fn restore_plan(&self) -> &RestorePlan {
		self.plan.restore_plan()
	}
	/// The destination roots: the destination first, then each prefix folder once.
	pub fn roots(&self) -> &[PathBuf] {
		self.plan.roots()
	}
	pub fn destination_freshness(&self) -> &DestinationFreshnessSnapshot {
		self.plan.destination_freshness()
	}
	/// Owned buffer capacities: the plan's plus the digest's.
	pub fn retained_heap_bytes(&self) -> usize {
		self.plan
			.retained_heap_bytes()
			.saturating_add(self.digest.as_ref().map_or(0, String::capacity))
	}
	/// Whether the target of delete `index` exists. Locally this is a stat
	/// made now. A worker's paths are not this machine's, so for a remote
	/// plan it is the worker's snapshot.
	pub fn delete_target_exists(&self, index: usize) -> bool {
		let Some(op) = self.plan.delete_operations().get(index) else {
			return false;
		};
		match self.digest {
			None => op.absolute_path.exists(),
			Some(_) => self
				.plan
				.destination_freshness()
				.target_files
				.get(&op.absolute_path)
				.is_some_and(|t| t.file_state.is_some()),
		}
	}
}

/// Why a paste did not plan, check or write.
#[derive(Debug, thiserror::Error)]
pub enum PasteError {
	/// A local folder (the destination or a prefix's) does not resolve.
	#[error("{source}")]
	Destination { path: PathBuf, source: io::Error },
	/// The local engine refused or failed. It prints as `paste_message`,
	/// the words a worker sends for the same refusal.
	#[error("{}", .0.paste_message())]
	Local(TransferError),
	/// The worker refused, or could not be reached. Nothing was written.
	#[error(transparent)]
	Remote(RemoteError),
	/// A remote Apply's answer was lost: the worker may have written.
	/// Only `apply_files` and `apply_commits` return it.
	#[error(transparent)]
	OutcomeUnknown(RemoteError),
	/// A replay Apply came back without its result: the worker answered,
	/// so this is no lost answer. Only `apply_commits` returns it. It
	/// prints as the bare words the CLI always used.
	#[error("the replay returned no result")]
	NoReplayResult,
}

impl PasteError {
	/// A stale destination's reason, without the `stale destination in
	/// '<root>': ` prefix the worker's message starts with.
	pub fn stale_reason(&self) -> Option<String> {
		match self {
			Self::Local(TransferError::StaleDestination { reason, .. }) => {
				Some(reason.clone())
			}
			Self::Remote(RemoteError::Refused {
				code: ErrorCode::Stale,
				message,
			}) => Some(
				message
					.strip_prefix("stale destination in '")
					.and_then(|rest| rest.split_once("': "))
					.map_or(message.as_str(), |(_, reason)| reason)
					.to_string(),
			),
			_ => None,
		}
	}

	/// Another paste (another tab, process or worker) holds the
	/// repository's write lock.
	pub fn is_repo_busy(&self) -> bool {
		match self {
			Self::Local(TransferError::Git(GitError::WorktreeBusy {
				..
			})) => true,
			Self::Remote(RemoteError::Refused {
				code: ErrorCode::Busy,
				message,
			}) => message.contains(REPO_BUSY_TEXT),
			_ => false,
		}
	}

	/// Two entries reach one file.
	pub fn is_collision(&self) -> bool {
		matches!(
			self,
			Self::Local(TransferError::TargetCollision { .. })
				| Self::Remote(RemoteError::Refused {
					code: ErrorCode::Collision,
					..
				})
		)
	}

	/// The engine's own refusal, in the local paste's words. `None` when no
	/// engine answered: a folder that does not resolve, or a worker that
	/// could not be reached.
	pub fn refusal(&self) -> Option<String> {
		match self {
			Self::Local(err) => Some(err.paste_message()),
			Self::Remote(RemoteError::Refused { message, .. }) => {
				Some(message.clone())
			}
			_ => None,
		}
	}
}

/// A remote Apply's failure. A lost answer may hide a write.
pub(crate) fn apply_failure(err: RemoteError) -> PasteError {
	if err.outcome_unknown() {
		PasteError::OutcomeUnknown(err)
	} else {
		PasteError::Remote(err)
	}
}

impl From<TransferError> for PasteError {
	fn from(error: TransferError) -> Self {
		Self::Local(error)
	}
}

impl PasteTarget {
	/// The selected commit file against its virtual destination before
	/// replay. Only original destination bytes are read on the worker;
	/// preceding payload contents are resolved by the common core engine.
	#[allow(clippy::too_many_arguments)]
	pub fn preview_commit_file(
		&self,
		preview: &CommitReplayPreview,
		commit_index: usize,
		file_index: usize,
		side: ReplayFileSide,
		limit: usize,
		opts: &RunOptions,
	) -> Result<ReplayFilePreview, PasteError> {
		let limit = limit.min(snip_core::browser::PREVIEW_LIMIT);
		preview.file_preview_with(
			commit_index,
			file_index,
			side,
			limit,
			opts,
			|path, opts| match self {
				Self::Local { .. } => {
					snip_core::transfer::read_replay_preview_with(
						path, limit, opts,
					)
					.map_err(PasteError::Local)
				}
				Self::Remote {
					client, workspace, ..
				} => client
					.read_replay_preview(
						workspace,
						path,
						limit,
						opts.cancel.as_ref(),
					)
					.map_err(PasteError::Remote),
			},
		)
	}

	/// Plans pasting the file payload `text`. Nothing is written. `opts`
	/// carries the preview's cancel token and read limits; a worker uses
	/// only the cancel token.
	///
	/// `note` hears the restore-base suggestion, if there is one. A local
	/// target calls it before it resolves the destination. A remote target
	/// calls it once the worker has answered. Without `note`, and without
	/// `route.adjust_paths`, a local target does not look for a suggestion.
	pub fn plan_files(
		&self,
		text: &str,
		route: &PasteRoute,
		opts: &RunOptions,
		note: Option<&mut dyn FnMut(&RestoreBaseSuggestion)>,
	) -> Result<FilePastePlan, PasteError> {
		match self {
			PasteTarget::Local { dest } => {
				let suggestion = if note.is_some() || route.adjust_paths {
					let paths: Vec<String> =
						snip_core::format::parse_clipboard(
							text,
							&route.header_format,
						)
						.into_iter()
						.map(|e| e.path)
						.collect();
					suggest(dest, &paths, text)
				} else {
					None
				};
				if let (Some(note), Some(s)) = (note, suggestion.as_ref()) {
					note(s);
				}
				let destination = |path: &Path| {
					CanonicalRootId::new(path).map_err(|source| {
						PasteError::Destination {
							path: path.to_path_buf(),
							source,
						}
					})
				};
				let primary_id = destination(dest)?;
				let (mapping, roots) = import_mapping(
					dest.clone(),
					primary_id,
					suggestion.as_ref().filter(|_| route.adjust_paths),
					route.prefixes.iter().map(|(p, f)| {
						Ok::<_, PasteError>((p.clone(), f.clone()))
					}),
					destination,
				)?;
				let plan = plan_import_with(
					text,
					&route.header_format,
					&roots,
					&mapping,
					opts,
				)
				.map_err(PasteError::Local)?;
				Ok(FilePastePlan { plan, digest: None })
			}
			PasteTarget::Remote {
				client,
				workspace,
				dest,
			} => {
				let planned = client
					.import_plan(
						workspace,
						dest,
						text,
						&route.mapping(),
						opts.cancel.as_ref(),
					)
					.map_err(PasteError::Remote)?;
				if let (Some(note), Some(s)) =
					(note, planned.suggestion.as_ref())
				{
					note(s);
				}
				Ok(FilePastePlan {
					plan: planned.plan,
					digest: Some(planned.digest),
				})
			}
		}
	}

	/// Writes `plan` as `selection` confirms. `text` and `route` must be the
	/// ones it was planned with: a worker plans again from them. It is never
	/// cancelled: a confirmed write runs to the end.
	pub fn apply_files(
		&self,
		text: &str,
		route: &PasteRoute,
		plan: &FilePastePlan,
		selection: &RestoreSelection,
	) -> Result<RestoreExecutionResult, PasteError> {
		match self {
			PasteTarget::Local { .. } => {
				if plan.digest.is_some() {
					// A worker's plan names the worker's paths: never write them here.
					return Err(PasteError::Local(
						TransferError::StaleDestination {
							root: plan
								.roots()
								.first()
								.cloned()
								.unwrap_or_default(),
							reason: "the plan was made on another machine"
								.into(),
						},
					));
				}
				plan.plan.apply(selection).map_err(PasteError::Local)
			}
			PasteTarget::Remote {
				client,
				workspace,
				dest,
			} => {
				let expect = ImportExpect {
					digest: plan.digest.clone().unwrap_or_default(),
					freshness: plan.plan.destination_freshness().clone(),
				};
				let mut result = client
					.import_apply(
						workspace,
						dest,
						text,
						&route.mapping(),
						selection,
						&expect,
						None,
					)
					.map_err(apply_failure)?;
				// Older workers omit this additive result field. The worker
				// already checked the exact preview digest before writing, so
				// its admitted plan has this same number of planning refusals.
				// Assign, rather than add, to avoid counting a new worker twice.
				result.planned_skipped_count =
					plan.restore_plan().skipped_operations.len();
				Ok(result)
			}
		}
	}

	/// Re-reads a local file plan's destination. It does NOT check a remote
	/// target: that answers Ok without asking the worker, whose Apply
	/// re-checks on its own disk. So this is no pre-Apply gate; only
	/// `apply_files` is. (Its one caller is the app's test-only
	/// `check_stale_destination`.)
	pub fn check_files(&self, plan: &FilePastePlan) -> Result<(), PasteError> {
		match self {
			PasteTarget::Local { .. } => plan
				.plan
				.destination_freshness()
				.revalidate()
				.map_err(PasteError::Local),
			PasteTarget::Remote { .. } => Ok(()),
		}
	}

	/// Plans replaying a commit payload into the destination's repository,
	/// with writes held to the destination folder. Nothing is written.
	pub fn plan_commits(
		&self,
		text: &str,
		payload: CommitsPayload,
		opts: &RunOptions,
	) -> Result<CommitReplayPreview, PasteError> {
		match self {
			PasteTarget::Local { dest } => {
				CommitReplayPreview::capture_in(dest, dest, &payload, opts)
					.map_err(PasteError::Local)
			}
			PasteTarget::Remote {
				client,
				workspace,
				dest,
			} => client
				.replay_plan(
					workspace,
					dest,
					text,
					payload,
					opts.cancel.as_ref(),
				)
				.map_err(PasteError::Remote),
		}
	}

	/// Re-checks a commit preview without writing. Stale wins over the
	/// overwrite prompt (spec 4.3), so the gate calls this first.
	pub fn check_commits(
		&self,
		text: &str,
		preview: &CommitReplayPreview,
	) -> Result<(), PasteError> {
		match self {
			PasteTarget::Local { .. } => {
				preview.revalidate().map_err(PasteError::Local)
			}
			PasteTarget::Remote {
				client,
				workspace,
				dest,
			} => client
				.replay_apply(workspace, dest, text, preview, true, None)
				.map(|_| ())
				.map_err(PasteError::Remote),
		}
	}

	/// Replays `preview`. Never cancelled.
	pub fn apply_commits(
		&self,
		text: &str,
		preview: &CommitReplayPreview,
	) -> Result<ReplayResult, PasteError> {
		match self {
			PasteTarget::Local { .. } => {
				preview.apply().map_err(PasteError::Local)
			}
			PasteTarget::Remote {
				client,
				workspace,
				dest,
			} => client
				.replay_apply(workspace, dest, text, preview, false, None)
				.map_err(apply_failure)?
				.ok_or(PasteError::NoReplayResult),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::cell::RefCell;
	use std::fs;

	use snip_core::restore::RestoreBase;

	use super::*;

	fn id(dir: &Path) -> CanonicalRootId {
		CanonicalRootId::new(dir).unwrap()
	}

	#[test]
	fn import_mapping_puts_the_destination_first_and_each_folder_once() {
		let p = tempfile::tempdir().unwrap();
		let a = tempfile::tempdir().unwrap();
		let b = tempfile::tempdir().unwrap();
		let (p, a, b) = (p.path(), a.path(), b.path());
		let (mapping, roots) = import_mapping(
			p.to_path_buf(),
			id(p),
			None,
			[
				Ok(("x".to_string(), a.to_path_buf())),
				Ok(("y".into(), b.to_path_buf())),
				Ok(("z".into(), a.to_path_buf())),
			],
			|f: &Path| CanonicalRootId::new(f),
		)
		.unwrap();
		assert_eq!(
			roots,
			vec![p.to_path_buf(), a.to_path_buf(), b.to_path_buf()]
		);
		assert!(mapping.prefix_destinations.contains_key("x"));
		assert!(mapping.prefix_destinations.contains_key("y"));
		assert!(mapping.prefix_destinations.contains_key("z"));
		assert_eq!(mapping.primary_destination, Some(id(p)));
		assert!(mapping.primary_prefix.is_none());
	}

	#[test]
	fn import_mapping_follows_the_restore_base() {
		let p = tempfile::tempdir().unwrap();
		let p = p.path();
		let add = RestoreBaseSuggestion {
			base: RestoreBase::Add {
				prefix: "src".into(),
			},
			label: String::new(),
			matched: 1,
			total: 1,
		};
		let (mapping, _) = import_mapping(
			p.to_path_buf(),
			id(p),
			Some(&add),
			std::iter::empty::<Result<(String, PathBuf), std::io::Error>>(),
			|f: &Path| CanonicalRootId::new(f),
		)
		.unwrap();
		assert_eq!(mapping.primary_prefix, Some("src".into()));

		let strip = RestoreBaseSuggestion {
			base: RestoreBase::Strip {
				segment: "proj".into(),
			},
			label: String::new(),
			matched: 1,
			total: 1,
		};
		let (mapping, _) = import_mapping(
			p.to_path_buf(),
			id(p),
			Some(&strip),
			std::iter::empty::<Result<(String, PathBuf), std::io::Error>>(),
			|f: &Path| CanonicalRootId::new(f),
		)
		.unwrap();
		assert_eq!(mapping.prefix_destinations["proj"], id(p));
	}

	#[test]
	fn import_mapping_stops_at_the_first_failing_prefix() {
		let p = tempfile::tempdir().unwrap();
		let a = tempfile::tempdir().unwrap();
		let b = tempfile::tempdir().unwrap();
		let (p, a, b) = (p.path(), a.path(), b.path());
		let seen: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
		let result = import_mapping(
			p.to_path_buf(),
			id(p),
			None,
			[
				Ok(("x".to_string(), a.to_path_buf())),
				Err("boom"),
				Ok(("y".into(), b.to_path_buf())),
			],
			|f: &Path| -> Result<CanonicalRootId, &'static str> {
				seen.borrow_mut().push(f.to_path_buf());
				Ok(id(f))
			},
		);
		match result {
			Err(e) => assert_eq!(e, "boom"),
			Ok(_) => panic!("the failing prefix must stop the mapping"),
		}
		assert_eq!(*seen.borrow(), vec![a.to_path_buf()]);
	}

	#[test]
	fn an_apply_with_a_lost_answer_is_outcome_unknown_and_a_refusal_is_not() {
		let lost = [
			RemoteError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "x")),
			RemoteError::TimedOut,
			RemoteError::Protocol("x".into()),
			RemoteError::Cancelled,
		];
		for err in lost {
			assert!(matches!(
				apply_failure(err),
				PasteError::OutcomeUnknown(_)
			));
		}
		let kept = [
			RemoteError::Refused {
				code: ErrorCode::Stale,
				message: "x".into(),
			},
			RemoteError::Connect("x".into()),
			RemoteError::WorkerTooOld {
				worker: "w".into(),
				have: 1,
				need: 2,
			},
		];
		for err in kept {
			assert!(matches!(apply_failure(err), PasteError::Remote(_)));
		}
	}

	#[test]
	fn paste_errors_classify_stale_busy_and_collision_on_both_sides() {
		let local_stale = PasteError::Local(TransferError::StaleDestination {
			root: "/r".into(),
			reason: "target file 'a' was created externally after preview"
				.into(),
		});
		assert_eq!(
			local_stale.stale_reason().as_deref(),
			Some("target file 'a' was created externally after preview")
		);
		let remote_stale = PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::Stale,
			message: "stale destination in '/w': target file 'a' was created externally after preview".into(),
		});
		assert_eq!(
			remote_stale.stale_reason().as_deref(),
			Some("target file 'a' was created externally after preview")
		);
		let odd = PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::Stale,
			message: "odd".into(),
		});
		assert_eq!(odd.stale_reason().as_deref(), Some("odd"));

		// The busy message is rendered from the real error, as the worker
		// sends it; REPO_BUSY_TEXT must not be used to build it.
		let busy =
			TransferError::Git(GitError::WorktreeBusy { args: "x".into() })
				.paste_message();
		assert!(
			busy.contains(REPO_BUSY_TEXT),
			"core's WorktreeBusy text changed: {busy}"
		);
		assert!(PasteError::Local(TransferError::Git(
			GitError::WorktreeBusy { args: "x".into() }
		))
		.is_repo_busy());
		assert!(PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::Busy,
			message: busy,
		})
		.is_repo_busy());
		assert!(!PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::Busy,
			message: "too many callers".into(),
		})
		.is_repo_busy());

		assert!(PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::Collision,
			message: "c".into(),
		})
		.is_collision());
		assert!(PasteError::Local(TransferError::TargetCollision {
			path: "a".into(),
			msg: "m".into(),
		})
		.is_collision());

		assert!(PasteError::Local(TransferError::UnsafePath("x".into()))
			.refusal()
			.is_some());
		assert!(PasteError::Remote(RemoteError::Refused {
			code: ErrorCode::Stale,
			message: "m".into(),
		})
		.refusal()
		.is_some());
		assert!(PasteError::Remote(RemoteError::Connect("x".into()))
			.refusal()
			.is_none());
		assert!(PasteError::OutcomeUnknown(RemoteError::TimedOut)
			.refusal()
			.is_none());
		assert!(PasteError::Destination {
			path: "/p".into(),
			source: io::Error::new(io::ErrorKind::NotFound, "n"),
		}
		.refusal()
		.is_none());

		assert_eq!(
			PasteError::Local(TransferError::SpecialFile("p".into()))
				.to_string(),
			TransferError::SpecialFile("p".into()).paste_message()
		);
	}

	#[test]
	fn a_remote_plan_answers_delete_rows_from_its_snapshot() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path().join("root");
		fs::create_dir_all(&root).unwrap();
		fs::write(root.join("old.txt"), "old\n").unwrap();
		let plan = plan_import_with(
			"// file: [DELETED] old.txt\n// file: [NEW] new.txt\nnew\n",
			"",
			std::slice::from_ref(&root),
			&ImportMapping::with_primary(CanonicalRootId::new(&root).unwrap()),
			&RunOptions::default(),
		)
		.unwrap();
		assert_eq!(plan.delete_operations().len(), 1);
		let local = FilePastePlan {
			plan: plan.clone(),
			digest: None,
		};
		let remote = FilePastePlan {
			plan,
			digest: Some("d".into()),
		};
		assert!(local.delete_target_exists(0));
		assert!(remote.delete_target_exists(0));
		fs::remove_file(root.join("old.txt")).unwrap();
		// Locally a stat made now; a remote plan keeps the worker's snapshot.
		assert!(!local.delete_target_exists(0));
		assert!(remote.delete_target_exists(0));
		assert!(!remote.delete_target_exists(1));
		assert!(!local.delete_target_exists(1));
	}

	#[test]
	fn a_local_target_names_a_missing_destination() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("nope");
		let result = PasteTarget::Local { dest: dest.clone() }.plan_files(
			"// file: a.txt\nx\n",
			&PasteRoute::default(),
			&RunOptions::default(),
			None,
		);
		match result {
			Err(PasteError::Destination { path, .. }) => assert_eq!(path, dest),
			other => panic!("expected a missing destination, got {other:?}"),
		}
	}

	#[test]
	fn a_local_target_hears_the_suggestion_before_it_resolves() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("proj");
		let text = "// clipcode-root: src\n// file: proj/x/y.txt\nbody\n";
		let mut heard = Vec::new();
		let mut note = |s: &RestoreBaseSuggestion| heard.push(s.base.clone());
		let result = PasteTarget::Local { dest: dest.clone() }.plan_files(
			text,
			&PasteRoute::default(),
			&RunOptions::default(),
			Some(&mut note),
		);
		match result {
			Err(PasteError::Destination { path, .. }) => assert_eq!(path, dest),
			other => panic!("expected a missing destination, got {other:?}"),
		}
		drop(note);
		assert_eq!(
			heard,
			vec![RestoreBase::Strip {
				segment: "proj".into()
			}]
		);
	}
}
