//! Safe paste restore preview and confirmation workflow.
//!
//! Replaces dangerous synchronous unconditional overwrite on Ctrl+V with:
//! 1. Ctrl+V only parses payload and builds preview.
//! 2. Shows exact explicit destination and operation list (CREATE / OVERWRITE / DELETE).
//! 3. Overwrite selection is OFF by default.
//! 4. Preserves the exact `RestorePlan` and `delete_operations` without lossy re-planning.
//! 5. Blocks apply on any destination modification since preview (freshness/stale checks).
//! 6. A distinct Apply control performs the confirmed plan on a background thread.

use std::collections::BTreeSet;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use snip_core::commits::{
	self, FilePlan, LayoutConflict, NotCopiedReason, ReplayAction,
	ReplaySkipReason,
};
use snip_core::format;
use snip_core::gitrun::{CancelToken, RunOptions};
use snip_core::restore::{
	RestoreExecutionResult, RestorePlan, RestoreSelection,
};
use snip_core::transfer::{
	detect_clipboard_prefixes, plan_import_with, CanonicalRootId,
	CommitReplayPreview, ImportMapping, TransferError, TransferImportPlan,
};

use crate::i18n::{tf, Locale, Msg};
use crate::reader::{Preview, PreviewSource};
use crate::syntax::Language;

pub mod preview;

/// Shared retained preview tier: UI, one active paste input, one latest
/// captured request, and one result mailbox. Builder scratch is temporary.
/// The core copy cap must not exceed this budget, so it is the same number.
pub const MAX_RETAINED_PREVIEW_BYTES: usize =
	snip_core::transfer::CLIPBOARD_PAYLOAD_MAX;
// Arc's two counters plus conservative alignment padding for these types.
const ARC_ALLOWANCE: usize = 3 * size_of::<usize>();

pub fn preview_budget_error() -> Msg {
	Msg::new("preview_memory_limit", [])
}

fn check_budget(bytes: usize, limit: usize) -> Result<(), Msg> {
	if bytes > limit {
		Err(preview_budget_error())
	} else {
		Ok(())
	}
}

/// Checks the complete proposed UI state, before replacing any valid view.
pub fn admit_preview_state(
	ordinary: Option<&Preview>,
	plan: Option<&PastePreviewPlan>,
	detail: Option<&Preview>,
) -> Result<(), Msg> {
	admit_preview_state_with_limit(
		ordinary,
		plan,
		detail,
		MAX_RETAINED_PREVIEW_BYTES,
	)
}

fn preview_state_bytes(
	ordinary: Option<&Preview>,
	plan: Option<&PastePreviewPlan>,
	detail: Option<&Preview>,
) -> usize {
	ordinary
		.map_or(0, Preview::retained_bytes)
		.saturating_add(plan.map_or(0, PastePreviewPlan::retained_bytes))
		.saturating_add(detail.map_or(0, Preview::retained_bytes))
}

fn admit_preview_state_with_limit(
	ordinary: Option<&Preview>,
	plan: Option<&PastePreviewPlan>,
	detail: Option<&Preview>,
	limit: usize,
) -> Result<(), Msg> {
	check_budget(preview_state_bytes(ordinary, plan, detail), limit)
}

/// Gesture-owned input. Deferred remaps keep choices in the disarmed UI shell.
pub enum PasteRequest {
	Clipboard {
		text: String,
		dest: PathBuf,
		roots: Vec<PathBuf>,
		generation: u64,
	},
	Remap {
		prefix: String,
		keep: bool,
	},
}

impl PasteRequest {
	fn bytes(&self) -> usize {
		size_of::<Self>().saturating_add(match self {
			Self::Clipboard {
				text, dest, roots, ..
			} => roots.iter().fold(
				text.capacity()
					.saturating_add(dest.capacity())
					.saturating_add(roots.capacity() * size_of::<PathBuf>()),
				|n, p| n.saturating_add(p.capacity()),
			),
			Self::Remap { prefix, .. } => prefix.capacity(),
		})
	}
}

pub struct PasteOutcome {
	pub result: Result<PastePreviewPlan, Msg>,
	pub remap: Option<(String, bool)>,
}

impl PasteOutcome {
	fn bytes(&self) -> usize {
		match &self.result {
			Ok(plan) => plan.retained_bytes(),
			Err(err) => msg_heap_bytes(err),
		}
		.saturating_add(self.remap.as_ref().map_or(0, |(p, _)| p.capacity()))
	}
}

fn msg_heap_bytes(msg: &Msg) -> usize {
	msg.args
		.iter()
		.fold(msg.args.capacity() * size_of::<String>(), |n, s| {
			n.saturating_add(s.capacity())
		})
}

/// One paste-specific mailbox, not a general job/resource manager. The UI's
/// lifecycle id remains occupied independently until its FinishFlag drops.
#[derive(Default)]
pub struct PastePending {
	ui_bytes: usize,
	apply_allowance: usize,
	apply_active: usize,
	ui_raw: Option<Weak<str>>,
	seq: u64,
	input: Option<(u64, usize, Option<Weak<str>>)>,
	latest: Option<PasteRequest>,
	pub ready: Option<PasteOutcome>,
}

pub fn lock_pending(
	pool: &Arc<Mutex<PastePending>>,
) -> MutexGuard<'_, PastePending> {
	// An unwinding worker still owns a charge guard. Recover this simple
	// bookkeeping lock so its Drop can release that charge after its input.
	pool.lock()
		.unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PastePending {
	fn shared_raw(&self, ui: Option<&Weak<str>>) -> usize {
		let held = self
			.input
			.as_ref()
			.and_then(|(_, _, raw)| raw.as_ref())
			.and_then(Weak::upgrade)
			.or_else(|| {
				self.ready
					.as_ref()
					.and_then(|r| r.result.as_ref().ok())
					.map(|p| p.raw_payload.clone())
			});
		match (ui.and_then(Weak::upgrade), held) {
			(Some(ui), Some(held)) if Arc::ptr_eq(&ui, &held) => {
				held.len().saturating_add(ARC_ALLOWANCE)
			}
			_ => 0,
		}
	}

	fn total_with_ui(
		&self,
		ui_bytes: usize,
		allowance: usize,
		raw: Option<&Weak<str>>,
	) -> usize {
		let held = self
			.input
			.as_ref()
			.map_or(0, |(_, bytes, _)| *bytes)
			.saturating_add(self.ready.as_ref().map_or(0, PasteOutcome::bytes));
		// Subtract only a proven identical, live Arc, from its held charge.
		let held = held
			.checked_sub(self.shared_raw(raw))
			.expect("shared raw exceeds owned paste input/result");
		size_of::<Self>()
			.saturating_add(ARC_ALLOWANCE)
			.saturating_add(
				size_of::<PasteWorker>() + size_of::<PasteApplyWorker>(),
			)
			.saturating_add(ui_bytes)
			.saturating_add(held)
			.saturating_add(self.latest.as_ref().map_or(0, PasteRequest::bytes))
			.saturating_add(allowance.max(self.apply_active))
	}

	pub fn admit_ui(
		&mut self,
		ordinary: Option<&Preview>,
		plan: Option<&PastePreviewPlan>,
		detail: Option<&Preview>,
	) -> Result<(), Msg> {
		let bytes = preview_state_bytes(ordinary, plan, detail);
		let allowance = plan
			.filter(|p| p.executable())
			.map_or(0, PastePreviewPlan::apply_clone_bytes);
		let raw = plan.map(|p| Arc::downgrade(&p.raw_payload));
		check_budget(
			self.total_with_ui(bytes, allowance, raw.as_ref()),
			MAX_RETAINED_PREVIEW_BYTES,
		)?;
		self.ui_bytes = bytes;
		self.apply_allowance = allowance;
		self.ui_raw = raw;
		Ok(())
	}

	pub fn invalidate(&mut self, seq: u64) {
		self.seq = seq;
		self.latest = None;
		self.ready = None;
	}

	pub fn enqueue(&mut self, request: PasteRequest) -> Result<(), Msg> {
		self.latest = Some(request);
		if check_budget(
			self.total_with_ui(
				self.ui_bytes,
				self.apply_allowance,
				self.ui_raw.as_ref(),
			),
			MAX_RETAINED_PREVIEW_BYTES,
		)
		.is_err()
		{
			self.latest = None;
			return Err(preview_budget_error());
		}
		Ok(())
	}

	pub fn has_pending(&self) -> bool {
		self.latest.is_some() || self.ready.is_some()
	}

	pub fn start(
		pool: &Arc<Mutex<Self>>,
		plan: Option<&PastePreviewPlan>,
		cancel: CancelToken,
	) -> Result<Option<PasteWorker>, Msg> {
		let mut state = lock_pending(pool);
		assert!(state.input.is_none() && state.ready.is_none());
		let Some(request) = state.latest.take() else {
			return Ok(None);
		};
		let input = match request {
			PasteRequest::Clipboard { .. } => PasteInput::Clipboard(request),
			PasteRequest::Remap { prefix, keep } => {
				let Some(plan) = plan else { return Ok(None) };
				PasteInput::Remap {
					plan: plan.clone(),
					prefix,
					keep,
				}
			}
		};
		let raw = match &input {
			PasteInput::Remap { plan, .. } => {
				Some(Arc::downgrade(&plan.raw_payload))
			}
			_ => None,
		};
		let seq = state.seq;
		state.input = Some((seq, input.bytes(), raw));
		if check_budget(
			state.total_with_ui(
				state.ui_bytes,
				state.apply_allowance,
				state.ui_raw.as_ref(),
			),
			MAX_RETAINED_PREVIEW_BYTES,
		)
		.is_err()
		{
			state.input = None;
			return Err(preview_budget_error());
		}
		Ok(Some(PasteWorker {
			input: Some(input),
			pool: pool.clone(),
			cancel,
			seq,
		}))
	}
}

enum PasteInput {
	Clipboard(PasteRequest),
	Remap {
		plan: PastePreviewPlan,
		prefix: String,
		keep: bool,
	},
}
impl PasteInput {
	fn bytes(&self) -> usize {
		size_of::<Self>().saturating_add(match self {
			Self::Clipboard(request) => request.bytes(),
			Self::Remap { plan, prefix, .. } => {
				plan.retained_bytes().saturating_add(prefix.capacity())
			}
		})
	}
}

pub struct PasteWorker {
	input: Option<PasteInput>,
	pool: Arc<Mutex<PastePending>>,
	cancel: CancelToken,
	seq: u64,
}
impl PasteWorker {
	pub fn run(mut self, opts: &RunOptions) {
		// Locals unwind before self's charge guard. Clipboard buffers drop at
		// the end of this match; remap allocations move into the candidate.
		let mut outcome = match self.input.take().expect("paste input") {
			PasteInput::Clipboard(PasteRequest::Clipboard {
				text,
				dest,
				roots,
				generation,
			}) => PasteOutcome {
				result: PastePreviewPlan::build_from_clipboard_text_with(
					&text, &dest, &roots, generation, opts,
				),
				remap: None,
			},
			PasteInput::Remap {
				mut plan,
				prefix,
				keep,
			} => {
				let result = plan.rebuild_file_plan_with(opts).map(|()| plan);
				PasteOutcome {
					result,
					remap: Some((prefix, keep)),
				}
			}
			_ => unreachable!(
				"deferred remap must capture its shell before spawn"
			),
		};
		let mut state = lock_pending(&self.pool);
		state.input = None;
		if self.cancel.is_cancelled() || state.seq != self.seq {
			drop(outcome);
			return;
		}
		state.ready = Some(outcome);
		if state.total_with_ui(
			state.ui_bytes,
			state.apply_allowance,
			state.ui_raw.as_ref(),
		) > MAX_RETAINED_PREVIEW_BYTES
		{
			outcome = state.ready.take().unwrap();
			drop(outcome);
			state.ready = Some(PasteOutcome {
				result: Err(preview_budget_error()),
				remap: None,
			});
		}
	}
}
impl Drop for PasteWorker {
	fn drop(&mut self) {
		// Input must go first, including an unpolled/dropped future.
		drop(self.input.take());
		let mut state = lock_pending(&self.pool);
		if state
			.input
			.as_ref()
			.is_some_and(|(seq, _, _)| *seq == self.seq)
		{
			state.input = None;
		}
	}
}

pub struct PasteApplyWorker {
	plan: Option<PastePreviewPlan>,
	pool: Arc<Mutex<PastePending>>,
}
impl PasteApplyWorker {
	pub fn new(
		plan: &PastePreviewPlan,
		pool: &Arc<Mutex<PastePending>>,
	) -> Self {
		let clone = plan.clone();
		let mut state = lock_pending(pool);
		let actual = clone.apply_clone_bytes();
		assert!(
			actual <= state.apply_allowance,
			"Apply clone exceeds admitted metadata allowance"
		);
		assert_eq!(state.apply_active, 0);
		state.apply_active = actual;
		Self {
			plan: Some(clone),
			pool: pool.clone(),
		}
	}
	pub fn execute(self) -> Result<PasteApplyResult, Msg> {
		self.plan.as_ref().unwrap().execute()
	}
}
impl Drop for PasteApplyWorker {
	fn drop(&mut self) {
		drop(self.plan.take());
		lock_pending(&self.pool).apply_active = 0;
	}
}

/// What the plan does to one row (spec 3.2: 新增 / 覆寫 / 刪除 / 跳過).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlannedOp {
	Create,
	Overwrite,
	Delete,
	Skip(SkipCause),
	CommitRefused(LayoutConflict),
}

/// Why a commit file is listed but never written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipCause {
	Binary,
	NonUtf8,
	NonUtf8Path,
	UnsupportedType,
	Unreadable,
	UnsafePath,
	NonUtf8Target,
	Other,
}

/// What Apply would do to a row under the user's current choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAction {
	/// Unticked. A commit replay refuses the whole Apply instead of dropping it.
	Excluded {
		by_commit: bool,
	},
	/// Commit replay only: listed, never written.
	Skip(SkipCause),
	Delete,
	/// Delete of a destination file that is already gone.
	DeleteMissing,
	Create,
	Overwrite,
	/// Commit replay: an existing file whose overwrite is not allowed yet; blocks Apply.
	OverwritePending,
	/// File restore: an existing file kept because overwrite is off.
	KeepExisting,
	/// Commit replay: layout conflict refuses the whole commit.
	CommitRefused(LayoutConflict),
}

impl RowAction {
	/// Not written by Apply: a commit header's "skipped" count.
	pub fn is_not_written(self) -> bool {
		match self {
			Self::Skip(_) | Self::DeleteMissing | Self::KeepExisting => true,
			// Per spec 4.3, layout conflicts refuse the whole commit instead of
			// skipping individual files, so CommitRefused is not counted as a skip.
			Self::CommitRefused(_)
			| Self::Excluded { .. }
			| Self::Delete
			| Self::Create
			| Self::Overwrite
			| Self::OverwritePending => false,
		}
	}
}

#[derive(Debug, Clone)]
pub struct PasteItem {
	pub path: String,
	pub dest_root: PathBuf,
	pub dest_root_name: String,
	pub dest_path: PathBuf,
	pub dest_exists: bool,
	pub op: PlannedOp,
	pub overwrite_allowed: bool, // CRITICAL: OFF by default!
	pub selected: bool,          // CRITICAL: whether this item is opted-in
	/// Shared with the plan's detail view; never copied per render.
	pub content: Arc<str>,
	pub bytes: usize,
	pub lines: usize,
	pub op_index: usize,
	/// Commit replay only: the commit (index into the payload) this row
	/// belongs to. `None` for file restores.
	pub commit: Option<usize>,
	/// Commit replay only: the other half of a rename ("→ new" on the
	/// deleted old path, "← old" on the written new path).
	pub rename_note: Option<String>,
}

impl PasteItem {
	pub fn is_delete(&self) -> bool {
		matches!(self.op, PlannedOp::Delete)
	}

	pub fn writes_content(&self) -> bool {
		matches!(self.op, PlannedOp::Create | PlannedOp::Overwrite)
	}

	/// An existing destination file that the user may choose to overwrite.
	/// A skipped row is never written, so it never asks.
	pub fn overwritable(&self) -> bool {
		self.dest_exists
			&& matches!(self.op, PlannedOp::Create | PlannedOp::Overwrite)
	}

	pub fn action(&self) -> RowAction {
		if !self.selected {
			return RowAction::Excluded {
				by_commit: self.commit.is_some(),
			};
		}
		match self.op {
			PlannedOp::CommitRefused(c) => RowAction::CommitRefused(c),
			PlannedOp::Skip(c) => RowAction::Skip(c),
			PlannedOp::Delete => {
				if self.dest_exists {
					RowAction::Delete
				} else {
					RowAction::DeleteMissing
				}
			}
			PlannedOp::Create | PlannedOp::Overwrite => {
				if !self.dest_exists {
					RowAction::Create
				} else if self.overwrite_allowed {
					RowAction::Overwrite
				} else if self.commit.is_some() {
					RowAction::OverwritePending
				} else {
					RowAction::KeepExisting
				}
			}
		}
	}

	pub fn shows_delete_notice(&self) -> bool {
		matches!(self.action(), RowAction::Delete)
	}

	fn retained_heap_bytes(&self) -> usize {
		self.path
			.capacity()
			.saturating_add(
				self.rename_note.as_ref().map_or(0, String::capacity),
			)
			.saturating_add(self.dest_root.capacity())
			.saturating_add(self.dest_root_name.capacity())
			.saturating_add(self.dest_path.capacity())
			.saturating_add(self.content.len())
			.saturating_add(ARC_ALLOWANCE)
	}
}

/// One row of the paste change tree (built per frame, never retained).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteNode {
	/// A destination repository and how many items land in it.
	Root(String, usize),
	/// A directory under that root, as one compressed path ("src/app").
	Dir(String, usize),
	/// An item (index into `items`) at this tree depth.
	File(usize, usize),
	/// A commit header (index into the payload's commits).
	Commit(usize),
}

/// "a/b/c.txt" as ("a/b", "c.txt"); a top-level file has an empty dir.
pub fn split_dir(path: &str) -> (&str, &str) {
	path.rsplit_once('/').unwrap_or(("", path))
}

/// One source-prefix row. Nothing is chosen until the user picks a path.
#[derive(Debug, Clone)]
pub struct PrefixChoice {
	pub prefix: String,
	pub destination: Option<PathBuf>,
	/// Explicit choice to keep `prefix/` as a directory under the primary root.
	pub keep_relative: bool,
	/// Canonical directories the user may pick. Full paths, not basenames.
	pub candidates: Vec<PathBuf>,
}

impl PrefixChoice {
	fn retained_heap_bytes(&self) -> usize {
		self.candidates.iter().fold(
			self.prefix
				.capacity()
				.saturating_add(
					self.destination.as_ref().map_or(0, PathBuf::capacity),
				)
				.saturating_add(
					self.candidates.capacity() * size_of::<PathBuf>(),
				),
			|bytes, path| bytes.saturating_add(path.capacity()),
		)
	}
}

/// Result of a confirmed apply. `created_commits` is empty for file restores.
#[derive(Debug, Clone)]
pub struct PasteApplyResult {
	pub files: RestoreExecutionResult,
	pub created_commits: Vec<String>,
}

impl PasteApplyResult {
	/// The result card after an apply. Per-file failures are listed, not
	/// just counted: the writes before them are kept, so the user must see
	/// which paths did not land.
	pub fn status(&self) -> Msg {
		if !self.created_commits.is_empty() {
			return Msg::new(
				"commit_replay_done",
				[self.created_commits.join(", ")],
			);
		}
		let f = &self.files;
		let counts = [
			f.created_count,
			f.overwritten_count,
			f.skipped_existing_count,
			f.deleted_count,
			f.errors.len(),
		]
		.map(|n| n.to_string());
		if f.errors.is_empty() {
			return Msg::new("status_paste_done", counts);
		}
		let mut args = counts.to_vec();
		args.push(f.errors.join("; "));
		Msg::new("status_paste_partial", args)
	}
}

#[derive(Debug, Clone)]
pub struct PastePreviewPlan {
	pub destination: PathBuf,
	/// Shared import plan containing restore plan and destination freshness.
	pub import_plan: Option<Arc<TransferImportPlan>>,
	/// Whole-commit replay. File checkboxes do not rewrite a subset.
	pub commit_preview: Option<Arc<CommitReplayPreview>>,
	pub whole_commit: bool,
	/// Commits whose file rows are folded away (commit replay only).
	pub collapsed_commits: BTreeSet<usize>,
	pub prefix_choices: Vec<PrefixChoice>,
	/// Shared so starting an apply does not copy every file body.
	pub plan: Arc<RestorePlan>,
	pub items: Vec<PasteItem>,
	pub selected_item_idx: usize,
	pub is_applying: bool,
	pub generation_snapshot: u64,
	pub error: Option<Msg>,
	/// Shared so a clone for a background rebuild or an apply is cheap.
	raw_payload: Arc<str>,
}

fn empty_plan() -> Arc<RestorePlan> {
	Arc::new(RestorePlan {
		roots: Vec::new(),
		create_operations: Vec::new(),
		delete_operations: Vec::new(),
		skipped_operations: Vec::new(),
	})
}

fn canonical_dirs(paths: &[PathBuf]) -> Vec<PathBuf> {
	let mut out = Vec::new();
	for path in paths {
		if let Ok(id) = CanonicalRootId::new(path) {
			if id.path().is_dir() && !out.iter().any(|e| e == id.path()) {
				out.push(id.path().to_path_buf());
			}
		}
	}
	out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowControl {
	Row,
	Include,
	Overwrite,
}

impl RowControl {
	/// Element and probe id of a paste row control: `paste-<kind>:<ix>:<path>`.
	/// `ix` is the item's index in the plan, which stays unique when several
	/// items share one relative path (the same file under two destination roots,
	/// or modified by two commits); GPUI merges the state of equal ids, so a
	/// path-only id left the second row's controls dead. The path stays last so
	/// drivers can recover it by splitting off `paste-<kind>:<ix>:`.
	pub fn id(self, ix: usize, path: &str) -> String {
		let kind = match self {
			Self::Row => "row",
			Self::Include => "include",
			Self::Overwrite => "overwrite",
		};
		format!("paste-{kind}:{ix}:{path}")
	}
}

/// Element and probe id of a commit header.
pub fn commit_header_id(c: usize) -> String {
	format!("paste-commit:{c}")
}

/// The cause explaining why a commit file is not written.
fn skip_cause(file: &FilePlan) -> SkipCause {
	match (file.not_copied, file.skip_reason) {
		(Some(NotCopiedReason::Binary), _) => SkipCause::Binary,
		(Some(NotCopiedReason::NonUtf8), _) => SkipCause::NonUtf8,
		(Some(NotCopiedReason::NonUtf8Path), _) => SkipCause::NonUtf8Path,
		(Some(NotCopiedReason::UnsupportedType), _) => {
			SkipCause::UnsupportedType
		}
		(Some(NotCopiedReason::Unreadable), _) => SkipCause::Unreadable,
		(None, Some(ReplaySkipReason::UnsafePath)) => SkipCause::UnsafePath,
		(None, Some(ReplaySkipReason::NonUtf8Target)) => {
			SkipCause::NonUtf8Target
		}
		_ => SkipCause::Other,
	}
}

pub(crate) fn layout_conflict_key(conflict: LayoutConflict) -> &'static str {
	match conflict {
		LayoutConflict::RenamedFromIsDirectory => {
			"reason_refused_renamed_from_dir"
		}
		LayoutConflict::DeleteTargetIsDirectory => "reason_refused_delete_dir",
		LayoutConflict::DirectoryInTheWay => "reason_refused_dir_in_way",
		LayoutConflict::FileInTheWayOfParent => "reason_refused_file_in_way",
	}
}

pub(crate) fn layout_conflict_cause_key(
	conflict: LayoutConflict,
) -> &'static str {
	match conflict {
		LayoutConflict::RenamedFromIsDirectory => {
			"reason_refusal_cause_renamed_from_dir"
		}
		LayoutConflict::DeleteTargetIsDirectory => {
			"reason_refusal_cause_delete_dir"
		}
		LayoutConflict::DirectoryInTheWay => "reason_refusal_cause_dir_in_way",
		LayoutConflict::FileInTheWayOfParent => {
			"reason_refusal_cause_file_in_way"
		}
	}
}

fn header_label(op: PlannedOp) -> &'static str {
	match op {
		PlannedOp::Create => "CREATE",
		PlannedOp::Overwrite => "OVERWRITE",
		PlannedOp::Delete => "DELETE",
		PlannedOp::Skip(_) => "SKIP",
		PlannedOp::CommitRefused(_) => "REFUSED",
	}
}

fn root_name(path: &Path) -> String {
	path.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_else(|| path.display().to_string())
}

/// A destination that could not be opened or captured, named by path so the
/// status says which directory was refused (the OS error alone does not).
fn destination_error(dest: &Path, err: &dyn std::fmt::Display) -> Msg {
	Msg::new(
		"paste_err_destination",
		[dest.display().to_string(), err.to_string()],
	)
}

fn stale_msg(err: TransferError) -> Msg {
	match err {
		TransferError::StaleDestination { reason, .. } => {
			if reason.contains("created") {
				Msg::new("stale_created", [reason])
			} else if reason.contains("deleted") {
				Msg::new("stale_deleted", [reason])
			} else {
				Msg::new("stale_modified", [reason])
			}
		}
		other => Msg::new("stale_modified", [other.to_string()]),
	}
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PasteCounts {
	pub creates: usize,
	pub overwrites: usize,
	pub existing: usize,
	pub deletes: usize,
	pub skips: usize,
}

impl PastePreviewPlan {
	pub fn detail_preview(&self) -> Option<Preview> {
		self.detail_at(self.selected_item_idx)
	}

	pub(crate) fn detail_at(&self, idx: usize) -> Option<Preview> {
		self.items.get(idx).filter(|i| !i.is_delete()).map(|i| {
			Preview::new(
				PreviewSource::PasteItem,
				Some(i.path.clone()),
				i.content.to_string(),
				false,
				Language::from_path_or_ext(&i.path, false),
			)
		})
	}

	/// Conservative retained UI bytes. Arc referents are charged in full,
	/// including the independent RestorePlan clone, without pointer dedup.
	pub fn retained_bytes(&self) -> usize {
		let mut bytes = size_of::<Self>()
			.saturating_add(self.destination.capacity())
			.saturating_add(self.raw_payload.len())
			.saturating_add(ARC_ALLOWANCE)
			.saturating_add(
				self.prefix_choices.capacity() * size_of::<PrefixChoice>(),
			)
			.saturating_add(self.items.capacity() * size_of::<PasteItem>())
			.saturating_add(size_of::<RestorePlan>() + ARC_ALLOWANCE)
			.saturating_add(self.plan.retained_heap_bytes());
		for choice in &self.prefix_choices {
			bytes = bytes.saturating_add(choice.retained_heap_bytes());
		}
		for item in &self.items {
			bytes = bytes.saturating_add(item.retained_heap_bytes());
		}
		if let Some(plan) = &self.import_plan {
			bytes = bytes
				.saturating_add(size_of::<TransferImportPlan>() + ARC_ALLOWANCE)
				.saturating_add(plan.retained_heap_bytes());
		}
		if let Some(plan) = &self.commit_preview {
			bytes = bytes
				.saturating_add(
					size_of::<CommitReplayPreview>() + ARC_ALLOWANCE,
				)
				.saturating_add(plan.retained_heap_bytes());
		}
		if let Some(error) = &self.error {
			bytes = bytes
				.saturating_add(error.args.capacity() * size_of::<String>());
			for arg in &error.args {
				bytes = bytes.saturating_add(arg.capacity());
			}
		}
		bytes
	}

	fn apply_clone_bytes(&self) -> usize {
		let mut bytes = size_of::<Self>()
			.saturating_add(self.destination.capacity())
			.saturating_add(
				self.prefix_choices.capacity() * size_of::<PrefixChoice>(),
			)
			.saturating_add(self.items.capacity() * size_of::<PasteItem>());
		for choice in &self.prefix_choices {
			bytes = bytes.saturating_add(choice.retained_heap_bytes());
		}
		for item in &self.items {
			bytes = bytes
				.saturating_add(item.path.capacity())
				.saturating_add(
					item.rename_note.as_ref().map_or(0, String::capacity),
				)
				.saturating_add(item.dest_root.capacity())
				.saturating_add(item.dest_root_name.capacity())
				.saturating_add(item.dest_path.capacity());
		}
		bytes.saturating_add(self.error.as_ref().map_or(0, msg_heap_bytes))
	}

	/// A complete mapping shell alone does not authorize any write.
	pub fn executable(&self) -> bool {
		self.mapping_ready()
			&& if self.whole_commit {
				self.commit_preview.is_some()
			} else {
				self.import_plan.is_some()
			}
	}

	pub fn mapping_ready(&self) -> bool {
		self.prefix_choices
			.iter()
			.all(|c| c.keep_relative || c.destination.is_some())
	}

	pub fn build_from_clipboard_text(
		raw_text: &str,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: u64,
	) -> Result<Self, Msg> {
		Self::build_from_clipboard_text_with(
			raw_text,
			dest,
			known_roots,
			generation,
			&RunOptions::default(),
		)
	}

	/// [`Self::build_from_clipboard_text`] with the caller's runner options,
	/// so a read-only preview can be cancelled. Nothing is written.
	pub fn build_from_clipboard_text_with(
		raw_text: &str,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: u64,
		opts: &RunOptions,
	) -> Result<Self, Msg> {
		Self::build_with_limit(
			raw_text,
			dest,
			known_roots,
			generation,
			opts,
			MAX_RETAINED_PREVIEW_BYTES,
		)
	}

	fn build_with_limit(
		raw_text: &str,
		dest: &Path,
		known_roots: &[PathBuf],
		generation: u64,
		opts: &RunOptions,
		limit: usize,
	) -> Result<Self, Msg> {
		// arboard has already allocated the OS clipboard String. Refuse before
		// parsing it or retaining any extra copy; never truncate a payload.
		check_budget(raw_text.len(), limit)?;
		if commits::is_commit_payload(raw_text) {
			return Self::build_commit(raw_text, dest, generation, opts, limit);
		}
		if format::parse_clipboard(raw_text, "").is_empty() {
			return Err(Msg::new("paste_err_not_payload", []));
		}
		let mut roots = vec![dest.to_path_buf()];
		for root in known_roots {
			if !roots.iter().any(|have| have == root) {
				roots.push(root.clone());
			}
		}
		let candidates = canonical_dirs(&roots);
		// A `clipcode-root` export is one root: nested directories stay under
		// the restore directory. Without that marker every first segment is
		// ambiguous (repo prefix or a directory) and needs an explicit choice.
		let prefixes = if format::extract_source_root(raw_text).is_some() {
			Vec::new()
		} else {
			detect_clipboard_prefixes(raw_text, "")
		};
		let mut plan = Self {
			destination: dest.to_path_buf(),
			import_plan: None,
			commit_preview: None,
			whole_commit: false,
			collapsed_commits: BTreeSet::new(),
			prefix_choices: Vec::new(),
			plan: empty_plan(),
			items: Vec::new(),
			selected_item_idx: 0,
			is_applying: false,
			generation_snapshot: generation,
			error: None,
			raw_payload: raw_text.into(),
		};
		let base = plan.retained_bytes();
		let mut choices_heap = 0usize;
		for prefix in prefixes {
			let choice = PrefixChoice {
				prefix,
				destination: None,
				keep_relative: false,
				candidates: candidates.clone(),
			};
			choices_heap =
				choices_heap.saturating_add(choice.retained_heap_bytes());
			plan.prefix_choices.push(choice);
			check_budget(
				base.saturating_add(choices_heap).saturating_add(
					plan.prefix_choices.capacity() * size_of::<PrefixChoice>(),
				),
				limit,
			)?;
		}
		plan.rebuild_with_limit(opts, limit)?;
		if plan.prefix_choices.is_empty() && plan.import_plan.is_none() {
			return Err(plan
				.error
				.unwrap_or_else(|| Msg::new("paste_err_nothing", [])));
		}
		Ok(plan)
	}

	fn build_commit(
		raw_text: &str,
		dest: &Path,
		generation: u64,
		opts: &RunOptions,
		limit: usize,
	) -> Result<Self, Msg> {
		let payload = commits::parse_commit_payload(raw_text)
			.map_err(|e| Msg::new("paste_err_plan", [e.to_string()]))?;
		let preview = CommitReplayPreview::capture_with(dest, &payload, opts)
			.map_err(|e| destination_error(dest, &e))?;
		let preview = Arc::new(preview);
		let mut plan = Self {
			destination: preview.destination().to_path_buf(),
			import_plan: None,
			commit_preview: Some(preview.clone()),
			whole_commit: true,
			collapsed_commits: BTreeSet::new(),
			prefix_choices: Vec::new(),
			plan: empty_plan(),
			items: Vec::new(),
			selected_item_idx: 0,
			is_applying: false,
			generation_snapshot: generation,
			error: None,
			raw_payload: raw_text.into(),
		};
		let base = plan.retained_bytes();
		check_budget(base, limit)?;
		let dest_name = root_name(dest);
		let mut items_heap = 0usize;
		let mut items = Vec::new();
		for (c_idx, (commit, record)) in preview
			.plan()
			.commits
			.iter()
			.zip(&preview.payload().commits)
			.enumerate()
		{
			for (f_idx, (file, src)) in
				commit.files.iter().zip(&record.files).enumerate()
			{
				let op = if let Some(conflict) = file.layout_conflict {
					PlannedOp::CommitRefused(conflict)
				} else {
					match file.action {
						ReplayAction::Delete => PlannedOp::Delete,
						ReplayAction::Write if file.existed => {
							PlannedOp::Overwrite
						}
						ReplayAction::Write => PlannedOp::Create,
						ReplayAction::Skip => PlannedOp::Skip(skip_cause(file)),
					}
				};
				let body = src.content.as_deref().unwrap_or_default();
				let head = |op: PlannedOp| {
					let label = header_label(op);
					format!(
						"commit: {}\nauthor: {} <{}>\ndate: {}\naction: {label}\n",
						commit.message.trim_end(),
						commit.author_name,
						commit.author_email,
						commit.author_date,
					)
				};
				let dest_path = file
					.absolute_path
					.clone()
					.unwrap_or_else(|| dest.join(&file.path));
				let row = |path: &str,
				           dest_path: PathBuf,
				           existed: bool,
				           op: PlannedOp,
				           text: String,
				           body_len: usize,
				           lines: usize,
				           rename_note: Option<String>| {
					PasteItem {
						path: path.to_string(),
						dest_root: dest.to_path_buf(),
						dest_root_name: dest_name.clone(),
						dest_path,
						dest_exists: existed,
						op,
						overwrite_allowed: false,
						selected: true,
						content: Arc::from(text),
						bytes: body_len,
						lines,
						op_index: c_idx * 1000 + f_idx,
						commit: Some(c_idx),
						rename_note,
					}
				};
				let mut rows = Vec::with_capacity(2);
				// A rename is a deletion of the old path plus a write of
				// the new one; the preview shows both.
				if let (ReplayAction::Write, Some(old)) =
					(file.action, file.old_path.as_deref())
				{
					let old_abs = file
						.old_absolute_path
						.clone()
						.unwrap_or_else(|| dest.join(old));
					rows.push(row(
						old,
						old_abs.clone(),
						file.old_existed,
						PlannedOp::Delete,
						format!(
							"{}path: {old}\nrenamed to: {}\n",
							head(PlannedOp::Delete),
							file.path
						),
						0,
						0,
						Some(format!("→ {}", file.path)),
					));
				}
				rows.push(row(
					&file.path,
					dest_path,
					file.existed,
					op,
					format!("{}path: {}\n\n{body}", head(op), file.path),
					body.len(),
					body.lines().count(),
					file.old_path
						.as_deref()
						.filter(|_| file.action == ReplayAction::Write)
						.map(|old| format!("← {old}")),
				));
				for item in rows {
					items_heap =
						items_heap.saturating_add(item.retained_heap_bytes());
					items.push(item);
					check_budget(
						base.saturating_add(items_heap).saturating_add(
							items.capacity() * size_of::<PasteItem>(),
						),
						limit,
					)?;
				}
			}
		}
		plan.items = items;
		plan.error = plan.refusal_banner();
		check_budget(plan.retained_bytes(), limit)?;
		Ok(plan)
	}

	/// Points `prefix` at a candidate directory and rebuilds the file plan.
	/// Same basenames stay distinct because the stored path is canonical.
	pub fn set_prefix_destination(
		&mut self,
		prefix: &str,
		dest: &Path,
	) -> Result<(), Msg> {
		self.choose_prefix_destination(prefix, dest)?;
		self.rebuild_file_plan_with(&RunOptions::default())
	}

	/// Records the choice only. The file plan is stale until
	/// [`Self::rebuild_file_plan_with`] has run.
	pub fn choose_prefix_destination(
		&mut self,
		prefix: &str,
		dest: &Path,
	) -> Result<(), Msg> {
		let id = CanonicalRootId::new(dest)
			.map_err(|e| destination_error(dest, &e))?;
		if !id.path().is_dir() {
			return Err(Msg::new(
				"paste_err_destination_not_dir",
				[id.path().display().to_string()],
			));
		}
		let choice = self
			.prefix_choices
			.iter_mut()
			.find(|c| c.prefix == prefix)
			.ok_or_else(|| {
				Msg::new("mapping_unknown_prefix", [prefix.to_string()])
			})?;
		if !choice.candidates.iter().any(|c| c == id.path()) {
			return Err(Msg::new(
				"mapping_unknown_dest",
				[id.path().display().to_string()],
			));
		}
		choice.keep_relative = false;
		choice.destination = Some(id.path().to_path_buf());
		Ok(())
	}

	/// Confirms that `prefix` is a directory under the primary destination.
	pub fn set_keep_relative(&mut self, prefix: &str) -> Result<(), Msg> {
		self.choose_keep_relative(prefix)?;
		self.rebuild_file_plan_with(&RunOptions::default())
	}

	/// Records the choice only, like [`Self::choose_prefix_destination`].
	pub fn choose_keep_relative(&mut self, prefix: &str) -> Result<(), Msg> {
		let choice = self
			.prefix_choices
			.iter_mut()
			.find(|c| c.prefix == prefix)
			.ok_or_else(|| {
				Msg::new("mapping_unknown_prefix", [prefix.to_string()])
			})?;
		choice.keep_relative = true;
		choice.destination = None;
		Ok(())
	}

	/// Drops the planned writes so a changed mapping cannot apply the old
	/// ones. A whole-commit replay has no mapping and keeps its plan.
	pub fn clear_file_plan(&mut self) {
		if self.whole_commit {
			return;
		}
		self.items = Vec::new();
		self.selected_item_idx = 0;
		self.import_plan = None;
		self.plan = empty_plan();
	}

	/// Plans the writes for the current mapping. Read-only; `opts` carries
	/// the caller's cancel token into the destination reads.
	pub fn rebuild_file_plan_with(
		&mut self,
		opts: &RunOptions,
	) -> Result<(), Msg> {
		self.rebuild_with_limit(opts, MAX_RETAINED_PREVIEW_BYTES)
	}

	fn rebuild_with_limit(
		&mut self,
		opts: &RunOptions,
		limit: usize,
	) -> Result<(), Msg> {
		if self.whole_commit {
			return Ok(());
		}
		self.clear_file_plan();
		self.error = None;
		let result = self
			.rebuild_inner(opts, limit)
			.and_then(|()| check_budget(self.retained_bytes(), limit));
		if let Err(err) = &result {
			// Even a caller that keeps this mapping shell cannot execute any
			// previously admitted writes after the rebuild was refused.
			self.clear_file_plan();
			self.error = Some(err.clone());
		}
		result
	}

	fn rebuild_inner(
		&mut self,
		opts: &RunOptions,
		limit: usize,
	) -> Result<(), Msg> {
		check_budget(self.retained_bytes(), limit)?;
		if !self.mapping_ready() {
			self.error = Some(Msg::new("mapping_required", []));
			return Ok(());
		}
		let primary = CanonicalRootId::new(&self.destination)
			.map_err(|e| destination_error(&self.destination, &e))?;
		let mut dest_roots = vec![self.destination.clone()];
		let mut mapping = ImportMapping::with_primary(primary);
		for choice in &self.prefix_choices {
			if choice.keep_relative {
				continue;
			}
			let Some(dest) = choice.destination.clone() else {
				continue;
			};
			if !dest_roots.iter().any(|r| r == &dest) {
				dest_roots.push(dest.clone());
			}
			let id = CanonicalRootId::new(&dest)
				.map_err(|e| destination_error(&dest, &e))?;
			mapping.map_prefix(&choice.prefix, id);
		}
		let import_plan = match plan_import_with(
			&self.raw_payload,
			"",
			&dest_roots,
			&mapping,
			opts,
		) {
			Ok(plan) => plan,
			Err(e) => {
				if crate::e2e_on() {
					if let TransferError::TargetCollision { .. } = &e {
						app_log!(
							"[APP:PASTE_PLAN_REFUSED: reason=target_collision]"
						);
					}
				}
				self.error = Some(Msg::new("paste_err_plan", [e.to_string()]));
				return Ok(());
			}
		};
		let base = self
			.retained_bytes()
			.saturating_add(size_of::<TransferImportPlan>() + ARC_ALLOWANCE)
			.saturating_add(import_plan.retained_heap_bytes())
			.saturating_add(import_plan.restore_plan().retained_heap_bytes());
		// The existing empty Arc<RestorePlan> charge covers its replacement.
		// Check before making the second full restore-plan copy.
		check_budget(base, limit)?;
		let planned = import_plan.restore_plan().clone();
		if planned.create_operations.is_empty()
			&& planned.delete_operations.is_empty()
			&& planned.skipped_operations.is_empty()
		{
			self.error = Some(Msg::new("paste_err_nothing", []));
			return Ok(());
		}
		let items = self.fill_items(&planned, &dest_roots, base, limit)?;
		self.plan = Arc::new(planned);
		self.import_plan = Some(Arc::new(import_plan));
		self.items = items;
		self.error = None;
		Ok(())
	}

	fn fill_items(
		&self,
		plan: &RestorePlan,
		dest_roots: &[PathBuf],
		base: usize,
		limit: usize,
	) -> Result<Vec<PasteItem>, Msg> {
		let mut items = Vec::new();
		let mut items_heap = 0usize;
		for (op_idx, op) in plan.create_operations.iter().enumerate() {
			let content: Arc<str> = op.content.as_str().into();
			let bytes = content.len();
			let lines = content.lines().count();
			let dest_root = op.root_path.clone();
			let item = PasteItem {
				path: op.relative_path.clone(),
				dest_root_name: root_name(&dest_root),
				dest_path: op.absolute_path.clone(),
				dest_exists: op.existed,
				op: if op.existed {
					PlannedOp::Overwrite
				} else {
					PlannedOp::Create
				},
				overwrite_allowed: false,
				selected: true,
				content,
				bytes,
				lines,
				op_index: op_idx,
				dest_root,
				commit: None,
				rename_note: None,
			};
			items_heap = items_heap.saturating_add(item.retained_heap_bytes());
			items.push(item);
			check_budget(
				base.saturating_add(items_heap)
					.saturating_add(items.capacity() * size_of::<PasteItem>()),
				limit,
			)?;
		}
		for (op_idx, op) in plan.delete_operations.iter().enumerate() {
			let dest_root = dest_roots
				.iter()
				.find(|r| op.absolute_path.starts_with(r))
				.cloned()
				.unwrap_or_else(|| self.destination.clone());
			let item = PasteItem {
				path: op.relative_path.clone(),
				dest_root_name: root_name(&dest_root),
				dest_path: op.absolute_path.clone(),
				dest_exists: op.absolute_path.exists(),
				op: PlannedOp::Delete,
				overwrite_allowed: true,
				selected: true,
				content: Arc::from(""),
				bytes: 0,
				lines: 0,
				op_index: op_idx,
				dest_root,
				commit: None,
				rename_note: None,
			};
			items_heap = items_heap.saturating_add(item.retained_heap_bytes());
			items.push(item);
			check_budget(
				base.saturating_add(items_heap)
					.saturating_add(items.capacity() * size_of::<PasteItem>()),
				limit,
			)?;
		}
		Ok(items)
	}

	pub fn toggle_overwrite(&mut self, idx: usize) {
		if let Some(item) = self.items.get_mut(idx) {
			item.overwrite_allowed = !item.overwrite_allowed;
		}
	}

	pub fn all_selected(&self) -> bool {
		self.items.iter().all(|i| i.selected)
	}

	pub fn toggle_selected(&mut self, idx: usize) {
		if let Some(item) = self.items.get_mut(idx) {
			item.selected = !item.selected;
		}
		if self.whole_commit {
			if !self.all_selected() {
				// Only the informational banner (or none) gives way; a real error stays visible.
				if self
					.error
					.as_ref()
					.is_none_or(|e| e.key == "commit_will_be_refused")
				{
					self.error = Some(Msg::new("commit_subset_rejected", []));
				}
			} else if self
				.error
				.as_ref()
				.is_some_and(|e| e.key == "commit_subset_rejected")
			{
				self.error = self.refusal_banner();
			}
		}
	}

	pub fn refusal_banner(&self) -> Option<Msg> {
		let preview =
			self.commit_preview.as_ref().filter(|_| self.whole_commit)?;
		for (c_idx, commit) in preview.plan().commits.iter().enumerate() {
			if commit.refused_by().is_some() {
				let subject =
					commit.message.lines().next().unwrap_or("").trim();
				let (subject_arg, is_key) = if subject.is_empty() {
					("commit_no_message", true)
				} else {
					(subject, false)
				};
				return Some(if is_key {
					Msg::with_key_arg(
						"commit_will_be_refused",
						[(c_idx + 1).to_string(), subject_arg.to_string()],
						1,
					)
				} else {
					Msg::new(
						"commit_will_be_refused",
						[(c_idx + 1).to_string(), subject_arg.to_string()],
					)
				});
			}
		}
		None
	}

	pub fn commit_summary_label(&self, loc: Locale) -> Option<String> {
		let preview =
			self.commit_preview.as_ref().filter(|_| self.whole_commit)?;
		let commits = &preview.plan().commits;
		let total = commits.len();
		let refused =
			commits.iter().filter(|c| c.refused_by().is_some()).count();
		if refused > 0 {
			Some(tf(
				"paste_commit_count_refused",
				loc,
				&[&total.to_string(), &refused.to_string()],
			))
		} else {
			Some(tf("paste_commit_count", loc, &[&total.to_string()]))
		}
	}

	/// Up / Down follow the rows on screen (the change tree's order).
	#[cfg(test)]
	pub fn select_prev(&mut self) {
		self.selected_item_idx = self.step_target(false);
	}

	#[cfg(test)]
	pub fn select_next(&mut self) {
		self.selected_item_idx = self.step_target(true);
	}

	pub fn step_target(&self, forward: bool) -> usize {
		let order = self.display_order();
		let Some(pos) = order.iter().position(|&i| i == self.selected_item_idx)
		else {
			// The selected row is folded away: re-enter at the near end.
			let edge = if forward { order.first() } else { order.last() };
			return edge.copied().unwrap_or(self.selected_item_idx);
		};
		let next = if forward {
			pos.checked_add(1)
		} else {
			pos.checked_sub(1)
		};
		next.and_then(|n| order.get(n))
			.copied()
			.unwrap_or(self.selected_item_idx)
	}

	/// Item indices in change-tree order: by destination root, then
	/// directory (a root's own files first), then name. `items` itself keeps
	/// the plan's order. A commit replay lists its visible rows in replay
	/// order instead, exactly as [`Self::commit_rows`] shows them.
	pub fn display_order(&self) -> Vec<usize> {
		if self.whole_commit {
			return self
				.commit_rows()
				.into_iter()
				.filter_map(|n| match n {
					PasteNode::File(ix, _) => Some(ix),
					_ => None,
				})
				.collect();
		}
		// Case-insensitive like IntelliJ's tree; the raw path breaks ties.
		let mut order: Vec<usize> = (0..self.items.len()).collect();
		order.sort_by_cached_key(|&i| {
			let it = &self.items[i];
			let (dir, name) = split_dir(&it.path);
			(
				it.dest_root_name.clone(),
				dir.to_lowercase(),
				name.to_lowercase(),
				it.path.clone(),
			)
		});
		order
	}

	/// The change tree IntelliJ's Apply Patch dialog shows: one root node per
	/// destination repository, one node per directory, then its files.
	pub fn tree_rows(&self) -> Vec<PasteNode> {
		let mut rows = Vec::new();
		let (mut root, mut dir): (Option<&str>, Option<&str>) = (None, None);
		// Rows of the open root / directory node, counted as files arrive.
		let (mut root_row, mut dir_row) = (0, None);
		for ix in self.display_order() {
			let it = &self.items[ix];
			if root != Some(it.dest_root_name.as_str()) {
				root = Some(&it.dest_root_name);
				dir = None;
				root_row = rows.len();
				rows.push(PasteNode::Root(it.dest_root_name.clone(), 0));
			}
			let (d, _) = split_dir(&it.path);
			if dir != Some(d) {
				dir = Some(d);
				dir_row = (!d.is_empty()).then_some(rows.len());
				if !d.is_empty() {
					rows.push(PasteNode::Dir(d.to_string(), 0));
				}
			}
			for row in [Some(root_row), dir_row].into_iter().flatten() {
				if let PasteNode::Root(_, n) | PasteNode::Dir(_, n) =
					&mut rows[row]
				{
					*n += 1;
				}
			}
			rows.push(PasteNode::File(ix, if d.is_empty() { 1 } else { 2 }));
		}
		rows
	}

	/// The commit replay as the user will see it: each commit's header, in
	/// replay order, followed by its file rows. An empty commit is a header
	/// with no rows; a collapsed commit keeps only its header.
	pub fn commit_rows(&self) -> Vec<PasteNode> {
		let Some(preview) = &self.commit_preview else {
			return Vec::new();
		};
		let mut rows = Vec::with_capacity(self.items.len());
		let mut ix = 0;
		for c in 0..preview.plan().commits.len() {
			rows.push(PasteNode::Commit(c));
			let shown = !self.collapsed_commits.contains(&c);
			while self.items.get(ix).is_some_and(|i| i.commit == Some(c)) {
				if shown {
					rows.push(PasteNode::File(ix, 1));
				}
				ix += 1;
			}
		}
		rows
	}

	/// Folds or unfolds one commit's file rows.
	///
	/// Does not mutate the selection: the caller (session) is responsible for
	/// moving the selection if this returns `Some(ix)`. When the fold hides
	/// the selected row, returns `Some(ix)` of the next visible row (or the
	/// previous one at the end), so Space and the detail pane never act on a
	/// row the user cannot see.
	pub fn toggle_commit_collapsed(&mut self, c: usize) -> Option<usize> {
		let before = self.display_order();
		if !self.collapsed_commits.remove(&c) {
			self.collapsed_commits.insert(c);
		}
		let after = self.display_order();
		if after.contains(&self.selected_item_idx) {
			return None;
		}
		let pos = before.iter().position(|&i| i == self.selected_item_idx)?;
		let visible = |i: &&usize| after.contains(i);
		let next = before[pos..].iter().find(visible);
		let prev = before[..pos].iter().rev().find(visible);
		next.or(prev).copied()
	}

	pub fn counts(&self) -> PasteCounts {
		let mut c = PasteCounts {
			skips: self.plan.skipped_operations.len(),
			..PasteCounts::default()
		};
		for it in &self.items {
			if it.overwritable() {
				c.existing += 1;
			}
			match it.action() {
				RowAction::Create => c.creates += 1,
				RowAction::Overwrite => c.overwrites += 1,
				RowAction::Delete => c.deletes += 1,
				RowAction::CommitRefused(_)
				| RowAction::OverwritePending
				| RowAction::Excluded { by_commit: true } => {}
				RowAction::Skip(_)
				| RowAction::DeleteMissing
				| RowAction::KeepExisting
				| RowAction::Excluded { by_commit: false } => c.skips += 1,
			}
		}
		c
	}

	/// (rows, not written) of commit `c`; items are grouped by commit.
	pub fn commit_counts(&self, c: usize) -> (usize, usize) {
		let start = self.items.partition_point(|i| i.commit < Some(c));
		let rows = self.items[start..]
			.iter()
			.take_while(|i| i.commit == Some(c));
		rows.fold((0, 0), |(n, off), it| {
			let not_written = it.action().is_not_written();
			(n + 1, off + usize::from(not_written))
		})
	}

	pub fn overwrite_missing(&self) -> bool {
		self.items
			.iter()
			.any(|i| i.overwritable() && !i.overwrite_allowed)
	}

	pub fn set_all_overwrite(&mut self, allowed: bool) {
		for item in &mut self.items {
			if item.overwritable() {
				item.overwrite_allowed = allowed;
			}
		}
	}

	/// Checks whether destination files on disk have changed since preview generation.
	pub fn check_stale_destination(&self) -> Result<(), Msg> {
		if let Some(preview) = &self.commit_preview {
			preview.revalidate().map_err(stale_msg)?;
		}
		if let Some(import_plan) = &self.import_plan {
			import_plan
				.destination_freshness()
				.revalidate()
				.map_err(stale_msg)?;
		}
		Ok(())
	}

	pub fn execute(&self) -> Result<PasteApplyResult, Msg> {
		if let Some(preview) = &self.commit_preview {
			return self.execute_commit(preview);
		}
		if !self.mapping_ready() {
			return Err(Msg::new("mapping_required", []));
		}

		let Some(ref import_plan) = self.import_plan else {
			return Err(Msg::new(
				"paste_err_plan",
				["no active import plan".into()],
			));
		};

		let mut unchecked_creates = BTreeSet::new();
		let mut unchecked_deletes = BTreeSet::new();
		let mut skipped_existing = 0;

		for item in &self.items {
			if !item.selected {
				if item.is_delete() {
					unchecked_deletes.insert(item.op_index);
				} else {
					unchecked_creates.insert(item.op_index);
				}
				continue;
			}
			if item.is_delete() {
				// selected delete proceeds
			} else if matches!(item.action(), RowAction::KeepExisting) {
				unchecked_creates.insert(item.op_index);
				skipped_existing += 1;
			}
		}

		let selection = RestoreSelection {
			overwrite_existing: true,
			skip_existing: false,
			unchecked_creates,
			unchecked_deletes,
		};

		// Run confirmed plan directly through shared transfer engine with freshness check
		let mut result = import_plan.apply(&selection).map_err(stale_msg)?;
		result.skipped_existing_count += skipped_existing;
		Ok(PasteApplyResult {
			files: result,
			created_commits: Vec::new(),
		})
	}

	fn execute_commit(
		&self,
		preview: &CommitReplayPreview,
	) -> Result<PasteApplyResult, Msg> {
		if self.items.iter().any(|item| !item.selected) {
			return Err(Msg::new("commit_subset_rejected", []));
		}
		// Core overwrites (spec 4.3); asking first is this session's rule. Stale still wins over the prompt, as before.
		if self.overwrite_missing() {
			preview.revalidate().map_err(stale_msg)?;
			return Err(Msg::new("commit_overwrite_required", []));
		}
		let replay_res = preview.apply().map_err(|e| match e {
			TransferError::Git(e) => {
				Msg::new("error_open_repo", [e.to_string()])
			}
			other => stale_msg(other),
		})?;
		if let Some(fail) = replay_res.failure {
			if replay_res.created.is_empty() {
				let subject = fail.message.lines().next().unwrap_or("").trim();
				let (subject_arg, subject_is_key) = if subject.is_empty() {
					("commit_no_message", true)
				} else {
					(subject, false)
				};
				let mut key_args = Vec::new();
				if subject_is_key {
					key_args.push(1);
				}
				let (cause_arg, cause_is_key) = match fail.layout_conflict {
					Some(conflict) => {
						(layout_conflict_cause_key(conflict), true)
					}
					None => (fail.error.as_str(), false),
				};
				if cause_is_key {
					key_args.push(2);
				}
				let msg = if key_args.is_empty() {
					Msg::new(
						"commit_replay_refused",
						[
							(fail.index + 1).to_string(),
							subject_arg.to_string(),
							cause_arg.to_string(),
						],
					)
				} else {
					Msg::with_key_args(
						"commit_replay_refused",
						[
							(fail.index + 1).to_string(),
							subject_arg.to_string(),
							cause_arg.to_string(),
						],
						key_args,
					)
				};
				return Err(msg);
			}
			let created = replay_res.created.join(", ");
			return Err(match fail.layout_conflict {
				Some(conflict) => {
					let path = fail
						.conflict_path
						.as_deref()
						.unwrap_or(fail.error.as_str());
					Msg::with_key_arg(
						"commit_replay_partial_refused",
						[
							created,
							(fail.index + 1).to_string(),
							path.to_string(),
							layout_conflict_cause_key(conflict).to_string(),
						],
						3,
					)
				}
				None => {
					Msg::new("commit_replay_partial", [created, fail.error])
				}
			});
		}
		Ok(PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: replay_res.created.len(),
				overwritten_count: 0,
				deleted_count: 0,
				skipped_existing_count: 0,
				errors: Vec::new(),
			},
			created_commits: replay_res.created,
		})
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;

	#[test]
	fn apply_status_lists_each_failed_path_not_only_the_count() {
		let result = |errors: Vec<String>| PasteApplyResult {
			files: RestoreExecutionResult {
				created_count: 2,
				overwritten_count: 1,
				skipped_existing_count: 0,
				deleted_count: 0,
				errors,
			},
			created_commits: Vec::new(),
		};
		let ok = result(Vec::new()).status();
		assert_eq!(ok.key, "status_paste_done");

		let partial = result(vec![
			"a/x.txt: Permission denied".into(),
			"b/y.txt: unsafe path".into(),
		])
		.status();
		assert_eq!(partial.key, "status_paste_partial");
		for loc in [crate::i18n::Locale::ZhTw, crate::i18n::Locale::En] {
			let text = partial.render(loc);
			assert!(
				text.contains(
					"a/x.txt: Permission denied; b/y.txt: unsafe path"
				),
				"{text}"
			);
			assert!(!text.contains("{}"), "{text}");
		}
	}

	#[test]
	fn change_tree_groups_by_root_and_directory_in_display_order() {
		let dir = tempfile::tempdir().unwrap();
		let raw = "// clipcode-root: r\n// FILE: r/b/z.txt\n1\n// FILE: r/a.txt\n2\n// FILE: r/b/y.txt\n3\n// FILE: r/c/d/e.txt\n4\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		let paths: Vec<String> =
			plan.items.iter().map(|i| i.path.clone()).collect();
		let path = |ix: usize| paths[ix].clone();
		let rows = plan.tree_rows();
		let root = plan.items[0].dest_root_name.clone();
		let shown: Vec<String> = rows
			.iter()
			.map(|r| match r {
				PasteNode::Root(n, c) => format!("R {n} {c}"),
				PasteNode::Dir(d, c) => format!("D {d} {c}"),
				PasteNode::File(ix, depth) => {
					format!("F {} {depth}", path(*ix))
				}
				PasteNode::Commit(c) => format!("C {c}"),
			})
			.collect();
		assert_eq!(
			shown,
			[
				format!("R {root} 4"),
				"D r 1".into(),
				"F r/a.txt 2".into(),
				"D r/b 2".into(),
				"F r/b/y.txt 2".into(),
				"F r/b/z.txt 2".into(),
				"D r/c/d 1".into(),
				"F r/c/d/e.txt 2".into(),
			]
		);
		// Up / Down walk the tree's order, not the plan's.
		plan.selected_item_idx =
			plan.items.iter().position(|i| i.path == "r/a.txt").unwrap();
		plan.select_next();
		assert_eq!(path(plan.selected_item_idx), "r/b/y.txt");
		plan.select_next();
		plan.select_next();
		plan.select_next();
		assert_eq!(path(plan.selected_item_idx), "r/c/d/e.txt");
		plan.select_prev();
		assert_eq!(path(plan.selected_item_idx), "r/b/z.txt");
	}
	use std::fs;

	fn captured(text: String, dest: &Path) -> PasteRequest {
		PasteRequest::Clipboard {
			text,
			dest: dest.into(),
			roots: Vec::new(),
			generation: 1,
		}
	}

	#[test]
	fn pending_repeated_paste_keeps_one_captured_latest_until_owned_job_finishes(
	) {
		let dir = tempfile::tempdir().unwrap();
		let pool = Arc::new(Mutex::new(PastePending::default()));
		let token = CancelToken::new();
		lock_pending(&pool)
			.enqueue(captured("// FILE: first.txt\nfirst".into(), dir.path()))
			.unwrap();
		let first = PastePending::start(&pool, None, token.clone())
			.unwrap()
			.unwrap();
		let mut life = crate::lifecycle::Lifecycle::new(1);
		let (id, held) = life.register(
			crate::lifecycle::JobKind::CancellableRead,
			Some(token.clone()),
		);
		let input_bytes = lock_pending(&pool).input.as_ref().unwrap().1;
		token.cancel();
		for n in 1..=100 {
			let mut pending = lock_pending(&pool);
			pending.invalidate(n);
			pending
				.enqueue(captured(
					format!("// FILE: latest-{n}.txt\ncaptured-{n}"),
					dir.path(),
				))
				.unwrap();
			assert_eq!(pending.input.as_ref().unwrap().1, input_bytes);
			assert!(pending.ready.is_none());
			assert!(
				pending.total_with_ui(
					pending.ui_bytes,
					pending.apply_allowance,
					pending.ui_raw.as_ref()
				) <= MAX_RETAINED_PREVIEW_BYTES
			);
			assert!(life.is_live(id));
			assert_eq!(life.live_jobs(), 1);
		}
		// Releasing input does not release the lifecycle slot prematurely.
		drop(first);
		assert!(lock_pending(&pool).input.is_none());
		assert!(life.is_live(id));
		drop(held);
		assert!(!life.is_live(id));
		PastePending::start(&pool, None, CancelToken::new())
			.unwrap()
			.unwrap()
			.run(&RunOptions::default());
		let plan = lock_pending(&pool).ready.take().unwrap().result.unwrap();
		assert_eq!(plan.items[0].path, "latest-100.txt");
		assert_eq!(&*plan.items[0].content, "captured-100");
		assert!(plan.executable());
		assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
	}

	#[test]
	fn pending_overflow_drops_older_intent_and_raw_share_requires_identical_arc(
	) {
		let dir = tempfile::tempdir().unwrap();
		let raw = "// FILE: a.txt\nbody";
		let mut shell = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		shell.clear_file_plan();
		let pool = Arc::new(Mutex::new(PastePending::default()));
		lock_pending(&pool)
			.admit_ui(None, Some(&shell), None)
			.unwrap();
		lock_pending(&pool)
			.enqueue(PasteRequest::Remap {
				prefix: "a".into(),
				keep: true,
			})
			.unwrap();
		let work = PastePending::start(&pool, Some(&shell), CancelToken::new())
			.unwrap()
			.unwrap();
		{
			let mut state = lock_pending(&pool);
			assert_eq!(
				state.shared_raw(state.ui_raw.as_ref()),
				raw.len() + ARC_ALLOWANCE
			);
			let independent: Arc<str> = raw.into();
			assert_eq!(
				state.shared_raw(Some(&Arc::downgrade(&independent))),
				0
			);
			let held = state.input.as_ref().unwrap().1;
			state.admit_ui(None, None, None).unwrap();
			assert_eq!(state.shared_raw(state.ui_raw.as_ref()), 0);
			assert_eq!(state.input.as_ref().unwrap().1, held);
			state
				.enqueue(captured(
					"// FILE: stale.txt\nstale".into(),
					dir.path(),
				))
				.unwrap();
			assert!(state
				.enqueue(captured(
					"x".repeat(MAX_RETAINED_PREVIEW_BYTES),
					dir.path()
				))
				.is_err());
			assert!(state.latest.is_none());
		}
		drop(work);
		assert!(lock_pending(&pool).input.is_none());
		assert!(!shell.executable());
		assert!(shell.mapping_ready());
		assert!(shell.execute().is_err());
		assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
	}

	#[test]
	fn pending_cancel_publication_orders_and_unwind_release_owned_input() {
		let dir = tempfile::tempdir().unwrap();
		for invalidate_before in [true, false] {
			let pool = Arc::new(Mutex::new(PastePending::default()));
			lock_pending(&pool)
				.enqueue(captured("// FILE: a.txt\nbody".into(), dir.path()))
				.unwrap();
			let work = PastePending::start(&pool, None, CancelToken::new())
				.unwrap()
				.unwrap();
			if invalidate_before {
				lock_pending(&pool).invalidate(1);
			}
			work.run(&RunOptions::default());
			assert!(lock_pending(&pool).input.is_none());
			assert_eq!(lock_pending(&pool).ready.is_none(), invalidate_before);
			lock_pending(&pool).invalidate(2);
			assert!(lock_pending(&pool).ready.is_none());
		}
		let pool = Arc::new(Mutex::new(PastePending::default()));
		lock_pending(&pool)
			.enqueue(captured("// FILE: panic.txt\nbody".into(), dir.path()))
			.unwrap();
		let work = PastePending::start(&pool, None, CancelToken::new())
			.unwrap()
			.unwrap();
		let result =
			std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
				let _owned = work;
				panic!("exercise owned input unwinding");
			}));
		assert!(result.is_err());
		assert!(lock_pending(&pool).input.is_none());
		assert!(lock_pending(&pool).ready.is_none());
		assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
	}

	#[test]
	fn pending_mailbox_transfer_uses_one_charge_and_admitted_apply_needs_no_more_budget(
	) {
		let dir = tempfile::tempdir().unwrap();
		let pool = Arc::new(Mutex::new(PastePending::default()));
		lock_pending(&pool)
			.enqueue(captured("// FILE: a.txt\nfull bytes".into(), dir.path()))
			.unwrap();
		PastePending::start(&pool, None, CancelToken::new())
			.unwrap()
			.unwrap()
			.run(&RunOptions::default());
		let outcome = lock_pending(&pool).ready.take().unwrap();
		let plan = outcome.result.unwrap();
		let detail = plan.detail_preview();
		let mut ordinary = Preview::new(
			PreviewSource::WorkingFile,
			None,
			"old".into(),
			false,
			Language::Plain,
		);
		{
			let mut state = lock_pending(&pool);
			state
				.admit_ui(Some(&ordinary), Some(&plan), detail.as_ref())
				.unwrap();
			let used = state.total_with_ui(
				state.ui_bytes,
				state.apply_allowance,
				state.ui_raw.as_ref(),
			);
			ordinary.source = PreviewSource::CommitFile {
				sha: String::with_capacity(MAX_RETAINED_PREVIEW_BYTES - used),
			};
			state
				.admit_ui(Some(&ordinary), Some(&plan), detail.as_ref())
				.unwrap();
			assert_eq!(
				state.total_with_ui(
					state.ui_bytes,
					state.apply_allowance,
					state.ui_raw.as_ref()
				),
				MAX_RETAINED_PREVIEW_BYTES
			);
			state.admit_ui(Some(&ordinary), None, None).unwrap();
			state.ready = Some(PasteOutcome {
				result: Ok(plan),
				remap: None,
			});
			let duplicate = state
				.ready
				.as_ref()
				.unwrap()
				.result
				.as_ref()
				.unwrap()
				.clone();
			assert!(state
				.admit_ui(Some(&ordinary), Some(&duplicate), detail.as_ref())
				.is_err());
		}
		let plan = {
			let mut state = lock_pending(&pool);
			let plan = state.ready.take().unwrap().result.unwrap();
			state
				.admit_ui(Some(&ordinary), Some(&plan), detail.as_ref())
				.unwrap();
			plan
		};
		let apply = PasteApplyWorker::new(&plan, &pool);
		{
			let state = lock_pending(&pool);
			assert!(
				state.apply_active > 0
					&& state.apply_active <= state.apply_allowance
			);
			assert_eq!(
				state.total_with_ui(
					state.ui_bytes,
					state.apply_allowance,
					state.ui_raw.as_ref()
				),
				MAX_RETAINED_PREVIEW_BYTES
			);
		}
		apply.execute().unwrap();
		assert_eq!(lock_pending(&pool).apply_active, 0);
		assert_eq!(fs::read(dir.path().join("a.txt")).unwrap(), b"full bytes");
	}

	#[test]
	fn retained_budget_refuses_raw_before_parsing_and_file_copy_amplification()
	{
		let dir = tempfile::tempdir().unwrap();
		let raw = "not a payload";
		assert_eq!(
			PastePreviewPlan::build_with_limit(
				raw,
				dir.path(),
				&[],
				1,
				&RunOptions::default(),
				raw.len() - 1
			)
			.unwrap_err()
			.key,
			"preview_memory_limit"
		);
		let raw = format!("// FILE: add.txt\n{}\n", "x".repeat(4096));
		let limit = raw.len() + 1024;
		assert_eq!(
			PastePreviewPlan::build_with_limit(
				&raw,
				dir.path(),
				&[],
				1,
				&RunOptions::default(),
				limit
			)
			.unwrap_err()
			.key,
			"preview_memory_limit"
		);
		assert!(!dir.path().join("add.txt").exists());
	}

	#[test]
	fn retained_budget_failed_rebuild_disarms_every_old_write() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("keep.txt"), "old").unwrap();
		fs::write(dir.path().join("delete.txt"), "keep until confirmed")
			.unwrap();
		let raw = "// FILE: keep.txt\nnew\n// FILE: add.txt\nadded\n// FILE: [DELETED] delete.txt\n// This file has been deleted in this change\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			raw,
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		for item in &mut plan.items {
			item.overwrite_allowed = true;
		}
		assert!(plan.import_plan.is_some());
		let err = plan
			.rebuild_with_limit(&RunOptions::default(), raw.len() + 512)
			.unwrap_err();
		assert_eq!(err.key, "preview_memory_limit");
		assert!(plan.import_plan.is_none());
		assert!(plan.items.is_empty());
		assert!(plan.plan.create_operations.is_empty());
		assert!(plan.plan.delete_operations.is_empty());
		assert!(plan.execute().is_err());
		assert_eq!(
			fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
			"old"
		);
		assert!(dir.path().join("delete.txt").exists());
		assert!(!dir.path().join("add.txt").exists());
	}

	#[test]
	fn retained_budget_counts_mapping_candidate_amplification() {
		let dir = tempfile::tempdir().unwrap();
		let roots: Vec<_> = (0..12)
			.map(|i| {
				let root = dir.path().join(format!("root-{i}"));
				fs::create_dir(&root).unwrap();
				root
			})
			.collect();
		let raw: String = (0..20)
			.map(|i| format!("// FILE: prefix-{i}/a.txt\nx\n"))
			.collect();
		let full = PastePreviewPlan::build_from_clipboard_text(
			&raw, &roots[0], &roots, 1,
		)
		.unwrap();
		let limit = full.retained_bytes() - 1;
		assert!(raw.len() < limit);
		assert_eq!(full.prefix_choices.len(), 20);
		assert_eq!(
			PastePreviewPlan::build_with_limit(
				&raw,
				&roots[0],
				&roots,
				1,
				&RunOptions::default(),
				limit
			)
			.unwrap_err()
			.key,
			"preview_memory_limit"
		);
		assert!(roots
			.iter()
			.all(|root| fs::read_dir(root).unwrap().next().is_none()));
	}

	#[test]
	fn retained_budget_counts_spare_capacities_in_native_and_restore_plan() {
		let dir = tempfile::tempdir().unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: a.txt\nx\n",
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		let before = plan.retained_bytes();
		let content =
			&mut Arc::make_mut(&mut plan.plan).create_operations[0].content;
		let old = content.capacity();
		content.reserve(65536);
		let growth = content.capacity() - old;
		assert_eq!(content.len(), 1);
		assert_eq!(plan.retained_bytes(), before + growth);
		let before = plan.retained_bytes();
		let old = plan.destination.capacity();
		plan.destination.reserve(16384);
		assert_eq!(
			plan.retained_bytes(),
			before + plan.destination.capacity() - old
		);
		assert!(admit_preview_state_with_limit(
			None,
			Some(&plan),
			None,
			before
		)
		.is_err());
	}

	// 導覽被拒的那一半已搬到
	// paste::preview::tests::refused_navigation_keeps_plan_selection_detail_and_busy_state。
	#[test]
	fn retained_budget_admits_aggregate_only_and_preserves_read_only_navigation(
	) {
		let dir = tempfile::tempdir().unwrap();
		let raw = format!(
			"// FILE: small.txt\nx\n// FILE: large.txt\n{}\n",
			"z\n".repeat(2000)
		);
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&raw,
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		let ordinary = Preview::new(
			PreviewSource::WorkingFile,
			Some("prior.txt".into()),
			"reader\n".repeat(1000),
			false,
			Language::Plain,
		);
		let detail_bytes = plan.detail_preview().unwrap().retained_bytes();
		let exact =
			ordinary.retained_bytes() + plan.retained_bytes() + detail_bytes;
		assert!(admit_preview_state_with_limit(
			Some(&ordinary),
			Some(&plan),
			plan.detail_preview().as_ref(),
			exact
		)
		.is_ok());
		let reject_limit = exact - 1;
		assert!(admit_preview_state_with_limit(
			Some(&ordinary),
			Some(&plan),
			plan.detail_preview().as_ref(),
			reject_limit
		)
		.is_err());
		assert_eq!(plan.execute().unwrap().files.created_count, 2);
		assert_eq!(
			fs::read_to_string(dir.path().join("large.txt")).unwrap(),
			"z\n".repeat(2000).trim_end_matches('\n')
		);
	}

	#[test]
	fn retained_budget_rejects_repeated_commit_headers_without_replay() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let dir = tempfile::tempdir().unwrap();
		git_init(dir.path());
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "message ".repeat(500),
				author_name: "Author".into(),
				author_email: "a@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: (0..12)
					.map(|i| CommitFile {
						path: format!("f-{i}.txt"),
						old_path: None,
						change: FileChange::Added,
						content: Some("body".into()),
						not_copied: None,
					})
					.collect(),
			}],
		};
		let raw = commits::to_clipboard_text(&payload);
		let full = PastePreviewPlan::build_from_clipboard_text(
			&raw,
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		let limit = full.retained_bytes() - 1;
		assert!(raw.len() < limit);
		assert_eq!(
			PastePreviewPlan::build_with_limit(
				&raw,
				dir.path(),
				&[],
				1,
				&RunOptions::default(),
				limit
			)
			.unwrap_err()
			.key,
			"preview_memory_limit"
		);
		assert!(
			(0..12).all(|i| !dir.path().join(format!("f-{i}.txt")).exists())
		);
		let head = git_output(dir.path(), &["rev-parse", "--verify", "HEAD"]);
		assert!(
			!head.status.success(),
			"overflow must not create even one commit"
		);
	}

	#[test]
	fn test_paste_plan_defaults_overwrite_to_false() {
		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path();
		let existing_file = dest.join("hello.txt");
		fs::write(&existing_file, "original content").unwrap();

		let payload = "// FILE: hello.txt\nhello modified\n// FILE: new.txt\nnew content\n";
		let plan =
			PastePreviewPlan::build_from_clipboard_text(payload, dest, &[], 1)
				.unwrap();

		assert_eq!(plan.items.len(), 2);
		let existing =
			plan.items.iter().find(|i| i.path == "hello.txt").unwrap();
		assert!(existing.dest_exists);
		assert_eq!(existing.op, PlannedOp::Overwrite);
		assert!(
			!existing.overwrite_allowed,
			"overwrite must be off by default"
		);

		let new_item = plan.items.iter().find(|i| i.path == "new.txt").unwrap();
		assert!(!new_item.dest_exists);
		assert_eq!(new_item.op, PlannedOp::Create);

		// Execute without enabling overwrite: existing file remains unchanged!
		let res = plan.execute().unwrap().files;
		assert_eq!(res.created_count, 1);
		assert_eq!(res.overwritten_count, 0);
		assert_eq!(res.skipped_existing_count, 1);
		assert_eq!(
			fs::read_to_string(&existing_file).unwrap(),
			"original content"
		);
		assert_eq!(
			fs::read_to_string(dest.join("new.txt")).unwrap(),
			"new content"
		);
	}

	#[test]
	fn test_gui_root_internal_absolute_path_resolved() {
		let dir = tempfile::tempdir().unwrap();
		let dest = dunce::canonicalize(dir.path()).unwrap();
		let target_file = dest.join("src").join("c.ts");
		let payload =
			format!("// FILE: {}\ncontent c\n", target_file.display());
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&payload,
			&dest,
			&[],
			1,
		)
		.unwrap();

		assert_eq!(plan.items.len(), 1);
		assert_eq!(plan.items[0].path, "src/c.ts");
		assert_eq!(plan.items[0].dest_path, target_file);
		assert_eq!(plan.items[0].op, PlannedOp::Create);

		let res = plan.execute().unwrap().files;
		assert_eq!(res.created_count, 1);
		assert!(target_file.exists());
		assert_eq!(fs::read_to_string(&target_file).unwrap(), "content c");
	}

	#[test]
	fn test_gui_unresolvable_absolute_deleted_leaves_nested_file() {
		let dir = tempfile::tempdir().unwrap();
		let dest = dunce::canonicalize(dir.path()).unwrap();
		let nested_file = dest.join("opt").join("unrelated").join("gone.txt");
		fs::create_dir_all(nested_file.parent().unwrap()).unwrap();
		fs::write(&nested_file, "nested body").unwrap();
		assert!(nested_file.exists());

		let payload = "// FILE: [DELETED] /opt/unrelated/gone.txt\n";
		let plan =
			PastePreviewPlan::build_from_clipboard_text(payload, &dest, &[], 1)
				.unwrap();

		// The plan has no delete operation for the unresolvable absolute deleted entry
		assert!(plan.items.iter().all(|i| i.op != PlannedOp::Delete));

		let _ = plan.execute().unwrap();
		assert!(
			nested_file.exists(),
			"pre-existing nested file must survive after apply"
		);
		assert_eq!(fs::read_to_string(&nested_file).unwrap(), "nested body");
	}

	#[test]
	fn test_paste_stale_destination_detection() {
		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path();
		let existing_file = dest.join("target.txt");
		fs::write(&existing_file, "initial").unwrap();

		let payload = "// FILE: target.txt\nupdated content\n";
		let plan =
			PastePreviewPlan::build_from_clipboard_text(payload, dest, &[], 1)
				.unwrap();

		// Destination changes on disk after preview was generated!
		std::thread::sleep(std::time::Duration::from_millis(15));
		fs::write(
			&existing_file,
			"external modification between preview and apply",
		)
		.unwrap();

		// Execute must fail with a meaningful stale destination error!
		let err = plan.execute().unwrap_err();
		assert!(
			err.key == "stale_modified",
			"stale destination modification must block execution: {err}"
		);
	}

	#[test]
	fn test_paste_plan_delete_and_individual_selection_toggle() {
		let dir = tempfile::tempdir().unwrap();
		let dest = dir.path();
		let file_to_del = dest.join("old.txt");
		fs::write(&file_to_del, "to be deleted").unwrap();

		let payload = "// FILE: [DELETED] old.txt\n// This file has been deleted in this change\n// FILE: add.txt\nadded\n";
		let mut plan =
			PastePreviewPlan::build_from_clipboard_text(payload, dest, &[], 1)
				.unwrap();

		assert_eq!(plan.items.len(), 2);
		let del_item = plan.items.iter().find(|i| i.path == "old.txt").unwrap();
		assert!(del_item.is_delete());
		assert_eq!(del_item.op, PlannedOp::Delete);

		// Toggle delete off
		let del_idx =
			plan.items.iter().position(|i| i.path == "old.txt").unwrap();
		plan.toggle_selected(del_idx);
		assert!(!plan.items[del_idx].selected);

		let res = plan.execute().unwrap().files;
		assert_eq!(res.created_count, 1);
		assert_eq!(res.deleted_count, 0);
		// old.txt must NOT have been deleted because it was unchecked!
		assert!(file_to_del.exists());

		// Rebuild plan reflecting current state on disk (add.txt now exists)
		let plan2 =
			PastePreviewPlan::build_from_clipboard_text(payload, dest, &[], 2)
				.unwrap();
		let del_idx2 = plan2
			.items
			.iter()
			.position(|i| i.path == "old.txt")
			.unwrap();
		assert!(plan2.items[del_idx2].selected);
		let res2 = plan2.execute().unwrap().files;
		assert_eq!(res2.deleted_count, 1);
		assert!(!file_to_del.exists());
	}

	#[test]
	fn test_paste_multi_repo_prefix_mapping() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path().join("repos");
		fs::create_dir(&root).unwrap();
		#[cfg(unix)]
		let root = {
			let alias = tmp.path().join("alias");
			std::os::unix::fs::symlink(&root, &alias).unwrap();
			assert_ne!(alias, CanonicalRootId::new(&alias).unwrap().path());
			alias
		};
		let repo_a = root.join("repo-a");
		let repo_b = root.join("repo-b");
		fs::create_dir(&repo_a).unwrap();
		fs::create_dir(&repo_b).unwrap();

		let payload = "// FILE: repo-a/file_a.txt\ncontent a\n// FILE: repo-b/file_b.txt\ncontent b\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&repo_a,
			&[repo_a.clone(), repo_b.clone()],
			1,
		)
		.unwrap();
		assert!(!plan.mapping_ready());
		assert_eq!(plan.execute().unwrap_err().key, "mapping_required");
		assert!(!repo_a.join("file_a.txt").exists());
		assert!(!repo_b.join("file_b.txt").exists());
		plan.set_prefix_destination("repo-a", &repo_a).unwrap();
		plan.set_prefix_destination("repo-b", &repo_b).unwrap();

		assert_eq!(plan.items.len(), 2);
		let item_a =
			plan.items.iter().find(|i| i.path == "file_a.txt").unwrap();
		assert_eq!(
			item_a.dest_root,
			CanonicalRootId::new(&repo_a).unwrap().path()
		);
		let item_b =
			plan.items.iter().find(|i| i.path == "file_b.txt").unwrap();
		assert_eq!(
			item_b.dest_root,
			CanonicalRootId::new(&repo_b).unwrap().path()
		);

		let res = plan.execute().unwrap().files;
		assert_eq!(res.created_count, 2);
		assert_eq!(
			fs::read_to_string(repo_a.join("file_a.txt")).unwrap(),
			"content a"
		);
		assert_eq!(
			fs::read_to_string(repo_b.join("file_b.txt")).unwrap(),
			"content b"
		);
	}

	/// Runs git with a bounded wait; the one process wait of the tests.
	pub(crate) fn git_output(
		path: &Path,
		args: &[&str],
	) -> std::process::Output {
		let mut child = std::process::Command::new("git")
			.current_dir(path)
			.args(args)
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped())
			.spawn()
			.expect("spawn git");
		let start = std::time::Instant::now();
		let timeout = std::time::Duration::from_secs(10);
		loop {
			if child.try_wait().expect("try_wait").is_some() {
				break;
			}
			if start.elapsed() > timeout {
				let _ = child.kill();
				panic!("git {args:?} timed out after {timeout:?}");
			}
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
		child.wait_with_output().expect("wait_with_output")
	}

	pub(crate) fn git_run(path: &Path, args: &[&str]) -> String {
		let out = git_output(path, args);
		assert!(
			out.status.success(),
			"git {args:?}: {}",
			String::from_utf8_lossy(&out.stderr)
		);
		String::from_utf8(out.stdout).expect("git stdout is utf-8")
	}

	pub(crate) fn git_init(path: &Path) {
		git_run(path, &["init", "-q", "-b", "main"]);
		git_run(path, &["config", "user.name", "Probe"]);
		git_run(path, &["config", "user.email", "probe@example.invalid"]);
	}

	fn commit_payload_modifying_a() -> String {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let payload = CommitsPayload {
			commits: vec![CommitRecord {
				message: "incoming\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "a.txt".into(),
					old_path: None,
					change: FileChange::Modified,
					content: Some("incoming\n".into()),
					not_copied: None,
				}],
			}],
		};
		commits::to_clipboard_text(&payload)
	}

	/// c1: a binary (not copied) file, a new text file and a rename; c2: no
	/// files at all; c3: a modification of an existing file.
	fn commit_payload_mixed() -> String {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
			NotCopiedReason,
		};
		let file = |path: &str, change, content: Option<&str>| CommitFile {
			path: path.into(),
			old_path: None,
			change,
			content: content.map(Into::into),
			not_copied: None,
		};
		let record = |message: &str, files| CommitRecord {
			message: message.into(),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files,
		};
		let mut binary = file("img.bin", FileChange::Added, None);
		binary.not_copied = Some(NotCopiedReason::Binary);
		let mut renamed =
			file("dir/new.txt", FileChange::Renamed, Some("moved\n"));
		renamed.old_path = Some("old.txt".into());
		commits::to_clipboard_text(&CommitsPayload {
			commits: vec![
				record(
					"first\n\nbody",
					vec![
						binary,
						file("fresh.txt", FileChange::Added, Some("fresh\n")),
						renamed,
					],
				),
				record("empty one", Vec::new()),
				record(
					"third",
					vec![file("a.txt", FileChange::Modified, Some("new\n"))],
				),
			],
		})
	}

	fn mixed_plan() -> (tempfile::TempDir, PastePreviewPlan) {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		fs::write(path.join("old.txt"), "old\n").unwrap();
		for args in [&["add", "."][..], &["commit", "-qm", "base"][..]] {
			git_run(&path, args);
		}
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&commit_payload_mixed(),
			&path,
			&[],
			1,
		)
		.unwrap();
		(dir, plan)
	}

	#[test]
	fn commit_preview_lists_skips_renames_and_groups_in_replay_order() {
		let (_dir, plan) = mixed_plan();
		let shown: Vec<String> = plan
			.commit_rows()
			.iter()
			.map(|r| match r {
				PasteNode::Commit(c) => format!("C{c}"),
				PasteNode::File(ix, _) => {
					let i = &plan.items[*ix];
					format!("{} {}", header_label(i.op), i.path)
				}
				_ => "?".into(),
			})
			.collect();
		assert_eq!(
			shown,
			[
				"C0",
				"SKIP img.bin",
				"CREATE fresh.txt",
				"DELETE old.txt",
				"CREATE dir/new.txt",
				"C1",
				"C2",
				"OVERWRITE a.txt",
			]
		);
		let by_path = |p: &str, op: PlannedOp| {
			plan.items
				.iter()
				.find(|i| i.path == p && i.op == op)
				.unwrap()
		};
		assert_eq!(
			by_path("img.bin", PlannedOp::Skip(SkipCause::Binary)).op,
			PlannedOp::Skip(SkipCause::Binary)
		);
		let old = by_path("old.txt", PlannedOp::Delete);
		assert!(old.is_delete() && old.dest_exists && old.commit == Some(0));
		assert_eq!(old.rename_note.as_deref(), Some("→ dir/new.txt"));
		assert_eq!(
			by_path("dir/new.txt", PlannedOp::Create)
				.rename_note
				.as_deref(),
			Some("← old.txt")
		);
		assert_eq!(by_path("a.txt", PlannedOp::Overwrite).commit, Some(2));
		// The empty commit has no rows but is still applied.
		assert!(plan.items.iter().all(|i| i.commit != Some(1)));
		assert!(plan.executable());
	}

	#[test]
	fn commit_apply_creates_the_empty_commit_too() {
		let (dir, mut plan) = mixed_plan();
		let count = || {
			git_run(dir.path(), &["rev-list", "--count", "HEAD"])
				.trim()
				.parse::<usize>()
		};
		let before = count().unwrap();
		// The existing a.txt is not skipped: Apply waits for the overwrite.
		assert_eq!(
			plan.execute().unwrap_err().key,
			"commit_overwrite_required"
		);
		assert_eq!(count().unwrap(), before);
		plan.set_all_overwrite(true);
		let applied = plan.execute().unwrap();
		assert_eq!(applied.created_commits.len(), 3);
		assert_eq!(count().unwrap(), before + 3);
	}

	#[test]
	fn nested_directory_without_repo_basename_stays_under_primary() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("dest");
		let repo = tmp.path().join("repo-a");
		fs::create_dir(&dest).unwrap();
		fs::create_dir(&repo).unwrap();
		let payload = "// FILE: 長路徑資料夾/新增.txt\nbody\n// FILE: existing.txt\nnew\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&dest,
			&[repo.clone(), dest.clone()],
			1,
		)
		.unwrap();
		assert!(!plan.mapping_ready());
		assert_eq!(plan.execute().unwrap_err().key, "mapping_required");
		assert!(!dest.join("長路徑資料夾/新增.txt").exists());
		plan.set_keep_relative("長路徑資料夾").unwrap();
		assert!(plan.mapping_ready());
		assert_eq!(plan.items.len(), 2);
		plan.execute().unwrap();
		assert_eq!(
			fs::read_to_string(dest.join("長路徑資料夾/新增.txt")).unwrap(),
			"body"
		);
		assert!(!repo.join("新增.txt").exists());
	}

	#[test]
	fn unmatched_prefix_maps_to_a_differently_named_dest_without_a_source_repo()
	{
		let tmp = tempfile::tempdir().unwrap();
		let primary = tmp.path().join("workspace-dest");
		let wanted = tmp.path().join("target-lib");
		let same = tmp.path().join("nested").join("lib");
		fs::create_dir_all(&primary).unwrap();
		fs::create_dir_all(&wanted).unwrap();
		fs::create_dir_all(&same).unwrap();
		let payload = "// FILE: lib/a.txt\nbody\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&primary,
			&[primary.clone(), wanted.clone(), same.clone()],
			1,
		)
		.unwrap();
		assert!(!plan.mapping_ready());
		assert!(plan.execute().is_err());
		plan.set_prefix_destination("lib", &wanted).unwrap();
		let chosen = plan.prefix_choices[0].destination.clone().unwrap();
		assert_eq!(chosen, CanonicalRootId::new(&wanted).unwrap().path());
		plan.execute().unwrap();
		assert_eq!(fs::read_to_string(wanted.join("a.txt")).unwrap(), "body");
		assert!(!primary.join("lib/a.txt").exists());
		assert!(!primary.join("a.txt").exists());
		assert!(!same.join("a.txt").exists());
	}

	#[test]
	fn destination_blocked_by_a_file_is_reported_with_its_path() {
		let tmp = tempfile::tempdir().unwrap();
		let primary = tmp.path().join("dest");
		let blocker = tmp.path().join("newdir");
		fs::create_dir(&primary).unwrap();
		fs::write(&blocker, "not a dir").unwrap();
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			"// FILE: lib/a.txt\nbody\n",
			&primary,
			std::slice::from_ref(&primary),
			1,
		)
		.unwrap();
		let below = blocker.join("lib");
		let err = plan.set_prefix_destination("lib", &below).unwrap_err();
		assert_eq!(err.key, "paste_err_destination");
		let text = err.render(crate::i18n::Locale::En);
		assert!(!text.starts_with("paste_err_"), "raw key leaked: {text}");
		assert!(text.contains(&*below.to_string_lossy()), "{text}");
		let not_dir = plan.set_prefix_destination("lib", &blocker).unwrap_err();
		assert_eq!(not_dir.key, "paste_err_destination_not_dir");
	}

	#[test]
	fn commit_paste_under_a_file_previews_and_applies_as_refused() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
			LayoutConflict, ReplaySkipReason,
		};
		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("a.txt"), "base\n").unwrap();
		fs::write(repo.join("newdir"), "i am a file\n").unwrap();
		for args in [&["add", "."][..], &["commit", "-qm", "base"][..]] {
			git_run(&repo, args);
		}
		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "incoming\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![file("newdir/x.txt"), file("fresh.txt")],
			}],
		});
		let mut plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 1)
				.expect("the blocked file is planned with layout refusal");

		// Core commit plan records layout conflict and refusal
		let commit_plan =
			&plan.commit_preview.as_ref().unwrap().plan().commits[0];
		assert_eq!(
			commit_plan.files[0].skip_reason,
			Some(ReplaySkipReason::UnsafePath)
		);
		assert_eq!(
			commit_plan.files[0].layout_conflict,
			Some(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(
			commit_plan.refused_by(),
			Some(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(commit_plan.files[1].skip_reason, None);
		assert_eq!(commit_plan.files[1].layout_conflict, None);

		// Item op and action are CommitRefused, and is_not_written() is false
		assert_eq!(
			plan.items[0].op,
			PlannedOp::CommitRefused(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(
			plan.items[0].action(),
			RowAction::CommitRefused(LayoutConflict::FileInTheWayOfParent)
		);
		assert!(!plan.items[0].action().is_not_written());

		// Header not-written count excludes the refused row
		let (total_rows, not_written) = plan.commit_counts(0);
		assert_eq!(total_rows, 2);
		assert_eq!(not_written, 0);

		// PasteCounts excludes the refused row from skips
		let counts = plan.counts();
		assert_eq!(counts.creates, 1);
		assert_eq!(counts.skips, 0);

		// Header labels include refused suffix
		let (hdr_zh, _, _) = crate::ui::commit_header_labels(
			commit_plan,
			crate::i18n::Locale::ZhTw,
		);
		assert_eq!(hdr_zh, "incoming (整個 commit 會被拒絕)");
		let (hdr_en, _, _) = crate::ui::commit_header_labels(
			commit_plan,
			crate::i18n::Locale::En,
		);
		assert_eq!(hdr_en, "incoming (whole commit will be refused)");

		// Summary label counts refused commits
		assert_eq!(
			plan.commit_summary_label(crate::i18n::Locale::ZhTw),
			Some("1 個 commit（1 個被拒絕）".to_string())
		);
		assert_eq!(
			plan.commit_summary_label(crate::i18n::Locale::En),
			Some("1 commit(s) (1 refused)".to_string())
		);

		// Red banner shows refusal on preview load
		assert_eq!(
			plan.error.as_ref().map(|m| m.key),
			Some("commit_will_be_refused")
		);
		let banner_zh = plan
			.error
			.as_ref()
			.unwrap()
			.render(crate::i18n::Locale::ZhTw);
		assert!(
			banner_zh.contains("第 1 個 commit「incoming」會被拒絕"),
			"{banner_zh}"
		);
		let banner_en =
			plan.error.as_ref().unwrap().render(crate::i18n::Locale::En);
		assert!(
			banner_en.contains("Commit #1 \"incoming\" will be refused"),
			"{banner_en}"
		);

		// S4 rule: unchecking a row sets commit_subset_rejected; re-including restores commit_will_be_refused
		plan.toggle_selected(1);
		assert_eq!(
			plan.error.as_ref().map(|m| m.key),
			Some("commit_subset_rejected")
		);
		plan.toggle_selected(1);
		assert_eq!(
			plan.error.as_ref().map(|m| m.key),
			Some("commit_will_be_refused")
		);

		// Apply refuses the whole commit with commit_replay_refused
		let err = plan.execute().unwrap_err();
		assert_eq!(err.key, "commit_replay_refused");
		assert_eq!(err.args[0], "1");
		assert_eq!(err.args[1], "incoming");
		assert_eq!(err.args[2], "reason_refusal_cause_file_in_way");

		// No (none) and no midway failure wording in zh-TW or en
		let rendered_zh = err.render(crate::i18n::Locale::ZhTw);
		assert!(!rendered_zh.contains("(none)"), "{rendered_zh}");
		assert!(!rendered_zh.contains("中途"), "{rendered_zh}");
		assert!(
			rendered_zh.contains("沒有建立任何 commit；第 1 個 commit「incoming」被拒絕：父目錄被檔案佔住"),
			"{rendered_zh}"
		);

		let rendered_en = err.render(crate::i18n::Locale::En);
		assert!(!rendered_en.contains("(none)"), "{rendered_en}");
		assert!(!rendered_en.contains("midway"), "{rendered_en}");
		assert!(
			rendered_en.contains("No commit was created; commit #1 \"incoming\" was refused: a file is in the way of its parent directory"),
			"{rendered_en}"
		);

		// Zero writes made to destination
		assert_eq!(
			fs::read_to_string(repo.join("newdir")).unwrap(),
			"i am a file\n"
		);
		assert!(!repo.join("fresh.txt").exists());
		assert_eq!(
			git_run(&repo, &["rev-list", "--count", "HEAD"]).trim(),
			"1"
		);
	}

	#[test]
	fn commit_empty_subject_refusal_banner_and_apply_uses_commit_no_message() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};

		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("blocker"), "regular file\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);

		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		// Empty commit subject (only whitespace / newline)
		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "   \n\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![file("blocker/child.txt")],
			}],
		});
		let plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 1)
				.expect("preview plan builds");

		// Refusal banner uses commit_no_message instead of empty brackets 「」
		let banner = plan.error.as_ref().expect("refusal banner present");
		assert_eq!(banner.key, "commit_will_be_refused");
		let banner_zh = banner.render(crate::i18n::Locale::ZhTw);
		assert!(
			banner_zh.contains("第 1 個 commit「（無訊息）」會被拒絕"),
			"{banner_zh}"
		);
		assert!(!banner_zh.contains("「」"), "{banner_zh}");
		let banner_en = banner.render(crate::i18n::Locale::En);
		assert!(
			banner_en.contains("Commit #1 \"(no message)\" will be refused"),
			"{banner_en}"
		);
		assert!(!banner_en.contains("\"\""), "{banner_en}");

		// Apply refusal uses commit_no_message and translates cause
		let err = plan.execute().unwrap_err();
		assert_eq!(err.key, "commit_replay_refused");
		let err_zh = err.render(crate::i18n::Locale::ZhTw);
		assert!(
			err_zh.contains(
				"第 1 個 commit「（無訊息）」被拒絕：父目錄被檔案佔住"
			),
			"{err_zh}"
		);
		assert!(!err_zh.contains("「」"), "{err_zh}");
		let err_en = err.render(crate::i18n::Locale::En);
		assert!(
			err_en.contains(
				"commit #1 \"(no message)\" was refused: a file is in the way of its parent directory"
			),
			"{err_en}"
		);
		assert!(!err_en.contains("\"\""), "{err_en}");
	}

	struct HeavyLockBlocker {
		held: snip_core::workspace::HeavyGuard,
		cancel: snip_core::gitrun::CancelToken,
		waiters: Vec<std::thread::JoinHandle<()>>,
	}

	impl HeavyLockBlocker {
		fn fill(repo: &Path) -> Self {
			use snip_core::gitrun::{CancelToken, RunOptions};
			use snip_core::gitsrc::{Git, GitError};
			use snip_core::workspace::{
				lock_heavy, RepoIdentity, MAX_HEAVY_WAITERS,
			};
			use std::time::{Duration, Instant};

			let git = Git::open(repo).unwrap();
			let id =
				RepoIdentity::resolve(&git, &RunOptions::default()).unwrap();
			let held = lock_heavy(&id, &RunOptions::default()).unwrap();
			let cancel = CancelToken::new();
			let mut waiters = Vec::new();
			for _ in 0..MAX_HEAVY_WAITERS {
				let id = id.clone();
				let cancel = cancel.clone();
				waiters.push(std::thread::spawn(move || {
					while !cancel.is_cancelled() {
						let res = lock_heavy(
							&id,
							&RunOptions {
								cancel: Some(cancel.clone()),
								queue_timeout: Duration::from_secs(60),
								..Default::default()
							},
						);
						if matches!(res, Err(GitError::Cancelled { .. })) {
							break;
						}
						std::thread::sleep(Duration::from_millis(5));
					}
				}));
			}
			let deadline = Instant::now() + Duration::from_secs(10);
			loop {
				let res = lock_heavy(
					&id,
					&RunOptions {
						queue_timeout: Duration::from_millis(50),
						..RunOptions::default()
					},
				);
				if matches!(res, Err(GitError::WorktreeBusy { .. })) {
					break;
				}
				assert!(
					Instant::now() < deadline,
					"timed out waiting for heavy lock waiting room to fill"
				);
				std::thread::sleep(Duration::from_millis(10));
			}
			Self {
				held,
				cancel,
				waiters,
			}
		}

		fn release(self) {
			use std::time::{Duration, Instant};

			self.cancel.cancel();
			drop(self.held);
			let deadline = Instant::now() + Duration::from_secs(30);
			for w in self.waiters {
				while !w.is_finished() {
					assert!(
						Instant::now() < deadline,
						"timed out waiting for heavy lock waiter thread to finish"
					);
					std::thread::sleep(Duration::from_millis(5));
				}
				let _ = w.join();
			}
		}
	}

	#[test]
	fn commit_apply_begin_failure_reports_refused_wording_without_none() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};

		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("a.txt"), "base\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);

		let file = |path: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some("x\n".into()),
			not_copied: None,
		};
		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "first commit\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![file("b.txt")],
			}],
		});
		let plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 1)
				.unwrap();

		let blocker = HeavyLockBlocker::fill(&repo);
		let err = plan.execute().unwrap_err();
		blocker.release();

		assert_eq!(err.key, "commit_replay_refused");
		assert_eq!(err.args[0], "1");
		assert_eq!(err.args[1], "first commit");
		let rendered_zh = err.render(crate::i18n::Locale::ZhTw);
		assert!(!rendered_zh.contains("(none)"), "{rendered_zh}");
		assert!(!rendered_zh.contains("中途"), "{rendered_zh}");
		assert!(
			rendered_zh.contains(
				"沒有建立任何 commit；第 1 個 commit「first commit」被拒絕："
			),
			"{rendered_zh}"
		);
	}

	#[test]
	fn commit_apply_raw_error_matching_i18n_key_is_not_translated() {
		// When execute_commit encounters a non-layout failure where the error or subject
		// matches a translation key (e.g. "op_skip"), it must not be translated.
		// Drive this through execute() using a begin/lock failure on a commit with
		// subject "op_skip".
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};

		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("a.txt"), "base\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);

		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![CommitRecord {
				message: "op_skip\n".into(),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: "b.txt".into(),
					old_path: None,
					change: FileChange::Added,
					content: Some("x\n".into()),
					not_copied: None,
				}],
			}],
		});
		let plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 1)
				.unwrap();

		let blocker = HeavyLockBlocker::fill(&repo);
		let err = plan.execute().unwrap_err();
		blocker.release();

		assert_eq!(err.key, "commit_replay_refused");
		assert_eq!(err.args[0], "1");
		assert_eq!(err.args[1], "op_skip");
		let rendered = err.render(crate::i18n::Locale::ZhTw);
		assert!(
			rendered.contains(
				"沒有建立任何 commit；第 1 個 commit「op_skip」被拒絕："
			),
			"{rendered}"
		);
		assert!(!rendered.contains("「跳過」"), "{rendered}");
	}

	#[test]
	fn commit_apply_partial_failure_with_layout_conflict_translates_cause() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
			LayoutConflict,
		};

		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("a.txt"), "base\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);

		let file = |path: &str, content: &str| CommitFile {
			path: path.into(),
			old_path: None,
			change: FileChange::Added,
			content: Some(content.into()),
			not_copied: None,
		};
		// Commit 1 creates regular file "newdir".
		// Commit 2 attempts to create "newdir/x.txt", hitting a layout conflict during replay.
		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![
				CommitRecord {
					message: "commit one\n".into(),
					author_name: "Author".into(),
					author_email: "author@example.invalid".into(),
					author_date: "2026-09-25T12:00:00+00:00".into(),
					files: vec![file("newdir", "i am a file\n")],
				},
				CommitRecord {
					message: "commit two\n".into(),
					author_name: "Author".into(),
					author_email: "author@example.invalid".into(),
					author_date: "2026-09-25T12:01:00+00:00".into(),
					files: vec![file("newdir/x.txt", "under file\n")],
				},
			],
		});
		let plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 2)
				.unwrap();

		// The preview already plans commit 2 after commit 1, so it shows the
		// refusal Apply will hit: header, summary and banner.
		let commit_plans = plan.commit_preview.as_ref().unwrap().plan();
		assert_eq!(commit_plans.commits[0].refused_by(), None);
		assert_eq!(
			commit_plans.commits[1].refused_by(),
			Some(LayoutConflict::FileInTheWayOfParent)
		);
		let (hdr, _, _) = crate::ui::commit_header_labels(
			&commit_plans.commits[1],
			crate::i18n::Locale::En,
		);
		assert_eq!(hdr, "commit two (whole commit will be refused)");
		assert_eq!(
			plan.commit_summary_label(crate::i18n::Locale::En),
			Some("2 commit(s) (1 refused)".to_string())
		);
		assert_eq!(
			plan.items[1].op,
			PlannedOp::CommitRefused(LayoutConflict::FileInTheWayOfParent)
		);
		assert_eq!(
			plan.error.as_ref().map(|m| m.key),
			Some("commit_will_be_refused")
		);
		let banner =
			plan.error.as_ref().unwrap().render(crate::i18n::Locale::En);
		assert!(
			banner.contains("Commit #2 \"commit two\" will be refused"),
			"{banner}"
		);

		let err = plan.execute().unwrap_err();
		assert_eq!(err.key, "commit_replay_partial_refused");
		assert_eq!(err.args.len(), 4);
		assert_eq!(err.args[1], "2");
		assert_eq!(err.args[2], "newdir/x.txt");
		assert_eq!(err.args[3], "reason_refusal_cause_file_in_way");

		// In zh-TW, banner uses translated cause and contains no English layout reason
		let rendered_zh = err.render(crate::i18n::Locale::ZhTw);
		assert!(rendered_zh.contains("重放中途停止"), "{rendered_zh}");
		assert!(
			rendered_zh.contains(
				"第 2 個 commit 被拒絕：newdir/x.txt：父目錄被檔案佔住"
			),
			"{rendered_zh}"
		);
		assert!(
			!rendered_zh.contains("a file is in the way"),
			"{rendered_zh}"
		);

		// In en, banner uses English cause
		let rendered_en = err.render(crate::i18n::Locale::En);
		assert!(
			rendered_en.contains("Replay stopped midway"),
			"{rendered_en}"
		);
		assert!(
			rendered_en.contains(
				"Commit #2 was refused: newdir/x.txt: a file is in the way of its parent directory"
			),
			"{rendered_en}"
		);

		// First commit was created; second commit was not
		assert_eq!(
			git_run(&repo, &["rev-list", "--count", "HEAD"]).trim(),
			"2"
		);
		assert_eq!(
			fs::read_to_string(repo.join("newdir")).unwrap(),
			"i am a file\n"
		);
		assert!(!repo.join("newdir/x.txt").exists());
	}

	#[test]
	fn commit_preview_does_not_refuse_a_blocker_an_earlier_commit_removes() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};

		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("newdir"), "i am a file\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);

		let record =
			|message: &str, path: &str, change, content| CommitRecord {
				message: format!("{message}\n"),
				author_name: "Author".into(),
				author_email: "author@example.invalid".into(),
				author_date: "2026-09-25T12:00:00+00:00".into(),
				files: vec![CommitFile {
					path: path.into(),
					old_path: None,
					change,
					content,
					not_copied: None,
				}],
			};
		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![
				record("remove blocker", "newdir", FileChange::Deleted, None),
				record(
					"write under it",
					"newdir/x.txt",
					FileChange::Added,
					Some("x\n".into()),
				),
			],
		});
		let plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 2)
				.unwrap();

		let commit_plans = plan.commit_preview.as_ref().unwrap().plan();
		assert_eq!(commit_plans.commits[1].refused_by(), None);
		assert_eq!(
			plan.commit_summary_label(crate::i18n::Locale::En),
			Some("2 commit(s)".to_string())
		);
		assert!(plan.error.is_none(), "{:?}", plan.error);
		let (hdr, _, _) = crate::ui::commit_header_labels(
			&commit_plans.commits[1],
			crate::i18n::Locale::En,
		);
		assert_eq!(hdr, "write under it");

		plan.execute().expect("both commits replay");
		assert_eq!(
			git_run(&repo, &["rev-list", "--count", "HEAD"]).trim(),
			"3"
		);
		assert_eq!(
			fs::read_to_string(repo.join("newdir/x.txt")).unwrap(),
			"x\n"
		);
	}

	#[test]
	fn commit_preview_rename_of_a_file_an_earlier_commit_adds_is_a_real_delete()
	{
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};

		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git_init(&repo);
		fs::write(repo.join("keep.txt"), "keep\n").unwrap();
		git_run(&repo, &["add", "."]);
		git_run(&repo, &["commit", "-qm", "base"]);

		let record = |message: &str,
		              path: &str,
		              old_path: Option<&str>,
		              change| CommitRecord {
			message: format!("{message}\n"),
			author_name: "Author".into(),
			author_email: "author@example.invalid".into(),
			author_date: "2026-09-25T12:00:00+00:00".into(),
			files: vec![CommitFile {
				path: path.into(),
				old_path: old_path.map(Into::into),
				change,
				content: Some("x\n".into()),
				not_copied: None,
			}],
		};
		let text = commits::to_clipboard_text(&CommitsPayload {
			commits: vec![
				record("add", "a.txt", None, FileChange::Added),
				record("rename", "b.txt", Some("a.txt"), FileChange::Renamed),
			],
		});
		let plan =
			PastePreviewPlan::build_from_clipboard_text(&text, &repo, &[], 2)
				.unwrap();

		let old = plan
			.items
			.iter()
			.find(|i| i.path == "a.txt" && i.op == PlannedOp::Delete)
			.expect("the rename shows its old path as a delete row");
		assert_eq!(old.action(), RowAction::Delete);
		assert!(old.shows_delete_notice());

		plan.execute().expect("both commits replay");
		assert!(!repo.join("a.txt").exists());
		assert_eq!(fs::read_to_string(repo.join("b.txt")).unwrap(), "x\n");
	}

	#[test]
	fn ordinary_single_root_does_not_follow_an_opened_basename() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("dest");
		let trap = tmp.path().join("lib");
		fs::create_dir(&dest).unwrap();
		fs::create_dir(&trap).unwrap();
		let payload = "// clipcode-root: lib\n// FILE: lib/a.txt\nbody\n";
		let plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&dest,
			&[trap.clone(), dest.clone()],
			1,
		)
		.unwrap();
		assert!(plan.prefix_choices.is_empty());
		let res = plan.execute().unwrap().files;
		assert_eq!(res.created_count, 1);
		assert_eq!(fs::read_to_string(dest.join("lib/a.txt")).unwrap(), "body");
		assert!(!trap.join("a.txt").exists());
	}

	#[test]
	fn same_basename_mapping_uses_the_chosen_canonical_path() {
		let tmp = tempfile::tempdir().unwrap();
		let dest = tmp.path().join("primary");
		let trap = tmp.path().join("one").join("lib");
		let want = tmp.path().join("two").join("lib");
		fs::create_dir_all(&dest).unwrap();
		fs::create_dir_all(&trap).unwrap();
		fs::create_dir_all(&want).unwrap();
		let payload = "// FILE: lib/a.txt\nbody\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			&dest,
			&[dest.clone(), trap.clone(), want.clone()],
			1,
		)
		.unwrap();
		assert!(!plan.mapping_ready());
		let named_lib = plan.prefix_choices[0]
			.candidates
			.iter()
			.filter(|p| p.file_name().and_then(|n| n.to_str()) == Some("lib"))
			.count();
		assert!(named_lib >= 2, "both lib directories stay selectable");
		plan.set_prefix_destination("lib", &want).unwrap();
		let chosen = plan.prefix_choices[0].destination.clone().unwrap();
		assert_eq!(chosen, CanonicalRootId::new(&want).unwrap().path());
		plan.execute().unwrap();
		assert_eq!(fs::read_to_string(want.join("a.txt")).unwrap(), "body");
		assert!(!trap.join("a.txt").exists());
		assert!(!dest.join("lib/a.txt").exists());
	}

	#[test]
	fn commit_replay_blocks_unchecked_overwrite_and_stale_bytes() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		git_run(&path, &["add", "."]);
		git_run(&path, &["commit", "-qm", "base"]);
		let text = commit_payload_modifying_a();

		let mut unchecked =
			PastePreviewPlan::build_from_clipboard_text(&text, &path, &[], 1)
				.unwrap();
		assert!(unchecked.items[0].content.contains("incoming"));
		assert!(unchecked.items[0].content.contains("Author"));
		assert!(!unchecked.items[0].overwrite_allowed);
		unchecked.items[0].selected = false;
		unchecked.items[0].overwrite_allowed = false;
		assert!(unchecked.check_stale_destination().is_ok());
		assert_eq!(
			unchecked.execute().unwrap_err().key,
			"commit_subset_rejected"
		);
		assert_eq!(fs::read_to_string(path.join("a.txt")).unwrap(), "base\n");

		let mut stale =
			PastePreviewPlan::build_from_clipboard_text(&text, &path, &[], 2)
				.unwrap();
		stale.items[0].overwrite_allowed = true;
		fs::write(path.join("a.txt"), "external change\n").unwrap();
		assert_eq!(
			stale.check_stale_destination().unwrap_err().key,
			"stale_modified"
		);
		assert_eq!(stale.execute().unwrap_err().key, "stale_modified");
		assert_eq!(
			fs::read_to_string(path.join("a.txt")).unwrap(),
			"external change\n"
		);
	}

	#[test]
	fn commit_replay_refuses_when_skipped_non_utf8_becomes_text() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		let git = |args: &[&str]| git_run(&path, args);
		git(&["add", "."]);
		git(&["commit", "-qm", "base"]);
		let head = git(&["rev-parse", "HEAD"]);
		let index = git(&["rev-parse", ":a.txt"]);
		fs::write(path.join("a.txt"), [0xff, 0xfe, 0x01]).unwrap();
		let plan = PastePreviewPlan::build_from_clipboard_text(
			&commit_payload_modifying_a(),
			&path,
			&[],
			1,
		)
		.unwrap();
		assert_eq!(plan.items[0].op, PlannedOp::Skip(SkipCause::NonUtf8Target));
		assert!(!plan.items[0].overwrite_allowed);
		fs::write(path.join("a.txt"), "external change\n").unwrap();
		assert_eq!(
			plan.check_stale_destination().unwrap_err().key,
			"stale_modified"
		);
		assert_eq!(plan.execute().unwrap_err().key, "stale_modified");
		assert_eq!(
			fs::read_to_string(path.join("a.txt")).unwrap(),
			"external change\n"
		);
		assert_eq!(git(&["rev-parse", "HEAD"]), head);
		assert_eq!(git(&["rev-parse", ":a.txt"]), index);
	}

	#[test]
	fn commit_replay_keeps_author_and_reports_partial_commits() {
		use snip_core::commits::{
			CommitFile, CommitRecord, CommitsPayload, FileChange,
		};
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		git_run(&path, &["add", "."]);
		git_run(&path, &["commit", "-qm", "base"]);

		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			&commit_payload_modifying_a(),
			&path,
			&[],
			1,
		)
		.unwrap();
		assert_eq!(
			plan.execute().unwrap_err().key,
			"commit_overwrite_required"
		);
		assert_eq!(fs::read_to_string(path.join("a.txt")).unwrap(), "base\n");
		plan.items[0].overwrite_allowed = true;
		let applied = plan.execute().unwrap();
		assert_eq!(applied.created_commits.len(), 1);
		let meta = git_run(&path, &["log", "-1", "--format=%an|%ae|%B"]);
		assert!(meta.contains("Author|author@example.invalid|incoming"));
		let body = git_output(&path, &["show", "HEAD:a.txt"]);
		assert_eq!(body.stdout, b"incoming\n");

		let nested = CommitsPayload {
			commits: vec![
				CommitRecord {
					message: "add leaf\n".into(),
					author_name: "Author".into(),
					author_email: "author@example.invalid".into(),
					author_date: "2026-09-25T12:00:00+00:00".into(),
					files: vec![CommitFile {
						path: "leaf".into(),
						old_path: None,
						change: FileChange::Added,
						content: Some("file-not-dir\n".into()),
						not_copied: None,
					}],
				},
				CommitRecord {
					message: "into leaf\n".into(),
					author_name: "Author".into(),
					author_email: "author@example.invalid".into(),
					author_date: "2026-09-25T12:01:00+00:00".into(),
					files: vec![CommitFile {
						path: "leaf/child.txt".into(),
						old_path: None,
						change: FileChange::Added,
						content: Some("nope\n".into()),
						not_copied: None,
					}],
				},
			],
		};
		let nested_plan = PastePreviewPlan::build_from_clipboard_text(
			&commits::to_clipboard_text(&nested),
			&path,
			&[],
			2,
		)
		.unwrap();
		let err = nested_plan.execute().unwrap_err();
		assert_eq!(err.key, "commit_replay_partial_refused");
		assert_ne!(err.args[0], "(none)");
		assert_eq!(err.args[1], "2");
		assert_eq!(err.args[2], "leaf/child.txt");
		assert_eq!(err.args[3], "reason_refusal_cause_file_in_way");
		assert_eq!(
			fs::read_to_string(path.join("leaf")).unwrap(),
			"file-not-dir\n"
		);
		assert!(!path.join("leaf/child.txt").exists());
	}

	#[test]
	fn commit_apply_reports_stale_before_the_overwrite_prompt() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		git_run(&path, &["add", "."]);
		git_run(&path, &["commit", "-qm", "base"]);

		let plan = PastePreviewPlan::build_from_clipboard_text(
			&commit_payload_modifying_a(),
			&path,
			&[],
			1,
		)
		.unwrap();

		fs::write(path.join("a.txt"), "external\n").unwrap();
		let err = plan.execute().unwrap_err();
		assert_eq!(err.key, "stale_modified");
		assert_eq!(fs::read(path.join("a.txt")).unwrap(), b"external\n");
	}

	fn make_item(
		selected: bool,
		commit: Option<usize>,
		op: PlannedOp,
		dest_exists: bool,
		overwrite_allowed: bool,
	) -> PasteItem {
		PasteItem {
			path: "test.txt".into(),
			dest_root: PathBuf::from("."),
			dest_root_name: "root".into(),
			dest_path: "test.txt".into(),
			dest_exists,
			op,
			overwrite_allowed,
			selected,
			content: Arc::from(""),
			bytes: 10,
			lines: 1,
			op_index: 0,
			commit,
			rename_note: None,
		}
	}

	#[test]
	fn row_action_covers_every_row_state() {
		assert_eq!(
			make_item(false, Some(0), PlannedOp::Create, false, false).action(),
			RowAction::Excluded { by_commit: true }
		);
		assert_eq!(
			make_item(false, None, PlannedOp::Create, false, false).action(),
			RowAction::Excluded { by_commit: false }
		);
		assert_eq!(
			make_item(
				true,
				Some(0),
				PlannedOp::Skip(SkipCause::Binary),
				false,
				false
			)
			.action(),
			RowAction::Skip(SkipCause::Binary)
		);
		assert_eq!(
			make_item(true, None, PlannedOp::Delete, true, true).action(),
			RowAction::Delete
		);
		assert_eq!(
			make_item(true, None, PlannedOp::Delete, false, true).action(),
			RowAction::DeleteMissing
		);
		assert_eq!(
			make_item(true, None, PlannedOp::Create, false, false).action(),
			RowAction::Create
		);
		assert_eq!(
			make_item(true, None, PlannedOp::Overwrite, true, true).action(),
			RowAction::Overwrite
		);
		assert_eq!(
			make_item(true, Some(0), PlannedOp::Overwrite, true, false)
				.action(),
			RowAction::OverwritePending
		);
		assert_eq!(
			make_item(true, None, PlannedOp::Overwrite, true, false).action(),
			RowAction::KeepExisting
		);
		assert_eq!(
			make_item(
				true,
				Some(0),
				PlannedOp::CommitRefused(LayoutConflict::FileInTheWayOfParent),
				false,
				false
			)
			.action(),
			RowAction::CommitRefused(LayoutConflict::FileInTheWayOfParent)
		);
		assert!(!RowAction::CommitRefused(
			LayoutConflict::FileInTheWayOfParent
		)
		.is_not_written());
	}

	#[test]
	fn commit_replay_toggle_selected_error_lifecycle() {
		let (_dir, mut plan) = mixed_plan();
		assert!(plan.items.len() >= 2);
		assert!(plan.whole_commit);
		assert!(plan.error.is_none());

		// Exclude a row -> plan.error key == "commit_subset_rejected"
		plan.toggle_selected(0);
		assert_eq!(
			plan.error.as_ref().map(|e| e.key),
			Some("commit_subset_rejected")
		);

		// Re-include -> plan.error is None
		plan.toggle_selected(0);
		assert!(plan.error.is_none());

		// Seed a DIFFERENT key manually after excluding
		plan.toggle_selected(0);
		assert_eq!(
			plan.error.as_ref().map(|e| e.key),
			Some("commit_subset_rejected")
		);
		plan.error = Some(Msg::new("stale_modified", []));
		// Re-include and assert the different error survives
		plan.toggle_selected(0);
		assert_eq!(plan.error.as_ref().map(|e| e.key), Some("stale_modified"));

		// Reset error for partial re-include test
		plan.error = None;
		// Two rows excluded
		plan.toggle_selected(0);
		plan.toggle_selected(1);
		assert_eq!(
			plan.error.as_ref().map(|e| e.key),
			Some("commit_subset_rejected")
		);

		// Partial re-include: one re-included, one still excluded -> keeps banner
		plan.toggle_selected(0);
		assert_eq!(
			plan.error.as_ref().map(|e| e.key),
			Some("commit_subset_rejected")
		);

		// Remaining row re-included -> all selected -> banner cleared
		plan.toggle_selected(1);
		assert!(plan.error.is_none());
	}

	#[test]
	fn commit_replay_space_cycle_keeps_a_pre_existing_real_error() {
		let (_dir, mut plan) = mixed_plan();
		assert!(plan.items.len() >= 2);
		// The informational refusal banner is not a real error: it gives way and returns.
		plan.error = plan.refusal_banner();
		let key = |p: &PastePreviewPlan| p.error.as_ref().map(|e| e.key);
		plan.toggle_selected(0);
		assert_eq!(key(&plan), Some("commit_subset_rejected"));
		plan.toggle_selected(0);
		assert_eq!(key(&plan), plan.refusal_banner().as_ref().map(|e| e.key));

		// A real error (what Apply records) survives exclude and re-include.
		plan.error = Some(Msg::new("stale_created", []));
		plan.toggle_selected(0);
		assert_eq!(key(&plan), Some("stale_created"));
		plan.toggle_selected(0);
		assert_eq!(key(&plan), Some("stale_created"));
	}

	#[test]
	fn commit_replay_reinclude_single_row_clears_banner() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		git_init(&path);
		fs::write(path.join("a.txt"), "base\n").unwrap();
		git_run(&path, &["add", "."]);
		git_run(&path, &["commit", "-qm", "base"]);
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			&commit_payload_modifying_a(),
			&path,
			&[],
			1,
		)
		.unwrap();
		assert!(plan.whole_commit);
		assert!(plan.error.is_none());

		plan.toggle_selected(0);
		assert_eq!(
			plan.error.as_ref().map(|e| e.key),
			Some("commit_subset_rejected")
		);

		plan.toggle_selected(0);
		assert!(plan.error.is_none());
	}

	#[test]
	fn commit_rows_classify_and_count_from_the_replay_plan() {
		let (_dir, plan) = mixed_plan();
		let shape: Vec<String> = plan
			.commit_rows()
			.iter()
			.map(|r| match r {
				PasteNode::Commit(c) => format!("C{c}"),
				PasteNode::File(ix, _) => {
					let i = &plan.items[*ix];
					format!("{:?}:{}", i.action(), i.path)
				}
				_ => "?".into(),
			})
			.collect();
		assert_eq!(
			shape,
			[
				"C0",
				"Skip(Binary):img.bin",
				"Create:fresh.txt",
				"Delete:old.txt",
				"Create:dir/new.txt",
				"C1",
				"C2",
				"OverwritePending:a.txt",
			]
		);
		assert_eq!(
			plan.counts(),
			PasteCounts {
				creates: 2,
				overwrites: 0,
				existing: 1,
				deletes: 1,
				skips: 1,
			}
		);
		assert_eq!(plan.commit_counts(0), (4, 1));
		assert_eq!(plan.commit_counts(1), (0, 0));
		assert_eq!(plan.commit_counts(2), (1, 0));
		let mut excluded = plan.items[0].clone();
		excluded.selected = false;
		assert_eq!(excluded.action(), RowAction::Excluded { by_commit: true });
		assert!(plan.executable());
	}

	#[test]
	fn up_down_walk_commit_rows_in_replay_order() {
		let (_dir, mut plan) = mixed_plan();
		plan.selected_item_idx = 0;
		let mut seen = vec![plan.items[0].path.clone()];
		for _ in 0..plan.items.len() + 2 {
			plan.select_next();
			seen.push(plan.items[plan.selected_item_idx].path.clone());
		}
		seen.dedup();
		assert_eq!(
			seen,
			["img.bin", "fresh.txt", "old.txt", "dir/new.txt", "a.txt"]
		);
		// A folded commit's rows are skipped, and Up re-enters
		// from a selection that is folded away.
		plan.toggle_commit_collapsed(0);
		plan.select_prev();
		assert_eq!(plan.items[plan.selected_item_idx].path, "a.txt");
	}

	#[test]
	fn counts_follow_row_actions_in_file_mode() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("existing.txt"), "old\n").unwrap();
		let payload = "// FILE: existing.txt\nnew\n// FILE: fresh.txt\nfresh\n";
		let mut plan = PastePreviewPlan::build_from_clipboard_text(
			payload,
			dir.path(),
			&[],
			1,
		)
		.unwrap();
		assert_eq!(
			plan.counts(),
			PasteCounts {
				creates: 1,
				overwrites: 0,
				existing: 1,
				deletes: 0,
				skips: 1,
			}
		);
		let exist_ix = plan
			.items
			.iter()
			.position(|i| i.path == "existing.txt")
			.unwrap();
		plan.toggle_overwrite(exist_ix);
		assert_eq!(
			plan.counts(),
			PasteCounts {
				creates: 1,
				overwrites: 1,
				existing: 1,
				deletes: 0,
				skips: 0,
			}
		);
		let fresh_ix = plan
			.items
			.iter()
			.position(|i| i.path == "fresh.txt")
			.unwrap();
		plan.toggle_selected(fresh_ix);
		assert_eq!(
			plan.counts(),
			PasteCounts {
				creates: 0,
				overwrites: 1,
				existing: 1,
				deletes: 0,
				skips: 1,
			}
		);
	}

	#[test]
	fn step_target_and_fold_do_not_move_the_selection() {
		let (_dir, mut plan) = mixed_plan();
		plan.selected_item_idx = 0;
		let target = plan.step_target(true);
		assert_eq!(
			plan.selected_item_idx, 0,
			"step_target must not mutate selection"
		);
		assert_eq!(target, 1);
		let target_back = plan.step_target(false);
		assert_eq!(
			plan.selected_item_idx, 0,
			"step_target must not mutate selection"
		);
		assert_eq!(target_back, 0);

		let folded_target = plan.toggle_commit_collapsed(0);
		assert_eq!(
			plan.selected_item_idx, 0,
			"toggle_commit_collapsed must not mutate selection"
		);
		assert_eq!(folded_target, Some(4));

		let unfolded_target = plan.toggle_commit_collapsed(0);
		assert_eq!(plan.selected_item_idx, 0);
		assert_eq!(unfolded_target, None);
	}

	#[test]
	fn overwrite_missing_is_the_overwritable_rows_without_permission() {
		let (_dir, mut plan) = mixed_plan();
		assert!(plan.overwrite_missing());
		plan.set_all_overwrite(true);
		assert!(!plan.overwrite_missing());
	}

	#[test]
	fn row_control_ids_keep_the_driver_shape() {
		assert_eq!(
			RowControl::Row.id(3, "長路徑/a b.txt"),
			"paste-row:3:長路徑/a b.txt"
		);
		assert_eq!(
			RowControl::Include.id(3, "長路徑/a b.txt"),
			"paste-include:3:長路徑/a b.txt"
		);
		assert_eq!(
			RowControl::Overwrite.id(3, "長路徑/a b.txt"),
			"paste-overwrite:3:長路徑/a b.txt"
		);
		assert_eq!(commit_header_id(2), "paste-commit:2");
	}

	#[test]
	fn writes_content_matches_only_create_and_overwrite() {
		let make = |op| PasteItem {
			path: "a.txt".into(),
			dest_root: PathBuf::new(),
			dest_root_name: String::new(),
			dest_path: PathBuf::new(),
			dest_exists: false,
			op,
			overwrite_allowed: false,
			selected: true,
			content: Arc::from(""),
			bytes: 42,
			lines: 1,
			op_index: 0,
			commit: None,
			rename_note: None,
		};
		assert!(make(PlannedOp::Create).writes_content());
		assert!(make(PlannedOp::Overwrite).writes_content());
		assert!(!make(PlannedOp::Delete).writes_content());
		assert!(!make(PlannedOp::Skip(SkipCause::Binary)).writes_content());
		assert!(!make(PlannedOp::Skip(SkipCause::Other)).writes_content());
	}
}
